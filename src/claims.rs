//! `<common>/klon/claims.json`: the paths each klon owns (handoff §6, R26).
//!
//! Two agents in two klons edit one repository. Nothing stops them from
//! touching the same file, and the conflict then appears at the merge, hours
//! later. A claim moves that discovery to the front: a klon names the paths it
//! works on, and a second klon that names an overlapping path is refused at
//! once.
//!
//! The rules are small on purpose.
//!
//! - A claim names one path inside one klon, relative to the klon root.
//! - Two paths conflict when they are equal, or when one is a prefix of the
//!   other **at a component boundary**. `src/app` conflicts with
//!   `src/app/main.rs`, and `src/app` does not conflict with `src/apple`.
//! - The overlap check and the append both run under one exclusive `flock` on
//!   `<common>/klon/claims.lock`, so two `claim` commands at once cannot both
//!   take the same path.
//! - The table lands with one `rename`, so a reader never sees half a file and
//!   a reader needs no lock.
//!
//! A klon is named by its branch, because that is what a person types
//! (`gh klon claim <branch> <paths...>`) and what `list` and `rm` already
//! carry. `rm` releases the claims of the klon it removes, so a claim never
//! outlives its klon. A hibernated klon keeps its claims: its work comes back,
//! and the paths it owns must still be its own when it does.
//!
//! The file has a `version` field. An unknown version fails closed, as the
//! journal and the slot table do (handoff §7).

use crate::{paths, time, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// The format version of the table. A table with another version fails closed.
pub const VERSION: u32 = 1;

/// How long `claim` waits for the lock before it gives up. A claim writes a
/// few hundred bytes, so a holder that keeps the lock this long is stuck, not
/// busy, and a command that hangs forever is worse than one that says so.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The pause between two lock attempts. It bounds the wasted wake-ups at 200
/// per second while it keeps the wait under a millisecond in the normal case.
const LOCK_POLL: Duration = Duration::from_millis(5);

/// What the claimed path is on disk. A missing path is a `file`: a claim may
/// name a file that the klon has not written yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Dir,
    File,
}

impl Kind {
    /// The name in the file and in `klon.claim/1`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Dir => "dir",
            Kind::File => "file",
        }
    }
}

/// One owned path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    /// The branch of the klon that owns the path.
    pub klon: String,
    /// The path inside the klon, relative, with `/` separators.
    pub path: String,
    pub kind: Kind,
    /// The time of the claim, RFC 3339 in UTC.
    pub created: String,
}

/// The whole file.
#[derive(Debug, Serialize, Deserialize)]
pub struct Table {
    pub version: u32,
    pub claims: Vec<Claim>,
}

impl Table {
    /// A table with no claim in it. A repository that never ran `claim` has
    /// no file, and this is what every reader gets there.
    pub fn empty() -> Table {
        Table {
            version: VERSION,
            claims: Vec::new(),
        }
    }

    /// The paths that `klon` owns, in file order.
    pub fn paths_of(&self, klon: &str) -> Vec<String> {
        self.claims
            .iter()
            .filter(|claim| claim.klon == klon)
            .map(|claim| claim.path.clone())
            .collect()
    }

    /// True when a claim of `klon` conflicts with a claim of another klon.
    ///
    /// The append refuses such a pair, so a table can only reach this state
    /// through a hand-edited file or a klon that wrote without the lock.
    /// `list` still asks, because the answer decides who owns the path and a
    /// person must see the question.
    pub fn overlaps(&self, klon: &str) -> bool {
        self.claims
            .iter()
            .filter(|mine| mine.klon == klon)
            .any(|mine| {
                self.claims
                    .iter()
                    .any(|other| other.klon != klon && conflict(&mine.path, &other.path))
            })
    }
}

// --- The path rules ----------------------------------------------------------

/// True when `prefix` is `path` or an ancestor of it at a component boundary.
/// `src/app` covers `src/app` and `src/app/main.rs`; it does not cover
/// `src/apple`.
pub fn covers(prefix: &str, path: &str) -> bool {
    if prefix == path {
        return true;
    }
    path.len() > prefix.len() && path.starts_with(prefix) && path.as_bytes()[prefix.len()] == b'/'
}

/// True when two claims cannot both stand: one covers the other, either way
/// round.
pub fn conflict(a: &str, b: &str) -> bool {
    covers(a, b) || covers(b, a)
}

/// The changed paths that no claim of the klon covers. `check` prints one
/// `claim escape` line per answer and records the list in the receipt.
pub fn escapes(claims: &[String], changed: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|path| !claims.iter().any(|claim| covers(claim, path)))
        .cloned()
        .collect()
}

/// Read `raw` as a path inside the klon at `root`.
///
/// The answer is relative to the klon root, with `/` separators and no `.` or
/// `..` component. A relative path is read against the klon root, not against
/// the current directory: the branch names the klon, so the path belongs to
/// that tree wherever the person stands.
///
/// The call refuses an empty path, a `..` component, an absolute path outside
/// the klon, and a path that names the klon root itself. It touches no file;
/// `refuse_symlink_ancestor` does that part.
pub fn normalize(root: &Path, raw: &str) -> Result<String> {
    if raw.trim().is_empty() {
        return Err(Error::klon("a claim needs a path; the path is empty"));
    }
    let given = Path::new(raw);
    let relative = if given.is_absolute() {
        let absolute = paths::absolute(given)?;
        absolute
            .strip_prefix(root)
            .map_err(|_| {
                Error::klon(format!(
                    "{raw} is outside the klon at {}; a claim names a path inside it",
                    root.display()
                ))
            })?
            .to_path_buf()
    } else {
        given.to_path_buf()
    };
    let mut parts: Vec<&str> = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => parts.push(name.to_str().ok_or_else(|| {
                Error::klon(format!(
                    "{raw} is not valid UTF-8; a claim names a text path"
                ))
            })?),
            Component::ParentDir => {
                return Err(Error::klon(format!(
                    "{raw} holds a .. component; a claim never leaves the klon"
                )))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::klon(format!("{raw} is not a path inside the klon")))
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::klon(format!(
            "{raw} names the klon root; a claim names a path inside it"
        )));
    }
    Ok(parts.join("/"))
}

/// Refuse a path whose ancestor inside the klon is a symlink.
///
/// A symlinked ancestor makes the path text a poor name for the file: two
/// different claims would then own one file, and the overlap check compares
/// text. An ancestor that does not exist is fine, because a claim may name a
/// path the klon has not written yet.
pub fn refuse_symlink_ancestor(root: &Path, relative: &str) -> Result<()> {
    let parts: Vec<&str> = relative.split('/').collect();
    let mut walk = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        walk.push(part);
        let symlink = fs::symlink_metadata(&walk)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if symlink {
            return Err(Error::klon(format!(
                "{} is a symlink; a claim names a path with no symlink above it",
                walk.display()
            )));
        }
    }
    Ok(())
}

/// `dir` when the path is a directory in the klon, `file` otherwise. A missing
/// path and a symlink are both `file`: git records a symlink as a blob, and a
/// path that does not exist yet is a file the klon is about to write.
pub fn kind_of(root: &Path, relative: &str) -> Kind {
    match fs::symlink_metadata(root.join(relative)) {
        Ok(meta) if meta.is_dir() => Kind::Dir,
        _ => Kind::File,
    }
}

// --- The table on disk -------------------------------------------------------

/// `<common>/klon`.
fn klon_dir(common: &Path) -> PathBuf {
    paths::absolute(common)
        .unwrap_or_else(|_| common.to_path_buf())
        .join("klon")
}

/// `<common>/klon/claims.json`.
pub fn table_path(common: &Path) -> PathBuf {
    klon_dir(common).join("claims.json")
}

/// The table on disk. A missing file gives an empty table. A file with an
/// unknown version, or one this klon cannot parse, is an error: a table klon
/// cannot read is not a table with no claim in it, and answering "nobody owns
/// this path" from a file klon did not understand is the one wrong answer.
pub fn load(common: &Path) -> Result<Table> {
    let path = table_path(common);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Table::empty()),
        Err(err) => return Err(Error::io(format!("read {}", path.display()))(err)),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| Error::klon(format!("{} is not valid JSON: {err}", path.display())))?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(VERSION) => {}
        Some(v) => {
            return Err(Error::klon(format!(
                "unknown claims version {v} in {}; upgrade klon",
                path.display()
            )))
        }
        None => {
            return Err(Error::klon(format!(
                "unknown claims version in {}; the version field is missing",
                path.display()
            )))
        }
    }
    serde_json::from_value(value)
        .map_err(|err| Error::klon(format!("{} is not a claim table: {err}", path.display())))
}

/// Write the table with one `rename`, so a concurrent reader sees either the
/// old table or the new one.
fn save(common: &Path, table: &Table) -> Result<()> {
    let dir = klon_dir(common);
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(table)
        .map_err(|err| Error::klon(format!("serialize the claim table: {err}")))?;
    let final_path = table_path(common);
    let temp_path = dir.join(format!(".claims.{}.tmp", std::process::id()));
    fs::write(&temp_path, text.as_bytes())
        .map_err(Error::io(format!("write {}", temp_path.display())))?;
    if let Err(err) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::io(format!("write {}", final_path.display()))(err));
    }
    Ok(())
}

// --- The two writes ----------------------------------------------------------

/// Take `wanted` for `klon`. The overlap check and the append run under one
/// lock, so of two commands that want one path exactly one succeeds.
///
/// A path this klon already holds is kept, not repeated, so a second `claim`
/// of the same path is not an error. A path another klon holds, or one that
/// covers or sits under another klon's path, refuses the whole call and writes
/// nothing: a half-taken claim list would leave the caller guessing.
pub fn acquire(common: &Path, klon: &str, wanted: &[(String, Kind)]) -> Result<Vec<Claim>> {
    let lock = Lock::acquire(common)?;
    let mut table = load(common)?;
    for (path, _) in wanted {
        if let Some(other) = table
            .claims
            .iter()
            .find(|claim| claim.klon != klon && conflict(&claim.path, path))
        {
            return Err(Error::klon(format!(
                "claim conflict: {path} held by {}",
                other.klon
            )));
        }
    }
    let mut taken = Vec::with_capacity(wanted.len());
    let mut changed = false;
    for (path, kind) in wanted {
        match table
            .claims
            .iter()
            .find(|claim| claim.klon == klon && claim.path == *path)
        {
            Some(held) => taken.push(held.clone()),
            None => {
                let claim = Claim {
                    klon: klon.to_string(),
                    path: path.clone(),
                    kind: *kind,
                    created: time::now_rfc3339(),
                };
                table.claims.push(claim.clone());
                taken.push(claim);
                changed = true;
            }
        }
    }
    if changed {
        save(common, &table)?;
    }
    drop(lock);
    Ok(taken)
}

/// Give back the named paths of `klon`. The answer names the paths that the
/// klon really held; a path it did not hold is not an error, because the state
/// the caller asked for is the state it gets.
pub fn release(common: &Path, klon: &str, paths: &[String]) -> Result<Vec<String>> {
    let wanted: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    remove(common, klon, |claim| wanted.contains(claim.path.as_str()))
}

/// Give back every claim of `klon`. `rm` and `merge` call it, so a claim never
/// outlives the klon that took it.
pub fn release_all(common: &Path, klon: &str) -> Result<Vec<String>> {
    remove(common, klon, |_| true)
}

/// Drop every claim of `klon` that `wanted` accepts. The answer names the
/// dropped paths.
///
/// A repository with no table pays one `stat` and no lock. `rm` must return
/// inside 100 ms (R8), and most repositories never claim a path at all.
fn remove(common: &Path, klon: &str, wanted: impl Fn(&Claim) -> bool) -> Result<Vec<String>> {
    if !table_path(common).exists() {
        return Ok(Vec::new());
    }
    let lock = Lock::acquire(common)?;
    let mut table = load(common)?;
    let mut dropped = Vec::new();
    table.claims.retain(|claim| {
        let go = claim.klon == klon && wanted(claim);
        if go {
            dropped.push(claim.path.clone());
        }
        !go
    });
    if !dropped.is_empty() {
        save(common, &table)?;
    }
    drop(lock);
    Ok(dropped)
}

// --- The lock ----------------------------------------------------------------

/// An exclusive `flock` on `<common>/klon/claims.lock`. The lock file is never
/// renamed or deleted, so every holder locks one inode. Closing the descriptor
/// releases the lock, so a killed `claim` never blocks the next one.
struct Lock {
    file: File,
}

impl Lock {
    /// Wait for the lock, and give up after `LOCK_TIMEOUT`.
    ///
    /// The wait is a poll of the non-blocking form rather than one blocking
    /// `flock`, because `flock` has no deadline on Linux or on macOS and a
    /// command that waits forever on a stuck holder tells a person nothing.
    fn acquire(common: &Path) -> Result<Lock> {
        let dir = klon_dir(common);
        fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
        let path = dir.join("claims.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(Error::io(format!("open {}", path.display())))?;
        let started = Instant::now();
        loop {
            // SAFETY: the descriptor is open and owned by `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Lock { file });
            }
            let err = std::io::Error::last_os_error();
            let busy = matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            );
            if !busy {
                return Err(Error::io(format!("lock {}", path.display()))(err));
            }
            if started.elapsed() >= LOCK_TIMEOUT {
                return Err(Error::klon(format!(
                    "another klon command has held {} for {} s; try again",
                    path.display(),
                    LOCK_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(LOCK_POLL);
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still open; the close below releases the
        // lock anyway, so a failure here cannot leave the lock held.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(paths: &[&str]) -> Vec<(String, Kind)> {
        paths
            .iter()
            .map(|p| ((*p).to_string(), Kind::File))
            .collect()
    }

    #[test]
    fn a_prefix_only_covers_at_a_component_boundary() {
        assert!(covers("src/app", "src/app"));
        assert!(covers("src/app", "src/app/main.rs"));
        assert!(covers("src/app", "src/app/deep/main.rs"));
        assert!(!covers("src/app", "src/apple"));
        assert!(!covers("src/app", "src/apple/main.rs"));
        assert!(!covers("src/app/main.rs", "src/app"));
        assert!(conflict("src/app", "src/app/main.rs"));
        assert!(conflict("src/app/main.rs", "src/app"));
        assert!(!conflict("src/app", "src/apple"));
    }

    #[test]
    fn normalize_folds_the_path_and_refuses_an_escape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        assert_eq!(normalize(root, "src/a").unwrap(), "src/a");
        assert_eq!(normalize(root, "./src//a/").unwrap(), "src/a");
        assert_eq!(
            normalize(root, &root.join("src/a").display().to_string()).unwrap(),
            "src/a"
        );
        for bad in ["", "  ", ".", "..", "src/../../x", "/etc/passwd"] {
            assert!(normalize(root, bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn a_symlink_ancestor_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert!(refuse_symlink_ancestor(root, "real/a").is_ok());
        assert!(refuse_symlink_ancestor(root, "missing/a").is_ok());
        assert!(refuse_symlink_ancestor(root, "link/a").is_err());
        // The last component may be a symlink: the claim then names the link.
        assert!(refuse_symlink_ancestor(root, "link").is_ok());
    }

    #[test]
    fn the_kind_follows_the_path_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "").unwrap();
        assert_eq!(kind_of(root, "src"), Kind::Dir);
        assert_eq!(kind_of(root, "src/a.rs"), Kind::File);
        assert_eq!(kind_of(root, "src/missing.rs"), Kind::File);
    }

    #[test]
    fn a_second_klon_cannot_take_a_held_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path();
        acquire(common, "left", &wanted(&["src/app"])).unwrap();
        let err = acquire(common, "right", &wanted(&["src/app/main.rs"]))
            .expect_err("a path under a held directory must refuse");
        assert!(err.to_string().contains("claim conflict"), "{err}");
        // The refused call wrote nothing.
        assert_eq!(load(common).unwrap().claims.len(), 1);
        // A path beside it is free.
        acquire(common, "right", &wanted(&["src/apple"])).unwrap();
        assert_eq!(load(common).unwrap().claims.len(), 2);
    }

    #[test]
    fn a_repeated_claim_of_one_path_is_not_a_second_row() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path();
        acquire(common, "left", &wanted(&["src/app"])).unwrap();
        acquire(common, "left", &wanted(&["src/app"])).unwrap();
        assert_eq!(load(common).unwrap().claims.len(), 1);
    }

    #[test]
    fn release_takes_the_named_paths_and_release_all_takes_the_rest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path();
        acquire(common, "left", &wanted(&["a", "b", "c"])).unwrap();
        acquire(common, "right", &wanted(&["d"])).unwrap();
        assert_eq!(
            release(common, "left", &["b".to_string()]).unwrap(),
            vec!["b".to_string()]
        );
        // A path the klon does not hold is not an error.
        assert!(release(common, "left", &["zz".to_string()])
            .unwrap()
            .is_empty());
        // One klon never releases another's path.
        assert!(release(common, "left", &["d".to_string()])
            .unwrap()
            .is_empty());
        assert_eq!(release_all(common, "left").unwrap().len(), 2);
        let table = load(common).unwrap();
        assert_eq!(table.paths_of("right"), vec!["d".to_string()]);
        assert!(table.paths_of("left").is_empty());
    }

    #[test]
    fn a_release_with_no_table_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path();
        assert!(release_all(common, "left").unwrap().is_empty());
        assert!(!table_path(common).exists());
    }

    #[test]
    fn an_unknown_version_fails_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path();
        fs::create_dir_all(klon_dir(common)).unwrap();
        fs::write(table_path(common), r#"{"version": 99, "claims": []}"#).unwrap();
        let err = load(common).expect_err("an unknown version must fail");
        assert!(
            err.to_string().contains("unknown claims version 99"),
            "{err}"
        );
    }

    /// A hand-edited file can hold two klons on one path. `list` must see it.
    #[test]
    fn the_overlap_check_reads_a_hand_edited_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path();
        fs::create_dir_all(klon_dir(common)).unwrap();
        let table = Table {
            version: VERSION,
            claims: vec![
                Claim {
                    klon: "left".into(),
                    path: "src/app".into(),
                    kind: Kind::Dir,
                    created: "2026-09-05T10:00:00Z".into(),
                },
                Claim {
                    klon: "right".into(),
                    path: "src/app/main.rs".into(),
                    kind: Kind::File,
                    created: "2026-09-05T10:00:00Z".into(),
                },
                Claim {
                    klon: "far".into(),
                    path: "src/apple".into(),
                    kind: Kind::Dir,
                    created: "2026-09-05T10:00:00Z".into(),
                },
            ],
        };
        save(common, &table).unwrap();
        let back = load(common).unwrap();
        assert!(back.overlaps("left"));
        assert!(back.overlaps("right"));
        assert!(!back.overlaps("far"));
        assert!(!back.overlaps("nobody"));
    }

    #[test]
    fn an_escape_is_a_changed_path_no_claim_covers() {
        let claims = vec!["src/app".to_string(), "docs/one.md".to_string()];
        let changed = vec![
            "src/app/main.rs".to_string(),
            "src/apple/main.rs".to_string(),
            "docs/one.md".to_string(),
            "README.md".to_string(),
        ];
        assert_eq!(
            escapes(&claims, &changed),
            vec!["src/apple/main.rs".to_string(), "README.md".to_string()]
        );
        // A klon with no claim escapes nothing: the caller skips the check.
        assert_eq!(escapes(&[], &changed).len(), changed.len());
    }
}
