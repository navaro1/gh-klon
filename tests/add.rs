//! Acceptance tests for `gh-klon add`. The shared harness lives in `tests/common`.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use common::{
    assert_clean, assert_worktree_parity, freeze_times, git_ok, klon, manifest,
    manifest_without_times, stderr, Entry, Fixture, BIN,
};

const SEED: u64 = 42;

#[test]
fn add_feature_on_the_10k_fixture() {
    let fx = Fixture::generate(SEED, 10_000, 100, 1_000, 20);
    let klon_path = fx.default_klon_path();
    let t0 = SystemTime::now();

    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        klon_path.to_str().unwrap()
    );

    // R2: the tracked files equal the branch tree.
    assert_clean(&klon_path);
    let list = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let block = list
        .split("\n\n")
        .find(|b| b.starts_with(&format!("worktree {}", klon_path.display())))
        .expect("the klon is registered");
    assert!(
        block.lines().any(|l| l == "branch refs/heads/feature"),
        "block: {block}"
    );
    assert!(
        !block.lines().any(|l| l == "locked"),
        "the klon must be unlocked"
    );
    let klon_tree = git_ok(&klon_path, &["rev-parse", "HEAD^{tree}"]);
    let feature_tree = git_ok(&fx.golden, &["rev-parse", "feature^{tree}"]);
    assert_eq!(klon_tree, feature_tree);

    // R3: the ignored directory is a faithful copy.
    assert_eq!(
        manifest(&klon_path.join("build")),
        manifest(&fx.golden.join("build"))
    );
    assert!(fx.golden.join("build").is_dir());

    // Only the differing files of `feature` got a new mtime.
    let tracked = git_ok(&klon_path, &["ls-files"]);
    let mut newer = BTreeSet::new();
    for rel in tracked.lines() {
        if fs::metadata(klon_path.join(rel))
            .unwrap()
            .modified()
            .unwrap()
            > t0
        {
            newer.insert(rel.to_string());
        }
    }
    assert_eq!(newer, fx.diff_paths);

    // The config and the exclude file are in place.
    assert_eq!(
        git_ok(&fx.golden, &["config", "core.checkStat"]).trim(),
        "minimal"
    );
    assert_eq!(
        git_ok(&fx.golden, &["config", "core.untrackedCache"]).trim(),
        "true"
    );
    assert_eq!(git_ok(&fx.golden, &["config", "index.version"]).trim(), "4");
    let exclude = fs::read_to_string(fx.golden.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|l| *l == "/.klon/").count(), 1);

    // A second add on the same path changes nothing.
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("path not empty"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before
    );
    assert_clean(&klon_path);
}

#[test]
fn add_with_a_dirty_golden_file_that_differs_on_feature() {
    let fx = Fixture::generate(SEED, 200, 10, 20, 20);
    fs::write(fx.golden.join("f2.txt"), "dirty in golden\n").unwrap();
    fs::write(fx.golden.join("untracked.txt"), "untracked in golden\n").unwrap();
    fs::write(fx.golden.join("build/extra.bin"), "ignored, stays\n").unwrap();

    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon_path = fx.default_klon_path();
    assert_clean(&klon_path);
    assert_eq!(
        fs::read_to_string(klon_path.join("f2.txt")).unwrap(),
        "root file 2 on feature\n"
    );
    assert!(
        !klon_path.join("untracked.txt").exists(),
        "git clean removes untracked files"
    );
    assert!(
        klon_path.join("build/extra.bin").exists(),
        "ignored files stay"
    );
    // Golden keeps its own state.
    assert_eq!(
        fs::read_to_string(fx.golden.join("f2.txt")).unwrap(),
        "dirty in golden\n"
    );
    assert!(fx.golden.join("untracked.txt").exists());
}

#[test]
fn add_refuses_a_branch_that_golden_has_checked_out() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "main"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("already checked out"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before
    );
    assert!(!fx.golden.parent().unwrap().join("golden.wt").exists());
}

#[test]
fn add_creates_an_unknown_branch_and_refuses_bad_paths() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    // C13: an unknown name is a new branch from base, and add exits 0.
    let out = klon(&fx.golden, &["add", "nope"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let head = git_ok(&fx.golden, &["rev-parse", "nope"]);
    let main = git_ok(&fx.golden, &["rev-parse", "main"]);
    assert_eq!(head.trim(), main.trim());

    let inside = fx.golden.join("sub").join("x");
    let out = klon(
        &fx.golden,
        &["add", "--path", inside.to_str().unwrap(), "feature"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("inside the repository"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(!inside.exists());

    // A non-empty destination is refused too.
    let busy = fx.golden.parent().unwrap().join("busy");
    fs::create_dir(&busy).unwrap();
    fs::write(busy.join("x"), "x").unwrap();
    let out = klon(
        &fx.golden,
        &["add", "--path", busy.to_str().unwrap(), "nope2"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("path not empty"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(!git_ok(&fx.golden, &["worktree", "list", "--porcelain"]).contains("busy"));
}

#[test]
fn add_under_claude_worktrees_inside_golden() {
    let fx = Fixture::generate(SEED, 300, 10, 30, 20);
    let inside = fx.golden.join(".claude").join("worktrees").join("x");
    let out = klon(
        &fx.golden,
        &["add", "--path", inside.to_str().unwrap(), "feature"],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_clean(&inside);
    assert!(!inside.join(".claude").join("worktrees").exists());
    assert_eq!(
        manifest(&inside.join("build")),
        manifest(&fx.golden.join("build"))
    );
}

#[test]
fn add_honours_klonignore_for_the_path_and_the_copy() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    fs::write(fx.golden.join(".klonignore"), "/wt/\n/build/cache/\n").unwrap();
    fs::create_dir(fx.golden.join("build/cache")).unwrap();
    fs::write(fx.golden.join("build/cache/c.bin"), "cache\n").unwrap();
    let inside = fx.golden.join("wt").join("x");
    let out = klon(
        &fx.golden,
        &["add", "--path", inside.to_str().unwrap(), "feature"],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert!(inside.join("build/o1.bin").exists());
    assert!(!inside.join("build/cache").exists());
    assert!(!inside.join("wt").exists());
}

#[test]
fn a_failed_fill_leaves_no_worktree_entry() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    // A missing golden index breaks step 6, after git registered the worktree.
    fs::remove_file(fx.golden.join(".git/index")).unwrap();
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("copy the index"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before
    );
    assert!(!fx.default_klon_path().exists());
}

#[test]
fn version_prints_the_crate_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("gh-klon {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn add_copies_read_only_ignored_directories() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let cache = fx.golden.join("build/cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("artifact"), "cached output").unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o555)).unwrap();
    let out = klon(&fx.golden, &["add", "feature"]);
    // Restore permissions even if the assertion fails, so the fixture can be removed.
    let actual = if out.status.success() {
        Some(manifest(&fx.default_klon_path().join("build")))
    } else {
        None
    };
    let expected = manifest(&fx.golden.join("build"));
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
    if let Some(actual) = actual {
        fs::set_permissions(
            fx.default_klon_path().join("build/cache"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }
    assert!(out.status.success(), "add failed: {}", stderr(&out));
}

#[test]
fn add_refuses_revision_expressions() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature~0"]);
    assert!(
        !out.status.success(),
        "a revision expression is not a branch"
    );
    // The DWIM falls through to `git branch`, which rejects the name.
    assert!(stderr(&out).contains("not a valid branch name"));
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before
    );
}

#[test]
fn add_refuses_a_golden_path_with_a_newline_before_registration() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let renamed = fx.golden.with_file_name("golden\nname");
    fs::rename(&fx.golden, &renamed).unwrap();
    let destination = renamed.parent().unwrap().join("copy");
    let out = klon(
        &renamed,
        &["add", "feature", "--path", destination.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("without newlines"));
    assert!(!destination.exists());
    assert!(!renamed.join(".git/worktrees").exists());
}

#[test]
fn add_resolves_symlinks_before_parent_components() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let parent = fx.golden.parent().unwrap();
    fs::create_dir(fx.golden.join("sub")).unwrap();
    std::os::unix::fs::symlink(fx.golden.join("sub"), parent.join("alias")).unwrap();
    let path = parent.join("alias/../escape");
    let out = klon(
        &fx.golden,
        &["add", "feature", "--path", path.to_str().unwrap()],
    );
    assert!(
        !out.status.success(),
        "the actual destination is inside golden"
    );
    assert!(stderr(&out).contains("inside the repository"));
    assert!(!parent.join("escape").exists());
    assert!(!fx.golden.join("escape").exists());
}

#[test]
fn add_does_not_reuse_staged_deletions_as_untracked_files() {
    let fx = Fixture::generate(SEED, 200, 10, 20, 20);
    let victim = fx.tracked_rel(1);
    git_ok(&fx.golden, &["rm", "--cached", &victim]);
    fs::write(fx.golden.join(&victim), "dirty replacement").unwrap();
    let before = git_ok(&fx.golden, &["status", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon_path = fx.default_klon_path();
    assert_clean(&klon_path);
    assert_eq!(
        fs::read_to_string(klon_path.join(&victim)).unwrap(),
        fx.tracked_content(1)
    );
    assert_eq!(git_ok(&fx.golden, &["status", "--porcelain"]), before);
}

#[test]
fn add_uses_the_branch_when_a_tag_has_the_same_name() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    git_ok(&fx.golden, &["tag", "feature", "main"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon_path = fx.default_klon_path();
    assert_eq!(
        git_ok(&klon_path, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/feature"
    );
    assert_eq!(
        git_ok(&klon_path, &["rev-parse", "HEAD"]),
        git_ok(&fx.golden, &["rev-parse", "refs/heads/feature"])
    );
}

#[test]
fn add_with_a_split_index() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    git_ok(&fx.golden, &["update-index", "--split-index"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_clean(&fx.default_klon_path());
}

#[test]
fn failed_fill_removes_read_only_copied_directories() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let cache = fx.golden.join("build/cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("artifact"), "cached output").unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o555)).unwrap();
    fs::remove_file(fx.golden.join(".git/index")).unwrap();
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("copy the index"));
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before
    );
    assert!(!fx.default_klon_path().exists());
}

#[test]
fn add_preserves_non_utf8_exclude_patterns() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    let exclude = fx.golden.join(".git/info/exclude");
    fs::write(&exclude, b"/local-\xff").unwrap();
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    // C12 appends the staging name of the warm process beside `/.klon/`.
    assert_eq!(
        fs::read(exclude).unwrap(),
        b"/local-\xff\n/.klon/\n/*.klon-warming/\n"
    );
}

#[test]
fn add_checks_a_default_path_through_a_symlink() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    std::os::unix::fs::symlink(&fx.golden, fx.golden.parent().unwrap().join("golden.wt")).unwrap();
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("inside the repository"));
    assert!(!fx.golden.join("feature").exists());
}

#[test]
fn add_refuses_the_git_common_directory_even_if_ignored() {
    let fx = Fixture::generate(SEED, 50, 5, 5, 20);
    fs::write(fx.golden.join(".klonignore"), "/.git/\n").unwrap();
    let destination = fx.golden.join(".git/copy");
    let out = klon(
        &fx.golden,
        &["add", "feature", "--path", destination.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("inside the git common directory"));
    assert!(!destination.exists());
}

// --- C1 harness acceptance ---------------------------------------------------

#[test]
fn same_seed_gives_the_same_manifest() {
    let strip_git = |list: Vec<common::EntryNoTimes>| -> Vec<common::EntryNoTimes> {
        list.into_iter()
            .filter(|e| !e.path.starts_with(".git"))
            .collect()
    };
    let first = strip_git(manifest_without_times(
        &Fixture::generate(7, 400, 20, 40, 20).golden,
    ));
    let second = strip_git(manifest_without_times(
        &Fixture::generate(7, 400, 20, 40, 20).golden,
    ));
    assert_eq!(first, second);
    // A different seed changes the file bytes.
    let other = strip_git(manifest_without_times(
        &Fixture::generate(8, 400, 20, 40, 20).golden,
    ));
    assert_ne!(first, other);
}

#[test]
fn manifest_reports_three_distinct_differences() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let (left_root, right_root) = (tmp.path().join("left"), tmp.path().join("right"));
    for root in [&left_root, &right_root] {
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/file"), "same bytes\n").unwrap();
        fs::write(root.join("d/a"), "target a\n").unwrap();
        fs::write(root.join("d/b"), "target b\n").unwrap();
    }
    std::os::unix::fs::symlink("a", left_root.join("d/link")).unwrap();
    std::os::unix::fs::symlink("b", right_root.join("d/link")).unwrap();
    // Freeze the clock so each case below differs in exactly one field.
    freeze_times(&left_root);
    freeze_times(&right_root);

    let find = |root: &std::path::Path, rel: &str| -> Entry {
        manifest(root)
            .into_iter()
            .find(|e| e.path == std::path::Path::new(rel))
            .unwrap()
    };

    // 1. A symlink target difference.
    let (l, r) = (find(&left_root, "d/link"), find(&right_root, "d/link"));
    assert_ne!(l, r);
    assert_eq!(l.kind, r.kind);
    assert_eq!(l.size, r.size);
    assert_eq!(l.mode, r.mode);
    assert_eq!(l.mtime, r.mtime);
    assert_eq!(l.hash, r.hash);
    assert_ne!(l.target, r.target);

    // 2. A mode difference.
    fs::set_permissions(right_root.join("d/file"), fs::Permissions::from_mode(0o755)).unwrap();
    let (l, r) = (find(&left_root, "d/file"), find(&right_root, "d/file"));
    assert_ne!(l, r);
    assert_eq!(l.kind, r.kind);
    assert_eq!(l.size, r.size);
    assert_eq!(l.mtime, r.mtime);
    assert_eq!(l.hash, r.hash);
    assert_eq!(l.target, r.target);
    assert_ne!(l.mode, r.mode);

    // 3. An mtime difference.
    fs::File::open(right_root.join("d/a"))
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    let (l, r) = (find(&left_root, "d/a"), find(&right_root, "d/a"));
    assert_ne!(l, r);
    assert_eq!(l.kind, r.kind);
    assert_eq!(l.size, r.size);
    assert_eq!(l.mode, r.mode);
    assert_eq!(l.hash, r.hash);
    assert_eq!(l.target, r.target);
    assert_ne!(l.mtime, r.mtime);
}

#[test]
fn add_matches_the_plain_git_oracle() {
    let klon_fx = Fixture::generate(11, 2_000, 50, 200, 20);
    let oracle_fx = Fixture::generate(11, 2_000, 50, 200, 20);
    let out = klon(&klon_fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon_path = klon_fx.default_klon_path();
    let oracle = oracle_fx.oracle_worktree_add("feature");
    assert_worktree_parity(&klon_path, &oracle);
    // The shape is the same; the warm state is not. The oracle has no ignored files.
    assert!(klon_path.join("build").is_dir());
    assert!(!oracle.join("build").exists());
    assert_eq!(
        manifest(&klon_path.join("build")),
        manifest(&klon_fx.golden.join("build"))
    );
}

#[test]
#[should_panic(expected = "must hold the same bytes")]
fn parity_fails_on_a_one_byte_difference() {
    let klon_fx = Fixture::generate(11, 2_000, 50, 200, 20);
    let oracle_fx = Fixture::generate(11, 2_000, 50, 200, 20);
    let out = klon(&klon_fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let klon_path = klon_fx.default_klon_path();
    let oracle = oracle_fx.oracle_worktree_add("feature");
    // Flip one byte of a file that `feature` did not touch. The length stays.
    // No mtime change: the parity helper reads the tracked bytes, so it fails
    // even when `core.checkStat=minimal` would hide the change from git.
    let victim = klon_path.join(klon_fx.tracked_rel(3));
    let mut bytes = fs::read(&victim).unwrap();
    bytes[0] = bytes[0].wrapping_add(1);
    fs::write(&victim, &bytes).unwrap();
    assert_worktree_parity(&klon_path, &oracle);
}

#[test]
fn add_100k() {
    if std::env::var("KLON_FIXTURE").as_deref() != Ok("100k") {
        println!("skipped: add_100k generates 100,000 files; set KLON_FIXTURE=100k to run it");
        return;
    }
    let generation_start = Instant::now();
    let fx = Fixture::generate(100, 100_000, 1_000, 10_000, 20);
    let generation = generation_start.elapsed();

    let klon_path = fx.default_klon_path();
    let add_start = Instant::now();
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let add = add_start.elapsed();

    // R11: the first status re-checks 100k files, the second uses the cache.
    let first_start = Instant::now();
    let first = git_ok(&klon_path, &["status", "--porcelain"]);
    let first_status = first_start.elapsed();
    let second_start = Instant::now();
    let second = git_ok(&klon_path, &["status", "--porcelain"]);
    let second_status = second_start.elapsed();

    println!("fixture generation: {generation:?}");
    println!("klon add:           {add:?}");
    println!("first git status:  {first_status:?} (limit 500 ms)");
    println!("second git status: {second_status:?} (limit 150 ms)");
    assert_eq!(first, "", "the klon must be clean after add");
    assert_eq!(second, "", "the klon must stay clean");
    assert!(
        first_status <= Duration::from_millis(500),
        "the first status took {first_status:?}; the limit is 500 ms"
    );
    assert!(
        second_status <= Duration::from_millis(150),
        "the second status took {second_status:?}; the limit is 150 ms"
    );
    assert_eq!(
        git_ok(&klon_path, &["rev-parse", "HEAD^{tree}"]),
        git_ok(&fx.golden, &["rev-parse", "feature^{tree}"])
    );
}
