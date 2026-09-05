//! `gh klon doctor [--json] [--repair]`: the host report and the journal repair
//! (handoff §7, spec R31). Every host feature is one row from one probe. A later
//! chunk adds a row to `FEATURES` and a function below it; nothing else changes.

use crate::journal::{self, Entry, Op, State};
use crate::{git, probe, repair, time, Error, Result};
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

/// The `doctor` rows. C5 adds `backend`, C18 the fence ABI, C20 the cgroup
/// delegation, C17 the jobserver, and C24 the radar form. Each is one line.
const FEATURES: &[(&str, Probe)] = &[
    ("btrfs-progs", btrfs_progs),
    ("inotify.max_user_instances", inotify_instances),
    ("inotify.max_user_watches", inotify_watches),
    ("make", make_version),
    ("ninja", ninja_version),
    ("pasta", pasta_version),
];

pub fn run(args: Args, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    let common = git::common_dir(&cwd)?;

    // The journal is read first. An unknown version fails here, before any
    // probe runs and before `--repair` touches anything.
    let found = journal::list(&common)?;
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

    let host = Host {
        golden: &golden,
        common: &common,
    };
    let features: Vec<(&'static str, probe::Status)> = FEATURES
        .iter()
        .map(|(name, probe)| (*name, probe(&host)))
        .collect();
    let report = Report {
        schema: SCHEMA,
        timestamp: time::now_rfc3339(),
        git_version: git_version(),
        filesystem: filesystem(&golden),
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

fn make_version(_host: &Host) -> probe::Status {
    probe::version_of("make", &["--version"])
}

fn ninja_version(_host: &Host) -> probe::Status {
    probe::version_of("ninja", &["--version"])
}

fn pasta_version(_host: &Host) -> probe::Status {
    probe::version_of("pasta", &["--version"])
}

/// The filesystem of golden, from the `statfs` magic on Linux and from
/// `f_fstypename` elsewhere. An unmapped magic prints as hexadecimal, so a new
/// filesystem still gives a stable, comparable answer.
#[cfg(target_os = "linux")]
fn filesystem(path: &Path) -> String {
    /// The magic numbers of the filesystems klon has a backend rule for.
    const NAMES: &[(u32, &str)] = &[
        (0x0102_1994, "tmpfs"),
        (0x5846_5342, "xfs"),
        (0x794c_7630, "overlay"),
        (0x9123_683e, "btrfs"),
        (0xef53, "ext4"),
    ];
    match statfs(path) {
        Some(stat) => {
            let magic = stat.f_type as u32;
            match NAMES.iter().find(|(m, _)| *m == magic) {
                Some((_, name)) => (*name).to_string(),
                None => format!("{magic:#x}"),
            }
        }
        None => "unknown".to_string(),
    }
}

/// macOS and the other BSD systems name the filesystem in the `statfs` result.
#[cfg(not(target_os = "linux"))]
fn filesystem(path: &Path) -> String {
    let stat = match statfs(path) {
        Some(stat) => stat,
        None => return "unknown".to_string(),
    };
    let bytes: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The raw `statfs` result for `path`, or None when the call fails.
fn statfs(path: &Path) -> Option<libc::statfs> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `stat` is a live, owned buffer.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    (rc == 0).then_some(stat)
}
