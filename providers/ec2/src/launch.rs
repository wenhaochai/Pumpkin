use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;
use std::{fmt, process, thread};

use aws::Bound;
use engine::confinement;
use jiff::Timestamp;
use provider::{
    Console, Create, Created, Listed, Role, RunTag, SandboxId, Sweep, Undeleted, UnixTime,
};
use schema::ec2_boot::{Start, boot_lines};
use schema::ir::Name;
use schema::sandbox::TcpDial;
use serde::Deserialize;
use serde_json::{Value, json};
use stdx::{Context as _, Deadline};

use crate::iam::CALLS;
use crate::user_data::user_data;
use crate::{Ec2, InstanceClass, Reached};

// The tag by which a run finds its instances, and a delete knows an instance is the run's.
pub(crate) const RUN_TAG: &str = "pumpkin:run";

const ROLE_TAG: &str = "pumpkin:role";

// EC2's API is eventually consistent, so an instance just launched may be missing from a describe
// for a while, and one still missing minutes later is lost, not late:
// https://docs.aws.amazon.com/ec2/latest/devguide/eventual-consistency.html
const LISTED_TIMEOUT: Duration = Duration::from_secs(200);

// An instance ends some time after it is told to: its OS shuts down, then EC2 frees its volume
// and network interface, which the envelope's templates delete with it. That takes seconds, or
// minutes for a slow shutdown; one still not terminated after 10 min is stuck, not slow.
const TERMINATED_TIMEOUT: Duration = Duration::from_mins(10);

const DESCRIBE_EVERY: Duration = Duration::from_secs(5);

impl Ec2<'_> {
    /// Creates `create`'s sandbox on an instance of its own, with its timer, and returns once its
    /// run's list finds it.
    ///
    /// # Errors
    ///
    /// An instance that was launched when it fails is left, which the error names or the run's
    /// list finds by its tag.
    pub(crate) fn create_instance(
        &self,
        create: &Create,
        dial: TcpDial,
    ) -> Result<Created, String> {
        let Create {
            run,
            name,
            role,
            // Each sandbox has an instance of its own, whichever instance of its role it is.
            instance: _,
            class,
            image,
            sandboxd,
            ops,
            model,
            hard_end,
            by,
        } = create;
        let TcpDial { gateway, grant } = dial;
        let SocketAddr::V4(gateway) = gateway else {
            // Docker's default bridge, which the container is on, has no IPv6.
            return Err(format!(
                "an EC2 sandbox's container dials the gateway at an IPv4 address on the \
                 envelope's network: expected one, not {gateway}"
            ));
        };
        let Role {
            name: role_name,
            user,
            resources,
            instances: _,
        } = role;
        let class = (self.classes.get(class))
            .ok_or_else(|| format!("class `{class}` is none of the envelope's"))?;
        let hard_end = HardEnd::of(*hard_end);
        // Whole seconds, rounded down, so that neither is later than the time it states.
        let start = Start {
            hard_end_unix_seconds: hard_end.0,
            // The create's own deadline: should the pull end too late for `sandboxd` to meet the
            // gateway by then, the run has stopped waiting, and the gateway refuses it.
            pulls_by_unix_seconds: by.since_epoch().as_secs(),
            role_image: image.as_str().to_owned(),
            sandboxd_image: sandboxd.as_str().to_owned(),
            confinement: confinement(*user, *resources),
            gateway,
            grant,
            ops: *ops,
            model: *model,
        };
        let by = Deadline::at_unix(by.since_epoch());
        // Each step has the one before it done: the timer bounds an instance as soon as it exists,
        // and the create returns only an instance that its run's list finds, as a sweep needs.
        let user_data = user_data(&start, self.repositories.roles.registry())?;
        let tags = tag_specifications(run, name, role_name);
        let id = self.launch(class, run, name, &tags, &user_data, by)?;
        self.set_timer(run, &id, hard_end, by)?;
        self.listed(run, &id, by)?;
        Ok(Created {
            id: SandboxId::from(id.0),
        })
    }

    fn launch(
        &self,
        class: &InstanceClass,
        run: &RunTag,
        name: &Name,
        tags: &str,
        user_data: &str,
        by: Deadline,
    ) -> Result<InstanceId, String> {
        // The user data holds the grant, so it reaches `aws` in a file only this user can read,
        // never on its command line, which every process can read. Neither a run's tag nor a name
        // holds a `+`, so no other launch of this process names the file.
        let file = self.dir.join(format!(
            "pumpkin-user-data-{}-{}+{name}",
            process::id(),
            run.as_str()
        ));
        let uri = format!("file://{}", file.display());
        let launch_template = format!(
            "LaunchTemplateId={},Version={}",
            class.template.0, class.version
        );
        // No client token: the run makes one create of each sandbox, and `aws` gives each call a
        // token of its own, which keeps its own retries of the call from launching twice:
        // https://docs.aws.amazon.com/cli/latest/reference/ec2/run-instances.html
        let args = [
            "--launch-template",
            &launch_template,
            // A shutdown, as the boot makes at its end and at the hard end, terminates the
            // instance, whatever its template says, so that nothing of it runs on, or is billed.
            "--instance-initiated-shutdown-behavior",
            "terminate",
            "--tag-specifications",
            tags,
            "--user-data",
            &uri,
        ];
        let launched = write_private(&self.dir, &file, user_data).and_then(|()| {
            (self.reached.cli).json::<Launched>(&CALLS.launch, &args, Bound::Create(by))
        });
        // A file left behind holds a grant that only this user can read, and that expires with its
        // registration.
        let _ = fs::remove_file(&file);
        let Launched {
            instances: [LaunchedInstance { instance_id }],
        } = launched?;
        Ok(instance_id)
    }

    fn set_timer(
        &self,
        run: &RunTag,
        id: &InstanceId,
        hard_end: HardEnd,
        by: Deadline,
    ) -> Result<(), String> {
        let (group, role_arn) = (&self.reached.group, &self.timer_role);
        let name = timer_name(run, id);
        // In UTC, as `--schedule-expression-timezone` says.
        let timer = Timestamp::UNIX_EPOCH.saturating_add(Duration::from_secs(hard_end.timer()));
        let at = timer
            .unwrap_or(Timestamp::MAX)
            .strftime("at(%Y-%m-%dT%H:%M:%S)")
            .to_string();
        let target = json!({
            // EventBridge Scheduler's universal target, which makes the call that its ARN names,
            // with the input as its parameters:
            // https://docs.aws.amazon.com/scheduler/latest/UserGuide/managing-targets-universal.html
            "Arn": format!("arn:{}:scheduler:::aws-sdk:ec2:terminateInstances", role_arn.partition()),
            "RoleArn": role_arn.as_str(),
            "Input": json!({ "InstanceIds": [id.0] }).to_string(),
        });
        let target = target.to_string();
        let args = [
            "--group-name",
            &group.0,
            "--name",
            &name,
            "--schedule-expression",
            &at,
            "--schedule-expression-timezone",
            "UTC",
            "--flexible-time-window",
            "Mode=OFF",
            "--action-after-completion",
            "DELETE",
            "--target",
            &target,
        ];
        let made = (self.reached.cli)
            .run(&CALLS.set_timer, &args, Bound::Create(by))
            .map(drop);
        made.context(format_args!(
            "making instance `{id}`'s timer, schedule `{}/{name}`, failed",
            group.0
        ))
    }

    // A run's list must find each sandbox from when its create returns, and the run takes one that
    // a list misses for ended (`Sweep::list`), so the create returns only once a list has found
    // the instance; one that has ended already is found, and so seen to have ended.
    fn listed(&self, run: &RunTag, id: &InstanceId, by: Deadline) -> Result<(), String> {
        let listing = Deadline::after(LISTED_TIMEOUT);
        let filters = [run_filter(run), ids_filter([id])];
        loop {
            if !(self.reached.described(&filters, Bound::Create(by))?).is_empty() {
                return Ok(());
            }
            let Some(left) = listing.remaining() else {
                return Err(format!(
                    "run `{}`'s list didn't find instance `{id}` within {} s of its launch: \
                     expected EC2 to list it by then",
                    run.as_str(),
                    LISTED_TIMEOUT.as_secs()
                ));
            };
            // Past `by`, the next list fails at once, saying so.
            let before_by = by.remaining().unwrap_or(Duration::ZERO);
            thread::sleep(left.min(before_by).min(DESCRIBE_EVERY));
        }
    }

    pub(crate) fn read_console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let not_ours = || {
            format!(
                "`{}` is no sandbox of run `{}`: name one that the run's list finds",
                id.as_str().escape_debug(),
                run.as_str()
            )
        };
        let Ok(id) = InstanceId::try_from(id.as_str().to_owned()) else {
            return Err(not_ours());
        };
        // By ID alone, so that one terminated, which the run's list leaves out, is found too, as a
        // start that has ended reads its console; and kept to the run by its tag.
        let found = (self.reached).described(&[ids_filter([&id])], Bound::Call)?;
        if !(found.iter()).any(|found| found.instance_id == id && found.is_of(run)) {
            return Err(not_ours());
        }
        let read = ["--instance-id", &id.0, "--latest"];
        let ConsoleOutput { output } =
            (self.reached.cli).json(&CALLS.console, &read, Bound::Call)?;
        let output = output.unwrap_or_default();
        // The boot's own lines alone, which tell of the sandbox: the kernel's, cloud-init's and the
        // shutdown's don't.
        let text = boot_lines(&output).flat_map(|line| [line, "\n"]).collect();
        Ok(Console { text })
    }
}

impl Sweep for Reached {
    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        let found = self.described(&[run_filter(run)], Bound::Call)?;
        let listed = found.into_iter().filter_map(|found| {
            let ListedInstance {
                instance_id,
                state: InstanceState { name },
                tags: _,
            } = found;
            // The boot runs once, and a later boot starts no container, so an instance that is
            // ending, or has stopped, never serves its sandbox again. One terminated is freed, and
            // is no sandbox to list.
            let ended = match name {
                State::Pending | State::Running => false,
                State::ShuttingDown | State::Stopping | State::Stopped => true,
                State::Terminated => return None,
            };
            let id = SandboxId::from(instance_id.0);
            Some(Listed { id, ended })
        });
        Ok(listed.collect())
    }

    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        // An id that isn't an instance's names none, and the filter takes only IDs.
        let asked = ids
            .iter()
            .map(|id| InstanceId::try_from(id.as_str().to_owned()));
        let asked: BTreeSet<InstanceId> = asked.filter_map(Result::ok).collect();
        if asked.is_empty() {
            return Ok(());
        }
        let unfound = format_args!(
            "looking up {} failed, so no instance was terminated",
            shown(&asked)
        );
        // By ID alone, so that another run's instance is found, and left, by its tag.
        let found = (self.described(&[ids_filter(&asked)], Bound::Call)).context(unfound);
        let found = match found {
            Ok(found) => found,
            Err(why) => {
                let failed = asked.iter().map(|id| (id.sandbox(), why.clone()));
                return Undeleted::of(failed.collect());
            }
        };
        let (mut ending, mut failures) = (BTreeSet::new(), Vec::new());
        // The instances whose timers are deleted: each terminated, and each that EC2 no longer
        // describes, which counts as deleted, as EC2 describes an instance only for a short while
        // after it is terminated, and each id a run deletes was found by a describe once already.
        // The timer of one that may still run is kept, to end it all the same:
        // https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-instance-lifecycle.html
        let mut ended: BTreeSet<&InstanceId> = (asked.iter())
            .filter(|id| !found.iter().any(|found| found.instance_id == **id))
            .collect();
        for found in &found {
            let id = &found.instance_id;
            match found.state.name {
                // Ahead of the tag's check, as EC2 takes a terminated instance's tags off some
                // while after, and one terminated is freed, whichever run's it was:
                // https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/Using_Tags.html
                State::Terminated => {
                    ended.insert(id);
                }
                _ if !found.is_of(run) => failures.push((
                    id.sandbox(),
                    format!(
                        "instance `{id}` is no sandbox of run `{}`, so it was left: name only the \
                         run's own, which its list finds",
                        run.as_str()
                    ),
                )),
                State::ShuttingDown => {
                    ending.insert(id);
                }
                State::Pending | State::Running | State::Stopping | State::Stopped => {
                    // Each on its own, as EC2 terminates none of a call's instances if it refuses
                    // one:
                    // https://docs.aws.amazon.com/cli/latest/reference/ec2/terminate-instances.html
                    match self.terminate(id, Bound::Call) {
                        Ok(()) => {
                            ending.insert(id);
                        }
                        Err(left) => failures.push((id.sandbox(), left)),
                    }
                }
            }
        }
        match self.terminated(ending.clone()) {
            Ok(()) => ended.extend(ending),
            Err((left, why)) => {
                failures.extend(left.iter().map(|id| (id.sandbox(), why.clone())));
                ended.extend(ending.difference(&left));
            }
        }
        let untimed = ended.into_iter().map(|id| {
            let unset = self.unset_timer(run, id);
            unset.map_err(|why| (id.sandbox(), why))
        });
        failures.extend(untimed.filter_map(Result::err));
        Undeleted::of(failures)
    }
}

impl Reached {
    // A timer gone already, as one that fired, counts as deleted.
    fn unset_timer(&self, run: &RunTag, id: &InstanceId) -> Result<(), String> {
        let group = &self.group.0;
        let name = timer_name(run, id);
        let args = ["--group-name", group, "--name", &name];
        (self
            .cli
            .run_found(&CALLS.unset_timer, &args, Bound::Call)
            .map(drop))
        .context(format_args!(
            "deleting terminated instance `{id}`'s timer, schedule `{group}/{name}`, failed"
        ))
    }

    // Fails with the ids it still waits on, each of which may still run.
    fn terminated<'i>(
        &self,
        mut ids: BTreeSet<&'i InstanceId>,
    ) -> Result<(), (BTreeSet<&'i InstanceId>, String)> {
        let by = Deadline::after(TERMINATED_TIMEOUT);
        while !ids.is_empty() {
            let waiting = format_args!("waiting for {} to be terminated failed", shown(&ids));
            let found =
                (self.described(&[ids_filter(ids.iter().copied())], Bound::Call)).context(waiting);
            let found = match found {
                Ok(found) => found,
                Err(why) => return Err((ids, why)),
            };
            let unterminated: BTreeSet<&InstanceId> = (found.iter())
                .filter(|found| !matches!(found.state.name, State::Terminated))
                .map(|found| &found.instance_id)
                .collect();
            // One that EC2 no longer describes counts as terminated, as one a delete never found
            // counts as deleted.
            ids.retain(|id| unterminated.contains(id));
            if ids.is_empty() {
                break;
            }
            let Some(left) = by.remaining() else {
                let why = format!(
                    "EC2 didn't describe {} as terminated within {} s of the delete's \
                     terminations: expected it to end each by then, and each may still run",
                    shown(&ids),
                    TERMINATED_TIMEOUT.as_secs()
                );
                return Err((ids, why));
            };
            thread::sleep(left.min(DESCRIBE_EVERY));
        }
        Ok(())
    }

    fn described(&self, filters: &[String], bound: Bound) -> Result<Vec<ListedInstance>, String> {
        let mut args = vec!["--filters"];
        args.extend(filters.iter().map(String::as_str));
        let Reservations { reservations } = self.cli.json(&CALLS.describe, &args, bound)?;
        let instances = reservations.into_iter().flat_map(|found| found.instances);
        Ok(instances.collect())
    }

    fn terminate(&self, id: &InstanceId, bound: Bound) -> Result<(), String> {
        let terminate = ["--instance-ids", &id.0];
        (self.cli.run(&CALLS.terminate, &terminate, bound).map(drop)).context(format_args!(
            "instance `{id}` is left, as terminating it failed"
        ))
    }
}

// `<run>.<id>`, which only that instance's timer has, as EC2 never reuses an ID.
fn timer_name(run: &RunTag, id: &InstanceId) -> String {
    format!("{}.{id}", run.as_str())
}

#[derive(Clone, Copy)]
// The run's hard end, in whole seconds since the epoch.
struct HardEnd(u64);

impl HardEnd {
    fn of(hard_end: UnixTime) -> Self {
        Self(hard_end.since_epoch().as_secs())
    }

    // The hard end rounded up to the minute. EventBridge Scheduler invokes a target within the
    // minute its time is in, so a time on the minute after the hard end fires only past it, after
    // the boot's own timer, which a healthy instance ends by first:
    // https://docs.aws.amazon.com/scheduler/latest/UserGuide/schedule-types.html
    fn timer(self) -> u64 {
        self.0.div_ceil(60) * 60
    }
}

fn run_filter(run: &RunTag) -> String {
    format!("Name=tag:{RUN_TAG},Values={}", run.as_str())
}

// Unlike `--instance-ids`, a filter doesn't fail on an ID of no instance.
fn ids_filter<'a>(ids: impl IntoIterator<Item = &'a InstanceId>) -> String {
    let ids: Vec<&str> = ids.into_iter().map(|id| id.0.as_str()).collect();
    format!("Name=instance-id,Values={}", ids.join(","))
}

fn shown(ids: &BTreeSet<impl fmt::Display>) -> String {
    let shown: Vec<String> = ids.iter().map(|id| format!("`{id}`")).collect();
    shown.join(", ")
}

// Checked so that an ID can neither carry a `,` into a filter's values nor read as an option.
#[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "String")]
struct InstanceId(String);

impl TryFrom<String> for InstanceId {
    type Error = String;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        let digits = id
            .strip_prefix("i-")
            .filter(|digits| matches!(digits.len(), 8 | 17));
        if !digits.is_some_and(|digits| digits.bytes().all(hex)) {
            return Err(format!(
                "`{}` is not an instance's ID: use one such as `i-0123456789abcdef0`",
                id.escape_debug()
            ));
        }
        Ok(Self(id))
    }
}

impl InstanceId {
    fn sandbox(&self) -> SandboxId {
        SandboxId::from(self.0.clone())
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Launched {
    instances: [LaunchedInstance; 1],
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchedInstance {
    instance_id: InstanceId,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservations {
    reservations: Vec<Reservation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservation {
    instances: Vec<ListedInstance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedInstance {
    instance_id: InstanceId,
    state: InstanceState,
    // EC2 leaves it out of the description of an instance that has none.
    #[serde(default)]
    tags: Vec<Tag>,
}

impl ListedInstance {
    fn is_of(&self, run: &RunTag) -> bool {
        (self.tags.iter()).any(|Tag { key, value }| key == RUN_TAG && value == run.as_str())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Tag {
    key: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InstanceState {
    name: State,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    Pending,
    Running,
    ShuttingDown,
    Terminated,
    Stopping,
    Stopped,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConsoleOutput {
    // EC2 sends it in base64 and `aws` decodes it; there is none if EC2 has none.
    output: Option<String>,
}

fn tag_specifications(run: &RunTag, name: &Name, role: &Name) -> String {
    // `<run>.<name>`, what EC2's console shows of the instance: which sandbox of which run it is,
    // read back as both, as a name holds no `.`.
    let named = format!("{}.{name}", run.as_str());
    let tags = [
        (RUN_TAG, run.as_str()),
        (ROLE_TAG, role.as_str()),
        ("Name", &named),
    ];
    let tags: Vec<Value> = (tags.iter())
        .map(|(key, value)| json!({ "Key": key, "Value": value }))
        .collect();
    let specifications = json!([
        { "ResourceType": "instance", "Tags": tags },
        { "ResourceType": "volume", "Tags": tags },
    ]);
    specifications.to_string()
}

fn write_private(dir: &Path, path: &Path, text: &str) -> Result<(), String> {
    let opened = fs::create_dir_all(dir)
        .and_then(|()| (OpenOptions::new().write(true).create_new(true).mode(0o600)).open(path));
    let written = opened.and_then(|mut file| file.write_all(text.as_bytes()));
    written.map_err(|error| {
        format!(
            "the user data's file {} can't be written: {error}: expected its directory to take a \
             new file of this user's",
            path.display()
        )
    })
}
