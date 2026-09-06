//! Hibernation (spec §7 C29, R28). A hibernated klon keeps its work in the
//! object store and gives its whole working directory back to the filesystem.
//!
//! Two things survive the removal:
//!
//! 1. One commit on `refs/klon/hibernate/<name>`. Its tree holds the tracked
//!    changes and the untracked, non-ignored files of the klon. One ref keeps
//!    the commit reachable, so `git gc` never drops it.
//! 2. One record in `<common>/klon/hibernate/<name>.json` with the head, the
//!    work commit, the path, the branch, and the loopback address.
//!
//! Together they cost a few hundred bytes outside the object store, so a
//! hibernated klon stays far under the 1 MB of R28.
//!
//! The ignored build state is **not** saved. `wake` runs a full `add`, so the
//! new klon takes golden's warm ignored directories instead. That is the whole
//! point: the build artifacts are the bytes the hibernation gives back.

use crate::journal::{self, State};
use crate::{config, git, paths, process, time, Error, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// The format version of a record. A record with another version fails closed.
pub const VERSION: u32 = 1;

/// The author and committer of a work commit. See `work_commit`.
const IDENTITY_NAME: &str = "gh-klon";
const IDENTITY_EMAIL: &str = "gh-klon@localhost";

/// One hibernated klon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    /// The HEAD of the klon at the moment of the hibernation.
    pub head: String,
    /// The commit that holds the tracked changes and the untracked files.
    pub work: String,
    /// The branch the klon had checked out.
    pub branch: String,
    /// The directory the klon lived in. `wake` puts it back there.
    pub path: PathBuf,
    /// The loopback address the klon held (R21), or null when it held none.
    pub ip: Option<String>,
    /// The time of the hibernation, RFC 3339 in UTC.
    pub created: String,
    /// The file stem. It is derived from the branch, so the file does not carry it.
    #[serde(skip)]
    pub name: String,
}

/// `<common>/klon/hibernate`.
pub fn dir(common: &Path) -> PathBuf {
    paths::absolute(common)
        .unwrap_or_else(|_| common.to_path_buf())
        .join("klon")
        .join("hibernate")
}

/// The stem of the record file and the last component of the ref, for a branch.
///
/// Every character outside `[A-Za-z0-9_-]` becomes `-`, and eight hex digits of
/// the branch follow. The result is always a legal ref component: it holds no
/// dot, so it can neither start with one, nor hold `..`, nor end with `.lock`;
/// and it holds no slash, so no branch name can escape the record directory.
pub fn name_for(branch: &str) -> String {
    use sha2::{Digest, Sha256};
    let stem: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = Sha256::digest(branch.as_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{stem}-{short}")
}

/// `refs/klon/hibernate/<name>`. One ref per hibernated klon.
pub fn ref_name(name: &str) -> String {
    format!("refs/klon/hibernate/{name}")
}

/// The journal file stem of a `hibernate` or a `wake`.
///
/// Both commands run an inner transaction over the same path: `hibernate` runs
/// the removal, which writes an `rm` entry, and `wake` runs a whole `add`,
/// which writes an `add` entry. One name each would put two entries in one
/// file. The `op` prefix keeps them apart, and `add`, which reads only
/// `journal::name_for`, never sees the entry of the `wake` that started it.
pub fn journal_name(op: &str, path: &Path) -> String {
    format!("{op}-{}", journal::name_for(path))
}

/// The record for `branch`, or None when the branch is not hibernated.
pub fn read(common: &Path, branch: &str) -> Result<Option<Record>> {
    let name = name_for(branch);
    let file = dir(common).join(format!("{name}.json"));
    if !file.exists() {
        return Ok(None);
    }
    read_file(&file, &name).map(Some)
}

/// Every record under `<common>/klon/hibernate`, sorted by name. A missing
/// directory gives an empty list. A record with an unknown `version` fails
/// closed, as the journal does.
pub fn list(common: &Path) -> Result<Vec<Record>> {
    let dir = dir(common);
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::io(format!("read {}", dir.display()))(err)),
    };
    let mut names = Vec::new();
    for item in read {
        let item = item.map_err(Error::io(format!("read {}", dir.display())))?;
        let file = item.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_suffix(".json") {
            if !name.is_empty() && !file.starts_with('.') {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
        .iter()
        .map(|name| read_file(&dir.join(format!("{name}.json")), name))
        .collect()
}

/// Read one record. The version is checked before the rest is parsed, so a
/// future format with another shape still gives the version error.
fn read_file(file: &Path, name: &str) -> Result<Record> {
    let text = fs::read_to_string(file).map_err(Error::io(format!("read {}", file.display())))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| Error::klon(format!("{} is not valid JSON: {err}", file.display())))?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(VERSION) => {}
        Some(v) => {
            return Err(Error::klon(format!(
                "unknown hibernate record version {v} in {}; upgrade klon",
                file.display()
            )))
        }
        None => {
            return Err(Error::klon(format!(
                "unknown hibernate record version in {}; the version field is missing",
                file.display()
            )))
        }
    }
    let mut record: Record = serde_json::from_value(value).map_err(|err| {
        Error::klon(format!(
            "{} is not a hibernate record: {err}",
            file.display()
        ))
    })?;
    record.name = name.to_string();
    Ok(record)
}

/// Write `record` to `<common>/klon/hibernate/<name>.json`. The write lands
/// with one `rename`, so a reader never sees a half-written record.
pub fn write(common: &Path, record: &Record) -> Result<()> {
    let dir = dir(common);
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(record)
        .map_err(|err| Error::klon(format!("serialize the hibernate record: {err}")))?;
    let final_path = dir.join(format!("{}.json", record.name));
    let temp_path = dir.join(format!(".{}.{}.tmp", record.name, std::process::id()));
    fs::write(&temp_path, text.as_bytes())
        .map_err(Error::io(format!("write {}", temp_path.display())))?;
    if let Err(err) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::io(format!("write {}", final_path.display()))(err));
    }
    Ok(())
}

/// Delete the record named `name`. A missing record is not an error.
pub fn remove_record(common: &Path, name: &str) -> Result<()> {
    let file = dir(common).join(format!("{name}.json"));
    match fs::remove_file(&file) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("delete {}", file.display()))(err)),
    }
}

/// Delete `refs/klon/hibernate/<name>`. A missing ref is not an error, so a
/// repeated `wake` and a repair both stay quiet.
///
/// A failure **is** an error. The ref is the only handle klon has on the saved
/// commit, so a ref that survives while the record goes leaves objects that no
/// klon command can reach or release. The caller reports it and the record
/// stays, which keeps the pair together for the next attempt.
pub fn remove_ref(golden: &Path, name: &str) -> Result<()> {
    let full = ref_name(name);
    if git::run(golden, &["rev-parse", "--verify", "--quiet", &full]).is_err() {
        return Ok(());
    }
    git::run(golden, &["update-ref", "-d", &full]).map(|_| ())
}

/// Roll one saved hibernation back: the ref and the record go, the klon stays.
/// `hibernate` calls it when a later step fails, and the repair calls it for an
/// entry that stopped in the state `saved`.
pub fn discard(golden: &Path, common: &Path, name: &str) -> Result<()> {
    remove_ref(golden, name)?;
    remove_record(common, name)
}

/// Save the work of the klon at `path` and remove the tree (R28).
///
/// The caller owns the refusals: this function assumes the klon is a registered
/// worktree with no live process. `add --evict` and `gh klon hibernate` both
/// call it, so both leave the same state behind.
///
/// The removal is `rm`'s own, under `Guard::Force` (C25): the work is already
/// on the ref, so a dirty tree must not stop it. The branch stays, so `wake`
/// can check it out again. That removal writes an `rm` journal entry for the
/// same path, so this entry carries a name of its own.
pub fn hibernate(
    golden: &Path,
    common: &Path,
    worktrees: &[git::Worktree],
    path: &Path,
    branch: &str,
    no_spare: bool,
) -> Result<Record> {
    let name = name_for(branch);
    if read(common, branch)?.is_some() {
        return Err(Error::klon(format!(
            "{branch} is already hibernated; run gh klon wake {branch} first"
        )));
    }
    let mut journal = journal::Record::start_as(
        common,
        &journal_name("hibernate", path),
        journal::Op::Hibernate,
        path,
        Some(branch),
    )?;
    let record = match save(golden, common, path, branch, &name) {
        Ok(record) => record,
        Err(err) => {
            // Nothing is removed yet, so the rollback is the ref and the record.
            let _ = discard(golden, common, &name);
            journal.close()?;
            return Err(err);
        }
    };
    // The state spans the removal below: a repair reads the register list and
    // the disk to tell a removal that ran from one that never started.
    journal.reach(State::Saved)?;
    crate::cli::rm::remove_target(
        golden,
        common,
        worktrees,
        path,
        Some(branch),
        crate::cli::rm::Guard::Force,
        no_spare,
    )?;
    journal.close()?;
    Ok(record)
}

/// Write the work commit, the ref, and the record. Nothing is removed here, so
/// a failure leaves the klon whole.
fn save(golden: &Path, common: &Path, path: &Path, branch: &str, name: &str) -> Result<Record> {
    let head = git::run(path, &["rev-parse", "HEAD"])?.trim().to_string();
    let work = work_commit(path, &head, branch)?;
    git::run(golden, &["update-ref", &ref_name(name), &work])?;
    let record = Record {
        version: VERSION,
        head,
        work,
        branch: branch.to_string(),
        path: path.to_path_buf(),
        ip: crate::envelope::env::value(path, "KLON_IP"),
        created: time::now_rfc3339(),
        name: name.to_string(),
    };
    write(common, &record)?;
    Ok(record)
}

/// Build the commit that holds the whole working state of the klon.
///
/// The spec names `git stash create` plus a second commit for the untracked
/// files. One temporary index does both in one tree: `git add -A` over the
/// working tree stages every tracked change **and** every untracked,
/// non-ignored file, and `git write-tree` turns that into one tree. The commit
/// takes the parent that `git stash create` would take, which is HEAD.
///
/// The temporary index lives in the klon's own admin directory, beside the real
/// one. A split index names its shared file by base name only, so an index
/// outside that directory could not resolve it.
///
/// The index is copied first, so `git add -A` keeps the stat cache and rehashes
/// only the files that changed. A fresh index would rehash the whole tree.
///
/// The commit carries klon's own identity, not the user's. It is machinery: it
/// never reaches a branch, a pull request, or a merge, and `wake` deletes it.
/// klon's identity also lets `hibernate` work in a repository where the user
/// configured none, which `git commit-tree` would otherwise refuse.
fn work_commit(path: &Path, head: &str, branch: &str) -> Result<String> {
    let admin = admin_dir(path)?;
    let temp = admin.join(format!("klon-hibernate.{}.index", std::process::id()));
    let result = (|| -> Result<String> {
        fs::copy(admin.join("index"), &temp).map_err(Error::io("copy the index"))?;
        let env: [(&str, &OsStr); 1] = [("GIT_INDEX_FILE", temp.as_os_str())];
        // `--renormalize` makes git hash every tracked file instead of trusting
        // the stat cache. `add` sets `core.checkStat=minimal`, which compares
        // only the size and the whole seconds of the modification time, so an
        // edit that keeps the size inside one second still looks clean. Without
        // the rehash `git add -A` would skip those bytes, and the removal that
        // follows would delete the only copy of them. The pass costs one hash
        // of the source tree, which a command that then deletes that tree can
        // afford (R28).
        git::run_env(path, &["add", "-A", "--renormalize"], &env)?;
        let tree = git::run_env(path, &["write-tree"], &env)?
            .trim()
            .to_string();
        let message = format!("klon hibernate {branch}\n");
        let author: [(&str, &OsStr); 4] = [
            ("GIT_AUTHOR_NAME", OsStr::new(IDENTITY_NAME)),
            ("GIT_AUTHOR_EMAIL", OsStr::new(IDENTITY_EMAIL)),
            ("GIT_COMMITTER_NAME", OsStr::new(IDENTITY_NAME)),
            ("GIT_COMMITTER_EMAIL", OsStr::new(IDENTITY_EMAIL)),
        ];
        Ok(git::run_env(
            path,
            &["commit-tree", &tree, "-p", head, "-m", &message],
            &author,
        )?
        .trim()
        .to_string())
    })();
    let _ = fs::remove_file(&temp);
    result
}

/// Put the saved work back into the klon at `path` (R28).
///
/// `git read-tree -m -u <work>` writes the saved tree into the index and the
/// working tree. `git checkout <work> -- .` would be wrong here: it leaves
/// every restored path staged. The reset that follows takes the index back to
/// the head, so the tracked changes end up unstaged and the untracked files end
/// up untracked, exactly as they were.
///
/// The reset carries the `-- .` pathspec. Without it `git reset` also moves the
/// branch, and a moved branch would silently lose its new commits. With it the
/// index moves and no ref does.
///
/// The caller checks the head first (`refuse_moved_branch`), so this function
/// only ever writes the saved tree over a klon that stands at that head.
pub fn restore(path: &Path, record: &Record) -> Result<()> {
    git::run(path, &["read-tree", "-m", "-u", &record.work])?;
    git::run(path, &["reset", "-q", &record.head, "--", "."])?;
    Ok(())
}

/// Refuse a `wake` whose branch moved while the klon slept.
///
/// The saved tree is a whole tree, not a patch. Writing it over a branch that
/// advanced would undo the new commits in the working directory and stage that
/// undo, which reads as work the developer never did. klon has no merge of its
/// own to offer here, so it stops before it touches anything and hands the
/// reader the two commands that resolve it. The saved work stays on the ref
/// either way.
pub fn refuse_moved_branch(golden: &Path, record: &Record) -> Result<()> {
    let full = format!("refs/heads/{}", record.branch);
    let current = git::run(golden, &["rev-parse", &full])?.trim().to_string();
    if current == record.head {
        return Ok(());
    }
    Err(Error::klon(format!(
        "{} moved from {} to {} while it slept, and klon restores a whole tree, not a patch. \
         Put the branch back with: git -C {} branch -f {} {}, then run gh klon wake {}. \
         The saved work stays on {}",
        record.branch,
        short(&record.head),
        short(&current),
        golden.display(),
        record.branch,
        record.head,
        record.branch,
        ref_name(&record.name)
    )))
}

/// Read `<path>/.git` and return `<common>/worktrees/<name>`.
fn admin_dir(path: &Path) -> Result<PathBuf> {
    let text = fs::read_to_string(path.join(".git")).map_err(Error::io("read .git"))?;
    text.strip_suffix('\n')
        .unwrap_or(&text)
        .strip_prefix("gitdir: ")
        .map(PathBuf::from)
        .ok_or_else(|| Error::klon(format!("unexpected .git file in {}", path.display())))
}

/// The first seven characters of an object name, for a message.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Refuse a klon that a process is still using. Both `hibernate` and the
/// `--evict` path call it, so neither one can take a tree out from under a
/// running build. The message holds the word `live`, as `rm`'s does.
pub fn refuse_live(path: &Path) -> Result<()> {
    if let Some(pid) = process::live_process(path) {
        return Err(Error::klon(format!(
            "{} has a live process (pid {pid}); hibernate refuses it",
            path.display()
        )));
    }
    Ok(())
}

/// The klons that `wake` and `list` may not see: a branch whose record names a
/// path that is a registered worktree again is awake, whatever the record says.
/// Only `doctor --repair` writes that state, so the check is one list scan.
pub fn is_awake(worktrees: &[git::Worktree], record: &Record) -> bool {
    worktrees.iter().any(|w| {
        paths::absolute(&w.path).is_ok_and(|p| p == record.path)
            || w.branch.as_deref() == Some(&format!("refs/heads/{}", record.branch))
    })
}

/// The `.klon.toml` answer for a klon that crosses the budget: true when the
/// file asks klon to hibernate the candidate without `--evict`.
pub fn config_evicts(config: &config::Config) -> bool {
    matches!(
        config.disk_budget_action,
        Some(config::BudgetAction::Hibernate)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_name_is_a_legal_ref_component() {
        for branch in [
            "feature",
            "feature/x",
            "a..b",
            ".hidden",
            "x.lock",
            "with space",
            "@",
            "unicode-łódź",
        ] {
            let name = name_for(branch);
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "unexpected name {name} for {branch}"
            );
            assert!(!name.starts_with('.'), "{name}");
            assert!(!name.contains(".."), "{name}");
            assert!(!name.ends_with(".lock"), "{name}");
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn two_branches_get_two_names() {
        assert_ne!(name_for("a/b"), name_for("a-b"));
        assert_eq!(name_for("feature"), name_for("feature"));
    }
}
