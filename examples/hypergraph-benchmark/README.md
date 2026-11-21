# Native hypergraph benchmark

Five Codex agents implement exact hypergraph core numbers in a native C++ starter repository.
Alice, Bob, Carol, Dan and Eve each have eight vCPUs, 32 GiB, a private Git checkout and a
$5 model budget. They share a 90-minute deadline and a message board. Each can send private
messages, train on two feedback graphs, select a patch archive and finish independently.
The [small team example](../team/README.md) has a scripted workload suitable for a quick run.

## Run the team

Use a separate Git task repository containing a copy of the entire `examples/` directory, with
the Pumpkin launcher and version pin installed as in the [examples guide](../README.md).
Configure the controller and a priced Responses API route for `glm-5.3-flash` with native
tools. The five model budgets permit up to $25 in total model usage. Compute is billed separately.

The workload binding, [`binding.json`](binding.json), names the public source repository and
the commit of it that the benchmark adapts. Fetch that commit, without its file contents, into a
separate Git repository outside the task repository. Preparation reads the commit's Git objects,
which Git fetches as it reads them, and writes role-specific build contexts to
`examples/benchmark-input/`. The controller builds the shared runtime and task Dockerfiles, runs
the upstream manifest generator, and verifies its five output files. Only the verifier build
fetches the four checksum-pinned graph files. The controller must be able to list the prepared
files with `git ls-files --cached --others --exclude-standard` in the task repository.

```sh
git clone --filter=blob:none --no-checkout --depth=1 https://github.com/FrontierCS/FrontierCS-2.git "${SOURCE:?set SOURCE to a new directory outside the task repository}" &&
git -C "$SOURCE" fetch --filter=blob:none --depth=1 origin 1867a902b24a3a89e8c1ede73f76c28d1f102c3c &&
python3 -B examples/hypergraph-benchmark/prepare.py "$SOURCE" &&
./pumpkin check examples/hypergraph-benchmark/task.star &&
./pumpkin run examples/hypergraph-benchmark/task.star --wait --outcome
```

Each agent's prompt directs it to the native task statement for the compiler flags, editable
C++ source and required output vector. Its image contains the starter and native packager.
The training worker checks submissions against the two feedback workloads. Only the final
and replay verifiers have the two additional graded graphs. The board sees messages and grants
authenticate each caller. Each agent uses `make-team-archive` to snapshot its patch, then sends
the archive to the training or selection grant. Each training job transfers the archive into a
scoped runner and retrieves its result with `runner.get`.

## Check the final result

The final judge compiles each present selection and evaluates roles serially. It checks the
complete core vector and times seven executions on each of four graphs at eight threads. The
score is `100 * exp(-t / 0.25)`, where `t` is the median of the four per-graph median candidate
times in seconds. The verifier also measures at one thread. A candidate can earn a timing score
despite failing a one-thread workload. Such a candidate cannot earn the separate reference win,
which requires being at least 3% faster on every workload at both thread counts.

The team receives the highest individual score and all tied winners. A missing selection has
no score. A verifier error leaves the team result incomplete. The run record preserves the
selection digests, the official JSON for each evaluated role, and the native evidence. With the
copied run record and the `zstd` command available, check the submitted bytes, timer samples,
aggregate, board messages and sandbox deletion receipts.

```sh
python3 examples/hypergraph-benchmark/verify.py "${RECORD:?set RECORD to the copied run directory}"
```

## Replay the selected archives

The replay reads the five selected submissions from the original record and checks their
digests and lengths before preparing an image context in `examples/benchmark-replay/`.
A fresh verifier compiles those selections and repeats the measurements. Its verdict reports
the original and fresh scores, winners and whether it reproduced each outcome. Fresh timings
can change the ranking. Replay needs compute and the prepared native input, and makes no
model calls. Use a fresh task repository when switching to another record.

```sh
python3 examples/hypergraph-benchmark/replay/prepare.py "${RECORD:?set RECORD to the copied run directory}" &&
./pumpkin check examples/hypergraph-benchmark/replay/task.star &&
./pumpkin run examples/hypergraph-benchmark/replay/task.star --wait --outcome
```
