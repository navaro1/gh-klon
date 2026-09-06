//! The envelope (handoff §5): everything `run`, `shell`, and `add -- cmd` put
//! around a command inside a klon.
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
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    /// The value of one variable of the env file. C17 reads `KLON_JOBSERVER`
    /// and C18 reads `TMPDIR` through it; C16 needs none of them.
    #[allow(dead_code)]
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
    /// rewrites a `KLON_` variable, so neither tag is ever lost.
    pub fn tags(&self) -> Vec<(String, String)> {
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
}
