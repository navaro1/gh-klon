//! `gh klon stop (<branch> | --path <p>)`: end every process of one klon (R22).
//!
//! `run` tags each command with `KLON_ID` and `KLON_IP`. `stop` finds every
//! process that still carries both tags, sends SIGTERM to each, waits up to
//! three seconds, and sends SIGKILL to the rest. The whole command finishes
//! well inside the five seconds R22 allows.

use crate::cli::run as runner;
use crate::envelope::Envelope;
use crate::{process, Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.stop/1";

/// How long a process may take to answer SIGTERM.
const GRACE: Duration = Duration::from_secs(3);

/// How long the process table may take to lose a process after SIGKILL.
const REAP: Duration = Duration::from_millis(500);

/// The gap between two scans of the process table.
const POLL: Duration = Duration::from_millis(100);

#[derive(clap::Args)]
pub struct Args {
    /// A branch that a klon has checked out.
    pub branch: Option<String>,
    /// The klon path. It must match a registered worktree.
    #[arg(long, conflicts_with = "branch")]
    pub path: Option<PathBuf>,
}

/// The `stop --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    name: &'a str,
    /// The processes the first scan found.
    found: usize,
    /// The processes that ended after SIGTERM.
    terminated: usize,
    /// The processes that needed SIGKILL.
    killed: usize,
    /// The process ids that survived both signals.
    survivors: Vec<u32>,
}

pub fn run(args: Args, json: bool) -> Result<()> {
    let klon = runner::resolve(args.branch.as_deref(), args.path.as_deref())?;
    let envelope = Envelope::load(&klon)?;
    let tags = envelope.tags();

    let found = process::tagged_processes(&tags);
    for pid in &found {
        process::signal(*pid, libc::SIGTERM);
    }
    // A dying tree can still start a child, so every scan reads the whole
    // process table instead of only the pids the first scan saw.
    let after_term = if found.is_empty() {
        Vec::new()
    } else {
        wait_for_exit(&tags, GRACE)
    };
    let killed = after_term.len();
    for pid in &after_term {
        process::signal(*pid, libc::SIGKILL);
    }
    let survivors = if killed == 0 {
        Vec::new()
    } else {
        wait_for_exit(&tags, REAP)
    };
    // A process that started after the first scan can make the second count
    // larger than the first, so the subtraction saturates at zero.
    let terminated = found.len().saturating_sub(killed);

    let report = Report {
        schema: SCHEMA,
        path: &klon,
        name: &envelope.name,
        found: found.len(),
        terminated,
        killed,
        survivors,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    } else if report.found == 0 {
        println!("{}: no live process", report.name);
    } else {
        println!(
            "{}: {} processes, {} ended after SIGTERM, {} after SIGKILL",
            report.name, report.found, report.terminated, report.killed
        );
    }
    if report.survivors.is_empty() {
        return Ok(());
    }
    Err(Error::klon(format!(
        "{} processes of {} survived SIGKILL: {}",
        report.survivors.len(),
        report.name,
        report
            .survivors
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    )))
}

/// Poll the process table until no tagged process is left or `limit` passes.
/// The answer is the list that is still there at the end.
fn wait_for_exit(tags: &[(String, String)], limit: Duration) -> Vec<u32> {
    let start = Instant::now();
    loop {
        let left = process::tagged_processes(tags);
        if left.is_empty() || start.elapsed() >= limit {
            return left;
        }
        std::thread::sleep(POLL);
    }
}
