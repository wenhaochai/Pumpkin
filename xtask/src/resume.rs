//! What `cargo xtask live examples` keeps of each example that passed on a commit, so that a run
//! of the commit again runs only the examples that haven't.

use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use serde_json::{Value, json};

/// A run of an example that passed.
pub(crate) struct Passed {
    pub(crate) id: String,
    /// The `--arg` values that the example ran with.
    pub(crate) args: Vec<String>,
    pub(crate) elapsed: Duration,
    /// What the run metered in US dollars, if it did.
    pub(crate) usd: Option<f64>,
}

/// Keeps `passed`, the run of the example `name`, in `dir`, whole or not at all.
pub(crate) fn save(dir: &Path, name: &str, passed: &Passed) -> Result<(), String> {
    let kept = json!({
        "id": passed.id,
        "args": passed.args,
        "elapsed_seconds": passed.elapsed.as_secs(),
        "usd": passed.usd,
    });
    let path = dir.join(format!("{name}.json"));
    // A rename leaves a file that is whole or absent wherever this stops.
    let partial = dir.join(format!("{name}.json.partial"));
    fs::write(&partial, kept.to_string())
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|error| format!("writing {} failed: {error}", path.display()))
}

/// The run of the example `name` that `dir` keeps, unless it ran with other `args`, which
/// `None` also says of no run. Fails if what `dir` keeps of `name` can't be read.
pub(crate) fn read(dir: &Path, name: &str, args: &[String]) -> Result<Option<Passed>, String> {
    let path = dir.join(format!("{name}.json"));
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("reading {} failed: {error}", path.display())),
    };
    let invalid = || {
        format!(
            "{} isn't a run that `cargo xtask live examples` kept: remove it, and run this again",
            path.display()
        )
    };
    let kept: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    let strings = |value: &Value| {
        let strings = value.as_array()?.iter();
        strings
            .map(|arg| Some(arg.as_str()?.to_owned()))
            .collect::<Option<Vec<String>>>()
    };
    let (Some(id), Some(ran_with), Some(seconds)) = (
        kept["id"].as_str(),
        strings(&kept["args"]),
        kept["elapsed_seconds"].as_u64(),
    ) else {
        return Err(invalid());
    };
    let usd = match &kept["usd"] {
        Value::Null => None,
        usd => Some(usd.as_f64().ok_or_else(invalid)?),
    };
    // Bazel keeps a test's result and reruns the test only if it failed or its inputs changed:
    // https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/analysis/test/TestConfiguration.java#L106-L121
    // Here the inputs are the commit, which names `dir`, and the arguments, among them the model.
    if ran_with != args {
        return Ok(None);
    }
    Ok(Some(Passed {
        id: id.to_owned(),
        args: ran_with,
        elapsed: Duration::from_secs(seconds),
        usd,
    }))
}
