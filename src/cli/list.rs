//! `gh klon list`: one line per klon with path, branch, short HEAD, and a
//! dirty flag: `<path> <branch> <head>[ *]`. The main worktree is not a klon
//! and never appears.

use crate::paths;
use crate::{git, Error, Result};

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    for worktree in worktrees.iter().skip(1) {
        let path = paths::absolute(&worktree.path)?;
        let branch = worktree
            .branch
            .as_deref()
            .and_then(|b| b.strip_prefix("refs/heads/"))
            .unwrap_or("(detached)");
        let head = git::run(&path, &["rev-parse", "--short", "HEAD"])
            .map(|out| out.trim().to_string())
            .unwrap_or_else(|_| "-".to_string());
        let dirty = match git::run(&path, &["status", "--porcelain"]) {
            Ok(status) if !status.trim().is_empty() => " *",
            _ => "", // A clean or broken klon lists without a dirty flag.
        };
        println!("{} {branch} {head}{dirty}", path.display());
    }
    Ok(())
}
