//! The members' keys: the list that the controller's admin keeps in the SSM parameter
//! `/pumpkin-<name>/members`, a line `<member> <key>` for each key, copied into [`MEMBERS`], the
//! only keys that `sshd` admits for the controller's user, each with the forced command that names
//! its member ([`link::forced`]). A list that fails to parse changes nothing.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::ExitCode;

use aws::{Cli, Region};
use stdx::Context as _;

use crate::controller::Name;
use crate::member::Member;
use crate::{link, say};

/// The file of the members' keys, which `sshd` reads.
pub(super) const MEMBERS: &str = "/etc/ssh/pumpkin/members";

// The length of an ed25519 public key's blob in base64: 51 bytes, unpadded.
const BLOB_CHARS: usize = 68;

/// Copies the members' keys of controller `name`, in `region`, as the module says, and returns 0.
pub(crate) fn members(name: &Name, region: &Region) -> Result<ExitCode, String> {
    copy(name, region)?;
    Ok(ExitCode::SUCCESS)
}

/// Copies the members' keys, as the module says, and says so if they changed.
pub(super) fn copy(name: &Name, region: &Region) -> Result<(), String> {
    let parameter = name.parameter("members");
    let list = crate::job::parameter_value(&Cli::connect(region.clone())?, &parameter)?;
    let keys = keys(&list).map_err(|why| {
        format!("the members' keys, SSM parameter `{parameter}`, {why}, so none is copied")
    })?;
    // Searchable by the controller's user, as `sshd` reads the file as that user.
    let dir = Path::new(MEMBERS).parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).context(dir.display())?;
    let mode = fs::metadata(dir)
        .context(dir.display())?
        .permissions()
        .mode();
    if mode & 0o7777 != 0o755 {
        let set = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        set.context(dir.display())?;
        say(format_args!("made {} 0755", dir.display()));
    }
    super::wrote(Path::new(MEMBERS), keys.as_bytes(), 0o644)?;
    Ok(())
}

/// The `authorized_keys` lines of the members' `list`, each key's forced command naming its member,
/// whose name then leads the key's comment, so that `ssh-keygen -l` names each key's member.
fn keys(list: &str) -> Result<String, String> {
    let mut keys = String::new();
    let mut blobs = BTreeSet::new();
    for (index, line) in list.lines().enumerate() {
        let at = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let (name, key) = line.split_once(' ').unwrap_or((line, ""));
        let member =
            Member::try_from(name.to_owned()).map_err(|why| format!("line {at}: {why}"))?;
        let mut words = key.splitn(3, ' ');
        let (kind, blob, comment) = (words.next(), words.next(), words.next());
        let base64 = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/';
        let fits = kind == Some("ssh-ed25519")
            && blob.is_some_and(|blob| blob.len() == BLOB_CHARS && blob.bytes().all(base64))
            && comment
                .is_none_or(|comment| comment.bytes().all(|byte| (b' '..=b'~').contains(&byte)));
        let Some(blob) = blob.filter(|_| fits) else {
            return Err(format!(
                "line {at}: member `{member}`'s key is not an ed25519 public key: expected \
                 `ssh-ed25519 <{BLOB_CHARS} characters of base64>` and an optional comment of \
                 printable ASCII, as `~/.config/pumpkin/ssh/key.pub` holds"
            ));
        };
        if !blobs.insert(blob) {
            return Err(format!(
                "line {at}: member `{member}`'s key is listed before, so it would name no one \
                 member: list each key once"
            ));
        }
        // `restrict` turns off forwarding, ptys and `~/.ssh/rc`. A member's name and the forced
        // command hold no `"` or `\`, but the command's own quotes.
        let command = link::forced(&member).replace('"', "\\\"");
        let comment = comment.map_or_else(String::new, |comment| format!(" {comment}"));
        let _ = writeln!(
            keys,
            "restrict,command=\"{command}\" ssh-ed25519 {blob} {member}{comment}"
        );
    }
    Ok(keys)
}
