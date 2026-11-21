# deploy/aws

The Terraform module that makes one team's Pumpkin on AWS, whose members its admin lists by key,
and which the admin applies with administrator credentials. Every name it makes begins
`pumpkin-<name>`, the team's name, so teams share an account without colliding, and its state is
in an S3 bucket of the account, so no laptop can lose it.

| What | Why |
| --- | --- |
| the sandboxes' security group: out to HTTPS and to the controller's ephemeral ports; nothing in | sandboxes pull from ECR and dial the gateway at port 0's pick, and reach nothing else |
| the controller's security group: in from the sandboxes on the ephemeral ports, and from `ssh_from`, none by default, on port 22; out to HTTPS | the gateway, and `sshd` for the members' machines alone |
| ECR `pumpkin-<name>/roles` and `/sandboxd`, immutable tags, untagged images expire after 14 days | private images, pushed tagged by digest |
| a launch template per class: the AMI, the type, IMDSv2 within one hop, a root volume deleted with the instance | the EC2 classes, and what the `ec2` provider relies on |
| the sandboxes' role: ECR pulls only | each sandbox's pulls |
| the timer: a schedule group and a role that terminates run instances | a kill bound outside each guest |
| the Modal pull role: ECR pulls from `roles`, assumed only by `modal_workspace`'s tokens | Modal imports the images of calls' targets, which hold hidden data |
| an AMI baked by EC2 Image Builder from a named AL2023 release, with Docker, ECR's credential helper and nftables | each sandbox's boot runs them, at the release's versions |
| the controller's role and instance profile: access to its jobs' settings parameter and the EC2 provider's resources | the runs' launches, timers, sweeps and pushes, from the controller |
| the controller: an on-demand instance on Ubuntu 24.04 LTS, with a public IPv4 address, IMDSv2 within one hop, an encrypted gp3 root, stopping on shutdown, tagged `pumpkin:controller=<name>`; an apply never replaces it | builds the tasks' images and runs the drivers, which a person's machine sends it, and runs no role |
| what cloud-init installs there: Docker, which runs as root, so its `docker` group, which holds `pumpkin` alone, is root-equivalent; Buildx, AWS CLI v2 and uv at exact versions, and the rest of `controller.yaml`'s packages from a snapshot of Ubuntu's archive; then the Pumpkin release `pumpkin_release`, whose `pumpkin controller install` installs Pumpkin's part, which `pumpkin controller install --help` says: the user `pumpkin`, lingering; an `sshd` that admits concurrent openings and ends a session whose client has gone; a unit that stops the controller after 30 minutes with no job and no SSH session, and one that runs each registered build's reap at boot | builds with the containerd image store; jobs that outlive every connection; a stop and a reap that need no client |
| the SSM parameter `/pumpkin-<name>/settings` (advanced tier): the Docker engine, the EC2 provider `aws`, and the Modal provider `modal`'s `images` | each job's settings, which an apply changes for the next job; the rest of the Modal provider is the client's |
| the SSM parameter `/pumpkin-<name>/members` (advanced tier): a line `<member> <key>` for each key in `members`, which the controller copies every minute into `/etc/ssh/pumpkin/members`, each with a forced command that names its member, the only keys `sshd` admits for `pumpkin` | who may reach the controller, and whose each job is; an apply changes it within a minute |
| with `public_page`: a private bucket `pumpkin-<name>-public-…` that a CloudFront distribution alone reads, and the controller's right to list it and upload to it | the public page, below |

`ami.tf`, `controller.tf` and the other files say more of each. What the `ec2` provider requires
of its envelope is that crate's module doc, "The envelope".

## Once per account

The state bucket, versioned so a bad write can be undone, and, if `aws iam
list-open-id-connect-providers` lists no `oidc.modal.com`, Modal's OIDC provider:

```sh
aws s3api create-bucket --bucket pumpkin-tfstate-<account>-<region> --region <region> \
    --create-bucket-configuration LocationConstraint=<region>
aws s3api put-bucket-versioning --bucket pumpkin-tfstate-<account>-<region> \
    --versioning-configuration Status=Enabled
aws iam create-open-id-connect-provider --url https://oidc.modal.com --client-id-list oidc.modal.com
```

## Per team

Terraform 1.11 or later, and the AWS CLI with administrator credentials for the account. In this
directory, write `terraform.tfvars` (`variables.tf` says each value and its default):

```hcl
name            = "frontier"
region          = "us-west-1"
vpc_id          = "vpc-…"
subnet_id       = "subnet-…"
modal_workspace = "ac-…"
client_roles    = ["alice-laptop"] # the IAM roles and users of the members' machines
client_users    = []
ssh_from        = ["10.0.1.23/32", "10.0.1.24/32"] # the members' machines' private addresses
members = {
  alice = ["ssh-ed25519 AAAA… alice-laptop", "ssh-ed25519 AAAA… alice-desk"]
  bob   = ["ssh-ed25519 AAAA… bob"]
}
pumpkin_release = "v… <commit> <sha256>" # a release's `pumpkinversion` line
```

`pumpkin` reaches the controller at its private address, so each member's machine is in the
controller's VPC, or routed to it, and `ssh_from` admits it. A team of one lists its own key.

Then:

```sh
terraform init -backend-config=bucket=pumpkin-tfstate-<account>-<region> \
    -backend-config=key=<name>/terraform.tfstate -backend-config=region=<region>
terraform plan -out=pumpkin.tfplan
terraform apply pumpkin.tfplan
```

The first apply waits while Image Builder bakes the AMI on a build instance of its own. A
concurrent `plan` or `apply` of the same state waits on its lock.

## Members

The controller admits a key only if `members` lists it, and names each job by the member whose key
started it: `ls` and `view` show each run's member, and `cancel` stops only the member's own runs,
and those that no member started, unless given `--any-member`. This prevents accidents, not
members: each member's build runs as the controller's one user, `pumpkin`, whose `docker` group is
root there.

- **Enrol.** A machine's first `pumpkin` command makes its key, and the controller refuses it with
  the line to send the admin, its `~/.config/pumpkin/ssh/key.pub`, which holds no secret. Add the
  line under the member's name, a list item for each of their machines, and apply. The controller
  refuses a list with a name that isn't 1 to 32 of a-z, 0-9 and `-` starting with a letter, a
  line that isn't an `ssh-ed25519` key with an optional comment, or a key listed twice, keeping
  the last list it took, and its journal says why: `journalctl -u pumpkin-members.service`.
- **Revoke.** Remove the line and apply. The controller refuses the key within a minute. A
  connection already open lasts until it ends: `serve` ends a link that asks nothing for a minute,
  but an open `view` page asks every few seconds until it is closed.
- **See the logins.** `sudo journalctl -u ssh --grep 'Accepted publickey for pumpkin'` on the
  controller lists each login with its key's fingerprint, which `ssh-keygen -l -f
  /etc/ssh/pumpkin/members` names by member.

A controller made before `members` gets the parameter and the controller role's read of it from
the apply, but nothing on the controller: its admin runs, once the apply has made
`/pumpkin-<name>/members`, the install of a release that knows members, as "Changes" says. It
copies the list first, and fails, changing nothing in `sshd`, if it can't; it then admits only
those keys. Check, as the install leaves it, that `sudo ssh-keygen -l -f /etc/ssh/pumpkin/members`
names each member's keys, and that `pumpkin ls` reaches it from a member's machine. The keys that
members enrolled before, in `~pumpkin/.ssh/authorized_keys`, then reach it no more.

`sshd` logs `AuthorizedKeysCommand path is not absolute` for each key that the list lacks, as it
reads `none` as the command's path; the key is refused, as it should be.

## What the outputs feed

- `terraform output -raw client_config` is the `[controller]` table of each member's
  `~/.config/pumpkin/pumpkin.toml`, beside its `[upstreams]` and, for runs on Modal,
  `[controller.modal]`, the Modal provider's profile and classes, and nothing else; `pumpkin help
  config` says what each does, with an example, and which programs the person's machine, Linux on
  x86-64, needs.
- `terraform output -raw client_policy_arn` is the managed IAM policy that the person's machine
  needs; an account whose roles live elsewhere attaches it by hand, as `aws iam attach-role-policy
  --role-name <role> --policy-arn <arn>` (or `attach-user-policy`). `outputs.tf` says what it
  allows. The machine reaches port 22 at the controller's private address from a network in
  `ssh_from`.
- `terraform output -raw ec2` is the body of the EC2 provider's table in `pumpkin.toml`, under a
  header such as `[providers.aws.ec2]`, whose keys `pumpkin help config` says, with each class's
  template at the version its creates launch. The controller's settings hold it already; a machine
  that runs Pumpkin itself in the controller's security group takes it from here.
- `terraform output -raw modal_images` is the `images` key of the Modal provider's table, which
  the controller's settings hold already.
- `[gateway] private`, the address the sandboxes dial, is `"<private ip>:0"`, as each job sets it
  on the controller: the groups admit only the ephemeral ports, 32768-60999, where the kernel's
  pick for port 0 falls.

## Changes

An apply that changes a class or the AMI makes a new version of its templates, which the
controller's settings name from its next job on, as the `ec2` output does. A Modal class or
profile needs no apply: it is the client's `[controller.modal]`, which each job takes.

With `variables.tf`'s defaults, the controller costs about $0.94 an hour while it runs in
us-west-1, less in us-east-1 and us-west-2; stopped, about $110 a month for its volume at
us-east-1's rates, more in other regions; beside ECR's storage. A new Ubuntu release or a new
cloud-init reaches it only by `terraform apply -replace=aws_instance.controller`, once no run is
live: its volume, and the records on it, go with it.

What Pumpkin installs on the controller, its user, units and `sshd`'s settings, an apply never
changes: a release changes it, as its admin runs its install there. With administrator access, as
`ubuntu`, download the release's archive for its `pumpkinversion` line, `<tag> <commit> <sha256>`,
and run its install, which says each change it makes, and nothing if it makes none:

```sh
curl --fail --location --output pumpkin.tar.xz \
    https://github.com/FrontierCS/Pumpkin/releases/download/<tag>/pumpkin-<tag>-x86_64-unknown-linux-musl.tar.xz
echo '<sha256>  pumpkin.tar.xz' | sha256sum --check --strict
mkdir pumpkin && tar --extract --xz --file=pumpkin.tar.xz --directory=pumpkin
sudo pumpkin/bin/pumpkin controller install --name <name> --region <region>
```

Set `pumpkin_release` to that line too, which a controller made later installs at its first boot.
The boot runs each job install that published a `bin/pumpkin-boot` link. Older installs need
their matching client to reap subsequent jobs. Sandboxes that remain unreachable expire at the
run's hard end.

## The public page

A read-only page of the controller's runs, for a team that knows its address: CloudFront serves it
over HTTPS from a private bucket, at the distribution's own `https://<id>.cloudfront.net/`, with
no login and kept out of search engines. The controller's jobs upload the runs, live and ended.

Three variables in `terraform.tfvars`, which `variables.tf` says, set it up: `public_page`,
`publish_from_jobs` and `publish_since`, such as `"2026-11-15T00:00:00Z"`.

- The controller role's policy `publish`, which `public_page` gives it, has no read and no
  delete. `terraform output -raw public_page` is the address, and `public_bucket` the bucket.
- Set `publish_from_jobs` once the controller runs a Pumpkin that knows the `[publish]` table: an
  older one refuses the settings that hold it.

Then subscribe the distribution to CloudFront's free flat-rate plan, in the console: past the
always-free 1 TB and 10 million requests a month, pay-as-you-go charges about $85 a TB, which a
downloader set on it could run up, and the free plan charges none, slowing the page instead (its
[terms](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/flat-rate-pricing-plan.html)).

The page costs about $2 a month to keep, estimated for 150 runs a month of 2 hours each, at
us-east-1's rates: S3's uploads, about 360,000 a month, $1.80, and its lists, $0.40; the files,
about 0.3 GB, a cent; CloudFront, nothing within its free tier. The first publish uploads the
controller's older runs, about $0.60 once.

To take a run down by hand, with administrator credentials: take it out of the list, which the
bucket holds deflated in zlib's format, as `Content-Encoding: deflate`, remove its prefix, and
invalidate it, so that no edge serves it from its cache. The next publish uploads it again while
the controller holds its record.

```sh
aws s3 cp s3://<bucket>/runs.json runs.json.z
python3 -c 'import sys, zlib; sys.stdout.buffer.write(zlib.decompress(open(sys.argv[1], "rb").read()))' runs.json.z > runs.json
python3 -c 'import json, sys; d = json.load(open(sys.argv[1], "rb")); d["runs"] = [r for r in d["runs"] if r["id"] != sys.argv[2]]; open(sys.argv[1], "w", encoding="utf-8").write(json.dumps(d, ensure_ascii=False, separators=(",", ":")))' runs.json <id>
python3 -c 'import sys, zlib; sys.stdout.buffer.write(zlib.compress(open(sys.argv[1], "rb").read()))' runs.json > runs.json.z
aws s3 cp runs.json.z s3://<bucket>/runs.json --content-encoding deflate \
    --content-type application/json --cache-control 'public, max-age=30'
aws s3 rm s3://<bucket>/runs/<id>/ --recursive
aws cloudfront list-distributions --query "DistributionList.Items[?DomainName=='<host of public_page>'].Id" --output text
aws cloudfront create-invalidation --distribution-id <distribution> --paths '/runs/<id>/*' /runs.json
```

To take the page down, set `public_page` and `publish_from_jobs` to `false`, remove
`publish_since`, and apply: the apply empties and deletes the bucket, whose files are copies of
the controller's records.

## Teardown

```sh
terraform destroy
```

This deletes the repositories with their images, so a record's images can no longer be pulled.
The AMIs that Image Builder baked stay registered, as do their snapshots: deregister each
`aws ec2 describe-images --owners self --filters 'Name=name,Values=pumpkin-<name>*'` lists. The
state bucket and the OIDC provider are the account's, and stay.
