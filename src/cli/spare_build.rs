//! `gh-klon spare-build <golden>`: the hidden builder that `add`, `up`, and
//! `rm` start detached at low priority (spec §7 C9, R40). It takes the spare
//! lock without waiting and builds `../<repo>.wt/.spare` when none exists.

use crate::{spare, Result};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    /// The repository whose spare to build: golden, or any path inside it.
    pub golden: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    spare::lower_priority();
    let outcome = spare::build(&args.golden)?;
    if crate::debug() {
        eprintln!("klon: debug: spare-build: {outcome:?}");
    }
    Ok(())
}
