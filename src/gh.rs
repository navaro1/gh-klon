//! Subprocess wrapper around the installed `gh` CLI. klon calls `gh` for the
//! GitHub facts that branch forms need and for `pr create`. Tests inject a
//! fake `gh` on PATH, so every call goes through `Command::new("gh")`.

use crate::{git, Error, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// Run `gh <args>` in `cwd` and return its stdout. A non-zero exit becomes
/// `Error::Git` with the stderr passed through unchanged.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(Error::io("run gh"))?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| Error::klon("gh output must be valid UTF-8"))
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The `{owner}/{repo}` slug of the `origin` remote. Accepts the https and
/// ssh URL forms (`https://host/owner/repo(.git)`, `ssh://git@host/owner/repo`,
/// `git@host:owner/repo`).
pub fn repo_slug(cwd: &Path) -> Result<String> {
    let url = git::run(cwd, &["remote", "get-url", "origin"])?;
    let url = url.trim();
    parse_slug(url).ok_or_else(|| {
        Error::klon(format!(
            "cannot read the owner and repository from the origin URL {url}"
        ))
    })
}

/// Pull the `owner/repo` slug out of one remote URL. The slug is the last two
/// non-empty path segments, so the https, ssh, scp-like, and `file://` forms
/// all work.
fn parse_slug(url: &str) -> Option<String> {
    let url = url.strip_suffix(".git").unwrap_or(url);
    let path = if let Some((_scheme, rest)) = url.split_once("://") {
        rest.split_once('/').map(|(_host, path)| path)?
    } else {
        url.split_once(':').map(|(_user_host, path)| path)?
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let repo = segments[segments.len() - 1];
    let owner = segments[segments.len() - 2];
    Some(format!("{owner}/{repo}"))
}

/// The head of a pull request: the head repository, its branch, and the commit.
pub struct PullHead {
    /// `owner/repo` of the head repository; a fork for fork pull requests.
    pub owner_repo: String,
    /// The branch name in the head repository.
    pub branch: String,
    /// The head commit.
    pub sha: String,
}

/// `gh api repos/<slug>/pulls/<n>`: the head of pull request `n`.
pub fn pull(cwd: &Path, slug: &str, n: u64) -> Result<PullHead> {
    let path = format!("repos/{slug}/pulls/{n}");
    let value = api(cwd, &path)?;
    let head = &value["head"];
    Ok(PullHead {
        owner_repo: field(head, &["repo", "full_name"], &path)?,
        branch: field(head, &["ref"], &path)?,
        sha: field(head, &["sha"], &path)?,
    })
}

/// `gh api repos/<slug>/issues/<n>`: the issue title.
pub fn issue_title(cwd: &Path, slug: &str, n: u64) -> Result<String> {
    let path = format!("repos/{slug}/issues/{n}");
    field(&api(cwd, &path)?, &["title"], &path)
}

/// The head commits of the merged pull requests that name `branch`. An empty
/// answer means no merged pull request. A missing or failing `gh` degrades to
/// empty with one stderr line (spec 5: every host feature is optional).
pub fn merged_pr_heads(cwd: &Path, branch: &str) -> Result<Vec<String>> {
    let args = [
        "pr",
        "list",
        "--head",
        branch,
        "--state",
        "merged",
        "--json",
        "headRefOid",
    ];
    match run(cwd, &args) {
        Ok(out) if out.trim().is_empty() => Ok(Vec::new()),
        Ok(out) => {
            let value: Value = serde_json::from_str(out.trim())
                .map_err(|_| Error::klon("gh pr list --json did not return a JSON document"))?;
            Ok(value
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| row["headRefOid"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default())
        }
        Err(err) => {
            eprintln!("klon: gh pr list failed: {err}; klon assumes no merged pull request");
            Ok(Vec::new())
        }
    }
}

/// `gh api <path>` and the parsed JSON body.
fn api(cwd: &Path, path: &str) -> Result<Value> {
    let out = run(cwd, &["api", path])?;
    serde_json::from_str(out.trim())
        .map_err(|_| Error::klon(format!("gh api {path} did not return JSON")))
}

/// One string field of a JSON value, addressed by a key path.
fn field(value: &Value, keys: &[&str], path: &str) -> Result<String> {
    let mut at = value;
    for key in keys {
        at = &at[*key];
    }
    at.as_str().map(str::to_string).ok_or_else(|| {
        Error::klon(format!(
            "gh api {path}: the response has no {}",
            keys.join(".")
        ))
    })
}

// --- list (C30) ---------------------------------------------------------------

/// The pull request facts `list` shows for one branch.
pub struct PrFacts {
    /// The pull request number.
    pub number: u64,
    /// `pass`, `fail`, `pending`, or `none` when the pull request runs no
    /// check. None when the answer named no rollup at all.
    pub checks: Option<String>,
}

/// `gh pr list --head <branch> --json number,statusCheckRollup --state all`.
/// The answer is the parsed array, so the caller can cache it verbatim for 60 s.
pub fn branch_pr_payload(cwd: &Path, branch: &str) -> Result<Value> {
    let args = [
        "pr",
        "list",
        "--head",
        branch,
        "--json",
        "number,statusCheckRollup",
        "--state",
        "all",
    ];
    let out = run(cwd, &args)?;
    serde_json::from_str(out.trim())
        .map_err(|_| Error::klon("gh pr list --json did not return a JSON document"))
}

/// The facts in a `branch_pr_payload` answer: the pull request with the
/// highest number, which is the newest one for the branch. An empty array, a
/// branch without a pull request, answers None.
pub fn pr_from_payload(payload: &Value) -> Option<PrFacts> {
    let rows = payload.as_array()?;
    let row = rows
        .iter()
        .max_by_key(|row| row["number"].as_u64().unwrap_or(0))?;
    Some(PrFacts {
        number: row["number"].as_u64()?,
        checks: checks_from_rollup(&row["statusCheckRollup"]),
    })
}

/// The verdict over one `statusCheckRollup` array. A failure answers `fail`
/// even when other checks still run, a not-yet-finished check answers
/// `pending`, and completed successes (with skipped and neutral ones) answer
/// `pass`. A rollup with no check answers `none`.
fn checks_from_rollup(rollup: &Value) -> Option<String> {
    const FAILING: [&str; 7] = [
        "FAILURE",
        "ERROR",
        "TIMED_OUT",
        "ACTION_REQUIRED",
        "CANCELLED",
        "STARTUP_FAILURE",
        "STALE",
    ];
    const WAITING: [&str; 4] = ["QUEUED", "IN_PROGRESS", "PENDING", "EXPECTED"];
    let items = rollup.as_array()?;
    if items.is_empty() {
        return Some("none".to_string());
    }
    let mut waiting = false;
    for item in items {
        let conclusion = item["conclusion"].as_str().unwrap_or("");
        let state = item["state"].as_str().unwrap_or("");
        let status = item["status"].as_str().unwrap_or("");
        if FAILING.contains(&conclusion) || FAILING.contains(&state) {
            return Some("fail".to_string());
        }
        if WAITING.contains(&status) || WAITING.contains(&state) {
            waiting = true;
        }
    }
    Some(if waiting { "pending" } else { "pass" }.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup(text: &str) -> Option<String> {
        checks_from_rollup(&serde_json::from_str(text).expect("rollup JSON"))
    }

    #[test]
    fn the_rollup_reads_each_state() {
        assert_eq!(rollup("[]").as_deref(), Some("none"));
        assert_eq!(
            rollup(r#"[{"status": "COMPLETED", "conclusion": "SUCCESS"}]"#).as_deref(),
            Some("pass")
        );
        assert_eq!(
            rollup(r#"[{"status": "COMPLETED", "conclusion": "SKIPPED"}]"#).as_deref(),
            Some("pass")
        );
        assert_eq!(rollup(r#"[{"state": "SUCCESS"}]"#).as_deref(), Some("pass"));
        assert_eq!(
            rollup(r#"[{"status": "COMPLETED", "conclusion": "FAILURE"}]"#).as_deref(),
            Some("fail")
        );
        assert_eq!(rollup(r#"[{"state": "ERROR"}]"#).as_deref(), Some("fail"));
        assert_eq!(
            rollup(r#"[{"status": "COMPLETED", "conclusion": "TIMED_OUT"}]"#).as_deref(),
            Some("fail")
        );
        assert_eq!(
            rollup(r#"[{"status": "QUEUED"}]"#).as_deref(),
            Some("pending")
        );
        assert_eq!(
            rollup(r#"[{"state": "PENDING"}]"#).as_deref(),
            Some("pending")
        );
        assert_eq!(
            rollup(
                r#"[{"status": "IN_PROGRESS"}, {"status": "COMPLETED", "conclusion": "SUCCESS"}]"#
            )
            .as_deref(),
            Some("pending")
        );
        // A failure wins over a waiting check, whichever comes first.
        assert_eq!(
            rollup(r#"[{"status": "IN_PROGRESS"}, {"state": "FAILURE"}]"#).as_deref(),
            Some("fail")
        );
        assert_eq!(
            rollup(r#"[{"state": "FAILURE"}, {"status": "IN_PROGRESS"}]"#).as_deref(),
            Some("fail")
        );
    }

    #[test]
    fn no_rollup_is_no_verdict() {
        assert_eq!(checks_from_rollup(&Value::Null), None);
        assert_eq!(checks_from_rollup(&serde_json::json!({})), None);
    }

    #[test]
    fn the_newest_pull_request_of_the_payload_wins() {
        let payload: Value = serde_json::from_str(
            r#"[{"number": 3, "statusCheckRollup": []},
                {"number": 5, "statusCheckRollup": [{"state": "SUCCESS"}]}]"#,
        )
        .expect("payload JSON");
        let facts = pr_from_payload(&payload).expect("a pull request");
        assert_eq!(
            facts.number, 5,
            "the newest pull request has the highest number"
        );
        assert_eq!(facts.checks.as_deref(), Some("pass"));
    }

    #[test]
    fn an_empty_payload_names_no_pull_request() {
        let payload: Value = serde_json::from_str("[]").expect("payload JSON");
        assert!(pr_from_payload(&payload).is_none());
        assert!(pr_from_payload(&Value::Null).is_none());
    }
}
