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
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    // A new commit or a staged change must give a new string. A build script
    // that names one watched path loses cargo's default watch, so both files
    // are named here.
    if let Some(git_dir) = git_dir(Path::new(&dir)) {
        for name in ["HEAD", "index"] {
            let path = git_dir.join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    println!("cargo:rustc-env=KLON_COMMIT={}", commit(&dir));
}

/// The git directory of `root`. In a linked worktree `.git` is a file that
/// holds `gitdir: <path>`, so the watched `HEAD` lives there.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.trim().strip_prefix("gitdir: ")?;
    Some(root.join(target))
}

fn commit(dir: &str) -> String {
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
