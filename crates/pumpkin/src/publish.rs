//! Publishing a controller's runs to the S3 bucket of the public page: the runs, as `view`
//! exports them, copied to the bucket.
//!
//! The export's directory (`EXPORT`) holds what was exported last, and the bucket what was
//! uploaded; nothing else is kept but the mark (`MARK`). One publisher at a time holds the mark's
//! lock, exports, plans the uploads of what changed since the mark (`plan`), runs them, then moves
//! the mark to the time it planned at, so that a file written during the uploads goes at the
//! next. One that fails leaves the mark where it was, so the next publish plans again what this
//! one did not upload.
//!
//! The uploads go in an order in which no file names one not uploaded yet: the files written once
//! (each run's `exchange/`, `diff/` and `kept/`, and the page's digest directories), then each
//! run's other files, then each run's `run.json`, then the page's `index.html` and `robots.txt`,
//! then `runs.json`. Each file's cache lifetime is its `Cache`'s.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};

use aws::iam::Call;
use aws::{Bound, Cli, Region};
use jiff::Timestamp;
use provider::RunTag;
use runtime::Record;
use serde::Deserialize;
use stdx::Context as _;
use toml::value::Datetime;

use crate::iam::PUBLISH;
use crate::view::{self, Exported, Scrub};
use crate::{cli, say};

// How often a live run's records are published. A reader sees a live run this far behind, and the
// lifetime of a `Cache::Mutable` file besides.
const PUBLISH_EVERY: Duration = Duration::from_mins(1);

// How often the wait between publishes looks whether the run has ended.
const ENDED_POLL: Duration = Duration::from_secs(1);

// The directory of the export, beside the runs directory.
const EXPORT: &str = "public";

// The file in the export that a publisher locks, whose modification time is when the last publish
// that uploaded all it planned read the directory: the Unix epoch until one has.
const MARK: &str = ".published";

// The directories of a run's export whose files are never rewritten, so that they are cached
// for good.
const WRITTEN_ONCE: [&str; 3] = ["exchange", "diff", "kept"];

// The extension of a file that the export is still writing, which no upload takes.
const PART: &str = "part";

/// `[publish]` of the controller's settings, as `pumpkin help config` says.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    bucket: Bucket,
    #[expect(
        dead_code,
        reason = "named for a take-down, which invalidates its caches"
    )]
    distribution: String,
    address: String,
    since: Option<Since>,
}

/// An S3 bucket's name.
#[derive(Clone, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct Bucket(String);

impl TryFrom<String> for Bucket {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        let allowed = |byte: &u8| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-');
        let ends = |byte: Option<&u8>| byte.is_some_and(u8::is_ascii_alphanumeric);
        let bytes = name.as_bytes();
        if (3..=63).contains(&bytes.len())
            && bytes.iter().all(allowed)
            && ends(bytes.first())
            && ends(bytes.last())
        {
            return Ok(Self(name));
        }
        Err(format!(
            "`{}` is no S3 bucket's name: name one of 3 to 63 lowercase letters, digits, `.` and \
             `-`, which starts and ends with a letter or a digit",
            name.escape_debug()
        ))
    }
}

/// An instant, to the second, as RFC 3339 writes one, such as `2026-11-15T00:00:00Z`.
#[derive(Clone, Copy, Deserialize)]
#[serde(try_from = "Datetime")]
pub(crate) struct Since {
    since_epoch: Duration,
}

impl TryFrom<Datetime> for Since {
    type Error = String;

    fn try_from(datetime: Datetime) -> Result<Self, String> {
        let (Some(date), Some(time), Some(offset)) =
            (datetime.date, datetime.time, datetime.offset)
        else {
            return Err(format!(
                "`{datetime}` is no instant: give a date, a time and an offset, as \
                 `2026-11-15T00:00:00Z`"
            ));
        };
        // RFC 3339 as jiff reads it: the offset is `Z` or `+hh:mm`, and a time without seconds
        // is at their start.
        let text = format!(
            "{date}T{:02}:{:02}:{:02}{offset}",
            time.hour,
            time.minute,
            time.second.unwrap_or(0)
        );
        let at: Timestamp = text.parse().map_err(|_| {
            format!(
                "`{datetime}` is no instant: give a date and a time that exist, as \
                 `2026-11-15T00:00:00Z`"
            )
        })?;
        let seconds = u64::try_from(at.as_second())
            .map_err(|_| format!("`{datetime}` is before 1970: give an instant after it"))?;
        Ok(Self {
            since_epoch: Duration::from_secs(seconds),
        })
    }
}

impl FromStr for Since {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let datetime: Datetime = text.parse().map_err(|_| {
            format!(
                "`{}` is no instant as RFC 3339 writes one: give one such as \
                 `2026-11-15T00:00:00Z`",
                text.escape_debug()
            )
        })?;
        Self::try_from(datetime)
    }
}

/// Publishes the runs of a directory once, waiting if a publish of it is under way, as
/// `pumpkin controller publish`'s help says.
pub(crate) fn by_hand(flags: cli::Publish) -> Result<ExitCode, String> {
    let cli::Publish {
        runs,
        bucket,
        region,
        since,
        controller_addresses,
    } = flags;
    let (runs, absolute) = crate::runs_dir(runs)?;
    let runs = absolute.unwrap_or(runs);
    if !Path::new(&runs).is_dir() {
        return Err(format!(
            "the runs directory {runs} isn't there: name the directory of the runs to publish \
             with --runs"
        ));
    }
    let to = Publisher::new(&runs, bucket, since, &controller_addresses, region)?;
    let published = to.publish()?;
    published.exported.say_refused();
    let Published {
        exported:
            Exported {
                ids,
                left_out,
                written,
                refused: _,
            },
        uploads_count,
    } = published;
    say(format_args!(
        "published {} to s3://{}, and left out {}: the export in {} wrote {written} files, and {} \
         uploaded what changed",
        crate::counted(ids.len(), "run"),
        to.bucket.0,
        left_out.len(),
        to.export,
        crate::counted(uploads_count, "`aws` command"),
    ));
    Ok(ExitCode::SUCCESS)
}

/// Waits until `record`'s run has ended, publishing the runs in `runs` as `settings` say, with
/// `aws` in `region`, every `PUBLISH_EVERY` while it is live, and once more as it ends, each with
/// the controller's addresses replaced (`crate::job::addresses`). A publish that fails, or one
/// that can't start, is said, and the next tries again; only a failed read of the record fails
/// the wait.
pub(crate) fn until_ended(
    record: &Record,
    settings: Settings,
    runs: &str,
    region: Region,
) -> Result<(), String> {
    let Settings {
        bucket,
        distribution: _,
        address,
        since,
    } = settings;
    let publisher = crate::job::addresses().and_then(|(private, public)| {
        Publisher::new(runs, bucket, since, &[private, public], region)
    });
    let publisher = match publisher {
        Ok(publisher) => publisher,
        Err(error) => {
            say(format_args!("the runs can't be published: {error}"));
            record.wait(|| {})?;
            return Ok(());
        }
    };
    say(format_args!(
        "publishing the runs at {address} every {} s while the run is live",
        PUBLISH_EVERY.as_secs()
    ));
    loop {
        let ended = record.ending()?.is_some();
        // The last publish waits for another's, which may have read the runs before this one ended.
        let result = if ended {
            publisher.publish().map(Some)
        } else {
            publisher.try_publish()
        };
        if let Err(error) = result {
            say(format_args!(
                "publishing the runs failed: {error}; the next publish uploads what this one \
                 didn't"
            ));
        }
        if ended {
            return Ok(());
        }
        let next = Instant::now() + PUBLISH_EVERY;
        while Instant::now() < next && record.ending()?.is_none() {
            std::thread::sleep(ENDED_POLL);
        }
    }
}

struct Publisher {
    runs: String,
    export: String,
    bucket: Bucket,
    since: Option<Since>,
    scrub: Scrub,
    aws: Cli,
}

struct Published {
    exported: Exported,
    uploads_count: usize,
}

impl Publisher {
    /// A publisher of the runs in `runs`, an absolute path, but those that started before `since`,
    /// to `bucket`, with `aws` in `region`, each address of `controller` replaced with
    /// `<controller>` beside what an export replaces (`Scrub`).
    fn new(
        runs: &str,
        bucket: Bucket,
        since: Option<Since>,
        controller: &[Ipv4Addr],
        region: Region,
    ) -> Result<Self, String> {
        let mut scrub = Scrub::of_machine();
        for address in controller {
            scrub.replace(&address.to_string(), "<controller>");
        }
        let export = Path::new(runs).with_file_name(EXPORT);
        let Some(export) = export.to_str().map(str::to_owned) else {
            unreachable!("the runs directory's path is UTF-8, and so is its sibling's");
        };
        Ok(Self {
            runs: runs.to_owned(),
            export,
            bucket,
            since,
            scrub,
            aws: Cli::connect(region)?,
        })
    }

    /// Opens the mark, dated at the epoch if new so that the first publish uploads everything.
    fn mark(&self) -> Result<(PathBuf, File), String> {
        let export = Path::new(&self.export);
        fs::create_dir_all(export).context(export.display())?;
        let path = export.join(MARK);
        let made = OpenOptions::new().write(true).create_new(true).open(&path);
        let mark = match made {
            Ok(made) => {
                made.set_modified(SystemTime::UNIX_EPOCH)
                    .context(path.display())?;
                made
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
                .write(true)
                .open(&path)
                .context(path.display())?,
            Err(error) => return Err(error).context(path.display()),
        };
        Ok((path, mark))
    }

    fn publish(&self) -> Result<Published, String> {
        let (path, mark) = self.mark()?;
        mark.lock().context(path.display())?;
        self.upload(&path, &mark)
    }

    fn try_publish(&self) -> Result<Option<Published>, String> {
        let (path, mark) = self.mark()?;
        match mark.try_lock() {
            Ok(()) => self.upload(&path, &mark).map(Some),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).context(path.display()),
        }
    }

    /// With `mark` locked, exports, uploads what changed since the mark in order, then moves the
    /// mark to the time it planned at.
    fn upload(&self, path: &Path, mark: &File) -> Result<Published, String> {
        let export = Path::new(&self.export);
        let since = self.since.map(|since| since.since_epoch);
        let exported = view::exported(Path::new(&self.runs), export, self.scrub.clone(), since)?;
        let planned_at = SystemTime::now();
        let marked_at = mark.metadata().and_then(|metadata| metadata.modified());
        let plan = plan(export, marked_at.context(path.display())?, &exported.ids)?;
        for (index, upload) in plan.iter().enumerate() {
            let (call, args) = upload.args(&self.export, &self.bucket);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.aws.run(call, &args, Bound::Call).map_err(|error| {
                format!(
                    "upload {} of {} to s3://{}: {error}",
                    index + 1,
                    plan.len(),
                    self.bucket.0
                )
            })?;
        }
        mark.set_modified(planned_at).context(path.display())?;
        Ok(Published {
            exported,
            uploads_count: plan.len(),
        })
    }
}

struct Upload {
    files: Files,
    cache: Cache,
    encoding: Encoding,
}

enum Files {
    Sync { dir: String, filter: Filter },
    Copy { key: String },
}

#[derive(Clone, Copy)]
enum Filter {
    WrittenOnce,
    Rewritten,
    All,
}

#[derive(Clone, Copy)]
enum Cache {
    /// A year, as a file written once never changes.
    Immutable,
    /// 30 s, as long as a reader may be late to a file's change.
    Mutable,
}

#[derive(Clone, Copy)]
enum Encoding {
    /// Deflated in zlib's format, which `Content-Encoding: deflate` names.
    Deflate,
    Identity,
}

impl Upload {
    /// The call of the upload, and its options.
    fn args(&self, export: &str, bucket: &Bucket) -> (&'static Call, Vec<String>) {
        let bucket = &bucket.0;
        let mut args = Vec::new();
        let call = match &self.files {
            Files::Sync { dir, filter } => {
                args.push(format!("{export}/{dir}"));
                args.push(format!("s3://{bucket}/{dir}"));
                let exclude = |pattern: &str| ["--exclude".to_owned(), pattern.to_owned()];
                let include = |pattern: &str| ["--include".to_owned(), pattern.to_owned()];
                match filter {
                    Filter::WrittenOnce => {
                        args.extend(exclude("*"));
                        for dir in WRITTEN_ONCE {
                            args.extend(include(&format!("{dir}/*")));
                        }
                    }
                    Filter::Rewritten => {
                        for dir in WRITTEN_ONCE {
                            args.extend(exclude(&format!("{dir}/*")));
                        }
                        args.extend(exclude("run.json"));
                    }
                    Filter::All => {}
                }
                // Last, as the last pattern that matches a file decides.
                args.extend(exclude(&format!("*.{PART}")));
                &PUBLISH.sync
            }
            Files::Copy { key } => {
                args.push(format!("{export}/{key}"));
                args.push(format!("s3://{bucket}/{key}"));
                &PUBLISH.copy
            }
        };
        let cache = match self.cache {
            Cache::Immutable => "public, max-age=31536000, immutable",
            Cache::Mutable => "public, max-age=30",
        };
        args.extend(["--cache-control".to_owned(), cache.to_owned()]);
        match self.encoding {
            Encoding::Deflate => {
                args.extend(["--content-encoding".to_owned(), "deflate".to_owned()]);
            }
            Encoding::Identity => {}
        }
        args.push("--only-show-errors".to_owned());
        (call, args)
    }
}

/// The uploads, in order, of what changed in the export's directory `export` since `mark`, of the
/// runs `ids` and the page: each file modified after `mark` is in one of them. A run's files that
/// are written once go with a cache that never expires; the rest with a short one.
fn plan(export: &Path, mark: SystemTime, ids: &[RunTag]) -> Result<Vec<Upload>, String> {
    let ids: BTreeSet<&str> = ids.iter().map(RunTag::as_str).collect();
    let (mut once, mut rewritten, mut run_files) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let (mut page_dirs, mut page_files, mut list) = (BTreeSet::new(), BTreeSet::new(), false);
    for key in changed(export, mark)? {
        let parts: Vec<&str> = key.split('/').collect();
        match parts[..] {
            ["runs", id, ref rest @ ..] if ids.contains(id) => {
                let dir = format!("runs/{id}");
                match rest {
                    ["run.json"] => run_files.insert(dir),
                    [first, _, ..] if WRITTEN_ONCE.contains(first) => once.insert(dir),
                    _ => rewritten.insert(dir),
                };
            }
            // Of a run that isn't exported, such as one before `since`; and the mark.
            ["runs", ..] | [MARK] => {}
            ["runs.json"] => list = true,
            [file] => {
                page_files.insert(file.to_owned());
            }
            [dir, ..] => {
                page_dirs.insert(dir.to_owned());
            }
            [] => unreachable!("`split` yields one part at least"),
        }
    }
    let sync = |dir: String, filter, cache, encoding| Upload {
        files: Files::Sync { dir, filter },
        cache,
        encoding,
    };
    let copy = |key: String, encoding| Upload {
        files: Files::Copy { key },
        cache: Cache::Mutable,
        encoding,
    };
    let mut plan = Vec::new();
    for dir in once {
        plan.push(sync(
            dir,
            Filter::WrittenOnce,
            Cache::Immutable,
            Encoding::Deflate,
        ));
    }
    for dir in page_dirs {
        plan.push(sync(dir, Filter::All, Cache::Immutable, Encoding::Identity));
    }
    for dir in rewritten {
        plan.push(sync(
            dir,
            Filter::Rewritten,
            Cache::Mutable,
            Encoding::Deflate,
        ));
    }
    for dir in run_files {
        plan.push(copy(format!("{dir}/run.json"), Encoding::Deflate));
    }
    for file in page_files {
        plan.push(copy(file, Encoding::Identity));
    }
    if list {
        plan.push(copy("runs.json".to_owned(), Encoding::Deflate));
    }
    Ok(plan)
}

/// The paths below `dir` of the files modified after `mark`, but for those still being written.
fn changed(dir: &Path, mark: SystemTime) -> Result<Vec<String>, String> {
    let mut changed = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(at) = dirs.pop() {
        for entry in fs::read_dir(&at).context(at.display())? {
            let entry = entry.context(at.display())?;
            let path = entry.path();
            let metadata = entry.metadata().context(path.display())?;
            if metadata.is_dir() {
                dirs.push(path);
                continue;
            }
            let modified = metadata.modified().context(path.display())?;
            let part = path.extension() == Some(OsStr::new(PART));
            if let Some(key) = path.strip_prefix(dir).ok().and_then(Path::to_str)
                && modified > mark
                && !part
            {
                changed.push(key.to_owned());
            }
        }
    }
    Ok(changed)
}
