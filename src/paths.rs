//! Path helpers: absolute paths without a requirement that the path exists.

use std::path::{Component, Path, PathBuf};

/// Make `path` absolute and resolve symlinks in the deepest ancestor that exists.
/// The tail that does not exist yet is appended unchanged. `..` and `.` are folded.
pub fn absolute(path: &Path) -> crate::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(crate::Error::io("read the current directory"))?
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => {
                resolved.push(other.as_os_str());
                match std::fs::canonicalize(&resolved) {
                    Ok(path) => resolved = path,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(crate::Error::io(format!("resolve {}", resolved.display()))(
                            err,
                        ))
                    }
                }
            }
        }
    }
    Ok(resolved)
}

/// True when `dir` exists and holds at least one entry.
pub fn is_non_empty_dir(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}
