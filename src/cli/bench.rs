//! `gh klon bench [--cell <name>] [--json] [--release] [--out <dir>]`: the
//! benchmark from spec §7 C8 and handoff §8.
//!
//! The command builds its own fixture, measures the selected cells against the
//! `git worktree add` baseline, writes a result file, and prints a table. It
//! never touches the repository it runs in, apart from writing the result file
//! below `bench/results`.
//!
//! Environment:
//!
//! | Variable | Effect |
//! |---|---|
//! | `KLON_BENCH_DIR` | Where the fixture is built. Default: `$HOME/.cache/klon/bench` |
//! | `KLON_FIXTURE` | `100k` lets the 100k cells run. Without it they are skipped |
//! | `KLON_BENCH_RUNS` | Override the sample count of every record. For a smoke test |
//! | `KLON_BENCH_DROP_CACHES` | A shell command that drops the page cache. Without it the cells are warm only |
//! | `KLON_BENCH_ORDER_SEED` | Repeat one random run order |
//! | `KLON_BENCH_INJECT_MISMATCH` | Damage one file before the correctness check, to prove the void path |

use crate::bench::manifest::Manifest;
use crate::bench::report::Report;
use crate::bench::runner::{self, Options};
use crate::{Error, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Where a result file lands without `--out`, relative to the current directory.
const RESULTS: &str = "bench/results";

#[derive(clap::Args)]
pub struct Args {
    /// Measure one cell, for example `m1-add-10k`. Default: every cell that
    /// this host may run.
    #[arg(long)]
    pub cell: Option<String>,
    /// Use the release sample counts: 30 warm and 10 cold.
    #[arg(long)]
    pub release: bool,
    /// The directory for the result file. Default: `bench/results`.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn run(args: Args, json: bool) -> Result<()> {
    let manifest = Manifest::load()?;
    let options = Options {
        cell: args.cell,
        release: args.release,
        bench_dir: bench_dir()?,
    };
    let report = runner::run(&manifest, &options)?;
    let written = write_result(&report, args.out.as_deref())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
        // `--json` promises one document on stdout, so the file name goes to
        // stderr.
        eprintln!("klon: bench: wrote {}", written.display());
    } else {
        report.print_table();
        println!("wrote {}", written.display());
    }
    Ok(())
}

/// Where the fixture is built.
///
/// `$KLON_BENCH_DIR` wins. The default is `$HOME/.cache/klon/bench`, not the
/// system temporary directory: `gh` from the snap store cannot read `/tmp`, so
/// a `gh klon bench` under snap would fail to reach its own fixture there. A
/// host without `$HOME` falls back to the temporary directory.
fn bench_dir() -> Result<PathBuf> {
    resolve_bench_dir(std::env::var_os("KLON_BENCH_DIR"), std::env::var_os("HOME"))
}

fn resolve_bench_dir(named: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(dir) = named {
        return crate::paths::absolute(Path::new(&dir));
    }
    match home {
        Some(home) => Ok(Path::new(&home).join(".cache").join("klon").join("bench")),
        None => Ok(std::env::temp_dir().join("klon-bench")),
    }
}

/// Write `<dir>/<date>-<host>.json` and answer the path.
fn write_result(report: &Report, out: Option<&Path>) -> Result<PathBuf> {
    let dir = match out {
        Some(dir) => dir.to_path_buf(),
        None => PathBuf::from(RESULTS),
    };
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let path = dir.join(report.file_name());
    let text = serde_json::to_string_pretty(report)
        .map_err(|err| Error::klon(format!("serialize the report: {err}")))?;
    fs::write(&path, format!("{text}\n"))
        .map_err(Error::io(format!("write {}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default lands under `$HOME`, because snap `gh` cannot read `/tmp`.
    #[test]
    fn the_bench_directory_follows_the_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let named = resolve_bench_dir(Some(tmp.path().into()), Some("/home/x".into())).unwrap();
        assert_eq!(named, tmp.path().canonicalize().unwrap());

        let default = resolve_bench_dir(None, Some("/home/x".into())).unwrap();
        assert_eq!(default, Path::new("/home/x/.cache/klon/bench"));

        let homeless = resolve_bench_dir(None, None).unwrap();
        assert!(
            homeless.ends_with("klon-bench"),
            "found {}",
            homeless.display()
        );
    }
}
