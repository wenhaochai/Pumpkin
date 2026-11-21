//! `run`: what it finds wrong before it reaches the engine, and once it has, and runs on Docker
//! followed to their ends with `--wait`, among them one whose agent makes ops through
//! `sandboxctl`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::Permissions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::{env, fs};

use expect_test::expect;
use provider::{RunTag, Sweep as _};
use schema::sandbox::{MODEL_PORT, SANDBOXCTL_PATH};
use serde_json::{Value, json};
use test_support::{BUSYBOX, WORKFLOW, docker_context, files, sandboxd, standard_outputs};

use super::build::Labelled;
use super::{
    EC2, LOCAL, PUMPKIN, RUNNER, Scratch, StandIn, Step, build_first, check, containers, copy_tree,
    cpu_list, docker_table, engine, final_running, invocation, ledger, machine, machine_cores,
    machine_cpus, ran, record, report, siblings, through_shell, with_room, workflow,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn usage() {
    let scratch = Scratch::new();
    let same = |args: &[&str], like: &[&str]| {
        let ran = |args| scratch.pumpkin(args).output().unwrap();
        let same = ran(args) == ran(like);
        let (args, like) = (args.join(" "), like.join(" "));
        Step::Ran(format!(
            "$ pumpkin {args}\ndoes what `pumpkin {like}` does: {same}"
        ))
    };
    let pumpkin = |args: &[&str]| Step::from(scratch.pumpkin(args));
    // Each path that `wait`, `cancel` and `reap` are given is UTF-8 too, as the commands that
    // `pumpkin` says to run next name them, and each that `run` is given, as its record keeps it.
    let not_utf8 = |shown: &str, args: &[&[u8]]| {
        let mut command = scratch.pumpkin(&[]);
        command.args(args.iter().map(|arg| OsStr::from_bytes(arg)));
        let output = command.output().unwrap();
        Step::Ran(report(
            format!("$ pumpkin {shown}"),
            &output,
            &scratch.0,
            false,
        ))
    };
    let steps = [
        pumpkin(&["--help"]),
        pumpkin(&["run", "--help"]),
        pumpkin(&["wait", "-h"]),
        pumpkin(&["ls", "--help"]),
        pumpkin(&["cancel", "--help"]),
        pumpkin(&["view", "--help"]),
        pumpkin(&["reap", "--help"]),
        pumpkin(&["help", "--help"]),
        pumpkin(&["help", "config"]),
        pumpkin(&["--version"]),
        same(&["help", "run"], &["run", "--help"]),
        same(&["help"], &["--help"]),
        same(&["-h"], &["--help"]),
        same(&["-V"], &["--version"]),
        same(&["bogus", "--help"], &["--help"]),
        same(&["run", "--bogus", "-h", "extra"], &["run", "--help"]),
        pumpkin(&[]),
        pumpkin(&["bogus"]),
        pumpkin(&["--bogus", "run"]),
        pumpkin(&["--version=x", "run", "a.star"]),
        pumpkin(&["run", "--docker-context", "nowhere"]),
        pumpkin(&["run"]),
        pumpkin(&["run", "--runs=a", "--runs", "b", "a.star"]),
        pumpkin(&["run", "a.json"]),
        pumpkin(&["run", "a.star", "--arg", "model"]),
        pumpkin(&["run", "a.star", "--arg", "a=1", "--arg", "a=2"]),
        pumpkin(&["run", "--outcome", "a.star"]),
        pumpkin(&["cancel", "--force=yes", "6a"]),
        pumpkin(&["reap", "6a"]),
        not_utf8("wait $'\\xff'", &[b"wait", b"\xff"]),
        not_utf8("wait $'\\xff/x'", &[b"wait", b"\xff/x"]),
        not_utf8(
            "cancel --runs $'\\xff' 6a",
            &[b"cancel", b"--runs", b"\xff", b"6a"],
        ),
        not_utf8("reap --runs $'\\xff'", &[b"reap", b"--runs", b"\xff"]),
        not_utf8(
            "run --config $'\\xff' a.star",
            &[b"run", b"--config", b"\xff", b"a.star"],
        ),
        pumpkin(&["help", "bogus"]),
        pumpkin(&["help", "controller"]),
    ];
    // What `--version` prints: this build, which names its commit if it was made in a git
    // checkout of Pumpkin's repository.
    let this = format!("pumpkin {}\n", runtime::BUILD);
    // Filled in by `UPDATE_EXPECT=1` once the crate builds.
    check(
        &scratch,
        steps,
        false,
        &[(&this, "pumpkin <this>\n")],
        &expect![[r#"
            $ pumpkin --help
            exit status: Some(0)
            printed:
              Run workflows: in each run, an agent works in a sandbox, and a final call judges
              what it submits. The repository's .pumpkin directory holds build and provider
              state and the runs' records, unless --runs names another directory.

              Usage: pumpkin <command>

              Commands:
                check   Check tasks' sources, building and running nothing
                fmt     Format the workflow language's `.star` files
                run     Build a task's images, and start a run of it
                wait    Wait until a run has ended, and say how it ended
                ls      List the runs and their states
                cancel  Stop a run, and remove what it left
                view    Serve the runs' records to this machine's browser
                help    Print the help of a command or a topic

              Options:
                -V, --version
                        Print the version.

                -h, --help
                        Print this help.

              Rarely needed:
                build   Build a task's images, and print its workflow's IR
                reap    Remove what the runs that have ended left

              Topics:
                config  The machine's configuration, pumpkin.toml

              `pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
              does, and `pumpkin help <topic>` a topic.
            $ pumpkin run --help
            exit status: Some(0)
            printed:
              Build a task as `pumpkin build` does, then start a run of its workflow, and print
              the run's id once it has started.

              Usage: pumpkin run [OPTIONS] <entry>

              Arguments:
                <entry>
                        The task's entry file, as `pumpkin check` takes it.

              Options:
                    --arg <name>=<value>
                        The value of the arg that an arg() call declares as <name>, as
                        `pumpkin check` takes it.

                    --runs <dir>
                        The directory runs are recorded in, each in <dir>/<id>.
                        Default: runs/ in the repository's .pumpkin directory

                    --config <file>
                        The machine's configuration; see `pumpkin help config`.
                        Default: $HOME/.config/pumpkin/pumpkin.toml

                    --sandboxd <path>
                        The static sandboxd that every sandbox runs, as `cargo xtask sandboxd`
                        builds it, with the static ec2-boot beside it, which an ec2 provider's
                        instances boot with.
                        Default: the one beside this pumpkin, where `cargo xtask build` puts
                        both

                    --wait
                        Follow the run to its end, as `pumpkin wait` does, rather than print
                        its id.

                    --outcome
                        Also print the run's outcome.json on the standard output once it has
                        ended.

                -h, --help
                        Print this help.

              Examples:
                pumpkin run tasks/bench/task.star --arg model=model-a
                pumpkin run tasks/bench/task.star --arg model=model-a --wait --outcome > outcome.json

              Exit status:
                0  The run has started.
                1  An error; with --outcome, a run that failed to start prints its
                   outcome.json first.
                2  A usage error.
              With --wait, `run` exits as `pumpkin wait` does once the run has started. Every
              exit but 0 ends with a `pumpkin:` line on the standard error, which says why.

              Every ending of a run ends with the command that runs it again: `pumpkin run`
              of the task's entry, with the args and the flags that started it, their paths
              made absolute. It builds the task as its source is then. A run takes no seed:
              its id's random digits, and the grants by which its sandboxes reach it, are
              fresh in every run, so that no two runs share them.

              The task is checked, and the configuration read, before the machine's Docker
              engine is reached, warning if that engine runs as root; the task's images are
              built there; and then each provider that the configuration names is reached,
              before anything starts. The run places each role in the smallest class of
              sandbox that offers what the role needs, of every provider's, and refuses a
              role that fits none, saying why of each class, before anything is reserved.
              A driver process that `run` starts does all of that, and drives the run, and
              writes what it says after the start to driver.log in the record. Whatever ends
              `run` before the start ends the driver too; after it, the run goes on without
              `run`, and `pumpkin wait` follows it.

              On the Docker engine, each run holds whole cores of its own until it ends, which
              no other run's containers are given, each role as many as hold its CPU limit
              rounded up. A run whose roles need more cores at once than other runs leave is
              refused before its record is made: run it again once they have ended.

              The run outlives a logout unless its engine stops then, as rootless Docker does
              without `loginctl enable-linger`, or logind kills the session's processes
              (KillUserProcesses): then only a run under the user's manager outlives it, so
              enable lingering and start `run` under `systemd-run --user --scope`.

              `pumpkin cancel` stops the run by SIGTERM to the driver, whose pid is in the
              record's driver.pid, as SIGINT and SIGHUP do too: the run removes its
              sandboxes, and its outcome.json says which signal interrupted it. A signal
              ignored as `run` starts stays ignored. SIGKILL to the driver leaves the run
              lost, with its sandboxes, which `pumpkin cancel` removes: its containers,
              labelled pumpkin.run=<id>, its EC2 instances, tagged pumpkin:run=<id>, and its
              Modal sandboxes, tagged pumpkin-run=<id>, which end at the run's hard end all
              the same; and its socket directories in the record's sockets/, which `cancel`
              removes too.

              On a machine whose configuration names [controller], as `pumpkin help config`
              says, `run` takes neither --runs nor --sandboxd: it says the run's id at once,
              starts the controller if it is stopped, sends there the files of the task's
              repository that git lists and the task reaches, and starts the run's job, which
              builds the task, then runs it there, with the id; it then returns, or with
              --wait follows the job.
            $ pumpkin wait -h
            exit status: Some(0)
            printed:
              Wait until a run has ended, then say how it ended.

              Usage: pumpkin wait [OPTIONS] <run>

              Arguments:
                <run>
                        The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
                        end it, that no other run in --runs shares; or its record's path, which
                        has a `/`.

              Options:
                    --runs <dir>
                        The directory that <run> is looked up in.
                        Default: runs/ in the repository's .pumpkin directory

                    --outcome
                        Also print the run's outcome.json on the standard output once it has
                        ended.

                -h, --help
                        Print this help.

              Examples:
                pumpkin wait 20261108-000905-126a21e9
                pumpkin wait 126a
                pumpkin wait runs/20261108-000905-126a21e9
                pumpkin wait --outcome 126a > outcome.json

              Exit status:
                0  The run finished with a result.
                3  The run finished without one.
                1  The run failed, was interrupted, or was lost as its driver ended without
                   an outcome.json, which `pumpkin cancel` then records; or another error.
                2  A usage error.
              While the run goes on, `wait` says on the standard error each of its milestones
              as its record logs it: the start of work, each call's start and end, each
              lease's start, its runner's readiness and its end, and the end of the work
              phase. Once the run has ended, it says its summary: when it started, how long it
              ran, each agent's start and end, each call with how long it took and how it
              ended, each lease with how long its runner lived and was charged for, and how
              much of that a command or transfer ran, its commands and how it ended, the model
              requests of an agent with a model route and the tokens reported, what each role
              spent of each entry's model budget, and the sandboxes, by provider and class,
              with how long they lasted and how many were deleted; then how it ended, and
              where its record is, then the command that runs it again, as
              `pumpkin run --help` says, and of a lost run, the commands that remove what it
              left; any other exit but 0 ends with a `pumpkin:` line that says why.

              Ending `pumpkin wait` doesn't stop the run: `pumpkin cancel` does.

              With [controller] in the configuration, and no --runs, `wait` follows the run's
              job on the controller, its build included, then the run, and copies its record
              into the default runs directory once it has ended; a run whose copy there has
              ended needs no controller.
            $ pumpkin ls --help
            exit status: Some(0)
            printed:
              List the runs in --runs, oldest first, a line for each, then a line of totals:
              without a prefix, those that are live or ended within the last day, and the
              line of totals says how many others there are, and how to list them. A live
              run's line gives its phase, how long it has been in it, the bound at which
              Pumpkin ends the phase, its task, and what it does; an ended run's, its state,
              how long it ran, its task, and the values its result shows.

              Usage: pumpkin ls [OPTIONS] [prefix]...

              Arguments:
                [prefix]...
                        A prefix of the ids of the runs to list, such as a day's or an hour's,
                        as ids start with the UTC time of their `pumpkin run`.
                        Default: every run

              Options:
                    --runs <dir>
                        The directory of the runs to list.
                        Default: runs/ in the repository's .pumpkin directory

                -h, --help
                        Print this help.

              Examples:
                pumpkin ls
                pumpkin ls 20261110-1
                pumpkin ls --runs runs

              Exit status:
                0  The runs are listed.
                1  A run can't be listed, which `ls` says, or another error; it lists the
                   others all the same.
                2  A usage error.

              A live run's phase is `publishing` what its sandboxes run, `starting` its agents,
              `work`, `ending` the work phase, or `final call`; under it, a line for
              each call, by function, and each lease, by its grant and
              state, that hasn't ended; a phase past its bound is a bug, which `ls` says. An
              ended run's state is `finished`, `failed`, `interrupted` (by a signal), or
              `lost`, if its driver ended without an outcome.json. Its task is its workflow's
              name, or `-` if its record has no ir.json yet. A directory of --runs that is not
              there yet is no runs, if it is the default one.

              With [controller] in the configuration, and no --runs, `ls` lists the
              controller's runs, a job yet to start its run as `waiting` while it waits for a
              build slot, else as `building`, or as `unstarted` once it has ended without one,
              beside the copies in the default runs directory; and says first how long the
              controller has been up, its jobs, how full its disk is, and whether its boot
              failed.
            $ pumpkin cancel --help
            exit status: Some(0)
            printed:
              Stop a run, wait until it has ended, remove what it left, and say how it ended.

              Usage: pumpkin cancel [OPTIONS] <run>

              Arguments:
                <run>
                        The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
                        end it, that no other run in --runs shares; or its record's path, which
                        has a `/`.

              Options:
                    --runs <dir>
                        The directory that <run> is looked up in.
                        Default: runs/ in the repository's .pumpkin directory

                    --force
                        End the run's driver at once, by SIGKILL, rather than stop the run by
                        SIGTERM; the run is then lost, and recorded as failed. A create that the
                        driver had given up on may still make a sandbox after `cancel` has
                        ended, which `pumpkin reap` removes.

                    --any-member
                        Name the run among every member's runs on the controller. Without it, a
                        prefix or an id is matched among this member's runs, and those that no member
                        started, and a run of another member's is refused.

                -h, --help
                        Print this help.

              Examples:
                pumpkin cancel 126a
                pumpkin cancel --force 126a

              Exit status:
                0  The run has ended, and nothing that it left remains.
                1  An error.
                2  A usage error.
                3  The run has ended, but a provider of it can't be reached, so what it left
                   there may remain.

              A live run's driver, whose pid is in the record's driver.pid, gets SIGTERM, and
              the run stops as `pumpkin run --help` says, which `cancel` waits for; ending
              `pumpkin cancel` doesn't end the stop. A run that has ended, or is lost, gets no
              signal. Then `cancel` removes what `pumpkin reap` would of the run, on each
              provider that it ran on, Docker, EC2 or Modal, as `pumpkin reap --help` says,
              then says how the run ended, as `pumpkin wait` does.

              With [controller] in the configuration, and no --runs, `cancel` stops the job
              of a run that is yet to start on the controller, and cancels one that has
              started there, sending this machine's Modal configuration file for the reap,
              then copies its record here, as `pumpkin wait` does.
            $ pumpkin view --help
            exit status: Some(0)
            printed:
              Serve the runs in --runs, or a controller's, to a browser on this machine, until
              it is stopped, and print the address to open: a page that lists the runs as
              `ls` does, newest first, and shows a run's summary, its sandboxes' lanes, who
              talks to whom, and its log, which it follows while the run is live.

              Usage: pumpkin view [OPTIONS]

              Options:
                    --runs <dir>
                        The directory of the runs to serve.
                        Default: runs/ in the repository's .pumpkin directory

                    --port <port>
                        The port to serve at, on 127.0.0.1.
                        Default: one that is free, which `view` says

                    --export <dir>
                        Serve nothing: write into <dir> the runs' data as files that a static
                        copy of the page reads, with the page, and a robots.txt that asks
                        search engines to stay out. Each file that holds a reply that the page
                        reads, as `pumpkin view` answers it, is deflated: a host
                        serves it with `Content-Encoding: deflate`. In each reply, a stand-in
                        replaces each sandbox's id, as `<sandbox of job-3>`; the home and user
                        of this machine and of the run's arguments, as `~` and `<user>`; and
                        each string of a provider's report but its Docker engine's and its
                        processors', as `<aws.account>`; a home, user or report's string only if
                        8 bytes or more. A file is written only if its bytes changed, one whose
                        reply can't change only once, and a run that had ended at the last
                        export is read no further. Says how many files it wrote.
                        Default: serve the runs

                -h, --help
                        Print this help.

              Examples:
                pumpkin view
                pumpkin view --runs runs --port 8080

              Exit status:
                1  The port can't be bound, or another error.
                2  A usage error.
              `view` serves until a signal stops it, as Ctrl-C's SIGINT or SIGTERM does, so it
              doesn't exit 0.

              `view` prints on the standard error the address to open, such as
              `http://127.0.0.1:41873/?t=0123...`. It listens on 127.0.0.1 alone, so no other
              machine reaches it, and answers a request only if it names this machine as its
              host, and, for the runs' data, carries the token after `?t=`, which the page
              sends: another user of this machine can't read the runs, nor can a page of
              another site that the browser has open. It reads the records as `ls` and `wait`
              do, and of the providers asks only EC2, for a controller's state and its start,
              as below.

              A run's page links to the trace of each caller that used a model, an agent or
              a call: its model requests as threads of steps, each with the entries it
              added and the model's answer, as the record's log and the bodies it keeps say.

              The page's colours follow the browser's light or dark setting; the choice in its
              header overrides that with light or dark, and the browser keeps the choice.
              `&scheme=light`, `&scheme=dark` or `&scheme=system` after the token picks one in
              the address.

              The page's cancel of a run runs `pumpkin cancel --runs <dir> <run>`, or of a
              controller's run `pumpkin cancel <run>`, and shows what that says. Nothing else
              the page does changes a run.

              With [controller] in the configuration, and no --runs, `view` serves the
              controller's runs that have started, and reads them there: it sends each of the
              page's requests for the runs' data over one `ssh` to `pumpkin controller serve`
              on the controller, a copy of this `pumpkin` that it installs there if the
              controller has none. It reaches the controller as it starts, starting it if it
              is stopped; once the connection has ended, the next request opens it again, and
              while that fails the page says so and asks again after 5 s, then after twice
              the wait before, up to a minute. When it fails as the controller has stopped
              since, as an idle controller stops itself, the page says so beside a button,
              "Start it", which starts it (`aws ec2 start-instances`); until the
              controller answers, each request asks EC2 whether it runs (`aws ec2
              describe-instances`) in place of connecting. --runs serves a directory here,
              such as the copies of the controller's runs that `wait` makes.
            $ pumpkin reap --help
            exit status: Some(0)
            printed:
              Remove what ended or lost runs left and print the run ids reported as cleaned.

              Usage: pumpkin reap [OPTIONS]

              Options:
                    --runs <dir>
                        The directory of the runs to reap.
                        Default: runs/ in the repository's .pumpkin directory

                -h, --help
                        Print this help.

              Examples:
                pumpkin reap
                pumpkin reap --runs runs

              Exit status:
                0  Nothing that the selected runs left remains.
                1  A run can't be reaped, which `reap` says, or another error; it reaps the
                   others all the same.
                2  A usage error.
                3  Only that a provider of a run, which `reap` names, can't be reached, as
                   when its Docker context is gone, or the configuration that ran the run no
                   longer names it, so what the run left there may remain.

              Of each run whose driver has ended, `reap` deletes the sandboxes of the run that
              remain on each provider that it ran on, such as one that a create given up at
              its deadline made: on the Docker engine of the context that the record's
              provider.json names, the containers labelled pumpkin.run=<id>; and on each
              provider of kind ec2 or modal, as the configuration that ran the run names it,
              the one its record.json's flags name or else the default, the EC2 instances
              tagged pumpkin:run=<id>, or the Modal sandboxes tagged pumpkin-run=<id>. It
              removes the run's socket directories, in the record's sockets/, and a lost run's
              record it marks failed, as its driver ended without an outcome.json. It leaves a
              live run to its driver: `pumpkin cancel` stops one.

              Each sandbox that `reap` removes it notes in the record's reaps.jsonl, with the
              time; and each that the record holds undeleted, but that its provider no longer
              lists, as when the configuration now points to another region or profile, it
              says, and notes there as gone.

              With [controller] in the configuration, and no --runs, `reap` reaps the
              controller's runs made by this build and names the runs left to their own
              builds. It uses this machine's Modal configuration file, which the boot lacks.
              If the reply is cut, it reaps again. The printed ids may omit runs already
              cleaned before the cut.
            $ pumpkin help --help
            exit status: Some(0)
            printed:
              Print the help of a command or a topic, or `pumpkin --help` without one.

              Usage: pumpkin help [topic]

              Arguments:
                [topic]
                        A command, or a topic: `config`, the machine's configuration.

              Options:
                -h, --help
                        Print this help.

              Examples:
                pumpkin help run
                pumpkin help config

              `pumpkin help <command>` prints what `pumpkin <command> --help` does.
            $ pumpkin help config
            exit status: Some(0)
            printed:
              pumpkin.toml, the machine's configuration, which every `pumpkin run` and
              `pumpkin build` reads: the file that --config names,
              $HOME/.config/pumpkin/pumpkin.toml by default. It is TOML, with the tables below.
              Every key is required, but those of [gateway], the [upstreams.<name>] tables,
              [publish] and its `since`, and any other key is refused. [controller] takes the
              place of [docker], [providers.<name>.<kind>] and [gateway] on a machine whose
              runs go to the person's controller.

              Example, of a machine whose runs go to EC2 and Modal:
                [docker]
                context = "default"

                [providers.aws.ec2]
                region = "us-west-1"
                repositories = { roles = "pumpkin/roles", sandboxd = "pumpkin/sandboxd" }

                [providers.aws.ec2.timer]
                group = "pumpkin"
                role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer"

                [providers.aws.ec2.classes.cpu]
                template = "lt-0123456789abcdef0"
                version = 1
                vcpus = 8
                memory_mib = 16384
                cpu_features = ["avx2"]

                [providers.gpus.modal]
                profile = "pumpkin"
                dials_from = "203.0.113.7"

                [providers.gpus.modal.images]
                region = "us-west-1"
                repository = "pumpkin/roles"
                role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull"

                [providers.gpus.modal.classes.a100]
                gpu = "A100-80GB"
                gpu_model = "NVIDIA A100-SXM4-80GB"
                cpu_millis = 8000
                memory_bytes = 68719476736
                startup_seconds = 1800

                [gateway]
                private = "172.31.0.10:7433"

                [upstreams.gateway]
                url = "https://gateway.example/v1"
                key_file = "gateway-key"

                [[upstreams.gateway.models]]
                name = "model-a"
                upstream_name = "provider/model-a"
                usd_per_mtok = { input = 1.25, cached = 0.125, output = 10 }

              Example, of a machine whose runs go to the person's controller, and Modal:
                [controller]
                name = "alice"
                region = "us-west-1"

                [controller.modal]
                profile = "pumpkin"

                [controller.modal.classes.a100]
                gpu = "A100-80GB"
                gpu_model = "NVIDIA A100-SXM4-80GB"
                cpu_millis = 8000
                memory_bytes = 68719476736
                startup_seconds = 1800

              [docker], the machine's Docker engine:
                context       The context that reaches it, as `docker context ls` lists it.
                cpus          The CPUs that its sandboxes may be given, each run whole cores
                              of its own, as the kernel lists CPUs, such as "1-7,9-15";
                              absent, all that are online.

              [providers.<name>.<kind>], for each provider that the machine's runs may place
              roles on, as provider <name>, of kind <kind>, whose table holds that kind's own
              settings:
                docker        Sandboxes on the engine of [docker]; it takes no settings. A
                              provider of this kind is the configuration's one provider, as
                              a machine runs its roles either on its own engine or on others,
                              of kind ec2 and modal. Its one class, `container`, offers the
                              CPUs that sandboxes may be given, the machine's memory, the
                              flags that /proc/cpuinfo lists for every CPU, no CPU model and
                              no GPU.
                ec2           Sandboxes on EC2 instances of their own, in the envelope that
                              its settings name, which pull their images from it, pushed
                              there from the engine of [docker]; they dial [gateway]'s
                              private address. It runs `aws`, AWS CLI v2, from PATH, with the
                              credentials that `aws` finds, such as the instance's role.
                              Its settings:
                  region        The envelope's AWS region, which every call names.
                  classes       Each class of sandbox, by its name; one at least: its
                                `template`, the ID of its launch template, and `version`, the
                                template's version that its instances launch from; `vcpus`
                                and `memory_mib`, its instance type's CPUs and memory, in
                                MiB, of which a role gets all the CPUs and all the memory but
                                a share the instance keeps; and `cpu_features`, its CPU's
                                flags, and `cpu_model`, its CPU's model, as /proc/cpuinfo
                                names them: without `cpu_features`, the class states no
                                flag, and without `cpu_model`, no model, and a class that
                                states a model is offered only to roles that need it; and
                                `usd_per_hour`, what an instance costs an hour, if given,
                                by which `ls` and a run's summary price its sandboxes; and
                                `startup_seconds`, the most seconds an instance's start may
                                take, its images' pulls among them: absent, 1740.
                  repositories  The envelope's ECR repositories: `roles`, of the roles'
                                images, and `sandboxd`, of the images that hold sandboxd.
                  timer         The sandboxes' timer, which terminates each instance at its
                                run's hard end: `group`, its EventBridge Scheduler schedule
                                group, and `role_arn`, the ARN of the role that its schedules
                                terminate instances with.
                modal         Sandboxes in a Modal workspace, which run images that Modal
                              imports from an ECR repository, pushed there from the engine of
                              [docker]; each listens behind Modal's TLS tunnel, which the run
                              dials. It runs Modal's helper, pumpkin-modal.py, beside
                              `pumpkin`, with `uv` from PATH, and `aws`, AWS CLI v2, as ec2
                              does. Its settings:
                  profile       The profile of Modal's configuration whose token reaches the
                                workspace.
                  config        The file of Modal's configuration that holds the profile, by
                                its absolute path; absent, ~/.modal.toml.
                  dials_from    This machine's public IPv4 address, from which alone the
                                sandboxes' tunnels take connections.
                  images        Where Modal imports images from: `region`, `repository`,
                                an ECR repository there, and `role_arn`, the ARN of the AWS
                                role that Modal pulls with, which trusts Modal's identity.
                  classes       Each class of sandbox, by its name; one at least: its
                                `cpu_millis`, `memory_bytes`, in whole MiB, and
                                `startup_seconds`, and its `gpu`, if any, as Modal names
                                one, such as "H100!", or "A100-80GB:2" for two, with
                                `gpu_model`, its model as nvidia-smi names it, such as
                                "NVIDIA H100 80GB HBM3". A class with a GPU is offered
                                only to roles that need its GPUs. With `usd_per_hour`, what
                                a sandbox costs an hour, `ls` and a run's summary price its
                                sandboxes.

              [gateway], where the gateway meets the sandboxes that dial it over a network:
                private       The address, ip:port, that the sandboxes of an ec2 provider
                              dial: one of this machine's on the envelope's network, at a
                              port that its security group admits. A run listens there while
                              it places a role on such a provider, so one such run at a time.

              [upstreams.<name>], for each upstream that a workflow's model route may reach:
                url           The base URL of its API: https, or http to a loopback IP
                              address, such as 127.0.0.1 but not localhost. The route reaches
                              each API's endpoint below it: /responses, /chat/completions, or
                              /messages, which an Anthropic Messages API serves, such as
                              https://api.anthropic.com/v1.
                key_file      The file that holds its key, below this file's directory unless
                              absolute. The route sends it as `authorization: Bearer <key>`,
                              and to /messages as `x-api-key: <key>`.
                models        The models it serves, each with the `name` that a route allows
                              and the `upstream_name` that the upstream takes for it, and,
                              if given, `usd_per_mtok`, what its tokens cost in US dollars a
                              million: `input` and `output`, and `cached` and `cache_write`,
                              the input that the upstream read from its cache and wrote to
                              it. Reasoning is output. A model without `usd_per_mtok` has no
                              cost, and nor has one whose responses report a kind of token
                              that its prices lack. No model is listed twice, under one
                              upstream or two.

              Each entry of a workflow's model route reaches the one upstream that serves
              every model it allows.

              [publish], on the controller, which deploy/aws writes into its settings: where
              each run's job publishes the runs, as `pumpkin controller publish --help` says;
              without it, nothing is published:
                bucket        The S3 bucket of the public page.
                distribution  The ID of the CloudFront distribution that serves the bucket.
                address       The page's address, which each job's log names.
                since         An instant, as a TOML offset date-time, such as
                              2026-11-15T00:00:00Z, unquoted: a run that started before it is
                              not published; absent, every run is.

              [controller], the person's controller, which deploy/aws makes, and to which
              `run`, `wait`, `ls`, `cancel`, `build` and `reap` go, as each one's help says.
              Each run is sent the [upstreams.<name>] tables, their keys, [controller.modal]
              and Modal's configuration file, into a directory of its own in the
              controller's tmpfs, which its end removes. The client uses AWS CLI v2, ssh,
              ssh-keygen and git from PATH. It sends the task's files and starts its job
              over SSH to the controller's private address, using up to four connections
              for large task uploads. The machine must have a route there and deploy/aws's
              `ssh_from` must admit it. The job runs the client's install with the module's
              settings and [controller.modal].
              The first command creates a key and enrols it through EC2 Instance Connect.
              The client keeps that key and the pinned host key in $HOME/.config/pumpkin/ssh/.
                name          The `name` that deploy/aws was applied with.
                region        The region it was applied in.

              [controller.modal], the Modal provider that the controller's runs may place
              roles on, if any: the settings of a provider of kind modal, but `images`, which
              the module's settings give, and `dials_from`, which each run sets:
                profile       As modal's.
                config        Modal's configuration file, below this file's directory unless
                              absolute; absent, ~/.modal.toml, if it is there.
                classes       As modal's.
            $ pumpkin --version
            exit status: Some(0)
            printed:
              pumpkin <this>
            $ pumpkin help run
            does what `pumpkin run --help` does: true
            $ pumpkin help
            does what `pumpkin --help` does: true
            $ pumpkin -h
            does what `pumpkin --help` does: true
            $ pumpkin -V
            does what `pumpkin --version` does: true
            $ pumpkin bogus --help
            does what `pumpkin --help` does: true
            $ pumpkin run --bogus -h extra
            does what `pumpkin run --help` does: true
            $ pumpkin
            exit status: Some(2)
            Run workflows: in each run, an agent works in a sandbox, and a final call judges
            what it submits. The repository's .pumpkin directory holds build and provider
            state and the runs' records, unless --runs names another directory.

            Usage: pumpkin <command>

            Commands:
              check   Check tasks' sources, building and running nothing
              fmt     Format the workflow language's `.star` files
              run     Build a task's images, and start a run of it
              wait    Wait until a run has ended, and say how it ended
              ls      List the runs and their states
              cancel  Stop a run, and remove what it left
              view    Serve the runs' records to this machine's browser
              help    Print the help of a command or a topic

            Options:
              -V, --version  Print the version.
              -h, --help     Print this help.

            Rarely needed:
              build   Build a task's images, and print its workflow's IR
              reap    Remove what the runs that have ended left

            Topics:
              config  The machine's configuration, pumpkin.toml

            `pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
            does, and `pumpkin help <topic>` a topic.
            pumpkin: to read how to use it: pumpkin help
            $ pumpkin bogus
            exit status: Some(2)
            error: unrecognized subcommand 'bogus'

            Usage: pumpkin <command>
            pumpkin: to read how to use it: pumpkin help
            $ pumpkin --bogus run
            exit status: Some(2)
            error: unexpected argument '--bogus' found

            Usage: pumpkin <command>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin --version=x run a.star
            exit status: Some(2)
            error: unexpected value 'x' for '--version' found; no more were expected

            Usage: pumpkin --version <command>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run --docker-context nowhere
            exit status: Some(2)
            error: unexpected argument '--docker-context' found

              tip: to pass '--docker-context' as a value, use '-- --docker-context'

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run
            exit status: Some(2)
            error: the following required arguments were not provided:
              <entry>

            Usage: pumpkin run <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run --runs=a --runs b a.star
            exit status: Some(2)
            error: the argument '--runs <dir>' cannot be used multiple times

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run a.json
            exit status: Some(2)
            error: invalid value 'a.json' for '<entry>': `a.json` names no `.star` file: name a task's entry file, whose name ends in `.star`
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run a.star --arg model
            exit status: Some(2)
            error: `--arg model` has no `=`: give it as `--arg <name>=<value>`

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run a.star --arg a=1 --arg a=2
            exit status: Some(2)
            error: `--arg a=2` names an arg given already: give each arg once

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run --outcome a.star
            exit status: Some(2)
            error: the following required arguments were not provided:
              --wait

            Usage: pumpkin run --wait --outcome <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin cancel --force=yes 6a
            exit status: Some(2)
            error: unexpected value 'yes' for '--force' found; no more were expected

            Usage: pumpkin cancel --force <run>
            pumpkin: to read how to use it: pumpkin help cancel
            $ pumpkin reap 6a
            exit status: Some(2)
            error: unexpected argument '6a' found

            Usage: pumpkin reap [OPTIONS]
            pumpkin: to read how to use it: pumpkin help reap
            $ pumpkin wait $'\xff'
            exit status: Some(2)
            error: invalid value '�' for '<run>': it isn't UTF-8, as every id is: name an id, a prefix of one, or a record's path, which has a `/`
            pumpkin: to read how to use it: pumpkin help wait
            $ pumpkin wait $'\xff/x'
            exit status: Some(2)
            error: invalid value '�/x' for '<run>': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a record with a UTF-8 path
            pumpkin: to read how to use it: pumpkin help wait
            $ pumpkin cancel --runs $'\xff' 6a
            exit status: Some(2)
            error: invalid value '�' for '--runs <dir>': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a directory with a UTF-8 path
            pumpkin: to read how to use it: pumpkin help cancel
            $ pumpkin reap --runs $'\xff'
            exit status: Some(2)
            error: invalid value '�' for '--runs <dir>': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a directory with a UTF-8 path
            pumpkin: to read how to use it: pumpkin help reap
            $ pumpkin run --config $'\xff' a.star
            exit status: Some(2)
            error: invalid value '�' for '--config <file>': it isn't UTF-8, as a run's record keeps each path that `run` is given: name one that is
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin help bogus
            exit status: Some(2)
            error: there is no help for `bogus`: expected a command (`check`, `fmt`, `run`, `wait`, `ls`, `cancel`, `view`, `help`, `build`, `reap`) or a topic (`config`)

            Usage: pumpkin help [topic]
            pumpkin: to read how to use it: pumpkin help help
            $ pumpkin help controller
            exit status: Some(2)
            error: there is no help for `controller`: expected a command (`check`, `fmt`, `run`, `wait`, `ls`, `cancel`, `view`, `help`, `build`, `reap`) or a topic (`config`)

            Usage: pumpkin help [topic]
            pumpkin: to read how to use it: pumpkin help help"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn unrunnable() {
    let scratch = Scratch::new();
    scratch.write_task(
        "workflow",
        &serde_json::from_str::<Value>(WORKFLOW).unwrap(),
    );
    scratch.write("empty/task.star", "");
    scratch.write_sandboxd("sandboxd");
    scratch.write_sandboxd("private");
    fs::set_permissions(scratch.0.join("private"), Permissions::from_mode(0o750)).unwrap();
    scratch.write_executable("script", b"#!/bin/sh\n");
    // Links to `pumpkin`: one alone, and one beside a `sandboxd` that is `pumpkin` too, linked
    // dynamically, both in no `.pumpkin`, and one in a workspace whose path isn't UTF-8.
    let (alone, checkout) = (scratch.0.join("alone"), scratch.0.join("checkout"));
    let unnamed = scratch.0.join(OsStr::from_bytes(b"\xff/.pumpkin/pinned"));
    for dir in [&alone, &checkout, &unnamed] {
        fs::create_dir_all(dir).unwrap();
        fs::hard_link(PUMPKIN, dir.join("pumpkin")).unwrap();
    }
    fs::hard_link(PUMPKIN, checkout.join("sandboxd")).unwrap();
    let runs = ["--runs", "runs"];
    let run = |entry, rest: &[&str]| scratch.pumpkin(&[&["run", entry], &runs[..], rest].concat());
    let dashed = [
        &["run"],
        &runs[..],
        &["--sandboxd", "sandboxd", "--", "-missing.star"],
    ];
    let dashed = scratch.pumpkin(&dashed.concat());
    let beside = |dir: &PathBuf| {
        let args = [&["run", "workflow/task.star"], &runs[..]].concat();
        scratch.program(dir.join("pumpkin"), &args)
    };
    let outside = |args: &[&str]| scratch.program(alone.join("pumpkin"), args);
    let not_utf8 = |args: &[&str]| scratch.program(unnamed.join("pumpkin"), args);
    let mut unset = scratch.pumpkin(&["run", "workflow/task.star", "--sandboxd", "sandboxd"]);
    unset.env_remove("HOME");
    let commands = [
        run("missing/task.star", &["--sandboxd", "sandboxd"]),
        dashed,
        run("empty/task.star", &["--sandboxd", "sandboxd"]),
        run("workflow/task.star", &["--sandboxd", "missing"]),
        run("workflow/task.star", &["--sandboxd", "private"]),
        run("workflow/task.star", &["--sandboxd", "script"]),
        beside(&alone),
        beside(&checkout),
        outside(&["run", "workflow/task.star", "--sandboxd", "sandboxd"]),
        outside(&["wait", "6a"]),
        not_utf8(&["run", "workflow/task.star", "--sandboxd", "sandboxd"]),
        not_utf8(&["wait", "6a"]),
        not_utf8(&["cancel", "6a"]),
        not_utf8(&["reap"]),
        unset,
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin run missing/task.star --runs runs --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: the entry's directory missing can't be found (No such file or directory (os error 2)): name the entry by a path that exists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/missing/task.star
            $ pumpkin run --runs runs --sandboxd sandboxd -- -missing.star
            exit status: Some(1)
            -missing.star: `-missing.star` doesn't exist: name a regular file below the root that does
            pumpkin: -missing.star: the task has 1 problem
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/-missing.star
            $ pumpkin run empty/task.star --runs runs --sandboxd sandboxd
            exit status: Some(1)
            empty/task.star: this file calls no `workflow()`: call it once, with the workflow
            pumpkin: empty/task.star: the task has 1 problem
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/empty/task.star
            $ pumpkin run workflow/task.star --runs runs --sandboxd missing
            exit status: Some(1)
            pumpkin: the sandboxd missing can't be read (No such file or directory (os error 2)): name a static one, which `cargo xtask sandboxd` builds
            $ pumpkin run workflow/task.star --runs runs --sandboxd private
            exit status: Some(1)
            pumpkin: the sandboxd private can't be executed by every user, as its mode is 0750: make it executable by every user (`chmod a+x`)
            $ pumpkin run workflow/task.star --runs runs --sandboxd script
            exit status: Some(1)
            pumpkin: the sandboxd script isn't a 64-bit ELF executable: name a static one, which `cargo xtask sandboxd` builds
            $ <scratch>/alone/pumpkin run workflow/task.star --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/alone/sandboxd, can't be read (No such file or directory (os error 2)): `cargo xtask build` puts `pumpkin` beside a static one, or name one with `--sandboxd`
            $ <scratch>/checkout/pumpkin run workflow/task.star --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/checkout/sandboxd, is linked dynamically, and a sandbox runs only a static one: `cargo xtask build` puts `pumpkin` beside a static one, or name one with `--sandboxd`
            $ <scratch>/alone/pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: this `pumpkin`, <scratch>/alone/pumpkin, is in no directory named `.pumpkin`, which would hold all it writes: run the one that a repository's `./pumpkin` launches, or that `cargo xtask build` puts in a checkout's `.pumpkin/bin`
            $ <scratch>/alone/pumpkin wait 6a
            exit status: Some(1)
            pumpkin: this `pumpkin`, <scratch>/alone/pumpkin, is in no directory named `.pumpkin`, which would hold all it writes: run the one that a repository's `./pumpkin` launches, or that `cargo xtask build` puts in a checkout's `.pumpkin/bin`
            $ <scratch>/�/.pumpkin/pinned/pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ <scratch>/�/.pumpkin/pinned/pumpkin wait 6a
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ <scratch>/�/.pumpkin/pinned/pumpkin cancel 6a
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ <scratch>/�/.pumpkin/pinned/pumpkin reap
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ env --unset=HOME pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

/// `wait` finds the `outcome.json` of each run recorded in `scratch`'s `runs`, which have
/// ended, and with `sandboxd`, shown as `<sandboxd>`: the engine is not reached.
fn wait_for_each_run(scratch: &Scratch, sandboxd: &str) {
    let mut ids: Vec<String> = (fs::read_dir(scratch.0.join("runs")).unwrap())
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    ids.sort();
    let waits: Vec<Command> = (ids.iter())
        .map(|id| scratch.pumpkin(&["wait", id, "--runs", "runs"]))
        .collect();
    let mut shown: Vec<(&str, &str)> = ids.iter().map(|id| (&id[..], "<id>")).collect();
    shown.push((sandboxd, "<sandboxd>"));
    check(
        scratch,
        waits,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                        model: 1 request, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 1 good answer, by the median, <t> to the end
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=1
                        model: 1 request, all answered 200; no response reported its tokens; of 1 good answer, by the median, <t> to the end
              model: 2 requests, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 2 good answers, by the median, <t> to the end
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=1); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/asks/task.star
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=3
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=3); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/submits/task.star
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: failed: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished without a result: the final call gave none (error: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)
                18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}
                   |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `final_call()`, called at fails/final.star:3:5); its commands' logs are in <scratch>/runs/<id>/jobs/0/log; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/fails/task.star"#]],
    );
}

#[test]
fn modes() {
    let shown = [0o002, 0o077].map(modes_under);
    expect![[r#"
        $ umask 002 && PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/runs --config pumpkin.toml
        shared: 755
        shared/runs: 755
        the record: 755
        $ umask 077 && PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/runs --config pumpkin.toml
        shared: 700
        shared/runs: 700
        the record: 700"#]].assert_eq(&shown.join("\n"));
}

/// What `pumpkin run` under `umask`, with a runs directory whose parent is missing too, shows:
/// its invocation, then the modes of the directories it made.
fn modes_under(umask: libc::mode_t) -> String {
    let scratch = Scratch::new();
    let _stand_in = StandIn::new(&scratch);
    let mut run = scratch.pumpkin(&["run", "lacking/task.star", "--sandboxd", "sandboxd"]);
    run.args(["--runs", "shared/runs", "--config", "pumpkin.toml"]);
    let bin = scratch.0.join("bin");
    run.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    let invoked = under_umask(&mut run, umask);
    let output = run.output().unwrap();
    let mut lines = vec![invoked.replace(scratch.0.to_str().unwrap(), "<scratch>")];
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (_, record) = record(&stderr).unwrap();
    for (name, path) in [
        ("shared", scratch.0.join("shared")),
        ("shared/runs", scratch.0.join("shared/runs")),
        ("the record", PathBuf::from(record)),
    ] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        lines.push(format!("{name}: {mode:o}"));
    }
    lines.join("\n")
}

#[test]
fn ancestors() {
    let scratch = Scratch::new();
    let dirs = [
        // Not writable by its group, which may or may not be the user's private group.
        ("shared", 0o757),
        ("shared/sealed", 0o555),
        ("sticky", 0o1777),
        ("own", 0o755),
    ];
    for (dir, mode) in dirs {
        fs::create_dir(scratch.0.join(dir)).unwrap();
        fs::set_permissions(scratch.0.join(dir), Permissions::from_mode(mode)).unwrap();
    }
    symlink("own", scratch.0.join("link")).unwrap();
    let _stand_in = StandIn::new(&scratch);
    check(
        &scratch,
        [
            run_into(&scratch, "shared/runs"),
            run_into(&scratch, "shared/sealed"),
            run_into(&scratch, "sticky/runs"),
            run_into(&scratch, "link/runs"),
        ],
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: `<scratch>/shared` is writable by others: expected a directory that only its owner, or a group that holds only this process's user, may write, or a sticky one, as `chmod o-w <scratch>/shared` makes it
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/sealed --config pumpkin.toml
            exit status: Some(1)
            pumpkin: `<scratch>/shared` is writable by others: expected a directory that only its owner, or a group that holds only this process's user, may write, or a sticky one, as `chmod o-w <scratch>/shared` makes it
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs sticky/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/sticky/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it; its record is <scratch>/sticky/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/sticky/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lacking/final.star, sources/task/lacking/submit.star, sources/task/lacking/task.star, sources/task/lib/train.star
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it","state":"failed"}
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs link/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/own/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it; its record is <scratch>/own/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/link/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lacking/final.star, sources/task/lacking/submit.star, sources/task/lacking/task.star, sources/task/lib/train.star
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it","state":"failed"}"#]],
    );
    for runs in ["shared/runs", "shared/sealed"] {
        let records = fs::read_dir(scratch.0.join(runs)).unwrap().count();
        assert_eq!(records, 0, "a refusal left a record in {runs}");
    }
}

#[test]
fn ancestors_writable_by_their_group() {
    let scratch = Scratch::new();
    let private = scratch.0.join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, Permissions::from_mode(0o775)).unwrap();
    let _stand_in = StandIn::new(&scratch);
    let expected = if is_private_group(&private) {
        expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs private/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/private/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it; its record is <scratch>/private/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/private/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json, sources/task/lacking/final.star, sources/task/lacking/submit.star, sources/task/lacking/task.star, sources/task/lib/train.star
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it","state":"failed"}"#]]
    } else {
        expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs private/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: `<scratch>/private` is writable by its group: expected a directory that only its owner, or a group that holds only this process's user, may write, or a sticky one, as `chmod g-w <scratch>/private` makes it"#]]
    };
    check(
        &scratch,
        [run_into(&scratch, "private/runs")],
        false,
        &[],
        &expected,
    );
}

/// `pumpkin run` in `scratch`, recording into `runs`, with the stand-in's `docker` first on the
/// `PATH`.
fn run_into(scratch: &Scratch, runs: &str) -> Command {
    let mut command = scratch.pumpkin(&["run", "lacking/task.star", "--sandboxd", "sandboxd"]);
    command.args(["--runs", runs, "--config", "pumpkin.toml"]);
    let bin = scratch.0.join("bin");
    command.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    command
}

/// Whether the group of `dir` is the private group of its owner, the test's user, by
/// `/etc/passwd` and `/etc/group`: that user's primary group and no other user's, which
/// `/etc/group` lists with no member but that user.
fn is_private_group(dir: &Path) -> bool {
    // Field `n` of `entry`: its name first, its id third, and fourth a user's primary group, or
    // a group's members.
    fn field(entry: &[String], n: usize) -> Option<&str> {
        entry.get(n).map(String::as_str)
    }
    let entries = |file| -> Vec<Vec<String>> {
        let text = fs::read_to_string(file).unwrap();
        (text.lines())
            .map(|line| line.split(':').map(str::to_owned).collect())
            .collect()
    };
    let metadata = fs::metadata(dir).unwrap();
    let (uid, gid) = (metadata.uid().to_string(), metadata.gid().to_string());
    let (uid, gid) = (Some(uid.as_str()), Some(gid.as_str()));
    let (users, groups) = (entries("/etc/passwd"), entries("/etc/group"));
    let Some(user) = (users.iter()).find(|user| field(user, 2) == uid) else {
        return false;
    };
    let others = (users.iter()).any(|other| field(other, 2) != uid && field(other, 3) == gid);
    let members: Vec<&str> = (groups.iter())
        .filter(|group| field(group, 2) == gid)
        .map(|group| field(group, 3).unwrap_or(""))
        .collect();
    field(user, 3) == gid
        && !others
        && !members.is_empty()
        && (members.iter().flat_map(|members| members.split(',')))
            .all(|member| member.is_empty() || member == user[0])
}

/// Has `command` run under `umask` ([`through_shell`]), and returns its invocation, as
/// [`invocation`] shows it, after `umask` and the mask.
fn under_umask(command: &mut Command, umask: libc::mode_t) -> String {
    let invoked = format!("$ umask {umask:03o} && {}", &invocation(command)[2..]);
    *command = through_shell(command, &format!("umask {umask:03o}"));
    invoked
}

/// Each mode that the directories below `dir`, and the files, have, in octal.
fn modes_below(dir: &Path) -> String {
    let (mut dirs, mut files) = (BTreeSet::new(), BTreeSet::new());
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let mode = format!("{:o}", metadata.permissions().mode() & 0o7777);
            if metadata.is_dir() {
                dirs.insert(mode);
                pending.push(path);
            } else {
                files.insert(mode);
            }
        }
    }
    let join = |modes: BTreeSet<String>| modes.into_iter().collect::<Vec<_>>().join(", ");
    format!("directories {}; files {}", join(dirs), join(files))
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn refusals() {
    let scratch = Scratch::new();
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    scratch.write_task("workflow", &ir);
    let mut budgeted = ir.clone();
    budgeted["roles"][0]["runtime"]["agent"]["egress"]["model"][0]["budget"] =
        json!({ "microusd": 1_000_000 });
    scratch.write_task("budgeted", &budgeted);
    ir["roles"][1]["name"] = json!("agent");
    // In directories named so that the command that runs it again quotes it, for its `'`, or
    // `~` alone, each the root of its tasks.
    scratch.write_task_in("it's broken", "tasks", &ir);
    scratch.write_task_in("a~b", "tasks", &ir);
    write_configurations(&scratch);
    let stand_in = StandIn::new(&scratch);
    let mut large = workflow(&format!("sha256:{}", "0".repeat(64)), &["true"]);
    large["roles"][1]["needs"]["cpu_millis"] = json!(i64::MAX);
    large["roles"][2]["needs"]["gpu"] = json!({ "count": 1, "model": "NVIDIA H100 80GB HBM3" });
    scratch.write_task("large", &large);
    let mut wide = workflow(&format!("sha256:{}", "0".repeat(64)), &["true"]);
    let cpus = machine_cpus();
    wide["roles"][0]["needs"]["cpu_millis"] = json!(cpus * 1000);
    wide["roles"][1]["needs"]["cpu_millis"] = json!(1000);
    scratch.write_task("wide", &wide);
    scratch.write("plain.toml", &machine("default"));
    // Two CPUs given, of which the agent's take both.
    wide["roles"][0]["needs"]["cpu_millis"] = json!(2000);
    scratch.write_task("narrow", &wide);
    let given =
        |cpus: &str| format!("[docker]\ncontext = \"default\"\ncpus = \"{cpus}\"\n\n{LOCAL}");
    scratch.write("offline.toml", &given("0,4096-4097"));
    scratch.write("narrow.toml", &given("0-1"));
    scratch.write("backward.toml", &given("3-1"));
    scratch.write("empty.toml", &given(""));
    let run = |entry, config: &str| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs", "--config", config]);
        command
    };
    let on_stand_in = |entry, config: &str| {
        let mut command = run(entry, config);
        let bin = scratch.0.join("bin");
        command.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        command
    };
    // Paths that need quoting for a space alone, and for a newline, which makes the command
    // span two lines.
    let mut quoted = scratch.pumpkin(&["run", "a~b/tasks/task.star", "--sandboxd", "sandboxd"]);
    quoted.args(["--runs", "two\nlines", "--config", "my served.toml"]);
    let steps = [
        Step::from(run("it's broken/tasks/task.star", "conf/served.toml")),
        Step::from(quoted),
        Step::from(run("workflow/task.star", "unserved.toml")),
        // Refused before the engine is reached, as their Docker context names none.
        Step::from(run("budgeted/task.star", "conf/uncached.toml")),
        Step::from(run("budgeted/task.star", "conf/unwritten.toml")),
        Step::from(run("budgeted/task.star", "conf/uncosted.toml")),
        Step::from(stand_in.run("large/task.star", &[])),
        Step::from(on_stand_in("wide/task.star", "plain.toml")),
        Step::from(on_stand_in("wide/task.star", "backward.toml")),
        Step::from(on_stand_in("wide/task.star", "empty.toml")),
        Step::from(on_stand_in("wide/task.star", "offline.toml")),
        Step::from(on_stand_in("narrow/task.star", "narrow.toml")),
    ];
    let cores = machine_cores();
    let shown = [
        (format!("offers {cpus}000 "), "offers <cpus>000 ".to_owned()),
        (format!("`agent` {cpus},"), "`agent` <cpus>,".to_owned()),
        (format!("the {cores} that"), "the <cores> that".to_owned()),
        (
            format!("CPUs of 0-{}", cpus - 1),
            "CPUs of 0-<cpus - 1>".to_owned(),
        ),
    ];
    let shown: Vec<(&str, &str)> = (shown.iter())
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect();
    check(
        &scratch,
        steps,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin run it's broken/tasks/task.star --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            tasks/task.star:9:895: `agent` also names /roles/0: give each a unique name
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               ^^^^^^^
                the IR's /roles/1/name
            tasks/task.star:9:674: no role is named `trainer`: name one listed in `roles`
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  ^^^^^^^^^
                the IR's /roles/0/runtime/agent/grants/0/job/target
            tasks/task.star:9:870: `agent` is a target that no job function or final call names: name it from one
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/1
            pumpkin: it's broken/tasks/task.star: the task has 3 problems
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd '<scratch>/it'\''s broken/tasks/task.star'
            $ pumpkin run a~b/tasks/task.star --sandboxd sandboxd --runs two
            lines --config my served.toml
            exit status: Some(1)
            tasks/task.star:9:895: `agent` also names /roles/0: give each a unique name
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               ^^^^^^^
                the IR's /roles/1/name
            tasks/task.star:9:674: no role is named `trainer`: name one listed in `roles`
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  ^^^^^^^^^
                the IR's /roles/0/runtime/agent/grants/0/job/target
            tasks/task.star:9:870: `agent` is a target that no job function or final call names: name it from one
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/1
            pumpkin: a~b/tasks/task.star: the task has 3 problems
            pumpkin: to run it again: pumpkin run --runs '<scratch>/two
            lines' --config '<scratch>/my served.toml' --sandboxd <scratch>/sandboxd '<scratch>/a~b/tasks/task.star'
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unserved.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run budgeted/task.star --sandboxd sandboxd --runs runs --config conf/uncached.toml
            exit status: Some(1)
            pumpkin: conf/uncached.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cached` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/uncached.toml --sandboxd <scratch>/sandboxd <scratch>/budgeted/task.star
            $ pumpkin run budgeted/task.star --sandboxd sandboxd --runs runs --config conf/unwritten.toml
            exit status: Some(1)
            pumpkin: conf/unwritten.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cache_write` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/unwritten.toml --sandboxd <scratch>/sandboxd <scratch>/budgeted/task.star
            $ pumpkin run budgeted/task.star --sandboxd sandboxd --runs runs --config conf/uncosted.toml
            exit status: Some(1)
            pumpkin: conf/uncosted.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cached` or `cache_write` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/uncosted.toml --sandboxd <scratch>/sandboxd <scratch>/budgeted/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run large/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: large/task.star: /roles/1/needs: `trainer` fits no class of sandbox, as `local`'s `container` offers <cpus>000 thousandths of a CPU, fewer than its 9223372036854775807: offer a class that meets its needs, or change its needs
            pumpkin: large/task.star: /roles/2/needs: `final_judge` fits no class of sandbox, as `local`'s `container` has no GPU, and it needs 1 × `NVIDIA H100 80GB HBM3`: offer a class that meets its needs, or change its needs
            pumpkin: large/task.star: placing the workflow's roles in the classes that pumpkin.toml's providers offer has 2 problems
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/large/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config plain.toml
            exit status: Some(1)
            pumpkin: reserving on provider `local`: the run's roles need more whole cores at once than the <cores> that this machine gives sandboxes, to hold their CPUs (`agent` <cpus>, `trainer` 1, `final_judge` 1): give them fewer CPUs
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config backward.toml
            exit status: Some(1)
            pumpkin: backward.toml: TOML parse error at line 3, column 8
              |
            3 | cpus = "3-1"
              |        ^^^^^
            `3-1` is no CPU list: expected one such as `0-3,8`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/backward.toml --sandboxd <scratch>/sandboxd <scratch>/wide/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config empty.toml
            exit status: Some(1)
            pumpkin: empty.toml: TOML parse error at line 3, column 8
              |
            3 | cpus = ""
              |        ^^
            the CPU list is empty: name one CPU at least, such as `0`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/empty.toml --sandboxd <scratch>/sandboxd <scratch>/wide/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config offline.toml
            exit status: Some(1)
            pumpkin: reaching provider `local` failed: the CPUs to give sandboxes, 0,4096-4097, name 4096-4097, which aren't online: name only CPUs of 0-<cpus - 1>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/offline.toml --sandboxd <scratch>/sandboxd <scratch>/wide/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run narrow/task.star --sandboxd sandboxd --runs runs --config narrow.toml
            exit status: Some(1)
            pumpkin: reserving on provider `local`: the run's roles need more whole cores at once than the 2 that this machine gives sandboxes, to hold their CPUs (`agent` 2, `trainer` 1, `final_judge` 1): give them fewer CPUs"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn configurations() {
    let scratch = Scratch::new();
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    scratch.write_task("workflow", &ir);
    ir["roles"][0]["runtime"]["agent"]
        .as_object_mut()
        .unwrap()
        .remove("egress");
    scratch.write_task("unrouted", &ir);
    write_configurations(&scratch);
    let run = |entry, config: &[&str]| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs"]).args(config);
        command
    };
    let config = |name: &str| run("workflow/task.star", &["--config", &format!("{name}.toml")]);
    let home = run("workflow/task.star", &[]);
    let mut homeless = run("workflow/task.star", &[]);
    homeless.env_remove("HOME");
    let mut served = config("conf/served");
    let mut unrouted = run("unrouted/task.star", &["--config", "docked.toml"]);
    // Without `docker` on `PATH`, `run` can't reach the engine past the configuration.
    for command in [&mut served, &mut unrouted] {
        command.env("PATH", scratch.0.join("nowhere"));
    }
    let refused = [
        "missing", "undocked", "nameless", "broken", "inline", "priced", "keyless", "invalid",
    ];
    let refused = (refused.into_iter()).chain([
        "negative",
        "outputless",
        "reasoned",
        "twice",
        "again",
        "query",
        "fragment",
        "unserved",
        "placed",
        "podman",
        "context",
        "valued",
        "second",
        "mixed",
        "bundled",
        "regional",
        "unimaged",
        "unspecified",
        "portless",
        "public",
        "docked",
        "conf/unkeyed",
    ]);
    let commands = [home, homeless]
        .into_iter()
        .chain(refused.map(config))
        .chain([served, unrouted]);
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: the configuration <scratch>/home/.config/pumpkin/pumpkin.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ env --unset=HOME pumpkin run workflow/task.star --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config missing.toml
            exit status: Some(1)
            pumpkin: the configuration missing.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/missing.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config undocked.toml
            exit status: Some(1)
            pumpkin: undocked.toml: `[docker]` is missing: name it, or `[controller]`, as `pumpkin help config` says
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/undocked.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config nameless.toml
            exit status: Some(1)
            pumpkin: nameless.toml: TOML parse error at line 2, column 11
              |
            2 | context = ""
              |           ^^
            the Docker context's name is empty: name one that `docker context ls` lists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/nameless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config broken.toml
            exit status: Some(1)
            pumpkin: broken.toml: TOML parse error at line 4, column 19
              |
            4 | [upstreams.gateway
              |                   ^
            unclosed table, expected `]`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/broken.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config inline.toml
            exit status: Some(1)
            pumpkin: inline.toml: TOML parse error at line 6, column 1
              |
            6 | key = "sk-1"
              | ^^^
            unknown field `key`, expected one of `url`, `key_file`, `models`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/inline.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config priced.toml
            exit status: Some(1)
            pumpkin: priced.toml: TOML parse error at line 7, column 73
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", price = 1 }]
              |                                                                         ^^^^^
            unknown field `price`, expected one of `name`, `upstream_name`, `usd_per_mtok`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/priced.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config keyless.toml
            exit status: Some(1)
            pumpkin: keyless.toml: TOML parse error at line 4, column 1
              |
            4 | [upstreams.gateway]
              | ^^^^^^^^^^^^^^^^^^^
            missing field `key_file`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/keyless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config invalid.toml
            exit status: Some(1)
            pumpkin: invalid.toml: TOML parse error at line 4, column 12
              |
            4 | [upstreams.Gateway]
              |            ^^^^^^^
            `Gateway` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/invalid.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config negative.toml
            exit status: Some(1)
            pumpkin: negative.toml: TOML parse error at line 7, column 88
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = { input = -1, output = 10 } }]
              |                                                                                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^
            `input` is -1, not a price in US dollars a million tokens: give one from 0 to 1000000, such as 1.25
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/negative.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config outputless.toml
            exit status: Some(1)
            pumpkin: outputless.toml: TOML parse error at line 7, column 88
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = { input = 1.25 } }]
              |                                                                                        ^^^^^^^^^^^^^^^^
            missing field `output`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/outputless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config reasoned.toml
            exit status: Some(1)
            pumpkin: reasoned.toml: TOML parse error at line 7, column 117
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = { input = 1.25, output = 10, reasoning = 10 } }]
              |                                                                                                                     ^^^^^^^^^
            unknown field `reasoning`, expected one of `input`, `cached`, `cache_write`, `output`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/reasoned.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config twice.toml
            exit status: Some(1)
            pumpkin: twice.toml: TOML parse error at line 9, column 12
              |
            9 | [upstreams.gateway]
              |            ^^^^^^^
            duplicate key
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/twice.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config again.toml
            exit status: Some(1)
            pumpkin: again.toml: `gpt-6-astra` is listed more than once: list each model once, under the upstream that serves it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/again.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config query.toml
            exit status: Some(1)
            pumpkin: query.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1?project=1"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches each API's endpoint
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/query.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config fragment.toml
            exit status: Some(1)
            pumpkin: fragment.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1#v2"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches each API's endpoint
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/fragment.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unserved.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config placed.toml
            exit status: Some(1)
            pumpkin: placed.toml: TOML parse error at line 11, column 2
               |
            11 | [placement.agent]
               |  ^^^^^^^^^
            unknown field `placement`, expected one of `docker`, `providers`, `gateway`, `upstreams`, `controller`, `publish`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/placed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config podman.toml
            exit status: Some(1)
            pumpkin: podman.toml: TOML parse error at line 9, column 18
              |
            9 | [providers.local.podman]
              |                  ^^^^^^
            unknown variant `podman`, expected one of `docker`, `ec2`, `modal`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/podman.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config context.toml
            exit status: Some(1)
            pumpkin: context.toml: TOML parse error at line 10, column 1
               |
            10 | context = "remote"
               | ^^^^^^^
            unknown field `context`, there are no fields
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/context.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config valued.toml
            exit status: Some(1)
            pumpkin: valued.toml: TOML parse error at line 10, column 10
               |
            10 | docker = true
               |          ^^^^
            invalid type: boolean `true`, expected an empty table, as the provider's kind takes no settings
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/valued.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config second.toml
            exit status: Some(1)
            pumpkin: second.toml: providers `local` and `remote` are each of kind `docker`, so each would be the machine's one Docker engine, `[docker]`'s: keep one of them
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/second.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config mixed.toml
            exit status: Some(1)
            pumpkin: mixed.toml: provider `local` is of kind `docker`, beside `aws`: a machine runs its roles either on its own engine or on other providers, so keep `local` alone, or remove it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/mixed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config bundled.toml
            exit status: Some(1)
            pumpkin: bundled.toml: TOML parse error at line 7, column 74
              |
            7 | repositories = { roles = "pumpkin/roles", sandboxd = "pumpkin/sandboxd", bundles = "pumpkin/bundles" }
              |                                                                          ^^^^^^^
            unknown field `bundles`, expected `roles` or `sandboxd`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/bundled.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config regional.toml
            exit status: Some(1)
            pumpkin: regional.toml: TOML parse error at line 5, column 10
              |
            5 | region = "us_west_1"
              |          ^^^^^^^^^^^
            `us_west_1` is not an AWS region: use one such as `us-west-1`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/regional.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unimaged.toml
            exit status: Some(1)
            pumpkin: unimaged.toml: TOML parse error at line 4, column 1
              |
            4 | [providers.gpus.modal]
              | ^^^^^^^^^^^^^^^^^^^^^^
            missing field `images`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unimaged.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unspecified.toml
            exit status: Some(1)
            pumpkin: unspecified.toml: TOML parse error at line 5, column 11
              |
            5 | private = "0.0.0.0:7433"
              |           ^^^^^^^^^^^^^^
            the gateway's address 0.0.0.0:7433 is unspecified, which no sandbox can dial: name the address its sandboxes dial
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unspecified.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config portless.toml
            exit status: Some(1)
            pumpkin: portless.toml: TOML parse error at line 5, column 11
              |
            5 | private = "172.31.0.10"
              |           ^^^^^^^^^^^^^
            `172.31.0.10` is not an IP address and a port: name one such as `172.31.0.10:7433`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/portless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config public.toml
            exit status: Some(1)
            pumpkin: public.toml: TOML parse error at line 5, column 1
              |
            5 | public = "203.0.113.1:7433"
              | ^^^^^^
            unknown field `public`, expected `private`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/public.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: docked.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: add an `[upstreams.<name>]` that lists them
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/docked.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config conf/unkeyed.toml
            exit status: Some(1)
            pumpkin: conf/unkeyed.toml: the key file conf/missing can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/unkeyed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ PATH=<scratch>/nowhere pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ PATH=<scratch>/nowhere pumpkin run unrouted/task.star --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/docked.toml --sandboxd <scratch>/sandboxd <scratch>/unrouted/task.star"#]],
    );
}

/// Writes into `scratch` the stand-in `sandboxd` that [`configurations`] and [`refusals`] give
/// `run`, and the machine's configurations, each named for what it holds: `conf/served`, of the
/// context `nowhere` and two upstreams, one of whose models has a price, its key in `conf/key`;
/// `docked`, without upstreams; and, to be refused, one for each rule a configuration breaks, as
/// its name says (a missing context, TOML errors, keys, upstreams, URLs, prices, unserved models,
/// `[placement]`, provider kinds and their
/// settings, [`EC2`]'s and Modal's tables, the gateway's address); and `conf/unkeyed`, whose key
/// file is missing. Each but `undocked` ends with [`LOCAL`]'s tables, changed as its name says.
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the configurations, a line or a few each"
)]
fn write_configurations(scratch: &Scratch) {
    let table = |name: &str, model: &str| {
        format!(
            r#"[upstreams.{name}]
url = "https://models.example.com/v1"
key_file = "key"
models = [{{ name = "{model}", upstream_name = "vendor/{model}" }}]
"#
        )
    };
    let gateway = table("gateway", "gpt-6-astra");
    let direct = table("direct", "flash");
    let priced =
        |prices: &str| gateway.replace(" }", &format!(", usd_per_mtok = {{ {prices} }} }}"));
    scratch.write_sandboxd("sandboxd");
    scratch.write("conf/key", "sk-test\n");
    let configs = [
        (
            "conf/served",
            format!(
                "{gateway}\n{}",
                direct.replace(" }", ", usd_per_mtok = { input = 0.3, output = 2.5 } }")
            ),
        ),
        ("docked", String::new()),
        ("nameless", gateway.clone()),
        (
            "broken",
            gateway.replace("[upstreams.gateway]", "[upstreams.gateway"),
        ),
        (
            "inline",
            gateway.replace("key_file = \"key\"", "key = \"sk-1\""),
        ),
        ("priced", gateway.replace(" }", ", price = 1 }")),
        ("negative", priced("input = -1, output = 10")),
        ("outputless", priced("input = 1.25")),
        (
            "conf/uncached",
            priced("input = 1.25, cache_write = 1.5, output = 10"),
        ),
        (
            "conf/unwritten",
            priced("input = 1.25, cached = 0.125, output = 10"),
        ),
        ("conf/uncosted", priced("input = 1.25, output = 10")),
        (
            "reasoned",
            priced("input = 1.25, output = 10, reasoning = 10"),
        ),
        ("keyless", gateway.replace("key_file = \"key\"\n", "")),
        ("invalid", table("Gateway", "gpt-6-astra")),
        ("twice", format!("{gateway}\n{}", table("gateway", "flash"))),
        (
            "again",
            format!("{gateway}\n{}", table("direct", "gpt-6-astra")),
        ),
        ("query", gateway.replace("/v1", "/v1?project=1")),
        ("fragment", gateway.replace("/v1", "/v1#v2")),
        ("unserved", direct),
        ("placed", gateway.clone()),
        ("podman", gateway.clone()),
        ("context", gateway.clone()),
        ("valued", gateway.clone()),
        ("second", gateway.clone()),
        ("mixed", EC2.to_owned()),
        (
            "bundled",
            EC2.replace(
                " sandboxd = \"pumpkin/sandboxd\" }",
                " sandboxd = \"pumpkin/sandboxd\", bundles = \"pumpkin/bundles\" }",
            ),
        ),
        ("regional", EC2.replace("\"us-west-1\"", "\"us_west_1\"")),
        (
            "unimaged",
            "[providers.gpus.modal]\nprofile = \"pumpkin\"\ndials_from = \"203.0.113.7\"\n\n\
             [providers.gpus.modal.classes.h100]\ngpu = \"H100!\"\n\
             gpu_model = \"NVIDIA H100 80GB HBM3\"\ncpu_millis = 4000\n\
             memory_bytes = 17179869184\nstartup_seconds = 1800\n"
                .to_owned(),
        ),
        (
            "unspecified",
            "[gateway]\nprivate = \"0.0.0.0:7433\"\n".to_owned(),
        ),
        (
            "portless",
            "[gateway]\nprivate = \"172.31.0.10\"\n".to_owned(),
        ),
        (
            "public",
            "[gateway]\npublic = \"203.0.113.1:7433\"\n".to_owned(),
        ),
        ("conf/unkeyed", gateway.replace("\"key\"", "\"missing\"")),
    ];
    for (name, config) in configs {
        let docker = match name {
            "nameless" => docker_table(""),
            _ => docker_table("nowhere"),
        };
        let placed = match name {
            "placed" => {
                format!("{LOCAL}\n[placement.agent]\nprovider = \"local\"\nclass = \"container\"\n")
            }
            "podman" => LOCAL.replace(".docker]", ".podman]"),
            "context" => LOCAL.replace(".docker]\n", ".docker]\ncontext = \"remote\"\n"),
            "valued" => LOCAL.replace("local.docker]\n", "local]\ndocker = true\n"),
            "second" => format!("[providers.remote.docker]\n\n{LOCAL}"),
            _ => LOCAL.to_owned(),
        };
        let config = format!("{docker}\n{config}\n{placed}");
        scratch.write(&format!("{name}.toml"), &config);
    }
    scratch.write("undocked.toml", &gateway);
}

/// Writes into `scratch` the task `asks`, [`workflow`]'s with the fixture's model route,
/// whose agent asks its model and keeps the answer in its `out`; and the configuration
/// `conf/pumpkin.toml`, of the Docker context `context`, which places the roles as [`LOCAL`]
/// does, and whose one upstream, the stand-in at `address`, serves the route's model as
/// `vendor/gpt-6-astra`, with a price, and with its key in `conf/key`.
fn write_asks(scratch: &Scratch, context: &str, address: &str) {
    let config = format!(
        r#"{}
[upstreams.provider]
url = "http://{address}/v1/"
key_file = "key"
models = [{{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = {{ input = 1.25, output = 10 }} }}]
"#,
        machine(context)
    );
    scratch.write("conf/pumpkin.toml", &config);
    fs::write(scratch.0.join("conf/key"), "sk-test\n").unwrap();
    let body = r#"{"model": "gpt-6-astra", "store": false, "input": "hi"}"#;
    let ask = format!(
        "wget --quiet --output-document=/pumpkin/out/answer --header 'content-type: application/json' \
         --post-data '{body}' http://127.0.0.1:{MODEL_PORT}/responses && printf six > six && \
         {SANDBOXCTL_PATH} call submit six --arg finalize=true"
    );
    let mut asks = workflow("", &["sh", "-c", &ask]);
    let fixture: Value = serde_json::from_str(WORKFLOW).unwrap();
    asks["roles"][0]["runtime"]["agent"]["egress"] =
        fixture["roles"][0]["runtime"]["agent"]["egress"].clone();
    let body = r#"{"model": "gpt-6-astra", "messages": [{"role": "user", "content": "grade"}]}"#;
    let grade = format!(
        "wget --quiet --output-document=/pumpkin/out/answer \
         --header 'content-type: application/json' --post-data '{body}' \
         http://127.0.0.1:{MODEL_PORT}/v1/chat/completions && \
         printf '{{\"raw_score\": 1}}' > /pumpkin/out/result.json"
    );
    asks["roles"][2]["runtime"]["target"]["egress"] =
        json!({ "model": [{ "api": "chat", "models": ["gpt-6-astra"] }] });
    scratch.write_task("asks", &asks);
    scratch.write("asks/final.star", &final_running(&grade));
}

/// A stand-in for a model provider, on the loopback, which answers each `POST` with a whole
/// stream that echoes the request's line, its `authorization` and its body.
struct Upstream {
    address: String,
    thread: JoinHandle<()>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let thread = thread::spawn(move || {
            for connection in listener.incoming() {
                let mut connection = connection.unwrap();
                let mut reader = BufReader::new(&connection);
                let mut head = Vec::new();
                while head.last().is_none_or(|line: &String| !line.is_empty()) {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    head.push(line.trim_end().to_owned());
                }
                // The connection that `stop` makes holds no request.
                if head[0].is_empty() {
                    return;
                }
                let header = |name| head.iter().find_map(|line| line.strip_prefix(name));
                let length = header("content-length: ").unwrap();
                let mut body = vec![0; length.parse().unwrap()];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                let echo = format!(
                    "data: {} {:?} {body}\n\n",
                    head[0],
                    header("authorization: ")
                );
                let stream = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     transfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{echo}\r\n0\r\n\r\n",
                    echo.len()
                );
                connection.write_all(stream.as_bytes()).unwrap();
            }
        });
        Self { address, thread }
    }

    fn stop(self) {
        drop(TcpStream::connect(&self.address).unwrap());
        self.thread.join().unwrap();
    }
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn runs_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let upstream = Upstream::start();
    write_asks(&scratch, &context, &upstream.address);
    scratch.write("home/.config/pumpkin/pumpkin.toml", &machine(&context));
    scratch.write_task("exits", &workflow("", &["true"]));
    let submit =
        format!("printf six > six && {SANDBOXCTL_PATH} call submit six --arg finalize=true");
    scratch.write_task("submits", &workflow("", &["sh", "-c", &submit]));
    scratch.write_task("fails", &workflow("", &["sh", "-c", &submit]));
    scratch.write("fails/final.star", &final_running("exit 1"));
    for task in ["asks", "exits", "submits", "fails"] {
        build_first(&scratch, task, "conf/pumpkin.toml");
    }
    let run = |entry, rest: &[&str]| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", &sandboxd, "--wait"]);
        command.args(rest);
        command
    };
    // `docker` finds its contexts below `HOME` unless `DOCKER_CONFIG` says where.
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    let docker_config = env::var_os("DOCKER_CONFIG").map_or(home.join(".docker"), PathBuf::from);
    let docker_config = docker_config.into_os_string().into_string().unwrap();
    let mut exits = run("exits/task.star", &[]);
    exits.env("DOCKER_CONFIG", &docker_config);
    let config = ["--runs", "runs", "--config", "conf/pumpkin.toml"];
    let commands = [
        run("asks/task.star", &config),
        exits,
        run("submits/task.star", &config),
        run("fails/task.star", &config),
    ];
    let shown = [
        (&sandboxd[..], "<sandboxd>"),
        (&docker_config[..], "<docker config>"),
        (&upstream.address[..], "<stand-in>"),
    ];
    let rootful = !engine.report().security.rootless;
    check(
        &scratch,
        commands.map(Step::OnEngine),
        rootful,
        &shown,
        &expect![[r#"
            $ pumpkin run asks/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: score=1
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                        model: 1 request, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 1 good answer, by the median, <t> to the end
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=1
                        model: 1 request, all answered 200; no response reported its tokens; of 1 good answer, by the median, <t> to the end
              model: 2 requests, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 2 good answers, by the median, <t> to the end
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=1); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/asks/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/answer, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/answer, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, model/0.request, model/0.response, model/1.request, model/1.response, model/items/06651be039f9e5a47490f8ea118fd0931e7824702c693557a77a7df2d5114aa4, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json, sources/task/asks/final.star, sources/task/asks/submit.star, sources/task/asks/task.star, sources/task/lib/train.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            provider.json routes: {"agent":[{"api":"responses","models":{"gpt-6-astra":{"upstream_name":"vendor/gpt-6-astra","usd_per_mtok":{"input":1.25,"output":10.0}}},"upstream":"provider"}],"final_judge":[{"api":"chat","models":{"gpt-6-astra":{"upstream_name":"vendor/gpt-6-astra","usd_per_mtok":{"input":1.25,"output":10.0}}},"upstream":"provider"}]}
            the run log records: {"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":57,"body_sha256":"8e1b803c11e5437c46a8ea56e92d591b284778ce8209d56a347f8c8fdc0ca260","bytes":57,"dropped_headers":["user-agent"]},"response":{"bytes":116}},"kind":"model_answered","model":"gpt-6-astra","request":0,"upstream":"provider","upstream_status":200}
            the run log records: {"answer":{"kind":"forwarded"},"api":"chat","caller":"job-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":77,"body_sha256":"018fa791e56a2db8c8c065da57294d744c30e14181cf2fdbfd272d3e8f009efd","bytes":117,"items":{"count":1,"field":"messages"}},"response":{"bytes":143}},"kind":"model_answered","model":"gpt-6-astra","request":1,"upstream":"provider","upstream_status":200}
            the agent's answer: data: POST /v1/responses HTTP/1.1 Some("Bearer sk-test") {"input":"hi","model":"vendor/gpt-6-astra","store":false}\n\n
            the final call's answer: data: POST /v1/chat/completions HTTP/1.1 Some("Bearer sk-test") {"messages":[{"content":"grade","role":"user"}],"model":"vendor/gpt-6-astra"}\n\n
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"calls":[{"grant":null,"job":0,"model_requests":1,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":2,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":1}]}}}
            $ DOCKER_CONFIG=<docker config> pumpkin run exits/task.star --sandboxd <sandboxd> --wait
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/.pumpkin/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/.pumpkin/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: score=0
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=0
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=0); its record is <scratch>/.pumpkin/runs/<id>
            pumpkin: to run it again: pumpkin run --sandboxd <sandboxd> <scratch>/exits/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/out/, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/, provider.json, record.json, sources/task/exits/final.star, sources/task/exits/submit.star, sources/task/exits/task.star, sources/task/lib/train.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            $ pumpkin run submits/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: score=3
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=3
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=3); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/submits/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json, sources/task/lib/train.star, sources/task/submits/final.star, sources/task/submits/submit.star, sources/task/submits/task.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":3}]}}}
            $ pumpkin run fails/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: failed: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: failed: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished without a result: the final call gave none (error: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)
                18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}
                   |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `final_call()`, called at fails/final.star:3:5); its commands' logs are in <scratch>/runs/<id>/jobs/0/log; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/fails/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json, sources/task/fails/final.star, sources/task/fails/submit.star, sources/task/fails/task.star, sources/task/lib/train.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"failure":{"kind":"error","reason":"fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {\"score\": json.decode(target.get(\"/pumpkin/out/result.json\"))[\"raw_score\"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5"},"kind":"no_result"}}"#]],
    );
    upstream.stop();
    wait_for_each_run(&scratch, &sandboxd);
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the steps and their snapshot"
)]
fn writes_only_in_its_workspace() {
    let scratch = Scratch::new();
    let label = format!("pumpkin.test=workspace-{}", std::process::id());
    let _images = Labelled(label.clone());
    write_hello(&scratch, &label);
    fs::hard_link(sandboxd(), scratch.0.join(".pumpkin/bin/sandboxd")).unwrap();
    let context = docker_context();
    scratch.write("pumpkin.toml", &machine(&context));
    let home = test_home(&scratch, &context);
    let temp = scratch.0.join("tmp");
    fs::create_dir(&temp).unwrap();
    let before = files(&home);
    let config = ["--config", "pumpkin.toml"];
    // Built first, as a run that waits for room may find its images built by an earlier try.
    let mut built = at_home(
        &scratch,
        &home,
        &temp,
        &[&["build", HELLO], &config[..]].concat(),
    );
    let built = built.output().unwrap();
    assert!(built.status.success(), "{built:?}");
    let mut run = at_home(
        &scratch,
        &home,
        &temp,
        &[&["run", HELLO, "--wait"], &config[..]].concat(),
    );
    let invoked = invocation(&run);
    let rootful = !engine(&scratch, &context).report().security.rootless;
    let output = with_room(&mut run);
    let written: Vec<String> = files(&home)
        .into_iter()
        .filter(|file| !before.contains(file))
        .collect();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (_, dir) = record(&stderr).unwrap();
    let mut lines = vec![
        report(invoked, &output, &scratch.0, rootful),
        format!("HOME's new files: {written:?}"),
        format!("TMPDIR holds: [{}]", held(&temp)),
        format!(".pumpkin holds: {}", held(&scratch.0.join(".pumpkin"))),
        format!(
            ".pumpkin/docker holds: {}",
            held(&scratch.0.join(".pumpkin/docker"))
        ),
    ];
    lines.extend(standard_outputs(&scratch.0.join(dir)));
    // So that the scratch directory can be removed.
    fs::set_permissions(home.join(".docker"), Permissions::from_mode(0o755)).unwrap();
    let shown = lines
        .join("\n")
        .replace(scratch.0.to_str().unwrap(), "<scratch>");
    expect![[r#"
        $ env --unset=DOCKER_CONFIG TMPDIR=<scratch>/tmp env --unset=XDG_CACHE_HOME env --unset=XDG_CONFIG_HOME env --unset=XDG_DATA_HOME pumpkin run examples/hello/task.star --wait --config pumpkin.toml
        exit status: Some(0)
        pumpkin: run <id> records into <scratch>/.pumpkin/runs/<id>
        pumpkin: run <id> started
        pumpkin: following run <id>, recorded in <scratch>/.pumpkin/runs/<id>: ending `pumpkin run` does not stop it
        pumpkin:    <t> agent started on local/container
        pumpkin:    <t> count job 0 of agent 0 asked on local/container
        pumpkin:    <t> count job 0 of agent 0 started
        pumpkin:    <t> count job 0 of agent 0 ended: lines=1 bytes=6
        pumpkin:    <t> the work phase ended
        pumpkin:    <t> final call job 1 asked on local/container
        pumpkin:    <t> final call job 1 started
        pumpkin:    <t> final call job 1 ended: score=6 lines=1
        pumpkin: run <id> started <t> after it was asked for, and ran <t>:
             <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
             <t> count job 0 of agent 0 on local/container, started after <t>, ran <t>: lines=1 bytes=6
             <t> final call job 1 on local/container, started after <t>, ran <t>: score=6 lines=1
          sandboxes: 3 on local/container, <t> in all, 3 created, all deleted
        pumpkin: run <id> finished with a result (score=6 lines=1); its record is <scratch>/.pumpkin/runs/<id>
        pumpkin: to run it again: pumpkin run --config <scratch>/pumpkin.toml <scratch>/examples/hello/task.star
        record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/counts.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/, jobs/0/sandbox.json, jobs/1/got/1/result.json, jobs/1/log/0/command.stderr, jobs/1/log/0/command.stdout, jobs/1/out/result.json, jobs/1/sandbox.json, log.jsonl, outcome.json, payloads/5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03, provider.json, record.json, sources/task/hello/count.star, sources/task/hello/final.star, sources/task/hello/task.star, sources/task/submit.star
        agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
        job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
        job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":3,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6},{"name":"lines","value":1}]}}}
        HOME's new files: []
        TMPDIR holds: []
        .pumpkin holds: bin, docker, runs
        .pumpkin/docker holds: buildx, publish-slots
        agent `agent`'s standard output:
        {"kind":"call","job":0,"remaining":{"calls":null,"seconds":null}}
        {"kind":"status","job":0,"state":"completed","returned":{"bytes":6,"lines":1},"remaining":{"calls":null,"seconds":null}}
        {"kind":"call","returned":{"state":"accepted"},"remaining_seconds":30}
        job 0's command 0's standard output:
        job 1's command 0's standard output:"#]].assert_eq(&shown);
}

/// Writes `examples/hello` into `scratch`, with the marker of its root, and with `label` on its
/// image.
fn write_hello(scratch: &Scratch, label: &str) {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for file in [
        "PUMPKIN.star",
        "submit.star",
        "hello/task.star",
        "hello/count.star",
        "hello/final.star",
        "hello/Dockerfile",
    ] {
        let text = fs::read_to_string(examples.join(file)).unwrap();
        scratch.write(&format!("examples/{file}"), &text);
    }
    let recipe = scratch.0.join("examples/hello/Dockerfile");
    let mut labelled = fs::OpenOptions::new().append(true).open(recipe).unwrap();
    writeln!(labelled, "LABEL {label}").unwrap();
}

/// The entry of the task that [`write_hello`] writes.
const HELLO: &str = "examples/hello/task.star";

/// `scratch`'s `pumpkin` with `args`, with `home` as `HOME` and `temp` as `TMPDIR`, and none of
/// the variables that would name other directories for them.
fn at_home(scratch: &Scratch, home: &Path, temp: &Path, args: &[&str]) -> Command {
    let mut run = scratch.pumpkin(args);
    run.env("HOME", home).env("TMPDIR", temp);
    for name in [
        "DOCKER_CONFIG",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        run.env_remove(name);
    }
    run
}

/// A home of `scratch`'s, whose Docker configuration, `.docker`, is read-only, and holds, for a
/// Docker context but `default`, `context`, a copy of the contexts of this process's, which
/// `docker` reads it from.
fn test_home(scratch: &Scratch, context: &str) -> PathBuf {
    let home = scratch.0.join("home");
    let docker = home.join(".docker");
    fs::create_dir_all(&docker).unwrap();
    if context != "default" {
        let real = env::var_os("DOCKER_CONFIG").map_or_else(
            || PathBuf::from(env::var_os("HOME").unwrap()).join(".docker"),
            PathBuf::from,
        );
        copy_tree(&real.join("contexts"), &docker.join("contexts"));
    }
    fs::set_permissions(&docker, Permissions::from_mode(0o555)).unwrap();
    home
}

/// The names in `dir`, sorted, between commas.
fn held(dir: &Path) -> String {
    let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names.join(", ")
}

/// Copies the tree at `from`, whose files are regular ones, to `to`, which doesn't exist yet.
const OPS_FINAL: &str = r#"
    payload=/pumpkin/in/payload/raw
    bytes=$(wc -c < $payload)
    printf '{"raw_score": %s}\n' $bytes > /pumpkin/out/result.json
    echo kept > /pumpkin/out/evidence.txt
    echo "judged $bytes bytes as $(id -u), with HOME=$HOME, in $(pwd)""#;

/// `sandboxctl` exits with 1 for a refusal and 2 for a job that gave no result.
const AGENT: &str = r#"#!/bin/sh
    cd /workspace && stat -c '%a %n' .codex .codex/skills .codex/config.toml .tmp agent
    echo "standard input: $(cat)"
    x() { head -c "$1" /dev/zero | tr '\0' x > "$2"; }
    x 6 six && x 9 nine && x 17 seventeen && printf slow > slow
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed 's/"remaining_seconds":[0-9]*/"remaining_seconds":"<s>"/'
    }
    ctl ops
    ctl status 0
    ctl call nope six --wait 15
    ctl call train nine
    ctl call train seventeen
    ctl call train slow --wait 1
    ctl call train six --wait 60
    ctl status 0
    ctl call submit six --arg finalize=true --wait 15"#;

/// The workflow of the run whose agent makes ops: [`workflow`]'s, whose agent runs
/// [`AGENT`], which the run puts in its sandbox with the fixture's directories, files and
/// standard input, and a directory listed before the one that holds it; whose job function
/// `train` takes payloads of up to 8 bytes, and whose final call runs [`OPS_FINAL`] on a payload
/// of up to 16 bytes, the most an op of the agent's may carry; and whose calls of `train` have a
/// timeout of 5 s, room for the body's put and command on a loaded engine, without a cumulative
/// limit, so that it is each call's timeout, and what a reply has left depends on no command's
/// time.
fn ops_workflow() -> Value {
    // Spliced in, since `format!` would need each of the shell's braces doubled.
    let script = AGENT.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    let mut ir = workflow("", &["/workspace/agent"]);
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    agent["prepend_grant_help"] = json!(true);
    let skills = json!({ "path": "/workspace/.codex/skills", "mode": "0750" });
    agent["dirs"].as_array_mut().unwrap().insert(0, skills);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": script });
    agent["files"].as_array_mut().unwrap().push(launcher);
    let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
    train["payload_max_bytes"] = json!(8);
    train["call_timeout_seconds"] = json!(5);
    train.as_object_mut().unwrap().remove("cumulative_seconds");
    ir["final"]["payload_max_bytes"] = json!(16);
    ir["roles"][0]["runtime"]["agent"]["grants"][1]["now"]["payload_max_bytes"] = json!(16);
    ir
}

const LEASING_AGENT: &str = r#"#!/bin/sh
    printf xxxxxx > /workspace/six
    mkdir --parents /workspace/src/deep && printf hello > /workspace/src/a && printf deep > /workspace/src/deep/b
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed --regexp-extended 's/"(remaining_seconds|elapsed_seconds|busy_seconds|idle_seconds|instance_seconds|cumulative_seconds)":[0-9]+/"\1":"<s>"/g'
    }
    # Waits until lease $1 is ready, then shows its status.
    ready() {
        until SANDBOXCTL call gpu.status "$1" | grep -q '"state":"ready"'; do sleep 1; done
        ctl call gpu.status "$1"
    }
    ctl call gpu.open
    ready 0
    ctl call gpu.put 0 /workspace/src /work/src
    ready 0
    ctl call gpu.run 0 -- sh -c 'echo "in $(pwd) as $(id -u), with CUDA=$CUDA"; echo to stderr >&2; mkdir out; tr a-z A-Z < src/a > out/A; cp --recursive src/deep out/'
    ready 0
    ctl call gpu.get 0 /work/out /workspace/got
    ready 0
    ctl call gpu.put 0 /workspace/src /work/src
    ready 0
    ctl call gpu.run 0 --timeout 1 -- sleep 60
    ready 0
    ctl call gpu.close 0
    echo "got: $(find /workspace/got | sort | tr '\n' ' '), A: $(cat /workspace/got/A), deep/b: $(cat /workspace/got/deep/b)"
    ctl call submit /workspace/six --arg finalize=true"#;

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn leases_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let script = LEASING_AGENT.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    let mut ir = workflow("", &["/workspace/agent"]);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": script });
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    agent["files"].as_array_mut().unwrap().push(launcher);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu_runner");
    runner["image"] = json!(RUNNER);
    runner["runtime"] = json!({ "runner": { "cwd": "/work", "env": ["CUDA=7"] } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "name": "gpu", "session": {
        "runner": "gpu_runner", "instances": 1, "instance_seconds": 120,
        "commands": 5, "transfer_bytes": 1024,
    } });
    ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    scratch.write_task("leases", &ir);
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "leases", "pumpkin.toml");
    let mut run = scratch.pumpkin(&["run", "leases/task.star", "--sandboxd", &sandboxd, "--wait"]);
    run.args(["--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let rootful = !engine.report().security.rootless;
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let mut lines = vec![format!("exit status: {:?}", output.status.code())];
    let listed = containers(&context).list(&RunTag::try_from(id.to_owned()).unwrap());
    lines.push(format!("listed once it ended: {:?}", listed.unwrap()));
    let leases = record.join("leases");
    lines.push(format!("the lease's record: {}", files(&leases).join(", ")));
    for command in ["1", "4"] {
        for stream in ["stdout", "stderr"] {
            let path = leases.join(format!("0/log/{command}/command.{stream}"));
            let text = fs::read_to_string(path).unwrap_or_default();
            lines.push(format!("command {command}'s {stream}: {text:?}"));
        }
    }
    lines.extend(
        standard_outputs(&record)
            .into_iter()
            .take_while(|line| !line.starts_with("job ")),
    );
    let shown = [(&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &shown,
        &expect![[r#"
            exit status: Some(0)
            listed once it ended: []
            the lease's record: 0/log/1/command.stderr, 0/log/1/command.stdout, 0/log/4/command.stderr, 0/log/4/command.stdout, 0/out/, 0/sandbox.json
            command 1's stdout: "in /work as 65532, with CUDA=7\n"
            command 1's stderr: "to stderr\n"
            command 4's stdout: ""
            command 4's stderr: ""
            agent `agent`'s standard output:
            sandboxctl call gpu.open: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":5}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":null,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":5}}
            sandboxctl call gpu.put 0 /workspace/src /work/src: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":4}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":0,"end":{"moved":{"entries":3,"bytes":9}}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":4}}
            sandboxctl call gpu.run 0 -- sh -c echo "in $(pwd) as $(id -u), with CUDA=$CUDA"; echo to stderr >&2; mkdir out; tr a-z A-Z < src/a > out/A; cp --recursive src/deep out/: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":3}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":1,"end":{"exited":{"code":0}},"stdout":"in /work as 65532, with CUDA=7\n","stderr":"to stderr\n"},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":3}}
            sandboxctl call gpu.get 0 /work/out /workspace/got: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":2}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":2,"end":{"moved":{"entries":3,"bytes":9}}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":2}}
            sandboxctl call gpu.put 0 /workspace/src /work/src: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":1}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":3,"end":{"failed":"putting the tree at /work/src in the runner failed: /work/src: File exists (os error 17): the tree's root becomes /work/src, which must not exist: give `--replace` to replace it"}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":1}}
            sandboxctl call gpu.run 0 --timeout 1 -- sleep 60: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":4,"end":"timed_out","stdout":"","stderr":""},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            sandboxctl call gpu.close 0: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            got: /workspace/got /workspace/got/A /workspace/got/deep /workspace/got/deep/b , A: HELLO, deep/b: deep
            sandboxctl call submit /workspace/six --arg finalize=true: exit 0: {"kind":"call","returned":{},"remaining_seconds":"<s>"}"#]],
    );
}

/// What the agent of the run whose final call's target leases a runner submits: a program that
/// prints `/answers`, a file of the target's image, or, if it can't read it, 7.
const PROGRAM: &str = "cat /answers || echo 7";

const LEASING_FINAL: &str = r#"
    echo "the judge reads /answers: $(cat /answers)"
    ready() { until SANDBOXCTL call box.status 0 | grep -q '"state":"ready"'; do sleep 1; done; }
    SANDBOXCTL call box.open > /dev/null
    ready
    SANDBOXCTL call box.put 0 /pumpkin/in/payload /work/submission > /dev/null
    ready
    SANDBOXCTL call box.run 0 -- sh /work/submission/raw > /dev/null
    ready
    status=$(SANDBOXCTL call box.status 0)
    echo "$status" | sed --regexp-extended 's/"(elapsed_seconds|busy_seconds|idle_seconds|instance_seconds)":[0-9]+/"\1":"<s>"/g'
    score=$(echo "$status" | sed --quiet 's/.*"stdout":"\([0-9]*\)\\n".*/\1/p')
    printf '{"raw_score": %s}' "$score" > /pumpkin/out/result.json"#;

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn final_call_leases_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let agent = format!(
        "#!/bin/sh\nprintf '%s\\n' '{PROGRAM}' > /workspace/program\n\
         {SANDBOXCTL_PATH} call submit /workspace/program --arg finalize=true\n"
    );
    let mut ir = workflow("", &["/workspace/agent"]);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": agent });
    let runtime = &mut ir["roles"][0]["runtime"]["agent"];
    runtime["files"].as_array_mut().unwrap().push(launcher);
    ir["roles"][2]["runtime"]["target"]["sessions"] = json!([{ "name": "box", "session": {
        "runner": "box", "instances": 1, "instance_seconds": 120, "transfer_bytes": 1_048_576,
    } }]);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("box");
    runner["image"] = json!(RUNNER);
    runner["runtime"] = json!({ "runner": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    scratch.write_task("isolation", &ir);
    let judge = LEASING_FINAL.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    scratch.write("isolation/final.star", &final_running(&judge));
    // The target's, and the agent's, image holds `/answers`; the runner's doesn't.
    let recipe =
        format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace && echo 42 > /answers\n");
    scratch.write("isolation/Dockerfile", &recipe);
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "isolation", "pumpkin.toml");
    let mut run = scratch.pumpkin(&["run", "isolation/task.star", "--sandboxd", &sandboxd]);
    run.args(["--wait", "--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let rootful = !engine.report().security.rootless;
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let mut lines = vec![format!("exit status: {:?}", output.status.code())];
    let listed = containers(&context).list(&RunTag::try_from(id.to_owned()).unwrap());
    lines.push(format!("listed once it ended: {:?}", listed.unwrap()));
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let outcome: Value = serde_json::from_str(&outcome).unwrap();
    lines.push(format!(
        "values: {}",
        outcome["verdict"]["result"]["values"]
    ));
    for stream in ["stdout", "stderr"] {
        // The lease's second work, after the put.
        let path = record.join(format!("leases/0/log/1/command.{stream}"));
        let text = fs::read_to_string(path).unwrap_or_default();
        lines.push(format!("the program's {stream}: {text:?}"));
    }
    lines.extend(standard_outputs(&record));
    let shown = [(&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &shown,
        &expect![[r#"
            exit status: Some(0)
            listed once it ended: []
            values: [{"name":"score","value":7}]
            the program's stdout: "7\n"
            the program's stderr: "cat: can't open '/answers': No such file or directory\n"
            agent `agent`'s standard output:
            {"kind":"call","returned":{},"remaining_seconds":60}
            job 0's command 0's standard output:
            the judge reads /answers: 42
            {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":1,"end":{"exited":{"code":0}},"stdout":"7\n","stderr":"cat: can't open '/answers': No such file or directory\n"},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":null}}"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn ops_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    scratch.write_task("ops", &ops_workflow());
    scratch.write("ops/final.star", &final_running(OPS_FINAL));
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "ops", "pumpkin.toml");
    let runs = format!("runs/{},\"quoted\"", "long".repeat(30));
    let mut run = scratch.pumpkin(&["run", "ops/task.star", "--sandboxd", &sandboxd, "--wait"]);
    run.args(["--runs", &runs, "--config", "pumpkin.toml"]);
    let invoked = under_umask(&mut run, 0o002);
    let output = with_room(&mut run);
    let rootful = !engine.report().security.rootless;
    let mut lines = vec![report(invoked, &output, &scratch.0, rootful)];
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let listed = containers(&context).list(&RunTag::try_from(id.to_owned()).unwrap());
    lines.push(format!("listed once it ended: {:?}", listed.unwrap()));
    let sockets = scratch.0.join(dir).join("sockets");
    let over = sockets.join("agent-0/s").into_os_string().len() > 107;
    lines.push(format!(
        "the agent's socket's path is over 107 bytes: {over}"
    ));
    lines.push(format!("socket directory left: {}", sockets.exists()));
    let below = modes_below(&scratch.0.join(dir));
    lines.push(format!("modes in the record: {below}"));
    lines.extend(standard_outputs(&scratch.0.join(dir)));
    let shown = [(&sandboxd[..], "<sandboxd>"), (&runs[..], "<runs>")];
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &shown,
        &expect![[r#"
            $ umask 002 && pumpkin run ops/task.star --sandboxd <sandboxd> --wait --runs <runs> --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/<runs>/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/<runs>/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> train job 0 of agent 0 asked on local/container
            pumpkin:    <t> train job 0 of agent 0 started
            pumpkin:    <t> train job 0 of agent 0 ended: failed: the call's timeout passed
            pumpkin:    <t> train job 1 of agent 0 asked on local/container
            pumpkin:    <t> train job 1 of agent 0 started
            pumpkin:    <t> train job 1 of agent 0 ended: bytes=6
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 2 asked on local/container
            pumpkin:    <t> final call job 2 started
            pumpkin:    <t> final call job 2 ended: score=6
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> train job 0 of agent 0 on local/container, started after <t>, ran <t>: failed: the call's timeout passed (timed out)
                 <t> train job 1 of agent 0 on local/container, started after <t>, ran <t>: bytes=6
                 <t> final call job 2 on local/container, started after <t>, ran <t>: score=6
              sandboxes: 4 on local/container, <t> in all, 4 created, all deleted
            pumpkin: run <id> finished with a result (score=6); its record is <scratch>/<runs>/<id>
            pumpkin: to run it again: pumpkin run --runs '<scratch>/<runs>' --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/ops/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/, jobs/0/sandbox.json, jobs/1/got/1/result.json, jobs/1/log/0/command.stderr, jobs/1/log/0/command.stdout, jobs/1/out/, jobs/1/sandbox.json, jobs/2/got/1/result.json, jobs/2/log/0/command.stderr, jobs/2/log/0/command.stdout, jobs/2/out/evidence.txt, jobs/2/out/result.json, jobs/2/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json, sources/task/lib/train.star, sources/task/ops/final.star, sources/task/ops/submit.star, sources/task/ops/task.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 2's sandbox, jobs/2/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":4,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
            listed once it ended: []
            the agent's socket's path is over 107 bytes: true
            socket directory left: false
            modes in the record: directories 755; files 644
            agent `agent`'s standard output:
            700 .codex
            750 .codex/skills
            600 .codex/config.toml
            700 .tmp
            755 agent
            standard input: Sandbox commands for role agent
            Work time upper bound: 300 seconds. Run `sandboxctl ops` for current limits.

            sandboxctl call train [FILE] [--wait SECONDS]
            Payload limit: 8 bytes. Timeout: 5 seconds per call. Call limit: 100.

            sandboxctl call submit [FILE] [--arg finalize=BOOL] [--wait SECONDS]
            Default for finalize: false.
            Payload limit: 16 bytes. Returns immediately.

            FILE is a regular file containing the payload. Omit FILE for an empty payload.
            Use --wait SECONDS to wait for the job. A completed job has a returned field. Without --wait, follow the job number with sandboxctl status JOB.

            ---

            Task

            Solve the task in /task/README.md.
            sandboxctl ops: exit 0: {"kind":"ops","role":"agent","remaining_seconds":"<s>","resources":{"cpu_millis":500,"memory_bytes":268435456},"grants":[{"kind":"job","name":"train","payload_max_bytes":8,"calls":100,"cumulative_seconds":null,"call_timeout_seconds":5},{"kind":"now","name":"submit","payload_max_bytes":16,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            sandboxctl status 0: exit 1: {"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}
            sandboxctl call nope six --wait 15: exit 1: {"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}
            sandboxctl call train nine: exit 1: {"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}
            sandboxctl call train seventeen: exit 1: {"kind":"refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16"}
            sandboxctl call train slow --wait 1: exit 124: sandboxctl: wait expired after job 0 was admitted: inspect it with `sandboxctl status 0`
            sandboxctl call train six --wait 60: exit 0: {"kind":"status","job":1,"state":"completed","returned":{"bytes":6},"remaining":{"calls":98,"seconds":null}}
            sandboxctl status 0: exit 2: {"kind":"status","job":0,"state":"errored","failure":{"code":"call_timeout","message":"the call ran past its timeout"},"remaining":{"calls":98,"seconds":null}}
            sandboxctl call submit six --arg finalize=true --wait 15: exit 0: {"kind":"call","returned":{},"remaining_seconds":"<s>"}
            job 0's command 0's standard output:
            job 1's command 0's standard output:
            job 2's command 0's standard output:
            judged 6 bytes as 65532, with HOME=/pumpkin/home, in /pumpkin/home"#]],
    );
}

/// How many records `runs`, the runs directory in `scratch`, holds.
fn runs_made(scratch: &Scratch) -> usize {
    fs::read_dir(scratch.0.join("runs")).map_or(0, Iterator::count)
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is what each share shows, and the snapshot"
)]
fn cpus() {
    let cores = machine_cores();
    if cores < 5 {
        eprintln!("skipped: the machine has {cores} cores, and the test needs 5 at least");
        return;
    }
    let scratch = Scratch::new();
    let stand_in = StandIn::new(&scratch);
    let threads = machine_cpus() / cores;
    let sized = |name: &str, [agent, train, last]: [u64; 3]| {
        let mut ir = workflow("", &["true"]);
        for (role, count) in [agent, train, last].into_iter().enumerate() {
            ir["roles"][role]["needs"]["cpu_millis"] = json!(count * 1000 - 500);
        }
        scratch.write_task(name, &ir);
    };
    sized("first", [1, 2, 1]);
    sized("second", [2, 1, 1]);
    // A core fewer than the two runs leave, then one for the trainer.
    sized("third", [threads * (cores - 4), 1, 1]);
    stand_in.hold("create");
    let started = |task: &str| {
        let output = stand_in
            .run(&format!("{task}/task.star"), &[])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let (first, second) = (started("first"), started("second"));
    let shares = |id: &str| -> BTreeMap<String, BTreeSet<u32>> {
        let entry = fs::read_to_string(ledger(&scratch).join(format!("{id}.json"))).unwrap();
        let entry: Value = serde_json::from_str(&entry).unwrap();
        // Each role runs one sandbox at a time, instance 0.
        let roles = entry["roles"].as_object().unwrap().iter();
        let role = |key: &str| key.strip_suffix("/0").unwrap().to_owned();
        roles
            .map(|(key, cpus)| (role(key), cpu_list(cpus.as_str().unwrap())))
            .collect()
    };
    let (ours, theirs) = (shares(&first), shares(&second));
    // Whole cores, and none more than hold `needed` CPUs.
    let fitted = |cpus: &BTreeSet<u32>, needed: usize| {
        let whole = cpus.iter().all(|&cpu| siblings(cpu).is_subset(cpus));
        let core = cpus.first().map_or(0, |&cpu| siblings(cpu).len());
        whole && cpus.len() >= needed && cpus.len() - core < needed
    };
    let all = |shares: &BTreeMap<String, BTreeSet<u32>>| -> BTreeSet<u32> {
        shares.values().flatten().copied().collect()
    };
    let mut lines = vec![
        format!(
            "the first's roles on whole cores, as few as hold their CPUs: {}",
            fitted(&ours["agent"], 1)
                && fitted(&ours["trainer"], 2)
                && fitted(&ours["final_judge"], 1)
        ),
        format!(
            "its agent and trainer apart: {}",
            ours["agent"].is_disjoint(&ours["trainer"])
        ),
        format!(
            "its final judge on its agent's: {}",
            ours["final_judge"] == ours["agent"]
        ),
        format!(
            "the second's on whole cores, as few as hold their CPUs, apart: {}",
            fitted(&theirs["agent"], 2)
                && fitted(&theirs["trainer"], 1)
                && theirs["agent"].is_disjoint(&theirs["trainer"])
        ),
        format!(
            "the second's apart from the first's: {}",
            all(&ours).is_disjoint(&all(&theirs))
        ),
    ];
    let refused = ran(stand_in.run("third/task.star", &[]), &scratch.0, false);
    let refused = (refused.replace(&first, "<run>").replace(&second, "<run>"))
        .replace(
            &format!("need {} whole", cores - 3),
            "need <cores - 3> whole",
        )
        .replace(
            &format!("`agent` {}", threads * (cores - 4)),
            "`agent` <threads × (cores - 4)>",
        )
        .replace(&format!("the {cores} that"), "the <cores> that");
    lines.push(format!("{refused}\nrecords made: {}", runs_made(&scratch)));
    stand_in.release("create");
    // Each run's driver outlives its `pumpkin run`, and writes into the scratch until it ends.
    let ended = |id: &str| {
        let wait = scratch.pumpkin(&["wait", id, "--runs", "runs"]).output();
        format!("the run ended: {}", wait.unwrap().status.code() == Some(3))
    };
    lines.extend([&first, &second].map(|id| ended(id)));
    let third = stand_in.run("third/task.star", &[]).output().unwrap();
    lines.push(format!("then the third starts: {}", third.status.success()));
    lines.push(ended(String::from_utf8(third.stdout).unwrap().trim()));
    check(
        &scratch,
        lines.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            the first's roles on whole cores, as few as hold their CPUs: true
            its agent and trainer apart: true
            its final judge on its agent's: true
            the second's on whole cores, as few as hold their CPUs, apart: true
            the second's apart from the first's: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run third/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: reserving on provider `local`: the run's roles need <cores - 3> whole cores at once, to hold their CPUs (`agent` <threads × (cores - 4)>, `trainer` 1, `final_judge` 1), and runs <run>, <run> hold 4 of the <cores> that this machine gives sandboxes: run it once they end, or give its roles fewer CPUs
            records made: 2
            the run ended: true
            the run ended: true
            then the third starts: true
            the run ended: true"#]],
    );
}
