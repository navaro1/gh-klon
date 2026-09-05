//! `gh klon list`: one line per klon with path, branch, short HEAD, a dirty flag,
//! and the three C24 radar columns: `<path> <branch> <head>[ *] | <vs-base> |
//! <vs-siblings> | behind <n>`. The main worktree is not a klon and never appears.
//! `--json` prints the same rows with the full HEAD.

use crate::paths;
use crate::radar;
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

/// One klon. `branch` is null for a klon with a detached HEAD. The radar fields
/// `vs_base`, `vs_siblings`, and `behind` sit beside the others in the document.
#[derive(Serialize)]
struct Row {
    path: PathBuf,
    branch: Option<String>,
    head: String,
    dirty: bool,
    locked: bool,
    #[serde(flatten)]
    radar: radar::Row,
}

pub fn run(json: bool) -> Result<()> {
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
    let radar_rows = radar::scan(&golden, &common, &targets);
    let mut rows = Vec::new();
    for (worktree, radar) in worktrees.iter().skip(1).zip(radar_rows) {
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
            radar,
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
            println!(
                "{} {branch} {}{flag} {}",
                row.path.display(),
                row.head,
                row.radar.columns()
            );
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
