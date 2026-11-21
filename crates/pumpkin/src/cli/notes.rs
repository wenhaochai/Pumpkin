//! The texts that end each command's help, below its options, and the topics that
//! `pumpkin help` prints: what a command's help says past its options, its examples and exit
//! statuses among it.

pub(super) const PUBLISH_NOTES: &str = "\
Example, of the runs copied from a controller:
  pumpkin controller publish --runs records --bucket pumpkin-alice-public --region us-west-1 --since 2026-11-15T00:00:00Z --controller-address 172.31.5.6 --controller-address 198.51.100.9

Exit status:
  0  Every upload ran.
  1  An error, which a `pumpkin:` line on the standard error says; the next
     publish uploads what this one didn't.
  2  A usage error.";

pub(super) const CHECK_NOTES: &str = "\
Examples:
  pumpkin check tasks/bench/task.star --arg model=model-a
  pumpkin check tasks/bench/task.star --arg model=model-a --arg deadline=3600
  pumpkin check tasks/*/task.star --arg model=model-a

Exit status:
  0  The check found no problem.
  1  A task has problems, each said at its place; or another error. Every
     task is checked all the same.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why, and an exit of 1 then with the command that checks again the tasks that
failed, their entries' paths made absolute. On success, `check` prints nothing.

A problem is said at its file's path below the root, its line and its column,
then its source line, with carets under what it is about; a recipe's, at the
image() call that declares it. Past 20 problems, the rest are counted.

The lint, once a task has no other problem, finds in the files that it read
mistakes that evaluating them doesn't, as they may lie on paths that the args
given don't run: a name loaded but never used; a name bound but never used, as
a function's local, or a function or a global whose name starts with `_`; an
expression whose value is never used; a name used where a path to the use may
not have bound it; a statement never reached; a function that returns a value
on some paths but not on others; and what the language refuses once run, as
iterating over a string. It holds each file to one convention too: a docstring
for the file and for each public function, which names its parameters under
`Args:` and says what it returns under `Returns:`; and names in
lower_snake_case, constants in UPPER_SNAKE_CASE, and none of `l`, `I` or `O`.
It runs the formatter beside `pumpkin`, as `pumpkin fmt` does.

`check` reads only files below the root, with no symbolic link on the way to
them, and PUMPKIN.star holds only comments. It reaches no Docker engine, no
network and no configuration, so an image's build may still fail, as on a
recipe's step that fails.";

pub(super) const FMT_NOTES: &str = "\
Examples:
  pumpkin fmt
  pumpkin fmt --check lib tasks/bench/task.star

Exit status:
  0  Every file is formatted; with --check, every file was already.
  1  With --check, a file isn't formatted; or a file can't be formatted, as it
     isn't valid Starlark; or another error. Every file is formatted, or
     checked, all the same.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why, and an exit of 1 then with the command that formats the files that
failed, their paths made absolute. On success, `fmt` prints nothing.

`fmt` runs the formatter beside `pumpkin`, where `cargo xtask build` puts it,
which reads no configuration, so that every `.star` file is written alike.";

pub(super) const RUN_NOTES: &str = "\
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
--wait follows the job.";

pub(super) const WAIT_NOTES: &str = "\
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
ended needs no controller.";

pub(super) const LS_NOTES: &str = "\
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
failed.";

pub(super) const CANCEL_NOTES: &str = "\
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
then copies its record here, as `pumpkin wait` does.";

pub(super) const VIEW_NOTES: &str = "\
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
\"Start it\", which starts it (`aws ec2 start-instances`); until the
controller answers, each request asks EC2 whether it runs (`aws ec2
describe-instances`) in place of connecting. --runs serves a directory here,
such as the copies of the controller's runs that `wait` makes.";

pub(super) const HELP_NOTES: &str = "\
Examples:
  pumpkin help run
  pumpkin help config

`pumpkin help <command>` prints what `pumpkin <command> --help` does.";

pub(super) const BUILD_NOTES: &str = "\
Examples:
  pumpkin build tasks/bench/task.star --arg model=model-a > workflow.json
  pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml

Exit status:
  0  The images are built, and the IR printed.
  1  The task has problems, or an image's build failed, each said at its place;
     or another error.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why, and an exit of 1 then with the command that builds the task again, its
paths made absolute. The standard output holds only the IR, as a run's record
keeps it in ir.json; the builds' output goes to the standard error.

Every problem that `check` finds is said before the configuration is read, and
so before the engine is reached and any image is built. An image is built from
a stage of its context, and tagged by the digest of the build's inputs, in
pumpkin/<its recipe's directory below the root>, or, for a recipe of the
standard library, in pumpkin-library/<its directory below @pumpkin//>; one that
the engine has under its tag already is not built again. Images are built at
once, as many as the CPUs of this machine that `pumpkin` may run on, as Cargo's
jobs are, even if the engine is another machine's, each once those it is built
on are built. All that each build's `docker build` wrote is said at once when it
ends, as `make --output-sync` says a job's, then that it ended. A failed build
is said at the image() call that declares its image; once one has failed, no
other starts but those started with it, and each that runs is waited for, and
said if it fails.

With [controller] in the configuration, `build` sends the task as `pumpkin run`
does, follows the build's job on the controller, and prints the IR it printed.
A job there, of `run` or `build`, builds once it takes one of the controller's
build slots, one for every two of its CPUs, waiting, if each is taken, until one
is free; and, once a prune has removed what images no job holds, refuses to
build while more than 85% of the controller's volume is used.";

pub(super) const REAP_NOTES: &str = "\
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
cleaned before the cut.";

pub(super) const CONTROLLER_NOTES: &str = "\
Examples:
  cd ~/trees/tasks/20261108-000905-126a21e9 && pumpkin controller job 20261108-000905-126a21e9 --region us-west-1 --parameter /pumpkin-alice/settings --tree 5d0c5f0e83a3c40be3e7f2b4d1f9a2b6c8e7d6a5b4c3d2e1f0a9b8c7d6e5f4a3 -- run tasks/bench/task.star --arg model=model-a
  pumpkin controller boot
  sudo ./pumpkin/bin/pumpkin controller install

Exit status:
  0  The job's command, the boot or the install has ended as it should.
  1  An error, which a `pumpkin:` line on the standard error says.
  2  A usage error.";

pub(super) const CONFIG: &str = "\
pumpkin.toml, the machine's configuration, which every `pumpkin run` and
`pumpkin build` reads: the file that --config names,
$HOME/.config/pumpkin/pumpkin.toml by default. It is TOML, with the tables below.
Every key is required, but those of [gateway], the [upstreams.<name>] tables,
[publish] and its `since`, and any other key is refused. [controller] takes the
place of [docker], [providers.<name>.<kind>] and [gateway] on a machine whose
runs go to the person's controller.

Example, of a machine whose runs go to EC2 and Modal:
  [docker]
  context = \"default\"

  [providers.aws.ec2]
  region = \"us-west-1\"
  repositories = { roles = \"pumpkin/roles\", sandboxd = \"pumpkin/sandboxd\" }

  [providers.aws.ec2.timer]
  group = \"pumpkin\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-timer\"

  [providers.aws.ec2.classes.cpu]
  template = \"lt-0123456789abcdef0\"
  version = 1
  vcpus = 8
  memory_mib = 16384
  cpu_features = [\"avx2\"]

  [providers.gpus.modal]
  profile = \"pumpkin\"
  dials_from = \"203.0.113.7\"

  [providers.gpus.modal.images]
  region = \"us-west-1\"
  repository = \"pumpkin/roles\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-modal-pull\"

  [providers.gpus.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

  [gateway]
  private = \"172.31.0.10:7433\"

  [upstreams.gateway]
  url = \"https://gateway.example/v1\"
  key_file = \"gateway-key\"

  [[upstreams.gateway.models]]
  name = \"model-a\"
  upstream_name = \"provider/model-a\"
  usd_per_mtok = { input = 1.25, cached = 0.125, output = 10 }

Example, of a machine whose runs go to the person's controller, and Modal:
  [controller]
  name = \"alice\"
  region = \"us-west-1\"

  [controller.modal]
  profile = \"pumpkin\"

  [controller.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

[docker], the machine's Docker engine:
  context       The context that reaches it, as `docker context ls` lists it.
  cpus          The CPUs that its sandboxes may be given, each run whole cores
                of its own, as the kernel lists CPUs, such as \"1-7,9-15\";
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
                  one, such as \"H100!\", or \"A100-80GB:2\" for two, with
                  `gpu_model`, its model as nvidia-smi names it, such as
                  \"NVIDIA H100 80GB HBM3\". A class with a GPU is offered
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
";
