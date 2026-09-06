//! The `list` extras (spec §7 C30, R38): what a klon costs on disk and in
//! memory, and what GitHub knows about its branch.
//!
//! Every reading degrades on its own, and none of them can fail `list`:
//!
//! | Reading | Best source | Degrades to |
//! |---|---|---|
//! | disk | `btrfs fi du -s --raw` on a subvolume klon | the size of the ignored directories, which bounds the delta from above |
//! | processes | the `KLON_ID`/`KLON_DIR` scan | zero, when no process runs or the klon has no env file |
//! | RSS | `memory.current` of the scope cgroup | the sum of `VmRSS` over the tagged processes |
//! | PR and checks | `gh pr list --state all`, cached 60 s | `-`, after one stderr line |

use crate::backend::btrfs;
use crate::envelope::scope;
use crate::{gh, git, process, time};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

// --- The local readings -------------------------------------------------------

/// The local readings of one klon.
pub struct Extra {
    /// The unique bytes when the klon is a btrfs subvolume, else the size of
    /// the ignored directories, which only bounds the delta from above.
    pub disk_bytes: u64,
    /// True when `disk_bytes` comes from `btrfs fi du`.
    pub disk_exact: bool,
    /// The live processes of the klon.
    pub procs: usize,
    /// Their resident memory, or the usage of the scope cgroup, in bytes.
    pub rss_bytes: u64,
}

/// Measure one klon. `name` is the `KLON_NAME` of its env file. A klon from an
/// older klon version has none, so no process can be attributed to it, and its
/// process and memory readings stay zero.
pub fn measure(klon: &Path, name: Option<&str>) -> Extra {
    let (procs, rss_bytes) = match name {
        Some(name) => live(klon, name),
        None => (0, 0),
    };
    let (disk_bytes, disk_exact) = disk_of(klon);
    Extra {
        disk_bytes,
        disk_exact,
        procs,
        rss_bytes,
    }
}

/// The live processes of one klon and their resident memory. One `run` command
/// makes one scope cgroup (C20), so a cgroup read answers for the whole tree
/// at once; without a cgroup the answer is the per-process `VmRSS` sum.
fn live(klon: &Path, name: &str) -> (usize, u64) {
    let tags = vec![
        ("KLON_ID".to_string(), name.to_string()),
        ("KLON_DIR".to_string(), klon.to_string_lossy().into_owned()),
    ];
    let pids = process::klon_processes(&tags);
    if pids.is_empty() {
        return (0, 0);
    }
    let cgroups = scope::klon_cgroups(&pids, name);
    let rss: u64 = if cgroups.is_empty() {
        pids.iter().filter_map(|pid| vm_rss_bytes(*pid)).sum()
    } else {
        cgroups
            .iter()
            .map(|dir| file_bytes(&dir.join("memory.current")))
            .sum()
    };
    (pids.len(), rss)
}

/// The resident set of `pid` from `/proc/<pid>/status`, in bytes. The kernel
/// prints `VmRSS:  1234 kB` and kB means KiB here. A process that just left or
/// a kernel thread with no such line counts as zero.
#[cfg(target_os = "linux")]
fn vm_rss_bytes(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = text.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Other systems have no `/proc`; the process scan never finds a pid there, so
/// this never runs.
#[cfg(not(target_os = "linux"))]
fn vm_rss_bytes(_pid: u32) -> Option<u64> {
    None
}

/// The whole number in a one-number file, or zero. `memory.current` holds the
/// current usage of a cgroup in bytes.
fn file_bytes(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

// --- Disk ---------------------------------------------------------------------

/// The disk reading of one klon: the unique bytes `btrfs fi du` reports when
/// the klon is a subvolume and the tool is present, else the size of its
/// ignored directories, which only bounds the delta from above.
fn disk_of(klon: &Path) -> (u64, bool) {
    if btrfs::is_subvolume(klon) && btrfs::tool().is_some() {
        if let Some(bytes) = unique_bytes(klon) {
            return (bytes, true);
        }
        eprintln!(
            "klon: cannot read the unique bytes of {}; klon shows the ignored-directory size",
            klon.display()
        );
    }
    (ignored_bytes(klon), false)
}

/// Run `btrfs fi du -s --raw <klon>` and read the Exclusive column: the bytes
/// that no other file set shares, which is what the klon adds to its snapshot.
fn unique_bytes(klon: &Path) -> Option<u64> {
    let output = Command::new(btrfs::tool()?)
        .args(["fi", "du", "-s", "--raw"])
        .arg(klon)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    unique_bytes_from_du(&String::from_utf8_lossy(&output.stdout))
}

/// The Exclusive column of `btrfs fi du -s --raw`: the last line names the
/// total over the argument, and its three leading numbers are Total,
/// Exclusive, and Set shared.
fn unique_bytes_from_du(text: &str) -> Option<u64> {
    let last = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut numbers = last.split_whitespace();
    let total: u64 = numbers.next()?.parse().ok()?;
    let exclusive: u64 = numbers.next()?.parse().ok()?;
    let shared: u64 = numbers.next()?.parse().ok()?;
    (exclusive <= total && shared <= total).then_some(exclusive)
}

/// The size of every ignored path of the klon: the files and directories git
/// excludes, which is the data a clone copies and every other klon re-copies.
/// The listing stays cheap when the klon has none; only then the walk starts.
///
/// `-z` gives NUL-separated raw paths: git quotes nothing, so a path with
/// spaces, tabs, or non-ASCII bytes still resolves and the sum stays a true
/// upper bound.
fn ignored_bytes(klon: &Path) -> u64 {
    let out = match git::run(
        klon,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    ) {
        Ok(out) => out,
        // A broken klon shows no disk reading.
        Err(_) => return 0,
    };
    let mut total = 0;
    for path in out.split('\0').filter(|path| !path.is_empty()) {
        // klon's own state directory is not part of the warm copy.
        if path == ".klon/" {
            continue;
        }
        let full = klon.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if meta.is_dir() {
            total += dir_bytes(&full);
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// The byte size of every file below `dir`. A symlink counts as zero: its
/// target lives outside the reading. An unreadable directory contributes what
/// its readable part holds.
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            total += dir_bytes(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// `bytes` in binary units: `630 B`, `1.5 KiB`, `10 MiB`. One decimal below
/// ten of a unit, none from there up.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// --- The gh cache -------------------------------------------------------------

/// The version of the cache file format. A file of another version is dropped.
const CACHE_VERSION: u32 = 1;

/// How long a cached answer stands in for a `gh` call (R38).
const TTL_SECS: u64 = 60;

/// `<common>/klon/gh-cache.json`: one entry per branch, the same document the
/// radar keeps per commit pair.
#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

/// One branch of the cache: when the answer was fetched and the verbatim
/// `gh pr list --json` payload.
#[derive(Serialize, Deserialize)]
struct Entry {
    fetched: String,
    payload: Value,
}

/// The `pr` and `checks` columns of one branch. The answer comes from the
/// cache when it holds a fresh entry, else from one `gh pr list` call whose
/// payload is cached for the next 60 s. A missing or failing `gh` answers None
/// after one stderr line and caches nothing, so the next list asks again.
pub fn pr_of(cwd: &Path, common: &Path, branch: &str) -> Option<gh::PrFacts> {
    let file = common.join("klon").join("gh-cache.json");
    let mut cache = read_cache(&file);
    if let Some(entry) = cache.entries.get(branch) {
        if fresh(&entry.fetched) {
            return gh::pr_from_payload(&entry.payload);
        }
    }
    let payload = match gh::branch_pr_payload(cwd, branch) {
        Ok(payload) => payload,
        Err(err) => {
            eprintln!("klon: gh pr list failed: {err}; the pr and checks columns show -");
            return None;
        }
    };
    let facts = gh::pr_from_payload(&payload);
    cache.entries.insert(
        branch.to_string(),
        Entry {
            fetched: time::now_rfc3339(),
            payload,
        },
    );
    write_cache(&file, &cache);
    facts
}

/// Read the cache, or an empty one when the file is absent, unreadable, or of
/// another version.
fn read_cache(file: &Path) -> Cache {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Cache::empty();
    };
    match serde_json::from_str::<Cache>(&text) {
        Ok(cache) if cache.version == CACHE_VERSION => cache,
        _ => Cache::empty(),
    }
}

/// Write the cache. A failure costs one stderr line; the next run refetches.
fn write_cache(file: &Path, cache: &Cache) {
    let write = std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))
        .and_then(|()| serde_json::to_vec(cache).map_err(std::io::Error::other))
        .and_then(|bytes| std::fs::write(file, bytes));
    if let Err(err) = write {
        eprintln!("klon: cannot write {}: {err}", file.display());
    }
}

/// True when `fetched` names an instant younger than the cache lifetime.
fn fresh(fetched: &str) -> bool {
    let Some(then) = time::parse_rfc3339(fetched) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|age| age.as_secs() as i64)
        .unwrap_or(0);
    now - then < TTL_SECS as i64
}

impl Cache {
    fn empty() -> Cache {
        Cache {
            version: CACHE_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_units_read_naturally() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(630), "630 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(10 * 1024), "10 KiB");
        assert_eq!(human(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human(17 * 1024 * 1024 * 1024), "17 GiB");
    }

    #[test]
    fn the_du_total_names_the_exclusive_bytes() {
        // The shape of `btrfs fi du -s --raw` on one path: a header, then the
        // total over the argument.
        let text = "\t Total  \t Exclusive \t Set shared \t Filename\n\
                    \x20\x2065536\t 16384\t 49152\t /tmp/klon\n";
        assert_eq!(unique_bytes_from_du(text), Some(16_384));
    }

    #[test]
    fn a_du_report_klon_cannot_read_is_refused() {
        assert_eq!(unique_bytes_from_du(""), None);
        // The header alone parses no number.
        assert_eq!(
            unique_bytes_from_du("\t Total \t Exclusive \t Set shared \t Filename\n"),
            None
        );
        // Exclusive above the total is not a report klon understands.
        assert_eq!(unique_bytes_from_du("100\t200\t0\t/x\n"), None);
    }

    #[test]
    fn a_fresh_stamp_stands_in_for_gh() {
        assert!(fresh(&time::now_rfc3339()));
        assert!(!fresh("1970-01-01T00:00:00Z"));
        assert!(!fresh("not a stamp"));
    }

    #[test]
    fn a_cache_of_another_version_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("gh-cache.json");
        std::fs::write(&file, r#"{"version": 99, "entries": {}}"#).expect("write");
        assert!(read_cache(&file).entries.is_empty());
    }
}
