//! The benchmark runner (spec §7 C8, R14; handoff §8).
//!
//! One run builds a fixture per profile, and drives every selected cell twice
//! over it: once with the klon backend that this host probes, and once with
//! plain `git worktree add`. The same runner drives both tools, with the same
//! fixture and the same commands, which is what makes the two numbers
//! comparable.
//!
//! Three rules shape the code:
//!
//! 1. Only the measured command sits inside the timer. The fixture build, the
//!    tree that a sample needs beforehand, the teardown, and the correctness
//!    check are all outside it.
//! 2. The samples of the two tools are interleaved in a random order, and the
//!    order is recorded. A cell that always ran klon first would hide drift.
//! 3. A correctness mismatch voids the timing of its record. A fast wrong
//!    answer is not a result.
//!
//! The klon `add` writes `core.checkStat=minimal` and `index.version=4` into
//! golden's configuration, so the baseline worktrees of the same fixture
//! inherit them. That is deliberate: it isolates what klon adds, the warm
//! ignored state and the copied index, from a configuration change that any
//! user of plain git could also make.

use super::fixture::{self, Fixture};
use super::manifest::{Action, Cell, Manifest};
use super::report::{self, Correctness, Environment, ManifestInfo, Record, Report, Skip, BASELINE};
use crate::{Error, Result};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// What the command line asked for.
pub struct Options {
    /// One cell name, or every cell that this host may run.
    pub cell: Option<String>,
    /// Use the release run counts.
    pub release: bool,
    /// The directory that holds the fixtures.
    pub bench_dir: PathBuf,
}

/// Which tool a record measures.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    /// `gh klon`, with the backend that this host probes.
    Klon,
    /// Plain `git worktree add` and `git worktree remove`.
    Baseline,
}

impl Tool {
    fn tag(self) -> &'static str {
        match self {
            Tool::Klon => "klon",
            Tool::Baseline => "base",
        }
    }
}

/// One measured sample: the primary series, plus the M4 steady calls.
struct Sample {
    primary_ms: f64,
    steady_ms: Vec<f64>,
}

/// Run the selected cells and build the report.
pub fn run(manifest: &Manifest, options: &Options) -> Result<Report> {
    let (cells, skipped) = select(manifest, options)?;
    let order_seed = order_seed();
    let drop = DropCaches::detect();
    let (warm_runs, cold_runs) = manifest.run_counts(options.release);
    fs::create_dir_all(&options.bench_dir).map_err(Error::io(format!(
        "create the bench directory {}",
        options.bench_dir.display()
    )))?;

    let mut records = Vec::new();
    // One fixture serves every cell of its profile, so a run of four cells
    // builds two repositories, not four.
    for name in profile_names(&cells) {
        let profile = manifest.profiles[&name];
        eprintln!(
            "klon: bench: building the {name} fixture ({} tracked files, {} ignored files)",
            profile.tracked_files, profile.ignored_files
        );
        let fixture = Fixture::build(&options.bench_dir, &name, manifest.seed, &profile)?;
        for cell in cells.iter().filter(|c| c.profile == name) {
            eprintln!("klon: bench: cell {}", cell.name);
            records.extend(run_cell(
                manifest, cell, &fixture, warm_runs, cold_runs, &drop, order_seed,
            )?);
        }
    }

    Ok(Report {
        schema: report::SCHEMA,
        timestamp: report::now(),
        release: options.release,
        smoke: manifest.smoke,
        manifest: ManifestInfo {
            version: manifest.version,
            path: super::manifest::PATH,
            seed: manifest.seed,
            warm_runs: manifest.runs.warm,
            cold_runs: manifest.runs.cold,
        },
        environment: Environment::read(&options.bench_dir, manifest.fixture_hash(), order_seed),
        records,
        skipped,
    })
}

/// The cells to run, and the cells that this host skips with a reason.
fn select(manifest: &Manifest, options: &Options) -> Result<(Vec<Cell>, Vec<Skip>)> {
    let wanted: Vec<Cell> = match &options.cell {
        Some(name) => vec![manifest.cell(name)?.clone()],
        None => manifest.cells.clone(),
    };
    let fixture = std::env::var("KLON_FIXTURE").unwrap_or_default();
    let mut run = Vec::new();
    let mut skipped = Vec::new();
    for cell in wanted {
        match &cell.requires_fixture {
            Some(required) if *required != fixture => skipped.push(Skip {
                cell: cell.name.clone(),
                reason: format!("the {} profile needs KLON_FIXTURE={required}", cell.profile),
            }),
            _ => run.push(cell),
        }
    }
    Ok((run, skipped))
}

/// The profile names of `cells`, each once, in the order they first appear.
fn profile_names(cells: &[Cell]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for cell in cells {
        if !names.contains(&cell.profile) {
            names.push(cell.profile.clone());
        }
    }
    names
}

/// One cell: the correctness check per tool, then the warm samples, then the
/// cold samples when this host can drop the page cache.
fn run_cell(
    manifest: &Manifest,
    cell: &Cell,
    fixture: &Fixture,
    warm_runs: u32,
    cold_runs: u32,
    drop: &DropCaches,
    order_seed: u64,
) -> Result<Vec<Record>> {
    let tools = [Tool::Klon, Tool::Baseline];
    // The verdict of a tool does not change between the warm and the cold
    // group, so one check per tool serves both records.
    let mut checked = Vec::new();
    for tool in tools {
        checked.push(verify(cell, fixture, tool)?);
    }

    let mut records = Vec::new();
    records.extend(group(
        manifest, cell, fixture, &checked, warm_runs, false, drop, order_seed,
    )?);
    if drop.available() {
        records.extend(group(
            manifest,
            cell,
            fixture,
            &checked,
            cold_runs,
            true,
            drop,
            order_seed.wrapping_add(1),
        )?);
    }
    Ok(records)
}

/// The verdict of one correctness check, with the backend that produced it.
struct Checked {
    tool: Tool,
    backend: String,
    correctness: Correctness,
}

/// One group of samples: `runs` samples per tool, interleaved in a random order.
#[allow(clippy::too_many_arguments)]
fn group(
    manifest: &Manifest,
    cell: &Cell,
    fixture: &Fixture,
    checked: &[Checked],
    runs: u32,
    cold: bool,
    drop: &DropCaches,
    order_seed: u64,
) -> Result<Vec<Record>> {
    // One work item per sample of every tool of this cell, then one shuffle.
    let mut plan: Vec<usize> = Vec::new();
    for (index, _) in checked.iter().enumerate() {
        plan.extend(std::iter::repeat_n(index, runs as usize));
    }
    shuffle(&mut plan, order_seed);

    let mut samples: Vec<Vec<Sample>> = checked.iter().map(|_| Vec::new()).collect();
    let mut orders: Vec<Vec<usize>> = checked.iter().map(|_| Vec::new()).collect();
    for (step, index) in plan.into_iter().enumerate() {
        if cold {
            drop.run()?;
        }
        samples[index].push(sample(
            cell,
            fixture,
            checked[index].tool,
            manifest.runs.steady_calls,
            step,
        )?);
        orders[index].push(step);
    }

    let mut records = Vec::new();
    for (index, check) in checked.iter().enumerate() {
        let taken = std::mem::take(&mut samples[index]);
        let mut record = Record {
            cell: cell.name.clone(),
            metric: cell.metric.clone(),
            profile: cell.profile.clone(),
            profile_shape: manifest.profile_of(cell),
            backend: check.backend.clone(),
            // C9 adds the hot spare. No v0 `add` uses one.
            spare: false,
            cold,
            cache_drop: drop.label(cold),
            timer: cell.timer.clone(),
            runs,
            order: std::mem::take(&mut orders[index]),
            samples_ms: taken.iter().map(|s| s.primary_ms).collect(),
            p50_ms: 0.0,
            p95_ms: 0.0,
            first_p50_ms: None,
            steady_p50_ms: None,
            steady_samples_ms: taken.iter().flat_map(|s| s.steady_ms.clone()).collect(),
            correctness: check.correctness.clone(),
            timing_valid: check.correctness.matched,
            pass_p50_ms: cell.pass_p50_ms,
            pass_steady_p50_ms: cell.pass_steady_p50_ms,
            pass: None,
        };
        record.summarize();
        records.push(record);
    }
    Ok(records)
}

/// One sample. Only the measured command is inside the timer.
fn sample(
    cell: &Cell,
    fixture: &Fixture,
    tool: Tool,
    steady_calls: u32,
    step: usize,
) -> Result<Sample> {
    let path = fixture
        .root()
        .join(format!("{}-{}-{step}", cell.name, tool.tag()));
    let golden = fixture.golden();
    match cell.action {
        Action::Add => {
            let primary_ms = timed(&mut create_command(tool, golden, &path))?.0;
            teardown(golden, &path)?;
            Ok(Sample {
                primary_ms,
                steady_ms: Vec::new(),
            })
        }
        Action::Status => {
            create(tool, golden, &path)?;
            let primary_ms = timed(&mut status_command(&path))?.0;
            let mut steady_ms = Vec::new();
            for _ in 0..steady_calls {
                steady_ms.push(timed(&mut status_command(&path))?.0);
            }
            teardown(golden, &path)?;
            Ok(Sample {
                primary_ms,
                steady_ms,
            })
        }
        Action::Rm => {
            create(tool, golden, &path)?;
            let primary_ms = timed(&mut remove_command(tool, golden, &path))?.0;
            // `klon rm` returns before the delete finishes. Wait for the
            // background process, so the next sample starts from a clean disk.
            drain_trash(golden)?;
            if path.exists() {
                teardown(golden, &path)?;
            }
            Ok(Sample {
                primary_ms,
                steady_ms: Vec::new(),
            })
        }
    }
}

// --- The measured commands -----------------------------------------------------

/// Run `command` and answer the milliseconds it took and its stdout. A failure
/// stops the run: a benchmark of a failing command measures nothing.
fn timed(command: &mut Command) -> Result<(f64, String)> {
    let started = Instant::now();
    let output = command.output().map_err(Error::io("run a bench command"))?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    if !output.status.success() {
        return Err(Error::klon(format!(
            "a bench command failed with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok((
        elapsed,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// The command that creates a tree at `path`.
fn create_command(tool: Tool, golden: &Path, path: &Path) -> Command {
    match tool {
        Tool::Klon => {
            let mut command = Command::new(klon_binary());
            command
                .current_dir(golden)
                .args(["add", "--json", fixture::BRANCH, "--path"])
                .arg(path);
            fixture::isolate(&mut command);
            command
        }
        Tool::Baseline => {
            let mut command = fixture::isolated_git(golden, &["worktree", "add"]);
            command.arg(path).arg(fixture::BRANCH);
            command
        }
    }
}

/// The command that removes the tree at `path`.
fn remove_command(tool: Tool, golden: &Path, path: &Path) -> Command {
    match tool {
        Tool::Klon => {
            let mut command = Command::new(klon_binary());
            command
                .current_dir(golden)
                .arg("rm")
                .arg("--path")
                .arg(path);
            fixture::isolate(&mut command);
            command
        }
        Tool::Baseline => {
            let mut command = fixture::isolated_git(golden, &["worktree", "remove", "--force"]);
            command.arg(path);
            command
        }
    }
}

fn status_command(path: &Path) -> Command {
    fixture::isolated_git(path, &["status", "--porcelain"])
}

/// Create a tree outside the timer. The answer is the backend that filled it.
fn create(tool: Tool, golden: &Path, path: &Path) -> Result<String> {
    let (_, stdout) = timed(&mut create_command(tool, golden, path))?;
    match tool {
        Tool::Klon => backend_of(&stdout),
        Tool::Baseline => Ok(BASELINE.to_string()),
    }
}

/// The `backend` field of an `add --json` document.
fn backend_of(stdout: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|err| Error::klon(format!("read the add report: {err}")))?;
    value["backend"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::klon("the add report has no backend field"))
}

/// The klon binary that the samples run. It is this process, so a bench never
/// measures a klon from PATH that a user built months ago.
fn klon_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gh-klon"))
}

/// Remove a tree between two samples. `git worktree remove` deletes it inline,
/// which is slow and correct; the timer is already closed.
fn teardown(golden: &Path, path: &Path) -> Result<()> {
    let text = path.to_str().unwrap_or_default();
    if fixture::git(golden, &["worktree", "remove", "--force", text]).is_err() {
        if path.exists() {
            fs::remove_dir_all(path).map_err(Error::io(format!("remove {}", path.display())))?;
        }
        fixture::git(golden, &["worktree", "prune"])?;
    }
    Ok(())
}

/// Wait for the background delete that `klon rm` started. Without the wait the
/// next sample competes with it for the disk, and the fixture directory grows
/// by one klon per sample.
fn drain_trash(golden: &Path) -> Result<()> {
    let trash = crate::paths::default_wt_root(golden).join(".trash");
    let deadline = Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let entries = match fs::read_dir(&trash) {
            Ok(entries) => entries.count(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io(format!("read {}", trash.display()))(err)),
        };
        if entries == 0 {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(Error::klon(format!(
                "the background delete of {} did not finish in 300 s",
                trash.display()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

// --- The correctness check -----------------------------------------------------

/// Build one tree, compare it with golden, and remove it again. The verdict
/// decides whether the timing of the cell counts (R14).
///
/// `KLON_BENCH_INJECT_MISMATCH=1` damages the tree first. It exists so that a
/// test can prove the void path works; klon never sets it itself.
fn verify(cell: &Cell, fixture: &Fixture, tool: Tool) -> Result<Checked> {
    let golden = fixture.golden();
    let path = fixture
        .root()
        .join(format!("verify-{}-{}", cell.name, tool.tag()));
    let backend = create(tool, golden, &path)?;
    if inject_mismatch() {
        inject(&path)?;
    }
    let correctness = check(golden, &path, tool);
    teardown(golden, &path)?;
    let correctness = correctness?;
    Ok(Checked {
        tool,
        backend,
        correctness,
    })
}

fn inject_mismatch() -> bool {
    std::env::var("KLON_BENCH_INJECT_MISMATCH").as_deref() == Ok("1")
}

/// Damage one file in `tree`: an ignored file when the tree has one, else a
/// tracked file. The first case fails the manifest test and the second fails
/// the `git status` test, so either tool can be voided.
fn inject(tree: &Path) -> Result<()> {
    let ignored = tree.join(fixture::IGNORED_DIR).join("o0.bin");
    let victim = if ignored.is_file() {
        ignored
    } else {
        first_file(tree)?.ok_or_else(|| Error::klon("the tree holds no file to damage"))?
    };
    let mut bytes = fs::read(&victim).map_err(Error::io("read the injected file"))?;
    bytes.push(b'!');
    fs::write(&victim, bytes).map_err(Error::io("write the injected file"))
}

/// The first regular file below `dir`, outside `.git`.
fn first_file(dir: &Path) -> Result<Option<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(Error::io(format!("read {}", dir.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n != ".git"))
        .collect();
    entries.sort();
    for path in entries {
        let meta = fs::symlink_metadata(&path).map_err(Error::io("stat a tree entry"))?;
        if meta.is_file() {
            return Ok(Some(path));
        }
        if meta.is_dir() {
            if let Some(found) = first_file(&path)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// The two manifest tests of a tree: the ignored directory against golden's,
/// and a clean `git status`.
fn check(golden: &Path, tree: &Path, tool: Tool) -> Result<Correctness> {
    let ignored_manifest = match tool {
        Tool::Klon => {
            let want = golden.join(fixture::IGNORED_DIR);
            let got = tree.join(fixture::IGNORED_DIR);
            match compare(&want, &got)? {
                None => "match".to_string(),
                Some(why) => format!("mismatch: {why}"),
            }
        }
        // Plain `git worktree add` copies no ignored state, so there is nothing
        // to compare. That absence is the point of the baseline, not a fault.
        Tool::Baseline => "not-applicable: the baseline copies no ignored state".to_string(),
    };
    let porcelain = fixture::git(tree, &["status", "--porcelain"])?;
    let status = if porcelain.trim().is_empty() {
        "clean".to_string()
    } else {
        format!("dirty: {}", porcelain.lines().next().unwrap_or("").trim())
    };
    Ok(Correctness {
        matched: !ignored_manifest.starts_with("mismatch") && status == "clean",
        ignored_manifest,
        status,
    })
}

/// One entry of a tree manifest.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    rel: PathBuf,
    kind: &'static str,
    size: u64,
    mode: u32,
    mtime: Option<SystemTime>,
    target: Option<PathBuf>,
    hash: String,
}

/// Compare two trees. The answer names the first difference, or None when they
/// agree. The comparison covers the type, the size, the mode, the mtime, the
/// symlink target, and a SHA-256 of the content: a warm build tree with wrong
/// mtimes would rebuild, so the time matters as much as the bytes.
fn compare(want: &Path, got: &Path) -> Result<Option<String>> {
    let left = manifest_of(want)?;
    let right = manifest_of(got)?;
    for (a, b) in left.iter().zip(right.iter()) {
        if a != b {
            return Ok(Some(format!("{} differs", a.rel.display())));
        }
    }
    match left.len().cmp(&right.len()) {
        std::cmp::Ordering::Greater => Ok(Some(format!(
            "{} is missing {} entries",
            got.display(),
            left.len() - right.len()
        ))),
        std::cmp::Ordering::Less => Ok(Some(format!(
            "{} holds {} extra entries",
            got.display(),
            right.len() - left.len()
        ))),
        std::cmp::Ordering::Equal => Ok(None),
    }
}

/// Every entry below `root`, sorted by path. A missing root gives no entry.
fn manifest_of(root: &Path) -> Result<Vec<Entry>> {
    let mut paths = Vec::new();
    collect(root, &mut paths)?;
    let mut entries: Vec<Entry> = paths
        .into_par_iter()
        .map(|path| entry_of(root, &path))
        .collect::<Result<Vec<Entry>>>()?;
    entries.sort();
    Ok(entries)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io(format!("read {}", dir.display()))(err)),
    };
    for entry in listing {
        let path = entry
            .map_err(Error::io(format!("read {}", dir.display())))?
            .path();
        out.push(path.clone());
        if fs::symlink_metadata(&path)
            .map_err(Error::io("stat a tree entry"))?
            .is_dir()
        {
            collect(&path, out)?;
        }
    }
    Ok(())
}

fn entry_of(root: &Path, path: &Path) -> Result<Entry> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).map_err(Error::io("stat a tree entry"))?;
    let kind = meta.file_type();
    let (name, target, hash) = if kind.is_symlink() {
        let target = fs::read_link(path).map_err(Error::io("read a symlink"))?;
        ("symlink", Some(target), String::new())
    } else if kind.is_dir() {
        ("dir", None, String::new())
    } else {
        let bytes = fs::read(path).map_err(Error::io(format!("read {}", path.display())))?;
        let digest = Sha256::digest(&bytes);
        (
            "file",
            None,
            digest[..8].iter().map(|b| format!("{b:02x}")).collect(),
        )
    };
    Ok(Entry {
        rel: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
        kind: name,
        size: if kind.is_file() { meta.len() } else { 0 },
        mode: meta.permissions().mode(),
        mtime: meta.modified().ok(),
        target,
        hash,
    })
}

// --- The page cache ------------------------------------------------------------

/// The cold-run helper. Dropping the page cache needs privileges that klon
/// never takes, so the user names a command that can do it. Without one the
/// cells are warm only (handoff §8).
struct DropCaches {
    command: Option<String>,
}

impl DropCaches {
    fn detect() -> DropCaches {
        let Ok(command) = std::env::var("KLON_BENCH_DROP_CACHES") else {
            return DropCaches { command: None };
        };
        if command.trim().is_empty() {
            return DropCaches { command: None };
        }
        // Prove the command works before a cold record promises a cold cache.
        match Command::new("sh").arg("-c").arg(&command).status() {
            Ok(status) if status.success() => DropCaches {
                command: Some(command),
            },
            Ok(status) => {
                eprintln!(
                    "klon: bench: KLON_BENCH_DROP_CACHES exited with {}; the cells are warm only",
                    status.code().unwrap_or(-1)
                );
                DropCaches { command: None }
            }
            Err(err) => {
                eprintln!(
                    "klon: bench: cannot run KLON_BENCH_DROP_CACHES: {err}; the cells are warm only"
                );
                DropCaches { command: None }
            }
        }
    }

    fn available(&self) -> bool {
        self.command.is_some()
    }

    /// What a record says about its page cache. A cold record on a host with a
    /// working drop command is `dropped`; a warm record is `warm`; every record
    /// of a host that cannot drop the cache is `warm-only`.
    fn label(&self, cold: bool) -> &'static str {
        match (self.available(), cold) {
            (false, _) => "warm-only",
            (true, true) => "dropped",
            (true, false) => "warm",
        }
    }

    /// Drop the page cache between two samples.
    fn run(&self) -> Result<()> {
        let Some(command) = &self.command else {
            return Ok(());
        };
        let status = Command::new("sh")
            .arg("-c")
            .arg(command)
            .status()
            .map_err(Error::io("run KLON_BENCH_DROP_CACHES"))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::klon(format!(
                "KLON_BENCH_DROP_CACHES exited with {}",
                status.code().unwrap_or(-1)
            )))
        }
    }
}

// --- The random run order ------------------------------------------------------

/// The seed of the run order. `KLON_BENCH_ORDER_SEED` repeats an order exactly;
/// without it the clock and the pid give a fresh one per run.
fn order_seed() -> u64 {
    if let Some(seed) = named_seed(std::env::var("KLON_BENCH_ORDER_SEED").ok()) {
        return seed;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32)
}

/// The seed that `KLON_BENCH_ORDER_SEED` names, when it names a number.
fn named_seed(named: Option<String>) -> Option<u64> {
    named?.trim().parse().ok()
}

/// A Fisher-Yates shuffle over the SplitMix64 stream of `seed`. klon carries no
/// random-number crate, and a benchmark order needs no cryptographic quality:
/// it needs to be unbiased and repeatable from a recorded seed.
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed;
    for i in (1..items.len()).rev() {
        let j = (fixture::splitmix64(&mut state) % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shuffle_keeps_every_item_and_two_seeds_differ() {
        let source: Vec<usize> = (0..20).collect();
        let mut first = source.clone();
        shuffle(&mut first, 1);
        let mut second = source.clone();
        shuffle(&mut second, 2);
        let mut again = source.clone();
        shuffle(&mut again, 1);

        let sorted = |items: &[usize]| {
            let mut copy = items.to_vec();
            copy.sort();
            copy
        };
        assert_eq!(sorted(&first), source, "the shuffle must keep every item");
        assert_eq!(sorted(&second), source);
        assert_ne!(first, second, "two seeds must give two orders");
        assert_eq!(first, again, "one seed must repeat its order");
        assert_ne!(first, source, "the order must not stay the identity");
    }

    #[test]
    fn a_shuffle_of_one_item_or_none_is_safe() {
        let mut empty: Vec<usize> = Vec::new();
        shuffle(&mut empty, 5);
        let mut one = vec![9];
        shuffle(&mut one, 5);
        assert_eq!(one, [9]);
    }

    /// A named run order seed wins over the clock, so a run repeats exactly.
    #[test]
    fn a_named_order_seed_wins() {
        assert_eq!(named_seed(Some("4242".to_string())), Some(4242));
        assert_eq!(named_seed(Some(" 7 ".to_string())), Some(7));
        assert_eq!(named_seed(Some("not a number".to_string())), None);
        assert_eq!(named_seed(None), None);
        assert_ne!(order_seed(), 0, "the clock must give a seed");
    }

    #[test]
    fn the_backend_comes_from_the_add_report() {
        let stdout = r#"{"schema":"klon.add/1","path":"/x","backend":"copy"}"#;
        assert_eq!(backend_of(stdout).unwrap(), "copy");
        assert!(
            backend_of("{}").is_err(),
            "a report without a backend fails"
        );
    }

    /// Give every entry below `dir` one mtime. A write also re-times the
    /// directory that holds the file, and the manifest compares both.
    fn freeze(dir: &Path) {
        let time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let mut paths = Vec::new();
        collect(dir, &mut paths).unwrap();
        for path in paths {
            filetime::set_file_mtime(path, time).unwrap();
        }
    }

    /// Two equal trees compare equal; one damaged file names itself.
    #[test]
    fn the_manifest_test_finds_a_damaged_file() {
        let tmp = tempfile::tempdir().unwrap();
        let want = tmp.path().join("want");
        let got = tmp.path().join("got");
        for dir in [&want, &got] {
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(dir.join("a.bin"), b"aaaa").unwrap();
            fs::write(dir.join("sub/b.bin"), b"bbbb").unwrap();
            freeze(dir);
        }
        assert_eq!(compare(&want, &got).unwrap(), None);

        // The same size and a new content: only the hash catches it.
        fs::write(got.join("sub/b.bin"), b"bbbc").unwrap();
        freeze(&got);
        let why = compare(&want, &got).unwrap().expect("a difference");
        assert!(why.contains("b.bin"), "unexpected reason {why}");

        fs::remove_file(got.join("sub/b.bin")).unwrap();
        freeze(&got);
        let why = compare(&want, &got).unwrap().expect("a difference");
        assert!(why.contains("missing"), "unexpected reason {why}");
    }

    /// The mtime is part of the manifest: a warm build tree with new times
    /// would rebuild, so a re-timed copy is not a correct copy.
    #[test]
    fn the_manifest_test_finds_a_changed_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let want = tmp.path().join("want");
        let got = tmp.path().join("got");
        for dir in [&want, &got] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("a.bin"), b"aaaa").unwrap();
            freeze(dir);
        }
        assert_eq!(compare(&want, &got).unwrap(), None);
        filetime::set_file_mtime(
            got.join("a.bin"),
            filetime::FileTime::from_unix_time(1_800_000_000, 0),
        )
        .unwrap();
        let why = compare(&want, &got).unwrap().expect("a difference");
        assert!(why.contains("a.bin"), "unexpected reason {why}");
    }

    /// A missing directory is an empty manifest, not an error. The baseline
    /// tree has no ignored directory at all.
    #[test]
    fn a_missing_tree_gives_an_empty_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(manifest_of(&tmp.path().join("gone")).unwrap().is_empty());
    }

    /// Without `KLON_BENCH_DROP_CACHES` a run is warm only, and the label says
    /// so in every record.
    #[test]
    fn a_host_without_a_drop_command_is_warm_only() {
        let drop = DropCaches { command: None };
        assert!(!drop.available());
        assert_eq!(drop.label(false), "warm-only");
        assert_eq!(drop.label(true), "warm-only");
        assert!(drop.run().is_ok(), "a warm-only run must not fail");
    }
}
