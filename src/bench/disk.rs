//! M5: the unique bytes of one idle tree (spec §7 C31; handoff §8).
//!
//! "Unique" means the bytes that this tree alone holds. A copy-on-write tree
//! shares most of its extents with golden, so its unique share is the diff plus
//! the inode metadata; a plain copy shares nothing.
//!
//! Two methods, and the record says which one answered:
//!
//! | Method | When | Meaning |
//! |---|---|---|
//! | `btrfs-fi-du` | The tree sits on btrfs and `btrfs` is present | Exact. `btrfs fi du -s --raw` reports the exclusive extent bytes |
//! | `upper-bound` | Every other host | The allocated size of the tree. Nothing shared is subtracted, so the true figure is this one or less |
//!
//! The fallback counts allocated blocks, not file lengths. A tree of many tiny
//! files costs a block each, so the sum of the lengths would sit far below what
//! the tree really uses and the figure would not be an upper bound at all. It
//! counts directories and symlinks for the same reason: their blocks are part
//! of what one tree costs.
//!
//! The walk covers the whole tree, not the ignored directories alone. A
//! baseline worktree holds no ignored state at all, so a figure over the
//! ignored directories would report zero for it and the two rows could not be
//! compared.

use crate::backend::btrfs;
use crate::probe;
use std::fs;
use std::path::Path;
use std::process::Command;

/// The exact method: `btrfs fi du -s --raw`.
pub const BTRFS: &str = "btrfs-fi-du";

/// The fallback method: the apparent size of the tree.
pub const UPPER_BOUND: &str = "upper-bound";

/// What one tree costs on disk.
pub struct Usage {
    pub bytes: u64,
    pub method: &'static str,
}

/// The unique bytes of `tree`. The answer always has a figure: a host without
/// `btrfs` falls back to the apparent size instead of reporting nothing.
pub fn measure(tree: &Path) -> Usage {
    if let Some(bytes) = exclusive(tree) {
        return Usage {
            bytes,
            method: BTRFS,
        };
    }
    Usage {
        bytes: allocated(tree),
        method: UPPER_BOUND,
    }
}

/// The exclusive extent bytes of `tree`, when this host can measure them.
fn exclusive(tree: &Path) -> Option<u64> {
    if probe::filesystem(tree) != "btrfs" {
        return None;
    }
    let program = btrfs::tool()?;
    let output = Command::new(program)
        .args(["filesystem", "du", "-s", "--raw"])
        .arg(tree)
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "klon: bench: btrfs fi du failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    parse_exclusive(&String::from_utf8_lossy(&output.stdout))
}

/// The `Exclusive` column of a `btrfs filesystem du -s --raw` report.
///
/// ```text
///      Total   Exclusive  Set shared  Filename
///   10485760        4096    10481664  /mnt/klon
/// ```
///
/// The header names the columns and the one data line carries the figures. A
/// column that btrfs could not compute holds `-`, which parses to nothing and
/// sends the caller to the fallback.
fn parse_exclusive(text: &str) -> Option<u64> {
    let mut last = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[0] == "Total" {
            continue;
        }
        last = fields[1].parse::<u64>().ok();
    }
    last
}

/// The allocated size of everything below `root`, in bytes.
///
/// `st_blocks` counts the 512-byte blocks that an entry really occupies, so a
/// one-byte file costs a whole block here as it does on the disk, and a sparse
/// file costs only what it wrote. Summing `len()` instead would report far less
/// than a tree of small files uses, and the figure would not bound anything.
///
/// A tree that shares extents with golden still counts them here, which is why
/// the figure is an upper bound and not the answer. `btrfs fi du` gives the
/// answer where the filesystem can.
fn allocated(root: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    // The root's own directory blocks belong to the figure too.
    let mut total = fs::symlink_metadata(root).map_or(0, |meta| meta.blocks() * BLOCK);
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            total += allocated(&path);
        } else {
            total += meta.blocks() * BLOCK;
        }
    }
    total
}

/// The unit of `st_blocks`. POSIX fixes it at 512 bytes, whatever the block
/// size of the filesystem is.
const BLOCK: u64 = 512;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exclusive_column_comes_from_the_data_line() {
        let report = "     Total   Exclusive  Set shared  Filename\n\
                      \x2010485760        4096    10481664  /mnt/klon\n";
        assert_eq!(parse_exclusive(report), Some(4096));
        // A btrfs that could not compute the column sends the caller to the
        // fallback instead of reporting a wrong figure.
        let unknown = "     Total   Exclusive  Set shared  Filename\n\
                       \x2010485760           -           -  /mnt/klon\n";
        assert_eq!(parse_exclusive(unknown), None);
        assert_eq!(parse_exclusive(""), None);
    }

    /// The fallback must bound what a tree of small files really uses. Summing
    /// the lengths would report three hundred bytes for a tree that occupies
    /// several blocks, and a bound below the truth bounds nothing.
    #[test]
    fn the_fallback_counts_allocated_blocks_and_bounds_the_lengths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("one.bin"), vec![0u8; 100]).unwrap();
        fs::write(root.join("a/two.bin"), vec![0u8; 250]).unwrap();
        fs::write(root.join("a/b/three.bin"), vec![0u8; 5]).unwrap();
        std::os::unix::fs::symlink("one.bin", root.join("link")).unwrap();

        let bytes = allocated(root);
        assert!(
            bytes >= 355,
            "{bytes} must be at least the 355 bytes the files hold"
        );
        assert_eq!(bytes % BLOCK, 0, "the figure counts whole blocks");
        // Three files of a few hundred bytes each occupy a block apiece on
        // every filesystem klon supports.
        assert!(
            bytes >= 3 * 4096,
            "{bytes} must cover a block per file, not the sum of the lengths"
        );
        assert_eq!(allocated(&root.join("nothing")), 0);
    }

    /// The measurement always answers, whatever this host runs.
    #[test]
    fn a_measurement_names_its_method() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("x.bin"), vec![7u8; 1024]).unwrap();
        let usage = measure(tmp.path());
        assert!([BTRFS, UPPER_BOUND].contains(&usage.method));
        if usage.method == UPPER_BOUND {
            assert!(usage.bytes >= 1024, "found {}", usage.bytes);
        }
    }
}
