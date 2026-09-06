//! Lifecycle helpers shared by `rm`, `prune`, and later chunks: the dirty
//! check, the live-process scan, and the detached background delete.

use crate::{git, Error, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// True when `git status --porcelain` in `dir` prints at least one line.
/// Untracked, non-ignored files count as dirty.
pub fn dirty(dir: &Path) -> Result<bool> {
    Ok(!git::run(dir, &["status", "--porcelain"])?.trim().is_empty())
}

/// A process id whose current directory is `dir` or inside it, or None.
/// Our own process is skipped, so `rm` works from inside its own klon.
pub fn live_process(dir: &Path) -> Option<u32> {
    live_process_os(dir)
}

/// Linux: read the `/proc/<pid>/cwd` symlink of every process. Unreadable
/// entries belong to other users or to processes that just left; skip them.
#[cfg(target_os = "linux")]
fn live_process_os(dir: &Path) -> Option<u32> {
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(pid) => pid,
            None => continue, // /proc also holds non-numeric entries.
        };
        if pid == me {
            continue;
        }
        match std::fs::read_link(entry.path().join("cwd")) {
            Ok(cwd) if cwd.starts_with(dir) => return Some(pid),
            _ => continue,
        }
    }
    None
}

/// macOS: `lsof -Fpn -d cwd` prints a `p<pid>` record and an `n<path>` record
/// per process. Not testable on the Linux development host; any failure
/// degrades to "no live process found" with one line on stderr.
#[cfg(not(target_os = "linux"))]
fn live_process_os(dir: &Path) -> Option<u32> {
    let me = std::process::id();
    let output = match Command::new("lsof")
        .args(["-wn", "-F", "pn", "-d", "cwd"])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!("klon: cannot run lsof: {err}; the live-process check found nothing");
            return None;
        }
    };
    if !output.status.success() {
        eprintln!("klon: lsof failed; the live-process check found nothing");
        return None;
    }
    let mut pid: Option<u32> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let Some(n) = line.strip_prefix('n') {
            if pid.is_some_and(|p| p != me) && Path::new(n).starts_with(dir) {
                return pid;
            }
        }
    }
    None
}

/// Start a detached `rm -rf` of `target` at the lowest disk and cpu priority.
/// The call returns at once; the delete continues after `klon` exits.
/// Every optional tool is checked on PATH before the command uses it, so a
/// missing tool can never leave the delete silently undone. Each absence
/// costs one stderr line.
pub fn spawn_background_delete(target: &Path) -> Result<()> {
    let mut missing: Vec<&'static str> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    if tool_on_path("setsid") {
        words.push("setsid");
    } else {
        missing.push("setsid");
    }
    if tool_on_path("nice") {
        words.extend(["nice", "-n", "19"]);
    } else {
        missing.push("nice");
    }
    if tool_on_path("ionice") {
        words.extend(["ionice", "-c", "3"]);
    } else {
        missing.push("ionice");
    }
    if !missing.is_empty() {
        eprintln!(
            "klon: {} missing from PATH; the delete runs without it",
            missing.join(" and ")
        );
    }
    words.extend(["rm", "-rf", "--"]);
    let (program, prefix) = words.split_first().expect("rm is always appended");
    Command::new(program)
        .args(prefix)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(Error::io(format!(
            "start the background delete of {}",
            target.display()
        )))
}

/// True when an executable `name` sits in a PATH directory.
fn tool_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// Every process that carries all of `tags` in its environment, sorted by pid.
/// `stop` uses it to find the tree of one klon (R22). Our own process is
/// skipped, so `stop` works from inside the klon it stops.
///
/// A tag is one `KEY=value` pair and the match is exact on a whole entry of the
/// environment, so the klon `x` never matches the klon `xy`.
pub fn tagged_processes(tags: &[(String, String)]) -> Vec<u32> {
    if tags.is_empty() {
        return Vec::new();
    }
    tagged_processes_os(tags)
}

/// Linux: read `/proc/<pid>/environ`, which holds the environment the process
/// started with, NUL between entries. An unreadable file belongs to another
/// user or to a process that just left; skip it.
#[cfg(target_os = "linux")]
fn tagged_processes_os(tags: &[(String, String)]) -> Vec<u32> {
    let me = std::process::id();
    let needles: Vec<Vec<u8>> = tags
        .iter()
        .map(|(key, value)| format!("{key}={value}").into_bytes())
        .collect();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        eprintln!("klon: cannot read /proc; the process scan found nothing");
        return pids;
    };
    for entry in entries.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(pid) => pid,
            None => continue, // /proc also holds non-numeric entries.
        };
        if pid == me {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        let items: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
        if needles
            .iter()
            .all(|needle| items.contains(&needle.as_slice()))
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// Every other system: the scan needs `/proc`. macOS reads the process group
/// with `proc_listpgrppids` instead; that lands with the macOS envelope in C21.
/// Until then `stop` reports one line and ends nothing.
#[cfg(not(target_os = "linux"))]
fn tagged_processes_os(_tags: &[(String, String)]) -> Vec<u32> {
    eprintln!(
        "klon: the process scan needs /proc; stop cannot find the klon's processes on this system"
    );
    Vec::new()
}

/// Send `signal` to `pid`. The answer is false when the process already left.
pub fn signal(pid: u32, signal: i32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` reads two integers and touches no memory of ours.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// Start a detached delete for every entry of the `.trash` directory.
/// A missing directory is not an error.
pub fn drain_trash(trash: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(trash) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io(format!("read {}", trash.display()))(err)),
    };
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", trash.display())))?;
        spawn_background_delete(&entry.path())?;
    }
    Ok(())
}
