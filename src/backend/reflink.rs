//! The `reflink-walk` backend (R35, handoff §4 "Backends"): one `FICLONE` per
//! file with four workers.
//!
//! The walk runs in three phases:
//!
//! | Phase | Threads | Work |
//! |---|---|---|
//! | 1 | one | read every directory, create the directories and the symlinks, list the files |
//! | 2 | four | clone each file, then restore its mode and its mtime |
//! | 3 | one | give each directory its mode and its mtime, deepest first |
//!
//! Phase 1 is cheap: it reads directories and creates empty ones. Phase 2 holds
//! the whole cost, so the four workers sit where the time is. Phase 3 must run
//! last, because a write to a child changes the mtime of its directory, and a
//! narrow source mode must not block the workers.
//!
//! `FICLONE` sets the destination mtime to the current time, so every clone
//! restores the source mtime (**V**, handoff §4). Four workers is the measured
//! optimum: 116k files took 3.3 s with 4 threads and 9 s with 10.
//!
//! `registry` holds this backend on Linux only, because C6 owns the APFS clone.
//! The walk below therefore has no caller on macOS, while `capability` still
//! answers the `doctor` row on every platform. One allow keeps the file
//! readable instead of a `cfg` on each item.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::{set_symlink_times, set_times, Backend, Exclusions, Timing};
use crate::{paths, probe, Error, Result};
use rayon::prelude::*;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The measured optimum worker count for the clone walk (handoff §4).
const WORKERS: usize = 4;

/// The bytes of the capability test file. It is above the btrfs inline-extent
/// limit, so a small file cannot make a capable filesystem look incapable.
const PROBE_BYTES: usize = 8192;

/// The copy-on-write clone backend for XFS with `reflink=1`, bcachefs, a btrfs
/// plain directory, and ZFS 2.2.6 or newer.
pub struct Reflink;

impl Backend for Reflink {
    fn name(&self) -> &'static str {
        "reflink-walk"
    }

    fn probe(&self, golden: &Path) -> probe::Status {
        // The capability test runs first, so a filesystem without `FICLONE`
        // answers with the documented short reason instead of a clone error.
        match capability(golden) {
            probe::Status::Present(_) => super::verify::run(self, golden),
            probe::Status::Absent(_) => probe::Status::Absent("reflink unsupported".to_string()),
            broken => broken,
        }
    }

    /// `FICLONE` shares blocks, so both files must live on one filesystem.
    fn same_filesystem_only(&self) -> bool {
        true
    }

    fn clone(&self, src: &Path, dst: &Path, excludes: &Exclusions) -> Result<Timing> {
        let started = Instant::now();
        let mut plan = Plan::default();
        collect(src, dst, excludes, &mut plan)?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(WORKERS)
            .build()
            .map_err(|err| Error::klon(format!("cannot start the clone workers: {err}")))?;
        pool.install(|| plan.files.par_iter().try_for_each(clone_one))?;
        // Deepest first: a directory keeps its mtime only after its children
        // are complete.
        for dir in plan.dirs.iter().rev() {
            let meta = fs::symlink_metadata(&dir.from)
                .map_err(Error::io(format!("stat {}", dir.from.display())))?;
            set_times(&dir.to, &meta)?;
            fs::set_permissions(&dir.to, meta.permissions())
                .map_err(Error::io(format!("chmod {}", dir.to.display())))?;
        }
        Ok(Timing {
            duration: started.elapsed(),
            entries: (plan.files.len() + plan.dirs.len() + plan.links) as u64,
        })
    }
}

/// One source and destination pair.
struct Pair {
    from: PathBuf,
    to: PathBuf,
}

/// The work that phase 1 found.
#[derive(Default)]
struct Plan {
    /// Every directory, parents before children.
    dirs: Vec<Pair>,
    /// Every regular file.
    files: Vec<Pair>,
    /// The symlinks are finished in phase 1; only the count survives.
    links: usize,
}

/// Phase 1: read `src`, create the directories and the symlinks under `dst`,
/// and list the files.
fn collect(src: &Path, dst: &Path, exclude: &Exclusions, plan: &mut Plan) -> Result<()> {
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
            plan.links += 1;
        } else if kind.is_dir() {
            // Create it with the default mode, so the workers can write into
            // it. Phase 3 narrows it back to the source mode.
            fs::create_dir(&to).map_err(Error::io(format!("mkdir {}", to.display())))?;
            plan.dirs.push(Pair {
                from: from.clone(),
                to: to.clone(),
            });
            collect(&from, &to, exclude, plan)?;
        } else if kind.is_file() {
            plan.files.push(Pair { from, to });
        } else {
            eprintln!("klon: skip special file {}", from.display());
        }
    }
    Ok(())
}

/// Phase 2: one `FICLONE` for one file, then its mode and its source mtime.
fn clone_one(pair: &Pair) -> Result<()> {
    reflink_copy::reflink(&pair.from, &pair.to)
        .map_err(Error::io(format!("reflink {}", pair.from.display())))?;
    let meta = fs::symlink_metadata(&pair.from)
        .map_err(Error::io(format!("stat {}", pair.from.display())))?;
    // `reflink` gives the clone the source mode. Set it again, so a platform
    // that ignores the creation mode still produces an equal manifest.
    fs::set_permissions(&pair.to, meta.permissions())
        .map_err(Error::io(format!("chmod {}", pair.to.display())))?;
    set_times(&pair.to, &meta)
}

// --- The capability probe ----------------------------------------------------

/// Try one `FICLONE` on a file pair next to `golden`. `doctor` prints this row,
/// and the backend probe reads it before it clones the fixture.
pub fn capability(golden: &Path) -> probe::Status {
    let dir = match TrialDir::next_to(golden) {
        Ok(dir) => dir,
        Err(why) => return probe::Status::Broken(why),
    };
    let source = dir.path().to_path_buf();
    answer(trial(&source, dir.path()))
}

/// `capability` from golden into a directory that may live on another
/// filesystem. `select` calls it when the device ids of the two ends differ,
/// because two btrfs subvolumes carry two device ids and still clone.
pub fn capability_across(golden: &Path, destination: &Path) -> probe::Status {
    let from = match TrialDir::next_to(golden) {
        Ok(dir) => dir,
        Err(why) => return probe::Status::Broken(why),
    };
    let to = match TrialDir::inside(destination) {
        Ok(dir) => dir,
        Err(why) => return probe::Status::Broken(why),
    };
    answer(trial(from.path(), to.path()))
}

/// Turn one trial into the probe result that `doctor` and `select` read.
fn answer(result: std::result::Result<(), Trial>) -> probe::Status {
    match result {
        Ok(()) => probe::Status::Present("FICLONE works on this filesystem".to_string()),
        Err(Trial::Unsupported(name)) => {
            probe::Status::Absent(format!("reflink unsupported: {name}"))
        }
        Err(Trial::Failed(why)) => probe::Status::Broken(why),
    }
}

/// Why the capability test did not succeed.
enum Trial {
    /// The filesystem answered that it cannot clone. The string names the errno.
    Unsupported(String),
    /// Something else went wrong. The string says what.
    Failed(String),
}

/// Write one test file in `from_dir` and clone it into `to_dir`.
fn trial(from_dir: &Path, to_dir: &Path) -> std::result::Result<(), Trial> {
    let from = from_dir.join("from");
    let to = to_dir.join("to");
    let write = || -> std::io::Result<()> {
        let mut file = fs::File::create(&from)?;
        file.write_all(&vec![0x6bu8; PROBE_BYTES])?;
        file.sync_all()
    };
    write().map_err(|err| Trial::Failed(format!("cannot write the reflink test file: {err}")))?;
    reflink_copy::reflink(&from, &to).map_err(|err| classify(&err))
}

/// An errno that means "this filesystem cannot clone" becomes `Unsupported`.
/// Every other errno is a real failure that `doctor` must show.
fn classify(err: &std::io::Error) -> Trial {
    let name = match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) => "EOPNOTSUPP",
        Some(libc::ENOTTY) => "ENOTTY",
        Some(libc::EXDEV) => "EXDEV",
        Some(libc::EINVAL) => "EINVAL",
        Some(libc::ENOSYS) => "ENOSYS",
        _ => return Trial::Failed(format!("FICLONE failed: {err}")),
    };
    Trial::Unsupported(name.to_string())
}

/// A directory next to golden's `.wt` root that removes itself. It sits on
/// golden's filesystem, which is the only place where the answer is true for
/// the real clone.
struct TrialDir(PathBuf);

impl TrialDir {
    /// A sibling of golden's `.wt` root, so the test runs on golden's
    /// filesystem.
    fn next_to(golden: &Path) -> std::result::Result<TrialDir, String> {
        let root = paths::default_wt_root(golden);
        let stem = root.file_name().unwrap_or_default().to_string_lossy();
        TrialDir::create(&root.with_file_name(format!("{stem}.reflink")))
    }

    /// A child of `parent`, so the test runs on the filesystem of `parent`.
    fn inside(parent: &Path) -> std::result::Result<TrialDir, String> {
        TrialDir::create(&parent.join(".klon-reflink"))
    }

    /// Create the first free `<prefix>.<pid>.<n>` directory.
    fn create(prefix: &Path) -> std::result::Result<TrialDir, String> {
        let pid = std::process::id();
        for n in 0..64u32 {
            let mut candidate = prefix.as_os_str().to_os_string();
            candidate.push(format!(".{pid}.{n}"));
            let candidate = PathBuf::from(candidate);
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(TrialDir(candidate)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(format!("cannot create {}: {err}", candidate.display()));
                }
            }
        }
        Err(format!(
            "cannot create a reflink test directory at {}",
            prefix.display()
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TrialDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability answer must be one of the three documented shapes, and
    /// an absent answer must name the errno. The development laptop runs ext4,
    /// where the answer is `Absent(EOPNOTSUPP)`.
    #[test]
    fn the_capability_probe_answers_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        let status = capability(&golden);
        match &status {
            probe::Status::Present(detail) => assert!(detail.contains("FICLONE")),
            probe::Status::Absent(detail) => {
                assert!(detail.starts_with("reflink unsupported: "), "{detail}")
            }
            probe::Status::Broken(detail) => panic!("the capability probe broke: {detail}"),
        }
        let leftovers: Vec<PathBuf> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p != &golden)
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// The selection reason on a filesystem without `FICLONE` is the short
    /// documented text, not the errno detail.
    #[test]
    fn an_unsupported_filesystem_gives_the_documented_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        let status = Reflink.probe(&golden);
        if let probe::Status::Absent(reason) = &status {
            assert_eq!(reason, "reflink unsupported");
        }
    }

    #[test]
    fn every_documented_errno_reads_as_unsupported() {
        for code in [
            libc::EOPNOTSUPP,
            libc::ENOTTY,
            libc::EXDEV,
            libc::EINVAL,
            libc::ENOSYS,
        ] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(matches!(classify(&err), Trial::Unsupported(_)), "{code}");
        }
        let other = std::io::Error::from_raw_os_error(libc::ENOSPC);
        assert!(matches!(classify(&other), Trial::Failed(_)));
    }
}
