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
//! ## The store lives as long as the klons do
//!
//! A fifo keeps its bytes in a kernel buffer that exists only while some
//! descriptor is open. The last close frees the buffer and every token in it,
//! and two open descriptions of one fifo share that single buffer (measured on
//! the development laptop). Three rules follow, and the whole module rests on
//! them:
//!
//! 1. `run` opens the store **before** it fills it, and keeps a copy of that
//!    descriptor open until the command ends. The tokens then belong to that
//!    run and to every klon that opens the store while it runs.
//! 2. A store that no klon holds open is empty. That is correct, not broken:
//!    with no client there is nothing to count.
//! 3. The lasting repair is therefore `run`'s work. `run` fills an idle store
//!    to the target. `doctor` reports what it finds and never writes a token
//!    that a live client holds, because that token comes back when the client
//!    ends.
//!
//! A client that a signal ends never writes its token back, so a store stays
//! short while its siblings keep it open. The next run of an idle store fills
//! it again, which closes the leak.
//!
//! ## How klon tells an idle store from a busy one
//!
//! `run` takes a **shared `flock`** on a marker file beside the fifo before it
//! hands the descriptors to the command, and passes that descriptor down as
//! well. The lock sits on the open file description, so the copies the command
//! inherits carry it, and the kernel drops it when the last of them closes. A
//! signal that ends the whole tree therefore releases the lock at the same
//! moment the fifo loses its buffer.
//!
//! A top-up asks the one question it needs with an **exclusive `flock` that
//! must succeed at once**. Success proves that no klon holds the store, so
//! klon may fill it. Any failure means a klon holds it, so klon writes
//! nothing. The test is exact and needs no process table, so macOS gets the
//! same answer as Linux.
//!
//! The marker is a plain file, not the fifo itself: `flock` on a fifo works on
//! Linux and fails with `ENOTSUP` on macOS (measured in CI).

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
/// person tunes the machine with it.
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
/// session owns, so nothing of the store outlives the session. A host without
/// that variable keeps the fifo under `~/.klon`, and a host without `HOME`
/// puts it in the temporary directory, one path per user id.
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
    let mut vars: Vec<(String, String)> = HANDSHAKE_VARS
        .iter()
        .map(|key| ((*key).to_string(), String::new()))
        .collect();
    // The env file still names a store that this command never opened. An
    // empty value replaces it, so nothing inside the klon reads a path that
    // its own descriptors do not match.
    vars.push((PATH_VAR.to_string(), String::new()));
    Part {
        vars,
        wrapper: Vec::new(),
    }
}

/// Open the store, fill it when it is idle, and build the handshake.
///
/// The order matters. The open comes first, because a fill before it would
/// vanish with the descriptor that carried it. The two copies stay open for
/// the life of this process on purpose: the command inherits them across
/// `exec`, they hold the store's buffer alive while the command runs, and
/// `run` exits as soon as the command does.
fn connect(envelope: &Envelope) -> Result<Part> {
    let path = fifo_for(envelope);
    let target = target();
    ensure(&path)?;
    let anchor = open_store(&path)?;
    // An idle store is empty, and a client that a signal ended never wrote its
    // token back. Both end here. The call also takes the shared lock that
    // marks the store as held for the life of this command. The repair stays
    // silent: `run` gives stderr to the command, not to klon.
    top_up_at(&anchor, &path, target, Job::Keep)?;

    let read = inherit_dup(anchor.as_raw_fd(), &path)?;
    let write = inherit_dup(anchor.as_raw_fd(), &path)?;
    drop(anchor);

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

/// Create the fifo, once per host. `mkfifo` is atomic, so of two concurrent
/// calls exactly one creates the file and the other reads `EEXIST`. The new
/// fifo holds no token: a fill here would die with the descriptor that wrote
/// it, so `connect` fills the store after it opens it.
fn ensure(path: &Path) -> Result<()> {
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
    let c_path = c_path(path)?;
    // SAFETY: `c_path` is NUL-terminated and lives past the call.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::AlreadyExists {
        return Ok(());
    }
    Err(Error::io(format!("create the fifo {}", path.display()))(
        err,
    ))
}

/// Open the fifo for reading and writing. `O_RDWR` on a fifo returns at once
/// even with no other end open, so klon needs no `O_NONBLOCK` dance and no
/// second process to hold the far end open.
///
/// The descriptor blocks, and it must: `F_DUPFD` shares one open file
/// description, so `O_NONBLOCK` on this descriptor would also reach the copies
/// the command inherits. GNU make reads the store with a blocking `read`, and
/// an `EAGAIN` there ends the build.
fn open_store(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Error::io(format!("open {}", path.display())))
}

/// `open_store` with `O_NONBLOCK`. klon reads and writes the store through it,
/// so a store that a client emptied first never makes klon wait. It is a
/// second open file description on the same buffer, and no copy of it ever
/// leaves the process, so the flag never reaches a client.
fn open_store_now(path: &Path) -> Result<File> {
    let file = open_store(path)?;
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

/// What the caller wants from the top-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// `run`: it keeps the store open for a command. It skips the probe when
    /// the store is already full, and it always marks the store as held.
    Keep,
    /// `doctor`: it reports only. It always probes and marks nothing.
    Look,
}

/// Whether a klon holds the store open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holders {
    /// klon did not probe: the store was already full.
    NotChecked,
    /// No klon holds the store open, so klon may fill it.
    Idle,
    /// A klon holds the store open. Its tokens come back when it ends, so
    /// klon writes none of them.
    Busy,
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
    /// The processes that hold the store open.
    pub live: Holders,
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
            Holders::NotChecked => {}
            // The count is 0 because a fifo drops its buffer with the last
            // descriptor, not because a token leaked. The note says so.
            Holders::Idle => notes.push("no klon holds the store open".to_string()),
            Holders::Busy => notes.push("a klon holds the store open".to_string()),
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

/// Read the store and, when no klon holds a token, fill it to the target.
/// `doctor` calls this.
///
/// The tokens this call writes live until the command ends, because a fifo
/// drops its buffer with its last descriptor. The lasting repair belongs to
/// `run`, which fills the store through the very descriptors it hands to the
/// command. `doctor` therefore reports a store that no klon holds open as
/// empty, and that is what it is.
pub fn top_up(job: Job) -> Result<Report> {
    let path = path();
    let target = target();
    ensure(&path)?;
    let anchor = open_store(&path)?;
    top_up_at(&anchor, &path, target, job)
}

/// The top-up on a store that `anchor` already holds open. Every caller must
/// pass a live descriptor: without one the fifo has no buffer, and every token
/// this call writes would be gone before the next call opens it.
fn top_up_at(anchor: &File, path: &Path, target: usize, job: Job) -> Result<Report> {
    let dir = path.parent().unwrap_or(path);
    // Every top-up runs alone, so two klons that start together never both
    // fill the store, and no klon can take the marker while another probes.
    let serial = Lock::acquire(dir)?;
    let mut report = Report {
        path: path.to_path_buf(),
        target,
        available: available(anchor)?,
        live: Holders::NotChecked,
        restored: 0,
        dropped: 0,
    };
    // A full store needs no repair, so `run` pays one `ioctl` and no probe.
    if job == Job::Keep && report.available == target {
        hold(dir)?;
        return Ok(report);
    }
    // A second open file description on the same buffer. `O_NONBLOCK` on it
    // never reaches a client, because no copy of it leaves this process.
    let mut scratch = open_store_now(path)?;
    report.live = probe(dir);
    if report.live == Holders::Idle {
        // No klon holds the store, and none can start: a new `run` must first
        // take the lock this call holds. Nothing can take or give a token now,
        // so the count is read again and no stale number reaches the write.
        report.available = available(&scratch)?;
        if report.available < target {
            let missing = target - report.available;
            scratch
                .write_all(&vec![TOKEN; missing])
                .map_err(Error::io(format!(
                    "write back the tokens of {}",
                    path.display()
                )))?;
            report.restored = missing;
        } else if report.available > target {
            report.dropped = take(&mut scratch, report.available - target)?;
        }
    }
    drop(scratch);
    if job == Job::Keep {
        hold(dir)?;
    }
    drop(serial);
    Ok(report)
}

/// `<dir>/jobserver.holders`: the file whose shared `flock` says that a klon
/// holds the store. It is a plain file, because `flock` on a fifo answers
/// `ENOTSUP` on macOS. It is never read or written; only its lock matters.
fn marker_path(dir: &Path) -> PathBuf {
    dir.join("jobserver.holders")
}

/// Open the marker. `append` and not `truncate`, so two klons that open it
/// together never fight over its length; the file stays empty for ever.
fn open_marker(dir: &Path) -> Result<File> {
    let path = marker_path(dir);
    OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .map_err(Error::io(format!("open {}", path.display())))
}

/// Ask whether any klon holds the store. An exclusive lock that the kernel
/// grants at once proves that no `run` holds its shared lock, so the store is
/// idle. Every other answer, a failure included, counts as busy: klon must
/// never write a token that a live client still owns. The lock releases at the
/// end of this call, when the file drops.
fn probe(dir: &Path) -> Holders {
    let Ok(marker) = open_marker(dir) else {
        return Holders::Busy;
    };
    // SAFETY: the descriptor is open and owned by `marker`.
    let rc = unsafe { libc::flock(marker.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Holders::Idle
    } else {
        Holders::Busy
    }
}

/// Mark the store as held for the life of this command.
///
/// The lock sits on the open file description, and the descriptor goes to the
/// command across `exec`, so the kernel drops the lock when the last process
/// of the tree closes it. A `kill -9` of the whole tree therefore releases the
/// marker at the same moment the fifo loses its buffer, and no stale marker
/// can outlive a klon. klon never closes the descriptor on purpose, exactly as
/// it never closes the two ends of the store.
///
/// The wait is bounded: the caller holds the serialization lock, and the only
/// other lock a klon takes on the marker is shared, which never conflicts.
fn hold(dir: &Path) -> Result<()> {
    let marker = open_marker(dir)?;
    let path = marker_path(dir);
    loop {
        // SAFETY: the descriptor is open and owned by `marker`.
        let rc = unsafe { libc::flock(marker.as_raw_fd(), libc::LOCK_SH) };
        if rc == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(Error::io(format!("hold {}", path.display()))(err));
        }
    }
    // The copy carries the lock and survives `exec`; the original closes here.
    let _kept = inherit_dup(marker.as_raw_fd(), &path)?;
    Ok(())
}

/// Take `count` tokens out of the store. The descriptor is non-blocking, so a
/// store that a client emptied first ends the loop instead of waiting.
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

/// An exclusive `flock` on `<dir>/jobserver.lock`. It holds for one top-up, so
/// two klons that start together never both fill the store and never race on
/// the marker. It is not the marker itself: a client never takes it, and a
/// build tool must not wait on klon to acquire a token.
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

    /// A fifo in a private directory, with one descriptor open. The descriptor
    /// is the anchor: without it the fifo holds no buffer and no token.
    struct Store {
        _tmp: tempfile::TempDir,
        path: PathBuf,
        anchor: File,
    }

    impl Store {
        fn new() -> Store {
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("jobserver");
            ensure(&path).expect("create the fifo");
            let anchor = open_store(&path).expect("open the fifo");
            Store {
                _tmp: tmp,
                path,
                anchor,
            }
        }

        fn count(&self) -> usize {
            available(&self.anchor).expect("count")
        }
    }

    #[test]
    fn a_new_store_is_empty_and_the_first_top_up_fills_it() {
        let store = Store::new();
        assert_eq!(store.count(), 0, "a fresh fifo holds no token");
        let report = top_up_at(&store.anchor, &store.path, 3, Job::Keep).expect("top up");
        assert_eq!(report.available, 0);
        assert_eq!(report.live, Holders::Idle);
        assert_eq!(report.restored, 3);
        assert_eq!(store.count(), 3);
        // A second call on a full store changes nothing and skips the probe.
        let report = top_up_at(&store.anchor, &store.path, 3, Job::Keep).expect("top up");
        assert_eq!(report.available, 3);
        assert_eq!(report.restored, 0);
        assert_eq!(report.live, Holders::NotChecked);
        assert_eq!(store.count(), 3);
    }

    /// The marker `run` takes is what tells a busy store from an idle one, on
    /// every system and with no process table. The lock lands on a plain file
    /// beside the fifo, because `flock` on a fifo fails on macOS.
    #[test]
    fn a_held_store_is_busy_and_the_top_up_writes_nothing() {
        let store = Store::new();
        // `Job::Keep` fills the store and takes the shared lock, as `run` does.
        top_up_at(&store.anchor, &store.path, 2, Job::Keep).expect("fill");
        // A client took both tokens and holds them.
        let mut scratch = open_store_now(&store.path).expect("open");
        assert_eq!(take(&mut scratch, 2).unwrap(), 2);
        drop(scratch);

        let report = top_up_at(&store.anchor, &store.path, 2, Job::Look).expect("report");
        assert_eq!(report.live, Holders::Busy, "the marker must be seen");
        assert_eq!(report.available, 0);
        assert_eq!(report.shortfall(), 2);
        assert_eq!(report.restored, 0, "a held store must not gain a token");
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn the_tokens_die_with_the_last_descriptor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("jobserver");
        ensure(&path).expect("create the fifo");
        {
            let anchor = open_store(&path).expect("open");
            top_up_at(&anchor, &path, 2, Job::Keep).expect("top up");
            assert_eq!(available(&anchor).unwrap(), 2);
        }
        // The rule the whole module rests on: no descriptor, no buffer, no
        // token. A store that no klon holds open is empty by construction.
        let anchor = open_store(&path).expect("reopen");
        assert_eq!(available(&anchor).unwrap(), 0);
    }

    #[test]
    fn a_path_that_is_not_a_fifo_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("jobserver");
        fs::write(&path, "not a fifo").unwrap();
        let err = ensure(&path).expect_err("a regular file must fail");
        assert!(err.to_string().contains("is not a fifo"), "{err}");
    }

    #[test]
    fn the_top_up_writes_back_a_lost_token_and_drops_an_extra_one() {
        // `Job::Look` throughout: this test plays `doctor` on an idle store,
        // so no call takes the marker that would make the next call see a
        // busy store.
        let store = Store::new();
        top_up_at(&store.anchor, &store.path, 4, Job::Look).expect("fill");

        // A client took two tokens and died with them.
        let mut scratch = open_store_now(&store.path).expect("open");
        assert_eq!(take(&mut scratch, 2).unwrap(), 2);
        drop(scratch);
        assert_eq!(store.count(), 2);

        let report = top_up_at(&store.anchor, &store.path, 4, Job::Look).expect("top up");
        assert_eq!(report.available, 2);
        assert_eq!(report.shortfall(), 2);
        assert_eq!(report.restored, 2);
        assert_eq!(store.count(), 4);

        // A smaller target takes the extra tokens out again.
        let report = top_up_at(&store.anchor, &store.path, 1, Job::Look).expect("top up");
        assert_eq!(report.available, 4);
        assert_eq!(report.dropped, 3);
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn a_duplicated_descriptor_survives_exec_and_avoids_the_standard_streams() {
        let store = Store::new();
        let copy = inherit_dup(store.anchor.as_raw_fd(), &store.path).expect("duplicate");
        assert!(copy >= 3, "the copy must not land on a standard stream");
        // SAFETY: `copy` is a live descriptor of this process.
        let flags = unsafe { libc::fcntl(copy, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "the copy must survive exec");
        // The copy shares the buffer, so it sees the tokens the anchor holds.
        top_up_at(&store.anchor, &store.path, 2, Job::Look).expect("fill");
        let mut count: libc::c_int = 0;
        // SAFETY: `copy` is open and `count` is a live, owned integer.
        unsafe { libc::ioctl(copy, FIONREAD, &mut count) };
        assert_eq!(count, 2);
        // SAFETY: the test owns `copy` and closes it once.
        unsafe { libc::close(copy) };
    }

    #[test]
    fn the_report_names_the_path_the_count_and_the_shortfall() {
        let full = Report {
            path: PathBuf::from("/run/user/1000/klon/jobserver"),
            target: 18,
            available: 18,
            live: Holders::NotChecked,
            restored: 0,
            dropped: 0,
        };
        assert_eq!(
            full.detail(),
            "/run/user/1000/klon/jobserver: 18 of 18 tokens"
        );
        let idle = Report {
            available: 0,
            restored: 18,
            live: Holders::Idle,
            ..full
        };
        assert_eq!(
            idle.detail(),
            "/run/user/1000/klon/jobserver: 0 of 18 tokens, 18 restored, \
             no klon holds the store open"
        );
        let busy = Report {
            path: PathBuf::from("/x/jobserver"),
            target: 18,
            available: 16,
            live: Holders::Busy,
            restored: 0,
            dropped: 0,
        };
        assert_eq!(
            busy.detail(),
            "/x/jobserver: 16 of 18 tokens, 2 short, a klon holds the store open"
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
