//! What `cargo xtask live` reads of a configuration that sends runs to the person's controller,
//! `pumpkin`'s, for a smoke run on one of its providers or for the runs of the examples on any of
//! them: the controller's region, the profile and configuration file that reach its Modal
//! workspace, and, for a smoke run on Modal, the GPU of a class there.

use std::error::Error;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Ec2,
    Modal,
}

/// The settings of live runs, as the configuration gives them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) name: String,
    /// The controller's region, which its EC2 sandboxes and their timers are in.
    pub(crate) region: String,
    /// What reaches the Modal workspace, for runs that may have sandboxes there.
    pub(crate) modal: Option<Modal>,
    /// For a smoke run on Modal, the model of the GPU that its GPU runner needs.
    pub(crate) gpu_model: Option<String>,
}

/// What reaches the Modal workspace of the controller's runs: its profile, and the configuration
/// file that holds it, if the configuration names one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Modal {
    pub(crate) profile: String,
    pub(crate) config: Option<PathBuf>,
}

impl Settings {
    /// Reads `text`, the configuration at `path`, for a smoke run on `provider`, whose GPU
    /// runner, on Modal, needs `gpu_model`, which must be the model of one of the classes of
    /// `[controller.modal]`; or, without a `provider`, for runs on any of the controller's
    /// providers, Modal's if it names `[controller.modal]`.
    pub(crate) fn parse(
        text: &str,
        path: &Path,
        provider: Option<Provider>,
        gpu_model: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let shown = path.display();
        let table: toml::Table = toml::from_str(text)
            .map_err(|error| format!("{shown} isn't a configuration: {error}"))?;
        let Some(controller) = table.get("controller").and_then(toml::Value::as_table) else {
            return Err(format!(
                "{shown} names no `[controller]`, which a live run goes through: name the one \
                 `deploy/aws` made, as the README says"
            )
            .into());
        };
        let name = self::text(controller, "name")
            .ok_or_else(|| format!("{shown}'s `[controller]` names no `name`: name it"))?;
        let region = self::text(controller, "region")
            .ok_or_else(|| format!("{shown}'s `[controller]` names no `region`: name it"))?;
        let modal = controller.get("modal").and_then(toml::Value::as_table);
        let (modal, gpu_model) = match (provider, modal) {
            (Some(Provider::Ec2), _) | (None, None) => (None, None),
            (None, Some(modal)) => (Some(Modal::parse(modal, path)?), None),
            (Some(Provider::Modal), None) => {
                return Err(format!(
                    "{shown} names no `[controller.modal]`, which a live run on Modal needs: \
                     name the workspace's profile and classes there, as the README says"
                )
                .into());
            }
            (Some(Provider::Modal), Some(modal)) => (
                Some(Modal::parse(modal, path)?),
                Some(gpu(modal, path, gpu_model)?),
            ),
        };
        Ok(Self {
            name,
            region,
            modal,
            gpu_model,
        })
    }
}

impl Modal {
    /// Reads `modal`, the `[controller.modal]` table of the configuration at `path`.
    fn parse(modal: &toml::Table, path: &Path) -> Result<Self, Box<dyn Error>> {
        let shown = path.display();
        let profile = text(modal, "profile")
            .ok_or_else(|| format!("{shown}'s `[controller.modal]` names no `profile`: name it"))?;
        let config = text(modal, "config").map(|config| {
            let dir = path.parent().unwrap_or(Path::new(""));
            dir.join(config)
        });
        Ok(Self { profile, config })
    }
}

/// `gpu_model`, which a smoke run's GPU runner needs, once it is the GPU of a class of `modal`,
/// the `[controller.modal]` table of the configuration at `path`.
fn gpu(
    modal: &toml::Table,
    path: &Path,
    gpu_model: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let shown = path.display();
    let classes = modal.get("classes").and_then(toml::Value::as_table);
    let mut models: Vec<String> = (classes.into_iter().flatten())
        .filter_map(|(_, class)| class.as_table().and_then(|class| text(class, "gpu_model")))
        .collect();
    models.sort();
    models.dedup();
    let offered = models.join("`, `");
    let Some(gpu_model) = gpu_model else {
        return Err(format!(
            "a live run on Modal needs the model of a GPU that a class of \
             `[controller.modal]` has, the cheapest best: give one of `{offered}` with \
             `--gpu-model`"
        )
        .into());
    };
    if !models.iter().any(|model| model == gpu_model) {
        return Err(format!(
            "no class of {shown}'s `[controller.modal]` has the GPU `{gpu_model}`: give one \
             of `{offered}`"
        )
        .into());
    }
    Ok(gpu_model.to_owned())
}

fn text(table: &toml::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}
