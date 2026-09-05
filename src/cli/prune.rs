//! `gh klon prune`: `git worktree prune`, then a drained `.trash` directory.

use crate::git;
use crate::paths;
use crate::process;
use crate::Result;

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().map_err(crate::Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    git::run(&golden, &["worktree", "prune"])?;
    process::drain_trash(&paths::default_wt_root(&golden).join(".trash"))
}
