//! Why a command failed, in the cases that callers branch on. A failure's message says what went
//! wrong, then what was expected or what to fix; what the failure means to the caller's operation
//! is the caller's to add.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fmt, fs, io};

use super::{Bound, Stream};

// How much of the end of a failed command's standard error its failure reports: the end, where a
// program, such as Python with its traceback, says what failed last.
const REPORTED_BYTES: usize = 4096;

/// Why a [`run`](super::run) failed. Each case names the command as [`Run::named`] says.
///
/// [`Run::named`]: super::Run::named
#[derive(Debug)]
pub enum Failure {
    /// It didn't start: `expected` is what its message says was expected instead, such as the
    /// program on `PATH`.
    Unstarted {
        name: String,
        error: io::Error,
        expected: String,
    },
    /// It started but couldn't be followed to its end, as when its output couldn't be read, or
    /// passed what a run captures, and it was killed if it hadn't ended.
    Unwatched { name: String, error: io::Error },
    /// It hadn't ended at `bound`, and was killed then if it had `started`; else the bound had
    /// passed before it could start.
    Unended {
        name: String,
        bound: Bound,
        started: bool,
    },
    /// It exited with a status other than 0, or a signal ended it: `output` holds what it wrote to
    /// each stream that was captured, and `stderr` says where its standard error went.
    Exited {
        name: String,
        output: Output,
        stderr: Stream,
    },
}

impl Failure {
    /// The failure of `command`, named `name`, which didn't start for `error`.
    pub(super) fn unstarted(command: &Command, name: String, error: io::Error) -> Self {
        let expected = expected(command, &error);
        Self::Unstarted {
            name,
            error,
            expected,
        }
    }

    /// The end of what an [`Exited`](Self::Exited) command wrote to its standard error, if that
    /// was captured, as a message quotes it: its last 4096 bytes, trimmed, less one trailing
    /// period, which the message's own punctuation follows. Empty for any other failure.
    pub fn reported(&self) -> Cow<'_, str> {
        let Self::Exited {
            output,
            stderr: Stream::Captured,
            ..
        } = self
        else {
            return Cow::Borrowed("");
        };
        let tail = &output.stderr[output.stderr.len().saturating_sub(REPORTED_BYTES)..];
        // From the first whole character, where the cut splits one.
        let split = (tail.iter().take(3))
            .take_while(|&&byte| byte & 0xC0 == 0x80)
            .count();
        match String::from_utf8_lossy(&tail[split..]) {
            Cow::Borrowed(text) => Cow::Borrowed(trimmed(text)),
            Cow::Owned(text) => Cow::Owned(trimmed(&text).to_owned()),
        }
    }

    /// What went wrong, as the failure's message says it, without what was expected or what to
    /// fix: for a caller that says what to do itself.
    pub fn unadvised(&self) -> String {
        self.parts().0
    }

    /// The failure's message in two: what went wrong, then what was expected or what to fix, if
    /// the message says.
    fn parts(&self) -> (String, Option<&str>) {
        match self {
            Self::Unstarted {
                name,
                error,
                expected,
            } => (format!("`{name}` didn't start: {error}"), Some(expected)),
            Self::Unwatched { name, error } => (format!("running `{name}` failed: {error}"), None),
            Self::Unended {
                name,
                bound,
                started,
            } => {
                let (wrong, expected) = unended(name, *bound, *started);
                (wrong, Some(expected))
            }
            Self::Exited {
                name,
                output,
                stderr,
            } => {
                let status = output.status;
                let (wrong, expected) = match stderr {
                    Stream::Captured => match self.reported() {
                        reported if reported.is_empty() => (
                            format!(
                                "`{name}` failed with {status} and wrote nothing to standard error"
                            ),
                            "expected it to exit with 0",
                        ),
                        reported => (
                            format!("`{name}` failed with {status}: {reported}"),
                            "fix what it reports",
                        ),
                    },
                    Stream::Inherited => (
                        format!("`{name}` failed with {status}"),
                        "fix what it printed above",
                    ),
                    Stream::Discarded => (
                        format!("`{name}` failed with {status}, its standard error discarded"),
                        "expected it to exit with 0",
                    ),
                };
                (wrong, Some(expected))
            }
        }
    }
}

/// What was expected of `command`, which didn't start for `error`.
// A spawn fails with these when its working directory can't be entered, as well as when its
// program is missing or can't be executed, which xshell tells apart in the same way:
// https://github.com/matklad/xshell/blob/9a405946ecbbfe50fe4a7229bd764194154e4ce6/src/lib.rs#L1020-L1029
fn expected(command: &Command, error: &io::Error) -> String {
    use io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
    let kind = error.kind();
    if !matches!(kind, NotFound | PermissionDenied | NotADirectory) {
        return "fix what the error says".to_owned();
    }
    // `dir/.` can be read only where `dir` is a directory that this process may enter.
    let entered = (command.get_current_dir()).map(|dir| (dir, fs::metadata(dir.join("."))));
    if let Some((dir, Err(entering))) = entered {
        let dir = dir.display();
        return match entering.kind() {
            NotFound => format!("expected its working directory `{dir}` to exist"),
            NotADirectory => format!("expected its working directory `{dir}` to be a directory"),
            _ => format!("expected its working directory `{dir}` to be one that it may enter"),
        };
    }
    let program = command.get_program();
    let shown = program.display();
    match (kind, found(command, program)) {
        (PermissionDenied, _) => format!("expected `{shown}` to be a program that it may execute"),
        // The kernel says so, too, of a script whose first line names an interpreter it lacks.
        (_, Some(path)) => format!(
            "expected the interpreter that the first line of `{}` names to exist",
            path.display()
        ),
        (_, None) if program.as_encoded_bytes().contains(&b'/') => {
            format!("expected a program at `{shown}`")
        }
        (_, None) => format!("install `{shown}` on `PATH`"),
    }
}

/// The file that runs as `program`, if there is one: a path, as it is, or the first file named
/// `program` in a directory on the `PATH` that `command` gets.
fn found(command: &Command, program: &OsStr) -> Option<PathBuf> {
    if program.as_encoded_bytes().contains(&b'/') {
        return Path::new(program).is_file().then(|| program.into());
    }
    let set = (command.get_envs()).find_map(|(name, value)| (name == "PATH").then_some(value));
    let path = match set {
        Some(value) => value.map(OsStr::to_owned),
        None => env::var_os("PATH"),
    };
    let dirs: Vec<PathBuf> = path.iter().flat_map(env::split_paths).collect();
    dirs.into_iter()
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

fn trimmed(text: &str) -> &str {
    let text = text.trim();
    text.strip_suffix('.').unwrap_or(text)
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.parts() {
            (wrong, Some(expected)) => write!(f, "{wrong}: {expected}"),
            (wrong, None) => f.write_str(&wrong),
        }
    }
}

fn unended(name: &str, bound: Bound, started: bool) -> (String, &'static str) {
    match (bound, started) {
        (Bound::Within(timeout), true) => {
            let secs = timeout.as_secs_f64();
            (
                format!("`{name}` didn't end within {secs} s, so it was killed"),
                "expected it to end by then",
            )
        }
        (Bound::Within(_), false) => (
            format!("`{name}` didn't start, as its bound was 0 s"),
            "expected a bound that leaves it time",
        ),
        (Bound::By(_), true) => (
            format!("`{name}` hadn't ended by its deadline, so it was killed"),
            "expected it to end by then",
        ),
        (Bound::By(_), false) => (
            format!("`{name}` didn't start, as its deadline had passed"),
            "expected one that leaves it time",
        ),
    }
}
