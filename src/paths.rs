//! Path helpers: absolute paths without a requirement that the path exists.

use std::path::{Component, Path, PathBuf};

/// Make `path` absolute and resolve symlinks in the deepest ancestor that exists.
/// The tail that does not exist yet is appended unchanged. `..` and `.` are folded.
pub fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut existing = normalized.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                existing = parent;
            }
            _ => return normalized,
        }
    }
    let mut out = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    for name in tail.iter().rev() {
        out.push(name);
    }
    out
}

/// The default klon path: `../<repo>.wt/<branch>` next to golden.
pub fn default_klon_path(golden: &Path, branch: &str) -> PathBuf {
    let repo = golden
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let parent = golden.parent().unwrap_or(golden);
    parent.join(format!("{repo}.wt")).join(branch)
}

/// True when `dir` exists and holds at least one entry.
pub fn is_non_empty_dir(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}
