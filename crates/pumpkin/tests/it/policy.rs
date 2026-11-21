//! `controller policy`: each principal's IAM policy, as the calls of this build's code on AWS
//! state it, which a deployment's review reads.

use expect_test::{Expect, expect};

use super::Scratch;

/// Compares what `pumpkin controller policy <arg>` prints with `expect`; it says nothing else,
/// and succeeds.
fn check(arg: &str, expect: &Expect) {
    let scratch = Scratch::new();
    let output = (scratch.pumpkin(&["controller", "policy", arg]))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    expect.assert_eq(&String::from_utf8(output.stdout).unwrap());
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn controller() {
    check(
        "controller",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:CompleteLayerUpload",
                "ecr:DescribeRepositories",
                "ecr:InitiateLayerUpload",
                "ecr:PutImage",
                "ecr:UploadLayerPart"
              ],
              "Effect": "Allow",
              "Resource": [
                "${modal_repository_arn}"
              ]
            },
            {
              "Action": [
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:CompleteLayerUpload",
                "ecr:DescribeRepositories",
                "ecr:InitiateLayerUpload",
                "ecr:PutImage",
                "ecr:UploadLayerPart"
              ],
              "Effect": "Allow",
              "Resource": [
                "${roles_repository_arn}",
                "${sandboxd_repository_arn}"
              ]
            },
            {
              "Action": [
                "iam:PassRole"
              ],
              "Condition": {
                "StringEquals": {
                  "iam:PassedToService": [
                    "ec2.amazonaws.com"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "${sandbox_role_arn}"
              ]
            },
            {
              "Action": [
                "ssm:GetParameter"
              ],
              "Effect": "Allow",
              "Resource": [
                "${settings_parameter_arn}"
              ]
            },
            {
              "Action": [
                "iam:PassRole"
              ],
              "Condition": {
                "StringEquals": {
                  "iam:PassedToService": [
                    "scheduler.amazonaws.com"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "${timer_role_arn}"
              ]
            },
            {
              "Action": [
                "ec2:RunInstances"
              ],
              "Condition": {
                "ArnEquals": {
                  "ec2:LaunchTemplate": ${jsonencode(launch_template_arns)}
                },
                "Bool": {
                  "ec2:IsLaunchTemplateResource": [
                    "true"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            },
            {
              "Action": [
                "ec2:DescribeInstances",
                "ecr:GetAuthorizationToken"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            },
            {
              "Action": [
                "ec2:GetConsoleOutput",
                "ec2:TerminateInstances"
              ],
              "Condition": {
                "ArnEquals": {
                  "ec2:InstanceProfile": [
                    "${sandbox_profile_arn}"
                  ]
                },
                "Null": {
                  "ec2:ResourceTag/pumpkin:run": [
                    "false"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:ec2:${region}:${account}:instance/*"
              ]
            },
            {
              "Action": [
                "ec2:CreateTags"
              ],
              "Condition": {
                "StringEquals": {
                  "ec2:CreateAction": [
                    "RunInstances"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:ec2:${region}:${account}:instance/*",
                "arn:${partition}:ec2:${region}:${account}:volume/*"
              ]
            },
            {
              "Action": [
                "scheduler:CreateSchedule",
                "scheduler:DeleteSchedule"
              ],
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:scheduler:${region}:${account}:schedule/${timer_group}/*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn publish() {
    check(
        "publish",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "s3:ListBucket"
              ],
              "Effect": "Allow",
              "Resource": [
                "${public_bucket_arn}"
              ]
            },
            {
              "Action": [
                "s3:PutObject"
              ],
              "Effect": "Allow",
              "Resource": [
                "${public_bucket_arn}/*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn timer() {
    check(
        "timer",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ec2:TerminateInstances"
              ],
              "Condition": {
                "ArnEquals": {
                  "ec2:InstanceProfile": [
                    "${sandbox_profile_arn}"
                  ]
                },
                "Null": {
                  "ec2:ResourceTag/pumpkin:run": [
                    "false"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "arn:${partition}:ec2:${region}:${account}:instance/*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn sandbox() {
    check(
        "sandbox",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ecr:BatchGetImage",
                "ecr:GetDownloadUrlForLayer"
              ],
              "Effect": "Allow",
              "Resource": [
                "${roles_repository_arn}",
                "${sandboxd_repository_arn}"
              ]
            },
            {
              "Action": [
                "ecr:GetAuthorizationToken"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn modal_pull() {
    check(
        "modal-pull",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ecr:BatchCheckLayerAvailability",
                "ecr:BatchGetImage",
                "ecr:GetDownloadUrlForLayer"
              ],
              "Effect": "Allow",
              "Resource": [
                "${modal_repository_arn}"
              ]
            },
            {
              "Action": [
                "ecr:GetAuthorizationToken"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn client() {
    check(
        "client",
        &expect![[r#"
        {
          "Statement": [
            {
              "Action": [
                "ec2-instance-connect:SendSSHPublicKey"
              ],
              "Condition": {
                "StringEquals": {
                  "ec2:osuser": [
                    "pumpkin"
                  ]
                }
              },
              "Effect": "Allow",
              "Resource": [
                "${controller_instance_arn}"
              ]
            },
            {
              "Action": [
                "ec2:StartInstances"
              ],
              "Effect": "Allow",
              "Resource": [
                "${controller_instance_arn}"
              ]
            },
            {
              "Action": [
                "ec2:DescribeInstances"
              ],
              "Effect": "Allow",
              "Resource": [
                "*"
              ]
            }
          ],
          "Version": "2012-10-17"
        }
    "#]],
    );
}

#[test]
fn placeholders() {
    check(
        "--help",
        &expect![[r#"
            Print the IAM policy that a principal of the controller's deployment needs, as
            IAM's JSON: each statement that the calls of this build's code on AWS need, as
            that principal, and nothing else. A value that only the deployment knows, such
            as its account, is a placeholder that Terraform's `templatefile` fills.

            Usage: pumpkin controller policy <principal>

            Arguments:
              <principal>
                      The principal whose policy to print.

                      Possible values:
                      - controller: The controller's role: its jobs' providers and their settings
                      - publish:    What the controller's role adds to publish the public page
                      - timer:      The role of the EC2 timer, which terminates instances
                      - sandbox:    The EC2 provider's instances' role, which pulls their images
                      - modal-pull: The role that Modal assumes to import the roles' images
                      - client:     What a machine whose configuration names `[controller]` needs

            Options:
              -h, --help
                      Print this help.

            Placeholders, which `templatefile` fills with a string where a policy holds
            ${name}, and with a list where it holds ${jsonencode(name)}:
              account: the ID of the deployment's account
              controller_instance_arn: the ARN of the controller's instance
              launch_template_arns: a list of the ARNs of the envelope's launch templates
              modal_repository_arn: the ARN of the repository that Modal imports images from
              partition: the AWS partition, such as `aws`
              public_bucket_arn: the ARN of the public page's bucket
              region: the AWS region of the deployment
              roles_repository_arn: the ARN of the roles' images' repository
              sandbox_profile_arn: the ARN of the sandbox instances' instance profile
              sandbox_role_arn: the ARN of the sandbox instances' role
              sandboxd_repository_arn: the ARN of the repository of the images of `sandboxd`
              settings_parameter_arn: the ARN of the SSM parameter of the controller's
                settings
              timer_group: the name of the timer's schedule group
              timer_role_arn: the ARN of the timer's role

            Each principal's placeholders:
              controller: account, launch_template_arns, modal_repository_arn, partition,
                region, roles_repository_arn, sandbox_profile_arn, sandbox_role_arn,
                sandboxd_repository_arn, settings_parameter_arn, timer_group, timer_role_arn
              publish: public_bucket_arn
              timer: account, partition, region, sandbox_profile_arn
              sandbox: roles_repository_arn, sandboxd_repository_arn
              modal-pull: modal_repository_arn
              client: controller_instance_arn
        "#]],
    );
}
