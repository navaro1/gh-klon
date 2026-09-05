//! Acceptance tests for the conflict radar and `sync --check` (spec §7 C24).
//! The shared harness lives in `tests/common`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{git_ok, klon, klon_env, stderr, stdout, Fixture};

const SEED: u64 = 42;

// --- Helpers -------------------------------------------------------------------

/// The `(major, minor)` of the installed git.
fn git_version() -> (u32, u32) {
    let out = String::from_utf8_lossy(
        &Command::new("git")
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    let number = out.split_whitespace().nth(2).unwrap_or("0.0").to_string();
    let mut parts = number.split('.');
    (
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
    )
}

/// True when the installed git has `merge-tree --write-tree` (2.38 and above).
fn has_write_tree() -> bool {
    git_version() >= (2, 38)
}

/// Branch off `main` in golden, write every `(path, content)` pair, and commit.
/// Golden returns to `main`.
fn branch_with(golden: &Path, branch: &str, files: &[(&str, &str)]) {
    git_ok(golden, &["checkout", "-qb", branch, "main"]);
    for (path, content) in files {
        let file = golden.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(file, content).unwrap();
    }
    git_ok(golden, &["add", "-A"]);
    git_ok(golden, &["commit", "-qm", branch]);
    git_ok(golden, &["checkout", "-q", "main"]);
}

/// Add one klon and fail the test with git's own message when `add` refuses.
fn add_klon(golden: &Path, branch: &str) {
    let out = klon(golden, &["add", branch]);
    assert!(
        out.status.success(),
        "add {branch} failed: {}",
        stderr(&out)
    );
}

/// The `list` line of the klon on `branch`.
fn line_for(text: &str, branch: &str) -> String {
    text.lines()
        .find(|line| line.split_whitespace().nth(1) == Some(branch))
        .unwrap_or_else(|| panic!("no list line for branch {branch} in:\n{text}"))
        .to_string()
}

/// The three radar columns of a `list` or `sync --check` line, as
/// `(vs-base, vs-siblings, behind)`.
fn columns(line: &str) -> (String, String, String) {
    let mut parts = line.split(" | ");
    let _head = parts.next().unwrap_or_default();
    let vs_base = parts.next().unwrap_or_default().to_string();
    let vs_siblings = parts.next().unwrap_or_default().to_string();
    let behind = parts.next().unwrap_or_default().to_string();
    (vs_base, vs_siblings, behind)
}

/// The `doctor` line whose first word is `name`, with the column padding
/// squeezed to one space so the assertion does not depend on the widest row.
fn line_for_name(text: &str, name: &str) -> String {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(name))
        .unwrap_or_else(|| panic!("no doctor line for {name} in:\n{text}"));
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run `list` and return its stdout, failing the test on a non-zero exit.
fn list(golden: &Path) -> String {
    let out = klon(golden, &["list"]);
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    stdout(&out)
}

// --- The conflict verdicts -----------------------------------------------------

#[test]
fn two_klons_that_edit_one_line_conflict_with_each_other() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "left", &[("f2.txt", "root file 2 on left\n")]);
    branch_with(&fx.golden, "right", &[("f2.txt", "root file 2 on right\n")]);
    add_klon(&fx.golden, "left");
    add_klon(&fx.golden, "right");

    let text = list(&fx.golden);
    for (branch, other) in [("left", "right"), ("right", "left")] {
        let (vs_base, vs_siblings, behind) = columns(&line_for(&text, branch));
        assert_eq!(
            vs_siblings,
            format!("1 conflict with {other}"),
            "{branch} and {other} edit the same line, so the radar must pair them:\n{text}"
        );
        // Neither klon has fallen behind base, and neither conflicts with it.
        assert_eq!(vs_base, "clean", "{branch} vs base:\n{text}");
        assert_eq!(behind, "behind 0");
    }
    if !has_write_tree() {
        println!(
            "note: this host runs git {:?}, so the legacy merge-tree form produced the verdict",
            git_version()
        );
    }
}

#[test]
fn a_klon_that_edits_a_file_untouched_by_base_is_clean() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(
        &fx.golden,
        "solo",
        &[("solo.txt", "only this klon writes here\n")],
    );
    add_klon(&fx.golden, "solo");

    let (vs_base, vs_siblings, behind) = columns(&line_for(&list(&fx.golden), "solo"));
    assert_eq!(vs_base, "clean", "base never touched solo.txt");
    assert_eq!(vs_siblings, "clean", "a lone klon has no sibling");
    assert_eq!(behind, "behind 0");
}

#[test]
fn a_klon_behind_base_says_so_and_stays_clean() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(
        &fx.golden,
        "solo",
        &[("solo.txt", "only this klon writes here\n")],
    );
    add_klon(&fx.golden, "solo");
    // Base moves, and it touches a file the klon never wrote.
    fs::write(fx.golden.join("base-only.txt"), "base moved on\n").unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "base moves"]);

    let (vs_base, vs_siblings, behind) = columns(&line_for(&list(&fx.golden), "solo"));
    assert_eq!(
        vs_base, "behind 1",
        "the merge is clean but base moved ahead"
    );
    assert_eq!(vs_siblings, "clean");
    assert_eq!(behind, "behind 1");
}

#[test]
fn the_legacy_form_finds_the_same_conflict() {
    if has_write_tree() {
        println!(
            "skipped: this host runs git {:?}, which has merge-tree --write-tree; \
             the legacy form needs a git below 2.38",
            git_version()
        );
        return;
    }
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "left", &[("f2.txt", "left edit\n")]);
    branch_with(&fx.golden, "right", &[("f2.txt", "right edit\n")]);
    add_klon(&fx.golden, "left");
    add_klon(&fx.golden, "right");

    let text = list(&fx.golden);
    assert_eq!(
        columns(&line_for(&text, "left")).1,
        "1 conflict with right",
        "the legacy merge-tree form must find the conflict too:\n{text}"
    );
}

#[test]
fn doctor_names_the_merge_tree_form_in_use() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    let expected = if has_write_tree() {
        "merge-tree --write-tree"
    } else {
        "legacy merge-tree"
    };

    let out = klon(&fx.golden, &["doctor"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let row = line_for_name(&stdout(&out), "radar");
    assert_eq!(
        row,
        format!("radar present: {expected}"),
        "doctor must name the form the radar uses"
    );

    let out = klon(&fx.golden, &["--json", "doctor"]);
    assert!(
        out.status.success(),
        "doctor --json failed: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "\"radar\":{{\"status\":\"present\",\"detail\":\"{expected}\"}}"
        )),
        "the radar row belongs in klon.doctor/1: {text}"
    );
}

#[test]
fn a_klon_toml_klon_cannot_read_leaves_the_columns_empty() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "solo", &[("solo.txt", "solo\n")]);
    add_klon(&fx.golden, "solo");
    // A broken file must not silently fall back to golden's HEAD and call the
    // result clean: the klon may measure against another base entirely.
    fs::write(fx.golden.join(".klon.toml"), "base = [oops\n").unwrap();

    let out = klon(&fx.golden, &["list"]);
    assert!(out.status.success(), "list must still list the klons");
    let (vs_base, vs_siblings, behind) = columns(&line_for(&stdout(&out), "solo"));
    assert_eq!((vs_base.as_str(), behind.as_str()), ("-", "-"));
    assert_eq!(vs_siblings, "-");
    assert!(
        stderr(&out).contains("the radar has no base"),
        "the reason belongs on stderr: {}",
        stderr(&out)
    );
}

#[test]
fn a_klon_measures_against_the_base_from_klon_toml() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    // `trunk` is the real base and it already edits the line the klon edits.
    branch_with(&fx.golden, "trunk", &[("f2.txt", "trunk edit\n")]);
    branch_with(&fx.golden, "solo", &[("f2.txt", "solo edit\n")]);
    add_klon(&fx.golden, "solo");
    fs::write(fx.golden.join(".klon.toml"), "base = \"trunk\"\n").unwrap();

    let (vs_base, _, behind) = columns(&line_for(&list(&fx.golden), "solo"));
    assert_eq!(
        vs_base, "1 conflict",
        "the radar reads `base` from .klon.toml"
    );
    assert_eq!(behind, "behind 1");
}

#[test]
fn a_klon_that_conflicts_with_base_reports_the_count() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "left", &[("f2.txt", "left edit\n")]);
    add_klon(&fx.golden, "left");
    // Base now edits the same line, so the klon can no longer merge cleanly.
    fs::write(fx.golden.join("f2.txt"), "base edit\n").unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "base edits f2"]);

    let (vs_base, _, behind) = columns(&line_for(&list(&fx.golden), "left"));
    assert_eq!(vs_base, "1 conflict", "base and the klon edit one line");
    assert_eq!(behind, "behind 1");
}

#[test]
fn one_klon_names_every_sibling_it_collides_with() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    for branch in ["left", "middle", "right"] {
        branch_with(
            &fx.golden,
            branch,
            &[("f2.txt", &format!("{branch} edit\n"))],
        );
        add_klon(&fx.golden, branch);
    }
    let (_, vs_siblings, _) = columns(&line_for(&list(&fx.golden), "middle"));
    assert_eq!(
        vs_siblings, "1 conflict with left, 1 conflict with right",
        "every sibling that collides appears, in a fixed order"
    );
}

// --- `sync --check` ------------------------------------------------------------

#[test]
fn sync_check_prints_the_radar_row_of_one_klon() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "left", &[("f2.txt", "left edit\n")]);
    branch_with(&fx.golden, "right", &[("f2.txt", "right edit\n")]);
    add_klon(&fx.golden, "left");
    add_klon(&fx.golden, "right");

    let out = klon(&fx.golden, &["sync", "left", "--check"]);
    assert!(
        out.status.success(),
        "sync --check failed: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "one klon, one row: {text}");
    let (vs_base, vs_siblings, behind) = columns(text.trim());
    assert_eq!(vs_base, "clean");
    assert_eq!(vs_siblings, "1 conflict with right");
    assert_eq!(behind, "behind 0");
    assert!(text.starts_with(&format!("{} left ", fx.klon_path("left").display())));
}

#[test]
fn sync_without_check_refuses_until_c14() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "left", &[("f2.txt", "left edit\n")]);
    add_klon(&fx.golden, "left");

    let out = klon(&fx.golden, &["sync", "left"]);
    assert!(!out.status.success(), "sync must refuse before C14");
    assert!(
        stderr(&out).contains("not implemented until C14"),
        "sync must name the chunk that finishes it: {}",
        stderr(&out)
    );
    // A branch with no klon is a plain refusal, not a panic.
    let out = klon(&fx.golden, &["sync", "nosuch", "--check"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no klon has branch nosuch"));
}

// --- The cache -----------------------------------------------------------------

/// Write a `git` shim that appends its arguments to `log` and then runs the real
/// git. Returns the directory to put in front of `PATH`.
fn git_shim(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let real = String::from_utf8_lossy(
        &Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("find git")
            .stdout,
    )
    .trim()
    .to_string();
    assert!(!real.is_empty(), "git must be on PATH");
    let bin = dir.join("shim");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{real}' \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Run a klon command through the shim and return every logged `merge-tree` call.
fn merge_tree_calls_of(golden: &Path, bin: &Path, log: &Path, args: &[&str]) -> Vec<String> {
    let _ = fs::remove_file(log);
    let path = std::env::var("PATH").unwrap_or_default();
    let joined = format!("{}:{path}", bin.display());
    let out = klon_env(golden, &[("PATH", std::ffi::OsStr::new(&joined))], args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("merge-tree"))
        .map(str::to_string)
        .collect()
}

/// Run `list` through the shim and return every logged `merge-tree` call.
fn merge_tree_calls(golden: &Path, bin: &Path, log: &Path) -> Vec<String> {
    merge_tree_calls_of(golden, bin, log, &["list"])
}

#[test]
fn a_second_list_with_unchanged_heads_makes_no_merge_tree_call() {
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    branch_with(&fx.golden, "left", &[("f2.txt", "left edit\n")]);
    branch_with(&fx.golden, "right", &[("f2.txt", "right edit\n")]);
    add_klon(&fx.golden, "left");
    add_klon(&fx.golden, "right");

    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("git.log");
    let bin = git_shim(tmp.path(), &log);

    let first = merge_tree_calls(&fx.golden, &bin, &log);
    assert!(
        !first.is_empty(),
        "the first list must run merge-tree, or this test proves nothing"
    );
    let second = merge_tree_calls(&fx.golden, &bin, &log);
    assert!(
        second.is_empty(),
        "the cache must answer the second list, but git ran: {second:?}"
    );

    // A moved HEAD gives a new key, so the radar computes again.
    let klon_path = fx.klon_path("left");
    fs::write(klon_path.join("f2.txt"), "left edits again\n").unwrap();
    git_ok(&klon_path, &["add", "-A"]);
    git_ok(&klon_path, &["commit", "-qm", "left moves"]);
    let third = merge_tree_calls(&fx.golden, &bin, &log);
    assert!(
        !third.is_empty(),
        "a moved HEAD must invalidate the cached pair"
    );
}

#[test]
fn sync_check_takes_only_the_pairs_that_reach_its_klon() {
    // Four klons make four base pairs and six sibling pairs. One klon sits in its
    // own base pair and in three of the sibling pairs.
    let fx = Fixture::generate(SEED, 20, 4, 4, 0);
    for branch in ["one", "two", "three", "four"] {
        branch_with(&fx.golden, branch, &[("f2.txt", &format!("{branch}\n"))]);
        add_klon(&fx.golden, branch);
    }
    // Every computed pair leaves one cache file. Counting the files counts the
    // pairs whichever merge-tree form the host offers, and the batch form on git
    // 2.40 and above answers many pairs in a single process.
    let radar_dir = fx.golden.join(".git").join("klon").join("radar");
    let cached = || fs::read_dir(&radar_dir).map(Iterator::count).unwrap_or(0);
    assert_eq!(cached(), 0, "the radar cache starts empty");

    let out = klon(&fx.golden, &["sync", "one", "--check"]);
    assert!(
        out.status.success(),
        "sync --check failed: {}",
        stderr(&out)
    );
    assert_eq!(cached(), 4, "one pair against base and one per sibling");

    // `list` needs every row, so it adds the three other base pairs and the three
    // sibling pairs that leave `one` out.
    let _ = list(&fx.golden);
    assert_eq!(cached(), 10, "four base pairs and six sibling pairs");
}

// --- Speed ---------------------------------------------------------------------

/// Five klons make five base pairs and ten sibling pairs.
const RADAR_PAIRS: usize = 15;

/// The wall-clock limit R23 and the C24 acceptance list give the whole radar.
const RADAR_LIMIT: Duration = Duration::from_millis(400);

/// One timed `list`.
fn time_list(golden: &Path) -> Duration {
    let start = Instant::now();
    let out = klon(golden, &["list"]);
    let elapsed = start.elapsed();
    assert!(out.status.success(), "list failed: {}", stderr(&out));
    elapsed
}

/// The middle sample.
fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// The time `list` needs for the radar alone: the median run with an empty radar
/// cache minus the median run that reads it. Both runs do the same `git status`
/// and `rev-parse` work on a warm page cache, and the cache test proves the second
/// run starts no `merge-tree`, so the difference is the radar.
///
/// The two medians come from separate samples on purpose. Subtracting paired runs
/// and keeping the smallest difference would let one slow warm run cancel the radar
/// and leave a test that cannot fail.
fn radar_cost(golden: &Path, common: &Path) -> Duration {
    const ROUNDS: usize = 5;
    let radar_dir = common.join("klon").join("radar");
    let mut cold = Vec::with_capacity(ROUNDS);
    let mut warm = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let _ = fs::remove_dir_all(&radar_dir);
        cold.push(time_list(golden));
        warm.push(time_list(golden));
    }
    let (cold, warm) = (median(&mut cold), median(&mut warm));
    println!("  cold list {cold:?}, warm list {warm:?}");
    cold.saturating_sub(warm)
}

/// The time this host needs to start `count` `git` processes one after another.
/// The radar starts two per pair, so this is what it would cost with no threads.
/// The number grows with the load on the host, so it calibrates the budget.
fn sequential_git_cost(golden: &Path, count: usize) -> Duration {
    let start = Instant::now();
    for _ in 0..count {
        git_ok(golden, &["rev-parse", "HEAD"]);
    }
    start.elapsed()
}

/// The one-minute load average and the CPU count, when the host reports them.
fn load_and_cpus() -> Option<(f64, f64)> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    let load: f64 = text.split_whitespace().next()?.parse().ok()?;
    let cpus = std::thread::available_parallelism().ok()?.get() as f64;
    Some((load, cpus))
}

/// A host the 400 ms figure describes: at least this many cores, and no more than
/// half of them already busy. R23 measured the figure on a 20-core laptop, so a
/// small or loaded runner is not the host it speaks about.
const QUIET_CPUS: f64 = 8.0;

/// Check the radar cost against both budgets.
///
/// The first budget holds under any load: the thread pool must beat the same pairs
/// run one at a time, and `merge-tree` costs more than the `rev-parse` this baseline
/// measures, so a radar that lost its threads lands above the line.
///
/// The wall-clock limit only means something on a host with cores to spare. This
/// suite runs beside other builds, so the test applies the limit on a big and mostly
/// idle host, and otherwise prints why it did not.
fn assert_radar_budget(golden: &Path, cost: Duration, fixture: &str) {
    let sequential = sequential_git_cost(golden, RADAR_PAIRS * 2);
    println!(
        "radar cost for 5 klons on the {fixture} fixture: {cost:?} \
         ({} git starts in a row cost {sequential:?})",
        RADAR_PAIRS * 2
    );
    assert!(
        cost < sequential,
        "the radar took {cost:?}, more than {} plain git starts in a row ({sequential:?}); \
         the pairs no longer run together",
        RADAR_PAIRS * 2
    );
    match load_and_cpus() {
        Some((load, cpus)) if cpus >= QUIET_CPUS && load < cpus / 2.0 => assert!(
            cost < RADAR_LIMIT,
            "the radar for 5 klons took {cost:?}, over the {RADAR_LIMIT:?} limit"
        ),
        Some((load, cpus)) => println!(
            "  the {RADAR_LIMIT:?} limit did not apply: this host has {cpus} cpus at load \
             {load}; the limit needs at least {QUIET_CPUS} cpus and half of them free"
        ),
        None => {
            println!("  the {RADAR_LIMIT:?} limit did not apply: this host reports no load average")
        }
    }
}

/// Five klons, each on its own branch with its own edit, made with plain
/// `git worktree add`. The radar reads the worktree list and the commits only, so
/// it does not need the ignored-file copy that `klon add` makes.
fn five_worktrees(fx: &Fixture) -> PathBuf {
    let root = fx.golden.parent().unwrap().join("radar-wt");
    fs::create_dir_all(&root).unwrap();
    for i in 0..5 {
        let branch = format!("radar{i}");
        branch_with(
            &fx.golden,
            &branch,
            &[("f2.txt", &format!("radar {i} edits the shared line\n"))],
        );
        let path = root.join(&branch);
        git_ok(
            &fx.golden,
            &["worktree", "add", "-q", path.to_str().unwrap(), &branch],
        );
    }
    root
}

#[test]
fn the_radar_for_five_klons_is_fast_on_the_10k_fixture() {
    let fx = Fixture::generate(1, 10_000, 100, 1_000, 22);
    five_worktrees(&fx);
    let common = fx.golden.join(".git");
    let cost = radar_cost(&fx.golden, &common);
    assert_radar_budget(&fx.golden, cost, "10k");
}

#[test]
fn the_radar_for_five_klons_is_fast_on_the_100k_fixture() {
    if std::env::var("KLON_FIXTURE").as_deref() != Ok("100k") {
        println!(
            "skipped: this test generates 100,000 files and 5 worktrees; \
             set KLON_FIXTURE=100k to run it"
        );
        return;
    }
    let fx = Fixture::generate(1, 100_000, 1_000, 10_000, 22);
    five_worktrees(&fx);
    let common = fx.golden.join(".git");
    let cost = radar_cost(&fx.golden, &common);
    assert_radar_budget(&fx.golden, cost, "100k");
}
