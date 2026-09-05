//! Subprocess wrapper around the installed `git`. klon never reimplements plumbing.

use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `git -C <cwd> <args>` and return its stdout. A non-zero exit becomes `Error::Git`.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(Error::io("run git"))?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| Error::klon("git output must be valid UTF-8"))
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Run `git` and ignore the result. Used on the cleanup path only.
pub fn run_quiet(cwd: &Path, args: &[&str]) {
    let _ = run(cwd, args);
}

/// One block of `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    /// `refs/heads/<name>` when the worktree has a branch checked out.
    pub branch: Option<String>,
}

/// Parse `git worktree list --porcelain`. The first entry is the main worktree.
pub fn worktree_list(cwd: &Path) -> Result<Vec<Worktree>> {
    let text = run(cwd, &["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    for block in text.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.to_string());
            }
        }
        if let Some(path) = path {
            list.push(Worktree { path, branch });
        }
    }
    Ok(list)
}

/// The absolute common directory: the output of `git rev-parse --git-common-dir`.
pub fn common_dir(cwd: &Path) -> Result<PathBuf> {
    let out = run(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(out.strip_suffix('\n').unwrap_or(&out)))
}

/// True when `refs/heads/<branch>` exists.
pub fn local_branch_exists(cwd: &Path, branch: &str) -> bool {
    let rev = format!("refs/heads/{branch}");
    run(cwd, &["show-ref", "--verify", "--quiet", &rev]).is_ok()
}
