//! `gh klon merge <branch> [--no-ff | --ff-only] [--keep]`: land a klon's
//! branch in golden and remove the klon (handoff §6, R24).
//!
//! The command runs six steps in order. Each one refuses before the next one
//! changes anything, so a merge that stops leaves golden where it stood.
//!
//! 1. Refuse a dirty golden, a dirty klon, and a golden that is not on `base`.
//! 2. `git fetch` in golden.
//! 3. Run the merge gate inside the klon: the `pre_merge` hook, else the
//!    approved `[proof] steps`.
//! 4. Configure the mergiraf merge driver when the host has mergiraf.
//! 5. Merge the branch into golden. On a conflict, abort and name the paths.
//! 6. Remove the klon.
//!
//! `merge` never runs `git push`. Landing a branch and publishing it are two
//! decisions, and only the user makes the second one.

use crate::cli::{rm, run};
use crate::config::{self, Ff};
use crate::envelope::env;
use crate::journal;
use crate::{branch, git, paths, process, Error, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.merge/1";

/// The gate that ran, as the `hook` field of the report reports it.
const GATE_HOOK: &str = "pre_merge";
const GATE_PROOF: &str = "proof.steps";

/// The mergiraf merge driver. git substitutes `%O`, `%A`, `%B`, and `%P` with
/// the ancestor, the golden side, the branch side, and the path.
///
/// `%S`, `%X`, and `%Y` are the three conflict labels, and git learned them in
/// 2.44. An older git leaves each of them in the command unchanged, so mergiraf
/// labels a conflict `%S` instead of the branch name. The labels are cosmetic
/// and only appear inside a conflict that klon reports and aborts anyway, so
/// one driver string serves every git.
const MERGIRAF_DRIVER: &str = "mergiraf merge --git %O %A %B -s %S -x %X -y %Y -p %P";
const MERGIRAF_NAME: &str = "mergiraf structured merge";

/// The two lines klon writes to `<common>/info/attributes`. The comment marks
/// them as generated, so a person knows what to delete to turn the driver off.
const ATTRIBUTES_MARKER: &str =
    "# gh-klon: merge every path with the mergiraf driver. Delete both lines to stop it.";
const ATTRIBUTES_RULE: &str = "* merge=mergiraf";

/// `merge.conflictStyle=zdiff3` arrived in git 2.35. An older git rejects the
/// value and every merge in the repository then fails, so klon writes `diff3`
/// there instead.
const ZDIFF3_SINCE: (u32, u32) = (2, 35);

/// The `merge --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    /// The branch that golden took.
    branch: &'a str,
    /// The branch golden has checked out.
    base: &'a str,
    /// Golden's HEAD before the merge.
    head_before: &'a str,
    /// Golden's HEAD after the merge. It equals `head_before` on a conflict.
    head_after: &'a str,
    /// `no-ff` or `ff-only`.
    mode: &'static str,
    /// True when `merge` removed the klon.
    removed: bool,
    /// `pre_merge`, `proof.steps`, or null when the repository has no gate.
    hook: Option<&'static str>,
    /// The conflicting paths. It is empty for every merge that landed.
    conflicts: Vec<String>,
}

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the klon to land in base.
    pub branch: String,
    /// Write a merge commit even where a fast-forward would do.
    #[arg(long, conflicts_with = "ff_only")]
    pub no_ff: bool,
    /// Refuse the merge unless base can fast-forward to the branch.
    #[arg(long)]
    pub ff_only: bool,
    /// Keep the klon after the merge instead of removing it.
    #[arg(long)]
    pub keep: bool,
}

pub fn run(args: Args, yes: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = paths::absolute(
        &worktrees
            .first()
            .ok_or_else(|| Error::klon("not inside a git repository"))?
            .path,
    )?;
    let common = git::common_dir_of_main(&golden)?;
    let full = format!("refs/heads/{}", args.branch);
    // The main worktree is not a klon, so `skip(1)` keeps `merge <base>` from
    // naming golden itself.
    let klon = worktrees
        .iter()
        .skip(1)
        .find(|w| w.branch.as_deref() == Some(full.as_str()))
        .map(|w| paths::absolute(&w.path))
        .transpose()?
        .ok_or_else(|| {
            Error::klon(format!(
                "no klon has the branch {} checked out",
                args.branch
            ))
        })?;

    // --- Step 1: the two trees and the branch golden stands on ---------------
    let base = branch::base(&golden)?;
    if args.branch == base {
        return Err(Error::klon(format!(
            "{base} is the base branch; merge lands a klon's branch in it"
        )));
    }
    if process::dirty(&golden)? {
        return Err(Error::klon(format!(
            "{} is dirty; commit or stash golden before a merge",
            golden.display()
        )));
    }
    if process::dirty(&klon)? {
        return Err(Error::klon(format!(
            "dirty klon: {} holds uncommitted work; commit it before a merge",
            klon.display()
        )));
    }
    // A detached golden gives an empty answer, which no branch name matches.
    let head_branch = git::run(&golden, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    if head_branch != base {
        let standing = if head_branch.is_empty() {
            "a detached HEAD".to_string()
        } else {
            head_branch
        };
        return Err(Error::klon(format!(
            "golden stands on {standing} and base is {base}; check out {base} in golden first"
        )));
    }

    // --- Step 2: the fetch ---------------------------------------------------
    if git::run(&golden, &["remote", "get-url", "origin"]).is_ok() {
        git::run(&golden, &["fetch", "-q", "origin"])?;
    } else {
        eprintln!("klon: the repository has no origin remote; merge skips the fetch");
    }

    // --- Step 3: the merge gate ---------------------------------------------
    let hook = gate(&klon, &golden, yes)?;

    // --- Step 4: the structured merge driver --------------------------------
    configure_mergiraf(&golden, &common)?;

    // --- Step 5: the merge ---------------------------------------------------
    configure_merge(&golden)?;
    let mode = pick_mode(&args, &golden)?;
    let head_before = head(&golden)?;
    // The entry marks the window in which golden's history moves. The removal
    // in step 6 writes its own `rm` entry over this one, so a kill there
    // repairs through the `rm` tail; see `repair::entry`.
    let record = journal::Record::start(&common, journal::Op::Merge, &klon, Some(&args.branch))?;
    let landed = git::run(&golden, &["merge", mode.flag(), "--no-edit", &full]);
    if let Err(err) = landed {
        // `--ff-only` on a branch that needs a merge commit fails with no
        // conflicted path. That is git's own refusal, so klon passes it on.
        let conflicts = unmerged(&golden)?;
        record.close()?;
        if conflicts.is_empty() {
            return Err(err);
        }
        return report_conflict(&args, &base, &head_before, mode, hook, conflicts, json);
    }
    let head_after = head(&golden)?;

    // --- Step 6: the removal -------------------------------------------------
    let removed = if args.keep {
        false
    } else {
        remove(&golden, &common, &worktrees, &klon, &args.branch)?
    };
    record.close()?;

    if json {
        print_report(&Report {
            schema: SCHEMA,
            branch: &args.branch,
            base: &base,
            head_before: &head_before,
            head_after: &head_after,
            mode: mode.name(),
            removed,
            hook,
            conflicts: Vec::new(),
        })?;
    } else {
        println!(
            "{base} {} {} ({}) after {}",
            short(&head_before),
            short(&head_after),
            mode.name(),
            args.branch
        );
    }
    Ok(())
}

/// Step 3: the merge gate. The `pre_merge` hook wins where the klon has an
/// executable one; else the approved `[proof] steps` run, in file order.
///
/// Every command runs inside the klon under the envelope, so the write fence
/// holds a test that writes where it should not. The first failure stops the
/// merge and golden never moves.
fn gate(klon: &Path, golden: &Path, yes: bool) -> Result<Option<&'static str>> {
    if let Some(hook) = pre_merge_hook(klon) {
        let argv = vec![hook.to_string_lossy().into_owned()];
        exec_step(klon, &argv, &hook.display().to_string())?;
        return Ok(Some(GATE_HOOK));
    }
    let cfg = config::load(golden)?;
    let steps = cfg
        .proof
        .as_ref()
        .and_then(|proof| proof.steps.clone())
        .unwrap_or_default();
    if steps.is_empty() {
        eprintln!("klon: no pre_merge hook and no [proof] steps; merge runs no gate");
        return Ok(None);
    }
    cfg.ensure_approved(yes, &["proof.steps"])?;
    for step in &steps {
        let argv = vec!["sh".to_string(), "-c".to_string(), step.clone()];
        exec_step(klon, &argv, step)?;
    }
    Ok(Some(GATE_PROOF))
}

/// One gate command inside the klon. `run` spawns and waits, so `merge`
/// continues with the answer instead of handing its process to the command.
fn exec_step(klon: &Path, argv: &[String], what: &str) -> Result<()> {
    match run::exec_with(klon, argv, run::Options::default()) {
        Ok(()) => Ok(()),
        // The command already reported its own failure on its own stderr.
        Err(Error::Exit(_)) => Err(Error::klon(format!("pre_merge failed: {what}"))),
        Err(err) => Err(Error::klon(format!("pre_merge failed: {what}: {err}"))),
    }
}

/// `<klon>/.klon/hooks/pre_merge` where the klon holds an executable one. C22
/// fills that directory at `add` time from the repository hooks; a klon
/// without the hook falls through to the `[proof] steps`.
fn pre_merge_hook(klon: &Path) -> Option<PathBuf> {
    let path = env::hooks_dir(klon).join(GATE_HOOK);
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.permissions().mode() & 0o111 == 0 {
        eprintln!(
            "klon: {} is not executable; merge skips the hook",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// Step 4: the mergiraf merge driver. mergiraf merges by syntax, so two edits
/// in one file that touch two declarations join without a conflict.
///
/// It is an optional host feature (spec §5). A host without mergiraf keeps
/// git's line merge and takes one stderr line. The answer says whether the
/// driver is configured, for the caller that wants to report it.
fn configure_mergiraf(golden: &Path, common: &Path) -> Result<bool> {
    if !tool_on_path("mergiraf") {
        eprintln!("klon: mergiraf is not on PATH; merge uses git's line merge");
        return Ok(false);
    }
    git::ensure_config(golden, "merge.mergiraf.name", MERGIRAF_NAME)?;
    git::ensure_config(golden, "merge.mergiraf.driver", MERGIRAF_DRIVER)?;
    write_attributes(common)?;
    Ok(true)
}

/// Add the two generated lines to `<common>/info/attributes` once. Every line
/// that is already there stays, and a second call adds nothing.
fn write_attributes(common: &Path) -> Result<()> {
    let file = common.join("info").join("attributes");
    let mut current = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(Error::io("read info/attributes")(err)),
    };
    let present = current.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line == ATTRIBUTES_RULE.as_bytes()
    });
    if present {
        return Ok(());
    }
    fs::create_dir_all(common.join("info")).map_err(Error::io("create info/"))?;
    if !current.is_empty() && !current.ends_with(b"\n") {
        current.push(b'\n');
    }
    for line in [ATTRIBUTES_MARKER, ATTRIBUTES_RULE] {
        current.extend_from_slice(line.as_bytes());
        current.push(b'\n');
    }
    fs::write(&file, current).map_err(Error::io("write info/attributes"))
}

/// The two merge keys of handoff §6, in the shared config. `zdiff3` shows the
/// merge base inside a conflict and drops the lines both sides share, and
/// `rerere` replays a resolution the user already made.
fn configure_merge(golden: &Path) -> Result<()> {
    let style = if git::version(golden) >= ZDIFF3_SINCE {
        "zdiff3"
    } else {
        eprintln!("klon: git is older than 2.35; merge sets conflictStyle=diff3 instead of zdiff3");
        "diff3"
    };
    git::ensure_config(golden, "merge.conflictStyle", style)?;
    git::ensure_config(golden, "rerere.enabled", "true")
}

/// The merge mode: the flags first, then `[merge] ff`, then `no-ff`.
fn pick_mode(args: &Args, golden: &Path) -> Result<Ff> {
    if args.ff_only {
        return Ok(Ff::FfOnly);
    }
    if args.no_ff {
        return Ok(Ff::NoFf);
    }
    Ok(config::load(golden)?
        .merge
        .and_then(|merge| merge.ff)
        .unwrap_or(Ff::NoFf))
}

/// Step 6: hand the klon to the `rm` logic. The branch is in base now, so a
/// file the gate left behind must not keep the klon alive; a live process
/// must, and the klon then stays with one line. The branch itself stays:
/// `merge` never deletes a branch, and `rm --merged` is the command for that.
fn remove(
    golden: &Path,
    common: &Path,
    worktrees: &[git::Worktree],
    klon: &Path,
    branch: &str,
) -> Result<bool> {
    if let Some(pid) = process::live_process(klon) {
        eprintln!(
            "klon: {} has a live process (pid {pid}); the klon stays. \
             Remove it with gh klon rm {branch} once the process ends",
            klon.display()
        );
        return Ok(false);
    }
    rm::remove_target(
        golden,
        common,
        worktrees,
        klon,
        Some(branch),
        rm::Guard::Merged,
        false,
    )?;
    Ok(true)
}

/// Step 5 on a conflict: name the paths, put golden back, and fail.
fn report_conflict(
    args: &Args,
    base: &str,
    head_before: &str,
    mode: Ff,
    hook: Option<&'static str>,
    conflicts: Vec<String>,
    json: bool,
) -> Result<()> {
    if json {
        print_report(&Report {
            schema: SCHEMA,
            branch: &args.branch,
            base,
            head_before,
            head_after: head_before,
            mode: mode.name(),
            removed: false,
            hook,
            conflicts,
        })?;
        // The document carries the whole answer, so the error prints nothing.
        return Err(Error::Exit(1));
    }
    eprintln!(
        "klon: {} conflicts with {base}. These paths conflict:",
        args.branch
    );
    for path in &conflicts {
        eprintln!("  {path}");
    }
    Err(Error::klon(format!(
        "golden stays at {}; resolve the conflict in the klon, then run merge again",
        short(head_before)
    )))
}

/// The conflicted paths of golden, then `git merge --abort`. The read comes
/// first: the abort drops the index that names them.
fn unmerged(golden: &Path) -> Result<Vec<String>> {
    let text = git::run(golden, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    let paths: Vec<String> = text.lines().map(str::to_string).collect();
    if !paths.is_empty() {
        if let Err(err) = git::run(golden, &["merge", "--abort"]) {
            eprintln!("klon: git merge --abort failed: {err}");
        }
    }
    Ok(paths)
}

/// The full object id of golden's HEAD.
fn head(golden: &Path) -> Result<String> {
    Ok(git::run(golden, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// The first seven characters of an object id, for a report line.
fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// True when an executable `name` sits in a PATH directory.
fn tool_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            fs::metadata(dir.join(name))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

fn print_report(report: &Report) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(report)
            .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
    );
    Ok(())
}
