//! The controller host's own software, which `pumpkin controller install` installs as root, from
//! the release that its admin pins: the user that runs every Pumpkin process there, lingering, so
//! that its job units outlive every SSH session; this `pumpkin`, at [`PUMPKIN`], which the units
//! run; the units that reap at boot, stop the controller when idle and copy the members' keys
//! (`members`); and `sshd`'s settings, which admit only those keys for the user.
//!
//! What the host's provisioner installs first, the OS, Docker and the other packages that jobs
//! run, is left to it: the install fails, naming it, where it finds one missing. The install
//! converges: it writes each file whole, only where it differs, says each change, starts or
//! reloads only what changed, and says nothing when nothing did, so running it again changes
//! nothing.
// As k3s's install keeps a binary whose hash matches and starts its unit again only if the
// binary, the unit or its environment changed:
// https://github.com/k3s-io/k3s/blob/9e97a3b4aac734441c3ebabe85689303cf35d9a0/install.sh#L559-L581
// https://github.com/k3s-io/k3s/blob/9e97a3b4aac734441c3ebabe85689303cf35d9a0/install.sh#L903-L907
// and as `teleport install systemd` runs the binary that renders the unit, by default:
// https://github.com/gravitational/teleport/blob/1283425b60ec5f60d509ba4c791183d452923ff7/lib/config/systemd/systemd.go#L76-L84

mod boot;
mod idle;
mod members;

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use aws::Region;
pub(crate) use boot::boot_all;
pub(crate) use idle::idle;
pub(crate) use members::members;
use stdx::Context as _;
use stdx::process::{Bound, Echo, Environment, Failure, Run};

use crate::controller::{Name, USER};
use crate::{say, whole};

/// Where the install puts the `pumpkin` that runs it, which each unit runs.
const PUMPKIN: &str = "/usr/local/sbin/pumpkin";

// The login shell of `USER`, through which `sshd` runs each forced command: one that reads no
// startup file of the user's for `-c`, so that a file that a job writes in the home runs before
// no member's command.
const SHELL: &str = "/bin/sh";

// The group of the Docker engine, which Docker's package makes, and whose members the engine
// serves as root.
const DOCKER_GROUP: &str = "docker";

const UNITS_DIR: &str = "/etc/systemd/system";

const BOOT_UNIT: &str = crate::serve::BOOT_UNIT;

const IDLE_TIMER: &str = "pumpkin-idle.timer";

const MEMBERS_TIMER: &str = "pumpkin-members.timer";

// Each unit, by name, as its file holds it before `fill`.
const UNITS: [(&str, &str); 5] = [
    (BOOT_UNIT, include_str!("host/pumpkin-boot.service")),
    (
        "pumpkin-idle.service",
        include_str!("host/pumpkin-idle.service"),
    ),
    (IDLE_TIMER, include_str!("host/pumpkin-idle.timer")),
    (
        "pumpkin-members.service",
        include_str!("host/pumpkin-members.service"),
    ),
    (MEMBERS_TIMER, include_str!("host/pumpkin-members.timer")),
];

// The units that are enabled, which the others are started by.
const ENABLED: [&str; 3] = [BOOT_UNIT, IDLE_TIMER, MEMBERS_TIMER];

// The enabled units that are started once enabled, as the others start at boot.
const STARTED: [&str; 2] = [IDLE_TIMER, MEMBERS_TIMER];

const SSHD_DIR: &str = "/etc/ssh/sshd_config.d";

// Each of `sshd`'s drop-ins, by name, as its file holds it before `fill`.
const SSHD: [(&str, &str); 2] = [
    (
        "00-pumpkin-keepalive.conf",
        include_str!("host/00-pumpkin-keepalive.conf"),
    ),
    (
        "10-pumpkin-members.conf",
        include_str!("host/10-pumpkin-members.conf"),
    ),
];

// What older installs wrote that this one doesn't, which it removes.
const RETIRED: [&str; 2] = [
    "/usr/local/bin/pumpkin-boot",
    "/usr/local/sbin/pumpkin-idle",
];

// The bound of each program that the install runs, which systemd answers or acts for: past the
// 90 s by which systemd fails a unit's start by default, which a `systemctl` that waits for it
// meets first.
const SYSTEM_TIMEOUT: Duration = Duration::from_mins(2);

/// Installs the software of the host of controller `name`, in `region`, as the module says, and
/// returns 0.
///
/// # Errors
///
/// It doesn't run as root, the provisioner's part is missing, or a step fails; the steps before it
/// stay done. The members' keys are copied before `sshd` is set to admit only them, so that a copy
/// that fails leaves `sshd` as it was.
pub(crate) fn install(name: &Name, region: &Region) -> Result<ExitCode, String> {
    if !rustix::process::geteuid().is_root() {
        return Err(
            "this `pumpkin controller install` doesn't run as root: run it as root, as with sudo"
                .to_owned(),
        );
    }
    user()?;
    let exe = crate::workspace::exe("expected the `pumpkin` to install")?;
    let bytes = fs::read(&exe).context(exe.display())?;
    wrote(Path::new(PUMPKIN), &bytes, 0o755)?;
    for retired in RETIRED {
        match fs::remove_file(retired) {
            Ok(()) => say(format_args!("removed {retired}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(retired),
        }
    }
    let mut changed = false;
    for (file, text) in UNITS {
        changed |= wrote(
            &Path::new(UNITS_DIR).join(file),
            fill(text, name, region).as_bytes(),
            0o644,
        )?;
    }
    units(changed)?;
    members::copy(name, region)?;
    let mut sshd = false;
    for (file, text) in SSHD {
        let filled = fill(text, name, region);
        sshd |= wrote(&Path::new(SSHD_DIR).join(file), filled.as_bytes(), 0o644)?;
    }
    if sshd {
        // Ubuntu's `ssh.service` checks the configuration before its reload signals `sshd`, and
        // the next connection starts an `sshd` that isn't running, as `ssh.socket` does.
        systemctl(&["try-reload-or-restart", "ssh.service"])?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `text` with each placeholder replaced by its value, each of which a unit's command line and
/// `sshd`'s configuration read as one word.
fn fill(text: &str, name: &Name, region: &Region) -> String {
    (text.replace("@PUMPKIN@", PUMPKIN).replace("@USER@", USER))
        .replace("@MEMBERS@", members::MEMBERS)
        .replace("@NAME@", &name.to_string())
        .replace("@REGION@", region.as_str())
}

/// Writes `bytes` to `path` with permission bits `mode`, as [`whole::converge`] does, and says so
/// if it did. True if it did.
pub(crate) fn wrote(path: &Path, bytes: &[u8], mode: u32) -> Result<bool, String> {
    let wrote = whole::converge(path, bytes, mode)?;
    if wrote {
        say(format_args!("wrote {}", path.display()));
    }
    Ok(wrote)
}

/// Adds `USER`, with its shell, in Docker's group, and makes it linger, each unless it is so.
fn user() -> Result<(), String> {
    let docker = getent("group", DOCKER_GROUP)?.ok_or_else(|| {
        format!(
            "the group `{DOCKER_GROUP}` is missing: install Docker first, whose package makes it"
        )
    })?;
    match getent("passwd", USER)? {
        None => {
            let groups = format!("--groups={DOCKER_GROUP}");
            let shell = format!("--shell={SHELL}");
            let args = ["--create-home", "--user-group", &shell, &groups, USER];
            ran("useradd", &args)?;
            say(format_args!("added the user {USER}"));
        }
        Some(entry) => {
            // name:password:UID:GID:GECOS:home:shell, as passwd(5) says.
            if entry.split(':').nth(6) != Some(SHELL) {
                ran("usermod", &[&format!("--shell={SHELL}"), USER])?;
                say(format_args!("made {SHELL} the shell of {USER}"));
            }
            // name:password:GID:members, the members split by commas, as group(5) says.
            let members = docker.split(':').nth(3).unwrap_or_default();
            if !members.split(',').any(|member| member == USER) {
                ran(
                    "usermod",
                    &["--append", &format!("--groups={DOCKER_GROUP}"), USER],
                )?;
                say(format_args!("added {USER} to the group {DOCKER_GROUP}"));
            }
        }
    }
    // The file whose presence is a user's lingering, which `loginctl enable-linger` makes.
    let linger = Path::new("/var/lib/systemd/linger").join(USER);
    if !linger.try_exists().context(linger.display())? {
        ran("loginctl", &["enable-linger", USER])?;
        say(format_args!("made {USER} linger"));
    }
    Ok(())
}

/// The entry that `getent` gives for `key` in `database`, or `None` if it has none.
fn getent(database: &str, key: &str) -> Result<Option<String>, String> {
    match run(command("getent", &[database, key])) {
        Ok(output) => stdx::process::printed(output).map(Some),
        // The exit status of a key that the database lacks, as getent(1) says.
        Err(Failure::Exited { output, .. }) if output.status.code() == Some(2) => Ok(None),
        Err(failure) => Err(failure.to_string()),
    }
}

/// Reloads systemd's units if any changed, and enables and starts those to be, unless they are.
// A reload sets a waiting timer by its new file, so a timer that changed needs no restart:
// https://github.com/systemd/systemd/blob/e8dc52766e1fdb4f8c09c3ab654d1270e1090c8d/src/core/timer.c#L306-L321
fn units(changed: bool) -> Result<(), String> {
    if changed {
        systemctl(&["daemon-reload"])?;
    }
    let asked = [&["is-enabled", "--quiet"], &ENABLED[..]].concat();
    match run(command("systemctl", &asked)) {
        Ok(_) => return Ok(()),
        // Not 0, as some unit isn't enabled.
        Err(Failure::Exited { .. }) => {}
        Err(failure) => return Err(failure.to_string()),
    }
    systemctl(&[&["enable"], &ENABLED[..]].concat())?;
    systemctl(&[&["start"], &STARTED[..]].concat())?;
    say(format_args!("enabled {}", ENABLED.join(" ")));
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    ran("systemctl", args)
}

/// Runs `program` with `args` to its end, as [`run`] does, failing unless it exits with 0.
fn ran(program: &str, args: &[&str]) -> Result<(), String> {
    run(command(program, args))
        .map(drop)
        .map_err(|failure| failure.to_string())
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut command = stdx::process::command(program, Environment::Only(&["PATH"]));
    command.args(args);
    command
}

/// Runs `command`, silently, within [`SYSTEM_TIMEOUT`], with both its outputs captured.
fn run(mut command: Command) -> Result<std::process::Output, Failure> {
    stdx::process::run(
        &mut command,
        Run::new(Bound::Within(SYSTEM_TIMEOUT), Echo::Silent),
    )
}
