//! `pumpkin`'s command line: [`parse`] reads it with clap into a [`Command`], each command a
//! struct of its operands and flags.
//!
//! The help texts here are the home of what each command and option means. `-h` or `--help`
//! before `--` prints the help of the command that the line names, as `pumpkin help` does, which
//! also prints the topics, such as [`CONFIG`]. The commands that [`RARE`] names are hidden from
//! clap's list and listed apart; the controller's own side is listed nowhere.
//! What a command does is left to its own module.

mod notes;

use std::ffi::OsString;
use std::iter;
use std::net::Ipv4Addr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use aws::Region;
use clap::builder::{OsStringValueParser, TypedValueParser};
use clap::error::ErrorKind;
use clap::{
    Arg, ArgAction, ArgMatches, Args, CommandFactory as _, FromArgMatches, Parser, Subcommand,
};
use provider::RunTag;
use schema::ir::Sha256Digest;

use self::notes::{
    BUILD_NOTES, CANCEL_NOTES, CHECK_NOTES, CONFIG, CONTROLLER_NOTES, FMT_NOTES, HELP_NOTES,
    LS_NOTES, PUBLISH_NOTES, REAP_NOTES, RUN_NOTES, VIEW_NOTES, WAIT_NOTES,
};
use crate::controller::Name;
use crate::ending::Printed;
use crate::member::Member;
use crate::publish::{Bucket, Since};
use crate::star::Entry;

/// A command, as clap parses it.
#[derive(Parser)]
#[command(
    name = "pumpkin",
    about = "\
Run workflows: in each run, an agent works in a sandbox, and a final call judges
what it submits. The repository's .pumpkin directory holds build and provider
state and the runs' records, unless --runs names another directory.",
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true,
    subcommand_value_name = "command",
    subcommand_help_heading = "Commands"
)]
enum Parsed {
    #[command(about = "Check tasks' sources, building and running nothing")]
    #[command(long_about = "\
Check each task's source: evaluate its entry file, and the files that it loads,
with the args given, then check the recipe of each image that it declares, and
its workflow, then lint those files, and say every problem found, in all the
tasks.")]
    #[command(after_help = CHECK_NOTES)]
    #[command(mut_arg(ARG, |arg| arg.help(CHECK_ARG)))]
    Check(Check),
    #[command(about = "Format the workflow language's `.star` files")]
    #[command(long_about = "\
Format each `.star` file named, and each below a directory named, in place, in
the one style that the workflow language's files are written in.")]
    #[command(after_help = FMT_NOTES)]
    Fmt(Fmt),
    #[command(about = "Build a task's images, and start a run of it")]
    #[command(long_about = "\
Build a task as `pumpkin build` does, then start a run of its workflow, and print
the run's id once it has started.")]
    #[command(after_help = RUN_NOTES)]
    Run(Run),
    #[command(about = "Wait until a run has ended, and say how it ended")]
    #[command(long_about = "Wait until a run has ended, then say how it ended.")]
    #[command(after_help = WAIT_NOTES)]
    Wait(Wait),
    #[command(about = "List the runs and their states")]
    #[command(long_about = "\
List the runs in --runs, oldest first, a line for each, then a line of totals:
without a prefix, those that are live or ended within the last day, and the
line of totals says how many others there are, and how to list them. A live
run's line gives its phase, how long it has been in it, the bound at which
Pumpkin ends the phase, its task, and what it does; an ended run's, its state,
how long it ran, its task, and the values its result shows.")]
    #[command(after_help = LS_NOTES)]
    Ls(Ls),
    #[command(about = "Stop a run, and remove what it left")]
    #[command(long_about = "\
Stop a run, wait until it has ended, remove what it left, and say how it ended.")]
    #[command(after_help = CANCEL_NOTES)]
    Cancel(CancelRun),
    #[command(about = "Serve the runs' records to this machine's browser")]
    #[command(long_about = "\
Serve the runs in --runs, or a controller's, to a browser on this machine, until
it is stopped, and print the address to open: a page that lists the runs as
`ls` does, newest first, and shows a run's summary, its sandboxes' lanes, who
talks to whom, and its log, which it follows while the run is live.")]
    #[command(after_help = VIEW_NOTES)]
    View(View),
    #[command(about = "Print the help of a command or a topic")]
    #[command(long_about = "\
Print the help of a command or a topic, or `pumpkin --help` without one.")]
    #[command(after_help = HELP_NOTES)]
    Help {
        #[arg(value_name = "topic")]
        #[arg(help = "A command, or a topic: `config`, the machine's configuration.")]
        topic: Option<String>,
    },
    #[command(
        hide = true,
        about = "Build a task's images, and print its workflow's IR"
    )]
    #[command(long_about = "\
Build a task: check its source as `pumpkin check` does, then build each image
that it declares on the machine's Docker engine, and print the IR of its
workflow, which names each image by its ID.")]
    #[command(after_help = BUILD_NOTES)]
    Build(Build),
    #[command(hide = true, about = "Remove what the runs that have ended left")]
    #[command(long_about = "\
Remove what ended or lost runs left and print the run ids reported as cleaned.")]
    #[command(after_help = REAP_NOTES)]
    Reap(Reap),
    #[command(
        hide = true,
        about = "The controller's own side of the controller target"
    )]
    #[command(long_about = "\
On the controller: `job` runs a job of the controller target, which `pumpkin
run` and `pumpkin build` start there in a unit of its own; `boot` reaps what lost
runs left, at the controller's boot, which `boot-all` runs for each installed
build; `install` installs, as root, the host's software, whose units run
`boot-all`, `idle`, the idle stop, and `members`, which copies the members' keys;
`cancel` cancels a run for `pumpkin cancel`, on a client, which says the run's
ending from its copy of the record; `serve` answers, on its standard input and
output, the client that follows a job, lists the runs or views them; and `publish` uploads the runs to the bucket
of the public page.")]
    #[command(after_help = CONTROLLER_NOTES)]
    #[command(disable_help_subcommand = true, subcommand_value_name = "action")]
    #[command(subcommand_help_heading = "Actions")]
    Controller {
        #[command(subcommand)]
        action: Action,
    },
}

// Hidden from clap's list of commands, so `command` lists them apart.
const RARE: [&str; 2] = ["build", "reap"];

/// The actions of `pumpkin controller`.
#[derive(Subcommand)]
enum Action {
    #[command(about = "Run a job of the controller target")]
    Job {
        #[arg(value_name = "id", value_parser = |id: &str| RunTag::try_from(id.to_owned()))]
        #[arg(help = "The job's id, which its run takes.")]
        id: RunTag,
        #[arg(long, value_name = "region")]
        #[arg(value_parser = |region: &str| Region::try_from(region.to_owned()))]
        #[arg(help = "The controller's AWS region.")]
        region: Region,
        #[arg(long, value_name = "name")]
        #[arg(value_parser = utf8(", as every SSM parameter's name is: name one that is"))]
        #[arg(help = "The SSM parameter that holds the controller's settings.")]
        parameter: String,
        #[arg(long, value_name = "sha256")]
        #[arg(value_parser = tree)]
        #[arg(help = "\
The SHA-256 of the manifest of the job's tree, in the controller's store, from
which the job makes the tree in its working directory, which is to be the tree's
directory, trees/<repository>/<id> in the home, that the job's start made.")]
        tree: Sha256Digest,
        #[arg(last = true, required = true, value_name = "command")]
        #[arg(help = "\
`run` or `build` and their arguments, but `--config`, which the job
writes.")]
        command: Vec<OsString>,
    },
    #[command(about = "Reap what lost runs left, at the controller's boot")]
    Boot,
    #[command(about = "Install the controller host's software, as root")]
    #[command(long_about = INSTALL_ABOUT, after_help = INSTALL_NOTES)]
    Install(Host),
    #[command(name = "boot-all", about = "Run each installed build's boot")]
    BootAll,
    #[command(about = "Stop the controller if it has been idle for 30 minutes")]
    Idle,
    #[command(about = "Copy the members' keys, which sshd admits, from SSM")]
    Members(Host),
    #[command(about = "Cancel a run for a client, which says its ending")]
    Cancel(Cancel),
    #[command(about = "Answer a client's link, on the standard input and output")]
    Serve {
        #[arg(long, value_name = "member")]
        #[arg(value_parser = |member: &str| Member::try_from(member.to_owned()))]
        #[arg(help = "\
The member that the link acts for, as the controller's forced command names
them, which `serve` trusts: each job it starts is theirs, and it refuses to
cancel another member's job unless asked to.
Default: none, for a link that names no member")]
        member: Option<Member>,
    },
    #[command(about = "Upload the runs to the bucket of the public page")]
    #[command(long_about = PUBLISH_ABOUT, after_help = PUBLISH_NOTES)]
    Publish(Publish),
}

const INSTALL_ABOUT: &str = "\
Install, as root, what Pumpkin needs on the controller host, from this
`pumpkin`, which its admin's release pins: the user `pumpkin`, lingering, in
Docker's group; this `pumpkin` at /usr/local/sbin/pumpkin; the units that run
each installed build's boot (`boot-all`) and stop the controller when idle
(`idle`) and copy the members' keys from SSM every minute (`members`); and
sshd's settings, which admit only those keys for `pumpkin`, each with a forced
command that names its member. It copies the keys first, and changes nothing
in sshd if it can't. It needs Docker and AWS CLI v2 installed first, and leaves
the rest of what jobs run to the host's provisioner too. It writes each file whole, only where it differs, reloads only what
changed, and says each change, so a second run says nothing.";

const INSTALL_NOTES: &str = "\
The members' list, SSM parameter /pumpkin-<name>/members, holds a line
`<member> <key>` for each key: a member's name, 1 to 32 of a-z, 0-9 and `-`
starting with a letter, and the line of their machine's
~/.config/pumpkin/ssh/key.pub, an `ssh-ed25519` key with an optional comment.
A list with any other line, or a key twice, is refused whole.";

/// Which controller `pumpkin controller install` and `pumpkin controller members` are on.
#[derive(Args)]
pub(crate) struct Host {
    #[arg(long, value_name = "name")]
    #[arg(value_parser = |name: &str| Name::try_from(name.to_owned()))]
    #[arg(help = "The `name` that deploy/aws was applied with.")]
    pub(crate) name: Name,
    #[arg(long, value_name = "region")]
    #[arg(value_parser = |region: &str| Region::try_from(region.to_owned()))]
    #[arg(help = "The region it was applied in.")]
    pub(crate) region: Region,
}

const PUBLISH_ABOUT: &str = "\
Upload the runs in --runs to an S3 bucket, from which a static copy of the
view's page serves them: export them, as `pumpkin view --export` does, but those
that started before --since, into public/ beside --runs, with each of the
controller's addresses replaced too; then upload, with `aws`, AWS CLI v2, in
--region, what changed since the last publish that uploaded all it planned, in
an order in which no file names one not uploaded yet.
It deletes nothing, so the credentials that `aws` finds need only list the
bucket and put objects there. One publish at a time: this one waits for another
that runs. A run's job on the controller publishes every minute while its
run is live, and once more as it ends, as [publish] in the controller's settings
says (`pumpkin help config`).";

/// What `pumpkin controller publish` is asked for, as its help says.
#[derive(Args)]
pub(crate) struct Publish {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to publish.
Default: runs/ in the repository's .pumpkin directory")]
    pub(crate) runs: Option<String>,
    #[arg(long, value_name = "name", help = "The S3 bucket to upload to.")]
    #[arg(value_parser = |name: &str| Bucket::try_from(name.to_owned()))]
    pub(crate) bucket: Bucket,
    #[arg(long, value_name = "region", help = "The bucket's AWS region.")]
    #[arg(value_parser = |region: &str| Region::try_from(region.to_owned()))]
    pub(crate) region: Region,
    #[arg(long, value_name = "instant", value_parser = |since: &str| since.parse::<Since>())]
    #[arg(help = "\
An instant, as RFC 3339 writes one, such as 2026-11-15T00:00:00Z: a run that
started before it, as its id says, is not published.
Default: every run")]
    pub(crate) since: Option<Since>,
    #[arg(
        long = "controller-address",
        value_name = "ipv4",
        help = "\
An IPv4 address of the controller that ran the runs, which becomes
<controller> in what is published; given once for each.
Default: none"
    )]
    pub(crate) controller_addresses: Vec<Ipv4Addr>,
}

/// What a command line asks for. `Job` and the variants after it are the controller's own side,
/// `pumpkin controller <action>`; `Print` holds help or the version, for the standard output.
pub(crate) enum Command {
    Run(Run),
    Wait(Wait),
    Ls(Ls),
    Cancel(CancelRun),
    Reap(Reap),
    Check(Check),
    Fmt(Fmt),
    Build(Build),
    View(View),
    Job(Job),
    Boot,
    Install(Host),
    BootAll,
    Idle,
    Members(Host),
    CancelFor(Cancel),
    Serve(Option<Member>),
    Publish(Publish),
    Print(String),
}

const ARG: &str = "arg";

const CHECK_ARG: &str = "\
The value of the arg that an arg() call declares as <name>, read as the
type that it declares. Each arg is given at most once, and one with no
default must be, for each task.";

/// The `--arg` flag of the commands that evaluate a task, each value read as
/// [`lang::Args::parse`] reads it.
pub(crate) struct TaskArgs(pub(crate) lang::Args);

impl Args for TaskArgs {
    fn augment_args(command: clap::Command) -> clap::Command {
        let arg = Arg::new(ARG).long(ARG).value_name("name>=<value");
        let arg = arg.action(ArgAction::Append).help(
            "\
The value of the arg that an arg() call declares as <name>, as
`pumpkin check` takes it.",
        );
        let utf8 = utf8(", as every arg's name and value is: give it in UTF-8");
        command.arg(arg.value_parser(utf8))
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl FromArgMatches for TaskArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let given = matches.get_many::<String>(ARG).into_iter().flatten();
        let args = lang::Args::parse(given.map(String::as_str)).map_err(|problems| {
            // clap says the first problem of a line alone.
            let Some(first) = problems.into_iter().next() else {
                unreachable!(
                    "`lang::Args::parse` failed with no problem, though it fails with each"
                )
            };
            clap::Error::raw(ErrorKind::ValueValidation, first)
        })?;
        Ok(Self(args))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

const OUTCOME: &str = "outcome";

impl Args for Printed {
    fn augment_args(command: clap::Command) -> clap::Command {
        let outcome = Arg::new(OUTCOME).long(OUTCOME).action(ArgAction::SetTrue);
        command.arg(outcome.help(
            "\
Also print the run's outcome.json on the standard output once it has
ended.",
        ))
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl FromArgMatches for Printed {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        Ok(if matches.get_flag(OUTCOME) {
            Self::Outcome
        } else {
            Self::Nothing
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

const WAIT: &str = "wait";

/// Whether `run` follows the run to its end (`Some`), and what it then prints.
pub(crate) struct Waiting(pub(crate) Option<Printed>);

impl Args for Waiting {
    fn augment_args(command: clap::Command) -> clap::Command {
        let wait = Arg::new(WAIT).long(WAIT).action(ArgAction::SetTrue).help(
            "\
Follow the run to its end, as `pumpkin wait` does, rather than print
its id.",
        );
        let command = Printed::augment_args(command.arg(wait));
        command.mut_arg(OUTCOME, |outcome| outcome.requires(WAIT))
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl FromArgMatches for Waiting {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let printed = Printed::from_arg_matches(matches)?;
        Ok(Self(matches.get_flag(WAIT).then_some(printed)))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

/// Parses a UTF-8 value, and refuses another with `it isn't UTF-8` followed by `why`, which
/// starts with the punctuation that joins the two.
fn utf8(why: &'static str) -> impl TypedValueParser<Value = String> {
    OsStringValueParser::new().try_map(move |value| {
        value
            .into_string()
            .map_err(|_| format!("it isn't UTF-8{why}"))
    })
}

const RECORDED: &str = ", as a run's record keeps each path that `run` is given: name one that is";

const NAMED_NEXT: &str = ", as the commands that `pumpkin` says to run next name it: name one \
                          that is";

const RUNS_NAMED_NEXT: &str = ", as the commands that `pumpkin` says to run next name it: name \
                               a directory with a UTF-8 path";

fn entry() -> impl TypedValueParser<Value = Entry> {
    utf8(NAMED_NEXT).try_map(Entry::parse)
}

/// Its paths are UTF-8, so that a run's record keeps them, and a command that names them names the
/// same files.
#[derive(Args)]
pub(crate) struct Run {
    #[arg(value_name = "entry", value_parser = entry())]
    #[arg(help = "The task's entry file, as `pumpkin check` takes it.")]
    pub(crate) entry: Entry,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    #[arg(long, value_name = "dir", value_parser = utf8(RECORDED), help = "\
The directory runs are recorded in, each in <dir>/<id>.
Default: runs/ in the repository's .pumpkin directory")]
    pub(crate) runs: Option<String>,
    #[arg(long, value_name = "file", value_parser = utf8(RECORDED), help = "\
The machine's configuration; see `pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml")]
    pub(crate) config: Option<String>,
    #[arg(long, value_name = "path", value_parser = utf8(RECORDED), help = "\
The static sandboxd that every sandbox runs, as `cargo xtask sandboxd`
builds it, with the static ec2-boot beside it, which an ec2 provider's
instances boot with.
Default: the one beside this pumpkin, where `cargo xtask build` puts
both")]
    pub(crate) sandboxd: Option<String>,
    #[command(flatten)]
    pub(crate) wait: Waiting,
}

impl Run {
    /// The path flags that were given, each as `(flag, value)`, in the order of the usage.
    pub(crate) fn given(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let Self {
            entry: _,
            args: _,
            runs,
            config,
            sandboxd,
            wait: _,
        } = self;
        let flags = [
            ("--runs", runs),
            ("--config", config),
            ("--sandboxd", sandboxd),
        ];
        (flags.into_iter()).filter_map(|(flag, value)| Some((flag, value.as_deref()?)))
    }
}

const RUN_HELP: &str = "\
The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
end it, that no other run in --runs shares; or its record's path, which
has a `/`.";

const RUNS_HELP: &str = "\
The directory that <run> is looked up in.
Default: runs/ in the repository's .pumpkin directory";

/// Its paths are UTF-8, as those of `ls`, `cancel` and `reap` are, so that a command that names
/// them names the same files.
#[derive(Args)]
pub(crate) struct Wait {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
    #[command(flatten)]
    pub(crate) printed: Printed,
}

/// What `pumpkin view` is asked for, as `pumpkin view --help` says.
#[derive(Args)]
pub(crate) struct View {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to serve.
Default: runs/ in the repository's .pumpkin directory")]
    pub(crate) runs: Option<String>,
    #[arg(
        long,
        value_name = "port",
        help = "\
The port to serve at, on 127.0.0.1.
Default: one that is free, which `view` says"
    )]
    pub(crate) port: Option<u16>,
    #[arg(
        long,
        value_name = "dir",
        conflicts_with = "port",
        help = "\
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
Default: serve the runs"
    )]
    pub(crate) export: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Ls {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to list.
Default: runs/ in the repository's .pumpkin directory")]
    pub(crate) runs: Option<String>,
    /// The prefixes of the ids of the runs to list, or none for every run.
    #[arg(value_name = "prefix", value_parser = utf8(", as every id is: name a prefix of an id"))]
    #[arg(help = "\
A prefix of the ids of the runs to list, such as a day's or an hour's,
as ids start with the UTC time of their `pumpkin run`.
Default: every run")]
    pub(crate) prefixes: Vec<String>,
}

/// What `pumpkin cancel` is asked for: a [`Cancel`], and whose runs its run is named among.
#[derive(Args)]
pub(crate) struct CancelRun {
    #[command(flatten)]
    pub(crate) cancel: Cancel,
    #[arg(
        long,
        help = "\
Name the run among every member's runs on the controller. Without it, a
prefix or an id is matched among this member's runs, and those that no member
started, and a run of another member's is refused."
    )]
    pub(crate) any_member: bool,
}

#[derive(Args)]
pub(crate) struct Cancel {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
    #[arg(
        long,
        help = "\
End the run's driver at once, by SIGKILL, rather than stop the run by
SIGTERM; the run is then lost, and recorded as failed. A create that the
driver had given up on may still make a sandbox after `cancel` has
ended, which `pumpkin reap` removes."
    )]
    pub(crate) force: bool,
    #[arg(long, hide = true, value_name = "file", help = MODAL_CONFIG_HELP)]
    pub(crate) modal_config: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Reap {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to reap.
Default: runs/ in the repository's .pumpkin directory")]
    pub(crate) runs: Option<String>,
    #[arg(long, hide = true, value_name = "file", help = MODAL_CONFIG_HELP)]
    pub(crate) modal_config: Option<PathBuf>,
}

// Hidden: only the controller target gives `--modal-config`, as a job's Modal configuration
// file goes with the job, and the client sends it again.
const MODAL_CONFIG_HELP: &str = "\
Modal's configuration file, read for each provider of kind modal in place of
the one that the configuration of the run names.";

#[derive(Args)]
pub(crate) struct Check {
    #[arg(value_name = "entry", required = true, value_parser = entry(), help = "\
A task's entry file, a .star file below the root of its repository: the
nearest directory, at or above the entry's, that holds PUMPKIN.star.
Each task is checked with the same args.")]
    pub(crate) entries: Vec<Entry>,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
}

#[derive(Args)]
pub(crate) struct Fmt {
    /// The files and directories to format, as given; none for the working directory.
    #[arg(value_name = "path", value_parser = utf8(NAMED_NEXT), help = "\
A `.star` file, or a directory, each `.star` file below which is
formatted, but those whose name, or that of a directory on the way to
them, starts with `.`, and those that a symbolic link leads to.
Default: the working directory")]
    pub(crate) paths: Vec<String>,
    #[arg(
        long,
        help = "\
Change no file: say each one that isn't formatted, and fail if one
isn't."
    )]
    pub(crate) check: bool,
}

const BUILD_CONFIG: &str = "\
The machine's configuration, whose [docker] names the engine; see
`pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml";

#[derive(Args)]
pub(crate) struct Build {
    #[arg(value_name = "entry", value_parser = entry())]
    #[arg(help = "The task's entry file, as `pumpkin check` takes it.")]
    pub(crate) entry: Entry,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    #[arg(long, value_name = "file", value_parser = utf8(NAMED_NEXT), help = BUILD_CONFIG)]
    pub(crate) config: Option<String>,
}

pub(crate) struct Job {
    pub(crate) id: RunTag,
    pub(crate) region: Region,
    pub(crate) parameter: String,
    pub(crate) tree: Sha256Digest,
    pub(crate) command: JobCommand,
}

pub(crate) enum JobCommand {
    Run(Run),
    Build(Build),
}

#[derive(Clone)]
/// A run named by its record's path, which has a `/`, or by its id or a prefix of it.
pub(crate) enum RunName {
    Record(String),
    Prefix(String),
}

fn tree(hex: &str) -> Result<Sha256Digest, &'static str> {
    crate::manifest::digest(hex)
        .ok_or("it isn't 64 lowercase hexadecimal digits: give the SHA-256 of the tree's manifest")
}

fn run_name() -> impl TypedValueParser<Value = RunName> {
    OsStringValueParser::new().try_map(|run| {
        if run.as_bytes().contains(&b'/') {
            return run.into_string().map(RunName::Record).map_err(|_| {
                "it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a \
                 record with a UTF-8 path"
            });
        }
        run.into_string().map(RunName::Prefix).map_err(|_| {
            "it isn't UTF-8, as every id is: name an id, a prefix of one, or a record's path, \
             which has a `/`"
        })
    })
}

/// A command line that clap refuses.
pub(crate) struct Refusal {
    pub(crate) error: clap::Error,
    /// The command line that prints the help of the command that was refused.
    pub(crate) help: String,
}

/// Reads `args`, which leave out the program's name, into the command that they ask for.
/// `-h` or `--help` before `--`, and `-V` or `--version` before the command, are answered as
/// [`Command::Print`], not refused.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Refusal> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut cli = command();
    // clap's error doesn't name the command it was found in; the first word does, as only `-h`,
    // `--help`, `-V` and `--version` may come before it.
    let first = args
        .iter()
        .position(|arg| !arg.as_bytes().starts_with(b"-"));
    let named = first.and_then(|at| cli.find_subcommand(&args[at]));
    let named = named.map(|command| command.get_name().to_owned());
    let action = (named.as_deref() == Some("controller"))
        .then(|| first.and_then(|at| args.get(at + 1)))
        .flatten()
        .and_then(|word| word.to_str())
        .filter(|word| !word.starts_with('-'))
        .map(str::to_owned);
    let mut options = args.iter().take_while(|arg| *arg != "--");
    if options.any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Print(help(
            &mut cli,
            named.as_deref(),
            action.as_deref(),
        )));
    }
    let before = &args[..first.unwrap_or(args.len())];
    if before.iter().any(|arg| arg == "-V" || arg == "--version") {
        return Ok(Command::Print(format!("pumpkin {}\n", runtime::BUILD)));
    }
    let refusal = |error| Refusal {
        error,
        help: match (&named, &action) {
            (Some(controller), Some(action)) => format!("pumpkin {controller} {action} --help"),
            (Some(name), None) => format!("pumpkin help {name}"),
            (None, _) => "pumpkin help".to_owned(),
        },
    };
    let line = iter::once(OsString::from("pumpkin")).chain(args.iter().cloned());
    let matches = cli.try_get_matches_from_mut(line).map_err(refusal)?;
    let parsed = Parsed::from_arg_matches(&matches).map_err(|error| {
        // Only `--arg`'s check fails here, in a command that the line names, whose usage it shows.
        match named
            .as_deref()
            .and_then(|name| cli.find_subcommand_mut(name))
        {
            Some(command) => refusal(error.format(command)),
            None => refusal(error.format(&mut cli)),
        }
    })?;
    command_of(&mut cli, parsed).map_err(refusal)
}

fn command_of(cli: &mut clap::Command, parsed: Parsed) -> Result<Command, clap::Error> {
    Ok(match parsed {
        Parsed::Run(run) => Command::Run(run),
        Parsed::Wait(wait) => Command::Wait(wait),
        Parsed::Ls(ls) => Command::Ls(ls),
        Parsed::Cancel(cancel) => Command::Cancel(cancel),
        Parsed::Reap(reap) => Command::Reap(reap),
        Parsed::Check(check) => Command::Check(check),
        Parsed::Fmt(fmt) => Command::Fmt(fmt),
        Parsed::Build(build) => Command::Build(build),
        Parsed::View(view) => Command::View(view),
        Parsed::Controller {
            action: Action::Boot,
        } => Command::Boot,
        Parsed::Controller {
            action: Action::Install(host),
        } => Command::Install(host),
        Parsed::Controller {
            action: Action::Members(host),
        } => Command::Members(host),
        Parsed::Controller {
            action: Action::BootAll,
        } => Command::BootAll,
        Parsed::Controller {
            action: Action::Idle,
        } => Command::Idle,
        Parsed::Controller {
            action: Action::Cancel(cancel),
        } => Command::CancelFor(cancel),
        Parsed::Controller {
            action: Action::Serve { member },
        } => Command::Serve(member),
        Parsed::Controller {
            action: Action::Publish(publish),
        } => Command::Publish(publish),
        Parsed::Controller {
            action:
                Action::Job {
                    id,
                    region,
                    parameter,
                    tree,
                    command,
                },
        } => Command::Job(Job {
            id,
            region,
            parameter,
            tree,
            command: job_command(cli, command)?,
        }),
        Parsed::Help { topic } => return help_of(cli, topic.as_deref()),
    })
}

fn job_command(cli: &mut clap::Command, command: Vec<OsString>) -> Result<JobCommand, clap::Error> {
    // The job's command is a whole command line of its own, so it is parsed as one.
    match parse(command).map_err(|refusal| refusal.error)? {
        Command::Run(run) => Ok(JobCommand::Run(run)),
        Command::Build(build) => Ok(JobCommand::Build(build)),
        Command::Wait(_)
        | Command::Ls(_)
        | Command::Cancel(_)
        | Command::Reap(_)
        | Command::Check(_)
        | Command::Fmt(_)
        | Command::View(_)
        | Command::Job(_)
        | Command::Boot
        | Command::Install(_)
        | Command::BootAll
        | Command::Idle
        | Command::Members(_)
        | Command::CancelFor(_)
        | Command::Serve(_)
        | Command::Publish(_)
        | Command::Print(_) => {
            let problem = "the job's command is neither `run` nor `build`: give one";
            let controller = cli.find_subcommand_mut("controller");
            Err(
                match controller.and_then(|it| it.find_subcommand_mut("job")) {
                    Some(job) => job.error(ErrorKind::InvalidValue, problem),
                    None => cli.error(ErrorKind::InvalidValue, problem),
                },
            )
        }
    }
}

fn help_of(cli: &mut clap::Command, topic: Option<&str>) -> Result<Command, clap::Error> {
    let Some(topic) = topic else {
        return Ok(Command::Print(help(cli, None, None)));
    };
    if topic == "config" {
        return Ok(Command::Print(CONFIG.to_owned()));
    }
    let names: Vec<String> = (cli.get_subcommands())
        .filter(|command| listed(command))
        .map(|command| format!("`{}`", command.get_name()))
        .collect();
    if names.iter().any(|name| name.trim_matches('`') == topic) {
        return Ok(Command::Print(help(cli, Some(topic), None)));
    }
    let problem = format!(
        "there is no help for `{topic}`: expected a command ({}) or a topic (`config`)",
        names.join(", ")
    );
    // Found in `help`, so the error shows that command's usage.
    Err(match cli.find_subcommand_mut("help") {
        Some(command) => command.error(ErrorKind::InvalidValue, problem),
        None => cli.error(ErrorKind::InvalidValue, problem),
    })
}

fn listed(command: &clap::Command) -> bool {
    !command.is_hide_set() || RARE.contains(&command.get_name())
}

fn command() -> clap::Command {
    // No command has a help flag of clap's, so a usage error ends without clap's pointer to it,
    // and `pumpkin help <command>` is said instead; `parse` answers `-h`, `--help`, `-V` and
    // `--version`.
    let help = Arg::new("help").short('h').long("help").global(true);
    let help = help.action(ArgAction::SetTrue).help("Print this help.");
    let version = Arg::new("version").short('V').long("version");
    let version = version
        .action(ArgAction::SetTrue)
        .help("Print the version.");
    let cli = Parsed::command()
        .arg(version)
        .arg(help.display_order(usize::MAX));
    let width = (cli.get_subcommands())
        .filter(|command| listed(command))
        .map(|command| command.get_name().len())
        .max()
        .unwrap_or_default();
    let rare: Vec<String> = (cli.get_subcommands())
        .filter(|command| RARE.contains(&command.get_name()))
        .map(|command| {
            let about = command.get_about().map(ToString::to_string);
            format!(
                "  {:width$}  {}\n",
                command.get_name(),
                about.unwrap_or_default()
            )
        })
        .collect();
    let rare = rare.concat();
    cli.after_help(format!(
        "\
Rarely needed:
{rare}
Topics:
  {:width$}  The machine's configuration, pumpkin.toml

`pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
does, and `pumpkin help <topic>` a topic.",
        "config"
    ))
}

fn help(cli: &mut clap::Command, name: Option<&str>, action: Option<&str>) -> String {
    cli.build();
    let help = match name.and_then(|name| cli.find_subcommand_mut(name)) {
        Some(command) => match action.and_then(|action| command.find_subcommand_mut(action)) {
            Some(action) => action.render_long_help(),
            None => command.render_long_help(),
        },
        None => cli.render_long_help(),
    };
    help.to_string()
}
