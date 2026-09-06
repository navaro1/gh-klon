//! The JSON schema test (spec §7 C4, R9). The tables below are the documented
//! field set of each schema. A command that drops a field or changes its type
//! fails here. A command that adds a field passes: an addition is compatible.

mod common;

use common::{klon, stderr, stdout, Fixture};
use serde_json::{json, Value};

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
}

type Fields = &'static [(&'static str, Ty)];

const ADD: Fields = &[
    ("schema", Ty::Str),
    ("path", Ty::Str),
    ("branch", Ty::Str),
    ("head", Ty::Str),
    ("backend", Ty::Str),
    ("duration_ms", Ty::Num),
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
    // The C24 radar. `behind` is null when klon could not measure the klon.
    ("vs_base", Ty::Str),
    ("vs_siblings", Ty::Str),
    ("behind", Ty::NumOrNull),
];

const RM: Fields = &[
    ("schema", Ty::Str),
    ("path", Ty::Str),
    ("branch", Ty::StrOrNull),
    ("trash", Ty::StrOrNull),
];

const STOP: Fields = &[
    ("schema", Ty::Str),
    ("path", Ty::Str),
    ("name", Ty::Str),
    ("found", Ty::Num),
    ("terminated", Ty::Num),
    ("killed", Ty::Num),
    ("survivors", Ty::Arr),
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
    assert_eq!(list["schema"], "klon.list/1");
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

// --- The checker itself ------------------------------------------------------

/// A hand-built `klon.add/1` object without `path`. The AC: the check fails.
#[test]
fn an_object_without_path_fails_the_add_schema() {
    let doc = json!({
        "schema": "klon.add/1",
        "branch": "feature",
        "head": "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
        "backend": "copy",
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
        "duration_ms": 12,
        "spare_used": true,
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
        "vs_base": Value::Null,
        "vs_siblings": "clean",
        "behind": 0,
    });
    let why = check(&bad_behind, LIST_ROW).expect_err("a null vs_base must fail");
    assert!(why.contains("vs_base"), "unexpected reason: {why}");

    let bad = json!({
        "path": Value::Null,
        "branch": "feature",
        "head": "0f0f",
        "dirty": false,
        "locked": false,
        "ip": "127.0.0.2",
        "vs_base": "clean",
        "vs_siblings": "clean",
        "behind": 0,
    });
    let why = check(&bad, LIST_ROW).expect_err("a null path must fail");
    assert!(why.contains("path"), "unexpected reason: {why}");
}
