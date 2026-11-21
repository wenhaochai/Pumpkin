//! What `cargo xtask live` checks of the sandboxes that a run's record holds.

use std::path::Path;
use std::{fs, io};

/// What is wrong in the sandboxes of the record at `record`: each whose `sandbox.json` can't be
/// read, and each that isn't deleted. A directory below `agents`, `shared`, `jobs` or `leases`
/// that has no `sandbox.json` created no sandbox, so has nothing to check.
pub(crate) fn undeleted(record: &Path) -> Vec<String> {
    let mut wrong = Vec::new();
    let mut dirs = Vec::new();
    for kind in ["agents", "shared", "jobs", "leases"] {
        let path = record.join(kind);
        match fs::read_dir(&path) {
            Ok(entries) => dirs.extend(entries.flatten().map(|entry| entry.path())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => wrong.push(format!("reading {} failed: {error}", path.display())),
        }
    }
    dirs.sort();
    for dir in dirs {
        let path = dir.join("sandbox.json");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // As the record's reader skips it: `Record::sandboxes` in crates/runtime/src/record.rs,
            // which xtask can't link, as it would add the gateway and TLS to what xtask builds.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                wrong.push(format!("reading {} failed: {error}", path.display()));
                continue;
            }
        };
        let sandbox: serde_json::Value = match serde_json::from_str(&text) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                wrong.push(format!("{} isn't JSON: {error}", path.display()));
                continue;
            }
        };
        let deleted = sandbox["delete"]["state"].as_str().unwrap_or("not deleted");
        if deleted != "deleted" {
            wrong.push(format!(
                "sandbox {} on {} is {deleted}",
                sandbox["id"], sandbox["provider"]
            ));
        }
    }
    wrong
}
