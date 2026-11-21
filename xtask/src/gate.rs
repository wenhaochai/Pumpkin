//! The gate of a release: a tag publishes only if every shipped example passed on its commit,
//! which `cargo xtask live examples --status` records there on GitHub as a commit status.

/// The context of the commit status that records whether the examples passed.
pub(crate) const CONTEXT: &str = "pumpkin/examples";

/// Fails, saying what to run, unless the latest [`CONTEXT`] status among `statuses`, GitHub's
/// list of the statuses of `commit` as JSON, newest first, is a success.
pub(crate) fn passed(statuses: &str, commit: &str) -> Result<(), String> {
    let listed: serde_json::Value = serde_json::from_str(statuses).map_err(|error| {
        format!("GitHub's statuses of {commit} aren't JSON ({error}): expected a list")
    })?;
    let Some(listed) = listed.as_array() else {
        return Err(format!(
            "GitHub's statuses of {commit} are {listed}: expected a list"
        ));
    };
    let again = format!("run `cargo xtask live examples --status` on {commit}");
    // GitHub lists a commit's statuses newest first, so a context's first is its latest:
    // https://docs.github.com/en/rest/commits/statuses#list-commit-statuses-for-a-reference
    match listed.iter().find(|status| status["context"] == CONTEXT) {
        Some(status) if status["state"] == "success" => Ok(()),
        Some(status) => Err(format!(
            "the latest `{CONTEXT}` status of {commit} is {} ({}): fix what failed, and {again}",
            status["state"], status["description"]
        )),
        None => Err(format!(
            "{commit} has no `{CONTEXT}` status, so its examples haven't passed there: {again}"
        )),
    }
}
