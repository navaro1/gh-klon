//! `gh klon list`: one line per klon with path, branch, short HEAD, a dirty flag,
//! the five C30 extras columns (disk, RSS, live processes, PR, checks), and the
//! three C24 radar columns: `<path> <branch> <head>[ *] | <disk> | <rss> |
//! <procs> | <pr> | <checks> | <vs-base> | <vs-siblings> | behind <n>`. The main
//! worktree is not a klon and never appears. `--json` prints the same rows with
//! the full HEAD, and `--no-gh` skips the pull request fetch.

use crate::envelope::env;
use crate::extras;
use crate::paths;
use crate::radar;
use crate::{git, Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
/// C30 extended `klon.list/1` with the disk, process, memory, and PR fields.
pub const SCHEMA: &str = "klon.list/2";

/// The `list` flags.
#[derive(clap::Args)]
pub struct Args {
    /// Skip every `gh` call: the pr and checks columns show `-`.
    #[arg(long)]
    pub no_gh: bool,
}

/// The `list --json` document.
#[derive(Serialize)]
struct Report {
    schema: &'static str,
    klons: Vec<Row>,
}

/// One klon. `branch` is null for a klon with a detached HEAD. The radar fields
/// `vs_base`, `vs_siblings`, and `behind` sit beside the others in the document,
/// and the C30 extras follow the envelope fields.
#[derive(Serialize)]
struct Row {
    path: PathBuf,
    branch: Option<String>,
    head: String,
    dirty: bool,
    locked: bool,
    /// The loopback address of the klon, from `<klon>/.klon/env` (R21). It is
    /// null for a klon that an older klon version created.
    ip: Option<String>,
    /// The unique bytes when the klon is a btrfs subvolume, else the size of
    /// the ignored directories, which only bounds the delta from above.
    disk_bytes: u64,
    /// True when `disk_bytes` comes from `btrfs fi du`.
    disk_exact: bool,
    /// The live processes of the klon.
    procs: usize,
    /// The resident memory of those processes, or the usage of the scope
    /// cgroup when one exists, in bytes. Zero when nothing runs.
    rss_bytes: u64,
    /// The pull request of the branch, or null when there is none or `gh`
    /// could not answer.
    pr: Option<u64>,
    /// `pass`, `fail`, `pending`, or `none` when the pull request runs no
    /// check. Null when there is no pull request or the answer named no rollup.
    checks: Option<String>,
    #[serde(flatten)]
    radar: radar::Row,
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
        // A klon from an older klon version has no env file and no name, so no
        // process of it can be recognized; its readings stay zero.
        let name = env::value(&path, "KLON_NAME");
        let extra = extras::measure(&path, name.as_deref());
        let pr = match (&branch, args.no_gh) {
            (Some(branch), false) => extras::pr_of(&golden, &common, branch),
            _ => None,
        };
        let (number, checks) = match &pr {
            Some(facts) => (Some(facts.number), facts.checks.clone()),
            None => (None, None),
        };
        rows.push(Row {
            head: head_of(&path, json),
            dirty: dirty(&path),
            ip: env::value(&path, "KLON_IP"),
            disk_bytes: extra.disk_bytes,
            disk_exact: extra.disk_exact,
            procs: extra.procs,
            rss_bytes: extra.rss_bytes,
            pr: number,
            checks,
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
                "{} {branch} {}{flag} {} {}",
                row.path.display(),
                row.head,
                extra_columns(row),
                row.radar.columns()
            );
        }
    }
    Ok(())
}

/// The five extras columns: disk, RSS, live processes, PR, checks. A dash
/// means klon measured nothing: no ignored directory, no live process, no
/// pull request, or a `gh` that did not answer.
fn extra_columns(row: &Row) -> String {
    let disk = if row.disk_bytes == 0 {
        "-".to_string()
    } else if row.disk_exact {
        extras::human(row.disk_bytes)
    } else {
        format!("≤ {}", extras::human(row.disk_bytes))
    };
    let rss = if row.rss_bytes == 0 {
        "-".to_string()
    } else {
        extras::human(row.rss_bytes)
    };
    let pr = row
        .pr
        .map(|n| format!("#{n}"))
        .unwrap_or_else(|| "-".to_string());
    let checks = row.checks.as_deref().unwrap_or("-");
    format!("| {disk} | {rss} | {} | {pr} | {checks}", row.procs)
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
