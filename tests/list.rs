//! Acceptance tests for `gh klon list`. The shared harness lives in `tests/common`.

mod common;

use std::fs;
use std::path::Path;

use common::{git_ok, klon, stderr, stdout, Fixture};

const SEED: u64 = 42;

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
    let other = fx.golden.parent().unwrap().join("golden.wt").join("other");
    let head = |path: &Path| {
        git_ok(path, &["rev-parse", "--short", "HEAD"])
            .trim()
            .to_string()
    };
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "{} feature {}\n{} other {}",
            feature.display(),
            head(&feature),
            other.display(),
            head(&other)
        ),
        "one line per klon: path, branch, short HEAD"
    );

    // A modified file puts a `*` on that klon's line and on no other.
    fs::write(feature.join("f2.txt"), "dirty\n").unwrap();
    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "{} feature {} *\n{} other {}",
            feature.display(),
            head(&feature),
            other.display(),
            head(&other)
        )
    );
}
