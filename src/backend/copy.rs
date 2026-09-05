//! The `copy` backend: a single-thread `std::fs` copy that keeps mode, mtime, and symlinks.
//! It never creates a hardlink (R4).

use crate::{Error, Result};
use ignore::gitignore::Gitignore;
use std::collections::HashSet;
use std::fs::{self, FileTimes};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory names that klon never clones at the top level of golden.
/// They hold other worktrees or harness state, not project files.
const TOP_LEVEL_SKIP: &[&str] = &[".claude/worktrees", ".t3"];

/// Paths that the copy leaves out. Every path is absolute and normalized.
pub struct Exclusions {
    exact: HashSet<PathBuf>,
    klonignore: Option<Gitignore>,
    golden: PathBuf,
}

impl Exclusions {
    pub fn new(golden: &Path, exact: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut set: HashSet<PathBuf> = exact.into_iter().collect();
        for name in TOP_LEVEL_SKIP {
            set.insert(golden.join(name));
        }
        Exclusions {
            exact: set,
            klonignore: load_klonignore(golden),
            golden: golden.to_path_buf(),
        }
    }

    /// True when the copy must skip `path`. A `.git` entry is skipped at every depth (R39).
    pub fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        if path.file_name().is_some_and(|n| n == ".git") || self.exact.contains(path) {
            return true;
        }
        match (&self.klonignore, path.strip_prefix(&self.golden)) {
            (Some(ignore), Ok(rel)) => ignore.matched_path_or_any_parents(rel, is_dir).is_ignore(),
            _ => false,
        }
    }
}

/// Read `<golden>/.klonignore` when it exists. It uses gitignore syntax.
fn load_klonignore(golden: &Path) -> Option<Gitignore> {
    let file = golden.join(".klonignore");
    if !file.is_file() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(golden);
    builder.add(&file);
    builder.build().ok()
}

/// Copy the children of `src` into the existing directory `dst`.
pub fn clone_tree(src: &Path, dst: &Path, exclude: &Exclusions) -> Result<()> {
    copy_children(src, dst, exclude)
}

fn copy_children(src: &Path, dst: &Path, exclude: &Exclusions) -> Result<()> {
    let entries = fs::read_dir(src).map_err(Error::io(format!("read {}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", src.display())))?;
        let from = entry.path();
        let meta =
            fs::symlink_metadata(&from).map_err(Error::io(format!("stat {}", from.display())))?;
        let kind = meta.file_type();
        if exclude.excludes(&from, kind.is_dir()) {
            continue;
        }
        let to = dst.join(entry.file_name());
        if kind.is_symlink() {
            let target =
                fs::read_link(&from).map_err(Error::io(format!("readlink {}", from.display())))?;
            std::os::unix::fs::symlink(&target, &to)
                .map_err(Error::io(format!("symlink {}", to.display())))?;
            set_symlink_times(&to, &meta)?;
        } else if kind.is_dir() {
            fs::create_dir(&to).map_err(Error::io(format!("mkdir {}", to.display())))?;
            fs::set_permissions(&to, meta.permissions())
                .map_err(Error::io(format!("chmod {}", to.display())))?;
            copy_children(&from, &to, exclude)?;
            set_times(&to, &meta)?;
        } else if kind.is_file() {
            fs::copy(&from, &to).map_err(Error::io(format!("copy {}", from.display())))?;
            set_times(&to, &meta)?;
        } else {
            eprintln!("klon: skip special file {}", from.display());
        }
    }
    Ok(())
}

/// Give `path` the access and modification times of `meta`. Works on files and directories.
fn set_times(path: &Path, meta: &fs::Metadata) -> Result<()> {
    let mut times = FileTimes::new();
    if let Ok(m) = meta.modified() {
        times = times.set_modified(m);
    }
    if let Ok(a) = meta.accessed() {
        times = times.set_accessed(a);
    }
    fs::File::open(path)
        .and_then(|f| f.set_times(times))
        .map_err(Error::io(format!("set mtime {}", path.display())))
}

/// Give a symlink the times of `meta` without following it.
fn set_symlink_times(path: &Path, meta: &fs::Metadata) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let to_spec = |t: std::io::Result<SystemTime>| {
        let d = t
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .unwrap_or_default();
        libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        }
    };
    let times = [to_spec(meta.accessed()), to_spec(meta.modified())];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::klon(format!("path holds a NUL byte: {}", path.display())))?;
    // SAFETY: `c_path` is a valid NUL-terminated string and `times` holds two timespec values.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(Error::io(format!("set symlink mtime {}", path.display()))(
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}
