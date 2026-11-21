#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed test shows its containers' logs"
)]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::DirBuilder;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, thread};

use docker::Docker;
use engine::{BuildContext, ContextName, Engine, Recipe, Security};
use expect_test::{Expect, expect};
use gateway::{Arrival, IncomingModelRequest, Op, Process, Put, Registration, Session, Sockets};
use provider::{
    Create, Created, Creator, Listed, Provider, Reservation, Role, RunTag, SandboxId, Sweep,
    UnixTime,
};
use recipe::Given;
use schema::frame::MAX_MESSAGE_BYTES;
use schema::ir::{AbsolutePath, Argv, Env, EnvVar, Image, Mode, Name, Resources, User};
use schema::sandbox::op::{
    CallReply, CommandEnd, CommandStatus, Failure, FailureCode, JobReply, JobState, Last,
    LeaseRemaining, LeaseReply, LeaseState, LeaseStatusReply, NowReply, OpReply, OpsReply,
    Remaining, Returned, SessionOp, StatusReply,
};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    ExitStatus, Launch, MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT,
    MAX_OP_PAYLOAD_BYTES, MODEL_PORT, OpPayloadLimit, RelativePath, SOCKET_NAME, SocketDial,
    dir_mode, file_mode,
};
use stdx::process::Environment;
use stdx::{Context, Deadline};
use test_support::suite::{Locals, Subject, reserve_with_room};
use test_support::{BUSYBOX, TestDir, docker, hung};

// The directory, in this variable, in which this binary, run again by `hung_create`, makes a create
// through the stand-in `docker` and writes what came of it, to `created`.
const STAND_IN: &str = "PUMPKIN_TEST_STAND_IN";

// The roles that a fixture reserves for, one to a phase, so that a test may start any of them.
const ROLES: [&str; 13] = [
    "confined",
    "first",
    "hung",
    "leaver",
    "missing",
    "model",
    "not_utf8",
    "ops",
    "refused",
    "stand-in",
    "turns",
    "unexecutable",
    "unpulled",
];

struct Fixture {
    sandboxd: PathBuf,
    dir: PathBuf,
    sockets: Sockets,
    run: RunTag,
    other_run: RunTag,
    // Held for the fixture's life: a create takes its share of the run's reservation.
    _held: Reservation,
}

impl Fixture {
    fn new(name: &str, dir: &Path) -> Self {
        let tag = |suffix: &str| format!("pk-{}-{name}{suffix}", std::process::id());
        let tag = |suffix: &str| RunTag::try_from(tag(suffix)).unwrap();
        let run = tag("");
        let phases: Vec<Vec<Role>> = ROLES.iter().map(|&name| vec![role(name)]).collect();
        let held = reserve_with_room(provider(), &run, &phases);
        Self {
            sandboxd: test_support::sandboxd(),
            sockets: Sockets::create(dir.join("sockets")).unwrap(),
            dir: dir.to_owned(),
            run,
            other_run: tag("-other"),
            _held: held,
        }
    }

    fn start(&self, run: &RunTag, role: &str) -> Started {
        self.try_start(run, role, &self.sandboxd, &image(), None, false)
            .unwrap()
    }

    fn try_start(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: &Image,
        ops: Option<OpPayloadLimit>,
        model: bool,
    ) -> Result<Started, String> {
        let (create, dial, registration) =
            self.publish(run, role, sandboxd, image, ops.is_some(), model)?;
        Self::start_created(&create, dial, registration, ops, model)
    }

    fn start_created(
        create: &Create,
        dial: SocketDial,
        registration: Registration,
        ops: Option<OpPayloadLimit>,
        model: bool,
    ) -> Result<Started, String> {
        let run = &create.run;
        let by = startup();
        let Created { id } = provider().create(create, dial)?;
        let (sender, arrivals) = mpsc::channel();
        let accepted = registration.accept(by, ops, model, sender, |arrival| arrival, || false);
        let set_up = accepted
            .map_err(|error| error.to_string())
            .and_then(|mut session| {
                session
                    .set_up(by)
                    .context(format_args!("sandbox {}", id.as_str()))?;
                Ok(session)
            });
        match set_up {
            Ok(session) => Ok(Started {
                id,
                session,
                arrivals,
            }),
            Err(error) => {
                let console = provider().console(run, &id).unwrap();
                provider().delete(run, &[id]).unwrap();
                Err(format!(
                    "{error}; its console ends: {}",
                    console.text.trim()
                ))
            }
        }
    }

    fn publish(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: &Image,
        ops: bool,
        model: bool,
    ) -> Result<(Create, SocketDial, Registration), String> {
        let name = Name::try_from(role.to_owned()).unwrap();
        let (dial, registration) = self.sockets.issue(&name).unwrap();
        let role = self::role(role);
        let image = provider().publish_image(image)?;
        let sandboxd = provider().publish_sandboxd(sandboxd)?;
        let now = UnixTime::of(SystemTime::now()).unwrap();
        let create = Create {
            run: run.clone(),
            name,
            role,
            instance: 0,
            // The provider's one class.
            class: provider().ready().classes.keys().next().unwrap().clone(),
            image,
            sandboxd,
            ops,
            model,
            // Far past the test's end, which comes within minutes.
            hard_end: now.saturating_add(Duration::from_hours(1)),
            by: now.saturating_add(declared_startup()),
        };
        Ok((create, dial, registration))
    }
}

fn role(name: &str) -> Role {
    Role {
        name: Name::try_from(name.to_owned()).unwrap(),
        user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
        resources: serde_json::from_value::<Resources>(serde_json::json!({
            "cpu_millis": 500,
            "memory_bytes": 256 << 20,
        }))
        .unwrap(),
        instances: NonZeroU32::MIN,
    }
}

#[derive(Debug)]
struct Started {
    id: SandboxId,
    session: Session,
    arrivals: Receiver<Arrival>,
}

impl Started {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn session(&mut self) -> &mut Session {
        &mut self.session
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Nothing here panics, since a panic while a failed test unwinds would abort the test
        // binary and hide the other tests' results: failures are printed instead. Without an
        // engine, no test started a container.
        if let Ok(provider) = connected() {
            let provider: &dyn Provider = provider;
            for run in [&self.run, &self.other_run] {
                if thread::panicking()
                    && let Err(error) = show_logs(run)
                {
                    eprintln!("the logs of run {} can't be shown: {error}", run.as_str());
                }
                let listed = provider.list(run);
                let deleted = listed.and_then(|listed| {
                    let deleted = provider.delete(run, &ids(listed));
                    deleted.map_err(|error| error.to_string())
                });
                if let Err(error) = deleted {
                    eprintln!("run {} can't be cleaned up: {error}", run.as_str());
                }
            }
        }
    }
}

fn reached() -> &'static Result<Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let context = ContextName::try_from(test_support::docker_context())?;
        Engine::connect(context, &engine_dir())
    })
}

fn connected() -> &'static Result<Docker<'static>, String> {
    static PROVIDER: OnceLock<Result<Docker<'static>, String>> = OnceLock::new();
    PROVIDER.get_or_init(|| Docker::connect(reached().as_ref().map_err(String::clone)?, None))
}

// Shared by every run of the tests, as a machine's runs share their workspace's.
fn engine_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("docker")
}

fn engine() -> &'static Engine {
    reached().as_ref().unwrap()
}

fn provider() -> &'static Docker<'static> {
    connected().as_ref().unwrap()
}

fn startup() -> Deadline {
    Deadline::after(declared_startup())
}

fn declared_startup() -> Duration {
    let class = provider().ready().classes.values().next().unwrap();
    Duration::from_secs(class.startup_seconds.get())
}

fn ids(listed: Vec<Listed>) -> Vec<SandboxId> {
    (listed.into_iter())
        .map(|Listed { id, ended: _ }| id)
        .collect()
}

fn containers(run: &RunTag, format: &str) -> Result<Vec<String>, String> {
    let filter = format!("label=pumpkin.run={}", run.as_str());
    let ps = [
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        format,
    ];
    let output = docker()
        .args(ps)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("`docker ps` failed: {output:?}"));
    }
    let mut lines: Vec<String> = (String::from_utf8_lossy(&output.stdout).lines())
        .map(str::to_owned)
        .collect();
    lines.sort();
    Ok(lines)
}

fn show_logs(run: &RunTag) -> Result<(), String> {
    for id in containers(run, "{{.ID}}")? {
        let logs = docker().args(["logs", &id]).output();
        let logs = logs.map_err(|error| error.to_string())?;
        eprintln!(
            "logs of {id}:\n{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
    }
    Ok(())
}

fn image() -> Image {
    static ID: OnceLock<Image> = OnceLock::new();
    let id = ID.get_or_init(|| {
        // `/workspace` is world-writable, as a role's user needs, since the writable layer is the
        // workspace, and so is `/volume`, for which Docker makes each container an anonymous
        // volume. The engine builds it as `pumpkin build` does, and finds a build of the same
        // inputs by its tag, so that test processes that run at once share the image.
        let recipe =
            format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace /volume\nVOLUME /volume\n");
        let context = BuildContext::default();
        let given = Given::default();
        let built = engine().build(
            &context,
            &Recipe::Text(recipe),
            &given,
            "pumpkin-docker-tests",
        );
        built.unwrap().image().clone()
    });
    id.clone()
}

fn unpulled() -> Image {
    Image::try_from(format!("sha256:{}", "0".repeat(64))).unwrap()
}

fn check(name: &str, scenario: impl FnOnce(&Fixture) -> Vec<String>, expect: &Expect) {
    let dir = TestDir::new();
    let fixture = Fixture::new(name, dir.path());
    expect.assert_eq(&scenario(&fixture).join("\n"));
}

fn absolute(path: &str) -> AbsolutePath {
    AbsolutePath::try_from(path.to_owned()).unwrap()
}

fn relative(path: &str) -> RelativePath {
    RelativePath::try_from(path.to_owned()).unwrap()
}

fn launch(n: u32, script: &str) -> Launch {
    Launch {
        argv: Argv::try_from(["sh", "-c", script].map(str::to_owned).to_vec()).unwrap(),
        env: Env::try_from(vec![
            EnvVar::try_from("HOME=/pumpkin/home".to_owned()).unwrap(),
        ])
        .unwrap(),
        cwd: absolute("/pumpkin/home"),
        stdin: None,
        stdout: absolute(&format!("/pumpkin/log/{n}.stdout")),
        stderr: absolute(&format!("/pumpkin/log/{n}.stderr")),
    }
}

fn sh(sandbox: &mut Started, n: u32, script: &str) -> ExitStatus {
    let process = sandbox.session().start(launch(n, script), hung()).unwrap();
    match sandbox.arrivals.recv_timeout(Duration::from_secs(30)) {
        Ok(Arrival::Exited {
            process: exited,
            status,
        }) if exited == process => status,
        arrived => panic!("the command's exit was to arrive within 30 s, not {arrived:?}"),
    }
}

fn serve(sandbox: &mut Started, process: Process, payload: &[u8]) -> (ExitStatus, Vec<String>) {
    let deadline = Deadline::after(Duration::from_secs(30));
    let mut asked = Vec::new();
    loop {
        let left = deadline.remaining();
        match left.and_then(|left| sandbox.arrivals.recv_timeout(left).ok()) {
            Some(Arrival::Exited {
                process: exited,
                status,
            }) if exited == process => return (status, asked),
            Some(arrival) => asked.push(reply(&mut sandbox.session, arrival, payload)),
            None => panic!("the command ran on for 30 s, having made ops that asked {asked:?}"),
        }
    }
}

fn collect(fixture: &Fixture, sandbox: &mut Started, dir: &str) -> Vec<String> {
    let into = fixture.dir.join(format!("collected-{dir}"));
    let limits = Limits {
        bytes: 1 << 20,
        entries: 100,
    };
    let from = absolute(&format!("/pumpkin/{dir}"));
    sandbox
        .session()
        .collect(&from, &into, limits, hung())
        .unwrap();
    let mut lines = Vec::new();
    list(&into, &into, &mut lines);
    lines
}

fn list(base: &Path, dir: &Path, lines: &mut Vec<String>) {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            list(base, &path, lines);
        } else {
            let name = path.strip_prefix(base).unwrap().display();
            let contents = fs::read_to_string(&path).unwrap();
            lines.push(format!(
                "{name}: {}",
                contents.trim_end().replace('\n', "; ")
            ));
        }
    }
}

fn processes(sandbox: &Started, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let top = docker()
            .args(["top", sandbox.id(), "-o", "pid,args"])
            .output()
            .unwrap();
        let top = String::from_utf8(top.stdout).unwrap();
        let commands = top.lines().skip(1).map(|line| {
            let (_pid, command) = line.trim_start().split_once(' ').unwrap();
            command.trim_start().to_owned()
        });
        let listed = commands.collect::<Vec<_>>().join(", ");
        if done(&listed) || Instant::now() >= deadline {
            return listed;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

// A role's command exits leaving processes that freezing kills: one orphaned to `sandboxd`, and
// one in a session of its own, outside its command's process group.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn freezes_what_it_left() {
    check(
        "freezes",
        |fixture| {
            let mut sandbox = fixture.start(&fixture.run, "leaver");
            let (dir, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let put = [
                Put::Dir(dir, dir_mode()),
                Put::File(payload, file_mode(), b"ledger"),
            ];
            sandbox
                .session()
                .put(&absolute("/pumpkin"), put, hung())
                .unwrap();
            let script = r#"
                printf '{"bytes": %s}' "$(wc -c < ../in/payload/raw)" > ../out/result.json
                mkdir ../out/evidence && echo kept > ../out/evidence/evidence.txt
                echo "user $(id -u):$(id -g) home=$HOME in $(pwd)"
                (sleep 600 &); setsid sleep 600 &
                exit 3"#;
            let mut lines = vec![format!("{:?}", sh(&mut sandbox, 1, script))];
            // The script exits without waiting for its sleeps, which may not run yet: until then,
            // each shows as the command that starts it.
            let sleeping = |listed: &str| listed.split(", ").filter(|c| *c == "sleep 600").count();
            let before = processes(&sandbox, |listed| sleeping(listed) == 2);
            lines.push(format!("before freezing: {before}"));
            // Freezing returns once the processes it kills are gone.
            sandbox.session().freeze(hung()).unwrap();
            let after = processes(&sandbox, |_| true);
            lines.push(format!("after freezing: {after}"));
            lines.extend(collect(fixture, &mut sandbox, "out"));
            lines.extend(collect(fixture, &mut sandbox, "log"));
            lines
        },
        &expect![[r#"
            Code(3)
            before freezing: /.pumpkin/sandboxd /.pumpkin/gateway/s, sleep 600, sleep 600
            after freezing: /.pumpkin/sandboxd /.pumpkin/gateway/s
            evidence/evidence.txt: kept
            result.json: {"bytes": 6}
            1.stderr: 
            1.stdout: user 4242:4243 home=/pumpkin/home in /pumpkin/home"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn confinement() {
    check(
        "confinement",
        |fixture| {
            let mut sandbox = fixture.start(&fixture.run, "confined");
            let script = r#"
                echo "interfaces: $(ls /sys/class/net)"
                nc -w 2 1.1.1.1 80 </dev/null >/dev/null 2>&1
                echo "outbound connect: exit $?"
                echo "TCP listeners: $(awk '$4 == "0A"' /proc/net/tcp /proc/net/tcp6 | wc -l)"
                grep -E '^(Cap|NoNewPrivs)' /proc/self/status
                echo "uid=$(id -u) gid=$(id -g)"
                echo "/.pumpkin holds:" $(ls /.pumpkin)
                echo "/pumpkin holds:" $(ls -A /pumpkin)
                ln -s /.pumpkin/sandboxd sandboxctl && echo "sandboxctl ops: $(./sandboxctl ops 2>&1)"
                for limit in cpu.max memory.max memory.swap.max pids.max; do
                    echo "$limit: $(cat /sys/fs/cgroup/$limit)"
                done
                echo "core file size: $(ulimit -c)"
                awk '$2 ~ /^\/(pumpkin|tmp|dev\/shm|dev\/mqueue)$/ { print $3 " at " $2 }' /proc/mounts | sort
                echo "/pumpkin: $(stat -c '%u:%g %a' /pumpkin), $(df -k /pumpkin | awk 'NR == 2 { print $2 }') KiB"
                printf '#!/bin/sh\necho ran\n' > /pumpkin/home/program && chmod +x /pumpkin/home/program
                echo "a program in /pumpkin: $(/pumpkin/home/program 2>&1)"
                for file in /tmp/x /.pumpkin/sandboxd /.pumpkin/gateway/x /etc/hosts /workspace/x \
                    /volume/x /pumpkin/home/x /dev/shm/x /proc/1/fd/1; do
                    if echo > $file 2>/dev/null; then echo "$file: writable"; else echo "$file: refused"; fi
                done
                touch /dev/mqueue/x && echo "/dev/mqueue/x: made"
                cat /proc/self/uid_map > /pumpkin/out/uid_map"#;
            let status = sh(&mut sandbox, 1, script);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(collect(fixture, &mut sandbox, "log"));
            collect(fixture, &mut sandbox, "out");
            let uid_map = fs::read_to_string(fixture.dir.join("collected-out/uid_map")).unwrap();
            // Rootless Docker and user namespaces each map the container's users to others on the
            // host; a rootful engine without them maps every user to itself.
            let identity = uid_map.split_whitespace().eq(["0", "0", "4294967295"]);
            let Security {
                rootless,
                seccomp_profile: _,
                apparmor: _,
                userns,
            } = engine().report().security;
            lines.push(format!(
                "rootless or userns, exactly when the uid map isn't the identity: {}",
                (rootless || userns) != identity
            ));
            let from_host = |args: &[&str]| {
                let output = docker().args(args).arg(sandbox.id()).output();
                let output = output.unwrap();
                let (stdout, stderr) = (output.stdout, output.stderr);
                String::from_utf8([stdout, stderr].concat()).unwrap()
            };
            let pinned = "log config: {{json .HostConfig.LogConfig}}\n\
                          init: {{json .HostConfig.Init}}\n\
                          healthcheck: {{json .Config.Healthcheck}}";
            let inspected = from_host(&["inspect", "--format", pinned]);
            lines.extend(inspected.lines().map(str::to_owned));
            lines.push(format!("log: {:?}", from_host(&["logs", "--tail", "20"])));
            lines
        },
        &expect![[r#"
            Code(0)
            1.stderr: sh: can't create /tmp/x: Read-only file system; sh: can't create /.pumpkin/sandboxd: Read-only file system; sh: can't create /.pumpkin/gateway/x: Read-only file system; sh: can't create /etc/hosts: Permission denied; sh: can't create /proc/1/fd/1: Permission denied
            1.stdout: interfaces: lo; outbound connect: exit 1; TCP listeners: 0; CapInh:	0000000000000000; CapPrm:	0000000000000000; CapEff:	0000000000000000; CapBnd:	0000000000000000; CapAmb:	0000000000000000; NoNewPrivs:	1; uid=4242 gid=4243; /.pumpkin holds: gateway sandboxd; /pumpkin holds: home in log out; sandboxctl ops: sandboxctl: sandboxd at /pumpkin/sandboxctl/socket can't be reached: No such file or directory (os error 2): run sandboxctl in a sandbox that serves ops; cpu.max: 50000 100000; memory.max: 268435456; memory.swap.max: 0; pids.max: 4096; core file size: 0; mqueue at /dev/mqueue; tmpfs at /dev/shm; tmpfs at /pumpkin; tmpfs at /tmp; /pumpkin: 4242:4243 755, 1048576 KiB; a program in /pumpkin: ran; /tmp/x: refused; /.pumpkin/sandboxd: refused; /.pumpkin/gateway/x: refused; /etc/hosts: refused; /workspace/x: writable; /volume/x: writable; /pumpkin/home/x: writable; /dev/shm/x: writable; /proc/1/fd/1: refused; /dev/mqueue/x: made
            rootless or userns, exactly when the uid map isn't the identity: true
            log config: {"Type":"json-file","Config":{"max-file":"1","max-size":"1048576"}}
            init: false
            healthcheck: {"Test":["NONE"]}
            log: """#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is what each share shows, and the snapshot"
)]
fn cpus() {
    check(
        "cpus",
        |fixture| {
            let sized = |name: &str, cpus: u64| Role {
                resources: Resources {
                    cpu_millis: NonZeroU64::new(cpus * 1000).unwrap(),
                    ..role(name).resources
                },
                ..role(name)
            };
            let first = &fixture.other_run;
            let runners = || Role {
                instances: NonZeroU32::new(2).unwrap(),
                ..sized("runner", 1)
            };
            let phases = [
                vec![sized("agent", 2), sized("train", 1), runners()],
                vec![sized("final", 2)],
            ];
            let reserved = reserve_with_room(provider(), first, &phases);
            let start = |run: &RunTag, name: &str, role: Role, instance: u32| {
                let made = fixture.publish(run, name, &fixture.sandboxd, &image(), false, false);
                let (mut create, dial, registration) = made.unwrap();
                (create.role, create.instance) = (role, instance);
                Fixture::start_created(&create, dial, registration, None, false).unwrap()
            };
            let mut started = [
                ("the agent", start(first, "agent", sized("agent", 2), 0)),
                ("the trainer", start(first, "train", sized("train", 1), 0)),
                ("a runner", start(first, "runner-0", runners(), 0)),
                ("another runner", start(first, "runner-1", runners(), 1)),
                (
                    "another run's role",
                    start(&fixture.run, "leaver", role("leaver"), 0),
                ),
            ];
            let mut lines = Vec::new();
            let mut sets = Vec::new();
            for (name, sandbox) in &mut started {
                let (given, seen) = given_and_seen(fixture, sandbox);
                let whole = given.iter().all(|&cpu| siblings(cpu).is_subset(&given));
                lines.push(format!(
                    "{name}: on whole cores: {whole}, as it sees them: {}",
                    seen == given
                ));
                sets.push(given);
            }
            let apart =
                (0..sets.len()).all(|a| (a + 1..sets.len()).all(|b| sets[a].is_disjoint(&sets[b])));
            lines.push(format!("each apart from the others: {apart}"));
            let online = fs::read_to_string("/sys/devices/system/cpu/online").unwrap();
            let online = cpu_list(&online);
            let count = online.len() as u64;
            let cores = online
                .into_iter()
                .map(siblings)
                .collect::<BTreeSet<_>>()
                .len();
            let threads = count / cores as u64;
            let held = sets.iter().flatten().map(|&cpu| siblings(cpu));
            let held = held.collect::<BTreeSet<_>>().len() as u64;
            let wide = RunTag::try_from(format!("{}-wide", fixture.run.as_str())).unwrap();
            // One core more than the others leave.
            let left = threads * (cores as u64 - held + 1);
            let left = provider().reserve(&wide, &[vec![sized("wide", left)]]);
            let refused = left.unwrap_err();
            let named = [first, &fixture.run]
                .iter()
                .all(|run| refused.contains(run.as_str()));
            lines.push(format!(
                "one that needs more than are left: refused, naming them: {named}"
            ));
            let more = provider().reserve(&wide, &[vec![sized("wide", count + 1)]]);
            let more = more.unwrap_err();
            let more = more.replace(&(count + 1).to_string(), "<cpus + 1>");
            let more = more.replace(&cores.to_string(), "<cores>");
            lines.push(format!("one that needs more than the machine has: {more}"));
            drop(reserved);
            lines
        },
        &expect![[r#"
            the agent: on whole cores: true, as it sees them: true
            the trainer: on whole cores: true, as it sees them: true
            a runner: on whole cores: true, as it sees them: true
            another runner: on whole cores: true, as it sees them: true
            another run's role: on whole cores: true, as it sees them: true
            each apart from the others: true
            one that needs more than are left: refused, naming them: true
            one that needs more than the machine has: the run's roles need more whole cores at once than the <cores> that this machine gives sandboxes, to hold their CPUs (`wide` <cpus + 1>): give them fewer CPUs"#]],
    );
}

fn given_and_seen(fixture: &Fixture, sandbox: &mut Started) -> (BTreeSet<u32>, BTreeSet<u32>) {
    let inspect = [
        "inspect",
        "--format",
        "{{.HostConfig.CpusetCpus}}",
        sandbox.id(),
    ];
    let given = docker().args(inspect).output().unwrap();
    let given = cpu_list(&String::from_utf8(given.stdout).unwrap());
    sh(
        sandbox,
        1,
        "grep Cpus_allowed_list /proc/self/status | cut -f 2",
    );
    let seen = collect(fixture, sandbox, "log");
    fs::remove_dir_all(fixture.dir.join("collected-log")).unwrap();
    let seen = seen.iter().find_map(|line| line.strip_prefix("1.stdout: "));
    (given, cpu_list(seen.unwrap_or_default()))
}

fn siblings(cpu: u32) -> BTreeSet<u32> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    cpu_list(&fs::read_to_string(path).unwrap())
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

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn ready() {
    let mut ready = serde_json::to_value(provider().ready()).unwrap();
    let reported: Vec<&String> = ready["report"].as_object().unwrap().keys().collect();
    let reported = format!("{reported:?}");
    let classes = ready["classes"].as_object_mut().unwrap();
    for class in classes.values_mut() {
        let offers = &mut class["offers"];
        offers["cpu_millis"] = "<the host's>".into();
        offers["memory_bytes"] = "<the host's>".into();
        let sse2 = offers["cpu_features"]
            .as_array()
            .unwrap()
            .contains(&"sse2".into());
        // Whether the flags hold `sse2`, which every x86-64 CPU has, stands for them: the rest are
        // the host's own.
        offers["cpu_features"] = format!("<the host's, with sse2: {sse2}>").into();
    }
    let ready = format!(
        "a report of {reported}, and the classes {}",
        ready["classes"]
    );
    expect![[r#"a report of ["client_version", "context", "endpoint", "engine_version", "kernel", "os", "platform", "security"], and the classes {"container":{"offers":{"cpu_features":"<the host's, with sse2: true>","cpu_millis":"<the host's>","cpu_model":null,"gpu":null,"memory_bytes":"<the host's>"},"startup_seconds":540}}"#]]
    .assert_eq(&ready);
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn volumes() {
    check(
        "volumes",
        |fixture| {
            let _other = reserve_with_room(provider(), &fixture.other_run, &[vec![role("other")]]);
            let started = [("first", &fixture.run), ("other", &fixture.other_run)];
            let started = started.map(|(role, run)| (role, fixture.start(run, role)));
            let volumes = started.each_ref().map(|(_, sandbox)| volume(sandbox));
            let listed = ids(provider().list(&fixture.run).unwrap());
            provider().delete(&fixture.run, &listed).unwrap();
            let listed = docker().args(["volume", "ls", "--quiet"]).output().unwrap();
            let listed = String::from_utf8(listed.stdout).unwrap();
            let kept = (started.iter().zip(&volumes))
                .filter(|&(_, volume)| listed.lines().any(|listed| listed == volume))
                .map(|((role, _), _)| role);
            vec![format!("volumes left: {:?}", kept.collect::<Vec<_>>())]
        },
        &expect![[r#"volumes left: ["other"]"#]],
    );
}

fn volume(sandbox: &Started) -> String {
    let format = r#"{{range .Mounts}}{{if eq .Destination "/volume"}}{{.Name}}{{end}}{{end}}"#;
    let inspect = ["inspect", "--format", format, sandbox.id()];
    let output = docker().args(inspect).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let name = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    assert!(!name.is_empty(), "the sandbox has no volume at /volume");
    name
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn suite() {
    test_support::suite::check(&[("docker", &Platform)]);
}

struct Platform;

impl Subject for Platform {
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String> {
        Ok(Box::new(Docker::connect(engine(), None)?))
    }

    fn locals(&self, dir: &Path) -> Locals {
        // One that no user may execute, which the container's start fails to run.
        let broken = dir.join("unexecutable");
        fs::write(&broken, "not a program").unwrap();
        Locals {
            image: image(),
            sandboxd: test_support::sandboxd(),
            mute: mute_sandboxd(dir),
            broken,
            unreachable_image: unpulled(),
            unreachable_sandboxd: dir.join("absent"),
        }
    }

    // Pausing freezes every process in the container, `sandboxd` among them, before it returns.
    fn stall(&self, id: &SandboxId) {
        run(&["pause", id.as_str()]);
    }

    fn held(&self, run: &RunTag) -> Vec<SandboxId> {
        let held = containers(run, "{{.ID}}").unwrap();
        held.into_iter().map(SandboxId::from).collect()
    }

    fn reserves(&self) -> bool {
        true
    }
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn hung_create() {
    // The test runs twice: as the caller of a `create` that hangs, in a child whose `PATH` has a
    // `docker` that runs the real `create`, and then sleeps, and as the test that reads what the
    // child saw.
    if let Some(dir) = env::var_os(STAND_IN) {
        // Beside the caller's fixture, in its test's directory, and private to this user, as the
        // parent of every socket directory must be.
        let caller = Path::new(&dir).join("caller");
        DirBuilder::new().mode(0o700).create(&caller).unwrap();
        let fixture = Fixture::new("hung-create-caller", &caller);
        let made = fixture.publish(
            &fixture.run,
            "hung",
            &fixture.sandboxd,
            &image(),
            false,
            false,
        );
        let (mut create, dial, _registration) = made.unwrap();
        // Long after the stand-in has started Docker's create, and long before its sleep would end.
        create.by = UnixTime::of(SystemTime::now() + Duration::from_secs(5)).unwrap();
        let failed = provider().create(&create, dial).unwrap_err();
        let listed = provider().list(&fixture.run).unwrap().len();
        let created = format!("the create failed: {failed}\nthe run's list then finds: {listed}");
        fs::write(Path::new(&dir).join("created"), created).unwrap();
        return;
    }
    check(
        "hung-create",
        |fixture| {
            let bin = fixture.dir.join("bin");
            fs::create_dir(&bin).unwrap();
            let script = "#!/bin/sh\nPATH=${PATH#*:}\n\
                          [ \"$1\" = create ] && { setsid --wait docker \"$@\"; exec sleep 60; }\n\
                          exec docker \"$@\"\n";
            let docker = bin.join("docker");
            fs::write(&docker, script).unwrap();
            fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
            let path = format!("{}:{}", bin.display(), env::var("PATH").unwrap());
            // Docker's `docker` is on `PATH` after `bin`. Docker's create runs in a session of its
            // own, which the kill at the deadline doesn't reach, so it makes its container however
            // slow the engine is, and the provider's create, which waits for the output it holds,
            // returns once it has ended.
            let kept = &[
                "HOME",
                "DOCKER_CONFIG",
                "PUMPKIN_DOCKER_CONTEXT",
                "PUMPKIN_SANDBOXD",
            ];
            let mut caller =
                stdx::process::command(env::current_exe().unwrap(), Environment::Only(kept));
            caller.args(["--exact", "hung_create", "--include-ignored"]);
            let caller = caller.env("PATH", path).env(STAND_IN, &fixture.dir);
            let output = caller.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let created = fs::read_to_string(fixture.dir.join("created")).unwrap();
            created.lines().map(str::to_owned).collect()
        },
        &expect![[r#"
            the create failed: `docker create` hadn't ended by its deadline, so it was killed: check that the engine runs and isn't overloaded
            the run's list then finds: 1"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn failed_start() {
    check(
        "failed",
        |fixture| {
            let not_executable = fixture.dir.join("sandboxd");
            fs::write(&not_executable, "not a program").unwrap();
            let missing = fixture.dir.join("absent");
            let not_utf8 = fixture.dir.join(OsStr::from_bytes(b"sandboxd\xff"));
            fs::write(&not_utf8, "").unwrap();
            let attempts = [
                ("missing", missing.as_path(), image()),
                ("not_utf8", not_utf8.as_path(), image()),
                ("unexecutable", not_executable.as_path(), image()),
                ("unpulled", fixture.sandboxd.as_path(), unpulled()),
            ];
            let mut lines = Vec::new();
            for (role, sandboxd, image) in attempts {
                let failed = fixture.try_start(&fixture.run, role, sandboxd, &image, None, false);
                lines.push(format!("{role}: {}", redact(fixture, &failed.unwrap_err())));
            }
            let left = containers(&fixture.run, "{{.ID}}").unwrap();
            lines.push(format!("left: {left:?}"));
            lines
        },
        &expect![[r#"
            missing: publishing `sandboxd` failed: <dir>/absent can't be mounted at /.pumpkin/sandboxd: No such file or directory (os error 2): name a path that exists
            not_utf8: publishing `sandboxd` failed: <dir>/sandboxd� can't be mounted at /.pumpkin/sandboxd: name a path of UTF-8
            unexecutable: `docker start …: fix what it reports
            unpulled: the engine of context `<context>` has no image sha256:0000000000000000000000000000000000000000000000000000000000000000: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it
            left: []"#]],
    );
}

// What `docker start` reports varies with the container runtime and its version.
fn redact(fixture: &Fixture, message: &str) -> String {
    let context = format!("context `{}`", engine().report().context.as_str());
    let message = (message.replace(&fixture.dir.display().to_string(), "<dir>"))
        .replace(&context, "context `<context>`");
    let (from, to) = ("`docker start ", ": fix what it reports");
    if let Some((before, rest)) = message.split_once(from)
        && let Some((_, after)) = rest.split_once(to)
    {
        return format!("{before}{from}…{to}{after}");
    }
    message
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn console() {
    check(
        "console",
        |fixture| {
            let stand_in = mute_sandboxd(&fixture.dir);
            let published =
                fixture.publish(&fixture.run, "stand-in", &stand_in, &image(), false, false);
            let (create, dial, _) = published.unwrap();
            let Created { id } = provider().create(&create, dial).unwrap();
            // Docker finishes writing a container's log before it marks the container stopped,
            // which `docker wait` waits for, so the console then holds all the stand-in printed.
            run(&["wait", id.as_str()]);
            let console = provider().console(&fixture.run, &id).unwrap();
            vec![format!("console: {}", console.text.trim())]
        },
        &expect![[r#"
            console: sandboxd: a stand-in, which exits without dialing"#]],
    );
}

fn run(args: &[&str]) {
    let output = docker().args(args).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn mute_sandboxd(dir: &Path) -> PathBuf {
    let mute = dir.join("stand-in-sandboxd");
    let script = "#!/bin/sh\necho 'sandboxd: a stand-in, which exits without dialing' >&2\n";
    fs::write(&mute, script).unwrap();
    fs::set_permissions(&mute, fs::Permissions::from_mode(0o755)).unwrap();
    mute
}

const CTL: &str = r#"
    ctl() { out=$(sandboxctl "$@" 2>&1); echo "\$ sandboxctl $*: exit $?"; [ -z "$out" ] || echo "  $out"; }"#;

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn sandboxctl_on_path() {
    check(
        "ops-path",
        |fixture| {
            let sandbox = fixture.try_start(
                &fixture.run,
                "ops",
                &fixture.sandboxd,
                &image(),
                Some(OpPayloadLimit::NONE),
                false,
            );
            let mut sandbox = sandbox.unwrap();
            let executable = Mode::new(0o755);
            let put = [
                Put::File(relative("sandboxctl"), executable, b"#!/bin/sh\nexit 99\n"),
                Put::File(
                    relative("caller"),
                    executable,
                    b"#!/bin/sh\nexec sandboxctl ops\n",
                ),
            ];
            let home = "/pumpkin/home";
            sandbox.session().put(&absolute(home), put, hung()).unwrap();
            let mut lines = Vec::new();
            for (program, cwd, path) in [
                ("sandboxctl", "/workspace", "PATH=/pumpkin/home"),
                ("caller", "/workspace", "PATH=/pumpkin/home"),
                ("caller", home, "PATH="),
            ] {
                let launch = Launch {
                    argv: Argv::try_from([program, "ops"].map(str::to_owned).to_vec()).unwrap(),
                    cwd: absolute(cwd),
                    env: Env::try_from(vec![EnvVar::try_from(path.to_owned()).unwrap()]).unwrap(),
                    ..launch(1, "")
                };
                let process = sandbox.session().start(launch, hung()).unwrap();
                let (status, asked) = serve(&mut sandbox, process, &[]);
                lines.push(format!("{path} {program} in {cwd}: {status:?}, {asked:?}"));
            }
            lines
        },
        &expect![[r#"
            PATH=/pumpkin/home sandboxctl in /workspace: Code(0), ["ops"]
            PATH=/pumpkin/home caller in /workspace: Code(0), ["ops"]
            PATH= caller in /pumpkin/home: Code(0), ["ops"]"#]],
    );
}

// `sandboxd` closes the control socket's directory to all but the role's user, and `sandboxctl` is
// read-only, so writing it fails the last command.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the script and the snapshot"
)]
fn ops() {
    check(
        "ops",
        |fixture| {
            let payload = payload();
            let limit = OpPayloadLimit::try_from(u64::try_from(payload.len()).unwrap()).unwrap();
            let (sandboxd, ops) = (&fixture.sandboxd, Some(limit));
            let sandbox = fixture.try_start(&fixture.run, "ops", sandboxd, &image(), ops, false);
            let mut sandbox = sandbox.unwrap();
            let session = sandbox.session();
            let put = [Put::File(relative("payload"), file_mode(), &payload)];
            session
                .put(&absolute("/pumpkin/home"), put, hung())
                .unwrap();
            let script = format!(
                r#"{CTL}
                cp payload big && printf '!' >> big && ln -s payload link
                ctl ops
                ctl ops --text
                ctl call train
                /.pumpkin/bin/sandboxctl ops > /dev/full; echo "\$ sandboxctl ops > /dev/full: exit $?"
                ctl call train payload
                ctl call submit ../home/payload --arg finalize=true
                ctl call submit payload
                ctl call train link
                ctl status 1
                ctl status 2
                ctl status 3
                ctl call train big
                ctl call gpu.open
                ctl call gpu.open --seconds 30
                ctl call gpu.open --seconds 0
                ctl call gpu.run 0 --timeout 5 -- echo -n hi
                ctl call gpu.run 0 -- sh -c true
                ctl call gpu.status 0
                ctl call gpu.put 0 payload /work/in
                ctl call gpu.put 0 payload /work/in --replace
                ctl call gpu.get 0 /work/out ../got
                ctl call gpu.put 0 payload work
                ctl call gpu.close 0
                ctl call gpu.run 0
                ctl call gpu.close
                echo "control socket's directory: $(ls -ld /pumpkin/sandboxctl | cut -c1-10); TCP listeners: $(awk '$4 == "0A"' /proc/net/tcp /proc/net/tcp6 | wc -l)"
                echo > /.pumpkin/bin/sandboxctl"#
            );
            let process = session.start(launch(1, &script), hung()).unwrap();
            let (status, arrived) = serve(&mut sandbox, process, &payload);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(arrived.iter().map(|arrived| format!("owner: {arrived}")));
            lines
        },
        &expect![[r#"
            Code(1)
            $ sandboxctl ops: exit 0
              {"kind":"ops","role":"agent","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            $ sandboxctl ops --text: exit 0
              Sandbox commands for role agent
            Work time upper bound: 0 seconds. Run `sandboxctl ops` for current limits.
            $ sandboxctl call train: exit 0
              {"kind":"call","job":1,"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl ops > /dev/full: exit 0
            $ sandboxctl call train payload: exit 0
              {"kind":"call","job":1,"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl call submit ../home/payload --arg finalize=true: exit 0
              {"kind":"call","returned":{"finalize":"true"},"remaining_seconds":60}
            $ sandboxctl call submit payload: exit 0
              {"kind":"call","returned":{},"remaining_seconds":60}
            $ sandboxctl call train link: exit 0
              {"kind":"call","job":1,"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 1: exit 0
              {"kind":"status","job":1,"state":"completed","returned":{"score":1},"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 2: exit 2
              {"kind":"status","job":2,"state":"errored","failure":{"code":"call_timeout","message":"the call ran past its timeout"},"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 3: exit 1
              {"kind":"refused","message":"there is no job 3: name one a call returned"}
            $ sandboxctl call train big: exit 1
              {"kind":"refused","message":"the payload is 2621441 bytes, more than the limit of 2621440: send at most 2621440"}
            $ sandboxctl call gpu.open: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.open --seconds 30: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.open --seconds 0: exit 1
              sandboxctl: `0` is not a lease's time: give a whole number of seconds, 1 or more
            $ sandboxctl call gpu.run 0 --timeout 5 -- echo -n hi: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.run 0 -- sh -c true: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.status 0: exit 0
              {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":5,"busy_seconds":3,"idle_seconds":1,"failure":null,"last":{"kind":"command","command":0,"end":{"exited":{"code":0}},"stdout":"hi","stderr":""},"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.put 0 payload /work/in: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.put 0 payload /work/in --replace: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.get 0 /work/out ../got: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.put 0 payload work: exit 1
              sandboxctl: `work` is a path in the runner, which needs a whole one: start it with `/`
            $ sandboxctl call gpu.close 0: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl call gpu.run 0: exit 1
              sandboxctl: `gpu.run 0` is no session op: give `gpu.open [--seconds <seconds>]`, `gpu.run <lease> [--timeout <seconds>] -- <program> [<arg>...]`, `gpu.put <lease> <tree> <to> [--replace]`, `gpu.get <lease> <from> <to>`, `gpu.status <lease>` or `gpu.close <lease>`
            $ sandboxctl call gpu.close: exit 1
              sandboxctl: `gpu.close` is no session op: give `gpu.open [--seconds <seconds>]`, `gpu.run <lease> [--timeout <seconds>] -- <program> [<arg>...]`, `gpu.put <lease> <tree> <to> [--replace]`, `gpu.get <lease> <from> <to>`, `gpu.status <lease>` or `gpu.close <lease>`
            control socket's directory: drwx------; TCP listeners: 0
            stderr: sh: can't create /.pumpkin/bin/sandboxctl: Read-only file system
            owner: ops
            owner: ops
            owner: call train {}: 0 bytes, intact: false
            owner: ops
            owner: call train {}: 2621440 bytes, intact: true
            owner: call submit {Name("finalize"): "true"}: 2621440 bytes, intact: true
            owner: call submit {}: 2621440 bytes, intact: true
            owner: call train {}: 2621440 bytes, intact: true
            owner: status 1
            owner: status 2
            owner: status 3
            owner: refused Call { grant: Name("train"), arguments: {} }, over 2621440 bytes
            owner: gpu.open None
            owner: gpu.open Some(30)
            owner: gpu.run 0: ["echo","-n","hi"], Some(5)
            owner: gpu.run 0: ["sh","-c","true"], None
            owner: gpu.status 0
            owner: gpu.put 0: /pumpkin/home/payload to /work/in, replace false
            owner: gpu.put 0: /pumpkin/home/payload to /work/in, replace true
            owner: gpu.get 0: /work/out to /pumpkin/got
            owner: gpu.close 0"#]],
    );
}

// A refused call fails its caller alone, so that the session serves the next; `/proc/1/mem` can't
// be opened, as `sandboxd` is non-dumpable.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the script and its snapshot"
)]
fn refused_calls() {
    check(
        "refused",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let (sandboxd, image) = (&fixture.sandboxd, image());
            let sandbox = fixture.try_start(&fixture.run, "refused", sandboxd, &image, ops, false);
            let mut sandbox = sandbox.unwrap();
            // The second `sandboxd`'s session, at a socket moved to where the sandbox dials.
            let name = Name::try_from("second".to_owned()).unwrap();
            let (_, second) = fixture.sockets.issue(&name).unwrap();
            let issued = fixture.dir.join("sockets/second").join(SOCKET_NAME);
            fs::rename(issued, fixture.dir.join("sockets/refused/second")).unwrap();
            let script = format!(
                r#"{CTL}
                truncate -s {huge} huge
                mkfifo fifo
                ctl call submit huge
                ctl call train missing
                ctl call train .
                ctl call train fifo
                ctl call train /proc/1/mem
                ctl call train ''
                ctl call Train huge
                ctl status soon
                ctl call train one two
                ctl call train --seconds 5 huge
                ctl call submit huge --arg finalize
                ctl call submit huge --arg a=1 --arg a=2
                ctl call gpu.open --wait 1
                ctl call gpu.open --arg a=1
                ctl status 1
                second=$(/.pumpkin/sandboxd /.pumpkin/gateway/second --ops 2>&1)
                echo "a second sandboxd: exit $?: $second""#,
                huge = MAX_OP_PAYLOAD_BYTES + 1,
            );
            let process = sandbox.session().start(launch(1, &script), hung()).unwrap();
            // The command waits for the second `sandboxd`, which waits for its session to open, so
            // that session is accepted beside the owner's loop.
            let (status, arrived) = thread::scope(|scope| {
                let second = scope.spawn(move || {
                    let (arrivals, _) = mpsc::channel();
                    second.accept(hung(), None, false, arrivals, |arrival| arrival, || false)
                });
                let served = serve(&mut sandbox, process, &[]);
                drop(second.join().unwrap().unwrap());
                served
            });
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(arrived.iter().map(|arrived| format!("owner: {arrived}")));
            lines
        },
        &expect![[r#"
            Code(0)
            $ sandboxctl call submit huge: exit 1
              sandboxctl: /pumpkin/home/huge holds more than 67108864 bytes: send at most that
            $ sandboxctl call train missing: exit 1
              sandboxctl: missing: No such file or directory (os error 2): name a file this user can reach
            $ sandboxctl call train .: exit 1
              sandboxctl: /pumpkin/home is not a regular file: name one
            $ sandboxctl call train fifo: exit 1
              sandboxctl: /pumpkin/home/fifo is not a regular file: name one
            $ sandboxctl call train /proc/1/mem: exit 1
              sandboxctl: /proc/1/mem: Permission denied (os error 13)
            $ sandboxctl call train : exit 1
              sandboxctl: the payload file's path is empty: name a file
            $ sandboxctl call Train huge: exit 1
              sandboxctl: `Train` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            $ sandboxctl status soon: exit 1
              sandboxctl: `soon` is not a job: name one by the number a call returned
            $ sandboxctl call train one two: exit 1
              sandboxctl: `train` takes at most one payload file: call it as `sandboxctl call train [<file>] [--arg <name>=<value>]… [--wait <seconds>]`
            $ sandboxctl call train --seconds 5 huge: exit 1
              sandboxctl: `train` takes at most one payload file: call it as `sandboxctl call train [<file>] [--arg <name>=<value>]… [--wait <seconds>]`
            $ sandboxctl call submit huge --arg finalize: exit 1
              sandboxctl: `--arg finalize` has no `=`: give it as `--arg <name>=<value>`
            $ sandboxctl call submit huge --arg a=1 --arg a=2: exit 1
              sandboxctl: `--arg a=2` names an arg given already: give each arg once
            $ sandboxctl call gpu.open --wait 1: exit 1
              sandboxctl: a session's `open` takes no `--wait`: give its own words and flags
            $ sandboxctl call gpu.open --arg a=1: exit 1
              sandboxctl: a session's `open` takes no `--arg`: give its own words and flags
            $ sandboxctl status 1: exit 0
              {"kind":"status","job":1,"state":"completed","returned":{"score":1},"remaining":{"calls":1,"seconds":null}}
            a second sandboxd: exit 1: sandboxd: serving ops failed: the control socket's directory /pumpkin/sandboxctl can't be made: File exists (os error 17): expected nothing there yet, in a directory this user can write to
            owner: status 1"#]],
    );
}

// A payload's file is read in its caller's turn, so one replaced by a symbolic link while its
// caller waits is refused, as `sandboxd` opens no link.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn turns() {
    check(
        "turns",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "turns", sandboxd, &image(), ops, false);
            let mut sandbox = sandbox.unwrap();
            let script = r#"
                exec 2>&1
                ctl=/.pumpkin/bin/sandboxctl
                sent() { until grep -q '(sandboxctl) S' /proc/$1/stat; do sleep 0.01; done; }
                $ctl status 1 & first=$!
                until [ -e ../in/held ]; do sleep 0.01; done
                kill $first; wait $first 2>/dev/null
                echo "the caller of status 1, which left with its op in flight: exit $?"
                $ctl status 4 & sent $!; kill $!; wait $! 2>/dev/null
                echo "the caller of status 4, which left as it waited its turn: exit $?"
                printf ledger > payload && cp payload swapped
                stalled=$($ctl call train payload --wait 1 2>&1)
                echo "waiting on the held connection: exit $?: $stalled"
                $ctl status 2 > 2.out 2>&1 & second=$!
                $ctl status 3 > 3.out 2>&1 & third=$!
                $ctl call train swapped > swapped.out 2>&1 & fourth=$!
                sent $second; sent $third; sent $fourth
                ln -sf payload swapped
                touch ready
                wait $second; echo "status 2: exit $?: $(cat 2.out)"
                wait $third; echo "status 3: exit $?: $(cat 3.out)"
                wait $fourth; echo "call train swapped: exit $?: $(cat swapped.out)"
                ninth=$($ctl status 9 2>&1); echo "status 9: exit $?: $ninth""#;
            // The owner holds the first op, and puts `in/held`, while the other callers arrive. In
            // the script, `sent` waits until a caller's `sandboxctl` sleeps, which it does only as
            // it waits for its answer, since connecting to a Unix socket waits only on a full
            // backlog. Once each has sent its call, the script touches `ready`, which the second
            // command waits for, and the owner answers the first op.
            let session = sandbox.session();
            let callers = session.start(launch(1, script), hung()).unwrap();
            let first = sandbox
                .arrivals
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            let held = [Put::Dir(relative("held"), dir_mode())];
            sandbox
                .session()
                .put(&absolute("/pumpkin/in"), held, hung())
                .unwrap();
            sh(&mut sandbox, 2, "until [ -e ready ]; do sleep 0.01; done");
            // No op can arrive while the first is in flight.
            let mut arrived = vec![reply(sandbox.session(), first, &[])];
            let (status, rest) = serve(&mut sandbox, callers, &[]);
            arrived.extend(rest);
            // The callers of status 2 and 3 may take their turns in either order.
            arrived.sort();
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(arrived.iter().map(|arrived| format!("owner: {arrived}")));
            lines
        },
        &expect![[r#"
            Code(0)
            the caller of status 1, which left with its op in flight: exit 143
            the caller of status 4, which left as it waited its turn: exit 143
            waiting on the held connection: exit 124: sandboxctl: wait expired before the reply: the call may have started
            status 2: exit 2: {"kind":"status","job":2,"state":"errored","failure":{"code":"call_timeout","message":"the call ran past its timeout"},"remaining":{"calls":1,"seconds":null}}
            status 3: exit 1: {"kind":"refused","message":"there is no job 3: name one a call returned"}
            call train swapped: exit 1: sandboxctl: /pumpkin/home/swapped: Symbolic link loop (os error 40)
            status 9: exit 1: {"kind":"refused","message":"there is no job 9: name one a call returned"}
            owner: status 1
            owner: status 2
            owner: status 3
            owner: status 9"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn model_endpoint() {
    check(
        "model",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "model", sandboxd, &image(), ops, true);
            let mut sandbox = sandbox.unwrap();
            let script = format!(
                r#"
                awk '$2 ~ /:{MODEL_PORT:04X}$/ && $4 == "0A" {{ print "listening on", $2 }}' /proc/net/tcp
                post() {{ printf "$1" | nc -w 30 127.0.0.1 {MODEL_PORT} | tr -d '\r' | tr '\n' '|'; echo; }}
                for n in 1 2 3 4 5 6 7 8; do post 'POST /held HTTP/1.1\r\n\r\n' > flight-$n & held="$held $!"; done
                wait $held
                echo "the flight: $(grep -l ' 200 ' flight-* | wc -l) answered; $(grep -h ' 503 ' flight-*)"
                writing() {{ awk '$2 ~ /:{MODEL_PORT:04X}$/ && ($4 == "01" || $4 == "08") {{ found = 1 }} END {{ exit !found }}' /proc/net/tcp; }}
                late() {{ printf 'POST /late HTTP/1.1\r\n\r\n'; until [ -e ../in/late ]; do sleep 0.01; done; printf x; }}
                echo "a body past the client's buffers, with a byte sent late: $(late | nc -w 30 127.0.0.1 {MODEL_PORT} | {{ until [ -e ../in/late ] && ! writing; do sleep 0.01; done; wc -c; }}) bytes"
                post 'POST /responses HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{{"model":"m"}}'
                post 'POST /responses HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}'
                post 'GET /models HTTP/1.1\r\nAccept:  */*  \r\n\r\n'
                echo "ended without a head: [$(post 'POST /headless HTTP/1.1\r\n\r\n')]"
                echo "answered 204, then cut short: [$(post 'POST /empty HTTP/1.9\r\nContent-Length: 2\r\n\r\n{{}}')]"
                echo "answered 103, an interim status: [$(post 'POST /interim HTTP/1.1\r\n\r\n')]"
                post 'HEAD /models HTTP/1.1\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: +0\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: {over}\r\n\r\n'
                post 'HEAD /models HTTP/1.0\r\n\r\n'
                post 'POST /responses HTTP/1.0\r\n\r\n'
                post "GET / HTTP/1.1\r\nx: $(head -c {pad} /dev/zero | tr '\0' a)""#,
                over = MAX_MODEL_REQUEST_BYTES + 1,
                // A head of 16 KiB, the endpoint's bound, with no end: all of it is read, so the
                // refusal reaches the client before the connection closes.
                pad = (16 << 10) - "GET / HTTP/1.1\r\nx: ".len(),
            );
            let session = sandbox.session();
            let process = session.start(launch(1, &script), hung()).unwrap();
            let mut asked = hold_flight(&mut sandbox);
            asked.push(respond_late(&mut sandbox));
            let (status, rest) = serve_models(&mut sandbox, process);
            asked.extend(rest);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(asked);
            lines
        },
        &expect![[r#"
            Code(0)
            listening on 0100007F:2BE3
            the flight: 7 answered; HTTP/1.1 503 Service Unavailable|content-type: application/json|content-length: 86|connection: close||{"error":{"message":"7 model requests are in flight already: wait for one to finish"}}
            a body past the client's buffers, with a byte sent late: 1048657 bytes
            HTTP/1.1 200 OK|content-type: text/event-stream|transfer-encoding: chunked|connection: close||9|data: 1|||9|data: 2|||0||
            HTTP/1.1 200 OK|content-type: text/event-stream|transfer-encoding: chunked|connection: close||9|data: 1|||
            HTTP/1.1 404 Not Found|content-type: application/json|transfer-encoding: chunked|connection: close||9|data: 1|||0||
            ended without a head: []
            answered 204, then cut short: []
            answered 103, an interim status: []
            HTTP/1.1 200 OK|content-type: text/event-stream|connection: close||
            HTTP/1.1 411 Length Required|content-type: application/json|content-length: 82|connection: close||{"error":{"message":"a chunked body is refused: send its content-length instead"}}
            HTTP/1.1 400 Bad Request|content-type: application/json|content-length: 70|connection: close||{"error":{"message":"the content-length is not one number: send one"}}
            HTTP/1.1 400 Bad Request|content-type: application/json|content-length: 70|connection: close||{"error":{"message":"the content-length is not one number: send one"}}
            HTTP/1.1 411 Length Required|content-type: application/json|content-length: 82|connection: close||{"error":{"message":"a chunked body is refused: send its content-length instead"}}
            HTTP/1.1 413 Content Too Large|content-type: application/json|content-length: 73|connection: close||{"error":{"message":"the body is 67108865 bytes: send at most 67108864"}}
            HTTP/1.1 426 Upgrade Required|content-type: application/json|upgrade: HTTP/1.1|connection: upgrade|content-length: 72|connection: close||
            HTTP/1.1 426 Upgrade Required|content-type: application/json|upgrade: HTTP/1.1|connection: upgrade|content-length: 72|connection: close||{"error":{"message":"the request's version is HTTP/1.0: send HTTP/1.1"}}
            HTTP/1.1 431 Request Header Fields Too Large|content-type: application/json|content-length: 89|connection: close||{"error":{"message":"the request's head holds more than 16384 bytes: send at most that"}}
            owner: POST /held [] "", held
            owner: POST /late [] ""
            owner: POST /responses [("host", "x"), ("content-type", "application/json"), ("content-length", "13")] "{\"model\":\"m\"}"
            owner: POST /responses [("content-length", "2")] "{}"
            owner: GET /models [("accept", "*/*")] ""
            owner: POST /headless [] "": InvalidInput
            owner: POST /empty [("content-length", "2")] "{}"
            owner: POST /interim [] ""
            owner: HEAD /models [] """#]],
    );
}

fn hold_flight(sandbox: &mut Started) -> Vec<String> {
    let held: Vec<IncomingModelRequest> = (0..MAX_MODEL_REQUESTS_IN_FLIGHT)
        .map(|_| next_model_request(&sandbox.arrivals))
        .collect();
    let refused = "until grep -qs ' 503 ' flight-*; do sleep 0.01; done";
    assert_eq!(sh(sandbox, 2, refused), ExitStatus::Code(0));
    let mut asked: Vec<String> = held.into_iter().map(respond).collect();
    asked.dedup();
    asked.iter().map(|line| format!("{line}, held")).collect()
}

fn respond_late(sandbox: &mut Started) -> String {
    // `sandboxd` has read the request, and reads no further, so the byte the client sends once
    // told to waits unread in `sandboxd`'s queue, where the command finds it.
    let incoming = next_model_request(&sandbox.arrivals);
    let late = [Put::Dir(relative("late"), dir_mode())];
    sandbox
        .session()
        .put(&absolute("/pumpkin/in"), late, hung())
        .unwrap();
    let unread = format!(
        "until awk '$2 ~ /:{MODEL_PORT:04X}$/ && $4 != \"0A\" && $5 !~ /:0+$/ {{ found = 1 }} \
         END {{ exit !found }}' /proc/net/tcp; do sleep 0.01; done"
    );
    assert_eq!(sh(sandbox, 3, &unread), ExitStatus::Code(0));
    respond(incoming)
}

fn next_model_request(arrivals: &Receiver<Arrival>) -> IncomingModelRequest {
    match arrivals.recv_timeout(Duration::from_secs(30)) {
        Ok(Arrival::ModelRequest(incoming)) => incoming,
        arrived => panic!("a model request was to arrive within 30 s, not {arrived:?}"),
    }
}

fn serve_models(sandbox: &mut Started, process: Process) -> (ExitStatus, Vec<String>) {
    let deadline = Deadline::after(Duration::from_secs(30));
    let mut asked = Vec::new();
    loop {
        let left = deadline.remaining();
        match left.and_then(|left| sandbox.arrivals.recv_timeout(left).ok()) {
            Some(Arrival::Exited {
                process: exited,
                status,
            }) if exited == process => return (status, asked),
            Some(Arrival::ModelRequest(incoming)) => asked.push(respond(incoming)),
            Some(arrival) => {
                panic!("a model request or the command's exit was to arrive, not {arrival:?}")
            }
            None => panic!("the clients ran on for 30 s, having asked {asked:?}"),
        }
    }
}

fn respond(incoming: IncomingModelRequest) -> String {
    let request = &incoming.request;
    let body = String::from_utf8_lossy(&request.body).into_owned();
    let asked = format!(
        "owner: {} {} {:?} {body:?}",
        request.method, request.path, request.headers
    );
    if request.path == "/headless" {
        // A head too long for a frame, which ends the request without a response.
        let headers = vec![("x-pad".to_owned(), "a".repeat(MAX_MESSAGE_BYTES))];
        let refused = incoming.respond(200, headers, hung()).unwrap_err();
        return format!("{asked}: {:?}", refused.kind());
    }
    if request.path == "/late" {
        // Far more than the client's buffers hold, yet within what the kernel queues to send on
        // `sandboxd`'s socket, so that much of it is still queued there as `sandboxd` closes.
        let mut response = incoming.respond(200, Vec::new(), hung()).unwrap();
        response.send("a".repeat(1 << 20).as_bytes()).unwrap();
        response.end().unwrap();
        return asked;
    }
    let (status, kind) = match (request.method.as_str(), request.path.as_str()) {
        (_, "/empty") => (204, "text/event-stream"),
        (_, "/interim") => (103, "text/event-stream"),
        ("GET", _) => (404, "application/json"),
        _ => (200, "text/event-stream"),
    };
    let headers = [("content-type", kind), ("Content-Length", "1")];
    let headers = (headers.iter())
        .map(|&(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let mut response = incoming.respond(status, headers, hung()).unwrap();
    response.send(b"data: 1\n\n").unwrap();
    match body.as_str() {
        "" => response.end().unwrap(),
        "{}" => response.cut().unwrap(),
        _ => {
            response.send(b"data: 2\n\n").unwrap();
            response.end().unwrap();
        }
    }
    asked
}

fn log(fixture: &Fixture, sandbox: &mut Started) -> Vec<String> {
    collect(fixture, sandbox, "log");
    let stdout = fs::read_to_string(fixture.dir.join("collected-log/1.stdout")).unwrap();
    let stderr = fs::read_to_string(fixture.dir.join("collected-log/1.stderr")).unwrap();
    let stderr = (!stderr.is_empty()).then(|| format!("stderr: {}", stderr.trim_end()));
    stdout.lines().map(str::to_owned).chain(stderr).collect()
}

// Bytes that are no UTF-8, 2.5 MiB of them.
fn payload() -> Vec<u8> {
    (0..5 << 19)
        .map(|n: u32| n.to_le_bytes()[0] | 0x80)
        .collect()
}

fn reply(session: &mut Session, arrival: Arrival, payload: &[u8]) -> String {
    let incoming = match arrival {
        Arrival::Op(incoming) => incoming,
        Arrival::Refused(refused) => {
            let asked = format!(
                "refused {:?}, over {} bytes",
                refused.call(),
                refused.payload_limit().bytes()
            );
            session.refuse(refused, hung()).unwrap();
            return asked;
        }
        arrival @ (Arrival::ModelRequest(_) | Arrival::Exited { .. } | Arrival::Ended { .. }) => {
            panic!("an op was to arrive, not {arrival:?}")
        }
    };
    let remaining = Remaining {
        calls: Some(1),
        seconds: None,
    };
    let intact = |bytes: &[u8]| format!("{} bytes, intact: {}", bytes.len(), bytes == payload);
    let (asked, answer) = match &incoming.op {
        Op::Ops => {
            let resources = Resources {
                cpu_millis: NonZeroU64::MIN,
                memory_bytes: NonZeroU64::MIN,
            };
            let ops = OpsReply {
                role: Name::from_static("agent"),
                remaining_seconds: 0,
                resources,
                grants: Vec::new(),
            };
            ("ops".to_owned(), OpReply::Ops(ops))
        }
        Op::Call {
            function,
            arguments,
            payload,
        } => {
            let asked = format!("call {function} {arguments:?}: {}", intact(payload));
            // `train` answers as a job function, and any other function as a now function, whose
            // body returns the caller's arguments.
            let reply = if function.as_str() == "train" {
                CallReply::Job(JobReply { job: 1, remaining })
            } else {
                let returned = (arguments.iter())
                    .map(|(name, text)| (name.as_str().to_owned(), text.as_str().into()))
                    .collect();
                CallReply::Now(NowReply {
                    returned,
                    remaining_seconds: 60,
                })
            };
            (asked, OpReply::Call(reply))
        }
        &Op::Status { job } => (format!("status {job}"), status(job, remaining)),
        Op::Session { grant, op } => session_reply(grant, op),
    };
    session.reply(incoming, answer, hung()).unwrap();
    asked
}

fn session_reply(grant: &Name, op: &SessionOp) -> (String, OpReply) {
    match op {
        SessionOp::Open { instance_seconds } => {
            (format!("{grant}.open {instance_seconds:?}"), leased(0))
        }
        SessionOp::Run {
            lease,
            argv,
            timeout_seconds,
        } => {
            let asked = format!(
                "{grant}.run {lease}: {}, {timeout_seconds:?}",
                serde_json::json!(argv)
            );
            (asked, leased(*lease))
        }
        SessionOp::Put {
            lease,
            tree,
            to,
            replace,
        } => (
            format!("{grant}.put {lease}: {tree} to {to}, replace {replace}"),
            leased(*lease),
        ),
        SessionOp::Get { lease, from, to } => (
            format!("{grant}.get {lease}: {from} to {to}"),
            leased(*lease),
        ),
        &SessionOp::Status { lease } => {
            let command = CommandStatus {
                command: 0,
                end: Some(CommandEnd::Exited(ExitStatus::Code(0))),
                stdout: "hi".to_owned(),
                stderr: String::new(),
            };
            let status = LeaseStatusReply {
                lease,
                state: LeaseState::Ready,
                elapsed_seconds: 5,
                busy_seconds: 3,
                idle_seconds: 1,
                failure: None,
                last: Some(Last::Command(command)),
                remaining: LEASE_LEFT,
            };
            (
                format!("{grant}.status {lease}"),
                OpReply::LeaseStatus(status),
            )
        }
        &SessionOp::Close { lease } => (format!("{grant}.close {lease}"), leased(lease)),
    }
}

const LEASE_LEFT: LeaseRemaining = LeaseRemaining {
    instances: 0,
    instance_seconds: 60,
    cumulative_seconds: None,
    commands: Some(3),
};

fn leased(lease: u64) -> OpReply {
    OpReply::Lease(LeaseReply {
        lease,
        remaining: LEASE_LEFT,
    })
}

fn status(job: u64, remaining: Remaining) -> OpReply {
    let state = match job {
        1 => {
            let returned = Returned::from_iter([("score".to_owned(), 1.into())]);
            JobState::Completed {
                returned,
                model_requests_over_budget: None,
            }
        }
        2 => {
            let failure = Failure {
                code: FailureCode::CallTimeout,
                message: "the call ran past its timeout".to_owned(),
            };
            JobState::Errored {
                failure,
                model_requests_over_budget: None,
            }
        }
        _ => {
            let message = format!("there is no job {job}: name one a call returned");
            return OpReply::refused(message);
        }
    };
    OpReply::Status(StatusReply {
        job,
        state,
        remaining,
    })
}
