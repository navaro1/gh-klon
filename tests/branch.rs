//! Acceptance tests for the C13 branch forms: `add` of remote and new
//! branches, the explicit `origin/<name>` form, `--pr`, `--issue`,
//! `gh klon pr`, and `rm --merged`. The `origin` remote is a bare repository
//! in the temp dir, so `origin/<name>` resolution needs no network. The `gh`
//! calls run against a fake `gh` on PATH that prints recorded API bodies.

mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{git, git_ok, klon, klon_env, stderr, stdout, Fixture};

const SEED: u64 = 7;

/// A fixture with a bare `origin` remote that holds `main`.
struct Repo {
    fx: Fixture,
    origin: PathBuf,
}

fn setup(tracked_files: usize) -> Repo {
    let fx = Fixture::generate(SEED, tracked_files, 10, 5, 2);
    // The bare remote sits at `<tmp>/octo/repo.git` and the origin URL uses
    // the `file://` form, so `git fetch` needs no network while
    // `git remote get-url origin` still parses as owner/repo.
    let origin = fx.golden.parent().unwrap().join("octo").join("repo.git");
    git_ok(
        &fx.golden,
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    let url = format!("file://{}", origin.display());
    git_ok(&fx.golden, &["remote", "add", "origin", &url]);
    git_ok(&fx.golden, &["push", "-q", "origin", "main", "feature"]);
    Repo { fx, origin }
}

/// Create `branch` from `feature`, push it to `origin`, and delete the local
/// branch and its remote-tracking ref, so the branch exists only on the
/// remote.
fn push_remote_only(repo: &Repo, branch: &str) {
    git_ok(&repo.fx.golden, &["branch", branch, "feature"]);
    git_ok(&repo.fx.golden, &["push", "-q", "origin", branch]);
    git_ok(&repo.fx.golden, &["branch", "-D", branch]);
    git_ok(
        &repo.fx.golden,
        &["update-ref", "-d", &format!("refs/remotes/origin/{branch}")],
    );
}

/// `<tmp>/golden.wt/<branch>`: the default klon path.
fn klon_path(fx: &Fixture, branch: &str) -> PathBuf {
    fx.golden.parent().unwrap().join("golden.wt").join(branch)
}

fn registered(golden: &Path, path: &Path) -> bool {
    git_ok(golden, &["worktree", "list", "--porcelain"])
        .lines()
        .any(|l| l == format!("worktree {}", path.display()))
}

fn branch_exists(golden: &Path, branch: &str) -> bool {
    git(
        golden,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .status
    .success()
}

/// Write the fake `gh` into `<tmp>/bin` and return the `PATH` value that puts
/// it first, plus the log file it appends every call to. The recorded pull
/// request 7 reports `@SHA@` as its head commit; other pull requests fail.
/// `pr list --json` reports merged pull request heads: for `topic` the live
/// branch tip, for `stale` the recorded `@SHA@`; other branches get none.
fn install_fake_gh(dir: &Path, sha: &str) -> (OsString, PathBuf) {
    static SCRIPT: &str = r#"#!/bin/sh
# A fake gh(1) that serves recorded GitHub API responses for the tests.
printf 'PWD=%s\n' "$PWD" >> '@LOG@'
printf '%s\n' "$*" >> '@LOG@'
case "$*" in
  *"repos/octo/repo/pulls/7"*)
    cat <<'JSON'
{"number": 7, "state": "open", "title": "Patch 1",
 "head": {"label": "octocat:patch-1", "ref": "patch-1", "sha": "@SHA@",
          "repo": {"full_name": "octocat/repo", "fork": true}},
 "base": {"label": "octo:main", "ref": "main"}}
JSON
    ;;
  *repos/octo/repo/pulls/*)
    echo "fake gh: pull request not found: $*" >&2
    exit 1
    ;;
  *"repos/octo/repo/issues/11"*)
    printf '{"number": 11, "title": "Fix the Login Bug! (crash on empty input)"}\n'
    ;;
  *"repos/octo/repo/issues/12"*)
    printf '{"number": 12, "title": "Exploring a very long issue title that runs far beyond the fifty character limit easily"}\n'
    ;;
  *"pr list --head topic --state merged"*)
    sha=$(git rev-parse refs/heads/topic 2>/dev/null) || sha=@SHA@
    printf '[{"headRefOid": "%s"}]\n' "$sha"
    ;;
  *"pr list --head stale --state merged"*)
    printf '[{"headRefOid": "@SHA@"}]\n'
    ;;
  "pr list"*)
    ;;
  "pr create"*)
    printf 'https://github.com/octo/repo/pull/42\n'
    ;;
  *)
    echo "fake gh: unexpected arguments: $*" >&2
    exit 1
    ;;
esac
"#;
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh.log");
    let script = SCRIPT
        .replace("@LOG@", &log.to_string_lossy())
        .replace("@SHA@", sha);
    let gh = bin.join("gh");
    fs::write(&gh, script).unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let rest = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&rest))).unwrap();
    (path, log)
}

/// Run `gh-klon` with the fake `gh` first on PATH.
fn klon_gh(cwd: &Path, path: &OsStr, args: &[&str]) -> Output {
    klon_env(cwd, &[("PATH", path)], args)
}

fn read_log(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

// --- add: the git DWIM order (handoff §4) ------------------------------------

#[test]
fn add_of_a_remote_only_branch_creates_a_tracking_branch() {
    let repo = setup(50);
    push_remote_only(&repo, "remote-only");
    let out = klon(&repo.fx.golden, &["add", "remote-only"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    // The tracking setup names origin as the remote.
    assert_eq!(
        git_ok(&repo.fx.golden, &["config", "branch.remote-only.remote"]).trim(),
        "origin"
    );
    // The klon holds the remote commit on the local branch.
    let path = klon_path(&repo.fx, "remote-only");
    assert!(registered(&repo.fx.golden, &path));
    let head = git_ok(&path, &["rev-parse", "HEAD"]);
    let want = git_ok(&repo.origin, &["rev-parse", "remote-only"]);
    assert_eq!(head.trim(), want.trim());
    assert_eq!(
        git_ok(&path, &["status", "--porcelain"]),
        "",
        "the klon must be clean"
    );
}

#[test]
fn add_of_an_unknown_name_creates_a_branch_from_base() {
    let repo = setup(50);
    let out = klon(&repo.fx.golden, &["add", "brand-new"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    // The new branch sits at base with no upstream.
    let head = git_ok(&repo.fx.golden, &["rev-parse", "brand-new"]);
    let main = git_ok(&repo.fx.golden, &["rev-parse", "main"]);
    assert_eq!(head.trim(), main.trim());
    assert!(
        !git(&repo.fx.golden, &["config", "branch.brand-new.remote"])
            .status
            .success(),
        "a new branch must have no upstream"
    );
    let path = klon_path(&repo.fx, "brand-new");
    assert!(registered(&repo.fx.golden, &path));
    let klon_head = git_ok(&path, &["rev-parse", "HEAD"]);
    assert_eq!(klon_head.trim(), main.trim());
}

#[test]
fn add_creates_new_branches_from_the_base_key() {
    let repo = setup(50);
    fs::write(repo.fx.golden.join(".klon.toml"), "base = \"feature\"\n").unwrap();
    let out = klon(&repo.fx.golden, &["add", "from-feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let head = git_ok(&repo.fx.golden, &["rev-parse", "from-feature"]);
    let feature = git_ok(&repo.fx.golden, &["rev-parse", "feature"]);
    assert_eq!(head.trim(), feature.trim());
}

#[test]
fn add_accepts_an_explicit_origin_name_and_refuses_an_unknown_one() {
    let repo = setup(50);
    push_remote_only(&repo, "topic");

    let out = klon(&repo.fx.golden, &["add", "origin/topic"]);
    assert!(
        out.status.success(),
        "add origin/topic failed: {}",
        stderr(&out)
    );
    assert_eq!(
        git_ok(&repo.fx.golden, &["config", "branch.topic.remote"]).trim(),
        "origin"
    );
    assert!(registered(&repo.fx.golden, &klon_path(&repo.fx, "topic")));

    let out = klon(&repo.fx.golden, &["add", "origin/nope"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("origin/nope"),
        "stderr: {}",
        stderr(&out)
    );
}

// --- add --pr and --issue ----------------------------------------------------

#[test]
fn add_pr_checks_out_the_pr_head_commit() {
    let repo = setup(50);
    // The pull request head is a commit that only origin's refs/pull holds.
    let sha = git_ok(&repo.fx.golden, &["rev-parse", "feature"]);
    git_ok(
        &repo.origin,
        &["update-ref", "refs/pull/7/head", sha.trim()],
    );
    let (path_env, _log) = install_fake_gh(repo.fx.golden.parent().unwrap(), sha.trim());

    let out = klon_gh(&repo.fx.golden, &path_env, &["add", "--pr", "7"]);
    assert!(out.status.success(), "add --pr 7 failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("octocat/repo/patch-1"),
        "stderr must name the fork head: {}",
        stderr(&out)
    );

    // The klon sits on the local branch pr/7 at the recorded head commit.
    let path = klon_path(&repo.fx, "pr/7");
    assert!(registered(&repo.fx.golden, &path));
    let list = git_ok(&repo.fx.golden, &["worktree", "list", "--porcelain"]);
    let block = list
        .split("\n\n")
        .find(|b| b.starts_with(&format!("worktree {}", path.display())))
        .expect("the pr klon is registered");
    assert!(
        block.lines().any(|l| l == "branch refs/heads/pr/7"),
        "block: {block}"
    );
    let head = git_ok(&path, &["rev-parse", "HEAD"]);
    assert_eq!(head.trim(), sha.trim());
    assert_eq!(git_ok(&path, &["status", "--porcelain"]), "");

    // A pull request the API does not know fails the add.
    let out = klon_gh(&repo.fx.golden, &path_env, &["add", "--pr", "9"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("pull request not found"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn add_issue_names_the_branch_after_the_issue_title() {
    let repo = setup(50);
    let (path_env, _) = install_fake_gh(repo.fx.golden.parent().unwrap(), "");

    let out = klon_gh(&repo.fx.golden, &path_env, &["add", "--issue", "11"]);
    assert!(
        out.status.success(),
        "add --issue 11 failed: {}",
        stderr(&out)
    );
    let branch = "11-fix-the-login-bug-crash-on-empty-input";
    assert!(
        branch_exists(&repo.fx.golden, branch),
        "the branch {branch} must exist"
    );
    let main = git_ok(&repo.fx.golden, &["rev-parse", "main"]);
    let head = git_ok(&repo.fx.golden, &["rev-parse", branch]);
    assert_eq!(head.trim(), main.trim(), "the issue branch starts at base");
    let path = klon_path(&repo.fx, branch);
    assert!(registered(&repo.fx.golden, &path));

    // A long title truncates the slug at 50 characters with no trailing dash.
    let out = klon_gh(&repo.fx.golden, &path_env, &["add", "--issue", "12"]);
    assert!(
        out.status.success(),
        "add --issue 12 failed: {}",
        stderr(&out)
    );
    let long = git_ok(
        &repo.fx.golden,
        &["for-each-ref", "refs/heads", "--format=%(refname:short)"],
    )
    .lines()
    .find(|l| l.starts_with("12-"))
    .expect("the 12- branch exists")
    .to_string();
    let slug = long.strip_prefix("12-").unwrap();
    assert!(
        slug.len() <= 50 && !slug.ends_with('-'),
        "slug must be at most 50 characters: {slug}"
    );
    assert!(slug.starts_with("exploring-a-very"), "slug: {slug}");
    assert!(registered(&repo.fx.golden, &klon_path(&repo.fx, &long)));
}

// --- rm --merged -------------------------------------------------------------

#[test]
fn rm_merged_of_an_unmerged_branch_refuses() {
    let repo = setup(50);
    let (path_env, _) = install_fake_gh(repo.fx.golden.parent().unwrap(), "");
    let out = klon(&repo.fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let out = klon_gh(&repo.fx.golden, &path_env, &["rm", "--merged", "feature"]);
    assert!(!out.status.success(), "rm --merged must refuse");
    assert!(
        stderr(&out).contains("not merged"),
        "stderr must say not merged: {}",
        stderr(&out)
    );
    // Nothing was removed.
    let path = klon_path(&repo.fx, "feature");
    assert!(path.exists());
    assert!(registered(&repo.fx.golden, &path));
    assert!(branch_exists(&repo.fx.golden, "feature"));
}

#[test]
fn rm_merged_removes_a_branch_that_base_contains() {
    let repo = setup(50);
    let (path_env, _) = install_fake_gh(repo.fx.golden.parent().unwrap(), "");
    let out = klon(&repo.fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    // The merge makes feature an ancestor of main, so --merged passes.
    git_ok(&repo.fx.golden, &["merge", "-q", "feature"]);
    let out = klon_gh(&repo.fx.golden, &path_env, &["rm", "--merged", "feature"]);
    assert!(out.status.success(), "rm --merged failed: {}", stderr(&out));

    let path = klon_path(&repo.fx, "feature");
    assert!(!path.exists(), "the tree must be gone");
    assert!(!registered(&repo.fx.golden, &path));
    assert!(
        !branch_exists(&repo.fx.golden, "feature"),
        "the branch must be deleted"
    );
}

#[test]
fn rm_merged_accepts_a_merged_pull_request() {
    let repo = setup(50);
    let (path_env, _) = install_fake_gh(repo.fx.golden.parent().unwrap(), "");
    // topic is not an ancestor of main; only the merged PR proves it landed.
    git_ok(&repo.fx.golden, &["branch", "topic", "feature"]);
    let out = klon(&repo.fx.golden, &["add", "topic"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let out = klon_gh(&repo.fx.golden, &path_env, &["rm", "--merged", "topic"]);
    assert!(out.status.success(), "rm --merged failed: {}", stderr(&out));
    assert!(!branch_exists(&repo.fx.golden, "topic"));

    // Without a merged PR the same branch is refused.
    git_ok(&repo.fx.golden, &["branch", "plain", "feature"]);
    let out = klon(&repo.fx.golden, &["add", "plain"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let out = klon_gh(&repo.fx.golden, &path_env, &["rm", "--merged", "plain"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not merged"),
        "stderr: {}",
        stderr(&out)
    );
}

/// A merged pull request proves the landing, but the branch moved on after
/// it: `rm --merged` removes the klon and keeps the branch.
#[test]
fn rm_merged_keeps_a_branch_that_moved_on_after_its_merged_pr() {
    let repo = setup(50);
    let feature = git_ok(&repo.fx.golden, &["rev-parse", "feature"]);
    let (path_env, _) = install_fake_gh(repo.fx.golden.parent().unwrap(), feature.trim());
    git_ok(&repo.fx.golden, &["branch", "stale", "feature"]);
    let out = klon(&repo.fx.golden, &["add", "stale"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    // Commits land after the pull request merged, so the branch tip is not
    // the recorded head commit any more.
    let path = klon_path(&repo.fx, "stale");
    fs::write(path.join("late.txt"), "late").unwrap();
    git_ok(&path, &["add", "."]);
    git_ok(&path, &["commit", "-q", "-m", "late work"]);

    let out = klon_gh(&repo.fx.golden, &path_env, &["rm", "--merged", "stale"]);
    assert!(!out.status.success(), "the forced delete must refuse");
    assert!(
        stderr(&out).contains("moved on since the merge proof"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(!path.exists(), "the klon is removed");
    assert!(
        branch_exists(&repo.fx.golden, "stale"),
        "the branch must stay"
    );
}

// --- gh klon pr ----------------------------------------------------------------

#[test]
fn pr_runs_gh_pr_create_from_inside_the_klon() {
    let repo = setup(50);
    let (path_env, log) = install_fake_gh(repo.fx.golden.parent().unwrap(), "");
    let out = klon(&repo.fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let path = klon_path(&repo.fx, "feature");

    let out = klon_gh(
        &repo.fx.golden,
        &path_env,
        &["pr", "feature", "--", "--title", "T", "--body", "B"],
    );
    assert!(out.status.success(), "pr failed: {}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        "https://github.com/octo/repo/pull/42",
        "the pull request URL must pass through"
    );
    let entries = read_log(&log);
    let last = entries.len() - 2;
    assert_eq!(entries[last], format!("PWD={}", path.display()));
    assert_eq!(
        entries[last + 1],
        "pr create --head feature --title T --body B"
    );

    // Extra arguments are optional.
    let before = entries.len();
    let out = klon_gh(&repo.fx.golden, &path_env, &["pr", "feature"]);
    assert!(out.status.success(), "pr failed: {}", stderr(&out));
    let entries = read_log(&log);
    assert_eq!(
        entries[entries.len() - 1],
        "pr create --head feature",
        "the plain call passes only --head"
    );
    assert_eq!(
        entries[entries.len() - 2],
        format!("PWD={}", path.display()),
        "gh runs inside the klon"
    );
    assert_eq!(entries.len(), before + 2);

    // A branch that no klon has checked out is refused before gh runs.
    let out = klon_gh(&repo.fx.golden, &path_env, &["pr", "no-klon"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no klon has the branch no-klon"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(read_log(&log).len(), before + 2, "gh must not run");
}
