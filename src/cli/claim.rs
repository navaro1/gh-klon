//! `gh klon claim <branch> <paths...> [--release]`: record the paths a klon
//! owns, so a second klon that wants the same path hears about it now and not
//! at the merge (handoff §6, R26).
//!
//! The command runs four steps:
//!
//! 1. Find the klon that has `branch` checked out.
//! 2. Read each path against that klon's root: no `..`, no symlinked ancestor,
//!    nothing outside the tree.
//! 3. Under one exclusive lock, refuse a path another klon holds, then append.
//! 4. Report.
//!
//! `--release` runs the same first two steps and then drops the named paths.
//! `--release` with no path drops every claim of the klon, and that form needs
//! no klon directory, so a klon that is already gone can still be cleaned up.

use crate::claims::{self, Kind};
use crate::{git, paths, Error, Result};
use serde::Serialize;
use std::path::Path;

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.claim/1";

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the klon that owns the paths.
    pub branch: String,
    /// The paths to claim, relative to the klon root.
    pub paths: Vec<String>,
    /// Give the named paths back, or every claim of the klon with no path.
    #[arg(long)]
    pub release: bool,
}

/// The `claim --json` document. `claims` names what the klon took, and it is
/// empty for a release; `released` names what it gave back, and it is empty
/// for a claim.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    branch: &'a str,
    claims: Vec<Held<'a>>,
    released: Vec<String>,
}

/// One taken path.
#[derive(Serialize)]
struct Held<'a> {
    path: &'a str,
    kind: Kind,
}

pub fn run(args: Args, json: bool) -> Result<()> {
    if !args.release && args.paths.is_empty() {
        return Err(Error::klon(format!(
            "name at least one path to claim for {}, or pass --release",
            args.branch
        )));
    }
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = paths::absolute(
        &worktrees
            .first()
            .ok_or_else(|| Error::klon("not inside a git repository"))?
            .path,
    )?;
    let common = git::common_dir_of_main(&golden)?;

    // Step 1. A release with no path needs no tree: the paths it drops are the
    // ones the table already holds, and the klon may be gone by now.
    let root = match git::klon_of_branch(&worktrees, &args.branch) {
        Ok(worktree) => Some(paths::absolute(&worktree.path)?),
        Err(err) if args.release && args.paths.is_empty() => {
            eprintln!("klon: {err}; releasing the recorded claims anyway");
            None
        }
        Err(err) => return Err(err),
    };

    if args.release {
        let released = if args.paths.is_empty() {
            claims::release_all(&common, &args.branch)?
        } else {
            let root = root
                .as_deref()
                .expect("step 1 refuses a release with a path and no klon");
            let paths = read_paths(root, &args.paths, false)?;
            claims::release(&common, &args.branch, &paths)?
        };
        return report(&args, Vec::new(), released, json);
    }

    // Step 2.
    let root = root.expect("step 1 refuses a claim with no klon");
    let paths = read_paths(&root, &args.paths, true)?;
    let wanted: Vec<(String, Kind)> = paths
        .into_iter()
        .map(|path| {
            let kind = claims::kind_of(&root, &path);
            (path, kind)
        })
        .collect();

    // Step 3.
    let taken = claims::acquire(&common, &args.branch, &wanted)?;
    let held: Vec<Held> = taken
        .iter()
        .map(|claim| Held {
            path: &claim.path,
            kind: claim.kind,
        })
        .collect();
    report(&args, held, Vec::new(), json)
}

/// Step 2 for every argument. `on_disk` adds the symlink refusal, which a
/// claim needs and a release does not: a release names a row of the table, and
/// the tree it once described may have changed.
fn read_paths(root: &Path, raw: &[String], on_disk: bool) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for one in raw {
        let path = claims::normalize(root, one)?;
        if on_disk {
            claims::refuse_symlink_ancestor(root, &path)?;
        }
        out.push(path);
    }
    Ok(out)
}

/// Step 4.
fn report(args: &Args, claims: Vec<Held>, released: Vec<String>, json: bool) -> Result<()> {
    if json {
        let report = Report {
            schema: SCHEMA,
            branch: &args.branch,
            claims,
            released,
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
        return Ok(());
    }
    for held in &claims {
        println!(
            "{} claims {} ({})",
            args.branch,
            held.path,
            held.kind.name()
        );
    }
    for path in &released {
        println!("{} releases {path}", args.branch);
    }
    if claims.is_empty() && released.is_empty() {
        println!("{} holds no claim to release", args.branch);
    }
    Ok(())
}
