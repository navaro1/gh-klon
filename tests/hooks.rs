//! C22 acceptance tests: the per-tree hooks and the `[warm] steps` under the
//! envelope (spec §7 C22, R20).
//!
//! The fixture is a plain `git init` in place, so the repository hooks live in
//! `<golden>/.git/hooks`. `add` copies them into `<klon>/.klon/hooks`, `run`
//! points `core.hooksPath` at the copy, and `up` runs the approved steps in
//! golden with `MAKEFLAGS` exported.

mod common;

use common::{git, git_ok, identity, klon, stderr, stdout, Fixture};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SEED: u64 = 22;

/// A small fixture with a committer identity in the repository config.
fn fixture() -> Fixture {
    let fx = Fixture::generate(SEED, 30, 4, 4, 2);
    identity(&fx.golden);
    fx
}

/// The hooks directory of a plain repository.
fn hooks_dir(golden: &Path) -> PathBuf {
    golden.join(".git").join("hooks")
}

/// The hooks copy of a klon.
fn klon_hooks(klon_path: &Path) -> PathBuf {
    klon_path.join(".klon").join("hooks")
}

/// Write an executable hook. `body` is the whole file content.
fn write_hook(dir: &Path, name: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    let mut perm = fs::metadata(&path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&path, perm).unwrap();
}

/// A `pre-commit` body that appends one word to a marker file.
fn hook_body(marker: &Path, word: &str) -> String {
    format!("#!/bin/sh\necho {word} >> {}\n", marker.display())
}

/// `add <branch>` and assert that it worked. The answer is the klon path.
fn add(fx: &Fixture, branch: &str) -> PathBuf {
    let out = klon(&fx.golden, &["add", branch]);
    assert!(
        out.status.success(),
        "add {branch} failed: {}",
        stderr(&out)
    );
    fx.klon_path(branch)
}

/// A local branch on top of `main`, without a checkout.
fn make_branch(fx: &Fixture, name: &str) {
    git_ok(&fx.golden, &["branch", name, "main"]);
}

/// Commit one file with plain git in `dir`.
fn commit_in(dir: &Path, file: &str) {
    fs::write(dir.join(file), format!("{file}\n")).unwrap();
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-qm", file]);
}

/// Stage one file in the klon and commit it under `gh klon run`, so the commit
/// goes through the envelope and its `core.hooksPath`.
fn commit_via_run(golden: &Path, branch: &str, klon_path: &Path, file: &str) {
    fs::write(klon_path.join(file), format!("{file}\n")).unwrap();
    git_ok(klon_path, &["add", "-A"]);
    let out = klon(golden, &["run", branch, "--", "git", "commit", "-qm", file]);
    assert!(
        out.status.success(),
        "run commit in {branch} failed: {}",
        stderr(&out)
    );
}

/// Write the `.klon.toml` with one `[warm] steps` list and commit it, so
/// golden stays clean for `up`.
fn manifest_with(golden: &Path, steps: &str) {
    fs::write(
        golden.join(".klon.toml"),
        format!("[warm]\nsteps = {steps}\n"),
    )
    .unwrap();
    git_ok(golden, &["add", "-A"]);
    git_ok(golden, &["commit", "-qm", "warm steps"]);
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|err| panic!("not one JSON document: {err}"))
}

/// The names in the klon's hooks copy.
fn copied_names(klon_path: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(klon_hooks(klon_path)).unwrap() {
        names.push(entry.unwrap().file_name().to_string_lossy().into_owned());
    }
    names.sort();
    names
}

// --- The per-tree hooks ------------------------------------------------------

/// R20: a hook edited inside a klon runs for the commits of that klon, and
/// never for the commits of golden.
#[test]
fn hook_edited_in_a_klon_runs_there_and_not_in_golden() {
    let fx = fixture();
    let golden_marker = fx.golden.join("marker");
    write_hook(
        &hooks_dir(&fx.golden),
        "pre-commit",
        &hook_body(&golden_marker, "golden"),
    );
    // A non-executable file is not a hook and never reaches the copy.
    fs::write(hooks_dir(&fx.golden).join("post-commit"), "#!/bin/sh\n").unwrap();

    let klon_path = add(&fx, "feature");

    // The copy exists, keeps the mode, and drops git's examples.
    let copy = klon_hooks(&klon_path).join("pre-commit");
    assert!(copy.exists(), "the klon has no pre-commit copy");
    let mode = fs::metadata(&copy).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "the copy lost its executable bit");
    let names = copied_names(&klon_path);
    assert!(
        names.iter().all(|name| !name.ends_with(".sample")),
        "a .sample file reached the copy: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "post-commit"),
        "a non-executable file reached the copy: {names:?}"
    );

    // Edit the klon's own copy. The edit must not leave the klon.
    let klon_marker = klon_path.join("marker");
    write_hook(
        &klon_hooks(&klon_path),
        "pre-commit",
        &hook_body(&klon_marker, "klon"),
    );

    // A commit in golden runs golden's hook only.
    commit_in(&fx.golden, "g.txt");
    assert_eq!(
        fs::read_to_string(&golden_marker).unwrap(),
        "golden\n",
        "golden's own hook changed"
    );
    assert!(!klon_marker.exists(), "golden ran the klon's edited hook");

    // A commit under `run` in the klon runs the edited copy only.
    commit_via_run(&fx.golden, "feature", &klon_path, "k.txt");
    assert_eq!(
        fs::read_to_string(&klon_marker).unwrap(),
        "klon\n",
        "the klon did not run its edited hook"
    );
    assert_eq!(
        fs::read_to_string(&golden_marker).unwrap(),
        "golden\n",
        "the klon's commit ran golden's hook"
    );
}

/// R20: an edit in golden after `add` leaves the klon's copy alone.
#[test]
fn hook_edited_in_golden_after_add_leaves_the_klon_copy_alone() {
    let fx = fixture();
    write_hook(&hooks_dir(&fx.golden), "pre-commit", "#!/bin/sh\nexit 0\n");

    let klon_path = add(&fx, "feature");
    let copy = klon_hooks(&klon_path).join("pre-commit");
    let before = fs::read_to_string(&copy).unwrap();

    write_hook(&hooks_dir(&fx.golden), "pre-commit", "#!/bin/sh\nexit 1\n");
    assert_eq!(
        fs::read_to_string(&copy).unwrap(),
        before,
        "golden's edit reached the klon's copy"
    );
}

/// A hook edited in one klon never runs in a sibling klon.
#[test]
fn hook_edited_in_one_klon_stays_out_of_the_sibling() {
    let fx = fixture();
    let golden_marker = fx.golden.join("marker");
    write_hook(
        &hooks_dir(&fx.golden),
        "pre-commit",
        &hook_body(&golden_marker, "golden"),
    );

    let feature_klon = add(&fx, "feature");
    make_branch(&fx, "duo");
    let duo_klon = add(&fx, "duo");

    // Edit only the feature klon's copy.
    write_hook(
        &klon_hooks(&feature_klon),
        "pre-commit",
        &hook_body(&feature_klon.join("marker"), "feature"),
    );

    // A commit in the sibling runs the sibling's own copy (golden's body) and
    // never the edited one.
    commit_via_run(&fx.golden, "duo", &duo_klon, "d.txt");
    assert_eq!(
        fs::read_to_string(&golden_marker).unwrap(),
        "golden\n",
        "the sibling did not run its own copy"
    );
    assert!(
        !duo_klon.join("marker").exists(),
        "the sibling ran the feature klon's edited hook"
    );

    // The edited hook still runs only where it was written.
    commit_via_run(&fx.golden, "feature", &feature_klon, "f.txt");
    assert_eq!(
        fs::read_to_string(feature_klon.join("marker")).unwrap(),
        "feature\n"
    );
    assert_eq!(
        fs::read_to_string(&golden_marker).unwrap(),
        "golden\n",
        "the feature klon's commit wrote into golden"
    );
}

// --- Plain git and the worktree-config extension -----------------------------

/// Plain git in the klon uses the copy only when the repository already turned
/// `extensions.worktreeConfig` on, and the write lands in the klon's own
/// `config.worktree`, not in golden.
#[test]
fn plain_git_uses_the_copy_when_the_extension_is_on() {
    let fx = fixture();
    git_ok(&fx.golden, &["config", "extensions.worktreeConfig", "true"]);
    let golden_marker = fx.golden.join("marker");
    write_hook(
        &hooks_dir(&fx.golden),
        "pre-commit",
        &hook_body(&golden_marker, "golden"),
    );

    let klon_path = add(&fx, "feature");

    // The per-tree value landed in the klon's config.worktree.
    let value = git_ok(&klon_path, &["config", "--worktree", "core.hooksPath"]);
    assert_eq!(
        Path::new(value.trim()),
        klon_hooks(&klon_path).as_path(),
        "config.worktree does not name the copy"
    );
    // Golden keeps no per-tree value.
    let out = git(&fx.golden, &["config", "--worktree", "core.hooksPath"]);
    assert!(
        !out.status.success(),
        "golden grew a core.hooksPath of its own"
    );

    // A plain commit in the klon runs the copy, and it stays the klon's own
    // copy after an edit.
    let klon_marker = klon_path.join("marker");
    write_hook(
        &klon_hooks(&klon_path),
        "pre-commit",
        &hook_body(&klon_marker, "klon"),
    );
    commit_in(&klon_path, "k.txt");
    assert_eq!(
        fs::read_to_string(&klon_marker).unwrap(),
        "klon\n",
        "plain git in the klon did not use the copy"
    );

    // `doctor` reports the extension as on.
    let out = klon(&fx.golden, &["doctor", "--json"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["features"]["hooks"]["status"], "present");
}

/// Without the extension, `doctor` reports the row as absent with the way out.
#[test]
fn doctor_reports_absent_without_the_extension() {
    let fx = fixture();
    let out = klon(&fx.golden, &["doctor", "--json"]);
    assert!(out.status.success(), "doctor failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["features"]["hooks"]["status"], "absent");
    assert_eq!(
        doc["features"]["hooks"]["detail"],
        "per-tree hooks apply under run only; enable extensions.worktreeConfig for plain git"
    );
}

// --- The `[warm] steps` under the envelope ------------------------------------

/// A klon whose env file is gone refuses the command instead of running it
/// with no envelope. The C22 refactor made the caller name what the root is,
/// and this is the state the old sniff would have misread as golden.
#[test]
fn run_refuses_a_klon_without_an_env_file() {
    let fx = fixture();
    let klon_path = add(&fx, "feature");
    fs::remove_file(klon_path.join(".klon").join("env")).unwrap();
    let out = klon(&fx.golden, &["run", "feature", "--", "sh", "-c", "exit 0"]);
    assert!(!out.status.success(), "run accepted a broken klon");
    assert!(
        stderr(&out).contains("is missing; the klon predates the envelope"),
        "{}",
        stderr(&out)
    );
}

/// `up` runs the approved steps in golden, in order, and reports the count.
#[test]
fn up_runs_approved_steps_in_golden() {
    let fx = fixture();
    let log = fx.golden.parent().unwrap().join("steps.log");
    manifest_with(
        &fx.golden,
        &format!(
            "['echo one >> {}', 'echo two >> {}']",
            log.display(),
            log.display()
        ),
    );
    let out = klon(&fx.golden, &["up", "--yes", "--json"]);
    assert!(out.status.success(), "up failed: {}", stderr(&out));
    let doc = parse(&stdout(&out));
    assert_eq!(doc["steps_run"], 2);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "one\ntwo\n",
        "the steps did not run in order in golden"
    );
}

/// `up` stops at the first failing step, names it, and exits non-zero.
#[test]
fn up_stops_at_the_first_failing_step() {
    let fx = fixture();
    let log = fx.golden.parent().unwrap().join("steps.log");
    manifest_with(
        &fx.golden,
        &format!(
            "['echo one >> {}', 'false', 'echo two >> {}']",
            log.display(),
            log.display()
        ),
    );
    let out = klon(&fx.golden, &["up", "--yes"]);
    assert!(!out.status.success(), "up ignored a failing warm step");
    assert!(
        stderr(&out).contains("warm step failed (exit 1): false"),
        "up did not name the failing step: {}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "one\n",
        "up ran the steps after the failing one"
    );
}

/// Every warm step sees the jobserver handshake in `MAKEFLAGS`.
#[test]
fn warm_step_sees_the_jobserver_handshake() {
    let fx = fixture();
    manifest_with(&fx.golden, "['test -n \"$MAKEFLAGS\"']");
    let out = klon(&fx.golden, &["up", "--yes"]);
    assert!(
        out.status.success(),
        "a warm step saw no MAKEFLAGS: {}",
        stderr(&out)
    );
}
