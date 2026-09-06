//! `disk_budget` (spec §7 C29, R28). `add` asks this module whether one more
//! klon fits. The answer is an estimate: klon never walks every klon on every
//! `add`, and a budget is a guard rail, not an accountant.
//!
//! The estimate per klon, in order:
//!
//! | Source | When |
//! |---|---|
//! | `KLON_TEST_KLON_BYTES` | the variable is set; tests only |
//! | `btrfs fi du -s --raw` | `btrfs` is on PATH and the klon is a subvolume |
//! | golden's ignored directories | everywhere else |
//!
//! The last row is the default on ext4: every klon carries a copy of golden's
//! ignored build state, and that copy is what the disk pays for. klon measures
//! it once and multiplies by the klon count, so the check costs one walk.
//!
//! Over the budget, `add` refuses and names the least recently used klon with
//! no live process. Only `add --evict`, or `disk_budget_action = "hibernate"`,
//! hibernates that candidate.

use crate::backend::btrfs;
use crate::envelope::env;
use crate::{config, git, hibernate, paths, process, Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The test override of the per-klon estimate, in bytes.
const TEST_BYTES: &str = "KLON_TEST_KLON_BYTES";

/// What the check found. `add` reports it and stops, or evicts and continues.
struct Over {
    /// The estimated total with the new klon, in bytes.
    total: u64,
    /// The number of klons in that total, the new one included.
    count: usize,
    /// The least recently used klon with no live process, or None when every
    /// klon is busy and klon has nothing to offer.
    candidate: Option<Candidate>,
}

/// The klon that `add` names, and that `--evict` hibernates.
struct Candidate {
    path: PathBuf,
    branch: String,
}

/// Check the budget before `add` changes anything, and evict when asked.
///
/// `worktrees` is the current register list; the first entry is golden. `keep`
/// names the branches this `add` may claim, which can never be the candidate.
/// The answer is true when a klon was hibernated, so the caller reads the list
/// again. A repository with no `disk_budget` pays one `Option` test.
pub fn check(
    golden: &Path,
    common: &Path,
    config: &config::Config,
    worktrees: &[git::Worktree],
    keep: &[String],
    evict: bool,
) -> Result<bool> {
    let Some(text) = config.disk_budget.as_deref() else {
        return Ok(false);
    };
    let budget = parse_size(text).ok_or_else(|| {
        Error::klon(format!(
            "disk_budget = \"{text}\" is not a size; use a number with an optional K, M, G, or T"
        ))
    })?;
    let Some(over) = measure(golden, worktrees, keep, budget)? else {
        return Ok(false);
    };
    let candidate = match &over.candidate {
        Some(candidate) => candidate,
        None => return Err(refusal(&over, budget, None)),
    };
    if !(evict || hibernate::config_evicts(config)) {
        return Err(refusal(&over, budget, Some(candidate)));
    }
    hibernate::refuse_live(&candidate.path)?;
    eprintln!(
        "klon: disk budget: hibernating {} at {}",
        candidate.branch,
        candidate.path.display()
    );
    hibernate::hibernate(
        golden,
        common,
        worktrees,
        &candidate.path,
        &candidate.branch,
        // The clone of the new klon follows at once. A spare builder started
        // here would only compete with it for the disk; the `add` starts one
        // of its own when it finishes.
        true,
    )?;
    Ok(true)
}

/// The refusal line. It always holds `disk budget`, and it names the candidate
/// and the way to act on it, so the reader never has to guess the next command.
fn refusal(over: &Over, budget: u64, candidate: Option<&Candidate>) -> Error {
    let head = format!(
        "disk budget exceeded: {} klons would use about {}, over the disk_budget of {}",
        over.count,
        human(over.total),
        human(budget)
    );
    match candidate {
        Some(candidate) => Error::klon(format!(
            "{head}; the least recently used klon is {} at {}. \
             Run gh klon hibernate {}, or add --evict, or set disk_budget_action = \"hibernate\"",
            candidate.branch,
            candidate.path.display(),
            candidate.branch
        )),
        None => Error::klon(format!(
            "{head}; every klon is locked or has a live process, so klon has no candidate"
        )),
    }
}

/// The estimate, or None when the new klon still fits.
fn measure(
    golden: &Path,
    worktrees: &[git::Worktree],
    keep: &[String],
    budget: u64,
) -> Result<Option<Over>> {
    let klons: Vec<PathBuf> = worktrees
        .iter()
        .skip(1)
        .map(|w| paths::absolute(&w.path))
        .collect::<Result<Vec<_>>>()?;
    let mut fallback: Option<u64> = None;
    let mut total: u64 = 0;
    for klon in &klons {
        total = total.saturating_add(bytes_of(golden, klon, &mut fallback));
    }
    // The klon that `add` is about to make holds no bytes yet. It will hold a
    // copy of golden's ignored build state, which is what its siblings hold.
    // The btrfs measurement cannot answer for a tree that does not exist, and
    // golden's own exclusive bytes are the whole repository, not one klon.
    let new =
        test_bytes().unwrap_or_else(|| *fallback.get_or_insert_with(|| ignored_bytes(golden)));
    total = total.saturating_add(new);
    if total <= budget {
        return Ok(None);
    }
    Ok(Some(Over {
        total,
        count: klons.len() + 1,
        candidate: least_recently_used(worktrees, keep),
    }))
}

/// The unique bytes of one klon. `fallback` caches golden's ignored size, so a
/// repository with ten klons walks golden once.
fn bytes_of(golden: &Path, klon: &Path, fallback: &mut Option<u64>) -> u64 {
    if let Some(bytes) = test_bytes() {
        return bytes;
    }
    if let Some(bytes) = btrfs_exclusive(klon) {
        return bytes;
    }
    *fallback.get_or_insert_with(|| ignored_bytes(golden))
}

/// The `KLON_TEST_KLON_BYTES` override. klon never sets it itself, and no
/// command reads it for any other purpose.
fn test_bytes() -> Option<u64> {
    parse_size(std::env::var(TEST_BYTES).ok()?.trim())
}

/// `btrfs fi du -s --raw <klon>`, when the tool is there and the klon is a
/// subvolume. The `Exclusive` column is the bytes that only this klon holds,
/// which is exactly what the budget counts. Any failure gives None, and the
/// caller falls back; a missing `btrfs-progs` is the normal case (handoff §11).
fn btrfs_exclusive(klon: &Path) -> Option<u64> {
    if !btrfs::is_subvolume(klon) {
        return None;
    }
    let tool = btrfs::tool()?;
    let out = Command::new(&tool)
        .args(["filesystem", "du", "-s", "--raw"])
        .arg(klon)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // The header reads `Total Exclusive Set-shared Filename`, and one data
    // line follows. The first line whose first two fields are plain numbers is
    // that data line, so the header cannot be read as a measurement.
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let _total: u64 = fields.next()?.parse().ok()?;
        fields.next()?.parse().ok()
    })
}

/// The bytes in golden's ignored top-level directories. `git` names them, so
/// the answer follows `.gitignore` exactly; klon then walks only those trees.
/// `--directory` stops git inside each of them, so the list stays short.
fn ignored_bytes(golden: &Path) -> u64 {
    let listed = git::run(
        golden,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
            "-z",
        ],
    );
    let Ok(listed) = listed else {
        eprintln!("klon: cannot list the ignored files of golden; the disk budget counts none");
        return 0;
    };
    let mut total: u64 = 0;
    for rel in listed.split('\0').filter(|s| !s.is_empty()) {
        total = total.saturating_add(tree_bytes(&golden.join(rel)));
    }
    total
}

/// The bytes that `path` and everything below it occupy on disk. Blocks, not
/// apparent size, so a sparse file counts what it really costs, the way
/// `btrfs fi du` and `du` do. A symlink is never followed, so a link out of the
/// tree cannot pull a whole other filesystem into the count.
///
/// The walk is iterative. A directory tree that a build tool made can be deep,
/// and recursion over it would put the depth on the stack.
fn tree_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.blocks().saturating_mul(512);
    }
    let mut total: u64 = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(meta.blocks().saturating_mul(512));
            }
        }
    }
    total
}

/// The least recently used klon with no live process.
///
/// A klon's last use is the newer of two times: its `.klon/env` file, which
/// `add` writes once, and its index, which every `git` command in the klon
/// touches. The oldest of those across the klons is the candidate.
fn least_recently_used(worktrees: &[git::Worktree], keep: &[String]) -> Option<Candidate> {
    let mut best: Option<(SystemTime, Candidate)> = None;
    for worktree in worktrees.iter().skip(1) {
        let Ok(path) = paths::absolute(&worktree.path) else {
            continue;
        };
        let Some(branch) = worktree
            .branch
            .as_deref()
            .and_then(|b| b.strip_prefix("refs/heads/"))
        else {
            // A klon with a detached HEAD has no branch, and `wake` takes a
            // branch. klon never offers one as a candidate.
            continue;
        };
        // A locked klon is locked on purpose, and a klon with a live process
        // holds a build. Neither may disappear under its owner. A branch this
        // very `add` may claim is off the list too: hibernating it would let
        // the `add` build a fresh klon over the same path and hide the work
        // that the hibernation just saved.
        if keep.iter().any(|name| name == branch)
            || worktree.locked
            || process::live_process(&path).is_some()
        {
            continue;
        }
        let used = last_use(&path);
        let candidate = Candidate {
            path,
            branch: branch.to_string(),
        };
        if best.as_ref().is_none_or(|(when, _)| used < *when) {
            best = Some((used, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// The last use of one klon: the newer of its `.klon/env` file, which `add`
/// writes once, and its index, which every `git` command in the klon touches.
/// A klon whose files klon cannot read counts as used at the epoch, so a broken
/// klon is offered before a healthy one.
fn last_use(path: &Path) -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut files = vec![env::file(path)];
    if let Some(admin) = admin_dir(path) {
        files.push(admin.join("index"));
    }
    for file in files {
        if let Ok(when) = file.symlink_metadata().and_then(|m| m.modified()) {
            newest = newest.max(when);
        }
    }
    newest
}

/// `<common>/worktrees/<name>` from the klon's own `.git` file. git picks the
/// admin name itself and adds a suffix when two klons share a base name, so
/// klon reads the file instead of rebuilding the name.
fn admin_dir(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path.join(".git")).ok()?;
    let target = text
        .trim_end_matches(['\n', '\r'])
        .strip_prefix("gitdir: ")?;
    Some(PathBuf::from(target))
}

/// Parse `40G`, `500M`, `1024`, or `1.5G` into bytes. K, M, G, and T are
/// powers of 1024, with or without a `B` or `iB` tail. A bare number is bytes.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: f64 = number.parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let unit = unit.trim().to_ascii_lowercase();
    let scale: u64 = match unit.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1u64 << 40,
        _ => return None,
    };
    let bytes = number * scale as f64;
    (bytes <= u64::MAX as f64).then_some(bytes as u64)
}

/// A byte count as a person reads it: `1.0 GiB`, `600.0 MiB`, `512 B`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1 kib"), Some(1024));
        assert_eq!(parse_size("1G"), Some(1 << 30));
        assert_eq!(parse_size("40G"), Some(40 << 30));
        assert_eq!(parse_size("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_size("600M"), Some(600 << 20));
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("12X"), None);
        assert_eq!(parse_size("-1"), None);
    }

    #[test]
    fn byte_counts_read_well() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1 << 30), "1.0 GiB");
        assert_eq!(human(600 << 20), "600.0 MiB");
    }
}
