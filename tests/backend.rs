//! The backend tests (spec §7 C5, R4, R5, R35).
//!
//! The reflink tests need a filesystem that answers `FICLONE`. The development
//! laptop runs ext4, which does not, so every one of them reads
//! `KLON_TEST_REFLINK_DIR` and prints a skip reason when it is unset. The
//! `loop-fs` job in `.github/workflows/ci.yml` sets it to an XFS loop mount and
//! then to a btrfs loop mount, so CI is the proof for those lines.

mod common;

use common::{klon, klon_env, manifest, stderr, stdout, Fixture};
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SEED: u64 = 77;

/// The reflink filesystem for this run, or None with a printed reason.
fn reflink_dir(test: &str) -> Option<PathBuf> {
    match std::env::var_os("KLON_TEST_REFLINK_DIR") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                return Some(path);
            }
            println!("skipped: {test}: KLON_TEST_REFLINK_DIR is not a directory");
            None
        }
        None => {
            println!(
                "skipped: {test}: set KLON_TEST_REFLINK_DIR to a directory on xfs with reflink=1, \
                 on btrfs, or on another filesystem that answers FICLONE"
            );
            None
        }
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("not one JSON document: {err}\n{text}"))
}

/// `doctor --json` for `golden`, with extra environment variables.
fn doctor(golden: &Path, envs: &[(&str, &OsStr)]) -> Value {
    let out = klon_env(golden, envs, &["doctor", "--json"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    parse(&stdout(&out))
}

/// Every `(device, inode)` pair of the regular files below `root`.
fn inodes(root: &Path) -> HashSet<(u64, u64)> {
    manifest(root)
        .iter()
        .filter(|e| e.kind == "file")
        .map(|e| {
            let meta = fs::symlink_metadata(root.join(&e.path)).expect("stat a manifest entry");
            (meta.dev(), meta.ino())
        })
        .collect()
}

/// R4: no file of the klon may share an inode with a file of golden.
fn assert_no_shared_inode(golden: &Path, klon_path: &Path) {
    let source = inodes(golden);
    let clone = inodes(klon_path);
    let shared: Vec<_> = clone.intersection(&source).collect();
    assert!(
        shared.is_empty(),
        "{} files share an inode with golden",
        shared.len()
    );
    assert!(!clone.is_empty(), "the klon must hold files");
}

/// The ignored `build/` directory of both trees, with the mtimes.
fn ignored_manifest(root: &Path) -> Vec<common::Entry> {
    manifest(&root.join("build"))
}

// --- The probe on the host filesystem -----------------------------------------

/// The first C5 acceptance line. On ext4 the answer is fixed: `copy`, because
/// the filesystem cannot reflink. On another filesystem the test only checks
/// that the row is one klon knows.
#[test]
fn the_probe_reports_a_known_backend_and_its_reason() {
    let fx = Fixture::generate(SEED, 40, 4, 4, 2);
    let report = doctor(&fx.golden, &[]);
    let name = report["backend"].as_str().expect("a backend name");
    let reason = report["backend_reason"].as_str().expect("a reason");
    assert!(
        ["copy", "reflink-walk"].contains(&name),
        "unknown backend {name}"
    );
    assert!(!reason.is_empty(), "the reason must say why");
    if report["filesystem"] == "ext4" {
        assert_eq!(name, "copy");
        assert_eq!(reason, "reflink unsupported");
        assert_eq!(report["features"]["reflink"]["status"], "absent");
        let detail = report["features"]["reflink"]["detail"]
            .as_str()
            .expect("a detail");
        assert!(
            detail.starts_with("reflink unsupported: "),
            "the reflink row must name the errno: {detail}"
        );
    } else {
        println!(
            "note: this host runs {}, not ext4; the ext4 assertions did not run",
            report["filesystem"]
        );
    }
}

/// The probe answer is cached, `KLON_PROBE_REFRESH=1` rewrites it, and
/// `doctor --repair` deletes it first.
#[test]
fn the_probe_answer_is_cached_and_refreshed() {
    let fx = Fixture::generate(SEED, 30, 3, 3, 2);
    let first = doctor(&fx.golden, &[]);
    let cache = fx.golden.join(".git").join("klon").join("probe.json");
    assert!(cache.is_file(), "the probe answer must be cached");
    let text = fs::read_to_string(&cache).unwrap();
    let cached = parse(&text);
    assert_eq!(cached["version"], 1);
    assert_eq!(cached["backend"], first["backend"]);
    assert_eq!(cached["reason"], first["backend_reason"]);
    assert_eq!(cached["filesystem"], first["filesystem"]);
    assert!(cached["created"].as_str().is_some_and(|c| !c.is_empty()));

    // A hand-edited cache is used as is, which proves that the cache is read.
    let forged = text.replace("\"reason\": \"", "\"reason\": \"cached ");
    fs::write(&cache, &forged).unwrap();
    let second = doctor(&fx.golden, &[]);
    assert!(
        second["backend_reason"]
            .as_str()
            .unwrap()
            .starts_with("cached "),
        "the cached reason must win: {}",
        second["backend_reason"]
    );

    // The refresh flag probes again and overwrites the forged reason. The
    // answer must equal the first one: a reason that changes between two probes
    // of one host would make the cache disagree with itself.
    let third = doctor(&fx.golden, &[("KLON_PROBE_REFRESH", OsStr::new("1"))]);
    assert_eq!(third["backend"], first["backend"]);
    assert_eq!(third["backend_reason"], first["backend_reason"]);

    // `--repair` also refreshes.
    fs::write(&cache, &forged).unwrap();
    let out = klon(&fx.golden, &["doctor", "--json", "--repair"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let fourth = parse(&stdout(&out));
    assert_eq!(fourth["backend"], first["backend"]);
    assert_eq!(fourth["backend_reason"], first["backend_reason"]);
}

/// A probe cache from a future klon fails closed, like the journal. `doctor
/// --repair` must not delete a format that this binary cannot read, so it stops
/// before it repairs anything.
#[test]
fn a_future_probe_cache_version_fails_closed() {
    let fx = Fixture::generate(SEED, 20, 2, 2, 1);
    let dir = fx.golden.join(".git").join("klon");
    fs::create_dir_all(&dir).unwrap();
    let cache = dir.join("probe.json");
    let future = r#"{"version":99,"backend":"copy","reason":"x","filesystem":"ext4","created":"2026-01-01T00:00:00Z"}"#;
    fs::write(&cache, future).unwrap();

    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(!out.status.success(), "add must refuse a future cache");
    assert!(
        stderr(&out).contains("unknown probe cache version 99"),
        "unexpected stderr: {}",
        stderr(&out)
    );
    assert!(!fx.default_klon_path().exists(), "add must change nothing");

    // A stale journal entry gives the repair real work. It must stay untouched.
    let journal = dir.join("journal");
    fs::create_dir_all(&journal).unwrap();
    let entry = serde_json::json!({
        "version": 1,
        "op": "add",
        "state": "planned",
        "path": fx.klon_path("gone"),
        "branch": "feature",
        "started": "2026-09-05T10:00:00Z",
    })
    .to_string();
    fs::write(journal.join("stale.json"), &entry).unwrap();

    for args in [
        vec!["doctor", "--json"],
        vec!["doctor", "--json", "--repair"],
    ] {
        let out = klon(&fx.golden, &args);
        assert!(!out.status.success(), "{args:?} must refuse a future cache");
        assert!(
            stderr(&out).contains("unknown probe cache version 99"),
            "{args:?} stderr: {}",
            stderr(&out)
        );
    }
    assert_eq!(
        fs::read_to_string(&cache).unwrap(),
        future,
        "the repair must not delete a cache it cannot read"
    );
    assert!(
        journal.join("stale.json").is_file(),
        "the repair must not run before the version check"
    );
}

/// A destination on another filesystem cannot receive a block-sharing clone.
/// `add` must fall back to `copy` instead of failing halfway with `EXDEV`.
/// It needs a reflink golden and a second filesystem, so CI is the proof.
#[test]
fn a_destination_on_another_filesystem_falls_back_to_copy() {
    let name = "a_destination_on_another_filesystem_falls_back_to_copy";
    let Some(base) = reflink_dir(name) else {
        return;
    };
    let fx = Fixture::generate_in(&base, SEED, 40, 4, 10, 2);
    if doctor(&fx.golden, &[])["backend"] != "reflink-walk" {
        println!("skipped: {name}: the probe did not pick reflink-walk on {base:?}");
        return;
    }
    // The runner's temporary directory is not on the loop filesystem.
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let target = elsewhere.path().join("klon");
    let out = klon(
        &fx.golden,
        &[
            "add",
            "--json",
            "--path",
            target.to_str().unwrap(),
            "feature",
        ],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let report = parse(&stdout(&out));
    assert_eq!(
        report["backend"], "copy",
        "a cross-filesystem destination must fall back"
    );
    common::assert_clean(&target);
    assert_no_shared_inode(&fx.golden, &target);
}

/// The third C5 acceptance line: a backend that drops one file fails the probe,
/// `doctor` reports the mismatch, and the selection falls through to a backend
/// that passes.
#[test]
fn a_backend_that_drops_a_file_fails_the_probe() {
    let fx = Fixture::generate(SEED, 30, 3, 3, 2);
    let report = doctor(&fx.golden, &[("KLON_TEST_DROP_BACKEND", OsStr::new("1"))]);
    let reason = report["backend_reason"].as_str().expect("a reason");
    assert!(
        reason.contains("probe failed: manifest mismatch"),
        "doctor must report the mismatch: {reason}"
    );
    assert_ne!(
        report["backend"], "drop-one",
        "a backend that fails the probe must never be selected"
    );
    assert!(
        ["copy", "reflink-walk"].contains(&report["backend"].as_str().unwrap()),
        "a real backend must win"
    );
}

/// `--backend` names a backend directly, and an unknown name is refused before
/// any repository change.
#[test]
fn the_backend_override_picks_the_named_backend() {
    let fx = Fixture::generate(SEED, 30, 3, 3, 2);
    let out = klon(
        &fx.golden,
        &["add", "--json", "--backend", "copy", "feature"],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&stdout(&out))["backend"], "copy");

    let fx2 = Fixture::generate(SEED + 1, 20, 2, 2, 1);
    let out = klon(&fx2.golden, &["add", "--backend", "no-such", "feature"]);
    assert!(!out.status.success(), "an unknown backend must be refused");
    assert!(
        stderr(&out).contains("unknown backend no-such"),
        "unexpected stderr: {}",
        stderr(&out)
    );
    assert!(!fx2.default_klon_path().exists(), "add must change nothing");
    // The refusal happens before `git worktree add`, so git registered nothing.
    let list = common::git_ok(&fx2.golden, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 1, "{list}");

    // The test-only backend is not reachable through the override.
    let out = klon_env(
        &fx2.golden,
        &[("KLON_TEST_DROP_BACKEND", OsStr::new("1"))],
        &["add", "--backend", "drop-one", "feature"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unknown backend drop-one"));
}

/// The fourth C5 acceptance line, on the host filesystem with the `copy`
/// backend.
#[test]
fn no_klon_file_shares_an_inode_with_golden() {
    let fx = Fixture::generate(SEED, 60, 5, 20, 3);
    let out = klon(&fx.golden, &["add", "--backend", "copy", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_no_shared_inode(&fx.golden, &fx.default_klon_path());
}

// --- The reflink backend ---------------------------------------------------------

/// The second C5 acceptance line: on a reflink filesystem the probe picks
/// `reflink-walk`, and `add` reproduces golden's ignored manifest, mtimes
/// included.
#[test]
fn the_probe_picks_reflink_walk_and_add_keeps_the_ignored_manifest() {
    let name = "the_probe_picks_reflink_walk_and_add_keeps_the_ignored_manifest";
    let Some(base) = reflink_dir(name) else {
        return;
    };
    let fx = Fixture::generate_in(&base, SEED, 60, 5, 20, 3);
    let report = doctor(&fx.golden, &[]);
    assert_eq!(
        report["backend"], "reflink-walk",
        "the probe must pick the clone backend on {}: {}",
        report["filesystem"], report["backend_reason"]
    );

    let before = ignored_manifest(&fx.golden);
    let out = klon(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(parse(&stdout(&out))["backend"], "reflink-walk");

    let klon_path = fx.default_klon_path();
    let after = ignored_manifest(&klon_path);
    assert_eq!(
        before, after,
        "the ignored manifest must match golden, mtimes included"
    );
    assert_no_shared_inode(&fx.golden, &klon_path);
    common::assert_clean(&klon_path);
}

/// The reflink walk keeps the shapes that the probe fixture covers: a symlink,
/// a read-only file, and a directory with a narrow mode.
#[test]
fn the_reflink_walk_keeps_every_shape() {
    let name = "the_reflink_walk_keeps_every_shape";
    let Some(base) = reflink_dir(name) else {
        return;
    };
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::generate_in(&base, SEED, 30, 3, 5, 2);
    let build = fx.golden.join("build");
    std::os::unix::fs::symlink("o0.bin", build.join("link")).unwrap();
    fs::write(build.join("read-only.bin"), b"read only\n").unwrap();
    fs::set_permissions(
        build.join("read-only.bin"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let narrow = build.join("narrow");
    fs::create_dir(&narrow).unwrap();
    fs::write(narrow.join("inside.bin"), b"inside\n").unwrap();
    fs::set_permissions(&narrow, fs::Permissions::from_mode(0o500)).unwrap();

    let before = ignored_manifest(&fx.golden);
    let out = klon(&fx.golden, &["add", "--backend", "reflink-walk", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let after = ignored_manifest(&fx.default_klon_path());
    assert_eq!(before, after, "every shape must survive the clone");
}

/// The fifth C5 acceptance line. It needs both the 100k fixture and a reflink
/// filesystem, so it runs in the nightly job only.
#[test]
fn the_reflink_walk_of_the_100k_fixture_takes_under_10_s() {
    let name = "the_reflink_walk_of_the_100k_fixture_takes_under_10_s";
    if std::env::var("KLON_FIXTURE").as_deref() != Ok("100k") {
        println!("skipped: {name}: set KLON_FIXTURE=100k to run it");
        return;
    }
    let Some(base) = reflink_dir(name) else {
        return;
    };
    let fx = Fixture::generate_in(&base, SEED, 90_000, 300, 10_000, 20);
    let started = Instant::now();
    let out = klon(&fx.golden, &["add", "--backend", "reflink-walk", "feature"]);
    let elapsed = started.elapsed();
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    println!("the reflink walk of the 100k fixture took {elapsed:?}");
    assert!(
        elapsed.as_secs_f64() < 10.0,
        "the reflink walk took {elapsed:?}; the budget is 10 s"
    );
    assert_no_shared_inode(&fx.golden, &fx.default_klon_path());
}
