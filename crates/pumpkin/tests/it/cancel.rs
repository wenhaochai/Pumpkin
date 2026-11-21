//! `cancel`: it stops a live run, waits until it has ended, removes what it left, and says how it
//! ended; of a run that has ended, or is lost, it only removes what it left. The live runs reach a
//! stand-in `docker` ([`StandIn`]), which holds each driver at `create` after the start.

use std::io::{BufRead as _, BufReader};
use std::process::Stdio;

use expect_test::expect;
use serde_json::json;

use super::{
    Scratch, StandIn, Step, check, ended, invocation, outcome_line, ran, record, report,
    write_docker_record, write_record,
};

const LOST: &str = "20261108-120000-0b000001";

const FAILED: &str = "20261108-120100-0b000002";

/// A record made before records were locked.
const UNLOCKED: &str = "20261108-120200-0b000003";

/// A lost run whose Docker provider cannot be reached.
const UNREACHED: &str = "20261108-120300-0c000004";

/// What becomes of `pumpkin cancel`, with `force` if given, of a run that `run` started in a
/// scratch directory of its own while the stand-in held its driver at `create`: once `cancel`
/// says that it is stopping the run, or killing its driver, the stand-in lets the driver go on.
fn live(force: &[&str]) -> String {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in/task.star", &[]);
    let output = run.output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    stand_in.held("create");
    let mut cancel = scratch.pumpkin(&["cancel"]);
    cancel.args(force).args([id, "--runs", "runs"]);
    let path = run.get_envs().find(|(name, _)| *name == "PATH");
    let path = path.and_then(|(_, path)| path).unwrap().to_owned();
    cancel.env("PATH", path);
    cancel.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&cancel);
    let mut child = cancel.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    stderr.read_line(&mut said).unwrap();
    stand_in.release("create");
    let output = ended(child, stderr, said);
    let reported = report(invoked, &output, &scratch.0, false);
    let text = format!("{reported}\n{}", outcome_line(&scratch.0.join(dir)));
    let text = text.replace(id, "<id>");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn cancels() {
    // Each makes a directory of its own first, as a test holds one at a time.
    let [stopped, killed] = [live(&[]), live(&["--force"])];
    let scratch = Scratch::new();
    write_record(&scratch, LOST, true, None);
    let failed = json!({ "state": "failed", "reason": "r" });
    write_record(&scratch, FAILED, true, Some(failed));
    write_record(&scratch, UNLOCKED, false, None);
    write_docker_record(&scratch, UNREACHED, "recorded", None);
    let mut unreached = scratch.pumpkin(&["cancel", UNREACHED, "--runs", "runs"]);
    unreached.env("PATH", "");
    let cancel = |run| {
        ran(
            scratch.pumpkin(&["cancel", run, "--runs", "runs"]),
            &scratch.0,
            false,
        )
    };
    let steps = [
        stopped,
        killed,
        cancel(LOST),
        cancel(FAILED),
        cancel("0b0000"),
        cancel("nothing"),
        cancel(UNLOCKED),
        ran(unreached, &scratch.0, false),
    ];
    check(
        &scratch,
        steps.map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin cancel <id> --runs runs
            exit status: Some(0)
            pumpkin: stopping run <id>, recorded in <scratch>/runs/<id>: it ends once its sandboxes are deleted
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            outcome.json: {"signal":"SIGTERM","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin cancel --force <id> --runs runs
            exit status: Some(0)
            pumpkin: killing the driver of run <id>, recorded in <scratch>/runs/<id>
            pumpkin: run <id> left its socket directory, now removed: <scratch>/runs/<id>/sockets
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            outcome.json: {"reason":"its driver ended without an outcome","state":"failed"}
            $ pumpkin cancel 20261108-120000-0b000001 --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120000-0b000001 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120000-0b000001 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120000-0b000001
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin cancel 20261108-120100-0b000002 --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120100-0b000002 failed: r; its record is <scratch>/runs/20261108-120100-0b000002
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin cancel 0b0000 --runs runs
            exit status: Some(1)
            pumpkin: `0b0000` matches 3 runs: name one by more of its id
              20261108-120000-0b000001
              20261108-120100-0b000002
              20261108-120200-0b000003
            $ pumpkin cancel nothing --runs runs
            exit status: Some(1)
            pumpkin: no run in runs matches `nothing`: name an id, a unique prefix of an id or of its random digits, or a record's path
            $ pumpkin cancel 20261108-120200-0b000003 --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/20261108-120200-0b000003/driver.pid isn't there: expected the record of a run to have it
            $ PATH= pumpkin cancel 20261108-120300-0c000004 --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120300-0c000004's sandboxes on provider `local`, if it left any, can't be removed, as it can't be reached: the Docker context `recorded` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120300-0c000004, to stop reaping it
            pumpkin: run 20261108-120300-0c000004 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120300-0c000004 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120300-0c000004
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            pumpkin: to remove what it left once its provider can be reached: pumpkin reap --runs <scratch>/runs"#]],
    );
}
