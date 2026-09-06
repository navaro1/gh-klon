//! Whole-directory clone backends (spec §7 C5, handoff §4 "Backends").
//!
//! Every filesystem gets one file with one `Backend`. `select` runs each probe
//! in preference order and takes the first backend that passes. A backend that
//! fails the probe is never selected (R5). The answer is cached in
//! `<common>/klon/probe.json`, so only the first command in a repository pays
//! for the probe.
//!
//! A new filesystem adds a file and one line in `registry`. It never edits
//! `add`. C7 added `btrfs-snapshot` ahead of `reflink-walk`; C6 adds
//! `apfs-clone` the same way.

pub mod btrfs;
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

/// What one clone cost.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long the clone took. C8 (`bench`) reads it for the M1 cell. The
    /// probe must not print it: its detail becomes the cached selection reason,
    /// which has to stay the same for one host.
    #[allow(dead_code)]
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

    /// True when this host could take the backend at all. `probe_order` drops a
    /// backend that answers false, so a filesystem-specific rejection never
    /// joins the selection reason of an unrelated filesystem: `doctor` on ext4
    /// still says exactly `reflink unsupported`. The probe still decides
    /// whether a backend that applies is safe.
    fn applies(&self, _golden: &Path) -> bool {
        true
    }

    /// Copy the children of `src` into the existing directory `dst`.
    fn clone(&self, src: &Path, dst: &Path, excludes: &Exclusions) -> Result<Timing>;

    /// How many bytes `clone` will write into the klon (R41).
    ///
    /// `add` refuses before the first repository change when the free space of
    /// the destination filesystem is below 1.2 times this number. The default
    /// walks golden and answers the disk blocks its tree holds, which is what
    /// a byte copy writes. It is not the apparent size: a tree of many tiny
    /// files costs far more in blocks and inodes than in content. A backend
    /// that shares blocks writes almost nothing and answers 0, so the guard
    /// costs it neither a walk nor a refusal.
    fn estimate_bytes(&self, golden: &Path, excludes: &Exclusions) -> u64 {
        copy::survey(golden, excludes).total.disk
    }

    /// True when the backend shares blocks and therefore needs the source and
    /// the destination on one filesystem. `select` drops such a backend when
    /// `--path` names another filesystem, so `add` falls back instead of
    /// failing halfway through with `EXDEV`.
    fn same_filesystem_only(&self) -> bool {
        false
    }

    /// Remove a finished klon. The byte backends delete in the background at
    /// low priority (R8); C7's btrfs backend replaces this with one subvolume
    /// delete.
    ///
    /// `rm` reads the cached backend answer through `cached` and calls this
    /// method, so a btrfs klon takes the O(1) subvolume delete and every other
    /// klon takes the byte delete.
    fn delete(&self, dst: &Path) -> Result<()> {
        crate::process::spawn_background_delete(dst)
    }
}

// --- Exclusions ---------------------------------------------------------------

/// Directory names that klon never clones at the top level of golden.
/// They hold other worktrees or harness state, not project files.
const TOP_LEVEL_SKIP: &[&str] = &[".claude/worktrees", ".t3"];

/// Paths that a clone leaves out. Every path is absolute and normalized.
///
/// The rules run in this order, and the first answer wins (R39):
///
/// | # | Rule | A `.worktreeinclude` line can override it |
/// |---|---|---|
/// | 1 | a `.git` entry at any depth | no |
/// | 2 | the exact set: the destination, every other registered worktree, `.claude/worktrees`, `.t3` | no |
/// | 3 | a submodule path from `.gitmodules` | no |
/// | 4 | a `.klonignore` match | yes |
///
/// Rules 1 to 3 protect the clone itself: a nested `.git` would give the klon a
/// second repository, the exact set would copy a worktree into itself, and a
/// submodule directory without its admin entry is a broken checkout. Only
/// rule 4 states a preference, so only rule 4 gives way.
pub struct Exclusions {
    exact: HashSet<PathBuf>,
    klonignore: Option<Gitignore>,
    include: Includes,
    golden: PathBuf,
}

impl Exclusions {
    pub fn new(golden: &Path, exact: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut set: HashSet<PathBuf> = exact.into_iter().collect();
        for name in TOP_LEVEL_SKIP {
            set.insert(golden.join(name));
        }
        for path in submodule_paths(golden) {
            set.insert(golden.join(path));
        }
        Exclusions {
            exact: set,
            klonignore: load_klonignore(golden),
            include: Includes::load(golden),
            golden: golden.to_path_buf(),
        }
    }

    /// Skip one more path. `add` adds the ignored directories that the warm
    /// process takes, so the inline clone leaves them out (R36).
    pub fn add_exact(&mut self, path: PathBuf) {
        self.exact.insert(path);
    }

    /// True when the clone must skip `path`. A `.git` entry is skipped at every
    /// depth (R39).
    pub fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        if path.file_name().is_some_and(|n| n == ".git") || self.exact.contains(path) {
            return true;
        }
        let (Some(ignore), Ok(rel)) = (&self.klonignore, path.strip_prefix(&self.golden)) else {
            return false;
        };
        if !ignore.matched_path_or_any_parents(rel, is_dir).is_ignore() {
            return false;
        }
        // `.worktreeinclude` is additive: it takes a `.klonignore` match back.
        !self.include.covers(rel, is_dir)
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

/// The submodule paths of `golden`, relative to golden, from `.gitmodules`.
///
/// klon asks git instead of parsing the file: `.gitmodules` is git config
/// syntax, not INI. A repository without the file, or a file that git refuses,
/// gives an empty list; the clone then falls back on the nested `.git` rule,
/// which already skips a populated submodule.
fn submodule_paths(golden: &Path) -> Vec<PathBuf> {
    if !golden.join(".gitmodules").is_file() {
        return Vec::new();
    }
    // The pattern is anchored. A plain `path` also matches `submodule.<name>.url`
    // when the name holds `path`, and a branch called `main` would then exclude
    // golden's own `main/` directory. `-z` gives `key\nvalue\0` records, so a
    // path with a space or a newline still parses.
    let out = crate::git::run(
        golden,
        &[
            "config",
            "-z",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    );
    let Ok(text) = out else {
        return Vec::new();
    };
    text.split('\0')
        .filter_map(|record| record.split_once('\n'))
        .filter(|(key, _)| key.starts_with("submodule.") && key.ends_with(".path"))
        .map(|(_, value)| PathBuf::from(value))
        .filter(|p| p.is_relative() && !p.as_os_str().is_empty())
        .collect()
}

/// `<golden>/.worktreeinclude`: the additive include (R39). It uses gitignore
/// syntax, where a matching line means "clone this after all".
#[derive(Default)]
struct Includes {
    /// The lines, compiled as a gitignore matcher.
    patterns: Option<Gitignore>,
    /// Every directory that a plain include line sits below, relative to
    /// golden. The walk must descend into these, because a `.klonignore` line
    /// that excludes a directory would otherwise hide its included children.
    ancestors: HashSet<PathBuf>,
    /// The literal head of every include line that holds a wildcard. klon
    /// cannot name the directories below a wildcard, so it descends into all of
    /// them. An excluded directory can therefore appear empty in the klon; that
    /// is the price of a wildcard include, and `.worktreeinclude` is opt-in.
    open: Vec<PathBuf>,
}

impl Includes {
    fn load(golden: &Path) -> Includes {
        let file = golden.join(".worktreeinclude");
        let Ok(text) = fs::read_to_string(&file) else {
            return Includes::default();
        };
        let mut builder = ignore::gitignore::GitignoreBuilder::new(golden);
        let mut ancestors = HashSet::new();
        let mut open = Vec::new();
        for raw in text.lines() {
            // gitignore keeps a leading space and an escaped trailing one, so
            // the raw line is the pattern. The trimmed copy only answers
            // whether the line is blank or a comment.
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let looks_empty = line.trim();
            if looks_empty.is_empty() || looks_empty.starts_with('#') {
                continue;
            }
            let _ = builder.add_line(Some(file.clone()), line);
            let (dirs, wildcard) = ancestor_dirs(line);
            if wildcard {
                open.push(dirs.last().cloned().unwrap_or_default());
            }
            ancestors.extend(dirs);
        }
        Includes {
            patterns: builder.build().ok(),
            ancestors,
            open,
        }
    }

    /// True when `rel` is included, or is a directory on the way to one.
    fn covers(&self, rel: &Path, is_dir: bool) -> bool {
        if is_dir
            && (self.ancestors.contains(rel) || self.open.iter().any(|head| rel.starts_with(head)))
        {
            return true;
        }
        self.patterns
            .as_ref()
            .is_some_and(|p| p.matched_path_or_any_parents(rel, is_dir).is_ignore())
    }
}

/// The directories that an include line sits below, up to the first wildcard,
/// and whether a wildcard follows them.
///
/// `build/keep/**` gives `[build, build/keep]` and true. `build/keep/k.txt`
/// gives `[build, build/keep]` and false. `*.log` gives `[]` and true, so the
/// walk descends everywhere, which is what that line asks for.
fn ancestor_dirs(line: &str) -> (Vec<PathBuf>, bool) {
    let line = line.strip_prefix('!').unwrap_or(line);
    let cut = line.find(['*', '?', '[']);
    let literal = match cut {
        // A wildcard can start inside a name, as in `build/ke*p.txt`, so the
        // literal head keeps whole components only.
        Some(cut) => match line[..cut].rfind('/') {
            Some(slash) => &line[..=slash],
            None => "",
        },
        None => line,
    };
    let mut out = Vec::new();
    let mut walk = PathBuf::new();
    let parts: Vec<&str> = literal.split('/').filter(|p| !p.is_empty()).collect();
    // The last part is the entry itself when no separator follows it, so it
    // becomes a parent only when one does.
    let last = if literal.ends_with('/') {
        parts.len()
    } else {
        parts.len().saturating_sub(1)
    };
    for part in parts.iter().take(last) {
        walk.push(part);
        out.push(walk.clone());
    }
    (out, cut.is_some())
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
    // Only a narrow mode needs the call. A directory that already grants the
    // owner all three bits skips it, which saves one syscall per directory and
    // keeps a path that refuses `chmod` out of the way: btrfs stands a nested
    // subvolume of the source in a snapshot as a stub that answers `EPERM`, and
    // `rmdir` still removes it (C7, S1 §8).
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700)).map_err(Error::io(
            format!("restore access to {} for cleanup", path.display()),
        ))?;
    }
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
///
/// `reflink-walk` is a Linux backend. On macOS `reflink-copy` calls
/// `clonefile`, so its probe would pass on APFS and take a path that C6 owns:
/// the handoff keeps macOS on `copy` until the `apfs-clone` backend lands
/// (handoff §4, spec §7 C6). The `reflink` row of `doctor` still reports the
/// host fact on every platform.
fn registry() -> Vec<Box<dyn Backend>> {
    // C6 inserts `apfs-clone` ahead of these.
    #[cfg(target_os = "linux")]
    let list: Vec<Box<dyn Backend>> = vec![
        Box::new(btrfs::BtrfsSnapshot),
        Box::new(reflink::Reflink),
        Box::new(copy::Copy),
    ];
    #[cfg(not(target_os = "linux"))]
    let list: Vec<Box<dyn Backend>> = vec![Box::new(copy::Copy)];
    list
}

/// The list that `select` probes: every backend that applies to golden, plus
/// the test-only broken backend when `KLON_TEST_DROP_BACKEND=1` asks for it.
fn probe_order(golden: &Path) -> Vec<Box<dyn Backend>> {
    let mut list: Vec<Box<dyn Backend>> = Vec::new();
    if std::env::var("KLON_TEST_DROP_BACKEND").as_deref() == Ok("1") {
        list.push(Box::new(verify::DropOne));
    }
    list.extend(registry().into_iter().filter(|b| b.applies(golden)));
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

/// Choose the backend for `golden`. `destination` is the klon path that `add`
/// will fill; `doctor` passes None, because it clones nothing. `over` is the
/// `--backend` value, which skips the probe and the cache.
///
/// Without an override the cached answer wins while it matches golden's
/// filesystem; else every probe runs and the answer is cached. A backend that
/// shares blocks is then dropped when the destination cannot receive its clone.
pub fn select(
    golden: &Path,
    common: &Path,
    destination: Option<&Path>,
    over: Option<&str>,
) -> Result<Choice> {
    if let Some(name) = over {
        let backend = find(name)?;
        let reason = format!("--backend {name}");
        return Ok(Choice { backend, reason });
    }
    let choice = probed(golden, common)?;
    // The probe answers for golden's filesystem, which is where the cache is
    // keyed. The destination may sit on another one.
    if choice.backend.same_filesystem_only() {
        if let Some(dst) = destination {
            if let Err(why) = cow_reaches(golden, dst) {
                return Ok(Choice {
                    backend: find(copy::Copy.name())?,
                    reason: why,
                });
            }
        }
    }
    Ok(choice)
}

/// The cached answer for golden's filesystem, or a fresh probe.
fn probed(golden: &Path, common: &Path) -> Result<Choice> {
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

/// `Ok` when a block-sharing clone from `golden` can land in `destination`.
///
/// One device id usually settles it: one superblock always clones. Two btrfs
/// subvolumes of one filesystem carry two device ids and still clone, so a
/// differing device runs one real `FICLONE` before klon gives up the fast
/// backend. The destination does not exist yet, so the test uses its nearest
/// parent that does.
fn cow_reaches(golden: &Path, destination: &Path) -> std::result::Result<(), String> {
    let Some(parent) = nearest_existing(destination) else {
        return Ok(());
    };
    let (Some(here), Some(there)) = (device(golden), device(&parent)) else {
        // Without both device ids klon cannot tell. Keep the fast backend and
        // let the clone report the real error.
        return Ok(());
    };
    if here == there {
        return Ok(());
    }
    match reflink::capability_across(golden, &parent) {
        probe::Status::Present(_) => Ok(()),
        other => Err(format!(
            "the destination is on another filesystem: {}",
            other.detail()
        )),
    }
}

/// The nearest ancestor of `path`, `path` itself included, that exists.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.exists()).map(Path::to_path_buf)
}

/// The device id of `path`, or None when the stat fails.
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|meta| meta.dev())
}

/// Probe every backend in order and take the first that passes.
///
/// The reason is the rejection text of every backend that klon preferred, in
/// preference order, so `doctor` on ext4 says exactly `reflink unsupported`.
/// A backend that wins without a rejection above it reports its own detail.
fn run_probes(golden: &Path) -> Result<Choice> {
    let mut rejected: Vec<String> = Vec::new();
    for backend in probe_order(golden) {
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
    // A common directory that vanished under the running command names no
    // repository any more. `doctor --repair` reaches this state when it renames
    // golden back after an interrupted `init`: its own path is then stale, and
    // one `create_dir_all` below would rebuild the directory tree it just
    // removed. An answer that nothing will read is not worth that.
    if !common.is_dir() {
        return Ok(());
    }
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

/// Read the cache and answer only whether it is usable. `doctor` calls it
/// before it repairs or deletes anything, so a cache from a future klon stops
/// the command instead of losing a format that this binary cannot read
/// (spec §4 "State on disk").
pub fn check_probe_cache(common: &Path) -> Result<()> {
    read_cache(common).map(|_| ())
}

/// The backend of the cached probe answer, without a probe of its own.
///
/// `rm` must return inside 100 ms (R8) and a fresh probe clones a fixture, so
/// `rm` takes the cached answer or nothing. None means "delete the universal
/// way": no cache, an unreadable cache, or a name this binary does not know.
pub fn cached(common: &Path) -> Option<Box<dyn Backend>> {
    let cache = read_cache(common).ok()??;
    find(&cache.backend).ok()
}

/// Delete the cached answer, so the next `select` probes again. `doctor
/// --repair` calls this after `check_probe_cache` accepted the file.
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

    /// The detail of a passing probe becomes the cached selection reason when
    /// klon rejected no backend above it. Two probes of one host must therefore
    /// give the same text: no timing, no counter that moves.
    #[test]
    fn a_passing_probe_gives_the_same_detail_every_time() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = golden_in(&tmp);
        let first = copy::Copy.probe(&golden);
        let second = copy::Copy.probe(&golden);
        assert_eq!(
            first.detail(),
            second.detail(),
            "the probe detail must not carry a measurement"
        );
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

    /// `reflink-walk` is a Linux backend. On macOS `reflink-copy` calls
    /// `clonefile`, which C6 owns, so the name must not resolve there.
    #[test]
    fn the_registry_holds_reflink_walk_on_linux_only() {
        let names: Vec<&str> = registry().iter().map(|b| b.name()).collect();
        assert!(names.contains(&"copy"), "copy is the universal fallback");
        assert_eq!(
            names.contains(&"reflink-walk"),
            cfg!(target_os = "linux"),
            "reflink-walk belongs to Linux only, found {names:?}"
        );
        assert_eq!(
            find_error("reflink-walk").is_none(),
            cfg!(target_os = "linux")
        );
    }

    /// The universal backend never needs one filesystem; the clone backend
    /// does. `select` reads this to keep `--path` on another mount working.
    #[test]
    fn only_the_block_sharing_backend_needs_one_filesystem() {
        assert!(!copy::Copy.same_filesystem_only());
        assert!(reflink::Reflink.same_filesystem_only());
    }

    /// A wildcard include has to keep every directory below its literal head
    /// reachable, or the walk prunes the directory that holds the wanted file.
    #[test]
    fn an_include_line_gives_its_parent_directories() {
        let dirs = |line: &str| -> (Vec<String>, bool) {
            let (list, wild) = ancestor_dirs(line);
            (list.iter().map(|p| p.display().to_string()).collect(), wild)
        };
        assert_eq!(
            dirs("/build/cache/keep/"),
            (vec_of(&["build", "build/cache", "build/cache/keep"]), false)
        );
        assert_eq!(
            dirs("build/keep/k.txt"),
            (vec_of(&["build", "build/keep"]), false)
        );
        assert_eq!(dirs("build/**/keep.txt"), (vec_of(&["build"]), true));
        // A wildcard can open inside a name, so the head keeps whole components.
        assert_eq!(dirs("build/ke*p.txt"), (vec_of(&["build"]), true));
        assert_eq!(dirs("*.log"), (Vec::new(), true));
        assert_eq!(dirs("keep.txt"), (Vec::new(), false));
    }

    fn vec_of(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// An empty head must open every directory, because `*.log` matches at
    /// every depth.
    #[test]
    fn an_empty_head_is_a_prefix_of_every_path() {
        assert!(Path::new("build/deep").starts_with(Path::new("")));
    }

    #[test]
    fn the_nearest_existing_ancestor_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c");
        assert_eq!(nearest_existing(&deep).as_deref(), Some(tmp.path()));
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(nearest_existing(&deep).as_deref(), Some(deep.as_path()));
    }

    /// A destination on golden's own filesystem always accepts the clone, so
    /// the check costs one stat and answers yes.
    #[test]
    fn a_destination_beside_golden_reaches() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = golden_in(&tmp);
        let destination = tmp.path().join("golden.wt").join("feature");
        assert_eq!(cow_reaches(&golden, &destination), Ok(()));
    }

    /// `/dev/shm` is a tmpfs on Linux, so it is a second filesystem that cannot
    /// answer `FICLONE`. A destination there must lose the clone backend.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_destination_on_another_filesystem_does_not_reach() {
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            println!("skipped: this host has no /dev/shm tmpfs");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let golden = golden_in(&tmp);
        if device(&golden) == device(shm) {
            println!("skipped: golden and /dev/shm share one filesystem");
            return;
        }
        let why = cow_reaches(&golden, &shm.join("klon-test-destination"))
            .expect_err("tmpfs cannot receive a reflink");
        assert!(
            why.starts_with("the destination is on another filesystem"),
            "unexpected reason {why}"
        );
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
        fs::create_dir(&common).unwrap();
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

    /// `doctor --repair` renames golden back after an interrupted `init`, which
    /// leaves its own common path stale. The cache write must not rebuild that
    /// directory tree (C7).
    #[test]
    fn a_common_directory_that_vanished_gets_no_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("gone").join(".git");
        let cache = Cache {
            version: PROBE_VERSION,
            backend: "copy".to_string(),
            reason: "x".to_string(),
            filesystem: "ext4".to_string(),
            created: time::now_rfc3339(),
        };
        write_cache(&common, &cache).unwrap();
        assert!(
            !tmp.path().join("gone").exists(),
            "the write must create nothing"
        );
    }
}
