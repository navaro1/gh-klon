//! C3 acceptance tests for `gh klon list`. C1 moves the fixture helpers to
//! `tests/common`; the copies here go away then.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_gh-klon");

/// Run `git -C <cwd> <args>` with an isolated identity and config.
fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "klon")
        .env("GIT_AUTHOR_EMAIL", "klon@example.com")
        .env("GIT_COMMITTER_NAME", "klon")
        .env("GIT_COMMITTER_EMAIL", "klon@example.com")
        .output()
        .expect("run git")
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn klon(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run gh-klon")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A generated repository: `main`, a `feature` branch that edits one file,
/// and an ignored `build/` directory.
struct Fixture {
    _tmp: tempfile::TempDir,
    golden: PathBuf,
}

impl Fixture {
    /// `tracked` files spread over `dirs` directories, `ignored` files in `build/`.
    fn generate(tracked: usize, dirs: usize, ignored: usize) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        for i in 0..tracked {
            let dir = golden.join(format!("d{:03}", i % dirs));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("f{i}.txt")), format!("tracked file {i}\n")).unwrap();
        }
        let build = golden.join("build");
        fs::create_dir(&build).unwrap();
        for i in 0..ignored {
            fs::write(
                build.join(format!("o{i}.bin")),
                format!("object {i}\n").repeat(3),
            )
            .unwrap();
        }
        fs::write(golden.join(".gitignore"), "/build/\n").unwrap();
        git_ok(&golden, &["init", "-q", "-b", "main"]);
        git_ok(&golden, &["add", "-A"]);
        git_ok(&golden, &["commit", "-qm", "base"]);

        git_ok(&golden, &["checkout", "-qb", "feature"]);
        fs::write(golden.join("d000").join("f0.txt"), "edited on feature\n").unwrap();
        git_ok(&golden, &["commit", "-qam", "feature"]);
        git_ok(&golden, &["checkout", "-q", "main"]);
        Fixture { _tmp: tmp, golden }
    }

    fn klon_path(&self, branch: &str) -> PathBuf {
        self.golden.parent().unwrap().join("golden.wt").join(branch)
    }
}

#[test]
fn list_with_no_klons_prints_nothing() {
    let fx = Fixture::generate(20, 4, 4);
    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    assert_eq!(stdout(&out), "", "the main worktree is not a klon");
}

#[test]
fn list_shows_every_klon_with_a_dirty_flag() {
    let fx = Fixture::generate(50, 5, 5);
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
    let feature = fx.klon_path("feature");
    let other = fx.klon_path("other");
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
    fs::write(feature.join("d000").join("f0.txt"), "dirty\n").unwrap();
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
