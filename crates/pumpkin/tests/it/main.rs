//! The `pumpkin` command, run as an operator runs it. The tests on Docker are ignored: `cargo xtask
//! live docker` runs them.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a test says which image it can't remove, and why it skips"
)]

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::{Display, Write as _};
use std::fs::Permissions;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};
use std::{fs, iter, thread};

use docker::{Docker, Reached};
use engine::{ContextName, Engine};
use expect_test::Expect;
use serde_json::{Value, json};
use stdx::process::Environment;
use test_support::{BUSYBOX, TestDir, WORKFLOW, files};

mod build;
mod cancel;
mod check;
mod controller;
mod driver;
mod export;
mod fmt;
mod ls;
mod publish;
mod reap;
mod run;
mod view;
mod wait;

const PUMPKIN: &str = env!("CARGO_BIN_EXE_pumpkin");

const AWS_GUARD: &str = "no-aws";

// The command of the final call of each task that a test writes: it scores the payload by its
// size.
const SCORE: &str =
    r#"printf '{"raw_score": %s}' $(wc -c < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"#;

/// A task's `final.star`, whose final call puts the payload at `/pumpkin/in/payload/raw`, runs
/// `command` with `sh -c`, and shows as `score` the `raw_score` of the result that `command` wrote.
fn final_running(command: &str) -> String {
    let run = starlark(&json!(["sh", "-c", command]));
    format!(
        r#""""The final call of a task that a test writes."""

def final_call(target: Target, submissions):
    """Runs a command on the submission, and shows the score it wrote.

    Args:
      target: the call's target.
      submissions: each agent's submission.

    Returns:
      The score.
    """
    payload = submissions.values()[0]
    if payload == None:
        return {{"score": 0}}
    target.put("/pumpkin/in/payload/raw", payload)
    target.run({run})
    return {{"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}}
"#
    )
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let to = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

/// A test's own directory, with a copy of `pumpkin` in `.pumpkin/bin`.
///
/// Field `0` is its canonical path.
struct Scratch(PathBuf, TestDir);

/// A now function's body, which a task that [`Scratch::write_task_in`] writes loads.
const SUBMIT: &str = "def submit(run, payload, finalize: bool = False):
    run.set_submission(payload)
    if finalize:
        run.end_work()
    return {}
";

/// What [`Scratch::write_task_in`] writes in place of each now function's body, then replaces.
const SUBMIT_BODY: &str = "<the body of submit.star>";

/// What [`Scratch::write_task_in`] writes in place of the final call's body, then replaces.
const FINAL_BODY: &str = "<the body of final.star>";

impl Scratch {
    fn new() -> Self {
        let dir = TestDir::new();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        // `run` refuses a runs directory below one that its group may write, which the umask, or a
        // setgid temporary directory, could make this one.
        fs::set_permissions(&repo, Permissions::from_mode(0o755)).unwrap();
        let scratch = Self(fs::canonicalize(repo).unwrap(), dir);
        install(&scratch, ".pumpkin/bin");
        let guard = scratch.0.with_file_name(AWS_GUARD);
        fs::create_dir(&guard).unwrap();
        symlink("/bin/false", guard.join("aws")).unwrap();
        scratch
    }

    fn engine_id(&self) -> &str {
        self.1.path().file_name().unwrap().to_str().unwrap()
    }

    fn write(&self, name: &str, contents: &(impl Display + ?Sized)) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents.to_string()).unwrap();
    }

    fn write_task(&self, name: &str, ir: &Value) {
        self.write_task_in("", name, ir);
    }

    fn write_task_in(&self, root: &str, name: &str, ir: &Value) {
        let at = |file: &str| {
            let dir = Path::new(root).join(name).join(file);
            dir.to_str().unwrap().to_owned()
        };
        self.write(
            &Path::new(root).join("PUMPKIN.star").display().to_string(),
            "# The root of the tasks.\n",
        );
        let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
        self.write(&at("Dockerfile"), &recipe);
        let roles = ir["roles"].as_array().unwrap();
        let mut images = "IMAGE = image(\"Dockerfile\")\n".to_owned();
        if roles.iter().any(|role| role["image"] == RUNNER) {
            let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /work\n");
            self.write(&at("runner/Dockerfile"), &recipe);
            images.push_str("RUNNER = image(\"runner/Dockerfile\")\n");
        }
        let mut loads = String::new();
        for (file, def) in bodies(ir) {
            let path = Path::new(root).join(file.strip_prefix("//").unwrap());
            self.write(path.to_str().unwrap(), &BODY.replace("DEF", &def));
            writeln!(
                loads,
                "load({}, {})",
                starlark(&json!(file)),
                starlark(&json!(def))
            )
            .unwrap();
        }
        // Each now function's body is `submit` of a library beside the task, which an entry can't
        // define.
        let mut roles = ir["roles"].clone();
        let grants = roles.as_array_mut().unwrap().iter_mut();
        let grants = grants
            .filter_map(|role| role.pointer_mut("/runtime/agent/grants")?.as_array_mut())
            .flatten();
        for now in grants.filter_map(|grant| grant.get_mut("now")) {
            now["body"] = json!(SUBMIT_BODY);
        }
        self.write(&at("submit.star"), SUBMIT);
        let roles = starlark(&roles).replace(&starlark(&json!(SUBMIT_BODY)), "submit");
        // The final call's body is `final_call` of `final_running(SCORE)`, which a test may write
        // over.
        let mut final_call = ir["final"].clone();
        final_call["body"] = json!(FINAL_BODY);
        self.write(&at("final.star"), &final_running(SCORE));
        let final_call = starlark(&final_call).replace(&starlark(&json!(FINAL_BODY)), "final_call");
        let task = format!(
            "{loads}load(\"//{name}/submit.star\", \"submit\")\nload(\"//{name}/final.star\", \
             \"final_call\")\n\n{images}\nworkflow(\n    name = {},\n    roles = {roles},\n    \
             final = {final_call},\n    deadline_seconds = {},\n)\n",
            starlark(&ir["name"]),
            ir["deadline_seconds"],
        );
        self.write(&at("task.star"), &task);
    }

    fn write_executable(&self, name: &str, contents: &[u8]) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
    }

    /// Writes an executable of only the header that `pumpkin run` checks of a `sandboxd`: it
    /// can't run.
    fn write_sandboxd(&self, name: &str) {
        let mut header = [0; 64];
        header[..5].copy_from_slice(b"\x7fELF\x02");
        self.write_executable(name, &header);
    }

    fn pumpkin(&self, args: &[&str]) -> Command {
        self.program(self.0.join(".pumpkin/bin/pumpkin"), args)
    }

    fn program(&self, program: impl AsRef<Path>, args: &[&str]) -> Command {
        let mut command = stdx::process::command(program.as_ref(), Environment::Inherited);
        command.args(args).current_dir(&self.0);
        // A home of the test's own, so that no test reads or writes below the machine's.
        let home = self.0.join("home");
        fs::create_dir_all(&home).unwrap();
        command.env("HOME", home);
        // So that no test reaches AWS with the machine's credentials: one that runs `aws` puts its
        // own first on the `PATH` it gives.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let poisoned =
            iter::once(self.0.with_file_name(AWS_GUARD)).chain(std::env::split_paths(&path));
        command.env("PATH", std::env::join_paths(poisoned).unwrap());
        command
    }
}

/// The file and the `def` of each job function's body that the agent of `ir` holds.
fn bodies(ir: &Value) -> Vec<(String, String)> {
    let grants = ir["roles"][0]["runtime"]["agent"]["grants"].as_array();
    (grants.into_iter().flatten())
        .filter_map(|grant| grant["job"]["body"].as_object())
        .map(|body| {
            let text = |key: &str| body[key].as_str().unwrap().to_owned();
            (text("file"), text("def"))
        })
        .collect()
}

/// The body that [`Scratch::write_task`] writes for each `def` that a job function names, `DEF`
/// in its text: it returns the payload's size in bytes, after 300 s for the payload `slow`.
const BODY: &str = r#""""A job function's body, which the test writes."""

def DEF(target: Target, payload: Payload):
    """Counts the payload's bytes in the target.

    Args:
      target: the call's target, whose directory is `/workspace`.
      payload: the payload to count.

    Returns:
      The payload's size, as `bytes`.
    """
    target.put("/workspace/payload", payload)
    target.run(["sh", "-c", "if [ \"$(cat payload)\" = slow ]; then sleep 300; fi; printf '{\"bytes\": %s}' $(wc -c < payload) > result.json"])
    return json.decode(target.get("/workspace/result.json"))
"#;

fn starlark(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => serde_json::to_string(text).unwrap(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(starlark).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(fields) => {
            let fields = fields
                .iter()
                .map(|(key, value)| match (key.as_str(), value) {
                    ("image", Value::String(image)) if image == RUNNER => {
                        "\"image\": RUNNER".to_owned()
                    }
                    ("image", Value::String(_)) => "\"image\": IMAGE".to_owned(),
                    ("body", Value::Object(body)) => {
                        format!("\"body\": {}", body["def"].as_str().unwrap())
                    }
                    _ => format!(
                        "{}: {}",
                        serde_json::to_string(key).unwrap(),
                        starlark(value)
                    ),
                });
            format!("{{{}}}", fields.collect::<Vec<_>>().join(", "))
        }
    }
}

const RUNNER: &str = "runner";

enum Step {
    Run(Command),
    OnEngine(Command),
    Ran(String),
}

impl From<Command> for Step {
    fn from(command: Command) -> Self {
        Self::Run(command)
    }
}

/// Runs each step, replaces the scratch directory's path and each of `shown`'s values by its
/// name, and compares what the steps print with `expect`.
///
/// A line about the engine's warning that it runs as root appears only where the warning's
/// presence differs from `rootful`.
fn check(
    scratch: &Scratch,
    steps: impl IntoIterator<Item = impl Into<Step>>,
    rootful: bool,
    shown: &[(&str, &str)],
    expect: &Expect,
) {
    let ran: Vec<String> = (steps.into_iter())
        .map(|step| match step.into() {
            Step::Run(command) => ran(command, &scratch.0, rootful),
            Step::OnEngine(mut command) => {
                let invoked = invocation(&command);
                report(invoked, &with_room(&mut command), &scratch.0, rootful)
            }
            Step::Ran(text) => text,
        })
        .collect();
    let mut text = ran.join("\n");
    text = text.replace(scratch.0.to_str().unwrap(), "<scratch>");
    for (value, name) in shown {
        text = text.replace(value, name);
    }
    expect.assert_eq(&text);
}

/// Runs `command` again for up to ten minutes while it fails only because other runs hold the
/// machine's CPUs.
fn with_room(command: &mut Command) -> Output {
    let room = Instant::now() + Duration::from_mins(10);
    loop {
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let crowded = output.status.code() == Some(1)
            && stderr.contains("pumpkin: reserving on provider")
            && stderr.contains("run it once");
        if !crowded || Instant::now() >= room {
            return output;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Runs `command` and describes what it did as `check` shows a step, finding its run record
/// under `dir`.
fn ran(mut command: Command, dir: &Path, rootful: bool) -> String {
    let invoked = invocation(&command);
    let output = command.output().unwrap();
    report(invoked, &output, dir, rootful)
}

fn invocation(command: &Command) -> String {
    // The home and the `PATH` that `Scratch::program` gives each command, `<scratch>/home` and
    // one that starts with its AWS guard, are the rule: they are not shown.
    let set = command.get_envs().filter_map(|(name, value)| match value {
        Some(value) if name == "HOME" && Path::new(value).ends_with("home") => None,
        Some(value)
            if name == "PATH"
                && std::env::split_paths(value)
                    .next()
                    .is_some_and(|dir| dir.ends_with(AWS_GUARD)) =>
        {
            None
        }
        Some(value) => Some(format!("{}={} ", name.display(), value.display())),
        None => Some(format!("env --unset={} ", name.display())),
    });
    let args = command.get_args().map(|arg| match arg.to_string_lossy() {
        arg if arg.is_empty() => Cow::Borrowed("''"),
        arg => arg,
    });
    let set: String = set.collect();
    let program = command.get_program().to_string_lossy();
    let program = if program.contains("/.pumpkin/") && program.ends_with("/bin/pumpkin") {
        Cow::Borrowed("pumpkin")
    } else {
        program
    };
    let args: Vec<Cow<str>> = iter::once(program).chain(args).collect();
    format!("$ {set}{}", args.join(" "))
}

fn report(invoked: String, output: &Output, dir: &Path, rootful: bool) -> String {
    let mut lines = vec![invoked];
    lines.push(match output.status.signal() {
        Some(signal) => format!("ended by signal: {signal}"),
        None => format!("exit status: {:?}", output.status.code()),
    });
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (warnings, said): (Vec<&str>, Vec<&str>) =
        (stderr.lines()).partition(|line| line.contains(" runs as root, "));
    if warnings.is_empty() == rootful {
        lines.push(format!(
            "warned of a rootful engine: {}",
            !warnings.is_empty()
        ));
    }
    lines.extend(steady(&said));
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let Some((id, record)) = record(&stderr) else {
        // `wait --outcome` prints an `outcome.json` without having started its run.
        if let Ok(outcome) = serde_json::from_str::<Value>(&stdout) {
            lines.push(format!("printed outcome.json: {}", steady_outcome(outcome)));
        } else if !stdout.is_empty() {
            lines.push("printed:".to_owned());
            let printed = stdout
                .lines()
                .map(|line| format!("  {line}").trim_end().to_owned());
            lines.extend(printed);
        }
        return lines.join("\n");
    };
    let record = dir.join(record);
    if record.join("outcome.json").exists() {
        lines.extend(recorded(&record, &stdout));
    } else {
        lines.push("outcome.json: none".to_owned());
        lines.push(format!(
            "printed the id alone: {}",
            stdout == format!("{id}\n")
        ));
    }
    lines.join("\n").replace(id, "<id>")
}

/// The lines of `said` with each duration and clock time in a run's summary and its milestones
/// replaced by `<t>`.
fn steady(said: &[&str]) -> Vec<String> {
    let timed = |word: &str| {
        let word = word.trim_end_matches([',', ';', ':']);
        let clock = word
            .split(':')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
        let span = !word.is_empty()
            && word
                .trim_end_matches(|c: char| c.is_ascii_digit() || "hms.".contains(c))
                .is_empty()
            && word.ends_with(['h', 'm', 's'])
            && word.starts_with(|c: char| c.is_ascii_digit());
        (clock && word.contains(':')) || span
    };
    let mut summary = false;
    let lines = said.iter().map(|line| {
        let first =
            line.starts_with("pumpkin: run ") && line.ends_with(':') && line.contains(" ran ");
        summary = first || (summary && line.starts_with("  "));
        // A milestone, which `wait` says as it follows a run, starts with its time.
        let milestone = (line.strip_prefix("pumpkin:"))
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|word| timed(word) && word.contains(':'));
        if !summary && !milestone {
            return (*line).to_owned();
        }
        let words = line.split(' ').map(|word| {
            if timed(word) {
                let kept = &word[word.trim_end_matches([',', ';', ':']).len()..];
                format!("<t>{kept}")
            } else {
                word.to_owned()
            }
        });
        words.collect::<Vec<String>>().join(" ")
    });
    lines.collect()
}

/// `outcome` with the durations and prices of its usage, which differ between runs, replaced.
fn steady_outcome(mut outcome: Value) -> Value {
    if let Some(usage) = outcome.get_mut("usage") {
        usage["ran_ns"] = "<ns>".into();
        for sandboxes in usage["sandboxes"].as_array_mut().unwrap() {
            sandboxes["lasted_ns"] = "<ns>".into();
            if let Some(usd) = sandboxes.get_mut("usd") {
                *usd = "<usd>".into();
            }
        }
        if let Some(usd) = usage.get_mut("usd") {
            *usd = "<usd>".into();
        }
    }
    outcome
}

fn record(stderr: &str) -> Option<(&str, &str)> {
    (stderr.lines()).find_map(|line| {
        let line = line.strip_prefix("pumpkin: ")?;
        if let Some(line) = line.strip_prefix("run ") {
            return line.split_once(" records into ");
        }
        let (id, from) = line
            .strip_prefix("copying run ")?
            .split_once("'s record from ")?;
        Some((id, from.split_once(" to ")?.1))
    })
}

/// The line that shows the `outcome.json` of `record`, made steady.
fn outcome_line(record: &Path) -> String {
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let compact = serde_json::from_str::<Value>(&outcome).unwrap();
    format!("outcome.json: {}", steady_outcome(compact))
}

fn recorded(record: &Path, stdout: &str) -> Vec<String> {
    let mut lines = vec![format!("record: {}", files(record).join(", "))];
    lines.extend(sandboxes(record));
    // A run that didn't start has no `provider.json`.
    if let Ok(provided) = fs::read_to_string(record.join("provider.json")) {
        let routes = &serde_json::from_str::<Value>(&provided).unwrap()["routes"];
        if routes.as_object().is_some_and(|routes| !routes.is_empty()) {
            lines.push(format!("provider.json routes: {routes}"));
        }
    }
    // A run that didn't start has no run log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for line in log.lines() {
        let logged: Value = serde_json::from_str(line).unwrap();
        if logged["event"]["kind"] == "model_answered" {
            let mut event = logged["event"].clone();
            for time in ["first_byte_ns", "first_token_ns"] {
                if let Some(ns) = event.get_mut(time) {
                    *ns = json!("<ns>");
                }
            }
            lines.push(format!("the run log records: {event}"));
        }
    }
    for (whose, path) in [
        ("the agent's", "agents/agent/out/answer"),
        ("the final call's", "jobs/0/out/answer"),
    ] {
        if let Ok(answer) = fs::read_to_string(record.join(path)) {
            lines.push(format!("{whose} answer: {}", answer.replace('\n', "\\n")));
        }
    }
    let said = fs::read_to_string(record.join("driver.log")).unwrap_or_default();
    if !said.is_empty() {
        lines.push(format!("driver.log: {}", said.replace('\n', "\\n")));
    }
    lines.push(outcome_line(record));
    if !stdout.is_empty() {
        let printed = fs::read_to_string(record.join("outcome.json")).unwrap();
        lines.push(format!("printed outcome.json: {}", stdout == printed));
    }
    lines
}

fn sandboxes(record: &Path) -> Vec<String> {
    let container = |line: String| {
        let Some((before, after)) = line.split_once(r#""id":""#) else {
            return line;
        };
        let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        let id = after.split('"').next().unwrap();
        if id.len() == 64 && id.bytes().all(hex) {
            format!(r#"{before}"id":"<container>{}"#, &after[64..])
        } else {
            line
        }
    };
    let lines = test_support::sandboxes(record).into_iter();
    lines.map(container).collect()
}

/// Calls `look` every 100 ms until it returns a value, and panics if none comes in a minute.
fn poll<T>(mut look: impl FnMut() -> Option<T>) -> T {
    for _ in 0..600 {
        if let Some(found) = look() {
            return found;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("nothing came in a minute");
}

/// Waits for `child` to exit and collects its output, with what it said to `stderr` after
/// `said`.
///
/// The caller has already taken `child`'s standard error and left its standard output piped.
fn ended(mut child: Child, mut stderr: impl Read, mut said: String) -> Output {
    let status = poll(|| child.try_wait().unwrap());
    stderr.read_to_string(&mut said).unwrap();
    let mut stdout = Vec::new();
    (child.stdout.take().unwrap().read_to_end(&mut stdout)).unwrap();
    Output {
        status,
        stdout,
        stderr: said.into_bytes(),
    }
}

/// Whether process `pid` runs: a zombie has ended.
fn alive(pid: i64) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z "))
}

/// The listing `ran` with each time that moves between runs replaced by `<t>`.
///
/// A row is a line that starts with `ids`.
fn masked(ran: &str, ids: &str) -> String {
    let lines = ran.lines().map(|line| {
        let mut line = line.to_owned();
        // A line under a run's starts 25 columns further in, where its id would end.
        let indent = line.len() - line.trim_start().len();
        let row = line.trim_start().starts_with(ids).then_some(indent);
        if let Some(at) = row.or_else(|| (indent > 25).then(|| indent - 25)) {
            let at = at + 37;
            line.replace_range(at..at + 7, "    <t>");
        }
        for (from, to) in [(" answered ", " ago"), ("ends by ", " UTC")] {
            if let Some(at) = line.rfind(from)
                && let Some(end) = line[at..].find(to)
            {
                line.replace_range(at + from.len()..at + end, "<t>");
            }
        }
        if let Some(at) = line.find(" in flight")
            && let Some(span) = line[at..].find("for ")
        {
            let span = at + span + "for ".len();
            let end = line[span..].find(';').map_or(line.len(), |end| span + end);
            line.replace_range(span..end, "<t>");
        }
        line
    });
    lines.collect::<Vec<String>>().join("\n")
}

/// A workflow whose agent holds leases and a job function, matching `whole_log`.
fn lease_task() -> Value {
    let gpu = json!({ "count": 1, "model": "NVIDIA A100-SXM4-80GB" });
    let lease = |name: &str, seconds: u64| {
        json!({ "name": name, "session": {
            "runner": name, "instances": 1, "instance_seconds": seconds, "transfer_bytes": 4096,
        } })
    };
    let command = json!({ "argv": ["/bin/true"], "cwd": "/work" });
    let runner = json!({ "runner": { "cwd": "/work" } });
    let role = |name: &str, gpus: bool, runtime: Value| {
        let mut needs = json!({ "cpu_millis": 1000, "memory_bytes": 1_073_741_824 });
        if gpus {
            needs["gpu"] = gpu.clone();
        }
        let image = format!("sha256:{}", "1".repeat(64));
        json!({
            "name": name, "image": image, "user": { "uid": 1000, "gid": 1000 },
            "needs": needs, "runtime": runtime,
        })
    };
    let route = json!([{ "api": "responses", "models": ["model-a", "model-b"] }]);
    json!({
        "ir_version": "pumpkin.ir/v1",
        "name": "example/lease",
        "deadline_seconds": 1800,
        "roles": [
            role("solver", false, json!({ "agent": {
                "command": command,
                "finalize_grace_seconds": 0,
                "egress": { "model": route },
                "grants": [
                    { "name": "train", "job": {
                        "target": "trainer", "body": { "file": "//lib/train.star", "def": "train" },
                        "payload_max_bytes": 4096, "call_timeout_seconds": 60,
                    } },
                    lease("gpu", 60),
                    lease("cpu", 1800),
                ],
            } })),
            role("trainer", true, json!({ "target": {
                "cwd": "/work", "sessions": [lease("box", 60)],
            } })),
            role("scorer", true, json!({ "target": { "cwd": "/work", "egress": { "model": route } } })),
            role("gpu", true, runner.clone()),
            role("cpu", false, runner.clone()),
            role("box", false, runner),
        ],
        "final": {
            "target": "scorer", "body": { "file": "//task.star", "def": "final_call" },
            "payload_max_bytes": 4096, "timeout_seconds": 60,
        },
    })
}

#[derive(Clone, Copy)]
enum Prices {
    Both,
    Tokens,
    Sandboxes,
    Nothing,
}

/// Where `lease_task`'s roles run, with the prices that `prices` names.
fn lease_places(prices: Prices) -> Value {
    let (sandboxes, tokens) = match prices {
        Prices::Both => (true, true),
        Prices::Tokens => (false, true),
        Prices::Sandboxes => (true, false),
        Prices::Nothing => (false, false),
    };
    let place = |provider: &str, class: &str| json!({ "provider": provider, "class": class });
    let class = |startup_seconds: u64, usd_per_hour: f64| {
        let mut class = json!({ "startup_seconds": startup_seconds });
        if sandboxes {
            class["usd_per_hour"] = json!(usd_per_hour);
        }
        class
    };
    let mut model_a = json!({ "upstream_name": "vendor/model-a" });
    if tokens {
        // High enough that the fixture's few tokens cost cents.
        model_a["usd_per_mtok"] =
            json!({ "input": 300.0, "cached": 30.0, "cache_write": 375.0, "output": 1500.0 });
    }
    let route = |model: &str, served: Value| json!([{ "api": "responses", "upstream": "gateway", "models": { model: served } }]);
    json!({
        "roles": {
            "solver": place("aws", "judge"),
            "cpu": place("aws", "judge"),
            "box": place("aws", "box"),
            "trainer": place("modal", "a100_80"),
            "scorer": place("modal", "a100_80"),
            "gpu": place("modal", "a100_80"),
        },
        "providers": {
            "aws": { "classes": { "judge": class(120, 0.3672), "box": { "startup_seconds": 60 } } },
            "modal": { "classes": { "a100_80": class(1800, 2.5) } },
        },
        "routes": {
            "solver": route("model-a", model_a),
            "scorer": route("model-b", json!({ "upstream_name": "vendor/model-b" })),
        },
    })
}

fn logged(seconds: u64, event: Value, actions: Value) -> Value {
    let mut record = json!({ "at_ns": (seconds * 1_000_000_000).to_string() });
    record["event"] = event;
    record["actions"] = actions;
    record
}

fn started_work(seconds: u64) -> [Value; 2] {
    [
        logged(seconds, json!({ "kind": "work_started" }), json!([])),
        logged(
            seconds,
            json!({ "kind": "agent_started", "caller": "agent-0" }),
            json!([]),
        ),
    ]
}

fn agent_exited(seconds: u64) -> Value {
    logged(
        seconds,
        json!({ "kind": "agent_exited", "caller": "agent-0", "status": { "code": 0 } }),
        json!([{ "kind": "tear_down_agent", "caller": "agent-0" }]),
    )
}

fn returned(score: f64) -> Value {
    let values = json!([
        { "name": "score", "value": score },
        { "name": "metrics", "value": { "solved": 3 } },
    ]);
    finished(
        &json!({ "kind": "returned", "result": { "values": values } }),
        &json!({ "kind": "agent_exited", "status": { "code": 0 } }),
        &submission(),
    )
}

fn finished(verdict: &Value, work_end: &Value, submission: &Value) -> Value {
    json!({
        "state": "finished",
        "verdict": verdict,
        "agents": [{ "role": "solver", "work_end": work_end, "submission": submission }],
    })
}

fn unsubmitted() -> Value {
    let failure = json!({ "kind": "error", "reason": "solver submitted nothing" });
    finished(
        &json!({ "kind": "no_result", "failure": failure }),
        &json!({ "kind": "deadline" }),
        &Value::Null,
    )
}

/// A finished run's `outcome.json` as a build whose verdicts this one doesn't read wrote it.
fn older_finished() -> Value {
    json!({ "state": "finished", "verdict": { "kind": "graded" } })
}

fn submission() -> Value {
    json!({ "payload": payload(), "finalized": true })
}

/// A payload of 3 bytes, as the log and `outcome.json` name one.
fn payload() -> Value {
    let digest = "sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674";
    json!({ "size": 3, "digest": digest })
}

/// The log of a run of `lease_task`, with every kind of lease, call and model request that `ls`
/// and `wait` show.
#[expect(clippy::too_many_lines, reason = "one line for each record")]
fn whole_log() -> Vec<Value> {
    let none = || json!([]);
    let tick = || json!({ "kind": "tick" });
    let started = |request: u64| json!({ "kind": "model_request_started", "caller": "agent-0", "request": request });
    let answered = |request: u64, usage: Value, first_token_ns: Option<&str>| {
        let mut answered = json!({
            "kind": "model_answered", "caller": "agent-0", "request": request, "model": "model-a",
            "api": "responses", "answer": { "kind": "forwarded" }, "upstream_status": 200, "usage": usage,
        });
        if let Some(first_token_ns) = first_token_ns {
            answered["first_token_ns"] = json!(first_token_ns);
        }
        answered
    };
    let ask = |job: u64| {
        let request = json!({ "kind": "call", "grant": "train" });
        let timeout = "600000000000";
        let start = json!({
            "kind": "start_call", "caller": "agent-0", "job": job, "grant": "train", "payload": payload(),
            "timeout_ns": timeout,
        });
        (
            json!({ "kind": "op_arrived", "request": request }),
            json!([start]),
        )
    };
    let of_job = |kind: &str, job: u64| json!({ "kind": kind, "job": job });
    let of_lease = |kind: &str, lease: u64| json!({ "kind": kind, "lease": lease });
    let open = |grant: &str, lease: u64| {
        let request = json!({ "kind": "lease_open", "grant": grant });
        let start =
            json!({ "kind": "start_lease", "grant": grant, "holder": "agent-0", "lease": lease });
        (
            json!({ "kind": "op_arrived", "request": request }),
            json!([start]),
        )
    };
    let ran = |seconds: u64, lease: u64, argv: &[&str]| {
        let request = json!({ "kind": "lease_run", "lease": lease, "argv": argv });
        let run = json!([{ "kind": "run_command", "lease": lease, "command": 0 }]);
        logged(
            seconds,
            json!({ "kind": "op_arrived", "request": request }),
            run,
        )
    };
    let ended = |lease: u64, end: Value| json!({ "kind": "command_ended", "lease": lease, "command": 0, "end": end });
    let exited = || json!({ "exited": { "code": 0 } });
    let saw = |sandbox: &str, gpu: &str| json!({ "kind": "sandbox_gpus", "sandbox": sandbox, "gpus": [gpu] });
    let close = |lease: u64| json!({ "kind": "close_lease", "lease": lease });
    let call_returned = |job: u64, returned: Value| json!({ "kind": "call_returned", "job": job, "returned": returned });
    let failed =
        |job: u64, reason: &str| json!({ "kind": "call_failed", "job": job, "reason": reason });
    let deleted = |sandbox: &str| json!({ "kind": "sandbox_deleted", "sandbox": sandbox });
    let sxm = "NVIDIA A100-SXM4-80GB";
    let target_put = json!({
        "kind": "target_put", "job": 0, "path": "/work/payload", "bytes": 6,
        "digest": "sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd",
    });
    let tokens = |input: u64, output: u64, cached: u64| json!({ "input_tokens": input, "output_tokens": output, "cached_tokens": cached });
    let mut reasoned = tokens(1000, 100, 800);
    reasoned["reasoning_tokens"] = json!(40);
    let (asked_0, start_0) = ask(0);
    let (asked_1, start_1) = ask(1);
    let (asked_2, start_2) = ask(2);
    let (asked_3, start_3) = ask(3);
    let (opened_1, start_1_lease) = open("gpu", 1);
    let (opened_2, start_2_lease) = open("cpu", 2);
    let (opened_3, start_3_lease) = open("gpu", 3);
    let boxed = json!([{ "kind": "start_lease", "grant": "box", "holder": "job-0", "lease": 0 }]);
    let put = json!({ "kind": "op_arrived", "request": {
        "kind": "lease_put", "lease": 0, "tree": "/pumpkin/in/payload", "to": "/work/in",
        "replace": false,
    } });
    let transfer = json!([{ "kind": "transfer", "lease": 0, "command": 0, "direction": "put" }]);
    let moved = json!({
        "kind": "transfer_ended", "lease": 0, "command": 0,
        "end": { "moved": { "bytes": 1, "entries": 1 } },
    });
    let closing = json!({ "kind": "op_arrived", "request": { "kind": "lease_close", "lease": 1 } });
    let unstarted = "the job's sandbox didn't start: Modal refused the helper's `create`: \
                     ResourceExhaustedError: Insufficient capacity to create sandbox: fix what \
                     Modal reports";
    let final_call = json!([{
        "kind": "start_final_call", "job": 4, "selections": [{"role":"solver","selection":payload()}], "timeout_ns": "900000000000",
    }]);
    let values = json!([
        { "name": "score", "value": 49.36 },
        { "name": "metrics", "value": { "solved": 3 } },
    ]);
    let finish = json!([{ "kind": "finish" }]);
    let exit = json!({ "kind": "agent_exited", "caller":"agent-0", "status": { "code": 0 } });
    let final_asked = json!({ "kind": "model_request_started", "caller": "job-4", "request": 3 });
    let final_answered = json!({
        "kind": "model_answered", "caller": "job-4", "request": 3, "model": "model-b",
        "api": "responses", "answer": { "kind": "forwarded" }, "upstream_status": 200,
        "failed": { "code": "rate_limit_exceeded" },
        "usage": { "input_tokens": 500, "output_tokens": 50 },
        "first_token_ns": "1013000000000",
        "kept": { "request": { "dropped_headers": ["x-stainless-lang", "x-title"] } },
    });
    // As the Messages API reports a cache's writes.
    let mut written = tokens(2000, 200, 1800);
    written["cache_write_tokens"] = json!(150);
    vec![
        logged(5, json!({"kind":"work_started"}), none()),
        logged(
            5,
            json!({"kind":"agent_started","caller":"agent-0"}),
            none(),
        ),
        logged(8, started(0), none()),
        logged(10, answered(0, reasoned, Some("9250000000")), none()),
        logged(15, started(1), none()),
        logged(20, answered(1, written, Some("16500000000")), none()),
        logged(26, started(2), none()),
        logged(31, answered(2, Value::Null, None), none()),
        logged(40, asked_0, start_0),
        logged(46, of_job("call_started", 0), none()),
        logged(47, target_put, none()),
        logged(
            47,
            json!({ "kind": "target_run", "job": 0, "command": 0, "argv": ["train"] }),
            none(),
        ),
        logged(
            48,
            json!({ "kind": "command_ended", "job": 0, "command": 0, "end": exited() }),
            none(),
        ),
        logged(50, tick(), boxed),
        logged(55, of_lease("lease_ready", 0), none()),
        logged(56, put, transfer),
        logged(57, moved, none()),
        ran(58, 0, &["sh", "-c", "set -e\nprint 7"]),
        logged(62, ended(0, exited()), none()),
        logged(
            100,
            call_returned(0, json!([{ "name": "score", "value": 49.53 }])),
            json!([close(0)]),
        ),
        logged(102, of_lease("lease_ended", 0), none()),
        logged(110, deleted("job-0"), none()),
        logged(110, of_job("call_torn_down", 0), none()),
        logged(120, opened_1, start_1_lease),
        logged(120, opened_2, start_2_lease),
        logged(126, saw("lease-1", sxm), none()),
        logged(126, of_lease("lease_ready", 1), none()),
        ran(130, 1, &["nvidia-smi"]),
        logged(132, ended(1, exited()), none()),
        logged(140, closing, json!([close(1)])),
        logged(145, of_lease("lease_ended", 1), none()),
        logged(150, of_lease("lease_ready", 2), none()),
        ran(151, 2, &["sleep", "60"]),
        logged(181, ended(2, json!("timed_out")), none()),
        logged(200, asked_1, start_1),
        logged(260, failed(1, unstarted), none()),
        logged(260, of_job("call_torn_down", 1), none()),
        logged(300, asked_2, start_2),
        logged(310, saw("job-2", sxm), none()),
        logged(310, of_job("call_started", 2), none()),
        logged(320, tick(), json!([of_job("stop_call", 2)])),
        logged(330, failed(2, "the call was stopped"), none()),
        logged(335, deleted("job-2"), none()),
        logged(335, of_job("call_torn_down", 2), none()),
        logged(400, asked_3, start_3),
        logged(405, of_job("call_started", 3), none()),
        logged(410, of_job("call_timed_out", 3), none()),
        logged(415, call_returned(3, json!([])), none()),
        logged(415, deleted("job-3"), none()),
        logged(415, of_job("call_torn_down", 3), none()),
        logged(500, opened_3, start_3_lease),
        logged(505, saw("lease-3", "NVIDIA A100 80GB PCIe"), none()),
        logged(505, of_lease("lease_ready", 3), none()),
        ran(506, 3, &["true"]),
        logged(507, ended(3, exited()), none()),
        logged(565, tick(), json!([close(3)])),
        logged(570, of_lease("lease_ended", 3), none()),
        logged(
            1000,
            exit,
            json!([{ "kind": "tear_down_agent", "caller": "agent-0" }, close(2)]),
        ),
        logged(
            1002,
            json!({"kind":"agent_torn_down","caller":"agent-0"}),
            none(),
        ),
        logged(1005, of_lease("lease_ended", 2), none()),
        logged(1006, tick(), final_call),
        logged(1010, saw("job-4", sxm), none()),
        logged(1010, of_job("call_started", 4), none()),
        logged(1012, final_asked, none()),
        logged(1020, final_answered, none()),
        logged(1100, call_returned(4, values), none()),
        logged(1100, deleted("job-4"), none()),
        logged(1100, of_job("call_torn_down", 4), finish),
    ]
}

/// The records of `log` written within `seconds` of the run's start.
fn cut(log: &[Value], seconds: u64) -> Vec<Value> {
    let written = |record: &&Value| {
        let at: u64 = record["at_ns"].as_str().unwrap().parse().unwrap();
        at <= seconds * 1_000_000_000
    };
    log.iter().filter(written).cloned().collect()
}

/// Writes the record of run `id`, started at `started` since the Unix epoch, with `log` after
/// its `run_started`, and returns the run's directory.
///
/// The run has a `driver.pid` that no process locks, and an `outcome.json` only if `outcome` is
/// given.
fn write_run(
    scratch: &Scratch,
    id: &str,
    started: Duration,
    log: &[Value],
    outcome: Option<Value>,
    places: &Value,
) -> PathBuf {
    write_record(scratch, id, true, outcome);
    let dir = scratch.0.join("runs").join(id);
    scratch.write(&format!("runs/{id}/ir.json"), &lease_task());
    scratch.write(&format!("runs/{id}/provider.json"), places);
    let unix_ns = started.as_nanos().to_string();
    let start =
        json!({ "kind": "run_started", "unix_ns": unix_ns, "hard_end_ns": "7200000000000" });
    let start = logged(0, start, json!([{ "kind": "start_agents" }]));
    let mut lines = String::new();
    for (seq, record) in (0_u64..).zip(iter::once(&start).chain(log)) {
        let mut record = record.clone();
        record["seq"] = json!(seq);
        lines.push_str(&record.to_string());
        lines.push('\n');
        let event = &record["event"];
        let sandbox = match event["kind"].as_str() {
            Some("agent_started") => Some("agents/solver".to_owned()),
            Some("call_started") => Some(format!("jobs/{}", event["job"])),
            Some("lease_ready") => Some(format!("leases/{}", event["lease"])),
            _ => None,
        };
        if let Some(sandbox) = sandbox {
            let deleted = json!({ "delete": { "state": "deleted" } });
            scratch.write(&format!("runs/{id}/{sandbox}/sandbox.json"), &deleted);
        }
    }
    scratch.write(&format!("runs/{id}/log.jsonl"), &lines);
    dir
}

/// The time `seconds` after the one that `id` names, as a Unix time.
fn asked(id: &str, seconds: u64) -> Duration {
    runtime::minted(id).unwrap() + Duration::from_secs(seconds)
}

fn write_record(scratch: &Scratch, id: &str, pid: bool, outcome: Option<Value>) {
    let args = [
        "--runs".to_owned(),
        scratch.0.join("runs").display().to_string(),
        scratch.0.join("task.star").display().to_string(),
    ];
    scratch.write(
        &format!("runs/{id}/record.json"),
        &json!({ "record_version": "pumpkin.record/v1", "pumpkin": this_build(), "args": args }),
    );
    if pid {
        scratch.write(&format!("runs/{id}/driver.pid"), "4242\n");
    }
    if let Some(outcome) = outcome {
        scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
    }
}

/// What a record made by this build names as its `pumpkin`.
fn this_build() -> Value {
    let build = runtime::BUILD;
    let commit = build.commit_hash().map(|hash| {
        let shown = build.to_string();
        let date = shown.trim_end_matches(')').rsplit(' ').next().unwrap();
        json!({ "hash": hash, "date": date })
    });
    let version = build.to_string();
    let version = version.split(' ').next().unwrap();
    json!({ "version": version, "commit": commit })
}

fn write_docker_record(scratch: &Scratch, id: &str, context: &str, outcome: Option<Value>) {
    write_record(scratch, id, true, outcome);
    let ready = json!({ "report": { "context": context } });
    let provided = json!({ "providers": { "local": ready } });
    scratch.write(&format!("runs/{id}/provider.json"), &provided);
}

/// Builds the images of `task` on the engine that `config` names, so that a `run` of it after
/// finds them by their tags and says nothing of building them.
fn build_first(scratch: &Scratch, task: &str, config: &str) {
    let entry = format!("{task}/task.star");
    let built = scratch
        .pumpkin(&["build", &entry, "--config", config])
        .output();
    let built = built.unwrap();
    assert!(built.status.success(), "{built:?}");
}

/// The fixture's workflow with `argv` as the agent's command and no model route, every role
/// running `image` with a small share of a machine.
fn workflow(image: &str, argv: &[&str]) -> Value {
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    ir["roles"][0]["runtime"]["agent"]
        .as_object_mut()
        .unwrap()
        .remove("egress");
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] = json!(argv);
    for role in ir["roles"].as_array_mut().unwrap() {
        role["image"] = json!(image);
        role["needs"] = json!({ "cpu_millis": 500, "memory_bytes": 256 << 20 });
    }
    // Far later than the agent's end, so that a slow engine makes a run wait longer, never end
    // differently.
    ir["deadline_seconds"] = json!(300);
    ir
}

/// The number of CPUs, hardware threads included, that the machine has online.
fn machine_cpus() -> u64 {
    cpu_list(&fs::read_to_string("/sys/devices/system/cpu/online").unwrap()).len() as u64
}

/// The number of cores that the machine has online, each counted once for its hardware threads.
fn machine_cores() -> u64 {
    let online = cpu_list(&fs::read_to_string("/sys/devices/system/cpu/online").unwrap());
    let cores: BTreeSet<BTreeSet<u32>> = online.into_iter().map(siblings).collect();
    cores.len() as u64
}

fn cpu_list(text: &str) -> BTreeSet<u32> {
    let ranges = text.trim().split(',').filter(|range| !range.is_empty());
    ranges
        .flat_map(|range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            first.parse::<u32>().unwrap()..=last.parse().unwrap()
        })
        .collect()
}

fn siblings(cpu: u32) -> BTreeSet<u32> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    cpu_list(&fs::read_to_string(path).unwrap())
}

/// Sends the signal `name` to `target`, a process ID or a process group's negated, and returns
/// whether it was delivered.
fn signal(name: &str, target: i64) -> bool {
    let kill = stdx::process::command("kill", Environment::Inherited)
        .args(["--signal", name, "--", &target.to_string()])
        .status();
    kill.unwrap().success()
}

fn docker_table(context: &str) -> String {
    format!("[docker]\ncontext = \"{context}\"\n")
}

const LOCAL: &str = "[providers.local.docker]\n";

const EC2: &str = r#"[providers.aws.ec2]
region = "us-west-1"
classes = { agent = { template = "lt-0aaaaaaaaaaaaaaaa", version = 7, vcpus = 32, memory_mib = 262144 }, judge = { template = "lt-0bbbbbbbbbbbbbbbb", version = 2, vcpus = 8, memory_mib = 16384 } }
repositories = { roles = "pumpkin/roles", sandboxd = "pumpkin/sandboxd" }
timer = { group = "pumpkin", role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer" }
"#;

const MODAL: &str = r#"[providers.gpus.modal]
profile = "pumpkin"
dials_from = "203.0.113.7"
images = { region = "us-west-1", repository = "pumpkin/roles", role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull" }
classes = { h100 = { gpu = "H100!", gpu_model = "NVIDIA H100 80GB HBM3", cpu_millis = 4000, memory_bytes = 17179869184, startup_seconds = 1800 } }
"#;

fn install(scratch: &Scratch, dir: &str) -> PathBuf {
    let dir = scratch.0.join(dir);
    fs::create_dir_all(&dir).unwrap();
    // A link, which writes nothing where a copy would write the whole executable; `pumpkin` finds
    // its workspace by the path it runs from, this one.
    fs::hard_link(PUMPKIN, dir.join("pumpkin")).unwrap();
    // The pinned formatter, which `pumpkin fmt` and `pumpkin check` run from beside the `pumpkin`
    // that runs, by the link that `.cargo/config.toml` names.
    let link = env!("PUMPKIN_BUILDIFIER");
    let buildifier = fs::canonicalize(link).unwrap_or_else(|error| {
        panic!(
            "{link} doesn't lead to the formatter ({error}): make it with `cargo xtask \
             install-tools`"
        )
    });
    fs::hard_link(buildifier, dir.join("buildifier")).unwrap();
    dir.join("pumpkin")
}

fn engine(scratch: &Scratch, context: &str) -> Engine {
    let context = ContextName::try_from(context.to_owned()).unwrap();
    Engine::connect(context, &scratch.0.join(".pumpkin/docker")).unwrap()
}

fn containers(context: &str) -> Reached {
    Docker::reach(ContextName::try_from(context.to_owned()).unwrap()).unwrap()
}

fn machine(context: &str) -> String {
    format!("{}\n{LOCAL}", docker_table(context))
}

// What a recipe holds that the stand-in `docker` builds.
const BUILDS: &str = "# The stand-in builds this recipe.";

// What `docker info` prints of the stand-in's engine: a rootless one that enforces every limit,
// on a host of 8 GiB of memory, and keeps its images in the containerd image store.
const INFO: &str = r#"{"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUSet":true,"PidsLimit":true,"DriverStatus":[["driver-type","io.containerd.snapshotter.v1"]],"SecurityOptions":["name=seccomp,profile=builtin","name=rootless"],"KernelVersion":"6.0","OperatingSystem":"stand-in","ServerVersion":"29.0.0","ClientInfo":{"Version":"29.0.0"},"MemTotal":8589934592}"#;

const IMAGE: &str = r#"{"Os":"linux","Architecture":"amd64","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json"}}"#;

/// A stand-in for `docker` in the scratch directory, with the tasks `stand-in`, `lacking` and
/// `unbuilt`.
///
/// Its answers hold the calls of one command, such as `image`, between `hold` and `release`.
/// The stand-in's CPU ledger lives outside the scratch directory, and dropping it removes that.
struct StandIn<'a> {
    scratch: &'a Scratch,
}

impl<'a> StandIn<'a> {
    fn new(scratch: &'a Scratch) -> Self {
        Self::with(scratch, INFO, IMAGE)
    }

    #[expect(clippy::too_many_lines, reason = "most of it is the stand-in's script")]
    fn with(scratch: &'a Scratch, info: &str, image: &str) -> Self {
        let id = scratch.engine_id();
        let info = info.replace(r#""MemTotal""#, &format!(r#""ID":"{id}","MemTotal""#));
        let dir = scratch.0.display();
        let digest = format!("sha256:{}", "0".repeat(64));
        let absent = format!("sha256:{}", "1".repeat(64));
        let built = format!("sha256:{}", "2".repeat(64));
        let docker = format!(
            r#"#!/bin/sh
touch "{dir}/docker.ran"
# The build's look for an image by its tag, `image inspect --format {{{{.Id}}}} -- <tag>`, is a
# command of its own to hold.
name=$1
case "$1 $2 $4" in
    "image inspect "*.Id*) name=lookup ;;
    "image inspect "*.Size*) name=size ;;
esac
if [ -e "{dir}/hold-$name" ]; then
    echo $PPID > "{dir}/at-$name.tmp" && mv "{dir}/at-$name.tmp" "{dir}/at-$name"
    # At most the minute that `poll` waits, so that the driver of a test killed meanwhile ends.
    i=0
    while [ -e "{dir}/hold-$name" ] && kill -0 $PPID 2>/dev/null && [ $i -lt 1200 ]; do
        sleep 0.05; i=$((i + 1))
    done
fi
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{info}' ;;
    version) echo linux/amd64 ;;
    # `image ls --all --no-trunc --quiet`, `image inspect --format <format> -- <image>`, which
    # says it has no image that the engine lacks, or for
    # the tag `pumpkin/lacking:<digest>`, the image that the engine lacks, `image inspect --format
    # {{{{.Id}}}} -- <tag>`; for the tag `pumpkin/unbuilt:<digest>`, no image, which a build
    # then fails to make; and `image inspect --format {{{{.Size}}}} -- <image>`
    image) case $2 in
        ls) echo {digest}; cut --delimiter=' ' --fields=1 "{dir}/images" 2>/dev/null || true ;;
        rm) echo "docker $*" >> "{dir}/docker.pruned" ;;
        *) case $name in
            lookup) case $6 in
                pumpkin/lacking:*) echo {absent} ;;
                pumpkin/built:*) grep --quiet --line-regexp --fixed-strings "$6" "{dir}/tagged" 2>/dev/null && echo {built} ||
                    {{ echo "Error: No such image: $6" >&2; exit 1; }} ;;
                pumpkin/unbuilt:*) echo "Error: No such image: $6" >&2; exit 1 ;;
                *) echo {digest} ;;
            esac ;;
            size) sed --quiet "s/^$6 //p" "{dir}/images" 2>/dev/null | grep . || echo 0 ;;
            *) if [ "$6" = {absent} ]; then echo "Error response from daemon: No such image: $6" >&2; exit 1; fi
               echo '{image}' ;;
        esac ;;
    esac ;;
    container) ;;
    builder) echo "docker $*" >> "{dir}/docker.pruned" ;;
    # `build ... --file <recipe> --iidfile <file> -- <stage>`, which writes the image's ID there.
    build) for arg; do
        case $previous in
            --file) grep --quiet --fixed-strings "{BUILDS}" "$arg" || {{ echo "the stand-in creates nothing" >&2; exit 1; }} ;;
            --iidfile) echo {built} > "$arg" ;;
        esac
        previous=$arg
    done ;;
    # `tag -- <image> <tag>`.
    tag) echo "$4" >> "{dir}/tagged" ;;
    *) echo "the stand-in creates nothing" >&2; exit 1 ;;
esac
"#
        );
        scratch.write("bin/docker", &docker);
        let mode = Permissions::from_mode(0o755);
        fs::set_permissions(scratch.0.join("bin/docker"), mode).unwrap();
        scratch.write_sandboxd("sandboxd");
        scratch.write("pumpkin.toml", &machine("default"));
        scratch.write_task("stand-in", &workflow(&digest, &["true"]));
        scratch.write_task("lacking", &workflow(&absent, &["true"]));
        scratch.write_task("unbuilt", &workflow(&digest, &["true"]));
        Self { scratch }
    }

    fn hold(&self, command: &str) {
        self.scratch.write(&format!("hold-{command}"), "");
    }

    fn held(&self, command: &str) -> i64 {
        let at = self.scratch.0.join(format!("at-{command}"));
        poll(|| fs::read_to_string(&at).ok()?.trim().parse().ok())
    }

    fn release(&self, command: &str) {
        let _ = fs::remove_file(self.scratch.0.join(format!("hold-{command}")));
    }

    fn run(&self, entry: &str, rest: &[&str]) -> Command {
        signals_at_default();
        let mut run = (self.scratch).pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        run.args(["--runs", "runs", "--config", "pumpkin.toml"])
            .args(rest);
        let bin = self.scratch.0.join("bin");
        run.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        run.process_group(0);
        run
    }
}

/// `command` run by a shell after `setup`, in a process group of its own.
fn through_shell(command: &Command, setup: &str) -> Command {
    let mut shell = stdx::process::command("/bin/sh", Environment::Inherited);
    shell.args(["-c", &format!("{setup} && exec \"$@\""), "sh"]);
    shell.arg(command.get_program()).args(command.get_args());
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => shell.env(name, value),
            None => shell.env_remove(name),
        };
    }
    if let Some(dir) = command.get_current_dir() {
        shell.current_dir(dir);
    }
    shell.process_group(0);
    shell
}

/// Panics if this thread ignores or blocks `SIGINT` or `SIGTERM`, which a child would inherit.
fn signals_at_default() {
    let status = fs::read_to_string("/proc/thread-self/status").unwrap();
    let mask = |field: &str| {
        let hex = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap();
        u64::from_str_radix(hex.trim(), 16).unwrap()
    };
    let signals = [libc::SIGINT, libc::SIGTERM].map(|number| 1_u64 << (number - 1));
    let signals = signals.into_iter().fold(0, |all, signal| all | signal);
    let (ignored, blocked) = (mask("SigIgn:") & signals, mask("SigBlk:") & signals);
    assert!(
        ignored == 0 && blocked == 0,
        "the test ignores SIGINT or SIGTERM ({ignored:#x}) or blocks them ({blocked:#x}), and \
         `pumpkin run` would inherit that: run the tests from a terminal's foreground job, as \
         `cargo xtask ci` there does"
    );
}

impl Drop for StandIn<'_> {
    fn drop(&mut self) {
        for command in ["context", "image", "create"] {
            self.release(command);
        }
        let _ = fs::remove_dir_all(ledger(self.scratch));
    }
}

/// The directory of the CPU ledger that `StandIn`'s engine uses, which is named by its ID.
///
/// The engine is the scratch directory's own, so that the stand-in's runs never wait for others.
fn ledger(scratch: &Scratch) -> PathBuf {
    Path::new("/var/tmp/pumpkin-cpus").join(scratch.engine_id())
}
