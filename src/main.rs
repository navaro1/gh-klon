//! `gh-klon`: a `git worktree` replacement that spawns a warm copy of a project.

mod backend;
mod branch;
mod cli;
mod config;
mod gh;
mod git;
mod journal;
mod paths;
mod probe;
mod process;
mod radar;
mod repair;
mod time;

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
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

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
    /// List every klon with its branch, HEAD, a dirty flag, and the radar columns.
    List,
    /// Report the host features and the open journal entries.
    Doctor(cli::doctor::Args),
    /// Bring a klon up to date. C24 ships the `--check` dry run only.
    Sync(cli::sync::Args),
    /// Open a pull request for a klon's branch with `gh pr create`.
    Pr(cli::pr::Args),
}

fn main() -> ExitCode {
    let Cli { yes, json, command } = Cli::parse();
    // `--json` is global, like `--yes`. `up`, `prune`, and `pr` print no klon
    // document, so the flag would promise one that never arrives. A later
    // chunk that gives a command a document deletes its name from this list.
    if json && matches!(command, Command::Up | Command::Prune | Command::Pr(_)) {
        eprintln!(
            "{}",
            Error::klon("--json is not available for up, prune, and pr")
        );
        return ExitCode::from(1);
    }
    let result = match command {
        Command::Add(args) => cli::add::run(args, json),
        Command::Up => cli::up::run(yes),
        Command::Rm(args) => cli::rm::run(args, json),
        Command::Prune => cli::prune::run(),
        Command::List => cli::list::run(json),
        Command::Doctor(args) => cli::doctor::run(args, json),
        Command::Sync(args) => cli::sync::run(args),
        Command::Pr(args) => cli::pr::run(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}
