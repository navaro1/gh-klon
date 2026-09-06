//! Acceptance tests for `run --netns` with pasta (spec §7 C23, R21): the port
//! mapping into the namespace, two namespaces on one port, outbound traffic,
//! and the one-line degradation on a host without pasta.
//!
//! The development laptop has no pasta, so every test that needs the tool
//! skips with a printed reason there. The CI `netns` job on `ubuntu-24.04`
//! installs pasta and is the proof for those acceptance lines.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::{git_ok, klon, klon_env, stderr, stdout, Fixture, BIN};

const SEED: u64 = 23;

/// The port every acceptance line names.
const PORT: u16 = 3000;

/// A small fixture. Every test here reads the envelope, not the tree.
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

/// The last `n` lines of `text`. The `KLON_DEBUG` fence lines sit above the
/// pasta errors, so a failure message prints the tail, not the whole stderr.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// True when `program` sits in a PATH directory.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// True when the host has pasta. The pasta tests ask once and skip with a
/// printed reason when the answer is no.
fn has_pasta() -> bool {
    on_path("pasta")
}

/// The `KLON_IP` of the klon at `path`, from `.klon/env`.
fn klon_ip(path: &Path) -> String {
    let text = fs::read_to_string(path.join(".klon").join("env")).expect("read .klon/env");
    text.lines()
        .find_map(|line| line.strip_prefix("KLON_IP="))
        .expect("KLON_IP in .klon/env")
        .trim_matches('\'')
        .to_string()
}

/// One plain HTTP GET over one TCP connection. The answer is true when a
/// server answers `200`. A socket needs nothing from PATH and sees no proxy
/// variables, so the poll cannot false-fail on a runner with a proxy set.
fn http_ok(address: &str, port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect((address, port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    if stream.write_all(b"GET / HTTP/1.0\r\n\r\n").is_err() {
        return false;
    }
    let mut body = String::new();
    if stream.read_to_string(&mut body).is_err() {
        return false;
    }
    body.starts_with("HTTP/1.") && body.contains(" 200 ")
}

/// Poll `http_ok` every 200 ms until it holds or the timeout passes.
fn wait_http(address: &str, port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if http_ok(address, port) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// pasta binds `KLON_IP:<port>` on the host for every mapped port, and two
/// parallel test repositories would hand out the same `127.0.0.2`. The lock
/// serializes the tests that put a pasta on the host loopback; each holds it
/// for a few seconds at most.
static PORT_LOCK: Mutex<()> = Mutex::new(());

fn hold_ports() -> MutexGuard<'static, ()> {
    PORT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A background `run --netns` server: `python3 -m http.server 3000 --bind
/// 0.0.0.0` inside the namespace. The Drop ends the klon's process tree with
/// `gh klon stop`, so a failed assert leaks nothing on the runner.
struct Server {
    golden: PathBuf,
    branch: String,
    child: Child,
}

impl Server {
    fn start(fx: &Fixture, branch: &str) -> Server {
        let child = Command::new(BIN)
            .args([
                "run",
                branch,
                "--netns",
                "--",
                "python3",
                "-m",
                "http.server",
                "3000",
                "--bind",
                "0.0.0.0",
            ])
            .current_dir(&fx.golden)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn run --netns");
        Server {
            golden: fx.golden.clone(),
            branch: branch.to_string(),
            child,
        }
    }

    /// The server process still runs. A python that died on `EADDRINUSE`
    /// takes `run` with it, so a live process is the proof the AC asks for.
    fn alive(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = klon(&self.golden, &["stop", &self.branch]);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- With pasta (the CI netns job on ubuntu-24.04 proves these) --------------

/// AC: under `run --netns`, a server bound to `0.0.0.0:3000` inside is
/// reachable from the host at `<KLON_IP>:3000`.
#[test]
fn a_server_inside_the_namespace_answers_on_the_klon_address() {
    if !has_pasta() {
        println!("skipped: pasta is not on PATH");
        return;
    }
    if !on_path("python3") {
        println!("skipped: python3 is not on PATH");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    let ip = klon_ip(&fx.klon_path("feature"));
    let _ports = hold_ports();
    let mut server = Server::start(&fx, "feature");
    assert!(
        wait_http(&ip, PORT, Duration::from_secs(15)),
        "the host cannot reach {ip}:{PORT} through the namespace"
    );
    assert!(server.alive(), "the server under run --netns exited");
}

/// AC: two klons under `run --netns` both bind `0.0.0.0:3000` without
/// `EADDRINUSE`.
#[test]
fn two_klons_bind_the_same_port_in_their_own_namespaces() {
    if !has_pasta() {
        println!("skipped: pasta is not on PATH");
        return;
    }
    if !on_path("python3") {
        println!("skipped: python3 is not on PATH");
        return;
    }
    let fx = fixture();
    git_ok(&fx.golden, &["branch", "feature2", "main"]);
    add(&fx, "feature");
    add(&fx, "feature2");
    let first = klon_ip(&fx.klon_path("feature"));
    let second = klon_ip(&fx.klon_path("feature2"));
    assert_ne!(first, second, "two klons must hold two addresses");
    let _ports = hold_ports();
    let mut one = Server::start(&fx, "feature");
    let mut two = Server::start(&fx, "feature2");
    for (branch, ip) in [("feature", &first), ("feature2", &second)] {
        assert!(
            wait_http(ip, PORT, Duration::from_secs(15)),
            "the host cannot reach {ip}:{PORT} for {branch}"
        );
    }
    assert!(one.alive(), "the first server exited (EADDRINUSE?)");
    assert!(two.alive(), "the second server exited (EADDRINUSE?)");
}

/// AC: inside the namespace, `curl https://example.com` exits 0. A runner
/// without outbound network skips, because the failure would say nothing
/// about the namespace. The same request on the host is the control.
#[test]
fn outbound_traffic_works_inside_the_namespace() {
    if !has_pasta() {
        println!("skipped: pasta is not on PATH");
        return;
    }
    if !on_path("curl") {
        println!("skipped: curl is not on PATH");
        return;
    }
    let control = Command::new("curl")
        .args([
            "-fsS",
            "-o",
            "/dev/null",
            "--max-time",
            "10",
            "https://example.com",
        ])
        .output()
        .expect("run curl");
    if !control.status.success() {
        println!("skipped: the host has no outbound network to example.com");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    let _ports = hold_ports();
    let out = klon(
        &fx.golden,
        &[
            "run",
            "feature",
            "--netns",
            "--",
            "curl",
            "-fsS",
            "-o",
            "/dev/null",
            "--max-time",
            "20",
            "https://example.com",
        ],
    );
    assert!(
        out.status.success(),
        "curl inside the namespace failed: {}",
        stderr(&out)
    );
}

/// C23 x C18: pasta starts under the write fence. The fence is on by default,
/// and pasta writes `/proc/self/uid_map` inside it, so `allow_set` holds a
/// `/proc` rule. The `KLON_DEBUG` lines are the proof the rule is there.
#[test]
fn pasta_starts_under_the_write_fence() {
    if !has_pasta() {
        println!("skipped: pasta is not on PATH");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    let _ports = hold_ports();
    let out = klon_env(
        &fx.golden,
        &[("KLON_DEBUG", std::ffi::OsStr::new("1"))],
        &[
            "run",
            "feature",
            "--netns",
            "--",
            "sh",
            "-c",
            "echo under the fence",
        ],
    );
    assert!(
        out.status.success(),
        "run --netns failed under the fence: {}",
        tail(&stderr(&out), 12)
    );
    assert_eq!(stdout(&out).trim(), "under the fence");
    let text = stderr(&out);
    assert!(
        text.contains("klon: fence: allow "),
        "the write fence must be on for this test: {text}"
    );
    assert!(
        text.contains("klon: fence: allow /proc"),
        "the fence must hold a /proc rule for pasta's uid_map: {text}"
    );
}

// --- Without pasta (the development laptop proves this) ----------------------

/// AC: on a host without pasta, `run --netns` prints `pasta absent` and runs
/// the command. `shell --netns` degrades the same way.
#[test]
fn without_pasta_netns_prints_one_line_and_runs_the_command() {
    if has_pasta() {
        println!("skipped: pasta is on PATH, so the degradation cannot show");
        return;
    }
    let fx = fixture();
    add(&fx, "feature");
    let out = klon(
        &fx.golden,
        &["run", "feature", "--netns", "--", "echo", "hello"],
    );
    assert!(out.status.success(), "run failed: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "hello");
    assert_eq!(
        stderr(&out),
        "klon: pasta absent, running without a network namespace\n",
        "exactly one stderr line is allowed"
    );
    let out = klon(&fx.golden, &["shell", "feature", "--netns"]);
    assert!(out.status.success(), "shell failed: {}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "klon: pasta absent, running without a network namespace\n",
        "exactly one stderr line is allowed"
    );
}
