//! The envelope (handoff §5): everything `run`, `shell`, `add -- cmd`, and
//! from C22 `up` puts around a command inside a tree.
//!
//! C16 builds the two parts that every host has: the environment contract in
//! `<klon>/.klon/env` and a new session for the whole command tree. The four
//! optional parts arrive one chunk at a time. Each of them fills one `Option`
//! field below and needs no other change here:
//!
//! | Field | Chunk | What it adds |
//! |---|---|---|
//! | `jobserver` | C17 | `MAKEFLAGS` and two inherited descriptors |
//! | `fence` | C18, C19 | Landlock in process, or a `sandbox-exec` wrapper |
//! | `scope` | C20 | a `systemd-run --user --scope` wrapper |
//! | `netns` | C23 | a `pasta --config-net` wrapper |

pub mod env;
#[cfg(target_os = "linux")]
pub mod fence_linux;
pub mod jobserver;
pub mod scope;
pub mod slots;

#[cfg(target_os = "linux")]
pub use fence_linux::Fence;
/// The macOS fence arrives with C19 as a `sandbox-exec` wrapper. Until then
/// nothing builds one, and the type only keeps the field on every system.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub struct Fence;

use crate::{Error, Result};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

/// One optional part of the envelope. A part exports variables, wraps the
/// command in another program, or both.
#[derive(Debug, Default)]
pub struct Part {
    /// Variables the part adds to the command's environment.
    pub vars: Vec<(String, String)>,
    /// Words placed in front of the command, for example
    /// `systemd-run --user --scope --`.
    pub wrapper: Vec<String>,
}

/// What a caller asks of the envelope beyond the parts every command gets.
/// C22 moved it here from `run`, because `up` and the later `merge` gate run
/// commands through the same spawn.
#[derive(Debug, Default)]
pub struct Options {
    /// Skip the write fence. `KLON_NO_FENCE=1` in the environment does the
    /// same, for a harness that already runs klon inside a sandbox.
    pub no_fence: bool,
    /// Where the command's stdout goes. `None` inherits, which is what `run`
    /// and `shell` want. `up` points a warm step at stderr under `--json`,
    /// because klon owns stdout for its document there.
    pub stdout: Option<Stdio>,
}

/// The envelope of one klon.
pub struct Envelope {
    /// The klon directory. Every command runs with this as its directory.
    pub klon: PathBuf,
    /// `KLON_NAME` from the env file: the branch of the klon.
    pub name: String,
    /// The variables of `<klon>/.klon/env`, in file order.
    pub vars: Vec<(String, String)>,
    /// C17 fills this.
    pub jobserver: Option<Part>,
    /// C18 fills this on Linux: a Landlock ruleset that the child applies in
    /// process, right before the exec. C19 fills it on macOS.
    pub fence: Option<Fence>,
    /// C20 fills this.
    pub scope: Option<Part>,
    /// C23 fills this when `--netns` is given.
    pub netns: Option<Part>,
}

impl Envelope {
    /// Read `<klon>/.klon/env`. A klon with no env file is an error, because
    /// every later part reads a value from that file.
    pub fn load(klon: &Path) -> Result<Envelope> {
        let vars = env::read(klon)?;
        let name = vars
            .iter()
            .find(|(key, _)| key == "KLON_NAME")
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                Error::klon(format!("{} holds no KLON_NAME", env::file(klon).display()))
            })?;
        Ok(Envelope {
            klon: klon.to_path_buf(),
            name,
            vars,
            jobserver: None,
            fence: None,
            scope: None,
            netns: None,
        })
    }

    /// The envelope of the main worktree, which has no env file. C22 needs it:
    /// `up` runs the approved `[warm] steps` in golden under the jobserver and
    /// the scope, and golden is the write target, so the caller turns the
    /// fence off through `Options`. The name stays empty, so the command
    /// carries no `KLON_` tag and `stop` never sees it. The variables hold
    /// only what `command` adds, which is `gc.auto=0`.
    pub fn for_golden(golden: &Path) -> Envelope {
        Envelope {
            klon: golden.to_path_buf(),
            name: String::new(),
            vars: Vec::new(),
            jobserver: None,
            fence: None,
            scope: None,
            netns: None,
        }
    }

    /// The value of one variable of the env file. C17 reads `KLON_JOBSERVER`
    /// and C18 reads `TMPDIR` through it.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The tags that mark a process as a member of this klon. `stop` and, from
    /// C30, `list` look for all of them in `/proc/<pid>/environ`.
    ///
    /// `KLON_ID` is the branch, which is what a person and a script read.
    /// `KLON_DIR` is the klon directory, which is unique on the host: two
    /// repositories can hold one branch name, and each also hands out
    /// `127.0.0.2`, so the branch and the address together still match the
    /// wrong tree. Only the directory makes `stop` exact. No build tool
    /// rewrites a `KLON_` variable, so neither tag is ever lost. A golden
    /// envelope (see `for_golden`) holds no name and gets no tag: a warm step
    /// of `up` is nobody's klon.
    pub fn tags(&self) -> Vec<(String, String)> {
        if self.name.is_empty() {
            return Vec::new();
        }
        vec![
            ("KLON_ID".to_string(), self.name.clone()),
            (
                "KLON_DIR".to_string(),
                self.klon.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// The parts in the order they wrap the command: the scope holds the
    /// namespace, and the namespace holds the command. The Linux fence is not
    /// a wrapper: the child applies it in process (see `command`). The
    /// jobserver adds no wrapper on either system.
    fn parts(&self) -> impl Iterator<Item = &Part> {
        [&self.scope, &self.netns, &self.jobserver]
            .into_iter()
            .flatten()
    }

    /// The command `argv` under this envelope. The child starts a new session,
    /// so `stop` finds the whole tree and C20 can put one cgroup around it.
    pub fn command(&self, argv: &[String]) -> Result<Command> {
        let (program, rest) = self.words(argv)?;
        let mut command = Command::new(program);
        command.args(rest);
        command.current_dir(&self.klon);
        // `run` never repacks. A repack would write outside the paths the C18
        // fence allows, and it would also cost the agent's build time.
        for (key, value) in env::with_git_config(&self.vars, &[("gc.auto", "0")]) {
            command.env(key, value);
        }
        for part in self.parts() {
            for (key, value) in &part.vars {
                command.env(key, value);
            }
        }
        for (key, value) in self.tags() {
            command.env(key, value);
        }
        // SAFETY: `setsid` is async-signal-safe, so it is legal between the
        // fork and the exec. It cannot fail here: the child of a fresh fork is
        // never a process group leader.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // The fence is the last step before the exec, so every wrapper and
        // the command itself run inside it (C18).
        #[cfg(target_os = "linux")]
        if let Some(fence) = &self.fence {
            let step = fence.child_step()?;
            // SAFETY: the step makes two syscalls and allocates nothing (see
            // `Fence::child_step`), so it is legal between the fork and the
            // exec.
            unsafe {
                command.pre_exec(step);
            }
        }
        Ok(command)
    }

    /// The whole word list: every wrapper in order, then `argv`.
    fn words(&self, argv: &[String]) -> Result<(String, Vec<String>)> {
        let mut words: Vec<String> = Vec::new();
        for part in self.parts() {
            words.extend(part.wrapper.iter().cloned());
        }
        words.extend(argv.iter().cloned());
        let (program, rest) = words
            .split_first()
            .ok_or_else(|| Error::klon("name a command after --"))?;
        Ok((program.clone(), rest.to_vec()))
    }

    /// Compose the whole envelope around `argv`, spawn the command, and wait
    /// for it to end. The answer is the command's exit status; the caller
    /// decides what a failure means. C22 moved the composition here from
    /// `run`, because `up` needs the same spawn for its warm steps, and C25
    /// (`merge`) will need it for its gate.
    ///
    /// The caller names what `root` is. A klon without its env file refuses
    /// the spawn: a command that runs there without the envelope would run
    /// without the fence, and klon never guesses its way into that state.
    pub fn spawn_and_wait(root: Root<'_>, argv: &[String], options: Options) -> Result<ExitStatus> {
        let (path, mut envelope) = match root {
            Root::Klon(klon) => (klon, Envelope::load(klon)?),
            Root::Golden(golden) => (golden, Envelope::for_golden(golden)),
        };
        // C20: the resource scope is the outermost wrapper, so it holds the
        // whole command tree. The guard removes a cgroup that klon made once
        // the command has left it.
        let guard = scope::apply(&mut envelope);
        // C17: the shared build-slot store. `attach` repairs a store that a
        // killed client left short, then hands the command the two
        // descriptors of the pipe-style handshake. klon opens the fifo here,
        // in the parent, so the command inherits the descriptors and never
        // opens the fifo under the fence below. It never fails: a host that
        // cannot hold a store gets the handshake variables as empty strings
        // instead.
        envelope.jobserver = jobserver::attach(&envelope);
        // C18: the fence is the innermost step; the child applies it right
        // before the exec, so the scope wrapper runs inside it too. A cgroup
        // the scope made is joined from inside the child, so the fence opens
        // its `cgroup.procs`.
        envelope.fence = fence(path, &envelope, options.no_fence, guard.cgroup())?;
        // The child leads a new session, so the terminal never signals it:
        // Ctrl-C and a `kill` of the wrapper reach only this process. klon
        // relays each of them to the child's process group, so the whole tree
        // ends with the wrapper instead of outliving it. The handlers go
        // back after the wait: `up` runs one step after another in this
        // process, and a signal between two steps must end `up`, not fall
        // into a handler that holds no child.
        let saved = relay_signals();
        let outcome = Self::spawn_child(&envelope, argv, options);
        restore_signals(&saved);
        outcome
    }

    /// Build the command, spawn it, and wait. The body of the spawn above
    /// without the signal setup, so the handlers come back on every path.
    fn spawn_child(envelope: &Envelope, argv: &[String], options: Options) -> Result<ExitStatus> {
        let mut command = envelope.command(argv)?;
        if let Some(stdout) = options.stdout {
            command.stdout(stdout);
        }
        let mut child = command
            .spawn()
            .map_err(|err| spawn_error(err, argv, envelope.fence.is_some()))?;
        CHILD.store(i32::try_from(child.id()).unwrap_or(0), Ordering::SeqCst);
        let status = child
            .wait()
            .map_err(Error::io(format!("wait for {}", argv.join(" "))))?;
        CHILD.store(0, Ordering::SeqCst);
        Ok(status)
    }
}

/// What the command runs in. The caller knows, and a wrong guess in either
/// direction costs a fence: a klon treated as golden runs without one, and
/// golden treated as a klon would fail its env read anyway.
pub enum Root<'a> {
    /// A klon. The spawn needs `<klon>/.klon/env`; a klon without it is
    /// damaged, and klon refuses instead of running the command without the
    /// fence and the tags.
    Klon(&'a Path),
    /// The main worktree. Golden has no env file, so the command carries no
    /// klon tags and no envelope variables, and the caller turns the fence
    /// off through `Options` when golden is the write target.
    Golden(&'a Path),
}

/// Where a step of `up` (C22) or of the `merge` gate (C25) writes its stdout.
/// Under `--json` klon owns that stream, and a step that prints one line would
/// put it in front of the document, so the step writes to stderr instead.
pub fn step_stdout(json: bool) -> Result<Option<Stdio>> {
    if !json {
        return Ok(None);
    }
    let fd = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
        .map_err(Error::io("duplicate stderr for the step output"))?;
    Ok(Some(fd))
}

/// The fence of this run, or None when the caller or the environment turns
/// it off. A host without Landlock gives None too, with one line on stderr.
/// macOS has no fence until C19.
fn fence(
    root: &Path,
    envelope: &Envelope,
    no_fence: bool,
    cgroup: Option<&Path>,
) -> Result<Option<Fence>> {
    let skipped =
        std::env::var_os("KLON_NO_FENCE").is_some_and(|value| !value.is_empty() && value != "0");
    if no_fence || skipped {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    {
        crate::envelope::fence_linux::build(root, envelope.var("TMPDIR"), cgroup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, envelope, cgroup);
        Ok(None)
    }
}

/// The error of a failed spawn. Only an errno crosses the fork boundary, so
/// a failed fence step arrives as a bare code; the caller names the fence
/// when the code is one that `execve` never gives, and says how to run
/// without it.
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

/// Install the relay for every signal in `RELAYED` and give the previous
/// handlers back, so the caller can restore them after the child ends. A
/// signal that arrives before the spawn finds no child and does nothing; the
/// window is the few microseconds between the fork and the store above.
fn relay_signals() -> Vec<(libc::c_int, libc::sighandler_t)> {
    RELAYED
        .iter()
        .map(|&signal| {
            // The cast goes through a pointer: a direct cast of a function item to
            // an integer is a clippy error. SAFETY: `relay` is a plain function
            // with the C signature the call expects, and it touches only an atomic
            // and `killpg`.
            let handler = relay as *const () as libc::sighandler_t;
            let previous = unsafe { libc::signal(signal, handler) };
            (signal, previous)
        })
        .collect()
}

/// Put the handlers of the caller back. `signal` gave them to us, so they are
/// valid handler values again.
fn restore_signals(saved: &[(libc::c_int, libc::sighandler_t)]) {
    for &(signal, handler) in saved {
        // SAFETY: every value here came out of `libc::signal`, so it is a
        // handler the process held before klon replaced it.
        unsafe { libc::signal(signal, handler) };
    }
}

/// The exit code of a finished command. A command that a signal ended reports
/// `128 + signal`, the same as every shell.
pub fn exit_code(status: &ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(1);
    }
    match status.signal() {
        Some(signal) => u8::try_from(128 + signal).unwrap_or(1),
        None => 1,
    }
}
