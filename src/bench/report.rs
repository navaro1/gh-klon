//! The benchmark report: the record shapes, the percentiles, the environment
//! record, and the human table (spec §7 C8, R14; handoff §8).
//!
//! A result file is the product of a run. It has to say enough that a reader a
//! year later can tell whether two files measure the same thing: the manifest
//! version and the fixture hash, the raw samples, the run order, the host, and
//! the correctness verdict that decides whether the timing counts at all.

use crate::bench::manifest::Profile;
use crate::{probe, time};
use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.bench/1";

/// The backend name of the comparison baseline.
pub const BASELINE: &str = "git-worktree-add";

/// The whole result file.
#[derive(Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub timestamp: String,
    /// True when `--release` selected the release run counts.
    pub release: bool,
    /// True when the run used the tiny smoke shape instead of the manifest
    /// profiles. A smoke result measures nothing; it proves the plumbing.
    pub smoke: bool,
    pub manifest: ManifestInfo,
    pub environment: Environment,
    pub records: Vec<Record>,
    /// Every cell that did not run, with the reason.
    pub skipped: Vec<Skip>,
}

/// What the run took from the manifest.
#[derive(Serialize)]
pub struct ManifestInfo {
    pub version: u32,
    pub path: &'static str,
    pub seed: u64,
    /// The manifest default, whatever `KLON_BENCH_RUNS` did to a record.
    pub warm_runs: u32,
    pub cold_runs: u32,
}

/// The host and the build. Two result files compare only when these agree.
#[derive(Serialize)]
pub struct Environment {
    pub hostname: String,
    pub cpu_model: String,
    pub cpu_cores: usize,
    pub memory_total_kb: u64,
    pub os: String,
    pub kernel: String,
    pub arch: &'static str,
    /// The directory that held the fixture, and its filesystem.
    pub bench_dir: String,
    pub filesystem: String,
    pub mount_options: String,
    pub git_version: String,
    pub klon_version: &'static str,
    pub klon_commit: &'static str,
    /// A hash of the manifest seed and every profile shape.
    pub fixture_hash: String,
    /// The seed of the random run order. It repeats a run order exactly.
    pub order_seed: u64,
    /// The `KLON_BENCH_DROP_CACHES` command, or `none`.
    pub drop_caches: String,
}

/// One measured series: one cell for one tool.
#[derive(Serialize)]
pub struct Record {
    pub cell: String,
    pub metric: String,
    pub profile: String,
    /// The shape that the fixture of this record was built from.
    pub profile_shape: Profile,
    /// The klon backend that filled the tree, or `git-worktree-add`.
    pub backend: String,
    /// True when a hot spare served the `add`. C9 sets it; v0 has no spare.
    pub spare: bool,
    /// True when klon dropped the page cache between the samples.
    pub cold: bool,
    /// `dropped` or `warm-only`.
    pub cache_drop: &'static str,
    /// Where the timer started and stopped, from the manifest.
    pub timer: String,
    pub runs: u32,
    /// The position of each sample in the random run order of its cell.
    pub order: Vec<usize>,
    pub samples_ms: Vec<f64>,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// M4 only: the p50 of the first `git status` in a fresh tree.
    pub first_p50_ms: Option<f64>,
    /// M4 only: the p50 of the calls after the first one.
    pub steady_p50_ms: Option<f64>,
    /// M4 only: the raw samples behind `steady_p50_ms`.
    pub steady_samples_ms: Vec<f64>,
    pub correctness: Correctness,
    /// False when the correctness check failed. A wrong tree voids its timing.
    pub timing_valid: bool,
    pub pass_p50_ms: u64,
    /// The steady budget of an M4 cell. Null for every other cell.
    pub pass_steady_p50_ms: Option<u64>,
    /// Whether the record met the pass rule. Null for the baseline, which the
    /// klon budget does not bind.
    pub pass: Option<bool>,
}

/// The manifest test of one cell.
#[derive(Clone, Serialize)]
pub struct Correctness {
    pub matched: bool,
    /// The comparison of golden's ignored directory with the new tree's.
    pub ignored_manifest: String,
    /// `git status --porcelain` in the new tree.
    pub status: String,
}

/// A cell that did not run.
#[derive(Serialize)]
pub struct Skip {
    pub cell: String,
    pub reason: String,
}

impl Record {
    /// Fill the derived fields from the raw samples.
    pub fn summarize(&mut self) {
        self.p50_ms = percentile(&self.samples_ms, 0.50);
        self.p95_ms = percentile(&self.samples_ms, 0.95);
        if !self.steady_samples_ms.is_empty() {
            self.first_p50_ms = Some(self.p50_ms);
            self.steady_p50_ms = Some(percentile(&self.steady_samples_ms, 0.50));
        }
        let within = self.p50_ms <= self.pass_p50_ms as f64
            && match (self.steady_p50_ms, self.pass_steady_p50_ms) {
                (Some(steady), Some(budget)) => steady <= budget as f64,
                _ => true,
            };
        // The pass rule binds klon, not the tool it is compared against.
        self.pass = (self.backend != BASELINE).then_some(self.timing_valid && within);
    }
}

/// The nearest-rank percentile of `samples`, in the order they were measured.
/// `q` is a fraction: 0.50 gives the median. An empty series gives 0.
pub fn percentile(samples: &[f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

// --- The environment record ----------------------------------------------------

impl Environment {
    /// Read the host facts. Every one of them degrades to `unknown` instead of
    /// failing the run: a report with one missing row still holds its samples.
    pub fn read(bench_dir: &Path, fixture_hash: String, order_seed: u64) -> Environment {
        Environment {
            hostname: hostname(),
            cpu_model: cpu_model(),
            cpu_cores: cpu_cores(),
            memory_total_kb: memory_total_kb(),
            os: os_name(),
            kernel: first_line(&run("uname", &["-sr"])).unwrap_or_else(|| "unknown".to_string()),
            arch: std::env::consts::ARCH,
            bench_dir: bench_dir.display().to_string(),
            filesystem: probe::filesystem(bench_dir),
            mount_options: mount_options(bench_dir),
            git_version: git_version(),
            klon_version: env!("CARGO_PKG_VERSION"),
            klon_commit: env!("KLON_COMMIT"),
            fixture_hash,
            order_seed,
            drop_caches: std::env::var("KLON_BENCH_DROP_CACHES")
                .unwrap_or_else(|_| "none".to_string()),
        }
    }
}

/// The host name, for the result file name and the record.
pub fn hostname() -> String {
    read_trimmed("/proc/sys/kernel/hostname")
        .or_else(|| first_line(&run("uname", &["-n"])))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The first `model name` line of `/proc/cpuinfo`, or the macOS brand string.
fn cpu_model() -> String {
    if let Some(line) = proc_field("/proc/cpuinfo", "model name") {
        return line;
    }
    first_line(&run("sysctl", &["-n", "machdep.cpu.brand_string"]))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The number of `processor` lines in `/proc/cpuinfo`, else the parallelism
/// that this process may use.
fn cpu_cores() -> usize {
    let counted = std::fs::read_to_string("/proc/cpuinfo")
        .map(|text| text.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0);
    if counted > 0 {
        return counted;
    }
    std::thread::available_parallelism().map_or(0, |n| n.get())
}

fn memory_total_kb() -> u64 {
    if let Some(value) = proc_field("/proc/meminfo", "MemTotal") {
        // `MemTotal:       65707412 kB`
        if let Some(kb) = value.split_whitespace().next().and_then(|n| n.parse().ok()) {
            return kb;
        }
    }
    // macOS reports bytes.
    first_line(&run("sysctl", &["-n", "hw.memsize"]))
        .and_then(|text| text.parse::<u64>().ok())
        .map_or(0, |bytes| bytes / 1024)
}

fn os_name() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                return value.trim_matches('"').to_string();
            }
        }
    }
    match first_line(&run("sw_vers", &["-productVersion"])) {
        Some(version) => format!("macOS {version}"),
        None => "unknown".to_string(),
    }
}

fn git_version() -> String {
    let status = probe::version_of("git", &["--version"]);
    let detail = status.detail();
    detail
        .strip_prefix("git version ")
        .unwrap_or(detail)
        .to_string()
}

/// The mount options of the filesystem that holds `path`.
///
/// `/proc/self/mountinfo` gives the per-mount options and, after the ` - `
/// separator, the superblock options. Both matter for a benchmark: `relatime`
/// and `data=ordered` change what a copy costs. The longest mount point that is
/// a prefix of `path` wins, because a bind mount can nest.
fn mount_options(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return "unknown".to_string();
    };
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        let Some((head, tail)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = head.split(' ').collect();
        let (Some(point), Some(per_mount)) = (fields.get(4), fields.get(5)) else {
            continue;
        };
        if !path.starts_with(point) {
            continue;
        }
        let super_options = tail.split(' ').nth(2).unwrap_or("");
        let merged = merge_options(per_mount, super_options);
        if best.as_ref().is_none_or(|(len, _)| point.len() > *len) {
            best = Some((point.len(), merged));
        }
    }
    best.map_or_else(|| "unknown".to_string(), |(_, options)| options)
}

/// Join the per-mount and the superblock options without a repeat.
fn merge_options(per_mount: &str, super_options: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for option in per_mount.split(',').chain(super_options.split(',')) {
        if !option.is_empty() && !out.contains(&option) {
            out.push(option);
        }
    }
    out.join(",")
}

/// The value of `field` in a `key: value` file such as `/proc/cpuinfo`.
fn proc_field(path: &str, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == field {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn read_trimmed(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The stdout of `program`, or None when it is absent or fails.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn first_line(text: &Option<String>) -> Option<String> {
    let line = text.as_ref()?.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

// --- The human table -----------------------------------------------------------

impl Report {
    /// The file name of a result: `<date>-<host>.json`.
    pub fn file_name(&self) -> String {
        let date = self.timestamp.get(..10).unwrap_or("0000-00-00");
        let host: String = self
            .environment
            .hostname
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{date}-{host}.json")
    }

    /// The summary table. One row per record, plus the skipped cells.
    pub fn print_table(&self) {
        println!(
            "{} {} on {} {} ({})",
            self.environment.klon_version,
            self.environment.klon_commit,
            self.environment.os,
            self.environment.kernel,
            self.environment.arch
        );
        if self.smoke {
            println!("smoke run: the profiles are tiny and the numbers measure nothing");
        }
        println!(
            "fixture {} seed {} manifest v{} · {} {} · git {}",
            self.environment.fixture_hash,
            self.manifest.seed,
            self.manifest.version,
            self.environment.filesystem,
            self.environment.mount_options,
            self.environment.git_version
        );
        println!(
            "{:<16} {:<18} {:>4} {:>9} {:>9} {:>9} {:>6} verdict",
            "cell", "backend", "runs", "p50 ms", "p95 ms", "steady", "budget"
        );
        for record in &self.records {
            let steady = match record.steady_p50_ms {
                Some(value) => format!("{value:.1}"),
                None => "-".to_string(),
            };
            println!(
                "{:<16} {:<18} {:>4} {:>9.1} {:>9.1} {:>9} {:>6} {}",
                record.cell,
                record.backend,
                record.runs,
                record.p50_ms,
                record.p95_ms,
                steady,
                record.pass_p50_ms,
                verdict(record)
            );
        }
        for skip in &self.skipped {
            println!("skipped {}: {}", skip.cell, skip.reason);
        }
    }
}

/// The last column: the reason a record is void, else pass, fail, or baseline.
fn verdict(record: &Record) -> String {
    if !record.timing_valid {
        return format!("void: {}", record.correctness.reason());
    }
    match record.pass {
        Some(true) => "pass".to_string(),
        Some(false) => "fail".to_string(),
        None => "baseline".to_string(),
    }
}

impl Correctness {
    /// Why the check failed, in one phrase.
    pub fn reason(&self) -> String {
        if self.matched {
            return "none".to_string();
        }
        format!("{}; {}", self.ignored_manifest, self.status)
    }
}

/// The timestamp of a fresh report.
pub fn now() -> String {
    time::now_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_percentile_uses_the_nearest_rank() {
        let samples: Vec<f64> = (1..=10).map(|n| n as f64).collect();
        assert_eq!(percentile(&samples, 0.50), 5.0);
        assert_eq!(percentile(&samples, 0.95), 10.0);
        // The order of arrival must not change the answer.
        let mut reversed = samples.clone();
        reversed.reverse();
        assert_eq!(percentile(&reversed, 0.50), 5.0);
        assert_eq!(percentile(&[7.5], 0.95), 7.5);
        assert_eq!(percentile(&[], 0.50), 0.0);
    }

    #[test]
    fn the_mount_options_merge_without_a_repeat() {
        assert_eq!(
            merge_options("rw,relatime", "rw,errors=remount-ro"),
            "rw,relatime,errors=remount-ro"
        );
        assert_eq!(merge_options("ro", ""), "ro");
    }

    /// The environment record must never stop a run. On this host every field
    /// answers; on another one some fall back to `unknown`.
    #[test]
    fn the_environment_record_always_answers() {
        let env = Environment::read(Path::new("."), "abc123".to_string(), 42);
        assert!(!env.hostname.is_empty());
        assert!(!env.os.is_empty());
        assert!(!env.kernel.is_empty());
        assert!(!env.filesystem.is_empty());
        assert!(!env.mount_options.is_empty());
        assert_eq!(env.fixture_hash, "abc123");
        assert_eq!(env.order_seed, 42);
        assert!(!env.klon_commit.is_empty(), "build.rs must set KLON_COMMIT");
    }

    #[test]
    fn the_result_file_name_holds_the_date_and_the_host() {
        let mut report = sample_report();
        report.environment.hostname = "my/host".to_string();
        assert_eq!(report.file_name(), "2026-09-05-my-host.json");
    }

    fn sample_report() -> Report {
        Report {
            schema: SCHEMA,
            timestamp: "2026-09-05T10:00:00Z".to_string(),
            release: false,
            smoke: false,
            manifest: ManifestInfo {
                version: 1,
                path: "bench/manifests/v1.toml",
                seed: 1,
                warm_runs: 10,
                cold_runs: 5,
            },
            environment: Environment::read(Path::new("."), "abc".to_string(), 1),
            records: Vec::new(),
            skipped: Vec::new(),
        }
    }
}
