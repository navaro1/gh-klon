//! The `copy` backend: a single-thread `std::fs` copy that keeps mode, mtime,
//! and symlinks. It never creates a hardlink (R4). It works on every
//! filesystem, so it is the last entry in the probe order and the answer on
//! ext4 (handoff §4 "Backends").
//!
//! C12 gave the backend three more jobs (R36, R41):
//!
//! | Job | What it does |
//! |---|---|
//! | survey | walk golden once and count the bytes and files per top-level entry |
//! | estimate | answer the survey total, which the free-space guard multiplies by 1.2 |
//! | progress | print one in-place line while the inline copy runs |
//!
//! The survey feeds all three, so `add` walks golden once. The per-directory
//! numbers also decide which ignored directories the warm process takes
//! (`crate::warm`), so the inline copy can return while the big ones fill in
//! the background.

use super::{set_symlink_times, set_times, Backend, Exclusions, Timing};
use crate::{probe, Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
            tick(meta.len());
            *count += 1;
        } else {
            eprintln!("klon: skip special file {}", from.display());
        }
    }
    Ok(())
}

/// Copy the directory `src` to the new directory `dst`, mode and mtime kept.
/// `dst` must not exist. The warm process uses it for one ignored directory of
/// golden (R36).
pub fn copy_tree(src: &Path, dst: &Path, exclude: &Exclusions) -> Result<u64> {
    let meta = fs::symlink_metadata(src).map_err(Error::io(format!("stat {}", src.display())))?;
    fs::create_dir(dst).map_err(Error::io(format!("mkdir {}", dst.display())))?;
    let mut count = 0u64;
    copy_children(src, dst, exclude, &mut count)?;
    set_times(dst, &meta)?;
    fs::set_permissions(dst, meta.permissions())
        .map_err(Error::io(format!("chmod {}", dst.display())))?;
    Ok(count)
}

// --- The survey ----------------------------------------------------------------

/// The size of one tree, in three numbers.
///
/// `bytes` is the apparent size: what the progress line reports, because that
/// is what a person sees a copy move. `disk` is what the tree really costs a
/// filesystem, from `st_blocks`: a directory of 100 000 one-byte files holds
/// 100 KB of content and needs hundreds of megabytes of blocks and inodes, so
/// the free-space guard has to weigh blocks or it would let that clone start
/// on a filesystem that cannot hold it (R41). `files` counts regular files,
/// which is what the progress line counts down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub bytes: u64,
    pub disk: u64,
    pub files: u64,
}

/// The unit of `st_blocks`, fixed by POSIX whatever the block size of the
/// filesystem is.
const BLOCK: u64 = 512;

impl Sizes {
    /// Add one entry: a file, a directory, or a symlink.
    fn add(&mut self, meta: &fs::Metadata) {
        use std::os::unix::fs::MetadataExt;
        self.disk = self
            .disk
            .saturating_add(meta.blocks().saturating_mul(BLOCK));
        if meta.is_file() {
            self.bytes = self.bytes.saturating_add(meta.len());
            self.files += 1;
        }
    }

    /// This size less `other`, never below zero. `add` takes the warm
    /// directories out of the inline total this way.
    pub fn without(self, other: Sizes) -> Sizes {
        Sizes {
            bytes: self.bytes.saturating_sub(other.bytes),
            disk: self.disk.saturating_sub(other.disk),
            files: self.files.saturating_sub(other.files),
        }
    }
}

/// What one walk of golden found: the whole tree, and each top-level directory
/// on its own. The per-directory numbers decide the copy strategy (R36) and
/// the total drives the free-space guard (R41).
#[derive(Debug, Default)]
pub struct Survey {
    pub total: Sizes,
    /// One entry per top-level directory of golden, by name.
    pub dirs: BTreeMap<String, Sizes>,
}

/// The memoized survey. One `add` surveys one golden, so the first call walks
/// and every later call is free. The free-space guard, the copy plan, and the
/// progress line all read it, and golden is walked once for the three.
static SURVEY: OnceLock<Survey> = OnceLock::new();

/// Walk golden and count what a byte copy would write.
///
/// The walk applies the same exclusions as the clone, so a skipped worktree or
/// a `.klonignore` match never inflates the estimate. An unreadable entry is
/// counted as nothing: the estimate guards a copy, and a copy cannot write
/// what it cannot read either.
pub fn survey(golden: &Path, excludes: &Exclusions) -> &'static Survey {
    SURVEY.get_or_init(|| {
        let mut out = Survey::default();
        let Ok(entries) = fs::read_dir(golden) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let kind = meta.file_type();
            if excludes.excludes(&path, kind.is_dir()) {
                continue;
            }
            if kind.is_dir() {
                let mut sizes = Sizes::default();
                sizes.add(&meta);
                measure(&path, excludes, &mut sizes);
                out.total.bytes = out.total.bytes.saturating_add(sizes.bytes);
                out.total.disk = out.total.disk.saturating_add(sizes.disk);
                out.total.files += sizes.files;
                out.dirs
                    .insert(entry.file_name().to_string_lossy().into_owned(), sizes);
            } else {
                out.total.add(&meta);
            }
        }
        out
    })
}

/// Add every regular file below `dir` to `sizes`.
fn measure(dir: &Path, excludes: &Exclusions, sizes: &mut Sizes) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = meta.file_type();
        if excludes.excludes(&path, kind.is_dir()) {
            continue;
        }
        sizes.add(&meta);
        if kind.is_dir() {
            measure(&path, excludes, sizes);
        }
    }
}

// --- The progress line -----------------------------------------------------------

/// The shortest gap between two progress lines. A faster update would spend
/// more time on the terminal than on the copy.
const INTERVAL: Duration = Duration::from_millis(200);

/// The variable that forces the line on, for the pseudo-terminal test.
const FORCE: &str = "KLON_PROGRESS";

/// The state behind the in-place progress line. It exists only when `add`
/// armed it, so `tick` is one atomic load in every other run.
struct Progress {
    total: Sizes,
    bytes: AtomicU64,
    files: AtomicU64,
    /// The last render, and the widest line it wrote. The width pads the next
    /// line, so a shorter one never leaves the tail of a longer one behind.
    last: Mutex<(Instant, usize)>,
}

static PROGRESS: OnceLock<Progress> = OnceLock::new();

/// Turn the progress line on for the inline copy (R41).
///
/// It prints when `--json` is absent and both standard streams are terminals,
/// because a document reader and a pipe both want the stream clean. The spec
/// names stdout; stderr has to answer too, because stderr is the stream that
/// receives the line, and a redirected stderr would otherwise collect carriage
/// returns in a file. `KLON_PROGRESS=1` forces it on so a pseudo-terminal test
/// can read the line. The line goes to stderr because it reports work in
/// flight, not the result of the command.
pub fn arm_progress(total: Sizes, json: bool) {
    let forced = std::env::var(FORCE).as_deref() == Ok("1");
    let terminal = std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
    if json || total.files == 0 || !(forced || terminal) {
        return;
    }
    let _ = PROGRESS.set(Progress {
        total,
        bytes: AtomicU64::new(0),
        files: AtomicU64::new(0),
        last: Mutex::new((Instant::now() - INTERVAL, 0)),
    });
}

/// Count one copied file and render the line when the interval has passed.
fn tick(bytes: u64) {
    let Some(progress) = PROGRESS.get() else {
        return;
    };
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    progress.files.fetch_add(1, Ordering::Relaxed);
    let Ok(mut last) = progress.last.lock() else {
        return;
    };
    if last.0.elapsed() < INTERVAL {
        return;
    }
    last.0 = Instant::now();
    last.1 = progress.render(last.1, false);
}

/// End the progress line with a newline. `add` calls it after the clone, so
/// the next line of output starts at the left margin.
///
/// A clone that copied no file printed no line either: the hot spare can fill
/// the whole tree with one rename (C9), and a closing `copied 0 of ...` would
/// then report work that never happened.
pub fn finish_progress() {
    let Some(progress) = PROGRESS.get() else {
        return;
    };
    if progress.files.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Ok(mut last) = progress.last.lock() else {
        return;
    };
    last.1 = progress.render(last.1, true);
}

impl Progress {
    /// Write the line over the previous one and answer its width. `end` adds
    /// the closing newline instead of leaving the cursor on the line.
    fn render(&self, widest: usize, end: bool) -> usize {
        let copied = self.bytes.load(Ordering::Relaxed);
        let done = self.files.load(Ordering::Relaxed);
        let line = format!(
            "klon: copied {copied} of {}, {} files remaining",
            self.total.bytes,
            self.total.files.saturating_sub(done)
        );
        let pad = widest.saturating_sub(line.len());
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}{:pad$}", "");
        if end {
            let _ = writeln!(err);
        }
        let _ = err.flush();
        line.len().max(widest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_never_falls_below_zero() {
        let small = Sizes {
            bytes: 1,
            disk: 1,
            files: 1,
        };
        let big = Sizes {
            bytes: 10,
            disk: 10,
            files: 10,
        };
        assert_eq!(small.without(big), Sizes::default());
    }

    #[test]
    fn subtracting_the_warm_directory_leaves_the_inline_part() {
        let total = Sizes {
            bytes: 100,
            disk: 200,
            files: 20,
        };
        let warm = Sizes {
            bytes: 90,
            disk: 180,
            files: 15,
        };
        assert_eq!(
            total.without(warm),
            Sizes {
                bytes: 10,
                disk: 20,
                files: 5,
            }
        );
    }

    #[test]
    fn a_directory_of_tiny_files_costs_more_disk_than_content() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        for i in 0..40 {
            fs::write(dir.path().join(format!("f{i}")), b"x").expect("write");
        }
        let mut sizes = Sizes::default();
        measure(dir.path(), &Exclusions::new(dir.path(), []), &mut sizes);
        assert_eq!(sizes.files, 40);
        assert_eq!(sizes.bytes, 40);
        assert!(
            sizes.disk > sizes.bytes,
            "40 one-byte files need more than 40 bytes of blocks, not {}",
            sizes.disk
        );
    }
}
