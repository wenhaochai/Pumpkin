//! Direct SSH connections to this build's `serve`. A command keeps its first for its requests.
//! An opening repairs the destination if needed, and a cut request is retried from its progress.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use schema::ir::Sha256Digest;
use stdx::{Context as _, Deadline};

use super::{AGAIN, Found, Located, OPENINGS, Reached, SHORT_TIMEOUT, when};
use crate::link::{self, Link, Request, Unanswered};
use crate::{manifest, say, workspace};

/// This build's files, which the controller installs whole and runs the `pumpkin` of as `serve`:
/// its executable, and whichever of an install's other files lie beside it, as they are, so that
/// a `pumpkin` run from cargo's `target/debug` brings that directory's debug builds.
pub(super) struct Build {
    files: Vec<(&'static str, PathBuf)>,
    pub(super) hashed: manifest::Hashed,
}

impl Build {
    /// Hashes this build's files; `expected` ends the message of a failure to find its executable.
    pub(super) fn of_this(expected: &str) -> Result<Self, String> {
        let exe = workspace::exe(expected)?;
        let mut files = Vec::new();
        let mut entries = Vec::new();
        let mut sources = BTreeMap::new();
        for file in install::File::ALL {
            let own = file == install::File::Pumpkin;
            let path = if own {
                exe.clone()
            } else {
                exe.with_file_name(file.name())
            };
            if !own && !path.try_exists().context(path.display())? {
                continue;
            }
            let (digest, size_bytes) = manifest::file_sha256(&path)?;
            let kind = if file.is_executable() {
                manifest::Kind::Executable
            } else {
                manifest::Kind::File
            };
            entries.push(manifest::Entry {
                kind,
                digest: digest.clone(),
                size_bytes,
                path: file.name().to_owned(),
            });
            sources.insert(digest, manifest::Source::File(path.clone()));
            files.push((file.name(), path));
        }
        files.sort_unstable_by_key(|&(name, _)| name);
        let manifest = manifest::Manifest::new(entries);
        let hashed = manifest::Hashed { manifest, sources };
        Ok(Self { files, hashed })
    }
}

pub(super) enum Unlinked {
    Cut { said: String, denied: bool },
    HostKey(String),
    Failed(String),
}

impl From<String> for Unlinked {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

pub(super) struct Linked {
    link: Link,
    // OpenSSH puts only its own diagnostics here. The remote command's stderr stays the user's.
    diagnostics: tempfile::NamedTempFile,
}

impl Linked {
    pub(super) fn open(reached: &Reached, build: &Build, at: &Located) -> Result<Self, Unlinked> {
        let key = build.hashed.manifest.name();
        if let Some(linked) = Self::connect(reached, &key, at)? {
            return Ok(linked);
        }
        install(reached, build, at)?;
        Self::connect(reached, &key, at)?.ok_or_else(|| {
            Unlinked::Failed(format!(
                "controller `{}` has no serve of this pumpkin though it was just installed \
                 there, in `~/{}/{}`: see that its user may run it, and run the command again",
                reached.name,
                link::installs(),
                key.hex()
            ))
        })
    }
    /// The answer to `request`, or the message of its refusal: `reached` is the controller that
    /// the link goes to.
    pub(super) fn ask<R: Request>(
        &mut self,
        reached: &Reached,
        request: R,
    ) -> Result<Result<R::Answer, String>, Unlinked> {
        self.ask_with_body(reached, request, &[])
    }

    pub(super) fn ask_with_body<R: Request>(
        &mut self,
        reached: &Reached,
        request: R,
        body: &[u8],
    ) -> Result<Result<R::Answer, String>, Unlinked> {
        self.stream(reached, |link| {
            link.send(&request.ask(), body)?;
            link.answer::<R>()
        })
    }

    pub(super) fn stream<T>(
        &mut self,
        reached: &Reached,
        read: impl FnOnce(&mut Link) -> Result<T, Unanswered>,
    ) -> Result<T, Unlinked> {
        read(&mut self.link).map_err(|error| unlinked(reached, &self.diagnostics, error))
    }

    fn connect(
        reached: &Reached,
        key: &Sha256Digest,
        at: &Located,
    ) -> Result<Option<Self>, Unlinked> {
        let diagnostics = reached.location.diagnostics()?;
        let mut ssh = Reached::ssh_command();
        ssh.arg("-E").arg(diagnostics.path());
        reached.remote(&mut ssh, at, &link::command(key));
        match Link::open(ssh) {
            Ok(link) => Ok(link.map(|link| Self { link, diagnostics })),
            Err(error) => Err(unlinked(reached, &diagnostics, error)),
        }
    }
}

impl Reached {
    fn connected(&self, build: &Build, mut found: Found) -> Result<(Linked, Found), String> {
        let mut repaired = false;
        let mut enrolled = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (said, denied) = match Linked::open(self, build, &found.at) {
                Ok(link) => return Ok((link, found)),
                Err(Unlinked::Failed(error)) => return Err(error),
                Err(Unlinked::HostKey(error)) => {
                    if !repaired && attempt < OPENINGS {
                        // The cached address may now belong to a different host. Check the pin
                        // against the discovered address before reporting a change there.
                        (found, _) = self.repair(&found, false, &error)?;
                        repaired = true;
                        continue;
                    }
                    self.location.failed(&self.name, &found, &error)?;
                    return Err(error);
                }
                Err(Unlinked::Cut { said, denied }) => (said, denied),
            };
            if attempt == OPENINGS || denied && enrolled {
                let hosts = self.kept.join("known_hosts");
                let error = format!(
                    "{said}: check SSH access to the controller and the host key pinned for `{}` in {}, then run the command again",
                    self.alias,
                    hosts.display()
                );
                self.location.failed(&self.name, &found, &error)?;
                return Err(error);
            }
            if denied || !repaired && attempt > 1 {
                let (next, tried) = self.repair(&found, denied, &said)?;
                found = next;
                repaired = true;
                enrolled |= tried;
            }
            let pause = Duration::from_secs(((1_u64 << (attempt - 1)) - 1).min(30));
            say(format_args!(
                "{said}: opening it again {}, attempt {} of {OPENINGS}",
                when(pause),
                attempt + 1
            ));
            thread::sleep(pause);
        }
    }

    /// Opens the first link of a command, starting the controller if its opening needs repair.
    pub(super) fn opening(&self, build: &Build) -> Result<(Linked, Found), String> {
        self.connected(build, self.found()?)
    }

    /// Keeps a link to this build across requests, asking again after a cut. `what` names the
    /// operation, as in "listing the runs".
    pub(super) fn asked<T>(
        &mut self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(&Self, &mut Linked) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        self.carrying_on(build, what, |reached, linked, _, _| ask(reached, linked))
    }

    /// As [`Self::asked`], with each attempt opening its link and reporting its progress.
    pub(super) fn carrying_on<T>(
        &mut self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(
            &Self,
            &mut Linked,
            &Located,
            &mut (dyn FnMut() + Send),
        ) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        let mut attempt = 0;
        loop {
            let (mut linked, found) = match self.linked.take() {
                Some(link) => link,
                None => self.opening(build)?,
            };
            let error = match ask(self, &mut linked, &found.at, &mut || attempt = 0) {
                Ok(answer) => {
                    self.linked = Some((linked, found));
                    return Ok(answer);
                }
                Err(Unlinked::Cut { said, .. }) => said,
                Err(Unlinked::HostKey(error) | Unlinked::Failed(error)) => return Err(error),
            };
            drop(linked);
            let Some(pause) = AGAIN.get(attempt) else {
                let error =
                    format!("{error}: run the command again when the controller is reachable");
                self.location.failed(&self.name, &found, &error)?;
                return Err(error);
            };
            attempt += 1;
            say(format_args!(
                "{error}: {what} again {}, attempt {} of {}",
                when(*pause),
                attempt + 1,
                AGAIN.len() + 1
            ));
            thread::sleep(*pause);
        }
    }
}

fn unlinked(
    reached: &Reached,
    diagnostics: &tempfile::NamedTempFile,
    error: Unanswered,
) -> Unlinked {
    let name = &reached.name;
    match error {
        Unanswered::Unreached(ended) => {
            let line = last_line(diagnostics);
            // OpenSSH's fatal when the server's host key cannot be verified.
            // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/sshconnect2.c#L104
            let host_refused = line.as_deref() == Some("Host key verification failed.");
            let denied = line
                .as_ref()
                .is_some_and(|line| line.contains("Permission denied ("));
            let said = line.map_or(ended, |line| format!("ssh says: {line}"));
            let said = format!("controller `{name}` didn't answer: {said}");
            if host_refused {
                Unlinked::HostKey(reached.host_refused(&said))
            } else {
                Unlinked::Cut { said, denied }
            }
        }
        Unanswered::Failed(error) => {
            Unlinked::Failed(format!("linking to controller `{name}` failed: {error}"))
        }
    }
}

fn last_line(file: &tempfile::NamedTempFile) -> Option<String> {
    // OpenSSH's MSGBUFSIZ is 1024. Leave room for the timestamp and a line ending.
    const TAIL_BYTES: u64 = 2048;
    let mut file = file.reopen().ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .last()
        .map(str::to_owned)
}

/// Uploads `build`'s files to the controller, as the install whose `serve` a link runs there.
fn install(reached: &Reached, build: &Build, at: &Located) -> Result<(), Unlinked> {
    let name = &reached.name;
    say(format_args!(
        "controller `{name}` has no serve of this pumpkin: installing it there"
    ));
    let archive = manifest::archive(&build.files)?;
    let diagnostics = reached.location.diagnostics()?;
    let mut ssh = Reached::ssh_command();
    ssh.arg("-E").arg(diagnostics.path());
    reached.remote(&mut ssh, at, &link::installing(&build.hashed.manifest));
    let installed = stdx::process::fed(&mut ssh, &archive, Deadline::after(SHORT_TIMEOUT));
    match installed {
        Ok(Some(result)) if result.status.success() => Ok(()),
        Ok(Some(result)) if !result.stderr.is_empty() => Err(Unlinked::Failed(format!(
            "installing serve failed: {}",
            String::from_utf8_lossy(&result.stderr).trim_end()
        ))),
        Ok(None) => Err(Unlinked::Failed(format!(
            "installing serve timed out after {} s",
            SHORT_TIMEOUT.as_secs()
        ))),
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(Unlinked::Failed(
            format!("installing serve failed: {error}"),
        )),
        Ok(Some(_)) | Err(_) => Err(unlinked(
            reached,
            &diagnostics,
            Unanswered::Unreached("ended during the install".to_owned()),
        )),
    }
}
