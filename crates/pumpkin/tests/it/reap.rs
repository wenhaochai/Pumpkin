//! `reap`: what it removes of the runs that have ended, or are lost, and what it leaves.

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::Path;
use std::process::Stdio;

use expect_test::expect;
use serde_json::json;

use super::{
    EC2, INFO, LOCAL, Scratch, Step, check, docker_table, invocation, poll, ran, report, returned,
    write_docker_record, write_record,
};

/// A lost run, whose turn the test holds as the first reap begins.
const TURN: &str = "20261108-115900-0a000000";

/// A lost run, which left its socket directory.
const LOST: &str = "20261108-120000-0a000001";

/// A lost run, which left nothing else.
const BARE: &str = "20261108-120100-0a000002";

/// A run that finished, and left nothing.
const FINISHED: &str = "20261108-120200-0a000003";

/// A run whose `outcome.json` is not JSON.
const DAMAGED: &str = "20261108-120210-0a00000e";

/// A live run, whose lock the test holds.
const LIVE: &str = "20261108-120300-0a000004";

/// A record made before records were locked.
const UNLOCKED: &str = "20261108-120400-0a000005";

/// A failed run whose Docker provider cannot be reached.
const UNREACHED: &str = "20261108-120500-0a000006";

/// A lost record whose name is no run tag.
const BAD_NAME: &str = "lost name";

/// A run that failed on an engine that can't enforce every limit, and left a container there.
const UNFIT: &str = "20261108-120600-0a000007";

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the records, the steps and their snapshot"
)]
fn reaps() {
    let scratch = Scratch::new();
    write_record(&scratch, TURN, true, None);
    let turn = scratch.0.join(format!("runs/{TURN}"));
    let held = File::open(&turn).unwrap();
    held.lock().unwrap();
    write_record(&scratch, LOST, true, None);
    write_record(&scratch, BARE, true, None);
    let finished = returned(1.0);
    write_record(&scratch, FINISHED, true, Some(finished));
    write_record(&scratch, DAMAGED, true, None);
    scratch.write(&format!("runs/{DAMAGED}/outcome.json"), "{");
    write_record(&scratch, LIVE, true, None);
    let live = File::open(scratch.0.join(format!("runs/{LIVE}/driver.pid"))).unwrap();
    live.lock().unwrap();
    write_record(&scratch, UNLOCKED, false, None);
    let failed = json!({ "state": "failed", "reason": "r" });
    write_docker_record(&scratch, UNREACHED, "recorded", Some(failed));
    write_record(&scratch, BAD_NAME, true, None);
    scratch.write("runs/notes/todo", "");
    scratch.write(&format!("runs/{LOST}/sockets/agent-0/s"), "");
    let reap = |runs: &str| {
        let mut reap = scratch.pumpkin(&["reap", "--runs", runs]);
        reap.env("PATH", "");
        ran(reap, &scratch.0, false)
    };
    let lost = scratch.0.join(format!("runs/{LOST}/outcome.json"));
    let mut first = scratch.pumpkin(&["reap", "--runs", "runs"]);
    first
        .env("PATH", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let invoked = invocation(&first);
    let child = first.spawn().unwrap();
    poll(|| Some(()).filter(|()| locks(child.id(), &turn) == [true]));
    let pid = locks(child.id(), &turn.join("driver.pid"));
    let waited = format!(
        "while the test holds {TURN}'s turn, reap waits for it, holding {} locks on its \
         driver.pid, and {LOST} has an outcome.json: {}",
        pid.len(),
        lost.exists()
    );
    drop(held);
    let output = child.wait_with_output().unwrap();
    let mut steps = vec![waited, report(invoked, &output, &scratch.0, false)];
    let left = |id| {
        let outcome = scratch.0.join(format!("runs/{id}/outcome.json"));
        let socket = scratch.0.join(format!("runs/{id}/sockets"));
        format!(
            "{id}: outcome.json: {}; socket directory: {}",
            fs::read_to_string(outcome)
                .unwrap_or_else(|_| "none".to_owned())
                .trim(),
            socket.exists()
        )
    };
    steps.extend([LOST, LIVE, BAD_NAME, DAMAGED].map(left));
    let wait = scratch.pumpkin(&["wait", LOST, "--runs", "runs"]);
    steps.push(ran(wait, &scratch.0, false));
    for unreaped in [UNLOCKED, BAD_NAME, DAMAGED] {
        fs::remove_dir_all(scratch.0.join(format!("runs/{unreaped}"))).unwrap();
    }
    steps.push(reap("runs"));
    fs::remove_dir_all(scratch.0.join(format!("runs/{UNREACHED}"))).unwrap();
    steps.push(reap("runs"));
    steps.push(reap("nowhere"));
    drop(live);
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            while the test holds 20261108-115900-0a000000's turn, reap waits for it, holding 0 locks on its driver.pid, and 20261108-120000-0a000001 has an outcome.json: false
            $ PATH= pumpkin reap --runs runs
            exit status: Some(1)
            pumpkin: run 20261108-115900-0a000000 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120000-0a000001 left its socket directory, now removed: <scratch>/runs/20261108-120000-0a000001/sockets
            pumpkin: run 20261108-120000-0a000001 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120100-0a000002 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120210-0a00000e can't be reaped: <scratch>/runs/20261108-120210-0a00000e/outcome.json: EOF while parsing an object at line 1 column 1
            pumpkin: <scratch>/runs/20261108-120400-0a000005/driver.pid isn't there: expected the record of a run to have it
            pumpkin: run 20261108-120500-0a000006's sandboxes on provider `local`, if it left any, can't be removed, as it can't be reached: the Docker context `recorded` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120500-0a000006, to stop reaping it
            pumpkin: run lost name can't be reaped: the record <scratch>/runs/lost name is named by no run tag (the run tag `lost name` holds ` `: use only ASCII letters, digits, `_`, `.` and `-`): expected a record that `pumpkin run` made
            pumpkin: 3 runs can't be reaped, as said above: fix what each says, and reap again
            pumpkin: to reap again: pumpkin reap --runs <scratch>/runs
            printed:
              20261108-115900-0a000000
              20261108-120000-0a000001
              20261108-120100-0a000002
            20261108-120000-0a000001: outcome.json: {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }; socket directory: false
            20261108-120300-0a000004: outcome.json: none; socket directory: false
            lost name: outcome.json: none; socket directory: false
            20261108-120210-0a00000e: outcome.json: {; socket directory: false
            $ pumpkin wait 20261108-120000-0a000001 --runs runs
            exit status: Some(1)
            pumpkin: run 20261108-120000-0a000001 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120000-0a000001
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ PATH= pumpkin reap --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120500-0a000006's sandboxes on provider `local`, if it left any, can't be removed, as it can't be reached: the Docker context `recorded` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120500-0a000006, to stop reaping it
            pumpkin: 1 run ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --runs <scratch>/runs
            $ PATH= pumpkin reap --runs runs
            exit status: Some(0)
            $ PATH= pumpkin reap --runs nowhere
            exit status: Some(1)
            pumpkin: nowhere: No such file or directory (os error 2): expected the directory that runs are recorded in, as `--runs` names it"#]],
    );
}

fn write_docker(scratch: &Scratch, docker: &str) -> String {
    scratch.write_executable("bin/docker", docker.as_bytes());
    format!("{}:/usr/bin:/bin", scratch.0.join("bin").display())
}

#[test]
fn reaps_unfit() {
    let scratch = Scratch::new();
    let info = INFO.replace(r#""SwapLimit":true"#, r#""SwapLimit":false"#);
    let (calls, container) = (scratch.0.join("calls"), "c".repeat(64));
    let docker = format!(
        r#"#!/bin/sh
echo "$*" >> "{}"
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{info}' ;;
    container) echo "{container} exited {UNFIT}" ;;
    rm) ;;
    *) echo "the stand-in has no such command" >&2; exit 1 ;;
esac
"#,
        calls.display()
    );
    let failed = json!({ "state": "failed", "reason": "r" });
    write_docker_record(&scratch, UNFIT, "default", Some(failed));
    let path = write_docker(&scratch, &docker);
    let sandbox = json!({ "provider": "local", "id": container, "report": {} });
    scratch.write(&format!("runs/{UNFIT}/agents/agent/sandbox.json"), &sandbox);
    let mut reap = scratch.pumpkin(&["reap", "--runs", "runs"]);
    reap.env("PATH", &path);
    let reaped = ran(reap, &scratch.0, false);
    let called = fs::read_to_string(calls).unwrap();
    check(
        &scratch,
        [reaped, format!("docker was called with:\n{called}")].map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120600-0a000007 left 1 sandbox on provider `local`, now removed: cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            printed:
              20261108-120600-0a000007
            docker was called with:
            context inspect --format {{.Endpoints.docker.Host}} -- default
            info --format {{json .}}
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter label=pumpkin.run=20261108-120600-0a000007
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter id=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            rm --force --volumes cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
        "#]],
    );
}

/// A lost run on `local`, of kind `docker`, and `aws`, of kind `ec2`, which left a sandbox on
/// each.
const SPREAD: &str = "20261108-120700-0a000008";

/// A lost run on `aws`, which left a sandbox there, and which `cancel` reaps.
const CANCELLED: &str = "20261108-120800-0a000009";

/// A lost run on `aws`, whose configuration is gone.
const UNCONFIGURED: &str = "20261108-120900-0a00000a";

/// A lost run on `aws`, which the configuration that ran it no longer names.
const UNNAMED: &str = "20261108-121100-0a00000c";

// `aws` fails each call, as one does once its account is gone.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the records, the steps and their snapshot"
)]
fn reaps_providers() {
    let scratch = Scratch::new();
    let (calls, container) = (scratch.0.join("docker-calls"), "c".repeat(64));
    let docker = format!(
        r#"#!/bin/sh
echo "$*" >> "{}"
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{INFO}' ;;
    container) case "$*" in
        *label=pumpkin.run={SPREAD}*|*id={container}*) echo "{container} exited {SPREAD}" ;;
    esac ;;
    rm) ;;
    *) echo "the stand-in has no such command" >&2; exit 1 ;;
esac
"#,
        calls.display()
    );
    let path = write_docker(&scratch, &docker);
    symlink("/bin/false", scratch.0.join("bin/aws")).unwrap();
    let config = scratch.0.join("pumpkin.toml");
    scratch.write(
        "pumpkin.toml",
        &format!("{}\n{EC2}", docker_table("default")),
    );
    scratch.write(
        "bare.toml",
        &format!("{}\n{LOCAL}", docker_table("default")),
    );
    // Each run's record, which names the configuration that ran it, and what each of its
    // providers reported, of which a reap reads only a Docker engine's context.
    let records = [
        (SPREAD, &["local", "aws"][..], config.clone()),
        (CANCELLED, &["aws"], config),
        (UNCONFIGURED, &["aws"], scratch.0.join("gone.toml")),
        (UNNAMED, &["aws"], scratch.0.join("bare.toml")),
    ];
    for (id, providers, config) in records {
        write_record(&scratch, id, true, None);
        let args = [
            "--runs".to_owned(),
            scratch.0.join("runs").display().to_string(),
            "--config".to_owned(),
            config.display().to_string(),
        ];
        let record = json!({ "record_version": "pumpkin.record/v1", "args": args });
        scratch.write(&format!("runs/{id}/record.json"), &record);
        let reports = providers.iter().map(|&name| {
            let report = match name {
                "local" => json!({ "context": "default" }),
                _ => json!({ "region": "us-west-1" }),
            };
            (name.to_owned(), json!({ "report": report }))
        });
        let provided = json!({ "providers": reports.collect::<serde_json::Map<_, _>>() });
        scratch.write(&format!("runs/{id}/provider.json"), &provided);
    }
    let held = [
        (SPREAD, "agents/agent", "local", container.clone()),
        (SPREAD, "jobs/0", "aws", "i-00000000000000038".to_owned()),
        (CANCELLED, "jobs/0", "aws", "i-00000000000000039".to_owned()),
    ];
    for (run, dir, provider, id) in held {
        let sandbox = json!({ "provider": provider, "id": id, "report": {} });
        scratch.write(&format!("runs/{run}/{dir}/sandbox.json"), &sandbox);
    }
    let command = |args: &[&str]| {
        let mut command = scratch.pumpkin(args);
        command.args(["--runs", "runs"]).env("PATH", &path);
        ran(command, &scratch.0, false)
    };
    let mut steps = vec![command(&["cancel", CANCELLED]), command(&["reap"])];
    let reaps = fs::read_to_string(scratch.0.join(format!("runs/{SPREAD}/reaps.jsonl"))).unwrap();
    let reaps = reaps.lines().map(|line| {
        let mut line: serde_json::Value = serde_json::from_str(line).unwrap();
        line["at_ns"] = json!("<at>");
        line.to_string()
    });
    steps.push(format!(
        "{SPREAD}'s reaps.jsonl:\n{}",
        reaps.collect::<Vec<_>>().join("\n")
    ));
    let called = fs::read_to_string(&calls).unwrap();
    steps.push(format!("docker was called with:\n{}", called.trim_end()));
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin cancel 20261108-120800-0a000009 --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120800-0a000009's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: `aws --version` failed with exit status: 1 and wrote nothing to standard error; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120800-0a000009, to stop reaping it
            pumpkin: run 20261108-120800-0a000009 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120800-0a000009 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120800-0a000009
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml
            pumpkin: to remove what it left once its provider can be reached: pumpkin reap --runs <scratch>/runs
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120700-0a000008's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: `aws --version` failed with exit status: 1 and wrote nothing to standard error; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120700-0a000008, to stop reaping it
            pumpkin: run 20261108-120700-0a000008 left 1 sandbox on provider `local`, now removed: cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            pumpkin: run 20261108-120700-0a000008 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120800-0a000009's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: `aws --version` failed with exit status: 1 and wrote nothing to standard error; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120800-0a000009, to stop reaping it
            pumpkin: run 20261108-120900-0a00000a's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/gone.toml, can't be read (No such file or directory (os error 2)): put it back there, as it was when the run ran; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120900-0a00000a, to stop reaping it
            pumpkin: run 20261108-120900-0a00000a was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-121100-0a00000c's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/bare.toml, names no provider `aws`: put its `[providers.aws]` table back there, as it was when the run ran; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-121100-0a00000c, to stop reaping it
            pumpkin: run 20261108-121100-0a00000c was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: 4 runs ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --runs <scratch>/runs
            printed:
              20261108-120700-0a000008
              20261108-120900-0a00000a
              20261108-121100-0a00000c
            20261108-120700-0a000008's reaps.jsonl:
            {"at_ns":"<at>","id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","provider":"local","reaped":"removed"}
            docker was called with:
            context inspect --format {{.Endpoints.docker.Host}} -- default
            info --format {{json .}}
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter label=pumpkin.run=20261108-120700-0a000008
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter id=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            rm --force --volumes cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"#]],
    );
}

/// The locks that the process `pid` holds on `path`, or waits for, as `/proc/locks` lists them,
/// each by its process and its file's device and inode: for each, whether it waits, as one
/// listed after `->` does.
fn locks(pid: u32, path: &Path) -> Vec<bool> {
    let inode = format!(":{}", fs::metadata(path).unwrap().ino());
    let locks = fs::read_to_string("/proc/locks").unwrap();
    let found = locks.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (waits, rest) = match fields[..] {
            [_, "->", ref rest @ ..] => (true, rest),
            [_, ref rest @ ..] => (false, rest),
            [] => return None,
        };
        let [_, _, _, holder, file, ..] = rest[..] else {
            return None;
        };
        (holder == pid.to_string() && file.ends_with(&inode)).then_some(waits)
    });
    found.collect()
}
