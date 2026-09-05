//! The `copy` backend: a single-thread `std::fs` copy that keeps mode, mtime,
//! and symlinks. It never creates a hardlink (R4). It works on every
//! filesystem, so it is the last entry in the probe order and the answer on
//! ext4 (handoff §4 "Backends").

use super::{set_symlink_times, set_times, Backend, Exclusions, Timing};
use crate::{probe, Error, Result};
use std::fs;
use std::path::Path;
use std::time::Instant;

/// The universal fallback backend.
pub struct Copy;

impl Backend for Copy {
    fn name(&self) -> &'static str {
        "copy"
    }

    fn probe(&self, golden: &Path) -> probe::Status {
        super::verify::run(self, golden)
    }

    fn clone(&self, src: &Path, dst: &Path, excludes: &Exclusions) -> Result<Timing> {
        let started = Instant::now();
        let mut entries = 0u64;
        copy_children(src, dst, excludes, &mut entries)?;
        Ok(Timing {
            duration: started.elapsed(),
            entries,
        })
    }
}

fn copy_children(src: &Path, dst: &Path, exclude: &Exclusions, count: &mut u64) -> Result<()> {
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
            *count += 1;
        } else if kind.is_dir() {
            fs::create_dir(&to).map_err(Error::io(format!("mkdir {}", to.display())))?;
            // Keep the new directory writable until its children are complete.
            copy_children(&from, &to, exclude, count)?;
            set_times(&to, &meta)?;
            fs::set_permissions(&to, meta.permissions())
                .map_err(Error::io(format!("chmod {}", to.display())))?;
            *count += 1;
        } else if kind.is_file() {
            fs::copy(&from, &to).map_err(Error::io(format!("copy {}", from.display())))?;
            set_times(&to, &meta)?;
            *count += 1;
        } else {
            eprintln!("klon: skip special file {}", from.display());
        }
    }
    Ok(())
}
