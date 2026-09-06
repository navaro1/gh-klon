//! Acceptance tests for C27: `gh klon claim`, the claim table under `flock`,
//! the `list` overlap mark, the `check` escape list, and the release that `rm`
//! runs. Each test drives the real command against a generated fixture.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use common::{git_ok, identity, klon, klon_env, stderr, stdout, Fixture, BIN};
use serde_json::Value;

const SEED: u64 = 27;

// --- Helpers -----------------------------------------------------------------

fn fixture() -> Fixture {
    Fixture::generate(SEED, 20, 4, 3, 2)
}

/// A fixture with a committer identity and a committed `[proof]` table. The
/// file must be committed: an untracked file makes golden dirty.
fn proof_fixture() -> Fixture {
    let fx = fixture();
    identity(&fx.golden);
    fs::write(
        fx.golden.join(".klon.toml"),
        "[proof]\nsteps = [\"true\"]\n",
    )
    .expect("write .klon.toml");
    git_ok(&fx.golden, &["add", ".klon.toml"]);
    git_ok(&fx.golden, &["commit", "-qm", "add the proof steps"]);
    fx
}

/// `gh klon add <branch>`, with the klon path as the answer. A branch the
/// fixture does not have is created from base.
fn add(fx: &Fixture, branch: &str) -> PathBuf {
    let out = klon(&fx.golden, &["add", branch]);
    assert!(
        out.status.success(),
        "add {branch} failed: {}",
        stderr(&out)
    );
    fx.klon_path(branch)
}

/// Run klon and require success.
fn ok(fx: &Fixture, args: &[&str]) -> Output {
    let out = klon(&fx.golden, args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    out
}

/// `<golden>/.git/klon/claims.json`.
fn claims_file(fx: &Fixture) -> PathBuf {
    fx.golden.join(".git").join("klon").join("claims.json")
}

/// The whole claim table, or an empty one when no file exists yet.
fn table(fx: &Fixture) -> Value {
    match fs::read_to_string(claims_file(fx)) {
        Ok(text) => serde_json::from_str(&text).expect("the claim table is one JSON document"),
        Err(_) => serde_json::json!({"version": 1, "claims": []}),
    }
}

/// The paths one klon owns, in file order.
fn paths_of(fx: &Fixture, branch: &str) -> Vec<String> {
    table(fx)["claims"]
        .as_array()
        .expect("an array")
        .iter()
        .filter(|claim| claim["klon"] == branch)
        .map(|claim| claim["path"].as_str().expect("a path").to_string())
        .collect()
}

/// The number of rows in the table.
fn rows(fx: &Fixture) -> usize {
    table(fx)["claims"].as_array().expect("an array").len()
}

/// `gh klon check <branch>` with an isolated approval store, so the test never
/// writes into the user's config directory.
fn check(fx: &Fixture, branch: &str) -> Output {
    klon_env(
        &fx.golden,
        &[(
            "KLON_CONFIG_HOME",
            fx.golden.parent().expect("a parent").as_os_str(),
        )],
        &["--yes", "check", branch],
    )
}

/// The receipt of one commit, parsed.
fn receipt(fx: &Fixture, commit: &str) -> Value {
    let file = fx
        .golden
        .join(".git")
        .join("klon")
        .join("receipts")
        .join(format!("{commit}.json"));
    let text =
        fs::read_to_string(&file).unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
    serde_json::from_str(&text).expect("a receipt is one JSON document")
}

fn head(dir: &Path) -> String {
    git_ok(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// The `list` line of one branch.
fn line_for(text: &str, branch: &str) -> String {
    text.lines()
        .find(|line| line.split_whitespace().nth(1) == Some(branch))
        .unwrap_or_else(|| panic!("no list line for {branch} in:\n{text}"))
        .to_string()
}

/// Start one `claim` process without waiting for it. The concurrency test
/// starts both before it reads either, so the two really do race.
fn spawn_claim(fx: &Fixture, branch: &str, path: &str) -> Child {
    Command::new(BIN)
        .args(["claim", branch, path])
        .current_dir(&fx.golden)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KLON_SPARE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gh-klon claim")
}

// --- The acceptance lines ----------------------------------------------------

/// AC: two concurrent `claim` calls for `src/a` from two klons leave exactly
/// one winner, twenty rounds in a row. The lock decides; the loser hears why.
#[test]
fn two_concurrent_claims_of_one_path_leave_exactly_one_winner() {
    let fx = fixture();
    add(&fx, "left");
    add(&fx, "right");
    for round in 0..20 {
        let children: Vec<Child> = ["left", "right"]
            .iter()
            .map(|branch| spawn_claim(&fx, branch, "src/a"))
            .collect();
        let outs: Vec<Output> = children
            .into_iter()
            .map(|child| child.wait_with_output().expect("wait for claim"))
            .collect();
        let winners = outs.iter().filter(|out| out.status.success()).count();
        assert_eq!(
            winners,
            1,
            "round {round}: exactly one claim must win\nfirst: {}\nsecond: {}",
            stderr(&outs[0]),
            stderr(&outs[1])
        );
        let loser = outs
            .iter()
            .find(|out| !out.status.success())
            .expect("one loser");
        assert!(
            stderr(loser).contains("claim conflict: src/a held by"),
            "the loser must name the holder: {}",
            stderr(loser)
        );
        assert_eq!(rows(&fx), 1, "round {round}: one winner writes one row");
        // Reset for the next round.
        fs::remove_file(claims_file(&fx)).expect("remove the claim table");
    }
}

/// AC: a claim on `src/app` does not conflict with a claim on `src/apple`, and
/// it does conflict with a claim on `src/app/main.rs`.
#[test]
fn a_prefix_conflicts_only_at_a_component_boundary() {
    let fx = fixture();
    add(&fx, "left");
    add(&fx, "right");
    ok(&fx, &["claim", "left", "src/app"]);

    // A sibling whose name starts with the same letters is free.
    ok(&fx, &["claim", "right", "src/apple"]);
    assert_eq!(paths_of(&fx, "right"), vec!["src/apple".to_string()]);

    // A path under the claimed directory is not.
    let out = klon(&fx.golden, &["claim", "right", "src/app/main.rs"]);
    assert!(!out.status.success(), "a path under a claim must refuse");
    assert!(
        stderr(&out).contains("claim conflict: src/app/main.rs held by left"),
        "the message must name the path and the holder: {}",
        stderr(&out)
    );
    assert_eq!(rows(&fx), 2, "the refused claim writes nothing");
}

/// AC, the other direction: a klon that holds `src/app/main.rs` blocks a claim
/// on the directory above it.
#[test]
fn a_claim_conflicts_with_the_directory_above_a_held_file() {
    let fx = fixture();
    add(&fx, "left");
    add(&fx, "right");
    ok(&fx, &["claim", "right", "src/app/main.rs"]);
    let out = klon(&fx.golden, &["claim", "left", "src/app"]);
    assert!(
        !out.status.success(),
        "a claim above a held file must refuse"
    );
    assert!(
        stderr(&out).contains("claim conflict: src/app held by right"),
        "the message must name the path and the holder: {}",
        stderr(&out)
    );
    assert_eq!(rows(&fx), 1, "the refused claim writes nothing");
}

/// AC: `check` on a klon that changed a file outside its claims writes a
/// receipt with `claim_escape`, and names each escaped path on stderr.
#[test]
fn check_records_every_changed_path_outside_the_claims() {
    let fx = proof_fixture();
    let work = add(&fx, "work");
    fs::write(work.join("d000").join("mine.txt"), "mine\n").unwrap();
    fs::write(work.join("outside.txt"), "outside\n").unwrap();
    git_ok(&work, &["add", "-A"]);
    git_ok(
        &work,
        &["commit", "-qm", "one file inside d000 and one outside"],
    );
    ok(&fx, &["claim", "work", "d000"]);

    let out = check(&fx, "work");
    assert!(out.status.success(), "check failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("claim escape: outside.txt"),
        "check must name the escaped path: {}",
        stderr(&out)
    );
    let record = receipt(&fx, &head(&work));
    assert_eq!(
        record["claim_escape"],
        serde_json::json!(["outside.txt"]),
        "the receipt holds the escaped path and no other"
    );
    assert_eq!(
        record["status"], "pass",
        "an escape does not fail the check"
    );
}

/// A klon that claimed nothing owns nothing, so nothing it changed escapes.
#[test]
fn a_klon_with_no_claim_has_no_escape() {
    let fx = proof_fixture();
    let work = add(&fx, "work");
    fs::write(work.join("outside.txt"), "outside\n").unwrap();
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-qm", "one file"]);

    let out = check(&fx, "work");
    assert!(out.status.success(), "check failed: {}", stderr(&out));
    assert!(
        !stderr(&out).contains("claim escape"),
        "a klon with no claim escapes nothing: {}",
        stderr(&out)
    );
    let record = receipt(&fx, &head(&work));
    assert_eq!(record["claim_escape"], serde_json::json!([]));
}

/// AC: `rm` removes the claims of the klon it removes, and only those.
#[test]
fn rm_releases_the_claims_of_the_klon_it_removes() {
    let fx = fixture();
    add(&fx, "left");
    add(&fx, "right");
    ok(&fx, &["claim", "left", "src/app", "docs"]);
    ok(&fx, &["claim", "right", "src/apple"]);

    ok(&fx, &["rm", "left"]);
    assert!(
        paths_of(&fx, "left").is_empty(),
        "rm must release every claim of the klon"
    );
    assert_eq!(
        paths_of(&fx, "right"),
        vec!["src/apple".to_string()],
        "rm must leave the other klon alone"
    );
    // The freed path is free for the next klon.
    ok(&fx, &["claim", "right", "src/app/main.rs"]);
}

/// The path rules: no `..`, no empty path, no klon root, no symlinked
/// ancestor, and nothing outside the tree. A refused call writes nothing.
#[test]
fn a_claim_refuses_a_path_that_leaves_the_klon() {
    let fx = fixture();
    let work = add(&fx, "work");
    fs::create_dir_all(work.join("real")).unwrap();
    std::os::unix::fs::symlink(work.join("real"), work.join("link")).unwrap();
    let outside = fx.golden.join("f2.txt");

    for (path, reason) in [
        ("..", "holds a .. component"),
        ("src/../../elsewhere", "holds a .. component"),
        ("", "the path is empty"),
        (".", "names the klon root"),
        ("link/a", "is a symlink"),
        ("/etc/passwd", "is outside the klon"),
        (
            outside.to_str().expect("a text path"),
            "is outside the klon",
        ),
    ] {
        let out = klon(&fx.golden, &["claim", "work", path]);
        assert!(!out.status.success(), "{path} must be refused");
        assert!(
            stderr(&out).contains(reason),
            "{path} must say {reason}: {}",
            stderr(&out)
        );
    }
    // A claim with no path at all names nothing to own.
    let out = klon(&fx.golden, &["claim", "work"]);
    assert!(!out.status.success(), "a claim needs a path");
    assert!(
        stderr(&out).contains("name at least one path"),
        "{}",
        stderr(&out)
    );
    assert_eq!(rows(&fx), 0, "no refused claim reaches the table");

    // An absolute path inside the klon lands as the relative one, and a
    // directory reads as a directory.
    ok(
        &fx,
        &["claim", "work", work.join("real").to_str().expect("a path")],
    );
    assert_eq!(paths_of(&fx, "work"), vec!["real".to_string()]);
    assert_eq!(table(&fx)["claims"][0]["kind"], "dir");
    // A path that does not exist yet is a file, and a repeated claim of one
    // path is not a second row.
    ok(&fx, &["claim", "work", "src/later.rs", "src/later.rs"]);
    assert_eq!(rows(&fx), 2);
    assert_eq!(table(&fx)["claims"][1]["kind"], "file");
}

/// `--release` gives back the named paths, or every claim of the klon when it
/// names none.
#[test]
fn a_release_gives_back_one_path_or_every_path() {
    let fx = fixture();
    add(&fx, "work");
    ok(&fx, &["claim", "work", "src/app", "docs", "README.md"]);
    assert_eq!(paths_of(&fx, "work").len(), 3);

    let out = ok(&fx, &["claim", "--release", "work", "docs"]);
    assert!(stdout(&out).contains("releases docs"), "{}", stdout(&out));
    assert_eq!(
        paths_of(&fx, "work"),
        vec!["src/app".to_string(), "README.md".to_string()]
    );

    // A path the klon does not hold is not a failure.
    let out = ok(&fx, &["claim", "--release", "work", "docs"]);
    assert!(
        stdout(&out).contains("holds no claim to release"),
        "{}",
        stdout(&out)
    );

    ok(&fx, &["claim", "--release", "work"]);
    assert!(paths_of(&fx, "work").is_empty());
}

/// `list` marks a klon whose claim overlaps another klon's with `!`, and
/// `list --json` carries the paths and the flag. The append refuses such a
/// pair, so the test writes the table by hand: only a hand edit or a klon that
/// wrote without the lock can reach this state.
#[test]
fn list_marks_an_overlap_and_carries_the_claims() {
    let fx = fixture();
    add(&fx, "left");
    add(&fx, "right");
    ok(&fx, &["claim", "left", "src/app"]);
    ok(&fx, &["claim", "right", "src/apple"]);

    // No overlap yet: each klon shows its own count.
    let out = ok(&fx, &["list", "--no-gh"]);
    for branch in ["left", "right"] {
        assert!(
            line_for(&stdout(&out), branch).contains("| 1 |"),
            "{branch} owns one path: {}",
            stdout(&out)
        );
    }

    fs::write(
        claims_file(&fx),
        serde_json::json!({
            "version": 1,
            "claims": [
                {"klon": "left", "path": "src/app", "kind": "dir",
                 "created": "2026-09-05T10:00:00Z"},
                {"klon": "right", "path": "src/app/main.rs", "kind": "file",
                 "created": "2026-09-05T10:00:00Z"},
            ],
        })
        .to_string(),
    )
    .unwrap();

    let out = ok(&fx, &["list", "--no-gh"]);
    for branch in ["left", "right"] {
        assert!(
            line_for(&stdout(&out), branch).contains("| 1! |"),
            "{branch} must carry the overlap mark: {}",
            stdout(&out)
        );
    }

    let out = ok(&fx, &["list", "--json", "--no-gh"]);
    let doc: Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    for row in doc["klons"].as_array().expect("an array") {
        assert_eq!(row["claim_overlap"], true, "both klons overlap: {row}");
        assert_eq!(
            row["claims"].as_array().expect("an array").len(),
            1,
            "each klon owns one path: {row}"
        );
    }
}
