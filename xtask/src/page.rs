//! The type check of the page that `pumpkin view` serves: TypeScript's compiler
//! ([`tools::TSC`]) checks its modules against the types that their doc comments give, as the
//! `tsconfig.json` beside them says.

use stdx::process::{self, Bound, Echo, Environment, Failure, Run, Stream};

use crate::repo::STEP_TIMEOUT;
use crate::{Cargo, Result, root, tools};

/// The page's directory, from the workspace's root.
const PAGE: &str = "crates/pumpkin/src/view/page";

/// Fails unless the page's modules pass their type check, the compiler printing each error with
/// its line and column.
pub(crate) fn check(cargo: &Cargo) -> Result<()> {
    let tsc = tools::found(cargo, tools::TSC)?;
    let config = root().join(PAGE).join("tsconfig.json");
    let mut check = process::command(tsc, Environment::Inherited);
    check.current_dir(root()).arg("--project").arg(config);
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Line)
        .stdout(Stream::Inherited)
        .stderr(Stream::Inherited)
        .named("tsc");
    match process::run(&mut check, how) {
        Ok(_) => Ok(()),
        Err(Failure::Exited { .. }) => Err("the page's modules fail their type check, at the \
                                            lines that tsc printed above: fix them, then run \
                                            `cargo xtask ci` again"
            .into()),
        Err(failure) => Err(failure.to_string().into()),
    }
}
