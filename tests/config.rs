//! C10 acceptance tests: the `.klon.toml` loader, the approval gate, and the path template.

mod common;

use common::{git_ok, klon_env, stderr, stdout};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `gh-klon <args>` in `cwd`. `KLON_CONFIG_HOME` and `HOME` stay inside the test.
fn klon(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    klon_env(
        cwd,
        &[
            ("KLON_CONFIG_HOME", home.as_os_str()),
            ("HOME", home.as_os_str()),
        ],
        args,
    )
}

/// The first token of `sha256sum <file>`.
fn sha256_of(file: &Path) -> String {
    let out = Command::new("sha256sum")
        .arg(file)
        .output()
        .expect("run sha256sum");
    assert!(out.status.success(), "sha256sum failed");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("sha256sum prints a hash")
        .to_string()
}

/// A repository with a `main` branch, a `feature` branch, and a private config home.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    golden: PathBuf,
}

impl Fixture {
    fn generate() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        fs::write(golden.join("readme.txt"), "readme\n").unwrap();
        git_ok(&golden, &["init", "-q", "-b", "main"]);
        git_ok(&golden, &["add", "-A"]);
        git_ok(&golden, &["commit", "-qm", "base"]);
        git_ok(&golden, &["branch", "feature"]);
        Fixture {
            _tmp: tmp,
            home,
            golden,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.golden.join(".klon.toml")
    }

    fn write_config(&self, body: &str) {
        fs::write(self.config_path(), body).unwrap();
    }

    fn approvals_path(&self) -> PathBuf {
        self.home.join("klon").join("approvals.toml")
    }
}

#[test]
fn up_without_approval_refuses_and_runs_nothing() {
    let fx = Fixture::generate();
    fx.write_config("[warm]\nsteps = [\"touch up-ran\"]\n");
    let out = klon(&fx.golden, &fx.home, &["up"]);
    assert!(
        !out.status.success(),
        "up must fail without approval: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("needs approval"),
        "stderr must name the approval: {}",
        stderr(&out)
    );
    assert!(!fx.golden.join("up-ran").exists(), "up must run nothing");
}

#[test]
fn up_yes_runs_steps_and_writes_the_hash() {
    let fx = Fixture::generate();
    fx.write_config("[warm]\nsteps = [\"touch up-ran\"]\n");
    let out = klon(&fx.golden, &fx.home, &["up", "--yes"]);
    assert!(out.status.success(), "up --yes failed: {}", stderr(&out));
    assert!(fx.golden.join("up-ran").exists(), "the step must run");
    let approvals = fs::read_to_string(fx.approvals_path()).expect("approvals.toml");
    assert!(
        approvals.contains(&sha256_of(&fx.config_path())),
        "approvals.toml must hold the file hash: {approvals}"
    );
}

#[test]
fn one_byte_change_invalidates_the_approval() {
    let fx = Fixture::generate();
    fx.write_config("[warm]\nsteps = [\"touch m1\"]\n");
    assert!(klon(&fx.golden, &fx.home, &["up", "--yes"])
        .status
        .success());
    // One byte: m1 becomes m2.
    fx.write_config("[warm]\nsteps = [\"touch m2\"]\n");
    let out = klon(&fx.golden, &fx.home, &["up"]);
    assert!(
        !out.status.success(),
        "the old approval must not carry over"
    );
    assert!(stderr(&out).contains("needs approval"));
    assert!(!fx.golden.join("m2").exists(), "the step must not run");
    assert!(klon(&fx.golden, &fx.home, &["up", "--yes"])
        .status
        .success());
    assert!(
        fx.golden.join("m2").exists(),
        "the new approval must run it"
    );
    let approvals = fs::read_to_string(fx.approvals_path()).unwrap();
    assert!(approvals.contains(&sha256_of(&fx.config_path())));
}

#[test]
fn unknown_key_warns_once_and_does_not_fail() {
    let fx = Fixture::generate();
    fx.write_config("bogus = 1\n\n[warm]\nsteps = [\"true\"]\nextra = 2\n");
    let out = klon(&fx.golden, &fx.home, &["up", "--yes"]);
    assert!(
        out.status.success(),
        "an unknown key must not fail: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    let lines: Vec<&str> = err
        .lines()
        .filter(|line| line.contains("unknown"))
        .collect();
    assert_eq!(lines.len(), 1, "exactly one warning line: {}", stderr(&out));
    assert!(lines[0].contains("bogus"), "{}", lines[0]);
    assert!(lines[0].contains("warm.extra"), "{}", lines[0]);
}

#[test]
fn up_without_config_succeeds() {
    let fx = Fixture::generate();
    let out = klon(&fx.golden, &fx.home, &["up"]);
    assert!(out.status.success(), "up with no config: {}", stderr(&out));
}

#[test]
fn add_refuses_a_template_that_resolves_to_root() {
    let fx = Fixture::generate();
    fx.write_config("path = \"/\"\n");
    let out = klon(&fx.golden, &fx.home, &["add", "feature"]);
    assert!(!out.status.success(), "add must refuse path = /");
    assert!(
        stderr(&out).contains("refuses path template"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1,
        "no worktree may appear"
    );
}

#[test]
fn add_refuses_a_template_that_resolves_to_home() {
    let fx = Fixture::generate();
    fx.write_config(&format!("path = \"{}\"\n", fx.home.display()));
    let out = klon(&fx.golden, &fx.home, &["add", "feature"]);
    assert!(!out.status.success(), "add must refuse the home directory");
    assert!(
        stderr(&out).contains("refuses path template"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn add_refuses_a_template_that_resolves_to_the_repository_root() {
    let fx = Fixture::generate();
    fx.write_config("path = \".\"\n");
    let out = klon(&fx.golden, &fx.home, &["add", "feature"]);
    assert!(!out.status.success(), "add must refuse the repository root");
    assert!(
        stderr(&out).contains("refuses path template"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn add_resolves_a_relative_template_against_golden() {
    let fx = Fixture::generate();
    fx.write_config("path = \"../klons/{repo}-{branch}\"\n");
    let out = klon(&fx.golden, &fx.home, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let expected = fx
        .golden
        .parent()
        .unwrap()
        .join("klons")
        .join("golden-feature");
    assert_eq!(stdout(&out).trim(), expected.to_str().unwrap());
    assert!(expected.join(".git").is_file(), "the klon must exist");
    assert!(git_ok(&fx.golden, &["worktree", "list"]).contains("golden-feature"));
}

#[test]
fn an_unknown_template_placeholder_is_an_error() {
    let fx = Fixture::generate();
    fx.write_config("path = \"../{nope}\"\n");
    let out = klon(&fx.golden, &fx.home, &["add", "feature"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("{nope}"), "{}", stderr(&out));
}

#[test]
fn copy_reinstall_needs_its_own_approval_key() {
    let fx = Fixture::generate();
    fx.write_config("[copy]\nreinstall = { \"node_modules\" = \"touch reinstalled\" }\n");
    // `up` uses only warm.steps here, so the reinstall command must not run and
    // must not gate the run.
    let out = klon(&fx.golden, &fx.home, &["up"]);
    assert!(
        out.status.success(),
        "up without warm steps: {}",
        stderr(&out)
    );
    assert!(!fx.golden.join("reinstalled").exists());
}
