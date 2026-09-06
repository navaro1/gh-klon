//! The zero-compile tests (spec §7 C11, R10).
//!
//! R10 says that a fresh klon of a warm golden compiles zero units and
//! downloads zero bytes. Two ecosystems prove it: cargo, whose `target/` moves
//! as it is, and pnpm, whose `node_modules/.modules.yaml` names an absolute
//! store that the path fixup pass rewrites (handoff §9).
//!
//! Both fixtures are offline by construction. The Rust one depends on a crate
//! that lives inside the fixture through a `path` dependency. The pnpm one
//! depends on a tarball inside the fixture and keeps its store inside the
//! fixture, which `.npmrc` names with a relative path. Neither reaches a
//! registry, so no test needs the network.
//!
//! Each test skips with a printed reason when its tool is absent.

mod common;

use common::{git_ok, klon, stderr};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Environment variables that leak from `cargo test` into a nested build and
/// change where it writes or how it links. The child clears every one of them.
const CARGO_LEAKS: &[&str] = &[
    "CARGO",
    "CARGO_BUILD_JOBS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_MAKEFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_UNSTABLE_SPARSE_REGISTRY",
    "MAKEFLAGS",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
];

/// A git repository at `<tmp>/golden` with `main` and `feature`, holding
/// `files` and a `.gitignore` that lists `ignored`.
struct Project {
    _tmp: tempfile::TempDir,
    golden: PathBuf,
}

impl Project {
    fn new(files: &[(&str, &str)], ignored: &str) -> Project {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let golden = tmp
            .path()
            .canonicalize()
            .expect("canonical tempdir")
            .join("golden");
        fs::create_dir(&golden).unwrap();
        for (name, body) in files {
            let path = golden.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
        }
        fs::write(golden.join(".gitignore"), ignored).unwrap();
        git_ok(&golden, &["init", "-q", "-b", "main"]);
        git_ok(&golden, &["add", "-A"]);
        git_ok(&golden, &["commit", "-qm", "base"]);
        // `add` checks out `feature`, so the branch must exist and must carry
        // the same `.gitignore`.
        git_ok(&golden, &["branch", "feature"]);
        Project { _tmp: tmp, golden }
    }

    /// Commit every tracked and new file, and move `feature` to the new
    /// commit. A warm step that writes a lock file calls this, so the klon
    /// checks the file out instead of losing it to `git clean`.
    fn commit(&self, message: &str) {
        git_ok(&self.golden, &["add", "-A"]);
        git_ok(&self.golden, &["commit", "-qm", message]);
        git_ok(&self.golden, &["branch", "-f", "feature", "main"]);
    }

    /// `gh klon add feature`, then the klon path.
    fn klon(&self) -> PathBuf {
        let out = klon(&self.golden, &["add", "feature"]);
        assert!(out.status.success(), "add failed: {}", stderr(&out));
        self.golden
            .parent()
            .unwrap()
            .join("golden.wt")
            .join("feature")
    }
}

/// Run `program` in `cwd` with the leaking build variables cleared.
///
/// The colour is forced off. CI sets `CARGO_TERM_COLOR=always`, and a coloured
/// `Compiling` line carries ANSI escapes that no plain text match survives.
fn run(program: &Path, cwd: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1");
    for name in CARGO_LEAKS {
        command.env_remove(name);
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    command
        .output()
        .unwrap_or_else(|err| panic!("run {}: {err}", program.display()))
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The first program named `name` on PATH, or None with a printed reason.
fn tool(name: &str, test: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    for dir in extra {
        if dir.is_file() {
            return Some(dir.clone());
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    println!("skipped: {test}: {name} is not on PATH");
    None
}

// --- Rust --------------------------------------------------------------------

const CARGO_TOML: &str = r#"[workspace]
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
dep = { path = "dep" }
"#;

const DEP_TOML: &str = r#"[package]
name = "dep"
version = "0.1.0"
edition = "2021"
"#;

/// R10 and the C11 acceptance line: `cargo build` in a fresh klon of a warm
/// golden prints no `Compiling` line.
#[test]
fn cargo_build_in_a_fresh_klon_compiles_nothing() {
    let Some(cargo) = tool("cargo", "cargo_build_in_a_fresh_klon_compiles_nothing", &[]) else {
        return;
    };
    let project = Project::new(
        &[
            ("Cargo.toml", CARGO_TOML),
            (
                "src/main.rs",
                "fn main() { println!(\"{}\", dep::hello()); }\n",
            ),
            ("dep/Cargo.toml", DEP_TOML),
            (
                "dep/src/lib.rs",
                "pub fn hello() -> &'static str { \"hello\" }\n",
            ),
        ],
        "/target/\n",
    );

    // Warm golden. `--offline` proves that the fixture needs no registry.
    // `-j 2` keeps the nested build from claiming every core: this suite runs
    // beside its own other tests and beside the timing budgets of C24 and C8.
    let warm = run(
        &cargo,
        &project.golden,
        &["build", "--offline", "-j", "2"],
        &[],
    );
    assert!(
        warm.status.success(),
        "the warm build failed: {}",
        text(&warm)
    );
    assert!(
        text(&warm).contains("Compiling fixture"),
        "the warm build must compile the fixture: {}",
        text(&warm)
    );

    let klon_path = project.klon();
    let cold = run(&cargo, &klon_path, &["build", "--offline", "-j", "2"], &[]);
    assert!(
        cold.status.success(),
        "the klon build failed: {}",
        text(&cold)
    );
    let report = text(&cold);
    let compiled: Vec<&str> = report
        .lines()
        .filter(|line| line.contains("Compiling "))
        .collect();
    assert!(
        compiled.is_empty(),
        "the klon must compile zero units, found {compiled:?}\n{report}"
    );
}

// --- pnpm ---------------------------------------------------------------------

const PACKAGE_JSON: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "private": true,
  "dependencies": { "leftpad": "file:./vendor/leftpad-1.0.0.tgz" }
}
"#;

/// A deterministic tarball of one tiny package, written into `vendor/`.
/// `tar` is part of every supported host, and a tarball dependency lands in the
/// pnpm store, which a `file:` directory dependency does not.
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

/// Every entry below `root`: path to (kind, size, inode). A new inode proves a
/// relink, which a warm install must not do.
fn tree(root: &Path) -> BTreeMap<String, (char, u64, u64)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (char, u64, u64)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).expect("stat");
            let kind = if meta.is_symlink() {
                'l'
            } else if meta.is_dir() {
                walk(root, &path, out);
                'd'
            } else {
                'f'
            };
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.insert(name, (kind, meta.len(), meta.ino()));
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    // pnpm rewrites its own bookkeeping on every run, so those two never count.
    out.remove(".modules.yaml");
    out.remove(".pnpm/lock.yaml");
    out
}

/// R10 and the C11 acceptance line: `pnpm install --frozen-lockfile --offline`
/// in a fresh klon exits 0, changes nothing under `node_modules`, and finds a
/// `.modules.yaml` that names the klon.
#[test]
fn pnpm_install_in_a_fresh_klon_changes_nothing() {
    let test = "pnpm_install_in_a_fresh_klon_changes_nothing";
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let Some(pnpm) = tool("pnpm", test, &[home.join(".local/share/pnpm/pnpm")]) else {
        return;
    };
    if tool("node", test, &[]).is_none() {
        return;
    }
    // `store-dir` is relative, so it resolves inside whichever tree runs the
    // install. `.modules.yaml` still records it as an absolute path, which is
    // the line the fixup pass has to rewrite.
    let project = Project::new(
        &[
            ("package.json", PACKAGE_JSON),
            (".npmrc", "store-dir=.pnpm-store\n"),
        ],
        "node_modules/\n.pnpm-store/\n",
    );
    if !pack(&project.golden) {
        println!("skipped: {test}: tar cannot write the fixture package");
        return;
    }
    // `CI=1` keeps pnpm from asking whether it may rebuild `node_modules`.
    let warm = run(
        &pnpm,
        &project.golden,
        &["install", "--offline", "--child-concurrency=1"],
        &[("CI", "1"), ("UV_THREADPOOL_SIZE", "2")],
    );
    assert!(
        warm.status.success(),
        "the warm install failed: {}",
        text(&warm)
    );
    // A real project commits its lock file. Without that commit `git clean`
    // would delete it from the klon, because it is untracked and not ignored.
    project.commit("vendor and lock");

    let klon_path = project.klon();
    let modules = klon_path.join("node_modules");
    let yaml = fs::read_to_string(modules.join(".modules.yaml")).expect("read .modules.yaml");
    assert!(
        yaml.contains(&format!("storeDir: {}/.pnpm-store", klon_path.display())),
        ".modules.yaml must name the klon store after the fixup:\n{yaml}"
    );

    let before = tree(&modules);
    let cold = run(
        &pnpm,
        &klon_path,
        &[
            "install",
            "--frozen-lockfile",
            "--offline",
            "--child-concurrency=1",
        ],
        &[("CI", "1"), ("UV_THREADPOOL_SIZE", "2")],
    );
    assert!(
        cold.status.success(),
        "the klon install failed: {}",
        text(&cold)
    );
    assert_eq!(
        tree(&modules),
        before,
        "the install must relink nothing under node_modules:\n{}",
        text(&cold)
    );
}
