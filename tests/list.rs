//! Acceptance tests for `gh klon list`. The shared harness lives in `tests/common`.

mod common;

use std::fs;
use std::path::Path;

use common::{git_ok, klon, stderr, stdout, Fixture};

const SEED: u64 = 42;

/// The byte size of the fixture's ignored `build/` directory. `list` reports it
/// as the upper bound of the disk delta, marked with a `≤` (C30). Both klons of
/// this fixture carry the same five ignored files, so both show the same size.
fn ignored_bytes(klon_path: &Path) -> u64 {
    fs::read_dir(klon_path.join("build"))
        .expect("the ignored directory")
        .flatten()
        .map(|entry| entry.metadata().expect("stat").len())
        .sum()
}

#[test]
fn list_with_no_klons_prints_nothing() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(stdout(&out), "", "the main worktree is not a klon");
}

#[test]
fn list_shows_every_klon_with_a_dirty_flag() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 0);
    // A second branch gives a second klon.
    git_ok(&fx.golden, &["checkout", "-qb", "other"]);
    fs::write(fx.golden.join("other.txt"), "on other\n").unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "other"]);
    git_ok(&fx.golden, &["checkout", "-q", "main"]);

    for branch in ["feature", "other"] {
        let out = klon(&fx.golden, &["add", branch]);
        assert!(
            out.status.success(),
            "add {branch} failed: {}",
            stderr(&out)
        );
    }

    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    let feature = fx.default_klon_path();
    let other = fx.klon_path("other");
    let head = |path: &Path| {
        git_ok(path, &["rev-parse", "--short", "HEAD"])
            .trim()
            .to_string()
    };
    // C30 puts the five extras columns between the head and the radar columns:
    // disk, RSS, live processes, PR, checks. The klons idle, so the disk column
    // bounds the ignored-directory size and every other extra shows `-`. Neither
    // klon touches a file the other touches, so the radar reads `clean`.
    let extras = |path: &Path| format!("| ≤ {} B | - | 0 | - | -", ignored_bytes(path));
    const RADAR: &str = "| clean | clean | behind 0";
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "{} feature {} {} {RADAR}\n{} other {} {} {RADAR}",
            feature.display(),
            head(&feature),
            extras(&feature),
            other.display(),
            head(&other),
            extras(&other)
        ),
        "one line per klon: path, branch, short HEAD, extras, then the radar"
    );

    // A modified file puts a `*` on that klon's line and on no other. The disk
    // reading is unchanged: only the ignored directory counts.
    fs::write(feature.join("f2.txt"), "dirty\n").unwrap();
    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "{} feature {} * {} {RADAR}\n{} other {} {} {RADAR}",
            feature.display(),
            head(&feature),
            extras(&feature),
            other.display(),
            head(&other),
            extras(&other)
        )
    );
}
