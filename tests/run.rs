//! Acceptance tests for the envelope (spec §7 C16, R21, R22): the `.klon/env`
//! file, the loopback slots, and the `run`, `shell`, and `stop` commands.
//! The shared harness lives in `tests/common`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
// Only the `stop` tests wait for a process tree, and those need `/proc`.
#[cfg(target_os = "linux")]
use std::process::Child;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use common::{git_ok, klon, stderr, stdout, Fixture, BIN};

const SEED: u64 = 42;

/// A small fixture. Every test here reads the envelope, not the tree, so the
/// file counts stay low and each test finishes in a second or two.
fn fixture() -> Fixture {
    Fixture::generate(SEED, 40, 4, 5, 2)
}

/// `add <branch>` and assert that it worked. The answer is the klon path.
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

/// `run <branch> -- <cmd>` and assert that it worked. The answer is stdout.
fn run_ok(fx: &Fixture, branch: &str, command: &[&str]) -> String {
    let mut args = vec!["run", branch, "--"];
    args.extend_from_slice(command);
    let out = klon(&fx.golden, &args);
    assert!(
        out.status.success(),
        "run {branch} failed: {}",
        stderr(&out)
    );
    stdout(&out).trim().to_string()
}

/// The variables of `<klon>/.klon/env`.
fn env_of(klon_path: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(klon_path.join(".klon").join("env")).expect("read .klon/env");
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.trim_matches('\'').to_string()))
        .collect()
}

/// Poll `cond` until it holds or the timeout passes.
#[cfg(target_os = "linux")]
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

/// Every process whose environment holds `KLON_ID=<name>`. The test scans
/// `/proc` itself, so it never asks klon whether klon did its own work.
#[cfg(target_os = "linux")]
fn processes_with_klon_id(name: &str) -> Vec<u32> {
    processes_tagged(&format!("KLON_ID={name}"))
}

/// Every process whose environment holds `KLON_DIR=<klon>`: the tree of one
/// klon, whatever its branch name.
#[cfg(target_os = "linux")]
fn processes_in_klon(klon_path: &Path) -> Vec<u32> {
    processes_tagged(&format!("KLON_DIR={}", klon_path.display()))
}

/// Every process whose environment holds the whole entry `tag`.
#[cfg(target_os = "linux")]
fn processes_tagged(tag: &str) -> Vec<u32> {
    let needle = tag.as_bytes().to_vec();
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(bytes) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        if bytes
            .split(|b| *b == 0)
            .any(|item| item == needle.as_slice())
        {
            pids.push(pid);
        }
    }
    pids
}

/// Kill a spawned `run` when the test ends, whatever the outcome.
#[cfg(target_os = "linux")]
struct Reaper(Child);

#[cfg(target_os = "linux")]
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// True when `program` sits in a PATH directory. Only the two Linux tests that
/// shell out to another tool ask.
#[cfg(target_os = "linux")]
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// A branch name that no other test and no other build on this host shares.
/// The branch name becomes `KLON_ID`, and the `stop` tests scan the whole
/// process table, so a shared name would let two parallel runs see each other.
#[cfg(target_os = "linux")]
fn unique(tag: &str) -> String {
    format!("{tag}-{}", std::process::id())
}

// --- The env file ------------------------------------------------------------

#[test]
fn add_writes_the_env_file_and_the_klon_stays_clean() {
    let fx = fixture();
    let klon_path = add(&fx, "feature");

    // R21: the file holds the whole contract from handoff §5.
    let vars = env_of(&klon_path);
    assert_eq!(vars.get("KLON_NAME").map(String::as_str), Some("feature"));
    let ip = vars.get("KLON_IP").expect("KLON_IP");
    assert!(ip.starts_with("127.0.0."), "unexpected address {ip}");
    assert_eq!(vars.get("HOST"), Some(ip));
    assert_eq!(
        vars.get("TMPDIR").map(PathBuf::from),
        Some(klon_path.join(".klon").join("tmp"))
    );
    // C17 fills the jobserver path. Until then the variable is present and empty.
    assert_eq!(vars.get("KLON_JOBSERVER").map(String::as_str), Some(""));
    assert_eq!(vars.get("GIT_CONFIG_COUNT").map(String::as_str), Some("1"));
    assert_eq!(
        vars.get("GIT_CONFIG_KEY_0").map(String::as_str),
        Some("core.hooksPath")
    );
    assert_eq!(
        vars.get("GIT_CONFIG_VALUE_0").map(PathBuf::from),
        Some(klon_path.join(".klon").join("hooks"))
    );

    // The two directories the contract names exist.
    assert!(klon_path.join(".klon").join("tmp").is_dir());
    assert!(klon_path.join(".klon").join("hooks").is_dir());

    // AC: the klon is clean although `.klon/env` exists.
    assert!(klon_path.join(".klon").join("env").is_file());
    let status = git_ok(&klon_path, &["status", "--porcelain"]);
    assert_eq!(status, "", "the klon must be clean with .klon/env present");
    // Even the ignored files must stay out of sight, so `--ignored` is quiet
    // about nothing else than `.klon` and the fixture's `build`.
    let ignored = git_ok(&klon_path, &["status", "--porcelain", "--ignored"]);
    assert!(
        ignored.lines().any(|line| line.ends_with(".klon/")),
        "git must treat .klon/ as ignored: {ignored}"
    );
}

// --- The loopback address ----------------------------------------------------

#[test]
fn every_live_klon_holds_its_own_loopback_address() {
    let fx = fixture();
    branch(&fx, "other");
    let first = add(&fx, "feature");
    let second = add(&fx, "other");

    // AC: `run x -- sh -c 'echo $KLON_IP'` prints an address no sibling holds.
    let a = run_ok(&fx, "feature", &["sh", "-c", "echo $KLON_IP"]);
    let b = run_ok(&fx, "other", &["sh", "-c", "echo $KLON_IP"]);
    assert_eq!(a, "127.0.0.2", "the first klon takes the lowest address");
    assert_eq!(b, "127.0.0.3");
    assert_ne!(a, b);
    assert_eq!(
        env_of(&first).get("KLON_IP").map(String::as_str),
        Some("127.0.0.2")
    );
    assert_eq!(
        env_of(&second).get("KLON_IP").map(String::as_str),
        Some("127.0.0.3")
    );

    // `list --json` reports the same address.
    let out = klon(&fx.golden, &["--json", "list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let addresses: Vec<&str> = document["klons"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["ip"].as_str())
        .collect();
    assert_eq!(addresses, vec!["127.0.0.2", "127.0.0.3"]);
}

/// The AC names Linux: `lo` owns all of `127/8` there. macOS gives `lo0` only
/// `127.0.0.1`, and the alias needs a privilege klon never takes (handoff §5).
#[cfg(target_os = "linux")]
#[test]
fn a_command_under_run_binds_the_loopback_address() {
    if !on_path("python3") {
        println!("skipped: python3 is not on PATH");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    // AC: the bind to `$KLON_IP` succeeds on Linux.
    let printed = run_ok(
        &fx,
        "feature",
        &[
            "python3",
            "-c",
            "import os,socket; s=socket.socket(); s.bind((os.environ['KLON_IP'],3000)); print('ok')",
        ],
    );
    assert_eq!(printed, "ok");
}

#[test]
fn rm_releases_the_address_and_the_next_add_reuses_it() {
    let fx = fixture();
    branch(&fx, "other");
    branch(&fx, "third");
    add(&fx, "feature");
    add(&fx, "other");
    assert_eq!(
        run_ok(&fx, "feature", &["sh", "-c", "echo $KLON_IP"]),
        "127.0.0.2"
    );

    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));

    // AC: the freed address goes to the next klon; the live sibling keeps its own.
    let third = add(&fx, "third");
    assert_eq!(
        env_of(&third).get("KLON_IP").map(String::as_str),
        Some("127.0.0.2")
    );
    assert_eq!(
        run_ok(&fx, "other", &["sh", "-c", "echo $KLON_IP"]),
        "127.0.0.3"
    );
}

// --- The command environment -------------------------------------------------

#[test]
fn run_sets_the_directory_the_tag_and_the_git_config() {
    let fx = fixture();
    let klon_path = add(&fx, "feature");

    assert_eq!(
        run_ok(&fx, "feature", &["sh", "-c", "pwd"]),
        klon_path.to_string_lossy()
    );
    assert_eq!(
        run_ok(&fx, "feature", &["sh", "-c", "echo $KLON_ID"]),
        "feature"
    );
    // `KLON_DIR` names the klon itself, so `stop` never reaches a klon of
    // another repository that holds the same branch name.
    assert_eq!(
        run_ok(&fx, "feature", &["sh", "-c", "echo $KLON_DIR"]),
        klon_path.to_string_lossy()
    );
    assert_eq!(
        run_ok(&fx, "feature", &["sh", "-c", "echo $TMPDIR"]),
        klon_path.join(".klon").join("tmp").to_string_lossy()
    );
    // `run` appends `gc.auto=0` to the set that `add` wrote, so both keys reach git.
    assert_eq!(
        run_ok(&fx, "feature", &["sh", "-c", "echo $GIT_CONFIG_COUNT"]),
        "2"
    );
    assert_eq!(
        run_ok(&fx, "feature", &["git", "config", "--get", "gc.auto"]),
        "0"
    );
    assert_eq!(
        run_ok(
            &fx,
            "feature",
            &["git", "config", "--get", "core.hooksPath"]
        ),
        klon_path.join(".klon").join("hooks").to_string_lossy()
    );
}

/// Linux only: `ps -o sid=` prints the session id. The BSD `ps` of macOS has
/// no session column that compares with a process id, and `stop` reads the
/// session through `/proc` anyway.
#[cfg(target_os = "linux")]
#[test]
fn run_starts_a_new_session() {
    if !on_path("ps") {
        println!("skipped: ps is not on PATH");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    // The command's session id equals its own pid, so `stop` and the C20 scope
    // can hold the whole tree.
    let printed = run_ok(&fx, "feature", &["sh", "-c", "ps -o sid= -p $$; echo $$"]);
    let numbers: Vec<&str> = printed.split_whitespace().collect();
    assert_eq!(numbers.len(), 2, "unexpected output {printed}");
    assert_eq!(
        numbers[0], numbers[1],
        "the shell must lead its own session"
    );
}

#[test]
fn run_passes_the_exit_code_back_and_prints_nothing_of_its_own() {
    let fx = fixture();
    add(&fx, "feature");
    let out = klon(&fx.golden, &["run", "feature", "--", "sh", "-c", "exit 7"]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(stderr(&out), "", "klon must add no message of its own");
    let out = klon(&fx.golden, &["run", "feature", "--", "false"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn run_refuses_an_unknown_branch_and_the_main_worktree() {
    let fx = fixture();
    add(&fx, "feature");
    let out = klon(&fx.golden, &["run", "nope", "--", "true"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no klon has the branch nope"),
        "{}",
        stderr(&out)
    );
    // golden holds `main`, and golden is not a klon.
    let out = klon(&fx.golden, &["run", "main", "--", "true"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no klon has the branch main"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn add_with_a_command_runs_it_and_passes_the_exit_code() {
    let fx = fixture();
    branch(&fx, "other");
    // AC: `add x -- true` exits 0 after the klon exists.
    let out = klon(&fx.golden, &["add", "feature", "--", "true"]);
    assert!(out.status.success(), "add -- true failed: {}", stderr(&out));
    let klon_path = fx.klon_path("feature");
    assert!(klon_path.join(".klon").join("env").is_file());
    assert!(git_ok(&fx.golden, &["worktree", "list", "--porcelain"])
        .contains(&klon_path.to_string_lossy().to_string()));

    // The command runs inside the new klon with the envelope.
    let out = klon(
        &fx.golden,
        &[
            "add",
            "other",
            "--",
            "sh",
            "-c",
            "echo $KLON_IP > seen; exit 3",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "the exit code must pass through"
    );
    let seen = fs::read_to_string(fx.klon_path("other").join("seen")).expect("read seen");
    assert_eq!(seen.trim(), "127.0.0.3");
}

#[test]
fn shell_runs_the_shell_inside_the_klon() {
    let fx = fixture();
    let klon_path = add(&fx, "feature");
    let mut child = Command::new(BIN)
        .args(["shell", "feature"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shell");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"pwd\necho $KLON_ID\n")
        .expect("write to the shell");
    let out = child.wait_with_output().expect("wait for the shell");
    assert!(out.status.success(), "shell failed: {}", stderr(&out));
    let lines: Vec<String> = stdout(&out).lines().map(str::to_string).collect();
    assert_eq!(
        lines,
        vec![
            klon_path.to_string_lossy().to_string(),
            "feature".to_string()
        ]
    );
}

#[test]
fn add_with_a_command_refuses_json() {
    let fx = fixture();
    // The command owns stdout, so a report and the output would share one
    // stream. The refusal comes before any repository change.
    let out = klon(&fx.golden, &["--json", "add", "feature", "--", "true"]);
    assert!(!out.status.success(), "add --json -- cmd must fail");
    assert!(
        stderr(&out).contains("--json is not available"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        !fx.klon_path("feature").exists(),
        "the refusal must change nothing"
    );
}

/// A crash between the checkout and the envelope must not leave a klon that
/// the repair calls complete. `add` reaches `checked-out` only after the env
/// file exists, so the repair rolls this one back instead of unlocking it.
#[test]
fn a_klon_killed_before_the_envelope_is_rolled_back() {
    let fx = fixture();
    let klon_path = fx.klon_path("feature");
    let mut child = Command::new(BIN)
        .args(["add", "feature"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_TEST_PAUSE_AT", "cloned")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn add");
    let journal = fx.golden.join(".git").join("klon").join("journal");
    // Wait for the state itself, not for any file: the kill must land after
    // the clone and before the envelope.
    let paused = poll(|| journal_holds(&journal, "\"state\": \"cloned\""), 300);
    let _ = child.kill();
    let _ = child.wait();
    assert!(paused, "add never reached the cloned state");

    let out = klon(&fx.golden, &["doctor", "--repair"]);
    assert!(out.status.success(), "repair failed: {}", stderr(&out));
    assert!(
        !klon_path.join(".klon").join("env").exists(),
        "a klon with no envelope must not survive the repair"
    );
    // A second `add` now works, which proves the rollback was complete.
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(
        out.status.success(),
        "add after the repair failed: {}",
        stderr(&out)
    );
    assert!(klon_path.join(".klon").join("env").is_file());
}

/// Poll `cond` every 100 ms until it holds or `tries` polls pass. The `stop`
/// tests need `Duration`, which is Linux only here, so this counter serves the
/// tests that run everywhere.
fn poll(mut cond: impl FnMut() -> bool, tries: u32) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cond()
}

/// True when a journal entry under `dir` holds `needle`.
fn journal_holds(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|item| fs::read_to_string(item.path()).is_ok_and(|text| text.contains(needle)))
}

// --- stop --------------------------------------------------------------------

#[test]
fn stop_reports_when_no_process_runs() {
    let fx = fixture();
    add(&fx, "feature");
    let out = klon(&fx.golden, &["stop", "feature"]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(stdout(&out).contains("no live process"), "{}", stdout(&out));

    let out = klon(&fx.golden, &["--json", "stop", "feature"]);
    assert!(out.status.success(), "stop --json failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(document["schema"], "klon.stop/1");
    assert_eq!(document["found"], 0);
    assert_eq!(document["killed"], 0);
}

/// A command that clears its own environment loses the tags. It keeps the
/// session that `run` gave it, and a tagged sibling names that session, so
/// `stop` still ends it.
#[cfg(target_os = "linux")]
#[test]
fn stop_ends_a_descendant_that_cleared_its_environment() {
    let fx = fixture();
    let name = unique("bare");
    branch(&fx, &name);
    add(&fx, &name);
    // The shell keeps the tags. The background `env -i` child drops every one
    // of them, so only the session still joins it to the klon.
    let child = Command::new(BIN)
        .args([
            "run",
            &name,
            "--",
            "sh",
            "-c",
            "env -i sleep 1000 & sleep 1000",
        ])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    let reaper = Reaper(child);
    assert!(
        wait_until(
            || processes_with_klon_id(&name).len() >= 2,
            Duration::from_secs(10)
        ),
        "the run tree never started"
    );
    // The bare child carries no tag at all.
    let bare = processes_in_session_of(&processes_with_klon_id(&name));
    assert!(
        bare.len() > processes_with_klon_id(&name).len(),
        "the env -i child must be in the session and carry no tag"
    );

    let out = klon(&fx.golden, &["stop", &name]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(
        processes_in_session_of(&bare).is_empty(),
        "stop must end the untagged descendant too"
    );
    drop(reaper);
}

/// Every process that shares a session with one of `pids`, `pids` included.
#[cfg(target_os = "linux")]
fn processes_in_session_of(pids: &[u32]) -> Vec<u32> {
    let wanted: Vec<i32> = pids.iter().filter_map(|pid| session_of(*pid)).collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if session_of(pid).is_some_and(|s| wanted.contains(&s)) {
            found.push(pid);
        }
    }
    found
}

/// The session id of `pid` from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn session_of(pid: u32) -> Option<i32> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = text.get(text.rfind(')')? + 1..)?;
    tail.split_whitespace().nth(3)?.parse().ok()
}

/// A caller that ends `gh klon run` must not leave the command behind. The
/// child leads its own session, so the terminal never signals it; `run` relays
/// the signal itself.
#[cfg(target_os = "linux")]
#[test]
fn a_signal_to_run_ends_the_command_too() {
    let fx = fixture();
    let name = unique("relay");
    branch(&fx, &name);
    add(&fx, &name);
    let mut child = Command::new(BIN)
        .args(["run", &name, "--", "sleep", "1000"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    assert!(
        wait_until(
            || !processes_with_klon_id(&name).is_empty(),
            Duration::from_secs(10)
        ),
        "the command never started"
    );

    // SIGTERM to the wrapper only. Without the relay the sleep would survive.
    let wrapper = i32::try_from(child.id()).expect("a pid fits in i32");
    // SAFETY: `kill` takes two integers; the pid names the live wrapper.
    assert_eq!(unsafe { libc::kill(wrapper, libc::SIGTERM) }, 0);
    let _ = child.wait();
    assert!(
        wait_until(
            || processes_with_klon_id(&name).is_empty(),
            Duration::from_secs(5)
        ),
        "the command survived the signal to the wrapper: {:?}",
        processes_with_klon_id(&name)
    );
}

/// Two repositories can hold one branch name, and each hands out `127.0.0.2`
/// to its first klon. `stop` must still end only the tree of the klon it was
/// asked about, so the tag set names the klon directory as well as the branch.
#[cfg(target_os = "linux")]
#[test]
fn stop_leaves_a_klon_of_another_repository_alone() {
    let mine = fixture();
    let other = fixture();
    add(&mine, "feature");
    add(&other, "feature");
    // Both klons take the lowest address of their own repository.
    assert_eq!(
        run_ok(&mine, "feature", &["sh", "-c", "echo $KLON_IP"]),
        run_ok(&other, "feature", &["sh", "-c", "echo $KLON_IP"])
    );

    let child = Command::new(BIN)
        .args(["run", "feature", "--", "sleep", "1000"])
        .current_dir(&other.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    let mut reaper = Reaper(child);
    let victim = other.klon_path("feature");
    assert!(
        wait_until(
            || !processes_in_klon(&victim).is_empty(),
            Duration::from_secs(10)
        ),
        "the other repository must have a live process"
    );

    let out = klon(&mine.golden, &["stop", "feature"]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(
        !processes_in_klon(&victim).is_empty(),
        "stop must leave the klon of the other repository alone"
    );
    assert!(reaper.0.try_wait().is_ok_and(|status| status.is_none()));

    let out = klon(&other.golden, &["stop", "feature"]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(processes_in_klon(&victim).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn stop_ends_the_whole_tree_including_the_grandchild() {
    let fx = fixture();
    let name = unique("tree");
    branch(&fx, &name);
    let klon_path = add(&fx, &name);
    // A shell with one background child and one foreground child. The
    // background `sleep` is the grandchild the AC names. `dash` keeps all
    // three processes: it execs no command of a two-command script.
    let child = Command::new(BIN)
        .args(["run", &name, "--", "sh", "-c", "sleep 1000 & sleep 1000"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    let mut reaper = Reaper(child);

    // The tree is up when the shell and both sleeps carry the tag.
    assert!(
        wait_until(
            || processes_with_klon_id(&name).len() >= 3,
            Duration::from_secs(10)
        ),
        "the run tree never started: {:?}",
        processes_with_klon_id(&name)
    );

    let started = Instant::now();
    let out = klon(&fx.golden, &["stop", &name]);
    let elapsed = started.elapsed();
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    // AC: every process ends within 5 s, the grandchild included.
    assert!(
        elapsed < Duration::from_secs(5),
        "stop took {elapsed:?}, over the 5 s limit"
    );
    let left = processes_with_klon_id(&name);
    assert!(left.is_empty(), "these processes survived stop: {left:?}");
    assert!(
        stdout(&out).contains("3 processes"),
        "stop must report the count: {}",
        stdout(&out)
    );

    // The wrapper sees its child die and returns.
    assert!(
        wait_until(
            || reaper.0.try_wait().is_ok_and(|status| status.is_some()),
            Duration::from_secs(5)
        ),
        "the run wrapper never returned"
    );
    // A stopped klon is still a klon: nothing on disk changed.
    assert!(klon_path.join(".klon").join("env").is_file());
}

#[cfg(target_os = "linux")]
#[test]
fn stop_leaves_a_sibling_klon_alone() {
    let fx = fixture();
    let first = unique("left");
    let second = unique("right");
    let mut children = Vec::new();
    for name in [&first, &second] {
        branch(&fx, name);
        add(&fx, name);
        let child = Command::new(BIN)
            .args(["run", name, "--", "sleep", "1000"])
            .current_dir(&fx.golden)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn run");
        children.push(Reaper(child));
    }
    assert!(
        wait_until(
            || !processes_with_klon_id(&first).is_empty()
                && !processes_with_klon_id(&second).is_empty(),
            Duration::from_secs(10)
        ),
        "both klons must have a live process"
    );

    let out = klon(&fx.golden, &["stop", &first]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(processes_with_klon_id(&first).is_empty());
    assert!(
        !processes_with_klon_id(&second).is_empty(),
        "stop must leave the sibling klon alone"
    );

    let out = klon(&fx.golden, &["stop", &second]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    assert!(processes_with_klon_id(&second).is_empty());
}

// --- doctor ------------------------------------------------------------------

#[test]
fn doctor_reports_the_address_pool_and_the_loopback_bind() {
    let fx = fixture();
    add(&fx, "feature");
    let out = klon(&fx.golden, &["--json", "doctor"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(document["features"]["slots"]["status"], "present");
    assert!(
        document["features"]["slots"]["detail"]
            .as_str()
            .is_some_and(|text| text.contains('1')),
        "one klon means one address: {}",
        document["features"]["slots"]["detail"]
    );
    // Linux gives `lo` all of `127/8`, so the bind must work. macOS gives
    // `lo0` only `127.0.0.1`; the row then reports `broken` and names the
    // `ifconfig` alias, which is the documented state until C21.
    let loopback = document["features"]["loopback"]["status"]
        .as_str()
        .expect("a loopback row");
    if cfg!(target_os = "linux") {
        assert_eq!(loopback, "present", "127.0.0.2 must accept a bind on Linux");
    } else {
        assert!(
            loopback == "present" || loopback == "broken",
            "unexpected loopback status {loopback}"
        );
    }
}
