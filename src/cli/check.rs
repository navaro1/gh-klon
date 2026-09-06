//! `gh klon check <branch>`: run the approved `[proof] steps` in a klon at a
//! clean HEAD and write a receipt (handoff §6, R25).
//!
//! The command runs five steps in order:
//!
//! 1. Find the klon that has `branch` checked out.
//! 2. Refuse a dirty klon. A receipt names a commit, and a dirty tree is not
//!    that commit, so klon writes nothing at all.
//! 3. Refuse a repository with no `[proof] steps`, and take the approval for
//!    the ones it has.
//! 4. Run every step inside the klon under the envelope, in file order, and
//!    stop at the first failure. Refuse a klon whose HEAD moved while they ran.
//! 5. Write `<common>/klon/receipts/<commit>.json`.
//!
//! `merge` then reads that receipt instead of running the steps again, so a
//! long test suite runs once, when the agent asks for it, and not inside the
//! merge.

use crate::envelope::{step_stdout, Envelope, Options, Root};
use crate::receipt::{self, Receipt, StepResult};
use crate::{config, git, paths, process, Error, Result};
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.check/1";

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the klon to check.
    pub branch: String,
}

/// The `check --json` document: the receipt, plus the schema name and the
/// klon path. The path is a fact about this host, so it stays out of the
/// receipt file itself and appears only here.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    #[serde(flatten)]
    receipt: &'a Receipt,
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

    // --- Step 1: the klon ----------------------------------------------------
    let klon = paths::absolute(&git::klon_of_branch(&worktrees, &args.branch)?.path)?;

    // --- Step 2: the dirty refusal -------------------------------------------
    // It comes first, before the approval prompt and before any step runs. A
    // receipt binds a verdict to a commit; work that is not in that commit
    // would make the receipt a lie.
    if process::dirty(&klon)? {
        return Err(Error::klon(format!(
            "dirty klon: {} holds uncommitted work; commit it before a check",
            klon.display()
        )));
    }

    // --- Step 3: the steps and their approval --------------------------------
    let cfg = config::load(&golden)?;
    let steps = cfg
        .proof
        .as_ref()
        .and_then(|proof| proof.steps.clone())
        .unwrap_or_default();
    if steps.is_empty() {
        return Err(Error::klon(format!(
            "no [proof] steps in {}; add the table before a check",
            golden.join(".klon.toml").display()
        )));
    }
    cfg.ensure_approved(yes, &["proof.steps"])?;

    // --- Step 4: the run -----------------------------------------------------
    // The commit is read before the first step, so the receipt names the tree
    // that the steps saw. A commit inside the klon while the steps run leaves
    // the receipt bound to the older commit, and `merge` then calls it stale.
    let commit = rev(&klon, "HEAD")?;
    let tree = rev(&klon, "HEAD^{tree}")?;
    let started = Instant::now();
    let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());
    for step in &steps {
        let (status, duration_ms) = exec_step(&klon, step, json);
        let failed = status == receipt::Status::Failed;
        results.push(StepResult {
            cmd: step.clone(),
            status,
            duration_ms,
        });
        if failed {
            break;
        }
    }
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // A test suite takes minutes, and the agent that owns the klon can commit
    // inside that window. The steps then saw two trees, and the run proves
    // neither commit. klon writes nothing and says so, the same way `merge`
    // refuses a branch tip that moved under its gate.
    let now = rev(&klon, "HEAD")?;
    if now != commit {
        return Err(Error::klon(format!(
            "{} moved from {} to {} while the steps ran; run check again",
            args.branch,
            short(&commit),
            short(&now)
        )));
    }

    // --- Step 5: the receipt -------------------------------------------------
    let record = receipt::build(
        &commit,
        &tree,
        &args.branch,
        &receipt::steps_hash(&steps),
        results,
        duration_ms,
    );
    let file = receipt::write(&common, &record)?;
    report(&args, &klon, &file, &record, json)
}

/// One step inside the klon under the envelope: the write fence, the resource
/// scope, and the jobserver. The answer is the verdict and the wall time.
///
/// A step that klon cannot even spawn counts as a failed step, not as a failed
/// command: a receipt that names the broken step is more use than an error
/// that names none.
///
/// Under `--json` the step's stdout goes to stderr, because klon owns stdout
/// for the one document.
fn exec_step(klon: &Path, step: &str, json: bool) -> (receipt::Status, u64) {
    let argv = ["sh".to_string(), "-c".to_string(), step.to_string()];
    let started = Instant::now();
    let stdout = match step_stdout(json) {
        Ok(stdout) => stdout,
        Err(err) => {
            eprintln!("{err}");
            None
        }
    };
    let options = Options {
        no_fence: false,
        stdout,
    };
    let status = match Envelope::spawn_and_wait(Root::Klon(klon), &argv, options) {
        Ok(status) if status.success() => receipt::Status::Pass,
        // The step already reported its own failure on its own stderr.
        Ok(_) => receipt::Status::Failed,
        Err(err) => {
            eprintln!("klon: cannot run the step {step}: {err}");
            receipt::Status::Failed
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    (status, duration_ms)
}

/// The report, then the exit code. A failed check is a failed command: an
/// agent that runs `check && merge` must stop at the first half.
fn report(args: &Args, klon: &Path, file: &Path, record: &Receipt, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&Report {
                schema: SCHEMA,
                path: klon,
                receipt: record,
            })
            .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
        // The document carries the whole answer, so the error prints nothing.
        return match record.status {
            receipt::Status::Pass => Ok(()),
            receipt::Status::Failed => Err(Error::Exit(1)),
        };
    }
    for step in &record.results {
        println!(
            "{} {} ({} ms)",
            step.status.name(),
            step.cmd,
            step.duration_ms
        );
    }
    match record.status {
        receipt::Status::Pass => {
            println!(
                "{} passes at {} ({} ms); receipt {}",
                args.branch,
                short(&record.commit),
                record.duration_ms,
                file.display()
            );
            Ok(())
        }
        receipt::Status::Failed => {
            let failed = record
                .results
                .iter()
                .find(|step| step.status == receipt::Status::Failed)
                .map(|step| step.cmd.as_str())
                .unwrap_or("a proof step");
            eprintln!("klon: the receipt is at {}", file.display());
            Err(Error::klon(format!(
                "check failed in {}: {failed}",
                klon.display()
            )))
        }
    }
}

/// The full object id that `revision` names inside the klon.
fn rev(klon: &Path, revision: &str) -> Result<String> {
    Ok(git::run(klon, &["rev-parse", revision])?.trim().to_string())
}

/// The first seven characters of an object id, for a report line.
fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}
