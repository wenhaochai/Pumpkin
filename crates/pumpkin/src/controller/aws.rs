//! Finding a controller's EC2 instance by its tag, starting it, waiting until its `sshd` answers
//! at its private address, and enrolling this machine's key with it through EC2 Instance Connect.

use std::fs::{self, DirBuilder, File};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, io, thread};

use aws::{Bound, Cli, Region};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use super::{Name, Reached, SHORT_TIMEOUT, USER, bounded, finished, locked};
use crate::iam::CLIENT;
use crate::say;

// The tag that `deploy/aws` puts on the controller's instance, valued with the controller's name.
const TAG: &str = "pumpkin:controller";

// How long a controller has, from its launch, to run and have its `sshd` answer. An instance that
// EC2 starts does both within a few minutes; past this, something else is wrong.
const START_TIMEOUT: Duration = Duration::from_mins(10);

const START_POLL: Duration = Duration::from_secs(5);

// How long a probe of `sshd` waits to connect. Inside the controller's VPC a connection takes
// milliseconds, so a longer wait would only delay the next probe. SkyPilot's:
// https://github.com/skypilot-org/skypilot/blob/052914aaccb29a9a2364dd73654f2e61f0af42a0/sky/provision/provisioner.py#L500-L501
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservations {
    reservations: Vec<Reservation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservation {
    instances: Vec<Instance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Instance {
    #[serde(rename = "InstanceId")]
    id: String,
    state: State,
    /// When it last started, as RFC 3339.
    launch_time: String,
    private_ip_address: Option<IpAddr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct State {
    name: String,
}

/// Where a controller was found: its instance, and the private address at which its `sshd`
/// answered.
#[derive(Deserialize, Serialize)]
pub(super) struct Located {
    pub(super) instance: String,
    pub(super) address: IpAddr,
}

/// The directory that holds this machine's `ssh` key to its controllers, `key` and `key.pub`,
/// which this makes if there is none, and `known_hosts`, which pins the host key that each
/// controller showed first, by its [`Reached::alias`]. Processes may call it at once.
// A credential, so beside the configuration, outside every workspace; one for the machine, so
// that each controller enrols it once, whatever workspace reaches it.
pub(super) fn kept() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").filter(|home| !home.is_empty()).ok_or(
        "HOME is unset or empty: set it, as this machine's key to its controllers is below it",
    )?;
    let config = Path::new(&home).join(".config/pumpkin");
    let kept = config.join("ssh");
    if kept.join("key.pub").exists() {
        return Ok(kept);
    }
    let made = DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&config);
    made.context(config.display())?;
    // Made apart and moved in whole, so that of the commands that start at once on a new machine,
    // the first to move its key in is the one all of them use; the others' go.
    let mut made = tempfile::Builder::new();
    let made = made
        .prefix(".ssh-")
        .permissions(fs::Permissions::from_mode(0o700));
    let made = made.tempdir_in(&config);
    let made = made.context(config.display())?;
    let mut keygen = stdx::process::command("ssh-keygen", Environment::Only(&["PATH"]));
    keygen.args(["-q", "-t", "ed25519", "-N", "", "-C", "pumpkin", "-f"]);
    bounded(
        keygen.arg(made.path().join("key")),
        "ssh-keygen",
        SHORT_TIMEOUT,
    )?;
    if let Err(error) = fs::rename(made.path(), &kept)
        && !kept.join("key.pub").exists()
    {
        return Err(error).context(kept.display());
    }
    Ok(kept)
}

/// Finds controller `name` in `region` by its tag, starts it if it is stopped, and waits until its
/// `sshd` answers, and returns where it is.
pub(super) fn find(region: &Region, name: &Name) -> Result<Located, String> {
    let cli = Cli::connect(region.clone())?;
    let tag = format!("Name=tag:{TAG},Values={name}");
    let live = |instance: &Instance| {
        !matches!(instance.state.name.as_str(), "shutting-down" | "terminated")
    };
    let mut instances = instances(&cli, &tag)?.into_iter().filter(live);
    let (Some(instance), None) = (instances.next(), instances.next()) else {
        return Err(format!(
            "no one instance in {} carries the tag {TAG}={name}: name the controller that \
             `deploy/aws` made, with the `name` and `region` it was applied with",
            region.as_str()
        ));
    };
    ready(&cli, name, &instance.id)
}

fn instances(cli: &Cli, filter: &str) -> Result<Vec<Instance>, String> {
    let found: Reservations = cli.json(&CLIENT.describe, &["--filters", filter], Bound::Call)?;
    Ok(found
        .reservations
        .into_iter()
        .flat_map(|found| found.instances)
        .collect())
}

fn ready(cli: &Cli, name: &Name, id: &str) -> Result<Located, String> {
    let by = Deadline::after(START_TIMEOUT);
    let mut started = None;
    loop {
        let instance = instance(cli, name, id)?;
        match instance.state.name.as_str() {
            "running" => return answering(name, instance, started),
            "stopped" if started.is_none() => {
                say(format_args!(
                    "starting controller `{name}`, {id}, which is stopped"
                ));
                start(cli, id)?;
                started = Some(Instant::now());
                continue;
            }
            // On its way to running, or to being stopped, which a start then follows.
            "pending" | "stopping" | "stopped" => {}
            state => {
                return Err(format!(
                    "controller `{name}`, {id}, is {state}: start it again once it can be"
                ));
            }
        }
        if by.remaining().is_none() {
            return Err(format!(
                "controller `{name}`, {id}, is {} after {} s: expected it to run by then",
                instance.state.name,
                START_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(START_POLL);
    }
}

/// The state that EC2 gives instance `id` of controller `name`, as it names it, such as `running`
/// or `stopped`.
pub(super) fn state(cli: &Cli, name: &Name, id: &str) -> Result<String, String> {
    instance(cli, name, id).map(|instance| instance.state.name)
}

/// Starts instance `id`, as `start-instances` does: one that runs or is starting is left so.
pub(super) fn start(cli: &Cli, id: &str) -> Result<(), String> {
    let start = ["--instance-ids", id];
    cli.run(&CLIENT.start, &start, Bound::Call).map(drop)
}

fn instance(cli: &Cli, name: &Name, id: &str) -> Result<Instance, String> {
    let found = instances(cli, &format!("Name=instance-id,Values={id}"))?;
    found.into_iter().next().ok_or_else(|| {
        format!("controller `{name}`, {id}, is no longer there: run the command again")
    })
}

/// Waits until the `sshd` of `instance`, which runs, answers at its private address, until
/// `START_TIMEOUT` after its launch, and says how long that took if this command `started` it.
fn answering(name: &Name, instance: Instance, started: Option<Instant>) -> Result<Located, String> {
    let Instance {
        id,
        state: _,
        launch_time,
        private_ip_address,
    } = instance;
    let Some(address) = private_ip_address else {
        return Err(format!(
            "controller `{name}`, {id}, runs with no private address: expected the one of its \
             subnet that `deploy/aws` gives it"
        ));
    };
    let launched: Timestamp = launch_time.parse().map_err(|error| {
        format!(
            "the launch time that `aws ec2 describe-instances` printed, `{launch_time}`, isn't \
             a time: {error}"
        )
    })?;
    let launched = Duration::try_from(launched.as_duration()).unwrap_or_default();
    let by = Deadline::at_unix(launched.saturating_add(START_TIMEOUT));
    let waiting = Instant::now();
    loop {
        let Err(error) = answers(address) else {
            if let Some(started) = started {
                say(format_args!(
                    "controller `{name}` answers, after {} s",
                    started.elapsed().as_secs()
                ));
            }
            return Ok(Located {
                instance: id,
                address,
            });
        };
        if by.remaining().is_none() {
            return Err(format!(
                "controller `{name}`, {id}, didn't answer SSH at its private address, {address}, \
                 in the {} s it was waited for: {error}: {}",
                waiting.elapsed().as_secs(),
                unanswered(&error)
            ));
        }
        thread::sleep(START_POLL);
    }
}

/// Why `sshd` of a controller that runs didn't answer a probe ([`answers`]) that failed with
/// `error`, and what to do.
pub(super) fn unanswered(error: &io::Error) -> &'static str {
    match error.kind() {
        // A security group drops what it doesn't admit, so a refusal is the instance's.
        io::ErrorKind::ConnectionRefused => {
            "nothing listens there, as when the controller is stopping though EC2 says it runs, \
             or its `sshd` is down: run the command again shortly"
        }
        io::ErrorKind::TimedOut
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable => {
            "this machine doesn't reach the controller's VPC, or `deploy/aws`'s `ssh_from` doesn't \
             admit it: apply it with this machine's address among them"
        }
        _ => "run the command again",
    }
}

/// Whether a connection to `sshd`'s port at `address` opens within `PROBE_TIMEOUT`, or why not.
// A connection alone: a booting controller's `sshd` takes one before it admits logins, and one
// past `MaxStartups` sends no banner, "Not allowed at this time" in its place, so a login still has
// to be tried, as SkyPilot tries one after its probe:
// https://github.com/skypilot-org/skypilot/blob/052914aaccb29a9a2364dd73654f2e61f0af42a0/sky/provision/provisioner.py#L478-L515
// https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/sshd.c#L589-L639
pub(super) fn answers(address: IpAddr) -> io::Result<()> {
    // The port of `sshd`, which `deploy/aws` admits.
    TcpStream::connect_timeout(&SocketAddr::new(address, 22), PROBE_TIMEOUT).map(drop)
}

impl Reached {
    /// This machine's lock of enrolling its key, which one command holds at a time, so that
    /// commands that find the key refused at once push it once and add it once.
    // Beside the key, as one machine-wide lock guards SkyPilot's registration of its key:
    // https://github.com/skypilot-org/skypilot/blob/052914aaccb29a9a2364dd73654f2e61f0af42a0/sky/authentication.py#L236-L243
    pub(super) fn enrolling(&self) -> Result<File, String> {
        let waiting = format_args!(
            "waiting for another command, which is enrolling this machine's key with a controller"
        );
        locked(&self.kept.join("enrol"), waiting)
    }

    pub(super) fn enrol(&self, at: &Located) -> Result<(), String> {
        let public = self.kept.join("key.pub");
        let public = fs::read(&public).context(public.display())?;
        let mut ssh = self.ssh(
            at,
            "umask 077 && mkdir --parents .ssh && cat >> .ssh/authorized_keys",
        );
        let fed = stdx::process::fed(&mut ssh, &public, Deadline::after(SHORT_TIMEOUT));
        finished(fed, "ssh", SHORT_TIMEOUT).map(|_| ())
    }

    /// Pushes this machine's public key to the instance with EC2 Instance Connect, which accepts it
    /// for a minute: the connection that uses it must begin within.
    pub(super) fn push(&self, at: &Located) -> Result<(), String> {
        let public = format!("file://{}", self.kept.join("key.pub").display());
        let args = [
            "--instance-id",
            &at.instance,
            "--instance-os-user",
            USER,
            "--ssh-public-key",
            &public,
        ];
        let cli = Cli::connect(self.region.clone())?;
        cli.run(&CLIENT.push_key, &args, Bound::Call).map(|_| ())
    }
}
