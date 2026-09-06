//! Check receipts (handoff §6, R25): the light proof that `gh klon check`
//! writes and `gh klon merge` reads.
//!
//! A receipt binds one run of the approved `[proof] steps` to one commit. It
//! lives in `<common>/klon/receipts/<commit>.json` and holds the commit, the
//! tree, the branch, a hash of the step list, one record per step, the
//! verdict, the duration, and the time. `merge` refuses a klon whose HEAD has
//! no passing receipt unless the user passes `--no-check`.
//!
//! **A receipt holds no environment values.** No variable, no working
//! directory, and no host name reaches the file. Only the step text from
//! `.klon.toml` is recorded, and that text is the same for every host. A
//! receipt is therefore safe to read, to copy, and to show.
//!
//! The full proof worktree and the execution manifest of the evidence-gated
//! proposal are a v2 candidate (handoff §2). This is the light form.
//!
//! **Extending a receipt.** C27 adds `claim_escape` to the struct. A new field
//! carries `#[serde(default)]`, so this klon still reads a receipt that an
//! older klon wrote, and `VERSION` stays 1 while every addition is optional.
//! A field removal or a type change bumps `VERSION`.

use crate::config;
use crate::time;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The format version of a receipt. A file with a higher version fails closed:
/// this binary cannot judge a shape it does not know (spec §4).
pub const VERSION: u32 = 1;

/// `prune` deletes a receipt older than this. A receipt is keyed by commit and
/// is a few hundred bytes, so klon keeps a month of them.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The verdict of one step and of the whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Every step exited zero.
    Pass,
    /// One step exited non-zero. The run stopped there.
    Failed,
}

impl Status {
    /// The name in the file and in `klon.check/1`.
    pub fn name(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Failed => "failed",
        }
    }
}

/// One `[proof] steps` entry and what it did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    /// The step text from `.klon.toml`, unchanged. klon runs it as `sh -c`.
    pub cmd: String,
    pub status: Status,
    pub duration_ms: u64,
}

/// One receipt. The field order here is the field order in the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    /// The full object id of the klon's HEAD when the run started.
    pub commit: String,
    /// The full object id of that commit's tree.
    pub tree: String,
    /// The branch the klon had checked out.
    pub branch: String,
    /// `steps_hash` of the step list that ran.
    pub steps_hash: String,
    /// One record per step that ran, in file order. The list stops at the
    /// first failure, so a failed receipt names fewer steps than the config.
    pub results: Vec<StepResult>,
    pub status: Status,
    /// The whole run, including the steps that did not run.
    pub duration_ms: u64,
    /// The end of the run, RFC 3339 in UTC.
    pub created: String,
}

/// What a klon's HEAD receipt says. `merge` turns each answer into a refusal
/// and `list` into a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A receipt for HEAD, for these steps, that passed.
    Pass,
    /// A receipt for HEAD, for these steps, that failed.
    Failed,
    /// A receipt for the branch, but not for HEAD, or one whose steps differ.
    Stale,
    /// No receipt for the branch at all. Nobody ran `check` here.
    Missing,
}

impl Verdict {
    /// The `receipt` field of `klon.list/2`. `Missing` is null there: a klon
    /// nobody checked has nothing to report.
    pub fn json(self) -> Option<&'static str> {
        match self {
            Verdict::Pass => Some("pass"),
            Verdict::Failed => Some("failed"),
            Verdict::Stale => Some("stale"),
            Verdict::Missing => None,
        }
    }

    /// The `list` column. A person reads the mark, not the word.
    pub fn column(self) -> &'static str {
        match self {
            Verdict::Pass => "✓",
            Verdict::Failed => "✗",
            Verdict::Stale => "stale",
            Verdict::Missing => "-",
        }
    }
}

/// The hash of a step list: SHA-256 of the steps joined by a NUL byte, in
/// lower-case hexadecimal. NUL cannot appear inside a step, so no two lists
/// join to the same bytes: `["a", "b"]` and `["a b"]` hash apart.
pub fn steps_hash(steps: &[String]) -> String {
    let mut hasher = Sha256::new();
    for (index, step) in steps.iter().enumerate() {
        if index > 0 {
            hasher.update([0u8]);
        }
        hasher.update(step.as_bytes());
    }
    config::hex(&hasher.finalize())
}

/// `<common>/klon/receipts`.
pub fn dir(common: &Path) -> PathBuf {
    crate::paths::absolute(common)
        .unwrap_or_else(|_| common.to_path_buf())
        .join("klon")
        .join("receipts")
}

/// The receipt file of one commit. The caller passes a full object id, so the
/// name holds hexadecimal only and can never leave the directory.
pub fn path(common: &Path, commit: &str) -> PathBuf {
    dir(common).join(format!("{commit}.json"))
}

/// Write the receipt of `commit`. The write goes to a temporary file in the
/// same directory and lands with one `rename`, so a reader never sees half a
/// receipt.
pub fn write(common: &Path, receipt: &Receipt) -> Result<PathBuf> {
    let dir = dir(common);
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(receipt)
        .map_err(|err| Error::klon(format!("serialize the receipt: {err}")))?;
    let final_path = dir.join(format!("{}.json", receipt.commit));
    let temp_path = dir.join(format!(".{}.{}.tmp", receipt.commit, std::process::id()));
    fs::write(&temp_path, text.as_bytes())
        .map_err(Error::io(format!("write {}", temp_path.display())))?;
    if let Err(err) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::io(format!("write {}", final_path.display()))(err));
    }
    Ok(final_path)
}

/// The receipt of `commit`, or None when there is none. A file this binary
/// cannot parse counts as none: a damaged receipt proves nothing, and the
/// caller then asks for a fresh `check`. A receipt from a future klon is the
/// one hard failure, because its shape may carry a rule this binary would
/// ignore (spec §4).
pub fn read(common: &Path, commit: &str) -> Result<Option<Receipt>> {
    let file = path(common, commit);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io(format!("read {}", file.display()))(err)),
    };
    check_version(&file, &text)?;
    Ok(serde_json::from_str(&text).ok())
}

/// The version field alone, read before the rest, so a future shape fails with
/// the version message instead of a field error.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

fn check_version(file: &Path, text: &str) -> Result<()> {
    let Ok(head) = serde_json::from_str::<VersionOnly>(text) else {
        return Ok(());
    };
    if head.version > VERSION {
        return Err(Error::klon(format!(
            "unknown receipt version {} in {}; upgrade klon",
            head.version,
            file.display()
        )));
    }
    Ok(())
}

/// The verdict for one klon: its HEAD, its branch, and the steps that are
/// configured now.
pub fn verdict(common: &Path, commit: &str, branch: &str, steps_hash: &str) -> Result<Verdict> {
    if let Some(receipt) = read(common, commit)? {
        if receipt.steps_hash != steps_hash {
            return Ok(Verdict::Stale);
        }
        return Ok(match receipt.status {
            Status::Pass => Verdict::Pass,
            Status::Failed => Verdict::Failed,
        });
    }
    // No receipt for HEAD. One for an earlier commit of the same branch means
    // somebody ran `check` and then committed, which is the stale case; no
    // receipt for the branch at all means nobody ran `check` here.
    Ok(match branch_was_checked(common, branch) {
        true => Verdict::Stale,
        false => Verdict::Missing,
    })
}

/// True when any receipt in the directory names `branch`. The scan stops at
/// the first match, and a file it cannot parse is skipped.
fn branch_was_checked(common: &Path, branch: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir(common)) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if serde_json::from_str::<Receipt>(&text).is_ok_and(|r| r.branch == branch) {
            return true;
        }
    }
    false
}

/// Delete every receipt older than `max_age`, by the file's own timestamp. A
/// receipt is written once and never changed, so its mtime is the time it was
/// made. `prune` calls this; a failure costs one stderr line, never the
/// command, because a stale receipt harms nobody.
pub fn prune(common: &Path, max_age: Duration) -> usize {
    let dir = dir(common);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|when| now.duration_since(when).unwrap_or_default() > max_age)
            .unwrap_or(false);
        if !old {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) => eprintln!("klon: cannot remove {}: {err}", path.display()),
        }
    }
    removed
}

/// A fresh receipt for a run that just ended.
pub fn build(
    commit: &str,
    tree: &str,
    branch: &str,
    steps_hash: &str,
    results: Vec<StepResult>,
    duration_ms: u64,
) -> Receipt {
    let status = match results.iter().all(|step| step.status == Status::Pass) {
        true => Status::Pass,
        false => Status::Failed,
    };
    Receipt {
        version: VERSION,
        commit: commit.to_string(),
        tree: tree.to_string(),
        branch: branch.to_string(),
        steps_hash: steps_hash.to_string(),
        results,
        status,
        duration_ms,
        created: time::now_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_steps_hash_separates_every_list() {
        let one = steps_hash(&steps(&["a", "b"]));
        assert_ne!(one, steps_hash(&steps(&["a b"])));
        assert_ne!(one, steps_hash(&steps(&["ab"])));
        assert_ne!(one, steps_hash(&steps(&["b", "a"])));
        assert_ne!(one, steps_hash(&steps(&["a"])));
        assert_eq!(one, steps_hash(&steps(&["a", "b"])));
        assert_eq!(steps_hash(&[]).len(), 64);
    }

    #[test]
    fn a_receipt_round_trips_through_json() {
        let receipt = build(
            "c0ffee",
            "7ea",
            "feature",
            &steps_hash(&steps(&["true"])),
            vec![StepResult {
                cmd: "true".to_string(),
                status: Status::Pass,
                duration_ms: 3,
            }],
            4,
        );
        let text = serde_json::to_string(&receipt).unwrap();
        assert!(text.contains("\"status\":\"pass\""), "{text}");
        let back: Receipt = serde_json::from_str(&text).unwrap();
        assert_eq!(back.commit, "c0ffee");
        assert_eq!(back.status, Status::Pass);
        assert_eq!(back.results.len(), 1);
    }

    #[test]
    fn one_failed_step_fails_the_receipt() {
        let receipt = build(
            "c0ffee",
            "7ea",
            "feature",
            "hash",
            vec![
                StepResult {
                    cmd: "true".to_string(),
                    status: Status::Pass,
                    duration_ms: 1,
                },
                StepResult {
                    cmd: "false".to_string(),
                    status: Status::Failed,
                    duration_ms: 1,
                },
            ],
            2,
        );
        assert_eq!(receipt.status, Status::Failed);
    }

    #[test]
    fn a_receipt_from_a_future_klon_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fs::create_dir_all(super::dir(common)).unwrap();
        fs::write(
            path(common, "abc"),
            format!("{{\"version\": {}}}", VERSION + 1),
        )
        .unwrap();
        let err = read(common, "abc").expect_err("a future version must refuse");
        assert!(err.to_string().contains("unknown receipt version"), "{err}");
    }

    #[test]
    fn a_damaged_receipt_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fs::create_dir_all(super::dir(common)).unwrap();
        fs::write(path(common, "abc"), "not json").unwrap();
        assert!(read(common, "abc").unwrap().is_none());
    }
}
