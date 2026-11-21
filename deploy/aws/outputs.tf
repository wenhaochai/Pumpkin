# Pumpkin's settings as TOML lines of dotted keys, which hold under any table's header. TOML's
# basic strings and arrays take JSON's, so `jsonencode` writes each value.
locals {
  modal_images   = "images = { region = ${jsonencode(var.region)}, repository = ${jsonencode(aws_ecr_repository.images["roles"].name)}, role_arn = ${jsonencode(aws_iam_role.modal_pull.arn)} }"
  controller_arn = "${local.ec2}:instance/${aws_instance.controller.id}"
  ec2_settings = concat(
    [
      "region = ${jsonencode(var.region)}",
      "repositories = { roles = ${jsonencode(aws_ecr_repository.images["roles"].name)}, sandboxd = ${jsonencode(aws_ecr_repository.images["sandboxd"].name)} }",
      "timer = { group = ${jsonencode(aws_scheduler_schedule_group.timer.name)}, role_arn = ${jsonencode(aws_iam_role.timer.arn)} }",
    ],
    [for class, template in aws_launch_template.class : join("", [
      "classes.${class} = { template = ${jsonencode(template.id)}, version = ${template.latest_version}",
      ", vcpus = ${data.aws_ec2_instance_type.class[class].default_vcpus}",
      ", memory_mib = ${data.aws_ec2_instance_type.class[class].memory_size}",
      ", cpu_features = ${jsonencode(sort(var.classes[class].cpu_features))}",
      var.classes[class].cpu_model == null ? "" : ", cpu_model = ${jsonencode(var.classes[class].cpu_model)}",
      var.classes[class].usd_per_hour == null ? "" : ", usd_per_hour = ${jsonencode(var.classes[class].usd_per_hour)}",
      var.classes[class].startup_seconds == null ? "" : ", startup_seconds = ${jsonencode(var.classes[class].startup_seconds)}",
      " }",
    ])],
  )
}

output "ec2" {
  description = "The EC2 provider's settings, the body of a `[providers.<name>.ec2]` table."
  value       = join("\n", local.ec2_settings)
}

output "modal_images" {
  description = "Where the Modal provider imports images from, its settings' `images` key."
  value       = local.modal_images
}

output "client_config" {
  description = "The `[controller]` table of each member's machine's `pumpkin.toml`, beside its `[upstreams]`."
  value       = "[controller]\nname = ${jsonencode(var.name)}\nregion = ${jsonencode(var.region)}\n"
}

# What a member's machine needs to reach the controller: find it by its tag and start it, then
# reach its `sshd` from `ssh_from` with a key that `members` lists. It pushes no key: the admin's
# list is the only way in.
resource "aws_iam_policy" "client" {
  name        = "${local.prefix}-client"
  description = "What the machine of a person needs to reach its Pumpkin controller"
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = "ec2:DescribeInstances", Resource = "*" },
      { Effect = "Allow", Action = "ec2:StartInstances", Resource = local.controller_arn },
      # A live check's look for a run's timers left behind: Scheduler's list takes no resource.
      { Effect = "Allow", Action = "scheduler:ListSchedules", Resource = "*" },
    ]
  })
}

resource "aws_iam_role_policy_attachment" "client" {
  for_each   = toset(var.client_roles)
  role       = each.value
  policy_arn = aws_iam_policy.client.arn
}

resource "aws_iam_user_policy_attachment" "client" {
  for_each   = toset(var.client_users)
  user       = each.value
  policy_arn = aws_iam_policy.client.arn
}

output "client_policy_arn" {
  description = "The managed IAM policy that each member's machine needs to reach the controller, which the apply attaches to `client_roles` and `client_users`, and an account whose roles live elsewhere attaches by hand."
  value       = aws_iam_policy.client.arn
}

output "public_page" {
  description = "The public page's address, if `public_page`."
  value       = one([for distribution in aws_cloudfront_distribution.public : "https://${distribution.domain_name}/"])
}

output "public_bucket" {
  description = "The bucket that holds the public page's files, if `public_page`."
  value       = one(aws_s3_bucket.public[*].bucket)
}
