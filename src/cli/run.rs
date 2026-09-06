//! `gh klon run (<branch> | --path <p>) -- <cmd...>`: run a command inside a
//! klon under the envelope (handoff §5, R21).
//!
//! The command starts in its own session with the klon's environment, its own
//! `TMPDIR`, its own loopback address, and `gc.auto=0`. It carries `KLON_ID`
//! and `KLON_DIR`, so `stop` finds the whole tree. The exit code passes back
//! unchanged, and a signal to `run` passes on to the command.

use crate::envelope::{scope, Envelope};
use crate::{git, paths, Error, Result};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};

#[derive(clap::Args)]
pub struct Args {
    /// A branch that a klon has checked out.
    pub branch: Option<String>,
    /// The klon path. It must match a registered worktree.
    #[arg(long, conflicts_with = "branch")]
    pub path: Option<PathBuf>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn run(args: Args) -> Result<()> {
    let klon = resolve(args.branch.as_deref(), args.path.as_deref())?;
    exec(&klon, &args.command)
}

/// The klon directory for a branch or a path. The main worktree is not a klon,
/// so a branch that golden has checked out gives the "no klon" answer.
pub fn resolve(branch: Option<&str>, path: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    // The first entry is the main worktree. Every other entry is a klon.
    let klons = worktrees.iter().skip(1);
    match (branch, path) {
        (Some(branch), _) => {
            let full = format!("refs/heads/{branch}");
            klons
                .filter(|w| w.branch.as_deref() == Some(full.as_str()))
                .map(|w| paths::absolute(&w.path))
                .next()
                .unwrap_or_else(|| {
                    Err(Error::klon(format!(
                        "no klon has the branch {branch} checked out"
                    )))
                })
        }
        (None, Some(path)) => {
            let wanted = paths::absolute(path)?;
            for worktree in klons {
                if paths::absolute(&worktree.path).is_ok_and(|p| p == wanted) {
                    return Ok(wanted);
                }
            }
            Err(Error::klon(format!("no klon at {}", wanted.display())))
        }
        (None, None) => Err(Error::klon("name a branch or a path with --path")),
    }
}

/// Run `argv` inside `klon` under the envelope and pass the exit code back.
/// A command that fails gives `Error::Exit`, which prints nothing: the command
/// already reported its own failure on its own stderr.
pub fn exec(klon: &Path, argv: &[String]) -> Result<()> {
    let mut envelope = Envelope::load(klon)?;
    // C20: the resource scope is the outermost wrapper, so it holds the whole
    // command tree. The guard removes a cgroup that klon made once the command
    // has left it.
    let _scope = scope::apply(&mut envelope);
    // The child leads a new session, so the terminal never signals it: Ctrl-C
    // and a `kill` of `gh klon run` reach only this process. klon relays each
    // of them to the child's process group, so the whole tree ends with the
    // wrapper instead of outliving it.
    relay_signals();
    let mut child = envelope
        .command(argv)?
        .spawn()
        .map_err(Error::io(format!("run {}", argv.join(" "))))?;
    CHILD.store(i32::try_from(child.id()).unwrap_or(0), Ordering::SeqCst);
    let status = child
        .wait()
        .map_err(Error::io(format!("wait for {}", argv.join(" "))))?;
    CHILD.store(0, Ordering::SeqCst);
    match exit_code(&status) {
        0 => Ok(()),
        code => Err(Error::Exit(code)),
    }
}

/// The process group of the running child, or 0 when none runs. The child
/// calls `setsid`, so its process group id equals its process id.
static CHILD: AtomicI32 = AtomicI32::new(0);

/// The signals a person or a supervisor sends to end a command.
const RELAYED: &[libc::c_int] = &[libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// Send `signal` on to the child's process group.
extern "C" fn relay(signal: libc::c_int) {
    let group = CHILD.load(Ordering::SeqCst);
    if group > 0 {
        // SAFETY: `killpg` is async-signal-safe, so it is legal in a handler.
        // A group that already left gives ESRCH, which the handler ignores.
        unsafe { libc::killpg(group, signal) };
    }
}

/// Install the relay for every signal in `RELAYED`. A signal that arrives
/// before the spawn finds no child and does nothing; the window is the few
/// microseconds between the fork and the store above.
fn relay_signals() {
    for signal in RELAYED {
        // The cast goes through a pointer: a direct cast of a function item to
        // an integer is a clippy error. SAFETY: `relay` is a plain function
        // with the C signature the call expects, and it touches only an atomic
        // and `killpg`.
        let handler = relay as *const () as libc::sighandler_t;
        unsafe { libc::signal(*signal, handler) };
    }
}

/// The exit code of a finished command. A command that a signal ended reports
/// `128 + signal`, the same as every shell.
fn exit_code(status: &ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(1);
    }
    match status.signal() {
        Some(signal) => u8::try_from(128 + signal).unwrap_or(1),
        None => 1,
    }
}
