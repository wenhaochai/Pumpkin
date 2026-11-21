//! What IAM must allow of each command that [`Cli`](crate::Cli) runs ([`Call`]), ECR's among
//! them ([`Ecr`]), and a principal's policy, from what IAM must allow of each call that it makes
//! ([`policy`]).
//!
//! A statement ([`Allow`]) names actions, the resources they act on and the conditions that the
//! policy keys on, as AWS's Service Authorization Reference gives them for the command's
//! operation: <https://docs.aws.amazon.com/service-authorization/latest/reference/>. A call whose
//! operation needs more than its own action, such as a launch that tags what it launches, states
//! each. A value that only a deployment knows, such as the account, is a [`Placeholder`], or a
//! [`List`] of values, which a policy leaves for Terraform's `templatefile` to fill.

pub use self::policy::{Fill, placeholders, policy};

mod policy;

pub const PARTITION: Placeholder =
    Placeholder::new("partition", "the AWS partition, such as `aws`");

pub const REGION: Placeholder = Placeholder::new("region", "the AWS region of the deployment");

pub const ACCOUNT: Placeholder = Placeholder::new("account", "the ID of the deployment's account");

/// A string that a deployment fills in, named `name`, which `about` describes.
#[derive(Debug)]
pub struct Placeholder {
    name: &'static str,
    about: &'static str,
}

impl Placeholder {
    /// # Panics
    ///
    /// If `name` is empty or holds a byte but an ASCII lowercase letter and `_`, which a
    /// Terraform variable's name may hold, as a placeholder in a `const` fails to compile.
    pub const fn new(name: &'static str, about: &'static str) -> Self {
        let bytes = name.as_bytes();
        assert!(!bytes.is_empty(), "a placeholder's name is empty");
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            assert!(
                byte.is_ascii_lowercase() || byte == b'_',
                "a placeholder's name holds a byte but `a`-`z` and `_`"
            );
            index += 1;
        }
        Self { name, about }
    }
}

/// A list of strings that a deployment fills in, as a [`Placeholder`] names and describes it,
/// which only a condition's values take ([`Values::Listed`]).
#[derive(Debug)]
pub struct List(Placeholder);

impl List {
    /// # Panics
    ///
    /// As [`Placeholder::new`] does.
    pub const fn new(name: &'static str, about: &'static str) -> Self {
        Self(Placeholder::new(name, about))
    }
}

/// A part of a resource's ARN or of a condition's value.
#[derive(Debug)]
pub enum Part {
    Text(&'static str),
    /// The value that a placeholder stands for.
    Fill(&'static Placeholder),
}

/// The resources that a statement allows actions on.
// By the ARN formats of the Service Authorization Reference, as Policy Sentry reads them: an
// action's resource types, each with its ARN format and its condition keys, and the actions that
// it depends on, which a `Call` states as `also`.
// https://github.com/salesforce/policy_sentry/blob/2357c69171c2733150e99cc03340e662bc40382c/policy_sentry/querying/actions.py#L35-L95
#[derive(Debug)]
pub enum Resource {
    /// Every resource, `*`, for an action that takes no resource type.
    Any,
    /// The resource whose ARN a placeholder stands for.
    Arn(&'static Placeholder),
    /// The resources below one whose ARN a placeholder stands for: `<ARN>/<path>`.
    Below(&'static Placeholder, &'static str),
    /// The resources of a service, such as `ec2`, in the deployment's region and account:
    /// `arn:${partition}:<service>:${region}:${account}:<parts>`.
    In(&'static str, &'static [Part]),
}

/// A condition operator of IAM's.
#[derive(Clone, Copy, Debug)]
pub enum Test {
    ArnEquals,
    Bool,
    Null,
    StringEquals,
}

/// A condition key.
#[derive(Debug)]
pub enum Key {
    /// A key by its whole name, such as `iam:PassedToService`.
    Named(&'static str),
    /// A service's key of a resource's tag: `<service>:ResourceTag/<tag>`.
    Tag(&'static str, &'static str),
}

/// The values that a condition compares its key with.
#[derive(Debug)]
pub enum Values {
    /// Each part, a value.
    Are(&'static [Part]),
    /// The values of a list that a deployment fills in.
    Listed(&'static List),
}

/// A condition of a statement: its key, tested by its operator, against its values.
#[derive(Debug)]
pub struct Condition {
    test: Test,
    key: Key,
    values: Values,
}

impl Condition {
    pub const fn new(test: Test, key: Key, values: Values) -> Self {
        Self { test, key, values }
    }
}

/// A statement of a policy: `actions`, such as `ec2:RunInstances`, on `resources`, under every
/// one of its conditions.
#[derive(Debug)]
pub struct Allow {
    actions: &'static [&'static str],
    resources: &'static [Resource],
    conditions: &'static [Condition],
}

impl Allow {
    pub const fn new(actions: &'static [&'static str], resources: &'static [Resource]) -> Self {
        Self {
            actions,
            resources,
            conditions: &[],
        }
    }

    /// This statement, allowed only under `conditions`.
    #[must_use]
    pub const fn when(self, conditions: &'static [Condition]) -> Self {
        Self { conditions, ..self }
    }
}

/// A command that `aws` runs, such as `ec2 run-instances`, and what IAM must allow of it.
#[derive(Debug)]
pub struct Call {
    command: [&'static str; 2],
    allow: Option<Allow>,
    also: &'static [Allow],
}

impl Call {
    /// `command`, its service and operation as `aws` names them, which `allow` allows.
    pub const fn new(command: [&'static str; 2], allow: Allow) -> Self {
        Self {
            command,
            allow: Some(allow),
            also: &[],
        }
    }

    /// `command`, which IAM allows every caller, as it does `sts get-caller-identity`.
    // As iamlive's mapping lists such calls apart:
    // https://github.com/iann0036/iamlive/blob/d8e67a2d9a422a00c9246531b3a5f3892c465585/iamlivecore/map.json#L3-L5
    pub const fn open(command: [&'static str; 2]) -> Self {
        Self {
            command,
            allow: None,
            also: &[],
        }
    }

    /// This call, which needs what `also` allows besides, such as the actions of the resources
    /// that its operation makes.
    #[must_use]
    pub const fn also(self, also: &'static [Allow]) -> Self {
        Self { also, ..self }
    }

    pub(crate) fn command(&self) -> [&'static str; 2] {
        self.command
    }

    /// What IAM must allow of this call.
    pub fn allows(&self) -> impl Iterator<Item = &Allow> {
        self.allow.iter().chain(self.also)
    }
}

/// The calls on ECR of a user of some repositories, and what IAM must allow of them.
///
/// They describe the repositories ([`Cli::repositories`](crate::Cli::repositories)) and publish
/// an image to one ([`Cli::publish`](crate::Cli::publish)), whose push the engine makes with the
/// password of a login to ECR.
#[derive(Debug)]
pub struct Ecr {
    pub(crate) describe: Call,
    pub(crate) holds: Call,
    pub(crate) login: Call,
    push: Allow,
}

impl Ecr {
    /// The calls on `repositories`, the ARNs of the repositories that they name.
    pub const fn on(repositories: &'static [Resource]) -> Self {
        Self {
            describe: Call::new(
                ["ecr", "describe-repositories"],
                Allow::new(&["ecr:DescribeRepositories"], repositories),
            ),
            holds: Call::new(
                ["ecr", "batch-get-image"],
                Allow::new(&["ecr:BatchGetImage"], repositories),
            ),
            login: Call::new(
                ["ecr", "get-login-password"],
                Allow::new(&["ecr:GetAuthorizationToken"], &[Resource::Any]),
            ),
            // ECR's policy for a push:
            // https://docs.aws.amazon.com/AmazonECR/latest/userguide/image-push-iam.html
            push: Allow::new(
                &[
                    "ecr:BatchCheckLayerAvailability",
                    "ecr:CompleteLayerUpload",
                    "ecr:InitiateLayerUpload",
                    "ecr:PutImage",
                    "ecr:UploadLayerPart",
                ],
                repositories,
            ),
        }
    }

    /// What IAM must allow of these calls.
    pub fn allows(&self) -> impl Iterator<Item = &Allow> {
        let Self {
            describe,
            holds,
            login,
            push,
        } = self;
        [describe, holds, login]
            .into_iter()
            .flat_map(Call::allows)
            .chain([push])
    }
}
