//! The Docker provider: a sandbox for a role as a container that needs no root to start.
//!
//! [`Docker`], a [`Provider`], makes each sandbox a container on the operator's [`Engine`], with:
//!
//! - no network but loopback; the dial's socket, in a host directory mounted read-only, is the
//!   only way out, and files move only over the session;
//! - the role's [`confinement`], and CPUs of its run's own (`cpus`);
//! - `sandboxd`, read-only, as its first process, dialing its [`SocketDial`], its log's only
//!   writer;
//! - for a role that makes ops ([`Create::ops`]), [`sandboxctl`](SANDBOXCTL_PATH), read-only;
//! - for a role with a model route ([`Create::model`]), `sandboxd` serving the model endpoint.
//!
//! On the engine, it creates only containers, labelled with their run's [`RunTag`] for
//! [`Sweep::list`], and their anonymous volumes, and pulls no image; outside it, it writes only
//! its runs' entries in the ledger of CPUs. It needs an engine that enforces every limit that a
//! container has, as a rootless one does only with `cpuset` ([`engine::Security::rootless`]).

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::Output;

use engine::{ContextName, Engine, REMOVE_TIMEOUT, confinement, docker, docker_to, fix_reported};
use provider::{
    Console, Create, Created, Creator, ImageRef, Listed, Meeting, Provider, Ready, Reservation,
    Role, RunTag, SandboxId, SandboxdRef, Sweep, Undeleted,
};
use schema::ir::{Image, Name};
use schema::sandbox::{SANDBOXCTL_PATH, SANDBOXD_PATH, SOCKET_NAME, SocketDial};
use schema::workflow::{Class, Offers};
use serde_json::json;
use stdx::process::{Bound, Failure};
use stdx::{Context, Deadline};

use crate::cpus::Cpus;

mod cpus;

pub use cpus::CpuList;

/// The name of the provider's one class of sandbox, a container on the engine's host, which
/// gives a role as many CPUs as the provider gives sandboxes, and as much memory as the host has.
const CLASS: Name = Name::from_static("container");

/// The most seconds a sandbox's start takes, as [`Class::startup_seconds`] declares it.
const STARTUP_SECONDS: NonZeroU64 = NonZeroU64::new(540).unwrap();

/// Where the session's socket directory is mounted in the container.
const GATEWAY_DIR: &str = "/.pumpkin/gateway";

const RUN_LABEL: &str = "pumpkin.run";

const ROLE_LABEL: &str = "pumpkin.role";

/// The provider of sandboxes as containers on an [`Engine`], which every container of its runs
/// in. Its commands are bounded as the engine's are, a create's at the create's deadline
/// ([`Create::by`]).
#[derive(Debug)]
pub struct Docker<'e> {
    engine: &'e Engine,
    /// What [`Provider::ready`] gives: the engine's report as JSON, and the one class.
    ready: Ready,
    /// The CPUs it shares out between runs.
    cpus: Cpus,
}

impl<'e> Docker<'e> {
    /// The provider on `engine`, whose sandboxes are given the CPUs of `cpus`, or else all that
    /// are online. It refuses an engine that can't enforce every container's limits, and `cpus`
    /// that aren't online.
    pub fn connect(engine: &'e Engine, cpus: Option<&CpuList>) -> Result<Self, String> {
        let report = engine.report();
        let &engine::Host {
            memory_limit,
            swap_limit,
            cpu_cfs_quota,
            cpu_set,
            pids_limit,
            memory_bytes,
            ref id,
        } = engine.host()?;
        check_limits(
            &report.context,
            [
                ("MemoryLimit", memory_limit),
                ("SwapLimit", swap_limit),
                ("CpuCfsQuota", cpu_cfs_quota),
                ("CPUSet", cpu_set),
                ("PidsLimit", pids_limit),
            ],
        )?;
        // The engine runs on this machine's kernel, as its containers mount this machine's paths,
        // so its CPUs are those the kernel here lists.
        let cpus = Cpus::of_machine(id, cpus)?;
        let offers = Offers {
            cpu_millis: cpus.cpu_millis(),
            memory_bytes,
            gpu: None,
            cpu_features: cpus::flags()?,
            // A class that states its CPU's model is offered only to roles that need that model,
            // and the engine has this one class, which every role is to fit.
            cpu_model: None,
        };
        Ok(Self {
            engine,
            ready: ready(report, offers),
            cpus,
        })
    }

    /// Reaches the engine through `context`, as [`Engine::reach`] does, for a sweep of a run's
    /// containers alone: whether the engine is fit to run on is not checked.
    pub fn reach(context: ContextName) -> Result<Reached, String> {
        Engine::reach(&context)?;
        Ok(Reached { context })
    }

    fn context(&self) -> &'e ContextName {
        &self.engine.report().context
    }
}

/// The Docker engine a context reaches, unchecked for running on, which can only sweep a run's
/// containers ([`Docker::reach`]). Its commands are bounded as [`Engine`]'s are.
#[derive(Debug)]
pub struct Reached {
    context: ContextName,
}

impl Sweep for Reached {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        delete(&self.context, run, ids)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        of_run(&self.context, run)
    }
}

impl Sweep for Docker<'_> {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        delete(self.context(), run, ids)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        of_run(self.context(), run)
    }
}

impl Provider for Docker<'_> {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    fn reserve(&self, run: &RunTag, phases: &[Vec<Role>]) -> Result<Reservation, String> {
        cpus::reserve(&self.cpus, run, phases)
    }

    // A sandbox runs what the engine has, as it pulls nothing, so a publish checks that the
    // engine has the image, for its own platform, and names it as it is.
    fn publish_image(&self, image: &Image) -> Result<ImageRef, String> {
        let report = self.engine.report();
        let runs = format!("the engine of context `{}` runs", report.context.as_str());
        (self.engine).check_platform(image, &report.platform, &runs)?;
        Ok(ImageRef::from(image.to_string()))
    }

    // A sandbox mounts `sandboxd` from the host, so a publish checks that it can be mounted, and
    // names it by its path, resolved.
    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String> {
        (mountable(path, SANDBOXD_PATH))
            .context("publishing `sandboxd` failed")
            .map(SandboxdRef::from)
    }

    // A container reaches the host only through the socket mounted into it.
    fn meeting(&self) -> Meeting<'_> {
        Meeting::Local(self)
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let context = self.context();
        let id = id.as_str();
        if !of_run(context, run)?
            .iter()
            .any(|ours| ours.id.as_str() == id)
        {
            return Err(format!(
                "container {id} is no sandbox of run {}: name one that the run's list finds",
                run.as_str()
            ));
        }
        let text = log_tail(context, id).map_err(|error| fix_reported(&error))?;
        Ok(Console { text })
    }
}

impl Creator<SocketDial> for Docker<'_> {
    fn create(&self, create: &Create, dial: SocketDial) -> Result<Created, String> {
        let context = self.context();
        // Each of its commands gives up at its deadline (`Docker`). A `docker create` given up on
        // may still make its container: the engine finishes a request whose client has gone, and
        // a list sees the container only once the engine has registered it, which may be after
        // the run's end has listed. Nothing starts it (`Creator::create`).
        let by = Bound::By(Deadline::at_unix(create.by.since_epoch()));
        // Creating and starting apart gives the container's ID before anything can fail with
        // a container left behind, so a failure removes this container by its ID.
        let args = create_args(create, &dial, &self.cpus);
        let args = args.context("creating the container failed")?;
        let created = docker_to(context, &args, by).map_err(|error| fix_reported(&error))?;
        let id = String::from_utf8_lossy(&created.stdout).trim().to_owned();
        if let Err(error) = docker_to(context, ["start", &id], by) {
            let mut message = fix_reported(&error);
            let left = format_args!("container {id} is left behind, as removing it failed");
            let removed = remove(context, &id, by).map_err(|failure| fix_reported(&failure));
            if let Err(removal) = removed.context(left) {
                message = format!("{message}; {removal}");
            }
            return Err(message);
        }
        Ok(Created {
            id: SandboxId::from(id),
        })
    }
}

/// What [`Provider::ready`] gives of an engine that `report` describes, whose one class `offers`
/// what its host has.
fn ready(report: &engine::Report, offers: Offers) -> Ready {
    let class = Class {
        offers,
        startup_seconds: STARTUP_SECONDS,
        usd_per_hour: None,
    };
    Ready {
        report: json!(report),
        classes: BTreeMap::from([(CLASS, class)]),
    }
}

/// The arguments of `docker create` for the container, which dials the socket `dial` names, on
/// its run's share of `cpus`.
fn create_args(create: &Create, dial: &SocketDial, cpus: &Cpus) -> Result<Vec<String>, String> {
    let Create {
        run,
        name: sandbox_name,
        role,
        instance,
        // The provider has one class (`CLASS`), so every container is of it.
        class: _,
        image,
        sandboxd,
        ops,
        model,
        // Nothing here reads the hard end, and no clock ends a container: it ends with its
        // `sandboxd`, which ends with its session, over a socket on the driver's machine that the
        // driver's death closes, as `Creator::create` asks of a sandbox there. While the driver
        // holds the session, even hung, the container runs on.
        hard_end: _,
        // The deadline bounds the create's commands (`Docker::create`), and none of their options.
        by: _,
    } = create;
    let SocketDial { dir } = dial;
    let Role {
        name,
        user,
        resources,
        // The ledger holds a share for each instance, and `instance` names the one to take.
        instances: _,
    } = role;
    let sandboxd = Path::new(sandboxd.as_str());
    let mut args = vec![
        "create".to_owned(),
        // `<run>.<name>`, so that `docker ps` shows which sandbox of which run it is: unique on
        // the engine, as run tags are and a name is within its run, and read back as both, as a
        // name holds no `.`.
        format!("--name={}.{sandbox_name}", run.as_str()),
        format!("--label={RUN_LABEL}={}", run.as_str()),
        format!("--label={ROLE_LABEL}={name}"),
        "--network=none".to_owned(),
    ];
    args.extend(confinement(*user, *resources));
    let share = cpus::share(cpus, run, name, *instance)?;
    args.push(format!("--cpuset-cpus={share}"));
    args.extend([
        bind_mount(sandboxd, SANDBOXD_PATH)?,
        bind_mount(dir, GATEWAY_DIR)?,
    ]);
    if *ops {
        args.push(bind_mount(sandboxd, SANDBOXCTL_PATH)?);
    }
    args.extend([
        format!("--entrypoint={SANDBOXD_PATH}"),
        "--".to_owned(),
        image.as_str().to_owned(),
        format!("{GATEWAY_DIR}/{SOCKET_NAME}"),
    ]);
    if *ops {
        args.push("--ops".to_owned());
    }
    if *model {
        args.push("--model".to_owned());
    }
    Ok(args)
}

/// A read-only `--mount` of `host`, which must exist, at `container`.
fn bind_mount(host: &Path, container: &str) -> Result<String, String> {
    // `--mount` reads its value as a line of CSV, so the source, which may hold commas and
    // quotes, is a quoted field, with each quote doubled.
    let source = mountable(host, container)?.replace('"', "\"\"");
    Ok(format!(
        "--mount=type=bind,\"source={source}\",target={container},readonly"
    ))
}

/// The path that a `--mount` of `host`, which must exist, at `container` names: `host`'s own,
/// resolved, in UTF-8, as the engine's API takes it.
fn mountable(host: &Path, container: &str) -> Result<String, String> {
    let host = host.canonicalize().map_err(|error| {
        format!(
            "{} can't be mounted at {container}: {error}: name a path that exists",
            host.display()
        )
    })?;
    let Some(source) = host.to_str() else {
        return Err(format!(
            "{} can't be mounted at {container}: name a path of UTF-8",
            host.display()
        ));
    };
    Ok(source.to_owned())
}

/// Removes the container with ID `id`, and its anonymous volumes, within `bound`. `--force` kills
/// the container first if it runs. A container already gone is removed.
fn remove(context: &ContextName, id: &str, bound: Bound) -> Result<(), Failure> {
    match docker_to(context, ["rm", "--force", "--volumes", id], bound) {
        Ok(_) => Ok(()),
        // Docker 29 exits 0 here; older engines, down to the engine's floor, Docker 28.0, fail.
        Err(failure) if failure.reported().contains("No such container") => Ok(()),
        Err(error) => Err(error),
    }
}

fn delete(context: &ContextName, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
    // With no id to filter by, the lookup would list every run's containers, for nothing.
    if ids.is_empty() {
        return Ok(());
    }
    // Every sandbox's container carries its run's label, so a container this doesn't find is
    // no sandbox, or one removed already. One lookup tells whose container each id names; one
    // that fails tells none's, so each is then left.
    let filters = (ids.iter()).map(|id| format!("id={}", id.as_str()));
    let shown: Vec<&str> = ids.iter().map(SandboxId::as_str).collect();
    let unfound = format_args!(
        "looking up {} failed, so no container was removed",
        shown.join(", ")
    );
    let found = match labelled(context, filters).context(unfound) {
        Ok(found) => found,
        Err(why) => {
            let failed = ids.iter().map(|id| (id.clone(), why.clone()));
            return Undeleted::of(failed.collect());
        }
    };
    let failures = ids.iter().filter_map(|sandbox| {
        let id = sandbox.as_str();
        let found = found.iter().find(|found| found.id == id)?;
        if found.run != run.as_str() {
            return Some((
                sandbox.clone(),
                format!(
                    "container {id} is another run's sandbox, not run {}'s, so it was left: name \
                     only the run's own, which its list finds",
                    run.as_str()
                ),
            ));
        }
        // A forced removal stops the container, then removes it and its anonymous volumes,
        // and the engine answers once all are done, but for a volume it fails to remove,
        // which it only logs; `sandboxd`, the container's first process, takes every other
        // process of its PID namespace with it as it ends.
        let removed = remove(context, id, Bound::Within(REMOVE_TIMEOUT))
            .map_err(|error| fix_reported(&error));
        let left = format_args!("container {id} is left, as removing it failed");
        let left = removed.context(left).err();
        left.map(|why| (sandbox.clone(), why))
    });
    Undeleted::of(failures.collect())
}

/// Run `run`'s containers, running or not: those whose run label holds its tag. A container
/// stays until it is removed, so one whose `sandboxd` has exited is listed as ended.
fn of_run(context: &ContextName, run: &RunTag) -> Result<Vec<Listed>, String> {
    let filter = format!("label={RUN_LABEL}={}", run.as_str());
    let found = labelled(context, [filter])?;
    let listed = found
        .into_iter()
        .map(|Labelled { id, state, run: _ }| Listed {
            id: SandboxId::from(id),
            ended: ended(&state),
        });
    Ok(listed.collect())
}

/// A container that carries a run label, as `docker container ls` shows it.
struct Labelled {
    id: String,
    /// Its state, as [`ended`] reads it.
    state: String,
    /// The tag its run label holds.
    run: String,
}

/// The containers, running or not, that carry a run label and match `filters` too, each a
/// filter that `docker container ls --filter` takes.
fn labelled(
    context: &ContextName,
    filters: impl IntoIterator<Item = String>,
) -> Result<Vec<Labelled>, String> {
    let format = format!("{{{{.ID}}}} {{{{.State}}}} {{{{.Label \"{RUN_LABEL}\"}}}}");
    let label = format!("label={RUN_LABEL}");
    let ls = [
        "container",
        "ls",
        "--all",
        "--no-trunc",
        "--format",
        &format,
        "--filter",
        &label,
    ];
    let mut ls: Vec<String> = ls.map(str::to_owned).into();
    for filter in filters {
        ls.extend(["--filter".to_owned(), filter]);
    }
    let listed = docker(context, ls).map_err(|error| fix_reported(&error))?;
    let rows = String::from_utf8_lossy(&listed.stdout);
    let found = rows.lines().filter_map(|row| {
        let (id, row) = row.split_once(' ')?;
        let (state, run) = row.split_once(' ')?;
        Some(Labelled {
            id: id.to_owned(),
            state: state.to_owned(),
            run: run.to_owned(),
        })
    });
    Ok(found.collect())
}

/// Whether a container in `state`, as `docker container ls` prints it, has ended: it no longer
/// runs, so its first process, `sandboxd`, has exited.
fn ended(state: &str) -> bool {
    // Of the states Docker's API names, `created`, `running`, `paused` and `restarting` run or
    // will. A state that a later engine adds counts as running too: a start then waits on a
    // sandbox in it until its deadline, rather than failing one that may yet dial, and the list,
    // which consoles and sweeps read too, doesn't fail on it.
    matches!(state, "exited" | "dead" | "removing")
}

/// The last lines of the container's log: those of `sandboxd`'s standard output, then those of
/// its standard error, which `docker logs` writes to its own two streams.
fn log_tail(context: &ContextName, id: &str) -> Result<String, Failure> {
    let logs = ["logs", "--tail", "20", id];
    let Output {
        status: _,
        stdout,
        stderr,
    } = docker(context, logs)?;
    Ok(String::from_utf8_lossy(&[stdout, stderr].concat()).into_owned())
}

/// Fails unless the engine of `context` supports each of `limits`, a name in `docker info`
/// and whether it reports the limit supported. Those are the limits a container's confinement
/// sets (`engine::confinement`), and the CPUs it is confined to, and the engine discards one it
/// doesn't support with only a warning from `docker create`, so a container would start without
/// it.
fn check_limits(context: &ContextName, limits: [(&str, bool); 5]) -> Result<(), String> {
    let unsupported: Vec<&str> = (limits.iter())
        .filter_map(|&(name, supported)| (!supported).then_some(name))
        .collect();
    if unsupported.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the Docker engine of context `{}` can't enforce every limit a sandbox has: \
         `docker info` reports {} false: use an engine that reports each true{}",
        context.as_str(),
        unsupported.join(", "),
        // systemd delegates no `cpuset` to a user's manager by default, so rootless Docker
        // there lacks it.
        if unsupported.contains(&"CPUSet") {
            ", as rootless Docker does once systemd delegates `cpuset` to the user's manager: a \
             `user@.service.d` drop-in with `Delegate=cpu cpuset io memory pids`"
        } else {
            ""
        }
    ))
}
