# Testing

The rules for writing Pumpkin's tests and their snapshots.

- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- EARN each test: it pins what a user, a task or a later change relies on, and no other test pins
- PREFER testing a decision as a function of values to running it through a stand-in
- START a snapshot as `expect![[""]]`, fill it with
  `cargo xtask refill --package <crate> [<test>]`, and read the diff before keeping it
- ASSERT in a file's `check` helper what holds of every case, such as files left or modes; show
  in the snapshot what the test is about, and what departs from that
- MAKE the state that a reader of Pumpkin's own format reads with Pumpkin's writer, or build a
  small one in the test; CHECK IN only data a third party wrote, such as a vendored suite
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- RUN `cargo xtask install-tools` before running tests by any command, a plain `cargo test` or
  `cargo nextest run` too: pumpkin's tests run the pinned formatter that it links at
  `bin/buildifier`, which `.cargo/config.toml` names
- ASSUME that each test runs alone, in a process of its own, as cargo-nextest runs it in
  `cargo xtask ci`: tests share no per-process state
- SPLIT cases that each start a process into tests of their own, which nextest runs at once
- WRITE a test's scratch into its `test_support::TestDir`: no other test shares it, its paths fit
  a socket's, which holds 107 bytes at most, and a test keeps only its last failure's; `stdx`'s
  tests, whose manifest names no Pumpkin crate, write theirs into a `tempfile::TempDir`
- RUN `cargo xtask mutants crates/decision crates/schema crates/route` on demand, when tests of
  the decision core, the schema's refusals or the route's request rules, which guard the
  operator's key, are deleted or rewritten, with `CARGO_MUTANTS_JOBS=2` at most, and read each
  mutant it reports missed, as ci doesn't run it; where the change leaves the source below those
  paths as it is, it mutates that source whole, and names the mutants missed that
  `origin/main`'s tests catch, which the change left uncaught
- KEEP `cargo xtask ci` pure: its tests reach no Docker engine, network or cloud; temporary
  files, Pumpkin's own processes and loopback sockets are fine. IGNORE each test that needs a Docker engine, the static `sandboxd` or
  a server elsewhere (`#[ignore = "…: run cargo xtask live docker"]`)
- SET each bound that a test chooses at its floor, and script the clock (`SimHost`) for a case
  past a bound; wait on the real clock only for a bound that production states in whole seconds
- NEVER test against an invented model of a cloud or a service, which pins what the model answers,
  and NEVER add a stand-in that answers for another program either, though one may record that it
  ran: what a program really answers is tested by `cargo xtask live`, or not at all; test a provider
  by `cargo xtask live ec2` and `cargo xtask live modal` instead; RUN them at review of a change to
  a provider, `crates/aws`, `crates/tls`, the gateway's meeting, `sandboxd`'s listen, dial or user
  drop, `ec2-boot`, the EC2 user data or the Modal helper, and give the run's id in the pull request
- RUN `cargo xtask live examples --status`, which costs money, on the commit of a release before
  its tag is pushed: it runs each shipped example through the controller with real models, as
  their READMEs say, and posts the outcome as the commit's `pumpkin/examples` status, without whose
  success `cargo xtask release <tag> --publish` refuses the tag
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness; SET
  `doctest = false` too on each library whose docs hold no Rust example, which rustdoc would build
  an empty doctest run for
- `cargo xtask ci` checks the rest: each test ends within 5 s, but those that
  `.config/nextest.toml` names with why, and no file but Rust sits under a `tests/` directory
  outside `crates/lang`'s vendored suites and `.star` fixtures
