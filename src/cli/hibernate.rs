//! `gh klon hibernate <branch>`: save the work of a klon in the object store
//! and give the working directory back to the filesystem (spec §7 C29, R28).
//!
//! The command is the safe removal of `rm --force` with one step in front of
//! it: klon writes the tracked changes and the untracked, non-ignored files to
//! one commit on `refs/klon/hibernate/<name>` and one small record, both before
//! the tree goes. `gh klon wake <branch>` puts the klon back.
//!
//! The branch stays. Only the directory goes.

use crate::{git, hibernate, paths, Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.hibernate/1";

/// The `hibernate --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    branch: &'a str,
    head: &'a str,
    /// The commit that holds the tracked changes and the untracked files.
    work: &'a str,
}

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the klon to hibernate.
    pub branch: String,
    /// Start no hot-spare builder after the removal.
    #[arg(long)]
    pub no_spare: bool,
}

pub fn run(args: Args, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = paths::absolute(
        &worktrees
            .first()
            .ok_or_else(|| Error::klon("not inside a git repository"))?
            .path,
    )?;
    let common = git::common_dir_of_main(&golden)?;
    let target = resolve(&worktrees, &golden, &args.branch)?;

    // A lock protects a klon from removal on purpose, and `hibernate` removes.
    if worktrees
        .iter()
        .any(|w| w.locked && same_dir(&w.path, &target))
    {
        return Err(Error::klon(format!(
            "{} is locked; unlock it with git worktree unlock first",
            target.display()
        )));
    }
    // A build that still runs in the klon would lose its tree under it. Unlike
    // `rm`, `hibernate` has no `--force`: the whole point is to keep the work,
    // and a half-written build output is not work worth keeping.
    hibernate::refuse_live(&target)?;

    let record = hibernate::hibernate(
        &golden,
        &common,
        &worktrees,
        &target,
        &args.branch,
        args.no_spare,
    )?;
    if json {
        let report = Report {
            schema: SCHEMA,
            path: &record.path,
            branch: &record.branch,
            head: &record.head,
            work: &record.work,
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    } else {
        println!(
            "hibernated {} at {}; run gh klon wake {} to bring it back",
            record.branch,
            record.path.display(),
            record.branch
        );
    }
    Ok(())
}

/// The registered worktree that has `branch` checked out.
fn resolve(worktrees: &[git::Worktree], golden: &Path, branch: &str) -> Result<PathBuf> {
    let full = format!("refs/heads/{branch}");
    let path = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(full.as_str()))
        .map(|w| w.path.clone())
        .ok_or_else(|| Error::klon(format!("no klon has the branch {branch} checked out")))?;
    let path = paths::absolute(&path)?;
    if path == golden {
        return Err(Error::klon(format!(
            "{} is the repository root; hibernate never removes it",
            path.display()
        )));
    }
    Ok(path)
}

/// Compare two worktree paths after making both absolute.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (paths::absolute(a), paths::absolute(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
