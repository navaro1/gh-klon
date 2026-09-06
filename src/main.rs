//! `gh-klon`: a `git worktree` replacement that spawns a warm copy of a project.

mod backend;
mod bench;
mod branch;
mod cli;
mod config;
mod envelope;
mod extras;
mod fixup;
mod gh;
mod git;
mod journal;
mod paths;
mod probe;
mod process;
mod radar;
mod repair;
mod spare;
mod time;
mod volume;

use clap::{Parser, Subcommand};
use std::fmt;
use std::process::ExitCode;

/// Every failure that a command can return.
#[derive(Debug)]
pub enum Error {
    /// A klon rule refused the request.
    Klon(String),
    /// A `git` subprocess failed. The stderr is passed through unchanged.
    Git { code: i32, stderr: String },
    /// A filesystem operation failed.
    Io {
        context: String,
        source: std::io::Error,
    },
    /// A command that `run` wrapped exited with this code. klon prints nothing:
    /// the command already reported its own failure on its own stderr.
    Exit(u8),
}

impl Error {
    pub fn klon(msg: impl Into<String>) -> Self {
        Error::Klon(msg.into())
    }

    pub fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Error::Io { context, source }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Error::Git { code, .. } => u8::try_from(*code).unwrap_or(1).max(1),
            Error::Exit(code) => (*code).max(1),
            _ => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Klon(msg) => write!(f, "klon: {msg}"),
            Error::Git { stderr, .. } => write!(f, "{}", stderr.trim_end()),
            Error::Io { context, source } => write!(f, "klon: {context}: {source}"),
            // The wrapped command owns the message.
            Error::Exit(_) => Ok(()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// True when `KLON_DEBUG=1` asks for the timing lines on stderr.
pub fn debug() -> bool {
    std::env::var("KLON_DEBUG").as_deref() == Ok("1")
}

#[derive(Parser)]
#[command(name = "gh-klon", version, about, long_about = None)]
struct Cli {
    /// Approve the commands in `.klon.toml` and skip every prompt.
    #[arg(long, global = true)]
    yes: bool,
    /// Print one JSON document on stdout instead of the human report. An error
    /// still goes to stderr as text and keeps the same exit code.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a linked worktree with a warm copy of golden's ignored files.
    Add(cli::add::Args),
    /// Run the approved `[warm] steps` in golden.
    Up,
    /// Remove a klon: rename it to .trash and delete it in the background.
    Rm(cli::rm::Args),
    /// Drop stale worktree admin entries and drain the .trash directory.
    Prune,
    /// List every klon with its branch, HEAD, a dirty flag, the cost and PR
    /// columns, and the radar columns.
    List(cli::list::Args),
    /// Report the host features and the open journal entries.
    Doctor(cli::doctor::Args),
    /// Convert golden into a btrfs subvolume, or move it onto a btrfs loop volume.
    Init(cli::init::Args),
    /// Bring a klon up to date. C24 ships the `--check` dry run only.
    Sync(cli::sync::Args),
    /// Open a pull request for a klon's branch with `gh pr create`.
    Pr(cli::pr::Args),
    /// Run a command inside a klon under the envelope.
    Run(cli::run::Args),
    /// Start $SHELL inside a klon under the envelope.
    Shell(cli::shell::Args),
    /// End every process of a klon: SIGTERM, then SIGKILL after 3 s.
    Stop(cli::stop::Args),
    /// Measure klon against a plain worktree on a generated fixture.
    Bench(cli::bench::Args),
    /// Build the hot spare of a repository. `add`, `up`, and `rm` start this
    /// detached; a user never needs it.
    #[command(hide = true)]
    SpareBuild(cli::spare_build::Args),
}

fn main() -> ExitCode {
    let Cli { yes, json, command } = Cli::parse();
    // `--json` is global, like `--yes`. `up`, `prune`, and `pr` print no klon
    // document, so the flag would promise one that never arrives. A later
    // chunk that gives a command a document deletes its name from this list.
    // `run` and `shell` hand stdout to the wrapped command, so klon must not
    // write a document of its own into that stream.
    if json
        && matches!(
            command,
            Command::Up
                | Command::Prune
                | Command::Pr(_)
                | Command::Run(_)
                | Command::Shell(_)
                | Command::SpareBuild(_)
        )
    {
        eprintln!(
            "{}",
            Error::klon("--json is not available for up, prune, pr, run, shell, and spare-build")
        );
        return ExitCode::from(1);
    }
    let result = match command {
        Command::Add(args) => cli::add::run(args, json),
        Command::Up => cli::up::run(yes),
        Command::Rm(args) => cli::rm::run(args, json),
        Command::Prune => cli::prune::run(),
        Command::List(args) => cli::list::run(args, json),
        Command::Doctor(args) => cli::doctor::run(args, json),
        Command::Init(args) => cli::init::run(args, yes, json),
        Command::Sync(args) => cli::sync::run(args),
        Command::Pr(args) => cli::pr::run(args),
        Command::Run(args) => cli::run::run(args),
        Command::Shell(args) => cli::shell::run(args),
        Command::Stop(args) => cli::stop::run(args, json),
        Command::Bench(args) => cli::bench::run(args, json),
        Command::SpareBuild(args) => cli::spare_build::run(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let text = err.to_string();
            if !text.is_empty() {
                eprintln!("{text}");
            }
            ExitCode::from(err.exit_code())
        }
    }
}
