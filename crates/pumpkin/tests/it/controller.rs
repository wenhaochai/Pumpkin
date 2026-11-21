use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File, Permissions};
use std::io::{BufRead as _, BufReader, Read};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use expect_test::{Expect, expect};
use schema::ir::Sha256Digest;
use serde_json::json;
use stdx::process::Environment;
use test_support::BUSYBOX;

use super::view::Served;
use super::{
    BUILDS, EC2, IMAGE, INFO, LOCAL, MODAL, PUMPKIN, Prices, Scratch, StandIn, asked, cpu_list,
    cut, docker_table, ended, install, invocation, lease_places, masked, older_finished, poll,
    report, returned, whole_log, workflow, write_record, write_run,
};

fn parameter() -> String {
    let images = MODAL
        .lines()
        .find(|line| line.starts_with("images"))
        .unwrap();
    format!(
        "{}\n{EC2}\n[providers.gpus.modal]\n{images}\n",
        docker_table("default")
    )
}

const SENT_MODAL: &str = r#"[modal]
profile = "alice"
classes = { a100 = { gpu = "A100-80GB", gpu_model = "NVIDIA A100-SXM4-80GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 } }
"#;

// Notes each call in `calls`, and answers only `--version` and `ssm get-parameter`.
const AWS: &str = r#"#!/bin/sh
{ echo aws; printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
case "$*" in
    --version) echo aws-cli/2.38.1 Python/3.14.6 Linux/7.0.0 exe/x86_64.stand-in ;;
    *" ssm get-parameter "*) cat "${0%/*}/parameter.json" ;;
    *) echo "the stand-in aws answers only \`--version\` and \`ssm get-parameter\`" >&2; exit 1 ;;
esac
"#;

fn write_parameter(scratch: &Scratch, text: &str) {
    let parameter = json!({ "Parameter": { "Name": "/pumpkin-alice/settings", "Type": "String", "Value": text } });
    scratch.write("bin/parameter.json", &parameter);
}

const CURL: &str = r#"#!/bin/sh
{ echo curl; printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
for url; do :; done
case $url in
    */api/token) echo stand-in-token ;;
    */meta-data/local-ipv4) echo 172.31.5.6 ;;
    */meta-data/public-ipv4) echo 198.51.100.9 ;;
    *) echo "curl: (22) The requested URL returned error: 404" >&2; exit 22 ;;
esac
"#;

// Answers the size and the bytes available that `disk` holds; a `disk.next` then replaces `disk`,
// so that the space changes between two reads.
const DF: &str = r#"#!/bin/sh
{ echo df; printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
echo '1B-blocks Avail'
cat "${0%/*}/disk"
[ ! -e "${0%/*}/disk.next" ] || mv "${0%/*}/disk.next" "${0%/*}/disk"
"#;

fn id_ago(days: u64, random: &str) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let then = now.checked_sub(Duration::from_hours(days * 24)).unwrap();
    let stamp = jiff::Timestamp::from_second(i64::try_from(then.as_secs()).unwrap()).unwrap();
    format!("{}-{random}", stamp.strftime("%Y%m%d-%H%M%S"))
}

fn image(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

fn set_up(scratch: &Scratch) {
    scratch.write_executable("bin/aws", AWS.as_bytes());
    scratch.write_executable("bin/curl", CURL.as_bytes());
    scratch.write_executable("bin/df", DF.as_bytes());
    scratch.write("bin/disk", "1000 500\n");
    write_parameter(scratch, &parameter());
    install(scratch, &format!("home/.pumpkin/{PINNED}/bin"));
    fs::create_dir_all(scratch.0.join("run")).unwrap();
    let empty = format!("home/store/manifests/{EMPTY_TREE}");
    scratch.write(&empty, "pumpkin-tree/1\n");
}

const EMPTY_TREE: &str = "dff3cfced0b99bbac128797b268e1756972cb1285a305e2dc1b9d96e97e6eb5e";

fn controllers(scratch: &Scratch) -> PathBuf {
    scratch
        .0
        .join(format!("home/.pumpkin/{PINNED}/bin/pumpkin"))
}

fn on_controller(scratch: &Scratch, args: &[&str]) -> Command {
    let mut command = scratch.program(controllers(scratch), args);
    command.env("XDG_RUNTIME_DIR", scratch.0.join("run")).env(
        "PATH",
        format!("{}:/usr/bin:/bin", scratch.0.join("bin").display()),
    );
    command
}

fn send(scratch: &Scratch, id: &str, config: &str, modal: bool) {
    let sent = format!("run/pumpkin/{id}");
    scratch.write(&format!("{sent}/config.toml"), config);
    scratch.write(&format!("{sent}/gateway.key"), "sk-stand-in\n");
    if modal {
        scratch.write(
            &format!("{sent}/modal.toml"),
            "[pumpkin]\ntoken_id = \"ak-x\"\n",
        );
    }
    fs::set_permissions(scratch.0.join(sent), Permissions::from_mode(0o700)).unwrap();
}

const UPSTREAMS: &str = r#"[upstreams.gateway]
url = "https://gateway.example/v1"
key_file = "gateway.key"
models = [{ name = "model-a", upstream_name = "provider/model-a" }]
"#;

fn noted(scratch: &Scratch) -> Vec<String> {
    let taken = |file: &str| {
        let path = scratch.0.join(file);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(path);
        text
    };
    let calls = taken("bin/calls");
    let calls =
        (calls.split_terminator("\n\n")).map(|call| call.lines().collect::<Vec<_>>().join(" "));
    let mut lines: Vec<String> = calls.collect();
    lines.extend(taken("docker.pruned").lines().map(str::to_owned));
    lines
}

fn check(scratch: &Scratch, text: &str, shown: &[(&str, &str)], expect: &Expect) {
    let mut text = text.replace(scratch.0.to_str().unwrap(), "<scratch>");
    text = text.replace(EMPTY_TREE, "<empty tree>");
    for (value, name) in shown {
        text = text.replace(value, name);
    }
    // The uptime and the disk that the controller's health reports vary.
    let lines = text.lines().map(|line| match line.split_once("`: up ") {
        Some((before, after)) => {
            let rest = after.split_once(',').map_or("", |(_, rest)| rest);
            let (rest, disk) = rest.split_once(", disk ").unwrap_or((rest, ""));
            let disk = disk
                .split_once("% used")
                .map_or(disk.to_owned(), |(_, rest)| {
                    format!(", disk <n>% used{rest}")
                });
            format!("{before}`: up <t>,{rest}{disk}")
        }
        None => line.to_owned(),
    });
    expect.assert_eq(&lines.collect::<Vec<String>>().join("\n"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the configuration's snapshot"
)]
fn job() {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let modal = MODAL.replace("dials_from = \"203.0.113.7\"\n", "");
    let parameter = format!("{}\n{EC2}\n{modal}", docker_table("default"));
    write_parameter(&scratch, &parameter);
    let id = id_ago(0, "0b000001");
    let (recent, old) = (id_ago(3, "0b000002"), id_ago(30, "0b000003"));
    let (built, failed) = (id_ago(1, "0b000007"), id_ago(2, "0b000008"));
    send(&scratch, &id, &format!("{UPSTREAMS}\n{SENT_MODAL}"), true);
    let ir = |digits: &str| {
        let roles = digits.chars().map(|digit| json!({ "image": image(digit) }));
        json!({ "roles": roles.collect::<Vec<_>>() })
    };
    for (run, digits) in [(&recent, "0a"), (&old, "b")] {
        scratch.write(&format!("home/jobs/{run}/pumpkin.toml"), "");
        let record = format!("home/.pumpkin/runs/{run}");
        scratch.write(&format!("{record}/ir.json"), &ir(digits));
    }
    let (stale, fresh) = (id_ago(30, "0b00000a"), id_ago(1, "0b00000b"));
    for unstarted in [&stale, &fresh] {
        scratch.write(&format!("home/jobs/{unstarted}/log"), "");
    }
    scratch.write(&format!("home/builds/{built}/out"), &ir("c"));
    scratch.write(&format!("home/builds/{failed}/out"), "");
    let held = format!("pumpkin/held:{}", "c".repeat(64));
    let sized = ['0', 'a', 'b', 'c', 'd'].map(|digit| match digit {
        'c' => format!("{} 100 {held}\n", image(digit)),
        _ => format!("{} 100\n", image(digit)),
    });
    scratch.write("images", &sized.concat());
    scratch.write("bin/disk", "1000 150\n");
    for tree in [&recent, &old, &".pumpkin".to_owned()] {
        scratch.write(&format!("home/trees/tasks/{tree}/pumpkin"), "");
    }
    let mut job = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    // As its unit's `StandardOutput=` does.
    fs::create_dir_all(scratch.0.join(format!("home/builds/{id}"))).unwrap();
    let out = scratch.0.join(format!("home/builds/{id}/out"));
    job.stdout(File::create(&out).unwrap());
    let invoked = invocation(&job);
    let mut lines = vec![report(invoked, &job.output().unwrap(), &scratch.0, false)];
    let ir = fs::read_to_string(&out).unwrap();
    lines.push(format!("out names the image: {}", ir.contains(&image('0'))));
    let config = fs::read_to_string(scratch.0.join(format!("home/builds/{id}/pumpkin.toml")));
    lines.push(format!("pumpkin.toml:\n{}", config.unwrap()));
    lines.extend(noted(&scratch));
    let names = |dir: &str| {
        let entries = fs::read_dir(scratch.0.join(dir)).unwrap();
        let mut names: Vec<String> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names.join(", ")
    };
    lines.push(format!("trees: {}", names("home/trees/tasks")));
    lines.push(format!("jobs: {}", names("home/jobs")));
    // Another job is running, its run started, and the volume is full.
    send(&scratch, &recent, UPSTREAMS, false);
    let log = format!("home/.pumpkin/runs/{recent}/log.jsonl");
    scratch.write(&log, "");
    scratch.write("bin/disk", "1000 0\n");
    for started in [true, false] {
        if !started {
            fs::remove_file(scratch.0.join(&log)).unwrap();
            scratch.write(&format!("run/pumpkin/{recent}/held"), &format!("{held}\n"));
        }
        send(&scratch, &id, UPSTREAMS, false);
        let mut again = on_controller(
            &scratch,
            &[
                "controller",
                "job",
                &id,
                "--region",
                "us-west-1",
                "--parameter",
                "/pumpkin-alice/settings",
                "--tree",
                EMPTY_TREE,
                "--",
                "build",
                "stand-in/task.star",
            ],
        );
        again.stdout(Stdio::null());
        let invoked = invocation(&again);
        lines.push(format!("another job's run started: {started}"));
        lines.push(report(invoked, &again.output().unwrap(), &scratch.0, false));
        lines.extend(
            noted(&scratch)
                .into_iter()
                .filter(|line| line.starts_with("docker")),
        );
    }
    // Full as the prune begins, and no longer once it has the lock.
    scratch.write("bin/disk", "1000 0\n");
    scratch.write("bin/disk.next", "1000 500\n");
    send(&scratch, &id, UPSTREAMS, false);
    let mut freed = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    freed.stdout(Stdio::null());
    let invoked = invocation(&freed);
    lines.push("freed while the prune waited for the lock:".to_owned());
    lines.push(report(invoked, &freed.output().unwrap(), &scratch.0, false));
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker") || line.starts_with("df")),
    );
    let config = fs::read_to_string(scratch.0.join(format!("home/builds/{id}/pumpkin.toml")));
    let config: toml::Table = toml::from_str(&config.unwrap()).unwrap();
    let providers: Vec<&String> = config["providers"].as_table().unwrap().keys().collect();
    lines.push(format!("the providers: {providers:?}"));
    let shown = [
        (id.as_str(), "<id>"),
        (recent.as_str(), "<recent>"),
        (old.as_str(), "<old>"),
        (built.as_str(), "<built>"),
        (failed.as_str(), "<failed>"),
        (stale.as_str(), "<stale>"),
        (fresh.as_str(), "<fresh>"),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &shown,
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            out names the image: true
            pumpkin.toml:
            [docker]
            context = "default"

            [gateway]
            private = "172.31.5.6:0"

            [providers.aws.ec2]
            region = "us-west-1"

            [providers.aws.ec2.classes.agent]
            memory_mib = 262144
            template = "lt-0aaaaaaaaaaaaaaaa"
            vcpus = 32
            version = 7

            [providers.aws.ec2.classes.judge]
            memory_mib = 16384
            template = "lt-0bbbbbbbbbbbbbbbb"
            vcpus = 8
            version = 2

            [providers.aws.ec2.repositories]
            roles = "pumpkin/roles"
            sandboxd = "pumpkin/sandboxd"

            [providers.aws.ec2.timer]
            group = "pumpkin"
            role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer"

            [providers.gpus.modal]
            config = "<scratch>/run/pumpkin/<id>/modal.toml"
            dials_from = "198.51.100.9"
            profile = "alice"

            [providers.gpus.modal.classes.a100]
            cpu_millis = 8000
            gpu = "A100-80GB"
            gpu_model = "NVIDIA A100-SXM4-80GB"
            memory_bytes = 68719476736
            startup_seconds = 1800

            [providers.gpus.modal.images]
            region = "us-west-1"
            repository = "pumpkin/roles"
            role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull"

            [upstreams.gateway]
            key_file = "<scratch>/run/pumpkin/<id>/gateway.key"
            url = "https://gateway.example/v1"

            [[upstreams.gateway.models]]
            name = "model-a"
            upstream_name = "provider/model-a"

            aws --version
            aws --no-cli-pager --output json --region us-west-1 ssm get-parameter --name /pumpkin-alice/settings
            curl --disable --silent --show-error --fail --max-time 10 --request PUT --header X-aws-ec2-metadata-token-ttl-seconds: 60 http://169.254.169.254/latest/api/token
            curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/local-ipv4
            curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/public-ipv4
            df --block-size=1 --output=size,avail -- /
            df --block-size=1 --output=size,avail -- /
            docker image rm --force sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
            docker image rm --force sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            docker image rm --force sha256:0000000000000000000000000000000000000000000000000000000000000000
            docker builder prune --force --min-free-space 400
            trees: .pumpkin, <recent>
            jobs: <old>, <recent>, <fresh>
            another job's run started: true
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            docker image rm --force sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
            docker image rm --force sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            docker image rm --force sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            docker builder prune --force --min-free-space 400
            another job's run started: false
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            docker image rm --force sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
            docker image rm --force sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            docker image rm --force sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
            docker builder prune --force --min-free-space 400
            freed while the prune waited for the lock:
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            df --block-size=1 --output=size,avail -- /
            df --block-size=1 --output=size,avail -- /
            the providers: ["aws"]"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn job_refusals() {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let id = id_ago(0, "0b000004");
    let job = |scratch: &Scratch, tree: &str| {
        on_controller(
            scratch,
            &[
                "controller",
                "job",
                &id,
                "--region",
                "us-west-1",
                "--parameter",
                "/pumpkin-alice/settings",
                "--tree",
                tree,
                "--",
                "build",
                "stand-in/task.star",
            ],
        )
    };
    let mut lines = Vec::new();
    let cases = [
        (
            "a parameter with [gateway]",
            format!("{}\n[gateway]\nprivate = \"10.0.0.1:0\"\n", parameter()),
            UPSTREAMS.to_owned(),
        ),
        (
            "a parameter with no Modal provider",
            format!("{}\n{EC2}", docker_table("default")),
            format!("{UPSTREAMS}\n{SENT_MODAL}"),
        ),
        (
            "a sent [docker]",
            parameter(),
            format!("{UPSTREAMS}\n[docker]\ncontext = \"elsewhere\"\n"),
        ),
    ];
    for (case, parameter, sent) in cases {
        write_parameter(&scratch, &parameter);
        send(&scratch, &id, &sent, false);
        let mut job = job(&scratch, EMPTY_TREE);
        lines.push(format!("{case}:"));
        let invoked = invocation(&job);
        lines.push(report(invoked, &job.output().unwrap(), &scratch.0, false));
    }
    let mut unset = job(&scratch, EMPTY_TREE);
    unset.env_remove("XDG_RUNTIME_DIR");
    let invoked = invocation(&unset);
    lines.push(report(invoked, &unset.output().unwrap(), &scratch.0, false));
    // Trees that no job makes: a file below the controller's `.pumpkin`, and one outside any tree.
    let planted = sha256(b"planted\n");
    scratch.write(&format!("home/store/{planted}"), "planted\n");
    let mut shown = vec![
        (id.clone(), "<id>".to_owned()),
        (planted.clone(), "<blob>".to_owned()),
    ];
    for path in [".pumpkin/runs/planted/record", "stand-in/notes"] {
        let text = format!("pumpkin-tree/1\nf {planted} 8 {path}\n");
        let tree = sha256(text.as_bytes());
        scratch.write(&format!("home/store/manifests/{tree}"), &text);
        let mut refused = job(&scratch, &tree);
        let invoked = invocation(&refused);
        lines.push(report(
            invoked,
            &refused.output().unwrap(),
            &scratch.0,
            false,
        ));
        shown.push((tree, format!("<tree of {path}>")));
    }
    let made = scratch.0.join("stand-in/notes").exists();
    lines.push(format!("made the file outside a job's tree: {made}"));
    let shown: Vec<(&str, &str)> = (shown.iter())
        .map(|(value, name)| (value.as_str(), name.as_str()))
        .collect();
    check(
        &scratch,
        &lines.join("\n"),
        &shown,
        &expect![[r#"
            a parameter with [gateway]:
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: the controller's settings, SSM parameter `/pumpkin-alice/settings`, hold `[gateway]`, which each job sets: remove it from them
            a parameter with no Modal provider:
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: the controller's settings, SSM parameter `/pumpkin-alice/settings`, name no Modal provider, whose `images` the client's `[controller.modal]` needs: apply `deploy/aws`, whose settings name one
            a sent [docker]:
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: what the run sent, config.toml, is refused: TOML parse error at line 6, column 2
              |
            6 | [docker]
              |  ^^^^^^
            unknown field `docker`, expected `upstreams` or `modal`
            $ PATH=<scratch>/bin:/usr/bin:/bin env --unset=XDG_RUNTIME_DIR pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: XDG_RUNTIME_DIR is unset or empty: run the job in a unit of the user's systemd, as the controller target starts it
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <tree of .pumpkin/runs/planted/record> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: <scratch>/home/store/manifests/<tree of .pumpkin/runs/planted/record>: the manifest's line `f <blob> 8 .pumpkin/runs/planted/record` is refused: `.pumpkin/runs/planted/record` can't be sent to the controller, where a job's `.pumpkin` is the controller's own workspace: leave it out of the task
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <tree of stand-in/notes> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: the job's working directory, <scratch>, isn't its tree, <scratch>/home/trees/<repository>/<id>, which its start makes: run the job there, as its unit does
            made the file outside a job's tree: false"#]],
    );
}

// A job's build slots are one for every two CPUs it may use, and one at least: on one CPU there
// is one, so a second job waits.
fn on_one_cpu(scratch: &Scratch, id: &str) -> Child {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"));
    let cpu = cpu_list(allowed.unwrap()).into_iter().next().unwrap();
    let job = on_controller(
        scratch,
        &[
            "controller",
            "job",
            id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "built/task.star",
        ],
    );
    let mut on_one = stdx::process::command("taskset", Environment::Inherited);
    on_one.args(["--cpu-list", &cpu.to_string()]);
    on_one.arg(job.get_program()).args(job.get_args());
    for (name, value) in job.get_envs() {
        on_one.env(name, value.unwrap());
    }
    on_one.current_dir(job.get_current_dir().unwrap());
    on_one.stdout(Stdio::piped()).stderr(Stdio::piped());
    on_one.spawn().unwrap()
}

fn job_ended(child: Child, stderr: impl Read, said: String) -> String {
    let output = ended(child, stderr, said);
    let mut lines = vec![format!("exit status: {:?}", output.status.code())];
    lines.extend(
        String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .map(str::to_owned),
    );
    let printed = String::from_utf8(output.stdout).unwrap();
    let built = format!("sha256:{}", "2".repeat(64));
    lines.push(format!(
        "printed the IR of the image built: {}",
        printed.contains(&built)
    ));
    lines.join("\n")
}

fn job_ended_whole(mut child: Child) -> String {
    let stderr = child.stderr.take().unwrap();
    job_ended(child, stderr, String::new())
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the jobs' steps and the snapshot"
)]
fn job_admission() {
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    scratch.write_task("built", &workflow(&image('0'), &["true"]));
    let recipe = format!("FROM {BUSYBOX}\n{BUILDS}\n");
    scratch.write("built/Dockerfile", &recipe);
    let [first, second, third] =
        ["0ad00001", "0ad00002", "0ad00003"].map(|random| id_ago(0, random));
    for id in [&first, &second, &third] {
        send(&scratch, id, UPSTREAMS, false);
    }
    let mut lines = Vec::new();
    let marks = |id: &str| {
        let dir = scratch.0.join(format!("run/pumpkin/{id}"));
        let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names.join(", ")
    };
    stand_in.hold("build");
    let building = on_one_cpu(&scratch, &first);
    stand_in.held("build");
    let held = scratch.0.join(format!("run/pumpkin/{first}/held"));
    let held = fs::read_to_string(held).unwrap();
    lines.push(format!("the first holds: {}", held.trim()));
    lines.push(format!(
        "the first's directory while it builds: {}",
        marks(&first)
    ));
    let tag = held.lines().next().unwrap().to_owned();
    scratch.write(
        "images",
        &format!(
            "{} 100 {tag}\n{} 100 pumpkin/other:{}\n",
            image('e'),
            image('f'),
            "f".repeat(64)
        ),
    );
    scratch.write("bin/disk", "1000 150\n");
    let mut waiting = on_one_cpu(&scratch, &second);
    let mut stderr = BufReader::new(waiting.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting for a build slot") {
        assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
    }
    lines.push(format!(
        "the second's directory while it waits: {}",
        marks(&second)
    ));
    lines.push("the second's prune, while the first builds:".to_owned());
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker")),
    );
    scratch.write("bin/disk", "1000 500\n");
    stand_in.release("build");
    lines.push(format!("the first:\n{}", job_ended_whole(building)));
    let held = scratch.0.join(format!("run/pumpkin/{first}/held"));
    let held = fs::read_to_string(held).unwrap();
    lines.push(format!(
        "the first holds once built: {}",
        held.trim().replace('\n', " ")
    ));
    let waited = job_ended(waiting, stderr, said);
    lines.push(format!("the second:\n{waited}"));
    lines.push(format!(
        "the second's directory once it ended: {}",
        marks(&second)
    ));
    for id in [&first, &second] {
        fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    }
    fs::remove_file(scratch.0.join("tagged")).unwrap();
    scratch.write("bin/disk", "1000 100\n");
    let refused = on_one_cpu(&scratch, &third);
    lines.push(format!("the third:\n{}", job_ended_whole(refused)));
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker")),
    );
    // How long the second waited varies.
    let mut text = lines.join("\n");
    let at = text.find("after waiting ").unwrap() + "after waiting ".len();
    let seconds = text[at..].split(' ').next().unwrap().to_owned();
    text.replace_range(at..at + seconds.len(), "<n>");
    check(
        &scratch,
        &text,
        &[
            (first.as_str(), "<first>"),
            (second.as_str(), "<second>"),
            (third.as_str(), "<third>"),
            (tag.strip_prefix("pumpkin/built:").unwrap(), "<hex>"),
        ],
        &expect![[r#"
            the first holds: pumpkin/built:<hex>
            the first's directory while it builds: building, config.toml, gateway.key, held
            the second's directory while it waits: config.toml, gateway.key, held, waiting
            the second's prune, while the first builds:
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400
            the first:
            exit status: Some(0)
            pumpkin: building an image in pumpkin/built
            pumpkin: built an image in pumpkin/built
            printed the IR of the image built: true
            the first holds once built: pumpkin/built:<hex> sha256:2222222222222222222222222222222222222222222222222222222222222222
            the second:
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 1 image, least recently used first
            pumpkin: waiting for a build slot: all 1 held, by jobs <first>
            pumpkin: took a build slot after waiting <n> s
            printed the IR of the image built: true
            the second's directory once it ended: config.toml, gateway.key, held
            the third:
            exit status: Some(1)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            pumpkin: the volume is more than 80% used: the prune removed 1 image, least recently used first
            built/task.star:5:9: building the image failed: the controller's volume is 90% used, past the 85% at which a build is refused, though the prune has removed what no job holds: free space there, or let the jobs that run end, then run the task again
                5 | IMAGE = image("Dockerfile")
                  |         ^^^^^^^^^^^^^^^^^^^
            pumpkin: built/task.star: the task has 1 problem
            printed the IR of the image built: false
            docker image rm --force sha256:0000000000000000000000000000000000000000000000000000000000000000
            docker image rm --force sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the jobs' steps and the snapshot"
)]
fn job_publish_wait() {
    let scratch = Scratch::new();
    // The controller's engine is rootful, of which the job says nothing.
    let rootful = INFO.replace(r#","name=rootless""#, "");
    let _stand_in = StandIn::with(&scratch, &rootful, IMAGE);
    set_up(&scratch);
    write_parameter(
        &scratch,
        &format!("{}\n[providers.local.docker]\n", docker_table("default")),
    );
    scratch.write_sandboxd(&format!("home/.pumpkin/{PINNED}/bin/sandboxd"));
    scratch.write_task("built", &workflow(&image('0'), &["true"]));
    scratch.write("built/Dockerfile", &format!("FROM {BUSYBOX}\n{BUILDS}\n"));
    let [run, other] = ["0ad00011", "0ad00012"].map(|random| id_ago(0, random));
    for id in [&run, &other] {
        send(&scratch, id, UPSTREAMS, false);
    }
    // Each of the engine's 4 publish slots held, as by other runs that publish.
    let slots = scratch.0.join("home/.pumpkin/docker/publish-slots");
    fs::create_dir_all(&slots).unwrap();
    let held: Vec<File> = (0..4)
        .map(|index| {
            let slot = File::create(slots.join(index.to_string())).unwrap();
            slot.lock().unwrap();
            slot
        })
        .collect();
    let mut job = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &run,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "run",
            "built/task.star",
        ],
    );
    let mut publishing = (job.stdout(Stdio::piped()).stderr(Stdio::piped()))
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(publishing.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting for a publish slot") {
        assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
    }
    let mut lines = vec![format!("the run's job, as it waits:\n{}", said.trim_end())];
    let named = fs::read_to_string(scratch.0.join(format!("run/pumpkin/{run}/held"))).unwrap();
    lines.push(format!("it holds: {}", named.trim().replace('\n', " ")));
    let hex = named
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("pumpkin/built:");
    let hex = hex.to_owned();
    scratch.write(
        "images",
        &format!(
            "{} 100\n{} 100 pumpkin/other:{}\n",
            image('2'),
            image('f'),
            "f".repeat(64)
        ),
    );
    scratch.write("bin/disk", "1000 150\n");
    noted(&scratch);
    let mut pruning = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &other,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    let pruned = pruning.stdout(Stdio::null()).output().unwrap();
    lines.push(format!(
        "the other job's prune:\n{}",
        String::from_utf8(pruned.stderr).unwrap().trim_end()
    ));
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker")),
    );
    scratch.write("bin/disk", "1000 500\n");
    drop(held);
    let output = ended(publishing, stderr, String::new());
    let record = scratch.0.join(format!("home/.pumpkin/runs/{run}"));
    let ir = fs::read_to_string(record.join("ir.json")).unwrap();
    lines.extend([
        format!("the run's job: exit status {:?}", output.status.code()),
        format!("the run started: {}", record.join("log.jsonl").exists()),
        format!("its IR names the image: {}", ir.contains(&image('2'))),
    ]);
    check(
        &scratch,
        &lines.join("\n"),
        &[
            (run.as_str(), "<run>"),
            (other.as_str(), "<other>"),
            (&hex, "<hex>"),
        ],
        &expect![[r#"
            the run's job, as it waits:
            pumpkin: building an image in pumpkin/built
            pumpkin: built an image in pumpkin/built
            pumpkin: waiting for a publish slot: all 4 held, by 0, 1, 2, 3
            it holds: pumpkin/built:<hex> sha256:2222222222222222222222222222222222222222222222222222222222222222
            the other job's prune:
            pumpkin: the volume is more than 80% used: the prune removed 2 images, least recently used first
            docker image rm --force sha256:0000000000000000000000000000000000000000000000000000000000000000
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400
            the run's job: exit status Some(0)
            the run started: true
            its IR names the image: true"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the job's command line and the snapshot"
)]
fn job_publish_fails() {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let publish = "[publish]\nbucket = \"pumpkin-alice-public\"\ndistribution = \"E2STANDIN\"\n\
                   address = \"https://runs.example\"\nsince = 2026-11-01T00:00:00Z\n";
    write_parameter(
        &scratch,
        &format!(
            "{}\n[providers.local.docker]\n{publish}",
            docker_table("default")
        ),
    );
    scratch.write_sandboxd(&format!("home/.pumpkin/{PINNED}/bin/sandboxd"));
    scratch.write_task("built", &workflow(&image('0'), &["true"]));
    scratch.write("built/Dockerfile", &format!("FROM {BUSYBOX}\n{BUILDS}\n"));
    let run = id_ago(0, "0ad00021");
    send(&scratch, &run, UPSTREAMS, false);
    let mut job = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &run,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "run",
            "built/task.star",
        ],
    );
    let output = job.stdout(Stdio::null()).output().unwrap();
    let mut lines = vec![format!(
        "the run's job: exit status {:?}",
        output.status.code()
    )];
    // Once while the run is live, if it still is, and once as it has ended.
    let mut said: Vec<String> = String::from_utf8(output.stderr)
        .unwrap()
        .lines()
        .map(|line| match line.split_once("upload 1 of ") {
            Some((before, after)) => {
                let rest = after.split_once(' ').map_or("", |(_, rest)| rest);
                format!("{before}upload 1 of <n> {rest}")
            }
            None => line.to_owned(),
        })
        .collect();
    said.dedup();
    lines.extend(said);
    let mut noted = noted(&scratch);
    noted.dedup_by(|call, last| call.contains(" s3 ") && last.contains(" s3 "));
    lines.extend(
        noted
            .into_iter()
            .map(|call| match call.split_once("/public/") {
                Some((before, _)) if call.contains(" s3 ") => format!("{before}/public/…"),
                _ => call,
            }),
    );
    let record = scratch.0.join(format!("home/.pumpkin/runs/{run}"));
    let ended = record.join("outcome.json").exists();
    lines.push(format!("the run ended: {ended}"));
    check(
        &scratch,
        &lines.join("\n"),
        &[(run.as_str(), "<run>")],
        &expect![[r#"
        the run's job: exit status Some(0)
        pumpkin: building an image in pumpkin/built
        pumpkin: built an image in pumpkin/built
        pumpkin: run <run> started
        pumpkin: to stop it: pumpkin cancel <run>
        pumpkin: publishing the runs at https://runs.example every 60 s while the run is live
        pumpkin: publishing the runs failed: upload 1 of <n> to s3://pumpkin-alice-public: `aws s3 sync` failed with exit status: 1: the stand-in aws answers only `--version` and `ssm get-parameter`: fix what `aws` reports; the next publish uploads what this one didn't
        aws --version
        aws --no-cli-pager --output json --region us-west-1 ssm get-parameter --name /pumpkin-alice/settings
        curl --disable --silent --show-error --fail --max-time 10 --request PUT --header X-aws-ec2-metadata-token-ttl-seconds: 60 http://169.254.169.254/latest/api/token
        curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/local-ipv4
        curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/public-ipv4
        df --block-size=1 --output=size,avail -- /
        df --block-size=1 --output=size,avail -- /
        df --block-size=1 --output=size,avail -- /
        df --block-size=1 --output=size,avail -- /
        curl --disable --silent --show-error --fail --max-time 10 --request PUT --header X-aws-ec2-metadata-token-ttl-seconds: 60 http://169.254.169.254/latest/api/token
        curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/local-ipv4
        curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/public-ipv4
        aws --version
        aws --no-cli-pager --output json --region us-west-1 s3 sync <scratch>/home/.pumpkin/public/…
        the run ended: true"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the records and the snapshot"
)]
fn boot() {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let home = scratch.0.join("home");
    let (clean, local, lost, older, older_left) = (
        id_ago(2, "0b000007"),
        id_ago(1, "0b000005"),
        id_ago(1, "0b000008"),
        id_ago(2, "0b000009"),
        id_ago(1, "0b00000a"),
    );
    let runs = [
        (&clean, "local", Some(returned(1.0)), true),
        (&local, "local", None, false),
        (&lost, "aws", None, false),
        // On a provider that the boot can't reach, so that a reap of it would say so.
        (&older, "aws", Some(older_finished()), true),
        (&older_left, "local", None, false),
    ];
    for (run, provider, outcome, deleted) in runs {
        let config = match provider {
            "local" => docker_table("default") + LOCAL,
            _ => format!("{}\n{EC2}", docker_table("default")),
        };
        let path = home.join(format!("jobs/{run}/pumpkin.toml"));
        scratch.write(&format!("home/jobs/{run}/pumpkin.toml"), &config);
        let record = format!("home/.pumpkin/runs/{run}");
        let args = ["--config".to_owned(), path.display().to_string()];
        let writer = if run == &older_left {
            json!({ "version": "1.2.3" })
        } else {
            super::this_build()
        };
        let header =
            json!({ "record_version": "pumpkin.record/v1", "pumpkin": writer, "args": args });
        scratch.write(&format!("{record}/record.json"), &header);
        scratch.write(&format!("{record}/driver.pid"), "4242\n");
        let ir = json!({ "roles": [{ "name": "solver", "image": image('a') }] });
        scratch.write(&format!("{record}/ir.json"), &ir);
        let report = match provider {
            "local" => json!({ "context": "default" }),
            _ => json!({ "region": "us-west-1" }),
        };
        scratch.write(
            &format!("{record}/provider.json"),
            &json!({ "providers": { provider: { "report": report } } }),
        );
        let mut sandbox =
            json!({ "provider": provider, "id": format!("sandbox-{run}"), "report": {} });
        if deleted {
            sandbox["delete"] = json!({ "state": "deleted" });
        }
        if let Some(outcome) = outcome {
            scratch.write(&format!("{record}/outcome.json"), &outcome);
        }
        scratch.write(&format!("{record}/agents/solver/sandbox.json"), &sandbox);
    }
    let foreign = format!("home/.pumpkin/runs/{}/record.json", id_ago(1, "0e000001"));
    scratch.write(
        &foreign,
        &json!({ "record_version": "future", "pumpkin": { "version": "9.9.9" } }),
    );
    // The image of a job that a client started as the controller came up, which no IR names yet.
    scratch.write("images", &format!("{}\n{}\n", image('a'), image('b')));
    send(&scratch, &id_ago(0, "0b000006"), UPSTREAMS, false);
    let mut boot = on_controller(&scratch, &["controller", "boot"]);
    // As its unit has it: systemd's `/run/user/%U` names root's, the system manager's user's.
    boot.env("XDG_RUNTIME_DIR", scratch.0.join("root-run"));
    let invoked = invocation(&boot);
    let mut lines = vec![report(invoked, &boot.output().unwrap(), &scratch.0, false)];
    lines.extend(noted(&scratch));
    for run in [&clean, &local, &lost] {
        let outcome = format!("home/.pumpkin/runs/{run}/outcome.json");
        let outcome = fs::read_to_string(scratch.0.join(outcome)).unwrap();
        lines.push(format!("{run}'s outcome.json: {outcome}"));
    }
    let foreign_outcome = home.join(format!(".pumpkin/runs/{older_left}/outcome.json"));
    lines.push(format!(
        "{older_left}'s outcome.json exists: {}",
        foreign_outcome.exists()
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[
            (clean.as_str(), "<clean>"),
            (local.as_str(), "<local>"),
            (lost.as_str(), "<lost>"),
            (older.as_str(), "<older>"),
            (older_left.as_str(), "<older left>"),
        ],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/root-run pumpkin controller boot
            exit status: Some(0)
            pumpkin: run <local>'s sandbox sandbox-<local> on provider `local` had ended, as it isn't listed there now: what it left there is now removed
            pumpkin: run <local> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <lost>'s sandboxes on provider `aws`, if it left any, end by the run's hard end, as the boot can't reach it: `aws sts get-caller-identity` failed with exit status: 1: the stand-in aws answers only `--version` and `ssm get-parameter`: fix what `aws` reports
            pumpkin: run <lost> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            printed:
              <local>
              <lost>
            aws --version
            aws --no-cli-pager --output json --region us-west-1 sts get-caller-identity
            <clean>'s outcome.json: {"agents":[{"role":"solver","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","verdict":{"kind":"returned","result":{"values":[{"name":"score","value":1.0},{"name":"metrics","value":{"solved":3}}]}}}
            <local>'s outcome.json: {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }

            <lost>'s outcome.json: {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }

            <older left>'s outcome.json exists: false"#]],
    );
}

#[test]
fn boot_without_runs() {
    let scratch = Scratch::new();
    install(&scratch, &format!("home/.pumpkin/{PINNED}/bin"));
    let boot = on_controller(&scratch, &["controller", "boot"]);
    check(
        &scratch,
        &super::ran(boot, &scratch.0, false),
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller boot
            exit status: Some(0)"#]],
    );
}

#[test]
fn boot_registered_builds() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    write_record(&scratch, &id, true, None);
    move_runs(&scratch, "home/.pumpkin/runs");
    scratch.write_executable(
        "home/.pumpkin/legacy/bin/pumpkin",
        b"#!/bin/sh\ntouch \"$HOME/legacy-ran\"\n",
    );
    let broken = scratch.0.join("home/.pumpkin/0/bin");
    fs::create_dir_all(&broken).unwrap();
    symlink("/bin/false", broken.join("pumpkin-boot")).unwrap();
    // As the boot's unit runs it, from outside every install.
    let status = scratch
        .program(PUMPKIN, &["controller", "boot-all"])
        .status()
        .unwrap();
    let home = scratch.0.join("home");
    let log = fs::read_to_string(home.join("boot.log")).unwrap();
    let recovered = home
        .join(format!(".pumpkin/runs/{id}/outcome.json"))
        .is_file();
    let legacy = home.join("legacy-ran").exists();
    check(
        &scratch,
        &format!("{status}\n{log}recovered: {recovered}\nlegacy ran: {legacy}"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 1
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            <id>
            controller boot failed for <scratch>/home/.pumpkin/0/bin/pumpkin-boot (exit 1)
            recovered: true
            legacy ran: false"#]],
    );
}

const PINNED: &str = "1fe98e9dcf36c2dafe0df654e3899facc7dfa6f8";

const INSTANCE: &str = "i-0c00000000000a11c";

const SHARED: &str = "repo/.pumpkin/controller/alice.us-west-1";

const KEY: &str = "sk-client-stand-in";

// Runs the controller command in the test's home, with cuts before the requested byte on either
// stream. Each before-N action runs just before the Nth connection. A client home with a
// `.member` is a key that the admin lists under that member, so the command runs as the
// controller's forced command runs it.
const SSH: &str = r#"#!/bin/sh
dir=${0%/*}
printf '%s\n' "$@" >> "$dir/argv"
while :; do
    case $1 in
        -o|-F|-i|-E) shift 2 ;;
        *) break ;;
    esac
done
shift
command=$*
if [ -f "$HOME/.member" ]; then set -- pumpkin --member "$(cat "$HOME/.member")"; else set --; fi
first=$(printf '%s\n' "$command" | head --lines=1)
printf '%s\n' "$first" >> "$dir/scripts"
calls=$(wc --lines < "$dir/scripts")
if [ -f "$dir/before-$calls" ]; then sh "$dir/before-$calls"; rm "$dir/before-$calls"; fi
cd "$dir/../home" || exit 1
export HOME="$PWD" XDG_RUNTIME_DIR="$dir/../run"
if mv "$dir/cut-in" "$dir/cut-in-$$" 2>/dev/null; then
    stdbuf --output=0 head --bytes="$(cat "$dir/cut-in-$$")" | /bin/sh -c "$command" "$@"
    exit 1
fi
if mv "$dir/answer" "$dir/answered" 2>/dev/null; then exec cat "$dir/answered"; fi
if mv "$dir/cut-out" "$dir/cut-out-$$" 2>/dev/null; then
    exec 3<&0; mkfifo "$dir/fifo-$$"
    /bin/sh -c "$command" "$@" <&3 > "$dir/fifo-$$" &
    dd bs=1 count="$(cat "$dir/cut-out-$$")" status=none < "$dir/fifo-$$"
    exit 1
fi
if [ -f "$dir/link-pid" ]; then echo $$ > "$dir/link-pid"; fi
exec /bin/sh -c "$command" "$@"
"#;

const SYSTEMCTL: &str = "#!/bin/sh\n";

const SYSTEMD_RUN: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${0%/*}/argv\"\n";

fn set_up_client(scratch: &Scratch) -> [(String, String); 2] {
    if !scratch.0.join("bin/aws").exists() {
        scratch.write_executable(
            "bin/aws",
            b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\nexit 1\n",
        );
    }
    for (name, script) in [
        ("ssh", SSH),
        ("systemctl", SYSTEMCTL),
        ("systemd-run", SYSTEMD_RUN),
    ] {
        scratch.write_executable(&format!("bin/{name}"), script.as_bytes());
    }
    for dir in ["home", "run"] {
        fs::create_dir_all(scratch.0.join(dir)).unwrap();
    }
    install(scratch, &format!("repo/.pumpkin/{PINNED}/bin"));
    let config = format!(
        "[controller]\nname = \"alice\"\nregion = \"us-west-1\"\n\n{}",
        UPSTREAMS.replace("\"gateway.key\"", "\"gateway-key\"")
    );
    scratch.write("client/.config/pumpkin/pumpkin.toml", &config);
    scratch.write("client/.config/pumpkin/gateway-key", KEY);
    scratch.write("client/.member", "alice\n");
    // Not git's own variables, such as the `GIT_DIR` that `git rebase --exec` exports, which
    // would point `git init` at another repository.
    let git = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]))
        .args(["init", "--quiet", "repo"])
        .current_dir(&scratch.0)
        .status();
    assert!(git.unwrap().success());
    scratch.write_task_in("repo", "stand-in", &workflow(&image('0'), &["true"]));
    scratch.write("repo/.pumpkin/.gitignore", "*\n");
    scratch.write("repo/other/data", "another task's\n");
    let path = format!("{}:/usr/bin:/bin", scratch.0.join("bin").display());
    [
        (
            "HOME".to_owned(),
            scratch.0.join("client").display().to_string(),
        ),
        ("PATH".to_owned(), path),
    ]
}

fn on_client(scratch: &Scratch, environment: &[(String, String); 2], args: &[&str]) -> Command {
    on_repo(scratch, "repo", environment, args)
}

// The client's command in its repository `repo`, below the scratch directory.
fn on_repo(
    scratch: &Scratch,
    repo: &str,
    environment: &[(String, String); 2],
    args: &[&str],
) -> Command {
    let pumpkin = scratch
        .0
        .join(format!("{repo}/.pumpkin/{PINNED}/bin/pumpkin"));
    let mut command = scratch.program(pumpkin, args);
    command
        .current_dir(scratch.0.join(repo))
        .envs(environment.iter().map(|(name, value)| (name, value)));
    command
}

// A teammate of the client's, `name`, whose key the admin lists under that name: a home of
// their own, `client-<name>`, with the client's configuration, and a copy of the client's
// repository, `repo-<name>`, that has found the same controller.
fn set_up_member(scratch: &Scratch, name: &str) -> [(String, String); 2] {
    let home = format!("client-{name}");
    let config = fs::read_to_string(scratch.0.join("client/.config/pumpkin/pumpkin.toml"));
    scratch.write(
        &format!("{home}/.config/pumpkin/pumpkin.toml"),
        &config.unwrap(),
    );
    scratch.write(&format!("{home}/.config/pumpkin/gateway-key"), KEY);
    scratch.write(&format!("{home}/.member"), &format!("{name}\n"));
    let repo = format!("repo-{name}");
    fs::create_dir_all(scratch.0.join(&repo)).unwrap();
    for entry in fs::read_dir(scratch.0.join("repo")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".pumpkin" {
            continue;
        }
        let copied = stdx::process::command("cp", Environment::Only(&["PATH"]))
            .args(["--recursive", "--"])
            .arg(entry.path())
            .arg(scratch.0.join(&repo))
            .status();
        assert!(copied.unwrap().success());
    }
    install(scratch, &format!("{repo}/.pumpkin/{PINNED}/bin"));
    scratch.write(&format!("{repo}/.pumpkin/.gitignore"), "*\n");
    found_at(
        &scratch
            .0
            .join(format!("{repo}/.pumpkin/controller/alice.us-west-1")),
    );
    [
        (
            "HOME".to_owned(),
            scratch.0.join(home).display().to_string(),
        ),
        (
            "PATH".to_owned(),
            format!("{}:/usr/bin:/bin", scratch.0.join("bin").display()),
        ),
    ]
}

fn modes(dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(dir) else {
        return "gone".to_owned();
    };
    let mut files: Vec<String> = entries
        .map(|entry| {
            let entry = entry.unwrap();
            let mode = entry.metadata().unwrap().permissions().mode() & 0o7777;
            format!("{} {mode:o}", entry.file_name().display())
        })
        .collect();
    files.sort();
    files.join(", ")
}

fn mode_and_modes(dir: &Path) -> String {
    let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
    format!("{mode:o}: {}", modes(dir))
}

fn started_job(scratch: &Scratch) -> ([(String, String); 2], String, String) {
    let client = set_up_client(scratch);
    let (id, ran) = start(scratch, &client);
    (client, id, ran)
}

// As a previous command that found the controller left the address, with no AWS call on the
// next warm connection.
fn connect(scratch: &Scratch, client: &[(String, String); 2]) {
    let dir = scratch.0.join(SHARED);
    if dir.join("found").exists() {
        return;
    }
    let first = on_client(scratch, client, &["ls"]).output().unwrap();
    assert!(!first.status.success(), "{first:?}");
    fs::remove_file(dir.join("failed")).unwrap();
    found_at(&dir);
}

// The location of the controller, in the workspace's directory `dir` of it.
fn found_at(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let found = json!({ "instance": INSTANCE, "address": "255.255.255.255" });
    let file = dir.join("found");
    fs::write(&file, found.to_string()).unwrap();
    fs::set_permissions(file, Permissions::from_mode(0o600)).unwrap();
}

fn start(scratch: &Scratch, client: &[(String, String); 2]) -> (String, String) {
    connect(scratch, client);
    start_in(scratch, "repo", client)
}

// As `start`, from the repository `repo`, whose workspace has found the controller.
fn start_in(scratch: &Scratch, repo: &str, client: &[(String, String); 2]) -> (String, String) {
    if !scratch.0.join("home/.pumpkin/serve").is_dir() {
        served(scratch);
    }
    let mut run = on_repo(scratch, repo, client, &["run", "stand-in/task.star"]);
    let invoked = invocation(&run);
    let ran = run.output().unwrap();
    let said = String::from_utf8(ran.stderr.clone()).unwrap();
    let id = said.lines().find_map(|line| {
        let id = line
            .strip_prefix("pumpkin: run ")?
            .split_once(" goes to controller")?
            .0;
        Some(id.to_owned())
    });
    let id = id.unwrap_or_default();
    let mut ran = report(invoked, &ran, &scratch.0, false);
    if !id.is_empty() {
        ran = ran.replace(&id, "<id>");
    }
    (id, ran)
}

#[test]
fn keys_on_standard_input() {
    let scratch = Scratch::new();
    let (_, id, ran) = started_job(&scratch);
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let sent = fs::read_to_string(scratch.0.join(format!("run/pumpkin/{id}/gateway.key")));
    let held = stdx::process::command("grep", Environment::Only(&["PATH"]))
        .args(["--recursive", "--files-with-matches", "--fixed-strings"])
        .arg(format!("--exclude-dir={PINNED}"))
        .args(["--", KEY])
        .arg(scratch.0.join("repo/.pumpkin"))
        .output()
        .unwrap();
    let held = String::from_utf8(held.stdout).unwrap();
    let lines = [
        ran,
        format!("an argument holds the key: {}", argv.contains(KEY)),
        format!(
            "ssh ran on the controller: {}",
            argv.contains("controller serve")
        ),
        format!(
            "the job's directory holds the key: {}",
            sent.unwrap() == KEY
        ),
        format!(
            "files in the workspace that hold the key: {}",
            if held.is_empty() { "none" } else { &held }
        ),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 6 of the task's files: sending 2.9 KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            an argument holds the key: false
            ssh ran on the controller: true
            the job's directory holds the key: true
            files in the workspace that hold the key: none"#]],
    );
}

#[test]
fn start_with_large_arguments_and_model_catalog() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let config = scratch.0.join("client/.config/pumpkin/pumpkin.toml");
    let models = (0..1500)
        .map(|n| format!(r#"{{ name = "model-{n}", upstream_name = "provider/model-{n}" }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let contents = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        contents.replace("models = [", &format!("models = [{models}, ")),
    )
    .unwrap();
    let key = b"\0binary key\xff\n";
    fs::write(scratch.0.join("client/.config/pumpkin/gateway-key"), key).unwrap();
    let task = scratch.0.join("repo/stand-in/task.star");
    let contents = fs::read_to_string(&task).unwrap();
    fs::write(
        &task,
        contents.replace("workflow(", "TEXT = arg(\"text\", str)\nworkflow("),
    )
    .unwrap();
    connect(&scratch, &client);
    served(&scratch);
    let word = format!("text={}", "x".repeat(schema::frame::MAX_MESSAGE_BYTES));
    let output = on_client(
        &scratch,
        &client,
        &["run", "stand-in/task.star", "--arg", &word],
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8(output.stdout).unwrap();
    let sent = scratch.0.join("run/pumpkin").join(id.trim());
    let config: toml::Table = fs::read_to_string(sent.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        config["upstreams"]["gateway"]["models"]
            .as_array()
            .unwrap()
            .len(),
        1501
    );
    assert_eq!(fs::read(sent.join("gateway.key")).unwrap(), key);
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    assert!(argv.lines().any(|line| line == word));
}

fn start_head(body_bytes: usize, credential_bytes: usize) -> serde_json::Value {
    json!({ "type": "start", "body_bytes": body_bytes, "credential_bytes": credential_bytes })
}

fn start_job(id: &str, modal_bytes: usize) -> serde_json::Value {
    json!({
        "id": id, "kind": "build", "repo": "tasks",
        "tree": format!("sha256:{EMPTY_TREE}"), "install": format!("sha256:{EMPTY_TREE}"),
        "words": ["--", "task.star"],
        "region": "us-west-1", "parameter": "/pumpkin-alice/settings",
        "sent": {}, "key_bytes": [], "modal_bytes": modal_bytes,
    })
}

fn ask_start(scratch: &Scratch, key: &str, path: &str, id: &str, body: &[u8]) -> serde_json::Value {
    let mut request = Vec::new();
    let install = install_manifest(&client_bin(scratch));
    let lacks = json!({"type": "lacks", "manifest": format!("sha256:{key}"), "install": true, "body_bytes": install.len()});
    schema::frame::write(&mut request, &lacks, &[]).unwrap();
    schema::frame::write(&mut request, &(), install.as_bytes()).unwrap();
    let credential_bytes = body.len();
    let mut start = start_job(id, credential_bytes);
    start["install"] = json!(format!("sha256:{key}"));
    let mut body = body.to_vec();
    serde_json::to_writer(&mut body, &start).unwrap();
    schema::frame::write(&mut request, &start_head(body.len(), credential_bytes), &[]).unwrap();
    for part in body.chunks(schema::frame::MAX_BODY_BYTES) {
        schema::frame::write(&mut request, &(), part).unwrap();
    }
    let serve = scratch
        .0
        .join("home")
        .join(format!(".pumpkin/serve/{key}/pumpkin"));
    let mut process = scratch.program(serve, &["controller", "serve"]);
    process
        .env("PATH", path)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("XDG_RUNTIME_DIR", scratch.0.join("run"));
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), &request).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut frames = &output.stdout[..];
    let _hello = schema::frame::read::<serde_json::Value>(&mut frames)
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        schema::frame::read::<serde_json::Value>(&mut frames)
            .unwrap()
            .unwrap();
    }
    let (answer, _) = schema::frame::read::<serde_json::Value>(&mut frames)
        .unwrap()
        .unwrap();
    answer
}

#[test]
fn retry_start() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let key = served(&scratch);
    let home = scratch.0.join("home");
    let id = id_ago(0, "0123abcd");
    let ask = |body: &[u8]| ask_start(&scratch, &key, &client[1].1, &id, body);
    let absent = ask(b"no tree yet");
    assert_eq!(absent["type"], "refused");
    assert!(absent["message"].as_str().unwrap().contains(EMPTY_TREE));
    scratch.write(
        &format!("home/store/manifests/{EMPTY_TREE}"),
        "pumpkin-tree/1\n",
    );
    let tmpfs = scratch.0.join(format!("run/pumpkin/{id}"));
    let path = scratch.0.join("without-systemd");
    fs::create_dir(&path).unwrap();
    for tool in ["sh", "find", "awk"] {
        symlink(Path::new("/usr/bin").join(tool), path.join(tool)).unwrap();
    }
    let failed = ask_start(&scratch, &key, path.to_str().unwrap(), &id, b"failed");
    assert_eq!(failed["type"], "refused");
    assert!(
        failed["message"]
            .as_str()
            .unwrap()
            .contains("systemd-run could not start")
    );
    for path in [
        &tmpfs,
        &home.join("builds").join(&id),
        &home.join("trees/tasks").join(&id),
    ] {
        assert!(!path.exists(), "{}", path.display());
    }
    let modal = vec![b'm'; schema::frame::MAX_BODY_BYTES + 1];
    assert_eq!(ask(&modal)["type"], "started");
    assert_eq!(ask(b"repeated")["type"], "already_started");
    assert_eq!(fs::read(tmpfs.join("modal.toml")).unwrap(), modal);
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    assert_eq!(
        argv.lines()
            .filter(|line| line.starts_with("--unit="))
            .count(),
        1
    );
    let bin = home.join(format!(".pumpkin/{key}/bin"));
    assert!(
        argv.lines()
            .any(|line| Path::new(line) == bin.join("pumpkin"))
    );
    fs::remove_dir_all(home.join(format!(".pumpkin/serve/{key}"))).unwrap();
    assert!(BUILT.iter().all(|name| bin.join(name).is_file()));
}

#[test]
fn interrupted_launch_keeps_the_job() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let key = served(&scratch);
    scratch.write(
        &format!("home/store/manifests/{EMPTY_TREE}"),
        "pumpkin-tree/1\n",
    );
    scratch.write_executable("bin/systemd-run", b"#!/bin/sh\nkill -TERM $$\n");
    let id = id_ago(0, "0777abcd");
    let ask = |body: &[u8]| ask_start(&scratch, &key, &client[1].1, &id, body);
    let unconfirmed = ask(b"first");
    assert_eq!(unconfirmed["type"], "unconfirmed");
    assert!(
        unconfirmed["message"]
            .as_str()
            .unwrap()
            .contains("systemctl --user status")
    );
    scratch.write_executable("bin/systemd-run", SYSTEMD_RUN.as_bytes());
    assert_eq!(ask(b"second")["type"], "unconfirmed");
    assert_eq!(
        fs::read(scratch.0.join(format!("run/pumpkin/{id}/modal.toml"))).unwrap(),
        b"first"
    );
    // A reboot loses the tmpfs even when the server died before submitting the unit.
    fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    assert_eq!(ask(b"third")["type"], "unconfirmed");
    // The link that controller job writes survives the unit, including an older install's job.
    symlink(
        scratch.0.join(format!("home/.pumpkin/{key}/bin/pumpkin")),
        scratch.0.join(format!("home/builds/{id}/pumpkin")),
    )
    .unwrap();
    assert_eq!(ask(b"fourth")["type"], "already_started");
    assert!(!scratch.0.join("bin/argv").exists());
}

#[test]
fn start_after_a_cut() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let cut = scratch.0.join("bin/cut-out");
    let (_, first) = start(&scratch, &client);
    assert!(first.contains("exit status: Some(0)"), "{first}");
    let mut replies = Vec::new();
    schema::frame::write(&mut replies, &json!({ "type": "serving" }), &[]).unwrap();
    for _ in 0..2 {
        schema::frame::write(
            &mut replies,
            &json!({"type":"missing", "body_bytes":2}),
            &[],
        )
        .unwrap();
        schema::frame::write(&mut replies, &(), b"[]").unwrap();
    }
    fs::write(cut, (replies.len() + 1).to_string()).unwrap();
    let (id, ran) = start(&scratch, &client);
    assert!(ran.contains("exit status: Some(0)"), "{ran}");
    assert!(ran.contains("starting the job again"), "{ran}");
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    assert_eq!(
        argv.lines()
            .filter(|line| *line == format!("--unit=pumpkin-job-{id}"))
            .count(),
        1
    );
    let sent = scratch.0.join(format!("run/pumpkin/{id}/gateway.key"));
    assert_eq!(fs::read_to_string(sent).unwrap(), KEY);
}

// No file of the client's that another user may read, but a public key.
#[test]
fn modes_of_what_the_client_keeps() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    // The test's own, which `connect` wrote.
    fs::remove_file(scratch.0.join(SHARED).join("found")).unwrap();
    let lines = [
        format!(
            "this machine's key: {}",
            mode_and_modes(&scratch.0.join("client/.config/pumpkin/ssh"))
        ),
        format!(
            "the saved controller location: {}",
            mode_and_modes(&scratch.0.join(SHARED))
        ),
        format!(
            "the jobs' directories in tmpfs: {}",
            mode_and_modes(&scratch.0.join("run/pumpkin")).replace(&id, "<id>")
        ),
        format!(
            "the job's directory in tmpfs: {}",
            mode_and_modes(&scratch.0.join(format!("run/pumpkin/{id}")))
        ),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            this machine's key: 700: key 600, key.pub 644
            the saved controller location: 700: lock 600
            the jobs' directories in tmpfs: 700: <id> 700
            the job's directory in tmpfs: 700: config.toml 600, gateway.key 600"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn target_refusals() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let mut lines = Vec::new();
    let config = scratch.0.join("client/.config/pumpkin/pumpkin.toml");
    let both = format!(
        "{}\n[providers.local.docker]\n",
        fs::read_to_string(&config).unwrap()
    );
    scratch.write("both.toml", &both);
    // A class that Modal's settings refuse, so that it is refused before anything is sent.
    let misnamed = SENT_MODAL.replace("[modal]", "[controller.modal]");
    let misnamed = format!(
        "{}\n{}",
        fs::read_to_string(&config).unwrap(),
        misnamed.replace("cpu_millis", "cpus_millis")
    );
    scratch.write("misnamed.toml", &misnamed);
    for named in ["../both.toml", "../misnamed.toml"] {
        let mut refused = on_client(
            &scratch,
            &client,
            &["run", "stand-in/task.star", "--config", named],
        );
        let invoked = invocation(&refused);
        lines.push(report(
            invoked,
            &refused.output().unwrap(),
            &scratch.0,
            false,
        ));
    }
    let mut runs = on_client(
        &scratch,
        &client,
        &["run", "stand-in/task.star", "--runs", "runs"],
    );
    let invoked = invocation(&runs);
    lines.push(report(invoked, &runs.output().unwrap(), &scratch.0, false));
    // The task's root above the repository that holds it, which alone is sent.
    let (marker, above) = (
        scratch.0.join("repo/PUMPKIN.star"),
        scratch.0.join("PUMPKIN.star"),
    );
    fs::rename(&marker, &above).unwrap();
    // The files the task loads where its labels now find them, so that the root is what is
    // refused.
    for loaded in [
        "lib/train.star",
        "stand-in/submit.star",
        "stand-in/final.star",
    ] {
        let text = fs::read_to_string(scratch.0.join("repo").join(loaded)).unwrap();
        scratch.write(loaded, &text);
    }
    let mut outside = on_client(&scratch, &client, &["run", "stand-in/task.star"]);
    let invoked = invocation(&outside);
    lines.push(report(
        invoked,
        &outside.output().unwrap(),
        &scratch.0,
        false,
    ));
    fs::rename(&above, &marker).unwrap();
    let calls = fs::read_to_string(scratch.0.join("bin/calls")).unwrap_or_default();
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap_or_default();
    lines.push(format!(
        "calls of aws and ssh: {}",
        calls.lines().count() + argv.lines().count()
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../both.toml
            exit status: Some(1)
            pumpkin: ../both.toml: it names `[controller]` beside `[providers]`: a machine's runs go either to its controller or to its own providers, so keep `[controller]`, beside `[upstreams]`, or remove it
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../misnamed.toml
            exit status: Some(1)
            pumpkin: ../misnamed.toml: TOML parse error at line 12, column 78
               |
            12 | classes = { a100 = { gpu = "A100-80GB", gpu_model = "NVIDIA A100-SXM4-80GB", cpus_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 } }
               |                                                                              ^^^^^^^^^^^
            unknown field `cpus_millis`, expected one of `gpu`, `gpu_model`, `cpu_millis`, `memory_bytes`, `startup_seconds`, `usd_per_hour`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --runs runs
            exit status: Some(1)
            pumpkin: `--runs` names this machine's, but its configuration sends runs to controller `alice`: leave it out
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: stand-in/task.star: the task's root, <scratch>, is above its git repository, <scratch>/repo, which is all that a run on the controller is sent: put the root's `PUMPKIN.star` in the repository
            calls of aws and ssh: 0"#]],
    );
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = <[u8; 32]>::from(sha2::Sha256::digest(bytes));
    Sha256Digest::from(digest).hex().to_owned()
}

fn walked(dir: &Path, skipped: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(below) = dirs.pop() {
        for entry in fs::read_dir(dir.join(&below)).unwrap() {
            let entry = entry.unwrap();
            let path = below.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                if !skipped.iter().any(|name| entry.file_name() == *name) {
                    dirs.push(path);
                }
            } else {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}

fn holders(scratch: &Scratch, named: &[(String, &str)]) -> Vec<(String, String)> {
    let repo = scratch.0.join("repo");
    let mut holders: Vec<(String, String)> = (named.iter())
        .map(|(hex, name)| (hex.clone(), (*name).to_owned()))
        .collect();
    holders.push((EMPTY_TREE.to_owned(), "a tree of no files".to_owned()));
    let bin = client_bin(scratch);
    holders.push((key(&bin), "the install".to_owned()));
    for name in BUILT {
        holders.push((
            sha256(&fs::read(bin.join(name)).unwrap()),
            format!("the installed {name}"),
        ));
    }
    for path in walked(&repo, &[".git", ".pumpkin"]) {
        let at = repo.join(&path);
        let bytes = match fs::read_link(&at) {
            Ok(target) => target.into_os_string().into_vec(),
            Err(_) => fs::read(&at).unwrap(),
        };
        let hex = sha256(&bytes);
        if !named.iter().any(|(named, _)| *named == hex) {
            holders.push((hex, path.display().to_string()));
        }
    }
    holders
}

fn stored(scratch: &Scratch, holders: &[(String, String)]) -> Vec<String> {
    let store = scratch.0.join("home/store");
    let named = |hex: &str| {
        let names: Vec<&str> = (holders.iter())
            .filter(|(held, _)| held == hex)
            .map(|(_, name)| name.as_str())
            .collect();
        if names.is_empty() {
            format!("unnamed {hex}")
        } else {
            names.join(", ")
        }
    };
    let mut lines = Vec::new();
    for path in walked(&store, &["manifests", "incoming"]) {
        let name = path.to_str().unwrap();
        if name == "lock" {
            continue;
        }
        let (hex, copy) = match name.strip_suffix(".x") {
            Some(hex) => (hex, ", to run"),
            None => (name, ""),
        };
        assert_eq!(sha256(&fs::read(store.join(&path)).unwrap()), hex, "{name}");
        let metadata = fs::metadata(store.join(&path)).unwrap();
        let mode = metadata.permissions().mode() & 0o7777;
        let linked = match metadata.nlink() {
            1 => "1 link".to_owned(),
            count => format!("{count} links"),
        };
        lines.push(format!("  {}{copy}: {mode:o}, {linked}", named(hex)));
    }
    lines.sort();
    lines.insert(0, "the store:".to_owned());
    let mut manifests = Vec::new();
    for path in walked(&store.join("manifests"), &[]) {
        let name = path.to_str().unwrap().to_owned();
        let text = fs::read(store.join("manifests").join(&path)).unwrap();
        assert_eq!(sha256(&text), name);
        manifests.push(format!("  a manifest: {}", named(&name)));
    }
    manifests.sort();
    lines.extend(manifests);
    let incoming = walked(&store.join("incoming"), &[]);
    if incoming.is_empty() {
        lines.push("  incoming/: none".to_owned());
    }
    for path in incoming {
        let name = path.to_str().unwrap();
        let size_bytes = fs::metadata(store.join("incoming").join(&path))
            .unwrap()
            .len();
        let hex = name.strip_suffix(".part").unwrap();
        lines.push(format!(
            "  incoming/: partial {}, {size_bytes} bytes",
            named(hex)
        ));
    }
    lines
}

fn calls(scratch: &Scratch) -> String {
    let path = scratch.0.join("bin/scripts");
    let text = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(path);
    let calls: Vec<_> = text.lines().collect();
    if calls.is_empty() {
        return "the calls: none".to_owned();
    }
    format!("the calls: {}", calls.join(", "))
}

fn send_calls(scratch: &Scratch) -> String {
    let key = key(&client_bin(scratch));
    calls(scratch).replace(&format!("serve=.pumpkin/serve/{key}/pumpkin"), "serve")
}

fn last_tree(scratch: &Scratch) -> String {
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    let at = lines.iter().rposition(|line| *line == "--tree").unwrap();
    lines[at + 1].to_owned()
}

fn job_of(scratch: &Scratch, id: &str, tree: &str) -> std::process::Output {
    let mut job = on_controller(
        scratch,
        &[
            "controller",
            "job",
            id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            tree,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    job.current_dir(scratch.0.join(format!("home/trees/repo/{id}")));
    job.output().unwrap()
}

fn write_files(scratch: &Scratch) {
    scratch.write_executable("repo/stand-in/run.sh", b"#!/bin/sh\necho run\n");
    symlink("Dockerfile", scratch.0.join("repo/stand-in/CLAUDE.md")).unwrap();
    scratch.write("repo/stand-in/with space.txt", "spaced\n");
    scratch.write("repo/stand-in/empty", "");
    scratch.write("repo/stand-in/gone", "");
    let added = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]))
        .args(["-C", "repo", "add", "stand-in/gone"])
        .current_dir(&scratch.0)
        .status();
    assert!(added.unwrap().success());
    fs::remove_file(scratch.0.join("repo/stand-in/gone")).unwrap();
}

fn tree_files(scratch: &Scratch, dir: &Path) -> Vec<String> {
    let store = scratch.0.join("home/store");
    let mut lines = vec!["the job's tree:".to_owned()];
    for path in walked(dir, &[".pumpkin"]) {
        let at = dir.join(&path);
        let metadata = fs::symlink_metadata(&at).unwrap();
        let line = if metadata.is_symlink() {
            format!(
                "a symbolic link to {}",
                fs::read_link(&at).unwrap().display()
            )
        } else {
            let hex = sha256(&fs::read(&at).unwrap());
            let linked = |name: String| {
                let blob = fs::metadata(store.join(name));
                blob.is_ok_and(|blob| blob.ino() == metadata.ino())
            };
            let of = if linked(hex.clone()) {
                "its blob"
            } else if linked(format!("{hex}.x")) {
                "its blob's copy to run"
            } else {
                "nothing of the store's"
            };
            let mode = metadata.permissions().mode() & 0o7777;
            format!("a link of {of}, {mode:o}")
        };
        lines.push(format!("  {}: {line}", path.display()));
    }
    lines
}

// The job's build refuses the link in its context, once it has made the tree.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn send_by_content() {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let client = set_up_client(&scratch);
    write_files(&scratch);
    let (id, first) = start(&scratch, &client);
    let mut lines = vec![first, send_calls(&scratch)];
    let tree = [(last_tree(&scratch), "the task's tree")];
    lines.extend(stored(&scratch, &holders(&scratch, &tree)));
    job_of(&scratch, &id, &last_tree(&scratch));
    let dir = scratch.0.join(format!("home/trees/repo/{id}"));
    lines.extend(tree_files(&scratch, &dir));
    let (_, again) = start(&scratch, &client);
    lines.extend([again, send_calls(&scratch)]);
    scratch.write("repo/stand-in/with space.txt", "spaced again\n");
    let (_, changed) = start(&scratch, &client);
    lines.extend([changed, send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 10 of the task's files: sending 3.0 KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve
            the store:
              PUMPKIN.star: 444, 1 link
              lib/train.star: 444, 1 link
              stand-in/CLAUDE.md: 444, 1 link
              stand-in/Dockerfile: 444, 1 link
              stand-in/empty: 444, 1 link
              stand-in/final.star: 444, 1 link
              stand-in/run.sh: 444, 1 link
              stand-in/submit.star: 444, 1 link
              stand-in/task.star: 444, 1 link
              stand-in/with space.txt: 444, 1 link
              the installed buildifier, to run: 555, 2 links
              the installed pumpkin, to run: 555, 2 links
              a manifest: a tree of no files
              a manifest: the install
              a manifest: the task's tree
              incoming/: none
            the job's tree:
              .pumpkin: a symbolic link to <scratch>/home/.pumpkin
              PUMPKIN.star: a link of its blob, 444
              lib/train.star: a link of its blob, 444
              stand-in/CLAUDE.md: a symbolic link to Dockerfile
              stand-in/Dockerfile: a link of its blob, 444
              stand-in/empty: a link of its blob, 444
              stand-in/final.star: a link of its blob, 444
              stand-in/run.sh: a link of its blob's copy to run, 555
              stand-in/submit.star: a link of its blob, 444
              stand-in/task.star: a link of its blob, 444
              stand-in/with space.txt: a link of its blob, 444
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` holds all 10 of the task's files
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks 1 of the task's 10 files: sending 13 bytes
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve"#]],
    );
}

#[test]
fn send_refused_partial() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    // A full-length partial with the wrong digest must be discarded.
    let hex = sha256(b"noted\n");
    scratch.write(&format!("home/store/incoming/{hex}.part"), "Noted\n");
    let (_, refused) = start(&scratch, &client);
    let mut lines = vec![refused, send_calls(&scratch)];
    let store = scratch.0.join("home/store");
    lines.push(format!(
        "the store holds it: {}; its partial: {}",
        store.join(&hex).exists(),
        store.join(format!("incoming/{hex}.part")).exists()
    ));
    let (_, again) = start(&scratch, &client);
    lines.extend([again, send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 2.9 KiB
            pumpkin: <scratch>/repo/stand-in/notes changed since it was hashed: send the task again
            the calls: serve
            the store holds it: false; its partial: false
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 2.9 KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve"#]],
    );
}

#[test]
fn send_carries_on() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    fs::write(scratch.0.join("repo/stand-in/big.bin"), vec![42; 3 << 20]).unwrap();
    // End the link in the second frame of the large file, after the first MiB is durable.
    scratch.write("bin/cut-in", &(1536 << 10).to_string());
    let (_, sent) = start(&scratch, &client);
    let holders = holders(&scratch, &[]);
    let mut lines = vec![sent, send_calls(&scratch)];
    lines.extend(
        stored(&scratch, &holders)
            .into_iter()
            .filter(|line| line.contains("big.bin") || line.contains("incoming")),
    );
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 3.0 MiB
            pumpkin: controller `alice` didn't answer: ended during the upload: starting the job again now, attempt 2 of 7
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 2.0 MiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve, serve
              stand-in/big.bin: 444, 1 link
              incoming/: none"#]],
    );
}

#[test]
fn cut_upload_finishes_on_the_original_connection() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    for mib in [224, 32] {
        let path = scratch.0.join(format!("repo/stand-in/{mib}.bin"));
        fs::File::create(path).unwrap().set_len(mib << 20).unwrap();
    }
    scratch.write(
        "bin/before-2",
        &format!("echo {} > '{}/bin/cut-in'", 1536 << 10, scratch.0.display()),
    );
    let (_, sent) = start(&scratch, &client);
    check(
        &scratch,
        &format!("{sent}\n{}", send_calls(&scratch)),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 8 of the task's files: sending 256.0 MiB
            pumpkin: controller `alice` didn't answer: ended during the upload: continuing the upload on the open connection
            pumpkin: controller `alice` lacks 1 of the task's 8 files: sending 31.0 MiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve, serve"#]],
    );
}

#[test]
fn cut_during_group_answers() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let (_, warm) = start(&scratch, &client);
    assert!(warm.contains("exit status: Some(0)"), "{warm}");
    send_calls(&scratch);
    let mut blobs = Vec::new();
    for index in 0..9 {
        let contents = format!("small file {index}");
        scratch.write(&format!("repo/stand-in/new-{index}"), &contents);
        blobs.push(json!({
            "digest": format!("sha256:{}", sha256(contents.as_bytes())),
            "size_bytes": contents.len(),
            "held_bytes": 0,
        }));
    }
    let missing = |body: &[u8]| {
        frame_bytes(&json!({"type":"missing","body_bytes":body.len()}), &[])
            + frame_bytes(&serde_json::Value::Null, body)
    };
    let install = frame_bytes(&json!({"type":"serving"}), &[]) + missing(b"[]");
    // Cut inside the tree query, then inside the second Stored on the next connection.
    let tree = missing(&serde_json::to_vec(&blobs).unwrap());
    let stored = frame_bytes(&json!({"type":"stored"}), &[]);
    scratch.write("bin/cut-out", &(install + 1));
    scratch.write(
        "bin/before-2",
        &format!(
            "echo {} > '{}/bin/cut-out'",
            install + tree + stored + 1,
            scratch.0.display()
        ),
    );
    let (_, sent) = start(&scratch, &client);
    check(
        &scratch,
        &[sent, send_calls(&scratch)].join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` didn't answer: ended within the answer: starting the job again now, attempt 2 of 7
            pumpkin: controller `alice` lacks 9 of the task's 15 files: sending 108 bytes
            pumpkin: controller `alice` didn't answer: ended within the upload's answer: starting the job again now, attempt 2 of 7
            pumpkin: controller `alice` lacks 1 of the task's 15 files: sending 12 bytes
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve, serve, serve"#]],
    );
}

#[test]
fn group_drains_after_a_discarded_partial() {
    let scratch = Scratch::new();
    let store = scratch.0.join("home/store");
    fs::create_dir_all(store.join("incoming")).unwrap();
    let bodies = [b"one", b"two", b"end"];
    let blobs = bodies.map(|body| {
        json!({
            "digest": format!("sha256:{}", sha256(body)), "size_bytes":body.len(), "from":0,
        })
    });
    let mut serve = serve_process(&scratch);
    let mut input = serve.stdin.take().unwrap();
    let puts = json!({"type":"puts","blobs":blobs});
    schema::frame::write(&mut input, &puts, &[]).unwrap();
    schema::frame::write(&mut input, &(), bodies[0]).unwrap();
    let (prefix, rest) = bodies[1].split_at(1);
    schema::frame::write(&mut input, &(), prefix).unwrap();
    let partial = store.join(format!("incoming/{}.part", sha256(bodies[1])));
    poll(|| (fs::metadata(&partial).ok()?.len() == 1).then_some(()));
    fs::remove_file(&partial).unwrap();
    fs::create_dir(partial).unwrap();
    schema::frame::write(&mut input, &(), rest).unwrap();
    schema::frame::write(&mut input, &(), bodies[2]).unwrap();
    // Retrying the group reaches the directory in place of its partial, which the store refuses.
    schema::frame::write(&mut input, &puts, &[]).unwrap();
    for body in bodies {
        schema::frame::write(&mut input, &(), body).unwrap();
    }
    schema::frame::write(&mut input, &json!({"type":"jobs"}), &[]).unwrap();
    drop(input);
    let done = serve.wait_with_output().unwrap();
    assert!(done.status.success(), "{done:?}");
    let mut frames = &done.stdout[..];
    let mut kinds = Vec::new();
    while let Some((head, _)) = schema::frame::read::<serde_json::Value>(&mut frames).unwrap() {
        if let Some(kind) = head["type"].as_str() {
            kinds.push(kind.to_owned());
        }
    }
    let stored = bodies.map(|body| store.join(sha256(body)).exists());
    check(
        &scratch,
        &format!("{kinds:?}\nstored: {stored:?}"),
        &[],
        &expect![[r#"
            ["serving", "stored", "offset_changed", "stored", "refused", "jobs"]
            stored: [true, false, false]"#]],
    );
}

#[test]
fn concurrent_uploads() {
    racing_upload(true);
}

#[test]
fn concurrent_upload_resumes_after_cut() {
    racing_upload(false);
}

fn racing_upload(first_finishes: bool) {
    let scratch = Scratch::new();
    let contents = vec![42; schema::frame::MAX_BODY_BYTES];
    let digest = sha256(&contents);
    let manifest = format!("pumpkin-tree/1\nf {digest} {} notes\n", contents.len());
    let mut query = Vec::new();
    let lacks = json!({"type":"lacks", "manifest":format!("sha256:{}", sha256(manifest.as_bytes())), "body_bytes":manifest.len(), "install":false});
    schema::frame::write(&mut query, &lacks, &[]).unwrap();
    schema::frame::write(&mut query, &(), manifest.as_bytes()).unwrap();
    assert!(serve_fed(&scratch, &query).contains(r#""type":"missing""#));

    let mut put = json!({"type":"put", "digest":format!("sha256:{digest}"), "size_bytes":contents.len(), "from":0});
    let mut first = serve_process(&scratch);
    let mut writing = first.stdin.take().unwrap();
    schema::frame::write(&mut writing, &put, &[]).unwrap();
    let (prefix, rest) = contents.split_at(contents.len() / 2);
    schema::frame::write(&mut writing, &(), prefix).unwrap();
    let partial = scratch.0.join(format!("home/store/incoming/{digest}.part"));
    poll(|| (fs::metadata(&partial).ok()?.len() == prefix.len() as u64).then_some(()));

    // This frame exceeds the pipe's capacity. Its writer can finish only if the second
    // serve reads the body while the first still holds the partial.
    let mut second = serve_process(&scratch);
    let mut racing = second.stdin.take().unwrap();
    schema::frame::write(&mut racing, &put, &[]).unwrap();
    schema::frame::write(&mut racing, &(), &contents).unwrap();
    if first_finishes {
        schema::frame::write(&mut writing, &(), rest).unwrap();
    }
    drop(writing);
    let done = first.wait_with_output().unwrap();
    assert!(done.status.success());
    std::io::Write::write_all(&mut racing, &query).unwrap();
    drop(racing);
    let done = second.wait_with_output().unwrap();
    assert!(done.status.success());
    let mut frames = &done.stdout[..];
    let answer = if first_finishes {
        "stored"
    } else {
        "offset_changed"
    };
    for kind in ["serving", answer, "missing"] {
        let (head, _) = schema::frame::read::<serde_json::Value>(&mut frames)
            .unwrap()
            .unwrap();
        assert_eq!(head["type"], kind);
    }
    let ((), body) = schema::frame::read::<()>(&mut frames).unwrap().unwrap();
    let missing: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    if first_finishes {
        assert_eq!(missing, [] as [serde_json::Value; 0]);
    } else {
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0]["held_bytes"], prefix.len());
        put["from"] = missing[0]["held_bytes"].clone();
        let mut resume = Vec::new();
        schema::frame::write(&mut resume, &put, &[]).unwrap();
        schema::frame::write(&mut resume, &(), rest).unwrap();
        assert!(serve_fed(&scratch, &resume).contains(r#""type":"stored""#));
    }
    assert_eq!(
        fs::read(scratch.0.join("home/store").join(digest)).unwrap(),
        contents
    );
    assert!(!partial.exists());
}

#[test]
fn sends_from_a_repository_with_a_native_name() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let repo = scratch.0.join("repo");
    let native = scratch.0.join(OsStr::from_bytes(b"native-\xff\n"));
    fs::rename(&repo, &native).unwrap();
    symlink(&native, &repo).unwrap();
    let (_, sent) = start(&scratch, &client);
    check(
        &scratch,
        &sent,
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 6 of the task's files: sending 2.9 KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>"#]],
    );
}

#[test]
fn refuses_a_non_utf8_entry_below_the_repository() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write_task_in("repo/tasks", "stand-in", &workflow(&image('0'), &["true"]));
    scratch.write("repo/.gitignore", "*\n");
    let tasks = scratch.0.join("repo/tasks");
    let native = scratch.0.join(OsStr::from_bytes(b"repo/tasks-\xff"));
    fs::rename(&tasks, &native).unwrap();
    symlink(&native, &tasks).unwrap();
    let run = on_client(&scratch, &client, &["run", "tasks/stand-in/task.star"]);
    let refused = super::ran(run, &scratch.0, false);
    check(
        &scratch,
        &refused,
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run tasks/stand-in/task.star
        exit status: Some(1)
        pumpkin: tasks/stand-in/task.star: its path below the git repository isn't UTF-8: rename it so it can be sent to the controller"#]],
    );
}

#[test]
fn send_refusals() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let mut lines = Vec::new();
    for name in [&b"repo/stand-in/a\nb"[..], b"repo/stand-in/\xff"] {
        let path = scratch.0.join(OsStr::from_bytes(name));
        fs::write(&path, "").unwrap();
        let (_, refused) = start(&scratch, &client);
        lines.extend([refused, send_calls(&scratch)]);
        fs::remove_file(path).unwrap();
    }
    // A budget whose model the client's configuration gives no price.
    let mut budgeted = workflow(&image('0'), &["true"]);
    let route = json!([{ "api": "responses", "models": ["model-a"], "budget": { "microusd": 1 } }]);
    budgeted["roles"][0]["runtime"]["agent"]["egress"] = json!({ "model": route });
    scratch.write_task_in("repo", "stand-in", &budgeted);
    let (_, refused) = start(&scratch, &client);
    lines.extend([refused, send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: `stand-in/a\nb` can't be sent to the controller, whose tree names each file by a line: rename it with no newline or tab, and no empty, `.` or `..` component
            the calls: none
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: `stand-in/�` isn't UTF-8, so it can't be sent to the controller, whose tree names each file by a line of text: rename it
            the calls: none
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: <scratch>/client/.config/pumpkin/pumpkin.toml: `agent` has a model budget, but `model-a` has no price for `input`, `cached`, `cache_write` or `output` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            the calls: none"#]],
    );
}

#[test]
fn send_changed_file() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    // Shortened once it is hashed, as the first call to the controller begins.
    let notes = scratch.0.join("repo/stand-in/notes");
    scratch.write(
        "bin/before-1",
        &format!("printf no > '{}'\n", notes.display()),
    );
    let (_, changed) = start(&scratch, &client);
    let mut lines = vec![changed, send_calls(&scratch)];
    let (_, again) = start(&scratch, &client);
    lines.extend([again, send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 2.9 KiB
            pumpkin: <scratch>/repo/stand-in/notes changed since the task was hashed: send the task again
            the calls: serve
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 2.9 KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve"#]],
    );
}

#[test]
fn send_evicted_file() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    let (_, first) = start(&scratch, &client);
    assert!(first.contains("exit status: Some(0)"), "{first}");
    send_calls(&scratch);
    let blob = scratch.0.join("home/store").join(sha256(b"noted\n"));
    fs::remove_file(blob).unwrap();
    let mut lines = Vec::new();
    let (_, gone) = start(&scratch, &client);
    lines.extend([gone, send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks 1 of the task's 7 files: sending 6 bytes
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: serve"#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn store_collection() {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let client = set_up_client(&scratch);
    let mut named = Vec::new();
    let mut trees = Vec::new();
    for notes in ["first", "second", "third"] {
        scratch.write("repo/stand-in/notes", notes);
        let (id, ran) = start(&scratch, &client);
        assert!(ran.contains("exit status: Some(0)"), "{ran}");
        named.push((
            sha256(notes.as_bytes()),
            format!("stand-in/notes, as the {notes} sent it"),
        ));
        named.push((
            last_tree(&scratch),
            format!("the tree that the {notes} sent"),
        ));
        trees.push((id, last_tree(&scratch)));
    }
    // The first's job makes its tree, which links its files; the second's never starts.
    let (first, tree) = &trees[0];
    assert!(job_of(&scratch, first, tree).status.success());
    let two_days_ago = SystemTime::now() - Duration::from_hours(48);
    for (_, tree) in &trees[..2] {
        let manifest = scratch.0.join(format!("home/store/manifests/{tree}"));
        File::open(manifest)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    }
    for (bytes, when) in [
        ("of two days ago", two_days_ago),
        ("of now", SystemTime::now()),
    ] {
        let hex = sha256(bytes.as_bytes());
        let piece = format!("home/store/incoming/{hex}.part");
        scratch.write(&piece, bytes);
        File::open(scratch.0.join(piece))
            .unwrap()
            .set_modified(when)
            .unwrap();
        named.push((hex, format!("a blob {bytes}")));
    }
    let active_hex = sha256(b"uploading");
    let active_path = format!("home/store/incoming/{active_hex}.part");
    scratch.write(&active_path, "up");
    let active = File::open(scratch.0.join(active_path)).unwrap();
    active.lock().unwrap();
    active.set_modified(two_days_ago).unwrap();
    named.push((active_hex, "an active upload".to_owned()));
    let bundle = "a bundle from an older client";
    let digest = sha256(bundle.as_bytes());
    let older = format!("pumpkin-tree/1\nb {digest} {} {PINNED}\n", bundle.len());
    let older_name = sha256(older.as_bytes());
    let old_blob = format!("home/store/{digest}");
    scratch.write(&old_blob, bundle);
    fs::set_permissions(scratch.0.join(old_blob), Permissions::from_mode(0o444)).unwrap();
    scratch.write(&format!("home/store/manifests/{older_name}"), &older);
    named.push((older_name, "an earlier client's manifest".to_owned()));
    named.push((digest, "its bundle".to_owned()));
    // Of a later Pumpkin's, say, which may name any blob.
    let foreign = "pumpkin-tree/2\nwhat a later form holds\n";
    let foreign_hex = sha256(foreign.as_bytes());
    let planted = format!("home/store/manifests/{foreign_hex}");
    scratch.write(&planted, foreign);
    named.push((foreign_hex.clone(), "a manifest of another form".to_owned()));
    let (third, tree) = &trees[2];
    let built = job_of(&scratch, third, tree);
    assert!(built.status.success(), "{built:?}");
    let named: Vec<(String, &str)> = (named.iter())
        .map(|(hex, name)| (hex.clone(), name.as_str()))
        .collect();
    let said = String::from_utf8(built.stderr).unwrap();
    let mut lines: Vec<String> = (said.lines())
        .filter(|line| line.contains("keeps every blob"))
        .map(str::to_owned)
        .collect();
    let shown = ["second", "manifest", "incoming"];
    lines.extend(
        (stored(&scratch, &holders(&scratch, &named)).into_iter())
            .filter(|line| shown.iter().any(|name| line.contains(name))),
    );
    drop(active);
    fs::remove_file(scratch.0.join(planted)).unwrap();
    let (fourth, ran) = start(&scratch, &client);
    assert!(ran.contains("exit status: Some(0)"), "{ran}");
    let built = job_of(&scratch, &fourth, tree);
    assert!(built.status.success(), "{built:?}");
    lines.extend(stored(&scratch, &holders(&scratch, &named)));
    let config = scratch.0.join(format!("home/builds/{fourth}/pumpkin.toml"));
    let mut here = on_controller(
        &scratch,
        &[
            "build",
            "--config",
            config.to_str().unwrap(),
            "stand-in/task.star",
        ],
    );
    let here = here.current_dir(scratch.0.join("repo")).output().unwrap();
    assert!(here.status.success(), "{here:?}");
    lines.push(format!(
        "the job built the IR that the repository builds: {}",
        built.stdout == here.stdout
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[(&foreign_hex, "<another form>")],
        &expect![[r#"
            pumpkin: <scratch>/home/store/manifests/<another form>: the manifest doesn't start with `pumpkin-tree/1`: expected a manifest of this Pumpkin's form; so the prune keeps every blob of the store, which it may name
              stand-in/notes, as the second sent it: 444, 1 link
              a manifest: a manifest of another form
              a manifest: a tree of no files
              a manifest: an earlier client's manifest
              a manifest: the install
              a manifest: the tree that the third sent
              incoming/: partial an active upload, 2 bytes
              incoming/: partial a blob of now, 6 bytes
            the store:
              PUMPKIN.star: 444, 4 links
              its bundle: 444, 1 link
              lib/train.star: 444, 4 links
              stand-in/Dockerfile: 444, 4 links
              stand-in/final.star: 444, 4 links
              stand-in/notes, as the first sent it: 444, 2 links
              stand-in/notes, as the third sent it: 444, 3 links
              stand-in/submit.star: 444, 4 links
              stand-in/task.star: 444, 4 links
              the installed buildifier, to run: 555, 2 links
              the installed pumpkin, to run: 555, 2 links
              a manifest: a tree of no files
              a manifest: an earlier client's manifest
              a manifest: the install
              a manifest: the tree that the third sent
              incoming/: partial a blob of now, 6 bytes
            the job built the IR that the repository builds: true"#]],
    );
}

fn ended_job(scratch: &Scratch, id: &str, log: &str) {
    let job = format!("home/jobs/{id}");
    scratch.write(&format!("{job}/log"), log);
    scratch.write(&format!("{job}/out"), &format!("{id}\n"));
    let record = format!("home/.pumpkin/runs/{id}");
    let args = ["stand-in/task.star"];
    scratch.write(
        &format!("{record}/record.json"),
        &json!({ "record_version": "pumpkin.record/v1", "args": args }),
    );
    scratch.write(&format!("{record}/driver.pid"), "4242\n");
    scratch.write(&format!("{record}/outcome.json"), &returned(1.0));
}

#[test]
fn shell_client_cancels_with_the_jobs_executable() {
    let scratch = Scratch::new();
    let id = id_ago(0, "0c000005");
    ended_job(&scratch, &id, "");
    let pumpkin = install(&scratch, &format!("home/.pumpkin/{PINNED}/bin"));
    let own = scratch.0.join(format!("home/jobs/{id}/pumpkin"));
    symlink(pumpkin, &own).unwrap();
    let args = [
        "controller",
        "cancel",
        "--force",
        "--modal-config",
        "modal.toml",
        &id,
    ];
    let mut cancel = scratch.program(own, &args);
    let invoked = invocation(&cancel);
    let output = cancel.output().unwrap();
    check(
        &scratch,
        &report(invoked, &output, &scratch.0, false),
        &[(&id, "<id>")],
        &expect![[r#"
            $ <scratch>/home/jobs/<id>/pumpkin controller cancel --force --modal-config modal.toml <id>
            exit status: Some(102)"#]],
    );
}

#[test]
fn cancel_stops_the_unit_once() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0c000006");
    scratch.write(&format!("home/jobs/{id}/log"), "a job still building\n");
    scratch.write(&format!("run/pumpkin/{id}/held"), "");
    let ctl = r#"#!/bin/sh
echo "$*" >> "${0%/*}/units"
"#;
    scratch.write_executable("bin/systemctl", ctl.as_bytes());
    let mut lines = Vec::new();
    for _ in 0..2 {
        let cancel = on_client(&scratch, &client, &["cancel", "--force", &id])
            .output()
            .unwrap();
        lines.push(format!(
            "{}: {}",
            cancel.status,
            String::from_utf8_lossy(&cancel.stderr).trim()
        ));
    }
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    assert!(scratch.0.join(format!("home/jobs/{id}")).is_dir());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
        exit status: 0: pumpkin: run <id>'s job on controller `alice` is stopped, before the run started
        exit status: 0: pumpkin: run <id>'s job on controller `alice` is stopped, before the run started
        --user kill --signal=KILL pumpkin-job-<id>
        --user stop pumpkin-job-<id>"#]],
    );
}

#[test]
fn cancel_another_builds_job() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0c000007");
    ended_job(&scratch, &id, "running a different build\n");
    let dir = format!("home/.pumpkin/runs/{id}");
    let foreign = json!({ "record_version": "another format", "pumpkin": { "version": "1.2.3" } });
    scratch.write(&format!("{dir}/record.json"), &foreign);
    scratch.write(&format!("{dir}/outcome.json"), "the other build's outcome");
    scratch.write(&format!("run/pumpkin/{id}/held"), "");
    let ctl = r#"#!/bin/sh
echo "$*" >> "${0%/*}/units"
rm -r -- "$XDG_RUNTIME_DIR/pumpkin/${3#pumpkin-job-}"
"#;
    scratch.write_executable("bin/systemctl", ctl.as_bytes());
    let cancel = on_client(&scratch, &client, &["cancel", &id])
        .output()
        .unwrap();
    let copied = scratch
        .0
        .join(format!("repo/.pumpkin/runs/{id}/outcome.json"));
    assert_eq!(
        fs::read_to_string(copied).unwrap(),
        "the other build's outcome"
    );
    let said = String::from_utf8_lossy(&cancel.stderr);
    assert!(said.contains("1.2.3"), "{said}");
    assert!(cancel.status.success(), "{said}");
    assert_eq!(
        fs::read_to_string(scratch.0.join("bin/units")).unwrap(),
        format!("--user stop pumpkin-job-{id}\n")
    );
}

#[test]
fn cancel_copies_the_record_when_reaping_fails() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0c000008");
    scratch.write(&format!("home/jobs/{id}/log"), "");
    write_record(&scratch, &id, true, None);
    scratch.write(&format!("runs/{id}/agents/agent/sandbox.json"), "{}");
    move_runs(&scratch, "home/.pumpkin/runs");
    let cancel = on_client(&scratch, &client, &["cancel", &id])
        .output()
        .unwrap();
    assert_eq!(cancel.status.code(), Some(1));
    let said = String::from_utf8_lossy(&cancel.stderr);
    assert!(
        said.contains("sandbox.json names no provider and id"),
        "{said}"
    );
    let copy = scratch
        .0
        .join(format!("repo/.pumpkin/runs/{id}/outcome.json"));
    let outcome: serde_json::Value = serde_json::from_slice(&fs::read(copy).unwrap()).unwrap();
    assert_eq!(outcome["reason"], "its driver ended without an outcome");
}

#[test]
fn reap_by_writer() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let made = "home/.pumpkin/runs";
    let own = id_ago(0, "0e000001");
    let other = id_ago(0, "0e000002");
    let old = id_ago(0, "0e000003");
    let record =
        json!({"record_version":"pumpkin.record/v1","pumpkin":super::this_build(),"args":[]});
    scratch.write(&format!("{made}/{own}/record.json"), &record);
    scratch.write(&format!("{made}/{own}/driver.pid"), "4242\n");
    scratch.write(
        &format!("{made}/{own}/provider.json"),
        &json!({"providers":{}}),
    );
    scratch.write(
        &format!("{made}/{other}/record.json"),
        &json!({"pumpkin":{"version":"1.2.3"},"record_version":"future"}),
    );
    scratch.write(
        &format!("{made}/{old}/record.json"),
        &json!({"record_version":"old"}),
    );
    let mut lines = Vec::new();
    for _ in 0..2 {
        let mut reap = on_client(&scratch, &client, &["reap"]);
        let invoked = invocation(&reap);
        let reaped = reap.output().unwrap();
        lines.push(report(invoked, &reaped, &scratch.0, false));
    }
    for foreign in [&other, &old] {
        assert!(
            !scratch
                .0
                .join(format!("{made}/{foreign}/outcome.json"))
                .exists()
        );
    }
    assert!(
        scratch
            .0
            .join(format!("{made}/{own}/outcome.json"))
            .is_file()
    );
    check(
        &scratch,
        &lines.join("\n"),
        &[(&own, "<own>"), (&other, "<other>"), (&old, "<old>")],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap
            exit status: Some(0)
            pumpkin: run <own> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: to reap runs made by an older pumpkin, use that build: <old>
            pumpkin: to reap runs made by pumpkin 1.2.3, use that build: <other>
            printed:
              <own>
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap
            exit status: Some(0)
            pumpkin: to reap runs made by an older pumpkin, use that build: <old>
            pumpkin: to reap runs made by pumpkin 1.2.3, use that build: <other>"#]],
    );
}

#[test]
fn reap_after_a_cut() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0e000004");
    write_record(&scratch, &id, true, None);
    move_runs(&scratch, "home/.pumpkin/runs");
    let record = format!("home/.pumpkin/runs/{id}");
    scratch.write(
        &format!("{record}/provider.json"),
        &json!({"providers": {}}),
    );
    // One byte of the reply proves the first sweep finished.
    let cut = frame_bytes(&json!({ "type": "serving" }), &[]) + 1;
    scratch.write("bin/cut-out", &cut);
    let mut reap = on_client(&scratch, &client, &["reap"]);
    let invoked = invocation(&reap);
    let reaped = reap.output().unwrap();
    let outcome = fs::read(scratch.0.join(format!("{record}/outcome.json"))).unwrap();
    let outcome: serde_json::Value = serde_json::from_slice(&outcome).unwrap();
    assert_eq!(outcome["state"], "failed");
    check(
        &scratch,
        &report(invoked, &reaped, &scratch.0, false),
        &[(&id, "<id>")],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap
            exit status: Some(0)
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: the reap's reply was cut: the output may omit ids of runs already cleaned
            pumpkin: controller `alice` didn't answer: ended within the answer: reaping the runs again now, attempt 2 of 7"#]],
    );
}

fn frame_bytes(message: &serde_json::Value, body: &[u8]) -> usize {
    let mut frame = Vec::new();
    schema::frame::write(&mut frame, message, body).unwrap();
    frame.len()
}

// Status precedes Follow on the same link. The job is the only one in this controller home, and
// names no member.
fn named_bytes(id: &str) -> usize {
    let jobs = json!({ "member": "alice", "jobs": [{ "id": id, "owner": { "type": "nobody" } }] });
    let body = serde_json::to_vec(&jobs).unwrap();
    frame_bytes(&json!({ "type": "jobs", "body_bytes": body.len() }), &[])
        + frame_bytes(&serde_json::Value::Null, &body)
}

#[test]
fn follow_carries_on() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0f000001");
    let log = "building\0task\nstep 1\nstep 1\npublish";
    ended_job(&scratch, &id, log);
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let logged = frame_bytes(&json!({ "type": "logged" }), log.as_bytes());
    let mut lines = Vec::new();
    for (when, bytes) in [
        ("within the log", hello + logged - 1),
        ("after the log", hello + logged),
        ("within the ending", hello + logged + 8),
    ] {
        let _ = calls(&scratch);
        scratch.write(
            "bin/before-1",
            &format!(
                "echo {} > '{}/bin/cut-out'",
                bytes + named_bytes(&id),
                scratch.0.display()
            ),
        );
        let _ = fs::remove_dir_all(scratch.0.join(format!("repo/.pumpkin/runs/{id}")));
        let waited = on_client(&scratch, &client, &["wait", &id])
            .output()
            .unwrap();
        assert!(waited.status.success(), "{waited:?}");
        let stderr = String::from_utf8(waited.stderr).unwrap();
        let (shown, _) = stderr.split_once("pumpkin: copying run").unwrap();
        lines.push(format!("cut {when}: {:?}", shown.replace('\0', "<NUL>")));
    }
    check(
        &scratch,
        &lines.join("\n"),
        &[(id.as_str(), "<id>")],
        &expect![[r#"
            cut within the log: "pumpkin: controller `alice` didn't answer: ended within the answer: following job <id> again now, attempt 2 of 7\nbuilding<NUL>task\nstep 1\nstep 1\npublish\n"
            cut after the log: "building<NUL>task\nstep 1\nstep 1\npublish\npumpkin: controller `alice` didn't answer: ended before the answer: following job <id> again now, attempt 2 of 7\n"
            cut within the ending: "building<NUL>task\nstep 1\nstep 1\npublish\npumpkin: controller `alice` didn't answer: ended within the answer: following job <id> again now, attempt 2 of 7\n""#]],
    );
}

#[test]
fn silent_build_loses_its_follower() {
    let scratch = Scratch::new();
    let id = id_ago(0, "0f000007");
    let log = format!("home/builds/{id}/log");
    scratch.write(&log, "building\n");
    let active = scratch.0.join("run/pumpkin").join(&id);
    fs::create_dir_all(&active).unwrap();
    let mut child = scratch
        .program(PUMPKIN, &["controller", "serve"])
        .env("XDG_RUNTIME_DIR", scratch.0.join("run"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = json!({ "type": "follow", "id": id, "log_from_bytes": 0, "after_cut": false });
    schema::frame::write(&mut child.stdin.take().unwrap(), &request, &[]).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut next = || {
        schema::frame::read::<serde_json::Value>(&mut stdout)
            .unwrap()
            .unwrap()
    };
    assert_eq!(next().0, json!({ "type": "serving" }));
    assert_eq!(next().1, b"building\n");
    scratch.write(&log, "building\nready\n");
    assert_eq!(next().1, b"ready\n");
    drop(stdout);
    let exit = child.wait().unwrap();
    check(
        &scratch,
        &format!("{exit}; build still active: {}", active.is_dir()),
        &[],
        &expect!["exit status: 0; build still active: true"],
    );
}

fn said_bytes(message: &str) -> usize {
    frame_bytes(&json!({ "type": "said", "body_bytes": message.len() }), &[])
        + frame_bytes(&serde_json::Value::Null, message.as_bytes())
}

#[test]
fn follow_repeats_no_milestone() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0f000005");
    let [work, agent] = super::started_work(1);
    let dir = write_run(
        &scratch,
        &id,
        asked(&id, 30),
        &[work, agent, super::agent_exited(2)],
        None,
        &lease_places(Prices::Nothing),
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    move_runs(&scratch, "home/.pumpkin/runs");
    scratch.write(&format!("home/jobs/{id}/out"), &id);
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let first = format!("following run {id}: ending this command does not stop it");
    let _ = calls(&scratch);
    let started = "   0:01 solver started on aws/judge";
    let cut = hello + named_bytes(&id) + said_bytes(&first) + said_bytes(started) + 3;
    let root = scratch.0.display();
    // Cut in the next milestone. The run ends before the next link opens.
    scratch.write(
        "bin/before-1",
        &format!("echo {cut} > '{root}/bin/cut-out'"),
    );
    scratch.write("outcome", &returned(1.0));
    scratch.write(
        "bin/before-2",
        &format!("cp '{root}/outcome' '{root}/home/.pumpkin/runs/{id}/outcome.json'"),
    );
    let child = on_client(&scratch, &client, &["wait", &id])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let outcome = scratch
        .0
        .join(format!("home/.pumpkin/runs/{id}/outcome.json"));
    poll(|| outcome.exists().then_some(()));
    drop(driver);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let shown: Vec<_> = stderr
        .lines()
        .filter(|line| {
            line.contains("ending this command")
                || line.contains(started)
                || line.contains("0:02 the work phase ended")
                || line.contains("attempt 2 of 7")
        })
        .collect();
    check(
        &scratch,
        &shown.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            pumpkin: following run <id>: ending this command does not stop it
            pumpkin:    0:01 solver started on aws/judge
            pumpkin: controller `alice` didn't answer: ended within the answer: following job <id> again now, attempt 2 of 7
            pumpkin:    0:02 the work phase ended"#]],
    );
}

#[test]
fn build_carries_on_inside_the_ir() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let ir = r#"{"workflow":"whole IR"}"#;
    let log = "building the images\n";
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let logged = frame_bytes(&json!({ "type": "logged" }), log.as_bytes());
    let head = frame_bytes(&json!({ "type": "built", "body_bytes": ir.len() }), &[]);
    let part = frame_bytes(&serde_json::Value::Null, ir.as_bytes());
    // The warmed store answers the two Lacks with empty lists, then starts the build on this link.
    let (_, warm) = start(&scratch, &client);
    assert!(warm.contains("exit status: Some(0)"), "{warm}");
    let missing = frame_bytes(&json!({"type":"missing","body_bytes":2}), &[])
        + frame_bytes(&serde_json::Value::Null, b"[]");
    let started = frame_bytes(&json!({"type":"started"}), &[]);
    let cut = hello + 2 * missing + started + logged + head + part - 1;
    scratch.write("bin/cut-out", &cut.to_string());
    let mut build = on_client(&scratch, &client, &["build", "stand-in/task.star"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let dir = poll(|| {
        fs::read_dir(scratch.0.join("home/builds"))
            .ok()?
            .next()?
            .ok()
    });
    let id = dir.file_name();
    let id = id.to_str().unwrap();
    scratch.write(&format!("home/builds/{id}/log"), log);
    // A build's out can already hold bytes while its job still runs.
    scratch.write(&format!("home/builds/{id}/out"), "{");
    let mut stderr = BufReader::new(build.stderr.take().unwrap());
    let mut said = String::new();
    while !said.ends_with(log) && stderr.read_line(&mut said).unwrap() > 0 {}
    scratch.write(&format!("home/builds/{id}/out"), ir);
    fs::remove_dir_all(scratch.0.join("run/pumpkin").join(id)).unwrap();
    let output = ended(build, stderr, said);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let logged = stderr
        .lines()
        .filter(|line| *line == log.trim_end())
        .count();
    let again = stderr.matches("attempt 2 of 7").count();
    check(
        &scratch,
        &format!(
            "job's log shown {logged} time(s), {again} immediate retries\nIR on stdout: {}",
            String::from_utf8(output.stdout).unwrap()
        ),
        &[],
        &expect![[r#"
            job's log shown 1 time(s), 1 immediate retries
            IR on stdout: {"workflow":"whole IR"}"#]],
    );
}

#[test]
fn follow_cut_often() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0f000002");
    ended_job(&scratch, &id, "");
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let logged = frame_bytes(&json!({ "type": "logged" }), b"line 01\n");
    let _ = calls(&scratch);
    // A new line on each connection advances the byte offset and starts the pauses over.
    for index in 1..=9 {
        let before = if index == 1 { named_bytes(&id) } else { 0 };
        scratch.write(
            &format!("bin/before-{index}"),
            &format!(
                "echo 'line {index:02}' >> '{0}/home/jobs/{id}/log'\necho {1} > '{0}/bin/cut-out'\n",
                scratch.0.display(), hello + before + logged + 2
            ),
        );
    }
    let waited = on_client(&scratch, &client, &["wait", &id])
        .output()
        .unwrap();
    assert!(waited.status.success(), "{waited:?}");
    let stderr = String::from_utf8(waited.stderr).unwrap();
    let (shown, _) = stderr.split_once("pumpkin: copying run").unwrap();
    let lines: Vec<_> = shown
        .lines()
        .filter(|line| line.starts_with("line "))
        .collect();
    let again = shown.matches("attempt 2 of 7").count();
    check(
        &scratch,
        &format!("{}\n{again} immediate retries", lines.join("\n")),
        &[],
        &expect![[r#"
            line 01
            line 02
            line 03
            line 04
            line 05
            line 06
            line 07
            line 08
            line 09
            9 immediate retries"#]],
    );
}

#[test]
fn follow_cut_before_a_line() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0f000003");
    ended_job(&scratch, &id, "line 01 of 01\n");
    let _ = calls(&scratch);
    let cut = scratch.0.join("bin/cut-out");
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    for cut_index in 0..3 {
        let bytes = if cut_index == 0 {
            hello + named_bytes(&id)
        } else {
            hello
        };
        scratch.write(
            &format!("bin/before-{}", 1 + cut_index),
            &format!("echo {bytes} > '{}'\n", cut.display()),
        );
    }
    let mut waiting = on_client(&scratch, &client, &["wait", &id])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(waiting.stderr.take().unwrap());
    let mut shown = Vec::new();
    for line in stderr.lines() {
        let line = line.unwrap();
        // The third attempt is said before its pause, which the test doesn't wait out.
        let third = line.ends_with("attempt 3 of 7");
        shown.push(line);
        if third {
            break;
        }
    }
    waiting.kill().unwrap();
    waiting.wait().unwrap();
    check(
        &scratch,
        &shown.join("\n"),
        &[(id.as_str(), "<id>")],
        &expect![[r#"
            pumpkin: controller `alice` didn't answer: ended before the answer: following job <id> again now, attempt 2 of 7
            pumpkin: controller `alice` didn't answer: ended before the answer: following job <id> again in 5 s, attempt 3 of 7"#]],
    );
}

#[test]
fn follow_prints_the_outcome() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0f000004");
    ended_job(&scratch, &id, "line 01 of 01\n");
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/{id}"));
    let mut lines = Vec::new();
    for args in [&["wait", &id][..], &["wait", "--outcome", &id]] {
        let _ = fs::remove_dir_all(&copy);
        let waited = on_client(&scratch, &client, args).output().unwrap();
        assert!(waited.status.success(), "{waited:?}");
        let stderr = String::from_utf8(waited.stderr).unwrap();
        let ended = stderr
            .lines()
            .find(|line| line.contains(" finished "))
            .unwrap();
        let copied = fs::read(copy.join("outcome.json")).unwrap();
        let printed = match &waited.stdout {
            output if output.is_empty() => "nothing",
            output if *output == copied => "the copy's outcome.json",
            _ => "something else",
        };
        lines.extend([
            format!("$ pumpkin {}", args.join(" ")),
            ended.to_owned(),
            format!("printed: {printed}"),
        ]);
    }
    check(
        &scratch,
        &lines.join("\n"),
        &[(id.as_str(), "<id>")],
        &expect![[r#"
            $ pumpkin wait <id>
            pumpkin: run <id> finished with a result (score=1 metrics={"solved":3}); its record is <scratch>/repo/.pumpkin/runs/<id>
            printed: nothing
            $ pumpkin wait --outcome <id>
            pumpkin: run <id> finished with a result (score=1 metrics={"solved":3}); its record is <scratch>/repo/.pumpkin/runs/<id>
            printed: the copy's outcome.json"#]],
    );
}

#[test]
fn follow_copies_another_writers_record() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0f000006");
    ended_job(&scratch, &id, "build output\n");
    let path = scratch
        .0
        .join(format!("home/.pumpkin/runs/{id}/record.json"));
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/{id}"));
    let mut lines = Vec::new();
    for record in [
        json!({ "pumpkin": { "version": "1.2.3" }, "record_version": "another form" }).to_string(),
        "{".to_owned(),
        json!({ "pumpkin": super::this_build(), "record_version": "another form" }).to_string(),
    ] {
        let _ = fs::remove_dir_all(&copy);
        fs::write(&path, &record).unwrap();
        let waited = on_client(&scratch, &client, &["wait", &id])
            .output()
            .unwrap();
        let stderr = String::from_utf8(waited.stderr).unwrap();
        let copied = fs::read_to_string(copy.join("record.json")).ok() == Some(record);
        lines.push(format!(
            "{}; copied the record: {copied}\n  {}",
            waited.status,
            stderr.lines().last().unwrap()
        ));
    }
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 1; copied the record: true
              pumpkin: copied run <id>'s record to <scratch>/repo/.pumpkin/runs/<id>; it was made by pumpkin 1.2.3: read it with that build
            exit status: 1; copied the record: true
              pumpkin: <scratch>/repo/.pumpkin/runs/<id>/record.json: EOF while parsing an object at line 1 column 1
            exit status: 1; copied the record: false
              pumpkin: controller `alice` refused: <scratch>/home/.pumpkin/runs/<id>/record.json holds record_version "another form": expected "pumpkin.record/v1", which this `pumpkin` reads"#]],
    );
}

// As the first `ls` installs it, but as links to the client's files, which write nothing.
fn served(scratch: &Scratch) -> String {
    let key = key(&client_bin(scratch));
    let dir = scratch.0.join(format!("home/.pumpkin/serve/{key}"));
    if dir.is_dir() {
        return key;
    }
    fs::create_dir_all(&dir).unwrap();
    for name in BUILT {
        fs::hard_link(client_bin(scratch).join(name), dir.join(name)).unwrap();
    }
    key
}

// What `install` lays out of a build: its `pumpkin` and the formatter.
const BUILT: [&str; 2] = ["pumpkin", "buildifier"];

fn client_bin(scratch: &Scratch) -> PathBuf {
    scratch.0.join(format!("repo/.pumpkin/{PINNED}/bin"))
}

fn install_manifest(bin: &Path) -> String {
    let mut names = BUILT;
    names.sort_unstable();
    let mut text = "pumpkin-tree/1\n".to_owned();
    for name in names {
        let bytes = fs::read(bin.join(name)).unwrap();
        let _ = writeln!(text, "x {} {} {name}", sha256(&bytes), bytes.len());
    }
    text
}

fn key(bin: &Path) -> String {
    sha256(install_manifest(bin).as_bytes())
}

// Whether each file of the install in `bin` is the client's file of its name.
fn files_of(scratch: &Scratch, bin: &Path) -> String {
    let lines = BUILT.map(|name| {
        let client = fs::read(client_bin(scratch).join(name)).unwrap();
        let other = if fs::read(bin.join(name)).unwrap() == client {
            ""
        } else {
            "other than "
        };
        format!("  {name}: {other}the client's {name}")
    });
    format!("its files:\n{}", lines.join("\n"))
}

/// Moves the records that `write_record` wrote to `to`, below the scratch directory.
fn move_runs(scratch: &Scratch, to: &str) {
    for entry in fs::read_dir(scratch.0.join("runs")).unwrap() {
        let entry = entry.unwrap();
        let dir = scratch.0.join(to).join(entry.file_name());
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::rename(entry.path(), dir).unwrap();
    }
}

fn days_ago(path: &Path, days: u64) {
    let at = SystemTime::now() - Duration::from_hours(24 * days);
    File::open(path).unwrap().set_modified(at).unwrap();
}

fn ls_ran(scratch: &Scratch, client: &[(String, String); 2], args: &[&str]) -> String {
    let mut ls = on_client(scratch, client, &["ls"]);
    ls.args(args);
    let output = ls.output().unwrap();
    let invoked = format!("$ pumpkin ls {}", args.join(" "));
    report(invoked.trim_end().to_owned(), &output, &scratch.0, false)
}

#[test]
fn ls_answered_there() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let (ended, live, old) = (
        "20261107-120000-1b8a0001",
        "20261107-120100-1b8a0002",
        "20261106-120000-1b8a0003",
    );
    let failed = json!({ "state": "failed", "reason": "r" });
    write_record(&scratch, ended, true, Some(returned(7.0)));
    write_record(&scratch, live, true, None);
    write_record(&scratch, old, true, Some(failed.clone()));
    move_runs(&scratch, "home/.pumpkin/runs");
    let there = scratch.0.join("home/.pumpkin/runs");
    days_ago(&there.join(format!("{old}/outcome.json")), 2);
    let driver = File::open(there.join(format!("{live}/driver.pid"))).unwrap();
    driver.lock().unwrap();
    // The jobs: one of a run that has a record, and one of each state yet to start a run.
    let (waiting, building) = ("20261107-130000-2c9b0004", "20261107-130100-2c9b0005");
    let unstarted = ["20261107-130200-2c9b0006", "20261106-130300-2c9b0007"];
    for id in [ended, waiting, building].iter().chain(&unstarted) {
        scratch.write(&format!("home/jobs/{id}/log"), "");
    }
    scratch.write(&format!("run/pumpkin/{waiting}/waiting"), "");
    fs::create_dir_all(scratch.0.join(format!("run/pumpkin/{building}"))).unwrap();
    days_ago(
        &scratch.0.join(format!("home/jobs/{}/log", unstarted[1])),
        2,
    );
    // The copies here: of a run that the controller lacks, and of one that it lists otherwise.
    let lacked = "20261107-110000-0d0c0008";
    write_record(&scratch, lacked, true, Some(returned(3.0)));
    write_record(&scratch, ended, true, Some(failed));
    move_runs(&scratch, "repo/.pumpkin/runs");
    let mut steps = [&[][..], &["20261106"]].map(|args| ls_ran(&scratch, &client, args));
    // The rows whose times are relative to now.
    for varying in ["20261107-1201", "20261107-13", "20261106-13"] {
        steps = steps.map(|ran| masked(&ran, varying));
    }
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
        $ pumpkin ls
        exit status: Some(0)
        pumpkin: controller `alice`: up <t>, 3 jobs, disk <n>% used
        printed:
          20261107-110000-0d0c0008 finished          -  -               -  -: score=3 metrics={"solved":3}
          20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
          20261107-120100-1b8a0002 publishing      <t>                  -  -
          20261107-130000-2c9b0004 waiting         <t>                  -  -
          20261107-130100-2c9b0005 building        <t>                  -  -
          20261107-130200-2c9b0006 unstarted       <t>                  -  -: its job ended before the run started: to see why: pumpkin wait 20261107-130200-2c9b0006
          6 runs: 1 waiting, 1 building, 1 running, 2 finished, 1 unstarted; 2 runs that ended over a day ago aren't listed: to list them: pumpkin ls 20261106
        $ pumpkin ls 20261106
        exit status: Some(0)
        pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
        printed:
          20261106-120000-1b8a0003 failed            -  -               -  -
          20261106-130300-2c9b0007 unstarted       <t>                  -  -: its job ended before the run started: to see why: pumpkin wait 20261106-130300-2c9b0007
          2 runs: 1 failed, 1 unstarted"#]],
    );
}

#[test]
fn ls_lists_other_formats_unread() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let other =
        json!({ "version": "9.9.9", "commit": { "hash": "0f3a2b1c9d8e", "date": "2026-11-08" } });
    let record = json!({ "record_version": "pumpkin.record/v9", "pumpkin": other, "args": [] });
    scratch.write(
        "home/.pumpkin/runs/20261107-120200-3d0c0002/record.json",
        &record,
    );
    let steps = ls_ran(&scratch, &client, &[]);
    check(
        &scratch,
        &steps,
        &[],
        &expect![[r#"
        $ pumpkin ls
        exit status: Some(0)
        pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
        printed:
          20261107-120200-3d0c0002 unread            -                  -  made by pumpkin 9.9.9 (0f3a2b1c9 2026-11-08)
          1 run: 1 unread"#]],
    );
}

#[test]
fn ls_installs_its_serve_there() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    // The executable's local name can differ from its name in the install.
    let bin = client_bin(&scratch);
    let renamed = bin.with_file_name("renamed");
    fs::rename(bin.join("pumpkin"), &renamed).unwrap();
    symlink(&renamed, bin.join("pumpkin")).unwrap();
    fs::hard_link(bin.join("buildifier"), renamed.with_file_name("buildifier")).unwrap();
    connect(&scratch, &client);
    write_record(
        &scratch,
        "20261107-120000-1b8a0001",
        true,
        Some(returned(7.0)),
    );
    move_runs(&scratch, "home/.pumpkin/runs");
    // Another build's install, and what an install left, that none used for two days, and the
    // install of a build used since.
    let installs = scratch.0.join("home/.pumpkin/serve");
    for (name, days) in [
        ("a".repeat(64), 2),
        (".made.Xy12Zq".to_owned(), 2),
        ("b".repeat(64), 0),
    ] {
        let path = installs.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o775)).unwrap();
        days_ago(&path, days);
    }
    let mut steps = Vec::new();
    for _ in 0..2 {
        steps.push(ls_ran(&scratch, &client, &[]));
        steps.push(calls(&scratch));
    }
    let hex = key(&client_bin(&scratch));
    // Sorted as named in the snapshot, as this build's SHA-256 sorts anywhere.
    let mut left: Vec<String> = (modes(&installs).split(", "))
        .map(|install| install.replace(&hex, "<this build>"))
        .collect();
    left.sort();
    steps.push(format!("installs: {}", left.join(", ")));
    steps.push(format!("the install: {}", modes(&installs.join(&hex))));
    steps.push(files_of(&scratch, &installs.join(&hex)));
    check(
        &scratch,
        &steps.join("\n"),
        &[(&hex, "<this build>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice` has no serve of this pumpkin: installing it there
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            printed:
              20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
              1 run: 1 finished
            the calls: serve=.pumpkin/serve/<this build>/pumpkin, installs=.pumpkin/serve, serve=.pumpkin/serve/<this build>/pumpkin
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            printed:
              20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
              1 run: 1 finished
            the calls: serve=.pumpkin/serve/<this build>/pumpkin
            installs: <this build> 700, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 775
            the install: buildifier 555, pumpkin 555
            its files:
              pumpkin: the client's pumpkin
              buildifier: the client's buildifier"#]],
    );
}

#[test]
fn ls_carries_on_after_a_cut() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    write_record(
        &scratch,
        "20261107-120000-1b8a0001",
        true,
        Some(returned(7.0)),
    );
    move_runs(&scratch, "home/.pumpkin/runs");
    // Past the hello, within the answer to `Health`.
    scratch.write("bin/cut-out", "40");
    let steps = [ls_ran(&scratch, &client, &[]), calls(&scratch)];
    check(
        &scratch,
        &steps.join("\n"),
        &[(&hex, "<this build>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice` didn't answer: ended within the answer: listing the runs again now, attempt 2 of 7
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            printed:
              20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
              1 run: 1 finished
            the calls: serve=.pumpkin/serve/<this build>/pumpkin, serve=.pumpkin/serve/<this build>/pumpkin"#]],
    );
}

#[test]
fn ls_refusals() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let mut steps = Vec::new();
    scratch.write("bin/answer", "Welcome to the controller!\n");
    steps.push(ls_ran(&scratch, &client, &[]));
    // A run of this build that doesn't read, beside one that does.
    write_record(
        &scratch,
        "20261107-120000-1b8a0001",
        true,
        Some(returned(7.0)),
    );
    write_record(&scratch, "20261107-120100-1b8a0002", false, None);
    move_runs(&scratch, "home/.pumpkin/runs");
    steps.push(ls_ran(&scratch, &client, &[]));
    let runs = scratch.0.join("home/.pumpkin/runs");
    fs::rename(&runs, scratch.0.join("aside")).unwrap();
    fs::write(&runs, "").unwrap();
    steps.push(ls_ran(&scratch, &client, &[]));
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: linking to controller `alice` failed: its hello is no frame that `pumpkin controller serve` sends: a frame's message is 1466264675 bytes: it holds at most 65536: expected its frames alone on the standard output there, which a shell's startup file may write to
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: <scratch>/home/.pumpkin/runs/20261107-120100-1b8a0002/driver.pid isn't there: expected the record of a run to have it
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            pumpkin: 1 run can't be listed, as said above
            printed:
              20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
              1 run: 1 finished
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: controller `alice` refused: <scratch>/home/.pumpkin/runs: Not a directory (os error 20): expected the directory that runs are recorded in, as `--runs` names it"#]],
    );
}

#[test]
fn repair_uses_the_location_found_while_it_waited() {
    repair_while_waiting(
        None,
        &expect![[r#"
            SSH attempts: 3
            AWS calls: 0"#]],
    );
}

#[test]
fn repair_uses_the_failure_at_the_new_location() {
    repair_while_waiting(
        Some("another command could not reach this location"),
        &expect![[r#"
            SSH attempts: 2
            AWS calls: 0"#]],
    );
}

fn repair_while_waiting(failure: Option<&str>, expected: &Expect) {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let dir = scratch.0.join(SHARED);
    let lock = File::open(dir.join("lock")).unwrap();
    lock.lock().unwrap();
    let _ = noted(&scratch);
    let _ = calls(&scratch);
    for call in 1..=2 {
        scratch.write(
            &format!("bin/before-{call}"),
            &format!("echo 0 > '{}/bin/cut-out'\n", scratch.0.display()),
        );
    }
    let mut child = on_client(&scratch, &client, &["ls"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting for another command") {
        assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
    }
    // A second replacement can reuse the first inode once all readers have closed it.
    for _ in 0..2 {
        fs::copy(dir.join("found"), dir.join("found.new")).unwrap();
        fs::rename(dir.join("found.new"), dir.join("found")).unwrap();
    }
    if let Some(error) = failure {
        fs::write(dir.join("failed"), error).unwrap();
    }
    drop(lock);
    let output = ended(child, stderr, said);
    assert_eq!(output.status.success(), failure.is_none(), "{output:?}");
    if let Some(error) = failure {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(error),
            "{output:?}"
        );
    }
    let attempts = fs::read_to_string(scratch.0.join("bin/scripts"))
        .unwrap()
        .lines()
        .count();
    check(
        &scratch,
        &format!(
            "SSH attempts: {attempts}\nAWS calls: {}",
            noted(&scratch).len()
        ),
        &[],
        expected,
    );
}

#[test]
fn commands_share_a_recent_failed_repair() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let _ = calls(&scratch);
    let dir = scratch.0.join("bin");
    for call in 1..=2 {
        scratch.write(
            &format!("bin/before-{call}"),
            &format!("echo 0 > '{}/cut-out'\n", dir.display()),
        );
    }
    let first = ls_ran(&scratch, &client, &[]);
    let attempts = fs::read_to_string(dir.join("scripts"))
        .unwrap()
        .lines()
        .count();
    let _ = calls(&scratch);
    let _ = noted(&scratch);
    let second = ls_ran(&scratch, &client, &[]);
    let recent = fs::read_to_string(scratch.0.join(SHARED).join("failed")).unwrap();
    assert!(first.contains(&recent) && second.contains(&recent));
    let lines = format!(
        "first command: {attempts} SSH attempts\nsecond command: {}\nAWS calls after the failure: {}",
        calls(&scratch),
        noted(&scratch).len()
    );
    check(
        &scratch,
        &lines,
        &[],
        &expect![[r#"
        first command: 2 SSH attempts
        second command: the calls: none
        AWS calls after the failure: 0"#]],
    );
}

#[test]
fn wait_names_its_run_there() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    // Two runs' jobs, whose runs never started, with a prefix in common.
    scratch.write("home/jobs/20261107-120000-1b8a0001/log", "");
    scratch.write("home/jobs/20261107-130000-2c9b0002/log", "");
    let mut steps = Vec::new();
    for run in ["1b8a", "20261107", "9f"] {
        let output = on_client(&scratch, &client, &["wait", run])
            .output()
            .unwrap();
        steps.push(report(
            format!("$ pumpkin wait {run}"),
            &output,
            &scratch.0,
            false,
        ));
    }
    steps.push(calls(&scratch));
    check(
        &scratch,
        &steps.join("\n"),
        &[(&hex, "<this build>")],
        &expect![[r#"
            $ pumpkin wait 1b8a
            exit status: Some(1)
            pumpkin: run 20261107-120000-1b8a0001's job on controller `alice` ended before the run started, as its log says above: once that is fixed, run the task again as it was sent; to see that log again: pumpkin wait 20261107-120000-1b8a0001
            $ pumpkin wait 20261107
            exit status: Some(1)
            pumpkin: `20261107` matches 2 runs: name one by more of its id
              20261107-120000-1b8a0001
              20261107-130000-2c9b0002
            $ pumpkin wait 9f
            exit status: Some(1)
            pumpkin: no run on controller `alice` matches `9f`: name an id, a unique prefix of an id or of its random digits, or a record's path
            the calls: serve=.pumpkin/serve/<this build>/pumpkin, serve=.pumpkin/serve/<this build>/pumpkin, serve=.pumpkin/serve/<this build>/pumpkin"#]],
    );
}

// What `cmd` of a client said, as `$ pumpkin <args>` shows it.
fn ran_by(scratch: &Scratch, mut cmd: Command, args: &[&str]) -> String {
    let output = cmd.output().unwrap();
    report(
        format!("$ pumpkin {}", args.join(" ")),
        &output,
        &scratch.0,
        false,
    )
}

#[test]
fn members_attributed() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    let member = fs::read_to_string(scratch.0.join(format!("home/jobs/{id}/member"))).unwrap();
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    let unit = argv.iter().rposition(|line| *line == "--user").unwrap();
    // Ended before its run started, so that its row's time is no clock's.
    fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    let ls = ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &["ls"]),
        &["ls"],
    );
    let lines = [
        format!("the job's member: {member:?}"),
        format!("systemd-run {}", argv[unit..].join(" ")),
        ls,
    ];
    let (install, tree) = (key(&client_bin(&scratch)), last_tree(&scratch));
    let shown = [
        (id.as_str(), "<id>"),
        (&install, "<install>"),
        (&tree, "<tree>"),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &shown,
        &expect![[r#"
        the job's member: "alice\n"
        systemd-run --user --quiet --expand-environment=no --unit=pumpkin-job-<id> --working-directory=<scratch>/home/trees/repo/<id> --property=ExitType=cgroup --property=ExecStopPost=/usr/bin/rm --recursive --force <scratch>/bin/../run/pumpkin/<id> --property=StandardOutput=append:<scratch>/home/jobs/<id>/out --property=StandardError=append:<scratch>/home/jobs/<id>/log -- <scratch>/home/.pumpkin/<install>/bin/pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <tree> -- run -- stand-in/task.star
        $ pumpkin ls
        exit status: Some(0)
        pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
        printed:
          <id> alice unstarted         -                  -  -: its job ended before the run started: to see why: pumpkin wait <id>
          1 run: 1 unstarted"#]],
    );
}

#[test]
fn members_share_uploads() {
    let scratch = Scratch::new();
    let (_, _, ran) = started_job(&scratch);
    let _ = calls(&scratch);
    let bob = set_up_member(&scratch, "bob");
    let (id, by_bob) = start_in(&scratch, "repo-bob", &bob);
    let member = fs::read_to_string(scratch.0.join(format!("home/jobs/{id}/member"))).unwrap();
    let lines = [
        ran,
        by_bob,
        send_calls(&scratch),
        format!("the job's member: {member:?}"),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks all 6 of the task's files: sending 2.9 KiB
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        $ HOME=<scratch>/client-bob PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` holds all 6 of the task's files
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: serve
        the job's member: "bob\n""#]],
    );
}

// Records each stop of a unit, as `systemctl` gets it.
const SYSTEMCTL_NOTED: &str = "#!/bin/sh\necho \"$*\" >> \"${0%/*}/units\"\n";

#[test]
fn cancel_scoped() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let (_, random) = id.rsplit_once('-').unwrap();
    let prefix = &random[..4];
    let mut lines = Vec::new();
    for args in [&["cancel", prefix][..], &["cancel", "--any-member", prefix]] {
        let cancel = on_repo(&scratch, "repo-bob", &bob, args);
        lines.push(ran_by(&scratch, cancel, args));
    }
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>"), (prefix, "<prefix>")],
        &expect![[r#"
            $ pumpkin cancel <prefix>
            exit status: Some(1)
            pumpkin: run <id> on controller `alice` is alice's: to stop it: pumpkin cancel --any-member <id>
            $ pumpkin cancel --any-member <prefix>
            exit status: Some(0)
            pumpkin: run <id>'s job on controller `alice` is stopped, before the run started
            --user stop pumpkin-job-<id>"#]],
    );
}

#[test]
fn cancel_full_id_refused() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let args = ["cancel", &id];
    let by_bob = ran_by(&scratch, on_repo(&scratch, "repo-bob", &bob, &args), &args);
    // As a build of Bob's that skips its own check asks it.
    let cancel = json!({ "type": "cancel", "id": id, "force": false, "any_member": false, "modal_bytes": null });
    let mut asked = Vec::new();
    schema::frame::write(&mut asked, &cancel, &[]).unwrap();
    let served = serve_fed_for(&scratch, &["--member", "bob"], &asked);
    let units = scratch.0.join("bin/units").exists();
    let lines = [by_bob, served, format!("a unit stopped: {units}")];
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
        $ pumpkin cancel <id>
        exit status: Some(1)
        pumpkin: run <id> on controller `alice` is alice's: to stop it: pumpkin cancel --any-member <id>
        exit status: 0
          {"type":"serving"}, and a body of 0 bytes
          {"message":"run <id> is alice's: to stop it: pumpkin cancel --any-member <id>","type":"refused"}, and a body of 0 bytes
        a unit stopped: false"#]],
    );
}

#[test]
fn unattributed_jobs() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    // As a build from before members leaves a job: no `member` beside its log. Its digits sort
    // it after the other job, which may share its second.
    let older = id_ago(0, "ffffffff");
    scratch.write(&format!("home/jobs/{older}/log"), "");
    let mut lines = vec![ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &["ls"]),
        &["ls"],
    )];
    fs::create_dir_all(scratch.0.join(format!("run/pumpkin/{older}"))).unwrap();
    let args = ["cancel", "ffff"];
    lines.push(ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &args),
        &args,
    ));
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>"), (&older, "<older>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            printed:
              <id> alice unstarted         -                  -  -: its job ended before the run started: to see why: pumpkin wait <id>
              <older> -     unstarted         -                  -  -: its job ended before the run started: to see why: pumpkin wait <older>
              2 runs: 2 unstarted
            $ pumpkin cancel ffff
            exit status: Some(0)
            pumpkin: run <older>'s job on controller `alice` is stopped, before the run started
            show --property=ActiveState --property=ExecMainStatus --property=ExecMainExitTimestamp pumpkin-boot.service
            --user stop pumpkin-job-<older>"#]],
    );
}

// Jobs yet to start a run, each with the member that `member` names, or none, and its tmpfs
// directory, so that a cancel stops its unit.
fn waiting_jobs(scratch: &Scratch, jobs: &[(&str, Option<&str>)]) {
    for (id, member) in jobs {
        scratch.write(&format!("home/jobs/{id}/log"), "");
        if let Some(member) = member {
            scratch.write(&format!("home/jobs/{id}/member"), member);
        }
        fs::create_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    }
}

#[test]
fn cancel_prefers_own_run() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let (alices, bobs) = (id_ago(0, "0b000001"), id_ago(0, "0b000002"));
    waiting_jobs(
        &scratch,
        &[(&alices, Some("alice\n")), (&bobs, Some("bob\n"))],
    );
    let mut lines = Vec::new();
    for args in [&["cancel", "0b00"][..], &["cancel", "--any-member", "0b00"]] {
        let cancel = on_repo(&scratch, "repo-bob", &bob, args);
        lines.push(ran_by(&scratch, cancel, args));
    }
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&alices, "<alice's>"), (&bobs, "<bob's>")],
        &expect![[r#"
            $ pumpkin cancel 0b00
            exit status: Some(0)
            pumpkin: run <bob's>'s job on controller `alice` is stopped, before the run started
            $ pumpkin cancel --any-member 0b00
            exit status: Some(1)
            pumpkin: `0b00` matches 2 runs: name one by more of its id
              <alice's>
              <bob's>
            --user stop pumpkin-job-<bob's>"#]],
    );
}

#[test]
fn member_unread() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let (unread, other) = (id_ago(0, "0c000001"), id_ago(0, "0c000002"));
    waiting_jobs(&scratch, &[(&unread, Some("Bad!\n")), (&other, None)]);
    // Ended before its run started, so that its row's time is no clock's.
    fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{other}"))).unwrap();
    let mut lines = vec![ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &["ls"]),
        &["ls"],
    )];
    let args = ["cancel", &unread];
    lines.push(ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &args),
        &args,
    ));
    let cancel = json!({ "type": "cancel", "id": unread, "force": false, "any_member": false, "modal_bytes": null });
    let mut asked = Vec::new();
    schema::frame::write(&mut asked, &cancel, &[]).unwrap();
    lines.push(serve_fed_for(&scratch, &["--member", "bob"], &asked));
    let args = ["cancel", "--any-member", &unread];
    lines.push(ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &args),
        &args,
    ));
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&unread, "<unread>"), (&other, "<other>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: run <unread>: <scratch>/home/jobs/<unread>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            pumpkin: 1 run can't be listed, as said above
            printed:
              <other> unstarted         -                  -  -: its job ended before the run started: to see why: pumpkin wait <other>
              1 run: 1 unstarted
            $ pumpkin cancel <unread>
            exit status: Some(1)
            pumpkin: run <unread> on controller `alice` names a member that can't be read: <scratch>/home/jobs/<unread>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter: to stop it: pumpkin cancel --any-member <unread>
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"run <unread> names a member that can't be read: <scratch>/home/jobs/<unread>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter: to stop it: pumpkin cancel --any-member <unread>","type":"refused"}, and a body of 0 bytes
            $ pumpkin cancel --any-member <unread>
            exit status: Some(0)
            pumpkin: run <unread>'s job on controller `alice` is stopped, before the run started
            show --property=ActiveState --property=ExecMainStatus --property=ExecMainExitTimestamp pumpkin-boot.service
            --user stop pumpkin-job-<unread>"#]],
    );
}

#[test]
fn serve_refuses_member() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let bad = set_up_member(&scratch, "bob");
    scratch.write("client-bob/.member", "Bad!\n");
    let ls = ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bad, &["ls"]),
        &["ls"],
    );
    // What follows is the repair's, which finds no controller, as no `aws` answers.
    let (ls, _) = ls.split_once("; SSH at its last private address").unwrap();
    check(
        &scratch,
        ls,
        &[],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            error: invalid value 'Bad!' for '--member <member>': `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
            pumpkin: to read how to use it: pumpkin controller serve --help
            pumpkin: controller `alice` didn't answer: ended before its hello: opening it again now, attempt 2 of 8
            error: invalid value 'Bad!' for '--member <member>': `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
            pumpkin: to read how to use it: pumpkin controller serve --help
            pumpkin: controller `alice` didn't answer: ended before its hello"#]],
    );
}

fn serve_process(scratch: &Scratch) -> Child {
    serve_for(scratch, &[])
}

// `controller serve` with `args`, such as the `--member` that a forced command gives.
fn serve_for(scratch: &Scratch, args: &[&str]) -> Child {
    let mut serve = scratch.program(PUMPKIN, &[&["controller", "serve"], args].concat());
    serve.env("HOME", scratch.0.join("home"));
    serve.env("XDG_RUNTIME_DIR", scratch.0.join("run"));
    serve
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    serve.spawn().unwrap()
}

/// What `controller serve` does with `input` on its standard input.
fn serve_fed(scratch: &Scratch, input: &[u8]) -> String {
    serve_fed_for(scratch, &[], input)
}

/// As [`serve_fed`], of `controller serve` with `args`.
fn serve_fed_for(scratch: &Scratch, args: &[&str], input: &[u8]) -> String {
    let mut child = serve_for(scratch, args);
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input).unwrap();
    let output = child.wait_with_output().unwrap();
    let mut lines = vec![output.status.to_string()];
    let mut out = &output.stdout[..];
    while let Some((message, body)) = schema::frame::read::<serde_json::Value>(&mut out).unwrap() {
        lines.push(format!("  {message}, and a body of {} bytes", body.len()));
    }
    let said = String::from_utf8_lossy(&output.stderr);
    lines.extend(said.lines().map(|line| format!("  said: {line}")));
    lines.join("\n")
}

#[test]
fn grouped_upload_limits() {
    let scratch = Scratch::new();
    let mut steps = Vec::new();
    let puts = |count, from, size_bytes| {
        let blob =
            json!({"digest":format!("sha256:{EMPTY_TREE}"), "from":from, "size_bytes":size_bytes});
        json!({"type":"puts", "blobs": vec![blob; count]})
    };
    for (what, request) in [
        ("too many blobs in a group:", puts(9, 0, 0)),
        ("a large blob in a group:", puts(2, 0, 32769)),
        ("a resumed blob in a group:", puts(2, 1, 2)),
    ] {
        let mut head = Vec::new();
        schema::frame::write(&mut head, &request, &[]).unwrap();
        steps.push(what.to_owned());
        steps.push(serve_fed(&scratch, &head));
    }
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            too many blobs in a group:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a group holds 2 to 8 whole blobs of at most 32768 bytes each: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a large blob in a group:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a group holds 2 to 8 whole blobs of at most 32768 bytes each: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a resumed blob in a group:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a group holds 2 to 8 whole blobs of at most 32768 bytes each: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes"#]],
    );
}

#[test]
fn serve_refusals() {
    let scratch = Scratch::new();
    let mut steps = vec!["no request:".to_owned(), serve_fed(&scratch, b"")];
    let mut past = (u32::try_from(schema::frame::MAX_MESSAGE_BYTES).unwrap() + 1)
        .to_be_bytes()
        .to_vec();
    past.extend(b"{}");
    steps.push("a frame past the cap:".to_owned());
    steps.push(serve_fed(&scratch, &past));
    let mut other = Vec::new();
    schema::frame::write(&mut other, &json!({ "type": "absent_request" }), &[]).unwrap();
    steps.push("a message that is no request:".to_owned());
    steps.push(serve_fed(&scratch, &other));
    for (what, bytes) in [
        ("an incomplete request body:", 1),
        ("a request body past the cap:", 16 * 1024 * 1024 + 1),
    ] {
        let mut head = Vec::new();
        schema::frame::write(&mut head, &start_head(bytes, 0), &[]).unwrap();
        steps.push(what.to_owned());
        steps.push(serve_fed(&scratch, &head));
    }
    let mut job = start_job("once", 1);
    job["key_bytes"] = json!([usize::MAX]);
    let body = serde_json::to_vec(&job).unwrap();
    let mut bad_lengths = Vec::new();
    schema::frame::write(&mut bad_lengths, &start_head(body.len(), 0), &[]).unwrap();
    schema::frame::write(&mut bad_lengths, &(), &body).unwrap();
    steps.push("credential lengths that overflow:".to_owned());
    steps.push(serve_fed(&scratch, &bad_lengths));
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            no request:
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
            a frame past the cap:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a frame's message is 65537 bytes: it holds at most 65536: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a message that is no request:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"unknown variant `absent_request`, expected one of `copy`, `follow`, `health`, `jobs`, `list`, `view`, `start`, `cancel`, `reap`, `lacks`, `put`, `puts` at line 1 column 24: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            an incomplete request body:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"unexpected end of file: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a request body past the cap:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a request's body holds at most 16 MiB: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            credential lengths that overflow:
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"the job gives a different length for its credentials","type":"refused"}, and a body of 0 bytes"#]],
    );
}

#[test]
fn serve_cut_and_malformed_uploads() {
    let scratch = Scratch::new();
    let mut upload = Vec::new();
    let put = json!({"type":"put", "digest":format!("sha256:{}", sha256(b"x")), "from":0, "size_bytes":1});
    schema::frame::write(&mut upload, &put, &[]).unwrap();
    schema::frame::write(&mut upload, &(), b"xx").unwrap();
    let cut = serve_fed(&scratch, &upload[..upload.len() - 1]);
    let oversized = serve_fed(&scratch, &upload);
    check(
        &scratch,
        &format!(
            "a cut within an upload part:\n{cut}\nan upload part past the declared size:\n{oversized}"
        ),
        &[],
        &expect![[r#"
            a cut within an upload part:
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
            an upload part past the declared size:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a part of the request's body holds 2 bytes: expected 1 to 1: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes"#]],
    );
}

fn record_files(dir: &Path) -> Vec<String> {
    let files = walked(dir, &[]).into_iter().map(|path| {
        let metadata = fs::symlink_metadata(dir.join(&path)).unwrap();
        assert!(metadata.is_file(), "{}", path.display());
        let bytes = fs::read(dir.join(&path)).unwrap();
        format!(
            "{:o} {} {} {} {}",
            metadata.mode() & 0o7777,
            metadata.len(),
            metadata.mtime(),
            sha256(&bytes),
            path.display().to_string().escape_debug()
        )
    });
    files.collect()
}

fn inodes(dir: &Path) -> Vec<(PathBuf, u64)> {
    let inodes = walked(dir, &[]).into_iter().map(|path| {
        let inode = fs::symlink_metadata(dir.join(&path)).unwrap().ino();
        (path, inode)
    });
    inodes.collect()
}

fn copied_record(
    scratch: &Scratch,
    client: &[(String, String); 2],
    args: &[&str],
    copy: &Path,
) -> Vec<String> {
    let before = if copy.exists() {
        inodes(copy)
    } else {
        Vec::new()
    };
    let waited = on_client(scratch, client, args).output().unwrap();
    let mut lines = vec![format!("{}: {}", args[0], waited.status)];
    let said = String::from_utf8_lossy(&waited.stderr);
    let said = said.lines().filter(|line| {
        let ending = line.starts_with("pumpkin: copying run") || line.contains("its record is");
        line.contains("connection") || line.contains("record") && !ending
    });
    lines.extend(said.map(|line| format!("  {line}")));
    lines.push(format!("  {}", calls(scratch)));
    let written = inodes(copy)
        .into_iter()
        .filter(|file| !before.contains(file));
    let written: Vec<String> = written
        .map(|(path, _)| path.display().to_string().escape_debug().to_string())
        .collect();
    lines.push(format!("  written: {}", written.join(", ")));
    lines
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn record_copy() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    let id = id_ago(0, "0d000001");
    ended_job(&scratch, &id, "built\n");
    let record = scratch.0.join(format!("home/.pumpkin/runs/{id}"));
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/{id}"));
    let at = |path: &str| format!("home/.pumpkin/runs/{id}/{path}");
    scratch.write(&at("log.jsonl"), "{\"a\": 1}\n");
    scratch.write(&at("model/a b"), "a name with a space\n");
    scratch.write(&at("model/a\nb"), "a name with a newline\n");
    scratch.write(&at("model/marked"), "set-id and sticky\n");
    scratch.write(&at("jobs/0/out"), "out\n");
    scratch.write(&at("jobs/0/secret"), "only its user's\n");
    fs::create_dir(record.join("jobs/2")).unwrap();
    // Modes of the record's own, whatever this machine's umask.
    for path in walked(&record, &[]) {
        fs::set_permissions(record.join(path), Permissions::from_mode(0o644)).unwrap();
    }
    fs::set_permissions(record.join("jobs/0/secret"), Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(record.join("model/marked"), Permissions::from_mode(0o7755)).unwrap();
    // Left by copies of the run: one killed, one under way, and two of no copy's name.
    let runs = scratch.0.join("repo/.pumpkin/runs");
    for left in ["a1B2c3", "locked", "a1B2c3d", "a1-2c3"] {
        scratch.write(&format!("repo/.pumpkin/runs/.{id}.{left}/sent/x"), "left\n");
    }
    let locked = File::open(runs.join(format!(".{id}.locked"))).unwrap();
    locked.lock().unwrap();
    let _ = calls(&scratch);
    let mut lines = Vec::new();
    for step in ["unread", "whole", "unchanged", "changed"] {
        if step == "whole" {
            let secret = Permissions::from_mode(0o600);
            fs::set_permissions(record.join("jobs/0/secret"), secret).unwrap();
        }
        if step == "changed" {
            // The same size, but an older time, as a file written anew in place.
            scratch.write(&at("log.jsonl"), "{\"a\": 2}\n");
            let log = File::options().write(true).open(record.join("log.jsonl"));
            let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
            log.unwrap().set_modified(old).unwrap();
            scratch.write(&at("model/added"), "new\n");
            let added = Permissions::from_mode(0o644);
            fs::set_permissions(record.join("model/added"), added).unwrap();
            fs::remove_file(record.join("jobs/0/out")).unwrap();
        }
        lines.push(format!("{step}:"));
        let command = if ["unchanged", "changed"].contains(&step) {
            "cancel"
        } else {
            "wait"
        };
        lines.extend(copied_record(&scratch, &client, &[command, &id], &copy));
        if step == "whole" {
            let marked = fs::metadata(copy.join("model/marked")).unwrap().mode();
            lines.push(format!("  model/marked: {:o}", marked & 0o7777));
            let jobs = fs::read_dir(copy.join("jobs")).unwrap();
            let mut jobs: Vec<String> = jobs
                .map(|entry| entry.unwrap().file_name().display().to_string())
                .collect();
            jobs.sort();
            lines.push(format!("  jobs: {}", jobs.join(", ")));
            let marked = Permissions::from_mode(0o755);
            fs::set_permissions(record.join("model/marked"), marked).unwrap();
        }
    }
    let mut kept = record_files(&copy);
    kept.retain(|file| !record_files(&record).contains(file));
    let kept = kept.iter().map(|file| file.splitn(5, ' ').nth(4).unwrap());
    lines.push(format!(
        "only here: {}",
        kept.collect::<Vec<_>>().join(", ")
    ));
    // A cut in the second file leaves only the first complete.
    let big = |seed: u8| -> Vec<u8> { (0..100_000u32).map(|i| seed ^ (i % 251) as u8).collect() };
    fs::write(record.join("model/big-1"), big(1)).unwrap();
    fs::write(record.join("model/big-2"), big(2)).unwrap();
    fs::write(record.join("model/big-3"), big(3)).unwrap();
    for big in ["big-1", "big-2", "big-3"] {
        let path = record.join("model").join(big);
        fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
    }
    let cut = scratch.0.join("bin/cut-out");
    let before_copy = format!("echo 150000 > '{}'\n", cut.display());
    scratch.write("bin/before-1", &before_copy);
    // Withhold driver.pid before the retry to observe exactly what the cut placed.
    scratch.write(
        "bin/before-2",
        &format!(
            "mv '{}' '{}'\n",
            record.join("driver.pid").display(),
            record.join(".held-pid").display()
        ),
    );
    lines.push("cut, then refused:".to_owned());
    lines.extend(copied_record(&scratch, &client, &["cancel", &id], &copy));
    fs::rename(record.join(".held-pid"), record.join("driver.pid")).unwrap();
    scratch.write("bin/before-1", &before_copy);
    lines.push("cut:".to_owned());
    lines.extend(copied_record(&scratch, &client, &["cancel", &id], &copy));
    let mut here = record_files(&copy);
    here.retain(|file| !file.ends_with(" jobs/0/out"));
    assert_eq!(here, record_files(&record));
    lines.push("the copy (but `jobs/0/out`) is the record's:".to_owned());
    lines.extend(here.iter().map(|file| {
        let mut words = file.splitn(5, ' ');
        let mode = words.next().unwrap();
        format!("  {} {mode}", words.nth(3).unwrap())
    }));
    let runs = fs::read_dir(runs).unwrap();
    let mut runs: Vec<String> = runs
        .map(|entry| entry.unwrap().file_name().display().to_string())
        .collect();
    runs.sort();
    lines.push(format!("runs: {}", runs.join(", ")));
    check(
        &scratch,
        &lines.join("\n"),
        &[(id.as_str(), "<id>"), (&hex, "<this build>")],
        &expect![[r#"
            unread:
            wait: exit status: 1
              pumpkin: controller `alice` copying run <id>: <scratch>/home/.pumpkin/runs/<id>/jobs/0/secret: Permission denied (os error 13): the copy keeps the files that came whole. Once the controller can read the record, copy it again
              the calls: serve=.pumpkin/serve/<this build>/pumpkin
              written: driver.pid, jobs/0/out, log.jsonl, model/a\nb, model/a b, model/marked, outcome.json
            whole:
            wait: exit status: 0
              the calls: serve=.pumpkin/serve/<this build>/pumpkin
              written: jobs/0/secret, record.json
              model/marked: 755
              jobs: 0, 2
            unchanged:
            cancel: exit status: 0
              pumpkin: stopped job <id> on controller `alice`; copied its record to <scratch>/repo/.pumpkin/runs/<id>: read it with the pumpkin that made it
              the calls: serve=.pumpkin/serve/<this build>/pumpkin
              written: 
            changed:
            cancel: exit status: 0
              pumpkin: stopped job <id> on controller `alice`; copied its record to <scratch>/repo/.pumpkin/runs/<id>: read it with the pumpkin that made it
              the calls: serve=.pumpkin/serve/<this build>/pumpkin
              written: log.jsonl, model/added
            only here: jobs/0/out
            cut, then refused:
            cancel: exit status: 1
              pumpkin: controller `alice` didn't answer: ended within the answer: copying run <id>'s record again now, attempt 2 of 7
              the calls: serve=.pumpkin/serve/<this build>/pumpkin, serve=.pumpkin/serve/<this build>/pumpkin
              written: model/big-1
            cut:
            cancel: exit status: 0
              pumpkin: controller `alice` didn't answer: ended within the answer: copying run <id>'s record again now, attempt 2 of 7
              pumpkin: stopped job <id> on controller `alice`; copied its record to <scratch>/repo/.pumpkin/runs/<id>: read it with the pumpkin that made it
              the calls: serve=.pumpkin/serve/<this build>/pumpkin, serve=.pumpkin/serve/<this build>/pumpkin
              written: model/big-2, model/big-3
            the copy (but `jobs/0/out`) is the record's:
              driver.pid 644
              jobs/0/secret 600
              log.jsonl 644
              model/a\nb 644
              model/a b 644
              model/added 644
              model/big-1 755
              model/big-2 755
              model/big-3 755
              model/marked 755
              outcome.json 644
              record.json 644
            runs: .<id>.a1-2c3, .<id>.a1B2c3d, .<id>.locked, <id>"#]],
    );
}

#[test]
fn copy_with_unreadable_directories() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    let id = id_ago(0, "0d000003");
    ended_job(&scratch, &id, "built\n");
    symlink(
        scratch.0.join(format!("home/.pumpkin/serve/{hex}/pumpkin")),
        scratch.0.join(format!("home/jobs/{id}/pumpkin")),
    )
    .unwrap();
    let record = scratch.0.join(format!("home/.pumpkin/runs/{id}"));
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/{id}"));
    let remote = record.join("private");
    let local = copy.join("local");
    fs::create_dir(&remote).unwrap();
    fs::write(remote.join("output"), "whole").unwrap();
    fs::create_dir_all(&local).unwrap();
    fs::set_permissions(&local, Permissions::from_mode(0o000)).unwrap();
    let mut lines = Vec::new();
    for mode in [0o000, 0o700] {
        fs::set_permissions(&remote, Permissions::from_mode(mode)).unwrap();
        let output = on_client(&scratch, &client, &["cancel", &id])
            .output()
            .unwrap();
        lines.push(output.status.to_string());
        lines.extend(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter(|line| line.contains("Permission denied"))
                .map(str::to_owned),
        );
        lines.push(format!(
            "outcome.json: {}, private/output: {}, record.json: {}",
            copy.join("outcome.json").exists(),
            copy.join("private/output").exists(),
            copy.join("record.json").exists()
        ));
    }
    fs::set_permissions(&local, Permissions::from_mode(0o700)).unwrap();
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 1
            pumpkin: controller `alice` copying run <id>: <scratch>/home/.pumpkin/runs/<id>/private: Permission denied (os error 13): the copy keeps the files that came whole. Once the controller can read the record, copy it again
            outcome.json: true, private/output: false, record.json: false
            exit status: 0
            outcome.json: true, private/output: true, record.json: true"#]],
    );
}

#[test]
fn copy_refuses_a_live_run_or_linked_directory() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    served(&scratch);
    let id = id_ago(0, "0d000002");
    ended_job(&scratch, &id, "built\n");
    let record = scratch.0.join(format!("home/.pumpkin/runs/{id}"));
    let mut request = Vec::new();
    schema::frame::write(
        &mut request,
        &json!({"type":"copy","id":id,"held_bytes":0}),
        &[],
    )
    .unwrap();
    let driver = File::open(record.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let mut lines = vec![serve_fed(&scratch, &request)];
    drop(driver);
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/{id}"));
    fs::create_dir_all(&copy).unwrap();
    fs::create_dir(scratch.0.join("outside")).unwrap();
    symlink(scratch.0.join("outside"), copy.join("model")).unwrap();
    fs::create_dir(record.join("model")).unwrap();
    fs::write(record.join("model/input"), "private").unwrap();
    let output = on_client(&scratch, &client, &["cancel", &id])
        .output()
        .unwrap();
    lines.push(output.status.to_string());
    lines.extend(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|s| s.contains("expected a directory"))
            .map(str::to_owned),
    );
    lines.push(format!(
        "files outside the copy: {}",
        fs::read_dir(scratch.0.join("outside")).unwrap().count()
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"run <id> is still writing its record: wait until it ends and copy it again","type":"refused"}, and a body of 0 bytes
            exit status: 1
            pumpkin: linking to controller `alice` failed: <scratch>/repo/.pumpkin/runs/<id>/model is not a directory: expected a directory inside the copy
            files outside the copy: 0"#]],
    );
}

fn viewed(scratch: &Scratch) -> ([(String, String); 2], File, [&'static str; 2]) {
    let client = set_up_client(scratch);
    connect(scratch, &client);
    let (ended, live) = ("20261110-090000-0000000a", "20261110-110000-0000000c");
    let places = lease_places(Prices::Both);
    let outcome = returned(49.36);
    let dir = write_run(
        scratch,
        ended,
        asked(ended, 30),
        &whole_log(),
        Some(outcome),
        &places,
    );
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("log.jsonl"))
        .unwrap();
    // More than a read of the log holds.
    for index in 0..3000 {
        let padding = format!("{index:0>85}");
        std::io::Write::write_all(
            &mut log,
            format!("{{\"padding\":\"{padding}\"}}\n").as_bytes(),
        )
        .unwrap();
    }
    let dir = write_run(
        scratch,
        live,
        asked(live, 30),
        &cut(&whole_log(), 130),
        None,
        &places,
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    fs::create_dir_all(scratch.0.join("home/.pumpkin")).unwrap();
    fs::rename(scratch.0.join("runs"), scratch.0.join("home/.pumpkin/runs")).unwrap();
    (client, driver, [ended, live])
}

fn got(served: &Served, path: &str, fields: &[&str]) -> String {
    let (status, body) = served.ask("GET", path, &[]);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    if let Some(error) = body.get("error") {
        return format!("GET {path}\n{status}\n{error}");
    }
    let fields: Vec<String> = (fields.iter())
        .map(|field| format!("{field}: {}", body[field]))
        .collect();
    format!("GET {path}\n{status}\n{}", fields.join(", "))
}

#[test]
fn views_the_controllers_runs_over_a_reopened_connection() {
    let scratch = Scratch::new();
    let (client, driver, [ended, live]) = viewed(&scratch);
    served(&scratch);
    // The ended run's job is a member's; the live one's names none.
    scratch.write(&format!("home/jobs/{ended}/member"), "alice\n");
    scratch.write("bin/cut-out", "1000");
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let mut steps = vec![served.said.trim_end().to_owned()];
    let window = format!("/api/runs/{ended}/log?at=0");
    steps.push(got(&served, &window, &["next", "end"]));
    assert!(!scratch.0.join("bin/cut-out").exists());
    let (status, listed) = served.ask("GET", "/api/runs", &[]);
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let rows: Vec<String> = (listed["runs"].as_array().unwrap().iter())
        .map(|row| format!("{} {} {}", row["id"], row["state"], row["member"]))
        .collect();
    steps.push(format!("GET /api/runs\n{status}\n{}", rows.join("\n")));
    steps.push(got(
        &served,
        &format!("/api/runs/{ended}"),
        &["state", "end"],
    ));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(ended, "<ended>"), (live, "<live>")],
        &expect![[r#"
            pumpkin: serving controller `alice`'s runs to this machine alone, until stopped, at
                http://127.0.0.1:<port>/?t=<token>
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            next: 310332, end: 310332
            GET /api/runs
            HTTP/1.1 200 OK
            "<live>" "work" null
            "<ended>" "finished" "alice"
            GET /api/runs/<ended>
            HTTP/1.1 200 OK
            state: "finished", end: 310332"#]],
    );
}

#[test]
fn view_answers_502_once_the_reopened_connection_is_cut_too_then_recovers() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    served(&scratch);
    let calls = fs::read_to_string(scratch.0.join("bin/scripts")).unwrap_or_default();
    let cut_out = scratch.0.join("bin/cut-out");
    scratch.write("bin/cut-out", "1000");
    // So that the connection opened anew is cut too.
    let again = format!("echo 1000 > {}\n", cut_out.display());
    scratch.write(&format!("bin/before-{}", calls.lines().count() + 2), &again);
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut steps = vec![got(&served, &window, &["next", "end"])];
    assert!(!cut_out.exists());
    steps.push(got(&served, &window, &["next", "end"]));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(ended, "<ended>")],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "controller `alice` didn't answer: ended within the answer's body"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            next: 310332, end: 310332"#]],
    );
}

#[test]
fn view_answers_502_for_a_reply_it_cant_read() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    let hex = served(&scratch);
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut hello = Vec::new();
    schema::frame::write(&mut hello, &json!({ "type": "serving" }), &[]).unwrap();
    fs::write(scratch.0.join("hello"), hello).unwrap();
    // Unlinked first, as it is a link to this build. It says its hello, takes the request's
    // length, which is all that the link needs of it, and sends the reply.
    let exe = format!("home/.pumpkin/serve/{hex}/pumpkin");
    fs::remove_file(scratch.0.join(&exe)).unwrap();
    let script = format!(
        "#!/bin/sh\ncat {0}/hello\nhead --bytes=4 > /dev/null\ncat {0}/reply\n",
        scratch.0.display()
    );
    scratch.write_executable(&exe, script.as_bytes());
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let mut past = (u32::try_from(schema::frame::MAX_MESSAGE_BYTES).unwrap() + 1)
        .to_be_bytes()
        .to_vec();
    past.extend(b"{}");
    let mut body_past = Vec::new();
    let answer = json!({
        "type": "viewed", "status": "200 OK", "content_type": "text/plain", "deflated": false,
        "body_bytes": 256 * 1024 * 1024 + 1,
    });
    schema::frame::write(&mut body_past, &answer, &[]).unwrap();
    let mut other = Vec::new();
    schema::frame::write(&mut other, &json!({ "type": "reap" }), &[]).unwrap();
    let mut steps = Vec::new();
    for reply in [past, body_past, other] {
        fs::write(scratch.0.join("reply"), reply).unwrap();
        steps.push(got(&served, &window, &[]));
    }
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(ended, "<ended>")],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "linking to controller `alice` failed: the answer is no frame that `pumpkin controller serve` sends: a frame's message is 65537 bytes: it holds at most 65536: expected its frames alone on the standard output there, which a shell's startup file may write to"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "linking to controller `alice` failed: its answer's body is of 268435457 bytes: expected 256 MiB at most"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "linking to controller `alice` failed: the answer is no frame that `pumpkin controller serve` sends: it is neither an answer to the request nor a refusal: expected its frames alone on the standard output there, which a shell's startup file may write to""#]],
    );
}

#[test]
fn view_asks_ec2_once_no_link_opens() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    let hex = served(&scratch);
    scratch.write("bin/cut-out", "1000");
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    // A `serve` that ends before its hello, so that no link opens; then EC2 is asked, which the
    // stand-in `aws` refuses.
    let exe = scratch.0.join(format!("home/.pumpkin/serve/{hex}/pumpkin"));
    let aside = scratch.0.join("aside");
    fs::rename(&exe, &aside).unwrap();
    scratch.write_executable(
        &format!("home/.pumpkin/serve/{hex}/pumpkin"),
        b"#!/bin/sh\n",
    );
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut steps = vec![got(&served, &window, &["end"])];
    fs::rename(&aside, &exe).unwrap();
    steps.push(got(&served, &window, &["end"]));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(ended, "<ended>")],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "controller `alice` didn't answer: ended before its hello; asking EC2 whether it runs failed too: `aws --version` failed with exit status: 1 and wrote nothing to standard error"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            end: 310332"#]],
    );
}

#[test]
fn view_reopens_between_requests() {
    let scratch = Scratch::new();
    let (client, driver, [id, _]) = viewed(&scratch);
    served(&scratch);
    scratch.write("bin/link-pid", "");
    let page = Served::of(on_client(&scratch, &client, &["view"]));
    let path = format!("/api/runs/{id}");
    let first = got(&page, &path, &["state"]);
    let pid = fs::read_to_string(scratch.0.join("bin/link-pid")).unwrap();
    let pid = pid.trim().parse::<i32>().unwrap();
    // A previously answered connection ends between requests, as when serve's idle time ends.
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    poll(|| (!super::alive(i64::from(pid))).then_some(()));
    let second = got(&page, &path, &["state"]);
    assert_eq!(first, second);
    let connections = fs::read_to_string(scratch.0.join("bin/scripts"))
        .unwrap()
        .lines()
        .count();
    drop(driver);
    check(
        &scratch,
        &format!("{second}\nSSH connections: {connections}"),
        &[(id, "<id>")],
        &expect![[r#"
            GET /api/runs/<id>
            HTTP/1.1 200 OK
            state: "finished"
            SSH connections: 2"#]],
    );
}

#[test]
fn view_installs_its_serve_there() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut steps = Vec::new();
    // The first request installs it and then asks, and the second asks over the same link.
    for _ in 0..2 {
        steps.push(got(&served, &window, &["end"]));
        steps.push(calls(&scratch));
    }
    let hex = key(&client_bin(&scratch));
    let install = scratch.0.join(format!("home/.pumpkin/serve/{hex}"));
    steps.push(format!("the install: {}", modes(&install)));
    steps.push(files_of(&scratch, &install));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(ended, "<ended>"), (&hex, "<this build>")],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            end: 310332
            the calls: serve=.pumpkin/serve/<this build>/pumpkin, installs=.pumpkin/serve, serve=.pumpkin/serve/<this build>/pumpkin
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            end: 310332
            the calls: none
            the install: buildifier 555, pumpkin 555
            its files:
              pumpkin: the client's pumpkin
              buildifier: the client's buildifier"#]],
    );
}

// Each is refused before EC2 is asked, which the stand-in `aws` doesn't answer.
#[test]
fn view_refuses_a_start_but_from_its_page() {
    let scratch = Scratch::new();
    let (client, driver, _) = viewed(&scratch);
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let page = format!("http://127.0.0.1:{}", served.port);
    let start = "/api/controller/start";
    let mut steps = Vec::new();
    for (method, headers) in [
        ("GET", &[][..]),
        ("POST", &[("Origin", "http://pumpkin.example")]),
        ("POST", &[("Origin", &page), ("X-Pumpkin-Token", "")]),
    ] {
        let (status, body) = served.ask(method, start, headers);
        steps.push(format!("{method} {start} {headers:?}\n{status}\n{body}"));
    }
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(&page, "<page>")],
        &expect![[r#"
            GET /api/controller/start []
            HTTP/1.1 405 Method Not Allowed
            {"error":"the controller's start is sent by `POST`, as the page sends it: start it from the page that `pumpkin view` serves, or run a command that reaches it, which starts it: pumpkin ls"}
            POST /api/controller/start [("Origin", "http://pumpkin.example")]
            HTTP/1.1 403 Forbidden
            {"error":"the controller's start comes from `http://pumpkin.example`, a page of another site: start it from the page that `pumpkin view` serves, or run a command that reaches it, which starts it: pumpkin ls"}
            POST /api/controller/start [("Origin", "<page>"), ("X-Pumpkin-Token", "")]
            HTTP/1.1 401 Unauthorized
            {"error":"the request carries no token in `X-Pumpkin-Token`: give the one in the address that `pumpkin view` printed as it started"}"#]],
    );
}

// The stand-in controller has no job of the run, which is what the cancel says.
#[test]
fn view_cancels_on_the_controller() {
    let scratch = Scratch::new();
    let (client, driver, [_, live]) = viewed(&scratch);
    served(&scratch);
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let origin = format!("http://127.0.0.1:{}", served.port);
    let cancel = format!("/api/runs/{live}/cancel");
    let (status, _) = served.ask("POST", &cancel, &[("Origin", &origin)]);
    let cancelling = format!("/api/runs/{live}/cancelling");
    let said = poll(|| {
        let (_, body) = served.ask("GET", &cancelling, &[]);
        let said: serde_json::Value = serde_json::from_str(&body).unwrap();
        (said["ended"] == true).then_some(said)
    });
    drop(driver);
    check(
        &scratch,
        &format!(
            "POST {cancel}\n{status}\n{}",
            said["said"].as_str().unwrap()
        ),
        &[(live, "<live>")],
        &expect![[r#"
            POST /api/runs/<live>/cancel
            HTTP/1.1 202 Accepted
            pumpkin: no run on controller `alice` matches `<live>`: name an id, a unique prefix of an id or of its random digits, or a record's path"#]],
    );
}
