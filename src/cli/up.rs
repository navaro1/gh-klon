//! `gh klon up`: load `.klon.toml`, gate the commands, run `[warm] steps` in golden.

use crate::{config, git, spare, Error, Result};
use std::process::Command;

// TODO(C14): refuse a dirty golden or a golden not on `base`, run `git fetch origin`,
// and fast-forward with `git merge --ff-only`.

/// C10 runs the approved `[warm] steps` in golden with `sh -c`, in order.
/// C9 starts the spare builder after them (R40).
pub fn run(yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    let cfg = config::load(&golden)?;
    cfg.ensure_approved(yes, &["warm.steps"])?;
    let depth = cfg.spare;
    let steps = cfg.warm.and_then(|warm| warm.steps).unwrap_or_default();
    for step in steps {
        let status = Command::new("sh")
            .arg("-c")
            .arg(&step)
            .current_dir(&golden)
            .status()
            .map_err(Error::io(format!("run sh -c {step:?}")))?;
        if !status.success() {
            let why = match status.code() {
                Some(code) => format!("exit {code}"),
                None => "killed by a signal".to_string(),
            };
            return Err(Error::klon(format!("warm step failed ({why}): {step}")));
        }
    }
    spare::start_after(&golden, depth, false);
    Ok(())
}
