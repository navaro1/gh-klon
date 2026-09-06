//! Acceptance tests for the `list` extras (spec §7 C30, R38): the disk, RSS,
//! live-process, PR, and checks columns, and the 60 s cache behind the PR
//! facts. The shared harness lives in `tests/common`.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{klon, klon_env, stderr, stdout, Fixture};
// The process-and-RSS test spawns a `run`; the other targets gate it out.
#[cfg(target_os = "linux")]
use common::BIN;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

const SEED: u64 = 42;

/// A small fixture. The disk walk reads the ignored `build/` directory, so the
/// fixture keeps it small.
fn fixture() -> Fixture {
    Fixture::generate(SEED, 30, 3, 5, 2)
}

/// The byte size of the fixture's ignored `build/` directory.
fn ignored_bytes(klon_path: &Path) -> u64 {
    fs::read_dir(klon_path.join("build"))
        .expect("the ignored directory")
        .flatten()
        .map(|entry| entry.metadata().expect("stat").len())
        .sum()
}

/// Add `feature` and answer the klon path.
fn add(fx: &Fixture) -> PathBuf {
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    fx.default_klon_path()
}

/// The `pr list` body: one pull request with number `n` and the raw rollup.
fn pr_body(n: u64, rollup: &str) -> String {
    format!(r#"[{{"number": {n}, "statusCheckRollup": {rollup}}}]"#)
}

/// The `gh` script behind every PR fact in these tests: one call per line in
/// `$KLON_FAKE_GH_LOG`, one answer from `$KLON_FAKE_GH_BODY`.
const COUNTING_GH: &str = "\
#!/bin/sh
printf '%s\\n' \"$*\" >> \"$KLON_FAKE_GH_LOG\"
printf '%s\\n' \"$KLON_FAKE_GH_BODY\"
";

/// A `gh` that records the call and then fails the way an offline one does.
const FAILING_GH: &str = "\
#!/bin/sh
printf '%s\\n' \"$*\" >> \"$KLON_FAKE_GH_LOG\"
echo 'gh: offline' >&2
exit 1
";

/// Write a `gh` script into `dir` and answer a PATH value that finds it first.
fn fake_gh(dir: &Path, script: &str) -> std::ffi::OsString {
    fs::create_dir_all(dir).expect("create the fake gh directory");
    let file = dir.join("gh");
    fs::write(&file, script).expect("write the fake gh");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod the fake gh");
    let mut path = dir.as_os_str().to_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// Run `gh-klon list` with the fake gh first on PATH and its variables set.
fn list_with_gh(fx: &Fixture, path: &OsStr, log: &Path, body: &str, args: &[&str]) -> Output {
    klon_env(
        &fx.golden,
        &[
            ("PATH", path),
            ("KLON_FAKE_GH_LOG", log.as_os_str()),
            ("KLON_FAKE_GH_BODY", OsStr::new(body)),
        ],
        args,
    )
}

/// How often the fake gh ran.
fn gh_calls(log: &Path) -> usize {
    fs::read_to_string(log)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// The `klons[0]` row of a successful `list --json`.
fn first_row(out: &Output) -> serde_json::Value {
    assert!(out.status.success(), "list failed: {}", stderr(out));
    let value: serde_json::Value = serde_json::from_str(&stdout(out)).expect("valid JSON");
    value["klons"][0].clone()
}

/// AC: `list` for a klon with a running `run` shows a process count of at
/// least 1 and an RSS above 0. Linux only: the scan reads `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn a_running_run_shows_processes_and_rss() {
    let fx = fixture();
    add(&fx);
    let child = Command::new(BIN)
        .args(["run", "feature", "--", "sleep", "1000"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    struct Reaper(std::process::Child);
    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _reaper = Reaper(child);

    // The command takes a moment to start; poll until list sees it. `--no-gh`
    // keeps every poll off the network.
    let mut seen = None;
    for _ in 0..40 {
        let row = first_row(&klon(&fx.golden, &["list", "--json", "--no-gh"]));
        if row["procs"].as_u64().unwrap_or(0) >= 1 && row["rss_bytes"].as_u64().unwrap_or(0) > 0 {
            seen = Some(row);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let row = seen.expect("the running command never reached the list");
    assert!(row["procs"].as_u64().expect("procs") >= 1);
    assert!(row["rss_bytes"].as_u64().expect("rss_bytes") > 0);
}

/// AC: two `list` calls within 60 s make one `gh` call.
#[test]
fn two_list_calls_within_60s_make_one_gh_call() {
    let fx = fixture();
    add(&fx);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = fake_gh(&scratch.path().join("bin"), COUNTING_GH);
    let log = scratch.path().join("gh.log");
    let body = pr_body(7, r#"[{"status": "COMPLETED", "conclusion": "SUCCESS"}]"#);

    let out = list_with_gh(&fx, &path, &log, &body, &["list", "--json"]);
    let row = first_row(&out);
    assert_eq!(row["pr"], 7, "the fake gh named pull request 7");
    assert_eq!(row["checks"], "pass");
    assert_eq!(gh_calls(&log), 1);
    assert!(
        fx.golden
            .join(".git")
            .join("klon")
            .join("gh-cache.json")
            .is_file(),
        "the cache must sit in the common directory"
    );

    // The second call reads the cache and leaves the log alone.
    let out = list_with_gh(&fx, &path, &log, &body, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(gh_calls(&log), 1, "the second list must read the cache");
    assert!(
        stdout(&out).contains("| #7 | pass |"),
        "the human line shows both columns: {}",
        stdout(&out)
    );
}

/// AC: `list --json` holds every `klon.list/1` field plus the C30 extras. The
/// disk reading is the ignored-directory size, and `--no-gh` leaves the PR
/// fields null.
#[test]
fn list_json_holds_the_list1_fields_and_the_new_ones() {
    let fx = fixture();
    let klon_path = add(&fx);
    let out = klon(&fx.golden, &["list", "--json", "--no-gh"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(value["schema"], "klon.list/2");
    let row = &value["klons"][0];
    for field in [
        "path",
        "branch",
        "head",
        "dirty",
        "locked",
        "ip",
        "vs_base",
        "vs_siblings",
        "behind",
    ] {
        assert!(
            row.get(field).is_some(),
            "the klon.list/1 field {field} is missing"
        );
    }
    let size = ignored_bytes(&klon_path);
    assert_eq!(row["disk_bytes"].as_u64(), Some(size));
    assert_eq!(row["disk_exact"], false);
    assert_eq!(row["procs"], 0);
    assert_eq!(row["rss_bytes"], 0);
    assert!(row["pr"].is_null());
    assert!(row["checks"].is_null());

    // The human line marks the bound with the `≤` prefix.
    let out = klon(&fx.golden, &["list", "--no-gh"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("| ≤ {size} B |")),
        "{}",
        stdout(&out)
    );
}

/// `--no-gh` skips the call, and a list with no klon never calls gh either.
#[test]
fn no_gh_and_no_klons_skip_the_gh_call() {
    let fx = fixture();
    add(&fx);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = fake_gh(&scratch.path().join("bin"), COUNTING_GH);
    let log = scratch.path().join("gh.log");
    let body = pr_body(7, "[]");

    let out = list_with_gh(&fx, &path, &log, &body, &["list", "--json", "--no-gh"]);
    let row = first_row(&out);
    assert!(row["pr"].is_null());
    assert!(row["checks"].is_null());
    assert_eq!(gh_calls(&log), 0, "--no-gh must not call gh");

    // No klon: the extras never start, so gh is never asked.
    let empty = fixture();
    let out = list_with_gh(&empty, &path, &log, &body, &["list", "--json"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(gh_calls(&log), 0, "a list with no klon must not call gh");
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(
        value["klons"].as_array().expect("an array").len(),
        0,
        "the main worktree is not a klon"
    );
}

/// A failing gh costs one stderr line and never a failure, and its failure is
/// not cached: the next list asks again.
#[test]
fn a_failing_gh_costs_one_line_and_no_failure() {
    let fx = fixture();
    add(&fx);
    let scratch = tempfile::tempdir().expect("tempdir");
    let path = fake_gh(&scratch.path().join("bin"), FAILING_GH);
    let log = scratch.path().join("gh.log");
    let body = pr_body(7, "[]");

    let out = list_with_gh(&fx, &path, &log, &body, &["list", "--json"]);
    let row = first_row(&out);
    assert!(row["pr"].is_null());
    assert!(row["checks"].is_null());
    let noise = stderr(&out);
    let lines: Vec<&str> = noise
        .lines()
        .filter(|line| line.contains("gh pr list failed"))
        .collect();
    assert_eq!(lines.len(), 1, "one degradation line expected: {lines:?}");

    let out = list_with_gh(&fx, &path, &log, &body, &["list", "--json"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(gh_calls(&log), 2, "the failure must not be cached");
}

/// The checks column reads the rollup: `pass`, `fail`, `pending`, `none`.
#[test]
fn the_checks_column_reads_the_rollup() {
    for (rollup, expected) in [
        ("[]", "none"),
        (
            r#"[{"status": "COMPLETED", "conclusion": "SUCCESS"}]"#,
            "pass",
        ),
        (
            r#"[{"status": "IN_PROGRESS"}, {"status": "COMPLETED", "conclusion": "SUCCESS"}]"#,
            "pending",
        ),
        (r#"[{"state": "FAILURE"}]"#, "fail"),
    ] {
        let fx = fixture();
        add(&fx);
        let scratch = tempfile::tempdir().expect("tempdir");
        let path = fake_gh(&scratch.path().join("bin"), COUNTING_GH);
        let log = scratch.path().join("gh.log");
        let out = list_with_gh(&fx, &path, &log, &pr_body(3, rollup), &["list", "--json"]);
        let row = first_row(&out);
        assert_eq!(row["checks"], expected, "rollup {rollup}");
        assert_eq!(row["pr"], 3);
    }
}
