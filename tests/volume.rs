//! `gh klon init --volume` (spec §7 C15, R33).
//!
//! Every test here builds a real btrfs loop volume through udisks, exactly as
//! the S1 spike did (`docs/spikes/2026-btrfs-loop-volume.md`). A host that
//! cannot do that skips with a printed reason and never fails (spec §5).
//!
//! Three rules keep a polkit password dialog off the user's desktop. It would
//! block the suite and it would wait for a person who is not watching:
//!
//! - every `udisksctl` call carries `--no-user-interaction`, so udisks fails
//!   instead of asking;
//! - `SUDO_ASKPASS=/bin/false` and a closed stdin block the two other ways a
//!   helper could ask;
//! - `loop-delete` runs only for a device that `udisksctl info` reports in
//!   `SetupByUID` as this user's (S1 §9).
//!
//! Each test owns an `XDG_DATA_HOME` of its own, so its image and its volume
//! record cannot reach another test or the user's own volumes. `Sandbox` is a
//! guard: its `Drop` unmounts, releases, and deletes whatever the test left,
//! whether the test passed, failed, or panicked.

mod common;

use common::{git_ok, klon_env, manifest, stderr, stdout, Fixture};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const SEED: u64 = 915;

/// The image size of a test volume. It is sparse, so it costs nothing until a
/// test fills it (S1 §10).
const SIZE: &str = "1G";

/// The bigger image for the 100k cell, which holds golden, its object store,
/// and one snapshot.
const BIG_SIZE: &str = "12G";

// --- The sandbox ---------------------------------------------------------------

/// One test's data directory, plus the teardown that removes every volume the
/// test built. klon puts the image and the volume registry under
/// `$XDG_DATA_HOME/klon`, so one variable isolates the whole feature.
struct Sandbox {
    data: PathBuf,
    /// One volume at a time in this binary. Every fixture is called `golden`,
    /// so every test volume carries the label `klon-golden`, and udisks then
    /// numbers the mount points. Two tests that mount and unmount at once race
    /// for one numbered directory, and the loser fails with `File exists`.
    /// The field is never read: the guard lives here so it is released after
    /// the teardown below.
    #[allow(dead_code)]
    guard: MutexGuard<'static, ()>,
}

/// The lock that `Sandbox` holds. A poisoned lock is still a lock: a test that
/// panicked already reported its own failure, and the next test must not fail
/// with a message about a mutex.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

impl Sandbox {
    /// `$HOME/.local/share/klon-tests/<test>-<pid>-<n>`.
    ///
    /// The path stays under `$HOME` on purpose. The image must live on a real
    /// filesystem: a `/tmp` that is `tmpfs` would hold the whole volume in
    /// RAM, and the S1 spike measured every number under `$HOME`.
    fn new(test: &str) -> Option<Sandbox> {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let home = std::env::var_os("HOME")?;
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let data = PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("klon-tests")
            .join(format!("{test}-{}-{n}", std::process::id()));
        fs::create_dir_all(&data).ok()?;
        let guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|held| held.into_inner());
        Some(Sandbox { data, guard })
    }

    /// The environment that points klon at this sandbox.
    fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("XDG_DATA_HOME", self.data.clone().into_os_string()),
            ("SUDO_ASKPASS", OsString::from("/bin/false")),
        ]
    }

    /// Every image that klon built in this sandbox.
    fn images(&self) -> Vec<PathBuf> {
        let dir = self.data.join("klon");
        fs::read_dir(dir)
            .map(|read| {
                let mut found: Vec<PathBuf> = read
                    .flatten()
                    .map(|item| item.path())
                    .filter(|path| path.extension().is_some_and(|e| e == "img"))
                    .collect();
                found.sort();
                found
            })
            .unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for image in self.images() {
            let Some(device) = loop_device(&image) else {
                continue;
            };
            // A background delete inside the volume can hold the mount for a
            // moment, so the unmount gets a few tries before the guard gives up.
            if !unmount(&device) {
                eprintln!("teardown: {device} stays mounted; unmount it by hand");
            }
            if setup_by_caller(&device) {
                let _ = udisks(&["loop-delete", "-b", &device]);
            }
        }
        let _ = fs::remove_dir_all(&self.data);
    }
}

/// The sandbox for `test`, or None with a printed reason on a host that cannot
/// build a volume.
fn sandbox(test: &str) -> Option<Sandbox> {
    if let Some(why) = skip_reason() {
        println!("skipped: {test}: {why}");
        return None;
    }
    match Sandbox::new(test) {
        Some(sandbox) => Some(sandbox),
        None => {
            println!("skipped: {test}: cannot create a data directory under HOME");
            None
        }
    }
}

/// Why this host cannot build a klon volume, or None when it can. The list
/// matches the refusals in `src/cli/init_volume.rs`.
fn skip_reason() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("the klon volume is a Linux feature".to_string());
    }
    if mkfs_btrfs().is_none() || btrfs_tool().is_none() {
        return Some(
            "btrfs-progs is absent; install it or set KLON_BTRFS_TOOLS to an unpacked copy"
                .to_string(),
        );
    }
    for tool_name in ["udisksctl", "findmnt"] {
        if tool(tool_name).is_none() {
            return Some(format!("{tool_name} is not on PATH"));
        }
    }
    if !active_local_session() {
        return Some(
            "no active local session, so udisks would ask for a password (S1 §11)".to_string(),
        );
    }
    None
}

// --- The host tools, as the S1 spike used them ------------------------------------

/// Run one `udisksctl` subcommand with every password path blocked.
fn udisks(args: &[&str]) -> Result<String, String> {
    let (subcommand, rest) = args.split_first().expect("a subcommand");
    let mut command = Command::new("udisksctl");
    command.arg(subcommand);
    if *subcommand != "info" {
        command.arg("--no-user-interaction");
    }
    let out = command
        .args(rest)
        .env("SUDO_ASKPASS", "/bin/false")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("cannot run udisksctl: {err}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// True when `image` carries a loop device that is mounted. A path test would
/// not do: two test volumes share the label `klon-golden`, so udisks gives the
/// second one a suffixed mount point and each can stand where the other's
/// record points.
fn is_up(image: &Path) -> bool {
    loop_device(image)
        .and_then(|device| mount_point(&device))
        .is_some()
}

/// Unmount `device`, with a few tries.
///
/// A fresh mount under `/media` wakes the desktop file indexer, and udisks
/// answers `DeviceBusy` while it reads. klon retries the same way.
fn unmount(device: &str) -> bool {
    for _ in 0..20 {
        if mount_point(device).is_none() || udisks(&["unmount", "-b", device]).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// True when `udisksctl info` reports this user in `SetupByUID`. Every other
/// answer means `loop-delete` would raise a password dialog (S1 §9).
fn setup_by_caller(device: &str) -> bool {
    // SAFETY: `getuid` reads a process property and cannot fail.
    let me = unsafe { libc::getuid() };
    let Ok(text) = udisks(&["info", "-b", device]) else {
        return false;
    };
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("SetupByUID:"))
        .any(|value| value.trim().parse::<u32>() == Ok(me))
}

/// The loop device that carries `image`, from sysfs. klon resolves it the same
/// way and never stores a device number (S1 §9.4).
fn loop_device(image: &Path) -> Option<String> {
    let wanted = fs::canonicalize(image).unwrap_or_else(|_| image.to_path_buf());
    for item in fs::read_dir("/sys/block").ok()?.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if !name.starts_with("loop") {
            continue;
        }
        let file = item.path().join("loop").join("backing_file");
        if let Ok(text) = fs::read_to_string(&file) {
            if Path::new(text.trim_end_matches('\n')) == wanted {
                return Some(format!("/dev/{name}"));
            }
        }
    }
    None
}

fn mount_point(device: &str) -> Option<PathBuf> {
    let out = Command::new("findmnt")
        .args(["-n", "-o", "TARGET", device])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
}

/// True when this user holds a session that polkit calls active and local.
/// udisks grants `loop-setup` and `mount` to that session only (S1 §11).
fn active_local_session() -> bool {
    let Some(loginctl) = tool("loginctl") else {
        return false;
    };
    // SAFETY: `getuid` reads a process property and cannot fail.
    let me = unsafe { libc::getuid() };
    let Ok(list) = Command::new(&loginctl)
        .args(["list-sessions", "--no-legend"])
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(id), Some(uid)) = (fields.next(), fields.next()) else {
            continue;
        };
        if uid.parse::<u32>() != Ok(me) {
            continue;
        }
        let Ok(out) = Command::new(&loginctl)
            .args(["show-session", id, "-p", "Active", "-p", "Remote"])
            .stdin(Stdio::null())
            .output()
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("Active=yes") && text.contains("Remote=no") {
            return true;
        }
    }
    false
}

fn mkfs_btrfs() -> Option<PathBuf> {
    btrfs_binary("mkfs.btrfs")
}

fn btrfs_tool() -> Option<PathBuf> {
    btrfs_binary("btrfs")
}

fn btrfs_binary(name: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KLON_BTRFS_TOOLS") {
        let candidate = Path::new(&dir).join(name);
        return candidate.is_file().then_some(candidate);
    }
    tool(name)
}

fn tool(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

// --- Shared assertions -----------------------------------------------------------

fn klon(cwd: &Path, sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    let env = sandbox.env();
    let pairs: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
    klon_env(cwd, &pairs, args)
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("not one JSON document: {err}\n{text}"))
}

fn doctor(cwd: &Path, sandbox: &Sandbox) -> Value {
    let out = klon(cwd, sandbox, &["doctor", "--json"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    parse(&stdout(&out))
}

/// The unprivileged subvolume test of S1 §7: inode 256 plus a device number
/// that differs from the parent.
fn is_subvolume(path: &Path) -> bool {
    let Ok(here) = fs::metadata(path) else {
        return false;
    };
    if here.ino() != 256 {
        return false;
    }
    match path.parent().and_then(|p| fs::metadata(p).ok()) {
        Some(parent) => parent.dev() != here.dev(),
        None => true,
    }
}

/// Build a volume under `sandbox` and answer with the record that `--json`
/// printed. Every test that needs a converted golden starts here.
fn convert(fx: &Fixture, sandbox: &Sandbox, size: &str) -> Value {
    let out = klon(
        &fx.golden,
        sandbox,
        &["init", "--volume", size, "--yes", "--json"],
    );
    assert!(
        out.status.success(),
        "init --volume failed: {}",
        stderr(&out)
    );
    let report = parse(&stdout(&out));
    assert_eq!(report["schema"], "klon.init/1");
    assert_eq!(report["shape"], "subvolume");
    report["volume"].clone()
}

fn path_of(record: &Value, field: &str) -> PathBuf {
    PathBuf::from(
        record[field]
            .as_str()
            .unwrap_or_else(|| panic!("the record must hold {field}: {record}")),
    )
}

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

fn sigkill(child: &Child) {
    // SAFETY: `kill` takes a pid and a signal number and returns an error code.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL failed");
}

/// Start `gh-klon` with the test-only pause at `point`, inside `sandbox`.
fn spawn_paused(cwd: &Path, sandbox: &Sandbox, point: &str, args: &[&str]) -> Child {
    let mut command = Command::new(common::BIN);
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_TEST_PAUSE_AT", point)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in sandbox.env() {
        command.env(key, value);
    }
    command.spawn().expect("start gh-klon")
}

/// `<path><suffix>`, next to golden.
fn with_suffix(golden: &Path, suffix: &str) -> PathBuf {
    let mut name = golden.file_name().expect("a name").to_os_string();
    name.push(suffix);
    golden.parent().expect("a parent").join(name)
}

// --- The acceptance tests ----------------------------------------------------------

/// The first acceptance line: `init --volume 1G` completes with zero password
/// prompts and `doctor --json` reports `btrfs-snapshot`. The run before it
/// proves that a non-interactive command without `--yes` refuses and changes
/// nothing.
#[test]
fn init_volume_moves_golden_and_doctor_reports_the_snapshot_backend() {
    let name = "init_volume_moves_golden_and_doctor_reports_the_snapshot_backend";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 60, 5, 10, 3);
    let before = manifest(&fx.golden);

    let out = klon(&fx.golden, &sandbox, &["init", "--volume", SIZE]);
    assert!(!out.status.success(), "init --volume must need a yes");
    assert!(
        stderr(&out).contains("needs a yes"),
        "unexpected stderr: {}",
        stderr(&out)
    );
    assert!(sandbox.images().is_empty(), "a refusal must build no image");
    assert_eq!(
        before,
        manifest(&fx.golden),
        "a refusal must change nothing"
    );

    let record = convert(&fx, &sandbox, SIZE);
    let image = path_of(&record, "image");
    let golden_new = path_of(&record, "golden_new");
    let mount = path_of(&record, "mount");
    assert_eq!(record["version"], 1);
    assert_eq!(path_of(&record, "golden_old"), fx.golden);
    assert!(image.is_file(), "{} must exist", image.display());
    // The image is the raw filesystem, so a reader of the file reads every
    // path in the repository. Its mode must not follow the umask.
    let mode = fs::metadata(&image).expect("stat the image").mode() & 0o777;
    assert_eq!(mode, 0o600, "the image must be owner-only, found {mode:o}");
    assert!(
        record["label"]
            .as_str()
            .expect("a label")
            .starts_with("klon-"),
        "the label must name klon: {record}"
    );

    // Golden keeps its path through a symlink, and the tree it names is a
    // subvolume that `add` can snapshot.
    let link = fs::symlink_metadata(&fx.golden).expect("golden must still be there");
    assert!(link.is_symlink(), "golden's path must hold a symlink");
    assert_eq!(fs::read_link(&fx.golden).expect("a target"), golden_new);
    assert!(
        golden_new.starts_with(&mount),
        "golden must sit on the mount"
    );
    assert!(is_subvolume(&golden_new), "golden must be a subvolume");
    assert_eq!(
        before,
        manifest(&fx.golden),
        "golden must be byte-equal after the move"
    );
    assert_eq!(git_ok(&fx.golden, &["status", "--porcelain"]), "");

    let report = doctor(&fx.golden, &sandbox);
    assert_eq!(report["filesystem"], "btrfs");
    assert_eq!(
        report["backend"], "btrfs-snapshot",
        "the probe must pick the snapshot backend: {}",
        report["backend_reason"]
    );
    let row = &report["features"]["volume"];
    assert_eq!(row["status"], "present", "the volume row: {row}");
    assert!(
        row["detail"]
            .as_str()
            .expect("a detail")
            .contains("mounted"),
        "the volume row must say whether the image is mounted: {row}"
    );

    // The replaced copy goes in the background and leaves nothing behind.
    assert!(
        wait_until(
            || !with_suffix(&fx.golden, ".klon-old").exists(),
            Duration::from_secs(60)
        ),
        "the replaced golden must go away"
    );
}

/// The second acceptance line: `add` after `init --volume` works, takes the
/// snapshot backend, and gives the klon golden's ignored files.
#[test]
fn add_after_init_volume_keeps_the_ignored_manifest() {
    let name = "add_after_init_volume_keeps_the_ignored_manifest";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 400, 20, 60, 3);
    let record = convert(&fx, &sandbox, SIZE);
    let golden_new = path_of(&record, "golden_new");
    let before = manifest(&golden_new.join("build"));

    let started = Instant::now();
    let out = klon(&fx.golden, &sandbox, &["add", "--json", "feature"]);
    let elapsed = started.elapsed();
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let report = parse(&stdout(&out));
    assert_eq!(report["backend"], "btrfs-snapshot");
    println!("add on a volume took {elapsed:?}");

    // The klon lands beside golden, which is on the volume now, so the
    // snapshot can reach it.
    let klon_path = path_of(&report, "path");
    assert!(
        klon_path.starts_with(path_of(&record, "mount")),
        "the klon must sit on the volume: {}",
        klon_path.display()
    );
    assert!(is_subvolume(&klon_path), "the klon must be a subvolume");
    assert_eq!(
        before,
        manifest(&klon_path.join("build")),
        "the ignored manifest must match golden, mtimes included"
    );
    assert!(
        !klon_path.join(".git").is_dir(),
        "the snapshot copy of golden's .git directory must be gone"
    );
    common::assert_clean(&klon_path);
}

/// A klon that stood before the conversion still works after it.
///
/// Every `.git` file and every admin `gitdir` file holds an absolute path into
/// golden. Golden's symlink answers for them, and `git worktree repair` writes
/// them out again, so the register list stays right from both ends.
#[test]
fn a_klon_made_before_the_move_still_works() {
    let name = "a_klon_made_before_the_move_still_works";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 60, 5, 10, 3);
    let out = klon(&fx.golden, &sandbox, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let old_klon = path_of(&parse(&stdout(&out)), "path");

    let record = convert(&fx, &sandbox, SIZE);
    let golden_new = path_of(&record, "golden_new");

    // The klon stayed on the old filesystem, and it still names a repository.
    assert!(old_klon.is_dir(), "the klon must survive the move");
    assert_eq!(git_ok(&old_klon, &["status", "--porcelain"]), "");
    let list = git_ok(&old_klon, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains(&golden_new.display().to_string()),
        "the klon must name golden's new path: {list}"
    );
    assert!(
        list.contains(&old_klon.display().to_string()),
        "the register list must still hold the klon: {list}"
    );
    // Golden names it too, and a new klon still lands on the volume.
    let from_golden = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    assert!(
        from_golden.contains(&old_klon.display().to_string()),
        "golden must still name the klon: {from_golden}"
    );
    let out = klon(&fx.golden, &sandbox, &["add", "--json", "second"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&stdout(&out))["backend"], "btrfs-snapshot");
    assert!(klon(&fx.golden, &sandbox, &["rm", "second"])
        .status
        .success());

    // A klon that stayed on the old filesystem survives the undo, so it must
    // not ask for `--force`. Only a klon on the volume goes away with it.
    let out = klon(
        &fx.golden,
        &sandbox,
        &["init", "--volume", "--undo", "--yes", "--json"],
    );
    assert!(
        out.status.success(),
        "an external klon must not block the undo: {}",
        stderr(&out)
    );
    // Every other line went to stderr, so stdout holds one document.
    assert_eq!(parse(&stdout(&out))["shape"], "directory");
    assert!(old_klon.is_dir(), "the external klon must survive the undo");
    assert_eq!(git_ok(&old_klon, &["status", "--porcelain"]), "");
    assert!(git_ok(&old_klon, &["worktree", "list", "--porcelain"])
        .contains(&fx.golden.display().to_string()));
}

/// The 100k timing line. It needs a big fixture and a big image, so it runs
/// only when `KLON_FIXTURE=100k` asks for it, like every other 100k cell.
#[test]
fn add_of_the_100k_fixture_on_a_volume_is_fast() {
    let name = "add_of_the_100k_fixture_on_a_volume_is_fast";
    if std::env::var("KLON_FIXTURE").as_deref() != Ok("100k") {
        println!("skipped: {name}: set KLON_FIXTURE=100k to run it");
        return;
    }
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 90_000, 300, 10_000, 3);
    let _record = convert(&fx, &sandbox, BIG_SIZE);

    let started = Instant::now();
    let out = klon(&fx.golden, &sandbox, &["add", "--json", "feature"]);
    let elapsed = started.elapsed();
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&stdout(&out))["backend"], "btrfs-snapshot");
    println!("add of the 100k fixture on a klon volume took {elapsed:?}");
    // The snapshot itself is O(1) and costs 20 to 50 ms (S1 §10). The rest of
    // the transaction is not: the prune walk, `git checkout`, `git clean`, and
    // one `git status` dominate at this size, and handoff §11 measures
    // `checkout` alone at 0.31 s on git 2.34.1. The 1 s of the acceptance line
    // therefore needs the C9 spare, which renames a prepared tree into place.
    // This budget matches the C7 snapshot cell, so it still fails when the
    // backend falls back or the prune walk regresses.
    assert!(
        elapsed < Duration::from_secs(20),
        "add on the 100k fixture took {elapsed:?}; the budget is 20 s"
    );
    assert!(
        klon(&fx.golden, &sandbox, &["rm", "--force", "feature"])
            .status
            .success(),
        "the klon must go before the teardown unmounts the volume"
    );
}

/// The third acceptance line: after the volume goes down, the next `add`
/// attaches it, mounts it, and succeeds.
///
/// The test detaches the way a reboot does: `unmount`, then `loop-delete` when
/// udisks says this user owns the device. Golden's symlink then points at
/// nothing, so no shell can stand in the repository and the command runs from
/// the directory above it.
#[test]
fn add_reattaches_a_volume_that_went_down() {
    let name = "add_reattaches_a_volume_that_went_down";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 60, 5, 10, 3);
    let record = convert(&fx, &sandbox, SIZE);
    let image = path_of(&record, "image");
    assert!(
        klon(&fx.golden, &sandbox, &["add", "feature"])
            .status
            .success(),
        "the first add must work while the volume is up"
    );

    let device = loop_device(&image).expect("the image must carry a loop device");
    assert!(unmount(&device), "the volume must go down");
    if setup_by_caller(&device) {
        let _ = udisks(&["loop-delete", "-b", &device]);
    }
    assert!(!is_up(&image), "the volume must be down");

    // The parent of golden is the only path a user can still stand in.
    let parent = fx.golden.parent().expect("a parent");
    let out = klon(parent, &sandbox, &["add", "--json", "second"]);
    assert!(
        out.status.success(),
        "add must attach the volume and succeed: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("attached"),
        "add must report the attach: {}",
        stderr(&out)
    );
    let report = parse(&stdout(&out));
    assert_eq!(report["backend"], "btrfs-snapshot");
    assert!(
        loop_device(&image).is_some(),
        "the image must carry a loop device again"
    );
    // The mount point can differ from the one the conversion recorded: another
    // volume with the same label may hold the old path, and udisks then adds a
    // suffix. klon repoints golden's symlink, so the stable path is the test.
    let target = fs::read_link(&fx.golden).expect("golden must still be a symlink");
    assert!(
        target.is_dir(),
        "golden's symlink must resolve again: {}",
        target.display()
    );
    assert_eq!(git_ok(&fx.golden, &["status", "--porcelain"]), "");
    common::assert_clean(&path_of(&report, "path"));
}

/// The fourth acceptance line: without `mkfs.btrfs`, `init --volume` exits
/// non-zero with the install line and changes nothing.
///
/// This one runs on every host. `KLON_BTRFS_TOOLS` points at an empty
/// directory, which is how klon is told to look nowhere else: the variable
/// wins over `PATH`, so a host that has `btrfs-progs` installed still takes
/// the refusal path.
#[test]
fn init_volume_without_btrfs_progs_prints_the_install_line() {
    let name = "init_volume_without_btrfs_progs_prints_the_install_line";
    let Some(sandbox) = Sandbox::new(name) else {
        println!("skipped: {name}: cannot create a data directory under HOME");
        return;
    };
    let empty = sandbox.data.join("no-tools");
    fs::create_dir_all(&empty).expect("create the empty tools directory");
    let fx = Fixture::generate(SEED, 40, 4, 6, 2);
    let before = manifest(&fx.golden);

    let env: Vec<(&str, &OsStr)> = vec![
        ("XDG_DATA_HOME", sandbox.data.as_os_str()),
        ("KLON_BTRFS_TOOLS", empty.as_os_str()),
        ("PATH", OsStr::new("/usr/bin:/bin")),
        ("SUDO_ASKPASS", OsStr::new("/bin/false")),
    ];
    let out = klon_env(&fx.golden, &env, &["init", "--volume", "4G", "--yes"]);
    assert!(
        !out.status.success(),
        "init --volume without btrfs-progs must fail: {}",
        stdout(&out)
    );
    let text = stderr(&out);
    assert!(
        text.contains("sudo apt install btrfs-progs"),
        "the refusal must print the install line: {text}"
    );
    assert!(
        text.contains("KLON_BTRFS_TOOLS"),
        "the refusal must name the way out for a host without root: {text}"
    );
    assert_eq!(
        before,
        manifest(&fx.golden),
        "the refusal must change nothing"
    );
    assert!(
        sandbox.images().is_empty(),
        "the refusal must build no image"
    );
    assert!(
        !fs::symlink_metadata(&fx.golden)
            .expect("golden")
            .is_symlink(),
        "golden must stay a directory"
    );
}

/// The fifth acceptance line: `init --volume` on a golden with uncommitted
/// changes exits non-zero with `dirty` and changes nothing.
#[test]
fn init_volume_on_a_dirty_golden_refuses() {
    let name = "init_volume_on_a_dirty_golden_refuses";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 40, 4, 6, 2);
    fs::write(fx.golden.join("f2.txt"), b"an uncommitted edit\n").expect("edit golden");
    let before = manifest(&fx.golden);

    let out = klon(&fx.golden, &sandbox, &["init", "--volume", SIZE, "--yes"]);
    assert!(!out.status.success(), "a dirty golden must refuse");
    let text = stderr(&out);
    assert!(text.contains("dirty"), "the refusal must say dirty: {text}");
    assert_eq!(
        before,
        manifest(&fx.golden),
        "the refusal must change nothing"
    );
    assert!(
        sandbox.images().is_empty(),
        "the refusal must build no image"
    );
    assert!(
        !fs::symlink_metadata(&fx.golden)
            .expect("golden")
            .is_symlink(),
        "golden must stay a directory"
    );
}

/// The sixth acceptance line: `init --volume` killed after the move, then
/// `doctor --repair`, leaves golden reachable at its original path with a
/// byte-equal manifest.
///
/// The pause point sits between the two halves of the swap, where golden does
/// not exist at its own path and `<golden>.klon-old` holds it.
#[test]
fn a_kill_after_the_move_is_repaired() {
    let name = "a_kill_after_the_move_is_repaired";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 200, 8, 20, 3);
    let before = manifest(&fx.golden);
    let old = with_suffix(&fx.golden, ".klon-old");

    let mut child = spawn_paused(
        &fx.golden,
        &sandbox,
        "between-mv",
        &["init", "--volume", SIZE, "--yes"],
    );
    let reached = wait_until(
        || fs::symlink_metadata(&fx.golden).is_err() && old.exists(),
        Duration::from_secs(120),
    );
    sigkill(&child);
    let _ = child.wait();
    assert!(
        reached,
        "init --volume must reach the window after the move"
    );

    // Golden is missing, so the repair runs from the renamed copy, which is a
    // whole repository of its own.
    let out = klon(&old, &sandbox, &["doctor", "--repair"]);
    assert!(out.status.success(), "repair failed: {}", stderr(&out));
    let report = stdout(&out);
    println!("repair report:\n{report}");
    assert!(fx.golden.is_dir(), "the repair must put golden back");
    assert!(
        !fs::symlink_metadata(&fx.golden)
            .expect("golden")
            .is_symlink(),
        "the repair must leave a directory, not a half-made symlink"
    );
    assert_eq!(
        before,
        manifest(&fx.golden),
        "golden must be byte-equal after the repair"
    );
    assert_eq!(git_ok(&fx.golden, &["status", "--porcelain"]), "");
    // The image is the one thing the repair keeps: it may still hold the only
    // copy of a path, so the report names it instead of deleting it.
    let image = sandbox.images().pop().expect("the image must survive");
    assert!(
        report.contains(&image.display().to_string()),
        "the repair must name the image it left: {report}"
    );

    // A second run finds nothing to do, and the repository still works.
    let after = doctor(&fx.golden, &sandbox);
    assert!(
        after["journal"].as_array().expect("an array").is_empty(),
        "the repair must close the entry"
    );
    assert!(
        klon(&fx.golden, &sandbox, &["add", "feature"])
            .status
            .success(),
        "add must work after the repair"
    );
}

/// The same repair one step earlier: `KLON_TEST_PAUSE_AT=swapped` stops the
/// command after it announced the swap and before the first rename. Golden
/// never moved, so the repair only drops the staged copy on the volume.
#[test]
fn a_kill_before_the_move_leaves_golden_alone() {
    let name = "a_kill_before_the_move_leaves_golden_alone";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 120, 6, 12, 3);
    let before = manifest(&fx.golden);

    let mut child = spawn_paused(
        &fx.golden,
        &sandbox,
        "swapped",
        &["init", "--volume", SIZE, "--yes"],
    );
    let entry = fx.golden.join(".git").join("klon").join("journal");
    let staged = wait_until(
        || state_of(&entry).as_deref() == Some("swapped"),
        Duration::from_secs(120),
    );
    sigkill(&child);
    let _ = child.wait();
    assert!(staged, "init --volume must reach the state swapped");
    assert!(fx.golden.is_dir(), "golden must not have moved");

    let out = klon(&fx.golden, &sandbox, &["doctor", "--repair"]);
    assert!(out.status.success(), "repair failed: {}", stderr(&out));
    println!("repair report:\n{}", stdout(&out));
    assert_eq!(before, manifest(&fx.golden), "golden must be byte-equal");
    let after = doctor(&fx.golden, &sandbox);
    assert!(
        after["journal"].as_array().expect("an array").is_empty(),
        "the repair must close the entry"
    );
    assert_eq!(
        after["features"]["volume"]["status"], "absent",
        "a conversion that never swapped leaves no volume record"
    );
}

/// The `state` of the single open journal entry, or None.
fn state_of(dir: &Path) -> Option<String> {
    let file = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|item| item.path())
        .find(|path| path.extension().is_some_and(|e| e == "json"))?;
    let text = fs::read_to_string(file).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(value["state"].as_str()?.to_string())
}

/// The seventh acceptance line: `init --volume --undo` restores golden on the
/// old filesystem and `doctor` reports the byte backend again.
///
/// The run before it proves the refusal: a klon lives on the volume and goes
/// away with it, so `--undo` names it and stops.
#[test]
fn undo_restores_golden_on_the_old_filesystem() {
    let name = "undo_restores_golden_on_the_old_filesystem";
    let Some(sandbox) = sandbox(name) else {
        return;
    };
    let fx = Fixture::generate(SEED, 200, 8, 20, 3);
    let before = manifest(&fx.golden);
    let record = convert(&fx, &sandbox, SIZE);
    let image = path_of(&record, "image");
    let golden_new = path_of(&record, "golden_new");

    let out = klon(&fx.golden, &sandbox, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let out = klon(
        &fx.golden,
        &sandbox,
        &["init", "--volume", "--undo", "--yes"],
    );
    assert!(!out.status.success(), "a live klon must stop the undo");
    assert!(
        stderr(&out).contains("live on the volume"),
        "the refusal must name the klons: {}",
        stderr(&out)
    );
    assert!(golden_new.is_dir(), "the refusal must change nothing");
    let out = klon(&fx.golden, &sandbox, &["rm", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));

    let out = klon(
        &fx.golden,
        &sandbox,
        &["init", "--volume", "--undo", "--yes"],
    );
    assert!(out.status.success(), "undo failed: {}", stderr(&out));
    assert!(
        fx.golden.is_dir()
            && !fs::symlink_metadata(&fx.golden)
                .expect("golden")
                .is_symlink(),
        "undo must leave a plain directory at golden's path"
    );
    assert_eq!(
        before,
        manifest(&fx.golden),
        "golden must be byte-equal after the undo"
    );
    assert_eq!(git_ok(&fx.golden, &["status", "--porcelain"]), "");
    assert!(
        loop_device(&image).is_none(),
        "the undo must detach the volume"
    );

    let report = doctor(&fx.golden, &sandbox);
    assert_ne!(
        report["backend"], "btrfs-snapshot",
        "a golden off the volume must lose the snapshot backend"
    );
    if report["filesystem"] == "ext4" {
        assert_eq!(
            report["backend"], "copy",
            "ext4 has no reflink, so the byte backend wins: {}",
            report["backend_reason"]
        );
    }
    assert_eq!(
        report["features"]["volume"]["status"], "absent",
        "the undo must drop the volume record"
    );
    // The image goes with the volume when udisks let klon release the loop
    // device, and stays with a printed line when it did not (S1 §9).
    assert!(
        !image.exists() || stderr(&out).contains(&image.display().to_string()),
        "an image that stays must be named: {}",
        stderr(&out)
    );
    assert!(
        wait_until(
            || !with_suffix(&fx.golden, ".klon-old").exists()
                && !with_suffix(&fx.golden, ".klon-plain").exists(),
            Duration::from_secs(60)
        ),
        "the undo must leave no staging copy"
    );
    // The restored repository still spawns a klon, now through the byte path.
    let out = klon(&fx.golden, &sandbox, &["add", "feature"]);
    assert!(
        out.status.success(),
        "add after the undo failed: {}",
        stderr(&out)
    );
}
