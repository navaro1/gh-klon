//! `gh klon pr <branch> [-- <extra...>]`: run `gh pr create --head <branch>`
//! from inside the klon (handoff §3).

use crate::{git, Error, Result};
use std::process::Command;

#[derive(clap::Args)]
pub struct Args {
    /// A branch that a klon has checked out.
    pub branch: String,
    /// Extra arguments for `gh pr create`, after `--`.
    #[arg(last = true, num_args = 0..)]
    pub extra: Vec<String>,
}

pub fn run(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let full = format!("refs/heads/{}", args.branch);
    let path = git::worktree_list(&cwd)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(&full))
        .map(|w| w.path)
        .ok_or_else(|| {
            Error::klon(format!(
                "no klon has the branch {} checked out",
                args.branch
            ))
        })?;
    // `gh pr create` prompts for the title and body; keep its stdio on the
    // terminal so the prompts and the printed pull request URL arrive.
    let status = Command::new("gh")
        .args(["pr", "create", "--head"])
        .arg(&args.branch)
        .args(&args.extra)
        .current_dir(&path)
        .status()
        .map_err(Error::io("run gh"))?;
    if !status.success() {
        return Err(Error::Git {
            code: status.code().unwrap_or(1),
            stderr: "gh pr create failed\n".into(),
        });
    }
    Ok(())
}
