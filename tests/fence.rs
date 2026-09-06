//! Acceptance tests for the Linux write fence (spec §7 C18, R17). Every test
//! runs a command under `run` and checks what it can and cannot write.
//!
//! The fixture lives under `$HOME/.local/share`, outside every path the fence
//! allows: `/tmp` is in the allow set, so a fixture there could never show a
//! denied write to golden. Each test skips with a printed reason when the
//! kernel has no Landlock.
#![cfg(target_os = "linux")]

mod common;

use common::{git_ok, klon, klon_env, stderr, stdout, Fixture};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

const SEED: u64 = 43;

/// The Landlock ABI of the kernel, from the version query of
/// `landlock_create_ruleset(2)`. Below 1 the fence is absent.
fn landlock_abi() -> libc::c_long {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: the documented version query; the call touches no memory of ours.
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    }
}

/// `$HOME/.local/share/gh-klon-tests`: a place outside the allow set.
fn base_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    let base = PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("gh-klon-tests");
    fs::create_dir_all(&base).ok()?;
    Some(base)
}

/// True when `program` sits in a PATH directory.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// A fixture with a klon of `feature`, plus the base directory for more
/// temporary directories outside the allow set.
struct Ready {
    fx: Fixture,
    klon: PathBuf,
    base: PathBuf,
}

/// The fixture, or None with a printed reason.
fn ready() -> Option<Ready> {
    let abi = landlock_abi();
    if abi < 1 {
        println!("skipped: Landlock is absent; the ABI query returned {abi}");
        return None;
    }
    let Some(base) = base_dir() else {
        println!("skipped: no writable $HOME/.local/share for a fixture outside the allow set");
        return None;
    };
    let fx = Fixture::generate_in(&base, SEED, 40, 4, 5, 2);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon = fx.klon_path("feature");
    Some(Ready { fx, klon, base })
}

impl Ready {
    /// `run feature -- sh -c <script>` with extra environment variables.
    fn run(&self, envs: &[(&str, &OsStr)], script: &str) -> Output {
        klon_env(
            &self.fx.golden,
            envs,
            &["run", "feature", "--", "sh", "-c", script],
        )
    }

    /// A directory outside the allow set, for a temporary `HOME`.
    fn temp_home(&self) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("home-")
            .tempdir_in(&self.base)
            .expect("a temporary home")
    }
}

fn assert_denied(out: &Output, what: &str) {
    assert!(
        !out.status.success(),
        "{what} must fail under the fence; stdout: {}",
        stdout(out)
    );
    assert!(
        stderr(out).contains("Permission denied"),
        "{what}: stderr must say Permission denied: {}",
        stderr(out)
    );
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} must succeed under the fence: {}",
        stderr(out)
    );
}

fn touch(path: &Path) -> String {
    format!("touch '{}'", path.display())
}

// --- Denied writes -----------------------------------------------------------

#[test]
fn run_denies_golden_a_sibling_and_the_ssh_directory() {
    let Some(r) = ready() else { return };
    git_ok(&r.fx.golden, &["branch", "other", "main"]);
    let out = klon(&r.fx.golden, &["add", "other"]);
    assert!(out.status.success(), "add other failed: {}", stderr(&out));
    let sibling = r.fx.klon_path("other");

    // AC: `touch <golden>/x` fails with EACCES.
    let target = r.fx.golden.join("x");
    assert_denied(&r.run(&[], &touch(&target)), "touch golden/x");
    assert!(!target.exists(), "golden must stay untouched");

    // AC: `touch <sibling>/x` fails.
    let target = sibling.join("x");
    assert_denied(&r.run(&[], &touch(&target)), "touch sibling/x");
    assert!(!target.exists(), "the sibling must stay untouched");

    // AC: `touch ~/.ssh/x` fails. The home is a temporary one outside the
    // allow set; the real `~/.ssh` is never touched.
    let home = r.temp_home();
    fs::create_dir(home.path().join(".ssh")).unwrap();
    let out = r.run(
        &[("HOME", home.path().as_os_str())],
        "touch \"$HOME/.ssh/x\"",
    );
    assert_denied(&out, "touch ~/.ssh/x");
    assert!(!home.path().join(".ssh").join("x").exists());
}

// --- Allowed writes ----------------------------------------------------------

#[test]
fn run_allows_the_klon_the_tmpdir_and_the_cargo_home() {
    let Some(r) = ready() else { return };

    // AC: `touch <klon>/x` succeeds.
    assert_ok(&r.run(&[], &touch(&r.klon.join("x"))), "touch klon/x");
    assert!(r.klon.join("x").is_file());

    // AC: `touch $TMPDIR/x` succeeds, and `TMPDIR` is the klon's own.
    assert_ok(&r.run(&[], "touch \"$TMPDIR/x\""), "touch $TMPDIR/x");
    assert!(r.klon.join(".klon").join("tmp").join("x").is_file());

    // AC: a write under `~/.cargo` succeeds. The home is a temporary one.
    let home = r.temp_home();
    fs::create_dir(home.path().join(".cargo")).unwrap();
    let out = r.run(
        &[("HOME", home.path().as_os_str())],
        "touch \"$HOME/.cargo/x\"",
    );
    assert_ok(&out, "touch ~/.cargo/x");
    assert!(home.path().join(".cargo").join("x").is_file());

    // A move across two directories inside the klon needs the refer right
    // of ABI 2. Every host klon supports has it; ABI 1 is the documented
    // exception, so the check runs only where the kernel offers the right.
    if landlock_abi() >= 2 {
        let out = r.run(
            &[],
            "mkdir -p a b && touch a/f && mv a/f b/f && test -f b/f && ln b/f a/g",
        );
        assert_ok(&out, "mv and ln across directories inside the klon");
        // A move out of the klon into golden must still fail.
        let out = r.run(&[], &format!("mv b/f '{}/moved'", r.fx.golden.display()));
        assert!(!out.status.success(), "mv into golden must fail");
        assert!(!r.fx.golden.join("moved").exists());
    }
}

#[test]
fn cargo_build_of_a_small_crate_succeeds_under_run() {
    let Some(r) = ready() else { return };
    if !on_path("cargo") {
        println!("skipped: cargo is not on PATH");
        return;
    }
    let crate_dir = r.klon.join("hello");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(crate_dir.join("src").join("main.rs"), "fn main() {}\n").unwrap();

    // `CARGO_HOME` sits under a temporary home, so the build writes the
    // package cache lock there, not in the real one. The toolchain stays the
    // real one: `RUSTUP_HOME` names it when rustup drives `cargo`.
    let home = r.temp_home();
    let cargo_home = home.path().join(".cargo");
    fs::create_dir(&cargo_home).unwrap();
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| real_home.map(|home| home.join(".rustup")))
        .filter(|dir| dir.is_dir());
    let target_dir = crate_dir.join("target");
    let mut envs: Vec<(&str, &OsStr)> = vec![
        ("HOME", home.path().as_os_str()),
        ("CARGO_HOME", cargo_home.as_os_str()),
        ("CARGO_TARGET_DIR", target_dir.as_os_str()),
    ];
    if let Some(rustup_home) = &rustup_home {
        envs.push(("RUSTUP_HOME", rustup_home.as_os_str()));
    }
    // AC: `cargo build` exits 0 under `run`.
    let out = r.run(&envs, "cd hello && cargo build --offline -q");
    assert_ok(&out, "cargo build");
    assert!(
        target_dir.join("debug").join("hello").is_file(),
        "the binary must land in the klon"
    );
}

// --- The escape hatch --------------------------------------------------------

#[test]
fn run_without_the_fence_allows_the_write_to_golden() {
    let Some(r) = ready() else { return };
    // AC: `run --no-fence` allows the write to golden.
    let target = r.fx.golden.join("x");
    let out = klon(
        &r.fx.golden,
        &[
            "run",
            "--no-fence",
            "feature",
            "--",
            "sh",
            "-c",
            &touch(&target),
        ],
    );
    assert_ok(&out, "touch golden/x with --no-fence");
    assert!(target.is_file());
    fs::remove_file(&target).unwrap();

    // `KLON_NO_FENCE=1` does the same for a harness.
    let out = r.run(&[("KLON_NO_FENCE", OsStr::new("1"))], &touch(&target));
    assert_ok(&out, "touch golden/x with KLON_NO_FENCE=1");
    assert!(target.is_file());
    fs::remove_file(&target).unwrap();

    // Without either, the fence is back.
    assert_denied(&r.run(&[], &touch(&target)), "touch golden/x");
}

// --- git under the fence -----------------------------------------------------

#[test]
fn git_commit_succeeds_under_run_and_golden_stays_read_only() {
    let Some(r) = ready() else { return };
    let before = git_ok(&r.klon, &["rev-parse", "HEAD"]);
    let identity: &[(&str, &OsStr)] = &[
        ("GIT_AUTHOR_NAME", OsStr::new("klon")),
        ("GIT_AUTHOR_EMAIL", OsStr::new("klon@example.com")),
        ("GIT_COMMITTER_NAME", OsStr::new("klon")),
        ("GIT_COMMITTER_EMAIL", OsStr::new("klon@example.com")),
    ];
    // AC: `git -C <klon> commit --allow-empty -m x` exits 0 under `run`.
    let out = r.run(
        identity,
        &format!("git -C '{}' commit --allow-empty -qm x", r.klon.display()),
    );
    assert_ok(&out, "git commit");
    let after = git_ok(&r.klon, &["rev-parse", "HEAD"]);
    assert_ne!(before, after, "the commit must move HEAD");
    // The reflog moved too, so the write to `<common>/logs` went through.
    let reflog = git_ok(&r.klon, &["reflog", "-n", "1"]);
    assert!(reflog.contains("commit"), "reflog: {reflog}");
    // A second commit exercises the index and the object store once more.
    let out = r.run(
        identity,
        &format!(
            "echo hi > note.txt && git -C '{}' add note.txt && git -C '{0}' commit -qm note",
            r.klon.display()
        ),
    );
    assert_ok(&out, "git add and commit");

    // AC: `touch <golden>/src/x` still fails. The fixture has `d000` for `src`.
    let target = r.fx.golden.join("d000").join("x");
    assert_denied(&r.run(&[], &touch(&target)), "touch golden/d000/x");
    assert!(!target.exists());
    assert_eq!(
        git_ok(&r.fx.golden, &["status", "--porcelain"]),
        "",
        "golden must stay clean"
    );
}

#[test]
fn hooks_and_config_stay_read_only_and_doctor_names_the_residual() {
    let Some(r) = ready() else { return };
    let common = r.fx.golden.join(".git");
    fs::create_dir_all(common.join("hooks")).unwrap();

    // AC: `touch <common>/hooks/x` fails with EACCES.
    let target = common.join("hooks").join("x");
    assert_denied(&r.run(&[], &touch(&target)), "touch common/hooks/x");
    assert!(!target.exists());

    // AC: `git config --local user.name x` fails with EACCES: the lock file
    // would land in `<common>`, which is never in the allow set.
    let out = r.run(&[], "git config --local user.name x");
    assert_denied(&out, "git config --local");
    assert!(
        !klon(
            &r.fx.golden,
            &[
                "run",
                "feature",
                "--",
                "git",
                "config",
                "--get",
                "user.name"
            ]
        )
        .status
        .success(),
        "user.name must stay unset"
    );

    // AC: `doctor` lists `refs/heads/<base>` as writable under the fence.
    let out = klon(&r.fx.golden, &["--json", "doctor"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let document: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let residual = &document["features"]["fence.residual"];
    assert_eq!(residual["status"], "present");
    assert!(
        residual["detail"]
            .as_str()
            .is_some_and(|text| text.contains("refs/heads/main")),
        "the residual must name the base branch: {residual}"
    );
    // `doctor` reports the ABI.
    let landlock = &document["features"]["landlock"];
    assert_eq!(landlock["status"], "present");
    assert_eq!(
        landlock["detail"].as_str(),
        Some(format!("ABI {}", landlock_abi()).as_str())
    );
    let human = stdout(&klon(&r.fx.golden, &["doctor"]));
    assert!(human.contains("landlock"), "{human}");
    assert!(human.contains("refs/heads/main"), "{human}");
}

// --- `[fence] allow` and `add -- cmd` ----------------------------------------

#[test]
fn allow_entries_extend_the_fence_and_add_with_a_command_runs_fenced() {
    let Some(r) = ready() else { return };
    git_ok(&r.fx.golden, &["branch", "other", "main"]);
    // An absolute entry, a relative one (relative to the klon), and one that
    // klon must refuse: a repository must not open the whole fence.
    let extra = tempfile::Builder::new()
        .prefix("extra-")
        .tempdir_in(&r.base)
        .unwrap();
    let shared = r.fx.klon_path("other").parent().unwrap().join("shared");
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        r.fx.golden.join(".klon.toml"),
        format!(
            "[fence]\nallow = [\"{}\", \"../shared\", \"/\"]\n",
            extra.path().display()
        ),
    )
    .unwrap();

    // `add x -- cmd` runs the command under the fence, with the allow set.
    let script = format!("touch '{}/x' && touch ../shared/y", extra.path().display());
    let out = klon_env(
        &r.fx.golden,
        &[("KLON_DEBUG", OsStr::new("1"))],
        &["add", "other", "--", "sh", "-c", &script],
    );
    assert_ok(&out, "add other -- touch in the allow set");
    assert!(extra.path().join("x").is_file());
    assert!(shared.join("y").is_file());
    let text = stderr(&out);
    assert!(
        text.contains("skips [fence] allow entry /"),
        "the root entry must be refused: {text}"
    );
    assert!(
        text.contains("klon: fence: allow the klon"),
        "KLON_DEBUG must list the allowed paths: {text}"
    );
    assert!(
        text.contains("klon: fence: skip"),
        "KLON_DEBUG must list a skipped path: {text}"
    );

    // Golden stays read-only for that klon too.
    let target = r.fx.golden.join("x");
    let out = klon(
        &r.fx.golden,
        &["run", "other", "--", "sh", "-c", &touch(&target)],
    );
    assert_denied(&out, "touch golden/x from the second klon");
    assert!(!target.exists());
}
