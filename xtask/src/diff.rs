use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::repo::Repo;

/// The diff of the working tree against `base`, below `paths` alone if there are any. Fails if the
/// working tree holds an untracked Rust file that isn't ignored, which the diff would leave out,
/// saying to add it and run `again`.
pub(crate) fn since(repo: &Repo<'_>, base: &str, paths: &[PathBuf], again: &str) -> Result<String> {
    let others = repo.git(["ls-files", "-z", "--others", "--exclude-standard"])?;
    let untracked: Vec<String> = (others.split('\0'))
        .filter(|path| Path::new(path).extension() == Some("rs".as_ref()))
        .map(|path| repo.root.join(path).display().to_string())
        .collect();
    if !untracked.is_empty() {
        let untracked = untracked.join(" ");
        return Err(format!(
            "the diff would leave out untracked Rust files: add them to it with \
             `git add --intent-to-add -- {untracked}`, then run `{again}` again"
        )
        .into());
    }
    // No external diff or colour, whatever the repository configures, as cargo-mutants reads it.
    let args: [&OsStr; 5] = ["diff", "--no-ext-diff", "--no-color", base, "--"].map(AsRef::as_ref);
    let mut diff = repo.git(args.into_iter().chain(paths.iter().map(AsRef::as_ref)))?;
    // [`Repo::git`] drops the newline that ends the diff's last line.
    diff.push('\n');
    Ok(diff)
}
