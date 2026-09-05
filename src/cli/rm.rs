//! `gh klon rm (<branch> | --path <p>) [--force]`: the safe removal from handoff §7.
//! The klon is renamed into `.trash` and deleted in the background, so the
//! command returns at once. The branch is never deleted.

use crate::journal::{self, State};
use crate::paths;
use crate::process;
use crate::{git, Error, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.rm/1";

/// The `rm --json` document. `trash` is null when the klon did not reach the
/// trash directory: the cross-filesystem fallback deletes it in place.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    branch: Option<&'a str>,
    trash: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct Args {
    /// A branch that a klon has checked out.
    pub branch: Option<String>,
    /// The klon path. It must match a registered worktree.
    #[arg(long, conflicts_with = "branch")]
    pub path: Option<PathBuf>,
    /// Remove a dirty klon or one with live processes.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: Args, json: bool) -> Result<()> {
    if args.branch.is_none() && args.path.is_none() {
        return Err(Error::klon("name a branch or a path with --path"));
    }
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = paths::absolute(
        &worktrees
            .first()
            .ok_or_else(|| Error::klon("not inside a git repository"))?
            .path,
    )?;
    // The journal lives under the common directory. `rm` derives it from
    // golden instead of a second `git` process, because of the 100 ms budget.
    let common = git::common_dir_of_main(&golden)?;
    let target = resolve(&worktrees, &golden, &args)?;
    let branch = worktrees
        .iter()
        .find(|w| same_dir(&w.path, &target))
        .and_then(|w| w.branch.as_deref())
        .and_then(|b| b.strip_prefix("refs/heads/"))
        .map(str::to_string);

    // Step 1: refuse protected places before anything else.
    refuse_reserved(&target, &golden)?;
    // A lock protects a klon from removal on purpose; even --force honours it.
    if worktrees
        .iter()
        .any(|w| w.locked && same_dir(&w.path, &target))
    {
        return Err(Error::klon(format!(
            "{} is locked; unlock it with git worktree unlock first",
            target.display()
        )));
    }

    // Steps 2 and 3: refuse a dirty tree and a tree with live processes.
    if target.exists() && !args.force {
        if process::dirty(&target)? {
            return Err(Error::klon(format!(
                "{} is dirty; use --force to remove it",
                target.display()
            )));
        }
        if let Some(pid) = process::live_process(&target) {
            return Err(Error::klon(format!(
                "{} has a live process (pid {pid}); use --force to remove it",
                target.display()
            )));
        }
    }

    // Steps 4 to 6: rename into .trash, drop the git file, prune, delete later.
    // The journal entry precedes the rename, so `doctor --repair` can finish
    // the tail after a crash.
    let mut record = journal::Record::start(&common, journal::Op::Rm, &target, branch.as_deref())?;
    record.reach(State::Removing)?;
    let trash = remove_worktree(&golden, &target)?;
    record.close()?;

    if json {
        let report = Report {
            schema: SCHEMA,
            path: &target,
            branch: branch.as_deref(),
            trash,
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    }
    Ok(())
}

/// Step 1: find the registered worktree for the branch or the path.
fn resolve(worktrees: &[git::Worktree], golden: &Path, args: &Args) -> Result<PathBuf> {
    match (&args.branch, &args.path) {
        (Some(branch), None) => {
            let full = format!("refs/heads/{branch}");
            worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(full.as_str()))
                .map(|w| w.path.clone())
                .ok_or_else(|| Error::klon(format!("no klon has the branch {branch} checked out")))
        }
        (None, Some(path)) => {
            let text = path.to_string_lossy();
            if text.contains('{') || text.contains('}') {
                return Err(Error::klon(format!(
                    "unresolved template in the path {text}"
                )));
            }
            let abs = paths::absolute(path)?;
            refuse_reserved(&abs, golden)?;
            worktrees
                .iter()
                .find(|w| same_dir(&w.path, &abs))
                .map(|w| w.path.clone())
                .ok_or_else(|| Error::klon(format!("no klon at {}", abs.display())))
        }
        _ => unreachable!("run checked that one of branch and path is set"),
    }
}

/// Refuse the repository root and the home directory, even when a template
/// or a symlink lands on them.
fn refuse_reserved(target: &Path, golden: &Path) -> Result<()> {
    if target == golden {
        return Err(Error::klon(format!(
            "{} is the repository root; rm never removes it",
            target.display()
        )));
    }
    if let Some(home) = std::env::var_os("HOME") {
        if target == paths::absolute(Path::new(&home))? {
            return Err(Error::klon(format!(
                "{} is the home directory; rm never removes it",
                target.display()
            )));
        }
    }
    Ok(())
}

/// Steps 4 to 6. Rename the klon into `.trash` when that stays on one
/// filesystem, then let git forget it and delete the copy in the background.
/// The answer is the trash path, or None when the klon never reached the trash.
fn remove_worktree(golden: &Path, target: &Path) -> Result<Option<PathBuf>> {
    if !target.exists() {
        // A stale registration with no directory on disk: prune drops it.
        git::run(golden, &["worktree", "prune"])?;
        return Ok(None);
    }
    let trash = paths::default_wt_root(golden).join(".trash");
    fs::create_dir_all(&trash).map_err(Error::io("create the trash directory"))?;
    let victim = trash_victim(&trash, target)?;
    match fs::rename(target, &victim) {
        Ok(()) => {
            drop_git_file(&victim)?;
            git::run(golden, &["worktree", "prune"])?;
            process::spawn_background_delete(&victim)?;
            Ok(Some(victim))
        }
        // Step 4: `.trash` on another filesystem; delete in place instead.
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            eprintln!(
                "klon: warning: {} is on another filesystem than the trash directory; \
                 git worktree remove --force deletes it in place",
                target.display()
            );
            git::run(golden, &["worktree", "remove", "--force", path_str(target)])?;
            Ok(None)
        }
        Err(err) => Err(Error::io(format!("rename {}", target.display()))(err)),
    }
}

/// Delete the `.git` file in the trash copy, so nothing treats the dead tree
/// as a worktree while the background delete runs.
fn drop_git_file(victim: &Path) -> Result<()> {
    let git_file = victim.join(".git");
    match fs::remove_file(&git_file) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("delete {}", git_file.display()))(err)),
    }
}

/// A unique `.trash` name: `<name>-<unix seconds>`, bumped until it is free.
fn trash_victim(trash: &Path, target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::klon(format!("cannot name the klon at {}", target.display())))?;
    let mut ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    loop {
        let victim = trash.join(format!("{name}-{ts}"));
        if !victim.exists() {
            return Ok(victim);
        }
        ts += 1;
    }
}

/// Compare two worktree paths after making both absolute.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (paths::absolute(a), paths::absolute(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap_or_default()
}
