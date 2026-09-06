//! The free-space guard (R41, spec §7 C12).
//!
//! A byte backend writes a copy of golden into the klon. `add` asks the
//! backend how many bytes that is, reads the free space of the filesystem that
//! will hold the klon, and refuses before the first repository change when the
//! space is below 1.2 times the estimate. The refusal names the shortfall, so
//! a person knows how much to free.
//!
//! A backend that shares blocks writes almost nothing and answers 0. The check
//! then does no work at all.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// The safety factor over the estimate, as a fraction. The clone writes the
/// estimate; the extra fifth covers the metadata, the index copy, and the
/// build output of the first command.
const FACTOR_NUMERATOR: u64 = 12;
const FACTOR_DENOMINATOR: u64 = 10;

/// The variable that replaces the free-space reading in a test.
const OVERRIDE: &str = "KLON_TEST_FREE_BYTES";

/// Refuse when the filesystem that will hold `target` cannot take 1.2 times
/// `estimate`. An estimate of 0 means the backend writes no bytes, so the
/// check returns at once.
pub fn check(target: &Path, estimate: u64) -> Result<()> {
    if estimate == 0 {
        return Ok(());
    }
    let need = estimate
        .saturating_mul(FACTOR_NUMERATOR)
        .saturating_div(FACTOR_DENOMINATOR);
    let free = free_bytes(target)?;
    if free >= need {
        return Ok(());
    }
    Err(Error::klon(format!(
        "not enough space: need {need} bytes (1.2 × {estimate}), free {free} bytes, short by {} bytes",
        need - free
    )))
}

/// The free space a normal user may still take on the filesystem of `target`.
///
/// `target` does not exist yet, so the call climbs to the deepest ancestor
/// that does. `KLON_TEST_FREE_BYTES` replaces the reading, which lets a test
/// state a shortfall without a loop image.
pub fn free_bytes(target: &Path) -> Result<u64> {
    if let Ok(text) = std::env::var(OVERRIDE) {
        return text
            .trim()
            .parse()
            .map_err(|_| Error::klon(format!("{OVERRIDE} must be a byte count: {text}")));
    }
    let dir = nearest_existing(target)
        .ok_or_else(|| Error::klon(format!("no existing directory above {}", target.display())))?;
    statvfs_available(&dir)
}

/// The deepest ancestor of `path` that exists, `path` itself included.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut walk = path.to_path_buf();
    loop {
        if walk.exists() {
            return Some(walk);
        }
        if !walk.pop() {
            return None;
        }
    }
}

/// `statvfs(dir).f_bavail * f_frsize`: the bytes an unprivileged writer may
/// still use. `f_bfree` counts the reserved blocks too, which a normal user
/// cannot have, so the guard would then pass on a filesystem that refuses the
/// write.
///
/// The two fields have different widths per platform: on Linux both are 64
/// bits, on macOS `f_bavail` is 32. The cast is a no-op on one target and a
/// widening on the other, so clippy sees a needless cast on exactly one of
/// them.
#[allow(clippy::unnecessary_cast)]
fn statvfs_available(dir: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let text = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| Error::klon(format!("path with a NUL byte: {}", dir.display())))?;
    // SAFETY: `text` is NUL-terminated and `buffer` is a whole `statvfs`.
    let mut buffer: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: the kernel writes only into `buffer` and reads only `text`.
    let code = unsafe { libc::statvfs(text.as_ptr(), &mut buffer) };
    if code != 0 {
        return Err(Error::io(format!(
            "read the free space of {}",
            dir.display()
        ))(std::io::Error::last_os_error()));
    }
    let blocks = buffer.f_bavail as u64;
    let size = buffer.f_frsize as u64;
    Ok(blocks.saturating_mul(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_estimate_of_zero_never_reads_the_filesystem() {
        assert!(check(Path::new("/nonexistent/klon"), 0).is_ok());
    }

    #[test]
    fn the_nearest_existing_ancestor_of_a_new_path_is_a_directory() {
        let found = nearest_existing(Path::new("/tmp/klon-does-not-exist/a/b")).expect("ancestor");
        assert!(found.exists(), "{} must exist", found.display());
    }

    #[test]
    fn the_root_filesystem_reports_some_free_space() {
        let free = statvfs_available(Path::new("/")).expect("statvfs");
        assert!(free > 0, "the root filesystem reported {free} free bytes");
    }
}
