use std::fs::{self, File, TryLockError};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use docker::Reached;
use expect_test::expect;
use provider::{RunTag, Sweep as _};
use serde_json::Value;
use test_support::{docker_context, sandboxd};

use super::{
    IMAGE, INFO, Scratch, StandIn, Step, alive, build_first, check, containers, ended, engine,
    files, invocation, machine, poll, ran, record, report, sandboxes, signal, through_shell,
    with_room, workflow,
};

fn wait_once_ended(scratch: &Scratch, output: &Output, driver: i64) -> String {
    // A driver writes its log whole only after its lock goes, so wait for its end first.
    poll(|| Some(()).filter(|()| !alive(driver)));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    unlocked(&scratch.0.join(dir));
    let wait = scratch.pumpkin(&["wait", "--outcome", id, "--runs", "runs"]);
    ran(wait, &scratch.0, false).replace(id, "<id>")
}

#[derive(Clone, Copy)]
enum Whom {
    Run,
    Driver,
    Both,
}

fn unstarted(at: &str, whom: Whom, signal_name: &str, released: bool) -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold(at);
    let mut run = stand_in.run("stand-in/task.star", &[]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let mut child = run.spawn().unwrap();
    let pid = i64::from(child.id());
    let driver = stand_in.held(at);
    let target = match whom {
        Whom::Run => pid,
        Whom::Driver => driver,
        // A negative pid signals the run's whole process group, as a terminal's ^C does.
        Whom::Both => -pid,
    };
    assert!(signal(signal_name, target), "the process to signal is gone");
    if released {
        stand_in.release(at);
    }
    // `run`'s output ends only once the driver has, if it was still running.
    poll(|| Some(()).filter(|()| !alive(driver)));
    let stderr = child.stderr.take().unwrap();
    let output = ended(child, stderr, String::new());
    let mut lines = vec![report(invoked, &output, &scratch.0, false)];
    if record(&String::from_utf8_lossy(&output.stderr)).is_some() {
        lines.push(wait_once_ended(&scratch, &output, driver));
    } else {
        let made = scratch.0.join("runs").exists();
        lines.push(format!("the driver is gone, and made no record: {}", !made));
    }
    let text = lines.join("\n");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

fn unlogged() -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("image");
    let mut run = stand_in.run("stand-in/task.star", &[]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let child = run.spawn().unwrap();
    stand_in.held("image");
    let mut records = fs::read_dir(scratch.0.join("runs")).unwrap();
    let record = records.next().unwrap().unwrap().path();
    fs::remove_file(record.join("driver.log")).unwrap();
    stand_in.release("image");
    let output = child.wait_with_output().unwrap();
    let text = report(invoked, &output, &scratch.0, false);
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

fn on_engine(task: &str, info: &str, image: &str) -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::with(&scratch, info, image);
    let text = ran(
        stand_in.run(&format!("{task}/task.star"), &[]),
        &scratch.0,
        false,
    );
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

#[test]
fn lacks_its_image() {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    check(
        &scratch,
        [stand_in.run("lacking/task.star", &["--wait", "--outcome"])],
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml --wait --outcome
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lacking/final.star, sources/task/lacking/submit.star, sources/task/lacking/task.star, sources/task/lib/train.star
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it","state":"failed"}
            printed outcome.json: true"#]],
    );
}

#[test]
fn loses_its_log() {
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: run <id> records into <scratch>/runs/<id>
        pumpkin: run <id> failed: <scratch>/runs/<id>/driver.log: No such file or directory (os error 2); its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        record: driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lib/train.star, sources/task/stand-in/final.star, sources/task/stand-in/submit.star, sources/task/stand-in/task.star
        outcome.json: {"reason":"<scratch>/runs/<id>/driver.log: No such file or directory (os error 2)","state":"failed"}"#]].assert_eq(&unlogged());
}

#[test]
fn refuses_the_classic_store() {
    let containerd = r#"["driver-type","io.containerd.snapshotter.v1"]"#;
    let classic = INFO.replace(containerd, r#"["Backing Filesystem","extfs"]"#);
    let shown = on_engine("stand-in", &classic, IMAGE);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: the Docker engine of context `default` doesn't keep its images in the containerd image store, as `docker info` reports no `driver-type` of `io.containerd.snapshotter.v1`, so it can't find an image by its manifest's digest: turn the store on, as `"features": {"containerd-snapshotter": true}` in the engine's `daemon.json` does
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star"#]].assert_eq(&shown);
}

#[test]
fn refuses_an_undescribed_image() {
    let descriptor = r#","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let shown = on_engine("stand-in", INFO, &IMAGE.replace(descriptor, ""));
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: run <id> records into <scratch>/runs/<id>
        pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` doesn't say what sha256:0000000000000000000000000000000000000000000000000000000000000000 is the digest of, as `docker image inspect` gives it no `Descriptor`: use Docker 28.0 or later, whose engine and command line give one; its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lib/train.star, sources/task/stand-in/final.star, sources/task/stand-in/submit.star, sources/task/stand-in/task.star
        outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` doesn't say what sha256:0000000000000000000000000000000000000000000000000000000000000000 is the digest of, as `docker image inspect` gives it no `Descriptor`: use Docker 28.0 or later, whose engine and command line give one","state":"failed"}"#]].assert_eq(&shown);
}

#[test]
fn refuses_unlimited_swap() {
    // The task builds its image, and no build starts: the refusal comes first.
    let unlimited = INFO.replace(r#""SwapLimit":true"#, r#""SwapLimit":false"#);
    let shown = on_engine("unbuilt", &unlimited, IMAGE);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run unbuilt/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: reaching provider `local` failed: the Docker engine of context `default` can't enforce every limit a sandbox has: `docker info` reports SwapLimit false: use an engine that reports each true
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/unbuilt/task.star"#]].assert_eq(&shown);
}

#[test]
fn unstarted_context_driver_term() {
    let shown = unstarted("context", Whom::Driver, "TERM", false);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: the driver ended before the run started, by signal: 15 (SIGTERM)
        the driver is gone, and made no record: true"#]].assert_eq(&shown);
}

#[test]
fn unstarted_context_run_kill() {
    let shown = unstarted("context", Whom::Run, "KILL", false);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        ended by signal: 9
        the driver is gone, and made no record: true"#]].assert_eq(&shown);
}

#[test]
fn unstarted_context_both_int() {
    let shown = unstarted("context", Whom::Both, "INT", false);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        ended by signal: 2
        the driver is gone, and made no record: true"#]].assert_eq(&shown);
}

#[test]
fn unstarted_image_driver_kill() {
    let shown = unstarted("image", Whom::Driver, "KILL", false);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: run <id> records into <scratch>/runs/<id>
        pumpkin: the driver ended before the run started, by signal: 9 (SIGKILL)
        pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
        pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
        outcome.json: none
        printed the id alone: false
        $ pumpkin wait --outcome <id> --runs runs
        exit status: Some(1)
        pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
        pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs"#]].assert_eq(&shown);
}

#[test]
fn unstarted_image_driver_term_released() {
    let shown = unstarted("image", Whom::Driver, "TERM", true);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: run <id> records into <scratch>/runs/<id>
        pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lib/train.star, sources/task/stand-in/final.star, sources/task/stand-in/submit.star, sources/task/stand-in/task.star
        outcome.json: {"signal":"SIGTERM","state":"interrupted"}
        $ pumpkin wait --outcome <id> --runs runs
        exit status: Some(1)
        pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        printed outcome.json: {"signal":"SIGTERM","state":"interrupted"}"#]].assert_eq(&shown);
}

#[test]
fn unstarted_image_both_int() {
    let shown = unstarted("image", Whom::Both, "INT", false);
    expect![[r#"
        $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
        ended by signal: 2
        pumpkin: run <id> records into <scratch>/runs/<id>
        outcome.json: none
        printed the id alone: false
        $ pumpkin wait --outcome <id> --runs runs
        exit status: Some(1)
        pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
        pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
        pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs"#]].assert_eq(&shown);
}

fn detached(stand_in: &StandIn, invoked: String, output: &Output, driver: i64) -> String {
    let scratch = stand_in.scratch;
    let mut lines = vec![report(invoked, output, &scratch.0, false)];
    lines.push(format!(
        "the driver runs, leading a session of its own: {}",
        alive(driver) && session(driver) == Some(driver)
    ));
    let name = fs::read_to_string(format!("/proc/{driver}/comm")).unwrap();
    lines.push(format!("the driver's name: {}", name.trim_end()));
    stand_in.release("create");
    lines.push(wait_once_ended(scratch, output, driver));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (_, dir) = record(&stderr).unwrap();
    let log = fs::read_to_string(scratch.0.join(dir).join("driver.log")).unwrap();
    lines.push(format!("driver.log: {log:?}"));
    let text = lines.join("\n");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

fn returns() -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in/task.star", &[]);
    let invoked = invocation(&run);
    let output = run.output().unwrap();
    detached(&stand_in, invoked, &output, stand_in.held("create"))
}

fn interrupted() -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in/task.star", &["--wait"]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let mut child = run.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.ends_with("does not stop it\n") {
        assert_ne!(stderr.read_line(&mut said).unwrap(), 0, "run ended: {said}");
    }
    let driver = stand_in.held("create");
    assert!(signal("INT", -i64::from(child.id())), "run is gone");
    let output = ended(child, stderr, said);
    detached(&stand_in, invoked, &output, driver)
}

fn stopped() -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("image");
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in/task.star", &[]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let mut child = run.spawn().unwrap();
    let pid = i64::from(child.id());
    stand_in.held("image");
    assert!(signal("STOP", pid), "run is gone");
    stand_in.release("image");
    let driver = stand_in.held("create");
    // A stopped process holds a TERM pending until it continues.
    assert!(signal("TERM", pid) && signal("CONT", pid), "run is gone");
    let stderr = child.stderr.take().unwrap();
    let output = ended(child, stderr, String::new());
    detached(&stand_in, invoked, &output, driver)
}

#[test]
fn detaches() {
    // Each makes a directory of its own first, as a test holds one at a time.
    let steps = [returns(), interrupted(), stopped()].map(Step::Ran);
    let scratch = Scratch::new();
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: to follow it: pumpkin wait --runs <scratch>/runs <id>
            pumpkin: to stop it: pumpkin cancel --runs <scratch>/runs <id>
            outcome.json: none
            printed the id alone: true
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports; its work ended at <t>
                 <t> final call job 0 on local/container, never started, ended after <t>: failed: the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports
              sandboxes: 2 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a result: the final call gave none (error: the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports); it ran no command; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports"}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"failure":{"kind":"error","reason":"the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports"},"kind":"no_result"}}
            driver.log: ""
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml --wait
            ended by signal: 2
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            outcome.json: none
            printed the id alone: false
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports; its work ended at <t>
                 <t> final call job 0 on local/container, never started, ended after <t>: failed: the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports
              sandboxes: 2 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a result: the final call gave none (error: the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports); it ran no command; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports"}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"failure":{"kind":"error","reason":"the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports"},"kind":"no_result"}}
            driver.log: ""
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports; its work ended at <t>
                 <t> final call job 0 on local/container, never started, ended after <t>: failed: the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports
              sandboxes: 2 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a result: the final call gave none (error: the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports); it ran no command; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports"}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"failure":{"kind":"error","reason":"the job's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what it reports"},"kind":"no_result"}}
            driver.log: """#]],
    );
}

fn signaled(ignoring: bool, signals: &[&str], released: bool) -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in/task.star", &[]);
    if ignoring {
        // Ignores SIGHUP, as `nohup` starts a program.
        run = through_shell(&run, "trap '' HUP");
    }
    let output = run.output().unwrap();
    let driver = stand_in.held("create");
    let mut lines = vec![format!("the driver {}", dispositions(driver))];
    for name in signals {
        assert!(signal(name, driver), "the driver is gone");
    }
    if released {
        stand_in.release("create");
    }
    lines.push(wait_once_ended(&scratch, &output, driver));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let log = fs::read_to_string(scratch.0.join(dir).join("driver.log")).unwrap();
    lines.push(format!("driver.log: {:?}", log.replace(id, "<id>")));
    let text = lines.join("\n");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

fn dispositions(pid: i64) -> String {
    let (caught, ignored) = masks(pid).unwrap();
    let names = |mask: u64| {
        let signals = [
            (libc::SIGHUP, "HUP"),
            (libc::SIGINT, "INT"),
            (libc::SIGTERM, "TERM"),
        ];
        let set = signals.iter().filter(|(number, _)| has(mask, *number));
        let names: Vec<&str> = set.map(|(_, name)| *name).collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(" ")
        }
    };
    format!("catches {}; ignores {}", names(caught), names(ignored))
}

fn masks(pid: i64) -> Option<(u64, u64)> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mask = |field: &str| {
        let line = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap();
        u64::from_str_radix(line.trim(), 16).unwrap()
    };
    Some((mask("SigCgt:"), mask("SigIgn:")))
}

fn has(mask: u64, number: libc::c_int) -> bool {
    mask >> (number - 1) & 1 == 1
}

#[test]
fn signaled_after_the_start() {
    // Each makes a directory of its own first, as a test holds one at a time.
    let steps = [
        signaled(false, &["TERM"], true),
        signaled(false, &["HUP"], true),
        signaled(false, &["INT", "INT"], true),
        signaled(true, &["HUP", "TERM"], true),
    ];
    let scratch = Scratch::new();
    check(
        &scratch,
        steps.map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            the driver catches HUP INT TERM; ignores none
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGTERM","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n"
            the driver catches HUP INT TERM; ignores none
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGHUP; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGHUP","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGHUP; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n"
            the driver catches HUP INT TERM; ignores none
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGINT; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGINT","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGINT; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n"
            the driver catches INT TERM; ignores HUP
            $ pumpkin wait --outcome <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGTERM","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n""#]],
    );
}

struct Cleanup<'a> {
    containers: &'a Reached,
    runs: PathBuf,
}

// Kills the drivers and removes the containers that a failed test would leave on the engine.
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(&self.runs) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(driver) = live_driver(&entry.path()) {
                let _ = signal("KILL", driver);
            }
            let Ok(tag) = RunTag::try_from(id) else {
                continue;
            };
            if let Ok(listed) = self.containers.list(&tag) {
                let ids: Vec<_> = listed.into_iter().map(|listed| listed.id).collect();
                let _ = self.containers.delete(&tag, &ids);
            }
        }
    }
}

fn unlocked(dir: &Path) {
    // A driver's main thread shows as ended, a zombie, while another thread may still be exiting,
    // holding the descriptors and so the lock.
    poll(|| Some(()).filter(|()| live_driver(dir).is_none()));
}

fn live_driver(dir: &Path) -> Option<i64> {
    let pid = File::open(dir.join("driver.pid")).ok()?;
    // A live driver holds an exclusive lock on its pid file; a stale file is unlocked.
    let held = matches!(pid.try_lock_shared(), Err(TryLockError::WouldBlock));
    let text = fs::read_to_string(dir.join("driver.pid")).ok()?;
    text.trim().parse().ok().filter(|_| held)
}

fn session(pid: i64) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, in parentheses, may itself hold spaces and parentheses.
    let (_, after) = stat.rsplit_once(')').unwrap();
    Some(after.split_whitespace().nth(3).unwrap().parse().unwrap())
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the run's steps and their snapshot"
)]
fn lost_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let containers = containers(&context);
    scratch.write_task("sleeps", &workflow("", &["sleep", "300"]));
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "sleeps", "pumpkin.toml");
    let _cleanup = Cleanup {
        containers: &containers,
        runs: scratch.0.join("runs"),
    };
    let temp = scratch.0.join("tmp");
    fs::create_dir(&temp).unwrap();
    let mut run = scratch.pumpkin(&["run", "sleeps/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    run.env("TMPDIR", &temp);
    let invoked = invocation(&run);
    let output = with_room(&mut run);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    let record = scratch.0.join(dir);
    let created = record.join("agents/agent/sandbox.json");
    poll(|| Some(()).filter(|()| created.exists()));
    let listed = containers.list(&tag).unwrap();
    let agent = listed[0].id.as_str().to_owned();
    let driver = live_driver(&record).unwrap();
    assert!(signal("KILL", driver), "the driver is gone");
    unlocked(&record);
    let rootful = !engine.report().security.rootless;
    let mut killed = vec![report(invoked, &output, &scratch.0, rootful)];
    killed.extend(sandboxes(&record));
    // Names only: the agent's socket goes once the agent connects, which the kill may precede.
    let sockets = fs::read_dir(scratch.0.join(dir).join("sockets")).unwrap();
    let mut sockets: Vec<_> = sockets.map(|entry| entry.unwrap().file_name()).collect();
    sockets.sort();
    killed.push(format!("its socket directories: {sockets:?}"));
    killed.push(format!("its temporary directory holds: {:?}", files(&temp)));
    let mut steps = vec![killed.join("\n")];
    let pumpkin = |args: &[&str]| ran(scratch.pumpkin(args), &scratch.0, false);
    steps.push(pumpkin(&["wait", id, "--runs", "runs"]));
    steps.push(pumpkin(&["cancel", id, "--runs", "runs"]));
    let ir = fs::read_to_string(record.join("ir.json")).unwrap();
    let ir: serde_json::Value = serde_json::from_str(&ir).unwrap();
    let image = ir["roles"][0]["image"].as_str().unwrap();
    let mut create = test_support::docker();
    create.args([
        "create",
        "--label",
        &format!("pumpkin.run={id}"),
        image,
        "true",
    ]);
    let created = create.output().unwrap();
    assert!(created.status.success(), "{created:?}");
    let unstarted = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    steps.push(pumpkin(&["reap", "--runs", "runs"]));
    let left = containers.list(&tag).unwrap().len();
    steps.push(format!("containers left: {left}"));
    let shown = [
        (&sandboxd[..], "<sandboxd>"),
        (id, "<id>"),
        (&agent[..], "<the agent's container>"),
        (&unstarted[..], "<the unstarted container>"),
    ];
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            $ TMPDIR=<scratch>/tmp pumpkin run sleeps/task.star --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: to follow it: pumpkin wait --runs <scratch>/runs <id>
            pumpkin: to stop it: pumpkin cancel --runs <scratch>/runs <id>
            outcome.json: none
            printed the id alone: true
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"id":"<container>","provider":"local"}
            its socket directories: ["agent-0"]
            its temporary directory holds: []
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, 0 deleted
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ pumpkin cancel <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> left 1 sandbox on provider `local`, now removed: <the agent's container>
            pumpkin: run <id> left its socket directory, now removed: <scratch>/runs/<id>/sockets
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, 0 deleted
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            $ pumpkin reap --runs runs
            exit status: Some(0)
            pumpkin: run <id> left 1 sandbox on provider `local`, now removed: <the unstarted container>
            printed:
              <id>
            containers left: 0"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn signaled_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let containers = containers(&context);
    let ir = workflow("", &["sleep", "300"]);
    let deadline = ir["deadline_seconds"].as_u64();
    let deadline = Duration::from_secs(deadline.unwrap());
    scratch.write_task("sleeps", &ir);
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "sleeps", "pumpkin.toml");
    let _cleanup = Cleanup {
        containers: &containers,
        runs: scratch.0.join("runs"),
    };
    let mut run = scratch.pumpkin(&["run", "sleeps/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let log = || fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    poll(|| Some(()).filter(|()| log().contains(r#""kind":"agent_started""#)));
    let driver = live_driver(&record).unwrap();
    assert!(signal("TERM", driver), "the driver is gone");
    // So that `wait` finds the run ended, rather than follow it while the driver ends.
    unlocked(&record);
    let wait = ran(
        scratch.pumpkin(&["wait", id, "--runs", "runs"]),
        &scratch.0,
        false,
    );
    // The sandboxes' deletes come after the stop's note, in records that hold none.
    let log = log();
    let records = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap());
    let last = records
        .rev()
        .find(|record| record["note"].is_string())
        .unwrap();
    let at = Duration::from_nanos(last["at_ns"].as_str().unwrap().parse().unwrap());
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    let sockets = record.join("sockets");
    let lines = [
        wait,
        format!("the run log's last note: {}", last["note"]),
        format!("it came before the deadline: {}", at < deadline),
        format!("containers left: {}", containers.list(&tag).unwrap().len()),
        format!("socket directory left: {}", sockets.exists()),
    ];
    let shown = [(id, "<id>"), (&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        lines.map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, all deleted
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            the run log's last note: "the run was interrupted by SIGTERM"
            it came before the deadline: true
            containers left: 0
            socket directory left: false"#]],
    );
}
