//! Acceptance tests for the build-slot jobserver (spec §7 C17, R19).
//!
//! Every test points `XDG_RUNTIME_DIR` at its own temporary directory, so the
//! fifo belongs to that test alone. A parallel test, and a parallel build of
//! another agent, can then never take one of its tokens.
//!
//! The shared harness lives in `tests/common`.

mod common;

use common::{git_ok, klon_env, stderr, stdout, Fixture};
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use common::BIN;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;

const SEED: u64 = 42;

/// A small fixture. These tests read the envelope and the token store, not the
/// tree, so a low file count keeps every `add` under a second.
fn fixture() -> Fixture {
    Fixture::generate(SEED, 40, 4, 5, 2)
}

// --- The private token store -------------------------------------------------

/// One test's own token store. The directory dies with the test.
struct Store {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    tokens: usize,
}

impl Store {
    fn new(tokens: usize) -> Store {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        Store {
            _tmp: tmp,
            dir,
            tokens,
        }
    }

    /// The fifo klon creates under this runtime directory.
    fn fifo(&self) -> PathBuf {
        self.dir.join("klon").join("jobserver")
    }
}

/// `gh-klon <args>` with this test's runtime directory and token target.
fn klon_in(
    store: &Store,
    cwd: &std::path::Path,
    extra: &[(&str, &OsStr)],
    args: &[&str],
) -> Output {
    let tokens = store.tokens.to_string();
    let mut envs: Vec<(&str, &OsStr)> = vec![
        ("XDG_RUNTIME_DIR", store.dir.as_os_str()),
        ("KLON_JOBSERVER_TOKENS", OsStr::new(&tokens)),
    ];
    envs.extend_from_slice(extra);
    klon_env(cwd, &envs, args)
}

/// `add <branch>` with this test's store. The answer is the klon path.
fn add(store: &Store, fx: &Fixture, name: &str) -> PathBuf {
    let out = klon_in(store, &fx.golden, &[], &["add", name]);
    assert!(out.status.success(), "add {name} failed: {}", stderr(&out));
    fx.klon_path(name)
}

/// Create a local branch on top of `main`.
fn branch(fx: &Fixture, name: &str) {
    git_ok(&fx.golden, &["branch", name, "main"]);
}

/// A branch name that no other test and no other build on this host shares.
/// The name becomes `KLON_ID`, and two of these tests scan the whole process
/// table, so a shared name would let two parallel runs see each other.
fn unique(tag: &str) -> String {
    format!("{tag}-{}", std::process::id())
}

/// True when `program` sits in a PATH directory.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// The one-minute load average on Linux; None elsewhere.
fn load_average_1m() -> Option<f64> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// True when this host is busy enough to make a wall-clock budget meaningless.
/// Four agents build in parallel on the development laptop; a CI runner is quiet.
fn host_is_loaded() -> bool {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let quiet = (cores / 2).max(1) as f64;
    load_average_1m().is_some_and(|load| load > quiet)
}

/// The `detail` string of one `doctor --json` feature row.
fn doctor_row(store: &Store, fx: &Fixture, name: &str) -> (String, String) {
    let out = klon_in(store, &fx.golden, &[], &["--json", "doctor"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let row = &document["features"][name];
    (
        row["status"].as_str().expect("a status").to_string(),
        row["detail"].as_str().expect("a detail").to_string(),
    )
}

// --- AC 1: make ---------------------------------------------------------------

/// Eight one-second jobs and two tokens give three slots, so make needs three
/// rounds. Without the jobserver `-j` would start all eight at once.
const MAKEFILE: &str = "\
.PHONY: all t1 t2 t3 t4 t5 t6 t7 t8
all: t1 t2 t3 t4 t5 t6 t7 t8
t1 t2 t3 t4 t5 t6 t7 t8:
\t@sleep 1
";

#[test]
fn make_runs_eight_one_second_jobs_under_two_tokens() {
    if !on_path("make") {
        println!("skipped: make is not on PATH");
        return;
    }
    let store = Store::new(2);
    let fx = fixture();
    let name = unique("make");
    branch(&fx, &name);
    let klon_path = add(&store, &fx, &name);
    fs::write(klon_path.join("Makefile"), MAKEFILE).expect("write the Makefile");

    let started = Instant::now();
    let out = klon_in(
        &store,
        &fx.golden,
        &[],
        &["run", &name, "--", "make", "-f", "Makefile"],
    );
    let elapsed = started.elapsed();
    assert!(out.status.success(), "make failed: {}", stderr(&out));

    // AC: make prints no jobserver error. make 4.3 stops with an internal
    // error on a `fifo:` handshake and warns when the descriptors are gone.
    let text = stderr(&out).to_lowercase();
    assert!(
        !text.contains("jobserver"),
        "make reported a jobserver problem: {}",
        stderr(&out)
    );

    // AC: the run takes 3 to 5 s. Three slots and eight jobs need three
    // rounds. Under 3 s means make ran more than three jobs at a time; over
    // 5 s means it ran fewer, and a store with no token would take 8 s.
    println!("eight one-second jobs with two tokens took {elapsed:?}");
    assert!(
        elapsed >= Duration::from_millis(2900),
        "the eight jobs took {elapsed:?}; two tokens must hold them to three rounds"
    );
    // The jobs sleep, so load moves the wall time by very little. A loaded
    // host gets a wider ceiling that still separates three rounds from eight.
    let ceiling = if host_is_loaded() {
        println!("the load average is above half the core count; the ceiling grows to 7 s");
        Duration::from_secs(7)
    } else {
        Duration::from_secs(5)
    };
    assert!(
        elapsed <= ceiling,
        "the eight jobs took {elapsed:?}; the budget is {ceiling:?}"
    );
}

/// The handshake klon exports, and the `KLON_NO_JOBSERVER` switch beside it.
#[test]
fn run_exports_the_pipe_style_handshake() {
    let store = Store::new(2);
    let fx = fixture();
    let name = unique("flags");
    branch(&fx, &name);
    add(&store, &fx, &name);

    let out = klon_in(
        &store,
        &fx.golden,
        &[],
        &["run", &name, "--", "sh", "-c", "echo $MAKEFLAGS"],
    );
    assert!(out.status.success(), "run failed: {}", stderr(&out));
    let flags = stdout(&out).trim().to_string();
    let auth = flags
        .strip_prefix("-j --jobserver-auth=")
        .unwrap_or_else(|| panic!("unexpected MAKEFLAGS {flags}"));
    let (read, write) = auth.split_once(',').expect("two descriptor numbers");
    let read: i32 = read.parse().expect("a read descriptor");
    let write: i32 = write.parse().expect("a write descriptor");
    // The pipe style names two numbers, never `fifo:<path>`: make 4.3 stops
    // with a fatal error on that form (handoff §11).
    assert!(!flags.contains("fifo:"), "klon must not use the fifo style");
    // Neither end may land on a standard stream, which the spawn replaces.
    assert!(
        read >= 3 && write >= 3,
        "unexpected descriptors {read},{write}"
    );
    assert_ne!(read, write, "the read end and the write end are two copies");

    // The fifo exists and the klon knows where it is.
    assert!(store.fifo().exists(), "run must create the token store");
    let printed = klon_in(
        &store,
        &fx.golden,
        &[],
        &["run", &name, "--", "sh", "-c", "echo $KLON_JOBSERVER"],
    );
    assert_eq!(
        stdout(&printed).trim(),
        store.fifo().to_string_lossy(),
        "run must name the store it opened"
    );
}

// --- AC 4: the off switch -----------------------------------------------------

#[test]
fn no_jobserver_prints_an_empty_makeflags() {
    let store = Store::new(2);
    let fx = fixture();
    let name = unique("off");
    branch(&fx, &name);
    add(&store, &fx, &name);

    // AC: `KLON_NO_JOBSERVER=1 gh klon run x -- sh -c 'echo $MAKEFLAGS'`
    // prints an empty line.
    let out = klon_in(
        &store,
        &fx.golden,
        &[("KLON_NO_JOBSERVER", OsStr::new("1"))],
        &["run", &name, "--", "sh", "-c", "echo $MAKEFLAGS"],
    );
    assert!(out.status.success(), "run failed: {}", stderr(&out));
    assert_eq!(stdout(&out), "\n", "MAKEFLAGS must be empty");
    // klon touches no store at all when the switch is on.
    assert!(
        !store.fifo().exists(),
        "the off switch must create no fifo: {}",
        store.fifo().display()
    );

    // A caller's own value never leaks into the klon either.
    let out = klon_in(
        &store,
        &fx.golden,
        &[
            ("KLON_NO_JOBSERVER", OsStr::new("1")),
            ("MAKEFLAGS", OsStr::new("-j --jobserver-auth=91,92")),
        ],
        &["run", &name, "--", "sh", "-c", "echo $MAKEFLAGS"],
    );
    assert!(out.status.success(), "run failed: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "\n",
        "an inherited MAKEFLAGS must not reach the command"
    );

    // `doctor` reports the switch instead of a store.
    let (status, detail) = doctor_row_off(&store, &fx);
    assert_eq!(status, "absent", "doctor row: {detail}");
    assert!(
        detail.contains("KLON_NO_JOBSERVER"),
        "doctor must name the switch: {detail}"
    );
}

/// The `doctor --json` jobserver row with the switch on.
fn doctor_row_off(store: &Store, fx: &Fixture) -> (String, String) {
    let out = klon_in(
        store,
        &fx.golden,
        &[("KLON_NO_JOBSERVER", OsStr::new("1"))],
        &["--json", "doctor"],
    );
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let row = &document["features"]["jobserver"];
    (
        row["status"].as_str().expect("a status").to_string(),
        row["detail"].as_str().expect("a detail").to_string(),
    )
}

// --- doctor -------------------------------------------------------------------

#[test]
fn doctor_reports_the_store_and_the_make_handshake() {
    let store = Store::new(3);
    let fx = fixture();
    add(&store, &fx, "feature");

    let (status, detail) = doctor_row(&store, &fx, "jobserver");
    assert_eq!(status, "present", "jobserver row: {detail}");
    assert!(
        detail.starts_with(&store.fifo().to_string_lossy().to_string()),
        "the row must name the fifo: {detail}"
    );
    // No klon runs, so the store holds no token: a fifo keeps its buffer only
    // while a descriptor is open. The row names the target and the top-up.
    assert!(
        detail.contains("0 of 3 tokens"),
        "the row must name the count and the target: {detail}"
    );
    assert!(
        detail.contains("3 restored") && detail.contains("no klon holds the store open"),
        "the row must say the store is idle: {detail}"
    );

    let (status, detail) = doctor_row(&store, &fx, "make");
    if status == "present" {
        assert!(
            detail.contains("pipe-style jobserver handshake"),
            "the make row must name the handshake: {detail}"
        );
    } else {
        println!("skipped the make row: make is {status}: {detail}");
    }
}

// --- AC 3: a client that a signal ended --------------------------------------

/// Take one token from the store and hold it until a signal ends this process.
/// `dd bs=1 count=1` reads exactly one byte; `head -c 1` may read a whole block
/// from the fifo and would take several tokens at once.
#[cfg(target_os = "linux")]
const TAKE_TOKEN: &str = "\
auth=${MAKEFLAGS##*--jobserver-auth=}
read_fd=${auth%%,*}
eval \"exec 9<&${read_fd}\"
dd bs=1 count=1 <&9 >/dev/null 2>&1
echo held > held
exec sleep 1000
";

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

/// Every process whose environment names this store. The test scans `/proc`
/// itself, so it never asks klon whether klon did its own work.
#[cfg(target_os = "linux")]
fn store_holders(store: &Store) -> Vec<u32> {
    processes_tagged(&format!("KLON_JOBSERVER={}", store.fifo().display()))
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
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(bytes) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        if bytes
            .split(|byte| *byte == 0)
            .any(|item| item == needle.as_slice())
        {
            pids.push(pid);
        }
    }
    pids
}

/// Start `gh klon run <name> -- <cmd>` in the background with this test's store.
#[cfg(target_os = "linux")]
fn spawn_run(store: &Store, fx: &Fixture, name: &str, command: &[&str]) -> Child {
    let mut args = vec!["run", name, "--"];
    args.extend_from_slice(command);
    Command::new(BIN)
        .args(&args)
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_RUNTIME_DIR", &store.dir)
        .env("KLON_JOBSERVER_TOKENS", store.tokens.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run")
}

/// Every process whose environment holds `KLON_ID=<name>`: the tree of one klon.
#[cfg(target_os = "linux")]
fn processes_with_klon_id(name: &str) -> Vec<u32> {
    processes_tagged(&format!("KLON_ID={name}"))
}

/// SIGKILL every process of `pids`. A killed client never writes its token back.
#[cfg(target_os = "linux")]
fn kill_all(pids: &[u32]) {
    for pid in pids {
        // SAFETY: `kill` takes two integers; each pid names a process of this
        // test's own klon.
        unsafe {
            libc::kill(
                i32::try_from(*pid).expect("a pid fits in i32"),
                libc::SIGKILL,
            )
        };
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_killed_client_leaves_a_shortfall_that_doctor_reports_and_a_new_run_repairs() {
    let store = Store::new(2);
    let fx = fixture();
    let anchor_name = unique("anchor");
    let client_name = unique("client");
    branch(&fx, &anchor_name);
    branch(&fx, &client_name);
    add(&store, &fx, &anchor_name);
    let client_path = add(&store, &fx, &client_name);
    fs::write(client_path.join("take.sh"), TAKE_TOKEN).expect("write the client script");

    // A fifo drops every token when its last descriptor closes, so the store
    // needs a klon that holds it open for the shortfall to be observable at
    // all. The anchor takes no token; it only keeps the store alive.
    let anchor = Reaper(spawn_run(&store, &fx, &anchor_name, &["sleep", "1000"]));
    assert!(
        wait_until(
            || !processes_with_klon_id(&anchor_name).is_empty(),
            Duration::from_secs(30)
        ),
        "the anchor klon never started"
    );

    let client = Reaper(spawn_run(&store, &fx, &client_name, &["sh", "take.sh"]));
    assert!(
        wait_until(
            || client_path.join("held").exists(),
            Duration::from_secs(30)
        ),
        "the client never took a token"
    );

    // SIGKILL, so the client never writes its token back.
    let victims = processes_with_klon_id(&client_name);
    assert!(!victims.is_empty(), "the client must be running");
    kill_all(&victims);
    assert!(
        wait_until(
            || processes_with_klon_id(&client_name).is_empty(),
            Duration::from_secs(10)
        ),
        "the client survived SIGKILL"
    );
    drop(client);

    // AC: `doctor` reports the shortfall the killed client left.
    let (status, detail) = doctor_row(&store, &fx, "jobserver");
    assert_eq!(status, "present", "jobserver row: {detail}");
    assert!(
        detail.contains("1 of 2 tokens"),
        "doctor must report the shortfall: {detail}"
    );
    assert!(
        detail.contains("1 short"),
        "doctor must name the missing token: {detail}"
    );
    assert!(
        detail.contains("a klon holds the store open"),
        "doctor must say that a klon holds the store: {detail}"
    );
    // A live klon keeps its tokens: a write here would make the count grow
    // past the target when that klon gives a token back.
    assert!(
        !detail.contains("restored"),
        "the top-up must leave a live store alone: {detail}"
    );

    // AC: the top-up restores the count. The store goes idle, and the next run
    // fills it again through the descriptors it hands to its own command.
    kill_all(&processes_with_klon_id(&anchor_name));
    drop(anchor);
    assert!(
        wait_until(|| store_holders(&store).is_empty(), Duration::from_secs(10)),
        "the anchor klon survived SIGKILL"
    );

    let repaired = Reaper(spawn_run(&store, &fx, &anchor_name, &["sleep", "1000"]));
    assert!(
        wait_until(
            || !processes_with_klon_id(&anchor_name).is_empty(),
            Duration::from_secs(30)
        ),
        "the second anchor klon never started"
    );
    let (status, detail) = doctor_row(&store, &fx, "jobserver");
    assert_eq!(status, "present", "jobserver row: {detail}");
    assert!(
        detail.contains("2 of 2 tokens"),
        "the next run must restore the count: {detail}"
    );
    kill_all(&processes_with_klon_id(&anchor_name));
    drop(repaired);
}

/// A live client must keep its token: the top-up may only write to an idle
/// store, or the count would grow past the target.
#[cfg(target_os = "linux")]
#[test]
fn the_top_up_leaves_a_live_client_alone() {
    let store = Store::new(2);
    let fx = fixture();
    let name = unique("live");
    branch(&fx, &name);
    let klon_path = add(&store, &fx, &name);
    fs::write(klon_path.join("take.sh"), TAKE_TOKEN).expect("write the client script");

    let tokens = store.tokens.to_string();
    let child = Command::new(BIN)
        .args(["run", &name, "--", "sh", "take.sh"])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_RUNTIME_DIR", &store.dir)
        .env("KLON_JOBSERVER_TOKENS", &tokens)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    let reaper = Reaper(child);

    assert!(
        wait_until(|| klon_path.join("held").exists(), Duration::from_secs(30)),
        "the client never took a token"
    );

    let (status, detail) = doctor_row(&store, &fx, "jobserver");
    assert_eq!(status, "present", "jobserver row: {detail}");
    assert!(
        detail.contains("1 of 2 tokens"),
        "doctor must report the token the client holds: {detail}"
    );
    assert!(
        !detail.contains("restored"),
        "the top-up must leave a live client alone: {detail}"
    );
    assert!(
        detail.contains("a klon holds the store open"),
        "doctor must name the holder: {detail}"
    );
    drop(reaper);
}

// --- AC 2: cargo --------------------------------------------------------------

/// Four crates with no dependency on each other. Each build script sleeps two
/// seconds, so the overlap is long enough for a 50 ms sampler to see it.
#[cfg(target_os = "linux")]
fn write_workspace(root: &std::path::Path) {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"ja\", \"jb\", \"jc\", \"jd\"]\nresolver = \"2\"\n",
    )
    .expect("write the workspace");
    for name in ["ja", "jb", "jc", "jd"] {
        let crate_dir = root.join(name);
        fs::create_dir_all(crate_dir.join("src")).expect("create the crate");
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .expect("write the manifest");
        fs::write(crate_dir.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n")
            .expect("write the library");
        fs::write(
            crate_dir.join("build.rs"),
            "fn main() {\n    std::thread::sleep(std::time::Duration::from_secs(2));\n}\n",
        )
        .expect("write the build script");
    }
}

/// Every compile process of this klon: `rustc` and a build script both take one
/// jobserver token, and the `KLON_ID` tag keeps another agent's parallel build
/// out of the count.
///
/// cargo also asks `rustc` what it supports before it schedules any work, with
/// `rustc -vV` and `rustc --print=...`. Those calls take no token, so counting
/// them would report a breach that never happened.
#[cfg(target_os = "linux")]
fn compile_processes(name: &str) -> usize {
    let tag = format!("KLON_ID={name}").into_bytes();
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .and_then(|item| item.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let words: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|word| !word.is_empty())
            .map(|word| String::from_utf8_lossy(word).into_owned())
            .collect();
        let Some(argv0) = words.first() else {
            continue;
        };
        let program = argv0.rsplit('/').next().unwrap_or_default();
        if program != "rustc" && !program.contains("build-script") {
            continue;
        }
        // A question to rustc, not a compilation. It holds no token.
        if words
            .iter()
            .any(|word| word == "-vV" || word.starts_with("--print"))
        {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        if bytes
            .split(|byte| *byte == 0)
            .any(|item| item == tag.as_slice())
        {
            count += 1;
        }
    }
    count
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_keeps_at_most_three_compile_processes_under_two_tokens() {
    if !on_path("cargo") {
        println!("skipped: cargo is not on PATH");
        return;
    }
    let store = Store::new(2);
    let fx = fixture();
    let name = unique("cargo");
    branch(&fx, &name);
    let klon_path = add(&store, &fx, &name);
    write_workspace(&klon_path);

    // The sampler reads `/proc` every 50 ms while the build runs.
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let stop = Arc::clone(&stop);
        let peak = Arc::clone(&peak);
        let name = name.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let now = compile_processes(&name);
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };

    let target_dir = klon_path.join("cargo-target");
    let out = klon_in(
        &store,
        &fx.golden,
        &[("CARGO_TARGET_DIR", target_dir.as_os_str())],
        &["run", &name, "--", "cargo", "build", "-q"],
    );
    stop.store(true, Ordering::SeqCst);
    sampler.join().expect("the sampler thread");
    assert!(out.status.success(), "cargo build failed: {}", stderr(&out));
    assert!(
        !stderr(&out).to_lowercase().contains("jobserver"),
        "cargo reported a jobserver problem: {}",
        stderr(&out)
    );

    // AC: at most the token count plus one, which is the implicit slot every
    // jobserver client owns (R19).
    let peak = peak.load(Ordering::SeqCst);
    println!("the sampler saw at most {peak} compile processes of this klon");
    assert!(
        peak >= 2,
        "the sampler saw {peak} compile processes; the build must run in parallel"
    );
    assert!(
        peak <= store.tokens + 1,
        "{peak} compile processes ran at once; {} tokens allow {}",
        store.tokens,
        store.tokens + 1
    );
}
