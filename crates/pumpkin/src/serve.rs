//! `pumpkin controller serve`: the controller's end of a client's link ([`crate::link`]), on its
//! standard input and output, which answers from the runs, the jobs and the machine there.
//!
//! As it starts, it marks its install as used, and removes the other installs, and what an install
//! left half made, that no `serve` used for a day, as Zed's server removes its older binaries as
//! it starts
//! (<https://github.com/zed-industries/zed/blob/e231594b6b8818ea8d668ee2eb73e1fb09300654/crates/remote_server/src/server.rs#L1429-L1449>).
//! Then it answers each request until its input ends, brings no request for [`IDLE`], or breaks
//! the protocol, which it refuses and exits 1. It marks its install as used again as each request
//! arrives, so that a `serve` kept for days by an open page keeps its install.
//!
//! It acts for the member that `--member` names, as the controller's forced command gives it: each
//! job it starts names that member, its listings name each job's, and it cancels another member's
//! job only when asked to.

mod copy;
mod follow;
mod reaping;

use std::ffi::OsStr;
use std::fs::{self, DirEntry, File};
use std::io::{self, BufReader, Read as _, Seek as _, SeekFrom, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use engine::prune::Disk;
use provider::UnixTime;
use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use schema::frame;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use crate::job::{JOBS, WAITING};
use crate::link::{
    self, Ask, Boot, Health, HealthRequest, Hello, Job, Jobs, JobsRequest, ListRequest, Listed,
    Owner, ViewRequest,
};
use crate::manifest::store::{Missing, PutAnswer};
use crate::member::Member;
use crate::progress::{self, Entry, Listing, Row};
use crate::say;
use crate::view::Answering;

/// How long to wait for a request before ending: a page that is open polls every few seconds, so
/// a minute with none is a closed page or a lost connection, and ending closes its SSH connection.
pub(crate) const IDLE: Duration = Duration::from_mins(1);

// How long an install lasts that no `serve` uses: a day, as a store's partial blobs last, so
// that a build used every day keeps its install, and one left behind is gone by the next.
const INSTALL_KEPT: Duration = Duration::from_hours(24);

// The bound of `systemctl show`, which reads a unit's properties from systemd.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// The unit that reaps at the controller's boot.
pub(crate) const BOOT_UNIT: &str = "pumpkin-boot.service";

/// The file of the controller user's home that the boot's output is appended to.
pub(crate) const BOOT_LOG: &str = "boot.log";

const UPTIME: &str = "/proc/uptime";

/// Answers a client's link on the standard input and output, as the module says, for `member`.
pub(crate) fn serve(member: Option<&Member>) -> Result<ExitCode, String> {
    let home = crate::home("run `serve` as the controller's user, whose home holds the runs")?;
    let install = own_install();
    match &install {
        Ok(install) => prune(&home.join(link::installs()), install),
        Err(error) => say(format_args!(
            "{error}, so no install is removed, or marked as used"
        )),
    }
    let stdin = io::stdin().as_fd().try_clone_to_owned();
    let mut input = BufReader::new(File::from(stdin.context("the standard input")?));
    let mut output = io::stdout().lock();
    if !delivered(frame::write(&mut output, &Hello::Serving, &[]))? {
        return Ok(ExitCode::SUCCESS);
    }
    let install = install.as_deref().ok();
    requests(&mut input, &mut output, &home, install, member)
}

fn requests(
    input: &mut BufReader<File>,
    output: &mut impl Write,
    home: &Path,
    install: Option<&Path>,
    member: Option<&Member>,
) -> Result<ExitCode, String> {
    // Kept for the link's life: it holds what it read of the live runs.
    let mut view = Answering::new(home);
    while arrives(input)? {
        let ask = match request_head(input) {
            Ok(Some(ask)) => ask,
            Ok(None) => break,
            Err(error) => return invalid_request(output, error),
        };
        if let Some(install) = install {
            mark_install_used(install);
        }
        let body = match ask.read_buffered_body(input) {
            Ok(body) => body,
            Err(error) => return invalid_request(output, error),
        };
        let written = match ask {
            Ask::Copy(request) => copy::answer(output, home, request, &body),
            Ask::Lacks(request) => {
                link::answer(output, request, |request| missing(home, &request, &body))
            }
            Ask::Put(put) => match receive_put(home, &put, input) {
                Ok(Ok(answer)) => frame::write(output, &answer, &[]),
                Ok(Err(message)) => link::refuse(output, message),
                Err(error) => match error.kind() {
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => break,
                    _ => return invalid_request(output, error),
                },
            },
            Ask::Puts { blobs } => match receive_group(home, &blobs, input) {
                Ok(answers) => output.write_all(&answers).and_then(|()| output.flush()),
                Err(error) => match error.kind() {
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => break,
                    _ => return invalid_request(output, error),
                },
            },
            Ask::Start(request) => link::answer(output, request, |request| {
                let (start, credentials) = request.decode(&body)?;
                crate::job::start(home, start, credentials, member)
            }),
            Ask::Reap(request) => link::answer(output, request, |request| {
                reaping::reap(home, request, &body)
            }),
            Ask::Cancel(request) => link::answer(output, request, |request| {
                reaping::cancel(home, request, &body, member)
            }),
            Ask::Follow {
                id,
                log_from_bytes,
                after_cut,
            } => follow::answer(output, home, id, log_from_bytes, after_cut),
            Ask::Health(request) => link::answer(output, request, |HealthRequest {}| health(home)),
            Ask::Jobs(request) => {
                link::answer(output, request, |JobsRequest {}| jobs(home, member))
            }
            Ask::List(request) => link::answer(output, request, |ListRequest { prefixes }| {
                listed(home, &prefixes)
            }),
            Ask::View(request) => {
                link::answer(output, request, |ViewRequest { target, deflate }| {
                    Ok(view.answer(&target, deflate))
                })
            }
        };
        if !delivered(written)? {
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The runs' jobs in `home`, each with its member, for a link that acts for `member`.
fn jobs(home: &Path, member: Option<&Member>) -> Result<Jobs, String> {
    let dir = home.join(JOBS);
    let ids = crate::job::ids(&dir)?;
    let jobs = ids.into_iter().map(|id| {
        let owner = Owner::from(crate::job::member(&dir.join(&id)));
        Job { id, owner }
    });
    Ok(Jobs {
        member: member.cloned(),
        jobs: jobs.collect(),
    })
}

fn request_head(input: &mut impl io::Read) -> io::Result<Option<Ask>> {
    match frame::read::<Ask>(input)? {
        None => Ok(None),
        Some((ask, body)) if body.is_empty() => Ok(Some(ask)),
        Some((_, body)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a request has a {}-byte body", body.len()),
        )),
    }
}

fn invalid_request(
    output: &mut impl Write,
    error: impl std::fmt::Display,
) -> Result<ExitCode, String> {
    let message = format!("{error}: expected a request of this pumpkin");
    delivered(link::refuse(output, message))?;
    Ok(ExitCode::FAILURE)
}

fn missing(home: &Path, request: &link::store::Lacks, body: &[u8]) -> Result<Vec<Missing>, String> {
    let manifest = crate::manifest::Manifest::parse(body)?;
    if manifest.name() != request.manifest {
        return Err(
            "the manifest that arrived differs from its digest: send the task again".to_owned(),
        );
    }
    let install = request.install.then(own_install).transpose()?;
    crate::manifest::store::lacks(&home.join(crate::job::STORE), &manifest, install.as_deref())
}

fn receive_put(
    home: &Path,
    put: &link::store::Put,
    input: &mut impl io::Read,
) -> io::Result<Result<PutAnswer, String>> {
    let mut parts = link::store::Parts::new(input, put.body_bytes()?);
    let stored = crate::manifest::store::put(
        &home.join(crate::job::STORE),
        &put.digest,
        put.from,
        put.size_bytes,
        &mut parts,
    );
    parts.finish()?;
    Ok(stored)
}

fn receive_group(
    home: &Path,
    puts: &[link::store::Put],
    input: &mut impl io::Read,
) -> io::Result<Vec<u8>> {
    link::store::check_group(puts)?;
    // A refusal can fill the return pipe while the client is still writing the group's bodies.
    let mut answers = Vec::new();
    let mut failed = false;
    for put in puts {
        if failed {
            link::store::Parts::new(input, put.body_bytes()?).finish()?;
        } else {
            let stored = receive_put(home, put, input)?;
            failed = !matches!(stored, Ok(PutAnswer::Stored));
            match stored {
                Ok(answer) => frame::write(&mut answers, &answer, &[])?,
                Err(error) => link::refuse(&mut answers, error)?,
            }
        }
    }
    Ok(answers)
}

// The client may leave at any frame, including while an answer is still streaming.
fn delivered(written: io::Result<()>) -> Result<bool, String> {
    match written {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error).context("writing to the standard output failed"),
    }
}

/// Whether `input` brings a request, or its end, within [`IDLE`].
pub(crate) fn arrives(input: &BufReader<File>) -> Result<bool, String> {
    if !input.buffer().is_empty() {
        return Ok(true);
    }
    ready(input.get_ref(), PollFlags::IN, IDLE).context("polling the standard input failed")
}

fn ready(fd: &impl AsFd, events: PollFlags, timeout: Duration) -> io::Result<bool> {
    let Ok(timeout) = Timespec::try_from(timeout) else {
        unreachable!("{timeout:?} fits a timespec")
    };
    let mut polled = [PollFd::new(fd, events)];
    loop {
        match event::poll(&mut polled, Some(&timeout)) {
            Ok(count) => return Ok(count != 0),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// The directory of this `serve`'s install, which holds its executable and the rest of its build.
fn own_install() -> Result<PathBuf, String> {
    let exe = crate::workspace::exe("expected it to tell its install from the others")?;
    match exe.parent() {
        Some(install) => Ok(install.to_owned()),
        None => Err(format!(
            "{} is in no directory, as an install is",
            exe.display()
        )),
    }
}

/// Marks `install`, this `serve`'s, as used, and removes each other entry in `installs` that
/// nothing changed for `INSTALL_KEPT`, saying, but going on past, what it can't do.
fn prune(installs: &Path, install: &Path) {
    let own = install.file_name();
    let entries = match fs::read_dir(installs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => return say(format_args!("{}: {error}", installs.display())),
    };
    let now = SystemTime::now();
    for entry in entries {
        let pruned = (entry.context(installs.display())).and_then(|entry| pruned(&entry, own, now));
        if let Err(error) = pruned {
            say(format_args!(
                "{error}: expected to remove each install of `serve` there that none used for a \
                 day, which the next `serve` tries again"
            ));
        }
    }
}

/// Marks `install`, this `serve`'s, as used now, saying, but going on past, what it can't do.
fn mark_install_used(install: &Path) {
    if let Err(error) = mark_used(install, SystemTime::now()) {
        say(error);
    }
}

fn mark_used(install: &Path, now: SystemTime) -> Result<(), String> {
    let touched = File::open(install).and_then(|dir| dir.set_modified(now));
    touched.context(install.display())
}

fn pruned(entry: &DirEntry, own: Option<&OsStr>, now: SystemTime) -> Result<(), String> {
    let path = entry.path();
    if Some(entry.file_name().as_os_str()) == own {
        return mark_used(&path, now);
    }
    let modified = entry.metadata().and_then(|metadata| metadata.modified());
    let modified = modified.context(path.display())?;
    if now.duration_since(modified).unwrap_or_default() <= INSTALL_KEPT {
        return Ok(());
    }
    let removed = if entry.file_type().context(path.display())?.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };
    match removed {
        // Another `serve`'s prune removed it first.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed.context(path.display()),
    }
}

/// The whole seconds since the machine booted.
pub(crate) fn up_seconds() -> Result<u64, String> {
    let uptime = fs::read_to_string(UPTIME).context(UPTIME)?;
    let seconds = uptime.split(['.', ' ']).next().unwrap_or_default();
    seconds.parse().map_err(|_| {
        format!(
            "{UPTIME} holds `{}`: expected the seconds since the machine booted, first, as \
             Linux writes them",
            uptime.trim().escape_debug()
        )
    })
}

fn health(home: &Path) -> Result<Health, String> {
    let up_seconds = up_seconds()?;
    let Disk {
        size_bytes,
        free_bytes,
    } = crate::job::disk(crate::job::VOLUME)?;
    Ok(Health {
        up_seconds,
        disk_size_bytes: size_bytes,
        disk_free_bytes: free_bytes,
        boot: boot(home),
    })
}

fn boot(home: &Path) -> Boot {
    let mut systemctl = stdx::process::command("systemctl", Environment::Only(&["PATH"]));
    systemctl.args([
        "show",
        "--property=ActiveState",
        "--property=ExecMainStatus",
    ]);
    systemctl.args(["--property=ExecMainExitTimestamp", BOOT_UNIT]);
    // A `systemctl` that doesn't run, or fails, says no state.
    let printed = match stdx::process::output(&mut systemctl, Deadline::after(SYSTEMCTL_TIMEOUT)) {
        Ok(Some(output)) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Ok(None) | Err(_) => String::new(),
    };
    let property =
        |name: &str| (printed.lines()).find_map(|line| line.strip_prefix(name)?.strip_prefix('='));
    match property("ActiveState") {
        None | Some("") => Boot::Unknown,
        Some("activating") => Boot::Running,
        Some("failed") => match property("ExecMainStatus").map(str::parse) {
            Some(Ok(status)) => Boot::Failed {
                status,
                log: boot_log(&home.join(BOOT_LOG)).unwrap_or_else(|error| error.to_string()),
                at: property("ExecMainExitTimestamp")
                    .filter(|at| !at.is_empty())
                    .map(str::to_owned),
            },
            Some(Err(_)) | None => Boot::Unknown,
        },
        Some(_) => Boot::Done,
    }
}

/// The last lines of the boot unit's output, small enough to fit in the health frame even
/// when the JSON encoding escapes every byte.
fn boot_log(path: &Path) -> io::Result<String> {
    const TAIL_BYTES: u64 = 4096;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text.lines().rev().take(5).collect();
    lines.reverse();
    Ok(lines.join("\n"))
}

/// The listing of the runs below `home` whose ids start with one of `prefixes`, as
/// [`crate::listed_in`] makes it, at this machine's time, with the jobs yet to start a run.
fn listed(home: &Path, prefixes: &[String]) -> Result<Listed, String> {
    let now = crate::unix_now();
    let since = progress::listed_since(prefixes, now);
    let runs = home.join(crate::job::runs());
    let (mut listing, mut unlisted) = crate::listed_in(&runs, prefixes, since, now, None)?;
    unstarted(home, prefixes, since, now, &mut listing)?;
    let jobs = home.join(JOBS);
    // A run whose member can't be read is left out and said, as one whose record can't be.
    listing.runs.retain(|id, entry| {
        let Entry::Row(row) = entry else {
            return true;
        };
        match crate::job::member(&jobs.join(id)) {
            Ok(member) => {
                row.member = member;
                true
            }
            Err(error) => {
                say(format_args!("run {id}: {error}"));
                unlisted += 1;
                false
            }
        }
    });
    Ok(Listed {
        listing,
        unlisted_count: unlisted as u64,
    })
}

/// Adds to `listing` each job in `home`'s `JOBS` whose id starts with one of `prefixes`, or each
/// with none, that `listing` doesn't hold as a run or as older: `waiting` for a build slot,
/// `building`, or `unstarted` once its unit has ended, but only named as older if its log is
/// from before `since`.
fn unstarted(
    home: &Path,
    prefixes: &[String],
    since: Option<UnixTime>,
    now: UnixTime,
    listing: &mut Listing,
) -> Result<(), String> {
    let jobs = home.join(JOBS);
    let entries = match fs::read_dir(&jobs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(jobs.display()),
    };
    let held = crate::job::jobs()?;
    for entry in entries {
        let name = entry.context(jobs.display())?.file_name();
        // As `ls` lists them, which leaves out a name that starts with a dot.
        let Some(id) = name.to_str().filter(|id| !id.starts_with('.')) else {
            continue;
        };
        // A run whose directory is made, or whose record is written, after `listed_in` read the
        // runs is not in `listing` yet, and its job is its row, so that no job drops out.
        if !crate::picked(prefixes, id) || listing.runs.contains_key(id) {
            continue;
        }
        let asked = runtime::minted(id);
        let tmpfs = held.join(id);
        let listed = if exists(&tmpfs.join(WAITING))? {
            progress::Listed::Waiting { asked }
        } else if is_dir(&tmpfs)? {
            progress::Listed::Building { asked }
        } else if logged_since(&jobs.join(id).join("log"), since)? {
            progress::Listed::Unstarted
        } else {
            listing.runs.insert(id.to_owned(), Entry::Older);
            continue;
        };
        let row = Row::of(id, &listed, now, None);
        listing.runs.insert(id.to_owned(), Entry::Row(row));
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool, String> {
    path.try_exists().context(path.display())
}

fn is_dir(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context(path.display()),
    }
}

/// Whether `log`, a job's, was written since `since`, or is missing.
fn logged_since(log: &Path, since: Option<UnixTime>) -> Result<bool, String> {
    let Some(since) = since else {
        return Ok(true);
    };
    let modified = match fs::metadata(log).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).context(log.display()),
    };
    Ok(UnixTime::of(modified).unwrap_or(crate::EPOCH) >= since)
}
