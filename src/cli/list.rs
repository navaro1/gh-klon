//! `gh klon list`: one line per klon with path, branch, short HEAD, a dirty flag,
//! the five C30 extras columns (disk, RSS, live processes, PR, checks), the C12
//! warm column, the C26 receipt column, the C27 claim column, and the three
//! C24 radar columns:
//! `<path> <branch> <head>[ *] | <disk> | <rss> | <procs> | <pr> | <checks> |
//! <warm> | <receipt> | <claims> | <vs-base> | <vs-siblings> | behind <n>`. The radar
//! columns close the line, so a reader that cuts the last three fields keeps
//! working. The main worktree is not a klon and never appears. `--json` prints
//! the same rows with the full HEAD, and `--no-gh` skips the pull request
//! fetch.

use crate::claims;
use crate::envelope::env;
use crate::extras;
use crate::paths;
use crate::radar;
use crate::receipt;
use crate::warm;
use crate::{config, git, hibernate, Error, Result};
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
    /// The ignored directories a detached warm process still owes the klon
    /// (C12). It is empty once every directory landed.
    warming: Vec<String>,
    /// True for a klon that `gh klon hibernate` put away (C29). Its directory
    /// is gone; its work sits on `refs/klon/hibernate/<name>`.
    hibernated: bool,
    /// The C26 check receipt of the klon's HEAD: `pass`, `failed`, or `stale`.
    /// It is null when the repository configures no `[proof] steps`, when
    /// nothing has ever checked the branch, or when klon cannot read the
    /// receipt directory.
    receipt: Option<&'static str>,
    /// The paths the klon owns (C27), in the order it took them. It is empty
    /// for a klon that never ran `claim`, and for a claim table klon cannot
    /// read.
    claims: Vec<String>,
    /// True when a claim of this klon conflicts with a claim of another. The
    /// append refuses such a pair, so this can only follow a hand-edited file
    /// or a klon that wrote without the lock.
    claim_overlap: bool,
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
    // The receipt column compares against the steps that `.klon.toml` names
    // now, so the hash is read once for the whole list. A repository with no
    // `[proof] steps` has nothing to prove, and every receipt column is then
    // null.
    let proof_steps = config::load(&golden)
        .ok()
        .and_then(|cfg| cfg.proof.and_then(|proof| proof.steps))
        .unwrap_or_default();
    let steps_hash = (!proof_steps.is_empty()).then(|| receipt::steps_hash(&proof_steps));
    // The claim table is read once for the whole list. A table klon cannot
    // read costs the two claim fields and one stderr line, never the list:
    // every other column still answers.
    let table = claims::load(&common).unwrap_or_else(|err| {
        eprintln!("{err}");
        claims::Table::empty()
    });
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
        let (claimed, overlap) = claims_of(&table, branch.as_deref());
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
            warming: warm::pending(&path),
            receipt: receipt_of(
                &common,
                worktree.head.as_deref(),
                branch.as_deref(),
                steps_hash.as_deref(),
            ),
            claims: claimed,
            claim_overlap: overlap,
            branch,
            locked: worktree.locked,
            hibernated: false,
            path,
            radar,
        });
    }
    // C29: a hibernated klon has no directory, so `git worktree list` cannot
    // show it. Its record can, and a person who forgot a klon needs to see it.
    // The radar cannot measure a tree that is not there, so those columns stay
    // unknown.
    for record in hibernate::list(&common)? {
        if hibernate::is_awake(&worktrees, &record) {
            continue;
        }
        // A hibernated klon keeps its claims: its work comes back, and the
        // paths it owns must still be its own when it does.
        let (claimed, overlap) = claims_of(&table, Some(&record.branch));
        rows.push(Row {
            head: short_head(&record.head, json),
            dirty: false,
            ip: record.ip,
            // Nothing to measure: the directory is gone, so it costs no disk
            // outside the object store and runs no process. The pull request
            // is left out too, because `list` reads it per live klon and a
            // sleeping klon is not one a reader is about to work in.
            disk_bytes: 0,
            disk_exact: false,
            procs: 0,
            rss_bytes: 0,
            pr: None,
            checks: None,
            warming: Vec::new(),
            // The receipt is not a measurement of the tree: it sits in the
            // common directory and survives the hibernation, so the sleeping
            // klon still reports what its recorded HEAD proved.
            receipt: receipt_of(
                &common,
                Some(record.head.as_str()),
                Some(record.branch.as_str()),
                steps_hash.as_deref(),
            ),
            claims: claimed,
            claim_overlap: overlap,
            branch: Some(record.branch),
            locked: false,
            hibernated: true,
            path: record.path,
            radar: radar::Row::unknown(),
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
            if row.hibernated {
                // `zz` is the marker of a sleeping klon: no HEAD flag, no radar,
                // one word that says why the directory is missing. A sleeping
                // klon keeps its claims, so the claim column still prints:
                // an overlap that only a sleeping klon carries must be visible.
                let claims = match row.claims.is_empty() {
                    true => String::new(),
                    false => format!(" | {}", claim_column(row)),
                };
                println!("{} {branch} zz hibernated{claims}", row.path.display());
                continue;
            }
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

/// The five extras columns plus the warm one: disk, RSS, live processes, PR,
/// checks, and the directories a warm process still owes. A dash means klon
/// measured nothing: no ignored directory, no live process, no pull request, a
/// `gh` that did not answer, or a klon with nothing left to warm.
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
    let warm = match row.warming.is_empty() {
        true => "-".to_string(),
        false => format!("warming {}", row.warming.join(",")),
    };
    let receipt = row
        .receipt
        .map(mark)
        .unwrap_or_else(|| receipt::Verdict::Missing.column());
    let claims = claim_column(row);
    format!(
        "| {disk} | {rss} | {} | {pr} | {checks} | {warm} | {receipt} | {claims}",
        row.procs
    )
}

/// The C27 column: the number of owned paths, and a `!` when one of them is
/// also owned by another klon. A klon that claimed nothing shows a dash.
fn claim_column(row: &Row) -> String {
    match (row.claims.len(), row.claim_overlap) {
        (0, _) => "-".to_string(),
        (count, false) => count.to_string(),
        (count, true) => format!("{count}!"),
    }
}

/// The claim fields of one klon: the paths it owns, and whether one of them
/// overlaps another klon's. A klon with a detached HEAD has no branch, so
/// nothing names it in the table and it owns nothing.
fn claims_of(table: &claims::Table, branch: Option<&str>) -> (Vec<String>, bool) {
    match branch {
        Some(branch) => (table.paths_of(branch), table.overlaps(branch)),
        None => (Vec::new(), false),
    }
}

/// The `list` mark of a receipt verdict. The JSON name and the mark stay in
/// one place, so the two can never drift apart.
fn mark(name: &str) -> &'static str {
    for verdict in [
        receipt::Verdict::Pass,
        receipt::Verdict::Failed,
        receipt::Verdict::Stale,
    ] {
        if verdict.json() == Some(name) {
            return verdict.column();
        }
    }
    receipt::Verdict::Missing.column()
}

/// The C26 receipt verdict of one klon. A klon with no HEAD, no branch, or a
/// repository with no `[proof] steps` has nothing to prove and reports null.
/// A receipt directory that klon cannot read costs the column, never the list.
fn receipt_of(
    common: &Path,
    head: Option<&str>,
    branch: Option<&str>,
    steps_hash: Option<&str>,
) -> Option<&'static str> {
    let (head, branch, steps_hash) = (head?, branch?, steps_hash?);
    receipt::verdict(common, head, branch, steps_hash)
        .ok()?
        .json()
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

/// The recorded HEAD of a hibernated klon: the full object name for JSON, the
/// first seven characters for a person, as `git rev-parse --short` gives.
fn short_head(head: &str, full: bool) -> String {
    match full {
        true => head.to_string(),
        false => head[..head.len().min(7)].to_string(),
    }
}

/// True when `git status --porcelain` prints a line. A broken klon lists clean.
fn dirty(path: &Path) -> bool {
    matches!(git::run(path, &["status", "--porcelain"]), Ok(text) if !text.trim().is_empty())
}
