//! `gh klon prune`: `git worktree prune`, a drained `.trash` directory, and
//! the C26 receipts that have aged out.
//!
//! A receipt is keyed by commit, so a repository collects one file per checked
//! commit and nothing ever replaces an old one. Each file is a few hundred
//! bytes, so klon keeps a month of them (`receipt::MAX_AGE`) and drops the
//! rest here. The age comes from the file's own timestamp: a receipt is
//! written once and never changed, so its mtime is the time it was made.

use crate::git;
use crate::paths;
use crate::process;
use crate::receipt;
use crate::Result;

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().map_err(crate::Error::io("read the current directory"))?;
    let golden = git::main_worktree(&cwd)?;
    git::run(&golden, &["worktree", "prune"])?;
    // A receipt directory klon cannot read costs one line inside `prune`,
    // never the command: a stale receipt harms nobody.
    if let Ok(common) = git::common_dir_of_main(&golden) {
        let removed = receipt::prune(&common, receipt::MAX_AGE);
        if removed > 0 {
            println!("removed {removed} receipts older than 30 days");
        }
    }
    process::drain_trash(&paths::default_wt_root(&golden).join(".trash"))
}
