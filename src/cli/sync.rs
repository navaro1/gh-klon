//! `gh klon sync <branch> [--check]`: the dry run through the conflict radar.
//!
//! C24 ships `--check` only. Every other form of `sync` refuses.

use crate::{git, paths, radar, Error, Result};

// TODO(C14): implement the rest of `sync`: fetch once for the common directory,
// fast-forward a branch with no local divergence, else `rebase --autostash` or
// `merge` with `--merge`, refuse a force-pushed upstream that has unique local
// commits unless `--force`, and add `--onto <base>`, `--fresh`, and `--all`.

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the klon to report on.
    pub branch: String,
    /// Print the radar row for the klon and change nothing.
    #[arg(long)]
    pub check: bool,
}

pub fn run(args: Args) -> Result<()> {
    if !args.check {
        return Err(Error::klon(
            "sync is not implemented until C14; only `sync <branch> --check` works today",
        ));
    }
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = paths::absolute(
        &worktrees
            .first()
            .ok_or_else(|| Error::klon("not inside a git repository"))?
            .path,
    )?;
    let common = git::common_dir(&cwd)?;
    let targets = radar::targets(&worktrees);
    let which = targets
        .iter()
        .position(|target| target.branch == args.branch)
        .ok_or_else(|| Error::klon(format!("no klon has branch {} checked out", args.branch)))?;
    let path = paths::absolute(&targets[which].path)?;
    let row = radar::scan_one(&golden, &common, &targets, which);
    println!("{} {} {}", path.display(), args.branch, row.columns());
    Ok(())
}
