//! The `sandboxd` that every sandbox of a run runs, found and checked before the engine is
//! reached ([`Sandboxd`]).

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// What is wrong with a file that isn't a 64-bit ELF file, or one too short for its header.
const NOT_ELF: &str = "isn't a 64-bit ELF executable";

/// A `sandboxd` that a sandbox can run, by its absolute path, which a provider can hand on from
/// any directory: a regular file that every user may execute, a 64-bit ELF executable linked
/// statically, with no interpreter.
pub(crate) struct Sandboxd(PathBuf);

impl Sandboxd {
    /// The `sandboxd` at `named`, or, without one, beside the `pumpkin` executable that runs.
    /// Fails if the file isn't a `sandboxd` that a sandbox can run.
    pub(crate) fn find(named: Option<PathBuf>) -> Result<Self, String> {
        if let Some(path) = named {
            let replace = "name a static one, which `cargo xtask sandboxd` builds";
            let checked = check(&path, replace);
            checked.map_err(|wrong| format!("the sandboxd {} {wrong}", path.display()))?;
            let absolute = std::path::absolute(&path).map_err(|error| {
                format!(
                    "the sandboxd {} can't be made absolute ({error}): name it by its absolute \
                     path",
                    path.display()
                )
            })?;
            return Ok(Self(absolute));
        }
        let pumpkin = crate::workspace::exe("name a static sandboxd with `--sandboxd`")?;
        let path = pumpkin.with_file_name(install::File::Sandboxd.name());
        let replace = "`cargo xtask build` puts `pumpkin` beside a static one, or name \
                       one with `--sandboxd`";
        check(&path, replace).map_err(|wrong| {
            format!("the sandboxd beside `pumpkin`, {}, {wrong}", path.display())
        })?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

/// What is wrong with the file at `path` as a [`Sandboxd`], if anything, then what to do:
/// `replace`, for a file that another must replace.
fn check(path: &Path, replace: &str) -> Result<(), String> {
    let replaced = |wrong: String| format!("{wrong}: {replace}");
    let unreadable = |error: io::Error| replaced(format!("can't be read ({error})"));
    // Before it is opened, as opening a FIFO would wait for a writer.
    let metadata = fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(replaced("isn't a regular file".to_owned()));
    }
    let mode = metadata.permissions().mode();
    // A sandbox's processes run as its role's user, whom neither the file's owner nor its group
    // need be.
    if mode & 0o001 == 0 {
        return Err(format!(
            "can't be executed by every user, as its mode is {:04o}: make it executable by every \
             user (`chmod a+x`)",
            mode & 0o7777
        ));
    }
    let file = File::open(path).map_err(unreadable)?;
    statically_linked(&file).map_err(replaced)
}

/// What is wrong with `file` as a 64-bit ELF executable with no interpreter, if anything.
fn statically_linked(file: &File) -> Result<(), String> {
    let magic: [u8; 5] = read_at(file, 0)?;
    if magic != *b"\x7fELF\x02" {
        return Err(NOT_ELF.to_owned());
    }
    // The ELF header's `e_phoff`, `e_phentsize` and `e_phnum`: where the program headers are,
    // the size of each, and how many there are.
    let offset = u64::from_le_bytes(read_at(file, 32)?);
    let size = u64::from(u16::from_le_bytes(read_at(file, 54)?));
    let count = u64::from(u16::from_le_bytes(read_at(file, 56)?));
    // An interpreter would load libraries that a role's image need not have.
    for index in 0..count {
        // Each program header begins with its type, `p_type`.
        let at = offset.saturating_add(index * size);
        if u32::from_le_bytes(read_at(file, at)?) == libc::PT_INTERP {
            return Err("is linked dynamically, and a sandbox runs only a static one".to_owned());
        }
    }
    Ok(())
}

fn read_at<const N: usize>(file: &File, at: u64) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    file.read_exact_at(&mut bytes, at).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            NOT_ELF.to_owned()
        } else {
            format!("can't be read ({error})")
        }
    })?;
    Ok(bytes)
}
