//! Acceptance tests for the hot spare (spec §7 C9, R12, R40). The shared
//! harness turns the spare off with `KLON_SPARE=0`; every call here turns it
//! on again, and every test waits for the builder it started before the
//! fixture goes away.

mod common;

use common::{
    assert_clean, assert_worktree_parity, git_ok, klon, klon_env, manifest, stderr, stdout,
    Fixture, BIN,
};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SEED: u64 = 42;

/// `gh-klon <args>` with the spare on.
fn klon_on(cwd: &Path, args: &[&str]) -> std::process::Output {
    klon_env(cwd, &[("KLON_SPARE", OsStr::new("1"))], args)
}

fn parse(out: &std::process::Output) -> Value {
    let text = stdout(out);
    serde_json::from_str(text.trim()).unwrap_or_else(|err| panic!("not JSON: {err}\n{text}"))
}

/// `../golden.wt` next to golden.
fn wt_root(golden: &Path) -> PathBuf {
    golden.parent().unwrap().join("golden.wt")
}

fn spare_dir(golden: &Path) -> PathBuf {
    wt_root(golden).join(".spare")
}

fn spare_json(golden: &Path) -> PathBuf {
    spare_dir(golden).join(".klon").join("spare.json")
}

fn read_meta(golden: &Path) -> Value {
    let text = fs::read_to_string(spare_json(golden)).expect("read spare.json");
    serde_json::from_str(&text).expect("spare.json is JSON")
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

/// True when a complete spare sits next to golden within `timeout`.
fn wait_for_spare(golden: &Path, timeout: Duration) -> bool {
    wait_until(|| spare_json(golden).is_file(), timeout)
}

/// Build the spare in the foreground with the hidden command.
fn build_spare(golden: &Path) {
    let out = klon_on(golden, &["spare-build", golden.to_str().unwrap()]);
    assert!(out.status.success(), "spare-build failed: {}", stderr(&out));
    assert!(
        spare_json(golden).is_file(),
        "spare-build must leave a spare"
    );
}

/// True when the trash directory is empty or gone.
fn trash_is_empty(golden: &Path) -> bool {
    match fs::read_dir(wt_root(golden).join(".trash")) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// The one-minute load average on Linux; `None` elsewhere.
fn load_average_1m() -> Option<f64> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// A wall-clock budget means nothing while other builds share the host. The
/// budget check runs only on a quiet host; CI runners are quiet.
fn host_is_quiet() -> bool {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let quiet = (cores / 2).max(1) as f64;
    match load_average_1m() {
        Some(load) if load > quiet => {
            eprintln!("skip the timing budget: the load average {load} is above {quiet}");
            false
        }
        _ => true,
    }
}

/// The checks that every klon made from a spare must pass: clean, on its
/// branch, the same ignored state as golden, and none of the spare metadata.
fn assert_spare_klon(fx: &Fixture, path: &Path, branch: &str) {
    assert_clean(path);
    assert_eq!(
        git_ok(path, &["symbolic-ref", "HEAD"]).trim(),
        format!("refs/heads/{branch}")
    );
    assert_eq!(
        git_ok(path, &["rev-parse", "HEAD^{tree}"]),
        git_ok(&fx.golden, &["rev-parse", &format!("{branch}^{{tree}}")])
    );
    assert_eq!(
        manifest(&path.join("build")),
        manifest(&fx.golden.join("build")),
        "the ignored manifest must equal golden's"
    );
    for stray in ["spare.json", "index", "claim"] {
        assert!(
            !path.join(".klon").join(stray).exists(),
            "the klon must not hold .klon/{stray}"
        );
    }
    assert!(
        path.join(".klon").join("env").is_file(),
        "the envelope is written"
    );
    let list = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let block = list
        .split("\n\n")
        .find(|b| b.starts_with(&format!("worktree {}", path.display())))
        .expect("the klon is registered");
    assert!(
        block
            .lines()
            .any(|l| l == format!("branch refs/heads/{branch}")),
        "block: {block}"
    );
    assert!(
        !block.lines().any(|l| l == "locked"),
        "the klon must be unlocked"
    );
}

// --- The acceptance lines ------------------------------------------------------

/// AC 1: after `add`, a `.spare` directory appears within 60 s on the 10k
/// fixture with a valid `spare.json`.
#[test]
fn a_spare_appears_within_60_s_after_add_on_the_10k_fixture() {
    let fx = Fixture::generate(SEED, 10_000, 100, 1_000, 20);
    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(
        parse(&out)["spare"],
        false,
        "no spare exists before the first add"
    );

    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "a spare must appear within 60 s"
    );
    let meta = read_meta(&fx.golden);
    assert_eq!(meta["version"], 1);
    assert_eq!(
        meta["head"].as_str().expect("head"),
        git_ok(&fx.golden, &["rev-parse", "HEAD"]).trim()
    );
    assert_eq!(
        meta["status_hash"].as_str().expect("status_hash").len(),
        64,
        "a SHA-256 in hex"
    );
    assert_eq!(meta["top_mtimes_before"], meta["top_mtimes_after"]);
    assert!(
        meta["top_mtimes_before"]["build/"].is_string(),
        "the ignored directory is recorded: {}",
        meta["top_mtimes_before"]
    );
    assert!(
        ["copy", "reflink-walk"].contains(&meta["backend"].as_str().expect("backend")),
        "unknown backend {}",
        meta["backend"]
    );
    assert!(meta["created"].as_str().is_some_and(|c| c.ends_with('Z')));

    let spare = spare_dir(&fx.golden);
    assert!(
        spare.join(".klon").join("index").is_file(),
        "the index copy"
    );
    assert!(!spare.join(".git").exists(), "a spare is not a worktree");
    assert!(
        !spare.join("feature").exists(),
        "the klon is not inside the spare"
    );
    assert_eq!(
        manifest(&spare.join("build")),
        manifest(&fx.golden.join("build")),
        "the spare holds golden's ignored state"
    );
    assert!(
        !wt_root(&fx.golden).join(".spare.tmp").exists(),
        "the work directory is renamed away"
    );
}

/// AC 2 on the local fixture: with a valid spare, `add` completes fast. The
/// 100k line of R12 is the bench cell `m1-add-100k` with `spare: true`, and
/// `add_100k_with_a_spare` below prints the same number on request.
#[test]
fn add_with_a_valid_spare_is_fast_and_correct_on_the_10k_fixture() {
    let fx = Fixture::generate(SEED, 10_000, 100, 1_000, 20);
    build_spare(&fx.golden);

    let t0 = Instant::now();
    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    let elapsed = t0.elapsed();
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let report = parse(&out);
    assert_eq!(report["spare"], true, "the spare must serve the add");
    let path = fx.default_klon_path();
    assert_spare_klon(&fx, &path, "feature");
    // Only the paths that `feature` changes got a new mtime: the checkout
    // touched nothing else.
    let tracked = git_ok(&path, &["ls-files"]);
    let newer: std::collections::BTreeSet<String> = tracked
        .lines()
        .filter(|rel| {
            fs::metadata(path.join(rel))
                .and_then(|m| m.modified())
                .is_ok_and(|m| m > t0.into_system_time())
        })
        .map(str::to_string)
        .collect();
    assert_eq!(newer, fx.diff_paths);

    if host_is_quiet() {
        assert!(
            elapsed < Duration::from_secs(3),
            "add with a spare took {elapsed:?}; the local budget is 3 s"
        );
    }
    eprintln!("add with a spare on the 10k fixture: {elapsed:?}");
    // The add started the next builder; let it finish before the fixture goes.
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "the next spare must appear"
    );
}

/// `Instant` has no epoch, so the mtime comparison above needs the wall
/// clock of the same moment.
trait IntoSystemTime {
    fn into_system_time(self) -> std::time::SystemTime;
}

impl IntoSystemTime for Instant {
    fn into_system_time(self) -> std::time::SystemTime {
        std::time::SystemTime::now() - self.elapsed()
    }
}

/// AC 3: a spare whose `top_mtimes_after` differs from `top_mtimes_before` is
/// deleted, and `add` prints `spare torn` and clones directly.
#[test]
fn a_torn_spare_is_deleted_and_add_prints_spare_torn() {
    let fx = Fixture::generate(SEED, 200, 10, 20, 5);
    build_spare(&fx.golden);
    let mut meta = read_meta(&fx.golden);
    meta["top_mtimes_after"]["build/"] = Value::String("0.000000000".to_string());
    fs::write(spare_json(&fx.golden), meta.to_string()).unwrap();

    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("spare torn"),
        "stderr must say spare torn: {}",
        stderr(&out)
    );
    assert_eq!(parse(&out)["spare"], false, "a torn spare is not used");
    let path = fx.default_klon_path();
    assert_clean(&path);
    assert_eq!(
        manifest(&path.join("build")),
        manifest(&fx.golden.join("build"))
    );
    // The torn spare went to the trash and the background delete drained it.
    // The add started a builder, so a fresh spare may already stand there;
    // it must not be the torn one.
    assert!(
        wait_until(|| trash_is_empty(&fx.golden), Duration::from_secs(30)),
        "the trash must drain within 30 s"
    );
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "a fresh spare must appear"
    );
    let fresh = read_meta(&fx.golden);
    assert_eq!(fresh["top_mtimes_before"], fresh["top_mtimes_after"]);
}

/// AC 4: `spare = 0` results in no `.spare` directory after `add`. The two
/// other switches, `--no-spare` and `KLON_SPARE=0`, skip the claim too.
#[test]
fn spare_zero_gives_no_spare_and_the_switches_skip_the_claim() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 3);
    fs::write(fx.golden.join(".klon.toml"), "spare = 0\n").unwrap();
    git_ok(&fx.golden, &["add", ".klon.toml"]);
    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&out)["spare"], false);
    std::thread::sleep(Duration::from_secs(5));
    let root = wt_root(&fx.golden);
    for name in [".spare", ".spare.tmp", ".spare.lock"] {
        assert!(!root.join(name).exists(), "spare = 0 must leave no {name}");
    }

    // With the key gone and a spare in place, the per-call switches leave the
    // spare where it is.
    fs::remove_file(fx.golden.join(".klon.toml")).unwrap();
    git_ok(&fx.golden, &["rm", "-q", "--cached", ".klon.toml"]);
    build_spare(&fx.golden);
    let out = klon_on(&fx.golden, &["add", "--json", "--no-spare", "one"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&out)["spare"], false, "--no-spare skips the claim");
    assert_clean(&fx.klon_path("one"));
    let out = klon(&fx.golden, &["add", "--json", "two"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&out)["spare"], false, "KLON_SPARE=0 skips the claim");
    assert_clean(&fx.klon_path("two"));
    assert!(
        spare_json(&fx.golden).is_file(),
        "the spare stays for a call that may use it"
    );
    let out = klon(&fx.golden, &["rm", "--no-spare", "two"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
    assert!(
        wait_until(|| trash_is_empty(&fx.golden), Duration::from_secs(30)),
        "the trash must drain"
    );
}

/// AC 5: two concurrent `add` calls use the spare at most once; the other
/// clones directly, and both klons are clean.
#[test]
fn two_concurrent_adds_use_the_spare_at_most_once() {
    let fx = Fixture::generate(SEED, 2_000, 50, 200, 20);
    git_ok(&fx.golden, &["branch", "other", "main"]);
    build_spare(&fx.golden);

    let spawn = |branch: &str| -> Child {
        Command::new(BIN)
            .args(["add", "--json", branch])
            .current_dir(&fx.golden)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KLON_SPARE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start gh-klon")
    };
    let first = spawn("feature");
    let second = spawn("other");
    let outputs = [
        first.wait_with_output().expect("wait for the first add"),
        second.wait_with_output().expect("wait for the second add"),
    ];
    let mut used = 0;
    for (out, branch) in outputs.iter().zip(["feature", "other"]) {
        assert!(out.status.success(), "add {branch} failed: {}", stderr(out));
        if parse(out)["spare"] == true {
            used += 1;
        }
        let path = fx.klon_path(branch);
        assert_clean(&path);
        assert_eq!(
            git_ok(&path, &["rev-parse", "HEAD^{tree}"]),
            git_ok(&fx.golden, &["rev-parse", &format!("{branch}^{{tree}}")])
        );
        assert_eq!(
            manifest(&path.join("build")),
            manifest(&fx.golden.join("build"))
        );
    }
    assert_eq!(used, 1, "exactly one add takes the one spare");
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "the next spare must appear"
    );
}

// --- The rest of the contract ----------------------------------------------------

/// A klon from a spare agrees with the plain git oracle and holds none of the
/// builder's files.
#[test]
fn a_klon_from_a_spare_matches_the_oracle_and_holds_no_spare_metadata() {
    let fx = Fixture::generate(11, 300, 10, 30, 10);
    let oracle_fx = Fixture::generate(11, 300, 10, 30, 10);
    build_spare(&fx.golden);
    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&out)["spare"], true);
    let path = fx.default_klon_path();
    assert_spare_klon(&fx, &path, "feature");
    let oracle = oracle_fx.oracle_worktree_add("feature");
    assert_worktree_parity(&path, &oracle);
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "the next spare must appear"
    );
}

/// A stale spare, made before golden moved on, is still used: `git checkout
/// --force` rewrites the tracked paths that differ.
#[test]
fn a_stale_spare_is_still_used() {
    let fx = Fixture::generate(SEED, 300, 10, 30, 10);
    build_spare(&fx.golden);
    let spare_head = read_meta(&fx.golden)["head"].as_str().unwrap().to_string();
    // Golden moves on: one edit, one new file, one deletion, all on main.
    let edited = fx.tracked_rel(5);
    fs::write(fx.golden.join(&edited), "edited on main after the spare\n").unwrap();
    fs::write(fx.golden.join("main-only.txt"), "only on main\n").unwrap();
    let deleted = fx.tracked_rel(6);
    git_ok(&fx.golden, &["rm", "-q", &deleted]);
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "main moves on"]);
    assert_ne!(
        git_ok(&fx.golden, &["rev-parse", "HEAD"]).trim(),
        spare_head
    );

    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&out)["spare"], true, "a stale spare is still used");
    let path = fx.default_klon_path();
    assert_spare_klon(&fx, &path, "feature");
    // The klon holds feature's tree, not main's new one.
    assert_eq!(
        fs::read_to_string(path.join(&edited)).unwrap(),
        fx.tracked_content(5)
    );
    assert_eq!(
        fs::read_to_string(path.join(&deleted)).unwrap(),
        fx.tracked_content(6)
    );
    assert!(!path.join("main-only.txt").exists());
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "the next spare must appear"
    );
}

/// The claim is two renames. A kill between them leaves the target missing
/// and the spare holding the stub; a kill before them leaves the target as
/// `git worktree add` made it. Both states are `registered` in the journal,
/// `doctor --repair` closes them, and the spare serves the next `add`.
#[test]
fn a_killed_claim_is_repaired_and_the_spare_serves_the_next_add() {
    let fx = Fixture::generate(SEED, 200, 10, 20, 5);
    let path = fx.default_klon_path();
    // The stub sits beside the target, never inside the spare: a btrfs
    // snapshot spare is a subvolume of its own, and a directory cannot be
    // renamed into another subvolume.
    let stub = wt_root(&fx.golden).join(".feature.klon-claim");
    for (pause, reached) in [
        (
            "spare-claim",
            Box::new(|| spare_dir(&fx.golden).join(".git").is_file()) as Box<dyn Fn() -> bool>,
        ),
        ("spare-moved", Box::new(|| !path.exists() && stub.is_dir())),
    ] {
        if !spare_json(&fx.golden).is_file() {
            build_spare(&fx.golden);
        }
        let mut child = Command::new(BIN)
            .args(["add", "feature"])
            .current_dir(&fx.golden)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KLON_SPARE", "1")
            .env("KLON_TEST_PAUSE_AT", pause)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start gh-klon");
        let arrived = wait_until(&reached, Duration::from_secs(30));
        // SAFETY: `kill` takes a pid and a signal number and returns an error code.
        let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
        assert_eq!(rc, 0, "SIGKILL failed");
        let _ = child.wait();
        assert!(arrived, "add never reached the {pause} pause");

        // The AC of R6: the repair closes the entry and leaves no half-registered worktree.
        let out = klon(&fx.golden, &["doctor", "--repair"]);
        assert!(out.status.success(), "doctor failed: {}", stderr(&out));
        let list = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
        assert!(
            !list.contains(path.to_str().unwrap()),
            "the worktree must be gone after the repair at {pause}: {list}"
        );
        assert!(!path.exists(), "the target must be gone after {pause}");
        assert!(
            spare_json(&fx.golden).is_file(),
            "the spare survives a killed claim at {pause}"
        );
        assert!(
            fs::read_dir(fx.golden.join(".git/klon/journal"))
                .map(|d| d.count() == 0)
                .unwrap_or(true),
            "the journal must be empty after the repair at {pause}"
        );

        // The spare, stub and all, serves the next add.
        let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
        assert!(
            out.status.success(),
            "add failed after {pause}: {}",
            stderr(&out)
        );
        assert_eq!(
            parse(&out)["spare"],
            true,
            "the spare is used after {pause}"
        );
        assert_spare_klon(&fx, &path, "feature");
        assert!(!stub.exists(), "a completed claim leaves no stub");
        assert!(
            wait_for_spare(&fx.golden, Duration::from_secs(60)),
            "the next spare must appear"
        );
        let out = klon(&fx.golden, &["rm", "--no-spare", "feature"]);
        assert!(out.status.success(), "rm failed: {}", stderr(&out));
        assert!(
            wait_until(|| trash_is_empty(&fx.golden), Duration::from_secs(30)),
            "the trash must drain"
        );
    }
}

/// `add` refuses the paths klon keeps for itself, before any change. Without
/// the refusal, a klon at `.spare.tmp` would be deleted by the next builder.
#[test]
fn add_refuses_the_reserved_spare_and_trash_paths() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 3);
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let root = wt_root(&fx.golden);
    for name in [".spare", ".spare.tmp", ".trash/feature-1"] {
        let target = root.join(name);
        let out = klon_on(
            &fx.golden,
            &["add", "--path", target.to_str().unwrap(), "feature"],
        );
        assert!(!out.status.success(), "add --path {name} must fail");
        assert!(
            stderr(&out).contains("reserved"),
            "stderr must say reserved for {name}: {}",
            stderr(&out)
        );
        assert!(!target.exists(), "{name} must not be created");
    }
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before,
        "a refused add registers nothing"
    );
}

/// A spare made under other `.klonignore` rules may hold a directory that the
/// current rules leave out, and no git command removes an ignored path. Such
/// a spare is deleted, and `add` clones under the current rules.
#[test]
fn a_spare_from_before_a_klonignore_change_is_deleted() {
    let fx = Fixture::generate(SEED, 200, 10, 20, 5);
    build_spare(&fx.golden);
    assert!(spare_dir(&fx.golden).join("build").is_dir());
    fs::write(fx.golden.join(".klonignore"), "/build/\n").unwrap();

    let out = klon_on(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains(".klonignore"),
        "stderr must name the rule change: {}",
        stderr(&out)
    );
    assert_eq!(parse(&out)["spare"], false);
    let path = fx.default_klon_path();
    assert_clean(&path);
    assert!(
        !path.join("build").exists(),
        "the klon follows the current .klonignore"
    );
    assert!(
        wait_until(|| trash_is_empty(&fx.golden), Duration::from_secs(30)),
        "the old spare drains from the trash"
    );
    // The builder that the add started follows the new rules.
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "a fresh spare must appear"
    );
    assert!(!spare_dir(&fx.golden).join("build").exists());
    assert_eq!(
        read_meta(&fx.golden)["exclusions_hash"]
            .as_str()
            .expect("a hash")
            .len(),
        64
    );
}

/// `--backend` names the backend the user wants. A spare made by another one
/// is left for a call without the override, and the report names the backend
/// that made the tree, not the selected one.
#[test]
fn a_backend_override_skips_a_spare_of_another_backend_and_the_report_names_the_spare_backend() {
    let fx = Fixture::generate(SEED, 200, 10, 20, 5);
    build_spare(&fx.golden);
    let mut meta = read_meta(&fx.golden);
    let made_by = meta["backend"].as_str().unwrap().to_string();
    meta["backend"] = Value::String("other-backend".to_string());
    fs::write(spare_json(&fx.golden), meta.to_string()).unwrap();

    let out = klon_on(
        &fx.golden,
        &["add", "--json", "--backend", &made_by, "feature"],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let report = parse(&out);
    assert_eq!(report["spare"], false, "the override skips the spare");
    assert_eq!(report["backend"], made_by, "the override filled the tree");
    assert!(
        stderr(&out).contains("other-backend"),
        "stderr names the spare's backend: {}",
        stderr(&out)
    );
    assert!(
        spare_json(&fx.golden).is_file(),
        "a skipped spare stays for a call without the override"
    );
    assert_clean(&fx.default_klon_path());

    let out = klon_on(&fx.golden, &["add", "--json", "other"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let report = parse(&out);
    assert_eq!(report["spare"], true);
    assert_eq!(
        report["backend"], "other-backend",
        "the report names the backend that made the spare"
    );
    assert_clean(&fx.klon_path("other"));
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(60)),
        "the next spare must appear"
    );
}

/// The M1 number on the 100k fixture, printed for the record. R12 asks for
/// 1 s at p50; `bench --cell m1-add-100k` measures the p50, and this test
/// prints one sample on request.
#[test]
fn add_100k_with_a_spare() {
    if std::env::var("KLON_FIXTURE").as_deref() != Ok("100k") {
        println!("skipped: add_100k_with_a_spare generates 100,000 files; set KLON_FIXTURE=100k to run it");
        return;
    }
    let fx = Fixture::generate(100, 100_000, 1_000, 10_000, 20);
    let build_start = Instant::now();
    build_spare(&fx.golden);
    let build = build_start.elapsed();

    let add_start = Instant::now();
    let out = klon_env(
        &fx.golden,
        &[
            ("KLON_SPARE", OsStr::new("1")),
            ("KLON_DEBUG", OsStr::new("1")),
        ],
        &["add", "--json", "feature"],
    );
    let add = add_start.elapsed();
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    eprint!("{}", stderr(&out));
    assert_eq!(parse(&out)["spare"], true);
    println!("spare build:        {build:?}");
    println!("klon add with spare: {add:?} (limit 1 s at p50)");
    let path = fx.default_klon_path();
    assert_clean(&path);
    if host_is_quiet() {
        assert!(
            add <= Duration::from_secs(1),
            "add with a spare took {add:?}; the limit is 1 s"
        );
    }
    assert!(
        wait_for_spare(&fx.golden, Duration::from_secs(300)),
        "the next spare must appear"
    );
}
