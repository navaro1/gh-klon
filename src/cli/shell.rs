//! `gh klon shell (<branch> | --path <p>)`: an interactive shell inside a klon.
//! It is `run` with `$SHELL`, so the shell gets the same envelope as any other
//! command: the klon's `TMPDIR`, its loopback address, and its `KLON_ID` tag.

use crate::cli::run as runner;
use crate::envelope::Options;
use crate::Result;
use std::path::PathBuf;

/// The shell klon starts when `$SHELL` is unset or empty. Every POSIX system
/// has it.
const DEFAULT_SHELL: &str = "/bin/sh";

#[derive(clap::Args)]
pub struct Args {
    /// A branch that a klon has checked out.
    pub branch: Option<String>,
    /// The klon path. It must match a registered worktree.
    #[arg(long, conflicts_with = "branch")]
    pub path: Option<PathBuf>,
    /// Start the shell without the write fence.
    #[arg(long)]
    pub no_fence: bool,
    /// Wrap the shell in a pasta network namespace, like `run --netns`.
    #[arg(long)]
    pub netns: bool,
    /// The TCP ports pasta maps from the klon's loopback address into the
    /// namespace. Implies `--netns`. The default is `3000,5173,8000,8080`,
    /// or `[netns] ports` in `.klon.toml`.
    #[arg(long, value_name = "PORTS", value_delimiter = ',')]
    pub netns_ports: Option<Vec<u16>>,
}

pub fn run(args: Args) -> Result<()> {
    let klon = runner::resolve(args.branch.as_deref(), args.path.as_deref())?;
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SHELL.to_string());
    let netns = runner::netns_arg(args.netns, args.netns_ports.as_deref())?;
    runner::exec_with(
        &klon,
        &[shell],
        Options {
            no_fence: args.no_fence,
            stdout: None,
            netns,
        },
    )
}
