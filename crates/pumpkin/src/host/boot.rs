//! The controller's boot: each build installed in the controller user's workspace with a
//! [`BOOT_LINK`] runs its own `controller boot`, which reaps the runs that its jobs left, one
//! after another, with their output appended to [`BOOT_LOG`] in the user's home, which `serve`
//! reports the end of.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{ExitCode, Stdio};

use stdx::Context as _;
use stdx::process::Environment;

use crate::job::BOOT_LINK;
use crate::serve::BOOT_LOG;
use crate::workspace;

/// Runs each installed build's boot, as the module says. Returns 1 if one failed, which the log
/// then says, else 0.
pub(crate) fn boot_all() -> Result<ExitCode, String> {
    let home = crate::home("run it as the controller's user, whose home holds the builds")?;
    let installs = home.join(workspace::DIR);
    let entries = match fs::read_dir(&installs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ExitCode::SUCCESS),
        Err(error) => return Err(error).context(installs.display()),
    };
    let mut boots = Vec::new();
    for entry in entries {
        let boot = entry
            .context(installs.display())?
            .path()
            .join("bin")
            .join(BOOT_LINK);
        // A link that leads nowhere is a boot that fails, which the log says.
        match fs::symlink_metadata(&boot) {
            Ok(_) => boots.push(boot),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(boot.display()),
        }
    }
    if boots.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    boots.sort();
    let path = home.join(BOOT_LOG);
    let opened = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path);
    let mut log = opened.context(path.display())?;
    let mut failed = Vec::new();
    for boot in boots {
        let mut command = stdx::process::command(&boot, Environment::Inherited);
        command.args(["controller", "boot"]);
        let streams = log.try_clone().and_then(|out| Ok((out, log.try_clone()?)));
        let (out, err) = streams.context(path.display())?;
        // Unbounded, as each boot bounds its own reaps by their providers' deadlines.
        let ended = stdx::process::spawn(&mut command, Stdio::null(), out.into(), err.into())
            .and_then(|mut child| child.wait().context(boot.display()));
        let why = match ended {
            Ok(status) if status.success() => continue,
            Ok(status) => match status.code() {
                Some(code) => format!("exit {code}"),
                None => status.to_string(),
            },
            Err(error) => error,
        };
        failed.push(format!("{} ({why})", boot.display()));
    }
    if failed.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let failed = failed.join(" ");
    writeln!(log, "controller boot failed for {failed}").context(path.display())?;
    Ok(ExitCode::FAILURE)
}
