//! `gh klon list`: one line per klon with path, branch, short HEAD, and a
//! dirty flag: `<path> <branch> <head>[ *]`. The main worktree is not a klon
//! and never appears. `--json` prints the same rows with the full HEAD.

use crate::paths;
use crate::{git, Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.list/1";

/// The `list --json` document.
#[derive(Serialize)]
struct Report {
    schema: &'static str,
    klons: Vec<Row>,
}

/// One klon. `branch` is null for a klon with a detached HEAD.
#[derive(Serialize)]
struct Row {
    path: PathBuf,
    branch: Option<String>,
    head: String,
    dirty: bool,
    locked: bool,
}

pub fn run(json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let mut rows = Vec::new();
    for worktree in worktrees.iter().skip(1) {
        let path = paths::absolute(&worktree.path)?;
        let branch = worktree
            .branch
            .as_deref()
            .and_then(|b| b.strip_prefix("refs/heads/"))
            .map(str::to_string);
        rows.push(Row {
            head: head_of(&path, json),
            dirty: dirty(&path),
            branch,
            locked: worktree.locked,
            path,
        });
    }
    if json {
        let report = Report {
            schema: SCHEMA,
            klons: rows,
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    } else {
        for row in &rows {
            let branch = row.branch.as_deref().unwrap_or("(detached)");
            let flag = if row.dirty { " *" } else { "" };
            println!("{} {branch} {}{flag}", row.path.display(), row.head);
        }
    }
    Ok(())
}

/// The HEAD of a klon: the full object name for JSON, the short one for a
/// person. A klon that git cannot read lists with a dash instead.
fn head_of(path: &Path, full: bool) -> String {
    let args: &[&str] = if full {
        &["rev-parse", "HEAD"]
    } else {
        &["rev-parse", "--short", "HEAD"]
    };
    git::run(path, args)
        .map(|out| out.trim().to_string())
        .unwrap_or_else(|_| "-".to_string())
}

/// True when `git status --porcelain` prints a line. A broken klon lists clean.
fn dirty(path: &Path) -> bool {
    matches!(git::run(path, &["status", "--porcelain"]), Ok(text) if !text.trim().is_empty())
}
