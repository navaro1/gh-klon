//! The journal repair (handoff §7, R6). An interrupted `add` or `rm` leaves one
//! open entry. This module moves that entry to the prior valid state.
//!
//! Two callers use it. `doctor --repair` repairs every open entry. `add`
//! repairs the entry of its own destination before it validates the path, so a
//! repeated command recovers an interrupted one without `doctor`.

use crate::cli::init as cli_init;
use crate::journal::{self, Entry, Op, State};
use crate::{git, paths, process, Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// What one repair did. `failure` holds the reason when the entry stays open;
/// the caller then reports it and the next run tries again.
pub struct Outcome {
    /// One line per action taken.
    pub actions: Vec<String>,
    /// The reason the entry could not be closed.
    pub failure: Option<Error>,
}

impl Outcome {
    fn closed(actions: Vec<String>) -> Outcome {
        Outcome {
            actions,
            failure: None,
        }
    }

    fn open(actions: Vec<String>, failure: Error) -> Outcome {
        Outcome {
            actions,
            failure: Some(failure),
        }
    }
}

/// Repair one entry. The operation picks the tail: `add` undoes the steps that
/// ran or finishes the steps that remain, and `rm` finishes its own tail. An
/// interrupted `rm` that changed nothing leaves the klon in place.
pub fn entry(golden: &Path, common: &Path, entry: &Entry) -> Result<Outcome> {
    let mut actions = Vec::new();
    match entry.op {
        Op::Add => match entry.state {
            // Nothing was registered, unless the kill landed inside `git
            // worktree add`. Check the register list before the entry goes.
            State::Planned => {
                if git::is_registered(golden, &entry.path) {
                    if let Some(why) = unregister(golden, &entry.path, &mut actions) {
                        return Ok(Outcome::open(actions, why));
                    }
                } else {
                    actions.push("no worktree was registered".to_string());
                }
            }
            // The worktree exists and the working directory is partial.
            State::Registered | State::Cloned => {
                if let Some(why) = unregister(golden, &entry.path, &mut actions) {
                    return Ok(Outcome::open(actions, why));
                }
            }
            // The tree is correct and the lock is still on: finish the tail.
            State::CheckedOut => {
                git::run_quiet(
                    golden,
                    &["worktree", "unlock", &entry.path.to_string_lossy()],
                );
                actions.push(format!("unlocked {}", entry.path.display()));
            }
            // `add` wrote `ready` and stopped before it deleted the entry.
            State::Ready => actions.push("the klon is complete".to_string()),
            // `add` writes none of these: `removing` belongs to `rm`, and
            // `copied` and `swapped` belong to `init`.
            State::Removing | State::Copied | State::Swapped => {
                actions.push("add never reaches this state".to_string())
            }
        },
        Op::Rm => match entry.state {
            // Finish the `rm` tail. A trash copy that still holds a `.git` file
            // would keep the dead worktree alive for git, and a copy with no
            // background delete would sit in the trash until the next `prune`.
            State::Removing => {
                for copy in trash_copies(golden, &entry.path)? {
                    let file = copy.join(".git");
                    if is_file(&file) {
                        fs::remove_file(&file)
                            .map_err(Error::io(format!("delete {}", file.display())))?;
                        actions.push(format!("deleted the .git file in {}", copy.display()));
                    }
                    process::spawn_background_delete(&copy)?;
                    actions.push(format!("started the delete of {}", copy.display()));
                }
                git::run(golden, &["worktree", "prune"])?;
                actions.push("pruned the worktree list".to_string());
            }
            // `rm` stopped before the rename, so the klon is untouched.
            _ => actions.push("rm changed nothing; the klon stays".to_string()),
        },
        // C15 adds the `--volume` tail on top of this one.
        Op::Init => init(&entry.path, entry.state, &mut actions)?,
    }
    journal::remove(common, &entry.name)?;
    actions.push("deleted the journal entry".to_string());
    Ok(Outcome::closed(actions))
}

/// The `init` tail (spec §7 C7). `path` is golden, and the staging copies sit
/// next to it. The state says how far the conversion came; the paths on disk
/// say which half of the swap ran.
///
/// The repair has two steps. Step one puts golden back at its path when the
/// kill landed between the two renames: only the state `swapped` with a missing
/// golden can reach that, and `<golden>.klon-old` then holds the original.
/// Step two deletes every sibling copy that survived, whichever half of the
/// swap ran:
///
/// | State | Disk | Result |
/// |---|---|---|
/// | `planned` | golden in place, a partial `<golden>.klon-sub` | the staging copy goes |
/// | `copied` | golden in place, a complete staging copy | the staging copy goes |
/// | `swapped` | golden missing, `<golden>.klon-old` present | golden comes back, the staging copy goes |
/// | `swapped` | golden present, `<golden>.klon-old` present | the swap finished; the replaced copy goes |
/// | `swapped` | golden present, no `<golden>.klon-old` | the swap never started; the staging copy goes |
/// | `ready` | golden present | the replaced copy goes |
///
/// Every delete is the detached background delete, so a repair returns at once
/// and a second run finds nothing left to do.
fn init(golden: &Path, state: State, actions: &mut Vec<String>) -> Result<()> {
    let siblings = [
        cli_init::sibling(golden, cli_init::OLD_SUFFIX)?,
        cli_init::sibling(golden, cli_init::STAGING_SUFFIX)?,
        cli_init::sibling(golden, cli_init::PLAIN_SUFFIX)?,
    ];
    let old = &siblings[0];
    if !golden.exists() {
        if state != State::Swapped || !old.exists() {
            return Err(Error::klon(format!(
                "init left no directory at {} and none at {}; klon cannot repair that",
                golden.display(),
                old.display()
            )));
        }
        fs::rename(old, golden).map_err(Error::io(format!(
            "rename {} back to {}",
            old.display(),
            golden.display()
        )))?;
        actions.push(format!(
            "renamed {} back to {}",
            old.display(),
            golden.display()
        ));
    }
    // Golden is at its path now. Every remaining sibling is a copy that no
    // command reads: the staging copy of a conversion that stopped, or the
    // replaced golden of one that finished.
    for path in &siblings {
        if path.exists() {
            process::spawn_background_delete(path)?;
            actions.push(format!("started the delete of {}", path.display()));
        }
    }
    Ok(())
}

/// Unlock and remove a registered worktree, then prune. `git worktree remove`
/// refuses a locked worktree, so the unlock comes first (handoff §7). The
/// answer is the reason when the removal did not finish, else None.
fn unregister(golden: &Path, path: &Path, actions: &mut Vec<String>) -> Option<Error> {
    let text = path.to_string_lossy().into_owned();
    if path.exists() {
        if let Err(err) = crate::backend::make_removable(path) {
            eprintln!("klon: repair: {err}");
        }
    }
    git::run_quiet(golden, &["worktree", "unlock", &text]);
    actions.push(format!("unlocked {}", path.display()));
    let removed = git::run(golden, &["worktree", "remove", "--force", &text]);
    match &removed {
        Ok(_) => actions.push(format!("removed the worktree {}", path.display())),
        // A path that git no longer knows only needs a prune.
        Err(err) => {
            git::run_quiet(golden, &["worktree", "prune"]);
            actions.push(format!(
                "pruned {} after a failed remove: {}",
                path.display(),
                one_line(err)
            ));
        }
    }
    // The removal finished only when git forgot the path and the directory is
    // gone. Otherwise the entry stays open for another attempt.
    if git::is_registered(golden, path) || path.exists() {
        let why = match removed {
            Ok(_) => "the path is still there".to_string(),
            Err(err) => one_line(&err),
        };
        return Some(Error::klon(format!(
            "cannot remove the worktree {}: {why}",
            path.display()
        )));
    }
    None
}

/// Every `.trash` copy of `path`, sorted. `rm` renames the klon to
/// `<wt root>/.trash/<name>-<seconds>`, so the copies share that prefix.
fn trash_copies(golden: &Path, path: &Path) -> Result<Vec<PathBuf>> {
    let Some(name) = path.file_name() else {
        return Ok(Vec::new());
    };
    let trash = paths::default_wt_root(golden).join(".trash");
    let read = match fs::read_dir(&trash) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::io(format!("read {}", trash.display()))(err)),
    };
    let prefix = format!("{}-", name.to_string_lossy());
    let mut copies = Vec::new();
    for item in read {
        let item = item.map_err(Error::io(format!("read {}", trash.display())))?;
        if item.file_name().to_string_lossy().starts_with(&prefix) {
            copies.push(item.path());
        }
    }
    copies.sort();
    Ok(copies)
}

/// True for a plain file. A `.git` directory is a whole repository that
/// somebody moved into the trash by hand; klon never deletes that.
fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// A git error on one line, for an action string.
fn one_line(err: &Error) -> String {
    err.to_string().trim().replace('\n', "; ")
}
