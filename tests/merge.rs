//! Acceptance tests for C25: `gh klon merge`. Each test drives the real
//! command against a generated fixture. Two of them put a fake tool on PATH:
//! a `mergiraf` stand-in, because the host has none, and a `git` wrapper that
//! logs every argument, because the AC asks klon to prove that it never pushes.

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{git, git_ok, identity, klon, klon_env, stderr, stdout, Fixture};

const SEED: u64 = 25;

// --- Helpers -----------------------------------------------------------------

/// A fixture with a committer identity in the repository config. `git merge`
/// refuses to write a commit without one, and the test harness keeps the
/// global config at `/dev/null`.
fn repo(tracked_files: usize) -> Fixture {
    let fx = Fixture::generate(SEED, tracked_files, 4, 3, 2);
    identity(&fx.golden);
    fx
}

/// `gh klon add <branch>`, with the klon path as the answer.
fn add(fx: &Fixture, branch: &str) -> PathBuf {
    let out = klon(&fx.golden, &["add", branch]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    fx.klon_path(branch)
}

/// The full object id of a revision.
fn rev(dir: &Path, revision: &str) -> String {
    git_ok(dir, &["rev-parse", revision]).trim().to_string()
}

fn head(dir: &Path) -> String {
    rev(dir, "HEAD")
}

/// True when git still lists `path` as a worktree.
fn registered(golden: &Path, path: &Path) -> bool {
    git_ok(golden, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| Path::new(listed) == path)
}

/// Write an executable `pre_merge` hook into the klon's hook directory.
fn write_hook(klon_dir: &Path, body: &str) {
    let dir = klon_dir.join(".klon").join("hooks");
    fs::create_dir_all(&dir).expect("create the hook directory");
    let path = dir.join("pre_merge");
    fs::write(&path, body).expect("write the hook");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make the hook run");
}

/// Write an executable script and return its path.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("create the script directory");
    let path = dir.join(name);
    fs::write(&path, body).expect("write the script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make the script run");
    path
}

/// `<dir>:<the current PATH>`, so a fake tool in `dir` wins.
fn path_with(dir: &Path) -> OsString {
    let mut value = OsString::from(dir);
    value.push(":");
    value.push(std::env::var_os("PATH").unwrap_or_default());
    value
}

/// The absolute path of the real `git`, for a fake tool that forwards to it.
fn real_git() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("look for git");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// Commit `body` at `rel` in `dir`.
fn commit(dir: &Path, rel: &str, body: &str, message: &str) {
    fs::write(dir.join(rel), body).expect("write the file");
    git_ok(dir, &["add", rel]);
    git_ok(dir, &["commit", "-qm", message]);
}

/// Twenty numbered lines. Two edits far apart in this file do not overlap.
fn numbered(edit_line: Option<(usize, &str)>) -> String {
    let mut text = String::new();
    for i in 1..=20 {
        match edit_line {
            Some((line, body)) if line == i => text.push_str(&format!("{body}\n")),
            _ => text.push_str(&format!("line {i}\n")),
        }
    }
    text
}

// --- AC 1: a failing pre_merge hook ------------------------------------------

#[test]
fn a_failing_pre_merge_hook_stops_the_merge() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    write_hook(
        &klon_dir,
        "#!/bin/sh\necho 'the gate says no' >&2\nexit 1\n",
    );
    let before = head(&fx.golden);

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(!out.status.success(), "a failed gate must fail the merge");
    assert!(
        stderr(&out).contains("pre_merge failed"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
    assert!(klon_dir.exists(), "the klon must stay");
    assert!(
        registered(&fx.golden, &klon_dir),
        "the klon must stay listed"
    );
}

/// The other half of the gate: a hook that passes lets the merge through, and
/// the report names the gate that ran.
#[test]
fn a_passing_pre_merge_hook_lets_the_merge_through() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    write_hook(&klon_dir, "#!/bin/sh\nexit 0\n");

    let out = klon(&fx.golden, &["merge", "--json", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    assert_eq!(report["hook"], "pre_merge");
    assert_eq!(report["removed"], true);
}

/// Without a hook the approved `[proof] steps` are the gate. A step that fails
/// stops the merge in the same way.
#[test]
fn a_failing_proof_step_stops_the_merge() {
    let fx = repo(20);
    // The config must be committed: an untracked file makes golden dirty, and
    // a dirty golden fails before the gate ever runs.
    commit(
        &fx.golden,
        ".klon.toml",
        "[proof]\nsteps = [\"exit 3\"]\n",
        "add a proof step",
    );
    let klon_dir = add(&fx, "feature");
    let before = head(&fx.golden);

    let out = klon_env(
        &fx.golden,
        &[("KLON_CONFIG_HOME", fx.golden.parent().unwrap().as_os_str())],
        &["--yes", "merge", "feature"],
    );
    assert!(!out.status.success(), "a failed step must fail the merge");
    assert!(
        stderr(&out).contains("pre_merge failed: exit 3"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
    assert!(klon_dir.exists(), "the klon must stay");
}

// --- AC 2: a clean klon advances base and the klon goes ----------------------

#[test]
fn ff_only_fast_forwards_base_and_removes_the_klon() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    let branch_head = rev(&fx.golden, "refs/heads/feature");

    let out = klon(&fx.golden, &["merge", "--ff-only", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    assert_eq!(
        head(&fx.golden),
        branch_head,
        "--ff-only must move base to the branch"
    );
    assert!(
        !registered(&fx.golden, &klon_dir),
        "git worktree list must forget the klon"
    );
    assert!(
        git(
            &fx.golden,
            &["show-ref", "--verify", "--quiet", "refs/heads/feature"]
        )
        .status
        .success(),
        "merge must keep the branch"
    );
}

#[test]
fn the_default_mode_writes_a_merge_commit_and_removes_the_klon() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    let before = head(&fx.golden);
    let branch_head = rev(&fx.golden, "refs/heads/feature");

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    let parents = git_ok(&fx.golden, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    let parents: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(
        parents.len(),
        3,
        "--no-ff must write two parents: {parents:?}"
    );
    assert!(parents.contains(&before.as_str()), "{parents:?}");
    assert!(parents.contains(&branch_head.as_str()), "{parents:?}");
    assert!(!registered(&fx.golden, &klon_dir), "the klon must be gone");
}

/// `[merge] ff = "ff-only"` picks the mode when no flag does.
#[test]
fn the_config_picks_the_merge_mode() {
    let fx = repo(20);
    commit(
        &fx.golden,
        ".klon.toml",
        "[merge]\nff = \"ff-only\"\n",
        "ask for a fast-forward merge",
    );
    // The branch is behind the new config commit now, so a fast-forward is not
    // possible and `ff-only` must refuse.
    add(&fx, "feature");
    let before = head(&fx.golden);

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(
        !out.status.success(),
        "ff-only must refuse a diverged branch"
    );
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
}

#[test]
fn keep_leaves_the_klon_in_place() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");

    let out = klon(&fx.golden, &["merge", "--keep", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    assert!(
        registered(&fx.golden, &klon_dir),
        "--keep must leave the klon listed"
    );
}

#[test]
fn a_klon_with_a_live_process_stays_after_the_merge() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    let mut sleep = Command::new("sleep")
        .arg("30")
        .current_dir(&klon_dir)
        .spawn()
        .expect("spawn sleep in the klon");

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("live process"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(registered(&fx.golden, &klon_dir), "the klon must stay");

    sleep.kill().expect("kill sleep");
    sleep.wait().expect("reap sleep");
}

// --- AC 3: mergiraf ----------------------------------------------------------

/// The host has no mergiraf (spec §7 host facts), so the real driver cannot
/// run here. A stand-in on PATH takes its place: it merges the three files
/// with `git merge-file`, which joins two non-overlapping hunks cleanly, and
/// it logs its arguments so the test can prove that git called it.
#[test]
fn the_mergiraf_driver_merges_a_non_overlapping_same_file_edit() {
    eprintln!(
        "note: the host has no mergiraf; this test drives the driver path with \
         a git merge-file stand-in"
    );
    let fx = repo(20);
    let tools = fx.golden.parent().unwrap().join("tools");
    let log = fx.golden.parent().unwrap().join("mergiraf.log");
    write_script(
        &tools,
        "mergiraf",
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {log}\n\
             ancestor=\"$3\"; ours=\"$4\"; theirs=\"$5\"\n\
             merged=\"$(mktemp)\"\n\
             if {git} merge-file -p \"$ours\" \"$ancestor\" \"$theirs\" > \"$merged\"; then\n\
             \x20 cat \"$merged\" > \"$ours\"; rm -f \"$merged\"; exit 0\n\
             fi\n\
             cat \"$merged\" > \"$ours\"; rm -f \"$merged\"; exit 1\n",
            log = log.display(),
            git = real_git().display(),
        ),
    );

    // One file, two edits that do not overlap: line 2 on the branch and line
    // 18 on base.
    commit(&fx.golden, "poly.txt", &numbered(None), "add poly.txt");
    git_ok(&fx.golden, &["branch", "topic"]);
    let klon_dir = add(&fx, "topic");
    commit(
        &klon_dir,
        "poly.txt",
        &numbered(Some((2, "line 2 from the topic"))),
        "edit line 2",
    );
    commit(
        &fx.golden,
        "poly.txt",
        &numbered(Some((18, "line 18 from base"))),
        "edit line 18",
    );

    let out = klon_env(
        &fx.golden,
        &[("PATH", &path_with(&tools))],
        &["merge", "topic"],
    );
    assert!(out.status.success(), "merge failed: {}", stderr(&out));

    let calls = fs::read_to_string(&log).expect("the driver must have run");
    assert!(
        calls.contains("poly.txt"),
        "the driver must see the path: {calls}"
    );
    let merged = fs::read_to_string(fx.golden.join("poly.txt")).expect("read poly.txt");
    assert!(
        !merged.contains("<<<<<<<") && !merged.contains(">>>>>>>"),
        "the merged file must hold no conflict marker:\n{merged}"
    );
    assert!(merged.contains("line 2 from the topic"), "{merged}");
    assert!(merged.contains("line 18 from base"), "{merged}");

    // The generated attributes file names the driver and holds the marker.
    let attributes = fs::read_to_string(fx.golden.join(".git").join("info").join("attributes"))
        .expect("read info/attributes");
    assert!(attributes.contains("* merge=mergiraf"), "{attributes}");
    assert!(attributes.contains("gh-klon"), "{attributes}");
    assert_eq!(
        git_ok(&fx.golden, &["config", "--get", "merge.mergiraf.driver"]).trim(),
        "mergiraf merge --git %O %A %B -s %S -x %X -y %Y -p %P"
    );
}

// --- AC 4: no push -----------------------------------------------------------

#[test]
fn merge_never_runs_git_push() {
    let fx = repo(20);
    // A bare origin, so the fetch in step 2 has a remote to talk to and the
    // test still needs no network.
    let origin = fx.golden.parent().unwrap().join("octo").join("repo.git");
    git_ok(
        &fx.golden,
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git_ok(
        &fx.golden,
        &[
            "remote",
            "add",
            "origin",
            &format!("file://{}", origin.display()),
        ],
    );
    git_ok(&fx.golden, &["push", "-q", "origin", "main", "feature"]);

    let tools = fx.golden.parent().unwrap().join("tools");
    let log = fx.golden.parent().unwrap().join("git.log");
    write_script(
        &tools,
        "git",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nexec {git} \"$@\"\n",
            log = log.display(),
            git = real_git().display(),
        ),
    );

    let klon_dir = add(&fx, "feature");
    let out = klon_env(
        &fx.golden,
        &[("PATH", &path_with(&tools))],
        &["merge", "feature"],
    );
    assert!(out.status.success(), "merge failed: {}", stderr(&out));
    assert!(!registered(&fx.golden, &klon_dir), "the klon must be gone");

    let calls = fs::read_to_string(&log).expect("the wrapper must have logged");
    assert!(
        calls.lines().any(|line| line.contains("fetch")),
        "the merge must fetch:\n{calls}"
    );
    for line in calls.lines() {
        assert!(
            !line.split_whitespace().any(|word| word == "push"),
            "merge must never push, but it ran: {line}"
        );
    }
}

// --- AC 5: a dirty tree ------------------------------------------------------

#[test]
fn merge_refuses_a_dirty_golden() {
    let fx = repo(20);
    add(&fx, "feature");
    let before = head(&fx.golden);
    let rel = fx.tracked_rel(0);
    fs::write(fx.golden.join(&rel), "a local change\n").expect("dirty golden");

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(!out.status.success(), "a dirty golden must fail the merge");
    assert!(stderr(&out).contains("dirty"), "stderr: {}", stderr(&out));
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
}

#[test]
fn merge_refuses_a_dirty_klon() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    let before = head(&fx.golden);
    fs::write(klon_dir.join(fx.tracked_rel(0)), "a local change\n").expect("dirty klon");

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(!out.status.success(), "a dirty klon must fail the merge");
    assert!(
        stderr(&out).contains("dirty klon"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
}

// --- The conflict path -------------------------------------------------------

#[test]
fn a_conflicting_merge_names_the_paths_and_leaves_golden_clean() {
    let fx = repo(20);
    let klon_dir = add(&fx, "feature");
    // The fixture's `feature` branch already changed `f2.txt`. Base changes the
    // same line, so the merge cannot join the two.
    commit(
        &fx.golden,
        "f2.txt",
        "root file 2 on base\n",
        "edit f2.txt on base",
    );
    let before = head(&fx.golden);

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(!out.status.success(), "a conflict must fail the merge");
    assert!(stderr(&out).contains("f2.txt"), "stderr: {}", stderr(&out));
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
    assert_eq!(
        git_ok(&fx.golden, &["status", "--porcelain"]),
        "",
        "the abort must leave golden clean"
    );
    assert!(registered(&fx.golden, &klon_dir), "the klon must stay");

    // The JSON form carries the same paths and keeps the non-zero exit.
    let out = klon(&fx.golden, &["merge", "--json", "feature"]);
    assert!(!out.status.success(), "a conflict must fail the merge");
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("one JSON document");
    let conflicts: Vec<&str> = report["conflicts"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|path| path.as_str().expect("a path"))
        .collect();
    assert_eq!(conflicts, vec!["f2.txt"]);
    assert_eq!(report["head_before"], report["head_after"]);
    assert_eq!(report["removed"], false);
}

// --- The refusals before any change ------------------------------------------

#[test]
fn merge_refuses_a_golden_that_is_not_on_base() {
    let fx = repo(20);
    add(&fx, "feature");
    commit(
        &fx.golden,
        ".klon.toml",
        "base = \"trunk\"\n",
        "name a base",
    );
    git_ok(&fx.golden, &["branch", "trunk"]);
    let before = head(&fx.golden);

    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(!out.status.success(), "golden is on main, not on trunk");
    assert!(stderr(&out).contains("trunk"), "stderr: {}", stderr(&out));
    assert_eq!(head(&fx.golden), before, "base HEAD must not move");
}

#[test]
fn merge_refuses_a_branch_with_no_klon() {
    let fx = repo(20);
    let out = klon(&fx.golden, &["merge", "feature"]);
    assert!(!out.status.success(), "no klon holds the branch");
    assert!(stderr(&out).contains("no klon"), "stderr: {}", stderr(&out));
}

/// The type checker cannot stop `merge main`, so the command does.
#[test]
fn merge_refuses_the_base_branch() {
    let fx = repo(20);
    add(&fx, "feature");
    let out = klon(&fx.golden, &["merge", "main"]);
    assert!(!out.status.success(), "main is the base branch");
}

/// `--no-ff` and `--ff-only` name two different merges, so clap refuses both.
#[test]
fn the_two_mode_flags_conflict() {
    let fx = repo(10);
    let out = klon(&fx.golden, &["merge", "--no-ff", "--ff-only", "feature"]);
    assert!(!out.status.success(), "clap must refuse both flags");
}

/// A second `merge` in the same repository writes no second attributes block.
#[test]
fn the_generated_attributes_block_stays_one_block() {
    let fx = repo(20);
    let tools = fx.golden.parent().unwrap().join("tools");
    write_script(&tools, "mergiraf", "#!/bin/sh\nexit 1\n");
    let attributes = fx.golden.join(".git").join("info").join("attributes");
    fs::create_dir_all(attributes.parent().unwrap()).expect("create info/");
    fs::write(&attributes, "*.bin binary\n").expect("write info/attributes");

    for branch in ["feature", "second"] {
        if branch == "second" {
            git_ok(&fx.golden, &["branch", "second", "feature"]);
        }
        add(&fx, branch);
        let out = klon_env(
            &fx.golden,
            &[("PATH", &path_with(&tools))],
            &["merge", branch],
        );
        assert!(out.status.success(), "merge failed: {}", stderr(&out));
    }

    let text = fs::read_to_string(&attributes).expect("read info/attributes");
    assert_eq!(
        text.lines().filter(|l| *l == "* merge=mergiraf").count(),
        1,
        "the block must stay unique:\n{text}"
    );
    assert!(
        text.lines().any(|l| l == "*.bin binary"),
        "the other lines must stay:\n{text}"
    );
}
