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
    /// `gh klon add --no-spare`: a direct clone with the backend that this
    /// host probes.
    Klon,
    /// `gh klon add` with a hot spare ready before the timer starts (C9). The
    /// M1 budget of R12 binds this record.
    KlonSpare,
    /// Plain `git worktree add` and `git worktree remove`.
    Baseline,
}

impl Tool {
    fn tag(self) -> &'static str {
        match self {
            Tool::Klon => "klon",
            Tool::KlonSpare => "klon-spare",
            Tool::Baseline => "base",
        }
    }

    /// The tools of one cell. Only an `add` cell measures the spare: a status
    /// or a removal reads the same tree whichever way it was filled.
    fn for_cell(cell: &Cell) -> Vec<Tool> {
        match cell.action {
            Action::Add => vec![Tool::Klon, Tool::KlonSpare, Tool::Baseline],
            // The v2 cells measure the direct clone against the baseline. A
            // spare row would triple the cost of every one of them, and only
            // M2 would learn anything new; G1 opens that question.
            Action::Warm
            | Action::Build
            | Action::Status
            | Action::Disk
            | Action::Rm
            | Action::Throughput => vec![Tool::Klon, Tool::Baseline],
        }
    }

    /// True when this tool runs its command inside the klon envelope.
    fn under_envelope(self) -> bool {
        matches!(self, Tool::Klon | Tool::KlonSpare)
    }
}

/// One measured sample: the primary series, plus what the cell's metric needs.
#[derive(Default)]
struct Sample {
    primary_ms: f64,
    steady_ms: Vec<f64>,
    /// M2: whether the tree reached golden's ignored state.
    warm_reached: Option<bool>,
    /// M3: the units this build compiled.
    units: Option<u64>,
    /// M5: the unique bytes of the idle tree, and how they were measured.
    unique_bytes: Option<u64>,
    method: Option<&'static str>,
    /// M3 and M12: why a build failed, when one did.
    build_failure: Option<String>,
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
    // One fixture serves every cell that asks for the same recipe, so a run of
    // four synthetic cells builds two repositories, not four.
    for group in fixture_groups(manifest, &cells) {
        eprintln!("klon: bench: building the {} fixture", group.key);
        let fixture = Fixture::build(&options.bench_dir, &group.key, manifest.seed, &group.recipe)?;
        for index in group.cells {
            let cell = &cells[index];
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
///
/// Two reasons skip a cell: the big profile needs `KLON_FIXTURE=100k`, and an
/// ecosystem cell needs its tool. A missing tool is a fact about the host, so
/// the run reports it and carries on (spec §5).
fn select(manifest: &Manifest, options: &Options) -> Result<(Vec<Cell>, Vec<Skip>)> {
    let wanted: Vec<Cell> = match &options.cell {
        Some(name) => vec![manifest.cell(name)?.clone()],
        None => manifest.cells.clone(),
    };
    let fixture = std::env::var("KLON_FIXTURE").unwrap_or_default();
    let mut run = Vec::new();
    let mut skipped = Vec::new();
    for cell in wanted {
        if let Some(required) = &cell.requires_fixture {
            if *required != fixture {
                skipped.push(Skip {
                    cell: cell.name.clone(),
                    reason: format!("the {} profile needs KLON_FIXTURE={required}", cell.profile),
                });
                continue;
            }
        }
        match missing_tool(&cell) {
            Some(name) => skipped.push(Skip {
                cell: cell.name.clone(),
                reason: format!(
                    "a {} cell needs {name}, which is not on PATH",
                    cell.fixture.tag()
                ),
            }),
            None => run.push(cell),
        }
    }
    Ok((run, skipped))
}

/// The first program that `cell` needs and this host does not have.
fn missing_tool(cell: &Cell) -> Option<&'static str> {
    cell.fixture
        .tools()
        .iter()
        .find(|name| fixture::tool(name).is_none())
        .copied()
}

/// One generated repository and the cells that measure it.
struct FixtureGroup {
    key: String,
    recipe: fixture::Recipe,
    /// Indexes into the cell list.
    cells: Vec<usize>,
}

/// The fixtures that `cells` need, each once, in the order they first appear.
fn fixture_groups(manifest: &Manifest, cells: &[Cell]) -> Vec<FixtureGroup> {
    let mut groups: Vec<FixtureGroup> = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        let recipe = manifest.recipe_of(cell);
        match groups.iter_mut().find(|group| group.recipe == recipe) {
            Some(group) => group.cells.push(index),
            None => groups.push(FixtureGroup {
                key: recipe.key(&cell.profile),
                recipe,
                cells: vec![index],
            }),
        }
    }
    groups
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
    // The verdict of a tool does not change between the warm and the cold
    // group, so one check per tool serves both records.
    let mut checked = Vec::new();
    for tool in Tool::for_cell(cell) {
        checked.push(verify(cell, fixture, tool)?);
    }

    // M12 needs N trees alive at once, so it builds its own record instead of
    // going through the sample loop. It is a warm cell only: dropping the page
    // cache between six builds would measure the disk, not the contention.
    if cell.action == Action::Throughput {
        return throughput_cell(manifest, cell, fixture, &checked, drop, order_seed);
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
        samples[index].push(sample(
            cell,
            fixture,
            checked[index].tool,
            manifest.runs.steady_calls,
            step,
            cold.then_some(drop),
        )?);
        orders[index].push(step);
    }

    let mut records = Vec::new();
    for (index, check) in checked.iter().enumerate() {
        let taken = std::mem::take(&mut samples[index]);
        let mut record = blank_record(manifest, cell, check, cold, drop, runs);
        record.order = std::mem::take(&mut orders[index]);
        record.samples_ms = taken.iter().map(|s| s.primary_ms).collect();
        record.steady_samples_ms = taken.iter().flat_map(|s| s.steady_ms.clone()).collect();
        fill_metric(&mut record, &taken);
        record.summarize();
        records.push(record);
    }
    Ok(records)
}

/// A record with the fields that every cell fills. The caller adds the samples
/// and the metric of its own cell.
fn blank_record(
    manifest: &Manifest,
    cell: &Cell,
    check: &Checked,
    cold: bool,
    drop: &DropCaches,
    runs: u32,
) -> Record {
    Record {
        cell: cell.name.clone(),
        metric: cell.metric.clone(),
        profile: cell.profile.clone(),
        profile_shape: manifest.profile_of(cell),
        fixture: cell.fixture,
        fixture_shape: (cell.fixture == fixture::Kind::Rust).then(|| cell.shape()),
        backend: check.backend.clone(),
        spare: check.tool == Tool::KlonSpare,
        cold,
        cache_drop: drop.label(cold),
        timer: cell.timer.clone(),
        runs,
        order: Vec::new(),
        samples_ms: Vec::new(),
        p50_ms: 0.0,
        p95_ms: 0.0,
        first_p50_ms: None,
        steady_p50_ms: None,
        steady_samples_ms: Vec::new(),
        warm_reached: None,
        units_compiled: None,
        unique_bytes: None,
        method: None,
        ratio: None,
        t_solo_ms: None,
        t_wall6_ms: None,
        per_klon_ms: Vec::new(),
        builders: None,
        tokens: None,
        correctness: check.correctness.clone(),
        timing_valid: check.correctness.matched,
        pass_p50_ms: cell.pass_p50_ms,
        pass_steady_p50_ms: cell.pass_steady_p50_ms,
        pass_units_compiled: cell.pass_units_compiled,
        pass_ratio: cell.pass_ratio,
        pass: None,
    }
}

/// Fold what the samples measured into the record.
///
/// The unit count is the largest over the samples, not the median: one sample
/// that compiled a unit means the cell did not reach zero, and a median would
/// hide it. The unique-byte figure is the median, because every sample measured
/// one tree of the same shape and the median rejects a stray reading.
fn fill_metric(record: &mut Record, samples: &[Sample]) {
    if samples.iter().any(|s| s.warm_reached.is_some()) {
        record.warm_reached = Some(samples.iter().all(|s| s.warm_reached == Some(true)));
    }
    record.units_compiled = samples.iter().filter_map(|s| s.units).max();
    let bytes: Vec<f64> = samples
        .iter()
        .filter_map(|s| s.unique_bytes)
        .map(|n| n as f64)
        .collect();
    if !bytes.is_empty() {
        record.unique_bytes = Some(report::percentile(&bytes, 0.50) as u64);
        record.method = samples.iter().find_map(|s| s.method);
    }
    if let Some(why) = samples.iter().find_map(|s| s.build_failure.clone()) {
        record.correctness.build = format!("the build failed: {why}");
        record.correctness.matched = false;
        record.timing_valid = false;
    } else if samples.iter().any(|s| s.units.is_some()) {
        record.correctness.build = "ok".to_string();
    }
}

/// One sample. Only the measured command is inside the timer.
///
/// `drop` is Some for a cold sample. The page cache is dropped after the tree
/// that the sample needs exists and just before the timed command: a drop
/// before the setup would let the setup warm the cache again, and the record
/// would call a warm measurement cold.
fn sample(
    cell: &Cell,
    fixture: &Fixture,
    tool: Tool,
    steady_calls: u32,
    step: usize,
    drop: Option<&DropCaches>,
) -> Result<Sample> {
    let path = fixture
        .root()
        .join(format!("{}-{}-{step}", cell.name, tool.tag()));
    let golden = fixture.golden();
    let chill = || -> Result<()> {
        match drop {
            Some(drop) => drop.run(),
            None => Ok(()),
        }
    };
    match cell.action {
        Action::Add => {
            prepare(tool, golden)?;
            chill()?;
            let (primary_ms, stdout) =
                timed(&mut create_command(tool, golden, &path, fixture::BRANCH))?;
            check_spare(tool, &stdout)?;
            // The timer is closed. A detached warm that C12 started is still
            // running, so wait for it: the next sample must start on a quiet
            // disk, and the teardown must not delete a tree that a live process
            // is still writing into.
            wait_for_warm(&path)?;
            teardown(golden, &path)?;
            settle(tool, golden)?;
            Ok(Sample {
                primary_ms,
                ..Sample::default()
            })
        }
        Action::Warm => {
            chill()?;
            let sample = warm_sample(fixture, tool, &path)?;
            teardown(golden, &path)?;
            Ok(sample)
        }
        Action::Build => {
            create(tool, golden, &path)?;
            chill()?;
            let sample = build_sample(fixture, &path)?;
            teardown(golden, &path)?;
            Ok(sample)
        }
        Action::Disk => {
            create(tool, golden, &path)?;
            chill()?;
            // The tree is idle: nothing has run in it since `add` finished.
            let started = Instant::now();
            let usage = super::disk::measure(&path);
            let primary_ms = started.elapsed().as_secs_f64() * 1000.0;
            teardown(golden, &path)?;
            Ok(Sample {
                primary_ms,
                unique_bytes: Some(usage.bytes),
                method: Some(usage.method),
                ..Sample::default()
            })
        }
        // M12 needs N trees at once, which no single sample can hold. The
        // throughput path below builds its own record.
        Action::Throughput => Err(Error::klon(
            "an M12 cell does not run through the sample loop",
        )),
        Action::Status => {
            create(tool, golden, &path)?;
            chill()?;
            let primary_ms = timed(&mut status_command(&path))?.0;
            // The steady calls measure a warm index on purpose: M4 asks what a
            // later `git status` costs, not what a second cold one costs.
            let mut steady_ms = Vec::new();
            for _ in 0..steady_calls {
                steady_ms.push(timed(&mut status_command(&path))?.0);
            }
            teardown(golden, &path)?;
            Ok(Sample {
                primary_ms,
                steady_ms,
                ..Sample::default()
            })
        }
        Action::Rm => {
            create(tool, golden, &path)?;
            chill()?;
            let primary_ms = timed(&mut remove_command(tool, golden, &path))?.0;
            // `klon rm` returns before the delete finishes. Wait for the
            // background process, so the next sample starts from a clean disk.
            drain_trash(golden)?;
            // A removal that returns success and leaves the tree is a defect,
            // not a fast result. Stop instead of cleaning up after it.
            if path.exists() {
                return Err(Error::klon(format!(
                    "the measured removal left {} in place",
                    path.display()
                )));
            }
            Ok(Sample {
                primary_ms,
                ..Sample::default()
            })
        }
    }
}

// --- M12: build throughput at N builders ----------------------------------------

/// One finished build: how long it took and whether it worked.
struct Built {
    ms: f64,
    failure: Option<String>,
    /// True when the envelope told the command it had no build slots. The
    /// jobserver is what M12 exists to measure, so a build without slots makes
    /// the record say what it really measured.
    no_slots: bool,
}

/// The line the envelope prints when it could not hand the command the build
/// slots (`src/envelope/jobserver.rs`).
const NO_SLOTS: &str = "without build slots";

/// M12: `ratio = (builders × t_solo) / t_wall`.
///
/// `t_solo` is the median of `solo_runs` builds, each alone in a fresh tree.
/// `t_wall` is the wall time from the first start to the last finish of
/// `builders` builds that run at once, each in its own tree. A ratio of one
/// means that N builders together cost exactly N times one builder alone; the
/// pass rule of 0.80 allows a quarter of the time to contention (handoff §8).
///
/// Every klon build runs under `gh klon run`, so the jobserver, the resource
/// scope, and the write fence are all in the measurement. That is the point of
/// the cell: the envelope is what klon offers against six unbounded builds. The
/// baseline builds bare, which is what a `git worktree add` user gets.
///
/// The fixture of this cell has a cold golden, so every builder does the whole
/// compile. A warm golden would leave all six with nothing to do.
#[allow(clippy::too_many_arguments)]
fn throughput_cell(
    manifest: &Manifest,
    cell: &Cell,
    fixture: &Fixture,
    checked: &[Checked],
    drop: &DropCaches,
    order_seed: u64,
) -> Result<Vec<Record>> {
    let builders = manifest.builders(cell);
    let solo_runs = manifest.solo_runs();
    let golden = fixture.golden();
    // A phase of this cell takes minutes, so the tool that always went first
    // would meet a cooler machine than the one that always went second. The
    // recorded seed decides the phase order, exactly as it decides the sample
    // order of every other cell.
    let mut phases: Vec<usize> = (0..checked.len()).collect();
    shuffle(&mut phases, order_seed);
    let mut step = 0;
    let mut records: Vec<(usize, Record)> = Vec::new();
    for index in phases {
        let check = &checked[index];
        let tool = check.tool;
        eprintln!(
            "klon: bench: {}: {solo_runs} solo builds, then {builders} at once",
            tool.tag()
        );
        let mut solo: Vec<f64> = Vec::new();
        let mut order: Vec<usize> = Vec::new();
        let mut failure: Option<String> = None;
        let mut no_slots = false;
        for run in 0..solo_runs {
            let name = format!("{}-{}-solo{run}", cell.name, tool.tag());
            let built = one_build(fixture, tool, &name)?;
            solo.push(built.ms);
            no_slots |= built.no_slots;
            failure = failure.or(built.failure);
            order.push(step);
            step += 1;
        }

        // The concurrent run. Each builder needs a branch of its own: git
        // refuses two worktrees on one branch.
        let names: Vec<String> = (0..builders)
            .map(|i| format!("{}-{}-n{i}", cell.name, tool.tag()))
            .collect();
        let mut paths = Vec::new();
        for name in &names {
            fixture::git(golden, &["branch", "-f", name, fixture::BRANCH])?;
            let path = fixture.root().join(name);
            create_branch(tool, golden, &path, name)?;
            paths.push(path);
        }
        let (t_wall_ms, per_klon) = build_together(fixture, tool, &paths)?;
        for (path, name) in paths.iter().zip(&names) {
            teardown(golden, path)?;
            fixture::git(golden, &["branch", "-D", name])?;
        }
        no_slots |= per_klon.iter().any(|b| b.no_slots);
        failure = failure.or_else(|| per_klon.iter().find_map(|b| b.failure.clone()));

        let mut record = blank_record(manifest, cell, check, false, drop, solo_runs);
        record.order = order;
        record.samples_ms = solo;
        record.t_solo_ms = Some(report::percentile(&record.samples_ms, 0.50));
        record.t_wall6_ms = Some(t_wall_ms);
        record.per_klon_ms = per_klon.iter().map(|b| b.ms).collect();
        record.builders = Some(builders);
        record.tokens = tokens_in_effect(tool, no_slots);
        if let Some(why) = failure {
            record.correctness.build = format!("the build failed: {why}");
            record.correctness.matched = false;
            record.timing_valid = false;
        } else if tool.under_envelope() && record.tokens.is_none() {
            // The jobserver is what this cell measures. A run whose builds got
            // no slots measured six unbounded builds under another name, so its
            // timing must not stand.
            record.correctness.build =
                "the envelope handed the builds no jobserver slots, so the run \
                 did not measure what the cell claims"
                    .to_string();
            record.correctness.matched = false;
            record.timing_valid = false;
        } else {
            record.correctness.build = "ok".to_string();
        }
        record.summarize();
        records.push((index, record));
    }
    // The report lists the tools in the manifest order, whatever order they ran
    // in. `order` carries the order they ran in.
    records.sort_by_key(|(index, _)| *index);
    Ok(records.into_iter().map(|(_, record)| record).collect())
}

/// The build slots that an M12 phase really had.
///
/// The baseline runs no envelope, so it has none by design. A klon phase has
/// the jobserver target unless the user turned the store off or the envelope
/// could not open it; either way the answer is None, and the caller voids the
/// record rather than report a ratio the envelope did not produce.
fn tokens_in_effect(tool: Tool, no_slots: bool) -> Option<usize> {
    if !tool.under_envelope() || no_slots || crate::envelope::jobserver::is_off() {
        return None;
    }
    Some(crate::envelope::jobserver::target())
}

/// One build alone in a fresh tree, from create to teardown. Only the build is
/// timed.
fn one_build(fixture: &Fixture, tool: Tool, name: &str) -> Result<Built> {
    let golden = fixture.golden();
    let path = fixture.root().join(name);
    create(tool, golden, &path)?;
    let mut command = throughput_command(fixture, tool, &path)?;
    let started = Instant::now();
    let output = command
        .output()
        .map_err(Error::io("run the measured build"))?;
    let built = finished(started, &output);
    teardown(golden, &path)?;
    Ok(built)
}

/// Start one build per tree and wait for all of them. The answer is the wall
/// time of the whole run and the wall time of each build.
///
/// One thread per builder, so each build is timed from its own spawn to its own
/// exit. Reaping the children in order instead would charge every early
/// finisher with the wait for the slowest one.
fn build_together(fixture: &Fixture, tool: Tool, paths: &[PathBuf]) -> Result<(f64, Vec<Built>)> {
    let mut commands = Vec::new();
    for path in paths {
        commands.push(throughput_command(fixture, tool, path)?);
    }
    let started_all = Instant::now();
    let built: Vec<Result<Built>> = std::thread::scope(|scope| {
        let handles: Vec<_> = commands
            .iter_mut()
            .map(|command| {
                scope.spawn(|| {
                    let started = Instant::now();
                    let output = command
                        .output()
                        .map_err(Error::io("run a concurrent build"))?;
                    Ok(finished(started, &output))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(Error::klon("a builder thread panicked")))
            })
            .collect()
    });
    // Every builder has exited, so the elapsed time is the first start to the
    // last finish. The thread starts add a fraction of a millisecond to it,
    // which counts against klon and never for it.
    let t_wall_ms = started_all.elapsed().as_secs_f64() * 1000.0;
    Ok((
        t_wall_ms,
        built.into_iter().collect::<Result<Vec<Built>>>()?,
    ))
}

/// The verdict of one finished build.
fn finished(started: Instant, output: &std::process::Output) -> Built {
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let no_slots = text.contains(NO_SLOTS);
    if output.status.success() {
        return Built {
            ms,
            failure: None,
            no_slots,
        };
    }
    Built {
        ms,
        failure: Some(format!(
            "exit {}: {}",
            output.status.code().unwrap_or(-1),
            last_line(&text)
        )),
        no_slots,
    }
}

/// The build that one M12 builder runs. A klon build goes through `gh klon
/// run`, so the whole envelope is inside the timer; a baseline build runs bare.
fn throughput_command(fixture: &Fixture, tool: Tool, tree: &Path) -> Result<Command> {
    let kind = fixture.kind();
    let program = ecosystem_program(kind)?;
    let inner = fixture::build_command(kind, &program, tree, &fixture.store(), false);
    if !tool.under_envelope() {
        return Ok(inner);
    }
    let mut outer = Command::new(klon_binary());
    outer
        .current_dir(fixture.golden())
        .args(["run", "--path"])
        .arg(tree)
        .arg("--")
        .arg(inner.get_program())
        .args(inner.get_args());
    // The inner command cleared the leaking build variables and turned the
    // colour off. `run` passes its own environment on to the command, so the
    // same treatment on the wrapper reaches the build.
    for (key, value) in inner.get_envs() {
        match value {
            Some(value) => outer.env(key, value),
            None => outer.env_remove(key),
        };
    }
    fixture::isolate(&mut outer);
    Ok(outer)
}

// --- M2: the time to a warm tree ------------------------------------------------

/// The first wait between two looks at the ignored state.
const POLL_FIRST: std::time::Duration = std::time::Duration::from_millis(20);

/// The longest wait between two looks. The poll walks the ignored state, so a
/// fixed short interval would spend real CPU beside the copy it is timing. The
/// wait grows to this ceiling, which bounds the error of one sample.
const POLL_LAST: std::time::Duration = std::time::Duration::from_millis(250);

/// How long the M2 poll waits before it calls the warm state unreachable.
const WARM_DEADLINE: std::time::Duration = std::time::Duration::from_secs(900);

/// M2: the time from the start of `add` until the tree holds golden's ignored
/// state.
///
/// The timer starts before the child does and stops when the tree first holds
/// the same ignored state as golden. The measurement therefore holds whether
/// the copy runs inside `add` or behind it in the background: it asks when the
/// tree is usable, not when the command returned.
///
/// The poll is in three steps, because the full comparison hashes every byte
/// and a fixed 20 ms loop over a 2 GB state would cost more than the copy it
/// times:
///
/// 1. `warm::pending` must be empty. C12 lets a big ignored directory finish in
///    a detached process, and its marker names what the klon still waits for.
/// 2. Each look then compares a cheap fingerprint: the entry count and the
///    total apparent size. That is one `stat` per entry and no read.
/// 3. The first look whose fingerprint agrees stops the clock, and the full
///    byte-for-byte and time-for-time comparison then confirms it, outside the
///    timer. A fingerprint that agreed over wrong content fails that
///    confirmation, and the poll goes on with the clock still running.
///
/// The `add` process leaving is not the end of the poll. Since C12 it may
/// return long before the tree is warm, which is the whole reason this cell
/// times the state and not the command. Only the warm state or the deadline
/// ends the loop; an `add` that failed ends it with an error.
///
/// Plain `git worktree add` copies no ignored state, so it never reaches the
/// warm state. Its row measures the command instead and says `warm_reached:
/// false`, which is the `never` of the handoff table.
fn warm_sample(fixture: &Fixture, tool: Tool, path: &Path) -> Result<Sample> {
    let golden = fixture.golden();
    if !tool.under_envelope() {
        let primary_ms = timed(&mut create_command(tool, golden, path, fixture::BRANCH))?.0;
        return Ok(Sample {
            primary_ms,
            warm_reached: Some(false),
            ..Sample::default()
        });
    }
    let kind = fixture.kind();
    // golden does not change while the sample runs, so its fingerprint is read
    // once and outside the timer.
    let want = signature(golden, kind);
    let mut wait = POLL_FIRST;
    let started = Instant::now();
    let mut child = create_command(tool, golden, path, fixture::BRANCH)
        // The poll, not the report, ends this timer. The `add --json` document
        // would otherwise land on the stdout of `bench --json`, which promises
        // one document. Its stderr still reaches the terminal.
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(Error::io("start the measured add"))?;
    let mut exited = false;
    let primary_ms = loop {
        if !exited {
            match child.try_wait().map_err(Error::io("wait for the add"))? {
                Some(status) if !status.success() => {
                    return Err(Error::klon(format!(
                        "the measured add failed with {}",
                        status.code().unwrap_or(-1)
                    )))
                }
                Some(_) => exited = true,
                None => {}
            }
        }
        if crate::warm::pending(path).is_empty() && signature(path, kind) == want {
            let reached = started.elapsed().as_secs_f64() * 1000.0;
            // The confirmation is outside the timer: the tree was equal when
            // the fingerprints agreed, not when the hash finished.
            if is_warm(golden, path, kind) {
                break reached;
            }
        }
        if started.elapsed() > WARM_DEADLINE {
            if !exited {
                let _ = child.kill();
            }
            return Err(Error::klon(format!(
                "the tree at {} did not reach golden's ignored state in {} s",
                path.display(),
                WARM_DEADLINE.as_secs()
            )));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(POLL_LAST);
    };
    // The timer is closed. Reap the command, so the teardown finds no child of
    // this process still holding the tree.
    if !exited {
        let status = child.wait().map_err(Error::io("wait for the add"))?;
        if !status.success() {
            return Err(Error::klon(format!(
                "the measured add failed with {}",
                status.code().unwrap_or(-1)
            )));
        }
    }
    Ok(Sample {
        primary_ms,
        warm_reached: Some(true),
        ..Sample::default()
    })
}

/// True when `tree` holds the same ignored state as golden, byte for byte and
/// time for time.
///
/// A tree that is still being filled can make the walk itself fail: a file that
/// `read_dir` listed can be gone by the time `stat` reaches it. That is not
/// warm, and it is not a fault of the run either, so the answer is false and
/// the poll goes on.
fn is_warm(golden: &Path, tree: &Path, kind: fixture::Kind) -> bool {
    matches!(compare_ignored(golden, tree, kind), Ok(None))
}

/// The cheap fingerprint of an ignored state: how many entries it holds and how
/// many bytes those entries claim. One `stat` per entry, no read.
///
/// Two states with one fingerprint are usually equal; `is_warm` decides. A
/// state that cannot be read at all answers with the count it reached, which
/// never equals a filled golden's.
fn signature(root: &Path, kind: fixture::Kind) -> (usize, u64) {
    fn walk(dir: &Path, count: &mut usize, bytes: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            *count += 1;
            if meta.is_dir() {
                walk(&path, count, bytes);
            } else if meta.is_file() {
                *bytes += meta.len();
            }
        }
    }
    let mut count = 0;
    let mut bytes = 0;
    for dir in kind.ignored_dirs() {
        walk(&root.join(dir), &mut count, &mut bytes);
    }
    (count, bytes)
}

// --- M3: the units a first build compiles ---------------------------------------

/// M3: build in `tree` and count what the build compiled.
///
/// The build runs directly in the tree, not under `gh klon run`: M3 asks what
/// the ecosystem does with a warm state that moved, and the envelope changes
/// neither the unit count nor the answer. M12 below runs under `run`, because
/// the envelope is exactly what M12 measures.
///
/// A failed build is a result, not the end of the run. A plain `git worktree
/// add` tree has no warm state to install from, so its build may well fail;
/// the record then says so and its timing is void.
fn build_sample(fixture: &Fixture, tree: &Path) -> Result<Sample> {
    let kind = fixture.kind();
    let program = ecosystem_program(kind)?;
    let started = Instant::now();
    let output = fixture::build_command(kind, &program, tree, &fixture.store(), false)
        .output()
        .map_err(Error::io("run the measured build"))?;
    let primary_ms = started.elapsed().as_secs_f64() * 1000.0;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Ok(Sample {
            primary_ms,
            build_failure: Some(format!(
                "exit {}: {}",
                output.status.code().unwrap_or(-1),
                last_line(&text)
            )),
            ..Sample::default()
        });
    }
    Ok(Sample {
        primary_ms,
        units: Some(fixture::units_compiled(kind, &text)),
        ..Sample::default()
    })
}

/// The program that builds this kind of fixture. `select` proved it is here.
fn ecosystem_program(kind: fixture::Kind) -> Result<PathBuf> {
    let name = kind
        .tools()
        .first()
        .ok_or_else(|| Error::klon("a synthetic cell has no build tool"))?;
    fixture::tool(name).ok_or_else(|| Error::klon(format!("{name} is not on PATH")))
}

/// The last line of `text` that holds anything, for a one-line failure report.
fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string()
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
///
/// The plain klon tool passes `--no-spare`, so its number is the direct
/// clone. The spare tool sets `KLON_SPARE=1`, which beats a `KLON_SPARE=0`
/// that the test harness may have put in the environment of `bench`.
fn create_command(tool: Tool, golden: &Path, path: &Path, branch: &str) -> Command {
    match tool {
        Tool::Klon | Tool::KlonSpare => {
            let mut command = Command::new(klon_binary());
            command.current_dir(golden).args(["add", "--json"]);
            if tool == Tool::Klon {
                command.arg("--no-spare");
            } else {
                command.env("KLON_SPARE", "1");
            }
            command.args([branch, "--path"]).arg(path);
            fixture::isolate(&mut command);
            command
        }
        Tool::Baseline => {
            let mut command = fixture::isolated_git(golden, &["worktree", "add"]);
            command.arg(path).arg(branch);
            command
        }
    }
}

/// The command that removes the tree at `path`. `rm` starts no builder here:
/// a clone in the background would add noise to the next sample.
fn remove_command(tool: Tool, golden: &Path, path: &Path) -> Command {
    match tool {
        Tool::Klon | Tool::KlonSpare => {
            let mut command = Command::new(klon_binary());
            command
                .current_dir(golden)
                .args(["rm", "--no-spare", "--path"])
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

/// Before a spare sample: a spare must be ready, outside the timer. The
/// build waits for any builder that a previous sample started.
fn prepare(tool: Tool, golden: &Path) -> Result<()> {
    match tool {
        Tool::KlonSpare => crate::spare::ensure(golden),
        Tool::Klon | Tool::Baseline => Ok(()),
    }
}

/// After a spare sample: the measured `add` started the next builder. Wait
/// for it, so the next sample of any tool runs on a quiet disk.
fn settle(tool: Tool, golden: &Path) -> Result<()> {
    match tool {
        Tool::KlonSpare => {
            crate::spare::wait_for_builder(golden, std::time::Duration::from_secs(300))
        }
        Tool::Klon | Tool::Baseline => Ok(()),
    }
}

/// A spare sample whose `add` did not use the spare measured a direct clone
/// under the wrong label. That is a defect of the run, not a slow result.
fn check_spare(tool: Tool, stdout: &str) -> Result<()> {
    if tool == Tool::KlonSpare && !spare_of(stdout)? {
        return Err(Error::klon(
            "the spare sample did not use the spare; the report would mislabel a direct clone",
        ));
    }
    Ok(())
}

/// The `spare` field of an `add --json` document.
fn spare_of(stdout: &str) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|err| Error::klon(format!("read the add report: {err}")))?;
    value["spare"]
        .as_bool()
        .ok_or_else(|| Error::klon("the add report has no spare field"))
}

fn status_command(path: &Path) -> Command {
    fixture::isolated_git(path, &["status", "--porcelain"])
}

/// Create a tree outside the timer. The answer is the backend that filled it.
fn create(tool: Tool, golden: &Path, path: &Path) -> Result<String> {
    create_branch(tool, golden, path, fixture::BRANCH)
}

/// `create`, with the branch that the new tree checks out. M12 needs one branch
/// per builder: git refuses two worktrees on one branch.
fn create_branch(tool: Tool, golden: &Path, path: &Path, branch: &str) -> Result<String> {
    prepare(tool, golden)?;
    let (_, stdout) = timed(&mut create_command(tool, golden, path, branch))?;
    check_spare(tool, &stdout)?;
    // C12 sends a big ignored directory to a detached process, so `add` can
    // return before the tree is filled. Every step that reads a tree it made
    // outside a timer waits here first, or it would read a half-filled one.
    // The M2 cell is the exception: it times that wait itself.
    wait_for_warm(path)?;
    match tool {
        Tool::Klon | Tool::KlonSpare => backend_of(&stdout),
        Tool::Baseline => Ok(BASELINE.to_string()),
    }
}

/// How long a caller waits for a detached warm to land.
const WARM_WAIT: std::time::Duration = std::time::Duration::from_secs(900);

/// Wait until nothing of `klon` is still warming (C12).
///
/// A tree with a warm still running is not the tree a cell measures: the
/// correctness check would compare a half-filled state, and the next sample
/// would compete with the copy for the disk. A tree that never warmed, a
/// baseline worktree included, carries no marker and returns at once.
fn wait_for_warm(klon: &Path) -> Result<()> {
    let started = Instant::now();
    loop {
        let pending = crate::warm::pending(klon);
        if pending.is_empty() {
            return Ok(());
        }
        if started.elapsed() > WARM_WAIT {
            return Err(Error::klon(format!(
                "{} still warms {} after {} s",
                klon.display(),
                pending.join(", "),
                WARM_WAIT.as_secs()
            )));
        }
        std::thread::sleep(POLL_FIRST);
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
///
/// The warm pass has to finish first. A delete that walks a directory while a
/// detached process still writes into it fails with `ENOTEMPTY`, and the whole
/// run ends on a cleanup race instead of a result.
fn teardown(golden: &Path, path: &Path) -> Result<()> {
    wait_for_warm(path)?;
    let text = path.to_str().unwrap_or_default();
    let Err(why) = fixture::git(golden, &["worktree", "remove", "--force", text]) else {
        return Ok(());
    };
    if path.exists() {
        // A removal that runs beside a live writer fails with ENOTEMPTY, and a
        // bare "Directory not empty" hides which step left one behind. Both
        // reasons go into the error.
        fs::remove_dir_all(path).map_err(|err| {
            Error::klon(format!(
                "remove {}: {err}; git worktree remove said: {}",
                path.display(),
                why.to_string().trim()
            ))
        })?;
    }
    fixture::git(golden, &["worktree", "prune"])?;
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
/// An M6 cell measures the removal, so the check runs the removal too and
/// proves the tree is gone. A removal that returns success and leaves the tree
/// would otherwise report a fast, wrong result.
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
    let mut correctness = check(golden, &path, tool, fixture.kind())?;
    if cell.action == Action::Rm {
        let (ok, detail) = removal_verdict(golden, &path, tool);
        correctness.matched = correctness.matched && ok;
        correctness.removal = detail;
    }
    if path.exists() {
        teardown(golden, &path)?;
    }
    settle(tool, golden)?;
    Ok(Checked {
        tool,
        backend,
        correctness,
    })
}

/// Run the removal that an M6 cell measures, and answer whether the tree is
/// gone. A failed command is a verdict here, not the end of the run: the report
/// then voids the cell and still holds its samples.
fn removal_verdict(golden: &Path, path: &Path, tool: Tool) -> (bool, String) {
    if let Err(why) = timed(&mut remove_command(tool, golden, path)) {
        return (false, format!("the removal failed: {why}"));
    }
    if let Err(why) = drain_trash(golden) {
        return (false, format!("the background delete failed: {why}"));
    }
    if path.exists() {
        return (
            false,
            format!("the removal left {} in place", path.display()),
        );
    }
    (true, "removed".to_string())
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

/// The manifest tests of a new tree:
///
/// 1. The ignored directory against golden's, byte for byte and time for time.
/// 2. The tracked side: the tree holds the branch that the cell asked for, at
///    golden's commit for it.
/// 3. A clean `git status`, after klon forced git to compare content.
fn check(golden: &Path, tree: &Path, tool: Tool, kind: fixture::Kind) -> Result<Correctness> {
    let ignored_manifest = match tool {
        // The path fixup pass rewrites golden's absolute path inside a real
        // build tree, so a correct klon of a cargo or a pnpm fixture can never
        // equal golden byte for byte (handoff §9). Such a cell rests on the
        // metric it measures instead: a build that compiled zero units and
        // exited 0 is the proof that the state moved.
        _ if kind.fixup_rewrites_ignored_state() => format!(
            "not-applicable: the path fixup rewrites golden's path inside the {} state",
            kind.tag()
        ),
        Tool::Klon | Tool::KlonSpare => match compare_ignored(golden, tree, kind)? {
            None => "match".to_string(),
            Some(why) => format!("mismatch: {why}"),
        },
        // Plain `git worktree add` copies no ignored state, so there is nothing
        // to compare. That absence is the point of the baseline, not a fault.
        Tool::Baseline => "not-applicable: the baseline copies no ignored state".to_string(),
    };
    let (tracked_ok, tracked) = tracked_verdict(golden, tree)?;
    force_content_check(tree)?;
    let porcelain = fixture::git(tree, &["status", "--porcelain"])?;
    let status = if porcelain.trim().is_empty() {
        "clean".to_string()
    } else {
        format!("dirty: {}", porcelain.lines().next().unwrap_or("").trim())
    };
    Ok(Correctness {
        matched: !ignored_manifest.starts_with("mismatch") && tracked_ok && status == "clean",
        ignored_manifest,
        tracked,
        status,
        removal: "not-applicable: the cell removes no tree".to_string(),
        build: "not-applicable: the cell builds nothing".to_string(),
    })
}

/// The tracked side of the manifest test. A tree on another branch can be
/// perfectly clean, so a clean `git status` alone proves nothing: the tree must
/// hold the branch that the cell asked for, at golden's commit for it.
fn tracked_verdict(golden: &Path, tree: &Path) -> Result<(bool, String)> {
    let reference = format!("refs/heads/{}", fixture::BRANCH);
    let want = fixture::git(golden, &["rev-parse", &reference])?;
    let got = fixture::git(tree, &["rev-parse", "HEAD"])?;
    if want.trim() != got.trim() {
        return Ok((
            false,
            format!(
                "mismatch: HEAD is {} and {} is {}",
                got.trim(),
                fixture::BRANCH,
                want.trim()
            ),
        ));
    }
    match fixture::git(tree, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(name) if name.trim() == fixture::BRANCH => {
            Ok((true, format!("on {} at {}", fixture::BRANCH, got.trim())))
        }
        Ok(name) => Ok((
            false,
            format!(
                "mismatch: the tree is on {} and not on {}",
                name.trim(),
                fixture::BRANCH
            ),
        )),
        // `symbolic-ref` exits non-zero on a detached HEAD.
        Err(_) => Ok((
            false,
            format!(
                "mismatch: the tree has a detached HEAD, not {}",
                fixture::BRANCH
            ),
        )),
    }
}

/// Make the next `git status` compare content instead of stat information.
///
/// `add` sets `core.checkStat=minimal`, so git compares the size and the whole
/// second of the mtime. An edit that keeps both is invisible: that is the
/// documented blind spot. An index older than every working-tree file makes
/// every entry racily clean, and git then re-reads the file content
/// (handoff §11). The re-hash is outside every timer.
fn force_content_check(tree: &Path) -> Result<()> {
    let path = fixture::git(
        tree,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    let path = Path::new(path.trim());
    if !path.is_file() {
        return Err(Error::klon(format!(
            "the tree has no index at {}",
            path.display()
        )));
    }
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(0, 0))
        .map_err(Error::io(format!("re-time {}", path.display())))
}

/// Compare every ignored directory of `kind` between golden and `tree`. The
/// answer names the first difference, or None when they all agree.
fn compare_ignored(golden: &Path, tree: &Path, kind: fixture::Kind) -> Result<Option<String>> {
    for dir in kind.ignored_dirs() {
        if let Some(why) = compare(&golden.join(dir), &tree.join(dir))? {
            return Ok(Some(why));
        }
    }
    Ok(None)
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
