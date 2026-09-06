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
//! Golden is replaced and then deleted, so the copy has to be complete and it
//! has to stay complete:
//!
//! | Risk | Answer |
//! |---|---|
//! | a FIFO, a socket, or a device node that `FICLONE` cannot copy | `OnSpecial::Refuse` stops the copy |
//! | a `git gc --auto` that prunes a loose object the walk already passed | `git fsck --connectivity-only` on the copy |
//! | a commit in golden or in a klon while the copy runs | a tear check over every ref, HEAD, and index |
//! | a stale backend answer after the conversion | the copy leaves `probe.json` out, and step 8 forgets the cache |

use crate::backend::reflink::OnSpecial;
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
    /// Move golden onto a btrfs loop volume of this size, for example 4G.
    /// Use it on a filesystem without snapshots. `--volume --undo` moves
    /// golden back and detaches the volume.
    #[arg(long, value_name = "SIZE", num_args = 0..=1, default_missing_value = "")]
    pub volume: Option<String>,
    /// `--volume --undo` with live klons: remove the volume anyway. The klons
    /// live on it and go with it.
    #[arg(long, requires = "volume")]
    pub force: bool,
}

/// The `init --json` document.
#[derive(Serialize)]
pub(super) struct Report<'a> {
    pub schema: &'static str,
    pub golden: &'a Path,
    /// `subvolume` after `init`, `directory` after `init --undo`.
    pub shape: &'static str,
    /// True when `init` changed nothing because golden already had that shape.
    pub unchanged: bool,
    /// The btrfs loop volume that `init --volume` built (C15). It is absent
    /// for every other form of the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<&'a crate::volume::Volume>,
}

pub fn run(args: Args, yes: bool, json: bool) -> Result<()> {
    // C15 owns `--volume`. It converts no filesystem: it builds one.
    if let Some(size) = args.volume.clone() {
        return super::init_volume::run(&size, &args, yes, json);
    }
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
    if let Err(err) = convert(&golden, &common, &staging, &old, args.undo, &mut record) {
        // The entry stays. Every failure point has a repair rule, and only
        // `doctor --repair` can read the paths on disk and pick the right one.
        eprintln!("klon: run gh klon doctor --repair to finish or revert the conversion");
        return Err(err);
    }
    record.close()?;

    // The swap gave the path a new directory. A shell that stands in golden
    // still holds the old one, which a background process is deleting, so a
    // write from that shell would land in a tree that is going away.
    if cwd.starts_with(&golden) {
        eprintln!(
            "klon: your shell still stands in the replaced directory. \
             Run cd \"{}\" to follow the new one.",
            golden.display()
        );
    }
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
    common: &Path,
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
    // `OnSpecial::Refuse` stops a copy that would drop a FIFO, a socket, or a
    // device node: the original is deleted afterwards, so a skipped path would
    // be gone for good.
    let before = fingerprint(golden, common)?;
    let skip = skip_rule(golden);
    reflink::copy_tree(
        golden,
        staging,
        &skip,
        OnSpecial::Refuse,
        // Both ends sit on one btrfs filesystem here, so a refusal to share
        // blocks is a real error.
        crate::backend::reflink::OnCrossDevice::Refuse,
    )?;
    copy_root_metadata(golden, staging)?;
    verify(staging)?;
    tear_check(golden, common, &before)?;
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
    // Step 8: the journal lives inside the new golden now, because the copy
    // carried it over. The write moves the copied entry on to `ready`.
    record.reach(State::Ready)?;
    // The conversion changed which backend is right, and a cached answer names
    // the old one under a filesystem name that did not change. The copy left
    // `probe.json` out, which covers the usual layout; this line also covers a
    // repository whose common directory sits outside golden
    // (`git init --separate-git-dir`, a submodule).
    crate::backend::forget_probe(common)?;

    // Step 9: the old golden holds the same bytes, shared with the new one, so
    // the delete frees metadata only. It runs detached at the lowest priority.
    delete_old(old, undo)?;
    Ok(())
}

/// Remove the replaced golden.
///
/// The delete runs in the background, and it can take minutes on a big
/// repository. One rename first frees the `<golden>.klon-old` name, so a second
/// `init` on the same repository never waits for it and never refuses. The
/// rename stays inside one directory, so it costs one metadata operation.
/// `doctor --repair` removes every `<golden>.klon-old*` sibling, which closes
/// the window between the rename and the start of the delete.
///
/// After `--undo` the replaced golden is a subvolume, so the btrfs backend can
/// drop it in one ioctl where the mount allows that.
pub(super) fn delete_old(old: &Path, undo: bool) -> Result<()> {
    let victim = free_name(old)?;
    rename(old, &victim)?;
    if undo {
        use crate::backend::Backend;
        return btrfs::BtrfsSnapshot.delete(&victim);
    }
    process::spawn_background_delete(&victim)
}

/// The first free `<old>.<pid>.<n>` path.
fn free_name(old: &Path) -> Result<PathBuf> {
    let pid = std::process::id();
    for n in 0..64u32 {
        let mut name = old.as_os_str().to_os_string();
        name.push(format!(".{pid}.{n}"));
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::klon(format!(
        "cannot find a free name beside {}",
        old.display()
    )))
}

/// Prove that the staged copy is a whole repository before the swap.
///
/// A copy of a live `.git` can miss an object: `git gc --auto` writes a pack
/// and then prunes the loose objects it packed, and the walk can pass a
/// directory before that pack lands. `fsck --connectivity-only` reads every
/// reachable object without hashing its content, so it names a gap in seconds
/// and `init` stops while golden is still untouched.
pub(super) fn verify(staging: &Path) -> Result<()> {
    git::run(
        staging,
        &[
            "fsck",
            "--connectivity-only",
            "--no-progress",
            "--no-dangling",
        ],
    )
    .map_err(|err| {
        Error::klon(format!(
            "the copy at {} is not a whole repository, so golden stays as it is. \
             Let every build and every git command in golden finish, then run \
             gh klon doctor --repair and try again.\n{err}",
            staging.display()
        ))
    })?;
    Ok(())
}

/// The one path the copy leaves out: `<golden>/.git/klon/probe.json`.
///
/// That file names the backend of golden before this command, and the
/// conversion changes the answer while the filesystem name stays `btrfs`, so
/// the cache would still look valid. Everything else under
/// `<golden>/.git/klon` is copied: it holds the journal entries of other
/// commands, the radar cache, and the receipts, and the replaced golden is
/// deleted afterwards.
///
/// The journal entry of this command is copied too. It carries the state that
/// the walk saw, which is `planned`. That is the right answer for a kill
/// between the second rename and the `ready` write: the repair then reads
/// `planned` with golden in place and only removes the leftovers.
pub(super) fn skip_rule(golden: &Path) -> impl Fn(&Path, bool) -> bool {
    let cache = golden.join(".git").join("klon").join("probe.json");
    move |path: &Path, _is_dir: bool| path == cache
}

/// A fingerprint of everything a concurrent git command could change while the
/// copy runs: every ref, HEAD, and every index file under the common directory.
///
/// A commit in golden or in a klon during the copy writes an object and moves a
/// ref. The walk can pass those paths before the write lands, `git fsck` then
/// accepts the older but consistent staged repository, and the swap would drop
/// the new commit together with the replaced tree. The handoff calls the same
/// rule a tear check (§4, "Hot spare").
pub(super) fn fingerprint(golden: &Path, common: &Path) -> Result<String> {
    let mut text = git::run(
        golden,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    text.push_str(&git::run(golden, &["rev-parse", "HEAD"]).unwrap_or_default());
    // A staged change lives in an index file, which no ref names. The main
    // index sits in the common directory and every klon has one of its own.
    let mut indexes = vec![common.join("index")];
    if let Ok(read) = std::fs::read_dir(common.join("worktrees")) {
        let mut found: Vec<PathBuf> = read.flatten().map(|e| e.path().join("index")).collect();
        found.sort();
        indexes.append(&mut found);
    }
    for index in indexes {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(&index) {
            text.push_str(&format!(
                "{} {} {} {}\n",
                index.display(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec()
            ));
        }
    }
    Ok(text)
}

/// Refuse the swap when the repository moved under the copy.
pub(super) fn tear_check(golden: &Path, common: &Path, before: &str) -> Result<()> {
    if fingerprint(golden, common)? == before {
        return Ok(());
    }
    Err(Error::klon(format!(
        "a git command changed {} while init copied it, so the copy is already old and \
         golden stays as it is. Let every build and every git command in golden and in \
         every klon finish, then run gh klon doctor --repair and try again.",
        golden.display()
    )))
}

/// Give the staging copy golden's mode and mtime. `copy_tree` sets them for
/// every child; the root belongs to the caller that created it.
pub(super) fn copy_root_metadata(golden: &Path, staging: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(golden)
        .map_err(Error::io(format!("stat {}", golden.display())))?;
    std::fs::set_permissions(staging, meta.permissions())
        .map_err(Error::io(format!("chmod {}", staging.display())))?;
    crate::backend::set_times(staging, &meta)
}

/// One rename inside the parent directory of golden.
pub(super) fn rename(from: &Path, to: &Path) -> Result<()> {
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

pub(super) fn shape(subvolume: bool) -> &'static str {
    if subvolume {
        "subvolume"
    } else {
        "directory"
    }
}

fn print_json(golden: &Path, shape: &'static str, unchanged: bool) -> Result<()> {
    print_report(&Report {
        schema: SCHEMA,
        golden,
        shape,
        unchanged,
        volume: None,
    })
}

/// Print one `klon.init/1` document.
pub(super) fn print_report(report: &Report) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(report)
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

    /// The copy keeps `.git`, keeps the journal of every other command, and
    /// drops only the probe cache.
    #[test]
    fn the_copy_leaves_out_the_probe_cache_only() {
        let golden = Path::new("/repo");
        let klon_state = golden.join(".git").join("klon");
        let skip = skip_rule(golden);
        assert!(skip(&klon_state.join("probe.json"), false));
        assert!(!skip(&klon_state, true));
        assert!(!skip(&klon_state.join("journal"), true));
        assert!(!skip(&klon_state.join("journal").join("x.json"), false));
        assert!(!skip(&golden.join(".git"), true));
        assert!(!skip(&golden.join(".git").join("config"), false));
        assert!(!skip(&golden.join("build"), true));
    }

    /// The tear check must see a commit that lands while the copy runs. A
    /// commit moves a ref, so the fingerprint changes.
    #[test]
    fn a_commit_changes_the_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let golden = tmp.path().join("golden");
        std::fs::create_dir(&golden).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&golden)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "klon")
                .env("GIT_AUTHOR_EMAIL", "klon@example.com")
                .env("GIT_COMMITTER_NAME", "klon")
                .env("GIT_COMMITTER_EMAIL", "klon@example.com")
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(golden.join("a.txt"), b"one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "one"]);
        let common = golden.join(".git");

        let before = fingerprint(&golden, &common).expect("a fingerprint");
        assert_eq!(
            before,
            fingerprint(&golden, &common).expect("a fingerprint"),
            "a quiet repository must give the same answer twice"
        );
        assert!(tear_check(&golden, &common, &before).is_ok());

        std::fs::write(golden.join("a.txt"), b"two\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "two"]);
        let err =
            tear_check(&golden, &common, &before).expect_err("a commit must fail the tear check");
        assert!(
            err.to_string().contains("changed"),
            "unexpected error {err}"
        );
    }
}
