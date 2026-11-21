variable "name" {
  description = "The team's name, which prefixes every resource's name, such as `pumpkin-<name>-sandbox`."
  type        = string

  validation {
    condition     = can(regex("^[a-z]([a-z0-9-]{0,14}[a-z0-9])?$", var.name))
    error_message = "The name is not 1 to 16 of a-z, 0-9 and `-`, starting with a letter and ending with no `-`: give one such as `alice`."
  }
}

variable "region" {
  description = "The region of everything the module makes, such as `us-west-1`."
  type        = string
}

variable "vpc_id" {
  description = "The VPC of the sandboxes and of the controller that runs them."
  type        = string
}

variable "subnet_id" {
  description = "A public subnet of that VPC, which gives each instance launched in it a public IPv4 address: sandboxes pull from ECR through its internet gateway."
  type        = string
}

variable "modal_workspace" {
  description = "The ID of the team's Modal workspace, `ac-…`, whose tokens alone may assume the Modal pull role."
  type        = string

  validation {
    condition     = can(regex("^ac-[A-Za-z0-9]+$", var.modal_workspace))
    error_message = "The Modal workspace ID is not one: give one such as `ac-zrUhjAwtDAP4iR3aWkCHl2`."
  }
}

# The classes of sandbox: the instance type of each, and the CPU flags and model it offers a role,
# as `/proc/cpuinfo` names them. The flags are the x86-64 levels the instance's CPU meets (v2:
# cx16 to ssse3, v3: abm to xsave, v4: the avx512 ones), which the tasks' needs name. A class's
# `usd_per_hour` is what its instance type costs an hour in the region, as AWS's pricing page
# gives it.
variable "classes" {
  description = "The classes of sandbox, by name: each one's instance type, CPU flags and, for roles that require it alone, CPU model; and, if given, what an instance costs an hour, in US dollars, its instances' root volume, in GiB, which is `root_volume_gib` without it, and the most seconds an instance's start may take, its images' pulls among them, which is 1740 without it."
  type = map(object({
    instance_type   = string
    cpu_features    = set(string)
    cpu_model       = optional(string)
    usd_per_hour    = optional(number)
    root_volume_gib = optional(number)
    startup_seconds = optional(number)
  }))
  default = {
    # Targets of up to 8 CPUs (AMD EPYC 7R13, Zen 3).
    judge = {
      instance_type = "c6a.2xlarge"
      cpu_features  = ["abm", "avx", "avx2", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
    }
    # Agents and targets of up to 8 CPUs that need more memory than `judge` leaves them (Zen 3).
    memory = {
      instance_type = "r6a.2xlarge"
      cpu_features  = ["abm", "avx", "avx2", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
    }
    # Targets of 60 CPUs, and roles that need AVX-512 but no CPU model (AMD EPYC 9R14, Zen 4).
    judge_60 = {
      instance_type = "c7a.16xlarge"
      cpu_features  = ["abm", "avx", "avx2", "avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512vl", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
    }
    # Roles sealed to the Xeon Platinum 8375C (Ice Lake), which no other role is placed in.
    ice_lake = {
      instance_type = "m6i.xlarge"
      cpu_features  = ["abm", "avx", "avx2", "avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512vl", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
      cpu_model     = "Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz"
    }
  }
}

variable "root_volume_gib" {
  description = "The root volume of a sandbox's instance, in GiB, for a class that gives none, which holds its images and its workspace or scratch: 100 fits the GPU tasks' reference agents' 45 GB images."
  type        = number
  default     = 100
}

variable "controller_instance_type" {
  description = "The controller's instance type, which builds the tasks' images and runs the drivers."
  type        = string
  default     = "m7i.4xlarge"
}

variable "controller_volume_gib" {
  description = "The controller's root volume, in GiB, which holds the build cache and the images of recent tasks, at 20 to 70 GB a task, which each job prunes as the volume fills, the trees and the records."
  type        = number
  default     = 1000
}

variable "controller_volume_iops" {
  description = "The controller's root volume's IOPS, a gp3's provisioned I/O operations a second, which the builds of several tasks at once, each unpacking and writing its layers, and the prunes share: gp3's baseline is 3000."
  type        = number
  default     = 6000
}

variable "controller_volume_throughput" {
  description = "The controller's root volume's throughput, in MiB/s, a gp3's provisioned throughput, which the builds' layers and the pushes of their images read and write: gp3's baseline is 125."
  type        = number
  default     = 500
}

variable "pumpkin_release" {
  description = "The line of a Pumpkin release's `pumpkinversion` asset, its tag, commit and archive's SHA-256, whose `pumpkin controller install` the controller runs at its first boot; a later release's reaches a running controller only as its admin runs it there."
  type        = string

  validation {
    condition     = can(regex("^[a-zA-Z0-9][a-zA-Z0-9._-]* [0-9a-f]{40} [0-9a-f]{64}$", var.pumpkin_release))
    error_message = "The release is not a `pumpkinversion` line, `<tag> <commit> <sha256>`: give that asset's line, such as a task repository's `.pumpkinversion` holds."
  }
}

variable "ssh_from" {
  description = "The IPv4 CIDRs that may reach the controller's `sshd`, on port 22, such as each member's machine's private address as a `/32`: `pumpkin` reaches the controller at its private address, from the controller's VPC or a network routed to it. None by default, so no `pumpkin` reaches the controller."
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for cidr in var.ssh_from : can(cidrnetmask(cidr))])
    error_message = "An entry of `ssh_from` is not an IPv4 CIDR: give one such as `10.0.1.23/32`."
  }
}

variable "members" {
  description = "The team's members, by name, each with the `ssh-ed25519` lines of their machines' `~/.config/pumpkin/ssh/key.pub`: the controller admits only these keys, and names each key's jobs by its member."
  type        = map(list(string))

  validation {
    condition     = length(var.members) > 0
    error_message = "`members` lists no one, so no key reaches the controller: list each member's key, such as `{ alice = [\"ssh-ed25519 AAAA… alice-laptop\"] }`."
  }

  validation {
    condition     = alltrue([for keys in values(var.members) : length(keys) > 0])
    error_message = "A member lists no key, so they can't reach the controller: list the line of each of their machines' `~/.config/pumpkin/ssh/key.pub`, or leave them out."
  }
}

variable "client_roles" {
  description = "The names of the IAM roles of the members' machines, each of which gets the client policy; leave empty when the roles live elsewhere and an administrator attaches `client_policy_arn` by hand."
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for r in var.client_roles : can(regex("^[A-Za-z0-9+=,.@_-]{1,64}$", r))])
    error_message = "A client role is not an IAM role name (1 to 64 of A-Z, a-z, 0-9 and `+=,.@_-`, with no path): give one such as `alice-laptop`."
  }
}

variable "client_users" {
  description = "The names of the IAM users of the members' machines, each of which gets the client policy; leave empty when the users live elsewhere and an administrator attaches `client_policy_arn` by hand."
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for u in var.client_users : can(regex("^[A-Za-z0-9+=,.@_-]{1,64}$", u))])
    error_message = "A client user is not an IAM user name (1 to 64 of A-Z, a-z, 0-9 and `+=,.@_-`, with no path): give one such as `alice`."
  }
}

variable "public_page" {
  description = "Whether to make the public page: a bucket, the CloudFront distribution that serves it, and the controller's right to upload to it."
  type        = bool
  default     = false
}

variable "publish_from_jobs" {
  description = "Whether the controller's settings name the public page, so that each job publishes the runs to it; requires `public_page`."
  type        = bool
  default     = false

  validation {
    condition     = !var.publish_from_jobs || var.public_page
    error_message = "`publish_from_jobs` names the public page, which `public_page = false` does not make: set `public_page = true` too."
  }
}

variable "publish_since" {
  description = "If not empty, the `since` of the `[publish]` table, as RFC 3339 writes an instant with its offset, such as `2026-11-15T00:00:00Z`; requires `publish_from_jobs`."
  type        = string
  default     = ""

  validation {
    condition     = var.publish_since == "" || can(formatdate("YYYY", var.publish_since))
    error_message = "`publish_since` is no RFC 3339 date-time with an offset: give one such as `2026-11-15T00:00:00Z`."
  }

  validation {
    condition     = var.publish_since == "" || var.publish_from_jobs
    error_message = "`publish_since` sets the `[publish]` table's `since`, which `publish_from_jobs = false` does not write: set `publish_from_jobs = true` too, or leave `publish_since` empty."
  }
}
