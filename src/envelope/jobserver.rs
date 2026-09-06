//! The build-slot jobserver (handoff §5, spec §7 C17, R19).
//!
//! Every klon of one user shares one token store: a fifo at
//! `$XDG_RUNTIME_DIR/klon/jobserver`, or `~/.klon/jobserver` on a host without
//! a runtime directory. The store holds `nproc - 2` tokens. A build tool takes
//! one byte to start a job and writes the byte back when the job ends, so the
//! whole machine runs a bounded number of compilers however many klons build.
//!
//! klon speaks the **pipe style** of the handshake. `run` opens the fifo once
//! for reading and writing, duplicates the descriptor into a read end and a
//! write end, clears `FD_CLOEXEC` on both, and exports
//! `MAKEFLAGS=-j --jobserver-auth=<read>,<write>`. The other style,
//! `--jobserver-auth=fifo:<path>`, is a **fatal error** on make 4.3, which
//! Ubuntu 22.04 and 24.04 ship (handoff §11), so klon never emits it.
//!
//! A client that a signal ends never writes its token back. `top_up` repairs
//! the store: it counts the tokens without taking them, and, when no process
//! of a klon names this fifo, it writes the store back to the target.

use crate::envelope::{Envelope, Part};
use crate::{Error, Result};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

/// The variable that names the fifo. `add` writes it into `<klon>/.klon/env`
/// and `run` exports it again, so a command inside a klon can find the store.
pub const PATH_VAR: &str = "KLON_JOBSERVER";

/// The variable that turns the jobserver off.
pub const OFF_VAR: &str = "KLON_NO_JOBSERVER";

/// The variable that replaces the computed token target. Tests set it, and a
/// user tunes the machine with it.
pub const TOKENS_VAR: &str = "KLON_JOBSERVER_TOKENS";

/// The variables that carry the handshake. klon writes all three, because the
/// jobserver crate that cargo uses reads `CARGO_MAKEFLAGS` before `MAKEFLAGS`:
/// a value that klon inherited from an ancestor cargo would otherwise win and
/// point the command at descriptors it never received.
const HANDSHAKE_VARS: [&str; 3] = ["MAKEFLAGS", "MFLAGS", "CARGO_MAKEFLAGS"];

/// The byte one token is. A client may write back any non-NUL byte; `+` is the
/// byte GNU make itself uses.
const TOKEN: u8 = b'+';

/// The CPUs klon keeps outside the pool: one for the agent's own tools and one
/// for the person's interactive work.
const RESERVED: usize = 2;

/// The largest target klon accepts. A fifo buffer holds 64 KiB on Linux, so a
/// target below this can never block the write that fills the store.
const MAX_TOKENS: usize = 4096;

// --- The store ---------------------------------------------------------------

/// The fifo of this user. `XDG_RUNTIME_DIR` is a per-user tmpfs that the login
/// session owns, so the store dies with the session and never survives a
/// reboot with stale tokens. A host without that variable keeps the store
/// under `~/.klon`, and a host without `HOME` puts it in the temporary
/// directory, one path per user id.
pub fn path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("klon").join("jobserver");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home).join(".klon").join("jobserver");
    }
    // SAFETY: `geteuid` reads one integer and cannot fail.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("klon-{uid}"))
        .join("jobserver")
}

/// The token count klon aims for: `nproc - 2`, and at least one. A machine
/// with one or two CPUs still gets a token, so a build under `run` makes
/// progress.
pub fn target() -> usize {
    let Some(text) = std::env::var_os(TOKENS_VAR) else {
        return default_target();
    };
    match text.to_string_lossy().trim().parse::<usize>() {
        Ok(count) if count <= MAX_TOKENS => count,
        _ => {
            let fallback = default_target();
            eprintln!(
                "klon: {TOKENS_VAR}={} is not a count from 0 to {MAX_TOKENS}; klon uses {fallback}",
                text.to_string_lossy()
            );
            fallback
        }
    }
}

fn default_target() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cpus.saturating_sub(RESERVED).max(1)
}

/// True when the user turned the jobserver off. Any value but the empty string
/// and `0` counts, so both `KLON_NO_JOBSERVER=1` and `KLON_NO_JOBSERVER=yes`
/// work.
pub fn is_off() -> bool {
    std::env::var_os(OFF_VAR).is_some_and(|value| !value.is_empty() && value != "0")
}

// --- The envelope part -------------------------------------------------------

/// The jobserver part of the envelope. The answer is never None: a host that
/// cannot hold a store still gets the three handshake variables as empty
/// strings, so a value klon inherited can never point the command at a
/// descriptor of another process.
pub fn attach(envelope: &Envelope) -> Option<Part> {
    if is_off() {
        return Some(off_part());
    }
    match connect(envelope) {
        Ok(part) => Some(part),
        Err(err) => {
            eprintln!("{err}; the command runs without build slots");
            Some(off_part())
        }
    }
}

/// The part that turns every jobserver client off. An empty `MAKEFLAGS` is a
/// valid value that make and cargo both read as "no flags", and it replaces
/// any value the caller's own environment held.
fn off_part() -> Part {
    Part {
        vars: HANDSHAKE_VARS
            .iter()
            .map(|key| ((*key).to_string(), String::new()))
            .collect(),
        wrapper: Vec::new(),
    }
}

/// Open the store and build the handshake. The two descriptors stay open for
/// the life of this process on purpose: the command inherits them across
/// `exec`, and `run` exits as soon as the command does.
fn connect(envelope: &Envelope) -> Result<Part> {
    let path = fifo_for(envelope);
    let target = target();
    ensure(&path, target)?;
    // A client that a signal ended never wrote its token back. The repair runs
    // before the command starts, so this command sees a full store. It stays
    // silent: `run` gives stderr to the command, not to klon.
    top_up_at(&path, target, Look::WhenShort)?;

    let file = open_store(&path)?;
    let read = inherit_dup(file.as_raw_fd(), &path)?;
    let write = inherit_dup(file.as_raw_fd(), &path)?;
    drop(file);

    let auth = format!("-j --jobserver-auth={read},{write}");
    let mut vars: Vec<(String, String)> = HANDSHAKE_VARS
        .iter()
        .map(|key| ((*key).to_string(), auth.clone()))
        .collect();
    vars.push((PATH_VAR.to_string(), path.to_string_lossy().into_owned()));
    Ok(Part {
        vars,
        wrapper: Vec::new(),
    })
}

/// The fifo of one klon: the path `add` recorded in `<klon>/.klon/env`, else
/// the path of this host. The recorded value wins, so a klon keeps one store
/// even when the caller's `XDG_RUNTIME_DIR` differs from the one `add` saw.
fn fifo_for(envelope: &Envelope) -> PathBuf {
    match envelope.var(PATH_VAR).filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => path(),
    }
}

// --- The fifo ----------------------------------------------------------------

/// Create the fifo and fill it with `target` tokens, once per host. `mkfifo`
/// is atomic: of two concurrent calls exactly one creates the file, and only
/// that one fills it, so the store never holds twice the tokens.
fn ensure(path: &Path, target: usize) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        return if meta.file_type().is_fifo() {
            Ok(())
        } else {
            Err(Error::klon(format!(
                "{} is not a fifo; remove it and run the command again",
                path.display()
            )))
        };
    }
    let dir = path.parent().unwrap_or(path);
    fs::create_dir_all(dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let lock = Lock::acquire(dir)?;
    // Another klon may have created the fifo while this one waited.
    if path.exists() {
        return Ok(());
    }
    let c_path = c_path(path)?;
    // SAFETY: `c_path` is NUL-terminated and lives past the call.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        let err = std::io::Error::last_os_error();
        return Err(Error::io(format!("create the fifo {}", path.display()))(
            err,
        ));
    }
    let mut file = open_store(path)?;
    let written = file.write_all(&vec![TOKEN; target]);
    drop(file);
    drop(lock);
    written.map_err(Error::io(format!("fill {}", path.display())))
}

/// Open the fifo for reading and writing. `O_RDWR` on a fifo returns at once
/// even with no other end open, so klon needs no `O_NONBLOCK` dance and no
/// second process. `O_NONBLOCK` goes on afterwards, so a later read of an
/// empty store returns instead of waiting for a client to release a token.
fn open_store(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Error::io(format!("open {}", path.display())))?;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is open and owned by `file`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        // SAFETY: the same descriptor, with one flag added.
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    Ok(file)
}

/// Duplicate `fd` to the lowest free descriptor at 3 or above. `F_DUPFD`
/// clears `FD_CLOEXEC` on the copy, so the descriptor survives `exec`; the
/// floor of 3 keeps it away from the three standard streams, which the spawn
/// replaces. The explicit clear below states the promise the command depends
/// on.
fn inherit_dup(fd: RawFd, path: &Path) -> Result<RawFd> {
    // SAFETY: `fd` is open; `F_DUPFD` reads one integer argument.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
    if copy < 0 {
        let err = std::io::Error::last_os_error();
        return Err(Error::io(format!(
            "duplicate the descriptor of {}",
            path.display()
        ))(err));
    }
    // SAFETY: `copy` is a live descriptor of this process.
    let flags = unsafe { libc::fcntl(copy, libc::F_GETFD) };
    if flags >= 0 {
        // SAFETY: the same descriptor, with one flag removed.
        unsafe { libc::fcntl(copy, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    }
    Ok(copy)
}

/// The tokens the store holds right now. `FIONREAD` counts the bytes a read
/// would return and takes none of them, so `doctor` reports the count while a
/// klon builds.
fn available(file: &File) -> Result<usize> {
    let mut count: libc::c_int = 0;
    // SAFETY: the descriptor is open and `count` is a live, owned integer.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FIONREAD, &mut count) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        return Err(Error::io("count the jobserver tokens")(err));
    }
    Ok(usize::try_from(count).unwrap_or(0))
}

/// `FIONREAD` with the integer type each platform's `ioctl` takes.
#[cfg(target_os = "linux")]
const FIONREAD: libc::c_ulong = libc::FIONREAD as libc::c_ulong;
#[cfg(not(target_os = "linux"))]
const FIONREAD: libc::c_ulong = libc::FIONREAD;

// --- The top-up --------------------------------------------------------------

/// How hard `top_up` looks for a client that holds a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Scan the process table only when the store is short. `run` uses it: a
    /// full store needs no repair, so it needs no scan either.
    WhenShort,
    /// Always scan, so the report names the holders. `doctor` uses it.
    Always,
}

/// What klon found in the store and what it changed.
#[derive(Debug)]
pub struct Report {
    /// The fifo klon read.
    pub path: PathBuf,
    /// The token count klon aims for.
    pub target: usize,
    /// The tokens the store held when klon looked.
    pub available: usize,
    /// The processes of a klon that name this fifo. Each may hold a token.
    /// None when klon did not look, or when the host has no process scan.
    pub live: Option<usize>,
    /// The tokens klon wrote back.
    pub restored: usize,
    /// The tokens klon took out, because the target shrank.
    pub dropped: usize,
}

impl Report {
    /// The tokens the store lacks. A live client owns most of them.
    pub fn shortfall(&self) -> usize {
        self.target.saturating_sub(self.available)
    }

    /// The `doctor` row: the path, the count, the target, and the shortfall.
    pub fn detail(&self) -> String {
        let mut notes: Vec<String> = Vec::new();
        if self.restored > 0 {
            notes.push(format!("{} restored", self.restored));
        }
        if self.dropped > 0 {
            notes.push(format!("{} dropped", self.dropped));
        }
        if self.restored == 0 && self.dropped == 0 && self.shortfall() > 0 {
            notes.push(format!("{} short", self.shortfall()));
        }
        match self.live {
            Some(0) | None => {}
            Some(1) => notes.push("1 klon process holds the store".to_string()),
            Some(count) => notes.push(format!("{count} klon processes hold the store")),
        }
        if self.live.is_none() && self.shortfall() > 0 {
            notes.push("this system has no process scan, so klon changed nothing".to_string());
        }
        let head = format!(
            "{}: {} of {} tokens",
            self.path.display(),
            self.available,
            self.target
        );
        if notes.is_empty() {
            head
        } else {
            format!("{head}, {}", notes.join(", "))
        }
    }
}

/// Read the store and, when no klon holds a token, write it back to the
/// target. `doctor` prints the answer; `run` repairs the store in silence
/// before it starts a command.
pub fn top_up(look: Look) -> Result<Report> {
    let path = path();
    let target = target();
    ensure(&path, target)?;
    top_up_at(&path, target, look)
}

fn top_up_at(path: &Path, target: usize, look: Look) -> Result<Report> {
    let dir = path.parent().unwrap_or(path);
    let lock = Lock::acquire(dir)?;
    let mut file = open_store(path)?;
    let mut report = Report {
        path: path.to_path_buf(),
        target,
        available: available(&file)?,
        live: None,
        restored: 0,
        dropped: 0,
    };
    // A full store needs no repair. `run` then pays one `ioctl` and no scan of
    // the process table, which costs about 150 ms on a busy host.
    if look == Look::WhenShort && report.available == target {
        return Ok(report);
    }
    report.live = live_holders(path);
    // A token that a live client holds comes back when that client ends. Only
    // an idle store may be written, or the count would grow past the target.
    if report.live != Some(0) {
        return Ok(report);
    }
    if report.available < target {
        let missing = target - report.available;
        file.write_all(&vec![TOKEN; missing])
            .map_err(Error::io(format!(
                "write back the tokens of {}",
                path.display()
            )))?;
        report.restored = missing;
    } else if report.available > target {
        report.dropped = take(&mut file, report.available - target)?;
    }
    drop(file);
    drop(lock);
    Ok(report)
}

/// Take `count` tokens out of the store. The descriptor is non-blocking, so a
/// store that another process emptied first ends the loop instead of waiting.
fn take(file: &mut File, count: usize) -> Result<usize> {
    let mut buffer = vec![0u8; count];
    let mut taken = 0;
    while taken < count {
        match file.read(&mut buffer[taken..]) {
            Ok(0) => break,
            Ok(n) => taken += n,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(Error::io("take the extra jobserver tokens")(err)),
        }
    }
    Ok(taken)
}

/// The processes of a klon that name this fifo, from the `KLON_JOBSERVER` tag
/// that `run` puts in every command's environment. The answer is None on a
/// host with no process scan; klon then changes nothing, because a token that
/// a live client holds must never come back twice.
fn live_holders(path: &Path) -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let tags = vec![(PATH_VAR.to_string(), path.to_string_lossy().into_owned())];
        Some(crate::process::klon_processes(&tags).len())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

// --- make --------------------------------------------------------------------

/// The major and minor number of a `make --version` first line, for example
/// `GNU Make 4.3` to `(4, 3)`.
pub fn make_version_number(line: &str) -> Option<(u32, u32)> {
    for word in line.split_whitespace() {
        let digits: &str = word.trim_start_matches(|c: char| !c.is_ascii_digit());
        let Some((major, rest)) = digits.split_once('.') else {
            continue;
        };
        let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let (Ok(major), Ok(minor)) = (major.parse(), minor.parse()) {
            return Some((major, minor));
        }
    }
    None
}

/// True when this make treats a `fifo:` style `--jobserver-auth` as a fatal
/// error. GNU make learned that style in 4.4; every earlier version stops with
/// `internal error: invalid --jobserver-auth string` (handoff §11). klon emits
/// the pipe style for every version, so the answer only shapes the report.
pub fn make_needs_pipe_style(line: &str) -> bool {
    make_version_number(line).is_none_or(|version| version < (4, 4))
}

// --- The lock ----------------------------------------------------------------

/// An exclusive `flock` on `<dir>/jobserver.lock`. It holds for the fifo
/// creation and for one top-up, so two klons never fill the store twice. A
/// client never takes it: a build tool must not wait on klon to acquire a
/// token.
struct Lock {
    file: File,
}

impl Lock {
    fn acquire(dir: &Path) -> Result<Lock> {
        fs::create_dir_all(dir).map_err(Error::io(format!("create {}", dir.display())))?;
        let path = dir.join("jobserver.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(Error::io(format!("open {}", path.display())))?;
        loop {
            // SAFETY: the descriptor is open and owned by `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(Lock { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(Error::io(format!("lock {}", path.display()))(err));
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still open; the close below would release
        // the lock anyway.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// `path` as a NUL-terminated C string. A path with an interior NUL byte
/// cannot name a file, so it fails here.
fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::klon(format!("{} holds a NUL byte", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard that points the store at a private directory for one test.
    /// The variables are process-wide, so the tests below run under one lock.
    struct Runtime {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Runtime {
        fn new() -> Runtime {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().to_path_buf();
            Runtime { _tmp: tmp, dir }
        }

        fn fifo(&self) -> PathBuf {
            self.dir.join("jobserver")
        }
    }

    #[test]
    fn the_store_holds_the_target_and_a_second_call_leaves_it_alone() {
        let runtime = Runtime::new();
        let fifo = runtime.fifo();
        ensure(&fifo, 3).expect("create the store");
        let file = open_store(&fifo).expect("open");
        assert_eq!(available(&file).unwrap(), 3);
        drop(file);
        // A second call must not add a second set of tokens.
        ensure(&fifo, 3).expect("the second call");
        let file = open_store(&fifo).expect("open");
        assert_eq!(available(&file).unwrap(), 3);
    }

    #[test]
    fn a_path_that_is_not_a_fifo_is_an_error() {
        let runtime = Runtime::new();
        let fifo = runtime.fifo();
        fs::write(&fifo, "not a fifo").unwrap();
        let err = ensure(&fifo, 2).expect_err("a regular file must fail");
        assert!(err.to_string().contains("is not a fifo"), "{err}");
    }

    #[test]
    fn the_top_up_writes_back_a_lost_token_and_drops_an_extra_one() {
        let runtime = Runtime::new();
        let fifo = runtime.fifo();
        ensure(&fifo, 4).expect("create the store");

        // A client took two tokens and died with them.
        let mut file = open_store(&fifo).expect("open");
        assert_eq!(take(&mut file, 2).unwrap(), 2);
        drop(file);

        let report = top_up_at(&fifo, 4, Look::WhenShort).expect("top up");
        assert_eq!(report.available, 2);
        assert_eq!(report.shortfall(), 2);
        assert_eq!(report.restored, 2);
        let file = open_store(&fifo).expect("open");
        assert_eq!(available(&file).unwrap(), 4);
        drop(file);

        // A smaller target takes the extra tokens out again.
        let report = top_up_at(&fifo, 1, Look::Always).expect("top up");
        assert_eq!(report.available, 4);
        assert_eq!(report.dropped, 3);
        let file = open_store(&fifo).expect("open");
        assert_eq!(available(&file).unwrap(), 1);
    }

    #[test]
    fn a_full_store_needs_no_change() {
        let runtime = Runtime::new();
        let fifo = runtime.fifo();
        ensure(&fifo, 2).expect("create the store");
        let report = top_up_at(&fifo, 2, Look::WhenShort).expect("top up");
        assert_eq!(report.available, 2);
        assert_eq!(report.restored, 0);
        assert_eq!(report.dropped, 0);
        assert_eq!(report.shortfall(), 0);
        // The fast path skips the process scan.
        assert_eq!(report.live, None);
    }

    #[test]
    fn a_duplicated_descriptor_survives_exec_and_avoids_the_standard_streams() {
        let runtime = Runtime::new();
        let fifo = runtime.fifo();
        ensure(&fifo, 1).expect("create the store");
        let file = open_store(&fifo).expect("open");
        let copy = inherit_dup(file.as_raw_fd(), &fifo).expect("duplicate");
        assert!(copy >= 3, "the copy must not land on a standard stream");
        // SAFETY: `copy` is a live descriptor of this process.
        let flags = unsafe { libc::fcntl(copy, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "the copy must survive exec");
        // SAFETY: the test owns `copy` and closes it once.
        unsafe { libc::close(copy) };
    }

    #[test]
    fn the_report_names_the_path_the_count_and_the_shortfall() {
        let full = Report {
            path: PathBuf::from("/run/user/1000/klon/jobserver"),
            target: 18,
            available: 18,
            live: Some(0),
            restored: 0,
            dropped: 0,
        };
        assert_eq!(
            full.detail(),
            "/run/user/1000/klon/jobserver: 18 of 18 tokens"
        );
        let repaired = Report {
            available: 16,
            restored: 2,
            ..full
        };
        assert_eq!(
            repaired.detail(),
            "/run/user/1000/klon/jobserver: 16 of 18 tokens, 2 restored"
        );
        let busy = Report {
            path: PathBuf::from("/x/jobserver"),
            target: 18,
            available: 16,
            live: Some(2),
            restored: 0,
            dropped: 0,
        };
        assert_eq!(
            busy.detail(),
            "/x/jobserver: 16 of 18 tokens, 2 short, 2 klon processes hold the store"
        );
    }

    #[test]
    fn the_make_version_decides_the_note_not_the_handshake() {
        assert_eq!(make_version_number("GNU Make 4.3"), Some((4, 3)));
        assert_eq!(make_version_number("GNU Make 4.4.1"), Some((4, 4)));
        assert_eq!(make_version_number("GNU Make 3.81"), Some((3, 81)));
        assert_eq!(make_version_number("make without a number"), None);
        assert!(make_needs_pipe_style("GNU Make 4.3"));
        assert!(make_needs_pipe_style("GNU Make 3.81"));
        assert!(!make_needs_pipe_style("GNU Make 4.4.1"));
        // An unreadable version keeps the safe answer.
        assert!(make_needs_pipe_style("GNU Make"));
    }
}
