# The team's controller, which builds the tasks' images and runs the drivers, and runs no role.
# Its members reach it over ssh from `ssh_from`, each with a key that `members` lists, and it
# stops itself when idle (`pumpkin controller install`), so it costs only its volume between runs.

resource "aws_iam_role" "controller" {
  name        = "${local.prefix}-controller"
  description = "Pumpkin's controller: run sandboxes in the envelope, and push their images"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}

resource "aws_iam_role_policy" "controller" {
  name = "run"
  role = aws_iam_role.controller.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      # A create's `run-instances`, from the envelope's templates alone, with its run's tags.
      {
        Effect    = "Allow"
        Action    = "ec2:RunInstances"
        Resource  = "*"
        Condition = { ArnEquals = { "ec2:LaunchTemplate" = [for template in aws_launch_template.class : template.arn] } }
      },
      {
        Effect    = "Allow"
        Action    = "ec2:CreateTags"
        Resource  = ["${local.ec2}:instance/*", "${local.ec2}:volume/*"]
        Condition = { StringEquals = { "ec2:CreateAction" = "RunInstances" } }
      },
      {
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = aws_iam_role.sandbox.arn
        Condition = { StringEquals = { "iam:PassedToService" = "ec2.amazonaws.com" } }
      },
      # A sweep's list and delete, and a sandbox's console.
      { Effect = "Allow", Action = "ec2:DescribeInstances", Resource = "*" },
      local.terminate_sandboxes,
      merge(local.terminate_sandboxes, { Action = "ec2:GetConsoleOutput" }),
      # Each instance's timer.
      {
        Effect   = "Allow"
        Action   = ["scheduler:CreateSchedule", "scheduler:DeleteSchedule"]
        Resource = "${local.arn}:scheduler:${var.region}:${local.account}:schedule/${aws_scheduler_schedule_group.timer.name}/*"
      },
      {
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = aws_iam_role.timer.arn
        Condition = { StringEquals = { "iam:PassedToService" = "scheduler.amazonaws.com" } }
      },
      # A publish: whether a repository holds an image, and its push.
      { Effect = "Allow", Action = "ecr:GetAuthorizationToken", Resource = "*" },
      {
        Effect = "Allow"
        Action = [
          "ecr:BatchCheckLayerAvailability",
          "ecr:BatchGetImage",
          "ecr:CompleteLayerUpload",
          "ecr:DescribeRepositories",
          "ecr:InitiateLayerUpload",
          "ecr:PutImage",
          "ecr:UploadLayerPart",
        ]
        Resource = [for repository in aws_ecr_repository.images : repository.arn]
      },
    ]
  })
}

resource "aws_iam_instance_profile" "controller" {
  name = "${local.prefix}-controller"
  role = aws_iam_role.controller.name
}

# Each job's settings, which `pumpkin controller job` reads as it starts, so an apply that
# changes them takes effect at the next job: the controller's Docker engine, the EC2 provider
# `aws`, and the `images` of the Modal provider `modal`, which a job drops if its client sends no
# `[controller.modal]`; and, with `publish_from_jobs`, where each job publishes the public page
# (public.tf). Nothing else is here: the rest is each job's own or its client's. The
# advanced tier holds 8 KB.
resource "aws_ssm_parameter" "settings" {
  name = "/${local.prefix}/settings"
  type = "String"
  tier = "Advanced"
  value = join("\n", concat(
    ["[docker]", "context = \"default\"", "", "[providers.aws.ec2]"],
    local.ec2_settings,
    ["", "[providers.modal.modal]", local.modal_images],
    local.publish_settings,
    [""],
  ))
}

resource "aws_iam_role_policy" "controller_settings" {
  name = "settings"
  role = aws_iam_role.controller.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "ssm:GetParameter", Resource = aws_ssm_parameter.settings.arn }]
  })
}

# The members' keys, a line `<member> <key>` for each key that `members` lists, which the
# controller copies every minute into the keys that its `sshd` admits, each with the forced
# command that names its member, as `pumpkin controller install` sets it up.
resource "aws_ssm_parameter" "members" {
  name  = "/${local.prefix}/members"
  type  = "String"
  tier  = "Advanced"
  value = join("", flatten([for member, keys in var.members : [for key in keys : "${member} ${key}\n"]]))
}

resource "aws_iam_role_policy" "controller_members" {
  name = "members"
  role = aws_iam_role.controller.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "ssm:GetParameter", Resource = aws_ssm_parameter.members.arn }]
  })
}

# Canonical's Ubuntu 24.04 LTS, as its public parameter names the current release, whose Docker
# comes from Docker's own repository, as builds need Docker 28 or later with the containerd
# image store, and Buildx. An apply never replaces the controller for a newer release.
data "aws_ssm_parameter" "ubuntu" {
  name = "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
}

resource "aws_instance" "controller" {
  ami                    = data.aws_ssm_parameter.ubuntu.insecure_value
  instance_type          = var.controller_instance_type
  subnet_id              = var.subnet_id
  vpc_security_group_ids = [aws_security_group.controller.id]
  iam_instance_profile   = aws_iam_instance_profile.controller.name
  # A public address: the way out to the settings parameter, ECR and Modal, and the one Modal's tunnels admit.
  associate_public_ip_address = true
  # A shutdown stops it, keeping its volume: the build cache, the trees and the records.
  instance_initiated_shutdown_behavior = "stop"
  user_data = templatefile("${path.module}/controller.yaml", {
    ubuntu_snapshot    = "https://snapshot.ubuntu.com/ubuntu/${local.controller_versions.ubuntu_snapshot}"
    docker_key         = file("${path.module}/docker.asc")
    docker_version     = local.controller_versions.docker
    containerd_version = local.controller_versions.containerd
    buildx_version     = local.controller_versions.buildx
    aws_cli_version    = local.controller_versions.aws_cli
    aws_cli_sha256     = local.controller_versions.aws_cli_sha256
    uv_version         = local.controller_versions.uv
    uv_sha256          = local.controller_versions.uv_sha256
    pumpkin_release    = var.pumpkin_release
    name               = var.name
    region             = var.region
  })

  # The first boot's install reads the members' keys.
  depends_on = [aws_iam_role_policy.controller_members]

  # Tokens required, within one hop, so that no build's container reaches the role.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_size = var.controller_volume_gib
    volume_type = "gp3"
    iops        = var.controller_volume_iops
    throughput  = var.controller_volume_throughput
    encrypted   = true
  }

  # The tag by which `pumpkin` finds the controller of `[controller] name`.
  tags = { Name = "${local.prefix}-controller", "pumpkin:controller" = var.name }

  # An apply never replaces or stops the controller under live runs: cloud-init runs the user
  # data at the instance's first boot alone.
  lifecycle {
    ignore_changes = [ami, user_data]
  }
}

# What the controller installs, at these versions: Docker's packages, as its repository names
# them; AWS CLI v2; uv; and the rest from Ubuntu's archive as it was at the snapshot.
locals {
  controller_versions = {
    ubuntu_snapshot = "20261101T000000Z"
    docker          = "5:29.9.2-1~ubuntu.24.04~noble"
    containerd      = "2.4.2-3~ubuntu.24.04~noble"
    buildx          = "0.38.1-1~ubuntu.24.04~noble"
    aws_cli         = "2.38.1"
    aws_cli_sha256  = "b11ed416d91f4251da363a17706938eeaf0e520745754c104c0c1890dd8363c7"
    uv              = "0.13.5"
    uv_sha256       = "f68d291c75783122dad58ab77def0e13f3233f358731db5d6d7e8715603b928f"
  }
}
