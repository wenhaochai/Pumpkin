//! The operator's Docker engine, on which Pumpkin builds the images that its sandboxes run,
//! whichever provider runs them, and from which it publishes them.
//!
//! [`Engine`] runs `docker` as an ordinary user, and writes outside the engine only in the
//! directory it is given. It needs Docker 28.0 or later, the first whose `docker image inspect`
//! says what an image's ID is the digest of ([`Engine::check_platform`]), with the containerd image
//! store, rootful or rootless ([`Security::rootless`]).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write as _;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{fmt, fs, io};

use base64::Engine as _;
use schema::ir::{Image, MILLIS_PER_CPU, Resources, User};
use schema::sandbox::{PUMPKIN_BYTES, pumpkin_dir};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stdx::process::{self, Bound, Echo, Environment, Failure, Run};

mod build;
pub mod prune;
mod sandboxd;

pub use build::{Admit, BuildContext, Building, Built, Recipe, Started, finish, mounted};

// How long a `docker` command that is neither a create's nor a removal may run before it is
// killed: each such command makes one request of the engine, or a few, and none waits on a pull or
// a build.
const CALL_TIMEOUT: Duration = Duration::from_mins(2);

/// How long a removal that isn't a create's may run before it is killed ([`Engine`]).
// A removal deletes the container's writable layer and anonymous volumes, which have no size
// limit, so its time grows with what the sandbox wrote there, as a tree of many small files. And
// a run tries each removal once, and fails with it, while the engine goes on to finish one whose
// `docker rm` was killed: so the bound is sized only to catch an engine that hangs.
pub const REMOVE_TIMEOUT: Duration = Duration::from_mins(10);

// How long a push may run before it is killed. A push uploads the layers the registry lacks, so
// its time grows with their size and falls with the host's bandwidth, which no bound can know:
// this one is sized only to turn a push that hangs into a failure, rather than a wait without end.
// A later push of the image uploads only the layers the registry still lacks.
const PUSH_TIMEOUT: Duration = Duration::from_hours(2);

// The first release of Docker's command line whose push takes its login from its environment
// alone, from `DOCKER_AUTH_CONFIG` ([`Pusher`]). An older one ignores the variable, and pushes with
// whatever the operator's own configuration holds for the registry, such as another identity's,
// from a credential helper.
const PUSHING_CLI: [u64; 3] = [28, 3, 0];

// How many of the calls on the engine that publish an image run at once, of every `pumpkin` whose
// engine has the same directory, as of a workspace: a look at an image's platform, a push and a
// load of `sandboxd`'s image. The rest wait for a slot. Few, and the same on any machine, as the
// engine's API, not its CPUs, is what they wait on, and too many calls at once have hung an engine
// until it was restarted. A push already uploads up to 5 layers at once, dockerd's default
// `max-concurrent-uploads`, so 4 keep up to 20 uploads going.
const PUBLISH_SLOTS: NonZeroUsize = NonZeroUsize::new(4).unwrap();

const PUBLISH_SLOTS_DIR: &str = "publish-slots";

// The most the container's log may hold, in bytes, in one file. The log is kept on the host's
// disk, outside the container's other limits, so it is capped whatever writes there, with the
// `json-file` driver named, so that the cap doesn't rest on the daemon's default. It holds the
// output of `sandboxd` alone, which is a line if it fails: its commands' output goes to files, and
// they can't open its own, since it is non-dumpable.
const LOG_BYTES: u64 = 1 << 20;

// The most processes and threads a container may have.
const PIDS_LIMIT: u32 = 4096;

const CONTAINERD_STORE: (&str, &str) = ("driver-type", "io.containerd.snapshotter.v1");

/// The media types of an image's manifest: the OCI image format's, and Docker's schema 2, which it
/// was made from, and which some engines' builds still write.
pub const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// The name of a Docker context, which says what engine `docker` reaches, and how. `default` is
/// Docker's own context, which reaches the engine at Docker's default socket.
///
/// Any name but the empty one, which `docker` would take for no name, and so reach the engine of
/// the command line's current context. A name no context has is refused by [`Engine::connect`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ContextName(String);

impl ContextName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContextName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if name.is_empty() {
            Err(
                "the Docker context's name is empty: name one that `docker context ls` lists"
                    .to_owned(),
            )
        } else {
            Ok(Self(name))
        }
    }
}

/// A platform that images are for, as an image's configuration names it: an operating system and
/// an architecture, which Docker writes `<os>/<architecture>`, such as `linux/amd64`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    os: String,
    architecture: String,
}

impl Platform {
    /// Linux on x86-64, `linux/amd64`: the platform of the static `sandboxd`, which is built for it
    /// alone.
    pub fn sandboxd() -> Self {
        Self {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { os, architecture } = self;
        write!(f, "{os}/{architecture}")
    }
}

impl Serialize for Platform {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The credentials of an image registry, with which [`Pusher::push`] pushes to it. Its `Debug`
/// shows no password.
pub struct Login {
    registry: String,
    username: String,
    password: String,
}

impl Login {
    /// The credentials for `registry`, named as Docker names it in an image's name, such as
    /// `registry.example` or `127.0.0.1:5000`.
    pub fn new(registry: String, username: String, password: String) -> Self {
        Self {
            registry,
            username,
            password,
        }
    }

    fn auth_config(&self) -> String {
        let Self {
            registry,
            username,
            password,
        } = self;
        let auth =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        serde_json::json!({ "auths": { registry: { "auth": auth } } }).to_string()
    }
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            registry,
            username,
            password: _,
        } = self;
        (f.debug_struct("Login"))
            .field("registry", registry)
            .field("username", username)
            .finish_non_exhaustive()
    }
}

/// The Docker engine a context reaches, and its report.
#[derive(Debug)]
pub struct Engine {
    report: Report,
    // Read at the connect, but refused only by a caller that needs it ([`Engine::host`]): a build
    // or a push needs none of it.
    host: Result<Host, String>,
    dir: PathBuf,
    // Atomic, as the run's threads share the engine, through its providers.
    said_waiting: AtomicBool,
}

/// The engine, as `docker` describes it before anything starts.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub context: ContextName,
    /// The endpoint the context reaches the engine at, such as `unix:///var/run/docker.sock`.
    pub endpoint: String,
    /// The version of the `docker` command line.
    pub client_version: String,
    pub engine_version: String,
    pub security: Security,
    /// The version of the kernel of the engine's host.
    pub kernel: String,
    /// The operating system of the engine's host.
    pub os: String,
    /// The platform whose images the engine runs.
    pub platform: Platform,
}

/// What the engine's security options say.
#[derive(Clone, Debug, Serialize)]
pub struct Security {
    /// Whether the engine runs as an ordinary user rather than as root.
    // Rootless Docker is the setup to choose: its engine, and so every container, runs as an
    // ordinary user, so escaping a container gains no more than that user has. A rootful engine
    // runs as root, and whoever may use its socket, as the `docker` group may, can make it do
    // anything root can.
    pub rootless: bool,
    /// The seccomp profile containers get unless told otherwise, `builtin` for Docker's own, or
    /// `None` if the engine has no seccomp.
    pub seccomp_profile: Option<String>,
    /// Whether the engine confines containers with `AppArmor`.
    pub apparmor: bool,
    /// Whether the engine maps containers' users into a user namespace of their own.
    pub userns: bool,
}

/// What `docker info` says of the engine's host and of the limits that the engine enforces on a
/// container, as it said when [`Engine::connect`] reached the engine: facts that change only as the
/// engine restarts.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "`docker info` reports whether the engine supports each limit as a boolean"
)]
pub struct Host {
    pub memory_limit: bool,
    pub swap_limit: bool,
    /// Whether the engine supports CPU limits.
    pub cpu_cfs_quota: bool,
    #[serde(rename = "CPUSet")]
    /// Whether the engine supports confining a container to a set of CPUs.
    pub cpu_set: bool,
    pub pids_limit: bool,
    #[serde(rename = "MemTotal")]
    /// The memory of the engine's host.
    pub memory_bytes: NonZeroU64,
    #[serde(rename = "ID")]
    /// The engine's ID, which it keeps from one start to the next.
    pub id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InfoJson {
    driver_status: Option<Vec<(String, String)>>,
    security_options: Option<Vec<String>>,
    kernel_version: String,
    operating_system: String,
    server_version: String,
    client_info: ClientInfoJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientInfoJson {
    version: String,
}

impl Engine {
    /// Reaches the engine through `context`, and reports it, refusing one that keeps no containerd
    /// image store. What it writes on this machine goes in `dir`, made once a build or a load needs
    /// it: a directory of its own for each while it runs, and Buildx's state, in `buildx`.
    pub fn connect(context: ContextName, dir: &Path) -> Result<Self, String> {
        let Reach { endpoint, info } = reach(&context)?;
        let InfoJson {
            driver_status,
            security_options,
            kernel_version,
            operating_system,
            server_version,
            client_info,
        } = read_info(&info)?;
        check_store(&context, &driver_status.unwrap_or_default())?;
        let platform = platform(&context)?;
        let report = Report {
            context,
            endpoint,
            client_version: client_info.version,
            engine_version: server_version,
            security: Security::parse(&security_options.unwrap_or_default()),
            kernel: kernel_version,
            os: operating_system,
            platform,
        };
        Ok(Self {
            report,
            host: read_info(&info),
            dir: dir.to_owned(),
            said_waiting: AtomicBool::new(false),
        })
    }

    /// Reaches the engine through `context`, as [`connect`](Self::connect) does, but checks nothing
    /// more of it.
    pub fn reach(context: &ContextName) -> Result<(), String> {
        reach(context).map(drop)
    }

    /// The engine, as it was when [`connect`](Self::connect) reached it.
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// What `docker info` said of the engine's host when [`connect`](Self::connect) reached it.
    pub fn host(&self) -> Result<&Host, String> {
        self.host.as_ref().map_err(String::clone)
    }

    /// Checks that `image`, the digest of an image's manifest that the engine has, is for
    /// `platform`, which `runs`, such as `the provider runs sandboxes on`, says what runs.
    ///
    /// Refuses an image for another platform, one the engine lacks (it pulls none), and one that
    /// names anything but a manifest, such as an index.
    pub fn check_platform(
        &self,
        image: &Image,
        platform: &Platform,
        runs: &str,
    ) -> Result<(), String> {
        // An image of another platform would run, if at all, only under emulation, beside a
        // `sandboxd` built for this one.
        let of = self.platform_of(image)?;
        if of != *platform {
            return Err(format!(
                "{image} is an image for {of}, but {runs} {platform}: name an image for {platform}"
            ));
        }
        Ok(())
    }

    fn platform_of(&self, image: &Image) -> Result<Platform, String> {
        let _slot = self.publishing()?;
        let context = &self.report.context;
        let name = context.as_str();
        let inspect = [
            "image",
            "inspect",
            "--format",
            "{{json .}}",
            "--",
            image.as_str(),
        ];
        // One call, rather than a listing of every image before it, as many runs that publish at
        // once each call the engine. `docker image inspect` fails alike whatever went wrong, so an
        // image it lacks is told by its text, as `image_id` tells one; and in the containerd image
        // store an image's full ID names that image alone.
        let inspected = match docker(context, inspect) {
            Ok(inspected) => inspected,
            Err(failure) if failure.reported().contains("No such image") => {
                return Err(format!(
                    "the engine of context `{name}` has no image {image}: build it there, or \
                     `docker load` an archive that holds it, as Pumpkin pulls none onto it"
                ));
            }
            Err(error) => return Err(fix_reported(&error)),
        };
        let ImageJson {
            os,
            architecture,
            descriptor,
        } = serde_json::from_slice(&inspected.stdout).map_err(|error| {
            format!(
                "the JSON `docker image inspect` printed of {image} isn't of the form expected: \
                 {error}: use Docker's own command line, which prints it"
            )
        })?;
        let Some(DescriptorJson { media_type }) = descriptor else {
            return Err(format!(
                "the engine of context `{name}` doesn't say what {image} is the digest of, as \
                 `docker image inspect` gives it no `Descriptor`: use Docker 28.0 or later, whose \
                 engine and command line give one"
            ));
        };
        // An index names a manifest for each of several platforms, of which a container runs the
        // engine's: so the digest the workflow names, which the run's record holds, wouldn't be the
        // digest of what ran. A build for one platform makes an index too when it attaches an
        // attestation, as `docker build` does by default. And the engine finds no image by the
        // digest of a manifest that an index holds, so an image to name is built as a manifest
        // alone.
        if !MANIFEST_TYPES.contains(&media_type.as_str()) {
            return Err(format!(
                "{image} names an `{media_type}`, not an image's manifest: name an image built \
                 for one platform with no attestation, as by `docker build --provenance=false`, \
                 whose ID is its manifest's digest"
            ));
        }
        // Without a variant, such as arm's `v7`, as the engine's own platform names none.
        Ok(Platform { os, architecture })
    }

    // Takes one of the engine's publish slots ([`PUBLISH_SLOTS`]), held until the file returned is
    // dropped; the first time that this engine waits for one, it says so, and which processes hold
    // them.
    fn publishing(&self) -> Result<fs::File, String> {
        let dir = self.dir.join(PUBLISH_SLOTS_DIR);
        let holder = format!("process {}", std::process::id());
        stdx::slots::take(&dir, PUBLISH_SLOTS, &holder, |holders| {
            if !self.said_waiting.swap(true, Ordering::Relaxed) {
                // As `pumpkin` says everything else: a standard error that can't be written is left
                // unwritten, as the publish goes on all the same.
                let _ = writeln!(
                    io::stderr(),
                    "pumpkin: waiting for a publish slot: all {PUBLISH_SLOTS} held, by {}",
                    holders.join(", ")
                );
            }
            Ok(())
        })
    }

    /// Each image that the engine has, by its ID, as [`Engine::inspected`] gives it.
    pub fn images(&self) -> Result<Vec<prune::Cached>, String> {
        let context = &self.report.context;
        let listed = docker(context, ["image", "ls", "--all", "--no-trunc", "--quiet"]);
        let listed = listed.map_err(|error| fix_reported(&error))?;
        // An image of several tags is listed once for each.
        let listed: BTreeSet<String> = (String::from_utf8_lossy(&listed.stdout).lines())
            .map(str::to_owned)
            .collect();
        let mut images = Vec::new();
        for id in listed {
            let image = Image::try_from(id.clone()).map_err(|error| {
                format!("`docker image ls` listed `{id}`, which is no image's ID: {error}")
            })?;
            images.push(image);
        }
        self.inspected(&images)
    }

    /// Each of `images` that the engine has, with its size, as `docker image inspect` gives it, and
    /// its tags, as they are now; but none the engine has no more.
    pub fn inspected(&self, images: &[Image]) -> Result<Vec<prune::Cached>, String> {
        let context = &self.report.context;
        let mut inspected = Vec::new();
        for image in images {
            let id = image.as_str();
            // One at a time, so that one removed meanwhile is told from a failure.
            let format = "{{.Size}}{{range .RepoTags}} {{.}}{{end}}";
            let inspect = ["image", "inspect", "--format", format, "--", id];
            let printed = match docker(context, inspect) {
                Ok(printed) => printed,
                Err(failure) if failure.reported().contains("No such image") => continue,
                Err(error) => return Err(fix_reported(&error)),
            };
            let printed = String::from_utf8_lossy(&printed.stdout);
            let mut words = printed.split_whitespace();
            let size = words.next().and_then(|size| size.parse().ok());
            let Some(size) = size else {
                return Err(format!(
                    "`docker image inspect` gave {id} the size and tags `{}`: expected a number \
                     of bytes, then the tags",
                    printed.trim().escape_debug()
                ));
            };
            inspected.push(prune::Cached {
                image: image.clone(),
                tags: words.map(str::to_owned).collect(),
                size_bytes: size,
            });
        }
        Ok(inspected)
    }

    /// Removes each of `images`, with each of its tags, and fails at the first that can't be
    /// removed, as one a container uses.
    pub fn remove_images(&self, images: &[Image]) -> Result<(), String> {
        for image in images {
            let rm = ["image", "rm", "--force", image.as_str()];
            let removed = docker_to(&self.report.context, rm, Bound::Within(REMOVE_TIMEOUT));
            removed.map_err(|error| fix_reported(&error))?;
        }
        Ok(())
    }

    /// Removes what the engine's build cache holds, least recently used first, until the engine's
    /// disk has `free_bytes` free, as the engine's builder counts it, or the cache holds nothing
    /// that it can remove.
    pub fn prune_build_cache(&self, free_bytes: u64) -> Result<(), String> {
        let free = free_bytes.to_string();
        let pruned = docker_to(
            &self.report.context,
            ["builder", "prune", "--force", "--min-free-space", &free],
            Bound::Within(REMOVE_TIMEOUT),
        );
        pruned.map(|_| ()).map_err(|error| fix_reported(&error))
    }

    /// The engine, as one that can [push](Pusher::push), if its Docker command line is 28.3.0 or
    /// later (`PUSHING_CLI`).
    pub fn pusher(&self) -> Result<Pusher<'_>, String> {
        let version = &self.report.client_version;
        // As semantic versioning orders versions, such as `28.3.1`, `28.3.0-rc.1` or
        // `26.1.5+dfsg1`: build metadata doesn't count, and a pre-release comes before its release,
        // as `28.3.0-rc.1`, which ignores the variable, does.
        let release = version
            .split_once('+')
            .map_or(version.as_str(), |(release, _)| release);
        let (release, pre) = release
            .split_once('-')
            .map_or((release, false), |(release, _)| (release, true));
        let numbers: Option<Vec<u64>> = release.split('.').map(|part| part.parse().ok()).collect();
        let recent = numbers
            .and_then(|numbers| <[u64; 3]>::try_from(numbers).ok())
            .is_some_and(|numbers| numbers > PUSHING_CLI || (numbers == PUSHING_CLI && !pre));
        if recent {
            return Ok(Pusher { engine: self });
        }
        let [major, minor, patch] = PUSHING_CLI;
        Err(format!(
            "the Docker command line of context `{}` is `{}`: expected {major}.{minor}.{patch} or \
             later, the first whose push takes its login from its environment alone",
            self.report.context.as_str(),
            version.escape_debug()
        ))
    }
}

/// An [`Engine`] whose Docker command line can push with a [`Login`] that reaches that push alone
/// ([`Engine::pusher`]).
#[derive(Debug)]
pub struct Pusher<'e> {
    engine: &'e Engine,
}

impl<'e> Pusher<'e> {
    pub fn engine(&self) -> &'e Engine {
        self.engine
    }

    /// Pushes `image`, an image the engine has, by its ID, to `repository`, such as `team/app`, of
    /// the registry that `login` names, as `<registry>/<repository>:<tag>`, which the image keeps
    /// as a tag; with `login`'s credentials, which reach that push alone: neither the operator's
    /// Docker configuration nor a credential store holds them.
    pub fn push(
        &self,
        image: &Image,
        login: &Login,
        repository: &str,
        tag: &str,
    ) -> Result<(), String> {
        let _slot = self.engine.publishing()?;
        let context = &self.engine.report.context;
        let target = format!("{}/{repository}:{tag}", login.registry);
        let tagged = docker(context, ["tag", "--", image.as_str(), &target]);
        let push = ["push", "--quiet", "--", &target];
        let pushed =
            tagged.and_then(|_| docker_as(context, push, Bound::Within(PUSH_TIMEOUT), Some(login)));
        pushed
            .map(drop)
            .map_err(|error| format!("pushing `{target}` failed: {}", fix_reported(&error)))
    }
}

struct Reach {
    endpoint: String,
    info: Vec<u8>,
}

fn reach(context: &ContextName) -> Result<Reach, String> {
    let name = context.as_str();
    let template = "{{.Endpoints.docker.Host}}";
    // After `--`, a name that begins with `-` can't be taken for an option.
    let inspect = ["context", "inspect", "--format", template, "--", name];
    let inspected = docker(context, inspect).map_err(|failure| {
        let unread = format!("the Docker context `{name}` can't be read");
        match failure {
            Failure::Exited { .. } => format!(
                "{unread}: {}: name one that `docker context ls` lists",
                failure.unadvised()
            ),
            Failure::Unstarted { .. } | Failure::Unwatched { .. } | Failure::Unended { .. } => {
                format!("{unread}: {}", fix_reported(&failure))
            }
        }
    })?;
    let endpoint = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
    let info = docker(context, ["info", "--format", "{{json .}}"]).map_err(|failure| {
        let unreached = format!("the Docker engine isn't reachable through context `{name}`");
        let todo = match &failure {
            Failure::Exited { .. } if failure.reported().contains("permission denied") => {
                "let this user reach the engine's socket: by the `docker` group, which is \
                 root-equivalent, or by running rootless Docker and naming its context"
            }
            Failure::Exited { .. } => "start the engine, or name a context whose engine runs",
            Failure::Unstarted { .. } | Failure::Unwatched { .. } | Failure::Unended { .. } => {
                return format!("{unreached}: {}", fix_reported(&failure));
            }
        };
        format!("{unreached}: {}: {todo}", failure.unadvised())
    })?;
    Ok(Reach {
        endpoint,
        info: info.stdout,
    })
}

fn read_info<T: DeserializeOwned>(info: &[u8]) -> Result<T, String> {
    serde_json::from_slice(info).map_err(|error| {
        format!(
            "the JSON `docker info` printed isn't of the form expected: {error}: use Docker's \
             own command line, which prints it"
        )
    })
}

impl Security {
    fn parse(options: &[String]) -> Self {
        let mut security = Self {
            rootless: false,
            seccomp_profile: None,
            apparmor: false,
            userns: false,
        };
        for option in options {
            let mut name = None;
            let mut profile = None;
            for pair in option.split(',') {
                match pair.split_once('=') {
                    Some(("name", value)) => name = Some(value),
                    Some(("profile", value)) => profile = Some(value),
                    _ => {}
                }
            }
            match name {
                Some("rootless") => security.rootless = true,
                Some("seccomp") => {
                    security.seccomp_profile = Some(profile.unwrap_or_default().to_owned());
                }
                Some("apparmor") => security.apparmor = true,
                Some("userns") => security.userns = true,
                _ => {}
            }
        }
        security
    }
}

fn platform(context: &ContextName) -> Result<Platform, String> {
    // `docker version` names the platform the engine is built for as an image's configuration names
    // one; `docker info` names the host's machine as the kernel does, such as `x86_64` for `amd64`.
    let format = "{{.Server.Os}}/{{.Server.Arch}}";
    let unread = |error: String| {
        let name = context.as_str();
        format!("the platform of the Docker engine of context `{name}` can't be read: {error}")
    };
    let version = docker(context, ["version", "--format", format])
        .map_err(|error| unread(fix_reported(&error)))?;
    let printed = String::from_utf8_lossy(&version.stdout);
    let Some((os, architecture)) = printed.trim().split_once('/') else {
        return Err(unread(format!(
            "`docker version` printed `{}`: expected `<os>/<architecture>`",
            printed.trim().escape_debug()
        )));
    };
    Ok(Platform {
        os: os.to_owned(),
        architecture: architecture.to_owned(),
    })
}

fn check_store(context: &ContextName, status: &[(String, String)]) -> Result<(), String> {
    // There an image's ID is the digest of what its name points to, a manifest for an image of one
    // platform, which a workflow names it by; in the classic store, it is the digest of the image's
    // configuration, which no workflow names.
    let (key, value) = CONTAINERD_STORE;
    if status.iter().any(|(name, set)| name == key && set == value) {
        return Ok(());
    }
    Err(format!(
        "the Docker engine of context `{}` doesn't keep its images in the containerd image \
         store, as `docker info` reports no `{key}` of `{value}`, so it can't find an image by \
         its manifest's digest: turn the store on, as `\"features\": {{\"containerd-snapshotter\": \
         true}}` in the engine's `daemon.json` does",
        context.as_str()
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageJson {
    os: String,
    architecture: String,
    // Moby's API marks the field experimental, liable to change without notice. An engine that
    // drops it gives none here, and one that renames it, none by this name, so either fails every
    // publish, as an engine before 28.0 does, rather than taking an image unchecked.
    descriptor: Option<DescriptorJson>,
}

#[derive(Deserialize)]
struct DescriptorJson {
    #[serde(rename = "mediaType")]
    media_type: String,
}

/// The options of `docker create` or `docker run` that confine a container to its role's `user` and
/// `resources`, wherever a provider's sandbox is a Docker container.
///
/// The container's network, bind mounts and command, which say how it reaches its gateway and what
/// it runs, are the caller's to give.
pub fn confinement(user: User, resources: Resources) -> Vec<String> {
    let User { uid, gid } = user;
    let Resources {
        cpu_millis,
        memory_bytes,
    } = resources;
    let (millis, memory) = (cpu_millis.get(), memory_bytes.get());
    vec![
        format!("--user={uid}:{gid}"),
        "--cap-drop=ALL".to_owned(),
        "--security-opt=no-new-privileges".to_owned(),
        // The daemon's `init` setting would otherwise make `docker-init` the first process, in
        // place of `sandboxd`.
        "--init=false".to_owned(),
        // An image's `HEALTHCHECK` would otherwise run as the role's user outside `sandboxd`, after
        // freezing too, with its output kept on the host.
        "--no-healthcheck".to_owned(),
        // Images are the operator's or the build's: a pull would fetch one that no run owns, with
        // the operator's registry credentials.
        "--pull=never".to_owned(),
        format!(
            "--cpus={}.{:03}",
            millis / MILLIS_PER_CPU,
            millis % MILLIS_PER_CPU
        ),
        format!("--memory={memory}"),
        format!("--memory-swap={memory}"),
        "--log-driver=json-file".to_owned(),
        format!("--log-opt=max-size={LOG_BYTES}"),
        "--log-opt=max-file=1".to_owned(),
        format!("--pids-limit={PIDS_LIMIT}"),
        // A crash would otherwise dump core where the host's `core_pattern` says: into the
        // container's own files, such as its writable layer, or to a helper that may store it on
        // the host's disk, outside the session and every limit. A helper still runs, but is told
        // the limit is 0.
        "--ulimit=core=0".to_owned(),
        format!(
            "--tmpfs={}:exec,uid={uid},gid={gid},mode=0755,size={PUMPKIN_BYTES}",
            pumpkin_dir()
        ),
        "--tmpfs=/tmp:ro".to_owned(),
    ]
}

fn image_id(context: &ContextName, reference: &str) -> Result<Option<String>, Failure> {
    let inspect = ["image", "inspect", "--format", "{{.Id}}", "--", reference];
    match docker(context, inspect) {
        Ok(inspected) => Ok(Some(
            String::from_utf8_lossy(&inspected.stdout).trim().to_owned(),
        )),
        Err(failure) if failure.reported().contains("No such image") => Ok(None),
        Err(error) => Err(error),
    }
}

/// `failure`, of a `docker` command that failed for a reason its caller doesn't foresee, and what
/// to do about it.
///
/// That is to check the engine, if the command was killed at its bound, as Docker reported
/// nothing; nothing, if its deadline had passed before it could start, which only the caller can
/// say what to do about; or else what `failure` says.
pub fn fix_reported(failure: &Failure) -> String {
    match failure {
        Failure::Unended { started: true, .. } => format!(
            "{}: check that the engine runs and isn't overloaded",
            failure.unadvised()
        ),
        Failure::Unended { started: false, .. } => failure.unadvised(),
        Failure::Unstarted { .. } | Failure::Unwatched { .. } | Failure::Exited { .. } => {
            failure.to_string()
        }
    }
}

/// Runs `docker` against `context` alone with `args`, as [`docker_to`] does, within 2 min
/// (`CALL_TIMEOUT`).
pub fn docker<I, S>(context: &ContextName, args: I) -> Result<Output, Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    docker_to(context, args, Bound::Within(CALL_TIMEOUT))
}

/// Runs `docker` against `context` alone with `args`, killing it at `bound`, and returns its
/// output.
///
/// Of the environment, `docker` gets only `HOME`, `PATH` and `DOCKER_CONFIG`, and the context as
/// `DOCKER_CONTEXT`, so that the context named alone steers it.
///
/// A [`Failure`] says whether `docker` didn't start, failed or was killed at `bound`, and what
/// Docker reported, for the caller to add what that means.
pub fn docker_to<I, S>(context: &ContextName, args: I, bound: Bound) -> Result<Output, Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    docker_as(context, args, bound, None)
}

fn docker_command<I, S>(context: &ContextName, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // A context reached over `ssh://` would need `SSH_AUTH_SOCK` too, which isn't passed.
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut command = stdx::process::command("docker", kept);
    // In the environment, rather than as `--context`, so that the name a failure gives the command
    // is its own: `docker` takes `DOCKER_CONTEXT` as it takes `--context`, but for a `DOCKER_HOST`
    // set, which this environment never holds.
    // https://github.com/docker/cli/blob/f9ced58158d5e0b358052432244b483774a1983d/cli/command/cli.go#L453-L471
    command.env("DOCKER_CONTEXT", context.as_str()).args(args);
    command
}

fn docker_as<I, S>(
    context: &ContextName,
    args: I,
    bound: Bound,
    login: Option<&Login>,
) -> Result<Output, Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = docker_command(context, args);
    if let Some(login) = login {
        command.env("DOCKER_AUTH_CONFIG", login.auth_config());
    }
    process::run(&mut command, Run::new(bound, Echo::Silent))
}
