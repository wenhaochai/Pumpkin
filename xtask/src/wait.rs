//! What `cargo xtask live` reads of what `./pumpkin` says as it follows and reaps a run.

/// The id of the run that `line` says `./pumpkin run` sent to a controller, if it is the line
/// that says so, `pumpkin: run <id> goes to controller ...`.
pub(crate) fn run_started(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("pumpkin: run ")?;
    let (run, _) = rest.split_once(" goes to controller ")?;
    Some(run)
}

/// Whether `line`, one that `./pumpkin wait` says, is the milestone of a lease of an agent's
/// that its runner is ready: `<grant> lease <n> of agent <m> ready`, `<grant>` the name of the
/// lease's grant, with ` with <GPUs>` after it for a lease of GPUs, as `pumpkin`'s
/// `progress::milestones` says it, naming the holder with `progress::lease_name`. A lease that a
/// call's target holds says `of job <j>` instead, which this doesn't count.
pub(crate) fn lease_ready(line: &str) -> bool {
    let Some((_, after)) = line.split_once(" lease ") else {
        return false;
    };
    let number = |word: Option<&str>| {
        word.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut words = after.split(' ');
    number(words.next())
        && words.next() == Some("of")
        && words.next() == Some("agent")
        && number(words.next())
        && words.next() == Some("ready")
        && matches!(words.next(), None | Some("with"))
}

/// Whether `stderr`, what a `./pumpkin reap` that failed said, puts the failure on other runs
/// only: it names at least one run, as `run <id>`, the way `reap` names each run that it can't
/// reap or whose provider it can't reach, and none of them is run `id`.
pub(crate) fn others_failed(stderr: &str, id: &str) -> bool {
    let named: Vec<&str> = (stderr.split("run ").skip(1))
        .filter_map(|after| {
            let run = after
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                .next()?;
            let parts: Vec<&str> = run.split('-').collect();
            // A run's id: its start's day and time, then 8 random hex digits.
            let shaped = matches!(parts[..], [day, time, random]
                if day.len() == 8 && time.len() == 6 && random.len() == 8
                    && run.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()));
            shaped.then_some(run)
        })
        .collect();
    !named.is_empty() && !named.contains(&id)
}
