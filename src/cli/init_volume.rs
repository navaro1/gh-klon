//! `gh klon init --volume <size>` and `gh klon init --volume --undo`
//! (spec §7 C15, R33; handoff §4, the `btrfs-volume` row).
//!
//! ext4 has no snapshot, so `add` on a laptop copies bytes for a minute. A
//! btrfs loop volume gives the same laptop the snapshot backend without a
//! partition, without a reformat, and without `sudo`: udisks binds a sparse
//! image to a loop device and mounts it under `allow_active=yes`. Golden moves
//! onto that volume once and keeps its old path through a symlink.
//!
//! | Step | Action | State |
//! |---|---|---|
//! | 1 | refuse a host that cannot do it, print the plan, wait for `y` | — |
//! | 2 | write the journal entry | `planned` |
//! | 3 | create the sparse image and `mkfs.btrfs -L klon-<repo> --rootdir` | `planned` |
//! | 4 | `udisksctl loop-setup`, `udisksctl mount`, read the mount from `findmnt` | `attached` |
//! | 5 | `btrfs subvolume create <mount>/klon/<repo>`, copy golden into it | `copied` |
//! | 6 | announce the swap | `swapped` |
//! | 7 | rename golden to `<golden>.klon-old`, put a symlink at golden's path | `swapped` |
//! | 8 | `git worktree repair`, write `volume.json` | `ready` |
//! | 9 | delete `<golden>.klon-old` in the background, drop the entry | — |
//!
//! `--undo` walks back: it copies the subvolume into `<golden>.klon-plain` on
//! the old filesystem, swaps the symlink for that directory, empties the
//! volume, and detaches it.
//!
//! The command reuses the safety rules of C7 (`src/cli/init.rs`): the copy
//! refuses a FIFO, `git fsck` proves it, a tear check refuses a repository
//! that moved under it, and the journal states let `doctor --repair` finish or
//! revert every window. `src/volume.rs` holds the host layer and the S1 rules.

use super::init::{
    copy_root_metadata, delete_old, fingerprint, print_report, rename, shape, sibling, skip_rule,
    tear_check, verify, Report, OLD_SUFFIX, PLAIN_SUFFIX, SCHEMA,
};
use crate::backend::reflink::{OnCrossDevice, OnSpecial};
use crate::backend::{self, btrfs, reflink};
use crate::journal::{self, State, VolumeMark};
use crate::volume::{self, Volume};
use crate::{git, paths, probe, process, Error, Result};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// Every host tool that the volume needs beyond `btrfs-progs`.
const TOOLS: &[(&str, &str)] = &[("udisksctl", "udisks2"), ("findmnt", "util-linux")];

pub fn run(size: &str, args: &super::init::Args, yes: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    // A volume that a reboot took down leaves golden's symlink dangling, so
    // the repository is unreachable until klon mounts the image again.
    let cwd = volume::ensure_attached(&cwd)?;
    if args.undo {
        return undo(&cwd, args.force, yes, json);
    }
    if size.is_empty() {
        return Err(Error::klon(
            "gh klon init --volume needs a size, for example: gh klon init --volume 4G",
        ));
    }
    make(&cwd, size, yes, json)
}

// --- Forward: golden moves onto a new volume -------------------------------------

fn make(cwd: &Path, size: &str, yes: bool, json: bool) -> Result<()> {
    let golden = git::main_worktree(cwd)?;
    let common = git::common_dir(cwd)?;
    let bytes = volume::parse_size(size)?;
    refuse_a_host_that_cannot(&golden, &common)?;

    // R6: every change waits until the whole plan is legal. A refusal below
    // leaves the repository exactly as it was.
    if process::dirty(&golden)? {
        return Err(Error::klon(format!(
            "dirty: {} has uncommitted changes, and init --volume replaces the \
             directory. Commit or stash them, then run the command again.",
            golden.display()
        )));
    }
    let plan = Volume::plan(&golden, Path::new("<mount>"))?;
    if plan.image.exists() {
        return Err(Error::klon(format!(
            "{} already exists; delete it, or run gh klon init --volume --undo",
            plan.image.display()
        )));
    }
    let old = sibling(&golden, OLD_SUFFIX)?;
    if old.exists() {
        return Err(Error::klon(format!(
            "{} is in the way; remove it, or run gh klon doctor --repair",
            old.display()
        )));
    }
    if !confirmed(&print_plan(&golden, &plan, size), yes)? {
        return Err(Error::klon(
            "init --volume needs a yes; answer y at the prompt or pass --yes",
        ));
    }

    // Handoff §7: the journal entry precedes the first change.
    let mark = VolumeMark {
        record: plan.clone(),
        undo: false,
    };
    let mut record = journal::Record::start_volume(&common, &golden, mark)?;
    let live = match build(&golden, &common, &plan, bytes, &old, &mut record) {
        Ok(live) => live,
        Err(err) => {
            eprintln!("klon: run gh klon doctor --repair to finish or revert the conversion");
            return Err(err);
        }
    };
    record.close()?;

    if cwd.starts_with(&golden) {
        eprintln!(
            "klon: your shell still stands in the replaced directory. \
             Run cd \"{}\" to follow the new one.",
            golden.display()
        );
    }
    if json {
        print_report(&Report {
            schema: SCHEMA,
            golden: &golden,
            shape: shape(true),
            unchanged: false,
            volume: Some(&live),
        })
    } else {
        println!("{} now lives on {}", golden.display(), live.mount.display());
        Ok(())
    }
}

/// Steps 3 to 9. Runs after the journal entry exists. The answer is the record
/// with the mount point that udisks really chose.
fn build(
    golden: &Path,
    common: &Path,
    plan: &Volume,
    bytes: u64,
    old: &Path,
    record: &mut journal::Record,
) -> Result<Volume> {
    // Step 3: a sparse image with a user-owned `klon/` directory in it. The
    // mount root belongs to `root` under udisks, so without the seed the
    // volume would have no path this user can write (S1 §6).
    volume::create_image(&plan.image, bytes)?;
    let seed = plan.image.with_extension("seed");
    volume::mkfs(&plan.image, &plan.label, &seed)?;

    // Step 4: attach and mount. The mount point comes from `findmnt`, never
    // from the label, because a second volume with that label gets a suffix.
    let live = volume::attach(plan)?;
    record.set_volume(live.clone());
    record.reach(State::Attached)?;
    let work = volume::work_dir(&live.mount);
    if !work.is_dir() {
        return Err(Error::klon(format!(
            "{} is missing from the volume; mkfs.btrfs did not seed it",
            work.display()
        )));
    }

    // Step 5: one subvolume per repository, so `add` can snapshot it.
    btrfs::create_subvolume(&live.golden_new)?;
    let before = fingerprint(golden, common)?;
    let skip = skip_rule(golden);
    // Golden sits on another filesystem, where `FICLONE` answers `EXDEV`, so
    // the walk copies the bytes and keeps every mode and every mtime.
    reflink::copy_tree(
        golden,
        &live.golden_new,
        &skip,
        OnSpecial::Refuse,
        OnCrossDevice::CopyBytes,
    )?;
    copy_root_metadata(golden, &live.golden_new)?;
    verify(&live.golden_new)?;
    tear_check(golden, common, &before)?;
    record.reach(State::Copied)?;

    // Step 6 and 7: the swap. The state is written first, so a kill between
    // the two steps leaves an entry that says a swap is in flight.
    let linked = linked_worktrees(golden)?;
    record.reach(State::Swapped)?;
    rename(golden, old)?;
    journal::pause_at("between-mv");
    if let Err(err) = link(&live.golden_new, golden) {
        // Golden is at `old` and nothing else moved. Put it back at once.
        rename(old, golden)?;
        return Err(err);
    }
    // The process still stands in the directory that a background delete is
    // about to remove. Every later step reads the new tree.
    let _ = std::env::set_current_dir(&live.golden_new);

    // Step 8: git keeps its own absolute paths. The symlink already answers
    // for every one of them, and `repair` writes them out again, so a later
    // `--undo` finds a consistent register list.
    let new_common = git::common_dir_of_main(golden)?;
    record.relocate(&new_common);
    repair_worktrees(&live.golden_new, &linked);
    volume::write(&new_common, &live)?;
    record.reach(State::Ready)?;
    // The conversion changed which backend is right while the cached answer
    // still names a filesystem that did not change under golden's old path.
    backend::forget_probe(&new_common)?;

    // Step 9: the replaced golden holds a second copy of every byte, so the
    // delete runs detached at the lowest priority.
    delete_old(old, false)?;
    Ok(live)
}

/// Golden's path becomes a symlink to its new home, so every absolute path in
/// git, in a build cache, and in the user's shell history still resolves.
fn link(target: &Path, at: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, at).map_err(Error::io(format!(
        "link {} to {}",
        at.display(),
        target.display()
    )))
}

// --- Backward: golden moves off the volume ---------------------------------------

fn undo(cwd: &Path, force: bool, yes: bool, json: bool) -> Result<()> {
    let golden = git::main_worktree(cwd)?;
    let common = git::common_dir(cwd)?;
    let record = match volume::read(&common)? {
        Some(record) => record,
        None => {
            volume::find_for(cwd)?
                .ok_or_else(|| {
                    Error::klon(format!(
                        "{} sits on no klon volume; gh klon init --volume never ran here",
                        golden.display()
                    ))
                })?
                .record
        }
    };
    if paths::absolute(&record.golden_new)? != golden {
        return Err(Error::klon(format!(
            "the volume record names {} as golden, and git names {}; \
             run gh klon doctor --repair",
            record.golden_new.display(),
            golden.display()
        )));
    }
    let live = linked_worktrees(&golden)?;
    if !live.is_empty() && !force {
        return Err(Error::klon(format!(
            "{} klons live on the volume and go away with it: {}. \
             Remove them with gh klon rm, or pass --force.",
            live.len(),
            live.join(", ")
        )));
    }
    let plain = sibling(&record.golden_old, PLAIN_SUFFIX)?;
    let old = sibling(&record.golden_old, OLD_SUFFIX)?;
    for path in [&plain, &old] {
        if path.exists() {
            return Err(Error::klon(format!(
                "{} is in the way; remove it, or run gh klon doctor --repair",
                path.display()
            )));
        }
    }
    if !confirmed(&undo_plan(&record), yes)? {
        return Err(Error::klon(
            "init --volume --undo needs a yes; answer y at the prompt or pass --yes",
        ));
    }

    let mark = VolumeMark {
        record: record.clone(),
        undo: true,
    };
    let mut entry = journal::Record::start_volume(&common, &record.golden_old, mark)?;
    if let Err(err) = restore(&golden, &common, &record, &plain, &old, &live, &mut entry) {
        eprintln!("klon: run gh klon doctor --repair to finish or revert the conversion");
        return Err(err);
    }
    entry.close()?;

    if cwd.starts_with(&golden) {
        eprintln!(
            "klon: your shell still stands on the volume. Run cd \"{}\" to follow golden.",
            record.golden_old.display()
        );
    }
    if json {
        print_report(&Report {
            schema: SCHEMA,
            golden: &record.golden_old,
            shape: shape(false),
            unchanged: false,
            volume: None,
        })
    } else {
        println!("{} is a plain directory again", record.golden_old.display());
        Ok(())
    }
}

/// The `--undo` tail. Runs after the journal entry exists.
fn restore(
    golden: &Path,
    common: &Path,
    record: &Volume,
    plain: &Path,
    old: &Path,
    linked: &[String],
    entry: &mut journal::Record,
) -> Result<()> {
    fs::create_dir(plain).map_err(Error::io(format!("create {}", plain.display())))?;
    let before = fingerprint(golden, common)?;
    let skip = skip_rule(golden);
    reflink::copy_tree(
        golden,
        plain,
        &skip,
        OnSpecial::Refuse,
        OnCrossDevice::CopyBytes,
    )?;
    copy_root_metadata(golden, plain)?;
    verify(plain)?;
    tear_check(golden, common, &before)?;
    entry.reach(State::Copied)?;

    // The symlink moves out of the way first, so the window between the two
    // steps looks exactly like the one the forward direction leaves: golden is
    // missing and `<golden>.klon-old` holds what stood there.
    entry.reach(State::Swapped)?;
    rename(&record.golden_old, old)?;
    journal::pause_at("between-mv");
    if let Err(err) = rename(plain, &record.golden_old) {
        rename(old, &record.golden_old)?;
        return Err(err);
    }
    let _ = std::env::set_current_dir(&record.golden_old);

    let new_common = git::common_dir_of_main(&record.golden_old)?;
    entry.relocate(&new_common);
    repair_worktrees(&record.golden_old, linked);
    // The record goes before the volume does, so a kill below never leaves a
    // command chasing an image that is no longer golden's home.
    volume::forget(&new_common, &record.golden_old)?;
    entry.reach(State::Ready)?;
    backend::forget_probe(&new_common)?;

    // `rm -rf` on a symlink removes the link and never the target, so the
    // volume content survives this line and the next one clears it.
    fs::remove_file(old).map_err(Error::io(format!("delete {}", old.display())))?;
    empty_the_volume(record);
    detach(record);
    Ok(())
}

/// Remove golden's copy from the volume.
///
/// The delete runs in the foreground, unlike every other klon delete: the
/// unmount below must wait for it, and a detached process would race it.
/// `remove_dir_all` removes a subvolume too, because the kernel lets an
/// unprivileged user `rmdir` an empty one (S1 §8), and `btrfs subvolume
/// delete` needs root that klon never takes.
fn empty_the_volume(record: &Volume) {
    if !record.golden_new.exists() {
        return;
    }
    let removed = backend::make_removable(&record.golden_new).and_then(|()| {
        fs::remove_dir_all(&record.golden_new)
            .map_err(Error::io(format!("delete {}", record.golden_new.display())))
    });
    if let Err(err) = removed {
        eprintln!("klon: {err}");
        eprintln!("klon: the volume keeps golden's old copy; the image holds it");
    }
}

/// Unmount the volume, and release the loop device and the image when udisks
/// says this user set the device up.
///
/// `loop-delete` on a device that another uid set up raises a polkit password
/// dialog (S1 §9), so klon reads `SetupByUID` first and leaves the image in
/// place when the answer is not this user.
fn detach(record: &Volume) {
    let device = match volume::loop_device(&record.image) {
        Ok(Some(device)) => device,
        Ok(None) => return,
        Err(err) => {
            eprintln!("klon: {err}");
            return;
        }
    };
    if let Err(err) = volume::unmount(&device) {
        eprintln!("klon: {err}");
        eprintln!(
            "klon: the volume stays mounted; run udisksctl unmount -b {device} when it is idle"
        );
        return;
    }
    let released = match volume::loop_delete(&device) {
        Ok(released) => released,
        Err(err) => {
            eprintln!("klon: {err}");
            false
        }
    };
    if !released {
        eprintln!(
            "klon: {} stays on disk. Delete it with: rm -f {}",
            record.image.display(),
            record.image.display()
        );
        return;
    }
    match fs::remove_file(&record.image) {
        Ok(()) => println!("deleted the volume image {}", record.image.display()),
        Err(err) => eprintln!("klon: cannot delete {}: {err}", record.image.display()),
    }
}

// --- Shared -----------------------------------------------------------------------

/// Every reason this host cannot build a volume, in the order that costs the
/// least. Each one leaves the repository untouched.
fn refuse_a_host_that_cannot(golden: &Path, common: &Path) -> Result<()> {
    if btrfs::mkfs_tool().is_none() || btrfs::tool().is_none() {
        return Err(Error::klon(btrfs::install_lines()));
    }
    for (tool, package) in TOOLS {
        if probe::tool_path(tool).is_none() {
            return Err(Error::klon(format!(
                "{tool} is not on PATH; install {package}, \
                 then run gh klon init --volume again"
            )));
        }
    }
    // The udisks policy grants the mount to an active local session and asks
    // every other one for a password (S1 §11).
    volume::refuse_a_remote_session()?;
    if let Some(found) = volume::read(common)? {
        return Err(Error::klon(format!(
            "{} already sits on the klon volume {}; \
             run gh klon init --volume --undo to take it off",
            found.golden_old.display(),
            found.image.display()
        )));
    }
    if btrfs::is_subvolume(golden) {
        return Err(Error::klon(format!(
            "{} is already a btrfs subvolume, so gh klon add snapshots it already",
            golden.display()
        )));
    }
    if fs::symlink_metadata(golden).is_ok_and(|meta| meta.is_symlink()) {
        return Err(Error::klon(format!(
            "{} is a symlink, so klon cannot replace it with one; \
             run gh klon doctor --repair",
            golden.display()
        )));
    }
    Ok(())
}

/// The path of every linked worktree, golden left out. `git worktree repair`
/// takes the list, and `--undo` names it in its refusal.
fn linked_worktrees(golden: &Path) -> Result<Vec<String>> {
    Ok(git::worktree_list(golden)?
        .into_iter()
        .skip(1)
        .map(|w| w.path.to_string_lossy().into_owned())
        .collect())
}

/// `git worktree repair <path>...` from golden's new home.
///
/// Every `.git` file and every admin `gitdir` file holds an absolute path.
/// Golden's symlink already answers for all of them, and `repair` writes them
/// out again so the register list stays right when the symlink goes.
fn repair_worktrees(golden: &Path, linked: &[String]) {
    let mut args = vec!["worktree", "repair"];
    args.extend(linked.iter().map(String::as_str));
    match git::run(golden, &args) {
        Ok(text) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                eprintln!("klon: git worktree repair: {line}");
            }
        }
        Err(err) => eprintln!("klon: git worktree repair did not finish: {err}"),
    }
}

/// Handoff §7: print the plan and wait for `y`. `--yes` skips the prompt, and a
/// run without a terminal and without `--yes` refuses.
fn confirmed(plan: &str, yes: bool) -> Result<bool> {
    eprint!("{plan}");
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("Move golden onto the volume? [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(Error::io("read the answer"))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_plan(golden: &Path, plan: &Volume, size: &str) -> String {
    let mount = expected_mount(&plan.label);
    let name = plan
        .golden_new
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    format!(
        "klon: move golden onto a btrfs loop volume:\n\
         \x20 image   {} ({size}, sparse)\n\
         \x20 label   {}, so udisks mounts it at {}\n\
         \x20 golden  {}\n\
         \x20 moves to {} and its old path becomes a symlink to that\n\
         \x20 klon reads the real mount point from findmnt, and udisks adds a \
         suffix when that path is taken.\n\
         golden keeps its path. The content does not change. No password is needed \
         in this session.\n",
        plan.image.display(),
        plan.label,
        mount.display(),
        golden.display(),
        volume::work_dir(&mount).join(name).display(),
    )
}

fn undo_plan(record: &Volume) -> String {
    format!(
        "klon: move golden off the btrfs loop volume:\n\
         \x20 copy    {} back to {}\n\
         \x20 replace the symlink at {} with that directory\n\
         \x20 empty and unmount the volume {}\n\
         golden keeps its path. The content does not change.\n",
        record.golden_new.display(),
        record.golden_old.display(),
        record.golden_old.display(),
        record.image.display(),
    )
}

/// The mount point that udisks picks for a fresh label: `/media/<user>/<label>`
/// on a Debian or Ubuntu desktop. It is printed in the plan only. Every path
/// klon uses comes from `findmnt` (S1 §9).
fn expected_mount(label: &str) -> PathBuf {
    let user = std::env::var("USER")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "<you>".to_string());
    Path::new("/media").join(user).join(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan must name every path the user is about to lose sight of: the
    /// image, the label, golden, and the place golden goes.
    #[test]
    fn the_plan_names_every_path() {
        let golden = Path::new("/home/u/work/repo");
        let plan = Volume {
            version: volume::VERSION,
            image: PathBuf::from("/home/u/.local/share/klon/repo-0011aabb.img"),
            label: "klon-repo".to_string(),
            mount: PathBuf::from("<mount>"),
            golden_old: golden.to_path_buf(),
            golden_new: PathBuf::from("<mount>/klon/repo"),
            created: "2026-09-06T08:00:00Z".to_string(),
        };
        let text = print_plan(golden, &plan, "4G");
        for part in [
            "repo-0011aabb.img",
            "4G",
            "klon-repo",
            "/home/u/work/repo",
            "findmnt",
        ] {
            assert!(text.contains(part), "the plan must name {part}: {text}");
        }
        let back = undo_plan(&plan);
        assert!(back.contains("/home/u/work/repo") && back.contains("repo-0011aabb.img"));
    }

    /// The plan prints the udisks convention, and never a path klon invented
    /// for its own use.
    #[test]
    fn the_expected_mount_follows_the_label() {
        std::env::set_var("USER", "navaro");
        assert_eq!(
            expected_mount("klon-repo"),
            Path::new("/media/navaro/klon-repo")
        );
    }
}
