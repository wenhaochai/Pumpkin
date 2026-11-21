//! AWS's command line, `aws`, as Pumpkin's providers run it ([`Cli`]), with ECR's repositories
//! on it, and the IAM roles that providers name in their settings ([`RoleArn`]).
//!
//! `Cli` runs only a declared [`Call`], which states what IAM must allow of it, so a principal's
//! policy is what its code's calls state ([`iam::policy`]).
//!
//! Every call `aws` makes is in the region its [`Cli`] was made for, with the credentials that
//! `aws` finds on its own, which on an EC2 instance are the instance's role. No variable of this
//! process's environment, such as `AWS_REGION` or `AWS_PROFILE`, steers `aws` to another region
//! or account.

use std::process::Command;
use std::time::Duration;

use engine::{Login, MANIFEST_TYPES, Pusher};
use schema::ir::Image;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stdx::process::{self, Echo, Environment, Failure, Run};
use stdx::{Context as _, Deadline};

use crate::iam::{Call, Ecr};

pub mod iam;

// Every command a provider runs is one AWS answers at once, in one page. Throttled, `aws` tries
// again ([`RETRIES`]), waiting about 7 min in all at most; it gives up on a connection, and on a
// read that gets nothing, after 60 s each. So past 10 min a command is hung, as on a credentials
// process that never ends, or AWS is out, and only a bound turns that into a failure, rather
// than a wait without end.
const CALL_TIMEOUT: Duration = Duration::from_mins(10);

/// The variables of `aws`'s own that set how it retries a request, which it gets beside
/// [`CALL_ENVIRONMENT`]: 25 attempts in all, in its standard mode.
// AWS throttles a burst of calls, as when many runs are launched at once, and `aws`, which makes
// 3 attempts by default, then fails runs that only had to wait. Its standard mode waits a random
// time before each retry, up to twice the last bound, from 1 s to 20 s at most, so 25 attempts
// wait 7 min at most, half that on average; legacy mode, which a configuration may name, has no
// such cap.
const RETRIES: [(&str, &str); 2] = [("AWS_MAX_ATTEMPTS", "25"), ("AWS_RETRY_MODE", "standard")];

/// What `aws` gets of this process's environment: what it needs to find its own configuration.
// `HOME`, for `~/.aws`; `PATH`, for the programs its configuration may run for credentials; and
// its own variables for where that configuration lives. Any other variable of its, such as
// `AWS_REGION`, `AWS_PROFILE` or `AWS_ACCESS_KEY_ID`, would steer it past the region it is given.
const CALL_ENVIRONMENT: Environment = Environment::Only(&[
    "HOME",
    "PATH",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
]);

/// The user name of every login to ECR, whose password `aws ecr get-login-password` gives.
const ECR_USER: &str = "AWS";

/// The failure code by which ECR says it holds no image by the identifier it was asked.
const NOT_FOUND: &str = "ImageNotFound";

/// An AWS region, such as `us-west-1`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct Region(String);

impl Region {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Region {
    type Error = String;

    fn try_from(region: String) -> Result<Self, Self::Error> {
        let allowed = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        let holds = (region.split('-')).all(|word| !word.is_empty() && word.bytes().all(allowed));
        if !holds {
            return Err(format!(
                "`{}` is not an AWS region: use one such as `us-west-1`",
                region.escape_debug()
            ));
        }
        Ok(Self(region))
    }
}

/// The name of an ECR repository, such as `pumpkin/roles`, which AWS refuses if it names none.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RepositoryName(String);

impl RepositoryName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The ARN of an IAM role, such as `arn:aws:iam::123456789012:role/pumpkin-timer`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoleArn {
    arn: String,
    /// The AWS partition it is in, such as `aws`.
    partition: String,
}

impl RoleArn {
    pub fn as_str(&self) -> &str {
        &self.arn
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }
}

impl From<RoleArn> for String {
    fn from(role: RoleArn) -> Self {
        role.arn
    }
}

impl TryFrom<String> for RoleArn {
    type Error = String;

    fn try_from(arn: String) -> Result<Self, Self::Error> {
        // `arn:<partition>:iam::<account>:role/<name>`; AWS refuses a role it doesn't hold.
        let parts = (arn.strip_prefix("arn:")).and_then(|rest| rest.split_once(":iam::"));
        let partition = parts.filter(|(partition, rest)| {
            !partition.is_empty() && !partition.contains(':') && rest.contains(":role/")
        });
        let Some((partition, _)) = partition else {
            return Err(format!(
                "`{}` is not an IAM role's ARN: use one such as \
                 `arn:aws:iam::123456789012:role/pumpkin-timer`",
                arn.escape_debug()
            ));
        };
        let partition = partition.to_owned();
        Ok(Self { arn, partition })
    }
}

/// An ECR repository, as AWS describes it ([`Cli::repositories`]).
#[derive(Debug, Serialize)]
pub struct Repository {
    name: RepositoryName,
    /// Its URI, `<registry>/<name>`, which every image in it is named under.
    uri: String,
    /// The calls on it.
    #[serde(skip)]
    ecr: &'static Ecr,
}

impl Repository {
    pub fn name(&self) -> &RepositoryName {
        &self.name
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Its registry, which its URI names first.
    pub fn registry(&self) -> &str {
        let uri = &self.uri;
        uri.split_once('/').map_or(uri, |(registry, _)| registry)
    }
}

/// When an `aws` command is killed if it hasn't ended.
#[derive(Clone, Copy)]
pub enum Bound {
    /// 10 min after it starts (`CALL_TIMEOUT`).
    Call,
    /// At a create's deadline, `provider::Create::by`, as this deadline.
    Create(Deadline),
}

/// The `aws` command line, AWS CLI v2, found on `PATH`, run in one region, given only the
/// environment that finds its own configuration.
///
/// Production code reaches AWS only through it, but for the ECR login that an EC2 sandbox's entry
/// makes with its instance's own `aws`.
#[derive(Debug)]
pub struct Cli {
    region: Region,
    version: String,
}

/// What `aws ecr describe-repositories` prints.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EcrRepositories {
    repositories: Vec<EcrRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EcrRepository {
    repository_name: String,
    repository_uri: String,
}

/// What `aws ecr batch-get-image` prints of the images it was asked for: those it found, and a
/// failure for each other. Either list is left out when empty.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchImages {
    #[serde(default)]
    images: Vec<FoundImage>,
    #[serde(default)]
    failures: Vec<ImageFailure>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoundImage {
    image_id: ImageId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageFailure {
    image_id: ImageId,
    failure_code: String,
    failure_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageId {
    image_digest: Option<String>,
}

impl Cli {
    /// The command line, run in `region`, with the version `aws --version` printed.
    pub fn connect(region: Region) -> Result<Self, String> {
        let how = bounded(Bound::Call).named("aws --version");
        let output = process::run(aws().arg("--version"), how).map_err(|failure| {
            match failure {
                // Version 1, which some package managers install, takes none of the options that
                // every other command gives, such as `--no-cli-pager`.
                Failure::Unstarted { name, error, .. } => Failure::Unstarted {
                    name,
                    error,
                    expected: "install AWS CLI v2 on `PATH`".to_owned(),
                },
                failure @ (Failure::Unwatched { .. }
                | Failure::Unended { .. }
                | Failure::Exited { .. }) => failure,
            }
            .to_string()
        })?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Self { region, version })
    }

    /// What `aws --version` printed, such as `aws-cli/2.38.1 Python/3.14.6 Linux/…`.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Runs `call` with `args`, its options, killed at `bound`, and parses the JSON it prints.
    pub fn json<T: DeserializeOwned>(
        &self,
        call: &'static Call,
        args: &[&str],
        bound: Bound,
    ) -> Result<T, String> {
        serde_json::from_slice(&self.run(call, args, bound)?).map_err(|error| {
            format!(
                "the JSON `{}` printed isn't of the form expected: {error}",
                shown(call)
            )
        })
    }

    /// Runs `call` with `args`, its options, killed at `bound`, and returns what it prints on
    /// standard output.
    pub fn run(&self, call: &'static Call, args: &[&str], bound: Bound) -> Result<Vec<u8>, String> {
        let output = process::run(&mut self.command(call, args), bounded(bound));
        Ok(output.map_err(|failure| failure.to_string())?.stdout)
    }

    /// Runs `call` with `args`, its options, killed at `bound`, and returns what it prints on
    /// standard output, or `None` if AWS reports that what it names doesn't exist
    /// (`ResourceNotFoundException`).
    pub fn run_found(
        &self,
        call: &'static Call,
        args: &[&str],
        bound: Bound,
    ) -> Result<Option<Vec<u8>>, String> {
        // How AWS CLI v2 writes the code of an error that AWS reports.
        let absent = "An error occurred (ResourceNotFoundException)";
        match process::run(&mut self.command(call, args), bounded(bound)) {
            Ok(output) => Ok(Some(output.stdout)),
            Err(failure) if failure.reported().contains(absent) => Ok(None),
            Err(failure) => Err(failure.to_string()),
        }
    }

    /// The repositories that `names` name, in their order, each described by one call of
    /// `ecr`'s, on which each is published, or why AWS described none of that name.
    pub fn repositories<const N: usize>(
        &self,
        ecr: &'static Ecr,
        names: [&RepositoryName; N],
    ) -> Result<[Result<Repository, String>; N], String> {
        let mut args = vec!["--repository-names"];
        args.extend(names.iter().map(|name| name.as_str()));
        let EcrRepositories { repositories } = self.json(&ecr.describe, &args, Bound::Call)?;
        Ok(names.map(|name| {
            let found =
                (repositories.iter()).find(|repository| repository.repository_name == name.0);
            let Some(found) = found else {
                return Err(format!(
                    "AWS described no repository `{}`: expected each repository named to exist",
                    name.0
                ));
            };
            Ok(Repository {
                name: name.clone(),
                uri: found.repository_uri.clone(),
                ecr,
            })
        }))
    }

    /// `image`, an image of `engine`'s, in `repository`, `<URI>@<image>`, once the repository holds
    /// it: pushed from the engine, tagged with its digest's hexadecimal digits, unless the
    /// repository holds it already, by its digest, as ECR answers. It fails if the repository
    /// holds no image by its digest once the push ends.
    pub fn publish(
        &self,
        engine: &Pusher<'_>,
        repository: &Repository,
        image: &Image,
    ) -> Result<String, String> {
        let reference = format!("{}@{image}", repository.uri);
        if self.holds(repository, image)? {
            return Ok(reference);
        }
        let pushed = self.push(engine, repository, image);
        // Another publisher, such as a run that starts at once with the same image, may have
        // pushed it since: then this push fails, as the repository's tags can't be moved, and
        // the repository holds the image all the same.
        let holds = self.holds(repository, image);
        match (pushed, holds) {
            (_, Ok(true)) => Ok(reference),
            (Ok(()), Ok(false)) => Err(format!(
                "repository `{}` holds no image {image} once it was pushed there: expected the \
                 push to keep the image's manifest, as the containerd image store does",
                repository.name.0
            )),
            (Err(pushing), Ok(false)) => Err(pushing),
            (Ok(()), Err(looking)) => Err(looking),
            (Err(pushing), Err(looking)) => Err(format!("{pushing}; and then {looking}")),
        }
    }

    /// Whether `repository` holds `image`, by its digest, as `aws ecr batch-get-image` answers:
    /// with the image, or with a failure whose code is [`NOT_FOUND`] for it.
    fn holds(&self, repository: &Repository, image: &Image) -> Result<bool, String> {
        let id = format!("imageDigest={image}");
        let mut args = vec![
            "--repository-name",
            &repository.name.0,
            "--image-ids",
            &id,
            "--accepted-media-types",
        ];
        args.extend(MANIFEST_TYPES);
        let looking = format_args!(
            "looking up {image} in repository `{}` failed",
            repository.name.0
        );
        let found = self.json(&repository.ecr.holds, &args, Bound::Call);
        let BatchImages { images, failures } = found.context(looking)?;
        let ours = |id: &ImageId| id.image_digest.as_deref() == Some(image.as_str());
        if images.iter().any(|found| ours(&found.image_id)) {
            return Ok(true);
        }
        let absent =
            |failure: &ImageFailure| ours(&failure.image_id) && failure.failure_code == NOT_FOUND;
        if failures.iter().any(absent) {
            return Ok(false);
        }
        let reported = failures.iter().map(|failure| {
            let reason = failure.failure_reason.as_deref().unwrap_or_default();
            format!(
                "`{}` ({})",
                failure.failure_code,
                reason.trim_end_matches('.')
            )
        });
        Err(format!(
            "{looking}: ECR reports {}: expected the image, or `{NOT_FOUND}` for it",
            reported.collect::<Vec<_>>().join("; ")
        ))
    }

    /// Pushes `image` from `pusher`'s engine to `repository`, tagged with its digest's
    /// hexadecimal digits, logged in to the repository's registry as [`ECR_USER`] with the
    /// password that `aws ecr get-login-password` gives, which only that push gets
    /// (`engine::Pusher::push`).
    fn push(
        &self,
        pusher: &Pusher<'_>,
        repository: &Repository,
        image: &Image,
    ) -> Result<(), String> {
        // The password is good for the registry of the account and region `aws` calls, which
        // holds every repository there.
        let password = self.run(&repository.ecr.login, &[], Bound::Call)?;
        let password = String::from_utf8_lossy(&password).trim().to_owned();
        let login = Login::new(
            repository.registry().to_owned(),
            ECR_USER.to_owned(),
            password,
        );
        // A tag that its image's digest names names one image, as the repository's tags can't
        // be moved. The digits alone: `sha256-` and the digits is the tag of the index of the
        // image's referrers, such as its signatures, where a registry has no referrers API.
        pusher.push(image, &login, &repository.name.0, image.hex())
    }

    fn command(&self, call: &Call, args: &[&str]) -> Command {
        let mut command = aws();
        // After `args`, so that a failure is named by the command they give, as `aws` takes its
        // global options anywhere: its parser of them reads every argument, and leaves the rest
        // to the command.
        // https://github.com/aws/aws-cli/blob/7372e144ac4a20def37cc46ba60f1bcce4aec215/awscli/clidriver.py#L454-L460
        command.args(call.command()).args(args).args([
            "--no-cli-pager",
            "--output",
            "json",
            "--region",
            &self.region.0,
        ]);
        command
    }
}

fn aws() -> Command {
    let mut command = stdx::process::command("aws", CALL_ENVIRONMENT);
    command.envs(RETRIES);
    command
}

/// A silent run of an `aws` command, killed at `bound`.
fn bounded(bound: Bound) -> Run<'static> {
    let bound = match bound {
        Bound::Call => process::Bound::Within(CALL_TIMEOUT),
        Bound::Create(by) => process::Bound::By(by),
    };
    Run::new(bound, Echo::Silent)
}

/// How an error shows `call`'s command, such as `aws ec2 describe-images`, without its options,
/// which can be long.
fn shown(call: &Call) -> String {
    let [service, operation] = call.command();
    format!("aws {service} {operation}")
}
