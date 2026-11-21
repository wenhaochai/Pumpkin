//! The `pumpkin` command: `main` hands each parsed command to its module, and this file holds the
//! small commands and what the modules share to speak to the person at the terminal.
//!
//! What a command says about itself goes to the standard error as lines that start `pumpkin: `
//! ([`say`]); what it prints for a person or a program to read, such as a run's id or outcome,
//! goes to the standard output ([`print()`]).

use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;
use std::{env, fmt, fs};

use driver::ARGV0;
use engine::{ContextName, Engine};
use provider::UnixTime;
use runtime::{ReadError, Record};
use rustix::fs::Mode;
use rustix::process::umask;
use stdx::Context;
use workspace::Workspace;

mod bodies;
mod cli;
mod config;
mod controller;
mod driver;
mod ending;
mod host;
mod job;
mod link;
mod manifest;
mod member;
mod next;
mod progress;
mod publish;
mod reaping;
mod run;
mod sandboxd;
mod serve;
mod star;
mod style;
mod view;
mod whole;
mod workspace;

use ending::{end, follow};
use reaping::{Reaper, UNREACHED, cancel, cancelled, reap};
use run::{driver_run, run, start};

const MODAL_HELPER: &str = "pumpkin-modal.py";

// musl's allocator is much slower than glibc's at reading a run's records, as ripgrep found of its
// own work, and mimalloc builds with a C compiler alone, where jemalloc needs `make`:
// <https://github.com/BurntSushi/ripgrep/blob/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/core/main.rs#L19-L36>
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    stdx::abort_on_panic();
    // A run reads back what its record holds, such as a payload or a call's result, and the
    // operator trusts the outcome there; a umask such as 002 would leave all of it writable by a
    // group that may have other members. So every `pumpkin` process, and each program it starts,
    // makes nothing that another user may write, as a service does under systemd's default
    // `UMask=0022`.
    let shared_write = Mode::from_raw_mode(0o022);
    let started = umask(shared_write);
    umask(started | shared_write);
    let mut args = env::args_os();
    let argv0 = args.next();
    if argv0.is_some_and(|name| name.as_bytes() == ARGV0.to_bytes()) {
        return finish(driver_run(args.collect()));
    }
    let done = match cli::parse(args) {
        Ok(cli::Command::Run(flags)) => run(flags),
        Ok(cli::Command::Wait(flags)) => wait(flags),
        Ok(cli::Command::Ls(flags)) => ls(flags),
        Ok(cli::Command::Cancel(flags)) => cancel(flags),
        Ok(cli::Command::Reap(flags)) => reap(flags, Reaper::Command),
        Ok(cli::Command::Check(flags)) => check(flags),
        Ok(cli::Command::Fmt(flags)) => style::fmt(flags),
        Ok(cli::Command::Build(flags)) => build(flags, Reader::Operator),
        Ok(cli::Command::View(flags)) => view::view(flags),
        Ok(cli::Command::Job(flags)) => job::job(flags),
        Ok(cli::Command::Boot) => job::boot(),
        Ok(cli::Command::Install) => host::install(),
        Ok(cli::Command::BootAll) => host::boot_all(),
        Ok(cli::Command::Idle) => host::idle(),
        Ok(cli::Command::CancelFor(flags)) => job::cancel(flags),
        Ok(cli::Command::Serve(member)) => serve::serve(member.as_ref()),
        Ok(cli::Command::Publish(flags)) => publish::by_hand(flags),
        Ok(cli::Command::Print(text)) => print(text.as_bytes())
            .context("printing to the standard output failed")
            .map(|()| ExitCode::SUCCESS),
        Err(refusal) => return usage_error(refusal),
    };
    finish(done)
}

/// Says `refusal` as clap words it, then the command that prints the help it breaks, and returns
/// 2, clap's exit status for a usage error.
fn usage_error(refusal: cli::Refusal) -> ExitCode {
    let cli::Refusal { error, help } = refusal;
    to_stderr(error.render().to_string().as_bytes());
    say(format_args!("to read how to use it: {help}"));
    ExitCode::from(2)
}

fn finish(done: Result<ExitCode, String>) -> ExitCode {
    match done {
        Ok(exit) => exit,
        Err(message) => {
            say(message);
            ExitCode::FAILURE
        }
    }
}

/// Who reads what a command says of a run or a build: the operator, at this machine, or a
/// follower of a controller's job, at a client, who follows the run already, and can neither open
/// this machine's paths nor change its setup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader {
    Operator,
    Follower,
}

impl Reader {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Follower => "follower",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        [Self::Operator, Self::Follower]
            .into_iter()
            .find(|reader| reader.word() == word)
    }
}

/// The runs directory as a string, and `given` made absolute if one was given.
///
/// The second is what the commands to run next name; with the default directory they name none.
fn runs_dir(given: Option<String>) -> Result<(String, Option<String>), String> {
    match given {
        Some(runs) => {
            let named = absolute_utf8(&runs, "the runs directory")?;
            Ok((runs, Some(named)))
        }
        None => Ok((default_runs()?, None)),
    }
}

fn absolute_utf8(path: &str, what: &str) -> Result<String, String> {
    let absolute = std::path::absolute(path).map_err(|error| {
        format!("{what} `{path}` can't be made absolute ({error}): name it by its absolute path")
    })?;
    absolute
        .into_os_string()
        .into_string()
        .map_err(|absolute| not_utf8(what, path, Path::new(&absolute)))
}

fn not_utf8(what: &str, path: &str, absolute: &Path) -> String {
    format!(
        "{what} `{path}` made absolute is {}, which isn't UTF-8, as the current directory's \
         path isn't: name it by its absolute path",
        absolute.display()
    )
}

fn wait(flags: cli::Wait) -> Result<ExitCode, String> {
    let cli::Wait { run, runs, printed } = flags;
    if let (cli::RunName::Prefix(prefix), None) = (&run, &runs)
        && let Some(client) = config::client(None)?
    {
        // A run that has ended is copied here, and is read here; any other is waited for there.
        if let Ok(record) = Record::find(Path::new(&default_runs()?), prefix)
            && let Some(ending) = record.ending()?
        {
            let args = controller::here(&record, None)?;
            return end(&record, &ending, printed, None, &args);
        }
        return controller::wait(&client, prefix, printed);
    }
    let (record, named) = found(run, runs)?;
    follow(&record, "wait", named.as_deref(), printed)
}

fn ls(flags: cli::Ls) -> Result<ExitCode, String> {
    let cli::Ls { runs, prefixes } = flags;
    if runs.is_none()
        && let Some(client) = config::client(None)?
    {
        return controller::ls(&client, &prefixes);
    }
    let (runs, named) = runs_dir(runs)?;
    let now = unix_now();
    let since = progress::listed_since(&prefixes, now);
    let (listing, unlisted) = listed_in(Path::new(&runs), &prefixes, since, now, named.as_deref())?;
    let lines = progress::lines(&listing, named.as_deref());
    print(lines.as_bytes()).context("printing the runs failed")?;
    if unlisted > 0 {
        let runs = counted(unlisted, "run");
        return Err(format!("{runs} can't be listed, as said above"));
    }
    Ok(ExitCode::SUCCESS)
}

/// The time `since_epoch` after the Unix epoch, or the latest time jiff holds if that is past it.
fn utc(since_epoch: std::time::Duration) -> jiff::Timestamp {
    let at = jiff::Timestamp::UNIX_EPOCH.saturating_add(since_epoch);
    at.unwrap_or(jiff::Timestamp::MAX)
}

const EPOCH: UnixTime = UnixTime::after_epoch(std::time::Duration::ZERO);

fn unix_now() -> UnixTime {
    UnixTime::of(SystemTime::now()).unwrap_or(EPOCH)
}

/// The runs of `runs` whose ids start with one of `prefixes`, or every run if there are none, and
/// how many can't be listed, each said as it is found. A run that ended before `since` is listed
/// as older, and one that another build made and this can't read is an `unread` row. Each row is
/// made at `now`, with its commands naming `named` as the runs directory. A `runs` that is missing
/// is an empty listing unless `named` names it.
fn listed_in(
    runs: &Path,
    prefixes: &[String],
    since: Option<UnixTime>,
    now: UnixTime,
    named: Option<&str>,
) -> Result<(progress::Listing, usize), String> {
    let mut listing = progress::Listing::default();
    if named.is_none() && !runs.try_exists().context(runs.display())? {
        return Ok((listing, 0));
    }
    let mut unlisted = 0;
    for (id, record) in Record::all(runs)? {
        if !picked(prefixes, &id) {
            continue;
        }
        let listed = record
            .and_then(|record| {
                if let Some(since) = since
                    && ended(record.dir())?.is_some_and(|ended| ended < since)
                {
                    return Ok(None);
                }
                progress::Listed::run(&record, record.ending()?).map(Some)
            })
            .or_else(|error| {
                let dir = runs.join(&id);
                if let Some(since) = since
                    && ended(&dir)?.is_some_and(|ended| ended < since)
                {
                    return Ok(None);
                }
                progress::Listed::unread(&dir, error).map(Some)
            });
        match listed {
            Ok(Some(listed)) => {
                let row = progress::Row::of(&id, &listed, now, named);
                listing.runs.insert(id, progress::Entry::Row(row));
            }
            Ok(None) => {
                listing.runs.insert(id, progress::Entry::Older);
            }
            Err(error) => {
                say(error);
                unlisted += 1;
            }
        }
    }
    Ok((listing, unlisted))
}

fn ended(dir: &Path) -> Result<Option<UnixTime>, ReadError> {
    let outcome = dir.join("outcome.json");
    let modified = fs::metadata(&outcome).and_then(|metadata| metadata.modified());
    match modified {
        Ok(modified) => Ok(Some(UnixTime::of(modified).unwrap_or(EPOCH))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ReadError::Failed(format!("{}: {error}", outcome.display()))),
    }
}

fn picked(prefixes: &[String], id: &str) -> bool {
    prefixes.is_empty()
        || prefixes
            .iter()
            .any(|prefix| id.starts_with(prefix.as_str()))
}

/// The record that `run` names, and the runs directory that the commands to run next name for it
/// ([`runs_dir`]'s second).
fn found(run: cli::RunName, runs: Option<String>) -> Result<(Record, Option<String>), String> {
    let path = match run {
        cli::RunName::Record(path) => path,
        cli::RunName::Prefix(prefix) => {
            let (dir, named) = runs_dir(runs)?;
            return Ok((Record::find(Path::new(&dir), &prefix)?, named));
        }
    };
    // A path needs no runs directory, and so no `HOME`.
    let record = Record::open(Path::new(&path))?;
    // A record's directory is absolute, and named by its id, so it has a parent; a UTF-8 one is
    // what the commands that follow rely on.
    match record.dir().parent().and_then(Path::to_str) {
        Some(named) => {
            let named = named.to_owned();
            Ok((record, Some(named)))
        }
        None => Err(not_utf8("the record", &path, record.dir())),
    }
}

fn check(flags: cli::Check) -> Result<ExitCode, String> {
    let cli::Check {
        entries,
        args: cli::TaskArgs(args),
    } = flags;
    let formatter = style::Formatter::beside()?;
    let mut failed = Vec::new();
    for entry in &entries {
        let checked = match star::check(entry, &args, &formatter) {
            Ok(Ok(())) => continue,
            Ok(Err(problems)) => task_problems(entry, &problems),
            Err(error) => format!("{entry}: {error}"),
        };
        say(checked);
        failed.push(entry);
    }
    if failed.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let again = source_again("check", &failed, &args, None)?;
    let them = if failed.len() == 1 { "it" } else { "them" };
    Err(format!("to check {them} again: {again}"))
}

fn build(flags: cli::Build, reader: Reader) -> Result<ExitCode, String> {
    if let Some(client) = config::client(flags.config.as_deref())? {
        return controller::build(&client, flags);
    }
    let cli::Build {
        entry,
        args: cli::TaskArgs(args),
        config,
    } = flags;
    let again = source_again("build", &[&entry], &args, config.as_deref())?;
    match built(&entry, &args, config, reader) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) => {
            say(error);
            Err(format!("to build it again: {again}"))
        }
    }
}

fn built(
    entry: &star::Entry,
    args: &lang::Args,
    config: Option<String>,
    reader: Reader,
) -> Result<(), String> {
    let built = star::checked(entry, args, |task| {
        let engine = configured_engine(&config_path(config)?, reader)?;
        Ok(task.build(&engine, &mut ()))
    })?;
    let (workflow, _, _) = built.map_err(|problems| task_problems(entry, &problems))?;
    let ir = runtime::ir_json(&workflow);
    print(ir.as_bytes()).context("printing the IR failed")
}

fn source_again(
    command: &str,
    entries: &[&star::Entry],
    args: &lang::Args,
    config: Option<&str>,
) -> Result<String, String> {
    let mut flags = arg_flags(args).collect::<Vec<String>>();
    if let Some(config) = config {
        let config = absolute_utf8(config, "the configuration")?;
        flags.extend(["--config".to_owned(), config]);
    }
    let entries: Result<Vec<String>, String> = (entries.iter())
        .map(|entry| absolute_utf8(entry.path(), "the entry"))
        .collect();
    Ok(next::again(command, &flags, &entries?))
}

fn arg_flags(args: &lang::Args) -> impl Iterator<Item = String> {
    let flags = args
        .iter()
        .map(|(name, value)| ["--arg".to_owned(), format!("{name}={value}")]);
    flags.flatten()
}

fn task_problems(entry: &star::Entry, problems: &lang::Problems) -> String {
    // Each problem reads as its first line names its place and those after show it, so only the
    // line that closes them is said as `pumpkin`'s.
    to_stderr(format!("{problems}\n").as_bytes());
    closing_problems(format_args!("{entry}: the task has"), problems.count())
}

/// `text`, which a sandbox may have shaped, on one line for a terminal: each control character
/// but a tab escaped, a newline as `\n`.
fn escaped(text: &str) -> String {
    let mut shown = String::new();
    for c in text.chars() {
        // Control characters would steer the operator's terminal.
        if c.is_control() && c != '\t' {
            shown.extend(c.escape_debug());
        } else {
            shown.push(c);
        }
    }
    shown
}

fn default_runs() -> Result<String, String> {
    let runs = Workspace::find()?.runs();
    runs.into_os_string().into_string().map_err(|runs| {
        format!(
            "the workspace's runs directory {} isn't UTF-8, as the commands that `pumpkin` says \
             to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory \
             with `--runs`",
            Path::new(&runs).display()
        )
    })
}

fn config_path(given: Option<String>) -> Result<PathBuf, String> {
    given.map_or_else(default_config, |config| Ok(PathBuf::from(config)))
}

fn configured_engine(path: &Path, reader: Reader) -> Result<Engine, String> {
    let config::Config {
        docker: config::Docker { context, cpus: _ },
        providers: _,
        gateway: _,
        upstreams: _,
        publish: _,
    } = config::read(path)?;
    engine(context, reader)
}

fn default_config() -> Result<PathBuf, String> {
    Ok(home("name the configuration with `--config`")?.join(".config/pumpkin/pumpkin.toml"))
}

/// The home directory, `HOME`; failing without one, the error says what the operator can do
/// `instead`.
fn home(instead: &str) -> Result<PathBuf, String> {
    // From `HOME` alone, not `XDG_CONFIG_HOME`: a run already reads `HOME`, for `docker`, so
    // where its configuration is takes no variable of its own.
    let home = env::var_os("HOME").filter(|home| !home.is_empty());
    home.map(PathBuf::from)
        .ok_or_else(|| format!("HOME is unset or empty: set it, or {instead}"))
}

fn modal_helper() -> Result<PathBuf, String> {
    let pumpkin = workspace::exe("expected Modal's helper beside it")?;
    let helper = pumpkin.with_file_name(MODAL_HELPER);
    let lock = pumpkin.with_file_name(format!("{MODAL_HELPER}.lock"));
    for file in [&helper, &lock] {
        if !file.is_file() {
            return Err(format!(
                "{} doesn't exist: expected Modal's helper and its lock beside `pumpkin`, as \
                 `cargo xtask build` puts them",
                file.display()
            ));
        }
    }
    Ok(helper)
}

fn closing_problems(has: fmt::Arguments<'_>, count: usize) -> String {
    let problems = counted(count, "problem");
    format!("{has} {problems}")
}

/// `count` and `noun`, plural unless `count` is 1, as "1 problem", "2 problems", "2 entries" or
/// "2 sandboxes": every count of a thing that Pumpkin says is said so.
fn counted<C: Copy + fmt::Display + PartialEq + From<u8>>(count: C, noun: &str) -> String {
    if count == C::from(1) {
        return format!("{count} {noun}");
    }
    let consonant_y = noun
        .strip_suffix('y')
        .filter(|stem| !stem.ends_with(['a', 'e', 'o', 'u']));
    match consonant_y {
        Some(stem) => format!("{count} {stem}ies"),
        None if noun.ends_with(['s', 'x']) || noun.ends_with("sh") || noun.ends_with("ch") => {
            format!("{count} {noun}es")
        }
        None => format!("{count} {noun}s"),
    }
}

fn engine(context: ContextName, reader: Reader) -> Result<Engine, String> {
    let engine = Engine::connect(context, &Workspace::find()?.docker())?;
    let report = engine.report();
    // A rootful engine's `docker` group is root-equivalent; a follower can't change the engine.
    if reader == Reader::Operator && !report.security.rootless {
        say(format_args!(
            "the Docker engine that context `{}` reaches runs as root, so its `docker` group is \
             root-equivalent: rootless Docker is recommended",
            report.context.as_str()
        ));
    }
    Ok(engine)
}

fn print(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

fn advise(to: &str, command: &str) {
    say(format_args!("to {to}: {command}"));
}

/// Writes `message` as a line on the standard error, and ignores a failure to write it.
fn say(message: impl fmt::Display) {
    to_stderr(format!("pumpkin: {message}\n").as_bytes());
}

fn to_stderr(bytes: &[u8]) {
    // Writing to a terminal that has hung up fails, and `eprintln!` would panic; the exit
    // status and the run's record still say how it ended.
    let _ = io::stderr().write_all(bytes);
}
