//! `cargo xtask live examples`: a run of each shipped example through the controller, with real
//! models, from a task repository made of `examples/` at `HEAD`, as the examples' READMEs say to
//! make one, each checked as its README says. A release is made only of a commit on which every
//! run passed, which the commit status [`gate::CONTEXT`] records on GitHub.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{head, install, left, made, probes, pumpkin, reached, settings, target_directory};
use crate::settings::Settings;
use crate::{Cargo, Result, gate, wait};

/// How many runs go at once: the native benchmark's, which runs longest, and two others.
const AT_ONCE: usize = 3;

/// How often the runs that go are looked at, to start the next once one has ended.
const POLL: Duration = Duration::from_secs(5);

/// The native benchmark's binding, below `examples/`, whose `source` names the `repository` and
/// `revision` that its inputs are prepared from.
const BINDING: &str = "hypergraph-benchmark/binding.json";

/// How a run is checked once it has finished with every sandbox deleted.
enum Check {
    /// By the integers that its final call returned under these names, each of which must be the
    /// one given.
    Equal(&'static [(&'static str, i64)]),
    /// By the integer that its final call returned under this name, which must be above 0.
    Positive(&'static str),
    /// By the script below `examples/`, given the record and then these arguments.
    Script(&'static str, &'static [&'static str]),
    /// By the smoke's probes, of its sandboxes on CPUs.
    Probes,
}

/// A run of an example: its name, its entry below `examples/`, its `--arg` values and its check.
struct Example {
    name: &'static str,
    task: &'static str,
    args: Vec<String>,
    check: Check,
}

/// The runs, the native benchmark's first, as it runs longest. Those of a model run on `model`.
fn entries(model: &str) -> Vec<Example> {
    let model = format!("model={model}");
    let example = |name, task, args: &[&str], check| Example {
        name,
        task,
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        check,
    };
    let team = "team/verify.py";
    // `echo hello` writes 6 bytes, a line.
    let hello = Check::Equal(&[("score", 6), ("lines", 1)]);
    vec![
        example(
            "native",
            "hypergraph-benchmark/task.star",
            &[],
            Check::Script("hypergraph-benchmark/verify.py", &[]),
        ),
        example("hello", "hello/task.star", &[], hello),
        example(
            "codex",
            "codex/task.star",
            &[&model],
            Check::Positive("score"),
        ),
        example(
            "hypergraph",
            "hypergraph/task.star",
            &[&model],
            Check::Script("hypergraph/verify.py", &[]),
        ),
        example("team", "team/task.star", &[], Check::Script(team, &[])),
        example(
            "team-omit-bob",
            "team/task.star",
            &["omit=bob"],
            Check::Script(team, &["--omit", "bob"]),
        ),
        example(
            "codex-team",
            "team/codex.star",
            &[&model],
            Check::Script(team, &[]),
        ),
        example("smoke", "smoke/task.star", &[], Check::Probes),
    ]
}

/// A run that goes, followed by `./pumpkin run --wait` into its log.
struct Going {
    index: usize,
    example: Example,
    log: PathBuf,
    child: Child,
    started: Instant,
}

/// How a run ended, as its summary line says it.
struct Ran {
    index: usize,
    name: &'static str,
    id: Option<String>,
    /// The `state` of its `outcome.json`.
    state: String,
    checked: &'static str,
    elapsed: Duration,
    usd: Option<f64>,
    failed: Vec<String>,
}

/// Runs each shipped example at `HEAD` through the controller of `config`, or of `pumpkin`'s
/// default configuration, those of a model on `model`, and fails unless each run finished, its
/// checker passed, every sandbox of it was deleted and the providers hold nothing of it. With
/// `status`, posts how they ended as the commit status [`gate::CONTEXT`] of `HEAD` on GitHub.
pub(crate) fn examples(
    cargo: &Cargo,
    config: Option<&Path>,
    model: &str,
    status: bool,
) -> Result<()> {
    let (branch, commit) = head(cargo)?;
    let config = config
        .map(std::path::absolute)
        .transpose()
        .map_err(|error| format!("making the configuration's path absolute failed: {error}"))?;
    let settings = settings(config.as_deref(), None, None)?;
    reached(cargo, &settings)?;
    if status {
        let github = format!("repos/{{owner}}/{{repo}}/commits/{commit}");
        let found = cargo.sh.cmd("gh").args(["api", &github]).quiet().read();
        found.map_err(|error| {
            format!(
                "{error}: push {commit} to GitHub, and give `gh` a token that may post its \
                 statuses, as `gh auth login` does, then run this again"
            )
        })?;
    }
    // The native benchmark's checker reads its record's compressed files with it.
    let zstd = cargo.sh.cmd("zstd").arg("--version").quiet().read();
    zstd.map_err(|error| format!("{error}: install zstd, and run this again"))?;
    let (repo, logs) = repository(cargo, &commit)?;
    eprintln!(
        "xtask: running the examples at {commit} ({branch}) through the controller, {AT_ONCE} at \
         once, each followed in its log in {}",
        logs.display()
    );
    let started = Instant::now();
    let mut ran = run(cargo, &repo, &logs, config.as_deref(), entries(model));
    ran.sort_by_key(|ran| ran.index);
    for ran in &mut ran {
        leftovers(cargo, &settings, &repo, ran);
    }
    let (failed, state, description) = summary(&ran, started.elapsed());
    let posted = if status {
        post(cargo, &commit, state, &description)
    } else {
        Ok(())
    };
    if !failed.is_empty() {
        let posted = match posted {
            Ok(()) if status => format!("; posted `{}` as failed", gate::CONTEXT),
            Ok(()) => String::new(),
            Err(error) => format!("; posting `{}` failed: {error}", gate::CONTEXT),
        };
        return Err(format!(
            "{}{posted}: fix it, and run `cargo xtask live examples` again",
            failed.join("; ")
        )
        .into());
    }
    posted?;
    println!(
        "xtask: every run finished, its checker passed, and nothing of it is left{}",
        if status { "; posted it on GitHub" } else { "" }
    );
    Ok(())
}

/// Makes the task repository of the runs at `commit`, below the target directory, a Git
/// repository of `examples/` at `commit` with `pumpkin` built from this checkout and the native
/// benchmark's inputs prepared, and returns its path and that of the directory of their logs.
fn repository(cargo: &Cargo, commit: &str) -> Result<(PathBuf, PathBuf)> {
    let live = target_directory(cargo)?.join("live");
    let dir = live.join("examples").join(commit);
    if dir.exists() {
        return Err(format!(
            "{} holds the runs of an earlier `cargo xtask live examples` of {commit}: remove it, \
             and run this again",
            dir.display()
        )
        .into());
    }
    let (repo, logs) = (dir.join("repo"), dir.join("logs"));
    fs::create_dir_all(&logs).map_err(|error| made(&logs, &error))?;
    let git = cargo.sh.cmd("git").args(["init", "--quiet"]).arg(&repo);
    git.run()?;
    let archive = cargo.sh.cmd("git");
    let archive = archive.args(["archive", "--format=tar", commit, "examples"]);
    let archive = archive.output()?.stdout;
    let extract = cargo.sh.cmd("tar").args(["--extract", "--directory"]);
    extract.arg(&repo).stdin(archive).run()?;
    install(cargo, &repo)?;
    prepare(cargo, &repo, &live.join("benchmark-source"))?;
    Ok((repo, logs))
}

/// Prints a line of each of `ran`, which took `elapsed`, and of their total, and returns what
/// failed and the commit status that says how they ended, its state and description.
fn summary(ran: &[Ran], elapsed: Duration) -> (Vec<String>, &'static str, String) {
    for ran in ran {
        println!(
            "xtask: {:<13} {:<24} {:<11} checker {:<9} {:>8} {:>10}",
            ran.name,
            ran.id.as_deref().unwrap_or("-"),
            ran.state,
            ran.checked,
            shown(ran.elapsed),
            ran.usd.map_or("-".to_owned(), |usd| format!("${usd:.4}")),
        );
    }
    let usd: f64 = ran.iter().filter_map(|ran| ran.usd).sum();
    let failed: Vec<String> = (ran.iter())
        .flat_map(|ran| (ran.failed.iter()).map(|failed| format!("{}: {failed}", ran.name)))
        .collect();
    let (count, failed_count) = (
        ran.len(),
        ran.iter().filter(|ran| !ran.failed.is_empty()).count(),
    );
    println!(
        "xtask: {} of {count} runs passed, in {}, ${usd:.4} metered",
        count - failed_count,
        shown(elapsed)
    );
    let native = (ran.iter())
        .find(|ran| ran.name == "native")
        .and_then(|ran| ran.id.as_deref())
        .unwrap_or("none");
    let (state, ended) = if failed.is_empty() {
        ("success", format!("{count} runs passed"))
    } else {
        ("failure", format!("{failed_count} of {count} runs failed"))
    };
    (
        failed,
        state,
        format!("{ended}, ${usd:.2} metered; native run {native}"),
    )
}

/// Fetches the native benchmark's public source, which its binding names, into `source`, cloned
/// there blobless once, and prepares its inputs in `repo` from it.
fn prepare(cargo: &Cargo, repo: &Path, source: &Path) -> Result<()> {
    let examples = repo.join("examples");
    let path = examples.join(BINDING);
    let binding = fs::read_to_string(&path)
        .map_err(|error| format!("reading {} failed: {error}", path.display()))?;
    let binding: serde_json::Value = serde_json::from_str(&binding)
        .map_err(|error| format!("{} isn't JSON: {error}", path.display()))?;
    let named = |key: &str| {
        binding["source"][key]
            .as_str()
            .ok_or_else(|| format!("{}'s `source` names no `{key}`: name it", path.display()))
    };
    let (repository, revision) = (named("repository")?, named("revision")?);
    if !source.exists() {
        let clone = cargo.sh.cmd("git").args([
            "clone",
            "--quiet",
            "--filter=blob:none",
            "--no-checkout",
            "--depth=1",
            repository,
        ]);
        clone.arg(source).run()?;
    }
    // Git fetches each blob that preparation reads as it reads it.
    let fetch = cargo.sh.cmd("git").arg("-C").arg(source);
    let fetch = fetch.args([
        "fetch",
        "--quiet",
        "--filter=blob:none",
        "--depth=1",
        "origin",
    ]);
    fetch.arg(revision).run()?;
    let prepare = examples.join("hypergraph-benchmark/prepare.py");
    // `-B`, so that no bytecode joins the files of the task repository that Git lists.
    let prepare = cargo.sh.cmd("python3").arg("-B").arg(prepare).arg(source);
    prepare.run()?;
    Ok(())
}

/// Runs `examples` in `repo`, [`AT_ONCE`] at a time, in their order, through the controller of
/// `config`, each followed into its log in `logs`, and returns how each ended, in the order they
/// ended.
fn run(
    cargo: &Cargo,
    repo: &Path,
    logs: &Path,
    config: Option<&Path>,
    examples: Vec<Example>,
) -> Vec<Ran> {
    let mut waiting = examples.into_iter().enumerate();
    let mut going: Vec<Going> = Vec::new();
    let mut ran = Vec::new();
    loop {
        while going.len() < AT_ONCE
            && let Some((index, example)) = waiting.next()
        {
            let log = logs.join(format!("{}.log", example.name));
            let started = Instant::now();
            match spawn(repo, &log, config, &example) {
                Ok(child) => {
                    eprintln!(
                        "xtask: started {}, followed in {}",
                        example.name,
                        log.display()
                    );
                    going.push(Going {
                        index,
                        example,
                        log,
                        child,
                        started,
                    });
                }
                Err(error) => ran.push(Ran {
                    index,
                    name: example.name,
                    id: None,
                    state: "unstarted".to_owned(),
                    checked: "not run",
                    elapsed: started.elapsed(),
                    usd: None,
                    failed: vec![error],
                }),
            }
        }
        if going.is_empty() {
            return ran;
        }
        thread::sleep(POLL);
        let mut index = 0;
        while index < going.len() {
            let exited = match going[index].child.try_wait() {
                Ok(None) => {
                    index += 1;
                    continue;
                }
                Ok(Some(status)) => Ok(status),
                Err(error) => Err(format!("waiting for `./pumpkin run` failed: {error}")),
            };
            ran.push(finish(cargo, repo, going.swap_remove(index), exited));
        }
    }
}

/// Spawns `./pumpkin run --wait` of `example` in `repo`, with what it says going to `log`.
fn spawn(
    repo: &Path,
    log: &Path,
    config: Option<&Path>,
    example: &Example,
) -> std::result::Result<Child, String> {
    let file = File::create(log).and_then(|file| Ok((file.try_clone()?, file)));
    let (stdout, stderr) = file.map_err(|error| made(log, &error))?;
    let mut run = pumpkin(repo);
    run.args(["run", "--wait"]);
    if let Some(config) = config {
        run.arg("--config").arg(config);
    }
    for arg in &example.args {
        run.args(["--arg", arg]);
    }
    run.arg(Path::new("examples").join(example.task));
    run.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    run.spawn()
        .map_err(|error| format!("running `./pumpkin run` failed: {error}"))
}

/// How `going` ended, once `./pumpkin run --wait` has exited as `exited` says: its record read
/// and checked, and its checker run, which adds what it says to the log.
fn finish(
    cargo: &Cargo,
    repo: &Path,
    going: Going,
    exited: std::result::Result<ExitStatus, String>,
) -> Ran {
    let Going {
        index,
        example,
        log,
        child: _,
        started,
    } = going;
    let elapsed = started.elapsed();
    let mut failed = Vec::new();
    match exited {
        Ok(status) if status.success() => {}
        Ok(status) => failed.push(format!(
            "`./pumpkin run` ended with {status}, as {} says",
            log.display()
        )),
        Err(error) => failed.push(error),
    }
    let said = fs::read_to_string(&log).unwrap_or_else(|error| {
        failed.push(format!("reading {} failed: {error}", log.display()));
        String::new()
    });
    let id = said.lines().find_map(wait::run_started).map(str::to_owned);
    let Some(run) = &id else {
        failed.push(format!("it started no run, as {} says", log.display()));
        return Ran {
            index,
            name: example.name,
            id,
            state: "unstarted".to_owned(),
            checked: "not run",
            elapsed,
            usd: None,
            failed,
        };
    };
    let record = repo.join(".pumpkin/runs").join(run);
    let (outcome, problems) = super::ended(&record);
    failed.extend(problems);
    let checked = check(
        cargo,
        repo,
        &example.check,
        &record,
        &outcome,
        &log,
        &mut failed,
    );
    Ran {
        index,
        name: example.name,
        id,
        state: outcome["state"].as_str().unwrap_or("unread").to_owned(),
        checked,
        elapsed,
        usd: outcome["usage"]["usd"].as_f64(),
        failed,
    }
}

/// Checks the record at `record`, whose `outcome.json` is `outcome`, by `check`, adding what is
/// wrong to `failed` and what a checker says to `log`, and returns whether it passed, as the
/// summary says it.
fn check(
    cargo: &Cargo,
    repo: &Path,
    check: &Check,
    record: &Path,
    outcome: &serde_json::Value,
    log: &Path,
    failed: &mut Vec<String>,
) -> &'static str {
    let returned = |name: &str| {
        let values = outcome["verdict"]["result"]["values"].as_array();
        let value = (values.into_iter().flatten()).find(|value| value["name"] == name);
        value.and_then(|value| value["value"].as_i64())
    };
    match check {
        Check::Equal(expected) => {
            let before = failed.len();
            for &(name, expected) in *expected {
                let got = returned(name);
                if got != Some(expected) {
                    let got = got.map_or("nothing".to_owned(), |got| got.to_string());
                    failed.push(format!("its `{name}` is {got}: expected {expected}"));
                }
            }
            if failed.len() == before {
                "passed"
            } else {
                "failed"
            }
        }
        Check::Positive(name) => match returned(name) {
            Some(got) if got > 0 => "passed",
            got => {
                let got = got.map_or("nothing".to_owned(), |got| got.to_string());
                failed.push(format!("its `{name}` is {got}: expected one above 0"));
                "failed"
            }
        },
        Check::Probes => {
            let problems = probes(outcome, false);
            let checked = if problems.is_empty() {
                "passed"
            } else {
                "failed"
            };
            failed.extend(problems);
            checked
        }
        Check::Script(script, args) => {
            let script = repo.join("examples").join(script);
            match checker(cargo, &script, record, args, log) {
                Ok(()) => "passed",
                Err(error) => {
                    failed.push(error);
                    "failed"
                }
            }
        }
    }
}

/// Adds to what `ran` failed what of its run the providers that `settings` reach still hold, as
/// [`left`] finds it, each of which `./pumpkin cancel` in `repo` removes.
fn leftovers(cargo: &Cargo, settings: &Settings, repo: &Path, ran: &mut Ran) {
    let Some(id) = &ran.id else {
        return;
    };
    match left(cargo, settings, id) {
        Ok(left) if left.is_empty() => {}
        Ok(left) => ran.failed.push(format!(
            "left after the run: {}; remove it with `./pumpkin cancel {id}` in {}",
            left.join(", "),
            repo.display()
        )),
        Err(error) => ran
            .failed
            .push(format!("finding what is left of run {id} failed: {error}")),
    }
}

/// Runs `script` on the record at `record`, with `args` after it, adding what it says to `log`.
fn checker(
    cargo: &Cargo,
    script: &Path,
    record: &Path,
    args: &[&str],
    log: &Path,
) -> std::result::Result<(), String> {
    let file = File::options()
        .append(true)
        .open(log)
        .and_then(|file| Ok((file.try_clone()?, file)));
    let (stdout, stderr) = file.map_err(|error| made(log, &error))?;
    let check = cargo.sh.cmd("python3").arg("-B").arg(script).arg(record);
    let check = check.args(args);
    let line = check.to_string();
    let mut check = Command::from(check);
    let status = check
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .status();
    let status = status.map_err(|error| format!("running `{line}` failed: {error}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "`{line}` ended with {status}, as {} says",
        log.display()
    ))
}

/// Posts `state` with `description` as the commit status [`gate::CONTEXT`] of `commit`.
fn post(cargo: &Cargo, commit: &str, state: &str, description: &str) -> Result<()> {
    let statuses = format!("repos/{{owner}}/{{repo}}/statuses/{commit}");
    let post = cargo
        .sh
        .cmd("gh")
        .args(["api", "--method", "POST", &statuses]);
    let post = post.args(["--raw-field", &format!("state={state}")]);
    let post = post.args(["--raw-field", &format!("context={}", gate::CONTEXT)]);
    let post = post.args(["--raw-field", &format!("description={description}")]);
    post.quiet().read()?;
    Ok(())
}

/// `elapsed` in minutes and seconds, as `86m52s`.
fn shown(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}
