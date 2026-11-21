# Providers and the controller

A provider makes a run's sandboxes: containers on the machine's own Docker engine, containers on
EC2 instances, or Modal sandboxes. A controller is an EC2 instance that runs the runs that the
operator's machine sends it. The operator is whoever runs `pumpkin` and holds the machine's
configuration and keys. This page says what each is, what it needs, and where each is set up:
`pumpkin help config` holds every key of the machine's configuration, and
[`deploy/aws`](../deploy/aws/README.md) the AWS side.

![On the operator's machine, pumpkin builds images on the Docker engine, where the Docker provider's containers also run; a machine with a controller sends its runs over ssh to the controller, which builds the images on its own engine, pushes them to ECR, launches EC2 instances that pull them, and creates Modal sandboxes, which Modal imports from the same repositories.](figures/providers.svg)

## Placing a run

Every image is built on the Docker engine of the machine that runs the driver, the controller's
own when runs go to a controller, whichever provider runs it, and tagged by the digest of its
inputs. A role's `needs` says what one sandbox of it needs: its CPUs, its memory, and
a GPU or a CPU feature if any. The run places each role in the smallest class of sandbox, of every
provider's in the configuration, that offers that, and refuses a role that fits none, saying why
of each class, before anything is reserved or started. A provider whose sandboxes pull their images, EC2 or
Modal, first pushes them to where they pull from. Every sandbox carries its run's tag, so runs
that share a provider never reach each other's sandboxes.

## The Docker provider

Its sandboxes are containers on the machine's own engine, each confined, with no network but
loopback, on whole cores that no other run's containers get while the run lasts. A run whose roles
need more cores than other runs leave is refused, and can be run again once they end. It has one
class, `container`, that offers the machine's CPUs and memory and no GPU. A machine that uses it
uses no other provider, so where a run's roles are placed never depends on how much memory the
machine has. Rootless Docker is the recommended engine: `pumpkin run` warns of one that runs as
root.

## The EC2 provider

Its sandboxes are containers on EC2 instances of their own, one each, launched from the launch
templates of the envelope that `deploy/aws` makes, which the configuration's classes name. The
images go to the envelope's private ECR repositories, and each instance pulls its role's image with
its own role, through ECR's credential helper, which keeps no login. An instance's firewall lets its
container reach the gateway's address and port and nothing else, not the instance nor its metadata.
Each instance's termination is scheduled at its run's hard end, outside the instance, so it ends
even if the driver dies.

## The Modal provider

Its sandboxes are sandboxes in a Modal workspace, under gVisor, that run images which Modal
imports from the envelope's ECR repositories with a role that it assumes by its own identity, so
no AWS credential of the operator's reaches Modal. A sandbox opens no connection but DNS to
Modal's resolver; the driver dials it through Modal's TLS tunnel, which takes connections only
from the configuration's `dials_from` address. A class names its GPU as Modal does, such as `A100-80GB`, and Modal ends each sandbox at
its run's hard end.

## The controller

The controller is the same `pumpkin`, on an EC2 instance of the operator's own, under
`pumpkin controller`. Only the instance that `deploy/aws` makes can be one: its jobs read their
settings, such as the EC2 provider's launch templates, from the module's SSM parameter, and the
machine finds and starts it through EC2. It admits the keys that the module's `members` lists,
and names each member's jobs. The machine reaches it at its private
address alone, so it is in the controller's VPC, or routed to it (by peering or a VPN, say), and
`deploy/aws`'s `ssh_from` admits it. A machine whose configuration names
`[controller]` sends it each run, over ssh: `pumpkin run` sends the files of the task's
repository that git lists and the task reaches, then starts the run's job, which builds the images and runs the driver there, in a
systemd unit of its own. `wait`, `ls`, `cancel`, `view` and `reap` work on the controller's runs
with the same arguments, and `wait` copies an ended run's record back to the machine.

![The data between a machine and its controller, over ssh: the task's tree to the controller's store, the job's start, what the run says as it is followed, what ls reads of each run, and an ended run's record, each as the controller modules of pumpkin carry it, and the view's requests for the runs' data and their replies, over one ssh to the view on the controller.](figures/controller-data.svg)

The controller starts when a command needs it and stops itself once it has been idle a while,
with no job and no ssh session; at boot it reaps what lost runs left. A machine's keys go to it
only with a job, into the job's directory in memory, which the job's unit removes as it ends. A job
runs the same Pumpkin and helper programs as the machine, which uploads its install when
the controller lacks it. At boot, each build with a `bin/pumpkin-boot` link reaps its own
records and appends its output to `~/boot.log`. For older installs, run `pumpkin reap` from the
client build that wrote their records.

A controller can also publish its runs, live and ended, to a read-only page served from a private
bucket behind CloudFront, for a team that knows its address. `deploy/aws`'s README says how to
turn it on, what it costs, and how to take a run down.
