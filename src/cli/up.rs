//! `gh klon up` (spec §7 C14, R29): bring golden up to date, then warm it.
//!
//! The five steps, in this order:
//! 1. Refuse a dirty golden and a golden that is not on `base`.
//! 2. `git fetch origin`.
//! 3. `git merge --ff-only origin/<base>`.
//! 4. The approved `[warm] steps`, in golden, in order.
//! 5. A hot-spare builder (C9).
//!
//! Step 1 comes before the fetch, so a refusal leaves the repository exactly as
//! it was. The approval gate for the `[warm] steps` runs there too: a run that
//! cannot warm golden should not touch the remote-tracking refs either.

use crate::{branch, config, git, process, spare, Error, Result};
use serde::Serialize;
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.up/1";

#[derive(clap::Args)]
pub struct Args {
    /// Start no hot-spare builder after the run.
    #[arg(long)]
    pub no_spare: bool,
}

/// The `up --json` document. `head_before` and `head_after` are null in a
/// repository with no commit yet.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    base: &'a str,
    head_before: Option<String>,
    head_after: Option<String>,
    /// How many `[warm] steps` ran.
    steps_run: usize,
    /// True when klon asked for a hot-spare builder.
    spare_started: bool,
}

pub fn run(args: Args, yes: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    // One load: `base`, the `[warm] steps`, and the spare depth come from the
    // same file, and a second load would repeat its warning lines.
    let cfg = config::load(&golden)?;
    let base = branch::base_of(&cfg, &golden)?;

    // Step 1. Both refusals come before the fetch, so `up` changes nothing.
    if process::dirty(&golden)? {
        return Err(Error::klon(format!(
            "golden is dirty at {}; commit or stash the changes first",
            golden.display()
        )));
    }
    let on = current_branch(&golden);
    if on.as_deref() != Some(base.as_str()) {
        let has = match &on {
            Some(name) => format!("{name} checked out"),
            None => "a detached HEAD".to_string(),
        };
        return Err(Error::klon(format!(
            "golden is not on base {base}; it has {has}"
        )));
    }
    cfg.ensure_approved(yes, &["warm.steps"])?;

    // Steps 2 and 3.
    let head_before = head(&golden);
    fast_forward(&golden, &base, json)?;
    let head_after = head(&golden);

    // The fast-forward can bring a new `.klon.toml`. The steps that run below
    // must be the ones of the commit golden now holds, and a changed file
    // needs its own approval, so klon reads the file again where HEAD moved.
    let cfg = match head_after == head_before {
        true => cfg,
        false => {
            let cfg = config::load(&golden)?;
            cfg.ensure_approved(yes, &["warm.steps"])?;
            cfg
        }
    };

    // Step 4: `sh -c` per step, in golden, in file order. C22 wraps these in
    // the jobserver and the scope.
    let steps = cfg.warm.as_ref().and_then(|warm| warm.steps.clone());
    let steps = steps.unwrap_or_default();
    for step in &steps {
        let status = Command::new("sh")
            .arg("-c")
            .arg(step)
            .current_dir(&golden)
            .stdout(step_stdout(json)?)
            .status()
            .map_err(Error::io(format!("run sh -c {step:?}")))?;
        if !status.success() {
            let why = match status.code() {
                Some(code) => format!("exit {code}"),
                None => "killed by a signal".to_string(),
            };
            return Err(Error::klon(format!("warm step failed ({why}): {step}")));
        }
    }

    // Step 5: the next spare. The builder is detached and low priority, and a
    // failure to start it costs one line, never the `up`.
    let spare_started = spare::enabled(cfg.spare, args.no_spare);
    spare::start_after(&golden, cfg.spare, args.no_spare);

    if json {
        let report = Report {
            schema: SCHEMA,
            base: &base,
            head_before,
            head_after,
            steps_run: steps.len(),
            spare_started,
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    }
    Ok(())
}

/// Steps 2 and 3: fetch, then fast-forward golden onto `origin/<base>`.
///
/// A repository with no `origin` remote, and a remote that does not carry
/// `base`, each cost one line and no change: `up` still warms golden. A merge
/// that git refuses means golden and the remote diverged, and `up` stops
/// there, because a rebase of golden is the user's decision, not klon's.
fn fast_forward(golden: &Path, base: &str, json: bool) -> Result<()> {
    if git::run(golden, &["remote", "get-url", "origin"]).is_err() {
        eprintln!("klon: no origin remote; up skips the fetch");
        return Ok(());
    }
    git::run(golden, &["fetch", "origin"])?;
    let remote = format!("origin/{base}");
    let full = format!("refs/remotes/{remote}");
    if git::run(golden, &["show-ref", "--verify", "--quiet", &full]).is_err() {
        eprintln!("klon: origin has no {base}; up skips the merge");
        return Ok(());
    }
    let before = head(golden);
    if git::run(golden, &["merge", "--ff-only", &remote]).is_err() {
        return Err(Error::klon(format!(
            "golden diverged from {remote}; rebase or merge it yourself, then run up again"
        )));
    }
    if !json {
        let after = head(golden);
        match (before, after) {
            (Some(before), Some(after)) if before != after => {
                println!("{base} {}..{} fast-forward", short(&before), short(&after));
            }
            (_, Some(after)) => println!("{base} {} up to date", short(&after)),
            (_, None) => {}
        }
    }
    Ok(())
}

/// Where a warm step writes its stdout. Under `--json` klon owns that stream,
/// and a step that prints one line would put it in front of the document, so
/// the step writes to stderr instead.
fn step_stdout(json: bool) -> Result<Stdio> {
    if !json {
        return Ok(Stdio::inherit());
    }
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
        .map_err(Error::io("duplicate stderr for the warm steps"))
}

/// The branch golden has checked out, or None for a detached HEAD.
fn current_branch(golden: &Path) -> Option<String> {
    let name = git::run(golden, &["symbolic-ref", "--short", "HEAD"]).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The object id of HEAD, or None in a repository with no commit yet.
fn head(dir: &Path) -> Option<String> {
    let out = git::run(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).ok()?;
    let oid = out.trim();
    (!oid.is_empty()).then(|| oid.to_string())
}

/// The first seven characters of an object id.
fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}
