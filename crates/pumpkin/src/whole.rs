//! Files written whole or not at all, whenever the machine fails: each is written and synced
//! aside, in its own directory, renamed into place, and then the directory is synced, so that a
//! reader finds the old file or the new one, never a part.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use stdx::Context as _;

/// Replaces `path` with a file of `bytes` and permission bits `mode`, whole.
pub(crate) fn replace(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut aside = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(dir)
        .context(dir.display())?;
    // Set apart from the creation, which the umask would narrow.
    let written = aside
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .and_then(|()| aside.write_all(bytes))
        .and_then(|()| aside.as_file().sync_all());
    written.context(aside.path().display())?;
    aside.persist(path).context(path.display())?;
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .context(dir.display())
}

/// Replaces `path` as [`replace`] does, unless it holds `bytes` with permission bits `mode`
/// already. True if it replaced it.
pub(crate) fn converge(path: &Path, bytes: &[u8], mode: u32) -> Result<bool, String> {
    match fs::read(path) {
        Ok(held) if held == bytes => {
            let held_mode = fs::metadata(path)
                .context(path.display())?
                .permissions()
                .mode();
            if held_mode & 0o7777 == mode {
                return Ok(false);
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context(path.display()),
    }
    replace(path, bytes, mode)?;
    Ok(true)
}
