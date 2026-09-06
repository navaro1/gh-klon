//! Acceptance tests for `gh klon hibernate`, `gh klon wake`, and the
//! `disk_budget` gate of `add` (spec §7 C29, R28). The shared harness lives in
//! `tests/common`.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{git_ok, klon, klon_env, stderr, stdout, Fixture};

const SEED: u64 = 42;

/// `add feature` and assert that it worked.
fn add_feature(fx: &Fixture) -> PathBuf {
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    fx.default_klon_path()
}

fn worktree_list(golden: &Path) -> String {
    git_ok(golden, &["worktree", "list", "--porcelain"])
}

/// Poll `cond` until it holds or the timeout passes.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cond()
}

/// `<common>/klon/journal`: an entry here after a command means the command
/// left a transaction open.
fn journal_entries(golden: &Path) -> Vec<String> {
    let dir = golden.join(".git").join("klon").join("journal");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names
}

/// Every file below `root` with its bytes, paths relative to `root`. `.git` and
/// `.klon` are left out: git's admin files and klon's envelope are not the
/// user's work, and both change on every command.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name == ".git" || name == ".klon" {
                continue;
            }
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The bytes that a hibernated klon costs outside the object store: the record
/// and the ref, both under `<common>`.
fn metadata_bytes(golden: &Path) -> u64 {
    let common = golden.join(".git");
    let mut total = 0;
    for rel in ["klon/hibernate", "refs/klon"] {
        total += tree_bytes(&common.join(rel));
    }
    total
}

/// `du -sb` in Rust: the apparent size of every file below `path`.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    for entry in fs::read_dir(path).unwrap().flatten() {
        total += tree_bytes(&entry.path());
    }
    total
}

// --- AC 1: hibernate then wake restores both files byte for byte ------------

#[test]
fn hibernate_then_wake_restores_the_work_byte_for_byte() {
    let fx = Fixture::generate(SEED, 60, 5, 8, 0);
    let klon_path = add_feature(&fx);

    // A modified tracked file, a new untracked file, and a deleted tracked
    // file: the three shapes that a work in progress takes.
    let tracked = fx.tracked_rel(3);
    fs::write(klon_path.join(&tracked), "modified by the test\n").unwrap();
    fs::create_dir_all(klon_path.join("notes")).unwrap();
    fs::write(klon_path.join("notes/todo.md").as_path(), "one\ntwo\n").unwrap();
    fs::write(klon_path.join("scratch.txt"), "untracked at the root\n").unwrap();
    fs::remove_file(klon_path.join("f2.txt")).unwrap();

    let before_status = git_ok(&klon_path, &["status", "--porcelain"]);
    let before_head = git_ok(&klon_path, &["rev-parse", "HEAD"]);
    let before_files = files(&klon_path);
    assert!(
        before_status.contains(" M ") && before_status.contains("??"),
        "the test needs a modified and an untracked file: {before_status}"
    );

    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));
    assert!(!klon_path.exists(), "the tree must be gone: {out:?}");
    assert!(
        !worktree_list(&fx.golden).contains(&klon_path.to_string_lossy().to_string()),
        "the klon must be unregistered"
    );
    // The branch stays; only the directory goes.
    assert!(
        git_ok(&fx.golden, &["branch", "--list", "feature"]).contains("feature"),
        "hibernate must keep the branch"
    );
    assert!(
        journal_entries(&fx.golden).is_empty(),
        "hibernate must leave no journal entry"
    );

    let out = klon(&fx.golden, &["wake", "feature"]);
    assert!(out.status.success(), "wake failed: {}", stderr(&out));
    assert!(klon_path.exists(), "the tree must be back");

    let after_status = git_ok(&klon_path, &["status", "--porcelain"]);
    assert_eq!(
        before_status, after_status,
        "git status must be identical before and after"
    );
    assert_eq!(
        before_head,
        git_ok(&klon_path, &["rev-parse", "HEAD"]),
        "HEAD must be identical before and after"
    );
    let after_files = files(&klon_path);
    for (rel, bytes) in &before_files {
        assert_eq!(
            after_files.get(rel),
            Some(bytes),
            "{} must hold the same bytes",
            rel.display()
        );
    }
    assert_eq!(
        before_files.keys().collect::<Vec<_>>(),
        after_files.keys().collect::<Vec<_>>(),
        "the file set must be identical"
    );
    // The record and the ref are gone, so a second wake has nothing to do.
    assert_eq!(metadata_bytes(&fx.golden), 0, "wake must clean up after it");
    assert!(
        journal_entries(&fx.golden).is_empty(),
        "wake must leave no journal entry"
    );
    let out = klon(&fx.golden, &["wake", "feature"]);
    assert!(!out.status.success(), "a second wake must fail");
    assert!(
        stderr(&out).contains("not hibernated"),
        "stderr: {}",
        stderr(&out)
    );
}

// --- AC 2: a hibernated klon uses under 1 MB outside the object store -------

#[test]
fn a_hibernated_klon_costs_under_one_megabyte() {
    let fx = Fixture::generate(SEED, 200, 10, 40, 0);
    let klon_path = add_feature(&fx);
    fs::write(klon_path.join(fx.tracked_rel(1)), "changed\n").unwrap();
    fs::write(klon_path.join("new.txt"), "x".repeat(4096)).unwrap();

    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));

    let bytes = metadata_bytes(&fx.golden);
    assert!(bytes > 0, "the record and the ref must exist");
    assert!(
        bytes < 1024 * 1024,
        "a hibernated klon must cost under 1 MB outside the object store, not {bytes}"
    );
    // `list` shows it with the `zz` marker, so a forgotten klon is visible.
    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(" zz hibernated") && text.contains("feature"),
        "list must mark the hibernated klon: {text}"
    );
    let out = klon(&fx.golden, &["--json", "list"]);
    let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("list --json");
    let rows = doc["klons"].as_array().expect("klons");
    assert_eq!(rows.len(), 1, "one hibernated klon: {rows:?}");
    assert_eq!(rows[0]["hibernated"], serde_json::json!(true));
    assert_eq!(rows[0]["branch"], serde_json::json!("feature"));
}

// --- AC 3: hibernate refuses a klon with a live process ---------------------

#[test]
fn hibernate_refuses_a_klon_with_a_live_process() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 0);
    let klon_path = add_feature(&fx);
    let mut sleep = Command::new("sleep")
        .arg("30")
        .current_dir(&klon_path)
        .spawn()
        .expect("spawn sleep in the klon");

    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(
        !out.status.success(),
        "hibernate with a live process must fail"
    );
    assert!(
        stderr(&out).contains("live"),
        "stderr must say live: {}",
        stderr(&out)
    );
    assert!(klon_path.exists(), "the tree must still exist");
    assert_eq!(
        metadata_bytes(&fx.golden),
        0,
        "a refused hibernate must save nothing"
    );

    sleep.kill().expect("kill sleep");
    sleep.wait().expect("reap sleep");

    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));
    assert!(!klon_path.exists(), "the tree must be gone");
}

// --- AC 4 and 5: the disk budget --------------------------------------------

/// Two klons at 600 MB each and a budget of 1 GiB, so a third `add` is over.
/// `KLON_TEST_KLON_BYTES` names the per-klon estimate, because a real 600 MB
/// klon would take minutes to build and gigabytes of the test host.
fn budget_fixture(action: Option<&str>) -> (Fixture, PathBuf, PathBuf) {
    let fx = Fixture::generate(SEED, 40, 4, 4, 0);
    let mut toml = String::from("disk_budget = \"1G\"\n");
    if let Some(action) = action {
        toml.push_str(&format!("disk_budget_action = \"{action}\"\n"));
    }
    fs::write(fx.golden.join(".klon.toml"), toml).unwrap();
    git_ok(&fx.golden, &["add", ".klon.toml"]);
    git_ok(&fx.golden, &["commit", "-qm", "budget"]);

    // The first klon is the older one, so it is the eviction candidate.
    let first = klon(&fx.golden, &["add", "old"]);
    assert!(first.status.success(), "add old failed: {}", stderr(&first));
    // The coarse filesystem clock needs a moment to separate the two klons.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = klon(&fx.golden, &["add", "recent"]);
    assert!(
        second.status.success(),
        "add recent failed: {}",
        stderr(&second)
    );
    (fx, PathBuf::from("old"), PathBuf::from("recent"))
}

/// 600 MiB per klon: two klons plus a third cross the 1 GiB budget.
const SIX_HUNDRED_MB: &str = "600M";

#[test]
fn the_disk_budget_refuses_a_third_add_and_names_the_candidate() {
    let (fx, old, _recent) = budget_fixture(None);
    let old_path = fx.klon_path(old.to_str().unwrap());
    let before = worktree_list(&fx.golden);

    let out = klon_env(
        &fx.golden,
        &[("KLON_TEST_KLON_BYTES", OsStr::new(SIX_HUNDRED_MB))],
        &["add", "third"],
    );
    assert!(!out.status.success(), "the third add must fail");
    let text = stderr(&out);
    assert!(text.contains("disk budget"), "stderr: {text}");
    assert!(
        text.contains("old") && text.contains(&old_path.to_string_lossy().to_string()),
        "the refusal must name the least recently used klon: {text}"
    );

    // Nothing changed: the same register list, no new klon, no journal entry.
    assert_eq!(before, worktree_list(&fx.golden), "the register list moved");
    assert!(old_path.exists(), "the candidate must still exist");
    assert!(
        !fx.klon_path("third").exists(),
        "the refused klon must not exist"
    );
    assert!(
        journal_entries(&fx.golden).is_empty(),
        "a refused add must leave no journal entry: {:?}",
        journal_entries(&fx.golden)
    );
    assert_eq!(
        metadata_bytes(&fx.golden),
        0,
        "a refused add must hibernate nothing"
    );
    // The branch klon would have created is not left behind either.
    assert!(
        !git_ok(&fx.golden, &["branch", "--list", "third"]).contains("third"),
        "a refused add must create no branch"
    );
}

#[test]
fn add_evict_hibernates_the_candidate_and_then_succeeds() {
    let (fx, old, recent) = budget_fixture(None);
    let old_path = fx.klon_path(old.to_str().unwrap());
    let recent_path = fx.klon_path(recent.to_str().unwrap());
    fs::write(old_path.join("keep-me.txt"), "work in progress\n").unwrap();

    let out = klon_env(
        &fx.golden,
        &[("KLON_TEST_KLON_BYTES", OsStr::new(SIX_HUNDRED_MB))],
        &["add", "--evict", "third"],
    );
    assert!(out.status.success(), "add --evict failed: {}", stderr(&out));
    assert!(!old_path.exists(), "the candidate must be hibernated");
    assert!(recent_path.exists(), "the recent klon must stay");
    assert!(fx.klon_path("third").exists(), "the new klon must exist");
    assert!(
        journal_entries(&fx.golden).is_empty(),
        "add --evict must leave no journal entry"
    );

    // The evicted klon's work is safe: a wake brings the untracked file back.
    let out = klon(&fx.golden, &["wake", "old"]);
    assert!(out.status.success(), "wake failed: {}", stderr(&out));
    assert_eq!(
        fs::read_to_string(old_path.join("keep-me.txt")).unwrap(),
        "work in progress\n"
    );
}

#[test]
fn the_hibernate_action_evicts_without_the_switch() {
    let (fx, old, _recent) = budget_fixture(Some("hibernate"));
    let old_path = fx.klon_path(old.to_str().unwrap());

    let out = klon_env(
        &fx.golden,
        &[("KLON_TEST_KLON_BYTES", OsStr::new(SIX_HUNDRED_MB))],
        &["add", "third"],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(
        !old_path.exists(),
        "disk_budget_action = hibernate must evict without --evict"
    );
    assert!(fx.klon_path("third").exists(), "the new klon must exist");
}

#[test]
fn an_add_under_the_budget_changes_nothing() {
    let (fx, _old, _recent) = budget_fixture(None);
    // 100 MiB per klon: three klons are far under the 1 GiB budget.
    let out = klon_env(
        &fx.golden,
        &[("KLON_TEST_KLON_BYTES", OsStr::new("100M"))],
        &["add", "third"],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(fx.klon_path("third").exists(), "the new klon must exist");
    assert_eq!(
        metadata_bytes(&fx.golden),
        0,
        "an add under the budget must hibernate nothing"
    );
}

#[test]
fn a_budget_that_is_not_a_size_fails_the_add() {
    let fx = Fixture::generate(SEED, 30, 3, 3, 0);
    fs::write(fx.golden.join(".klon.toml"), "disk_budget = \"lots\"\n").unwrap();
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(!out.status.success(), "an unparsable budget must fail");
    assert!(
        stderr(&out).contains("is not a size"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        !fx.default_klon_path().exists(),
        "a refused add must create nothing"
    );
}

// --- Review regressions ------------------------------------------------------

/// The stat cache must never decide what `hibernate` saves. `add` sets
/// `core.checkStat=minimal`, which compares only the size and the whole seconds
/// of the modification time, so an edit of the same length under the same stamp
/// reads as clean. The bytes must still reach the work commit, because the
/// removal that follows deletes the only other copy of them.
#[test]
fn an_edit_that_the_stat_cache_hides_still_survives() {
    let fx = Fixture::generate(SEED, 40, 4, 4, 0);
    let klon_path = add_feature(&fx);
    let file = klon_path.join(fx.tracked_rel(2));
    let before = fs::read(&file).unwrap();

    // The same length, and the modification time that the index recorded.
    let mut changed = before.clone();
    changed[0] = b'X';
    assert_eq!(changed.len(), before.len(), "the edit must keep the size");
    assert_ne!(changed, before, "the edit must change the bytes");
    let stamp = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, &changed).unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(stamp)
        .unwrap();

    // git itself now reports the klon clean, which is the whole point.
    assert_eq!(
        git_ok(&klon_path, &["status", "--porcelain"]),
        "",
        "the test needs an edit that the minimal stat check hides"
    );

    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));
    let out = klon(&fx.golden, &["wake", "feature"]);
    assert!(out.status.success(), "wake failed: {}", stderr(&out));
    assert_eq!(
        fs::read(&file).unwrap(),
        changed,
        "the hidden edit must survive the round trip"
    );
}

/// `add --evict <branch>` must never hibernate the klon of `<branch>` itself.
/// The eviction would free the branch and the path, `add` would build a fresh
/// klon over them, and the work the eviction just saved would sit behind a path
/// that `wake` can no longer fill.
#[test]
fn evict_never_takes_the_klon_that_the_add_asks_for() {
    let (fx, old, _recent) = budget_fixture(None);
    let old_path = fx.klon_path(old.to_str().unwrap());
    fs::write(old_path.join("keep-me.txt"), "work in progress\n").unwrap();

    let out = klon_env(
        &fx.golden,
        &[("KLON_TEST_KLON_BYTES", OsStr::new(SIX_HUNDRED_MB))],
        &["add", "--evict", "old"],
    );
    assert!(!out.status.success(), "add --evict old must fail");
    assert!(
        old_path.join("keep-me.txt").exists(),
        "the klon of the requested branch must stay: {}",
        stderr(&out)
    );
    assert_eq!(
        metadata_bytes(&fx.golden),
        0,
        "nothing may be hibernated: {}",
        stderr(&out)
    );
}

/// A hibernated branch owns its path. A plain `add` of it would build a second
/// klon there and hide the saved work, so it refuses and names `wake`.
#[test]
fn add_refuses_a_hibernated_branch() {
    let fx = Fixture::generate(SEED, 40, 4, 4, 0);
    let klon_path = add_feature(&fx);
    fs::write(klon_path.join("keep-me.txt"), "work in progress\n").unwrap();
    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));

    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(
        !out.status.success(),
        "add of a hibernated branch must fail"
    );
    assert!(
        stderr(&out).contains("hibernated") && stderr(&out).contains("wake"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(!klon_path.exists(), "a refused add must create nothing");

    // The work is still reachable through wake.
    let out = klon(&fx.golden, &["wake", "feature"]);
    assert!(out.status.success(), "wake failed: {}", stderr(&out));
    assert_eq!(
        fs::read_to_string(klon_path.join("keep-me.txt")).unwrap(),
        "work in progress\n"
    );
}

/// The saved tree is a whole tree, not a patch. A branch that advanced while
/// the klon slept would lose those commits in the working directory, so `wake`
/// refuses before it changes anything and names the way back.
#[test]
fn wake_refuses_a_branch_that_moved_while_it_slept() {
    let fx = Fixture::generate(SEED, 40, 4, 4, 0);
    let klon_path = add_feature(&fx);
    fs::write(klon_path.join("keep-me.txt"), "work in progress\n").unwrap();
    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));

    // Another worktree moves the branch while the klon sleeps.
    git_ok(&fx.golden, &["branch", "-f", "feature", "main"]);

    let out = klon(&fx.golden, &["wake", "feature"]);
    assert!(!out.status.success(), "wake of a moved branch must fail");
    assert!(
        stderr(&out).contains("moved") && stderr(&out).contains("branch -f"),
        "stderr must name the way back: {}",
        stderr(&out)
    );
    assert!(!klon_path.exists(), "a refused wake must create nothing");
    assert!(
        metadata_bytes(&fx.golden) > 0,
        "a refused wake must keep the record and the ref"
    );
}

/// The `wake` journal entry must not share a file with the entry of the `add`
/// it runs, so `doctor --repair` can still name a `wake` that stopped.
#[test]
fn a_killed_wake_leaves_an_entry_that_the_repair_can_close() {
    let fx = Fixture::generate(SEED, 40, 4, 4, 0);
    let klon_path = add_feature(&fx);
    fs::write(klon_path.join("keep-me.txt"), "work in progress\n").unwrap();
    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(out.status.success(), "hibernate failed: {}", stderr(&out));

    // Stop the wake inside its own `add`, after the clone landed.
    let mut child = Command::new(common::BIN)
        .args(["wake", "feature"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_SPARE", "0")
        .env("KLON_TEST_PAUSE_AT", "cloned")
        .spawn()
        .expect("spawn wake");
    let paused = wait_until(
        || {
            journal_entries(&fx.golden)
                .iter()
                .any(|name| name.starts_with("wake-"))
        },
        std::time::Duration::from_secs(30),
    );
    child.kill().expect("kill wake");
    child.wait().expect("reap wake");
    assert!(paused, "the wake must write a journal entry of its own");

    let out = klon(&fx.golden, &["doctor", "--repair"]);
    assert!(
        out.status.success(),
        "doctor --repair failed: {}",
        stderr(&out)
    );
    assert!(
        journal_entries(&fx.golden).is_empty(),
        "the repair must close every entry: {:?}",
        journal_entries(&fx.golden)
    );
    assert!(!klon_path.exists(), "the half-made klon must be gone");

    // The klon stays hibernated, and a fresh wake still brings the work back.
    let out = klon(&fx.golden, &["wake", "feature"]);
    assert!(
        out.status.success(),
        "wake after the repair failed: {}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(klon_path.join("keep-me.txt")).unwrap(),
        "work in progress\n"
    );
}

/// A `hibernate` that stopped before the rename must roll back whole: the
/// record and the ref go, and the klon stays where it is.
#[test]
fn a_hibernate_killed_before_the_rename_rolls_back() {
    let fx = Fixture::generate(SEED, 40, 4, 4, 0);
    let klon_path = add_feature(&fx);
    fs::write(klon_path.join("keep-me.txt"), "work in progress\n").unwrap();

    let mut child = Command::new(common::BIN)
        .args(["hibernate", "feature"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_SPARE", "0")
        .env("KLON_TEST_PAUSE_AT", "removing")
        .spawn()
        .expect("spawn hibernate");
    let paused = wait_until(
        || metadata_bytes(&fx.golden) > 0 && !journal_entries(&fx.golden).is_empty(),
        std::time::Duration::from_secs(30),
    );
    child.kill().expect("kill hibernate");
    child.wait().expect("reap hibernate");
    assert!(paused, "the hibernate must reach the removing state");
    assert!(klon_path.exists(), "the rename must not have run yet");

    let out = klon(&fx.golden, &["doctor", "--repair"]);
    assert!(
        out.status.success(),
        "doctor --repair failed: {}",
        stderr(&out)
    );
    assert!(
        journal_entries(&fx.golden).is_empty(),
        "the repair must close every entry"
    );
    assert_eq!(
        metadata_bytes(&fx.golden),
        0,
        "the repair must drop the record and the ref"
    );
    assert!(klon_path.exists(), "the klon must stay");
    assert_eq!(
        fs::read_to_string(klon_path.join("keep-me.txt")).unwrap(),
        "work in progress\n"
    );

    // The klon is whole, so a fresh hibernate still works.
    let out = klon(&fx.golden, &["hibernate", "feature"]);
    assert!(
        out.status.success(),
        "hibernate after the repair failed: {}",
        stderr(&out)
    );
}
