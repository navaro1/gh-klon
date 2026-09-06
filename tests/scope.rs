//! Acceptance tests for the Linux resource scope (spec §7 C20, R18).
//!
//! Every test here reads the kernel, not klon's own report: the memory cap
//! comes from `memory.high` in the command's own cgroup, and the process list
//! comes from `/proc`. The whole file is Linux only; macOS has no cgroup and
//! C21 caps a klon another way.
#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{git_ok, klon, stderr, stdout, Fixture, BIN};

const SEED: u64 = 20;

/// The command that prints the memory cap of the cgroup it runs in. It is the
/// acceptance line of C20, word for word.
const READ_MEMORY_HIGH: &str = "cat /sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.high";

/// The share of `MemTotal` that klon must leave in one cgroup, within 1 %.
const TOLERANCE: f64 = 0.01;

fn fixture() -> Fixture {
    Fixture::generate(SEED, 30, 3, 4, 2)
}

/// `add <branch>` and assert that it worked.
fn add(fx: &Fixture, branch: &str) -> PathBuf {
    let out = klon(&fx.golden, &["add", branch]);
    assert!(
        out.status.success(),
        "add {branch} failed: {}",
        stderr(&out)
    );
    fx.klon_path(branch)
}

/// Create a local branch on top of `main`.
fn branch(fx: &Fixture, name: &str) {
    git_ok(&fx.golden, &["branch", name, "main"]);
}

/// A branch name that no other test and no other build on this host shares.
/// The name becomes `KLON_ID`, and these tests scan the whole process table.
fn unique(tag: &str) -> String {
    format!("{tag}-{}", std::process::id())
}

/// True when `program` sits in a PATH directory.
fn on_path(program: &str) -> bool {
    which(program).is_some()
}

/// The path of `program` in a PATH directory, or None.
fn which(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// `MemTotal` in bytes, read the same way klon reads it.
fn mem_total() -> u64 {
    let text = fs::read_to_string("/proc/meminfo").expect("read /proc/meminfo");
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .expect("a MemTotal line");
    let kib: u64 = line
        .split_whitespace()
        .next()
        .expect("a number")
        .parse()
        .expect("a number");
    kib * 1024
}

/// One feature row of `doctor --json`: its status and its detail.
fn feature_row(fx: &Fixture, name: &str) -> (String, String) {
    let out = klon(&fx.golden, &["--json", "doctor"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let row = &document["features"][name];
    (
        row["status"]
            .as_str()
            .unwrap_or_else(|| panic!("no {name} row"))
            .to_string(),
        row["detail"].as_str().expect("a detail").to_string(),
    )
}

/// The memory cap that a command under `run <branch>` really carries, or None
/// when `run` itself said that this host gave it no memory cap. `run` is the
/// only honest source: a host can hold a controller and still refuse the
/// `mkdir`, and then the command runs under `nice` with no cap at all.
fn memory_high_under_run(fx: &Fixture, branch: &str) -> Option<u64> {
    let out = klon(
        &fx.golden,
        &["run", branch, "--", "sh", "-c", READ_MEMORY_HIGH],
    );
    let note = stderr(&out);
    if note.contains("nice -n") || note.contains("no resource cap") {
        println!(
            "skipped: this host gives klon no memory cap: {}",
            note.trim()
        );
        return None;
    }
    assert!(out.status.success(), "run {branch} failed: {note}");
    let text = stdout(&out);
    Some(
        text.trim()
            .parse()
            .unwrap_or_else(|_| panic!("memory.high is not a number: {text:?}")),
    )
}

/// Assert that `got` is `MemTotal / share` within 1 %. The kernel rounds
/// `memory.high` down to a page, so an exact match is never the answer.
fn assert_share(got: u64, share: u64) {
    let want = mem_total() / share;
    let drift = (got as f64 - want as f64).abs() / want as f64;
    assert!(
        drift <= TOLERANCE,
        "memory.high is {got}, and MemTotal/{share} is {want}: {:.3} % apart",
        drift * 100.0
    );
}

/// Poll `cond` until it holds or the timeout passes.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// Every process whose environment holds the whole entry `KLON_ID=<name>`.
fn processes_with_klon_id(name: &str) -> Vec<u32> {
    let needle = format!("KLON_ID={name}").into_bytes();
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if fs::read(entry.path().join("environ"))
            .is_ok_and(|bytes| bytes.split(|b| *b == 0).any(|item| item == needle))
        {
            pids.push(pid);
        }
    }
    pids
}

/// The session id of `pid`, the sixth field of `/proc/<pid>/stat`. The second
/// field is the command name in brackets and may hold a space or a bracket, so
/// the read starts after the last `") "` of the line: the fields there are
/// state, ppid, pgrp, and session.
fn session_of(pid: u32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = text.rsplit_once(") ")?.1;
    tail.split_whitespace().nth(3)?.parse().ok()
}

/// Kill a spawned `run` when the test ends, whatever the outcome.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start `klon run <branch> -- <command>` and leave it running.
fn spawn_run(fx: &Fixture, branch: &str, command: &[&str]) -> Reaper {
    let mut args = vec!["run", branch, "--"];
    args.extend_from_slice(command);
    let child = Command::new(BIN)
        .args(&args)
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    Reaper(child)
}

// --- The memory cap ----------------------------------------------------------

/// AC: under `run`, `memory.high` of the command's own cgroup is
/// `MemTotal / (N + 1)` within 1 %. One klon exists, so the share is 2.
#[test]
fn run_caps_memory_at_the_share_of_one_klon() {
    if !on_path("cut") {
        println!("skipped: cut is not on PATH");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    let Some(got) = memory_high_under_run(&fx, "feature") else {
        return;
    };
    assert_share(got, 2);
}

/// AC: with two live klons, a new `run` gets `MemTotal / 3`. A command runs in
/// the sibling at the same time, so the two scopes really live together.
#[test]
fn two_live_klons_leave_a_third_of_the_memory_each() {
    if !on_path("cut") {
        println!("skipped: cut is not on PATH");
        return;
    }
    let fx = fixture();
    let name = unique("busy");
    branch(&fx, &name);
    add(&fx, "feature");
    add(&fx, &name);

    // Hold one klon busy, so the second scope is measured beside a live one.
    let reaper = spawn_run(&fx, &name, &["sleep", "60"]);
    assert!(
        wait_until(
            || !processes_with_klon_id(&name).is_empty(),
            Duration::from_secs(10)
        ),
        "the sibling command never started"
    );

    if let Some(got) = memory_high_under_run(&fx, "feature") {
        assert_share(got, 3);
    }

    let out = klon(&fx.golden, &["stop", &name]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    drop(reaper);
}

/// AC: `doctor` reports the delegated controllers. The two rows beside it name
/// the systemd version and the mechanism the next `run` would take.
#[test]
fn doctor_reports_the_scope_the_controllers_and_the_systemd_version() {
    let fx = fixture();
    add(&fx, "feature");

    let (status, detail) = feature_row(&fx, "cgroup.controllers");
    if status == "present" {
        assert!(
            detail.split_whitespace().any(|word| word == "memory"),
            "a delegated memory controller must show: {detail}"
        );
    } else {
        println!("note: this host delegates no controller: {detail}");
    }

    // The scope row names the mechanism, and `run` must take the same one.
    let (scope_status, scope_detail) = feature_row(&fx, "scope");
    let capped = memory_high_under_run(&fx, "feature").is_some();
    assert_eq!(
        scope_status == "present",
        capped,
        "doctor says {scope_status} ({scope_detail}) and run says capped={capped}"
    );

    let (systemd_status, systemd_detail) = feature_row(&fx, "systemd-run");
    if systemd_status == "present" {
        assert!(
            systemd_detail.starts_with("systemd "),
            "the row must hold a version line: {systemd_detail}"
        );
    }
}

// --- stop --------------------------------------------------------------------

/// AC: `stop` on a scope ends every process, including one that called
/// `setsid` and so left the session that `run` gave the tree.
#[test]
fn stop_ends_a_process_that_left_the_session() {
    if !on_path("setsid") {
        println!("skipped: setsid is not on PATH");
        return;
    }
    let fx = fixture();
    let name = unique("setsid");
    branch(&fx, &name);
    add(&fx, &name);

    let reaper = spawn_run(&fx, &name, &["sh", "-c", "setsid sleep 1000 & sleep 1000"]);
    assert!(
        wait_until(
            || processes_with_klon_id(&name).len() >= 2,
            Duration::from_secs(10)
        ),
        "the run tree never started: {:?}",
        processes_with_klon_id(&name)
    );

    // The `setsid` child really leads a session of its own, so no session scan
    // can reach it from the rest of the tree.
    let pids = processes_with_klon_id(&name);
    let sessions: Vec<u32> = pids.iter().filter_map(|pid| session_of(*pid)).collect();
    let first = sessions.first().copied().expect("a session");
    assert!(
        sessions.iter().any(|session| *session != first),
        "no process left the session: {sessions:?}"
    );

    let out = klon(&fx.golden, &["stop", &name]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(
        wait_until(
            || processes_with_klon_id(&name).is_empty(),
            Duration::from_secs(5)
        ),
        "stop left processes behind: {:?}",
        processes_with_klon_id(&name)
    );
    drop(reaper);
}

// --- The fallback ------------------------------------------------------------

/// AC: on a host without `systemd-run`, `run` prints one line about the
/// fallback and the command still runs. The PATH holds only the tools klon
/// and the command need, so `systemd-run` and `nice` are both out of reach.
#[test]
fn run_without_systemd_run_prints_one_line_and_still_runs() {
    let fx = fixture();
    add(&fx, "feature");

    let bin = fx.golden.parent().unwrap().join("thin-bin");
    fs::create_dir_all(&bin).expect("create the thin bin directory");
    for tool in ["sh", "cat", "sleep", "git"] {
        let Some(source) = which(tool) else {
            println!("skipped: {tool} is not on PATH");
            return;
        };
        unix_fs::symlink(&source, bin.join(tool)).expect("link the tool");
    }
    unix_fs::symlink(Path::new(BIN), bin.join("gh-klon")).expect("link klon");
    assert!(
        which("systemd-run").is_some(),
        "the test must hide a systemd-run that the host really has"
    );

    let out = Command::new(bin.join("gh-klon"))
        .args(["run", "feature", "--", "sh", "-c", "echo alive"])
        .current_dir(&fx.golden)
        .env("PATH", &bin)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run klon with a thin PATH");

    assert!(
        out.status.success(),
        "the command must still run: {}",
        stderr(&out)
    );
    assert_eq!(stdout(&out).trim(), "alive");
    let text = stderr(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one line: {lines:?}");
    let line = lines[0];
    assert!(line.starts_with("klon: "), "{line}");
    assert!(
        line.contains("cgroup") || line.contains("nice") || line.contains("no resource cap"),
        "the line must name the fallback: {line}"
    );
}
