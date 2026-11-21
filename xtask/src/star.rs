//! The pinned formatter of `.star` files, buildifier, which `pumpkin fmt` and `pumpkin check` run
//! beside `pumpkin`, and `ci`'s check that the repository's own `.star` files are formatted.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use stdx::process::{self, Bound, Echo, Environment, Run, Stream};

use crate::repo::{Repo, STEP_TIMEOUT};
use crate::{Cargo, Result, tidy, tools};

/// Where the `.star` files are that [`check`] leaves as they are: the language's test fixtures,
/// whose expected outputs pin lines and columns, and the conformance files of other
/// implementations, which stay as their authors wrote them.
const UNCHECKED: &str = "crates/lang/tests/";

/// Where pumpkin's tests find the formatter, under any command that runs them: the link that
/// [`link`] makes, which `.cargo/config.toml` names.
const LINK: &str = env!("PUMPKIN_BUILDIFIER");

/// The pinned buildifier, fetched once into the target directory ([`tools::fetched`]).
pub(crate) fn buildifier(cargo: &Cargo) -> Result<PathBuf> {
    tools::fetched(cargo, tools::BUILDIFIER)
}

/// Makes [`LINK`] a symbolic link to the pinned buildifier, fetched if need be, replacing what is
/// there, and returns the pinned one's path.
pub(crate) fn link(cargo: &Cargo) -> Result<PathBuf> {
    let link = Path::new(LINK);
    let Some(dir) = link.parent() else {
        return Err(format!("PUMPKIN_BUILDIFIER names `{LINK}`: give the path of a file").into());
    };
    let buildifier = buildifier(cargo)?;
    fs::create_dir_all(dir).map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    // Made beside it, then renamed over it, so that a test never finds the link missing. The
    // target is absolute, so that the link in a copy of the tree, as cargo-mutants makes,
    // finds it too.
    let made = dir.join(format!(".buildifier.{}", std::process::id()));
    let linked =
        std::os::unix::fs::symlink(&buildifier, &made).and_then(|()| fs::rename(&made, link));
    if let Err(error) = linked {
        // Made and not renamed, or not made.
        let _ = fs::remove_file(&made);
        return Err(format!("linking {} failed: {error}", link.display()).into());
    }
    Ok(buildifier)
}

/// Puts `buildifier` beside `pumpkin`, where that `pumpkin` runs it, as `cargo xtask build`
/// puts it beside an installed one: a link to it, which replaces what is there.
pub(crate) fn place(buildifier: &Path, pumpkin: &Path) -> Result<()> {
    let beside = pumpkin.with_file_name("buildifier");
    match fs::remove_file(&beside) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            return Err(format!("removing {} failed: {error}", beside.display()).into());
        }
        Ok(()) | Err(_) => {}
    }
    fs::hard_link(buildifier, &beside)
        .map_err(|error| format!("linking {} failed: {error}", beside.display()).into())
}

/// Fails unless each of the repository's `.star` files, tracked or not yet, but not ignored, and
/// but those below [`UNCHECKED`], is formatted, as `pumpkin fmt --check` of `pumpkin`, beside
/// which [`place`] has put the formatter, says.
pub(crate) fn check(repo: &Repo<'_>, pumpkin: &Path) -> Result<()> {
    let files = tidy::files(repo, "star")?.into_iter();
    let files: Vec<String> = files.filter(|path| !path.starts_with(UNCHECKED)).collect();
    // With no file named, `fmt` would format the working directory, the fixtures included.
    if files.is_empty() {
        return Ok(());
    }
    let mut fmt = process::command(pumpkin, Environment::Inherited);
    fmt.current_dir(repo.root)
        .args(["fmt", "--check", "--"])
        .args(files);
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Line)
        .stdout(Stream::Inherited)
        .stderr(Stream::Inherited)
        .named("pumpkin fmt");
    process::run(&mut fmt, how).map_err(|failure| failure.to_string())?;
    Ok(())
}
