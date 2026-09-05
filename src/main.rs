//! `gh-klon`: a `git worktree` replacement that spawns a warm copy of a project.

mod backend;
mod cli;
mod config;
mod git;
mod paths;

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a linked worktree with a warm copy of golden's ignored files.
    Add(cli::add::Args),
    /// Run the approved `[warm] steps` in golden.
    Up,
}

fn main() -> ExitCode {
    let Cli { yes, command } = Cli::parse();
    let result = match command {
        Command::Add(args) => cli::add::run(args),
        Command::Up => cli::up::run(yes),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}
