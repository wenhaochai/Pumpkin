# Architecture

This is a map of Pumpkin's code: what each part is for, and what stays true across the parts. It
holds what changes slowly; each module's doc holds the rest. The rules for changing the code are
in [`AGENTS.md`](AGENTS.md) and the guides in `docs/dev/`. Names in backticks are for symbol
search.

## Bird's-eye view

Pumpkin runs workflows in which one or more agents work on a task. A final call judges their
submissions. The agent's code is untrusted: it may be buggy, and it is goal-directed, so it uses
whatever it can reach to finish its task or raise its result. Pumpkin's job is to give each part
of a run exactly what the workflow grants it and nothing more, to bound what a run spends in time
and money, and to keep a record of the run that no sandbox can alter.

A workflow is data, the workflow IR. It declares the agent roles, targets for calls, and runners
for leases, the grants each role holds, each role's model route, and the final call, a body that runs
in a fresh sandbox of its target and returns the run's result. A task's
source, in Pumpkin's workflow language, a subset of Starlark, evaluates to that IR and to the
images its roles run. The IR is the only input the runtime reads, and the bodies of its functions
and of its final call reach it as `Bodies`, which the driver gives, so that the runtime depends on
no language.

Each instance of a role is a sandbox, which a provider makes: a container on the operator's Docker
engine, a container on an EC2 instance of its own, or a Modal sandbox. In every sandbox,
`sandboxd` meets the host and serves one session, which is the sandbox's only way out. Over it the
host puts and collects files, starts commands and freezes the sandbox, and code in the sandbox
makes ops, such as a call of a function, and model requests. On the host, the gateway holds each
session's end, the decision core decides what the run does next from what has happened, and the
runtime carries out those decisions on the providers and keeps the run's record.

A run is one process, the driver, on one machine: the operator's own, or the person's controller,
an EC2 instance that runs the runs a machine sends it.

![The programs and machines of a run: the operator's pumpkin starts the driver on the operator's machine or on the controller; the driver's parts, the sandboxes that the providers create and the model provider that the route reaches.](docs/figures/run-machines.svg)

## Entry points

`pumpkin` (`crates/pumpkin`) is the operator's command line. `check` evaluates and checks a task
without reaching an engine, `fmt` formats `.star` files, `build` builds a task's images and prints
its IR, and `run` builds them and starts the run in a driver process. `wait`, `ls`, `cancel` and
`reap` read runs, and end them, through their records, and `view` shows them in a browser.
`main.rs` dispatches the commands, and `run`'s path through the driver is the one to read first.

The controller is the same `pumpkin`, under `pumpkin controller`, on the EC2 instance that
`deploy/aws` makes. A machine whose configuration names `[controller]` sends `run` and `build`
there over `ssh` to its private address, and follows, lists, stops, reaps and views the runs
there. The client's `controller serve` receives the files by content, starts jobs, streams
their output, copies ended records and handles commands that inspect or stop runs. Each job runs
in its own systemd unit. A job install publishes its `bin/pumpkin-boot` link with the build.
On boot, systemd runs these entries so each build reaps the records it wrote.

![The data between a machine and its controller, over ssh: the task's tree to the controller's store, the job's start, what the run says as it is followed, the rows that ls lists, the ids of the jobs that wait and cancel find a run among, and an ended run's record, each as the controller modules of pumpkin carry it, and the view's requests for the runs' data and their replies, over one ssh to serve on the controller.](docs/figures/controller-data.svg)

`sandboxd` (`crates/sandboxd`) is the supervisor in every sandbox, a static binary. Run under the
name `sandboxctl`, it is the shim through which code in the sandbox makes ops. `ec2-boot`
(`providers/ec2/boot`) boots each instance of the EC2 provider.

`cargo xtask` (`xtask`) holds the contributor tasks, which `cargo xtask --help` lists. A task
repository runs the release its `.pumpkinversion` pins through `./pumpkin`, the released launcher.

The key types are `ir::Workflow`, a workflow as written; `workflow::Workflow`, parsed, and
`Placed`, its roles placed in the classes of sandbox that providers offer; `Core`, a run's
decisions, with its `Event`s and `Action`s; `Run`, a run on its `Host`; `Provider`, a provider a
run has connected to; `Session`, the host's end of a sandbox; `Engine`, the operator's Docker
engine; and `Record`, a run's record.

## Code map

Each crate depends only on crates listed above it here, but for `test-support`, which many
crates' tests use. Pay attention to the **Architecture Invariant** notes: they often name
something deliberately absent, and each says what holds it.

![How the workspace's crates depend on one another: a box for each crate, above the crates it depends on, with an arrow to each that no other of its arrows already leads to; the crates that build a binary are shaded.](docs/figures/crates.svg)

### `crates/stdx`

What `std` lacks and several crates share: errors with context (`Context`), deadlines
(`Deadline`), commands that get only the environment their caller states (`process::command`),
run to their end within a bound (`process::run`) or on their own (`process::spawn`), slots held by
a lock on a file (`slots`), signals noted rather than acted on (`signal`), and a panic that aborts
the whole process (`abort_on_panic`).

**Architecture Invariant:** every `process::run` states its `Bound`, which has no unbounded case
and no default, and its command is killed at the bound, or once a captured stream passes 128 MiB.
A run whose streams are all captured or discarded leads a process group of its own, which is
killed whole as the run ends, so what its command started ends with it, but for a process that
leaves the group. A run that inherits a stream stays in its caller's group, so that the terminal's
Ctrl-C reaches it, and its command alone is killed at the bound.

**Architecture Invariant:** it knows nothing of Pumpkin. Its manifest names no Pumpkin crate.

**Architecture Invariant:** it holds the workspace's only `unsafe`, the `sigaction` in
`signal::catch`. The workspace's lints deny `unsafe_code`, and that function alone expects it.

### `crates/schema`

The formats that Pumpkin's programs share: the workflow IR (`ir`); a workflow parsed and placed
(`workflow`, with `Workflow`, `Placed` and `Class`); the frames that its protocols carry
(`frame`); the sandbox protocol (`sandbox`), with its trees and ops, how a sandbox meets the
gateway, and the in-sandbox contract, the paths under `/pumpkin` and the limits that a role's
commands and the session's owner share; and what the EC2 provider hands `ec2-boot`
(`ec2_boot::Start`), in the user data that the provider writes and `ec2-boot` reads
(`ec2_boot::user_data`). The IR, the in-sandbox contract and `sandboxctl`'s JSON are interfaces that
task authors rely on.

**Architecture Invariant:** it depends on `serde` and `serde_json` alone, since `sandboxd` and
`ec2-boot`, small static binaries, depend on it. Its manifest holds this.

**Architecture Invariant:** only `Workflow::parse` makes a `workflow::Workflow`, and only
`Workflow::place` makes a `Placed`, so code that takes one need not check again what they
established. The types' private fields hold this.

**Architecture Invariant:** the IR holds no defaults. An absent field means what its doc says,
such as none or no limit, and whatever writes the IR picks the defaults. Review holds this, as no
check does: the IR's types default only an `Option` or an empty collection.

**Architecture Invariant:** the agent never reaches the final call's target, nor what a target
runs. No grant names the final call's target, as parsing refuses a final call whose target a job
function names, and parsing refuses an agent's session of a runner whose image is a target's.

### `crates/decision`

The decision core. `Core` takes each `Event` that the runtime observed, with the time since the
run started, and returns the `Action`s to perform, until `Action::Finish` carries the run's
`Outcome` or `Action::Abandon` ends it at its hard end. Its `ops` module authorizes ops, `call`
takes a call of a function by the function's kind, `jobs` meters the calls of job functions and
the final call, and `leases` meters and ends leases; `lasts` is a run's hard end.

**Architecture Invariant:** the core does no I/O, reads no clock and uses no randomly seeded hash,
so the same events give the same actions, and time reaches it only with each event. The crate is
`#![no_std]` and depends on `schema` and `serde_json` alone, though `schema`'s own I/O items stay
reachable through it.

**Architecture Invariant:** only the core authorizes ops. `sandboxd` carries any op that any code
in its sandbox makes. The gateway hands each to the session's owner as an `Arrival`. The runtime
identifies the caller by the agent role or job registered for that session and sends the core's
`Action::Reply` back to that caller.

**Architecture Invariant:** the agent learns of a call only its state, what its function has left,
how many of its target's model requests a budget barred, and what its body returns, so the
workflow decides the agent's feedback. The status reply's types hold nothing else.

**Architecture Invariant:** before the final call starts, the driver confirms deletion of every
work sandbox it claimed, including shared targets and lease runners: the core starts it once every
agent and call is torn down, its sandbox's delete confirmed, and every lease has ended. A call's
caller may hear its result before then. Work ends at the common
deadline or once every agent has ended its work. The final body
receives every agent role's last submission, an opaque payload or `None`. The body sends a selected payload
to a file with `put` or to a command’s standard input with `capture`.

A target with work lifetime has one sandbox across its work-phase grants. The core admits one complete
body at a time across all those grants. The runtime supplies the caller’s role from its session.

A failed provider create can allocate a sandbox without returning its identity. The driver gives
that sandbox no session or command. It sweeps the affected providers before the final call and
all providers at run end. An allocation can become visible only on a later sweep.

### `crates/recipe`

Image recipes, Dockerfiles. `check` refuses a recipe that may have Docker fetch what no digest
pins.

**Architecture Invariant:** it does no I/O and knows no Pumpkin crate, so the language, `pumpkin`'s
check of a task and the engine's build share one check without depending on each other. It is
`#![no_std]`, and its manifest names no dependency.

### `crates/lang`

The workflow language, a subset of Starlark. `evaluate` runs an entry file and the files it loads,
given its `Args`, to a `Plan` of the images it declares and the workflow that names them, or to
its `Problems`. `check` reads a body, a `def` of a file that a workflow loads, and `call` calls one
for a host during a run.

**Architecture Invariant:** evaluation is hermetic, so the same sources and args always give the
same plan. It reads source only through its `Loader`, by `Label`s that stay below their root, the
task's (`//`) or the standard library's (`@pumpkin//`); it only declares images, for its caller to
build, and reaches a host only through the handle that `call` gives a body; and the crate is
`#![no_std]`. What a call returns rests also on the grant's keywords and on what the `Host`'s
methods return. Every evaluation ends, within its `Bounds` and `STACK_BYTES`, once each method of
the `Host` that it calls has returned.

**Architecture Invariant:** the standard library depends on no task: a file below `@pumpkin//`
loads only files below it, which evaluation refuses otherwise.

**Architecture Invariant:** only `pumpkin` depends on it, and `library`'s tests, so the runtime,
the core and the providers never see the language.

### `crates/library`

Pumpkin's standard library: the files of the repository's `library/` directory, built into the
crate (`FILES`), which `pumpkin` serves below `@pumpkin//`. A harness is a directory there, such as
`harness/codex/`: a `.star` file whose function gives an agent role's image and runtime, with its
route, and, in a directory of its own, the `Dockerfile` that adds the harness to the agent's image,
so that the layer's inputs are that recipe alone.

**Architecture Invariant:** it holds files and nothing else, so that what a library function gives
rests on the language alone. It depends on no crate; its tests do.

**Architecture Invariant:** a harness is known only here. No other crate's code names one: the
headers a harness's requests and responses carry beyond its API's own are data its route entry
lists.

### `crates/tls`

TLS 1.3 below a session whose connection crosses the Internet: a `Client`, which trusts a server
only by its certificate, and a `Server`.

**Architecture Invariant:** TLS stays below the session. Each end hands its session a `Stream`, a
Unix socket that a relay thread carries to and from the TLS connection, so a session's protocol
and bounds are the same with TLS as without it.

### `crates/gateway`

The host's end of the sessions that sandboxes open. A sandbox meets the gateway where its
`Registration` says, which `Sockets`, a `GrantListener` or a `Dialer` issues. The `Session`'s one
owner moves trees, runs commands (`Process`) and hears of each op, model request and exit as an
`Arrival`; through `Trees`, another thread moves trees beside it.

**Architecture Invariant:** a sandbox can't harm the host through its session. Anything out of
protocol ends the session, and a session costs the host only bounded memory and bounded waits:
frames are capped, a collected tree is written within its `Limits`, and every request waits only
until its caller's deadline.

**Architecture Invariant:** it knows no provider and no driver, so every provider's sandboxes have
the same sessions. It depends on `schema`, `stdx` and `tls` alone.

### `crates/sandboxd`

The supervisor in every sandbox, built static by `cargo xtask sandboxd`. It meets the gateway,
serves the session (`session`, `files`, `commands`), carries ops from `sandboxctl` (`ops`, `ctl`)
and model requests from the model endpoint (`model`), and freezes the sandbox.

**Architecture Invariant:** the protocol's two ends share no Pumpkin crate but `schema` and
`stdx`, so the protocol is all that they agree on. `sandboxd` names `gateway` only among its
dev-dependencies.

**Architecture Invariant:** it is no boundary within its sandbox. It has its role's privileges
alone, from the start, or, started as root with `--user`, from before it serves anything, so what
it does the role's own code could do too.

**Architecture Invariant:** a passive sandbox can make no op. Without `--ops`, `sandboxd` serves no
control socket, and each provider mounts `sandboxctl` only for a role that makes ops
(`Create::ops`).

### `crates/route`

The model route. `serve` answers each model request through the entry for its API that allows its
model, whose `Route` admits it in a closed form (`closure`), sends it upstream (`Upstream`), streams
the response back, and says how it answered (`Answer`). `Serving` is the upstream that serves an
entry, and `Keep` is where an exchange is kept as it passes. `read` reads a kept exchange of any API
as the same kinds of items (`Item`), and takes a stream's first output where `serve` does, by the
same rule.

**Architecture Invariant:** the route stays closed. A request reaches the models that its workflow
allows and nothing else of the model provider's: none of the state it keeps, the tools it runs or
the URLs it fetches. `serve` sends upstream only the body that a `Route` admitted, and only the
headers of its API's own and those its entry lists, of which `schema` refuses any that carries a
credential, picks an account, signs a request, frames a message or sets its coding (`HeaderName`).
The headers that a gateway in front of a provider gives meaning to are the operator's to keep off,
since no list here can name every gateway's.

**Architecture Invariant:** it holds all that reaches a model provider, so the HTTP client, `ureq`,
is in this crate's manifest alone. It knows no run: the runtime depends on it, never the reverse.

### `crates/provider`

The interface every provider implements. `Provider` reserves what a run's roles need
(`Reservation`), publishes what the run's sandboxes run, says how they meet the gateway
(`Meeting`, whose `Creator` creates them), and reads their consoles. `Sweep`, its part that lists
and deletes a run's sandboxes, is all a reap needs. Its values include `Create`, `Ready`, `Listed`
and `RunTag`.

**Architecture Invariant:** it knows no provider. Of Pumpkin's crates, it depends on `schema`
alone.

**Architecture Invariant:** runs that share a provider never reach each other's sandboxes. Every
call on sandboxes names its run's `RunTag`, every sandbox carries it, and a provider lists and
deletes only the sandboxes that carry the tag it is given.

**Architecture Invariant:** a call's outcome rests only on what it is given and what the platform
holds, never on an earlier call's state in the instance. So any instance connected to the same
platform may make it, a sandbox outlives the instance that created it, and a reap works from a
record alone. The trait's contract states this, and the conformance suite checks it of the
runtime's simulated provider and of the Docker provider, not of EC2's or Modal's, as Testing
says.

**Architecture Invariant:** a provider holds no session, and the run's times reach it as data
(`Create::by`, `Create::hard_end`). Its sandboxes meet the gateway as its `Meeting` takes, so no
provider depends on `gateway` outside its tests.

**Architecture Invariant:** no provider depends on another's crate. What two providers share is a
crate of its own, as `engine` and `aws` are.

### `crates/engine`

The operator's Docker engine, on which Pumpkin builds the images that sandboxes run, whichever
provider runs them, and from which it publishes them. `Engine` reaches the engine of a named
Docker context (`ContextName`), and reports it (`Report`) and what `docker info` says of its host
and the limits it enforces (`Host`), which the Docker provider reads. It builds images tagged by
the digest of their inputs, says what a prune removes and when the disk is too full to build
(`prune`), loads the image of `sandboxd`, and pushes (`Pusher`). `confinement` is the
confinement of a role's container on every provider whose sandbox is one.

**Architecture Invariant:** it knows no provider, so a crate that only builds or publishes images
depends on no provider. It depends on `recipe`, `schema` and `stdx` alone of Pumpkin's crates.

**Architecture Invariant:** `docker` runs against the context the operator named, and nothing in
the environment overrides it. The crate makes every `docker` command one way, for `Engine` and for
the Docker provider's calls (`docker_to`) alike: with the context as its first argument, through
`stdx::process::command`, given of the environment only what finds `docker`'s own configuration.

**Architecture Invariant:** what a build fetches is pinned, an image by its digest, a file by its
checksum and a Git tree by its full commit, so an image's tag names what it is built from. Each
build runs `recipe::check` on its recipe first. What a step's own command fetches is outside this,
unless a stage that pins each download by its checksum, with `ADD --checksum`, supplies it.

### `crates/aws`

AWS's command line as Pumpkin runs it, `Cli`, in one region, with ECR's repositories on it, a push
from the operator's engine named by its digest (`Cli::publish`), and the IAM roles that settings
name (`RoleArn`).

**Architecture Invariant:** every call that `Cli` makes is in its region, with no variable of this
process's environment but those that find `aws`'s own configuration, so nothing steers it to
another region or account. `Cli` states `aws`'s environment.

### `providers/docker`

`Docker`, the provider whose sandboxes are containers on the operator's engine, each under the
engine's `confinement`, on CPUs of its run's own (`Cpus`). `Docker::connect` refuses an engine
that, as its `Host` says, can't enforce each limit a container has. For a reap, `Docker::reach`
gives a `Reached`, which is only a `Sweep`.

**Architecture Invariant:** a container's only way out is its session's socket. It has no network
but loopback, and files move only over the session.

**Architecture Invariant:** a run's containers run on whole cores that no other run's get, from its
reservation to its end. The engine's ledger holds an entry for each run, locked by the process
that reserved it, so the kernel frees a run's cores however that process ends.

### `providers/ec2`

`Ec2`, the provider whose sandboxes are containers on EC2 instances of their own, launched in the
envelope the operator made on AWS, which its `Settings` name. It pushes what its sandboxes pull to
the envelope's repositories, and schedules each instance's termination at the run's hard end
(`Timer`). `Ec2::reach` gives a `Reached` for a reap.

**Architecture Invariant:** it reaches AWS only through `aws::Cli`. An instance's Docker holds no
registry login: ECR's credential helper gets a token with the instance's role for each pull, and
keeps none, as the user data configures root's Docker.

**Architecture Invariant:** a container reaches the gateway's address and port and nothing else,
not its instance nor the instance's metadata. `ec2-boot` installs the firewall rules before the
container starts, and `cargo xtask live ec2` probes them.

### `providers/ec2/boot`

`ec2-boot`, a static binary that the image of `sandboxd` holds beside it. A systemd unit that the
instance's user data writes copies it out of that image and runs it with the `Start` the provider
wrote: it locks the instance's network down, checks its Docker, pulls the role's image and runs the
role's container, and the unit powers the instance off however it ends.

**Architecture Invariant:** it depends on `schema` and `stdx` alone of Pumpkin's crates, so it is
small and static, and the provider's values reach it only as a `Start`.

### `providers/modal`

`Modal`, the provider whose sandboxes are sandboxes in a Modal workspace, in Pumpkin's one App,
under gVisor, each with its `sandboxd` listening behind Modal's TLS tunnel. Modal imports a role's
image from ECR with a role that it assumes by its own identity. `Modal::reach` gives a `Reached`
for a reap.

**Architecture Invariant:** it reaches Modal only through its helper, `pumpkin-modal.py` (`Helper`),
which gets of this process's environment only `HOME`, `PATH`, the profile and the configuration
file; and AWS only through `aws::Cli`. No AWS credential of the operator's reaches Modal.

**Architecture Invariant:** a sandbox opens no connection, but DNS to Modal's resolver, and its
only way in is its tunnel, which takes connections from the settings' `dials_from` address alone.
The helper creates each sandbox so, and `cargo xtask live modal` probes that a sandbox reaches
neither the metadata address nor a public one.

### `crates/runtime`

The host side. `Run::prepare` reserves on each provider and begins a placed workflow's record, and
`Run::execute` drives the `Core` on the run's providers to its outcome, or until a signal stops it
(`Stopped`). One thread drives the core and waits on an inbox of the run's `Host`, whose clock
times the run (`System`, the machine's); each call, the final call and those of job functions,
and each lease is a child on a thread of its own (`child`, `call`, `lease`); every
sandbox starts on one path (`sandbox`); and `model` serves each role's route. `record` and `log`
lay out the record, and `Record` reads it back for
`pumpkin`: a run's progress (`Readout`), the arguments that started it, its model exchanges, and
the reap of what it left (`Record::reap`).

**Architecture Invariant:** the runtime is written once, against `Provider`. It depends on no
provider's crate, not even in its tests, which run on a simulated provider, so a new provider
changes no runtime code.

A job body can use a target's session grant through a scoped runner callback. The core authorizes
and meters its operations as the job's. The runtime sends the body the receipt of its admitted
command or transfer through the job's inbox. Closing waits for the runner's recorded deletion.
On a work-lifetime target, each body holds its own leases and the sandbox has no control socket.

**Architecture Invariant:** every sandbox starts on the one start path (`Sandboxes::start`), and a
run's end deletes whatever each of its providers still lists of it, so a run ends with none of its
sandboxes left, or fails.

**Architecture Invariant:** a run that doesn't fit beside other runs is refused with no record:
`Run::prepare` makes the record only once each provider has reserved, and the run holds each
`Reservation` until its end has deleted its sandboxes.

### `crates/pumpkin`

The command line and the controller's own side. `cli` parses the command line, `config` reads the
machine's configuration (`Config`, or `Client` for a machine whose runs go to a controller), `star`
reads a task's source below its root (`Root`) and serves the standard library's, and `driver`
starts the driver process. `run` starts a run and drives it in that process, `ending` follows a run
and says how it ended, and `reaping` stops runs and removes what they left. `controller` reaches
the controller, which its `aws` finds, starts and enrols this machine's key with, over a direct SSH
connection a command keeps for its requests (`Reached`). The workspace saves its address and
serialises repairs. Its `send` sends a run's or a build's job, sharing large task uploads by digest
across up to four connections before starting the job on the first. Its `copy` receives an ended record
file by file, with `record.json` last, its
`listing` lists the runs there for `ls` and its `viewing` carries `view`'s requests there (`Viewing`), each over a `Link` that its
`linking` opens, its `follow` follows a job, and its `commands` stop and reap runs; `link` is the protocol of a
client's `Link` to `serve`, the controller's end, which starts and cancels jobs, answers `ls` and `view`,
lists the jobs, each with its member (`member`), that `wait` and `cancel` find a run among, streams a job's log and its milestones or its build IR,
and is of the client's own build, installed there
whole, its executable and the files beside it, by a SHA-256 of theirs;
`manifest` describes the task tree and the client install that `send` transfers by content, and
its `store` admits complete uploads by their SHA-256. `job` is the controller's side, whose
`start` starts each id once under the jobs' lock and keeps the install its unit runs, with the
prune and the slots that admit its builds and publishes
(`Admission`), `publish` copies the runs, as `view` exports them, to the public page's bucket as
their jobs run, `progress` is what `ls` shows, `next` makes the commands to run next, `style` runs
the formatter, `view` serves the runs' records to a browser, its `json` the JSON forms of what it
answers with, its `Answering` the view that `serve` keeps to answer a client's, and `Workspace` is
the `.pumpkin` directory.

**Architecture Invariant:** `view` writes no record, and reads one only through `runtime`'s
readers (`Record`, `LogFold`), so a record's files are laid out in `runtime` alone, and a path
that a log names is read only below its record; and it says of a run what the CLI says through
`progress`, so the page and `ls` never differ.

**Architecture Invariant:** `view` serves the person who started it alone, and changes a run only
by running `pumpkin cancel`, as `pumpkin view --help` says.

**Architecture Invariant:** `view` reads a controller's records where they are, by the `pumpkin`
of its own build there; the page's files, the token, the `Origin` check and the cancel stay on
the person's machine.

**Architecture Invariant:** the page is plain modules that the browser runs as they are, with no
library, and puts the text of a record into the document only as text, under the content security
policy of `view`'s `http`.

**Architecture Invariant:** only `style` runs the formatter, buildifier, and none of its flags or
forms is Pumpkin's interface: each file's text goes to it on its standard input, and `style` reads
what it gives back.

**Architecture Invariant:** what a task's source names stays below its root on disk, as its labels
keep it there in name. `Root`, the one `Loader`, refuses a symbolic link on the way from the root.

**Architecture Invariant:** a machine runs its roles either on its own engine or on other
providers, so the class a role is placed in never turns on the machine's memory. A configuration
that names a provider of kind `docker` names no other (`Providers::new`).

**Architecture Invariant:** each file in the controller's store has the SHA-256 that names it.
A sender holds the partial file's lock while appending and checking its digest. A blob from the
running install is checked on a private copy. Both are admitted under the store lock, which the
prune also takes. The prune leaves active partials alone. A job's tree links to the store and
its `.pumpkin` links to the controller's workspace, which no manifest path is below. A client
sends only what the store lacks, and no secret of a job's goes there.

**Architecture Invariant:** a controller's prune never removes an image that a running job holds,
or that the IR of a running job's started run names. Each job names what it holds in its directory
in tmpfs, which its unit removes as it ends, and a prune reads those names under the lock of the
jobs' directory.

**Architecture Invariant:** `serve` acts for the member that the controller's forced command
names (`--member`, a `member::Member`), and each run's job it starts records that member.
`cancel` matches only that member's runs, and those that name no member, unless asked
otherwise. This prevents accidents, not members: each member's build runs as the controller's
one user.

**Architecture Invariant:** each `pumpkin` process adds 022 to its umask as it starts (`main`), so
nothing it makes, a run's record included, is writable by another user.

### `crates/test-support`

What the tests of more than one crate share: the conformance suite, which runs through the
`Provider` trait alone (`suite`); a complete workflow (`WORKFLOW`); the Docker engine, image and
static `sandboxd` that the live Docker tests reach, and stand-ins for a provider's TLS edge and a
model provider's HTTP server. A helper that one crate's tests use stays in that crate's tests.

**Architecture Invariant:** only tests use it. Every manifest that names it does so under
`[dev-dependencies]`.

### `xtask`

The contributor tasks, which `cargo xtask --help` says. `ci` runs the checks every change passes,
among them the checks of Rust files, of documents and their figures (`tidy`), of `.star` files
(`star`), of the figure of the crates (`figure`) and of the types of the page that `view` serves
(`page`). `live` runs those that reach a real environment. `build` builds `pumpkin` and lays out the
files it needs beside it. `release` packages and checks that build and publishes its archive,
launcher and pin on GitHub. `tools` pins the contributor tools.

**Architecture Invariant:** a tag publishes only if `live examples` passed on its commit, which
the commit's status records on GitHub (`gate`).

### `deploy/pumpkin`

The consumer entry, a POSIX shell script that a release ships as `pumpkin`. It verifies
the release archive pinned by `.pumpkinversion` and unpacks it into a per-user cache keyed by
checksum. It gives the binary the repository's `.pumpkin` as its workspace. A controller job
runs the same install, uploaded by the client.

**Architecture Invariant:** `xtask` builds the binaries and lays out `bin` for releases and
developer installs.

### `deploy/aws`

The Terraform module that makes one person's Pumpkin on AWS, as [its README](deploy/aws/README.md)
says: the EC2 envelope
that `ec2` relies on, the repositories that runs push to, Modal's pull role, the sandboxes' AMI,
and the controller, with the SSM parameter of its jobs' settings and the units that stop it when
idle and reap at its boot. No crate depends on it; its outputs feed a machine's configuration.

### `examples`

Tasks in the workflow language: `hello` needs no model, `codex` takes one as `--arg model`, and
`smoke`, which `cargo xtask live` runs, probes each sandbox's confinement. `cargo xtask live
examples` runs them all before a release. `pumpkin`'s tests copy `hello`.

## Cross-cutting concerns

### Security model

The agent's code, and whatever a target or a runner runs, is untrusted, and so is every sandbox,
which may also be buggy. Each guard either limits what that code can reach, or keeps a sandbox
from harming the host or the run.

A sandbox reaches the host only over its session, and nothing else of the network: the Docker
provider gives it loopback alone, an EC2 sandbox's instance drops all but the gateway's address and
port, and a Modal sandbox opens no connection but DNS. Through the session it reaches only what the
host serves: file trees and command exits go to the runtime, ops go to the core, which authorizes
each against the grants of the caller's role, and model requests go through the closed route.
`sandboxd` and `sandboxctl` are no boundary inside a sandbox; the session is.

![The data that flows in a run: between the sandboxes and the gateway, the runtime and the decision core, and from the route to the model provider, each arrow named by the code that carries it, and into the record that the record module lays out, which keeps each sandbox's out and log, and what a job's body got.](docs/figures/run-data.svg)

A sandbox proves which it is by how it meets the gateway. On the host, it dials a socket of its
own, which only it can reach. Elsewhere, its hello presents a grant (`RegistrationGrant`) that
the gateway issued for it alone, which only `sandboxd` is given, before anything else runs in the
sandbox, and which ends with its `Registration`. A session over the Internet runs over TLS, whose
client trusts the provider's edge only by its certificate.

No credential is reachable from a sandbox. The model provider's key stays in the host's `Upstream`,
which adds it to each admitted request, and the harness reaches the model at the endpoint `sandboxd`
serves, with no credential. An EC2 instance's Docker holds no registry login, and the container
never gets the instance's role or its metadata. Images are private, in the envelope's ECR
repositories: an EC2 instance pulls with its instance profile, and Modal imports with a pull role
that it assumes by its own identity, so no AWS credential of the operator's reaches Modal.

What a call's target holds and a lease's transfers stay where the workflow puts them: of what a
call's sandbox leaves, an op reply carries only what the call's body returns, so only the operator
sees the rest, which the record keeps: the target's `out/`, each file the body got (`got/`) and
each command's log (`log/`); and a lease's transfer moves trees between its holder's sandbox and
its runner alone, through the host.
The key never reaches the record either, as `Keep` is handed bodies and the names of headers
left behind, never a header's value.

On the operator's machine, the driver reads the configuration and each key file itself, so no key
crosses argv, the environment or a pipe; a machine sends its keys to the controller only on a
connection's standard input, into its job's directory in tmpfs, which the job's unit removes as it
ends.

### Deadlines and spending bounds

Every wait of a run is bounded. A provider's calls each return within a bound, a create by the
deadline the runtime gives it, and every gateway request waits only until its caller's deadline.
The `aws` command line, the Modal helper and `ec2-boot`'s commands are each killed at a deadline,
and so is each `docker` command but a build. Before a run starts, a build and a send of a task's
files to a controller have no bound on their whole time, as their inputs' size sets it: a send
gives up only once it stalls.

A run's bounds are the workflow's: the shared work deadline and each agent's finalize grace, each job function's
calls, seconds and payload size, each session's instances, seconds, commands and transfer bytes,
and the final call's payload size and timeout. The core meters each from its own jobs and leases,
and refuses an op that would pass one. From them and each class's startup, `lasts` gives the run's
hard end, by which the core abandons the run if nothing has ended it. The model budgets of the
entries of a role's route are the one bound the core doesn't meter: the runtime meters each, from
its entry's answers, at the machine's prices.

Every sandbox ends by its run's hard end even if its driver dies: the EC2 provider schedules each
instance's termination then, and an instance whose container has started starts powering itself off
then; Modal ends each sandbox then; a Docker sandbox ends with its session. A sandbox that no start
claims ends itself: a `sandboxd` that dials finds its registration gone, and one that listens gives
up soon after its start's deadline (`--by`).

A run's spend is in its record: each sandbox's class and its price an hour, as the provider's
settings give it, each model's prices for its tokens, as the machine's configuration gives them
when the run starts, and what each model response reports of its tokens; `ls` totals the cost, and
a run's summary the tokens too. On a controller, slots and a disk watermark admit builds and
publishes, and the controller stops itself when idle.

### The run record and observability

Each run has a directory of its own in the runs directory, whose files the `record` module lays
out. The run log (`log.jsonl`) holds each event that the core decided on, with the actions it
took, timed as the runtime observed it, so a run can be read back event by event. It holds each
fact once, and leaves to their own files what the record keeps elsewhere: a command's output, a
model exchange's bodies and the outcome. A reader takes the log in typed parts,
each whole or not at all: a part that this build can't read is listed as unread (`Readout`), so no
content of the log fails a read. Many builds write records, so a listing shows a record that this
build can't read, and another build or an older one wrote (`runtime::writer`), as one `unread` row
naming that build, and fails for a record of its own build that doesn't read, and for any record
that can't be read at all, whoever wrote it (`ReadError`).

The record is written as the run goes, and each file is synced through the `Host`. The run log
only grows, a whole line at a time, and `ir.json`, `sources/` (the text of each file that the
workflow's evaluation read) and `provider.json` are written before it and never again, so a reader
of a live run reads only what the log gained (`LogFold`). A record
appears only once its `driver.pid` is locked (`Lock`), and the lock is held until the run's last
write, so `outcome.json` and the lock tell an ended run from a live or a lost one. Only the lock's
holder writes the record; once it has let go, only `Record::reap` does.

The record keeps every model exchange as the route saw it pass, which no sandbox can alter: each
request as it went upstream, or as its client sent it if the route refused it, each conversation
item and each field its API keeps whole, such as the list of tools, once by its digest, and the
answer as the upstream sent it. `pumpkin ls`, `wait`, `view` and a run's ending read only the
record, through `Record` and its `Readout`, so whatever ran the run, a machine shows it the same
way; a controller's ended runs are copied into the machine's runs directory. `ls` of a
controller's runs reads them where they are: `serve` makes their rows there (`progress::Row`).

### Configuration

Where a setting lives follows [`docs/dev/style.md`](docs/dev/style.md): facts about a machine are
its configuration, `pumpkin.toml`, which `config` reads and `pumpkin help config` describes.
Secrets are never in the configuration: it names the files that hold the keys.

A machine's runs go either to its own engine and providers or to its controller, so where they go
has one home; `pumpkin help config` says what each names.

### Errors and next commands

Errors follow [`docs/dev/style.md`](docs/dev/style.md). A check of an input reports each problem
at its place: `workflow::Workflow::parse` every one it finds, each at a JSON Pointer into the IR,
and an evaluation's `Problems` each at a file, line and column.

What `pumpkin` says of a run's ending or a refusal ends with the commands to run next (`next`):
each a line that a POSIX shell runs as it is, with absolute paths, such as how to run the task
again with the arguments its record keeps, how to follow or stop a run, or how to reap what it
left.

### Stop, cancel and reap

A run stops short of its outcome by a signal or a failure (`Stopped`). The driver notes the
signals that stop a run, which `pumpkin run --help` names (`stdx::signal`), and deletes the run's
sandboxes before it exits. `pumpkin cancel` signals the driver, waits for the run to end, and reaps
it.

A run whose driver died is lost: its record has no `outcome.json` and no lock held. A reap,
`Record::reap`, waits for a driver that still runs, then deletes, through `Sweep` alone, on each
provider that the record names, every sandbox tagged with the run that the provider lists, and
every one that the record holds undeleted, listed or not, as one that has ended may still hold
what only its delete frees; it records each in `reaps.jsonl`, and a lost run as failed.
`pumpkin reap` reaps ended or lost runs. On a controller each build reaps its own runs and names
those left to other builds. The boot unit runs the registered builds and leaves to the hard end
what it cannot reach, such as Modal's sandboxes, whose credentials reach the controller
only with a job.

### Determinism and pinning

The same inputs give the same results. The core and the language read no clock and draw no
randomness, and the workspace's clippy configuration bans `HashMap` and `HashSet`, whose
iteration order follows a seed each process draws. What Pumpkin builds or runs is pinned: an
image's tag is the digest of its build's inputs, a recipe fetches only what a digest pins, a
harness is pinned by its recipe in the standard library, the Modal helper's packages by its lock,
the formatter by version and checksum, and each contributor tool by version.

### Processes, panics and threads

A panic is a bug, and it ends the whole process: release builds abort, and each binary that runs
threads installs `stdx::abort_on_panic` first. Clippy's configuration bans `catch_unwind`, and it
bans `Command::new` in favour of `stdx::process::command`, so every child gets only the environment
that its caller states.

### What Pumpkin writes on a machine

All that `pumpkin` writes on a machine is below its workspace's `.pumpkin` directory
(`Workspace`), as `pumpkin --help` says, and each provider kind keeps what it keeps in a directory
of its own that it is handed as it connects. Outside it are only the configuration and the tools'
credentials, which it reads; the runs' records, where `--runs` names another directory; the files
that `fmt` and `lock` rewrite, which their command lines name; the Docker engine's images and its
ledger of CPUs, which every run on the engine shares; and the machine's key to its controllers,
with the host keys it pinned. On a controller, below the controller user's home, as the `job`
module lays out, the jobs also keep the store of the files that clients sent, the trees of links
into it and each job's own directory, and its `.pumpkin` keeps the installs of `serve` that the
clients upload; each run's job directory also names its member.

### Testing

[`docs/dev/testing.md`](docs/dev/testing.md) holds the rules, and `cargo xtask --help` says what
`ci` and `live` run. `cargo xtask ci` is pure: its tests reach no Docker engine, network or cloud,
so the core, the gateway, the route and the runtime are tested with Pumpkin's own processes,
loopback sockets and a simulated host and provider. No test runs against an invented model of a
cloud. The conformance suite runs on the runtime's simulated provider in `ci` and on the Docker
provider in `live docker`; the EC2 and Modal providers have no test of their own but the smoke
run.
