//! The Linux resource scope of one command (handoff §5, R18).
//!
//! Every command under `run` gets a memory cap and a task cap, so one agent
//! cannot take the whole laptop from its siblings. The cap is
//! `MemTotal / (N + 1)`, where `N` is the number of live klons: the `+ 1`
//! leaves one share for the person at the keyboard.
//!
//! klon takes the strongest mechanism the host offers:
//!
//! | Order | Mechanism | What it gives | What it needs |
//! |---|---|---|---|
//! | 1 | `systemd-run --user --scope` | `memory.high`, `pids.max`, and `CPUWeight` on systemd ≥ 252 | a user D-Bus session |
//! | 2 | a cgroup written by hand | `memory.high` and `pids.max` | a writable cgroup whose `cgroup.subtree_control` holds `memory` |
//! | 3 | `nice -n 10` | a CPU share only | `nice` on PATH |
//!
//! Mechanism 2 and mechanism 3 each print one line on stderr, so a person
//! always knows which cap the command got. systemd 249 accepts `CPUWeight` and
//! ignores it (handoff §11), so klon adds `nice -n 10` inside the scope below
//! systemd 252 and lets `CPUWeight` do the work above it.
//!
//! `stop` reads each cgroup back from a live process and ends the whole tree
//! with one write to `cgroup.kill`. That catches a process which called
//! `setsid` and cleared its own environment, which no other scan can see.

use crate::envelope::{slots, Envelope, Part};
use crate::{git, probe};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The task limit of one klon. A build fans out to a few hundred processes;
/// four thousand leaves room for that and still stops a fork bomb.
const TASKS_MAX: u64 = 4096;

/// The CPU weight of one klon on systemd 252 and above. The default is 100, so
/// half of that makes a klon yield to the person's own session.
const CPU_WEIGHT: u32 = 50;

/// The first systemd that applies `CPUWeight` in a user scope (handoff §5).
const CPU_WEIGHT_SINCE: u32 = 252;

/// The niceness of a command when klon cannot set a CPU weight.
const NICE: &str = "10";

/// The cgroup v2 mount point. Every distribution klon supports mounts it here.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The longest branch part of a unit name. A systemd unit name may hold 255
/// characters, and the prefix and the process id take a few of them.
const NAME_LIMIT: usize = 100;

/// The caps one command gets.
struct Limits {
    /// `MemTotal / (N + 1)` in bytes, or None when `/proc/meminfo` is unreadable.
    memory_high: Option<u64>,
    tasks_max: u64,
}

/// What holds the command.
enum Mechanism {
    /// A transient systemd scope. The number is the systemd major version.
    Systemd(u32),
    /// A cgroup that klon created itself.
    Cgroup(PathBuf),
    /// `nice -n 10` only.
    Nice,
    /// No cap at all.
    Bare,
}

/// A cgroup that klon created for one command. The drop removes it, so the
/// cgroup tree does not grow one empty directory per `run`. An `rmdir` of a
/// cgroup that still holds a process fails, and that failure is correct: the
/// directory must stay while the command runs.
pub struct Scope {
    cgroup: Option<PathBuf>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(dir) = &self.cgroup {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// Fill `envelope.scope` with the strongest mechanism this host offers and
/// print one line when klon had to step down. The answer is the guard that
/// removes a cgroup klon made.
pub fn apply(envelope: &mut Envelope) -> Scope {
    let common = git::common_dir(&envelope.klon).ok();
    let limits = limits(common.as_deref());
    let unit = unit_name(&envelope.name);
    let mechanism = select(&unit, &limits);
    let mut scope = Scope { cgroup: None };
    let wrapper = match &mechanism {
        Mechanism::Systemd(version) => systemd_words(&unit, &limits, *version),
        Mechanism::Cgroup(dir) => {
            eprintln!(
                "klon: no systemd user scope here; klon caps memory through {}",
                dir.display()
            );
            scope.cgroup = Some(dir.clone());
            let mut words = join_words(dir);
            words.extend(nice_words());
            words
        }
        Mechanism::Nice => {
            eprintln!(
                "klon: no systemd user scope and no writable cgroup here; \
                 the command runs under nice -n {NICE}"
            );
            vec!["nice".to_string(), "-n".to_string(), NICE.to_string()]
        }
        Mechanism::Bare => {
            eprintln!(
                "klon: no systemd user scope, no writable cgroup, and no nice here; \
                 the command runs with no resource cap"
            );
            Vec::new()
        }
    };
    if !wrapper.is_empty() {
        envelope.scope = Some(Part {
            vars: Vec::new(),
            wrapper,
        });
    }
    scope
}

/// The mechanism this host offers, strongest first. The systemd check runs a
/// real scope, because `systemd-run` is on PATH on every systemd host and
/// still fails without a user D-Bus session, which is what a container and a
/// CI runner give.
fn select(unit: &str, limits: &Limits) -> Mechanism {
    if let Some(version) = systemd_version() {
        if user_scope_starts() {
            return Mechanism::Systemd(version);
        }
    }
    if probe::tool_path("sh").is_some() {
        if let Some(dir) = make_cgroup(unit, limits) {
            return Mechanism::Cgroup(dir);
        }
    }
    if probe::tool_path("nice").is_some() {
        return Mechanism::Nice;
    }
    Mechanism::Bare
}

// --- The numbers -------------------------------------------------------------

/// The caps of one command. `N` is the number of live klons of this
/// repository, which is the number of loopback addresses in use: `add` takes
/// one per klon and `rm` gives it back.
fn limits(common: Option<&Path>) -> Limits {
    let live = common
        .and_then(|common| slots::in_use(common).ok())
        .unwrap_or(0);
    let share = u64::try_from(live).unwrap_or(0) + 1;
    Limits {
        memory_high: mem_total().map(|total| total / share),
        tasks_max: TASKS_MAX,
    }
}

/// `MemTotal` from `/proc/meminfo`, in bytes. The line reads `MemTotal:
/// <n> kB`, and the unit has been kB since the file existed.
fn mem_total() -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
    kib.checked_mul(1024)
}

// --- systemd -----------------------------------------------------------------

/// The systemd major version, from `systemd-run --version`. The first line
/// reads `systemd 249 (249.11-0ubuntu3.22)`.
fn systemd_version() -> Option<u32> {
    match probe::version_of("systemd-run", &["--version"]) {
        probe::Status::Present(line) => version_number(&line),
        _ => None,
    }
}

/// The version number in a `systemd <n> (...)` line.
fn version_number(line: &str) -> Option<u32> {
    line.split_whitespace().find_map(|word| {
        word.trim_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()
    })
}

/// True when a transient user scope really starts. Starting one is the only
/// honest answer: a host can hold `systemd-run` and no user D-Bus session at
/// all, which is what a container and a CI runner give. The call costs about
/// 60 ms on the development laptop, and a host with no user bus at all pays
/// nothing, because the socket check below already answers for it.
fn user_scope_starts() -> bool {
    if !user_bus_exists() {
        return false;
    }
    Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// True when the user bus that `systemd-run --user` needs has an address.
/// `DBUS_SESSION_BUS_ADDRESS` names it, and `$XDG_RUNTIME_DIR/bus` is the
/// default socket when the variable is unset.
fn user_bus_exists() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| Path::new(&dir).join("bus").exists())
}

/// `systemd-run --user --scope --quiet --unit <unit> -p ... --`, and
/// `nice -n 10` behind it while `CPUWeight` does nothing (handoff §11).
fn systemd_words(unit: &str, limits: &Limits, version: u32) -> Vec<String> {
    let mut words: Vec<String> = ["systemd-run", "--user", "--scope", "--quiet", "--unit"]
        .iter()
        .map(|word| (*word).to_string())
        .collect();
    words.push(unit.to_string());
    if let Some(memory) = limits.memory_high {
        words.push("-p".to_string());
        words.push(format!("MemoryHigh={memory}"));
    }
    words.push("-p".to_string());
    words.push(format!("TasksMax={}", limits.tasks_max));
    if version >= CPU_WEIGHT_SINCE {
        words.push("-p".to_string());
        words.push(format!("CPUWeight={CPU_WEIGHT}"));
    }
    words.push("--".to_string());
    if version < CPU_WEIGHT_SINCE {
        words.extend(nice_words());
    }
    words
}

/// `nice -n 10`, or nothing when `nice` is absent. A missing `nice` costs the
/// command its CPU share and nothing else, so it prints no line of its own.
fn nice_words() -> Vec<String> {
    match probe::tool_path("nice") {
        Some(_) => vec!["nice".to_string(), "-n".to_string(), NICE.to_string()],
        None => Vec::new(),
    }
}

// --- The cgroup written by hand ----------------------------------------------

/// Create `<parent>/<unit>` and write the caps into it. The first parent that
/// takes the directory wins, and a parent that refuses it costs nothing.
fn make_cgroup(unit: &str, limits: &Limits) -> Option<PathBuf> {
    memory_parents()
        .into_iter()
        .find_map(|dir| fill_cgroup(&dir.join(unit), limits))
}

/// The cgroups of this process, its own first, that klon may put a capped
/// child in. A parent whose `cgroup.subtree_control` misses `memory` gives its
/// children no `memory.high` file at all, and a parent klon cannot write in
/// takes no child, so klon skips both. `doctor` reads the same list, so the
/// row it prints names the mechanism that `run` will really take.
fn memory_parents() -> Vec<PathBuf> {
    match own_cgroup() {
        Some(dir) => ancestors(dir)
            .into_iter()
            .filter(|dir| delegates_memory(dir) && writable(dir))
            .collect(),
        None => Vec::new(),
    }
}

/// True when this process may create a directory in `dir`. `access` answers
/// for a read-only mount as well as for a permission the user does not hold,
/// which is what a container and a locked-down host give. A parent that passes
/// here and still refuses the `mkdir` costs nothing: `make_cgroup` walks on.
fn writable(dir: &Path) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is NUL-terminated and `access` touches no memory of ours.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// `start` and every cgroup above it, the mount point excluded. klon never
/// makes a cgroup at the root of the hierarchy and never kills one there.
fn ancestors(start: PathBuf) -> Vec<PathBuf> {
    let root = Path::new(CGROUP_ROOT);
    let mut out = Vec::new();
    let mut here = Some(start);
    while let Some(dir) = here.filter(|dir| dir != root && dir.starts_with(root)) {
        here = dir.parent().map(Path::to_path_buf);
        out.push(dir);
    }
    out
}

/// The cgroup v2 directory of this process. `/proc/self/cgroup` holds one
/// `0::<path>` line under the unified hierarchy, and the path is relative to
/// the cgroup namespace, which is what `/sys/fs/cgroup` shows.
fn own_cgroup() -> Option<PathBuf> {
    cgroup_of(&fs::read_to_string("/proc/self/cgroup").ok()?)
}

/// The cgroup directory named by the body of a `/proc/<pid>/cgroup` file.
fn cgroup_of(text: &str) -> Option<PathBuf> {
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    Some(Path::new(CGROUP_ROOT).join(path.strip_prefix('/').unwrap_or(path)))
}

/// True when a child of `dir` would carry the memory controller.
fn delegates_memory(dir: &Path) -> bool {
    fs::read_to_string(dir.join("cgroup.subtree_control"))
        .is_ok_and(|text| text.split_whitespace().any(|word| word == "memory"))
}

/// Create the cgroup and write the caps. A directory that already exists is
/// reused: only this process id can have made it. A failed `memory.high`
/// leaves no cap, so the directory goes again and the walk carries on.
fn fill_cgroup(path: &Path, limits: &Limits) -> Option<PathBuf> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    if let Some(memory) = limits.memory_high {
        if fs::write(path.join("memory.high"), memory.to_string()).is_err() {
            let _ = fs::remove_dir(path);
            return None;
        }
    }
    // `pids.max` is a bonus. The memory cap is the one R18 names.
    let _ = fs::write(path.join("pids.max"), limits.tasks_max.to_string());
    Some(path.to_path_buf())
}

/// The words that move the command into `dir` and then become the command.
/// The shell writes its own process id, and `exec` keeps that id, so the
/// command itself lands in the cgroup with no window in between.
fn join_words(dir: &Path) -> Vec<String> {
    let procs = quote(&dir.join("cgroup.procs").to_string_lossy());
    let script = format!(
        "echo $$ > {procs} 2>/dev/null || \
         echo 'klon: cannot join the cgroup; the command runs with no memory cap' >&2\nexec \"$@\""
    );
    vec![
        "sh".to_string(),
        "-c".to_string(),
        script,
        "klon-scope".to_string(),
    ]
}

/// `text` as one single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

// --- Names -------------------------------------------------------------------

/// The systemd unit and cgroup name of one command: `klon-<branch>-<pid>`.
///
/// A systemd unit name holds only `A-Z a-z 0-9 : _ . -`, and a branch may hold
/// a slash, so every other character becomes `-`. The process id keeps two
/// commands of one klon apart, and `stop` reads the shape back to prove that a
/// cgroup belongs to klon before it writes `cgroup.kill`.
fn unit_name(name: &str) -> String {
    format!("klon-{}-{}", sanitize(name), std::process::id())
}

/// The branch name with every character a unit name refuses replaced by `-`.
fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Every character above is ASCII, so the cut lands on a boundary.
    out.truncate(NAME_LIMIT);
    out
}

// --- stop --------------------------------------------------------------------

/// Every cgroup that klon made for the klon `name`, read back from `pids`.
/// Two `run` commands in one klon make two cgroups, so the answer is a list
/// and `stop` empties each of them.
///
/// A directory joins the list only when it carries klon's own name shape,
/// `klon-<branch>-<digits>`, so `stop` can never write `cgroup.kill` into the
/// cgroup of the caller's own login session.
///
/// A command whose very first `exec` clears its own environment leaves no
/// tagged process and so names no cgroup here. That gap is older than C20:
/// `stop` finds nothing at all for such a tree, cgroup or no cgroup.
pub fn klon_cgroups(pids: &[u32], name: &str) -> Vec<PathBuf> {
    let prefix = format!("klon-{}-", sanitize(name));
    let mut found: Vec<PathBuf> = Vec::new();
    for pid in pids {
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            continue;
        };
        let Some(dir) = cgroup_of(&text) else {
            continue;
        };
        // A command may make cgroups of its own below klon's, so the walk
        // rises until it finds klon's own directory or leaves the hierarchy.
        if let Some(dir) = ancestors(dir)
            .into_iter()
            .find(|dir| is_klon_cgroup(dir, &prefix))
        {
            if !found.contains(&dir) {
                found.push(dir);
            }
        }
    }
    found
}

/// True when `dir` is named `<prefix><digits>`, with the `.scope` suffix that
/// systemd adds allowed, and holds a `cgroup.kill` file.
fn is_klon_cgroup(dir: &Path, prefix: &str) -> bool {
    let Some(base) = dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let base = base.strip_suffix(".scope").unwrap_or(base);
    let Some(tail) = base.strip_prefix(prefix) else {
        return false;
    };
    !tail.is_empty()
        && tail.bytes().all(|byte| byte.is_ascii_digit())
        && dir.join("cgroup.kill").exists()
}

/// Send SIGKILL to every process of `dir` with one write. `cgroup.kill`
/// arrived in kernel 5.14; an older kernel has no such file and the per-process
/// signals of `stop` stay the whole answer.
pub fn kill(dir: &Path) -> bool {
    fs::write(dir.join("cgroup.kill"), "1").is_ok()
}

// --- doctor ------------------------------------------------------------------

/// The `systemd-run` row: the version line of the tool.
pub fn systemd_status() -> probe::Status {
    probe::version_of("systemd-run", &["--version"])
}

/// The `cgroup.controllers` row: the controllers systemd delegated to this
/// user. Only a delegated controller can cap a klon without a privilege.
pub fn controllers_status() -> probe::Status {
    let Some(dir) = user_slice().or_else(own_cgroup) else {
        return probe::Status::Absent("no cgroup v2 hierarchy on this host".to_string());
    };
    let file = dir.join("cgroup.controllers");
    match fs::read_to_string(&file) {
        Ok(text) if text.trim().is_empty() => {
            probe::Status::Absent(format!("{} names no controller", file.display()))
        }
        Ok(text) => probe::Status::Present(text.trim().to_string()),
        Err(err) => probe::Status::Broken(format!("cannot read {}: {err}", file.display())),
    }
}

/// `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service`, the cgroup
/// systemd delegates to a login user (handoff §5), or None when it is absent.
fn user_slice() -> Option<PathBuf> {
    // SAFETY: `getuid` reads one integer and cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = Path::new(CGROUP_ROOT)
        .join("user.slice")
        .join(format!("user-{uid}.slice"))
        .join(format!("user@{uid}.service"));
    dir.is_dir().then_some(dir)
}

/// The `scope` row: the mechanism the next `run` of this repository would take
/// and the caps it would apply. The row makes no cgroup: it names the parent
/// that `run` would use instead.
pub fn scope_status(common: &Path) -> probe::Status {
    let limits = limits(Some(common));
    let caps = match limits.memory_high {
        Some(memory) => format!(
            "MemoryHigh={}M TasksMax={}",
            memory / (1024 * 1024),
            limits.tasks_max
        ),
        None => format!(
            "TasksMax={}; /proc/meminfo has no MemTotal",
            limits.tasks_max
        ),
    };
    match systemd_version().filter(|_| user_scope_starts()) {
        Some(version) => probe::Status::Present(format!("systemd {version} scope: {caps}")),
        None => match cgroup_parent() {
            Some(dir) => probe::Status::Present(format!("cgroup under {}: {caps}", dir.display())),
            None if probe::tool_path("nice").is_some() => {
                probe::Status::Absent(format!("no scope and no writable cgroup; nice -n {NICE}"))
            }
            None => probe::Status::Absent(
                "no scope, no writable cgroup, and no nice; commands run with no cap".to_string(),
            ),
        },
    }
}

/// The cgroup that `make_cgroup` would write into, without creating anything.
fn cgroup_parent() -> Option<PathBuf> {
    memory_parents().into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_name_becomes_a_legal_unit_name() {
        assert_eq!(sanitize("feature"), "feature");
        assert_eq!(sanitize("feat/x y"), "feat-x-y");
        assert_eq!(sanitize("a+b@c"), "a-b-c");
        assert_eq!(sanitize(&"x".repeat(200)).len(), NAME_LIMIT);
        assert!(unit_name("feat/x").starts_with("klon-feat-x-"));
    }

    #[test]
    fn the_cgroup_guard_refuses_a_directory_that_is_not_klons() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prefix = "klon-feature-";
        // A session scope of the caller: the name is not klon's.
        let session = tmp.path().join("session-3.scope");
        fs::create_dir(&session).unwrap();
        fs::write(session.join("cgroup.kill"), "").unwrap();
        assert!(!is_klon_cgroup(&session, prefix));
        // klon's own scope, with and without the systemd suffix.
        for name in ["klon-feature-12.scope", "klon-feature-12"] {
            let dir = tmp.path().join(name);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("cgroup.kill"), "").unwrap();
            assert!(is_klon_cgroup(&dir, prefix), "{name} must match");
        }
        // The right prefix and no process id at all is not klon's shape.
        let odd = tmp.path().join("klon-feature-x");
        fs::create_dir(&odd).unwrap();
        fs::write(odd.join("cgroup.kill"), "").unwrap();
        assert!(!is_klon_cgroup(&odd, prefix));
        // klon's shape without `cgroup.kill` is an older kernel; no write.
        let old = tmp.path().join("klon-feature-9");
        fs::create_dir(&old).unwrap();
        assert!(!is_klon_cgroup(&old, prefix));
    }

    #[test]
    fn the_version_line_gives_the_major_number() {
        assert_eq!(
            version_number("systemd 249 (249.11-0ubuntu3.22)"),
            Some(249)
        );
        assert_eq!(version_number("systemd 255 (255.4-1ubuntu8)"), Some(255));
        assert_eq!(version_number("no number here"), None);
    }

    #[test]
    fn a_path_with_a_quote_survives_the_shell() {
        assert_eq!(quote("/sys/fs/cgroup/a b"), "'/sys/fs/cgroup/a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_cgroup_line_names_a_directory_under_the_mount_point() {
        assert_eq!(
            cgroup_of("0::/user.slice/session-3.scope\n"),
            Some(PathBuf::from("/sys/fs/cgroup/user.slice/session-3.scope"))
        );
        assert_eq!(cgroup_of("1:name=systemd:/x\n"), None);
    }
}
