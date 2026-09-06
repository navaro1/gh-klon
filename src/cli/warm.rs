//! `gh-klon warm <klon> <golden>`: the detached second half of the `copy`
//! backend (R36, spec §7 C12).
//!
//! `add` starts it and returns. The command is hidden, because a person never
//! types it: it exists so the warm work runs in a process that outlives `add`.

use crate::{paths, warm, Result};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    /// The klon whose ignored directories are still missing.
    pub klon: PathBuf,
    /// The golden checkout the directories come from.
    pub golden: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    let klon = paths::absolute(&args.klon)?;
    let golden = paths::absolute(&args.golden)?;
    warm::run(&klon, &golden)
}
