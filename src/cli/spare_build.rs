//! `gh-klon spare-build <golden>`: the hidden builder that `add`, `up`, and
//! `rm` start detached at low priority (spec §7 C9, R40). It takes the spare
//! lock without waiting and builds `../<repo>.wt/.spare` when none exists.

use crate::{spare, Result};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    /// The repository whose spare to build: golden, or any path inside it.
    pub golden: PathBuf,
    /// A klon that the spare just served (G1). The builder writes its
    /// untracked cache back complete with one forced `git status` before it
    /// clones, so `add` did not have to.
    #[arg(long)]
    pub warm_status: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<()> {
    spare::lower_priority();
    let outcome = spare::build(&args.golden, args.warm_status.as_deref())?;
    if crate::debug() {
        eprintln!("klon: debug: spare-build: {outcome:?}");
    }
    Ok(())
}
