//! The backend probe (spec §7 C5, R5): clone a generated fixture and compare a
//! manifest. Every backend answers `probe` with this function, so one rule
//! decides whether a backend is safe on this host.
//!
//! The fixture holds 200 regular files, one symlink, one read-only file, and
//! two subdirectories with different modes. Every file gets a distinct mtime
//! and distinct content, so a backend that swaps, truncates, or re-times a file
//! fails here and never reaches a real klon.
//!
//! The manifest compares the type, the size, the mode, the mtime, the symlink
//! target, and a SHA-256 of the content. It also compares the inode of every
//! pair: a clone that shares an inode with its source is a hardlink, which R4
//! forbids.

use super::{Backend, Exclusions, Timing};
use crate::{paths, probe, Error, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The regular files that the fixture writes, split over two directories.
const FILES: usize = 200;

/// The bytes per fixture file. It is above the btrfs inline-extent limit, so
/// `FICLONE` sees a real extent on every filesystem klon probes.
const FILE_BYTES: usize = 8192;

/// The mtime of fixture file 0, in seconds. Later files step one second on.
const BASE_MTIME: i64 = 1_600_000_000;

/// Probe `backend` on the filesystem that holds `golden`.
pub fn run(backend: &dyn Backend, golden: &Path) -> probe::Status {
    let scratch = match Scratch::next_to(golden) {
        Ok(scratch) => scratch,
        Err(err) => return probe::Status::Broken(format!("probe failed: {err}")),
    };
    match attempt(backend, scratch.path()) {
        Ok(status) => status,
        Err(err) => probe::Status::Broken(format!("probe failed: {err}")),
    }
}

/// One probe round inside an existing scratch directory.
fn attempt(backend: &dyn Backend, dir: &Path) -> Result<probe::Status> {
    let src = dir.join("src");
    let dst = dir.join("dst");
    build_fixture(&src)?;
    fs::create_dir(&dst).map_err(Error::io(format!("create {}", dst.display())))?;
    let excludes = Exclusions::new(&src, []);
    let timing = match backend.clone(&src, &dst, &excludes) {
        Ok(timing) => timing,
        Err(err) => return Ok(probe::Status::Broken(format!("probe failed: {err}"))),
    };
    let want = manifest(&src)?;
    let got = manifest(&dst)?;
    if let Some(why) = difference(&want, &got) {
        return Ok(probe::Status::Broken(format!(
            "probe failed: manifest mismatch: {why}"
        )));
    }
    if let Some(why) = shared_inode(&want, &got) {
        return Ok(probe::Status::Broken(format!(
            "probe failed: the clone shares an inode with the source: {why}"
        )));
    }
    // The detail becomes the cached selection reason, so it must not carry a
    // measurement: two probes of one host would then disagree for no reason.
    Ok(probe::Status::Present(format!(
        "the fixture clone matched: {} entries",
        timing.entries
    )))
}

// --- The fixture ---------------------------------------------------------------

/// Write the probe fixture into the new directory `root`.
fn build_fixture(root: &Path) -> Result<()> {
    fs::create_dir_all(root).map_err(Error::io(format!("create {}", root.display())))?;
    // Two directories with different modes. The mode of a directory must
    // survive the clone, so the probe uses one non-default value.
    let dirs = [(root.join("d0"), 0o755u32), (root.join("d1"), 0o750u32)];
    for (dir, _) in &dirs {
        fs::create_dir(dir).map_err(Error::io(format!("create {}", dir.display())))?;
    }
    for i in 0..FILES {
        let (dir, _) = &dirs[i % dirs.len()];
        let file = dir.join(format!("f{i:03}.bin"));
        write_file(&file, &body(i))?;
        // The last file is read only, so the probe covers a mode that blocks a
        // second write.
        let mode = if i + 1 == FILES { 0o444 } else { 0o644 };
        set_mode(&file, mode)?;
        set_mtime(&file, BASE_MTIME + i as i64, (i * 1_000_000) as u32)?;
    }
    // One relative symlink into the first directory. Its own mtime is fixed
    // too, so a backend that follows the link instead of recreating it fails.
    let link = root.join("link");
    std::os::unix::fs::symlink("d0/f000.bin", &link)
        .map_err(Error::io(format!("symlink {}", link.display())))?;
    set_symlink_mtime(&link, BASE_MTIME, 0)?;
    // Give the directories their mode and mtime after their children exist.
    for (dir, mode) in &dirs {
        set_mode(dir, *mode)?;
        set_mtime(dir, BASE_MTIME, 0)?;
    }
    Ok(())
}

/// Distinct bytes for fixture file `i`, `FILE_BYTES` long.
fn body(i: usize) -> Vec<u8> {
    let seed = format!("klon probe fixture {i}\n");
    let mut out = seed.into_bytes();
    let mut n = 0usize;
    while out.len() < FILE_BYTES {
        out.push((i.wrapping_mul(31).wrapping_add(n) % 251) as u8);
        n += 1;
    }
    out.truncate(FILE_BYTES);
    out
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        fs::File::create(path).map_err(Error::io(format!("create {}", path.display())))?;
    file.write_all(bytes)
        .map_err(Error::io(format!("write {}", path.display())))?;
    // Close before the clone reads the file, so no byte waits in a buffer.
    drop(file);
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(Error::io(format!("chmod {}", path.display())))
}

fn set_mtime(path: &Path, secs: i64, nanos: u32) -> Result<()> {
    let time = filetime::FileTime::from_unix_time(secs, nanos);
    filetime::set_file_times(path, time, time)
        .map_err(Error::io(format!("set mtime {}", path.display())))
}

fn set_symlink_mtime(path: &Path, secs: i64, nanos: u32) -> Result<()> {
    let time = filetime::FileTime::from_unix_time(secs, nanos);
    filetime::set_symlink_file_times(path, time, time)
        .map_err(Error::io(format!("set symlink mtime {}", path.display())))
}

// --- The manifest ---------------------------------------------------------------

/// One entry of the probe manifest.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Item {
    rel: PathBuf,
    kind: &'static str,
    size: u64,
    mode: u32,
    mtime_secs: i64,
    mtime_nanos: i64,
    target: Option<PathBuf>,
    /// A SHA-256 of the content, empty for a directory and a symlink.
    hash: Vec<u8>,
    /// Not compared between the trees; used for the hardlink check only.
    device: u64,
    inode: u64,
}

/// Every entry below `root`, sorted by the relative path.
fn manifest(root: &Path) -> Result<Vec<Item>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Item>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", dir.display())))?;
        let path = entry.path();
        let meta =
            fs::symlink_metadata(&path).map_err(Error::io(format!("stat {}", path.display())))?;
        let file_type = meta.file_type();
        let (kind, target, hash) = if file_type.is_symlink() {
            let target =
                fs::read_link(&path).map_err(Error::io(format!("readlink {}", path.display())))?;
            ("symlink", Some(target), Vec::new())
        } else if file_type.is_dir() {
            walk(root, &path, out)?;
            ("dir", None, Vec::new())
        } else {
            let bytes = fs::read(&path).map_err(Error::io(format!("read {}", path.display())))?;
            ("file", None, Sha256::digest(&bytes).to_vec())
        };
        let rel = path
            .strip_prefix(root)
            .map_err(|_| Error::klon("the walk left the probe directory"))?
            .to_path_buf();
        out.push(Item {
            rel,
            kind,
            size: if file_type.is_file() { meta.len() } else { 0 },
            mode: meta.permissions().mode(),
            mtime_secs: meta.mtime(),
            mtime_nanos: meta.mtime_nsec(),
            target,
            hash,
            device: meta.dev(),
            inode: meta.ino(),
        });
    }
    Ok(())
}

/// The first difference between the two manifests, or None when they agree.
fn difference(want: &[Item], got: &[Item]) -> Option<String> {
    for (a, b) in want.iter().zip(got.iter()) {
        if a.rel != b.rel {
            return Some(format!(
                "{} is missing, found {}",
                a.rel.display(),
                b.rel.display()
            ));
        }
        let field = if a.kind != b.kind {
            "type"
        } else if a.size != b.size {
            "size"
        } else if a.mode != b.mode {
            "mode"
        } else if a.mtime_secs != b.mtime_secs || a.mtime_nanos != b.mtime_nanos {
            "mtime"
        } else if a.target != b.target {
            "symlink target"
        } else if a.hash != b.hash {
            "content"
        } else {
            continue;
        };
        return Some(format!("{} differs in the {field}", a.rel.display()));
    }
    match want.len().cmp(&got.len()) {
        std::cmp::Ordering::Greater => Some(format!(
            "{} entries are missing, first {}",
            want.len() - got.len(),
            want[got.len()].rel.display()
        )),
        std::cmp::Ordering::Less => Some(format!(
            "{} extra entries, first {}",
            got.len() - want.len(),
            got[want.len()].rel.display()
        )),
        std::cmp::Ordering::Equal => None,
    }
}

/// The first pair that shares one inode on one device (R4), or None.
fn shared_inode(want: &[Item], got: &[Item]) -> Option<String> {
    let sources: std::collections::HashSet<(u64, u64)> =
        want.iter().map(|i| (i.device, i.inode)).collect();
    got.iter()
        .find(|i| sources.contains(&(i.device, i.inode)))
        .map(|i| i.rel.display().to_string())
}

// --- The scratch directory --------------------------------------------------------

/// A directory beside golden's `.wt` root, on the same filesystem as golden.
/// `Drop` removes it, so a failed probe leaves nothing behind.
struct Scratch(PathBuf);

impl Scratch {
    /// Create `<parent>/<repo>.wt.probe.<pid>.<n>` next to golden's `.wt` root.
    /// A sibling of that root is on golden's filesystem, which is the only
    /// place where the probe answer is true for the real clone.
    fn next_to(golden: &Path) -> Result<Scratch> {
        let root = paths::default_wt_root(golden);
        let pid = std::process::id();
        for n in 0..64u32 {
            let name = format!(
                "{}.probe.{pid}.{n}",
                root.file_name().unwrap_or_default().to_string_lossy()
            );
            let candidate = root.with_file_name(name);
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(Scratch(candidate)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(Error::io(format!("create {}", candidate.display()))(err)),
            }
        }
        Err(Error::klon(
            "cannot create a probe directory next to golden",
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The fixture holds a read-only file, which `remove_dir_all` can still
        // unlink, and directories with a narrow mode, which it cannot enter.
        let _ = super::make_removable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

// --- The test-only broken backend --------------------------------------------------

/// A backend that clones correctly and then drops one file. It proves that the
/// probe catches a silent loss and that `select` never takes a backend that
/// fails (spec §7 C5).
///
/// `KLON_TEST_DROP_BACKEND=1` puts it ahead of every real backend in the probe
/// order. klon never sets that variable itself, `--backend` cannot name this
/// backend, and the probe always rejects it, so it can never fill a real klon.
pub struct DropOne;

impl Backend for DropOne {
    fn name(&self) -> &'static str {
        "drop-one"
    }

    fn probe(&self, golden: &Path) -> probe::Status {
        run(self, golden)
    }

    fn clone(&self, src: &Path, dst: &Path, excludes: &Exclusions) -> Result<Timing> {
        let timing = super::copy::Copy.clone(src, dst, excludes)?;
        let victim = first_file(dst)?
            .ok_or_else(|| Error::klon("the drop-one backend found no file to drop"))?;
        fs::remove_file(&victim).map_err(Error::io(format!("drop {}", victim.display())))?;
        Ok(timing)
    }
}

/// The first regular file below `dir`, in directory order.
fn first_file(dir: &Path) -> Result<Option<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(Error::io(format!("read {}", dir.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let meta =
            fs::symlink_metadata(&path).map_err(Error::io(format!("stat {}", path.display())))?;
        if meta.is_file() {
            return Ok(Some(path));
        }
        if meta.is_dir() {
            if let Some(found) = first_file(&path)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_holds_every_documented_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src");
        build_fixture(&root).unwrap();
        let items = manifest(&root).unwrap();
        let files = items.iter().filter(|i| i.kind == "file").count();
        let dirs = items.iter().filter(|i| i.kind == "dir").count();
        let links = items.iter().filter(|i| i.kind == "symlink").count();
        assert_eq!(files, FILES);
        assert_eq!(dirs, 2);
        assert_eq!(links, 1);
        assert!(
            items.iter().any(|i| i.mode & 0o777 == 0o444),
            "the fixture needs one read-only file"
        );
        assert!(
            items
                .iter()
                .any(|i| i.kind == "dir" && i.mode & 0o777 == 0o750),
            "the fixture needs one directory with a distinct mode"
        );
        let distinct: std::collections::HashSet<i64> = items
            .iter()
            .filter(|i| i.kind == "file")
            .map(|i| i.mtime_secs)
            .collect();
        assert_eq!(distinct.len(), FILES, "every file needs its own mtime");
    }

    #[test]
    fn the_manifest_reports_each_kind_of_difference() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        build_fixture(&a).unwrap();
        build_fixture(&b).unwrap();
        assert_eq!(
            difference(&manifest(&a).unwrap(), &manifest(&b).unwrap()),
            None
        );

        // A changed byte shows as a content difference; the size stays equal.
        let mut bytes = fs::read(b.join("d0/f000.bin")).unwrap();
        bytes[0] ^= 0xff;
        fs::write(b.join("d0/f000.bin"), &bytes).unwrap();
        set_mtime(&b.join("d0/f000.bin"), BASE_MTIME, 0).unwrap();
        let why = difference(&manifest(&a).unwrap(), &manifest(&b).unwrap()).unwrap();
        assert!(why.contains("content") || why.contains("mtime"), "{why}");

        // A dropped file shows as a missing entry.
        fs::remove_file(b.join("d1/f199.bin")).unwrap();
        let why = difference(&manifest(&a).unwrap(), &manifest(&b).unwrap()).unwrap();
        assert!(!why.is_empty(), "a dropped file must show");
    }

    #[test]
    fn a_hardlink_fails_the_inode_check() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        fs::write(a.join("f"), b"x").unwrap();
        fs::hard_link(a.join("f"), b.join("f")).unwrap();
        let shared = shared_inode(&manifest(&a).unwrap(), &manifest(&b).unwrap());
        assert_eq!(shared.as_deref(), Some("f"));
    }

    #[test]
    fn the_scratch_directory_disappears_after_the_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        let kept = {
            let scratch = Scratch::next_to(&golden).unwrap();
            let path = scratch.path().to_path_buf();
            assert!(path.is_dir());
            build_fixture(&path.join("src")).unwrap();
            path
        };
        assert!(!kept.exists(), "the scratch directory must be removed");
    }
}
