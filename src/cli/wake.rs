//! `gh klon wake <branch>`: put a hibernated klon back (spec §7 C29, R28).
//!
//! `wake` is a whole `add` at the recorded path plus one restore. The `add`
//! brings the tracked tree and a warm copy of golden's ignored directories; the
//! restore writes the saved tracked changes and untracked files over it.
//!
//! The klon that comes back is byte for byte the klon that went to sleep, for
//! every file git can see. The ignored build state is golden's, not the one the
//! klon had: those bytes are what the hibernation gave back.

use crate::journal::{self, State};
use crate::{git, hibernate, paths, Error, Result};
use serde::Serialize;
use std::path::Path;

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.wake/1";

/// The `wake --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    branch: &'a str,
    head: String,
}

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the hibernated klon.
    pub branch: String,
}

pub fn run(args: Args, yes: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let cwd = crate::volume::ensure_attached(&cwd)?;
    let golden = git::main_worktree(&cwd)?;
    let common = git::common_dir_of_main(&golden)?;
    let record = hibernate::read(&common, &args.branch)?.ok_or_else(|| {
        Error::klon(format!(
            "{} is not hibernated; gh klon list shows the hibernated klons",
            args.branch
        ))
    })?;
    if git::run(&golden, &["rev-parse", "--verify", "--quiet", &record.work]).is_err() {
        return Err(Error::klon(format!(
            "the work commit {} of {} is gone from the object store; \
             delete {}/klon/hibernate/{}.json to forget the klon",
            record.work,
            record.branch,
            common.display(),
            record.name
        )));
    }

    // The saved tree is a whole tree, so it only fits the head it was cut from.
    // The refusal comes before every change.
    hibernate::refuse_moved_branch(&golden, &record)?;

    // The journal entry covers the whole command, the `add` included, so a kill
    // at any point leaves a state that `doctor --repair` can name. It carries a
    // file name of its own, because the `add` below writes an entry for the
    // same path (`hibernate::journal_name`).
    let mut entry = journal::Record::start_as(
        &common,
        &hibernate::journal_name("wake", &record.path),
        journal::Op::Wake,
        &record.path,
        Some(&record.branch),
    )?;
    // Step 1: the whole `add` transaction at the recorded path.
    crate::cli::add::add_at(&record.branch, &record.path, yes)?;
    // Step 2: the saved files go back over the fresh tree.
    hibernate::restore(&record.path, &record)?;
    entry.reach(State::Restored)?;
    hibernate::remove_ref(&golden, &record.name)?;
    hibernate::remove_record(&common, &record.name)?;
    entry.close()?;

    let head = git::run(&record.path, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    if json {
        let report = Report {
            schema: SCHEMA,
            path: &record.path,
            branch: &record.branch,
            head,
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    } else {
        println!("{}", paths::absolute(&record.path)?.display());
    }
    Ok(())
}
