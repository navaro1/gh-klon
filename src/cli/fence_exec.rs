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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(clap::Args)]
pub struct Args {
    /// The klon directory, for the fence's allow set.
    #[arg(value_name = "KLON")]
    pub klon: PathBuf,
    /// The scope cgroup (C20) the fence may open for its join rule.
    #[arg(long, value_name = "DIR")]
    pub cgroup: Option<PathBuf>,
    /// The loopback resolver address to rescue, for example `127.0.0.53:53`.
    /// A namespace cannot reach a loopback resolver of the host, so this
    /// listener starts inside the namespace before the command.
    #[arg(long, value_name = "ADDR")]
    pub dns_rescue: Option<std::net::SocketAddr>,
    /// The upstream resolvers of the rescue listener, in try order.
    #[arg(long, value_delimiter = ',', value_name = "ADDRS")]
    pub dns_upstream: Vec<String>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Start the DNS rescue listener bound to `bind` inside this (already new)
/// network namespace. The child dies with this process: `PR_SET_PDEATHSIG`
/// kills it when the parent exits, and the parent's pid survives the exec of
/// the command, so the listener stops exactly when the command stops. The
/// listener runs unfenced, like pasta: it is plumbing, not the command.
fn spawn_dns_rescue(exe: &Path, bind: &std::net::SocketAddr, upstreams: &[String]) {
    if upstreams.is_empty() {
        return;
    }
    let mut child = Command::new(exe);
    child
        .args(["__dns-forward", "--bind"])
        .arg(bind.to_string())
        .args(upstreams)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let expected_parent = std::process::id();
    unsafe {
        child.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if libc::getppid() as u32 != expected_parent {
                // The parent already died between the fork and the prctl.
                libc::kill(std::process::id() as i32, libc::SIGKILL);
            }
            Ok(())
        });
    }
    if let Err(err) = child.spawn() {
        eprintln!("klon: cannot start the DNS rescue listener: {err}");
    }
}

pub fn run(args: Args) -> Result<()> {
    if let (Some(bind), true) = (args.dns_rescue, !args.dns_upstream.is_empty()) {
        let exe = std::env::current_exe().map_err(Error::io("find the klon binary"))?;
        spawn_dns_rescue(&exe, &bind, &args.dns_upstream);
    }
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
