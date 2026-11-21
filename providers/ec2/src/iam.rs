//! What IAM must allow of the provider's calls, and of the envelope's other principals.
//!
//! The provider's calls are made with the credentials of the process that runs it ([`caller`]);
//! its sandboxes rely on the timer's role ([`timer`]) and the sandbox instances' ([`sandbox`]).

use aws::iam::{Allow, Call, Condition, Ecr, Key, List, Part, Placeholder, Resource, Test, Values};

use crate::launch::RUN_TAG;

/// The envelope's launch templates, the only ones a create launches from.
const LAUNCH_TEMPLATES: List = List::new(
    "launch_template_arns",
    "the ARNs of the envelope's launch templates",
);

/// The role of the instance profile of the envelope's launch templates.
const SANDBOX_ROLE: Placeholder =
    Placeholder::new("sandbox_role_arn", "the ARN of the sandbox instances' role");

/// The instance profile of the envelope's launch templates.
const SANDBOX_PROFILE: Placeholder = Placeholder::new(
    "sandbox_profile_arn",
    "the ARN of the sandbox instances' instance profile",
);

const TIMER_ROLE: Placeholder = Placeholder::new("timer_role_arn", "the ARN of the timer's role");

const TIMER_GROUP: Placeholder =
    Placeholder::new("timer_group", "the name of the timer's schedule group");

const ROLES_REPOSITORY: Placeholder = Placeholder::new(
    "roles_repository_arn",
    "the ARN of the roles' images' repository",
);

const SANDBOXD_REPOSITORY: Placeholder = Placeholder::new(
    "sandboxd_repository_arn",
    "the ARN of the repository of the images of `sandboxd`",
);

const REPOSITORIES: &[Resource] = &[
    Resource::Arn(&ROLES_REPOSITORY),
    Resource::Arn(&SANDBOXD_REPOSITORY),
];

const INSTANCES: &[Resource] = &[Resource::In("ec2", &[Part::Text("instance/*")])];

/// The provider's own instances: each a run launched, with its run's tag, from the envelope's
/// templates, whose instance profile is the sandboxes'.
const SANDBOXES: &[Condition] = &[
    Condition::new(
        Test::Null,
        Key::Tag("ec2", RUN_TAG),
        Values::Are(&[Part::Text("false")]),
    ),
    Condition::new(
        Test::ArnEquals,
        Key::Named("ec2:InstanceProfile"),
        Values::Are(&[Part::Fill(&SANDBOX_PROFILE)]),
    ),
];

const TERMINATE: Allow = Allow::new(&["ec2:TerminateInstances"], INSTANCES).when(SANDBOXES);

/// The calls on the envelope's repositories.
pub(crate) const ECR: Ecr = Ecr::on(REPOSITORIES);

/// The provider's calls on AWS but those on ECR ([`ECR`]).
pub(crate) struct Calls {
    pub(crate) identity: Call,
    pub(crate) launch: Call,
    pub(crate) set_timer: Call,
    pub(crate) unset_timer: Call,
    pub(crate) describe: Call,
    pub(crate) terminate: Call,
    pub(crate) console: Call,
}

pub(crate) const CALLS: Calls = Calls {
    identity: Call::open(["sts", "get-caller-identity"]),
    // From the envelope's templates alone, with none of the resources they name overridden:
    // https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ExamplePolicies_EC2.html#iam-example-runinstances-launch-templates
    launch: Call::new(
        ["ec2", "run-instances"],
        Allow::new(&["ec2:RunInstances"], &[Resource::Any]).when(&[
            Condition::new(
                Test::ArnEquals,
                Key::Named("ec2:LaunchTemplate"),
                Values::Listed(&LAUNCH_TEMPLATES),
            ),
            Condition::new(
                Test::Bool,
                Key::Named("ec2:IsLaunchTemplateResource"),
                Values::Are(&[Part::Text("true")]),
            ),
        ]),
    )
    .also(&[
        // The launch's tags, on the instance and its volumes (`launch::tag_specifications`).
        Allow::new(
            &["ec2:CreateTags"],
            &[
                Resource::In("ec2", &[Part::Text("instance/*")]),
                Resource::In("ec2", &[Part::Text("volume/*")]),
            ],
        )
        .when(&[Condition::new(
            Test::StringEquals,
            Key::Named("ec2:CreateAction"),
            Values::Are(&[Part::Text("RunInstances")]),
        )]),
        // The templates' instance profile's role.
        Allow::new(&["iam:PassRole"], &[Resource::Arn(&SANDBOX_ROLE)]).when(&[Condition::new(
            Test::StringEquals,
            Key::Named("iam:PassedToService"),
            Values::Are(&[Part::Text("ec2.amazonaws.com")]),
        )]),
    ]),
    set_timer: Call::new(
        ["scheduler", "create-schedule"],
        Allow::new(&["scheduler:CreateSchedule"], TIMER_SCHEDULES),
    )
    // The target's role, which the schedule names.
    .also(&[
        Allow::new(&["iam:PassRole"], &[Resource::Arn(&TIMER_ROLE)]).when(&[Condition::new(
            Test::StringEquals,
            Key::Named("iam:PassedToService"),
            Values::Are(&[Part::Text("scheduler.amazonaws.com")]),
        )]),
    ]),
    unset_timer: Call::new(
        ["scheduler", "delete-schedule"],
        Allow::new(&["scheduler:DeleteSchedule"], TIMER_SCHEDULES),
    ),
    describe: Call::new(
        ["ec2", "describe-instances"],
        Allow::new(&["ec2:DescribeInstances"], &[Resource::Any]),
    ),
    terminate: Call::new(["ec2", "terminate-instances"], TERMINATE),
    console: Call::new(
        ["ec2", "get-console-output"],
        Allow::new(&["ec2:GetConsoleOutput"], INSTANCES).when(SANDBOXES),
    ),
};

const TIMER_SCHEDULES: &[Resource] = &[Resource::In(
    "scheduler",
    &[
        Part::Text("schedule/"),
        Part::Fill(&TIMER_GROUP),
        Part::Text("/*"),
    ],
)];

/// What IAM must allow the credentials of a process that runs the provider.
pub fn caller() -> impl Iterator<Item = &'static Allow> {
    let Calls {
        identity,
        launch,
        set_timer,
        unset_timer,
        describe,
        terminate,
        console,
    } = &CALLS;
    let calls = [
        identity,
        launch,
        set_timer,
        unset_timer,
        describe,
        terminate,
        console,
    ];
    calls.into_iter().flat_map(Call::allows).chain(ECR.allows())
}

/// What IAM must allow the timer's role, with which each schedule's target terminates its
/// instance (`launch::Ec2::set_timer`).
pub fn timer() -> impl Iterator<Item = &'static Allow> {
    [&TERMINATE].into_iter()
}

/// What IAM must allow the sandbox instances' role, with which ECR's credential helper, as the
/// instance's user data configures it, gets a token for each pull of the role's image and of
/// `sandboxd`'s.
// AWS's `AmazonEC2ContainerRegistryPullOnly` but `ecr:BatchImportUpstreamImage`, which only a
// pull-through cache needs:
// https://docs.aws.amazon.com/aws-managed-policy/latest/reference/AmazonEC2ContainerRegistryPullOnly.html
pub fn sandbox() -> impl Iterator<Item = &'static Allow> {
    static PULLS: [Allow; 2] = [
        Allow::new(&["ecr:GetAuthorizationToken"], &[Resource::Any]),
        Allow::new(
            &["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"],
            REPOSITORIES,
        ),
    ];
    PULLS.iter()
}
