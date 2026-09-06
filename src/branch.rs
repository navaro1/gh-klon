//! Branch resolution for `add` and the shared `base` lookup (handoff §4).
//! The order follows git's DWIM: a local branch; else `origin/<name>` after a
//! targeted fetch, as a tracking branch; else a new branch from `base`.

use crate::gh;
use crate::{config, git, Error, Result};
use std::path::Path;

/// The local branch for `add <name>`: an existing branch, an `origin/<name>`
/// tracking branch, or a new branch from `base`.
pub fn resolve(golden: &Path, name: &str) -> Result<String> {
    if let Some(rest) = name.strip_prefix("origin/") {
        return resolve_remote(golden, rest, true);
    }
    if git::local_branch_exists(golden, name) {
        return Ok(name.to_string());
    }
    resolve_remote(golden, name, false)
}

/// The local branch `pr/<n>` at the head of pull request `n`. The head repo
/// and branch name come from `gh api`; the commit comes from the
/// `refs/pull/<n>/head` ref of `origin`.
pub fn resolve_pr(golden: &Path, n: u64) -> Result<String> {
    let branch = format!("pr/{n}");
    if git::local_branch_exists(golden, &branch) {
        return Err(Error::klon(format!(
            "branch {branch} already exists; remove it first"
        )));
    }
    let slug = gh::repo_slug(golden)?;
    let head = gh::pull(golden, &slug, n)?;
    let short = head.sha.get(..7).unwrap_or(&head.sha);
    eprintln!(
        "klon: pr {n}: {}/{} at {short}",
        head.owner_repo, head.branch
    );
    git::run(
        golden,
        &["fetch", "-q", "origin", &format!("refs/pull/{n}/head")],
    )?;
    git::run(golden, &["branch", &branch, "FETCH_HEAD"])?;
    Ok(branch)
}

/// The local branch `<n>-<slug>` for the title of issue `n`, created from
/// `base`. The slug keeps ASCII letters and digits and separates words with
/// `-`, at most 50 characters.
pub fn resolve_issue(golden: &Path, n: u64) -> Result<String> {
    let slug = gh::repo_slug(golden)?;
    let title = gh::issue_title(golden, &slug, n)?;
    let branch = format!("{n}-{}", slugify(&title));
    if git::local_branch_exists(golden, &branch) {
        return Err(Error::klon(format!(
            "branch {branch} already exists; remove it first"
        )));
    }
    new_from_base(golden, &branch)?;
    Ok(branch)
}

/// The golden branch: the `base` key of `.klon.toml`, else the branch golden
/// has checked out, else `main`.
pub fn base(golden: &Path) -> Result<String> {
    if let Some(base) = config::load(golden)?.base {
        return Ok(base);
    }
    // A detached golden has no symbolic ref; fall back to the usual name.
    if let Ok(out) = git::run(golden, &["symbolic-ref", "--short", "HEAD"]) {
        let name = out.trim();
        if !name.is_empty() {
            return Ok(name.to_string());
        }
    }
    Ok("main".to_string())
}

/// How `rm --merged` proved that a branch landed. The proof travels to
/// `delete_branch`, so a forced delete stays limited to what the proof covers.
pub enum Merged {
    /// Every commit of the branch is reachable from `base`.
    Ancestor,
    /// Merged pull requests name the branch; the commits are their heads.
    PullRequest(Vec<String>),
}

/// The `rm --merged` gate: `branch` must be an ancestor of `base`, or a
/// merged pull request must name it as its head. The answer says which proof
/// held, so the delete knows when a force is safe.
pub fn merged_evidence(golden: &Path, branch: &str) -> Result<Merged> {
    let base = base(golden)?;
    if git::run(golden, &["merge-base", "--is-ancestor", branch, &base]).is_ok() {
        return Ok(Merged::Ancestor);
    }
    let heads = gh::merged_pr_heads(golden, branch)?;
    if heads.is_empty() {
        return Err(Error::klon(format!(
            "{branch} is not merged into {base}; merge or land it first, or drop --merged"
        )));
    }
    Ok(Merged::PullRequest(heads))
}

/// Delete the local branch after `rm --merged` removed its klon. `git branch
/// -d` is the safe delete; it refuses the squash merge, where the merged
/// evidence is the pull request and the commits are not an ancestor. A force
/// then needs a fresh proof at delete time, because another process can move
/// the branch between the gate and this call: the ancestor check repeats
/// against `base`, and the pull request proof compares the live tip.
pub fn delete_branch(golden: &Path, branch: &str, evidence: &Merged) -> Result<()> {
    if git::run(golden, &["branch", "-d", branch]).is_ok() {
        return Ok(());
    }
    let forced = match evidence {
        Merged::Ancestor => {
            let base = base(golden)?;
            git::run(golden, &["merge-base", "--is-ancestor", branch, &base]).is_ok()
        }
        Merged::PullRequest(heads) => {
            let tip = git::run(golden, &["rev-parse", &format!("refs/heads/{branch}")])?;
            heads
                .iter()
                .any(|head| head.eq_ignore_ascii_case(tip.trim()))
        }
    };
    if forced {
        git::run(golden, &["branch", "-D", branch]).map(|_| ())
    } else {
        Err(Error::klon(format!(
            "{branch} moved on since the merge proof; the klon is removed but the branch stays"
        )))
    }
}

/// Lowercase, keep ASCII letters and digits, separate runs of other
/// characters with one `-`, stop at 50 characters, and keep no trailing `-`.
fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut separated = true;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            separated = false;
        } else if !separated {
            slug.push('-');
            separated = true;
        }
    }
    let mut slug: String = slug.chars().take(50).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// The remote half of the DWIM: a targeted fetch, then a tracking branch.
/// `strict` is the explicit `origin/<name>` form; it refuses a name that the
/// remote does not have, where the plain form falls through to a new branch.
fn resolve_remote(golden: &Path, name: &str, strict: bool) -> Result<String> {
    let has_origin = git::run(golden, &["remote", "get-url", "origin"]).is_ok();
    if has_origin {
        // A failed targeted fetch means "not on the remote"; the plain form
        // then falls through, so the git stderr stays swallowed.
        git::run_quiet(golden, &["fetch", "-q", "origin", name]);
        let remote = format!("refs/remotes/origin/{name}");
        if git::run(golden, &["show-ref", "--verify", "--quiet", &remote]).is_ok() {
            if !git::local_branch_exists(golden, name) {
                // `--track` writes branch.<name>.remote=origin and .merge.
                git::run(
                    golden,
                    &["branch", "--track", name, &format!("origin/{name}")],
                )?;
            }
            return Ok(name.to_string());
        }
    }
    if strict {
        return Err(Error::klon(if has_origin {
            format!("origin/{name} does not exist on the origin remote")
        } else {
            "the repository has no origin remote".to_string()
        }));
    }
    new_from_base(golden, name)?;
    Ok(name.to_string())
}

/// Create the local branch `name` at `base`.
fn new_from_base(golden: &Path, name: &str) -> Result<()> {
    let base = base(golden)?;
    git::run(golden, &["branch", name, &base]).map(|_| ())
}
