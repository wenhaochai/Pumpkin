<p align="center">
  <img src="docs/figures/logo.svg" width="160" alt="Pumpkin's logo: an open ring with an orange tile, turned 45 degrees, in the break at its top.">
</p>

<h1 align="center">Pumpkin</h1>

<p align="center">
  Pumpkin is an orchestration layer that handles multi-agent multi-sandbox workflows. It provides optimized communication abstractions for easy management and error handling.
</p>

<p align="center">
  <a href="docs/workflows.md">Workflows</a> ·
  <a href="docs/language.md">Language</a> ·
  <a href="docs/running.md">Running</a> ·
  <a href="docs/record.md">Records</a> ·
  <a href="docs/providers.md">Providers</a> ·
  <a href="docs/models.md">Models</a> ·
  <a href="ARCHITECTURE.md">Architecture</a>
</p>


## Why Pumpkin?

Running a multi-agent multi-sandbox flow requires extensive communication and error handling. 
Naively setting up such a workflow can result in increased obscene failures and unncessary context overhead for agents.
We built Pumpkin to provide clean abstractions so people can easily do error handling.  

| | Without Pumpkin | With Pumpkin |
| --- | --- | --- |
| Agent | A container you start. It has the network and the mounts you give it. | A sandbox the workflow names. Its only way out is its session with the driver. |
| Model key | In the container's environment, or behind a proxy you run. | On the host. The route adds it to requests the workflow allows. |
| Judge | Another container or script, wired up per task. | A fresh sandbox of the final-call role, after the agent's work has ended. |
| Limits | Timeouts and spend caps in each launcher. | The workflow's deadline, call bounds, and model budget, on every run. |
| Record | The logs you saved. | One directory per run: the workflow, the log, sandbox output, model exchanges, and the result. `wait`, `ls`, and `view` read it. |
| Where it runs | Local Docker, or another launcher for EC2 or Modal. | The same `pumpkin run`, on this machine's Docker, on EC2, or on Modal. |
| Harbor Support | Need to implement communication with extra effort | Decoupled from runtime and can directly use Harbor Run |

## Quickstart

On Linux x86-64, run `./pumpkin` from the root of the task repository. The `.pumpkinversion` file
beside the launcher selects the release. If that release is missing from your cache, the launcher
downloads it, verifies it and unpacks it there. Every repository that pins the release uses that
install and keeps its own run state in `.pumpkin/`. Docker runs the sandboxes.
For a new task repository, [install the launcher and pin](#installing-or-upgrading-a-task-repository).

One file configures the machine for every task: `~/.config/pumpkin/pumpkin.toml`. A machine whose
runs stay on its own Docker engine needs only this:

```toml
[docker]
context = "default"

[providers.local.docker]
```

Each task is an entry file, `task.star`. The [examples](examples/README.md) start with
`examples/hello`, which needs no model and runs once `./pumpkin` and `.pumpkinversion` sit beside
`examples/`. From that directory:

```console
$ ./pumpkin check examples/hello/task.star
$ ./pumpkin run examples/hello/task.star
20261116-195228-234cd166
$ ./pumpkin wait 234c
$ ./pumpkin ls
$ ./pumpkin view
```

`check` prints nothing when it finds no problem. `run` builds the images, starts the run and prints
its id. `wait` follows a run named by a prefix of that id; ending it leaves the run going. `ls`
lists the runs that are live or ended in the last day. `view` serves them to this machine's
browser. The record is `.pumpkin/runs/<id>/`.

[Run it](#run-it) shows these commands with their output.
[docs/running.md](docs/running.md) goes through installing, configuring and each command.

### Installing or upgrading a task repository

For a first install or an upgrade, the task maintainer chooses a tag from the
[published Pumpkin releases](https://github.com/FrontierCS/Pumpkin/releases).
Replace `<tag>` below with your chosen tag and run the commands from the task repository root on
Linux x86-64. Staging the files is optional. This recipe creates and removes its own temporary
directory (`install_dir`) so a failed download or version check leaves the working pair in place.

```sh
(
    set -o errexit
    tag='<tag>'
    url="https://github.com/FrontierCS/Pumpkin/releases/download/$tag"
    install_dir=$(mktemp --directory "$PWD/.pumpkin-install.XXXXXXXX")
    trap 'rm --recursive --force "$install_dir"' EXIT
    cd "$install_dir"
    curl --fail --location --proto '=https' --output pumpkin "$url/pumpkin"
    curl --fail --location --proto '=https' --output .pumpkinversion "$url/pumpkinversion"
    chmod +x pumpkin
    ./pumpkin --version
    mv --target-directory .. pumpkin .pumpkinversion
)
```

Commit `pumpkin` and `.pumpkinversion` together, along with any workflow changes the release
requires.

## Workflow in Pumpkin

A task's entry file calls `workflow()` once, giving its name, work deadline, roles and the
final call that gives a run its result. `examples/hello` needs no model, so it runs
wherever Docker does. Its agent is a shell script: it has a job function count the lines and bytes
of its greeting, then submits the greeting, and the final call scores the greeting by its size.

[`examples/hello/task.star`](examples/hello/task.star), the entry file:

```python
"""A workflow that needs no model: the agent counts its greeting's lines and bytes by a call of its
job function `count`, then submits the greeting, and the final call scores its size and counts its
lines."""

load("//hello/count.star", "count")
load("//hello/final.star", "final_call")
load("//submit.star", "submit")

IMAGE = image("Dockerfile")

# Calls `count`, prints the job's status once it has ended, and submits the greeting.
AGENT = """
echo hello > greeting
sandboxctl call count greeting
while sandboxctl status 0 | grep -q -E '"state":"(queued|running)"'; do sleep 1; done
sandboxctl status 0
sandboxctl call final greeting --arg finalize=true
"""

workflow(
    deadline_seconds = 300,
    name = "examples/hello",
    roles = [
        {
            "name": "agent",
            "image": IMAGE,
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"agent": {
                "command": {
                    "argv": ["sh", "-c", "set -e" + AGENT],
                    "cwd": "/workspace",
                },
                "finalize_grace_seconds": 30,
                "grants": [
                    {"name": "count", "job": {
                        "target": "counter",
                        "body": count,
                        "payload_max_bytes": 1 * KiB,
                        "call_timeout_seconds": 60,
                    }},
                    {"name": "final", "now": {"body": submit, "payload_max_bytes": 1 * KiB}},
                ],
            }},
        },
        {
            "name": "counter",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"target": {"cwd": "/workspace"}},
        },
        {
            "name": "judge",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"target": {"cwd": "/pumpkin/home"}},
        },
    ],
    final = {"target": "judge", "body": final_call, "payload_max_bytes": 1 * KiB, "timeout_seconds": 60},
)
```

The bodies it loads, each a `def` with typed parameters, in
[`examples/hello/count.star`](examples/hello/count.star):

```python
"""The body of the agent's job function `count`."""

def count(target: Target, payload: Payload) -> dict[str, int]:
    """Counts the payload's lines and bytes with `wc`, in a fresh sandbox of the counter.

    Args:
      target: the counter's sandbox.
      payload: the greeting.

    Returns:
      The greeting's number of lines and of bytes.
    """
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"lines\": %s, \"bytes\": %s}' $(wc -l < greeting) $(wc -c < greeting) > counts.json"])
    return json.decode(target.get("/workspace/counts.json"))
```

[`examples/submit.star`](examples/submit.star), shared by the examples:

```python
"""The agent's submissions."""

def submit(run: Run, payload: Payload, finalize: bool = False) -> dict[str, str]:
    """Sets the calling agent's submission, replacing its previous payload.

    Args:
      run: the run's state.
      payload: the agent's submission.
      finalize: whether the call also locks the submission and ends the agent's work.

    Returns:
      `stored`, or `accepted` if the call locks the submission.
    """
    run.set_submission(payload)
    if finalize:
        run.end_work()
        return {"state": "accepted"}
    return {"state": "stored"}
```

and [`examples/hello/final.star`](examples/hello/final.star):

```python
"""The hello example's final call: it counts the greeting's bytes and lines in its target."""

def final_call(target: Target, submissions) -> dict[str, int]:
    """Shows the greeting's size as its score, and its lines.

    Args:
      target: the call's target.
      submissions: the greeting by agent role, or None if absent.

    Returns:
      The score and the lines, in the order the run shows them.
    """
    payload = submissions["agent"]
    if payload == None:
        return {"score": 0, "lines": 0}
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"score\": %s, \"lines\": %s}' $(wc -c < /workspace/greeting) $(wc -l < /workspace/greeting) > /pumpkin/out/result.json"])
    result = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": result["score"], "lines": result["lines"]}
```

**Roles.** A role is a template of sandboxes: an image, the user its commands run as, never root,
and what each of its sandboxes needs, from which the run picks the class of sandbox, among those the
providers offer, that each is placed in. Its `runtime` says its kind. The one `agent` works on the
task: its command runs from the run's start until it exits, its work ends, or `deadline_seconds`
pass. A `target` runs nothing of its own: each call of the function that names it gets a fresh
sandbox of it, which the function's body drives and which ends with the call. A `runner` is a
sandbox that a role holding a session leases and drives. What a role may reach and call is written
inside its kind: an agent holds `egress` and `grants`, a target `egress` and `sessions`, and a
runner neither.

**Grants.** A grant is a name and one function, under its kind's key: `{"name": "count", "job":
{…}}`. The agent calls it by name, `sandboxctl call count greeting`, and the file's bytes are the
call's payload, at most the grant's `payload_max_bytes`. The driver checks every call against the
grant before anything runs and refuses, with a reason, one that breaks it: a job function runs one
call at a time, and its `calls` and `cumulative_seconds`, left out here, bound how many it runs and
for how long.

**A job function's body**, here `count`, is a `def` of a file the entry loads, called once for each
call with a `Target` and the payload. The `Target` is the handle on a fresh sandbox of the grant's
`target`: `target.put` writes the payload as a file, `target.run` runs a command in the target's
`cwd` and waits for it, and `target.get` reads back a file of at most 1 MiB as text. The dict
the body returns is the job's result. The call answers at once with the job's number, and
`sandboxctl status 0` follows job 0 to its end. The agent never reaches the target: of a call it
learns only the job's state, what the grant has left, and the dict.

**A now function's body**, here `submit`, answers in the call's reply, with no sandbox. The agent
calls it as `final`, the name of its grant, which is not the final call. Its handle is a `Run`, the
run's state. `run.set_submission(payload)` replaces the payload the final call receives for that
role. `run.end_work()` locks the submission immediately and gives the agent
`finalize_grace_seconds` to exit. The run applies what the body asked once it has returned, all
of it or none. A parameter after the payload is the caller's. It is annotated `str`, `int` or
`bool`, given as `--arg finalize=true` and read as its type before the body runs.
`sandboxctl ops` lists each grant with its bounds and its caller's parameters.

**Sessions** are the third kind of function, for work that one call of a body can't hold, such as
training on a GPU. A session grant names a runner and bounds its leases: how many are open at once,
how long each and all of them last, how many commands they run, and how many bytes each copy moves.
The role opens a lease with `sandboxctl call <session>.open`, runs commands in its runner and
copies directory trees to and from it with `.run`, `.put` and `.get`, follows it with `.status`,
and ends it with `.close`.

**The final call** runs once the agent's work has ended and everything it started has ended too:
its body runs as a job's does, on a fresh sandbox of its `target`, which no job function may name,
with a dictionary mapping each agent role to its submitted payload or `None`. The dict it returns
is the run's result. The run shows its entries in their order, as `score=6 lines=1` in
`pumpkin ls`. A failed final call leaves the run without a result.

**Egress** is what a role reaches beyond its session. This agent has none; one that uses a model
has a model route, whose entries each name an API, the models it allows and, if it is bounded, a
budget: `"egress": {"model": [{"api": "chat", "models": ["model-a"], "budget": {"microusd": 5 *
USD}}]}`. The route counts each entry's spend at the prices of the machine's configuration, and
the model's key stays on the host.

**What `pumpkin check` proves** before anything is built: that every file evaluates, every
image's recipe pins what it fetches, every key and bound of the workflow is one the IR takes, every
role a grant or the final call names is of the kind it must be, and every body takes the handle and
the payload that its kind passes, with caller's parameters of a type `--arg` gives. It says each
problem at its file, line and column. A body's own statements run only when it is called.

[docs/workflows.md](docs/workflows.md) holds every key a workflow writes and what `sandboxctl`
answers; [docs/language.md](docs/language.md) holds the language.

## Run it

The operator runs the task repository's `./pumpkin` and holds the machine's configuration and
keys. A machine whose runs stay on its own Docker engine needs only this
`~/.config/pumpkin/pumpkin.toml`:

```toml
[docker]
context = "default"

[providers.local.docker]
```

With Pumpkin's `examples/` directory copied into the repository of tasks, `examples/hello`, which
needs no model, goes through the five commands:

```console
$ ./pumpkin check examples/hello/task.star   # evaluates and checks; silent if no problem
$ ./pumpkin run examples/hello/task.star     # builds the images, starts the run, prints its id
20261116-195228-234cd166
$ ./pumpkin wait 234c                        # follows the run, named by a prefix of its digits
pumpkin: run 20261116-195228-234cd166 finished with a result (score=6 lines=1), ...
$ ./pumpkin ls                               # lists the runs live or ended in the last day
20261116-195228-234cd166 finished         2s  -               -  examples/hello: score=6 lines=1
$ ./pumpkin view                             # serves the runs to this machine's browser
```

[docs/running.md](docs/running.md) goes through installing, configuring and these commands in order,
with the full output of each.

## Architecture

![A task's sources go to pumpkin, which evaluates them to the workflow IR and builds the images; the driver, one process for each run, reads the IR, calls the functions' bodies from the sources the evaluation read, and has a provider create a sandbox for each instance of a role; it writes the record, which pumpkin wait, pumpkin ls and the page read.](docs/figures/overview.svg)

### A workflow becomes data

`pumpkin` evaluates a task's entry file, and the files it loads, to the workflow IR, which is JSON,
and to the images its roles run, which it builds on the machine's Docker engine, each tagged by the
digest of its inputs. The runtime reads only the IR, so it depends on no language. A function's
body stays code: the IR names it by its file and its `def`, and the driver keeps the text of every
file the evaluation read, in the run's record, and evaluates the body from that text at each call,
so a call runs the body that was checked as the run started, whatever has changed on disk since.
Evaluation is hermetic and always ends: it reads files only below the task's root and Pumpkin's
library, and its arguments only from `--arg`.

### A run is one process

The driver runs a run from start to end, on the machine that started it or on a controller. In it,
the decision core takes each event that happened, such as a call that arrived or a sandbox that
ended, and returns what to do next; it does no I/O and reads no clock, so the same events give the
same actions. The runtime carries those actions out on the providers, the gateway holds one end of
each sandbox's session, and the model route answers the roles' model requests.

![The programs and machines of a run: the operator's pumpkin starts the driver on the operator's machine or on the controller; the driver's parts, the sandboxes that the providers create and the model provider that the route reaches.](docs/figures/run-machines.svg)

### A sandbox's only way out is its session

Every sandbox runs `sandboxd`, which meets the gateway and serves one session. Over it the driver
puts and collects files and starts commands, and code in the sandbox makes its calls, with
`sandboxctl`, and its model requests, at the endpoint `sandboxd` serves. Nothing else of the
network is reachable: a Docker sandbox has loopback alone, an EC2 sandbox's instance drops all but
the gateway's address and port, and a Modal sandbox opens no connection but DNS.
`ARCHITECTURE.md`'s security model says what each guard holds against.

### The record

The driver writes each run's record as the run goes, in a directory of its own: the IR and the
text of the files it came from, the run log of every event and what was done about it, each
sandbox's output, each model exchange, and how the run ended. `pumpkin wait`, `pumpkin ls` and the
page reads only the record, so each says the same about a run, wherever it ran.

Details: [workflows](docs/workflows.md) · [language](docs/language.md) ·
[running](docs/running.md) · [records](docs/record.md) ·
[providers](docs/providers.md) · [models](docs/models.md) · [architecture](ARCHITECTURE.md)

## Providers and the controller

A provider makes a run's sandboxes. Each role is placed in the smallest class of sandbox, of every
provider that offers what it needs, such as its CPUs, its memory or its GPU.

| Provider | A sandbox is | What the machine needs |
| --- | --- | --- |
| `docker` | a container on the machine's own Docker engine, on cores of its run's own | the engine |
| `ec2` | a container on an EC2 instance of its own | the envelope that `deploy/aws` makes |
| `modal` | a sandbox in a Modal workspace, under gVisor | a Modal profile, and the envelope's images |

The controller is the same `pumpkin`, on an EC2 instance of the operator's own, which
[`deploy/aws`](deploy/aws/README.md) makes. A machine whose configuration names `[controller]`
uploads its Pumpkin install and sends jobs over SSH to the controller's private address.
The controller builds task images and runs the drivers in systemd units. The operator can close
the connection and use the same commands to follow, list, stop, reap or view those runs later. It can also publish its runs to a read-only page for a team.
[docs/providers.md](docs/providers.md) says how the pieces fit.

## Documentation

- Writing workflows: [workflows](docs/workflows.md), [the language](docs/language.md).
- Running workflows: [running](docs/running.md), [records and the page](docs/record.md).
- Deploying: [providers and the controller](docs/providers.md),
  [`deploy/aws`](deploy/aws/README.md), `pumpkin help config`.
- Models: [routes, budgets and prices](docs/models.md).
- Contributing: [`ARCHITECTURE.md`](ARCHITECTURE.md), the map of the code, and
  [`docs/dev/`](docs/dev/), the rules for changing it.
- On Linux x86-64, run `cargo xtask release preview` to build and check the release archive for
  the current checkout. The archive, launcher and pin are in `target/release-assets/preview/`.
  Before tagging a commit, check it out on a branch pushed to GitHub and run
  `cargo xtask live examples --status --model <model>`, which runs every shipped example through
  the controller and posts the commit's `pumpkin/examples` status.
  Pushing a new tag builds its assets and runs `cargo xtask release <tag> --publish` to attach
  them to a GitHub draft and publish it as the latest release, which it refuses unless that status
  is a success. If the workflow leaves a draft, verify its assets against the local files for the
  tag, attach any missing file, and publish the draft as the latest release.
