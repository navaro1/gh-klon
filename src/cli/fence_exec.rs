//! The inner end of the `--netns` wrapper (C23): `gh-klon __fence-exec`
//! applies the write fence (C18) to the command that pasta starts, then
//! execs it.
//!
//! pasta cannot start under the fence: the kernel denies every
//! mount-topology syscall to a process inside a Landlock domain, and pasta's
//! own sandbox needs them (see `envelope::netns`). So the netns wrapper puts
//! this command after pasta's `--`, inside the namespace. It builds the same
//! ruleset as a run without `--netns` and applies it right before the exec.
//! A person never types it, and it cannot grant anything: it only restricts.

use crate::{Error, Result};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

#[derive(clap::Args)]
pub struct Args {
    /// The klon directory, for the fence's allow set.
    #[arg(value_name = "KLON")]
    pub klon: PathBuf,
    /// The scope cgroup (C20) the fence may open for its join rule.
    #[arg(long, value_name = "DIR")]
    pub cgroup: Option<PathBuf>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn run(args: Args) -> Result<()> {
    let tmpdir = std::env::var("TMPDIR").ok();
    #[cfg(target_os = "linux")]
    let step = {
        let fence = crate::envelope::fence_linux::build(
            &args.klon,
            tmpdir.as_deref(),
            args.cgroup.as_deref(),
        )?;
        match &fence {
            Some(fence) => Some(fence.child_step()?),
            None => None,
        }
    };
    #[cfg(not(target_os = "linux"))]
    let _ = tmpdir;
    let (program, rest) = args
        .command
        .split_first()
        .ok_or_else(|| Error::klon("name a command after --"))?;
    let mut command = Command::new(program);
    command.args(rest);
    command.current_dir(&args.klon);
    #[cfg(target_os = "linux")]
    if let Some(step) = step {
        // SAFETY: the step makes two syscalls and allocates nothing (see
        // `Fence::child_step`), so it is legal between the fork and the exec.
        unsafe {
            command.pre_exec(step);
        }
    }
    // `exec` replaces this process; the call only returns on failure.
    let err = command.exec();
    Err(Error::io(format!("exec {}", args.command.join(" ")))(err))
}
