//! The `copy` backend strategy, the free-space guard, the progress line, and
//! the background warm (spec §7 C12, R13, R36, R41).
//!
//! Every test builds a fixture, runs the real `gh klon add`, and reads the
//! result out of the klon. Nothing calls a private entry point, so each test
//! exercises the whole transaction.
//!
//! A test that needs a host feature skips with a printed reason. The loop
//! image variant needs `mkfs.ext4` and `udisksctl`; every `udisksctl` call
//! carries `--no-user-interaction`, so a host without the polkit rule fails
//! the call instead of showing a password prompt.

mod common;

use common::{git_ok, klon, klon_env, manifest, stderr, stdout, Fixture};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const SEED: u64 = 12;

/// How long a test waits for the warm process to land every directory.
const LANDING: Duration = Duration::from_secs(60);

/// `SUDO_ASKPASS=/bin/false` turns a password prompt into a failure, so a
/// `sudo` call cannot hang the suite and cannot pass unnoticed (R13).
fn no_sudo() -> Vec<(&'static str, &'static OsStr)> {
    vec![("SUDO_ASKPASS", OsStr::new("/bin/false"))]
}

/// Assert that a command asked for no password.
fn assert_no_sudo_prompt(text: &str) {
    assert!(
        !text.contains("password") && !text.contains("[sudo]"),
        "klon must never ask for a password:\n{text}"
    );
}

/// Write `files` on both branches of the fixture and leave golden on `main`.
/// A klon checks out `feature`, so a setting that only `main` carries would
/// disappear from the klon at the checkout.
fn commit_on_both(fx: &Fixture, files: &[(&str, &str)]) {
    for branch in ["feature", "main"] {
        git_ok(&fx.golden, &["checkout", "-q", branch]);
        for (name, body) in files {
            fs::write(fx.golden.join(name), body).expect("write the fixture file");
        }
        git_ok(&fx.golden, &["add", "-A"]);
        git_ok(&fx.golden, &["commit", "-qm", "klon settings"]);
    }
}

/// A fixture whose ignored `build/` holds 10 000 files, about 1.4 MB. The
/// `.klon.toml` sets a 1 MiB inline limit, so `build/` goes to the warm
/// process and every other entry is copied inline.
fn warm_fixture() -> Fixture {
    let fx = Fixture::generate(SEED, 40, 4, 10_000, 3);
    commit_on_both(&fx, &[(".klon.toml", "[copy]\ninline_limit = \"1M\"\n")]);
    fx
}

/// The `warming` list of an `add --json` document.
fn warming_of(document: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(document)
        .unwrap_or_else(|err| panic!("add --json must be one document: {err}\n{document}"));
    value["warming"]
        .as_array()
        .expect("add --json must hold a warming array")
        .iter()
        .map(|item| {
            item.as_str()
                .expect("a warming entry is a string")
                .to_string()
        })
        .collect()
}

/// Wait until the klon holds every warm directory, or fail after `LANDING`.
fn wait_for_landing(klon_path: &Path, dirs: &[&str]) {
    let marker = klon_path.join(".klon").join("warming.json");
    let started = Instant::now();
    while started.elapsed() < LANDING {
        let landed = dirs.iter().all(|dir| klon_path.join(dir).is_dir());
        if landed && !marker.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "the warm process did not land {dirs:?} in {LANDING:?}; warm.log:\n{}",
        fs::read_to_string(klon_path.join(".klon").join("warm.log")).unwrap_or_default()
    );
}

// --- The warm strategy ----------------------------------------------------------

#[test]
fn add_returns_before_the_big_copy_and_the_manifest_matches_after_it() {
    let fx = warm_fixture();
    // `KLON_TEST_WARM_PAUSE` holds the warm process just before its first
    // rename until the gate file exists, so the window this test proves is
    // deterministic. The detached warm process inherits the variable from this
    // `add` process. The gate file sits beside the fixture, outside golden and
    // outside the klon.
    let gate = fx.golden.parent().unwrap().join("warm-gate");
    let mut envs: Vec<(&str, &OsStr)> = no_sudo();
    envs.push(("KLON_TEST_WARM_PAUSE", gate.as_os_str()));
    let out = klon_env(&fx.golden, &envs, &["--json", "add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_no_sudo_prompt(&stderr(&out));
    let klon_path = fx.default_klon_path();

    // `add` returned while `build/` was still missing, so the report names it
    // and the marker file exists.
    assert_eq!(
        warming_of(&stdout(&out)),
        vec!["build".to_string()],
        "add --json must report the directory the warm process still owes"
    );
    assert!(
        !klon_path.join("build").is_dir(),
        "add must return before the big ignored copy finishes"
    );
    // The tracked checkout is complete, so the klon is usable at once.
    assert!(klon_path.join(fx.tracked_rel(0)).is_file());

    // The warm process fills the staging copy and then waits for the gate.
    // Wait for the staging copy first, so the checks below observe the held
    // window and not the start of the copy.
    let staging = klon_path.join("build.klon-warming");
    let started = Instant::now();
    while !staging.is_dir() {
        assert!(
            started.elapsed() < LANDING,
            "the warm process never created {staging:?}; warm.log:\n{}",
            fs::read_to_string(klon_path.join(".klon").join("warm.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // `list` reports the klon as warming while the rename waits for the gate.
    let listed = klon(&fx.golden, &["list"]);
    assert!(listed.status.success(), "list failed: {}", stderr(&listed));
    assert!(
        stdout(&listed).contains("warming build"),
        "list must show warming while a directory is missing:\n{}",
        stdout(&listed)
    );

    // Write the gate file: the warm process lands the staging copy with its
    // one rename.
    fs::write(&gate, b"").expect("write the warm gate file");

    wait_for_landing(&klon_path, &["build"]);

    // After the rename the ignored directory is golden's, entry for entry.
    assert_eq!(
        manifest(&klon_path.join("build")),
        manifest(&fx.golden.join("build")),
        "the landed ignored directory must equal golden's"
    );
    // The staging directory is gone and the klon is clean: `add` taught git to
    // ignore the staging name, so a half-filled copy never read as untracked.
    assert!(!klon_path.join("build.klon-warming").exists());
    assert_eq!(
        git_ok(&klon_path, &["status", "--porcelain"]),
        "",
        "the klon must be clean after the warm process finished"
    );
    // `list` drops the note once the marker is gone.
    let listed = klon(&fx.golden, &["list"]);
    assert!(
        !stdout(&listed).contains("warming"),
        "list must drop the warming note after the rename:\n{}",
        stdout(&listed)
    );
}

#[test]
fn a_small_ignored_directory_stays_inline() {
    // No `.klon.toml`, so the 64 MiB default limit applies and the two ignored
    // files are far below it. Nothing reaches the warm process.
    let fx = Fixture::generate(SEED + 1, 20, 2, 2, 3);
    let out = klon(&fx.golden, &["--json", "add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(
        warming_of(&stdout(&out)).is_empty(),
        "a small ignored directory must be copied inline"
    );
    let klon_path = fx.default_klon_path();
    assert!(klon_path.join("build").is_dir());
    assert!(!klon_path.join(".klon").join("warming.json").exists());
}

// --- The reinstall strategy -------------------------------------------------------

const PACKAGE_JSON: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "private": true,
  "dependencies": { "leftpad": "file:./vendor/leftpad-1.0.0.tgz" }
}
"#;

/// The absolute path of `name` on PATH, or None with a printed reason.
fn tool(name: &str, test: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    let found = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .chain(extra.iter().cloned())
        .find(|path| path.is_file());
    if found.is_none() {
        println!("skipped: {test}: {name} is not on PATH");
    }
    found
}

/// A deterministic tarball of one tiny package, written into `vendor/`. A
/// tarball dependency lands in the pnpm store, which a `file:` directory
/// dependency does not.
fn pack(golden: &Path) -> bool {
    let stage = golden.join("vendor").join("package");
    fs::create_dir_all(&stage).unwrap();
    fs::write(
        stage.join("package.json"),
        "{ \"name\": \"leftpad\", \"version\": \"1.0.0\", \"main\": \"index.js\" }\n",
    )
    .unwrap();
    fs::write(
        stage.join("index.js"),
        "module.exports = function (s) { return ' ' + s; };\n",
    )
    .unwrap();
    let out = Command::new("tar")
        .current_dir(golden.join("vendor"))
        .args([
            "--mtime=2020-01-01 00:00:00",
            "--sort=name",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-czf",
            "leftpad-1.0.0.tgz",
            "package",
        ])
        .output();
    let packed = out.is_ok_and(|o| o.status.success());
    if packed {
        fs::remove_dir_all(&stage).unwrap();
    }
    packed
}

/// Run `pnpm` in `dir` with a clean, offline environment.
fn pnpm_run(pnpm: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(pnpm)
        .current_dir(dir)
        .args(args)
        .env("CI", "1")
        .env("UV_THREADPOOL_SIZE", "2")
        .output()
        .expect("run pnpm")
}

#[test]
fn a_reinstall_entry_runs_the_command_instead_of_a_copy() {
    let test = "a_reinstall_entry_runs_the_command_instead_of_a_copy";
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let Some(pnpm) = tool("pnpm", test, &[home.join(".local/share/pnpm/pnpm")]) else {
        return;
    };
    if tool("node", test, &[]).is_none() {
        return;
    }
    let fx = Fixture::generate(SEED + 2, 6, 2, 1, 1);
    // `store-dir` is relative, so the store resolves inside whichever tree runs
    // the install. The store is small, so the clone copies it inline and the
    // reinstall command finds it without a network.
    let reinstall = "pnpm install --frozen-lockfile --offline --child-concurrency=1 \
                     && touch .klon-reinstalled";
    commit_on_both(
        &fx,
        &[
            ("package.json", PACKAGE_JSON),
            (".npmrc", "store-dir=.pnpm-store\n"),
            (".gitignore", "/build/\n/node_modules/\n/.pnpm-store/\n"),
            (
                ".klon.toml",
                &format!("[copy]\nreinstall = {{ node_modules = \"{reinstall}\" }}\n"),
            ),
        ],
    );
    if !pack(&fx.golden) {
        println!("skipped: {test}: tar cannot write the fixture package");
        return;
    }
    let warm = pnpm_run(
        &pnpm,
        &fx.golden,
        &["install", "--offline", "--child-concurrency=1"],
    );
    assert!(
        warm.status.success(),
        "the warm install failed: {}{}",
        stdout(&warm),
        stderr(&warm)
    );
    // A real project commits its lock file and its vendored tarball, else
    // `git clean` takes both out of the klon. The add is not forced: a forced
    // add would put `node_modules` in the index, and git would then stop
    // reporting it as an ignored directory.
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "vendor and lock"]);
    git_ok(&fx.golden, &["checkout", "-q", "feature"]);
    git_ok(&fx.golden, &["merge", "-q", "main"]);
    git_ok(&fx.golden, &["checkout", "-q", "main"]);
    // A file that only a copy could bring. Its absence proves the klon's
    // `node_modules` came from the command.
    fs::write(fx.golden.join("node_modules").join(".copied"), "golden\n").unwrap();

    // The approval store belongs to the fixture, never to the user's home.
    let config_home = fx.golden.parent().unwrap().join("config");
    let env = vec![
        ("CI", OsStr::new("1")),
        ("UV_THREADPOOL_SIZE", OsStr::new("2")),
        ("SUDO_ASKPASS", OsStr::new("/bin/false")),
        ("KLON_CONFIG_HOME", config_home.as_os_str()),
    ];
    let out = klon_env(&fx.golden, &env, &["--yes", "--json", "add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_no_sudo_prompt(&stderr(&out));
    assert_eq!(
        warming_of(&stdout(&out)),
        vec!["node_modules".to_string()],
        "a reinstall directory belongs to the warm process"
    );
    let klon_path = fx.default_klon_path();
    wait_for_landing(&klon_path, &["node_modules"]);

    let log = fs::read_to_string(klon_path.join(".klon").join("warm.log")).unwrap_or_default();
    assert!(
        klon_path.join(".klon-reinstalled").is_file(),
        "the approved command must run inside the klon; warm.log:\n{log}"
    );
    assert!(
        klon_path.join("node_modules").is_dir(),
        "the command must produce node_modules; warm.log:\n{log}"
    );
    assert!(
        !klon_path.join("node_modules").join(".copied").exists(),
        "node_modules must come from the command, not from a copy of golden"
    );
}

#[test]
fn a_reinstall_entry_without_approval_refuses_before_any_change() {
    let fx = Fixture::generate(SEED + 3, 8, 2, 1, 1);
    commit_on_both(
        &fx,
        &[(".klon.toml", "[copy]\nreinstall = { build = \"true\" }\n")],
    );
    // No terminal and no `--yes`, so the approval gate refuses.
    let config_home = fx.golden.parent().unwrap().join("config");
    let env = vec![("KLON_CONFIG_HOME", config_home.as_os_str())];
    let out = klon_env(&fx.golden, &env, &["add", "feature"]);
    assert!(!out.status.success(), "add must refuse without approval");
    assert!(
        stderr(&out).contains("needs approval"),
        "the refusal must name the approval gate:\n{}",
        stderr(&out)
    );
    assert!(
        !fx.default_klon_path().exists(),
        "a refused add creates no worktree"
    );
}

// --- The volume hint ---------------------------------------------------------------

#[test]
fn the_volume_hint_appears_once_across_two_adds() {
    let fx = Fixture::generate(SEED + 4, 10, 2, 2, 2);
    let first = klon_env(&fx.golden, &no_sudo(), &["add", "feature"]);
    assert!(first.status.success(), "add failed: {}", stderr(&first));
    git_ok(&fx.golden, &["branch", "second"]);
    let second = klon_env(&fx.golden, &no_sudo(), &["add", "second"]);
    assert!(second.status.success(), "add failed: {}", stderr(&second));

    let hint = "run gh klon init --volume for instant spawns";
    let seen = stderr(&first).matches(hint).count() + stderr(&second).matches(hint).count();
    for out in [&first, &second] {
        assert_no_sudo_prompt(&stderr(out));
    }
    // The hint only fits a byte backend on Linux. macOS has no volume, and a
    // btrfs or reflink host already spawns instantly.
    let backend = stdout(&klon(&fx.golden, &["--json", "doctor"]));
    let copies = cfg!(target_os = "linux") && backend.contains("\"backend\":\"copy\"");
    let expected = usize::from(copies);
    assert_eq!(
        seen,
        expected,
        "the volume hint must appear {expected} time(s):\nfirst:\n{}\nsecond:\n{}",
        stderr(&first),
        stderr(&second)
    );
}

// --- The free-space guard -----------------------------------------------------------

#[test]
fn too_little_free_space_refuses_before_any_change() {
    let fx = Fixture::generate(SEED + 5, 30, 3, 40, 3);
    let mut env = no_sudo();
    // Far below 1.2 times the estimate of a fixture that holds kilobytes.
    env.push(("KLON_TEST_FREE_BYTES", OsStr::new("1024")));
    let out = klon_env(&fx.golden, &env, &["add", "feature"]);

    assert!(!out.status.success(), "add must refuse without the space");
    let text = stderr(&out);
    assert_no_sudo_prompt(&text);
    assert!(
        text.contains("not enough space") && text.contains("short by"),
        "the refusal must name the shortfall:\n{text}"
    );
    let shortfall = text
        .split("short by ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|number| number.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("the shortfall must be a byte count:\n{text}"));
    assert!(shortfall > 0, "the shortfall must be positive:\n{text}");

    // Nothing was created: no directory, and no admin entry.
    assert!(
        !fx.default_klon_path().exists(),
        "a refused add creates no worktree directory"
    );
    let list = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("feature"),
        "a refused add registers no worktree:\n{list}"
    );
}

#[test]
fn enough_free_space_lets_add_through() {
    let fx = Fixture::generate(SEED + 6, 10, 2, 2, 2);
    let mut env = no_sudo();
    env.push(("KLON_TEST_FREE_BYTES", OsStr::new("1000000000")));
    let out = klon_env(&fx.golden, &env, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(fx.default_klon_path().join("build").is_dir());
}

/// `udisksctl` with `--no-user-interaction`: a host without the polkit rule
/// fails the call instead of showing a password prompt.
fn udisks(args: &[&str]) -> Option<Output> {
    Command::new("udisksctl")
        .args(args)
        .arg("--no-user-interaction")
        .output()
        .ok()
}

/// `udisksctl info`, which reads and never changes, so it needs no
/// authorization. It also rejects `--no-user-interaction` with a usage error,
/// which would leave the loop device behind.
fn udisks_info(device: &str) -> String {
    Command::new("udisksctl")
        .args(["info", "-b", device])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// The `/dev/loopN` path in the `loop-setup` answer, which reads
/// `Mapped file <image> as /dev/loop7.`
fn loop_device(text: &str) -> Option<String> {
    let tail = text.rsplit(" as ").next()?;
    Some(tail.trim().trim_end_matches('.').to_string())
}

/// The mount point in the `mount` answer, which reads
/// `Mounted /dev/loop7 at /media/user/label.`
fn mount_point(text: &str) -> Option<PathBuf> {
    let tail = text.rsplit(" at ").next()?;
    Some(PathBuf::from(tail.trim().trim_end_matches('.')))
}

/// Unmount the device, then release the loop only when this user set it up.
/// A loop that another user owns needs an administrator, and asking for one
/// would open the polkit prompt this test must never trigger.
fn release_loop(device: &str) {
    // udev is still settling right after the mount, and the first unmount can
    // answer `DeviceBusy` for a filesystem nothing is using. A few tries
    // settle it; without them the loop device would stay mapped.
    for _ in 0..20 {
        let done = udisks(&["unmount", "-b", device]).is_some_and(|out| out.status.success());
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let me = uid().to_string();
    let mine = udisks_info(device).lines().any(|line| {
        line.split_once("SetupByUID:")
            .is_some_and(|(_, value)| value.trim() == me)
    });
    if mine {
        udisks(&["loop-delete", "-b", device]);
    } else {
        // udisks reports another owner, so a delete would need an
        // administrator and would open the prompt this test must never
        // trigger. The device stays until the next reboot.
        println!("note: {device} was set up by another user; it stays mapped");
    }
}

/// This process's real user id.
fn uid() -> u32 {
    // SAFETY: `getuid` reads one integer and touches no memory of ours.
    unsafe { libc::getuid() }
}

#[test]
fn a_small_real_filesystem_refuses_the_clone() {
    let test = "a_small_real_filesystem_refuses_the_clone";
    if tool("mkfs.ext4", test, &[PathBuf::from("/usr/sbin/mkfs.ext4")]).is_none()
        || tool("udisksctl", test, &[]).is_none()
    {
        return;
    }
    // The fixture and the image both live under $HOME, because a snap `gh` and
    // a confined `udisksd` cannot read a private temporary directory.
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    if !home.is_dir() {
        println!("skipped: {test}: HOME names no directory");
        return;
    }
    let Ok(work) = tempfile::TempDir::new_in(&home) else {
        println!("skipped: {test}: cannot write a working directory under HOME");
        return;
    };
    let fx = Fixture::generate_in(work.path(), SEED + 7, 40, 4, 20, 3);
    // One big ignored file makes the estimate far larger than a small
    // filesystem can hold, whatever the block size of the image turns out to
    // be. The klon backend is `copy` here, so the estimate is these bytes.
    if fs::write(
        fx.golden.join("build").join("big.bin"),
        vec![7u8; 24 * 1024 * 1024],
    )
    .is_err()
    {
        println!("skipped: {test}: cannot write the fixture payload");
        return;
    }

    // `mkfs.ext4 -d` seeds the image from a directory this user owns, so the
    // klon root inside the mount is writable without an administrator.
    let seed = work.path().join("seed");
    fs::create_dir_all(seed.join("klon")).unwrap();
    let image = work.path().join("small.img");
    if fs::write(&image, vec![0u8; 3 * 1024 * 1024]).is_err() {
        println!("skipped: {test}: cannot write the loop image");
        return;
    }
    let made = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(&seed)
        .arg(&image)
        .output();
    if !made.is_ok_and(|out| out.status.success()) {
        println!("skipped: {test}: mkfs.ext4 refused a 3 MiB image");
        return;
    }
    let Some(setup) = udisks(&["loop-setup", "-f", image.to_str().unwrap()]) else {
        println!("skipped: {test}: udisksctl is not usable");
        return;
    };
    if !setup.status.success() {
        println!(
            "skipped: {test}: udisksctl loop-setup refused without a prompt: {}",
            String::from_utf8_lossy(&setup.stderr).trim()
        );
        return;
    }
    let Some(device) = loop_device(&String::from_utf8_lossy(&setup.stdout)) else {
        println!("skipped: {test}: udisksctl named no loop device");
        return;
    };
    let mounted = udisks(&["mount", "-b", &device]);
    let Some(point) = mounted
        .filter(|out| out.status.success())
        .and_then(|out| mount_point(&String::from_utf8_lossy(&out.stdout)))
    else {
        println!("skipped: {test}: udisksctl mount refused without a prompt");
        release_loop(&device);
        return;
    };

    // The klon lands on the small filesystem; golden stays where it is.
    let target = point.join("klon").join("feature");
    let out = klon_env(
        &fx.golden,
        &no_sudo(),
        &["add", "feature", "--path", target.to_str().unwrap()],
    );
    let text = stderr(&out);
    let refused = !out.status.success();
    let named = text.contains("not enough space") && text.contains("short by");
    let created = target.exists();
    // The release runs before any assertion and before the fixture goes away:
    // udisks loses the setup owner of a loop whose backing file is gone, and
    // the cleanup would then refuse to release the device.
    release_loop(&device);

    assert_no_sudo_prompt(&text);
    assert!(refused, "a 3 MiB filesystem must refuse the clone:\n{text}");
    assert!(named, "the refusal must name the shortfall:\n{text}");
    assert!(!created, "a refused add creates no worktree directory");
}

// --- The progress line ---------------------------------------------------------------

/// Run `gh-klon <args>` under a pseudo-terminal and answer what it wrote.
/// `script` gives the child a terminal on both streams and copies everything
/// it writes to its own stdout.
///
/// The two `script` programs take their command differently. util-linux reads
/// one command string after `-c` and names the typescript file last; the BSD
/// one on macOS names the typescript file first and takes the command as plain
/// argument words. The BSD one has neither `-c` nor `-f`.
fn under_pty(dir: &Path, args: &[&str]) -> String {
    let mut command = Command::new("script");
    let line = format!("{} {}", common::BIN, args.join(" "));
    if cfg!(target_os = "macos") {
        command
            .arg("-q")
            .arg("/dev/null")
            .arg(common::BIN)
            .args(args);
    } else {
        command.args(["-qfc", &line, "/dev/null"]);
    }
    let out = command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_SPARE", "0")
        .env("SUDO_ASKPASS", "/bin/false")
        .output()
        .expect("run script");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_pseudo_terminal_shows_the_progress_line_and_json_shows_none() {
    let test = "a_pseudo_terminal_shows_the_progress_line_and_json_shows_none";
    if tool("script", test, &[]).is_none() {
        return;
    }
    let fx = Fixture::generate(SEED + 8, 60, 3, 20, 3);
    git_ok(&fx.golden, &["branch", "second"]);

    let plain = under_pty(&fx.golden, &["add", "feature"]);
    assert_no_sudo_prompt(&plain);
    assert!(
        plain.contains("klon: copied") && plain.contains("files remaining"),
        "a terminal must show the progress line:\n{plain}"
    );
    assert!(
        fx.default_klon_path().is_dir(),
        "the pty run must still create the klon:\n{plain}"
    );

    let document = under_pty(&fx.golden, &["--json", "add", "second"]);
    assert!(
        !document.contains("klon: copied"),
        "--json must suppress the progress line:\n{document}"
    );
    assert!(
        document.contains("\"schema\""),
        "--json must still print its document:\n{document}"
    );
}

#[test]
fn a_pipe_shows_no_progress_line() {
    let fx = Fixture::generate(SEED + 9, 20, 2, 4, 2);
    let out = klon_env(&fx.golden, &no_sudo(), &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(
        !stderr(&out).contains("klon: copied"),
        "a pipe must show no progress line:\n{}",
        stderr(&out)
    );
}

#[test]
fn the_forced_progress_line_reaches_the_announced_total() {
    let fx = Fixture::generate(SEED + 10, 20, 2, 4, 2);
    let mut env = no_sudo();
    env.push(("KLON_PROGRESS", OsStr::new("1")));
    let out = klon_env(&fx.golden, &env, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let text = stderr(&out);
    let last = text
        .rsplit('\r')
        .find(|part| part.contains("klon: copied"))
        .unwrap_or_else(|| panic!("the forced line must appear:\n{text}"));
    assert!(
        last.contains("0 files remaining"),
        "the line must count every inline file down to zero:\n{last}"
    );
    assert!(
        text.ends_with('\n'),
        "the progress line must end with a newline:\n{text:?}"
    );
}

#[test]
fn a_directory_the_branch_does_not_ignore_never_lands() {
    // Golden ignores `staging/`, `feature` does not. The plan is made from
    // golden's rules, so the directory reaches the warm process; the klon must
    // still refuse it, else `add` would hand back a dirty klon.
    let fx = Fixture::generate(SEED + 11, 20, 2, 2, 2);
    git_ok(&fx.golden, &["checkout", "-q", "main"]);
    fs::write(fx.golden.join(".gitignore"), "/build/\n/staging/\n").unwrap();
    fs::write(
        fx.golden.join(".klon.toml"),
        "[copy]\ninline_limit = \"1K\"\n",
    )
    .unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(
        &fx.golden,
        &["commit", "-qm", "ignore staging on main only"],
    );
    let staging = fx.golden.join("staging");
    fs::create_dir(&staging).unwrap();
    for i in 0..40 {
        fs::write(staging.join(format!("o{i}.bin")), vec![b'z'; 2048]).unwrap();
    }

    let out = klon_env(&fx.golden, &no_sudo(), &["--json", "add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(
        warming_of(&stdout(&out)),
        vec!["staging".to_string()],
        "golden's rules put the directory in the plan"
    );
    let klon_path = fx.default_klon_path();
    let log = klon_path.join(".klon").join("warm.log");
    let started = Instant::now();
    while started.elapsed() < LANDING {
        if fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("not ignored")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        text.contains("staging is not ignored on this branch"),
        "the warm process must refuse a directory this branch does not ignore:\n{text}"
    );
    assert!(
        !klon_path.join("staging").exists(),
        "the klon must stay clean of a directory its branch does not ignore"
    );
    assert!(!klon_path.join("staging.klon-warming").exists());
    assert_eq!(
        git_ok(&klon_path, &["status", "--porcelain"]),
        "",
        "the klon must be clean"
    );
    // The step failed, so it stays pending and `list` keeps saying so.
    assert_eq!(warm_pending(&klon_path), vec!["staging".to_string()]);
    let listed = klon(&fx.golden, &["list"]);
    assert!(
        stdout(&listed).contains("warming staging"),
        "a step that failed must stay visible:\n{}",
        stdout(&listed)
    );
}

/// The `pending` list of `<klon>/.klon/warming.json`, or an empty list.
fn warm_pending(klon_path: &Path) -> Vec<String> {
    let text = match fs::read_to_string(klon_path.join(".klon").join("warming.json")) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("the marker is JSON");
    value["pending"]
        .as_array()
        .expect("pending is an array")
        .iter()
        .map(|item| item.as_str().expect("a name").to_string())
        .collect()
}

#[test]
fn no_fixup_reaches_the_warm_process() {
    // The warm directory names golden. Without `--no-fixup` the pass rewrites
    // that name; with it the klon keeps golden's path, and no log is written.
    let fx = warm_fixture();
    fs::write(
        fx.golden.join("build").join("path.txt"),
        format!("{}\n", fx.golden.display()),
    )
    .unwrap();
    let out = klon_env(&fx.golden, &no_sudo(), &["add", "feature", "--no-fixup"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon_path = fx.default_klon_path();
    wait_for_landing(&klon_path, &["build"]);

    let text = fs::read_to_string(klon_path.join("build").join("path.txt")).expect("read");
    assert_eq!(
        text.trim(),
        fx.golden.display().to_string(),
        "--no-fixup must reach the warm process and leave golden's path alone"
    );
    assert!(
        !klon_path.join(".klon").join("fixup.log").exists(),
        "a skipped pass writes no log"
    );
}

#[test]
fn a_fast_backend_neither_surveys_nor_warms() {
    // `--backend reflink-walk` shares blocks, so the estimate is zero and the
    // guard has nothing to weigh. A free-space reading far below the tree must
    // therefore let the klon through, and nothing may reach the warm process.
    let fx = warm_fixture();
    let mut env = no_sudo();
    env.push(("KLON_TEST_FREE_BYTES", OsStr::new("1")));
    let out = klon_env(
        &fx.golden,
        &env,
        &["--json", "add", "feature", "--backend", "reflink-walk"],
    );
    if !out.status.success() {
        // ext4 has no `FICLONE`, so the clone itself may refuse. The guard
        // must not be the reason.
        let text = stderr(&out);
        assert!(
            !text.contains("not enough space"),
            "a block-sharing backend must not meet the free-space guard:\n{text}"
        );
        println!(
            "skipped: a_fast_backend_neither_surveys_nor_warms: reflink-walk cannot clone here"
        );
        return;
    }
    assert!(
        warming_of(&stdout(&out)).is_empty(),
        "only the copy backend warms"
    );
    assert!(fx.default_klon_path().join("build").is_dir());
}

#[test]
fn the_estimate_counts_disk_blocks_not_content_bytes() {
    // 3000 one-byte ignored files hold 3 KB of content and need megabytes of
    // blocks and inodes. A guard that weighed content would let this through.
    let fx = Fixture::generate(SEED + 12, 10, 2, 0, 2);
    let build = fx.golden.join("build");
    for i in 0..3000 {
        fs::write(build.join(format!("t{i}")), b"x").unwrap();
    }
    let mut env = no_sudo();
    // Far above 1.2 times the content bytes, far below the blocks they need.
    env.push(("KLON_TEST_FREE_BYTES", OsStr::new("200000")));
    let out = klon_env(&fx.golden, &env, &["add", "feature"]);
    let text = stderr(&out);
    assert!(
        !out.status.success() && text.contains("not enough space"),
        "the guard must weigh the blocks a tiny-file tree needs:\n{text}"
    );
    assert!(!fx.default_klon_path().exists());
}
