//! C14 acceptance tests: `gh klon up` and `gh klon sync`.
//!
//! The `origin` remote is a bare repository in the temp directory and the URL
//! uses the `file://` form, so every fetch and push stays local. A second
//! clone of that bare repository plays the other developer: it pushes, and it
//! force-pushes.

mod common;

use common::{git, git_ok, klon, klon_env, manifest_without_times, stderr, stdout, Fixture};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const SEED: u64 = 14;

/// A fixture with a bare `origin` remote and a second clone of it.
struct Repo {
    fx: Fixture,
    /// A plain clone of the remote. The tests push from here.
    other: PathBuf,
}

/// Give the repository a committer identity. `sync` shells to `git rebase` and
/// `git merge`, which write a commit; the harness hides the global config, so
/// the identity has to live in the repository. Every worktree shares it.
fn identity(dir: &Path) {
    git_ok(dir, &["config", "user.name", "klon"]);
    git_ok(dir, &["config", "user.email", "klon@example.com"]);
}

/// A fixture with an identity and no remote.
fn plain(tracked_files: usize) -> Fixture {
    let fx = Fixture::generate(SEED, tracked_files, 4, 4, 2);
    identity(&fx.golden);
    fx
}

/// Golden on `main`, a bare `origin` that holds `main` and `feature`, and a
/// second clone. `main` in golden tracks `origin/main`.
fn setup(tracked_files: usize) -> Repo {
    let fx = plain(tracked_files);
    let root = fx.golden.parent().unwrap().to_path_buf();
    let origin = root.join("origin.git");
    git_ok(
        &fx.golden,
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    let url = format!("file://{}", origin.display());
    git_ok(&fx.golden, &["remote", "add", "origin", &url]);
    git_ok(
        &fx.golden,
        &["push", "-q", "-u", "origin", "main", "feature"],
    );
    let other = root.join("other");
    git_ok(&root, &["clone", "-q", &url, other.to_str().unwrap()]);
    identity(&other);
    Repo { fx, other }
}

/// Commit `body` to `file` on `branch` in the second clone and push it. The
/// clone starts from the remote tip each time, so a branch the clone has never
/// seen also works.
fn push_from_other(repo: &Repo, branch: &str, file: &str, body: &str) -> String {
    git_ok(&repo.other, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.other,
        &["checkout", "-q", "-B", branch, &format!("origin/{branch}")],
    );
    fs::write(repo.other.join(file), body).unwrap();
    git_ok(&repo.other, &["add", "-A"]);
    git_ok(
        &repo.other,
        &["commit", "-qm", &format!("{branch}: {file}")],
    );
    git_ok(&repo.other, &["push", "-q", "origin", branch]);
    git_ok(&repo.other, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

/// Rewrite the tip of `branch` on the remote: one commit that replaces the
/// commit the branch had. This is the force-push the AC needs. `file` should
/// name a path the replaced commit did not touch, so a later rebase over the
/// rewrite has no conflict to report.
fn force_push_from_other(repo: &Repo, branch: &str, file: &str, body: &str) -> String {
    git_ok(&repo.other, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.other,
        &[
            "checkout",
            "-q",
            "-B",
            branch,
            &format!("origin/{branch}~1"),
        ],
    );
    fs::write(repo.other.join(file), body).unwrap();
    git_ok(&repo.other, &["add", "-A"]);
    git_ok(&repo.other, &["commit", "-qm", "rewritten"]);
    git_ok(&repo.other, &["push", "-q", "--force", "origin", branch]);
    git_ok(&repo.other, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

fn head(dir: &Path) -> String {
    git_ok(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn add_klon(golden: &Path, branch: &str) -> PathBuf {
    let out = klon(golden, &["add", branch]);
    assert!(
        out.status.success(),
        "add {branch} failed: {}",
        stderr(&out)
    );
    PathBuf::from(stdout(&out).trim())
}

/// Commit one file inside a klon, so the klon has a commit of its own.
fn commit_in(dir: &Path, file: &str, body: &str) {
    fs::write(dir.join(file), body).unwrap();
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-qm", &format!("local {file}")]);
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text.trim())
        .unwrap_or_else(|err| panic!("not one JSON document: {err}\n{text}"))
}

/// Poll `cond` until it holds or the timeout passes.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

// --- A `git` that records every call -------------------------------------------

/// Write a `git` shim that appends its arguments to `log` and then runs the
/// real git. The answer is the directory to put in front of `PATH`.
fn git_shim(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let real = String::from_utf8_lossy(
        &Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("find git")
            .stdout,
    )
    .trim()
    .to_string();
    assert!(!real.is_empty(), "git must be on PATH");
    let bin = dir.join("shim");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{real}' \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Run a klon command with the shim in front of `PATH`.
fn klon_logged(cwd: &Path, bin: &Path, args: &[&str]) -> Output {
    let path = std::env::var("PATH").unwrap_or_default();
    let joined = format!("{}:{path}", bin.display());
    klon_env(cwd, &[("PATH", OsStr::new(&joined))], args)
}

fn logged_fetches(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.split_whitespace().any(|word| word == "fetch"))
        .map(str::to_string)
        .collect()
}

// --- `up` ----------------------------------------------------------------------

/// AC: `up` on a dirty golden exits non-zero with `dirty` and fetches nothing.
#[test]
fn up_on_a_dirty_golden_refuses_and_fetches_nothing() {
    let repo = setup(20);
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("git.log");
    let bin = git_shim(tmp.path(), &log);

    // A tracked change and an untracked file both make golden dirty.
    for change in ["tracked", "untracked"] {
        let _ = fs::remove_file(&log);
        match change {
            "tracked" => fs::write(repo.fx.golden.join("f2.txt"), "edited\n").unwrap(),
            _ => {
                git_ok(&repo.fx.golden, &["checkout", "--", "f2.txt"]);
                fs::write(repo.fx.golden.join("scratch.txt"), "scratch\n").unwrap();
            }
        }
        let out = klon_logged(&repo.fx.golden, &bin, &["up"]);
        assert!(!out.status.success(), "up must refuse a {change} change");
        assert!(
            stderr(&out).contains("dirty"),
            "the refusal must say dirty: {}",
            stderr(&out)
        );
        let fetches = logged_fetches(&log);
        assert!(
            fetches.is_empty(),
            "up must fetch nothing, but ran {fetches:?}"
        );
    }
}

/// AC: `up` on a golden behind `origin/main` fast-forwards and starts a spare.
#[test]
fn up_fast_forwards_golden_and_starts_a_spare() {
    let repo = setup(20);
    let before = head(&repo.fx.golden);
    let wanted = push_from_other(&repo, "main", "from-other.txt", "other work\n");
    assert_ne!(before, wanted, "the remote must be ahead");

    let out = klon_env(&repo.fx.golden, &[("KLON_SPARE", OsStr::new("1"))], &["up"]);
    assert!(out.status.success(), "up failed: {}", stderr(&out));
    assert_eq!(head(&repo.fx.golden), wanted, "golden must fast-forward");
    assert!(
        repo.fx.golden.join("from-other.txt").is_file(),
        "the new file must land in golden"
    );

    let spare = repo
        .fx
        .golden
        .parent()
        .unwrap()
        .join("golden.wt")
        .join(".spare")
        .join(".klon")
        .join("spare.json");
    assert!(
        wait_until(|| spare.is_file(), Duration::from_secs(60)),
        "up must start a spare builder"
    );
}

/// `up --json` reports the base and the two heads of the fast-forward.
#[test]
fn up_json_reports_the_fast_forward() {
    let repo = setup(20);
    let before = head(&repo.fx.golden);
    let wanted = push_from_other(&repo, "main", "from-other.txt", "other work\n");

    let out = klon(&repo.fx.golden, &["up", "--json"]);
    assert!(out.status.success(), "up --json failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["schema"], "klon.up/1");
    assert_eq!(doc["base"], "main");
    assert_eq!(doc["head_before"], before);
    assert_eq!(doc["head_after"], wanted);
    assert_eq!(doc["steps_run"], 0);
    // The harness turns the spare off, so `up` asks for no builder.
    assert_eq!(doc["spare_started"], false);
}

/// A `[warm]` step that prints must not break `up --json`, and a fetched
/// `.klon.toml` must replace the one klon read before the fast-forward.
#[test]
fn up_json_stays_one_document_and_runs_the_fetched_warm_steps() {
    let repo = setup(20);
    // The remote adds the config, so golden learns the steps from the merge.
    git_ok(&repo.other, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.other,
        &["checkout", "-q", "-B", "main", "origin/main"],
    );
    fs::write(repo.other.join(".gitignore"), "/build/\n/loud\n").unwrap();
    fs::write(
        repo.other.join(".klon.toml"),
        "[warm]\nsteps = [\"echo noise on stdout; touch loud\"]\n",
    )
    .unwrap();
    git_ok(&repo.other, &["add", "-A"]);
    git_ok(&repo.other, &["commit", "-qm", "add the warm steps"]);
    git_ok(&repo.other, &["push", "-q", "origin", "main"]);

    let out = klon(&repo.fx.golden, &["up", "--json", "--yes"]);
    assert!(out.status.success(), "up --json failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["schema"], "klon.up/1");
    assert_eq!(
        doc["steps_run"], 1,
        "up must run the step of the fetched config"
    );
    assert!(
        repo.fx.golden.join("loud").is_file(),
        "the fetched warm step must run"
    );
    assert!(
        stderr(&out).contains("noise on stdout"),
        "the step output must go to stderr: {}",
        stderr(&out)
    );
}

/// `up` refuses a golden that is not on `base`, and names the base.
#[test]
fn up_refuses_a_golden_that_is_not_on_base() {
    let repo = setup(20);
    git_ok(&repo.fx.golden, &["checkout", "-q", "feature"]);
    fs::write(repo.fx.golden.join(".klon.toml"), "base = \"main\"\n").unwrap();
    git_ok(&repo.fx.golden, &["add", "-A"]);
    git_ok(&repo.fx.golden, &["commit", "-qm", "config"]);

    let out = klon(&repo.fx.golden, &["up"]);
    assert!(!out.status.success(), "up must refuse a golden off base");
    assert!(
        stderr(&out).contains("not on base main"),
        "the refusal must name the base: {}",
        stderr(&out)
    );
}

/// `up` on a golden that the remote left behind refuses with `diverged` and
/// runs no warm step.
#[test]
fn up_refuses_a_diverged_golden() {
    let repo = setup(20);
    push_from_other(&repo, "main", "from-other.txt", "other work\n");
    // Golden commits its own `main` commit, so the two histories part.
    fs::write(repo.fx.golden.join(".gitignore"), "/build/\n/marker\n").unwrap();
    git_ok(&repo.fx.golden, &["add", "-A"]);
    git_ok(&repo.fx.golden, &["commit", "-qm", "local main work"]);
    fs::write(
        repo.fx.golden.join(".klon.toml"),
        "[warm]\nsteps = [\"touch marker\"]\n",
    )
    .unwrap();
    git_ok(&repo.fx.golden, &["add", "-A"]);
    git_ok(&repo.fx.golden, &["commit", "-qm", "config"]);

    let out = klon(&repo.fx.golden, &["up", "--yes"]);
    assert!(!out.status.success(), "up must refuse a diverged golden");
    assert!(
        stderr(&out).contains("diverged"),
        "the refusal must say diverged: {}",
        stderr(&out)
    );
    assert!(
        !repo.fx.golden.join("marker").exists(),
        "a refused up runs no warm step"
    );
}

/// A repository with no `origin` remote still warms golden.
#[test]
fn up_without_an_origin_remote_warms_golden() {
    let fx = plain(20);
    fs::write(fx.golden.join(".gitignore"), "/build/\n/marker\n").unwrap();
    fs::write(
        fx.golden.join(".klon.toml"),
        "[warm]\nsteps = [\"touch marker\"]\n",
    )
    .unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "config"]);

    let out = klon(&fx.golden, &["up", "--yes"]);
    assert!(out.status.success(), "up failed: {}", stderr(&out));
    assert!(fx.golden.join("marker").is_file(), "the warm step must run");
    assert!(
        stderr(&out).contains("no origin remote"),
        "up must say why it skipped the fetch: {}",
        stderr(&out)
    );
}

// --- `sync` --------------------------------------------------------------------

/// AC: `sync` of a klon behind its upstream with no local commits
/// fast-forwards.
#[test]
fn sync_fast_forwards_a_klon_behind_its_upstream() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    let before = head(&path);
    let wanted = push_from_other(&repo, "feature", "from-other.txt", "other work\n");
    assert_ne!(before, wanted);

    let out = klon(&repo.fx.golden, &["sync", "feature"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    assert_eq!(head(&path), wanted, "the klon must fast-forward");
    assert!(
        path.join("from-other.txt").is_file(),
        "the new file must land in the klon"
    );
    let line = stdout(&out);
    assert!(
        line.contains("fast-forward"),
        "the line must name the action: {line}"
    );

    // A second sync with nothing new says so and changes nothing.
    let out = klon(&repo.fx.golden, &["sync", "feature", "--json"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["schema"], "klon.sync/1");
    assert_eq!(doc["action"], "up-to-date");
    assert_eq!(doc["branch"], "feature");
    assert_eq!(doc["head_before"], wanted);
    assert_eq!(doc["head_after"], wanted);
}

/// A klon with a commit of its own rebases onto the upstream.
#[test]
fn sync_rebases_a_klon_that_has_a_local_commit() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    commit_in(&path, "local.txt", "local work\n");
    let remote = push_from_other(&repo, "feature", "from-other.txt", "other work\n");

    let out = klon(&repo.fx.golden, &["sync", "feature", "--json"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["action"], "rebase");
    // The rebase puts the local commit on top of the remote one.
    assert_eq!(
        git_ok(&path, &["rev-parse", "HEAD~1"]).trim(),
        remote,
        "the remote commit must be the new parent"
    );
    assert!(path.join("local.txt").is_file());
    assert!(path.join("from-other.txt").is_file());
}

/// `--merge` merges the upstream instead of rebasing onto it.
#[test]
fn sync_merge_makes_a_merge_commit() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    commit_in(&path, "local.txt", "local work\n");
    let remote = push_from_other(&repo, "feature", "from-other.txt", "other work\n");

    let out = klon(&repo.fx.golden, &["sync", "feature", "--merge", "--json"]);
    assert!(
        out.status.success(),
        "sync --merge failed: {}",
        stderr(&out)
    );
    assert_eq!(parse(&stdout(&out))["action"], "merge");
    let parents = git_ok(&path, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert!(
        parents.split_whitespace().any(|oid| oid == remote),
        "the merge commit must have the remote tip as a parent: {parents}"
    );
}

/// AC: `sync` of a klon whose upstream was force-pushed and that has one
/// unique local commit exits non-zero with `force-pushed`.
#[test]
fn sync_refuses_a_force_pushed_upstream_with_a_unique_local_commit() {
    let repo = setup(20);
    push_from_other(&repo, "feature", "from-other.txt", "other work\n");
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.fx.golden,
        &["branch", "-f", "feature", "origin/feature"],
    );
    let path = add_klon(&repo.fx.golden, "feature");
    commit_in(&path, "local.txt", "local work\n");
    let unique = head(&path);

    let rewritten = force_push_from_other(&repo, "feature", "rewritten.txt", "rewritten\n");
    let out = klon(&repo.fx.golden, &["sync", "feature"]);
    assert!(!out.status.success(), "sync must refuse a force-push");
    assert!(
        stderr(&out).contains("force-pushed"),
        "the refusal must say force-pushed: {}",
        stderr(&out)
    );
    assert_eq!(head(&path), unique, "a refused sync moves nothing");

    // `--force` accepts the rewrite and replays the local commit on top.
    let out = klon(&repo.fx.golden, &["sync", "feature", "--force", "--json"]);
    assert!(
        out.status.success(),
        "sync --force failed: {}",
        stderr(&out)
    );
    assert_eq!(parse(&stdout(&out))["action"], "rebase");
    assert!(
        git(&path, &["merge-base", "--is-ancestor", &rewritten, "HEAD"])
            .status
            .success(),
        "the klon must sit on the rewritten upstream"
    );
    assert!(
        path.join("local.txt").is_file(),
        "the local work must survive"
    );
}

/// A force-pushed upstream that the klon has no unique commit against needs no
/// `--force`: the klon loses nothing, so klon fast-forwards it.
#[test]
fn sync_accepts_a_force_push_when_the_klon_has_no_commit_of_its_own() {
    let repo = setup(20);
    push_from_other(&repo, "feature", "one.txt", "one\n");
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.fx.golden,
        &["branch", "-f", "feature", "origin/feature"],
    );
    // The klon stays at this commit while the remote adds one more and then
    // rewrites it. golden fetches that second commit, so the rewrite is
    // visible as a force-push and not as a plain fast-forward.
    let path = add_klon(&repo.fx.golden, "feature");
    push_from_other(&repo, "feature", "two.txt", "two\n");
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);
    let rewritten = force_push_from_other(&repo, "feature", "two-again.txt", "two again\n");

    let out = klon(&repo.fx.golden, &["sync", "feature", "--json"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    assert_eq!(parse(&stdout(&out))["action"], "fast-forward");
    assert_eq!(head(&path), rewritten, "the klon must follow the rewrite");
}

/// A klon that klon synced once keeps the evidence: a force-push that follows
/// a hand-made `git fetch` is still refused.
#[test]
fn a_recorded_tip_survives_a_fetch_between_two_syncs() {
    let repo = setup(20);
    push_from_other(&repo, "feature", "from-other.txt", "other work\n");
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.fx.golden,
        &["branch", "-f", "feature", "origin/feature"],
    );
    let path = add_klon(&repo.fx.golden, "feature");
    // The first sync records the upstream tip.
    assert!(klon(&repo.fx.golden, &["sync", "feature"]).status.success());
    commit_in(&path, "local.txt", "local work\n");

    force_push_from_other(&repo, "feature", "rewritten.txt", "rewritten\n");
    // The user fetches by hand, so the pre-fetch tip of the next run is
    // already the rewritten one. Only the record still holds the old tip.
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);

    let out = klon(&repo.fx.golden, &["sync", "feature"]);
    assert!(!out.status.success(), "sync must still refuse");
    assert!(
        stderr(&out).contains("force-pushed"),
        "the record must survive the hand-made fetch: {}",
        stderr(&out)
    );
}

/// The first `sync` of a branch has no record, so the reflog of the upstream
/// ref carries the evidence. It catches a rewrite that another program fetched
/// before klon ever ran.
#[test]
fn the_reflog_catches_a_force_push_that_another_program_fetched() {
    let repo = setup(20);
    push_from_other(&repo, "feature", "from-other.txt", "other work\n");
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);
    git_ok(
        &repo.fx.golden,
        &["branch", "-f", "feature", "origin/feature"],
    );
    let path = add_klon(&repo.fx.golden, "feature");
    commit_in(&path, "local.txt", "local work\n");

    force_push_from_other(&repo, "feature", "rewritten.txt", "rewritten\n");
    // Another program, for example an editor, fetches the rewrite first. klon
    // has recorded nothing yet, and its own fetch below changes no ref.
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);

    let out = klon(&repo.fx.golden, &["sync", "feature"]);
    assert!(!out.status.success(), "sync must refuse");
    assert!(
        stderr(&out).contains("force-pushed"),
        "the reflog must carry the evidence: {}",
        stderr(&out)
    );
}

/// A branch whose configured upstream is gone from the remote is not a branch
/// without an upstream. `sync` refuses instead of rebasing it onto `base`.
#[test]
fn sync_refuses_a_branch_whose_upstream_is_gone() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    let before = head(&path);
    // The remote drops the branch, and a pruning fetch drops the local
    // remote-tracking ref. `branch.feature.merge` still names the upstream.
    git_ok(
        &repo.other,
        &["push", "-q", "origin", "--delete", "feature"],
    );
    git_ok(&repo.fx.golden, &["fetch", "-q", "--prune", "origin"]);

    let out = klon(&repo.fx.golden, &["sync", "feature"]);
    assert!(!out.status.success(), "sync must refuse a gone upstream");
    assert!(
        stderr(&out).contains("gone from the remote"),
        "the refusal must name the cause: {}",
        stderr(&out)
    );
    assert_eq!(head(&path), before, "a refused sync moves nothing");

    // `--onto` names a target, so the same klon syncs.
    let out = klon(&repo.fx.golden, &["sync", "feature", "--onto", "main"]);
    assert!(out.status.success(), "--onto must work: {}", stderr(&out));
}

/// `--onto` records no upstream tip. A plain sync after it must not read the
/// `--onto` target as the last upstream tip and refuse.
#[test]
fn an_onto_run_leaves_the_upstream_record_alone() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    commit_in(&path, "local.txt", "local work\n");
    push_from_other(&repo, "main", "from-main.txt", "main work\n");
    assert!(
        klon(&repo.fx.golden, &["sync", "feature", "--onto", "main"])
            .status
            .success()
    );

    push_from_other(&repo, "feature", "from-other.txt", "other work\n");
    let out = klon(&repo.fx.golden, &["sync", "feature", "--json"]);
    assert!(
        out.status.success(),
        "the plain sync after --onto must not report a force-push: {}",
        stderr(&out)
    );
    assert_eq!(parse(&stdout(&out))["action"], "rebase");
}

/// A branch with no upstream syncs onto `base`.
#[test]
fn sync_of_a_branch_without_an_upstream_uses_base() {
    let repo = setup(20);
    // A branch that only this repository knows.
    git_ok(&repo.fx.golden, &["branch", "solo", "main"]);
    let path = add_klon(&repo.fx.golden, "solo");
    commit_in(&path, "solo.txt", "solo work\n");
    let wanted = push_from_other(&repo, "main", "from-other.txt", "other work\n");

    let out = klon(&repo.fx.golden, &["sync", "solo", "--json"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    assert_eq!(parse(&stdout(&out))["action"], "rebase");
    assert_eq!(
        git_ok(&path, &["rev-parse", "HEAD~1"]).trim(),
        wanted,
        "the klon must sit on the new origin/main"
    );
}

/// `--onto <base>` replaces the sync target.
#[test]
fn sync_onto_a_named_base_rebases_there() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    commit_in(&path, "local.txt", "local work\n");
    let wanted = push_from_other(&repo, "main", "from-other.txt", "other work\n");

    let out = klon(
        &repo.fx.golden,
        &["sync", "feature", "--onto", "main", "--json"],
    );
    assert!(out.status.success(), "sync --onto failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["action"], "rebase");
    assert!(
        git(&path, &["merge-base", "--is-ancestor", &wanted, "HEAD"])
            .status
            .success(),
        "the new origin/main must be an ancestor of the klon"
    );
}

/// AC: `sync --fresh` gives a klon on the same branch with the same HEAD and a
/// manifest equal to golden's ignored state.
#[test]
fn sync_fresh_rebuilds_the_klon_from_golden() {
    let repo = setup(30);
    let path = add_klon(&repo.fx.golden, "feature");
    let before = head(&path);
    // The klon's ignored state drifts away from golden's.
    fs::write(path.join("build").join("stale.bin"), "stale\n").unwrap();
    fs::remove_file(path.join("build").join("o0.bin")).unwrap();

    let out = klon(&repo.fx.golden, &["sync", "feature", "--fresh"]);
    assert!(
        out.status.success(),
        "sync --fresh failed: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains(path.to_str().unwrap()),
        "the line must name the new path: {}",
        stdout(&out)
    );
    assert_eq!(
        git_ok(&path, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "feature",
        "the fresh klon must sit on the same branch"
    );
    assert_eq!(
        head(&path),
        before,
        "the fresh klon must sit at the same HEAD"
    );
    assert_eq!(
        manifest_without_times(&path.join("build")),
        manifest_without_times(&repo.fx.golden.join("build")),
        "the ignored state must match golden again"
    );

    // The klon is registered, clean, and holds no leftover of the old tree.
    assert!(git_ok(&repo.fx.golden, &["worktree", "list"]).contains(path.to_str().unwrap()));
    assert_eq!(git_ok(&path, &["status", "--porcelain"]), "");
}

/// `--fresh` takes no hot spare. A spare made before golden's last build
/// holds the old ignored state, and `git checkout --force` rewrites only
/// tracked paths, so a spare would break the promise of `--fresh`.
#[test]
fn sync_fresh_ignores_a_stale_spare() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    // A spare of golden as it is now.
    let out = klon_env(
        &repo.fx.golden,
        &[("KLON_SPARE", OsStr::new("1"))],
        &["spare-build", repo.fx.golden.to_str().unwrap()],
    );
    assert!(out.status.success(), "spare-build failed: {}", stderr(&out));
    // Golden's ignored state moves on, so the spare is stale.
    fs::write(repo.fx.golden.join("build").join("fresh.bin"), "new\n").unwrap();
    fs::remove_file(repo.fx.golden.join("build").join("o0.bin")).unwrap();

    let out = klon_env(
        &repo.fx.golden,
        &[("KLON_SPARE", OsStr::new("1"))],
        &["sync", "feature", "--fresh"],
    );
    assert!(
        out.status.success(),
        "sync --fresh failed: {}",
        stderr(&out)
    );
    assert_eq!(
        manifest_without_times(&path.join("build")),
        manifest_without_times(&repo.fx.golden.join("build")),
        "--fresh must give golden's ignored state of now, not the spare's"
    );

    // The rebuild leaves a builder behind, so the next `add` is warm again.
    let spare = repo
        .fx
        .golden
        .parent()
        .unwrap()
        .join("golden.wt")
        .join(".spare")
        .join(".klon")
        .join("spare.json");
    assert!(
        wait_until(|| spare.is_file(), Duration::from_secs(60)),
        "--fresh must start the next spare"
    );
}

/// `--fresh` refuses a dirty klon: it must never lose work.
#[test]
fn sync_fresh_refuses_a_dirty_klon() {
    let repo = setup(20);
    let path = add_klon(&repo.fx.golden, "feature");
    fs::write(path.join("f2.txt"), "uncommitted work\n").unwrap();

    let out = klon(&repo.fx.golden, &["sync", "feature", "--fresh"]);
    assert!(!out.status.success(), "--fresh must refuse a dirty klon");
    assert!(
        stderr(&out).contains("dirty"),
        "the refusal must say dirty: {}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(path.join("f2.txt")).unwrap(),
        "uncommitted work\n",
        "the work must survive"
    );
}

/// `--all` prints one line per klon, keeps going after a failure, and ends
/// non-zero. It fetches once, not once per klon.
#[test]
fn sync_all_reports_every_klon_and_fetches_once() {
    let repo = setup(20);
    git_ok(&repo.fx.golden, &["branch", "second", "main"]);
    git_ok(&repo.fx.golden, &["push", "-q", "-u", "origin", "second"]);
    let feature = add_klon(&repo.fx.golden, "feature");
    let second = add_klon(&repo.fx.golden, "second");
    push_from_other(&repo, "feature", "from-other.txt", "other work\n");

    // `second` gets a unique local commit and a rewritten upstream, so it
    // refuses while `feature` fast-forwards.
    commit_in(&second, "local.txt", "local work\n");
    push_from_other(&repo, "second", "a.txt", "a\n");
    // This repository has to see the commit that the force-push replaces,
    // else the rewrite reads as a plain fast-forward.
    git_ok(&repo.fx.golden, &["fetch", "-q", "origin"]);
    force_push_from_other(&repo, "second", "rewritten.txt", "rewritten\n");

    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("git.log");
    let bin = git_shim(tmp.path(), &log);
    let out = klon_logged(&repo.fx.golden, &bin, &["sync", "--all"]);
    assert!(
        !out.status.success(),
        "one klon refused, so --all must fail"
    );
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one line per klon: {lines:?}");
    assert!(
        lines.iter().any(|line| line.contains("fast-forward")),
        "feature must fast-forward: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("refused")),
        "second must refuse: {lines:?}"
    );
    assert!(
        feature.join("from-other.txt").is_file(),
        "a refusal must not stop the other klon"
    );

    // One `fetch origin` for the whole common directory, not one per klon.
    let fetches = logged_fetches(&log);
    assert_eq!(fetches.len(), 1, "sync --all must fetch once: {fetches:?}");
}

/// `sync <branch>` with no origin remote still syncs onto the local base.
#[test]
fn sync_without_an_origin_remote_uses_the_local_base() {
    let fx = plain(20);
    let path = add_klon(&fx.golden, "feature");
    let base = head(&fx.golden);

    let out = klon(&fx.golden, &["sync", "feature", "--json"]);
    assert!(out.status.success(), "sync failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no origin remote"),
        "sync must say why it skipped the fetch: {}",
        stderr(&out)
    );
    assert_eq!(parse(&stdout(&out))["action"], "rebase");
    assert!(
        git(&path, &["merge-base", "--is-ancestor", &base, "HEAD"])
            .status
            .success(),
        "the klon must sit on the local main"
    );
}

/// `sync` needs a branch or `--all`.
#[test]
fn sync_without_a_branch_and_without_all_refuses() {
    let fx = plain(20);
    let out = klon(&fx.golden, &["sync"]);
    assert!(!out.status.success(), "sync must name what to do");
    assert!(
        stderr(&out).contains("--all"),
        "the refusal must name --all: {}",
        stderr(&out)
    );
}
