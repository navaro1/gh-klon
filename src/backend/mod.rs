//! Whole-directory clone backends (spec §7 C5, handoff §4 "Backends").
//!
//! Every filesystem gets one file with one `Backend`. `select` runs each probe
//! in preference order and takes the first backend that passes. A backend that
//! fails the probe is never selected (R5). The answer is cached in
//! `<common>/klon/probe.json`, so only the first command in a repository pays
//! for the probe.
//!
//! A new filesystem adds a file and one line in `registry`. It never edits
//! `add`. C6 adds `apfs-clone` and C7 adds `btrfs-snapshot` ahead of
//! `reflink-walk`.

pub mod copy;
pub mod reflink;
mod verify;

use crate::{probe, time, Error, Result};
use ignore::gitignore::Gitignore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The format version of `probe.json`. An unknown version fails closed.
pub const PROBE_VERSION: u32 = 1;

/// What one clone cost. C8 (`bench`) reads it and `probe` prints it.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub duration: Duration,
    /// Every entry the clone created: files, directories, and symlinks.
    pub entries: u64,
}

/// One way to fill a klon's working directory from golden.
///
/// `clone` writes the children of `src` into the existing empty directory
/// `dst`. `delete` removes a finished klon. `probe` answers whether this host
/// can use the backend; it must clone a fixture and compare a manifest, so a
/// silent data loss cannot reach a real klon.
pub trait Backend: Send + Sync {
    /// The stable name that `--backend`, `probe.json`, and `--json` use.
    fn name(&self) -> &'static str;

    /// `Present` when the backend cloned the probe fixture without a
    /// difference. `Absent` when the host lacks the feature. `Broken` when the
    /// feature exists and the clone was wrong.
    fn probe(&self, golden: &Path) -> probe::Status;

    /// Copy the children of `src` into the existing directory `dst`.
    fn clone(&self, src: &Path, dst: &Path, excludes: &Exclusions) -> Result<Timing>;

    /// Remove a finished klon. The byte backends delete in the background at
    /// low priority (R8); C7's btrfs backend replaces this with one subvolume
    /// delete.
    ///
    /// `rm` still calls `process::spawn_background_delete` itself, because in
    /// v0 every backend deletes the same way. C7 gives btrfs an O(1) delete and
    /// routes `rm` through this method.
    #[allow(dead_code)]
    fn delete(&self, dst: &Path) -> Result<()> {
        crate::process::spawn_background_delete(dst)
    }
}

// --- Exclusions ---------------------------------------------------------------

/// Directory names that klon never clones at the top level of golden.
/// They hold other worktrees or harness state, not project files.
const TOP_LEVEL_SKIP: &[&str] = &[".claude/worktrees", ".t3"];

/// Paths that a clone leaves out. Every path is absolute and normalized.
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

    /// True when the clone must skip `path`. A `.git` entry is skipped at every
    /// depth (R39).
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

// --- Shared filesystem helpers -------------------------------------------------

/// Give `path` the access and modification times of `meta`. Works on files and
/// directories. `FICLONE` sets the destination mtime to now, so `reflink` calls
/// this after every clone (R35).
///
/// `filetime` calls `utimensat` on the path. A call that opened the file first
/// would fail on a read-only file with no read bit, and golden may hold one.
pub(crate) fn set_times(path: &Path, meta: &fs::Metadata) -> Result<()> {
    let atime = filetime::FileTime::from_last_access_time(meta);
    let mtime = filetime::FileTime::from_last_modification_time(meta);
    filetime::set_file_times(path, atime, mtime)
        .map_err(Error::io(format!("set mtime {}", path.display())))
}

/// Give a symlink the times of `meta` without following it.
pub(crate) fn set_symlink_times(path: &Path, meta: &fs::Metadata) -> Result<()> {
    let atime = filetime::FileTime::from_last_access_time(meta);
    let mtime = filetime::FileTime::from_last_modification_time(meta);
    filetime::set_symlink_file_times(path, atime, mtime)
        .map_err(Error::io(format!("set symlink mtime {}", path.display())))
}

/// Restore owner access only in the newly cloned tree so rollback can delete it.
/// Do not follow symlinks: their targets may belong to golden or another tree.
pub fn make_removable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).map_err(Error::io("stat the failed clone"))?;
    if !meta.is_dir() {
        return Ok(());
    }
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(meta.permissions().mode() | 0o700),
    )
    .map_err(Error::io("restore directory access for cleanup"))?;
    for entry in fs::read_dir(path).map_err(Error::io("read the failed clone"))? {
        let entry = entry.map_err(Error::io("read the failed clone"))?;
        make_removable(&entry.path())?;
    }
    Ok(())
}

// --- Selection -----------------------------------------------------------------

/// The backend that a command will use, and why.
pub struct Choice {
    pub backend: Box<dyn Backend>,
    /// The reason for the selection: the rejection reason of every preferred
    /// backend, or the detail of the winning probe when none was rejected.
    pub reason: String,
}

/// The real backends in preference order. The first one whose probe passes wins.
fn registry() -> Vec<Box<dyn Backend>> {
    // C7 inserts `btrfs-snapshot` and C6 inserts `apfs-clone` above this line.
    vec![Box::new(reflink::Reflink), Box::new(copy::Copy)]
}

/// The list that `select` probes. It holds the real backends, plus the
/// test-only broken backend when `KLON_TEST_DROP_BACKEND=1` asks for it.
fn probe_order() -> Vec<Box<dyn Backend>> {
    let mut list: Vec<Box<dyn Backend>> = Vec::new();
    if std::env::var("KLON_TEST_DROP_BACKEND").as_deref() == Ok("1") {
        list.push(Box::new(verify::DropOne));
    }
    list.extend(registry());
    list
}

/// The backend named `name`, for `--backend`. The override skips the probe, so
/// it never resolves the test-only backend.
fn find(name: &str) -> Result<Box<dyn Backend>> {
    let mut names = Vec::new();
    for backend in registry() {
        if backend.name() == name {
            return Ok(backend);
        }
        names.push(backend.name());
    }
    Err(Error::klon(format!(
        "unknown backend {name}; klon knows {}",
        names.join(", ")
    )))
}

/// Choose the backend for `golden`. `over` is the `--backend` value, which
/// skips the probe and the cache. Otherwise the cached answer wins while it
/// matches the current filesystem; else every probe runs and the answer is
/// cached.
pub fn select(golden: &Path, common: &Path, over: Option<&str>) -> Result<Choice> {
    if let Some(name) = over {
        let backend = find(name)?;
        let reason = format!("--backend {name}");
        return Ok(Choice { backend, reason });
    }
    let filesystem = probe::filesystem(golden);
    if !refresh_requested() {
        if let Some(cache) = read_cache(common)? {
            if cache.filesystem == filesystem {
                if let Ok(backend) = find(&cache.backend) {
                    return Ok(Choice {
                        backend,
                        reason: cache.reason,
                    });
                }
            }
        }
    }
    let Choice { backend, reason } = run_probes(golden)?;
    write_cache(
        common,
        &Cache {
            version: PROBE_VERSION,
            backend: backend.name().to_string(),
            reason: reason.clone(),
            filesystem,
            created: time::now_rfc3339(),
        },
    )?;
    Ok(Choice { backend, reason })
}

/// Probe every backend in order and take the first that passes.
///
/// The reason is the rejection text of every backend that klon preferred, in
/// preference order, so `doctor` on ext4 says exactly `reflink unsupported`.
/// A backend that wins without a rejection above it reports its own detail.
fn run_probes(golden: &Path) -> Result<Choice> {
    let mut rejected: Vec<String> = Vec::new();
    for backend in probe_order() {
        match backend.probe(golden) {
            probe::Status::Present(detail) => {
                let reason = if rejected.is_empty() {
                    detail
                } else {
                    rejected.join("; ")
                };
                return Ok(Choice { backend, reason });
            }
            other => rejected.push(other.detail().to_string()),
        }
    }
    Err(Error::klon(format!(
        "no backend passed the probe: {}",
        rejected.join("; ")
    )))
}

/// True when the caller asked for a fresh probe. `doctor --repair` deletes the
/// file instead, so both paths reach the same place.
fn refresh_requested() -> bool {
    std::env::var("KLON_PROBE_REFRESH").as_deref() == Ok("1")
}

// --- The probe cache -----------------------------------------------------------

/// `<common>/klon/probe.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    version: u32,
    backend: String,
    reason: String,
    filesystem: String,
    created: String,
}

/// The path of the cache file.
fn cache_path(common: &Path) -> PathBuf {
    common.join("klon").join("probe.json")
}

/// Read the cache. A missing or unreadable file is no cache. A file with an
/// unknown version fails closed, like the journal.
fn read_cache(common: &Path) -> Result<Option<Cache>> {
    let path = cache_path(common);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io(format!("read {}", path.display()))(err)),
    };
    let version: VersionOnly = match serde_json::from_str(&text) {
        Ok(v) => v,
        // A damaged cache is not a reason to refuse work: probe again.
        Err(_) => return Ok(None),
    };
    if version.version > PROBE_VERSION {
        return Err(Error::klon(format!(
            "unknown probe cache version {} in {}; upgrade klon",
            version.version,
            path.display()
        )));
    }
    Ok(serde_json::from_str(&text).ok())
}

/// The version field alone, read before the rest, so a future shape still fails
/// with the version message instead of a field error.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// Write the cache atomically: a temporary file in the same directory, then one
/// rename.
fn write_cache(common: &Path, cache: &Cache) -> Result<()> {
    let path = cache_path(common);
    let dir = path.parent().unwrap_or(common);
    fs::create_dir_all(dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(cache)
        .map_err(|err| Error::klon(format!("serialize the probe cache: {err}")))?;
    let temp = dir.join(format!(".probe.{}.tmp", std::process::id()));
    fs::write(&temp, text.as_bytes()).map_err(Error::io(format!("write {}", temp.display())))?;
    if let Err(err) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(format!("write {}", path.display()))(err));
    }
    Ok(())
}

/// Delete the cached answer, so the next `select` probes again. `doctor
/// --repair` calls this.
pub fn forget_probe(common: &Path) -> Result<()> {
    let path = cache_path(common);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("delete {}", path.display()))(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe fixture lives next to golden, so the test gives golden a real
    /// parent directory.
    fn golden_in(tmp: &tempfile::TempDir) -> PathBuf {
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        golden
    }

    #[test]
    fn a_backend_that_drops_a_file_fails_the_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = golden_in(&tmp);
        let status = verify::DropOne.probe(&golden);
        assert!(
            status
                .detail()
                .starts_with("probe failed: manifest mismatch"),
            "expected a manifest mismatch, found {status:?}"
        );
        assert!(!status.present(), "a broken backend must not pass");
    }

    #[test]
    fn the_copy_backend_passes_its_own_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = golden_in(&tmp);
        let status = copy::Copy.probe(&golden);
        assert!(status.present(), "copy must pass the probe: {status:?}");
    }

    /// R5 plus the C5 acceptance line: a backend that drops a file is never
    /// selected, and the reason names the failure.
    #[test]
    fn selection_skips_a_backend_that_fails_the_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = golden_in(&tmp);
        let mut order: Vec<Box<dyn Backend>> = vec![Box::new(verify::DropOne)];
        order.extend(registry());
        let mut rejected = Vec::new();
        let mut chosen = None;
        for backend in order {
            match backend.probe(&golden) {
                probe::Status::Present(_) => {
                    chosen = Some(backend.name());
                    break;
                }
                other => rejected.push(other.detail().to_string()),
            }
        }
        assert_ne!(chosen, Some("drop-one"), "a broken backend must not win");
        assert!(chosen.is_some(), "one real backend must pass");
        assert!(
            rejected
                .iter()
                .any(|r| r.starts_with("probe failed: manifest mismatch")),
            "the rejection list must name the mismatch: {rejected:?}"
        );
    }

    /// `find` answers a boxed trait object, which has no `Debug`, so the tests
    /// below read the error through this helper instead of `unwrap_err`.
    fn find_error(name: &str) -> Option<String> {
        match find(name) {
            Ok(_) => None,
            Err(err) => Some(err.to_string()),
        }
    }

    #[test]
    fn an_unknown_backend_name_is_refused() {
        let err = find_error("no-such-backend").expect("an unknown name must fail");
        assert!(err.contains("unknown backend"), "unexpected error {err}");
    }

    #[test]
    fn the_test_only_backend_is_not_reachable_through_the_override() {
        assert!(find_error("drop-one").is_some());
    }

    #[test]
    fn a_future_cache_version_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("common");
        fs::create_dir_all(common.join("klon")).unwrap();
        fs::write(
            cache_path(&common),
            r#"{"version":99,"backend":"copy","reason":"x","filesystem":"ext4","created":"now"}"#,
        )
        .unwrap();
        let err = read_cache(&common).unwrap_err();
        assert!(
            err.to_string().contains("unknown probe cache version 99"),
            "unexpected error {err}"
        );
    }

    #[test]
    fn a_damaged_cache_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("common");
        fs::create_dir_all(common.join("klon")).unwrap();
        fs::write(cache_path(&common), "not json").unwrap();
        assert!(read_cache(&common).unwrap().is_none());
    }

    #[test]
    fn the_cache_round_trips_and_forget_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("common");
        let cache = Cache {
            version: PROBE_VERSION,
            backend: "copy".to_string(),
            reason: "reflink unsupported".to_string(),
            filesystem: "ext4".to_string(),
            created: time::now_rfc3339(),
        };
        write_cache(&common, &cache).unwrap();
        let read = read_cache(&common).unwrap().expect("a cached answer");
        assert_eq!(read.backend, "copy");
        assert_eq!(read.reason, "reflink unsupported");
        forget_probe(&common).unwrap();
        assert!(read_cache(&common).unwrap().is_none());
        // A second forget stays quiet.
        forget_probe(&common).unwrap();
    }
}
