//! `gh klon doctor [--json] [--repair]`: the host report and the journal repair
//! (handoff §7, spec R31). Every host feature is one row from one probe. A later
//! chunk adds a row to `FEATURES` and a function below it; nothing else changes.

use crate::envelope::slots;
use crate::journal::{self, Entry, Op, State};
use crate::{backend, git, probe, radar, repair, time, Error, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.doctor/1";

#[derive(clap::Args)]
pub struct Args {
    /// Move each journal entry to the prior valid state and print each action.
    #[arg(long)]
    pub repair: bool,
}

/// What every probe may read. A later chunk adds a field here, not a parameter.
/// The C5 backend probe reads both paths; the v0 probes read neither.
#[allow(dead_code)]
pub struct Host<'a> {
    pub golden: &'a Path,
    pub common: &'a Path,
}

/// One probe: a name and the function that answers for it.
type Probe = fn(&Host) -> probe::Status;

/// The `doctor` rows. C18 adds the fence ABI, C20 the cgroup delegation, and
/// C17 the jobserver. Each is one line. The selected backend is not a row: it
/// has its own two fields, because it carries a name and a reason.
const FEATURES: &[(&str, Probe)] = &[
    ("btrfs-progs", btrfs_progs),
    ("inotify.max_user_instances", inotify_instances),
    ("inotify.max_user_watches", inotify_watches),
    ("loopback", loopback),
    ("make", make_version),
    ("ninja", ninja_version),
    ("pasta", pasta_version),
    ("radar", radar_form),
    ("reflink", reflink_support),
    ("slots", slots_in_use),
];

pub fn run(args: Args, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    let common = git::common_dir(&cwd)?;

    // The two state files are read first. An unknown version of either fails
    // here, before any probe runs and before `--repair` touches anything. An
    // old binary must never repair or delete a format it cannot read.
    let found = journal::list(&common)?;
    backend::check_probe_cache(&common)?;
    let (repaired, failure) = if args.repair {
        repair_all(&golden, &common, &found)?
    } else {
        (Vec::new(), None)
    };
    // The array always shows the state after the repair, and `repaired` shows
    // what changed. An entry that the repair could not close is still listed.
    let entries = if args.repair {
        journal::list(&common)?
    } else {
        found
    };

    // `--repair` also refreshes the cached backend answer, so a host that
    // changed filesystem or gained a tool gets a fresh probe (C5).
    if args.repair {
        backend::forget_probe(&common)?;
    }

    let host = Host {
        golden: &golden,
        common: &common,
    };
    let features: Vec<(&'static str, probe::Status)> = FEATURES
        .iter()
        .map(|(name, probe)| (*name, probe(&host)))
        .collect();
    let (backend_name, backend_reason) = backend_row(&golden, &common);
    let report = Report {
        schema: SCHEMA,
        timestamp: time::now_rfc3339(),
        git_version: git_version(),
        filesystem: probe::filesystem(&golden),
        backend: backend_name,
        backend_reason,
        features: features
            .iter()
            .map(|(name, status)| (*name, status.report()))
            .collect(),
        journal: entries.iter().map(JournalRow::from).collect(),
        repaired,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    } else {
        print_human(&report, &features);
    }
    // The report prints first, so the user sees what the repair did. The exit
    // code then reports the entry that stayed open.
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Repair every entry and collect one row per action. The answer holds the
/// first reason an entry stayed open; the other entries are still repaired.
fn repair_all(
    golden: &Path,
    common: &Path,
    entries: &[Entry],
) -> Result<(Vec<RepairRow>, Option<Error>)> {
    let mut rows = Vec::new();
    let mut failure = None;
    for entry in entries {
        let outcome = repair::entry(golden, common, entry)?;
        for action in outcome.actions {
            rows.push(RepairRow {
                name: entry.name.clone(),
                state: entry.state,
                path: entry.path.clone(),
                action,
            });
        }
        if let Some(why) = outcome.failure {
            rows.push(RepairRow {
                name: entry.name.clone(),
                state: entry.state,
                path: entry.path.clone(),
                action: format!("the entry stays: {why}"),
            });
            failure.get_or_insert(why);
        }
    }
    Ok((rows, failure))
}

// --- The report --------------------------------------------------------------

#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    timestamp: String,
    git_version: String,
    filesystem: String,
    /// The backend that `add` will use, or `none` when no probe passed.
    backend: String,
    /// Why that backend won: the rejection reason of every preferred backend,
    /// or the detail of the winning probe.
    backend_reason: String,
    features: BTreeMap<&'static str, probe::Report<'a>>,
    journal: Vec<JournalRow>,
    repaired: Vec<RepairRow>,
}

/// One journal entry in the report.
#[derive(Serialize)]
struct JournalRow {
    name: String,
    op: Op,
    state: State,
    path: PathBuf,
    branch: Option<String>,
    started: String,
}

impl From<&Entry> for JournalRow {
    fn from(entry: &Entry) -> JournalRow {
        JournalRow {
            name: entry.name.clone(),
            op: entry.op,
            state: entry.state,
            path: entry.path.clone(),
            branch: entry.branch.clone(),
            started: entry.started.clone(),
        }
    }
}

/// One repair action. `--repair` may take several actions for one entry.
#[derive(Serialize)]
struct RepairRow {
    name: String,
    state: State,
    path: PathBuf,
    action: String,
}

fn print_human(report: &Report, features: &[(&'static str, probe::Status)]) {
    let width = FEATURES
        .iter()
        .map(|(name, _)| name.len())
        .chain([10])
        .max()
        .unwrap_or(10);
    println!("{:width$}  {}", "git", report.git_version);
    println!("{:width$}  {}", "filesystem", report.filesystem);
    println!(
        "{:width$}  {}: {}",
        "backend", report.backend, report.backend_reason
    );
    for (name, status) in features {
        println!("{name:width$}  {}: {}", status.key(), status.detail());
    }
    for row in &report.repaired {
        println!("repair {}: {}", row.name, row.action);
    }
    if report.journal.is_empty() {
        println!("journal: no open entry");
    } else {
        println!("journal: {} open entries", report.journal.len());
        for row in &report.journal {
            println!(
                "  {} {} {} {}",
                row.name,
                op_key(row.op),
                row.state.key(),
                row.path.display()
            );
        }
        println!("run gh klon doctor --repair to close them");
    }
}

fn op_key(op: Op) -> &'static str {
    match op {
        Op::Add => "add",
        Op::Rm => "rm",
        Op::Init => "init",
    }
}

// --- Probes ------------------------------------------------------------------

/// The installed git, without the `git version ` prefix.
fn git_version() -> String {
    let status = probe::version_of("git", &["--version"]);
    let detail = status.detail();
    detail
        .strip_prefix("git version ")
        .unwrap_or(detail)
        .to_string()
}

/// `btrfs-progs` on PATH, or under `$KLON_BTRFS_TOOLS` for a host that keeps it
/// outside PATH.
fn btrfs_progs(_host: &Host) -> probe::Status {
    if let Some(dir) = std::env::var_os("KLON_BTRFS_TOOLS") {
        let candidate = Path::new(&dir).join("btrfs");
        return match probe::executable(&candidate) {
            Some(path) => probe::run_version(&path, &["--version"]),
            None => probe::Status::Absent(format!(
                "KLON_BTRFS_TOOLS is set but {} is not an executable",
                candidate.display()
            )),
        };
    }
    probe::version_of("btrfs", &["--version"])
}

fn inotify_watches(_host: &Host) -> probe::Status {
    sysctl("/proc/sys/fs/inotify/max_user_watches")
}

fn inotify_instances(_host: &Host) -> probe::Status {
    sysctl("/proc/sys/fs/inotify/max_user_instances")
}

/// Read one `/proc/sys` value. The file exists on Linux only.
fn sysctl(path: &str) -> probe::Status {
    match fs::read_to_string(path) {
        Ok(text) => probe::Status::Present(text.trim().to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            probe::Status::Absent(format!("{path} does not exist on this system"))
        }
        Err(err) => probe::Status::Broken(format!("cannot read {path}: {err}")),
    }
}

/// How many loopback addresses the repository hands out right now (R21).
fn slots_in_use(host: &Host) -> probe::Status {
    match slots::in_use(host.common) {
        Ok(0) => probe::Status::Present("no address in use".to_string()),
        Ok(1) => probe::Status::Present("1 address in use".to_string()),
        Ok(count) => probe::Status::Present(format!("{count} addresses in use")),
        Err(err) => probe::Status::Broken(err.to_string()),
    }
}

/// A real bind on the first klon address. `lo` owns all of `127/8` on Linux, so
/// the bind needs no configuration. A host that refuses it cannot give a klon
/// its own address, and `doctor` says so before the first `run`.
fn loopback(_host: &Host) -> probe::Status {
    let address = slots::ip(2);
    match std::net::TcpListener::bind((address.as_str(), 0)) {
        Ok(_) => probe::Status::Present(format!("{address} accepts a bind")),
        Err(err) => probe::Status::Broken(format!("cannot bind {address}: {err}")),
    }
}

fn make_version(_host: &Host) -> probe::Status {
    probe::version_of("make", &["--version"])
}

fn ninja_version(_host: &Host) -> probe::Status {
    probe::version_of("ninja", &["--version"])
}

fn pasta_version(_host: &Host) -> probe::Status {
    probe::version_of("pasta", &["--version"])
}

/// Which `merge-tree` form the conflict radar uses (C24). Both forms work, so
/// the row is always present and the detail names the one in use: `merge-tree
/// --write-tree` on git 2.38 and above, else `legacy merge-tree`.
fn radar_form(host: &Host) -> probe::Status {
    probe::Status::Present(radar::form(host.golden).name().to_string())
}

/// Whether golden's filesystem answers `FICLONE` (C5). The detail names the
/// errno when it does not, so a user can tell "no reflink" from "broken".
fn reflink_support(host: &Host) -> probe::Status {
    backend::reflink::capability(host.golden)
}

/// The backend that `add` will use, and why. A probe failure is a report row,
/// not an exit code: the user asked for a report. `doctor` names no
/// destination, because it clones nothing.
fn backend_row(golden: &Path, common: &Path) -> (String, String) {
    match backend::select(golden, common, None, None) {
        Ok(choice) => (choice.backend.name().to_string(), choice.reason),
        Err(err) => ("none".to_string(), err.to_string()),
    }
}
