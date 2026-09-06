//! `gh klon bench` (spec §7 C8, R14).
//!
//! Every test runs the real command with the smoke profiles, so a run takes
//! seconds instead of minutes. `KLON_BENCH_SMOKE=1` shrinks every fixture and
//! `KLON_BENCH_RUNS` shrinks the sample count; both are recorded in the report,
//! and the smoke shape gives its own `fixture_hash`, so a smoke result can
//! never pass for a measurement. The tests below check the defaults of the
//! committed manifest instead of measuring with them.

mod common;

use common::{klon_env, stderr, stdout};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::Path;

/// The committed manifest, as the binary embeds it.
const MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench/manifests/v1.toml"
));

/// A working directory and a fixture directory for one bench run.
struct Run {
    _tmp: tempfile::TempDir,
    cwd: std::path::PathBuf,
    bench_dir: std::path::PathBuf,
}

impl Run {
    fn new() -> Run {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("cwd");
        let bench_dir = tmp.path().join("fixtures");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bench_dir).unwrap();
        Run {
            _tmp: tmp,
            cwd,
            bench_dir,
        }
    }

    /// Run `bench` with the smoke profiles and three samples per record.
    fn bench(&self, extra: &[(&str, &OsStr)], args: &[&str]) -> std::process::Output {
        let mut envs: Vec<(&str, &OsStr)> = vec![
            ("KLON_BENCH_SMOKE", OsStr::new("1")),
            ("KLON_BENCH_RUNS", OsStr::new("3")),
            ("KLON_BENCH_DIR", self.bench_dir.as_os_str()),
        ];
        envs.extend_from_slice(extra);
        klon_env(&self.cwd, &envs, args)
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("not one JSON document: {err}\n{text}"))
}

/// The one record of `report` whose backend is not the baseline.
fn klon_record(report: &Value) -> &Value {
    report["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["backend"] != "git-worktree-add")
        .expect("a klon record")
}

fn baseline_record(report: &Value) -> &Value {
    report["records"]
        .as_array()
        .expect("records")
        .iter()
        .find(|r| r["backend"] == "git-worktree-add")
        .expect("a baseline record")
}

fn numbers(value: &Value) -> Vec<f64> {
    value
        .as_array()
        .expect("an array of numbers")
        .iter()
        .map(|n| n.as_f64().expect("a number"))
        .collect()
}

/// The only result file below `<cwd>/bench/results`.
fn result_file(dir: &Path) -> std::path::PathBuf {
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    found.sort();
    assert_eq!(found.len(), 1, "one result file, found {found:?}");
    found.pop().unwrap()
}

// --- The acceptance lines ------------------------------------------------------

/// The first C8 acceptance line, plus the third: the JSON holds the schema, the
/// raw samples, the percentiles, and the environment record, and the baseline
/// prints its own samples from the same fixture.
#[test]
fn the_json_report_holds_the_samples_and_the_environment() {
    let run = Run::new();
    let out = run.bench(&[], &["bench", "--cell", "m1-add-10k", "--json"]);
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let report = parse(&stdout(&out));

    assert_eq!(report["schema"], "klon.bench/1");
    assert_eq!(report["manifest"]["version"], 1);
    assert_eq!(report["manifest"]["path"], "bench/manifests/v1.toml");
    // The samples are shortened for the test suite. The committed manifest
    // still asks for the 10 warm and 5 cold runs of a development run.
    assert_eq!(report["manifest"]["warm_runs"], 10);
    assert_eq!(report["manifest"]["cold_runs"], 5);
    assert!(
        MANIFEST.contains("\nwarm = 10\n") && MANIFEST.contains("\ncold = 5\n"),
        "the committed manifest must ask for 10 warm and 5 cold runs"
    );

    let records = report["records"].as_array().expect("records");
    assert_eq!(records.len(), 2, "one klon record and one baseline record");

    let klon = klon_record(&report);
    assert_eq!(klon["cell"], "m1-add-10k");
    assert_eq!(klon["metric"], "M1");
    assert_eq!(klon["runs"], 3);
    assert_eq!(klon["spare"], false, "v0 has no hot spare");
    assert_eq!(
        klon["cache_drop"], "warm-only",
        "this host cannot drop caches"
    );
    assert!(
        ["copy", "reflink-walk"].contains(&klon["backend"].as_str().expect("a backend")),
        "unknown backend {}",
        klon["backend"]
    );
    let samples = numbers(&klon["samples_ms"]);
    assert_eq!(samples.len(), 3, "one number per sample");
    assert!(samples.iter().all(|ms| *ms > 0.0), "found {samples:?}");
    let p50 = klon["p50_ms"].as_f64().expect("p50_ms");
    let p95 = klon["p95_ms"].as_f64().expect("p95_ms");
    assert!(samples.contains(&p50), "p50 must be one of the samples");
    assert!(p95 >= p50, "p95 {p95} must not be below p50 {p50}");
    assert_eq!(klon["timing_valid"], true);
    assert_eq!(klon["correctness"]["ignored_manifest"], "match");
    assert_eq!(klon["correctness"]["status"], "clean");
    assert_eq!(klon["pass_p50_ms"], 1000);
    assert_eq!(klon["first_p50_ms"], Value::Null, "M1 has no first series");
    assert_eq!(klon["steady_p50_ms"], Value::Null);

    // The third acceptance line: the baseline runs on the same fixture and
    // brings its own samples.
    let baseline = baseline_record(&report);
    assert_eq!(baseline["cell"], "m1-add-10k");
    assert_eq!(baseline["profile_shape"], klon["profile_shape"]);
    assert_eq!(numbers(&baseline["samples_ms"]).len(), 3);
    assert_ne!(
        numbers(&baseline["samples_ms"]),
        samples,
        "the two tools must give two series"
    );
    assert_eq!(
        baseline["pass"],
        Value::Null,
        "the klon budget is not the baseline's"
    );
    assert_eq!(baseline["timing_valid"], true);

    // The environment record.
    let env = &report["environment"];
    for field in [
        "hostname",
        "cpu_model",
        "os",
        "kernel",
        "arch",
        "bench_dir",
        "filesystem",
        "mount_options",
        "git_version",
        "klon_version",
        "klon_commit",
        "fixture_hash",
    ] {
        let value = env[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} is missing"));
        assert!(!value.is_empty(), "{field} must not be empty");
    }
    assert!(env["cpu_cores"].as_u64().expect("cpu_cores") > 0);
    assert_eq!(env["drop_caches"], "none");

    // The same document lands in a result file.
    let path = result_file(&run.cwd.join("bench").join("results"));
    let written = parse(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(written["schema"], "klon.bench/1");
    assert_eq!(written["environment"]["fixture_hash"], env["fixture_hash"]);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(report["timestamp"].as_str().unwrap().get(..10).unwrap()),
        "the file name starts with the date: {name}"
    );
}

/// The second C8 acceptance line: an injected manifest mismatch voids the cell.
#[test]
fn an_injected_mismatch_voids_the_timing() {
    let run = Run::new();
    let out = run.bench(
        &[("KLON_BENCH_INJECT_MISMATCH", OsStr::new("1"))],
        &["bench", "--cell", "m1-add-10k", "--json"],
    );
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let report = parse(&stdout(&out));

    let klon = klon_record(&report);
    assert_eq!(
        klon["timing_valid"], false,
        "a damaged tree voids its timing"
    );
    assert_eq!(klon["pass"], false, "a void record cannot pass");
    assert_eq!(klon["correctness"]["matched"], false);
    let why = klon["correctness"]["ignored_manifest"]
        .as_str()
        .expect("a reason");
    assert!(why.starts_with("mismatch:"), "unexpected reason {why}");
    // The samples are still there. The report voids the timing; it does not
    // hide it.
    assert_eq!(numbers(&klon["samples_ms"]).len(), 3);

    // The baseline holds no ignored state, so the damage lands on a tracked
    // file and `git status` catches it instead.
    let baseline = baseline_record(&report);
    assert_eq!(baseline["timing_valid"], false);
    assert!(
        baseline["correctness"]["status"]
            .as_str()
            .expect("a status")
            .starts_with("dirty:"),
        "found {}",
        baseline["correctness"]["status"]
    );
}

/// The fifth C8 acceptance line: the run order is random and recorded. The
/// seed of the order is recorded too, so a run repeats exactly.
#[test]
fn the_run_order_is_recorded() {
    let run = Run::new();
    let out = run.bench(
        &[("KLON_BENCH_ORDER_SEED", OsStr::new("987654321"))],
        &["bench", "--cell", "m1-add-10k", "--json"],
    );
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let report = parse(&stdout(&out));
    assert_eq!(report["environment"]["order_seed"], 987_654_321u64);

    let mut steps: Vec<u64> = Vec::new();
    for record in report["records"].as_array().expect("records") {
        let order = record["order"].as_array().expect("an order");
        assert_eq!(
            order.len(),
            record["samples_ms"].as_array().unwrap().len(),
            "one position per sample"
        );
        steps.extend(order.iter().map(|n| n.as_u64().expect("a position")));
    }
    steps.sort();
    let expected: Vec<u64> = (0..steps.len() as u64).collect();
    assert_eq!(
        steps, expected,
        "every sample of the cell must hold one position in the run order"
    );
}

/// The M6 cell measures a removal, so its correctness check runs one and proves
/// the tree is gone. A removal that returns success and leaves the tree would
/// otherwise report a fast, wrong result.
#[test]
fn the_removal_cell_proves_the_tree_is_gone() {
    let run = Run::new();
    let out = run.bench(
        &[("KLON_FIXTURE", OsStr::new("100k"))],
        &["bench", "--cell", "m6-rm-100k", "--json"],
    );
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let report = parse(&stdout(&out));
    for record in report["records"].as_array().expect("records") {
        assert_eq!(record["metric"], "M6");
        assert_eq!(
            record["correctness"]["removal"], "removed",
            "the check must remove the tree and say so"
        );
        assert_eq!(record["timing_valid"], true);
    }
}

/// The M4 cell reports the first call and the later calls as two series.
#[test]
fn the_status_cell_reports_the_first_and_the_steady_series() {
    let run = Run::new();
    let out = run.bench(
        &[("KLON_FIXTURE", OsStr::new("100k"))],
        &["bench", "--cell", "m4-status-100k", "--json"],
    );
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let report = parse(&stdout(&out));
    for record in report["records"].as_array().expect("records") {
        let first = record["first_p50_ms"].as_f64().expect("first_p50_ms");
        let steady = record["steady_p50_ms"].as_f64().expect("steady_p50_ms");
        assert!(first > 0.0 && steady > 0.0, "found {first} and {steady}");
        assert_eq!(
            record["first_p50_ms"], record["p50_ms"],
            "the primary series is the first call"
        );
        // Three steady calls per sample, three samples.
        assert_eq!(numbers(&record["steady_samples_ms"]).len(), 9);
        assert_eq!(record["pass_steady_p50_ms"], 150);
        // The correctness check names the branch and the commit it found, so a
        // tree on another clean branch cannot pass.
        let tracked = record["correctness"]["tracked"]
            .as_str()
            .expect("a tracked verdict");
        assert!(tracked.starts_with("on feature at "), "found {tracked}");
        assert_eq!(
            record["correctness"]["removal"],
            "not-applicable: the cell removes no tree"
        );
    }
}

// --- The command line ----------------------------------------------------------

/// A 100k cell needs `KLON_FIXTURE=100k`. Without it the run skips the cell and
/// says why; it does not fail.
#[test]
fn a_100k_cell_without_the_fixture_variable_is_skipped() {
    let run = Run::new();
    let out = run.bench(&[], &["bench", "--cell", "m1-add-100k", "--json"]);
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let report = parse(&stdout(&out));
    assert!(
        report["records"].as_array().unwrap().is_empty(),
        "the cell must not run"
    );
    let skipped = report["skipped"].as_array().expect("skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["cell"], "m1-add-100k");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .expect("a reason")
            .contains("KLON_FIXTURE=100k"),
        "found {}",
        skipped[0]["reason"]
    );
}

/// `--out` puts the result file where the caller asked.
#[test]
fn the_out_flag_moves_the_result_file() {
    let run = Run::new();
    let out_dir = run.cwd.join("elsewhere");
    let out = run.bench(
        &[],
        &[
            "bench",
            "--cell",
            "m1-add-10k",
            "--out",
            out_dir.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let path = result_file(&out_dir);
    assert!(
        !run.cwd.join("bench").exists(),
        "--out must not also write bench/results"
    );
    let table = stdout(&out);
    assert!(
        table.contains("m1-add-10k"),
        "the table names the cell: {table}"
    );
    assert!(
        table.contains("git-worktree-add"),
        "the table names the baseline: {table}"
    );
    assert!(
        table.contains(&path.display().to_string()),
        "the table names the result file: {table}"
    );
}

/// An unknown cell name lists the known ones instead of running nothing.
#[test]
fn an_unknown_cell_name_is_refused() {
    let run = Run::new();
    let out = run.bench(&[], &["bench", "--cell", "m9-nothing", "--json"]);
    assert!(!out.status.success(), "an unknown cell must fail");
    let why = stderr(&out);
    assert!(why.contains("unknown cell m9-nothing"), "found {why}");
    assert!(why.contains("m1-add-10k"), "found {why}");
}
