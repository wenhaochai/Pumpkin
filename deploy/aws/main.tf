# One team's Pumpkin on AWS. README.md says how to apply it, and what each output feeds.

data "aws_partition" "current" {}

data "aws_caller_identity" "current" {}

locals {
  prefix  = "pumpkin-${var.name}"
  account = data.aws_caller_identity.current.account_id
  arn     = "arn:${data.aws_partition.current.partition}"
  ec2     = "${local.arn}:ec2:${var.region}:${local.account}"
}
