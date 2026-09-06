//! `gh klon run (<branch> | --path <p>) -- <cmd...>`: run a command inside a
//! klon under the envelope (handoff §5, R21).
//!
//! The command starts in its own session with the klon's environment, its own
//! `TMPDIR`, its own loopback address, and `gc.auto=0`. It carries `KLON_ID`
//! and `KLON_DIR`, so `stop` finds the whole tree. On Linux it runs inside
//! the write fence (C18, R17) unless `--no-fence` or `KLON_NO_FENCE=1` says
//! otherwise. The exit code passes back unchanged, and a signal to `run`
//! passes on to the command.

use crate::envelope::{scope, Envelope, Fence};
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
    /// Run without the write fence. The command can then write wherever the
    /// user can, golden included.
    #[arg(long)]
    pub no_fence: bool,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// What a caller asks of the envelope beyond the parts every command gets.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// Skip the write fence. `KLON_NO_FENCE=1` in the environment does the
    /// same, for a harness that already runs klon inside a sandbox.
    pub no_fence: bool,
}

pub fn run(args: Args) -> Result<()> {
    let klon = resolve(args.branch.as_deref(), args.path.as_deref())?;
    exec_with(
        &klon,
        &args.command,
        Options {
            no_fence: args.no_fence,
        },
    )
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

/// Run `argv` inside `klon` under the whole envelope and pass the exit code
/// back. `add -- cmd` and `shell` come through here.
pub fn exec(klon: &Path, argv: &[String]) -> Result<()> {
    exec_with(klon, argv, Options::default())
}

/// `exec` with the caller's options. A command that fails gives
/// `Error::Exit`, which prints nothing: the command already reported its own
/// failure on its own stderr.
pub fn exec_with(klon: &Path, argv: &[String], options: Options) -> Result<()> {
    let mut envelope = Envelope::load(klon)?;
    // C20: the resource scope is the outermost wrapper, so it holds the whole
    // command tree. The guard removes a cgroup that klon made once the command
    // has left it.
    let guard = scope::apply(&mut envelope);
    // C18: the fence is the innermost step; the child applies it right
    // before the exec, so the scope wrapper runs inside it too. A cgroup the
    // scope made is joined from inside the child, so the fence opens its
    // `cgroup.procs`.
    envelope.fence = fence(klon, &envelope, options, guard.cgroup())?;
    // The child leads a new session, so the terminal never signals it: Ctrl-C
    // and a `kill` of `gh klon run` reach only this process. klon relays each
    // of them to the child's process group, so the whole tree ends with the
    // wrapper instead of outliving it.
    relay_signals();
    let mut child = envelope
        .command(argv)?
        .spawn()
        .map_err(|err| spawn_error(err, argv, envelope.fence.is_some()))?;
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

/// The fence of this run, or None when the caller or the environment turns
/// it off. A host without Landlock gives None too, with one line on stderr.
/// macOS has no fence until C19.
fn fence(
    klon: &Path,
    envelope: &Envelope,
    options: Options,
    cgroup: Option<&Path>,
) -> Result<Option<Fence>> {
    let skipped =
        std::env::var_os("KLON_NO_FENCE").is_some_and(|value| !value.is_empty() && value != "0");
    if options.no_fence || skipped {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    {
        crate::envelope::fence_linux::build(klon, envelope.var("TMPDIR"), cgroup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (klon, envelope, cgroup);
        Ok(None)
    }
}

/// The error of a failed spawn. Only an errno crosses the fork boundary, so
/// a failed fence step arrives as a bare code; `run` names the fence when the
/// code is one that `execve` never gives, and says how to run without it.
fn spawn_error(err: std::io::Error, argv: &[String], fenced: bool) -> Error {
    let context = format!("run {}", argv.join(" "));
    #[cfg(target_os = "linux")]
    if fenced && crate::envelope::fence_linux::is_fence_errno(&err) {
        return Error::klon(format!(
            "{context}: the write fence did not apply ({err}); \
             pass --no-fence or set KLON_NO_FENCE=1 to run without it"
        ));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = fenced;
    Error::io(context)(err)
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
