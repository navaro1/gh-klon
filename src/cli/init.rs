//! `gh klon init [--yes] [--undo]` (spec §7 C7, handoff §7 "`init` safety").
//!
//! `init` converts a plain golden directory on btrfs into a btrfs subvolume, so
//! `add` can snapshot it in one ioctl. `init --undo` converts it back.
//!
//! Golden cannot become a subvolume in place: a subvolume is created, never
//! promoted. `init` therefore stages a copy and swaps it in:
//!
//! | Step | Action | State |
//! |---|---|---|
//! | 1 | print the plan and wait for `y` | — |
//! | 2 | write the journal entry | `planned` |
//! | 3 | `btrfs subvolume create <golden>.klon-sub` | `planned` |
//! | 4 | reflink-copy golden into it, `.git` included | `copied` |
//! | 5 | announce the swap | `swapped` |
//! | 6 | rename golden to `<golden>.klon-old` | `swapped` |
//! | 7 | rename `<golden>.klon-sub` to golden | `swapped` |
//! | 8 | announce the finished swap | `ready` |
//! | 9 | delete `<golden>.klon-old` in the background, drop the entry | — |
//!
//! Step 5 writes `swapped` **before** step 6, so a kill between the two renames
//! leaves an entry that says a swap is in flight. `doctor --repair` then reads
//! the paths on disk and either reverts or finishes (see `repair::init`).
//!
//! The copy leaves out `<golden>/.git/klon`, which holds the journal and the
//! probe cache. Without that rule the new golden would carry a stale copy of
//! its own journal entry, and a cached `reflink-walk` answer would survive a
//! conversion that makes `btrfs-snapshot` the right backend.

use crate::backend::{btrfs, reflink};
use crate::journal::{self, State};
use crate::{git, journal::Op, paths, probe, process, Error, Result};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.init/1";

/// The staging copy that becomes golden.
pub const STAGING_SUFFIX: &str = ".klon-sub";

/// The staging copy of `--undo`, a plain directory.
pub const PLAIN_SUFFIX: &str = ".klon-plain";

/// The old golden, between the two renames and until the delete starts.
pub const OLD_SUFFIX: &str = ".klon-old";

#[derive(clap::Args)]
pub struct Args {
    /// Convert a subvolume golden back into a plain directory.
    #[arg(long)]
    pub undo: bool,
}

/// The `init --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    golden: &'a Path,
    /// `subvolume` after `init`, `directory` after `init --undo`.
    shape: &'static str,
    /// True when `init` changed nothing because golden already had that shape.
    unchanged: bool,
}

pub fn run(args: Args, yes: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    let common = git::common_dir(&cwd)?;

    // R32: a filesystem without subvolumes has nothing to convert. The refusal
    // comes before every check that costs a syscall on a real tree.
    let filesystem = probe::filesystem(&golden);
    if filesystem != "btrfs" {
        return Err(Error::klon(format!(
            "not btrfs: {} is on {filesystem}; gh klon init needs a btrfs filesystem",
            golden.display()
        )));
    }
    let is_subvolume = btrfs::is_subvolume(&golden);
    let want_subvolume = !args.undo;
    if is_subvolume == want_subvolume {
        let shape = shape(want_subvolume);
        if json {
            print_json(&golden, shape, true)?;
        } else {
            println!("{} is already a btrfs {shape}", golden.display());
        }
        return Ok(());
    }

    let staging = sibling(
        &golden,
        if args.undo {
            PLAIN_SUFFIX
        } else {
            STAGING_SUFFIX
        },
    )?;
    let old = sibling(&golden, OLD_SUFFIX)?;
    for path in [&staging, &old] {
        if path.exists() {
            return Err(Error::klon(format!(
                "{} is in the way; remove it, or run gh klon doctor --repair",
                path.display()
            )));
        }
    }
    if !confirmed(&golden, &staging, args.undo, yes)? {
        return Err(Error::klon(
            "init needs a yes; answer y at the prompt or pass --yes",
        ));
    }

    // Handoff §7: the journal entry precedes the first change. `init` moves
    // golden, not a klon, so the entry carries no branch.
    let mut record = journal::Record::start(&common, Op::Init, &golden, None)?;
    if let Err(err) = convert(&golden, &staging, &old, args.undo, &mut record) {
        // The entry stays. Every failure point has a repair rule, and only
        // `doctor --repair` can read the paths on disk and pick the right one.
        eprintln!("klon: run gh klon doctor --repair to finish or revert the conversion");
        return Err(err);
    }
    record.close()?;

    let shape = shape(want_subvolume);
    if json {
        print_json(&golden, shape, false)?;
    } else {
        println!("{} is now a btrfs {shape}", golden.display());
    }
    Ok(())
}

/// Steps 3 to 9. Runs after the journal entry exists.
fn convert(
    golden: &Path,
    staging: &Path,
    old: &Path,
    undo: bool,
    record: &mut journal::Record,
) -> Result<()> {
    // Step 3: the staging copy. `init` makes a subvolume, `--undo` a directory.
    if undo {
        std::fs::create_dir(staging).map_err(Error::io(format!("create {}", staging.display())))?;
    } else {
        btrfs::create_subvolume(staging)?;
    }
    // Step 4: golden's content, `.git` included. A klon leaves `.git` out (R3);
    // a conversion of golden must keep it, else the repository would be gone.
    let skip = skip_rule(golden);
    reflink::copy_tree(golden, staging, &skip)?;
    copy_root_metadata(golden, staging)?;
    record.reach(State::Copied)?;

    // Step 5: the swap is about to start. `doctor --repair` reads this state
    // together with the paths on disk, so a kill in the window below recovers.
    record.reach(State::Swapped)?;
    rename(golden, old)?;
    journal::pause_at("between-mv");
    if let Err(err) = rename(staging, golden) {
        // Golden is at `old` and nothing else moved. Put it back at once, so
        // the repository is usable even when the user never runs `doctor`.
        rename(old, golden)?;
        return Err(err);
    }
    // Step 8: the journal now lives inside the new golden, because the copy
    // left the old entry out. The write recreates it there.
    record.reach(State::Ready)?;

    // Step 9: the old golden holds the same bytes, shared with the new one, so
    // the delete frees metadata only. It runs detached at the lowest priority.
    delete_old(old, undo)?;
    Ok(())
}

/// Remove the replaced golden. After `--undo` it is a subvolume, so the btrfs
/// backend can drop it in one ioctl where the mount allows that.
fn delete_old(old: &Path, undo: bool) -> Result<()> {
    if undo {
        use crate::backend::Backend;
        return btrfs::BtrfsSnapshot.delete(old);
    }
    process::spawn_background_delete(old)
}

/// The paths that the copy leaves out: `<golden>/.git/klon`, which holds the
/// journal and the probe cache of this very command.
fn skip_rule(golden: &Path) -> impl Fn(&Path, bool) -> bool {
    let klon_state = golden.join(".git").join("klon");
    move |path: &Path, _is_dir: bool| path == klon_state
}

/// Give the staging copy golden's mode and mtime. `copy_tree` sets them for
/// every child; the root belongs to the caller that created it.
fn copy_root_metadata(golden: &Path, staging: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(golden)
        .map_err(Error::io(format!("stat {}", golden.display())))?;
    std::fs::set_permissions(staging, meta.permissions())
        .map_err(Error::io(format!("chmod {}", staging.display())))?;
    crate::backend::set_times(staging, &meta)
}

/// One rename inside the parent directory of golden.
fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(Error::io(format!(
        "rename {} to {}",
        from.display(),
        to.display()
    )))
}

/// `<golden><suffix>`, next to golden. A golden with no parent has no place for
/// the staging copy, so the command refuses instead of guessing one.
pub fn sibling(golden: &Path, suffix: &str) -> Result<PathBuf> {
    let name = golden
        .file_name()
        .ok_or_else(|| Error::klon(format!("{} has no name to extend", golden.display())))?;
    let mut extended = name.to_os_string();
    extended.push(suffix);
    let parent = golden
        .parent()
        .ok_or_else(|| Error::klon(format!("{} has no parent directory", golden.display())))?;
    paths::absolute(&parent.join(extended))
}

/// Handoff §7: print the plan with both paths and wait for `y`. `--yes` skips
/// the prompt. A run without a terminal and without `--yes` refuses.
fn confirmed(golden: &Path, staging: &Path, undo: bool, yes: bool) -> Result<bool> {
    let (from, to) = if undo {
        ("btrfs subvolume", "plain directory")
    } else {
        ("plain directory", "btrfs subvolume")
    };
    eprintln!("klon: convert golden from a {from} to a {to}:");
    eprintln!(
        "  1. copy   {} into {}",
        golden.display(),
        staging.display()
    );
    eprintln!(
        "  2. rename {} to {}",
        golden.display(),
        golden.display().to_string() + OLD_SUFFIX
    );
    eprintln!("  3. rename {} to {}", staging.display(), golden.display());
    eprintln!("  4. delete the replaced copy in the background");
    eprintln!("golden keeps its path. The content does not change.");
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("Convert? [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(Error::io("read the answer"))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn shape(subvolume: bool) -> &'static str {
    if subvolume {
        "subvolume"
    } else {
        "directory"
    }
}

fn print_json(golden: &Path, shape: &'static str, unchanged: bool) -> Result<()> {
    let report = Report {
        schema: SCHEMA,
        golden,
        shape,
        unchanged,
    };
    println!(
        "{}",
        serde_json::to_string(&report)
            .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_staging_paths_sit_beside_golden() {
        let golden = Path::new("/tmp/klon-init-test/repo");
        assert!(sibling(golden, STAGING_SUFFIX)
            .unwrap()
            .ends_with("repo.klon-sub"));
        assert!(sibling(golden, OLD_SUFFIX)
            .unwrap()
            .ends_with("repo.klon-old"));
        assert!(sibling(golden, PLAIN_SUFFIX)
            .unwrap()
            .ends_with("repo.klon-plain"));
    }

    #[test]
    fn a_root_golden_has_no_place_for_the_staging_copy() {
        assert!(sibling(Path::new("/"), STAGING_SUFFIX).is_err());
    }

    /// The copy must keep `.git` and drop only the klon state directory.
    #[test]
    fn the_copy_leaves_out_the_klon_state_only() {
        let golden = Path::new("/repo");
        let skip = skip_rule(golden);
        assert!(skip(&golden.join(".git").join("klon"), true));
        assert!(!skip(&golden.join(".git"), true));
        assert!(!skip(&golden.join(".git").join("config"), false));
        assert!(!skip(&golden.join("build"), true));
    }
}
