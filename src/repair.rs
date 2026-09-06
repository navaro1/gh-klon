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
            // `attached`, `copied`, and `swapped` belong to `init`.
            State::Removing | State::Attached | State::Copied | State::Swapped => {
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
        Op::Init => {
            // C15: `init --volume` moves golden onto a loop volume, so its
            // tail names an image and a mount point that no path beside golden
            // can hold. The entry carries the record that names them.
            if let Some(mark) = entry.volume.clone() {
                if let Some(why) = volume_init(entry, &mark, &mut actions)? {
                    return Ok(Outcome::open(actions, why));
                }
                close_init(common, entry, &mut actions)?;
                return Ok(Outcome::closed(actions));
            }
            if let Some(why) = init(&entry.path, entry.state, &mut actions)? {
                // The report still prints, and the entry waits for another
                // attempt. Every other operation ends the same way.
                return Ok(Outcome::open(actions, why));
            }
            close_init(common, entry, &mut actions)?;
            return Ok(Outcome::closed(actions));
        }
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
fn init(golden: &Path, state: State, actions: &mut Vec<String>) -> Result<Option<Error>> {
    let old = cli_init::sibling(golden, cli_init::OLD_SUFFIX)?;
    if !golden.exists() {
        if state != State::Swapped || !old.exists() {
            return Ok(Some(Error::klon(format!(
                "init left no directory at {} and none at {}; klon cannot repair that",
                golden.display(),
                old.display()
            ))));
        }
        if let Err(err) = fs::rename(&old, golden) {
            return Ok(Some(Error::io(format!(
                "rename {} back to {}",
                old.display(),
                golden.display()
            ))(err)));
        }
        actions.push(format!(
            "renamed {} back to {}",
            old.display(),
            golden.display()
        ));
    }
    // Golden is at its path now. Every remaining sibling is a copy that no
    // command reads: the staging copy of a conversion that stopped, or a
    // replaced golden whose background delete never started.
    for path in init_leftovers(golden)? {
        process::spawn_background_delete(&path)?;
        actions.push(format!("started the delete of {}", path.display()));
    }
    Ok(None)
}

/// Drop the entry of a finished `init` repair.
///
/// `init` keeps its journal inside golden, and the repair may have just
/// renamed golden back under the caller's feet. The entry is therefore removed
/// from the common directory the caller resolved and from the one golden holds
/// now. A missing file is not an error, so the removal that finds nothing stays
/// quiet.
fn close_init(common: &Path, entry: &Entry, actions: &mut Vec<String>) -> Result<()> {
    journal::remove(common, &entry.name)?;
    if let Ok(moved) = git::common_dir_of_main(&entry.path) {
        journal::remove(&moved, &entry.name)?;
    }
    actions.push("deleted the journal entry".to_string());
    Ok(())
}

/// The `init --volume` tail (spec §7 C15). `entry.path` is golden's stable
/// path: a directory before the swap and a symlink to the volume after it.
///
/// The state says how far the transaction came, and the shape of golden's path
/// says which half of the swap ran:
///
/// | Direction | State | Golden's path | Result |
/// |---|---|---|---|
/// | forward | `planned`, `attached`, `copied` | a directory | the staged copy goes; the image stays, and the report names it |
/// | forward | `swapped`, `ready` | missing, `<golden>.klon-old` present | golden comes back, then the staged copy goes |
/// | forward | `swapped`, `ready` | a symlink | the swap finished: write the record, repair the worktrees, drop the replaced copy |
/// | undo | `planned`, `copied` | a symlink | only `<golden>.klon-plain` goes |
/// | undo | `swapped`, `ready` | missing, `<golden>.klon-old` present | the symlink comes back and golden stays on the volume |
/// | undo | `swapped`, `ready` | a directory | the swap finished: forget the record and name the image |
///
/// The image is never deleted here. It holds golden's only copy in the window
/// where the repair cannot tell what landed, and a printed path costs the user
/// one command where a wrong delete costs a repository.
fn volume_init(
    entry: &Entry,
    mark: &journal::VolumeMark,
    actions: &mut Vec<String>,
) -> Result<Option<Error>> {
    let golden = &entry.path;
    let old = cli_init::sibling(golden, cli_init::OLD_SUFFIX)?;
    // One window has no golden at all: the kill landed between the two
    // renames. `<golden>.klon-old` then holds what stood there, a directory in
    // the forward direction and the symlink in the undo direction.
    if fs::symlink_metadata(golden).is_err() {
        if !matches!(entry.state, State::Swapped | State::Ready) || !exists(&old) {
            return Ok(Some(Error::klon(format!(
                "init --volume left no directory at {} and none at {}; klon cannot repair \
                 that. The volume image {} still holds a copy of golden.",
                golden.display(),
                old.display(),
                mark.record.image.display()
            ))));
        }
        if let Err(err) = fs::rename(&old, golden) {
            return Ok(Some(Error::io(format!(
                "rename {} back to {}",
                old.display(),
                golden.display()
            ))(err)));
        }
        actions.push(format!(
            "renamed {} back to {}",
            old.display(),
            golden.display()
        ));
    }
    let on_the_volume = fs::symlink_metadata(golden).is_ok_and(|meta| meta.is_symlink());
    if mark.undo {
        if !on_the_volume {
            // The undo finished the swap. Golden is a plain directory again,
            // and the record it carried over names a volume it left.
            finish_undo(golden, &mark.record, actions)?;
        }
    } else if on_the_volume {
        finish_make(golden, &mark.record, actions)?;
    } else {
        // The conversion never swapped, so golden never moved. The staged
        // copy on the volume is the only thing to drop.
        drop_staged(&mark.record, actions);
        actions.push(format!(
            "the volume image {} stays; delete it with rm -f {}",
            mark.record.image.display(),
            mark.record.image.display()
        ));
    }
    for path in init_leftovers(golden)? {
        // `rm -rf` on a symlink removes the link and never its target, so the
        // undo leftover cannot take the volume content with it.
        process::spawn_background_delete(&path)?;
        actions.push(format!("started the delete of {}", path.display()));
    }
    Ok(None)
}

/// Finish a conversion that swapped golden onto the volume but stopped before
/// it wrote the record. Without the record no later command can find the image
/// again, and the next reboot would leave golden's symlink dangling for good.
fn finish_make(
    golden: &Path,
    record: &crate::volume::Volume,
    actions: &mut Vec<String>,
) -> Result<()> {
    let common = git::common_dir_of_main(golden)?;
    crate::volume::write(&common, record)?;
    actions.push(format!(
        "wrote the volume record for {}",
        record.image.display()
    ));
    git::run_quiet(&record.golden_new, &["worktree", "repair"]);
    crate::backend::forget_probe(&common)?;
    Ok(())
}

/// Finish an undo that put golden back but stopped before it dropped the
/// record. The volume stays mounted and the image stays on disk: the repair
/// names both and lets the user finish.
fn finish_undo(
    golden: &Path,
    record: &crate::volume::Volume,
    actions: &mut Vec<String>,
) -> Result<()> {
    let common = git::common_dir_of_main(golden)?;
    crate::volume::forget(&common, &record.golden_old)?;
    actions.push("dropped the volume record".to_string());
    git::run_quiet(golden, &["worktree", "repair"]);
    crate::backend::forget_probe(&common)?;
    drop_staged(record, actions);
    actions.push(format!(
        "the volume image {} stays; run gh klon init --volume --undo is not needed, \
         delete it with rm -f {}",
        record.image.display(),
        record.image.display()
    ));
    Ok(())
}

/// Remove the copy of golden that the conversion staged on the volume. It runs
/// detached, like every other klon delete, because the mount stays up.
fn drop_staged(record: &crate::volume::Volume, actions: &mut Vec<String>) {
    if !exists(&record.golden_new) {
        return;
    }
    if let Err(err) = crate::backend::make_removable(&record.golden_new) {
        eprintln!("klon: repair: {err}");
    }
    match process::spawn_background_delete(&record.golden_new) {
        Ok(()) => actions.push(format!(
            "started the delete of the staged copy {}",
            record.golden_new.display()
        )),
        Err(err) => eprintln!("klon: repair: {err}"),
    }
}

/// True when the path names something, a dangling symlink included.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Every `init` leftover beside golden that still exists.
///
/// The staging copies carry an exact name. The replaced golden carries
/// `<golden>.klon-old`, or `<golden>.klon-old.<pid>.<n>` once `init` renamed it
/// out of the way for the background delete.
///
/// The match is exact on all four shapes. A prefix test would also select a
/// directory that a person made, for example `<golden>.klon-old-backup`, and
/// `--repair` deletes what it selects.
fn init_leftovers(golden: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for suffix in [
        cli_init::STAGING_SUFFIX,
        cli_init::PLAIN_SUFFIX,
        cli_init::OLD_SUFFIX,
    ] {
        let path = cli_init::sibling(golden, suffix)?;
        // A dangling symlink is a leftover too: `init --volume --undo` leaves
        // one when its own volume goes down before the repair runs (C15).
        if exists(&path) {
            found.push(path);
        }
    }
    // The parent comes from a resolved sibling, not from `golden` itself, so
    // every path in the answer starts at the same root. On macOS `/var` is a
    // symlink to `/private/var`, and a list from two roots sorts by two roots.
    let resolved = cli_init::sibling(golden, cli_init::OLD_SUFFIX)?;
    let (Some(parent), Some(name)) = (resolved.parent(), golden.file_name()) else {
        return Ok(found);
    };
    let prefix = format!("{}{}.", name.to_string_lossy(), cli_init::OLD_SUFFIX);
    let read = match fs::read_dir(parent) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(err) => return Err(Error::io(format!("read {}", parent.display()))(err)),
    };
    for item in read {
        let item = item.map_err(Error::io(format!("read {}", parent.display())))?;
        if is_discarded_golden(&item.file_name().to_string_lossy(), &prefix) {
            found.push(item.path());
        }
    }
    found.sort();
    Ok(found)
}

/// True for `<prefix><pid>.<n>`, the name `init` gives the replaced golden
/// before the background delete. Both tails must be plain digits, so a
/// directory that a person named cannot match.
fn is_discarded_golden(name: &str, prefix: &str) -> bool {
    let Some(tail) = name.strip_prefix(prefix) else {
        return false;
    };
    match tail.split_once('.') {
        Some((pid, n)) => {
            !pid.is_empty()
                && !n.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && n.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `--repair` deletes what `init_leftovers` selects, so the match must
    /// cover exactly the names `init` generates and nothing a person made.
    #[test]
    fn only_a_generated_name_counts_as_a_discarded_golden() {
        let prefix = "repo.klon-old.";
        assert!(is_discarded_golden("repo.klon-old.1234.0", prefix));
        assert!(is_discarded_golden("repo.klon-old.7.63", prefix));
        for other in [
            "repo.klon-old-backup",
            "repo.klon-old.backup",
            "repo.klon-old.1234",
            "repo.klon-old.1234.",
            "repo.klon-old..0",
            "repo.klon-old.1234.0a",
            "repo.klon-oldish.1.2",
            "repo.klon-old",
        ] {
            assert!(
                !is_discarded_golden(other, prefix),
                "{other} must not match"
            );
        }
    }

    /// The exact `<golden>.klon-old` name is still a leftover: `init` writes it
    /// with the first rename and a kill can leave it there.
    #[test]
    fn the_plain_old_name_is_a_leftover() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = tmp.path().join("repo");
        fs::create_dir(&golden).unwrap();
        for name in [
            "repo.klon-old",
            "repo.klon-sub",
            "repo.klon-old.99.0",
            "repo.klon-old-backup",
        ] {
            fs::create_dir(tmp.path().join(name)).unwrap();
        }
        let found: Vec<String> = init_leftovers(&golden)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            found,
            vec!["repo.klon-old", "repo.klon-old.99.0", "repo.klon-sub"]
        );
    }
}
