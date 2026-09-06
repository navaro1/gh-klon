//! Lifecycle helpers shared by `rm`, `prune`, and later chunks: the dirty
//! check, the live-process scan, and the detached background delete.

use crate::{git, Error, Result};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// True when `git status --porcelain` in `dir` prints at least one line.
/// Untracked, non-ignored files count as dirty.
pub fn dirty(dir: &Path) -> Result<bool> {
    Ok(!git::run(dir, &["status", "--porcelain"])?.trim().is_empty())
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

/// The `setsid nice -n 19 ionice -c 3` prefix of a detached low-priority
/// command, minus every tool that PATH lacks. Each absence costs one stderr
/// line that names `job`, so a missing tool can never leave the work silently
/// undone. `ionice` is a Linux tool; on macOS the spare builder lowers its
/// own priority with `PRIO_DARWIN_BG` instead.
fn low_priority_prefix(job: &str) -> Vec<&'static str> {
    let mut missing: Vec<&'static str> = Vec::new();
    let mut words: Vec<&'static str> = Vec::new();
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
    #[cfg(target_os = "linux")]
    if tool_on_path("ionice") {
        words.extend(["ionice", "-c", "3"]);
    } else {
        missing.push("ionice");
    }
    if !missing.is_empty() {
        eprintln!(
            "klon: {} missing from PATH; the {job} runs without it",
            missing.join(" and ")
        );
    }
    words
}

/// Start `words` detached, with every stream on `/dev/null`. The call returns
/// at once; the child continues after `klon` exits.
fn spawn_detached(words: &[&OsStr]) -> std::io::Result<()> {
    let (program, rest) = words.split_first().expect("a program word");
    Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Start a detached `rm -rf` of `target` at the lowest disk and cpu priority.
/// The call returns at once; the delete continues after `klon` exits.
pub fn spawn_background_delete(target: &Path) -> Result<()> {
    let mut words: Vec<&OsStr> = low_priority_prefix("delete")
        .into_iter()
        .map(OsStr::new)
        .collect();
    words.extend(["rm", "-rf", "--"].map(OsStr::new));
    words.push(target.as_os_str());
    spawn_detached(&words).map_err(Error::io(format!(
        "start the background delete of {}",
        target.display()
    )))
}

/// Start a detached `gh-klon <args>` at the lowest disk and cpu priority. The
/// binary is this process, so the child runs the same klon.
pub fn spawn_detached_klon(args: &[&OsStr], job: &str) -> Result<()> {
    spawn_detached_klon_with(args, job, Detached::default())
}

/// How a detached klon process differs from the plain one. The background warm
/// needs all three fields (C12, R36): the klon as its working directory, so
/// the live-process scan of `rm` finds it; a log file, because a detached
/// process has no terminal to report to; and the klon's `stop` tags, so `stop`
/// can end it (R22).
#[derive(Default)]
pub struct Detached<'a> {
    /// The working directory of the child. None keeps this process's own.
    pub cwd: Option<&'a Path>,
    /// The file that takes the child's stderr. None sends it to `/dev/null`.
    pub log: Option<std::fs::File>,
    /// Variables the child carries on top of this process's environment.
    pub env: Vec<(String, String)>,
}

/// `spawn_detached_klon` with a working directory, a log file, and extra
/// variables.
pub fn spawn_detached_klon_with(args: &[&OsStr], job: &str, setup: Detached) -> Result<()> {
    let exe = std::env::current_exe().map_err(Error::io("find the klon binary"))?;
    let mut words: Vec<&OsStr> = low_priority_prefix(job)
        .into_iter()
        .map(OsStr::new)
        .collect();
    words.push(exe.as_os_str());
    words.extend_from_slice(args);
    let (program, rest) = words.split_first().expect("a program word");
    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(match setup.log {
            Some(file) => Stdio::from(file),
            None => Stdio::null(),
        });
    if let Some(cwd) = setup.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in setup.env {
        command.env(key, value);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(Error::io(format!("start the {job}")))
}

/// True when an executable `name` sits in a PATH directory.
fn tool_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// Every process of one klon, sorted by pid. `stop` ends this list (R22).
/// Our own process is skipped, so `stop` works from inside the klon it stops.
///
/// A process belongs to the klon when it carries all of `tags` in its
/// environment, or when it shares a session with a process that does. A tag is
/// one `KEY=value` pair and the match is exact on a whole entry of the
/// environment, so the klon `x` never matches the klon `xy`.
///
/// The session part catches a descendant that lost the tags. `env -i cc` keeps
/// no variable of its parent, and `run` gives the whole command tree one
/// session, so the session still names it. A command that clears its own
/// environment in its very first `exec` leaves no tagged process at all and
/// stays invisible; the C20 cgroup closes that last gap.
pub fn klon_processes(tags: &[(String, String)]) -> Vec<u32> {
    if tags.is_empty() {
        return Vec::new();
    }
    klon_processes_os(tags)
}

/// A process id whose current directory is `dir` or inside it, or None.
/// Our own process is skipped, so `rm` works from inside its own klon.
///
/// The check is the current directory only. Reading `/proc/<pid>/environ` for
/// every process would also find a `run` command that changed directory, and
/// it measured 165 ms on this host, well past the 100 ms that R8 gives `rm`.
/// C20 puts the tree in a cgroup and answers the same question with one read.
pub fn live_process(dir: &Path) -> Option<u32> {
    live_process_os(dir)
}

/// Linux: read `/proc/<pid>/environ`, which holds the environment the process
/// started with, NUL between entries. An unreadable file belongs to another
/// user or to a process that just left; skip it. The session comes from
/// `getsid`, one syscall per process.
#[cfg(target_os = "linux")]
fn klon_processes_os(tags: &[(String, String)]) -> Vec<u32> {
    use std::collections::BTreeSet;

    let me = std::process::id();
    let needles = needles(tags);
    let Ok(entries) = std::fs::read_dir("/proc") else {
        eprintln!("klon: cannot read /proc; the process scan found nothing");
        return Vec::new();
    };
    // Pass one: the processes that carry the tags, and the sessions they sit in.
    let mut pids: Vec<u32> = Vec::new();
    let mut sessions: BTreeSet<i32> = BTreeSet::new();
    for entry in entries.flatten() {
        let Some(pid) = pid_of(&entry) else { continue };
        if pid == me {
            continue;
        }
        if !has_tags(&entry.path(), &needles) {
            continue;
        }
        pids.push(pid);
        if let Some(session) = session_of(pid) {
            sessions.insert(session);
        }
    }
    // klon's own session belongs to the caller's terminal. A tagged process
    // there means `stop` runs inside the very tree it stops, and the sweep
    // would take the terminal with it. The tagged processes still go.
    // SAFETY: `getsid(0)` names the calling process and reads one integer.
    let mine = unsafe { libc::getsid(0) };
    sessions.remove(&mine);
    if sessions.is_empty() {
        pids.sort_unstable();
        return pids;
    }
    // Pass two: every other member of those sessions, tagged or not. It runs
    // only when pass one found something, so an idle klon pays one pass.
    let Ok(entries) = std::fs::read_dir("/proc") else {
        pids.sort_unstable();
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = pid_of(&entry) else { continue };
        if pid == me {
            continue;
        }
        if session_of(pid).is_some_and(|s| sessions.contains(&s)) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Linux: read the `/proc/<pid>/cwd` symlink of every process. Unreadable
/// entries belong to other users or to processes that just left; skip them.
#[cfg(target_os = "linux")]
fn live_process_os(dir: &Path) -> Option<u32> {
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = pid_of(&entry) else { continue };
        if pid == me {
            continue;
        }
        if std::fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd.starts_with(dir)) {
            return Some(pid);
        }
    }
    None
}

/// The `KEY=value` byte strings that a member of the klon must carry.
#[cfg(target_os = "linux")]
fn needles(tags: &[(String, String)]) -> Vec<Vec<u8>> {
    tags.iter()
        .map(|(key, value)| format!("{key}={value}").into_bytes())
        .collect()
}

/// The process id of a `/proc` entry, or None for a non-numeric name.
#[cfg(target_os = "linux")]
fn pid_of(entry: &std::fs::DirEntry) -> Option<u32> {
    entry
        .file_name()
        .to_str()
        .and_then(|name| name.parse().ok())
}

/// True when the process at `proc_dir` carries every needle.
#[cfg(target_os = "linux")]
fn has_tags(proc_dir: &Path, needles: &[Vec<u8>]) -> bool {
    if needles.is_empty() {
        return false;
    }
    let Ok(bytes) = std::fs::read(proc_dir.join("environ")) else {
        return false;
    };
    let items: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    needles
        .iter()
        .all(|needle| items.contains(&needle.as_slice()))
}

/// The session id of `pid`, or None when the process already left. `getsid` is
/// one syscall. Reading `/proc/<pid>/stat` gives the same number and costs a
/// file open, a read, and a parse for every process; a whole pass over 700
/// processes measured 81 ms that way and about 1 ms this way, and `stop` makes
/// one pass every 100 ms while it waits.
///
/// Linux never refuses `getsid` for a process of another session, so the
/// answer is missing only for a pid that is gone.
#[cfg(target_os = "linux")]
fn session_of(pid: u32) -> Option<i32> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: `getsid` reads one integer and touches no memory of ours.
    let session = unsafe { libc::getsid(pid) };
    (session >= 0).then_some(session)
}

/// Every other system: the scan needs `/proc`. macOS reads the process group
/// with `proc_listpgrppids` instead; that lands with the macOS envelope in C21.
/// Until then `stop` reports one line and ends nothing.
#[cfg(not(target_os = "linux"))]
fn klon_processes_os(_tags: &[(String, String)]) -> Vec<u32> {
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
