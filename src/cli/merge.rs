//! `gh klon merge <branch> [--no-ff | --ff-only] [--keep] [--no-check]`: land
//! a klon's branch in golden and remove the klon (handoff §6, R24).
//!
//! The command runs six steps in order. Each one refuses before the next one
//! changes anything, so a merge that stops leaves golden where it stood.
//!
//! 1. Refuse a dirty golden, a dirty klon, and a golden that is not on `base`.
//! 2. `git fetch` in golden.
//! 3. Run the merge gate inside the klon: the `pre_merge` hook, then the C26
//!    receipt gate where the repository configures `[proof] steps`.
//! 4. Configure the mergiraf merge driver when the host has mergiraf.
//! 5. Merge the branch into golden. On a conflict, abort and name the paths.
//! 6. Remove the klon.
//!
//! `merge` never runs `git push`. Landing a branch and publishing it are two
//! decisions, and only the user makes the second one.

use crate::claims;
use crate::cli::rm;
use crate::config::{self, Ff};
use crate::envelope::{env, step_stdout, Envelope, Options, Root};
use crate::journal;
use crate::receipt::{self, Verdict};
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
    /// Land the branch without a `check` receipt (C26). The `pre_merge` hook
    /// still runs.
    #[arg(long)]
    pub no_check: bool,
}

// `merge` takes no `--yes`: since C26 it runs no repository command of its
// own. The `pre_merge` hook is a file of the repository, not a `.klon.toml`
// key, and the `[proof] steps` run under `gh klon check`, which takes the
// approval there.
pub fn run(args: Args, json: bool) -> Result<()> {
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
    let entry = git::klon_of_branch(&worktrees, &args.branch)?;
    let klon = paths::absolute(&entry.path)?;
    // A lock says that somebody wants this klon kept. The check belongs before
    // the merge: `remove_target` refuses a locked klon too, but by then base
    // has already taken the branch, and the command would fail after it
    // changed golden.
    if entry.locked {
        return Err(Error::klon(format!(
            "{} is locked; unlock it with git worktree unlock before a merge",
            klon.display()
        )));
    }

    // --- Step 1: the two trees and the branch golden stands on ---------------
    // `.klon.toml` is read once. A second read repeats every warning line the
    // loader prints, and `merge` reads three keys out of the same file.
    let cfg = config::load(&golden)?;
    let base = branch::base_of(&cfg, &golden)?;
    if args.branch == base {
        return Err(Error::klon(format!(
            "{base} is the base branch; merge lands a klon's branch in it"
        )));
    }
    // A merge that stopped comes first, because it is the sharper answer. A
    // stopped merge usually also reads dirty, and `git merge --continue` is
    // not advice that the word "dirty" gives. A merge that a `commit-msg` hook
    // refused leaves `MERGE_HEAD` with a clean working tree, which the dirty
    // check below cannot see at all.
    if merge_in_progress(&golden) {
        return Err(Error::klon(format!(
            "{} holds a merge that stopped; finish it with git merge --continue \
             or drop it with git merge --abort",
            golden.display()
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
    // The gate proves one commit. A gate that runs a test suite takes minutes,
    // and the agent that owns the klon can commit inside that window, so klon
    // reads the branch tip before and after and refuses a tip that moved.
    let tested = tip(&golden, &full)?;
    let hook = gate(
        &klon,
        &common,
        &args.branch,
        &tested,
        &cfg,
        args.no_check,
        json,
    )?;
    let now = tip(&golden, &full)?;
    if now != tested {
        return Err(Error::klon(format!(
            "{} moved from {} to {} while the gate ran; run merge again",
            args.branch,
            short(&tested),
            short(&now)
        )));
    }

    // --- Step 4: the structured merge driver --------------------------------
    configure_mergiraf(&golden, &common)?;

    // --- Step 5: the merge ---------------------------------------------------
    configure_merge(&golden)?;
    let mode = pick_mode(&args, &cfg);
    let head_before = head(&golden)?;
    // The entry marks the window in which golden's history moves. The removal
    // in step 6 writes its own `rm` entry over this one, so a kill there
    // repairs through the `rm` tail; see `repair::entry`.
    let record = journal::Record::start(&common, journal::Op::Merge, &klon, Some(&args.branch))?;
    // The merge names the commit the gate proved, not the branch name: a tag
    // beats a branch of the same name in git's short-name order, and the exact
    // id also pins what lands. The message then has to be explicit, because
    // git would title the commit `Merge commit '<id>'` from a bare id. A
    // fast-forward writes no commit and ignores the message.
    let message = format!("Merge branch '{}'", args.branch);
    let landed = git::run(
        &golden,
        &["merge", mode.flag(), "--no-edit", "-m", &message, &tested],
    );
    if let Err(err) = landed {
        let conflicts = unmerged(&golden)?;
        // Abort whatever the failed merge left behind, conflicted or not. A
        // `commit-msg` hook that refuses the merge commit leaves `MERGE_HEAD`
        // with no unmerged path, and golden would stay in an active merge.
        if merge_in_progress(&golden) {
            if let Err(why) = git::run(&golden, &["merge", "--abort"]) {
                eprintln!("klon: git merge --abort failed: {why}");
            }
        }
        record.close()?;
        // `--ff-only` on a branch that needs a merge commit fails with no
        // conflicted path. That is git's own refusal, so klon passes it on.
        if conflicts.is_empty() {
            return Err(err);
        }
        return report_conflict(&args, &base, &head_before, mode, hook, conflicts, json);
    }
    let head_after = head(&golden)?;

    // --- Step 6: the removal -------------------------------------------------
    // Base took the branch. From here a failure costs the removal, never the
    // command: the merge is in golden's history and no report may call it a
    // failure. The worktree list is read again, because the gate ran for as
    // long as a test suite takes and the first list is old by now.
    let removed = if args.keep {
        false
    } else {
        match git::worktree_list(&golden)
            .and_then(|fresh| remove(&golden, &common, &fresh, &klon, &args.branch))
        {
            Ok(removed) => removed,
            Err(err) => {
                eprintln!("{err}");
                eprintln!(
                    "klon: {base} took {}; remove the klon with gh klon rm {}",
                    args.branch, args.branch
                );
                false
            }
        }
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
            "{base} takes {}: {} to {} ({})",
            args.branch,
            short(&head_before),
            short(&head_after),
            mode.name()
        );
        if removed {
            println!("removed {}", klon.display());
        }
    }
    Ok(())
}

/// Step 3: the merge gate. It has two halves, and both can apply.
///
/// The `pre_merge` hook runs first where the klon has an executable one. It
/// runs inside the klon under the envelope, so the write fence holds a test
/// that writes where it should not, and a failure stops the merge before
/// golden moves.
///
/// The C26 receipt gate follows where the repository configures `[proof]
/// steps`. `merge` does not run those steps: `gh klon check` runs them and
/// writes a receipt, and `merge` reads it. A long test suite then runs once,
/// when the agent asks for it, instead of inside every merge. `--no-check`
/// skips this half.
///
/// The answer names the gate that ran, for the `hook` field of the report.
fn gate(
    klon: &Path,
    common: &Path,
    branch: &str,
    tested: &str,
    cfg: &config::Config,
    no_check: bool,
    json: bool,
) -> Result<Option<&'static str>> {
    let hook = match pre_merge_hook(klon) {
        Some(hook) => {
            let argv = vec![hook.to_string_lossy().into_owned()];
            exec_step(klon, &argv, &hook.display().to_string(), json)?;
            Some(GATE_HOOK)
        }
        None => None,
    };
    let steps = cfg
        .proof
        .as_ref()
        .and_then(|proof| proof.steps.clone())
        .unwrap_or_default();
    if steps.is_empty() {
        if hook.is_none() {
            eprintln!("klon: no pre_merge hook and no [proof] steps; merge runs no gate");
        }
        return Ok(hook);
    }
    if no_check {
        eprintln!("klon: --no-check: merge lands {branch} without a check receipt");
        return Ok(hook);
    }
    receipt_gate(klon, common, branch, tested, &steps)?;
    Ok(Some(GATE_PROOF))
}

/// The C26 receipt gate: the commit that `merge` will land needs a receipt of
/// its own, for the steps that `.klon.toml` names now, whose verdict is `pass`.
///
/// The gate names `tested`, the branch tip that step 5 merges, and never the
/// klon's live HEAD. The two are the same for an idle klon, because `check`
/// only ever runs in a klon that has the branch checked out. They part where a
/// `pre_merge` hook, or any other process, detaches the klon between the
/// check and the merge: a HEAD read here would then prove an old commit while
/// step 5 lands a newer one. The tip is also re-read after the gate, so a
/// branch that moves during a long gate stops the merge (see `run`).
///
/// Each refusal names what to do next. `receipt missing` means nobody ran
/// `check` here; `receipt stale` means the branch moved on, or the steps
/// changed, since the last one; `receipt failed` means the proof ran and said
/// no.
fn receipt_gate(
    klon: &Path,
    common: &Path,
    branch: &str,
    tested: &str,
    steps: &[String],
) -> Result<()> {
    let again = format!("run gh klon check {branch} again, or pass --no-check");
    match receipt::verdict(common, tested, branch, &receipt::steps_hash(steps))? {
        Verdict::Pass => Ok(()),
        Verdict::Failed => Err(Error::klon(format!(
            "receipt failed: the [proof] steps did not pass at {} in {}; \
             fix the branch and {again}",
            short(tested),
            klon.display()
        ))),
        Verdict::Stale => Err(Error::klon(format!(
            "receipt stale: no passing receipt covers {} with the current [proof] steps; {again}",
            short(tested)
        ))),
        Verdict::Missing => Err(Error::klon(format!(
            "receipt missing: nothing has checked {branch}; \
             run gh klon check {branch}, or pass --no-check"
        ))),
    }
}

/// One gate command inside the klon. The envelope spawns and waits, so `merge`
/// reads the exit status instead of handing its process to the command.
///
/// Under `--json` the command's stdout goes to stderr: klon owns stdout for
/// the one document, and a hook that prints a line would put it in front.
fn exec_step(klon: &Path, argv: &[String], what: &str, json: bool) -> Result<()> {
    let options = Options {
        no_fence: false,
        stdout: step_stdout(json)?,
    };
    match Envelope::spawn_and_wait(Root::Klon(klon), argv, options) {
        // The command already reported its own failure on its own stderr.
        Ok(status) if !status.success() => Err(Error::klon(format!("pre_merge failed: {what}"))),
        Ok(_) => Ok(()),
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
/// git's line merge and takes one stderr line.
///
/// A host that loses mergiraf also loses the setup. git runs a merge driver it
/// cannot find as a failed merge and calls every hunk a conflict, so a rule
/// left behind would turn a clean line merge into a conflict on every file.
/// The removal touches only what klon generated: the two marked lines in
/// `info/attributes`, and each config key whose value is klon's own string.
fn configure_mergiraf(golden: &Path, common: &Path) -> Result<()> {
    if !process::tool_on_path("mergiraf") {
        eprintln!("klon: mergiraf is not on PATH; merge uses git's line merge");
        return remove_mergiraf(golden, common);
    }
    git::ensure_config(golden, "merge.mergiraf.name", MERGIRAF_NAME)?;
    git::ensure_config(golden, "merge.mergiraf.driver", MERGIRAF_DRIVER)?;
    write_attributes(common)
}

/// Drop the generated mergiraf setup of an earlier run.
fn remove_mergiraf(golden: &Path, common: &Path) -> Result<()> {
    let dropped = drop_attributes(common)?;
    for (key, generated) in [
        ("merge.mergiraf.name", MERGIRAF_NAME),
        ("merge.mergiraf.driver", MERGIRAF_DRIVER),
    ] {
        // A value that klon did not write belongs to the user, who may point
        // the driver at a mergiraf that PATH does not name.
        if git::run(golden, &["config", "--get", key])
            .is_ok_and(|value| value.trim_end_matches('\n') == generated)
        {
            git::run_quiet(golden, &["config", "--unset", key]);
        }
    }
    if dropped {
        eprintln!("klon: dropped the generated mergiraf rule from info/attributes");
    }
    Ok(())
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

/// Take the generated block out of `<common>/info/attributes` again. Only the
/// marker line and the rule line right after it go; a rule that a person wrote
/// carries no marker and stays. The answer says whether a block was there.
fn drop_attributes(common: &Path) -> Result<bool> {
    let file = common.join("info").join("attributes");
    let current = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(Error::io("read info/attributes")(err)),
    };
    let trailing_newline = current.ends_with(b"\n");
    let lines: Vec<&[u8]> = current.split(|byte| *byte == b'\n').collect();
    let mut kept: Vec<&[u8]> = Vec::with_capacity(lines.len());
    let mut dropped = false;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].strip_suffix(b"\r").unwrap_or(lines[index]);
        if line == ATTRIBUTES_MARKER.as_bytes() {
            dropped = true;
            index += 1;
            let next = lines.get(index).map(|l| l.strip_suffix(b"\r").unwrap_or(l));
            if next == Some(ATTRIBUTES_RULE.as_bytes()) {
                index += 1;
            }
            continue;
        }
        kept.push(lines[index]);
        index += 1;
    }
    if !dropped {
        return Ok(false);
    }
    // `split` gives a trailing empty piece for a file that ends in a newline.
    // Dropping it and adding the newline back keeps the file byte-exact.
    if trailing_newline && kept.last() == Some(&&b""[..]) {
        kept.pop();
    }
    let mut out = kept.join(&b'\n');
    if trailing_newline && !out.is_empty() {
        out.push(b'\n');
    }
    fs::write(&file, out).map_err(Error::io("write info/attributes"))?;
    Ok(true)
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
fn pick_mode(args: &Args, cfg: &config::Config) -> Ff {
    if args.ff_only {
        return Ff::FfOnly;
    }
    if args.no_ff {
        return Ff::NoFf;
    }
    cfg.merge
        .as_ref()
        .and_then(|merge| merge.ff)
        .unwrap_or(Ff::NoFf)
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
    // The owner names are read before the tree moves: one of them lives in
    // `<klon>/.klon/env`, and the removal takes that file with it.
    let owners = claims::owners(klon, Some(branch));
    rm::remove_target(
        golden,
        common,
        worktrees,
        klon,
        Some(branch),
        rm::Guard::Merged,
        false,
    )?;
    // The branch landed and the klon is gone, so its C27 claims go too. A
    // stale claim would block the next klon that wants those paths forever.
    for name in &owners {
        if let Err(err) = claims::release_all(common, name) {
            eprintln!("{err}");
        }
    }
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

/// The conflicted paths of golden. The caller reads them before it aborts:
/// the abort drops the index that names them.
fn unmerged(golden: &Path) -> Result<Vec<String>> {
    let text = git::run(golden, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    Ok(text.lines().map(str::to_string).collect())
}

/// True when golden holds a merge that has not finished. `MERGE_HEAD` exists
/// from the start of a merge until the merge commit or the abort.
fn merge_in_progress(golden: &Path) -> bool {
    git::run(golden, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_ok()
}

/// The full object id of golden's HEAD.
fn head(golden: &Path) -> Result<String> {
    Ok(git::run(golden, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// The full object id that `reference` names.
fn tip(golden: &Path, reference: &str) -> Result<String> {
    Ok(git::run(golden, &["rev-parse", "--verify", reference])?
        .trim()
        .to_string())
}

/// The first seven characters of an object id, for a report line.
fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

fn print_report(report: &Report) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(report)
            .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
    );
    Ok(())
}
