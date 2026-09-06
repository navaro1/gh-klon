//! `gh klon rm (<branch> | --path <p>) [--merged] [--force]`: the safe removal
//! from handoff §7. The klon is renamed into `.trash` and deleted in the
//! background, so the command returns at once. The branch is deleted only
//! with `--merged`, and only after klon proved it is merged.

use crate::branch;
use crate::envelope::slots;
use crate::journal::{self, State};
use crate::paths;
use crate::process;
use crate::spare;
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
    /// Prove the branch is merged (an ancestor of `base`, or a merged pull
    /// request), then delete it after the removal.
    #[arg(long)]
    pub merged: bool,
    /// Remove a dirty klon or one with live processes.
    #[arg(long)]
    pub force: bool,
    /// Start no hot-spare builder after the removal.
    #[arg(long)]
    pub no_spare: bool,
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
    // The `--merged` gate is about the branch, not the tree: prove the merge
    // before anything is removed, then delete the branch after the removal.
    let merged_branch = if args.merged {
        let name = args
            .branch
            .clone()
            .or_else(|| branch.clone())
            .ok_or_else(|| {
                Error::klon("--merged needs a branch; the target has none checked out")
            })?;
        let evidence = branch::merged_evidence(&golden, &name)?;
        Some((name, evidence))
    } else {
        None
    };
    let guard = if args.force {
        Guard::Force
    } else {
        Guard::Strict
    };
    let trash = remove_target(
        &golden,
        &common,
        &worktrees,
        &target,
        branch.as_deref(),
        guard,
        args.no_spare,
    )?;

    if let Some((name, evidence)) = merged_branch {
        branch::delete_branch(&golden, &name, &evidence)?;
    }

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

/// What the removal refuses about the state of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Refuse a dirty tree and a tree with live processes. Plain `rm`.
    Strict,
    /// Remove the tree whatever it holds. `rm --force`.
    Force,
    /// Refuse only a tree with live processes. `merge` (C25) picks this after
    /// base took the branch: the tracked work is in base, so a file the merge
    /// gate left behind must not keep the klon alive, but a process that still
    /// runs there owns the directory and klon never kills it.
    Merged,
}

/// Steps 2 to 8 of the removal, after the caller resolved `target`. `rm` and
/// `merge` (C25) share it. The answer is the trash path, or None when the klon
/// never reached the trash.
pub fn remove_target(
    golden: &Path,
    common: &Path,
    worktrees: &[git::Worktree],
    target: &Path,
    branch: Option<&str>,
    guard: Guard,
    no_spare: bool,
) -> Result<Option<PathBuf>> {
    refuse_reserved(target, golden)?;
    // A lock protects a klon from removal on purpose; even --force honours it.
    if worktrees
        .iter()
        .any(|w| w.locked && same_dir(&w.path, target))
    {
        return Err(Error::klon(format!(
            "{} is locked; unlock it with git worktree unlock first",
            target.display()
        )));
    }

    // Steps 2 and 3: refuse a dirty tree and a tree with live processes.
    if target.exists() && guard != Guard::Force {
        if guard == Guard::Strict && process::dirty(target)? {
            return Err(Error::klon(format!(
                "{} is dirty; use --force to remove it",
                target.display()
            )));
        }
        // The scan reads the current directory of every process. A `run`
        // command that changed directory escapes it, so `rm` can remove its
        // klon and hand the loopback address to the next one; the new klon
        // then reports EADDRINUSE. The `run` tags would catch that command,
        // but reading `/proc/<pid>/environ` for every process measured 165 ms
        // on this host against the 100 ms budget of R8 (113 ms without the
        // read, 280 ms with it, same load). C20 puts the tree in a cgroup and
        // answers the same question with one read.
        if let Some(pid) = process::live_process(target) {
            return Err(Error::klon(format!(
                "{} has a live process (pid {pid}); use --force to remove it",
                target.display()
            )));
        }
    }

    // Steps 4 to 6: rename into .trash, drop the git file, prune, delete later.
    // The journal entry precedes the rename, so `doctor --repair` can finish
    // the tail after a crash.
    let mut record = journal::Record::start(common, journal::Op::Rm, target, branch)?;
    record.reach(State::Removing)?;
    let trash = remove_worktree(golden, common, target)?;
    // Step 7: the loopback address goes back to the pool, so the next `add`
    // takes it again. A failure here costs an address, never the removal, and
    // `rm` must still return inside 100 ms (R8).
    if let Err(err) = slots::release(common, target) {
        eprintln!("klon: {err}");
    }
    record.close()?;
    // Step 8: the next spare (R40). The start costs a stat, a lock probe, and
    // one spawn, well inside the 100 ms budget of R8.
    spare::start_after(golden, spare::configured_depth(golden), no_spare);
    Ok(trash)
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
///
/// `hibernate` (C29) removes its klon through this same function, so a
/// hibernated tree and a removed one leave the repository in the same shape.
pub fn remove_worktree(golden: &Path, common: &Path, target: &Path) -> Result<Option<PathBuf>> {
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
            delete(common, &victim)?;
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

/// Step 6: hand the trash copy to the backend that made it (C5, C7). The btrfs
/// backend drops a subvolume in one ioctl where the mount allows it; every
/// other backend, and every klon that is not a subvolume, takes the detached
/// `rm -rf`.
///
/// The backend comes from the cached probe answer only. A fresh probe clones a
/// fixture, and `rm` must return inside 100 ms (R8), so no cache means the
/// universal delete.
fn delete(common: &Path, victim: &Path) -> Result<()> {
    match crate::backend::cached(common) {
        Some(backend) => backend.delete(victim),
        None => process::spawn_background_delete(victim),
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
