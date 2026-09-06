//! `<common>/klon/slots.json`: the loopback address allocator (R21).
//!
//! `lo` owns all of `127/8`, so a bind to `127.0.0.N` needs no configuration on
//! Linux. Each live klon holds one `N`. `add` takes the lowest free one and
//! `rm` gives it back, so a repository with three klons uses `127.0.0.2`,
//! `127.0.0.3`, and `127.0.0.4`.
//!
//! Every change runs under an exclusive `flock` on `<common>/klon/slots.lock`,
//! so two `add` commands in two terminals never take the same address. The
//! table itself lands with one `rename`, so a reader never sees a half file.

use crate::{time, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The format version of the table. A table with another version fails closed.
pub const VERSION: u32 = 1;

/// The lowest slot. `127.0.0.1` stays free, because the host's own services use it.
const FIRST: u8 = 2;

/// The highest slot. `127.0.0.255` is the broadcast address of `127/8`.
const LAST: u8 = 254;

/// One held address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    /// The branch of the klon that holds the address.
    pub name: String,
    /// The klon directory. A slot whose directory is gone is free again.
    pub path: PathBuf,
    /// The time of the allocation, RFC 3339 in UTC.
    pub created: String,
}

/// The whole file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Table {
    pub version: u32,
    /// The slot number to the klon that holds it.
    pub slots: BTreeMap<u8, Slot>,
}

/// `127.0.0.<n>`.
pub fn ip(n: u8) -> String {
    format!("127.0.0.{n}")
}

/// `<common>/klon`.
fn klon_dir(common: &Path) -> PathBuf {
    crate::paths::absolute(common)
        .unwrap_or_else(|_| common.to_path_buf())
        .join("klon")
}

fn table_path(common: &Path) -> PathBuf {
    klon_dir(common).join("slots.json")
}

/// Take the lowest free address for the klon at `path`. A path that already
/// holds an address keeps it, so a repeated `add` of one klon is idempotent.
pub fn allocate(common: &Path, name: &str, path: &Path) -> Result<String> {
    let lock = Lock::acquire(common)?;
    let mut table = load(common)?;
    prune(&mut table);
    if let Some((n, _)) = table.slots.iter().find(|(_, slot)| slot.path == path) {
        return Ok(ip(*n));
    }
    let free = (FIRST..=LAST)
        .find(|n| !table.slots.contains_key(n))
        .ok_or_else(|| {
            Error::klon(format!(
                "every loopback address from {} to {} is in use",
                ip(FIRST),
                ip(LAST)
            ))
        })?;
    table.slots.insert(
        free,
        Slot {
            name: name.to_string(),
            path: path.to_path_buf(),
            created: time::now_rfc3339(),
        },
    );
    save(common, &table)?;
    drop(lock);
    Ok(ip(free))
}

/// Give back the address of the klon at `path`. The answer is the freed slot
/// number, or None when the klon held none. A missing table is not an error.
///
/// The call writes nothing when it changes nothing. `rm` must return inside
/// 100 ms (R8), and a repository with no table at all then pays no syscall
/// beyond the one `stat` below.
pub fn release(common: &Path, path: &Path) -> Result<Option<u8>> {
    if !table_path(common).exists() {
        return Ok(None);
    }
    let lock = Lock::acquire(common)?;
    let mut table = load(common)?;
    let before = table.slots.len();
    let freed = table
        .slots
        .iter()
        .find(|(_, slot)| slot.path == path)
        .map(|(n, _)| *n);
    if let Some(n) = freed {
        table.slots.remove(&n);
    }
    prune(&mut table);
    if table.slots.len() != before {
        save(common, &table)?;
    }
    drop(lock);
    Ok(freed)
}

/// The number of addresses in use. `doctor` prints it. The read takes no lock,
/// because every write lands with one `rename`.
pub fn in_use(common: &Path) -> Result<usize> {
    Ok(load(common)?.slots.len())
}

/// Drop every slot whose klon directory is gone. A crash between the
/// allocation and the rollback leaves such a slot, and this reclaims it.
fn prune(table: &mut Table) {
    table.slots.retain(|_, slot| slot.path.exists());
}

/// The table on disk. A missing file gives an empty table. A file with an
/// unknown version fails closed, as the journal does.
fn load(common: &Path) -> Result<Table> {
    let path = table_path(common);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Table {
                version: VERSION,
                slots: BTreeMap::new(),
            })
        }
        Err(err) => return Err(Error::io(format!("read {}", path.display()))(err)),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| Error::klon(format!("{} is not valid JSON: {err}", path.display())))?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(VERSION) => {}
        Some(v) => {
            return Err(Error::klon(format!(
                "unknown slots version {v} in {}; upgrade klon",
                path.display()
            )))
        }
        None => {
            return Err(Error::klon(format!(
                "unknown slots version in {}; the version field is missing",
                path.display()
            )))
        }
    }
    serde_json::from_value(value)
        .map_err(|err| Error::klon(format!("{} is not a slot table: {err}", path.display())))
}

/// Write the table with one `rename`, so a concurrent reader sees either the
/// old table or the new one.
fn save(common: &Path, table: &Table) -> Result<()> {
    let dir = klon_dir(common);
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(table)
        .map_err(|err| Error::klon(format!("serialize the slot table: {err}")))?;
    let final_path = table_path(common);
    let temp_path = dir.join(format!(".slots.{}.tmp", std::process::id()));
    fs::write(&temp_path, text.as_bytes())
        .map_err(Error::io(format!("write {}", temp_path.display())))?;
    if let Err(err) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::io(format!("write {}", final_path.display()))(err));
    }
    Ok(())
}

/// An exclusive `flock` on `<common>/klon/slots.lock`. The lock file is never
/// renamed or deleted, so every holder locks one inode. Closing the descriptor
/// releases the lock, so a killed `add` never blocks the next one.
struct Lock {
    file: File,
}

impl Lock {
    fn acquire(common: &Path) -> Result<Lock> {
        let dir = klon_dir(common);
        fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
        let path = dir.join("slots.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(Error::io(format!("open {}", path.display())))?;
        loop {
            // SAFETY: the descriptor is open and owned by `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(Lock { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(Error::io(format!("lock {}", path.display()))(err));
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still open; the unlock cannot fail in a way
        // that matters, because the close below releases the lock anyway.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_address_is_two_and_the_next_add_takes_three() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path().join("common");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert_eq!(allocate(&common, "a", &a).unwrap(), "127.0.0.2");
        assert_eq!(allocate(&common, "b", &b).unwrap(), "127.0.0.3");
        // A repeated allocation for one path keeps the address.
        assert_eq!(allocate(&common, "a", &a).unwrap(), "127.0.0.2");
        assert_eq!(in_use(&common).unwrap(), 2);
    }

    #[test]
    fn a_released_address_goes_to_the_next_klon() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path().join("common");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let c = tmp.path().join("c");
        for dir in [&a, &b, &c] {
            fs::create_dir_all(dir).unwrap();
        }
        assert_eq!(allocate(&common, "a", &a).unwrap(), "127.0.0.2");
        assert_eq!(allocate(&common, "b", &b).unwrap(), "127.0.0.3");
        assert_eq!(release(&common, &a).unwrap(), Some(2));
        assert_eq!(release(&common, &a).unwrap(), None);
        assert_eq!(allocate(&common, "c", &c).unwrap(), "127.0.0.2");
    }

    #[test]
    fn a_slot_whose_directory_is_gone_is_free_again() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path().join("common");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert_eq!(allocate(&common, "a", &a).unwrap(), "127.0.0.2");
        fs::remove_dir_all(&a).unwrap();
        assert_eq!(allocate(&common, "b", &b).unwrap(), "127.0.0.2");
    }

    #[test]
    fn an_unknown_version_fails_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path().join("common");
        let dir = klon_dir(&common);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("slots.json"), r#"{"version": 99, "slots": {}}"#).unwrap();
        let err = in_use(&common).expect_err("an unknown version must fail");
        assert!(err.to_string().contains("unknown slots version 99"));
    }
}
