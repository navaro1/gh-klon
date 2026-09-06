//! The JSON schema test (spec §7 C4, R9). The tables below are the documented
//! field set of each schema. A command that drops a field or changes its type
//! fails here. A command that adds a field passes: an addition is compatible.

mod common;

use common::{git_ok, klon, klon_env, stderr, stdout, Fixture};
use serde_json::{json, Value};
use std::ffi::OsStr;

const SEED: u64 = 42;

// --- The documented field sets ----------------------------------------------

/// The JSON type of a documented field.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ty {
    Str,
    Num,
    Bool,
    Obj,
    Arr,
    /// A string that is null when the value does not apply.
    StrOrNull,
    /// A number that is null when the value does not apply.
    NumOrNull,
    /// A boolean that is null when the value does not apply.
    BoolOrNull,
}

type Fields = &'static [(&'static str, Ty)];

const ADD: Fields = &[
    ("schema", Ty::Str),
    ("path", Ty::Str),
    ("branch", Ty::Str),
    ("head", Ty::Str),
    ("backend", Ty::Str),
    // C9: true when the hot spare served the add.
    ("spare", Ty::Bool),
    ("duration_ms", Ty::Num),
    // The C12 warm list. It is empty for every backend but `copy` and for a
    // klon whose ignored directories all fitted inline.
    ("warming", Ty::Arr),
];

const LIST: Fields = &[("schema", Ty::Str), ("klons", Ty::Arr)];

const LIST_ROW: Fields = &[
    ("path", Ty::Str),
    ("branch", Ty::StrOrNull),
    ("head", Ty::Str),
    ("dirty", Ty::Bool),
    ("locked", Ty::Bool),
    // The C16 loopback address. It is null for a klon with no `.klon/env`.
    ("ip", Ty::StrOrNull),
    // The C30 extras. `disk_bytes` only bounds the delta when `disk_exact` is
    // false, and `pr` and `checks` are null without a pull request or when
    // `gh` did not answer.
    ("disk_bytes", Ty::Num),
    ("disk_exact", Ty::Bool),
    ("procs", Ty::Num),
    ("rss_bytes", Ty::Num),
    ("pr", Ty::NumOrNull),
    ("checks", Ty::StrOrNull),
    // The C12 warm list: the directories a detached warm process still owes.
    ("warming", Ty::Arr),
    // The C24 radar. `behind` is null when klon could not measure the klon.
    ("vs_base", Ty::Str),
    ("vs_siblings", Ty::Str),
    ("behind", Ty::NumOrNull),
];

/// C14. `head_before` and `head_after` are null in a repository with no commit.
const UP: Fields = &[
    ("schema", Ty::Str),
    ("base", Ty::Str),
    ("head_before", Ty::StrOrNull),
    ("head_after", Ty::StrOrNull),
    ("steps_run", Ty::Num),
    ("spare_started", Ty::Bool),
];

/// C14. `sync --json` prints one of these per klon, one per line.
const SYNC: Fields = &[
    ("schema", Ty::Str),
    ("branch", Ty::Str),
    ("path", Ty::Str),
    ("action", Ty::Str),
    ("head_before", Ty::StrOrNull),
    ("head_after", Ty::StrOrNull),
    ("message", Ty::Str),
];

const RM: Fields = &[
    ("schema", Ty::Str),
    ("path", Ty::Str),
    ("branch", Ty::StrOrNull),
    ("trash", Ty::StrOrNull),
];

/// The C25 `merge` document. `hook` names the gate that ran and is null when
/// the repository has none. `conflicts` is empty for every merge that landed.
const MERGE: Fields = &[
    ("schema", Ty::Str),
    ("branch", Ty::Str),
    ("base", Ty::Str),
    ("head_before", Ty::Str),
    ("head_after", Ty::Str),
    ("mode", Ty::Str),
    ("removed", Ty::Bool),
    ("hook", Ty::StrOrNull),
    ("conflicts", Ty::Arr),
];

const STOP: Fields = &[
    ("schema", Ty::Str),
    ("path", Ty::Str),
    ("name", Ty::Str),
    ("found", Ty::Num),
    ("terminated", Ty::Num),
    ("killed", Ty::Num),
    ("survivors", Ty::Arr),
    ("cgroups", Ty::Arr),
];

const DOCTOR: Fields = &[
    ("schema", Ty::Str),
    ("timestamp", Ty::Str),
    ("git_version", Ty::Str),
    ("filesystem", Ty::Str),
    ("backend", Ty::Str),
    ("backend_reason", Ty::Str),
    ("features", Ty::Obj),
    ("journal", Ty::Arr),
    ("repaired", Ty::Arr),
];

const DOCTOR_FEATURE: Fields = &[("status", Ty::Str), ("detail", Ty::Str)];

const DOCTOR_JOURNAL_ROW: Fields = &[
    ("name", Ty::Str),
    ("op", Ty::Str),
    ("state", Ty::Str),
    ("path", Ty::Str),
    ("branch", Ty::StrOrNull),
    ("started", Ty::Str),
];

const BENCH: Fields = &[
    ("schema", Ty::Str),
    ("timestamp", Ty::Str),
    ("release", Ty::Bool),
    ("smoke", Ty::Bool),
    ("manifest", Ty::Obj),
    ("environment", Ty::Obj),
    ("records", Ty::Arr),
    ("skipped", Ty::Arr),
];

const BENCH_MANIFEST: Fields = &[
    ("version", Ty::Num),
    ("path", Ty::Str),
    ("seed", Ty::Num),
    ("warm_runs", Ty::Num),
    ("cold_runs", Ty::Num),
];

const BENCH_ENVIRONMENT: Fields = &[
    ("hostname", Ty::Str),
    ("cpu_model", Ty::Str),
    ("cpu_cores", Ty::Num),
    ("memory_total_kb", Ty::Num),
    ("os", Ty::Str),
    ("kernel", Ty::Str),
    ("arch", Ty::Str),
    ("bench_dir", Ty::Str),
    ("filesystem", Ty::Str),
    ("mount_options", Ty::Str),
    ("git_version", Ty::Str),
    ("klon_version", Ty::Str),
    ("klon_commit", Ty::Str),
    ("fixture_hash", Ty::Str),
    ("order_seed", Ty::Num),
    ("drop_caches", Ty::Str),
];

const BENCH_RECORD: Fields = &[
    ("cell", Ty::Str),
    ("metric", Ty::Str),
    ("profile", Ty::Str),
    ("profile_shape", Ty::Obj),
    ("backend", Ty::Str),
    // C9 sets `spare` when a hot spare served the add.
    ("spare", Ty::Bool),
    ("cold", Ty::Bool),
    ("cache_drop", Ty::Str),
    ("timer", Ty::Str),
    ("runs", Ty::Num),
    ("order", Ty::Arr),
    ("samples_ms", Ty::Arr),
    ("p50_ms", Ty::Num),
    ("p95_ms", Ty::Num),
    // The M4 cell alone reports the first and the steady series.
    ("first_p50_ms", Ty::NumOrNull),
    ("steady_p50_ms", Ty::NumOrNull),
    ("steady_samples_ms", Ty::Arr),
    ("correctness", Ty::Obj),
    ("timing_valid", Ty::Bool),
    ("pass_p50_ms", Ty::Num),
    ("pass_steady_p50_ms", Ty::NumOrNull),
    // Null for the baseline, which the klon budget does not bind.
    ("pass", Ty::BoolOrNull),
];

const BENCH_PROFILE_SHAPE: Fields = &[
    ("tracked_files", Ty::Num),
    ("dirs", Ty::Num),
    ("ignored_files", Ty::Num),
    ("ignored_file_bytes", Ty::Num),
    ("changed_files", Ty::Num),
    ("added_files", Ty::Num),
];

const BENCH_CORRECTNESS: Fields = &[
    ("matched", Ty::Bool),
    ("ignored_manifest", Ty::Str),
    ("tracked", Ty::Str),
    ("status", Ty::Str),
    ("removal", Ty::Str),
];

const DOCTOR_REPAIR_ROW: Fields = &[
    ("name", Ty::Str),
    ("state", Ty::Str),
    ("path", Ty::Str),
    ("action", Ty::Str),
];

// --- The checker -------------------------------------------------------------

fn matches(value: &Value, ty: Ty) -> bool {
    match ty {
        Ty::Str => value.is_string(),
        Ty::Num => value.is_number(),
        Ty::Bool => value.is_boolean(),
        Ty::Obj => value.is_object(),
        Ty::Arr => value.is_array(),
        Ty::StrOrNull => value.is_string() || value.is_null(),
        Ty::NumOrNull => value.is_number() || value.is_null(),
        Ty::BoolOrNull => value.is_boolean() || value.is_null(),
    }
}

/// Check one object against a documented field set. Every documented field must
/// be present with the documented type. An extra field is allowed.
fn check(value: &Value, fields: Fields) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("expected an object, found {value}"))?;
    for (name, ty) in fields {
        match object.get(*name) {
            None => return Err(format!("the field {name} is missing")),
            Some(found) if !matches(found, *ty) => {
                return Err(format!("the field {name} is not {ty:?}: {found}"))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn check_ok(value: &Value, fields: Fields) {
    if let Err(why) = check(value, fields) {
        panic!("{why}\nin {value}");
    }
}

fn check_rows(value: &Value, field: &str, fields: Fields) {
    for row in value[field].as_array().expect("an array") {
        check_ok(row, fields);
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("not one JSON document: {err}\n{text}"))
}

// --- The commands ------------------------------------------------------------

/// One `add --json`, `list --json`, `rm --json`, `doctor --json` round on a
/// small fixture. One fixture keeps the test under a second.
#[test]
fn every_command_matches_its_documented_schema() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 3);

    let out = klon(&fx.golden, &["add", "--json", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let add = parse(&stdout(&out));
    check_ok(&add, ADD);
    assert_eq!(add["schema"], "klon.add/1");
    // The probe picks the backend from the filesystem under the fixture (C5).
    // ext4 and macOS give `copy`; an xfs or btrfs checkout gives `reflink-walk`.
    assert!(
        ["copy", "reflink-walk"].contains(&add["backend"].as_str().expect("a backend name")),
        "unknown backend {}",
        add["backend"]
    );
    assert_eq!(add["branch"], "feature");

    let out = klon(&fx.golden, &["list", "--json"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    let list = parse(&stdout(&out));
    check_ok(&list, LIST);
    assert_eq!(list["schema"], "klon.list/2");
    assert_eq!(list["klons"].as_array().expect("an array").len(), 1);
    check_rows(&list, "klons", LIST_ROW);

    let out = klon(&fx.golden, &["doctor", "--json"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let doctor = parse(&stdout(&out));
    check_ok(&doctor, DOCTOR);
    assert_eq!(doctor["schema"], "klon.doctor/1");
    for (name, feature) in doctor["features"].as_object().expect("an object") {
        if let Err(why) = check(feature, DOCTOR_FEATURE) {
            panic!("feature {name}: {why}");
        }
    }
    check_rows(&doctor, "journal", DOCTOR_JOURNAL_ROW);
    check_rows(&doctor, "repaired", DOCTOR_REPAIR_ROW);

    // C14. The fixture has no origin, so `up` skips the fetch and the merge
    // and `sync` falls back to `base`; the document shape is the same.
    let out = klon(&fx.golden, &["up", "--json"]);
    assert!(out.status.success(), "up failed: {}", stderr(&out));
    let up = parse(&stdout(&out));
    check_ok(&up, UP);
    assert_eq!(up["schema"], "klon.up/1");
    assert_eq!(up["base"], "main");

    let out = klon(&fx.golden, &["sync", "--json", "feature", "--check"]);
    assert!(
        out.status.success(),
        "sync --check failed: {}",
        stderr(&out)
    );
    let sync = parse(&stdout(&out));
    check_ok(&sync, SYNC);
    assert_eq!(sync["schema"], "klon.sync/1");
    assert_eq!(sync["action"], "check");

    let out = klon(&fx.golden, &["sync", "--json", "--all"]);
    assert!(out.status.success(), "sync --all failed: {}", stderr(&out));
    for line in stdout(&out).lines() {
        let row = parse(line);
        check_ok(&row, SYNC);
        assert_eq!(row["schema"], "klon.sync/1");
    }

    let out = klon(&fx.golden, &["stop", "--json", "feature"]);
    assert!(out.status.success(), "stop failed: {}", stderr(&out));
    let stop = parse(&stdout(&out));
    check_ok(&stop, STOP);
    assert_eq!(stop["schema"], "klon.stop/1");
    assert_eq!(stop["name"], "feature");

    let out = klon(&fx.golden, &["rm", "--json", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
    let rm = parse(&stdout(&out));
    check_ok(&rm, RM);
    assert_eq!(rm["schema"], "klon.rm/1");
    assert_eq!(rm["branch"], "feature");
    assert!(
        rm["trash"].is_string(),
        "the klon reaches the trash on ext4"
    );
}

/// `merge --json` (C25). The merge removes the klon, so this round runs on its
/// own fixture. The repository gets a committer identity, because `git merge`
/// writes a commit and the harness keeps the global config empty.
#[test]
fn the_merge_report_matches_its_documented_schema() {
    let fx = Fixture::generate(SEED, 30, 3, 3, 2);
    git_ok(&fx.golden, &["config", "user.name", "klon"]);
    git_ok(&fx.golden, &["config", "user.email", "klon@example.com"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let out = klon(&fx.golden, &["merge", "--json", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    let merge = parse(&stdout(&out));
    check_ok(&merge, MERGE);
    assert_eq!(merge["schema"], "klon.merge/1");
    assert_eq!(merge["branch"], "feature");
    assert_eq!(merge["base"], "main");
    assert_eq!(merge["mode"], "no-ff");
    assert_eq!(merge["removed"], true);
    assert!(merge["hook"].is_null(), "the fixture has no gate");
    assert!(merge["conflicts"].as_array().expect("an array").is_empty());
}

/// `doctor --repair` fills `repaired` with rows of the documented shape.
#[test]
fn a_repair_row_matches_the_documented_schema() {
    let fx = Fixture::generate(SEED, 20, 2, 2, 2);
    let journal = fx.golden.join(".git").join("klon").join("journal");
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::write(
        journal.join("stale.json"),
        json!({
            "version": 1,
            "op": "add",
            "state": "planned",
            "path": fx.golden.parent().unwrap().join("golden.wt").join("gone"),
            "branch": "feature",
            "started": "2026-09-05T10:00:00Z",
        })
        .to_string(),
    )
    .unwrap();

    let out = klon(&fx.golden, &["doctor", "--json", "--repair"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let doctor = parse(&stdout(&out));
    check_ok(&doctor, DOCTOR);
    check_rows(&doctor, "repaired", DOCTOR_REPAIR_ROW);
    assert!(
        !doctor["repaired"].as_array().unwrap().is_empty(),
        "the repair must report an action"
    );
    assert!(
        doctor["journal"].as_array().unwrap().is_empty(),
        "the repair must close the entry"
    );
}

/// `bench --json` (C8). The run uses the smoke profiles and three samples, so
/// it takes seconds; the document shape is the same as a full run's.
#[test]
fn the_bench_report_matches_its_documented_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let fixtures = tmp.path().join("fixtures");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&fixtures).unwrap();
    let out = klon_env(
        &cwd,
        &[
            ("KLON_BENCH_SMOKE", OsStr::new("1")),
            ("KLON_BENCH_RUNS", OsStr::new("3")),
            ("KLON_BENCH_DIR", fixtures.as_os_str()),
        ],
        &["bench", "--cell", "m1-add-10k", "--json"],
    );
    assert!(out.status.success(), "bench failed: {}", stderr(&out));
    let bench = parse(&stdout(&out));
    check_ok(&bench, BENCH);
    assert_eq!(bench["schema"], "klon.bench/1");
    check_ok(&bench["manifest"], BENCH_MANIFEST);
    check_ok(&bench["environment"], BENCH_ENVIRONMENT);
    check_rows(&bench, "records", BENCH_RECORD);
    let records = bench["records"].as_array().expect("an array");
    assert_eq!(
        records.len(),
        3,
        "the direct klon record, the spare klon record, and the baseline record"
    );
    for record in records {
        check_ok(&record["profile_shape"], BENCH_PROFILE_SHAPE);
        check_ok(&record["correctness"], BENCH_CORRECTNESS);
    }
}

// --- The checker itself ------------------------------------------------------

/// A hand-built `klon.add/1` object without `path`. The AC: the check fails.
#[test]
fn an_object_without_path_fails_the_add_schema() {
    let doc = json!({
        "schema": "klon.add/1",
        "branch": "feature",
        "head": "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
        "backend": "copy",
        "spare": false,
        "duration_ms": 12,
    });
    let why = check(&doc, ADD).expect_err("a missing path must fail");
    assert!(why.contains("path"), "unexpected reason: {why}");
}

#[test]
fn a_retyped_field_fails() {
    let doc = json!({
        "schema": "klon.add/1",
        "path": "/tmp/x",
        "branch": "feature",
        "head": "0f0f",
        "backend": "copy",
        "spare": false,
        "duration_ms": "12",
    });
    let why = check(&doc, ADD).expect_err("a string duration must fail");
    assert!(why.contains("duration_ms"), "unexpected reason: {why}");
}

#[test]
fn an_added_field_passes() {
    let doc = json!({
        "schema": "klon.add/1",
        "path": "/tmp/x",
        "branch": "feature",
        "head": "0f0f",
        "backend": "copy",
        "spare": false,
        "duration_ms": 12,
        "warming": [],
        "extra": true,
    });
    check_ok(&doc, ADD);
}

#[test]
fn a_null_is_only_allowed_where_the_table_says_so() {
    let row = json!({
        "path": "/tmp/x",
        "branch": Value::Null,
        "head": "0f0f",
        "dirty": false,
        "locked": false,
        "ip": "127.0.0.2",
        "disk_bytes": 630,
        "disk_exact": false,
        "procs": 0,
        "rss_bytes": 0,
        "pr": 7,
        "checks": "pass",
        "warming": [],
        "vs_base": "clean",
        "vs_siblings": "clean",
        "behind": 0,
    });
    check_ok(&row, LIST_ROW);

    // The radar reports a null `behind` for a klon it could not measure, and a
    // string in the other two columns whatever happened.
    let unmeasured = json!({
        "path": "/tmp/x",
        "branch": "feature",
        "head": "0f0f",
        "dirty": false,
        "locked": false,
        "ip": "127.0.0.2",
        "disk_bytes": 630,
        "disk_exact": false,
        "procs": 0,
        "rss_bytes": 0,
        "pr": Value::Null,
        "checks": Value::Null,
        "warming": [],
        "vs_base": "-",
        "vs_siblings": "-",
        "behind": Value::Null,
    });
    check_ok(&unmeasured, LIST_ROW);

    let bad_behind = json!({
        "path": "/tmp/x",
        "branch": "feature",
        "head": "0f0f",
        "dirty": false,
        "locked": false,
        "ip": "127.0.0.2",
        "disk_bytes": 630,
        "disk_exact": false,
        "procs": 0,
        "rss_bytes": 0,
        "pr": 7,
        "checks": "pass",
        "warming": [],
        "vs_base": Value::Null,
        "vs_siblings": "clean",
        "behind": 0,
    });
    let why = check(&bad_behind, LIST_ROW).expect_err("a null vs_base must fail");
    assert!(why.contains("vs_base"), "unexpected reason: {why}");

    // A checks verdict is a string, never a number or null-by-typo.
    let bad_checks = json!({
        "path": "/tmp/x",
        "branch": "feature",
        "head": "0f0f",
        "dirty": false,
        "locked": false,
        "ip": "127.0.0.2",
        "disk_bytes": 630,
        "disk_exact": false,
        "procs": 0,
        "rss_bytes": 0,
        "pr": 7,
        "checks": 1,
        "vs_base": "clean",
        "vs_siblings": "clean",
        "behind": 0,
    });
    let why = check(&bad_checks, LIST_ROW).expect_err("a number checks must fail");
    assert!(why.contains("checks"), "unexpected reason: {why}");

    let bad = json!({
        "path": Value::Null,
        "branch": "feature",
        "head": "0f0f",
        "dirty": false,
        "locked": false,
        "ip": "127.0.0.2",
        "disk_bytes": 630,
        "disk_exact": false,
        "procs": 0,
        "rss_bytes": 0,
        "pr": 7,
        "checks": "pass",
        "warming": [],
        "vs_base": "clean",
        "vs_siblings": "clean",
        "behind": 0,
    });
    let why = check(&bad, LIST_ROW).expect_err("a null path must fail");
    assert!(why.contains("path"), "unexpected reason: {why}");
}
