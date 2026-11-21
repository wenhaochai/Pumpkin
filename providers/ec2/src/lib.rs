//! The EC2 provider: a sandbox for a role as a container on an EC2 instance of its own.
//!
//! [`Ec2::connect`] reaches the [envelope](#the-envelope) before anything starts.
//! It keeps to a run's own instances by the tag that carries its run.
//!
//! # The envelope
//!
//! The operator makes the envelope on AWS, once, with an administrator's credentials: the
//! sandboxes' security group and instance profile, an image repository for each kind of image a
//! sandbox pulls, a launch template for each instance class, which fixes its AMI and instance
//! type, and the sandboxes' timer, a schedule group and a role that terminates their instances
//! (`Timer`). What it made is the provider's [`Settings`].
//!
//! The provider relies on the envelope, and checks none of this on AWS: each launch template, at
//! the version the settings name, boots an AMI for `x86_64`, the only architecture `sandboxd` is
//! built for, which runs cloud-init and systemd, as AL2023 does, and holds Docker, ECR's
//! credential helper and nftables, as the instance's boot installs none; has each network
//! interface and EBS volume of its instances deleted when the instance is terminated, so that a
//! sandbox is freed with its instance; serves its instances' metadata only with a token,
//! within one hop of the instance; and launches an instance type built on the Nitro System, the
//! only kind whose latest serial console EC2 reads ([`Provider::console`]).

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use aws::{Bound, Cli, Region, Repository, RepositoryName, RoleArn};
use engine::{Platform, Pusher};
use provider::{
    Console, Create, Created, Creator, ImageRef, Listed, Meeting, Provider, Ready, Reservation,
    Role, RunTag, SandboxId, SandboxdRef, Sweep, Undeleted,
};
use schema::ec2_boot::{BOOT_IN_IMAGE, SANDBOXD_IN_IMAGE};
use schema::ir::{self, CpuFlag, CpuModel, Image, Name};
use schema::sandbox::TcpDial;
use schema::workflow::{Class, Offers, UsdPerHour};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::iam::{CALLS, ECR};

pub mod iam;
mod launch;
mod user_data;

/// The most seconds a sandbox's start takes, as a class's [`Class::startup_seconds`] declares it
/// unless its settings give another (`InstanceClass::startup_seconds`): the shares of the launch,
/// the boot, and Docker's install and the images' pulls, then the container's create and start,
/// and its `sandboxd`'s dial and set-up. The boot's steps are bounded by the create's deadline
/// alone, so a start asked with less than this ahead is still tried.
const STARTUP_SECONDS: NonZeroU64 = NonZeroU64::new(600 + 300 + 300 + 240 + 180 + 120).unwrap();

const MILLIS_PER_CPU: NonZeroU64 = NonZeroU64::new(ir::MILLIS_PER_CPU).unwrap();

const MIB: NonZeroU64 = NonZeroU64::new(1 << 20).unwrap();

/// The envelope's settings, which only deserialization makes, from a table such as this, in TOML,
/// which must name each key but those a class may leave out (`InstanceClass`):
///
/// ```toml
/// region = "us-west-1"
/// repositories = { roles = "pumpkin/roles", sandboxd = "pumpkin/sandboxd" }
/// timer = { group = "pumpkin", role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer" }
/// classes.cpu = { template = "lt-0123456789abcdef0", version = 1, vcpus = 8, memory_mib = 8192 }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The region of the envelope, which every call to AWS names.
    region: Region,
    classes: BTreeMap<Name, InstanceClass>,
    repositories: Repositories<RepositoryName>,
    timer: Timer,
}

/// The sandboxes' timer, which ends each instance soon after its run's hard end, outside its guest,
/// so also one whose guest hangs: the schedule group, and the role that each schedule's target runs
/// with, which `EventBridge` Scheduler assumes, and which may terminate run instances.
///
/// A create that returns an instance has made its one-shot schedule in the group, named by the
/// run's tag and the ID ([`launch::timer_name`]), which terminates it after the boot's own timer
/// ([`launch::HardEnd::timer`]), then deletes itself. A create that can't make it fails and leaves
/// the instance, as its errors say ([`Ec2::create_instance`]); a delete deletes it once the
/// instance is terminated, and one a lost run leaves fires on an instance gone already.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Timer {
    group: GroupName,
    role_arn: RoleArn,
}

/// The name of an `EventBridge` Scheduler schedule group, such as `pumpkin`.
#[derive(Clone, Debug, Deserialize)]
struct GroupName(String);

/// The envelope's image repositories, one for each kind of image a sandbox pulls, each as a `T`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Repositories<T> {
    /// The roles' images.
    roles: T,
    /// The images that each hold a static `sandboxd`.
    sandboxd: T,
}

/// An instance class, as the envelope made it: its launch template, at the version that each
/// create launches; its instance type's CPUs and memory, in MiB; its CPU's flags and model, as
/// `/proc/cpuinfo` names them; the price that the operator says an instance costs; and the most
/// seconds an instance's start takes. Without `cpu_features`, it states no flag, without
/// `cpu_model`, no model, without `usd_per_hour`, no price, and without `startup_seconds`,
/// [`STARTUP_SECONDS`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceClass {
    template: TemplateId,
    version: NonZeroU64,
    vcpus: NonZeroU64,
    memory_mib: NonZeroU64,
    #[serde(default)]
    cpu_features: BTreeSet<CpuFlag>,
    cpu_model: Option<CpuModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usd_per_hour: Option<UsdPerHour>,
    // A class whose roles' images take longer to pull than the default allows, at the instance's
    // disk's throughput, needs a longer start.
    startup_seconds: Option<NonZeroU64>,
}

impl InstanceClass {
    /// The most a role may have of the class's instance type: thousandths of a CPU, all of its
    /// CPUs, since Docker refuses a limit over the CPUs it sees, and none is kept for the
    /// instance, as a CPU limit only throttles; and bytes of memory, all of its memory but what
    /// the instance keeps for itself ([`reserved_mib`]).
    fn role_limits(&self) -> (NonZeroU64, NonZeroU64) {
        let memory_mib = self.memory_mib.get();
        let Some(left_mib) = NonZeroU64::new(memory_mib - reserved_mib(memory_mib)) else {
            unreachable!(
                "an instance of {memory_mib} MiB keeps all of it, though it keeps a quarter at most"
            )
        };
        (
            self.vcpus.saturating_mul(MILLIS_PER_CPU),
            left_mib.saturating_mul(MIB),
        )
    }
}

/// The memory, in MiB, that an instance with `memory_mib` keeps for itself, out of a role's
/// reach, for its kernel, its operating system and its Docker: 25% of the first 4 GiB, 20% of
/// the next 4, 10% of the next 8, 6% of the next 112, and 2% of the rest.
// An instance type's nominal memory is more than its kernel reports, and the kernel, the
// operating system and Docker keep some of what it reports, a share that shrinks as memory
// grows: the tiers leave them that, with room to spare.
fn reserved_mib(memory_mib: u64) -> u64 {
    let tiers = [
        (4096, 25),
        (4096, 20),
        (8192, 10),
        (114_688, 6),
        (u64::MAX, 2),
    ];
    let mut left = memory_mib;
    let mut reserved = 0;
    for (size, percent) in tiers {
        let tier = left.min(size);
        reserved += tier * percent / 100;
        left -= tier;
    }
    reserved
}

/// The ID of an EC2 launch template, such as `lt-0123456789abcdef0`.
#[derive(Clone, Debug, Deserialize)]
struct TemplateId(String);

/// The EC2 envelope, reached, which every sandbox of the provider's runs in, and the operator's
/// Docker engine, which holds what they pull until it is pushed.
///
/// Each command it runs is killed, and each wait gives up, so that [`connect`](Self::connect) and
/// each [`Provider`] call return within a bound: a create's `aws` commands at its deadline
/// ([`Create::by`]), which bounds too its wait for the run's list to find the instance, which gives
/// up 200 s after the launch; a delete's wait for its instances to be terminated 10 min after the
/// terminations; any other `aws` command as [`aws::Bound::Call`] bounds it; and a publish's
/// `docker` commands as [`engine::Engine`] bounds them.
#[derive(Debug)]
pub struct Ec2<'e> {
    reached: Reached,
    pusher: Pusher<'e>,
    classes: BTreeMap<Name, InstanceClass>,
    repositories: Repositories<Repository>,
    /// The timer's role, which each instance's schedule runs with.
    timer_role: RoleArn,
    /// What [`Provider::ready`] gives: what names the account, and the classes of sandbox.
    ready: Ready,
    dir: PathBuf,
}

impl<'e> Ec2<'e> {
    /// Reaches the envelope that `settings` name, with `pusher`, the Docker engine that the
    /// provider publishes from, once AWS answers to the command line's credentials and describes
    /// the repositories. It writes in `dir`,
    /// made once a launch needs it, each launch's user data, while `aws` reads it. It fails if AWS
    /// describes no repository that the settings name.
    pub fn connect(settings: &Settings, pusher: Pusher<'e>, dir: &Path) -> Result<Self, String> {
        let Settings {
            region,
            classes,
            repositories,
            timer,
        } = settings;
        let cli = Cli::connect(region.clone())?;
        let Identity { account, arn } = cli.json(&CALLS.identity, &[], Bound::Call)?;
        let repositories = described_repositories(&cli, repositories)?;
        let Timer { group, role_arn } = timer.clone();
        let report = json!({
            "account": account,
            "caller": arn,
            "repositories": {
                "roles": repositories.roles.uri(),
                "sandboxd": repositories.sandboxd.uri(),
            },
            "timer": { "group": group.0, "role_arn": role_arn },
        });
        Ok(Self {
            reached: Reached { cli, group },
            pusher,
            classes: classes.clone(),
            repositories,
            timer_role: role_arn,
            ready: ready(report, classes),
            dir: dir.to_owned(),
        })
    }
}

impl Ec2<'_> {
    /// Reaches the envelope that `settings` name for a sweep of a run's instances alone: nothing
    /// of the envelope is checked, only that AWS answers to the command line's credentials, in
    /// the settings' region, as [`connect`](Self::connect) has it answer.
    pub fn reach(settings: &Settings) -> Result<Reached, String> {
        let cli = Cli::connect(settings.region.clone())?;
        let Identity { .. } = cli.json(&CALLS.identity, &[], Bound::Call)?;
        let group = settings.timer.group.clone();
        Ok(Reached { cli, group })
    }
}

impl Sweep for Ec2<'_> {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        self.reached.delete(run, ids)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        self.reached.list(run)
    }
}

/// An envelope that its settings name, reached for a sweep of a run's instances alone
/// ([`Ec2::reach`]): it lists and deletes them as [`Ec2`] does, and bounds its calls alike.
#[derive(Debug)]
pub struct Reached {
    cli: Cli,
    /// The timer's schedule group, whose schedule of an instance a delete deletes with it.
    group: GroupName,
}

impl Provider for Ec2<'_> {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    // Each sandbox is an instance of its own, which no other run shares, so a run holds nothing
    // beforehand: a create that EC2 has no capacity for fails.
    fn reserve(&self, _run: &RunTag, _phases: &[Vec<Role>]) -> Result<Reservation, String> {
        Ok(Reservation::new(()))
    }

    /// Publishes `image`, a role's, as [`Provider::publish_image`] asks: the engine must have it,
    /// as the digest of a manifest for the instances' platform, which is checked before any call
    /// to AWS; and it is named as it is in the roles' repository ([`aws::Cli::publish`]).
    fn publish_image(&self, image: &Image) -> Result<ImageRef, String> {
        // The instances', whose AMIs are all for `sandboxd`'s architecture, as the envelope has it.
        let runs = "the envelope's instances run";
        (self.pusher.engine()).check_platform(image, &Platform::sandboxd(), runs)?;
        let cli = &self.reached.cli;
        let published = cli.publish(&self.pusher, &self.repositories.roles, image);
        published.map(ImageRef::from)
    }

    /// Publishes the static `sandboxd` at `path`, as [`Provider::publish_sandboxd`] asks, with
    /// the static `ec2-boot` beside it, as `cargo xtask sandboxd` builds them: in the image that
    /// the engine loads ([`engine::Engine::load_sandboxd`]), which the same pair always gives,
    /// named as it is in `sandboxd`'s repository ([`aws::Cli::publish`]).
    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String> {
        let boot = path.with_file_name("ec2-boot");
        let binaries = [(SANDBOXD_IN_IMAGE, path), (BOOT_IN_IMAGE, &*boot)];
        let image = self.pusher.engine().load_sandboxd(&binaries)?;
        let cli = &self.reached.cli;
        let published = cli.publish(&self.pusher, &self.repositories.sandboxd, &image);
        published.map(SandboxdRef::from)
    }

    // An instance reaches the gateway's host only on the envelope's network.
    fn meeting(&self) -> Meeting<'_> {
        Meeting::Private(self)
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        self.read_console(run, id)
    }
}

impl Creator<TcpDial> for Ec2<'_> {
    fn create(&self, create: &Create, dial: TcpDial) -> Result<Created, String> {
        self.create_instance(create, dial)
    }
}

/// What [`Provider::ready`] gives of an envelope of instance classes `classes`, whose account
/// `report` names.
fn ready(report: Value, classes: &BTreeMap<Name, InstanceClass>) -> Ready {
    // Each instance class is a class of sandbox, which offers a role what its instance type
    // leaves one ([`InstanceClass::role_limits`]), and the CPU's flags and model that the
    // settings state.
    let classes = (classes.iter()).map(|(name, class)| {
        let (cpu_millis, memory_bytes) = class.role_limits();
        let offers = Offers {
            cpu_millis,
            memory_bytes,
            gpu: None,
            cpu_features: class.cpu_features.clone(),
            cpu_model: class.cpu_model.clone(),
        };
        let class = Class {
            offers,
            startup_seconds: class.startup_seconds.unwrap_or(STARTUP_SECONDS),
            usd_per_hour: class.usd_per_hour,
        };
        (name.clone(), class)
    });
    Ready {
        report,
        classes: classes.collect(),
    }
}

/// What `aws sts get-caller-identity` prints, which a call answers if the credentials work: the
/// account that they are in, and whose they are, as an ARN.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Identity {
    account: String,
    arn: String,
}

/// `repositories`, with their URIs.
fn described_repositories(
    cli: &Cli,
    repositories: &Repositories<RepositoryName>,
) -> Result<Repositories<Repository>, String> {
    let Repositories { roles, sandboxd } = repositories;
    let [roles, sandboxd] = cli.repositories(&ECR, [roles, sandboxd])?;
    Ok(Repositories {
        roles: roles?,
        sandboxd: sandboxd?,
    })
}
