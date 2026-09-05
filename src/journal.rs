//! The lifecycle journal (handoff §7). `add`, `rm`, and `init` (C7, C15) write
//! one entry per klon under `<common>/klon/journal/<name>.json` and delete it
//! when the command completes. An entry that survives the command marks an
//! interrupted transaction; `doctor --repair` moves it to the prior valid state.
//!
//! An entry holds no repository content: only the operation, the state, the
//! path, the branch, and the start time. Every write is atomic, so a crash
//! leaves either the old entry or the new one, never a half-written file.

use crate::time;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// The format version of an entry. An entry with another version fails closed.
pub const VERSION: u32 = 1;

/// The command that owns the entry. `doctor --repair` picks the tail to finish
/// from this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    Add,
    Rm,
    /// `gh klon init`, added in C7 and C15.
    Init,
}

/// The step of the transaction that completed last. The order is the order of
/// the `add` transaction in handoff §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// `add` chose a path and has changed nothing yet.
    Planned,
    /// `git worktree add --no-checkout --detach --lock` registered the path.
    Registered,
    /// The backend filled the working directory and rewrote the `.git` file.
    Cloned,
    /// `git checkout --force` and `git clean -fdq` ran.
    CheckedOut,
    /// `git worktree unlock` ran. The klon is complete.
    Ready,
    /// `rm` is about to rename the klon into `.trash`.
    Removing,
}

impl State {
    /// The name that appears in the file and in `KLON_TEST_PAUSE_AT`.
    pub fn key(self) -> &'static str {
        match self {
            State::Planned => "planned",
            State::Registered => "registered",
            State::Cloned => "cloned",
            State::CheckedOut => "checked-out",
            State::Ready => "ready",
            State::Removing => "removing",
        }
    }
}

/// One journal file. `name` names the file and never reaches the JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub version: u32,
    pub op: Op,
    pub state: State,
    pub path: PathBuf,
    /// The branch of the klon. It is null for `init`, which moves golden, and
    /// for a klon with a detached HEAD.
    pub branch: Option<String>,
    /// The start of the command, RFC 3339 in UTC.
    pub started: String,
    /// The file stem. It is derived from `path`, so the file does not carry it.
    #[serde(skip)]
    pub name: String,
}

impl Entry {
    /// A fresh entry in state `planned` for a command that starts now.
    pub fn new(op: Op, path: &Path, branch: Option<&str>) -> Entry {
        Entry {
            version: VERSION,
            op,
            state: State::Planned,
            path: path.to_path_buf(),
            branch: branch.map(str::to_string),
            started: time::now_rfc3339(),
            name: name_for(path),
        }
    }
}

/// The journal side of one transaction. `add`, `rm`, and `init` (C7, C15) hold
/// one `Record` and call `reach` after each step, so `doctor --repair` knows
/// which steps ran. `close` drops the entry when the command reaches a valid
/// end state.
pub struct Record {
    common: PathBuf,
    entry: Entry,
}

impl Record {
    /// Write the first entry, in state `planned`, before the first change.
    pub fn start(common: &Path, op: Op, path: &Path, branch: Option<&str>) -> Result<Record> {
        let record = Record {
            common: common.to_path_buf(),
            entry: Entry::new(op, path, branch),
        };
        write(&record.common, &record.entry)?;
        Ok(record)
    }

    /// Record the step that just completed.
    pub fn reach(&mut self, state: State) -> Result<()> {
        self.entry.state = state;
        write(&self.common, &self.entry)
    }

    /// Delete the entry. A completed command leaves no entry behind.
    pub fn close(&self) -> Result<()> {
        remove(&self.common, &self.entry.name)
    }
}

/// `<common>/klon/journal`.
pub fn dir(common: &Path) -> PathBuf {
    common.join("klon").join("journal")
}

/// The file stem for a klon path: the last component with every character
/// outside `[A-Za-z0-9._-]` replaced by `-`, and eight hex digits of the whole
/// path. Two klons that share a last component keep separate entries, and no
/// branch name can escape the journal directory.
pub fn name_for(path: &Path) -> String {
    let stem: String = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('.');
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    if stem.is_empty() {
        short
    } else {
        format!("{stem}-{short}")
    }
}

/// Write `entry` to `<common>/klon/journal/<name>.json`. The write goes to a
/// temporary file in the same directory and lands with one `rename`, so a
/// reader never sees a half-written entry.
pub fn write(common: &Path, entry: &Entry) -> Result<()> {
    let dir = dir(common);
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string_pretty(entry)
        .map_err(|err| Error::klon(format!("serialize the journal entry: {err}")))?;
    let final_path = dir.join(format!("{}.json", entry.name));
    let temp_path = dir.join(format!(".{}.{}.tmp", entry.name, std::process::id()));
    // klon does not call `fsync` here. The journal protects against a killed
    // process, and every reader on the host sees the renamed file at once. An
    // `fsync` would only add power-loss durability, and `rm` must return inside
    // 100 ms (R8), which one slow flush can exceed.
    fs::write(&temp_path, text.as_bytes())
        .map_err(Error::io(format!("write {}", temp_path.display())))?;
    if let Err(err) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::io(format!("write {}", final_path.display()))(err));
    }
    pause_if_requested(entry.state);
    Ok(())
}

/// Delete the entry named `name`. A missing entry is not an error, so a
/// repeated `rm` or a second `doctor --repair` stays quiet.
pub fn remove(common: &Path, name: &str) -> Result<()> {
    let path = dir(common).join(format!("{name}.json"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("delete {}", path.display()))(err)),
    }
}

/// Every entry under `<common>/klon/journal`, sorted by name. A missing
/// directory gives an empty list. An entry with an unknown `version` fails
/// closed: the caller reports the error and changes nothing.
pub fn list(common: &Path) -> Result<Vec<Entry>> {
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
        // A `.tmp` file belongs to a write that is still in flight.
        if let Some(name) = file.strip_suffix(".json") {
            if !name.is_empty() && !file.starts_with('.') {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.iter().map(|name| read_entry(&dir, name)).collect()
}

/// Read one entry. The version is checked before the rest is parsed, so a
/// future format with another shape still gives the version error.
fn read_entry(dir: &Path, name: &str) -> Result<Entry> {
    let path = dir.join(format!("{name}.json"));
    let text = fs::read_to_string(&path).map_err(Error::io(format!("read {}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| Error::klon(format!("{} is not valid JSON: {err}", path.display())))?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    match version {
        Some(v) if v == u64::from(VERSION) => {}
        Some(v) => {
            return Err(Error::klon(format!(
                "unknown journal version {v} in {}; upgrade klon",
                path.display()
            )))
        }
        None => {
            return Err(Error::klon(format!(
                "unknown journal version in {}; the version field is missing",
                path.display()
            )))
        }
    }
    let mut entry: Entry = serde_json::from_value(value)
        .map_err(|err| Error::klon(format!("{} is not a journal entry: {err}", path.display())))?;
    entry.name = name.to_string();
    Ok(entry)
}

/// Test-only crash injection. `KLON_TEST_PAUSE_AT=<state>` stops the process
/// after it wrote that state, so a test can send SIGKILL at a known point of
/// the transaction. klon never sets this variable itself and no command reads
/// it for any other purpose.
fn pause_if_requested(state: State) {
    let requested = std::env::var("KLON_TEST_PAUSE_AT").unwrap_or_default();
    if requested != state.key() {
        return;
    }
    eprintln!("klon: KLON_TEST_PAUSE_AT={requested}: waiting for a signal");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_holds_only_safe_characters() {
        let name = name_for(Path::new("/tmp/wt/feature/../weird name"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
            "unexpected name {name}"
        );
        assert!(!name.contains('/'));
    }

    #[test]
    fn two_paths_with_one_last_component_get_two_names() {
        let a = name_for(Path::new("/a/feature"));
        let b = name_for(Path::new("/b/feature"));
        assert_ne!(a, b);
        assert_eq!(a, name_for(Path::new("/a/feature")));
    }

    #[test]
    fn every_state_key_round_trips_through_json() {
        for state in [
            State::Planned,
            State::Registered,
            State::Cloned,
            State::CheckedOut,
            State::Ready,
            State::Removing,
        ] {
            let text = serde_json::to_string(&state).expect("serialize");
            assert_eq!(text, format!("\"{}\"", state.key()));
        }
    }
}
