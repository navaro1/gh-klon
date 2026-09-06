//! `gh klon run (<branch> | --path <p>) -- <cmd...>`: run a command inside a
//! klon under the envelope (handoff §5, R21).
//!
//! The command starts in its own session with the klon's environment, its own
//! `TMPDIR`, its own loopback address, and `gc.auto=0`. It carries `KLON_ID`
//! and `KLON_DIR`, so `stop` finds the whole tree. On Linux it runs inside
//! the write fence (C18, R17) unless `--no-fence` or `KLON_NO_FENCE=1` says
//! otherwise. With `--netns` it also runs inside a pasta network namespace
//! (C23). The exit code passes back unchanged, and a signal to `run` passes
//! on to the command.
//!
//! C22 moved the composition of the envelope and the spawn into
//! `Envelope::spawn_and_wait`, which `up` now shares.

use crate::envelope::{exit_code, netns, Envelope, Options, Root};
use crate::{config, git, paths, Error, Result};
use std::path::{Path, PathBuf};

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
    /// Wrap the command in a pasta network namespace. A host without pasta
    /// gets one line and the command runs on the host network as before.
    #[arg(long)]
    pub netns: bool,
    /// The TCP ports pasta maps from the klon's loopback address into the
    /// namespace. Implies `--netns`. The default is `3000,5173,8000,8080`,
    /// or `[netns] ports` in `.klon.toml`.
    #[arg(long, value_name = "PORTS", value_delimiter = ',')]
    pub netns_ports: Option<Vec<u16>>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    pub command: Vec<String>,
}

pub fn run(args: Args) -> Result<()> {
    let klon = resolve(args.branch.as_deref(), args.path.as_deref())?;
    let netns = netns_arg(args.netns, args.netns_ports.as_deref())?;
    exec_with(
        &klon,
        &args.command,
        Options {
            no_fence: args.no_fence,
            stdout: None,
            netns,
        },
    )
}

/// The `netns` value of `Options`: `Some` holds the port list, and `None`
/// keeps the command on the host network. `--netns-ports` implies `--netns`.
pub fn netns_arg(netns: bool, flag_ports: Option<&[u16]>) -> Result<Option<Vec<u16>>> {
    if !netns && flag_ports.is_none() {
        return Ok(None);
    }
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    let from_config = config::load(&golden)?.netns.and_then(|table| table.ports);
    Ok(Some(netns::ports(flag_ports, from_config.as_deref())))
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
    let status = Envelope::spawn_and_wait(Root::Klon(klon), argv, options)?;
    match exit_code(&status) {
        0 => Ok(()),
        code => Err(Error::Exit(code)),
    }
}
