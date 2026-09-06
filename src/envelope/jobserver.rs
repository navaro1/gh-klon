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
    // The env file still names a store. An empty value replaces it, so a
    // top-up never counts a command that holds no token as a token holder.
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
    // token back. Both end here. The repair stays silent: `run` gives stderr
    // to the command, not to klon.
    top_up_at(&anchor, &path, target, Look::WhenShort)?;

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

/// How hard `top_up` looks for a client that holds a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Scan the process table only when the store is short. `run` uses it: a
    /// full store needs no repair, so it needs no scan either.
    WhenShort,
    /// Always scan, so the report names the holders. `doctor` uses it.
    Always,
}

/// The processes that hold the store open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holders {
    /// klon did not look: the store was already full.
    NotChecked,
    /// The processes of a klon that name this store. Each may hold a token.
    Count(usize),
    /// This system has no process scan, so klon writes nothing. Only a host
    /// without `/proc` builds it, so the Linux compiler never sees it used.
    #[allow(dead_code)]
    Unknown,
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
            Holders::Count(0) => notes.push("no klon holds the store open".to_string()),
            Holders::Count(1) => notes.push("1 klon process holds the store".to_string()),
            Holders::Count(count) => {
                notes.push(format!("{count} klon processes hold the store"));
            }
            Holders::Unknown => {
                notes.push("this system has no process scan, so klon wrote nothing".to_string());
            }
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
pub fn top_up(look: Look) -> Result<Report> {
    let path = path();
    let target = target();
    ensure(&path)?;
    let anchor = open_store(&path)?;
    top_up_at(&anchor, &path, target, look)
}

/// The top-up on a store that `anchor` already holds open. Every caller must
/// pass a live descriptor: without one the fifo has no buffer, and every token
/// this call writes would be gone before the next call opens it.
fn top_up_at(anchor: &File, path: &Path, target: usize, look: Look) -> Result<Report> {
    let dir = path.parent().unwrap_or(path);
    let lock = Lock::acquire(dir)?;
    let mut report = Report {
        path: path.to_path_buf(),
        target,
        available: available(anchor)?,
        live: Holders::NotChecked,
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
    if report.live != Holders::Count(0) {
        return Ok(report);
    }
    // A second open file description on the same buffer. `O_NONBLOCK` on it
    // never reaches a client, and it keeps klon from waiting on a store that a
    // client emptied between the count above and the read below.
    let mut scratch = open_store_now(path)?;
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
    drop(scratch);
    drop(lock);
    Ok(report)
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

/// The processes of a klon that name this fifo, from the `KLON_JOBSERVER` tag
/// that `run` puts in every command's environment. The answer is `Unknown` on
/// a host with no process scan; klon then changes nothing, because a token
/// that a live client holds must never come back twice.
fn live_holders(path: &Path) -> Holders {
    #[cfg(target_os = "linux")]
    {
        let tags = vec![(PATH_VAR.to_string(), path.to_string_lossy().into_owned())];
        Holders::Count(crate::process::klon_processes(&tags).len())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Holders::Unknown
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

/// An exclusive `flock` on `<dir>/jobserver.lock`. It holds for one top-up, so
/// two klons that start together never both fill the store. A client never
/// takes it: a build tool must not wait on klon to acquire a token.
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
        let report = top_up_at(&store.anchor, &store.path, 3, Look::WhenShort).expect("top up");
        assert_eq!(report.available, 0);
        assert_eq!(report.restored, 3);
        assert_eq!(store.count(), 3);
        // A second call on a full store changes nothing and skips the scan.
        let report = top_up_at(&store.anchor, &store.path, 3, Look::WhenShort).expect("top up");
        assert_eq!(report.available, 3);
        assert_eq!(report.restored, 0);
        assert_eq!(report.live, Holders::NotChecked);
        assert_eq!(store.count(), 3);
    }

    #[test]
    fn the_tokens_die_with_the_last_descriptor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("jobserver");
        ensure(&path).expect("create the fifo");
        {
            let anchor = open_store(&path).expect("open");
            top_up_at(&anchor, &path, 2, Look::WhenShort).expect("top up");
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
        let store = Store::new();
        top_up_at(&store.anchor, &store.path, 4, Look::WhenShort).expect("fill");

        // A client took two tokens and died with them.
        let mut scratch = open_store_now(&store.path).expect("open");
        assert_eq!(take(&mut scratch, 2).unwrap(), 2);
        drop(scratch);
        assert_eq!(store.count(), 2);

        let report = top_up_at(&store.anchor, &store.path, 4, Look::WhenShort).expect("top up");
        assert_eq!(report.available, 2);
        assert_eq!(report.shortfall(), 2);
        assert_eq!(report.restored, 2);
        assert_eq!(store.count(), 4);

        // A smaller target takes the extra tokens out again.
        let report = top_up_at(&store.anchor, &store.path, 1, Look::Always).expect("top up");
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
        top_up_at(&store.anchor, &store.path, 2, Look::WhenShort).expect("fill");
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
            live: Holders::Count(0),
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
            live: Holders::Count(2),
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
