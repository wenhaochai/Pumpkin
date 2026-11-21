//! What IAM must allow of the provider's calls, and of the role that Modal assumes.
//!
//! The provider's calls are made with the credentials of the process that runs it ([`caller`]);
//! Modal assumes the settings' pull role to import an image ([`puller`]).

use aws::iam::{Allow, Ecr, Placeholder, Resource};

const IMAGES: Placeholder = Placeholder::new(
    "modal_repository_arn",
    "the ARN of the repository that Modal imports images from",
);

const REPOSITORY: &[Resource] = &[Resource::Arn(&IMAGES)];

/// The calls on the repository of the settings' `images`.
pub(crate) const ECR: Ecr = Ecr::on(REPOSITORY);

/// What IAM must allow the credentials of a process that runs the provider.
pub fn caller() -> impl Iterator<Item = &'static Allow> {
    ECR.allows()
}

/// What IAM must allow the role of the settings' `images`, which Modal assumes, by its own
/// identity, to import an image from the repository.
// Among the reads that Modal's guide lists, those that an import makes:
// https://modal.com/docs/guide/private-registries
pub fn puller() -> impl Iterator<Item = &'static Allow> {
    static PULLS: [Allow; 2] = [
        Allow::new(&["ecr:GetAuthorizationToken"], &[Resource::Any]),
        Allow::new(
            &[
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:GetDownloadUrlForLayer",
            ],
            REPOSITORY,
        ),
    ];
    PULLS.iter()
}
