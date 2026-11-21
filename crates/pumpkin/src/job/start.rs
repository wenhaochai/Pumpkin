//! Starts a job of the build that received its request, once for the id the client chose.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use provider::RunTag;
use schema::ir::Sha256Digest;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use super::{BUILDS, JOBS, OWN, SENT_CONFIG, SENT_MODAL, TREES};
use crate::link::{JobKind, JobStart, StartAnswer};
use crate::member::Member;
use crate::{manifest, workspace};

// The bound on asking systemd to start a job.
const START_TIMEOUT: Duration = Duration::from_mins(10);

// In the job's persistent directory once systemd has confirmed the start.
const STARTED: &str = "started";

/// Starts the job of `start`, with the credentials in `body`, as `member`'s if it is a run's.
pub(crate) fn start(
    home: &Path,
    start: JobStart,
    body: &[u8],
    member: Option<&Member>,
) -> Result<StartAnswer, String> {
    let id = RunTag::try_from(start.id.clone())?;
    let name = super::unit_name(&id);
    let id = id.as_str();
    let jobs = super::jobs()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&jobs)
        .context(jobs.display())?;
    let lock = File::open(&jobs).context(jobs.display())?;
    lock.lock().context(jobs.display())?;
    for kind in [JOBS, BUILDS] {
        let dir = home.join(kind).join(id);
        if dir.try_exists().context(dir.display())? {
            // The job itself makes OWN, including one whose start wasn't recorded.
            if dir.join(STARTED).is_file() || dir.join(OWN).is_symlink() {
                return Ok(StartAnswer::AlreadyStarted);
            }
            return Ok(unconfirmed(
                &name,
                &jobs.join(id),
                "a previous start reserved this job id",
            ));
        }
    }
    if Path::new(&start.repo).file_name() != Some(start.repo.as_ref()) {
        return Err("a job's repository name must be a single directory name".to_owned());
    }
    if start.key_bytes.len() != start.sent.upstreams.key_files().len() {
        return Err("the job must send one key for each upstream in its configuration".to_owned());
    }
    manifest::store::required(&home.join(super::STORE), &start.tree)?;
    let pumpkin = installed(home, &start.install)?;
    let (kind, command) = match start.kind {
        JobKind::Run => (JOBS, "run"),
        JobKind::Build => (BUILDS, "build"),
    };
    let job = new_job_dir(&home.join(kind), id, 0o755)?;
    let sent = new_job_dir(&jobs, id, 0o700)?;
    let tree = new_job_dir(&home.join(TREES).join(&start.repo), id, 0o755)?;
    let workspace = home.join(workspace::DIR);
    symlink(&workspace, tree.path().join(workspace::DIR)).context(tree.path().display())?;
    write_sent(&start, sent.path(), body)?;
    // Only a run's job is listed with its member.
    if let (JobKind::Run, Some(member)) = (start.kind, member) {
        super::write_member(job.path(), member)?;
    }
    let answer = launch(
        &pumpkin,
        &name,
        start,
        command,
        tree.path(),
        sent.path(),
        job.path(),
    )?;
    let job = job.keep();
    drop((sent.keep(), tree.keep()));
    if matches!(answer, StartAnswer::Started) {
        let receipt = job.join(STARTED);
        if let Err(error) = File::create(&receipt) {
            crate::say(format_args!(
                "{name} started but writing {} failed: {error}",
                receipt.display()
            ));
        }
    }
    Ok(answer)
}

// Each directory is made anew, and removed on a failure before the unit takes it.
fn new_job_dir(parent: &Path, id: &str, mode: u32) -> Result<tempfile::TempDir, String> {
    fs::create_dir_all(parent).context(parent.display())?;
    tempfile::Builder::new()
        .prefix(id)
        .rand_bytes(0)
        .permissions(fs::Permissions::from_mode(mode))
        .tempdir_in(parent)
        .context(parent.join(id).display())
}

const SYSTEMD: Environment = Environment::Only(&[
    "HOME",
    "PATH",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
]);

fn launch(
    pumpkin: &Path,
    name: &str,
    start: JobStart,
    command: &str,
    tree: &Path,
    sent: &Path,
    job: &Path,
) -> Result<StartAnswer, String> {
    let mut unit = stdx::process::command("systemd-run", SYSTEMD);
    unit.args(["--user", "--quiet", "--expand-environment=no"])
        .arg(format!("--unit={name}"))
        .arg(format!("--working-directory={}", tree.display()))
        .arg("--property=ExitType=cgroup")
        .arg(format!(
            "--property=ExecStopPost=/usr/bin/rm --recursive --force {}",
            sent.display()
        ))
        .arg(format!(
            "--property=StandardOutput=append:{}/out",
            job.display()
        ))
        .arg(format!(
            "--property=StandardError=append:{}/log",
            job.display()
        ))
        .arg("--")
        .arg(pumpkin)
        .args([
            "controller",
            "job",
            &start.id,
            "--region",
            start.region.as_str(),
            "--parameter",
            &start.parameter,
            "--tree",
            start.tree.hex(),
            "--",
            command,
        ])
        .args(start.words);
    let (problem, exited) = match stdx::process::output(&mut unit, Deadline::after(START_TIMEOUT)) {
        Ok(Some(o)) if o.status.success() => return Ok(StartAnswer::Started),
        Ok(Some(o)) => (
            format!(
                "systemd-run failed with {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            o.status.code().is_some(),
        ),
        Ok(None) => ("systemd-run timed out".to_owned(), false),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Err(format!(
                "systemd-run could not start: {error}: install it on PATH"
            ));
        }
        Err(error) => (format!("systemd-run failed: {error}"), false),
    };
    after_launch(name, sent, job, &problem, exited)
}

fn after_launch(
    name: &str,
    sent: &Path,
    job: &Path,
    problem: &str,
    exited: bool,
) -> Result<StartAnswer, String> {
    if job.join(OWN).is_symlink() {
        return Ok(StartAnswer::Started);
    }
    let absent = exited && {
        let mut show = stdx::process::command("systemctl", SYSTEMD);
        show.args(["--user", "show", "--property=LoadState", "--value", name]);
        let state = stdx::process::output(&mut show, Deadline::after(Duration::from_secs(10)));
        matches!(state, Ok(Some(state)) if state.stdout == b"not-found\n")
    };
    // A short unit may have run ExecStopPost while systemctl was looking it up.
    match sent.try_exists() {
        Ok(false) => Ok(StartAnswer::Started),
        Ok(true) if absent => Err(format!(
            "{problem}: fix what systemd-run reports and run the command again"
        )),
        _ => Ok(unconfirmed(name, sent, problem)),
    }
}

fn unconfirmed(name: &str, sent: &Path, problem: &str) -> StartAnswer {
    StartAnswer::Unconfirmed {
        message: format!(
            "{problem}; the start of {name} is unconfirmed: check `systemctl --user status {name}` and `journalctl --user -u {name}` on the controller; if it never ran, remove {} there and run the task again",
            sent.display()
        ),
    }
}

fn write_sent(start: &JobStart, sent: &Path, body: &[u8]) -> Result<(), String> {
    let config = toml::to_string(&start.sent)
        .map_err(|error| format!("writing the job's configuration: {error}"))?;
    let write = |name: &str, bytes: &[u8]| {
        let path = sent.join(name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(bytes))
            .context(path.display())
    };
    write(SENT_CONFIG, config.as_bytes())?;
    let mut rest = body;
    for (name, len) in start.sent.upstreams.key_files().zip(&start.key_bytes) {
        let (key, left) = rest.split_at(*len);
        write(&name, key)?;
        rest = left;
    }
    if start.modal_bytes.is_some() {
        write(SENT_MODAL, rest)?;
    }
    Ok(())
}

/// Keeps the build that a job runs at the same location as a locally built install, under its
/// own digest. Older clients prune the serve installs after a day, even while its jobs still run.
fn installed(home: &Path, install: &Sha256Digest) -> Result<PathBuf, String> {
    let exe = workspace::exe("expected the build whose job is starting")?;
    if exe.parent().and_then(Path::file_name) != Some(install.hex().as_ref()) {
        return Err(format!(
            "{} is outside the install named by this start: link to the client's build",
            exe.display()
        ));
    }
    let root = home.join(workspace::DIR).join(install.hex());
    let bin = root.join("bin");
    let pumpkin = bin.join("pumpkin");
    if !pumpkin.try_exists().context(pumpkin.display())? {
        let store = home.join(super::STORE);
        let path = store.join(manifest::MANIFESTS).join(install.hex());
        let manifest = manifest::Manifest::parse(&fs::read(&path).context(path.display())?)?;
        fs::create_dir_all(&root).context(root.display())?;
        let made = tempfile::tempdir_in(&root).context(root.display())?;
        for entry in manifest.entries() {
            let source = match entry.kind {
                manifest::Kind::Executable => super::executable(&store, &entry.digest)?,
                manifest::Kind::File => store.join(entry.digest.hex()),
                manifest::Kind::Link => {
                    return Err(
                        "an install contains a symbolic link: expected the build's files"
                            .to_owned(),
                    );
                }
            };
            fs::hard_link(&source, made.path().join(&entry.path)).context(source.display())?;
        }
        // Older installs share this layout but their boot may reap another writer's records.
        let boot = made.path().join(super::BOOT_LINK);
        symlink("pumpkin", &boot).context(boot.display())?;
        fs::rename(made.path(), &bin).context(bin.display())?;
    }
    Ok(pumpkin)
}
