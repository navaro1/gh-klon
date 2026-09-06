//! Record the klon commit in the binary. `bench` prints it in the environment
//! record, so a result file says which build produced it (spec §7 C8, R14).
//!
//! The order is: the `KLON_COMMIT` environment variable, then `git describe
//! --always --dirty`, then `unknown`. A source tree without git, for example an
//! unpacked crate, still builds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=KLON_COMMIT");
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let root = Path::new(&root);
    // A build script that names one watched path loses cargo's default watch of
    // the crate, so this names every input of the string below: the sources,
    // because `git describe --dirty` reads them, and the git files that change
    // when a commit lands.
    for path in watched(root) {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rustc-env=KLON_COMMIT={}", commit(root));
}

/// Every path whose change can change the commit string.
fn watched(root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = ["src", "tests", "bench", "Cargo.toml", "Cargo.lock"]
        .iter()
        .map(|name| root.join(name))
        .collect();
    let Some(git_dir) = git_dir(root) else {
        return paths;
    };
    // `HEAD` and the index change on a checkout and on a staged edit.
    paths.push(git_dir.join("HEAD"));
    paths.push(git_dir.join("index"));
    // A commit on a branch rewrites the branch reference, not `HEAD`. The
    // references of a linked worktree live in the common directory, and a
    // packed reference lives in one file there.
    let common = common_dir(&git_dir);
    paths.push(common.join("packed-refs"));
    if let Some(reference) = head_reference(&git_dir) {
        paths.push(common.join(reference));
    }
    paths
}

/// The git directory of `root`. In a linked worktree `.git` is a file that
/// holds `gitdir: <path>`.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.trim().strip_prefix("gitdir: ")?;
    Some(root.join(target))
}

/// The common directory that holds the references. A linked worktree names it
/// in `<git dir>/commondir`; a main worktree is its own.
fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let target = Path::new(text.trim());
            if target.is_absolute() {
                target.to_path_buf()
            } else {
                git_dir.join(target)
            }
        }
        Err(_) => git_dir.to_path_buf(),
    }
}

/// The reference that `HEAD` points at, for example `refs/heads/main`. A
/// detached `HEAD` names none: its own file already holds the commit.
fn head_reference(git_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(text.trim().strip_prefix("ref: ")?.to_string())
}

fn commit(dir: &Path) -> String {
    if let Ok(text) = std::env::var("KLON_COMMIT") {
        let text = text.trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["describe", "--always", "--dirty"])
        .output();
    match output {
        Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => "unknown".to_string(),
        },
        _ => "unknown".to_string(),
    }
}
