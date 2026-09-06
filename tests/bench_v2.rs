//! The `gh klon bench` v2 cells (spec §7 C31, R14): M2, M3, M5, and M12.
//!
//! Every test runs the real command with the smoke shape, so a run takes
//! seconds instead of minutes. `KLON_BENCH_SMOKE=1` shrinks every profile and
//! every ecosystem cell, `KLON_BENCH_RUNS=1` shrinks the sample count, and
//! `KLON_BENCH_N` shrinks the builder count of the M12 cell. All three are
//! recorded, and the smoke shape gives its own `fixture_hash`, so a smoke
//! result can never pass for a measurement. The tests below check the defaults
//! of the committed manifest instead of measuring with them.
//!
//! A cell whose tool is absent is skipped by the command itself, with a reason
//! in the report. The test then asserts the skip instead of the record.

mod common;

use common::{klon_env, stderr, stdout};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::PathBuf;

/// The committed manifest, as the binary embeds it.
const MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench/manifests/v1.toml"
));

/// A working directory and a fixture directory for one bench run.
///
/// The fixture directory decides the filesystem under every measured tree, so
/// `KLON_TEST_BTRFS_DIR` moves it onto the btrfs mount that the CI `loop-fs`
/// job builds. That is what lets the M5 test see the exact `btrfs fi du`
/// figure; without the variable the fixture stays in a temporary directory on
/// whatever this host runs.
struct Run {
    _tmp: tempfile::TempDir,
    cwd: PathBuf,
    bench_dir: PathBuf,
    /// A directory outside the temporary one, when the caller named a
    /// filesystem. `Drop` removes it.
    borrowed: bool,
}

impl Run {
    fn new() -> Run {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let (bench_dir, borrowed) = match std::env::var_os("KLON_TEST_BTRFS_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
        {
            Some(dir) => (dir.join(unique("bench")), true),
            None => (tmp.path().join("fixtures"), false),
        };
        std::fs::create_dir_all(&bench_dir).unwrap();
        Run {
            _tmp: tmp,
            cwd,
            bench_dir,
            borrowed,
        }
    }

    /// Run `bench` with the smoke shape and one sample per record.
    ///
    /// Every variable that changes a run is named here, so a developer who has
    /// one of them set gets the same result as the CI. `extra` comes last and
    /// wins.
    fn bench(&self, extra: &[(&str, &OsStr)], cell: &str) -> Value {
        let mut envs: Vec<(&str, &OsStr)> = vec![
            ("KLON_BENCH_SMOKE", OsStr::new("1")),
            ("KLON_BENCH_RUNS", OsStr::new("1")),
            ("KLON_BENCH_DIR", self.bench_dir.as_os_str()),
            ("KLON_FIXTURE", OsStr::new("")),
            ("KLON_BENCH_DROP_CACHES", OsStr::new("")),
            ("KLON_BENCH_INJECT_MISMATCH", OsStr::new("")),
            ("KLON_BENCH_ORDER_SEED", OsStr::new("")),
            ("KLON_BENCH_N", OsStr::new("")),
        ];
        envs.extend_from_slice(extra);
        let out = klon_env(&self.cwd, &envs, &["bench", "--cell", cell, "--json"]);
        assert!(out.status.success(), "bench failed: {}", stderr(&out));
        let text = stdout(&out);
        serde_json::from_str(&text)
            .unwrap_or_else(|err| panic!("not one JSON document: {err}\n{text}"))
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if self.borrowed {
            let _ = std::fs::remove_dir_all(&self.bench_dir);
        }
    }
}

/// A directory name that no other test and no parallel run shares.
fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{prefix}-{}-{nanos}", std::process::id())
}

/// The record of `report` whose backend is not the baseline.
fn klon_record(report: &Value) -> &Value {
    records(report)
        .iter()
        .find(|r| r["backend"] != "git-worktree-add")
        .expect("a klon record")
}

fn baseline_record(report: &Value) -> &Value {
    records(report)
        .iter()
        .find(|r| r["backend"] == "git-worktree-add")
        .expect("a baseline record")
}

fn records(report: &Value) -> &Vec<Value> {
    report["records"].as_array().expect("records")
}

fn numbers(value: &Value) -> Vec<f64> {
    value
        .as_array()
        .expect("an array of numbers")
        .iter()
        .map(|n| n.as_f64().expect("a number"))
        .collect()
}

/// The reason the command gave for skipping `cell`, when it skipped it.
fn skip_reason(report: &Value, cell: &str) -> Option<String> {
    report["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .find(|s| s["cell"] == cell)
        .map(|s| s["reason"].as_str().unwrap_or_default().to_string())
}

/// The C31 acceptance line: every v2 cell has a baseline row from the same
/// fixture. Both rows must name one fixture kind, one shape, and one hash.
fn assert_baseline_shares_the_fixture(report: &Value, cell: &str) {
    let klon = klon_record(report);
    let baseline = baseline_record(report);
    assert_eq!(
        records(report).len(),
        2,
        "one klon row and one baseline row"
    );
    for record in [klon, baseline] {
        assert_eq!(record["cell"], cell);
    }
    assert_eq!(klon["fixture"], baseline["fixture"], "one fixture kind");
    assert_eq!(klon["fixture_shape"], baseline["fixture_shape"]);
    assert_eq!(klon["profile_shape"], baseline["profile_shape"]);
    assert_eq!(klon["profile"], baseline["profile"]);
    assert_eq!(
        baseline["pass"],
        Value::Null,
        "the klon budget is not the baseline's"
    );
}

// --- M2 -------------------------------------------------------------------------

/// M2: the sample runs from the start of `add` to the moment the ignored state
/// of the new tree equals golden's. A plain `git worktree add` copies none of
/// it, so its row says it never reached the warm state.
#[test]
fn the_warm_cell_reports_the_time_to_a_warm_tree() {
    let run = Run::new();
    let report = run.bench(&[], "m2-warm-10k");
    assert_baseline_shares_the_fixture(&report, "m2-warm-10k");

    let klon = klon_record(&report);
    assert_eq!(klon["metric"], "M2");
    assert_eq!(klon["warm_reached"], true);
    assert_eq!(klon["correctness"]["ignored_manifest"], "match");
    assert_eq!(klon["timing_valid"], true);
    let samples = numbers(&klon["samples_ms"]);
    assert_eq!(samples.len(), 1, "one sample per KLON_BENCH_RUNS=1");
    assert!(samples[0] > 0.0, "found {samples:?}");
    assert_eq!(klon["pass"], true);

    // The handoff table says `never` for the baseline. The row says so too, and
    // still reports the wall time of the command it did run.
    let baseline = baseline_record(&report);
    assert_eq!(baseline["warm_reached"], false);
    assert!(numbers(&baseline["samples_ms"])[0] > 0.0);

    // The big cell is the same measurement behind the fixture variable.
    assert!(
        MANIFEST.contains("name = \"m2-warm-100k\""),
        "the manifest must carry the 100k sibling"
    );
    let big = run.bench(&[], "m2-warm-100k");
    assert!(
        skip_reason(&big, "m2-warm-100k").is_some_and(|why| why.contains("KLON_FIXTURE=100k")),
        "the 100k cell needs the fixture variable"
    );
}

// --- M3 -------------------------------------------------------------------------

/// The first C31 acceptance line: `bench --cell m3-zero-compile-rust --json`
/// reports `units_compiled: 0` on a warm golden. The baseline has no warm
/// `target/`, so it compiles the workspace again; that difference is the
/// metric.
#[test]
fn the_rust_build_cell_compiles_nothing_in_a_klon() {
    let run = Run::new();
    let report = run.bench(&[], "m3-zero-compile-rust");
    if let Some(why) = skip_reason(&report, "m3-zero-compile-rust") {
        println!("skipped: {why}");
        return;
    }
    assert_baseline_shares_the_fixture(&report, "m3-zero-compile-rust");

    let klon = klon_record(&report);
    assert_eq!(klon["metric"], "M3");
    assert_eq!(klon["fixture"], "rust");
    assert_eq!(
        klon["units_compiled"], 0,
        "a klon of a warm golden compiles zero units"
    );
    assert_eq!(klon["pass_units_compiled"], 0);
    assert_eq!(klon["correctness"]["build"], "ok");
    assert_eq!(klon["timing_valid"], true);
    assert_eq!(klon["pass"], true);
    // The path fixup rewrites golden's path inside a real build tree, so the
    // ignored state cannot be compared byte for byte. The report says why.
    let why = klon["correctness"]["ignored_manifest"]
        .as_str()
        .expect("a verdict");
    assert!(
        why.starts_with("not-applicable: the path fixup"),
        "found {why}"
    );

    let baseline = baseline_record(&report);
    assert!(
        baseline["units_compiled"].as_u64().expect("a count") > 0,
        "the baseline must compile the workspace again"
    );
}

/// The pnpm half of M3. The store sits beside golden, so the baseline can
/// install from it and the two rows differ only in what the klon brought with
/// it. The cell is skipped with a reason when pnpm, node, or tar is absent.
#[test]
fn the_pnpm_build_cell_installs_nothing_in_a_klon() {
    let run = Run::new();
    let report = run.bench(&[], "m3-zero-compile-pnpm");
    if let Some(why) = skip_reason(&report, "m3-zero-compile-pnpm") {
        println!("skipped: {why}");
        return;
    }
    assert_baseline_shares_the_fixture(&report, "m3-zero-compile-pnpm");

    let klon = klon_record(&report);
    assert_eq!(klon["fixture"], "pnpm");
    assert_eq!(klon["units_compiled"], 0);
    // The AC asks for exit 0 as well as a zero count.
    assert_eq!(klon["correctness"]["build"], "ok");
    assert_eq!(klon["timing_valid"], true);
    assert_eq!(klon["pass"], true);
}

// --- M5 -------------------------------------------------------------------------

/// The third C31 acceptance line: the cell reports `unique_bytes` and names the
/// method. On btrfs it is `btrfs-fi-du` and exact; on every other filesystem it
/// is the apparent size of the tree, marked `upper-bound`. The CI loop-fs job
/// runs the btrfs half.
#[test]
fn the_disk_cell_reports_unique_bytes_and_its_method() {
    let run = Run::new();
    let report = run.bench(&[("KLON_FIXTURE", OsStr::new("100k"))], "m5-disk-100k");
    assert_baseline_shares_the_fixture(&report, "m5-disk-100k");

    let klon = klon_record(&report);
    assert_eq!(klon["metric"], "M5");
    let bytes = klon["unique_bytes"].as_u64().expect("unique_bytes");
    assert!(bytes > 0, "an idle klon holds bytes of its own");
    let method = klon["method"].as_str().expect("a method");
    assert!(
        ["btrfs-fi-du", "upper-bound"].contains(&method),
        "unknown method {method}"
    );
    // This host runs ext4, so the figure is an upper bound. A btrfs runner
    // reports the exact one instead.
    let filesystem = report["environment"]["filesystem"]
        .as_str()
        .expect("a filesystem");
    let baseline = baseline_record(&report);
    let base_bytes = baseline["unique_bytes"].as_u64().expect("unique_bytes");
    println!("unique bytes by {method}: klon {bytes}, baseline {base_bytes}");
    if filesystem == "btrfs" {
        assert_eq!(
            method, "btrfs-fi-du",
            "btrfs must give the exact figure; check that btrfs-progs is on PATH \
             and that btrfs filesystem du runs without privileges here"
        );
        // On btrfs a klon shares its extents with golden, so its exclusive
        // figure may sit far below the baseline's. Which way the two fall is
        // the answer of the cell, not a rule the test may impose.
    } else {
        assert_eq!(
            method, "upper-bound",
            "{filesystem} has no exact figure, so the record marks the bound"
        );
        // A plain copy shares nothing: the klon carries golden's ignored state
        // and the baseline worktree carries none of it.
        assert!(
            bytes > base_bytes,
            "the klon holds the warm state: {bytes} against {base_bytes}"
        );
    }
    assert_eq!(baseline["method"], klon["method"], "one method per cell");
}

// --- M12 ------------------------------------------------------------------------

/// The second C31 acceptance line: `bench --cell m12-throughput-n6 --json`
/// reports a `ratio` field and the per-klon build times.
///
/// The test measures two builders so it stays under a minute; it then asserts
/// that the committed manifest still asks for six.
#[test]
fn the_throughput_cell_reports_the_ratio_and_every_build_time() {
    let run = Run::new();
    let report = run.bench(&[("KLON_BENCH_N", OsStr::new("2"))], "m12-throughput-n6");
    if let Some(why) = skip_reason(&report, "m12-throughput-n6") {
        println!("skipped: {why}");
        return;
    }
    assert_baseline_shares_the_fixture(&report, "m12-throughput-n6");

    let klon = klon_record(&report);
    assert_eq!(klon["metric"], "M12");
    assert_eq!(klon["fixture"], "rust");
    assert_eq!(klon["builders"], 2, "KLON_BENCH_N asked for two");
    let per_klon = numbers(&klon["per_klon_ms"]);
    assert_eq!(per_klon.len(), 2, "one wall time per builder");
    assert!(per_klon.iter().all(|ms| *ms > 0.0), "found {per_klon:?}");
    assert_eq!(
        klon["order"].as_array().expect("an order").len(),
        numbers(&klon["samples_ms"]).len(),
        "one position per solo sample"
    );

    let solo = klon["t_solo_ms"].as_f64().expect("t_solo_ms");
    let wall = klon["t_wall6_ms"].as_f64().expect("t_wall6_ms");
    let ratio = klon["ratio"].as_f64().expect("ratio");
    assert!(solo > 0.0 && wall > 0.0, "found {solo} and {wall}");
    assert_eq!(
        solo,
        klon["p50_ms"].as_f64().expect("p50_ms"),
        "t_solo is the median of the solo samples"
    );
    assert!(
        (ratio - 2.0 * solo / wall).abs() < 1e-9,
        "ratio {ratio} must be builders x t_solo / t_wall"
    );
    // Every concurrent build finished inside the wall time of the whole run.
    for ms in &per_klon {
        assert!(*ms <= wall + 1.0, "{ms} ms is outside the {wall} ms wall");
    }
    assert_eq!(klon["pass_ratio"], 0.80);
    assert_eq!(klon["correctness"]["build"], "ok");
    assert_eq!(klon["timing_valid"], true);

    // The klon builds run under `gh klon run`, so the jobserver bounds them.
    // The baseline builds bare and reports no token count.
    assert!(
        klon["tokens"].as_u64().expect("a token count") > 0,
        "a klon build runs with build slots"
    );
    assert_eq!(baseline_record(&report)["tokens"], Value::Null);

    // The committed manifest still asks for six builders and the 0.80 rule.
    assert!(
        MANIFEST.contains("\nbuilders = 6\n"),
        "the committed cell must ask for six builders"
    );
    assert!(
        MANIFEST.contains("\npass_ratio = 0.80\n"),
        "the committed cell must keep the 0.80 pass rule"
    );
}
