//! Sending a run's or a build's job to the controller: the task's files, which go by content to
//! the controller's store, and the command that starts the job from them.
//!
//! The controller says which blobs of the task's manifest its store lacks, and only those are
//! sent, each from its held offset. Large task uploads can use up to four links.
//! After a cut, the next store query finds which complete frames arrived.
//!
//! The start request sends this machine's settings and keys on the link's standard input. The
//! controller writes them to the job's directory in tmpfs and starts its systemd unit, which
//! outlives the connection.

mod upload;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use provider::RunTag;
use schema::workflow::Workflow;
use stdx::Context as _;
use stdx::process::Environment;

use super::linking::Build;
use super::{Name, Reached, SHORT_TIMEOUT, bounded};
use crate::link::{JobKind, JobStart, StartAnswer, StartRequest};
use crate::manifest::store::Missing;
use crate::{config, manifest, say};

// How often a send says how much it has sent: often enough that a person sees a send move, and
// seldom enough that a log of a send of minutes keeps a few lines of it. BuildKit's plain progress
// likewise says a step's count again only once it has moved 5%, or 5 s have passed:
// https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/util/progress/progressui/printer.go#L93-L101
const PROGRESS: Duration = Duration::from_secs(15);

/// The name of the SSM parameter that holds the settings of the jobs of controller `name`.
fn parameter(name: &Name) -> String {
    name.parameter("settings")
}

/// A task's git repository, as this machine sends it to the controller.
pub(super) struct Tree {
    /// The files the task reaches, by content.
    hashed: manifest::Hashed,
    /// The entry's path below the repository's top.
    entry: String,
    /// The repository's name, the top's own as `plain` gives it, which its trees on the
    /// controller are below.
    repo: String,
    /// The task's workflow, as checking it gave it, which isn't sent.
    pub(super) workflow: Workflow,
}

/// Hashes the tracked and unignored files that the task reaches below its repository.
pub(super) fn tree(entry: &crate::star::Entry, args: &lang::Args) -> Result<Tree, String> {
    let path = fs::canonicalize(entry.path()).context(entry.path())?;
    let dir = path.parent().unwrap_or(Path::new("/"));
    let git = |dir: &Path, args: &[&str]| {
        let mut git = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]));
        git.arg("-C").arg(dir).args(args);
        bounded(&mut git, &format!("git {}", args.join(" ")), SHORT_TIMEOUT)
    };
    let top = git(dir, &["rev-parse", "--show-toplevel"]).map_err(|error| {
        format!("{entry}: {error}: a run on the controller sends the entry's git repository: put it in one")
    })?;
    let top = Path::new(OsStr::from_bytes(top.strip_suffix(b"\n").unwrap_or(&top)));
    let top = fs::canonicalize(top).context(top.display())?;
    let below = path.strip_prefix(&top).map_err(|_| {
        format!(
            "{entry}: {} is outside its git repository, {}: put the entry in the repository",
            path.display(),
            top.display()
        )
    })?;
    let below = below.to_str().ok_or_else(|| {
        format!(
            "{entry}: its path below the git repository isn't UTF-8: rename it so it can be sent \
             to the controller"
        )
    })?;
    let files = git(
        &top,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let reach = match crate::star::reached(entry, args)? {
        Ok(reach) => reach,
        Err(problems) => return Err(crate::task_problems(entry, &problems)),
    };
    let files = needed(entry, &reach.root, &reach.paths, &top, &files)?;
    let hashed = manifest::hash(&top, &files)?;
    let name = top
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let repo = plain(&name);
    Ok(Tree {
        hashed,
        entry: below.to_owned(),
        repo,
        workflow: reach.workflow,
    })
}

/// `name` with each character that isn't ASCII alphanumeric or one of `._-` made `_`, to be a
/// directory's name.
fn plain(name: &str) -> String {
    let kept = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    name.chars()
        .map(|c| if kept(c) { c } else { '_' })
        .collect()
}

/// Of `listed`, the files that git lists in `top`, each ended by a NUL, those that checking the
/// task whose entry is `entry` reaches, `reached` below its `root`, as files or as directories
/// that hold them.
fn needed(
    entry: &crate::star::Entry,
    root: &Path,
    reached: &BTreeSet<String>,
    top: &Path,
    listed: &[u8],
) -> Result<Vec<u8>, String> {
    // At or below the top, unless the marker is above the repository that holds the entry.
    let prefix = root.strip_prefix(top).map_err(|_| {
        format!(
            "{entry}: the task's root, {}, is above its git repository, {}, which is all that a \
             run on the controller is sent: put the root's `{}` in the repository",
            root.display(),
            top.display(),
            crate::star::MARKER
        )
    })?;
    let prefix = prefix.to_string_lossy();
    let prefix = prefix.as_ref();
    let under = |path: &str| match prefix {
        "" => path.to_owned(),
        prefix if path.is_empty() => prefix.to_owned(),
        prefix => format!("{prefix}/{path}"),
    };
    let kept: Vec<String> = reached.iter().map(|path| under(path)).collect();
    let wanted = |file: &str| {
        kept.iter().any(|path| {
            path.is_empty()
                || file == path
                || file
                    .strip_prefix(path.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut files = Vec::new();
    for file in listed
        .split(|byte| *byte == 0)
        .filter(|file| !file.is_empty())
    {
        if wanted(&String::from_utf8_lossy(file)) {
            files.extend_from_slice(file);
            files.push(0);
        }
    }
    Ok(files)
}

/// Sends the task's `tree` and the client's configuration, keys and Modal configuration to the
/// controller, and starts the job `id` of `kind` there, which runs the task with `args`.
/// The keys go on the connection's standard input, to the job's directory in tmpfs, and are not
/// stored.
pub(super) fn send(
    reached: &mut Reached,
    client: &config::Client,
    id: &RunTag,
    kind: JobKind,
    tree: &Tree,
    args: &lang::Args,
) -> Result<Build, String> {
    let build = Build::of_this("expected to send it to the controller to run the job")?;
    let (sent, keys) = client.sent();
    let mut body = Vec::new();
    let mut key_bytes = Vec::new();
    for key in keys {
        let read = fs::read(&key).map_err(|error| {
            format!(
                "the key file {} can't be read ({error}): put the key there",
                key.display()
            )
        })?;
        key_bytes.push(read.len());
        body.extend(read);
    }
    let modal = client.modal_file()?;
    let modal_bytes = modal.as_ref().map(Vec::len);
    if let Some(modal) = modal {
        body.extend(modal);
    }
    let start = JobStart {
        id: id.as_str().to_owned(),
        kind,
        repo: tree.repo.clone(),
        tree: tree.hashed.manifest.name(),
        install: build.hashed.manifest.name(),
        words: crate::arg_flags(args)
            .chain(["--".to_owned(), tree.entry.clone()])
            .collect(),
        region: client.controller.region.clone(),
        parameter: parameter(&client.controller.name),
        sent,
        key_bytes,
        modal_bytes,
    };
    let request = StartRequest::encode(&start, &mut body)?;
    let (mut install_left, mut tree_left) = (None, None);
    match reached.carrying_on(
        &build,
        "starting the job",
        |reached, linked, at, advanced| {
            upload::send(
                linked,
                reached,
                &build.hashed,
                None,
                &mut install_left,
                advanced,
            )?;
            upload::send(
                linked,
                reached,
                &tree.hashed,
                Some((&build, at)),
                &mut tree_left,
                advanced,
            )?;
            linked.ask_with_body(reached, request, &body)
        },
    )?? {
        StartAnswer::Started | StartAnswer::AlreadyStarted => Ok(build),
        StartAnswer::Unconfirmed { message } => Err(message),
    }
}

fn holds(
    name: &Name,
    manifest: &manifest::Manifest,
    missing: &[Missing],
    bytes: u64,
    install: bool,
) -> String {
    let what = if install {
        "the build's files"
    } else {
        "the task's files"
    };
    let files = manifest.entries().len();
    if missing.is_empty() {
        return format!("controller `{name}` holds all {files} of {what}");
    }
    let digests: BTreeSet<_> = missing.iter().map(|blob| &blob.digest).collect();
    let count = manifest
        .entries()
        .iter()
        .filter(|entry| digests.contains(&entry.digest))
        .count();
    let which = if count == files {
        format!("all {files} of {what}")
    } else {
        format!(
            "{count} of {} {files} files",
            if install { "the build's" } else { "the task's" }
        )
    };
    format!("controller `{name}` lacks {which}: sending {}", size(bytes))
}

fn counting(name: &Name, bytes: u64) -> impl FnMut(u64) + '_ {
    let began = Instant::now();
    let mut next = began + PROGRESS;
    let mut sent = 0;
    move |count| {
        sent += count;
        let now = Instant::now();
        if now >= next {
            say(progress(name, sent, bytes, now - began));
            next = now + PROGRESS;
        }
    }
}

// A count of what is done against the whole, as Modal says of the files of a mount it uploads:
// https://github.com/modal-labs/modal-client/blob/dbb24aba92e2c6c4fa8a123234813cfa48e09b0b/py/modal/mount.py#L597
fn progress(name: &Name, sent_bytes: u64, size_bytes: u64, elapsed: Duration) -> String {
    let rate = u128::from(sent_bytes) * 1000 / elapsed.as_millis().max(1);
    format!(
        "sent {} of {} to controller `{name}`, at {}/s",
        size(sent_bytes),
        size(size_bytes),
        size(u64::try_from(rate).unwrap_or(u64::MAX))
    )
}

fn size(bytes: u64) -> String {
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    let Some((unit, name)) = units.into_iter().find(|(unit, _)| bytes >= *unit) else {
        return crate::counted(bytes, "byte");
    };
    let tenths = bytes * 10 / unit;
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}
