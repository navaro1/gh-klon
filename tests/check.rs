//! Acceptance tests for C26: `gh klon check`, the receipt, and the `merge`
//! receipt gate. Each test drives the real command against a generated
//! fixture with a committed `[proof]` table.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use common::{git_ok, identity, klon, klon_env, stderr, stdout, Fixture};
use serde_json::Value;

const SEED: u64 = 26;

// --- Helpers -----------------------------------------------------------------

/// A fixture with a committer identity and a committed `.klon.toml` that names
/// `steps`. The file must be committed: an untracked file makes golden dirty,
/// and a dirty golden fails `merge` before any gate runs.
fn repo(steps: &str) -> Fixture {
    let fx = Fixture::generate(SEED, 20, 4, 3, 2);
    identity(&fx.golden);
    fs::write(
        fx.golden.join(".klon.toml"),
        format!("[proof]\nsteps = [{steps}]\n"),
    )
    .expect("write .klon.toml");
    git_ok(&fx.golden, &["add", ".klon.toml"]);
    git_ok(&fx.golden, &["commit", "-qm", "add the proof steps"]);
    fx
}

/// `gh klon add <branch>`, with the klon path as the answer.
fn add(fx: &Fixture, branch: &str) -> PathBuf {
    let out = klon(&fx.golden, &["add", branch]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    fx.klon_path(branch)
}

/// Run klon with an isolated approval store, so the test never writes into the
/// user's config directory and `--yes` records its approval in the fixture.
fn run(fx: &Fixture, args: &[&str]) -> std::process::Output {
    run_env(fx, &[], args)
}

/// `run` with extra environment variables.
fn run_env(fx: &Fixture, envs: &[(&str, &OsStr)], args: &[&str]) -> std::process::Output {
    let mut all: Vec<(&str, &OsStr)> =
        vec![("KLON_CONFIG_HOME", fx.golden.parent().unwrap().as_os_str())];
    all.extend_from_slice(envs);
    let mut with_yes = vec!["--yes"];
    with_yes.extend_from_slice(args);
    klon_env(&fx.golden, &all, &with_yes)
}

/// `<golden>/.git/klon/receipts`, the receipt directory of the fixture.
fn receipts(fx: &Fixture) -> PathBuf {
    fx.golden.join(".git").join("klon").join("receipts")
}

/// Every receipt file in the directory, sorted by name.
fn receipt_files(fx: &Fixture) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(receipts(fx)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

/// The receipt of `commit`, parsed.
fn receipt(fx: &Fixture, commit: &str) -> Value {
    let file = receipts(fx).join(format!("{commit}.json"));
    let text =
        fs::read_to_string(&file).unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
    serde_json::from_str(&text).expect("a receipt is one JSON document")
}

fn head(dir: &Path) -> String {
    git_ok(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Commit `body` at `rel` in `dir`.
fn commit(dir: &Path, rel: &str, body: &str, message: &str) {
    fs::write(dir.join(rel), body).expect("write the file");
    git_ok(dir, &["add", rel]);
    git_ok(dir, &["commit", "-qm", message]);
}

// --- AC 1: a dirty klon ------------------------------------------------------

#[test]
fn check_refuses_a_dirty_klon_and_writes_no_receipt() {
    let fx = repo("\"true\"");
    let klon_dir = add(&fx, "feature");
    fs::write(klon_dir.join("f2.txt"), "uncommitted work\n").expect("dirty the klon");

    let out = run(&fx, &["check", "feature"]);
    assert!(!out.status.success(), "a dirty klon must fail the check");
    assert!(
        stderr(&out).contains("dirty"),
        "stderr must name the dirty tree: {}",
        stderr(&out)
    );
    assert!(
        receipt_files(&fx).is_empty(),
        "a refused check must write no receipt"
    );
}

/// The other refusal: a repository with no `[proof]` table has nothing to
/// prove, and `check` says so instead of writing an empty receipt.
#[test]
fn check_refuses_a_repository_without_proof_steps() {
    let fx = Fixture::generate(SEED, 20, 4, 3, 2);
    identity(&fx.golden);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let out = klon(&fx.golden, &["check", "feature"]);
    assert!(!out.status.success(), "no steps must fail the check");
    assert!(
        stderr(&out).contains("no [proof] steps"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(receipt_files(&fx).is_empty(), "no receipt without steps");
}

// --- AC 2: a failing step ----------------------------------------------------

#[test]
fn a_failing_step_writes_a_failed_receipt_and_merge_refuses() {
    let fx = repo("\"true\", \"exit 3\", \"true\"");
    let klon_dir = add(&fx, "feature");
    let commit_id = head(&klon_dir);
    let before = head(&fx.golden);

    let out = run(&fx, &["check", "feature"]);
    assert!(!out.status.success(), "a failed step must fail the check");

    let record = receipt(&fx, &commit_id);
    assert_eq!(record["version"], 1);
    assert_eq!(record["status"], "failed");
    assert_eq!(record["commit"], commit_id);
    assert_eq!(record["branch"], "feature");
    let results = record["results"].as_array().expect("an array");
    assert_eq!(results.len(), 2, "the run stops at the first failure");
    assert_eq!(results[0]["cmd"], "true");
    assert_eq!(results[0]["status"], "pass");
    assert_eq!(results[1]["cmd"], "exit 3");
    assert_eq!(results[1]["status"], "failed");

    let out = run(&fx, &["merge", "feature"]);
    assert!(
        !out.status.success(),
        "a failed receipt must fail the merge"
    );
    assert!(
        stderr(&out).contains("receipt failed"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
    assert!(klon_dir.exists(), "the klon must stay");
}

// --- AC 3: one more commit makes the receipt stale ---------------------------

#[test]
fn one_more_commit_makes_the_receipt_stale_and_no_check_proceeds() {
    let fx = repo("\"true\"");
    let klon_dir = add(&fx, "feature");

    let out = run(&fx, &["check", "feature"]);
    assert!(out.status.success(), "check failed: {}", stderr(&out));
    let before = head(&fx.golden);

    commit(&klon_dir, "after.txt", "one more commit\n", "one more");

    let out = run(&fx, &["merge", "feature"]);
    assert!(!out.status.success(), "a stale receipt must fail the merge");
    assert!(
        stderr(&out).contains("receipt stale"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");

    let out = run(&fx, &["merge", "--no-check", "feature"]);
    assert!(
        out.status.success(),
        "--no-check must proceed: {}",
        stderr(&out)
    );
    assert_ne!(head(&fx.golden), before, "base must take the branch");
    assert!(!klon_dir.exists(), "the merge removes the klon");
}

/// A changed `[proof] steps` list makes an existing receipt stale too: the
/// receipt proves the old steps, not the new ones.
#[test]
fn changed_proof_steps_make_the_receipt_stale() {
    let fx = repo("\"true\"");
    let klon_dir = add(&fx, "feature");

    let out = run(&fx, &["check", "feature"]);
    assert!(out.status.success(), "check failed: {}", stderr(&out));

    commit(
        &fx.golden,
        ".klon.toml",
        "[proof]\nsteps = [\"true\", \"true\"]\n",
        "change the proof steps",
    );

    let out = run(&fx, &["merge", "feature"]);
    assert!(!out.status.success(), "changed steps must fail the merge");
    assert!(
        stderr(&out).contains("receipt stale"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(klon_dir.exists(), "the klon must stay");
}

// --- AC 4: the receipt holds no environment values ---------------------------

#[test]
fn the_receipt_holds_no_environment_value() {
    // The step reads the canary, so the value is in the step's own environment
    // and in its output. It must still not reach the receipt.
    let fx = repo("\"echo \\\"$KLON_CANARY\\\"\"");
    let klon_dir = add(&fx, "feature");
    let canary = format!(
        "canary-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    let out = run_env(
        &fx,
        &[("KLON_CANARY", OsStr::new(canary.as_str()))],
        &["check", "feature"],
    );
    assert!(out.status.success(), "check failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains(&canary) || stderr(&out).contains(&canary),
        "the step must really see the canary"
    );

    let files = receipt_files(&fx);
    assert_eq!(files.len(), 1, "one check writes one receipt");
    for file in &files {
        let text = fs::read_to_string(file).expect("read the receipt");
        assert!(
            !text.contains(&canary),
            "the receipt must hold no environment value:\n{text}"
        );
        // The step text itself is recorded, unexpanded.
        assert!(
            text.contains("KLON_CANARY"),
            "the receipt records the step text:\n{text}"
        );
    }

    // No environment fact of any kind reaches the file.
    let record = receipt(&fx, &head(&klon_dir));
    for absent in ["env", "cwd", "hostname", "path", "environment"] {
        assert!(
            record.get(absent).is_none(),
            "the receipt must have no {absent} field: {record}"
        );
    }
}

// --- AC 5: a passing check lets the merge through ----------------------------

#[test]
fn a_passing_check_lets_the_merge_through() {
    let fx = repo("\"true\", \"exit 0\"");
    let klon_dir = add(&fx, "feature");
    let commit_id = head(&klon_dir);
    let before = head(&fx.golden);

    let out = run(&fx, &["check", "feature"]);
    assert!(out.status.success(), "check failed: {}", stderr(&out));
    let record = receipt(&fx, &commit_id);
    assert_eq!(record["status"], "pass");
    assert_eq!(record["results"].as_array().expect("an array").len(), 2);
    assert_eq!(
        record["tree"],
        git_ok(&klon_dir, &["rev-parse", "HEAD^{tree}"]).trim()
    );

    let out = run(&fx, &["merge", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    assert_ne!(head(&fx.golden), before, "base must take the branch");
    assert!(!klon_dir.exists(), "the merge removes the klon");
}

/// The `merge --json` report names the gate that the receipt satisfied.
#[test]
fn the_merge_report_names_the_proof_gate() {
    let fx = repo("\"true\"");
    add(&fx, "feature");
    let out = run(&fx, &["check", "feature"]);
    assert!(out.status.success(), "check failed: {}", stderr(&out));

    let out = run(&fx, &["merge", "--json", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    let report: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(report["hook"], "proof.steps");
}

// --- AC 6: the list mark -----------------------------------------------------

#[test]
fn list_marks_a_klon_with_a_passing_receipt() {
    let fx = repo("\"true\"");
    let klon_dir = add(&fx, "feature");

    // Before the check the column is empty and the JSON field is null.
    let out = klon(&fx.golden, &["list", "--json", "--no-gh"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    let list: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert!(
        list["klons"][0]["receipt"].is_null(),
        "an unchecked klon reports null: {}",
        list["klons"][0]
    );

    let out = run(&fx, &["check", "feature"]);
    assert!(out.status.success(), "check failed: {}", stderr(&out));

    let out = klon(&fx.golden, &["list", "--no-gh"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains('✓'),
        "list must mark the klon: {}",
        stdout(&out)
    );

    let out = klon(&fx.golden, &["list", "--json", "--no-gh"]);
    let list: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(list["klons"][0]["receipt"], "pass");

    // One more commit and the same klon reads stale in both forms.
    commit(&klon_dir, "after.txt", "one more commit\n", "one more");
    let out = klon(&fx.golden, &["list", "--json", "--no-gh"]);
    let list: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(list["klons"][0]["receipt"], "stale");
    let out = klon(&fx.golden, &["list", "--no-gh"]);
    assert!(
        stdout(&out).contains("stale"),
        "list must mark the stale klon: {}",
        stdout(&out)
    );
}

// --- prune -------------------------------------------------------------------

/// `prune` drops a receipt older than 30 days and keeps a fresh one.
#[test]
fn prune_removes_a_receipt_older_than_thirty_days() {
    let fx = repo("\"true\"");
    add(&fx, "feature");
    let out = run(&fx, &["check", "feature"]);
    assert!(out.status.success(), "check failed: {}", stderr(&out));
    assert_eq!(receipt_files(&fx).len(), 1);

    // A second receipt, aged past the limit by its own timestamp.
    let old = receipts(&fx).join("0000000000000000000000000000000000000000.json");
    fs::write(&old, "{}").expect("write the old receipt");
    let long_ago = filetime::FileTime::from_system_time(
        SystemTime::now() - std::time::Duration::from_secs(31 * 24 * 60 * 60),
    );
    filetime::set_file_mtime(&old, long_ago).expect("age the old receipt");
    assert_eq!(receipt_files(&fx).len(), 2);

    let out = klon(&fx.golden, &["prune"]);
    assert!(out.status.success(), "prune failed: {}", stderr(&out));
    let left = receipt_files(&fx);
    assert_eq!(left.len(), 1, "prune keeps the fresh receipt only");
    assert!(!old.exists(), "prune removes the aged receipt");
}
