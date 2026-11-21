//! The Git repository that xtask checks: its files, by their paths from its root, and the git
//! commands that list and diff them.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::time::Duration;

use stdx::Context as _;
use stdx::process::{self, Bound, Echo, Environment, Run, printed};

use crate::Result;

/// The bound of each command that xtask runs as a step of a check, which takes seconds; one that
/// lasts past it is taken as hung, and killed.
// TigerBeetle's default bound of each command that its scripts run:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L462-L469
pub(crate) const STEP_TIMEOUT: Duration = Duration::from_mins(10);

// Git gets `PATH` alone of this process's environment, and reads no configuration, attributes or
// ignore file but the repository's own, so that a check reads the repository alike on every
// machine: none of the user's, which `HOME` and `XDG_CONFIG_HOME` would find, nor the system's,
// nor a variable of git's own, such as the `GIT_DIR` that `git rebase --exec` sets. As git's own
// tests run it:
// https://github.com/git/git/blob/73876f4861cd3d187a4682290ab75c9dccadbc56/t/test-lib.sh#L516-L517
// https://github.com/git/git/blob/73876f4861cd3d187a4682290ab75c9dccadbc56/t/test-lib.sh#L1493-L1496
const GIT_PINNED: [(&str, &str); 3] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_ATTR_NOSYSTEM", "1"),
];

pub(crate) struct Repo<'a> {
    pub(crate) root: &'a Path,
}

impl Repo<'_> {
    /// What `git` with `args` printed, at the root, as text less its trailing newline; an error
    /// if it failed, saying what git reported.
    pub(crate) fn git<I: AsRef<OsStr>>(&self, args: impl IntoIterator<Item = I>) -> Result<String> {
        let mut git = process::command("git", Environment::Only(&["PATH"]));
        git.current_dir(self.root).args(args).envs(GIT_PINNED);
        let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
        let output = process::run(&mut git, how).map_err(|failure| failure.to_string())?;
        let shown = process::shown(&git);
        Ok(printed(output).context(format_args!("reading what `{shown}` printed failed"))?)
    }

    /// The text of the file at `path`, from the root.
    pub(crate) fn read(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let read = fs::read_to_string(self.root.join(path));
        Ok(read.context(format_args!("reading {} failed", path.display()))?)
    }

    /// Whether a file or a directory is at `path`, from the root.
    pub(crate) fn has(&self, path: impl AsRef<Path>) -> bool {
        self.root.join(path).exists()
    }
}
