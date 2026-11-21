//! The contributor tasks of the workspace, in the shape of
//! [matklad's `cargo xtask`](https://github.com/matklad/cargo-xtask/), run through the alias in
//! `.cargo/config.toml`.
//!
//! A check that every change must pass is a step of `ci`, as `tidy`'s are; the checks that reach
//! a real environment are `live`'s.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command line reports on its standard output and error"
)]

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use xshell::{Cmd, Shell, cmd};

mod diff;
mod figure;
mod gate;
mod live;
mod op_stats;
mod page;
mod release;
mod settings;
mod star;
mod tidy;
mod tools;
mod wait;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The target of the static binaries in an install.
const MUSL: &str = "x86_64-unknown-linux-musl";

/// The toolchain whose `rustfmt` formats the workspace: `rustfmt.toml` sets nightly-only options.
const RUSTFMT_TOOLCHAIN: &str = "nightly-2026-11-07";

mod flags {
    #![expect(
        unreachable_pub,
        reason = "xflags declares the types it generates `pub`"
    )]
    #![expect(
        clippy::allow_attributes,
        clippy::allow_attributes_without_reason,
        reason = "xflags generates `#[allow(dead_code)]`, without a reason, on its parsers"
    )]

    use std::path::PathBuf;

    xflags::xflags! {
        src "./src/main.rs"

        /// Contributor tasks.
        cmd xtask {
            /// Formats the workspace with the pinned nightly rustfmt.
            cmd fmt {
                /// Checks formatting without changing files.
                optional --check
            }

            /// Runs the checks every change must pass, in order, and stops at the first that
            /// fails. They reach no Docker engine, network or cloud.
            cmd ci {}

            /// Writes the figures drawn from the workspace, which `ci` checks are up to date:
            /// `docs/figures/crates.svg`, of how its crates depend on one another, from
            /// `cargo metadata`.
            cmd figures {}

            /// Runs the checks that reach a real environment, with Pumpkin at `HEAD`: `docker`, the
            /// tests that need a Docker engine, the static sandboxd or a server elsewhere, through
            /// the Docker context that `PUMPKIN_DOCKER_CONTEXT` names, or `default`; and, at a
            /// cost, through the configuration's controller, from a task repository below the
            /// target directory: `ec2` and `modal`, a run of `examples/smoke` on that provider,
            /// whose probes must pass, with nothing of it left on EC2, EventBridge Scheduler or
            /// Modal, which `pumpkin cancel` then removes; and `examples`, about $10 over two
            /// hours, a run of each shipped example on real models, whose checker must pass. Each
            /// run must end with its sandboxes deleted.
            cmd live {
                /// `docker`, `ec2`, `modal` or `examples`.
                required target: String
                /// With `modal`, the model of the GPU, of a class of `[controller.modal]`, that
                /// the smoke's GPU runner needs.
                optional --gpu-model model: String
                /// With `ec2` or `modal`, loses the run once the agent holds a lease of each
                /// grant, by killing its job on the controller, and checks that each sandbox ends
                /// by itself by the run's hard end, about 95 minutes after its start, and that
                /// `pumpkin reap` then removes what is left: at most about $1 on EC2, $4 on Modal.
                optional --lost
                /// With `examples`, the machine's configuration that the runs go by, whose
                /// `[controller]` they go to and whose upstreams serve `--model` and the native
                /// benchmark's `glm-5.3-flash`. The default, and the one of `ec2` and `modal`, is
                /// `~/.config/pumpkin/pumpkin.toml`.
                optional --config file: PathBuf
                /// With `examples`, the model of the examples that take one as `--arg model`.
                optional --model name: String
                /// With `examples`, posts how they ended on GitHub as the commit status
                /// `pumpkin/examples` of `HEAD`, which `release --publish` requires to be a
                /// success.
                optional --status
            }

            /// Counts incoming operations by request kind in a run's `log.jsonl`. Reports the
            /// bytes of matching lines, including their newlines, and the full log size.
            cmd op-stats {
                /// The run's record directory.
                required run_dir: PathBuf
                /// Only count requests with this caller name in the record, such as `job-17`.
                optional --caller name: String
            }

            /// Builds the static sandboxd that every sandbox runs, and beside it the static
            /// ec2-boot that the EC2 provider's image of it holds too, and prints sandboxd's path.
            cmd sandboxd {}

            /// Builds `pumpkin`, the static sandboxd and ec2-boot, and puts them in `<root>/bin`
            /// with the pinned buildifier, the Modal helper and its lock. Prints each file's path.
            /// The pinned tools require Linux on x86-64.
            cmd build {
                /// The directory whose `bin` they go in: this checkout's `.pumpkin` by default.
                optional --root dir: PathBuf
                /// The target for `pumpkin`, as in cargo's `--target`. The default is
                /// `x86_64-unknown-linux-musl`, built static with Zig as its C compiler.
                optional --target triple: String
            }

            /// Builds and checks the Linux x86-64 archive, launcher and pin from this checkout
            /// in `target/release-assets/<tag>/`.
            cmd release {
                /// The tag to put in the archive's name.
                required tag: String
                /// Publishes the existing files as the latest GitHub release for the tag.
                /// The default is to build them locally.
                optional --publish
            }

            /// Runs cargo-mutants on the working tree's changes: each mutant it reports missed,
            /// listed in `mutants.out`, is a finding to read, not a failure. Given paths whose
            /// source the changes leave as it is, it mutates that source whole, and names the
            /// mutants missed that the tests of the merge base catch.
            cmd mutants {
                /// The paths whose changes alone are taken, if any are given.
                repeated paths: PathBuf
                /// The revision since whose merge base with `HEAD` the changes are taken,
                /// `origin/main` by default.
                optional --in-diff rev: String
            }

            /// Runs a package's tests with `UPDATE_EXPECT=1`, which rewrites each snapshot that
            /// differs in this checkout, then prints `git diff --stat` of what changed, to read
            /// before keeping it.
            cmd refill {
                /// The package whose tests run.
                required -p, --package package: String
                /// Only the tests whose names hold it.
                optional filter: String
            }

            /// Installs each contributor tool that xtask runs, such as cargo-nextest, at its
            /// pinned version, fetches each that its project releases as a file, such as
            /// TypeScript's compiler, into the target directory, and links the pinned buildifier
            /// where the tests find it, `bin/buildifier`.
            cmd install-tools {}
        }
    }
    // generated start
    // The following code is generated by `xflags` macro.
    // Run `env UPDATE_XFLAGS=1 cargo build` to regenerate.
    #[derive(Debug)]
    pub struct Xtask {
        pub subcommand: XtaskCmd,
    }

    #[derive(Debug)]
    pub enum XtaskCmd {
        Fmt(Fmt),
        Ci(Ci),
        Figures(Figures),
        Live(Live),
        OpStats(OpStats),
        Sandboxd(Sandboxd),
        Build(Build),
        Release(Release),
        Mutants(Mutants),
        Refill(Refill),
        InstallTools(InstallTools),
    }

    #[derive(Debug)]
    pub struct Fmt {
        pub check: bool,
    }

    #[derive(Debug)]
    pub struct Ci;

    #[derive(Debug)]
    pub struct Figures;

    #[derive(Debug)]
    pub struct Live {
        pub target: String,

        pub gpu_model: Option<String>,
        pub lost: bool,
        pub config: Option<PathBuf>,
        pub model: Option<String>,
        pub status: bool,
    }

    #[derive(Debug)]
    pub struct OpStats {
        pub run_dir: PathBuf,

        pub caller: Option<String>,
    }

    #[derive(Debug)]
    pub struct Sandboxd;

    #[derive(Debug)]
    pub struct Build {
        pub root: Option<PathBuf>,
        pub target: Option<String>,
    }

    #[derive(Debug)]
    pub struct Release {
        pub tag: String,

        pub publish: bool,
    }

    #[derive(Debug)]
    pub struct Mutants {
        pub paths: Vec<PathBuf>,

        pub in_diff: Option<String>,
    }

    #[derive(Debug)]
    pub struct Refill {
        pub filter: Option<String>,

        pub package: String,
    }

    #[derive(Debug)]
    pub struct InstallTools;

    impl Xtask {
        #[allow(dead_code)]
        pub fn from_env_or_exit() -> Self {
            Self::from_env_or_exit_()
        }

        #[allow(dead_code)]
        pub fn from_env() -> xflags::Result<Self> {
            Self::from_env_()
        }

        #[allow(dead_code)]
        pub fn from_vec(args: Vec<std::ffi::OsString>) -> xflags::Result<Self> {
            Self::from_vec_(args)
        }
    }
    // generated end
}

fn main() -> ExitCode {
    if let Some(failed) = tools::run_as_musl_cc() {
        return failed;
    }
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// xflags exits by itself on `--help`, with 0, and on a command line it can't parse, with 2.
fn try_main() -> Result<()> {
    let flags = flags::Xtask::from_env_or_exit();
    let cargo = Cargo::new()?;
    match flags.subcommand {
        flags::XtaskCmd::Fmt(flags::Fmt { check }) => {
            let fmt = cargo.fmt();
            let fmt = if check { fmt.arg("--check") } else { fmt };
            fmt.run()?;
            Ok(())
        }
        flags::XtaskCmd::Ci(flags::Ci) => ci(&cargo),
        flags::XtaskCmd::Figures(flags::Figures) => figure::write(&cargo.sh, &metadata(&cargo)?),
        flags::XtaskCmd::Live(flags) => live(&cargo, flags),
        flags::XtaskCmd::OpStats(flags::OpStats { run_dir, caller }) => {
            op_stats::report(&run_dir, caller.as_deref())
        }
        flags::XtaskCmd::Sandboxd(flags::Sandboxd) => {
            println!("{}", sandboxd(&cargo)?.display());
            Ok(())
        }
        flags::XtaskCmd::Build(flags::Build { root, target }) => {
            build(&cargo, root, target.as_deref().unwrap_or(MUSL))
        }
        flags::XtaskCmd::Release(flags::Release { tag, publish }) => {
            release::run(&cargo, &tag, publish)
        }
        flags::XtaskCmd::Mutants(flags::Mutants { paths, in_diff }) => {
            mutants(&cargo, &paths, in_diff.as_deref())
        }
        flags::XtaskCmd::Refill(flags::Refill { package, filter }) => {
            refill(&cargo, &package, filter.as_deref())
        }
        flags::XtaskCmd::InstallTools(flags::InstallTools) => {
            tools::install(&cargo)?;
            star::link(&cargo).map(drop)
        }
    }
}

/// Runs `cargo xtask live` as `flags` ask.
fn live(cargo: &Cargo, flags: flags::Live) -> Result<()> {
    let flags::Live {
        target,
        gpu_model,
        lost,
        config,
        model,
        status,
    } = flags;
    let examples_only = [
        ("--config", config.is_some()),
        ("--model", model.is_some()),
        ("--status", status),
    ];
    let given = examples_only
        .iter()
        .find_map(|(flag, given)| given.then_some(*flag));
    let provider = match target.as_str() {
        "examples" if gpu_model.is_some() || lost => {
            return Err(
                "`--gpu-model` and `--lost` are for `ec2` and `modal`: leave them out".into(),
            );
        }
        "examples" => {
            let Some(model) = model else {
                let error = "`cargo xtask live examples` runs examples on a model: name it with \
                             `--model`";
                return Err(error.into());
            };
            return live::examples(cargo, config.as_deref(), &model, status);
        }
        _ if let Some(flag) = given => {
            return Err(
                format!("`{flag}` is for `cargo xtask live examples` alone: leave it out").into(),
            );
        }
        "docker" if lost => {
            return Err("`--lost` is for `ec2` and `modal`: leave it out".into());
        }
        "docker" => return live::docker(cargo),
        "ec2" => settings::Provider::Ec2,
        "modal" => settings::Provider::Modal,
        _ => {
            return Err(format!(
                "`{target}` is no live target: give `docker`, `ec2`, `modal` or `examples`"
            )
            .into());
        }
    };
    if gpu_model.is_some() && provider != settings::Provider::Modal {
        return Err("`--gpu-model` is for `cargo xtask live modal` alone: leave it out".into());
    }
    live::smoke(cargo, provider, gpu_model.as_deref(), lost)
}

/// Builds the cargo commands xtask runs, at the workspace's root, without the variables that
/// `cargo run` sets for xtask's own package ([`stdx::process::cargo_package_vars`]). The others
/// stay, such as `CARGO` and rustup's, which name the toolchain a child cargo is to use. xshell's
/// `Shell` can set a variable for all its commands but not remove one, so each command removes
/// them.
struct Cargo {
    sh: Shell,
    package_vars: Vec<OsString>,
}

impl Cargo {
    fn new() -> Result<Self> {
        let sh = Shell::new()?;
        sh.change_dir(root());
        let package_vars = stdx::process::cargo_package_vars();
        Ok(Self { sh, package_vars })
    }

    fn cmd(&self, args: &[&str]) -> Cmd<'_> {
        let cmd = self.sh.cmd("cargo").args(args);
        self.package_vars.iter().fold(cmd, Cmd::env_remove)
    }

    /// The pinned formatter, without implicit installation or an inherited binary override.
    fn fmt(&self) -> Cmd<'_> {
        let toolchain = format!("+{RUSTFMT_TOOLCHAIN}");
        self.cmd(&[&toolchain, "fmt", "--all"])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env_remove("RUSTFMT")
    }
}

fn root() -> &'static Path {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = xtask.parent() else {
        unreachable!(
            "xtask's directory {} has no parent, though it is the workspace's `xtask/`, below \
             its root, and only a root or an empty path has none",
            xtask.display()
        )
    };
    root
}

/// Runs the checks every change must pass, in order, and stops at the first that fails. The
/// tests leave out those ignored, which [`live::docker`] runs.
fn ci(cargo: &Cargo) -> Result<()> {
    tools::require(cargo, tools::NEXTEST)?;
    cargo.fmt().arg("--check").run()?;
    tidy::check(&cargo.sh)?;
    figure::check(&cargo.sh, &metadata(cargo)?)?;
    let clippy = cargo.cmd(&["clippy", "--workspace", "--all-targets", "--locked"]);
    clippy.args(["--", "--deny", "warnings"]).run()?;
    let doc = cargo.cmd(&["doc", "--no-deps", "--workspace", "--locked"]);
    doc.env("RUSTDOCFLAGS", "--deny warnings").run()?;
    page::check(cargo)?;
    let buildifier = star::link(cargo)?;
    let pumpkin = tests_pumpkin(cargo)?;
    star::place(&buildifier, &pumpkin)?;
    star::check(&cargo.sh, &pumpkin)?;
    let test = cargo.cmd(&["nextest", "run", "--workspace", "--locked"]);
    let doctests = cargo.cmd(&["test", "--doc", "--workspace", "--locked"]);
    // ci checks snapshots, never rewrites them.
    tests(
        test.env_remove("UPDATE_EXPECT"),
        doctests.env_remove("UPDATE_EXPECT"),
        "cargo xtask ci",
    )
}

/// What `cargo metadata` says of the workspace's own packages, as JSON.
fn metadata(cargo: &Cargo) -> Result<String> {
    let metadata = cargo.cmd(&["metadata", "--format-version", "1", "--no-deps", "--locked"]);
    Ok(metadata.read()?)
}

/// Runs `test` and `doctests` at once, and prints what `doctests` printed, if it failed, once
/// `test` has ended. Fails if either fails, saying to run `again`.
fn tests(test: Cmd<'_>, doctests: Cmd<'_>, again: &str) -> Result<()> {
    let (test_line, doctests_line) = (test.to_string(), doctests.to_string());
    let doctests = Command::from(doctests)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let doctests =
        doctests.map_err(|error| format!("starting `{doctests_line}` failed: {error}"))?;
    let tested = Command::from(test).status();
    let tested = tested.map_err(|error| format!("running `{test_line}` failed: {error}"))?;
    let doctested = doctests.wait_with_output();
    let doctested =
        doctested.map_err(|error| format!("waiting for `{doctests_line}` failed: {error}"))?;
    if doctested.status.success() {
        println!("xtask: `{doctests_line}` passed");
    } else {
        eprint!("{}", String::from_utf8_lossy(&doctested.stderr));
        print!("{}", String::from_utf8_lossy(&doctested.stdout));
    }
    let ended = [(test_line, tested), (doctests_line, doctested.status)];
    let failed: Vec<_> = (ended.iter().filter(|(_, status)| !status.success()))
        .map(|(line, status)| format!("`{line}` ended with {status}"))
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    let failed = failed.join(", and ");
    Err(format!("{failed}: fix what it printed above, and run `{again}` again").into())
}

/// Runs the tests of `package`, those whose names hold `filter` if it is given, with
/// `UPDATE_EXPECT=1`, which rewrites each snapshot that differs; then prints `git diff --stat`,
/// also if a test failed.
fn refill(cargo: &Cargo, package: &str, filter: Option<&str>) -> Result<()> {
    tools::require(cargo, tools::NEXTEST)?;
    star::link(cargo)?;
    let test = cargo.cmd(&["nextest", "run", "--locked", "--package", package]);
    let test = test.args(filter).env("UPDATE_EXPECT", "1");
    let ran = test.run();
    cargo.sh.cmd("git").args(["diff", "--stat"]).run()?;
    Ok(ran?)
}

/// Runs cargo-mutants on the working tree's changes since `rev`, `origin/main` by default, below
/// `paths` alone if there are any. If the changes leave the source below `paths` as it is, it
/// mutates that source whole, then runs the mutants missed again on the merge base, to name those
/// that the changes left uncaught. Missed mutants and timeouts are findings, not failures.
fn mutants(cargo: &Cargo, paths: &[PathBuf], rev: Option<&str>) -> Result<()> {
    tools::require(cargo, tools::MUTANTS)?;
    tools::require(cargo, tools::NEXTEST)?;
    let buildifier = star::link(cargo)?;
    let mut again = "cargo xtask mutants".to_owned();
    for path in paths {
        again = format!("{again} {}", path.display());
    }
    if let Some(rev) = rev {
        again = format!("{again} --in-diff {rev}");
    }
    let sh = &cargo.sh;
    let rev = rev.unwrap_or("origin/main");
    let base = cmd!(sh, "git merge-base {rev} HEAD").read()?;
    let diff = diff::since(sh, &base, paths, &again)?;
    let scratch = sh.create_temp_dir()?;
    let diff_path = scratch.path().join("changes.diff");
    sh.write_file(&diff_path, diff)?;
    // A glob with a slash matches the whole path in cargo-mutants, so a directory needs its `**`.
    let files = paths
        .iter()
        .flat_map(|path| [path.clone(), path.join("**")]);
    let files: Vec<_> = files
        .flat_map(|glob| ["--file".into(), glob.into_os_string()])
        .collect();
    let whole = !paths.is_empty() && unchanged(cargo, &base, &files)?;
    let run = if whole {
        cargo.cmd(&["mutants"]).args(&files)
    } else {
        cargo.cmd(&["mutants", "--in-diff"]).arg(&diff_path)
    };
    let report = root().join("mutants.out");
    if !missed(run, &again)? {
        return Ok(());
    }
    if !whole {
        println!(
            "xtask: the mutants cargo-mutants missed or timed out on are findings, listed in \
             `{}` with their diffs",
            report.display()
        );
        return Ok(());
    }
    // The source is as it is on `base`, so each mutant there has the name it has here, and
    // `--iterate` tests there only those that weren't caught or unviable here.
    let archive = scratch.path().join("base.tar");
    cmd!(sh, "git archive --output {archive} {base}").run()?;
    let tree = sh.create_dir(scratch.path().join("base"))?;
    cmd!(sh, "tar --extract --file {archive} --directory {tree}").run()?;
    // Its tests find the formatter by the link in their own tree, which the archive doesn't hold.
    let bin = sh.create_dir(tree.join("bin"))?;
    std::os::unix::fs::symlink(&buildifier, bin.join("buildifier"))?;
    let run = cargo.cmd(&["mutants", "--iterate", "--dir"]).arg(&tree);
    missed(run.arg("--output").arg(root()).args(&files), &again)?;
    let brought = sh.read_file(report.join("caught.txt"))?;
    let brought = if brought.is_empty() {
        "none\n"
    } else {
        &brought
    };
    println!(
        "xtask: the mutants cargo-mutants missed or timed out on are listed in `{}` with their \
         diffs; the changes left these uncaught, which the tests on {base} catch:\n{brought}",
        root().join("mutants.out.old").display()
    );
    Ok(())
}

/// Whether the source files that `files`, cargo-mutants' `--file` arguments, select are the same
/// in the working tree as on `base`, and there are any.
fn unchanged(cargo: &Cargo, base: &str, files: &[OsString]) -> Result<bool> {
    let listed = cargo.cmd(&["mutants", "--list-files"]).args(files).read()?;
    let sources: Vec<&str> = listed.lines().collect();
    if sources.is_empty() {
        return Ok(false);
    }
    let sh = &cargo.sh;
    let diff = cmd!(sh, "git diff --quiet {base} -- {sources...}")
        .quiet()
        .ignore_status();
    Ok(diff.output()?.status.success())
}

/// Runs `run`, a cargo-mutants command, and returns whether it missed a mutant or one timed out.
fn missed(run: Cmd<'_>, again: &str) -> Result<bool> {
    // Relative, so that each copy of the tree cargo-mutants builds has its own: in a shared one,
    // cargo would take a mutant's artifacts as fresh for the unmutated tree.
    let run = run.env("CARGO_TARGET_DIR", "target");
    let line = run.to_string();
    let status = Command::from(run).status();
    let status = status.map_err(|error| format!("running `{line}` failed: {error}"))?;
    match status.code() {
        Some(0) => Ok(false),
        // cargo-mutants exits with 2 when it missed a mutant and with 3 when one timed out.
        Some(2 | 3) => Ok(true),
        _ => Err(format!(
            "`{line}` ended with {status}: fix what it printed above, and run `{again}` again"
        )
        .into()),
    }
}

/// Builds the workspace's tests, as `cargo nextest run` then finds them, and returns the
/// `pumpkin` that they run.
fn tests_pumpkin(cargo: &Cargo) -> Result<PathBuf> {
    let build = cargo.cmd(&["test", "--workspace", "--locked", "--no-run"]);
    let artifacts = executables(build)?;
    let mut named = artifacts.iter().filter(|built| {
        built["target"]["name"] == "pumpkin" && built["target"]["kind"][0] == "bin"
    });
    match named.find_map(|built| built["executable"].as_str()) {
        Some(pumpkin) => Ok(pumpkin.into()),
        None => Err("cargo reported no pumpkin executable for the tests: expected one".into()),
    }
}

/// Builds the static `sandboxd` that every sandbox runs, and the static `ec2-boot`, which cargo
/// puts beside it, where the EC2 provider finds it, and returns `sandboxd`'s path.
fn sandboxd(cargo: &Cargo) -> Result<PathBuf> {
    release_binary(cargo, "ec2-boot", MUSL)?;
    release_binary(cargo, "sandboxd", MUSL)
}

/// Builds the release binary of `package`, which has its name, for `target`,
/// and returns the path cargo reports for it, so that the path is right wherever cargo's
/// configuration puts the target directory.
fn release_binary(cargo: &Cargo, package: &str, target: &str) -> Result<PathBuf> {
    let build = cargo.cmd(&["build", "--release", "--locked", "--package", package]);
    let build = build.args(["--target", target]);
    let build = if package == "pumpkin" && target == MUSL {
        tools::musl_cc(cargo, build)?
    } else {
        build
    };
    let build = build.args(["--message-format", "json-render-diagnostics"]);
    // One JSON message per line; diagnostics are rendered on standard error instead.
    for line in build.read()?.lines() {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("cargo printed `{line}`, which isn't JSON: {error}"))?;
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == package
            && let Some(path) = message["executable"].as_str()
        {
            return Ok(path.into());
        }
    }
    Err(format!(
        "cargo reported no {package} executable: expected a compiler-artifact message for it"
    )
    .into())
}

/// Runs `build`, a cargo command that builds, and returns cargo's compiler-artifact message for
/// each executable it built or found fresh, so that each path is right wherever cargo's
/// configuration puts the target directory.
fn executables(build: Cmd<'_>) -> Result<Vec<serde_json::Value>> {
    let build = build.args(["--message-format", "json-render-diagnostics"]);
    let mut executables = Vec::new();
    for line in build.read()?.lines() {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("cargo printed `{line}`, which isn't JSON: {error}"))?;
        if message["reason"] == "compiler-artifact" && message["executable"].is_string() {
            executables.push(message);
        }
    }
    Ok(executables)
}

/// Builds `pumpkin` for `target` and the static `sandboxd` and `ec2-boot`, and installs them
/// with the Modal provider's helper and its lock, and the pinned
/// buildifier ([`star::buildifier`]), in `<root>/bin`, made if need be, replacing what is there:
/// `root` made absolute, or this checkout's `.pumpkin`. Prints each one's path, and, without a
/// `root`, says if `<root>/bin` is not on `PATH`.
fn build(cargo: &Cargo, root: Option<PathBuf>, target: &str) -> Result<()> {
    let default_root = root.is_none();
    let root = root.unwrap_or_else(|| self::root().join(".pumpkin"));
    let bin = root.join("bin");
    let bin = std::path::absolute(&bin)
        .map_err(|error| format!("making {} absolute failed: {error}", bin.display()))?;
    // The Modal provider's helper and its lock, which `pumpkin` finds beside it, under the
    // names it looks for.
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/modal/helper");
    let mut built = vec![
        ("pumpkin", release_binary(cargo, "pumpkin", target)?, 0o755),
        ("buildifier", star::buildifier(cargo)?, 0o755),
    ];
    let sandboxd = sandboxd(cargo)?;
    built.extend([
        ("ec2-boot", sandboxd.with_file_name("ec2-boot"), 0o755),
        ("sandboxd", sandboxd, 0o755),
        ("pumpkin-modal.py", helper.join("pumpkin-modal.py"), 0o644),
        (
            "pumpkin-modal.py.lock",
            helper.join("pumpkin-modal.py.lock"),
            0o644,
        ),
    ]);
    fs::create_dir_all(&bin)
        .map_err(|error| format!("making {} failed: {error}", bin.display()))?;
    for (name, from, mode) in built {
        let to = install_file(&bin, name, &from, mode)?;
        println!("{}", to.display());
    }
    let path = env::var_os("PATH").unwrap_or_default();
    if default_root && !env::split_paths(&path).any(|dir| dir == bin) {
        eprintln!(
            "xtask: {} is not on PATH: add it there to run `pumpkin` by its name",
            bin.display()
        );
    }
    Ok(())
}

fn install_file(bin: &Path, name: &str, from: &Path, mode: u32) -> Result<PathBuf> {
    let to = bin.join(name);
    // Linux refuses a write to a running executable. Rename the finished copy into place.
    let copy = bin.join(format!(".{name}.xtask-build.{}", std::process::id()));
    let installed = fs::copy(from, &copy)
        .and_then(|_| fs::set_permissions(&copy, Permissions::from_mode(mode)))
        .and_then(|()| fs::rename(&copy, &to));
    if let Err(error) = installed {
        let _ = fs::remove_file(&copy);
        return Err(format!("installing {} failed: {error}", to.display()).into());
    }
    Ok(to)
}
