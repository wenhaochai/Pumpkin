//! A release ships a launcher, its one-line pin and the static archive named by the pin.
//! Publication checks all three against the tag before attaching them to a draft.

use std::fs;
use std::path::Path;

use stdx::Context as _;
use xshell::{Shell, cmd};

use crate::{Cargo, MUSL, Result, build, gate, root};

struct Release<'a> {
    tag: &'a str,
    commit: String,
    archive: String,
    launcher: Vec<u8>,
}

impl Release<'_> {
    fn pin(&self, sh: &Shell) -> Result<String> {
        let archive = &self.archive;
        let sum = cmd!(sh, "sha256sum {archive}").read()?;
        let (digest, _) = sum
            .split_once(' ')
            .ok_or("sha256sum omitted the archive name")?;
        Ok(format!("{} {} {digest}\n", self.tag, self.commit))
    }
}

pub(super) fn run(cargo: &Cargo, tag: &str, publish: bool) -> Result<()> {
    if !tag.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(format!(
            "unsupported release tag {tag:?}: use letters, digits, dots, underscores and hyphens, \
             starting with a letter or digit"
        )
        .into());
    }

    let sh = cargo.sh.clone();
    let revision = if publish {
        format!("refs/tags/{tag}^{{commit}}")
    } else {
        "HEAD".to_owned()
    };
    let commit = cmd!(sh, "git rev-parse --verify {revision}").read()?;
    let launcher = cmd!(sh, "git show {commit}:deploy/pumpkin")
        .output()?
        .stdout;
    let release = Release {
        tag,
        commit,
        archive: format!("pumpkin-{tag}-{MUSL}.tar.xz"),
        launcher,
    };

    let output = root().join("target/release-assets").join(tag);
    if publish {
        // The latest 100, as a page holds at most that many; an older one counts for nothing.
        let commit = &release.commit;
        let statuses = format!("repos/{{owner}}/{{repo}}/commits/{commit}/statuses?per_page=100");
        let statuses = cmd!(sh, "gh api {statuses}").read()?;
        gate::passed(&statuses, &release.commit)?;
        sh.change_dir(output);
        return publish_assets(&sh, &release);
    }

    sh.create_dir(&output)?;
    sh.change_dir(&output);
    // Stage beside the outputs so a checked archive can replace a previous one by rename.
    let scratch = cmd!(sh, "mktemp --directory .build.XXXXXXXX").read()?;
    sh.change_dir(scratch);
    let built = package(cargo, &sh, &output, &release);
    let cleaned = sh.remove_path(sh.current_dir());
    built?;
    Ok(cleaned?)
}

fn package(cargo: &Cargo, sh: &Shell, output: &Path, release: &Release<'_>) -> Result<()> {
    let archive = &release.archive;
    let commit = &release.commit;
    build(cargo, Some(sh.current_dir().join("dist")), MUSL)?;
    sh.write_file("dist/commit", format!("{commit}\n"))?;
    sh.copy_file(
        root().join("deploy/buildifier-LICENSE"),
        "dist/buildifier-LICENSE",
    )?;
    // The upstream formatter includes compressed Go debug sections in its release binary.
    cmd!(sh, "strip --strip-debug dist/bin/buildifier").run()?;

    cmd!(sh, "tar --create --file {archive} --directory dist")
        .args([
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
        ])
        .arg("--mode=u=rwX,go=rX")
        .args(["--use-compress-program", "xz --threads=1"])
        .args(["bin", "buildifier-LICENSE", "commit"])
        .run()?;
    sh.create_dir("unpacked")?;
    cmd!(
        sh,
        "tar --extract --xz --file {archive} --directory unpacked"
    )
    .run()?;
    check(sh, commit)?;

    sh.write_file("pumpkin", &release.launcher)?;
    cmd!(sh, "chmod 0755 pumpkin").run()?;
    sh.write_file("pumpkinversion", release.pin(sh)?)?;
    cmd!(
        sh,
        "mv --target-directory {output} pumpkin pumpkinversion {archive}"
    )
    .run()?;
    Ok(())
}

fn check(sh: &Shell, commit: &str) -> Result<()> {
    let _dir = sh.push_dir("unpacked");
    if sh.read_file("commit")? != format!("{commit}\n") {
        return Err(format!("archived commit differs from the checkout's {commit}").into());
    }
    for program in ["pumpkin", "sandboxd", "ec2-boot", "buildifier"] {
        let elf = cmd!(sh, "file --brief bin/{program}")
            .env("LC_ALL", "C")
            .read()?;
        if !(elf.starts_with("ELF 64-bit")
            && elf.contains("x86-64")
            && ["statically linked", "static-pie linked", "static linked"]
                .iter()
                .any(|kind| elf.contains(kind)))
        {
            return Err(format!("bin/{program} is {elf}: expected a static x86-64 ELF").into());
        }
    }
    for helper in ["pumpkin-modal.py", "pumpkin-modal.py.lock"] {
        let path = sh.current_dir().join("bin").join(helper);
        if fs::metadata(&path).context(path.display())?.len() == 0 {
            return Err(format!("archived bin/{helper} is empty").into());
        }
    }
    cmd!(sh, "bin/buildifier --version").run()?;
    let version = cmd!(sh, "bin/pumpkin --version").read()?;
    if !version.contains(&format!("({commit:.9} ")) {
        return Err(
            format!("archived pumpkin reports {version:?}: expected commit {commit}").into(),
        );
    }
    println!("{version}");
    Ok(())
}

fn publish_assets(sh: &Shell, release: &Release<'_>) -> Result<()> {
    let Release {
        tag,
        commit,
        archive,
        launcher,
    } = release;
    if sh.read_binary_file("pumpkin")? != *launcher {
        return Err(format!("pumpkin differs from deploy/pumpkin at tag {tag}").into());
    }
    if sh.read_file("pumpkinversion")? != release.pin(sh)? {
        return Err(
            format!("pumpkinversion differs from the pin for tag {tag} and {archive}").into(),
        );
    }
    let archived = cmd!(sh, "tar --extract --to-stdout --xz --file {archive} commit").output()?;
    if archived.stdout != format!("{commit}\n").as_bytes() {
        return Err(format!("the commit in {archive} differs from tag {tag} ({commit})").into());
    }
    cmd!(
        sh,
        "gh release create {tag} --draft --verify-tag --title {tag} --generate-notes"
    )
    .run()?;
    cmd!(
        sh,
        "gh release upload {tag} pumpkin pumpkinversion {archive}"
    )
    .run()?;
    cmd!(sh, "gh release edit {tag} --draft=false --latest=true").run()?;
    Ok(())
}
