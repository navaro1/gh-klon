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
//! | `upper-bound` | Every other host | The apparent size of the tree. Nothing shared is subtracted, so the true figure is this one or less |
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
        bytes: apparent(tree),
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

/// The apparent size of every regular file below `root`, in bytes. A directory
/// and a symlink cost inode metadata that no portable call reports, so neither
/// adds to the figure; that keeps this an upper bound of the shared case and a
/// close reading of the copied case.
fn apparent(root: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            total += apparent(&path);
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

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

    #[test]
    fn the_apparent_size_counts_every_file_below_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("one.bin"), vec![0u8; 100]).unwrap();
        fs::write(root.join("a/two.bin"), vec![0u8; 250]).unwrap();
        fs::write(root.join("a/b/three.bin"), vec![0u8; 5]).unwrap();
        std::os::unix::fs::symlink("one.bin", root.join("link")).unwrap();
        assert_eq!(apparent(root), 355, "a symlink adds no apparent bytes");
        assert_eq!(apparent(&root.join("nothing")), 0);
    }

    /// The measurement always answers, whatever this host runs.
    #[test]
    fn a_measurement_names_its_method() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("x.bin"), vec![7u8; 1024]).unwrap();
        let usage = measure(tmp.path());
        assert!([BTRFS, UPPER_BOUND].contains(&usage.method));
        if usage.method == UPPER_BOUND {
            assert_eq!(usage.bytes, 1024);
        }
    }
}
