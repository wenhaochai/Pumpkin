//! The calls that `pumpkin`'s own code makes on AWS, and each principal's policy ([`policy`]).
//!
//! A principal's policy is what IAM must allow of each call that it makes: the controller's, as
//! its jobs run the providers, read their settings and publish; the providers' other principals;
//! and a client's, as it reaches its controller.

use aws::iam::{
    self, Allow, Call, Condition, Fill, Key, Part, Placeholder, Resource, Test, Values,
};
use clap::ValueEnum;

use crate::controller::USER;

/// A principal of a deployment of the controller, or a part of one, whose policy
/// `pumpkin controller policy` prints.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Principal {
    #[value(help = "The controller's role: its jobs' providers and their settings")]
    Controller,
    #[value(help = "What the controller's role adds to publish the public page")]
    Publish,
    #[value(help = "The role of the EC2 timer, which terminates instances")]
    Timer,
    #[value(help = "The EC2 provider's instances' role, which pulls their images")]
    Sandbox,
    #[value(help = "The role that Modal assumes to import the roles' images")]
    ModalPull,
    #[value(help = "What a machine whose configuration names `[controller]` needs")]
    Client,
}

const SETTINGS: Placeholder = Placeholder::new(
    "settings_parameter_arn",
    "the ARN of the SSM parameter of the controller's settings",
);

const BUCKET: Placeholder =
    Placeholder::new("public_bucket_arn", "the ARN of the public page's bucket");

const CONTROLLER: Placeholder = Placeholder::new(
    "controller_instance_arn",
    "the ARN of the controller's instance",
);

/// The calls of a job of its own.
pub(crate) struct Job {
    /// Its read of its settings.
    pub(crate) settings: Call,
}

pub(crate) const JOB: Job = Job {
    settings: Call::new(
        ["ssm", "get-parameter"],
        Allow::new(&["ssm:GetParameter"], &[Resource::Arn(&SETTINGS)]),
    ),
};

/// The calls of a publish.
pub(crate) struct Publish {
    pub(crate) sync: Call,
    pub(crate) copy: Call,
}

pub(crate) const PUBLISH: Publish = Publish {
    // A sync lists the bucket, to upload only what it lacks.
    sync: Call::new(
        ["s3", "sync"],
        Allow::new(&["s3:ListBucket"], &[Resource::Arn(&BUCKET)]),
    )
    .also(&[PUT_OBJECTS]),
    copy: Call::new(["s3", "cp"], PUT_OBJECTS),
};

const PUT_OBJECTS: Allow = Allow::new(&["s3:PutObject"], &[Resource::Below(&BUCKET, "*")]);

/// The calls of a client on its controller's instance.
pub(crate) struct Client {
    pub(crate) describe: Call,
    pub(crate) start: Call,
    pub(crate) push_key: Call,
}

pub(crate) const CLIENT: Client = Client {
    describe: Call::new(
        ["ec2", "describe-instances"],
        Allow::new(&["ec2:DescribeInstances"], &[Resource::Any]),
    ),
    start: Call::new(
        ["ec2", "start-instances"],
        Allow::new(&["ec2:StartInstances"], &[Resource::Arn(&CONTROLLER)]),
    ),
    push_key: Call::new(
        ["ec2-instance-connect", "send-ssh-public-key"],
        Allow::new(
            &["ec2-instance-connect:SendSSHPublicKey"],
            &[Resource::Arn(&CONTROLLER)],
        )
        .when(&[Condition::new(
            Test::StringEquals,
            Key::Named("ec2:osuser"),
            Values::Are(&[Part::Text(USER)]),
        )]),
    ),
};

/// What IAM must allow `principal`.
// A call that its crate declares but lists in no principal's statements compiles all the same,
// and IAM refuses it, which `cargo xtask live ec2` and `cargo xtask live modal` show as AWS's
// `AccessDenied` once a deployment applies these policies.
fn allows(principal: Principal) -> Vec<&'static Allow> {
    match principal {
        Principal::Controller => {
            let Job { settings } = &JOB;
            (ec2::iam::caller())
                .chain(modal::iam::caller())
                .chain(settings.allows())
                .collect()
        }
        Principal::Publish => {
            let Publish { sync, copy } = &PUBLISH;
            [sync, copy].into_iter().flat_map(Call::allows).collect()
        }
        Principal::Timer => ec2::iam::timer().collect(),
        Principal::Sandbox => ec2::iam::sandbox().collect(),
        Principal::ModalPull => modal::iam::puller().collect(),
        Principal::Client => {
            let Client {
                describe,
                start,
                push_key,
            } = &CLIENT;
            [describe, start, push_key]
                .into_iter()
                .flat_map(Call::allows)
                .collect()
        }
    }
}

/// `principal`'s policy, as IAM's JSON, with each value that only a deployment knows as
/// Terraform's `templatefile` takes it.
pub(crate) fn policy(principal: Principal) -> String {
    iam::policy(&allows(principal))
}

/// The help of `pumpkin controller policy`'s placeholders: what each stands for, and which each
/// principal's policy holds.
pub(crate) fn placeholders() -> String {
    let principals = Principal::value_variants().iter();
    let all: Vec<&Allow> = principals
        .clone()
        .flat_map(|principal| allows(*principal))
        .collect();
    let mut lines = vec![
        "Placeholders, which `templatefile` fills with a string where a policy holds".to_owned(),
        "${name}, and with a list where it holds ${jsonencode(name)}:".to_owned(),
    ];
    for (name, Fill { about, list }) in iam::placeholders(&all) {
        let about = if list {
            format!("a list of {about}")
        } else {
            about.to_owned()
        };
        lines.extend(wrapped(&format!("  {name}:"), about.split(' ')));
    }
    lines.push(String::new());
    lines.push("Each principal's placeholders:".to_owned());
    for principal in principals {
        let Some(name) = principal.to_possible_value() else {
            unreachable!("every principal is a value of the command line's");
        };
        let names: Vec<&str> = iam::placeholders(&allows(*principal)).into_keys().collect();
        let joined = names.join(", ");
        lines.extend(wrapped(
            &format!("  {}:", name.get_name()),
            joined.split(' '),
        ));
    }
    lines.join("\n")
}

/// `words` after `head`, a space between each two, in lines of 80 columns at most but for a
/// word longer, each line after the first indented by 4.
fn wrapped<'a>(head: &str, words: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut lines = vec![head.to_owned()];
    for word in words {
        let Some(line) = lines.last_mut() else {
            unreachable!("the lines start with the head's");
        };
        if line.len() + 1 + word.len() > 80 {
            lines.push(format!("    {word}"));
        } else {
            line.push(' ');
            line.push_str(word);
        }
    }
    lines
}
