//! Acceptance tests for the journal and `gh klon doctor` (spec §7 C4).
//! The shared harness lives in `tests/common`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{assert_clean, git_ok, klon, stderr, stdout, Fixture, BIN};
use serde_json::Value;

const SEED: u64 = 42;

/// `<golden>/.git/klon/journal`, the journal of the main repository.
fn journal_dir(golden: &Path) -> PathBuf {
    golden.join(".git").join("klon").join("journal")
}

/// Every `<name>.json` under the journal directory, sorted.
fn entries(golden: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = match fs::read_dir(journal_dir(golden)) {
        Ok(read) => read
            .flatten()
            .map(|item| item.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    found
}

/// The `state` of the single open entry, or None when no entry is open.
fn open_state(golden: &Path) -> Option<String> {
    let files = entries(golden);
    let first = files.first()?;
    let text = fs::read_to_string(first).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(value["state"].as_str()?.to_string())
}

fn worktree_list(golden: &Path) -> String {
    git_ok(golden, &["worktree", "list", "--porcelain"])
}

/// Poll `cond` until it holds or the timeout passes.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Start `gh-klon add feature` with the test-only pause at `state`.
fn spawn_paused_add(golden: &Path, state: &str) -> Child {
    spawn_paused(golden, state, &["add", "feature"])
}

/// Start `gh-klon <args>` with the test-only pause at `state`.
fn spawn_paused(golden: &Path, state: &str, args: &[&str]) -> Child {
    Command::new(BIN)
        .args(args)
        .current_dir(golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_TEST_PAUSE_AT", state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start gh-klon")
}

/// True when the journal directory entry named `file` is a landed entry.
///
/// `journal::write` lands through a `.<name>.<pid>.tmp` file in the same
/// directory, so a test that only asks for an extension can see the write in
/// flight and kill the writer before the rename. The rule here is the one
/// `journal::list` uses.
fn is_entry(file: &std::ffi::OsStr) -> bool {
    let name = file.to_string_lossy();
    name.ends_with(".json") && !name.starts_with('.')
}

/// True when `inbox` holds a landed entry in `state`.
///
/// A command writes several states into one file name, so "a file exists" is
/// not "the command reached the state I paused it at". `rm` writes `planned`
/// first and `removing` second, and a test that waits for the file alone can
/// kill the process between the two.
fn entry_in_state(inbox: &Path, state: &str) -> bool {
    let Ok(read) = fs::read_dir(inbox) else {
        return false;
    };
    read.flatten()
        .filter(|item| is_entry(&item.file_name()))
        .filter_map(|item| fs::read_to_string(item.path()).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .any(|entry| entry["state"] == state)
}

fn sigkill(child: &Child) {
    // SAFETY: `kill` takes a pid and a signal number and returns an error code.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL failed");
}

/// The AC: `add` killed between registration and checkout leaves a `registered`
/// entry and a locked worktree; `doctor --repair` removes both; a repeated
/// `add` then completes and leaves no entry.
#[test]
fn a_killed_add_is_repaired_and_the_next_add_completes() {
    let fx = Fixture::generate(SEED, 60, 5, 10, 3);
    let path = fx.default_klon_path();

    let mut child = spawn_paused_add(&fx.golden, "registered");
    let reached = wait_until(
        || open_state(&fx.golden).as_deref() == Some("registered"),
        Duration::from_secs(30),
    );
    if !reached {
        sigkill(&child);
        let _ = child.wait();
        panic!("add never reached the registered state");
    }
    sigkill(&child);
    let status = child.wait().expect("wait for the killed add");
    assert!(!status.success(), "the killed add must not report success");

    // The AC: a registered journal entry and a locked worktree.
    assert_eq!(open_state(&fx.golden).as_deref(), Some("registered"));
    let list = worktree_list(&fx.golden);
    assert!(
        list.contains(path.to_str().unwrap()),
        "the klon must stay registered: {list}"
    );
    assert!(list.contains("locked"), "the klon must stay locked: {list}");

    // The AC: `doctor --repair` unlocks, removes, and clears the entry.
    let out = klon(&fx.golden, &["doctor", "--repair"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let printed = stdout(&out);
    assert!(
        printed.contains("repair"),
        "the repair must print one line per action: {printed}"
    );
    let list = worktree_list(&fx.golden);
    assert!(
        !list.contains(path.to_str().unwrap()),
        "the klon must be gone: {list}"
    );
    assert!(entries(&fx.golden).is_empty(), "the entry must be gone");

    // The AC: a repeated `add` completes and the entry is gone.
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(
        out.status.success(),
        "the repeated add failed: {}",
        stderr(&out)
    );
    assert!(path.is_dir(), "the klon must exist");
    assert_clean(&path);
    assert!(
        entries(&fx.golden).is_empty(),
        "a completed add leaves no entry"
    );
}

/// The AC: a repeated `add` after the kill completes the klon, with no
/// `doctor --repair` in between. `add` closes the entry of its own destination
/// first (R6), so the leftover `.git` file cannot refuse it with `path not
/// empty`.
#[test]
fn a_repeated_add_recovers_without_doctor() {
    let fx = Fixture::generate(SEED, 60, 5, 10, 3);
    let path = fx.default_klon_path();

    let mut child = spawn_paused_add(&fx.golden, "registered");
    let reached = wait_until(
        || open_state(&fx.golden).as_deref() == Some("registered"),
        Duration::from_secs(30),
    );
    sigkill(&child);
    let _ = child.wait();
    assert!(reached, "add never reached the registered state");
    assert!(path.join(".git").exists(), "the kill leaves a .git file");

    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(
        out.status.success(),
        "the repeated add failed: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("klon: recovery:"),
        "the recovery must print what it did: {}",
        stderr(&out)
    );
    assert_clean(&path);
    assert!(
        entries(&fx.golden).is_empty(),
        "a completed add leaves no entry"
    );
    let list = worktree_list(&fx.golden);
    assert!(
        list.contains(path.to_str().unwrap()),
        "the klon must be registered: {list}"
    );
    assert!(
        !list.contains("locked"),
        "the completed klon must be unlocked: {list}"
    );
}

/// A command that completes leaves no entry behind.
#[test]
fn a_completed_add_leaves_no_entry() {
    let fx = Fixture::generate(SEED, 40, 4, 5, 3);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(entries(&fx.golden).is_empty(), "no entry may survive add");

    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
    assert!(entries(&fx.golden).is_empty(), "no entry may survive rm");
}

/// The AC: a journal file with `"version": 99` makes `doctor` exit non-zero
/// with `unknown journal version` and change nothing.
#[test]
fn an_unknown_journal_version_fails_closed() {
    let fx = Fixture::generate(SEED, 40, 4, 5, 3);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let dir = journal_dir(&fx.golden);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("future.json");
    let body = r#"{"version":99,"op":"add","state":"landed","path":"/nowhere","branch":"feature","started":"2026-09-05T10:00:00Z","extra":[1,2]}"#;
    fs::write(&file, body).unwrap();
    let before = worktree_list(&fx.golden);

    for args in [
        vec!["doctor"],
        vec!["doctor", "--json"],
        vec!["doctor", "--repair"],
    ] {
        let out = klon(&fx.golden, &args);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            stderr(&out).contains("unknown journal version"),
            "{args:?} must name the version: {}",
            stderr(&out)
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            body,
            "{args:?} must not change the entry"
        );
        assert_eq!(
            worktree_list(&fx.golden),
            before,
            "{args:?} changed a worktree"
        );
    }
}

/// The AC: `doctor` run twice gives byte-equal JSON except the timestamp.
#[test]
fn two_doctor_runs_agree_except_on_the_timestamp() {
    let fx = Fixture::generate(SEED, 30, 3, 3, 2);
    let first = doctor_json(&fx);
    std::thread::sleep(Duration::from_millis(1100));
    let second = doctor_json(&fx);
    assert_ne!(first, second, "the timestamp must move");
    assert_eq!(
        blank_timestamp(&first),
        blank_timestamp(&second),
        "doctor must be repeatable"
    );
}

fn doctor_json(fx: &Fixture) -> String {
    let out = klon(&fx.golden, &["doctor", "--json"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    stdout(&out)
}

/// Cut the value of the `timestamp` field out of the raw text, so the rest is
/// compared byte for byte.
fn blank_timestamp(text: &str) -> String {
    let key = "\"timestamp\":\"";
    let start = text.find(key).expect("a timestamp field") + key.len();
    let end = start + text[start..].find('"').expect("the end of the timestamp");
    format!("{}{}", &text[..start], &text[end..])
}

/// `doctor` reports the host rows that R31 names, and the human report names
/// every open entry.
#[test]
fn doctor_reports_the_host_rows() {
    let fx = Fixture::generate(SEED, 20, 2, 2, 2);
    let out = klon(&fx.golden, &["doctor"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let printed = stdout(&out);
    for row in [
        "git",
        "filesystem",
        "btrfs-progs",
        "inotify.max_user_watches",
        "inotify.max_user_instances",
        "make",
        "ninja",
        "pasta",
        "journal",
    ] {
        assert!(
            printed.contains(row),
            "the report must hold {row}: {printed}"
        );
    }

    let out = klon(&fx.golden, &["doctor", "--json"]);
    let report: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    let features = report["features"].as_object().expect("an object");
    assert!(features.contains_key("btrfs-progs"));
    assert!(!report["git_version"].as_str().unwrap().is_empty());
    assert!(!report["filesystem"].as_str().unwrap().is_empty());
}

/// A `removing` entry makes `doctor --repair` finish the `rm` tail: it drops
/// the `.git` file that the trash copy still holds and prunes the worktree.
#[test]
fn a_killed_rm_is_repaired() {
    let fx = Fixture::generate(SEED, 40, 4, 5, 3);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let path = fx.default_klon_path();

    // Reproduce the state after a kill between the rename and the prune: the
    // klon sits in the trash, still holds its `.git` file, and git still lists
    // it. The journal entry says `removing`.
    let trash = fx.golden.parent().unwrap().join("golden.wt").join(".trash");
    fs::create_dir_all(&trash).unwrap();
    let victim = trash.join("feature-1700000000");
    fs::rename(&path, &victim).unwrap();
    assert!(victim.join(".git").is_file(), "the trash copy keeps .git");
    let dir = journal_dir(&fx.golden);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("feature-test.json"),
        serde_json::json!({
            "version": 1,
            "op": "rm",
            "state": "removing",
            "path": path,
            "branch": "feature",
            "started": "2026-09-05T10:00:00Z",
        })
        .to_string(),
    )
    .unwrap();

    let out = klon(&fx.golden, &["doctor", "--repair"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    assert!(!victim.join(".git").exists(), "the .git file must be gone");
    let list = worktree_list(&fx.golden);
    assert!(
        !list.contains(path.to_str().unwrap()),
        "prune must drop the klon: {list}"
    );
    assert!(entries(&fx.golden).is_empty(), "the entry must be gone");
    // The repair also starts the background delete that the killed `rm` never
    // reached, so the trash copy does not wait for the next `prune`.
    assert!(
        wait_until(|| !victim.exists(), Duration::from_secs(30)),
        "the trash copy must be deleted in the background"
    );
}

/// `rm` derives the common directory from golden instead of asking `git` a
/// second time, because one more process costs 10 to 50 ms of its 100 ms budget
/// (R8). The derived directory must be the one `doctor` reads. Both layouts of
/// the main worktree are checked: a `.git` directory and a `.git` file that
/// names a repository directory elsewhere.
#[test]
fn rm_writes_its_entry_where_doctor_reads_it() {
    for separate_git_dir in [false, true] {
        let fx = Fixture::generate(SEED, 30, 3, 3, 2);
        if separate_git_dir {
            let elsewhere = fx.golden.parent().unwrap().join("repository");
            fs::rename(fx.golden.join(".git"), &elsewhere).unwrap();
            fs::write(
                fx.golden.join(".git"),
                format!("gitdir: {}\n", elsewhere.display()),
            )
            .unwrap();
        }
        let common = PathBuf::from(
            git_ok(
                &fx.golden,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .trim(),
        );
        let out = klon(&fx.golden, &["add", "feature"]);
        assert!(out.status.success(), "add failed: {}", stderr(&out));

        // Pause `rm` after it wrote `removing` and before the rename.
        let mut child = spawn_paused(&fx.golden, "removing", &["rm", "feature"]);
        let inbox = common.join("klon").join("journal");
        let reached = wait_until(
            || entry_in_state(&inbox, "removing"),
            Duration::from_secs(30),
        );
        sigkill(&child);
        let _ = child.wait();
        assert!(
            reached,
            "rm must write its entry under {} (separate git dir: {separate_git_dir})",
            inbox.display()
        );

        // `doctor` reads the same directory, so it sees the entry and closes it.
        let out = klon(&fx.golden, &["doctor", "--json"]);
        assert!(out.status.success(), "doctor failed: {}", stderr(&out));
        let report: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
        let rows = report["journal"].as_array().expect("an array");
        assert_eq!(rows.len(), 1, "doctor must see the entry: {rows:?}");
        assert_eq!(rows[0]["op"], "rm");
        assert_eq!(rows[0]["state"], "removing");

        let out = klon(&fx.golden, &["doctor", "--repair"]);
        assert!(out.status.success(), "repair failed: {}", stderr(&out));
        assert!(
            !fs::read_dir(&inbox)
                .map(|read| read.flatten().any(|i| is_entry(&i.file_name())))
                .unwrap_or(false),
            "the repair must close the entry"
        );
    }
}

/// `--json` is global, so clap accepts it everywhere. A command with no JSON
/// document refuses the flag instead of ignoring it.
#[test]
fn json_is_refused_where_no_document_exists() {
    let fx = Fixture::generate(SEED, 20, 2, 2, 2);
    for command in ["up", "prune"] {
        let out = klon(&fx.golden, &[command, "--json"]);
        assert!(!out.status.success(), "{command} --json must fail");
        assert!(
            stderr(&out).contains("--json is not available"),
            "{command} --json must say why: {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "", "{command} --json must print no document");
    }
    // The same commands still work without the flag.
    for command in ["up", "prune"] {
        let out = klon(&fx.golden, &[command]);
        assert!(out.status.success(), "{command} failed: {}", stderr(&out));
    }
}

/// An interrupted `rm` that changed nothing leaves the klon in place.
#[test]
fn a_planned_rm_entry_keeps_the_klon() {
    let fx = Fixture::generate(SEED, 40, 4, 5, 3);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let path = fx.default_klon_path();

    let dir = journal_dir(&fx.golden);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("feature-test.json"),
        serde_json::json!({
            "version": 1,
            "op": "rm",
            "state": "planned",
            "path": path,
            "branch": "feature",
            "started": "2026-09-05T10:00:00Z",
        })
        .to_string(),
    )
    .unwrap();

    let out = klon(&fx.golden, &["doctor", "--repair"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    assert!(path.is_dir(), "the klon must stay");
    assert!(
        worktree_list(&fx.golden).contains(path.to_str().unwrap()),
        "the klon must stay registered"
    );
    assert!(entries(&fx.golden).is_empty(), "the entry must be gone");
}
