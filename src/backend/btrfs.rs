//! The `btrfs-snapshot` backend (spec §7 C7, handoff §4 "Backends").
//!
//! A btrfs snapshot of a subvolume is one ioctl. It costs about 5 ms whatever
//! the file count, and it reproduces every mode, every mtime, and every byte,
//! so the ignored manifest of a klon equals golden's without a walk.
//!
//! The backend needs golden to be a user-owned subvolume. `gh klon init`
//! (`src/cli/init.rs`) converts a plain golden directory into one.
//!
//! Three host facts from the S1 spike (`docs/spikes/2026-btrfs-loop-volume.md`)
//! shape this file:
//!
//! | Fact | Consequence |
//! |---|---|
//! | `btrfs subvolume show` and `list` need `CAP_SYS_ADMIN` | klon detects a subvolume with `stat`: inode 256 plus a device number that differs from the parent |
//! | `btrfs subvolume delete` needs `CAP_SYS_ADMIN` without `user_subvol_rm_allowed` | `delete` reads the mount options first and falls back to the background `rm -rf` |
//! | udisks refuses the `user_subvol_rm_allowed` mount option | the fallback is the normal path on a desktop, not the exception |
//!
//! The walk below has no caller on macOS, where `registry` never holds this
//! backend. One allow keeps the file readable instead of a `cfg` on each item.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::{Backend, Exclusions, Timing};
use crate::{probe, process, Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// The inode number of every btrfs subvolume root.
const SUBVOLUME_INODE: u64 = 256;

/// The mount option that lets an unprivileged user delete a subvolume.
const RM_ALLOWED: &str = "user_subvol_rm_allowed";

/// The copy-on-write clone backend for a golden that is a btrfs subvolume.
pub struct BtrfsSnapshot;

impl Backend for BtrfsSnapshot {
    fn name(&self) -> &'static str {
        "btrfs-snapshot"
    }

    /// A snapshot shares extents, so both ends must sit on one btrfs
    /// filesystem. Two subvolumes carry two device numbers and still share, so
    /// `select` falls back to the real `FICLONE` test instead of the device
    /// comparison alone.
    fn same_filesystem_only(&self) -> bool {
        true
    }

    /// Only a btrfs golden. Every other filesystem drops the backend before the
    /// probe, so its rejection never joins the selection reason on ext4.
    fn applies(&self, golden: &Path) -> bool {
        probe::filesystem(golden) == "btrfs"
    }

    /// The cheap host checks run first, so a filesystem without btrfs answers
    /// with a short reason instead of a failed `btrfs` process. Only a golden
    /// that passes all of them pays for the fixture clone.
    fn probe(&self, golden: &Path) -> probe::Status {
        if let Some(why) = unusable(golden) {
            return probe::Status::Absent(why);
        }
        // The fixture source must be a subvolume, else the snapshot cannot
        // read it. The probe therefore prepares its own source directory.
        super::verify::run_with(self, golden, &create_subvolume)
    }

    fn clone(&self, src: &Path, dst: &Path, excludes: &Exclusions) -> Result<Timing> {
        let started = Instant::now();
        // A snapshot creates its own destination, so the empty directory that
        // `git worktree add --no-checkout` left must go first. `add` reads the
        // `.git` file before the clone and writes it again after, so removing
        // it here loses nothing.
        clear_destination(dst)?;
        snapshot(src, dst)?;
        // The snapshot copied every path, golden's `.git` directory included.
        // `add` writes a `.git` file at that path next, and R3 keeps `.git` out
        // of a klon, so the excluded paths are removed from the copy.
        let entries = prune(dst, src, dst, excludes)?;
        Ok(Timing {
            duration: started.elapsed(),
            entries,
        })
    }

    /// One `btrfs subvolume delete` when the mount allows it, else the
    /// background `rm -rf` that every other backend uses. A klon that is not a
    /// subvolume, for example one that an earlier backend cloned, always takes
    /// the fallback.
    fn delete(&self, dst: &Path) -> Result<()> {
        if !is_subvolume(dst) || !rm_allowed(dst) {
            return process::spawn_background_delete(dst);
        }
        let Some(tool) = tool() else {
            return process::spawn_background_delete(dst);
        };
        // The ioctl queues the work and returns, so this stays inside the
        // 100 ms budget of `rm` (R8). A refusal is not fatal: the byte delete
        // removes a populated subvolume too, it only costs more.
        match Command::new(&tool)
            .args(["subvolume", "delete", "--"])
            .arg(dst)
            .output()
        {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                eprintln!(
                    "klon: btrfs subvolume delete refused {}: {}",
                    dst.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                process::spawn_background_delete(dst)
            }
            Err(err) => {
                eprintln!("klon: cannot run {}: {err}", tool.display());
                process::spawn_background_delete(dst)
            }
        }
    }
}

// --- Host facts ----------------------------------------------------------------

/// The `btrfs` binary: `$KLON_BTRFS_TOOLS/btrfs` when that variable names a
/// directory, else the one on PATH. The S1 spike unpacks `btrfs-progs` into a
/// user directory, because the development laptop has none on PATH.
pub fn tool() -> Option<PathBuf> {
    match std::env::var_os("KLON_BTRFS_TOOLS") {
        Some(dir) => probe::executable(&Path::new(&dir).join("btrfs")),
        None => probe::tool_path("btrfs"),
    }
}

/// Why this host cannot take the backend, or None when it can.
fn unusable(golden: &Path) -> Option<String> {
    if probe::filesystem(golden) != "btrfs" {
        return Some("golden is not on btrfs".to_string());
    }
    if !is_subvolume(golden) {
        return Some(format!(
            "golden is a plain directory on btrfs; run gh klon init to make {} a subvolume",
            golden.display()
        ));
    }
    if !owned_by_caller(golden) {
        return Some("golden is a btrfs subvolume that another user owns".to_string());
    }
    if tool().is_none() {
        return Some(
            "btrfs is not on PATH; install btrfs-progs or set KLON_BTRFS_TOOLS".to_string(),
        );
    }
    None
}

/// True when `path` is the root of a btrfs subvolume.
///
/// A subvolume root has inode 256 and a device number of its own, so it differs
/// from the device number of its parent directory. `btrfs subvolume show` would
/// answer the same question, but it needs `CAP_SYS_ADMIN` (S1 §7), so klon
/// never calls it.
pub fn is_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(here) = fs::metadata(path) else {
        return false;
    };
    if here.ino() != SUBVOLUME_INODE {
        return false;
    }
    match path.parent().and_then(|p| fs::metadata(p).ok()) {
        Some(parent) => parent.dev() != here.dev(),
        // A path with no parent is a mount root, which is a subvolume too.
        None => true,
    }
}

/// True when the caller owns `path`. An unprivileged snapshot needs a
/// user-owned source subvolume (handoff §4).
fn owned_by_caller(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` reads a process property and cannot fail.
    let me = unsafe { libc::geteuid() };
    fs::metadata(path).is_ok_and(|meta| meta.uid() == me)
}

/// True when the filesystem that holds `path` carries `user_subvol_rm_allowed`.
/// Without it `btrfs subvolume delete` fails with `EPERM` for a normal user
/// (S1 §7), so `delete` must take the byte path instead.
fn rm_allowed(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mount_options(&text, path)
        .is_some_and(|options| options.split(',').any(|option| option.trim() == RM_ALLOWED))
}

/// The super options of the mount that holds `path`, from `/proc/self/mountinfo`.
///
/// A line reads `<id> <parent> <maj:min> <root> <mount point> <options> [tags]
/// - <type> <source> <super options>`. The longest mount point that is a prefix
/// of `path` wins, because a later mount can cover an earlier one.
fn mount_options(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let mut fields = line.split(' ');
        let point = match fields.nth(4) {
            Some(point) => PathBuf::from(unescape(point)),
            None => continue,
        };
        // Everything after the ` - ` separator: type, source, super options.
        let Some((_, tail)) = line.split_once(" - ") else {
            continue;
        };
        let Some(options) = tail.split(' ').nth(2) else {
            continue;
        };
        if path.starts_with(&point) {
            let depth = point.components().count();
            if best.as_ref().is_none_or(|(best, _)| depth > *best) {
                best = Some((depth, options.to_string()));
            }
        }
    }
    best.map(|(_, options)| options)
}

/// `/proc/self/mountinfo` escapes space, tab, newline, and backslash as octal.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let digits = rest.get(at + 1..at + 4).unwrap_or_default();
        match u8::from_str_radix(digits, 8) {
            Ok(byte) => {
                out.push(byte as char);
                rest = &rest[at + 4..];
            }
            Err(_) => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// --- The btrfs commands ---------------------------------------------------------

/// `btrfs subvolume create <path>`. `init` calls it for the staging subvolume.
pub fn create_subvolume(path: &Path) -> Result<()> {
    run(&["subvolume", "create", "--"], &[path])
}

/// `btrfs subvolume snapshot <src> <dst>`. `dst` must not exist.
fn snapshot(src: &Path, dst: &Path) -> Result<()> {
    run(&["subvolume", "snapshot"], &[src, dst])
}

/// Run one `btrfs` subcommand and turn a non-zero exit into an error.
fn run(args: &[&str], paths: &[&Path]) -> Result<()> {
    let tool = tool().ok_or_else(|| {
        Error::klon("btrfs is not on PATH; install btrfs-progs or set KLON_BTRFS_TOOLS")
    })?;
    let output = Command::new(&tool)
        .args(args)
        .args(paths)
        .output()
        .map_err(Error::io(format!("run {}", tool.display())))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::klon(format!(
        "btrfs {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

// --- The destination ------------------------------------------------------------

/// Remove the empty destination directory that the caller created, so the
/// snapshot can take its place.
///
/// `add` hands over a directory that holds the `.git` file of
/// `git worktree add --no-checkout`, and the probe hands over an empty one.
/// Anything else is a directory klon did not make, so the clone refuses rather
/// than delete it.
fn clear_destination(dst: &Path) -> Result<()> {
    let entries = fs::read_dir(dst).map_err(Error::io(format!("read {}", dst.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", dst.display())))?;
        let path = entry.path();
        let is_git_file = entry.file_name() == ".git"
            && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
        if !is_git_file {
            return Err(Error::klon(format!(
                "the destination {} is not empty; the btrfs snapshot cannot replace it",
                dst.display()
            )));
        }
        fs::remove_file(&path).map_err(Error::io(format!("delete {}", path.display())))?;
    }
    fs::remove_dir(dst).map_err(Error::io(format!("delete {}", dst.display())))
}

/// Remove every excluded path from the finished snapshot and count what stays.
///
/// A snapshot copies the whole subvolume, so the exclusions apply as deletions.
/// The walk maps each path in the snapshot back to its source path, because
/// `Exclusions` answers for golden's paths. A directory that loses a child gets
/// its source mtime back, so the manifest of the klon still equals golden's.
fn prune(dir: &Path, src_root: &Path, dst_root: &Path, excludes: &Exclusions) -> Result<u64> {
    let mut kept = 0u64;
    let mut removed_here = false;
    let entries = fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", dir.display())))?;
        let path = entry.path();
        let meta =
            fs::symlink_metadata(&path).map_err(Error::io(format!("stat {}", path.display())))?;
        let is_dir = meta.is_dir();
        let source = match path.strip_prefix(dst_root) {
            Ok(rel) => src_root.join(rel),
            Err(_) => return Err(Error::klon("the prune walk left the snapshot")),
        };
        if excludes.excludes(&source, is_dir) {
            remove(&path, is_dir)?;
            removed_here = true;
            continue;
        }
        kept += 1;
        if is_dir {
            kept += prune(&path, src_root, dst_root, excludes)?;
        }
    }
    if removed_here {
        let source = match dir.strip_prefix(dst_root) {
            Ok(rel) => src_root.join(rel),
            Err(_) => return Err(Error::klon("the prune walk left the snapshot")),
        };
        if let Ok(meta) = fs::symlink_metadata(&source) {
            super::set_times(dir, &meta)?;
        }
    }
    Ok(kept)
}

/// Delete one pruned path. A nested subvolume cannot be removed with `rmdir`
/// alone, so a directory that fails takes the recursive path.
fn remove(path: &Path, is_dir: bool) -> Result<()> {
    if !is_dir {
        return fs::remove_file(path).map_err(Error::io(format!("delete {}", path.display())));
    }
    super::make_removable(path)?;
    fs::remove_dir_all(path).map_err(Error::io(format!("delete {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line of `/proc/self/mountinfo` from the S1 spike, plus the root mount.
    const MOUNTINFO: &str = "\
25 30 0:24 / / rw,relatime shared:1 - ext4 /dev/sda2 rw,errors=remount-ro
36 25 7:57 / /mnt/klon-btrfs rw,relatime shared:2 - btrfs /dev/loop57 rw,user_subvol_rm_allowed,ssd
41 25 7:58 / /media/navaro/klon\\040demo rw,nosuid shared:3 - btrfs /dev/loop58 rw,ssd,discard=async
";

    #[test]
    fn the_longest_matching_mount_point_wins() {
        let options = mount_options(MOUNTINFO, Path::new("/mnt/klon-btrfs/work/golden"));
        assert_eq!(
            options.as_deref(),
            Some("rw,user_subvol_rm_allowed,ssd"),
            "the btrfs mount must win over the root mount"
        );
        let root = mount_options(MOUNTINFO, Path::new("/home/user/repo"));
        assert_eq!(root.as_deref(), Some("rw,errors=remount-ro"));
    }

    /// udisks mounts at `/media/<user>/<label>`, and a label may hold a space.
    #[test]
    fn an_escaped_mount_point_still_matches() {
        let options = mount_options(MOUNTINFO, Path::new("/media/navaro/klon demo/klon"));
        assert_eq!(options.as_deref(), Some("rw,ssd,discard=async"));
    }

    #[test]
    fn the_mount_option_decides_the_delete_path() {
        assert!(MOUNTINFO.lines().any(|line| line.contains(RM_ALLOWED)));
        let allowed = mount_options(MOUNTINFO, Path::new("/mnt/klon-btrfs")).unwrap();
        assert!(allowed.split(',').any(|o| o == RM_ALLOWED));
        let plain = mount_options(MOUNTINFO, Path::new("/media/navaro/klon demo")).unwrap();
        assert!(!plain.split(',').any(|o| o == RM_ALLOWED));
    }

    #[test]
    fn octal_escapes_become_characters() {
        assert_eq!(unescape("klon\\040demo"), "klon demo");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("tail\\"), "tail\\");
    }

    /// A plain temporary directory is never a subvolume, whatever the host
    /// filesystem is. The real answer needs btrfs, which `tests/btrfs.rs` covers.
    #[test]
    fn a_plain_directory_is_not_a_subvolume() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_subvolume(tmp.path()));
        assert!(!is_subvolume(&tmp.path().join("absent")));
    }

    /// The probe must reject a non-btrfs golden before it runs any `btrfs`
    /// process, so the reason names the filesystem, not a missing tool.
    #[test]
    fn a_golden_outside_btrfs_is_rejected_first() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        if probe::filesystem(&golden) == "btrfs" {
            println!("skipped: the temporary directory is on btrfs");
            return;
        }
        assert_eq!(unusable(&golden).as_deref(), Some("golden is not on btrfs"));
    }
}
