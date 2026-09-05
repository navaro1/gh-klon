//! C0 acceptance tests for `gh-klon add`. C1 moves the fixture and manifest helpers to `tests/common`.

use std::collections::BTreeSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_gh-klon");

/// Run `git -C <cwd> <args>` with an isolated identity and config.
fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "klon")
        .env("GIT_AUTHOR_EMAIL", "klon@example.com")
        .env("GIT_COMMITTER_NAME", "klon")
        .env("GIT_COMMITTER_EMAIL", "klon@example.com")
        .output()
        .expect("run git")
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn klon(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run gh-klon")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A generated repository: `main`, a `feature` branch, and an ignored `build/` directory.
struct Fixture {
    _tmp: tempfile::TempDir,
    golden: PathBuf,
    /// Paths that `feature` changes or adds, relative to the root.
    diff_paths: BTreeSet<String>,
}

impl Fixture {
    /// `tracked` files spread over `dirs` directories, `ignored` files in `build/`.
    fn generate(tracked: usize, dirs: usize, ignored: usize) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        for i in 0..tracked {
            let dir = golden.join(format!("d{:03}", i % dirs));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("f{i}.txt")), format!("tracked file {i}\n")).unwrap();
        }
        fs::write(golden.join("f2.txt"), "root file 2\n").unwrap();
        let build = golden.join("build");
        fs::create_dir(&build).unwrap();
        for i in 0..ignored {
            fs::write(
                build.join(format!("o{i}.bin")),
                format!("object {i}\n").repeat(3),
            )
            .unwrap();
        }
        fs::write(golden.join(".gitignore"), "/build/\n").unwrap();
        git_ok(&golden, &["init", "-q", "-b", "main"]);
        git_ok(&golden, &["add", "-A"]);
        git_ok(&golden, &["commit", "-qm", "base"]);

        git_ok(&golden, &["checkout", "-qb", "feature"]);
        let mut diff_paths = BTreeSet::new();
        for i in 0..20 {
            let rel = format!("d{:03}/f{}.txt", (i * 7) % dirs, i * 7);
            fs::write(golden.join(&rel), format!("feature edit {i}\n")).unwrap();
            diff_paths.insert(rel);
        }
        fs::write(golden.join("f2.txt"), "root file 2 on feature\n").unwrap();
        diff_paths.insert("f2.txt".into());
        for name in ["new-a.txt", "d000/new-b.txt"] {
            fs::write(golden.join(name), "added on feature\n").unwrap();
            diff_paths.insert(name.into());
        }
        git_ok(&golden, &["add", "-A"]);
        git_ok(&golden, &["commit", "-qm", "feature"]);
        git_ok(&golden, &["checkout", "-q", "main"]);
        // Let the coarse filesystem clock move past every fixture mtime.
        std::thread::sleep(Duration::from_millis(20));
        Fixture {
            _tmp: tmp,
            golden,
            diff_paths,
        }
    }

    fn default_klon_path(&self) -> PathBuf {
        self.golden
            .parent()
            .unwrap()
            .join("golden.wt")
            .join("feature")
    }
}

/// (path, type, size, mode, mtime, symlink target, content hash), sorted by path.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    path: PathBuf,
    kind: &'static str,
    size: u64,
    mode: u32,
    mtime: SystemTime,
    target: Option<PathBuf>,
    hash: u64,
}

fn manifest(root: &Path) -> Vec<Entry> {
    use std::os::unix::fs::PermissionsExt;
    fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let kind = meta.file_type();
            let mut hasher = DefaultHasher::new();
            let (kind_name, target) = if kind.is_symlink() {
                ("symlink", Some(fs::read_link(&path).unwrap()))
            } else if kind.is_dir() {
                walk(root, &path, out);
                ("dir", None)
            } else {
                fs::read(&path).unwrap().hash(&mut hasher);
                ("file", None)
            };
            out.push(Entry {
                path: path.strip_prefix(root).unwrap().to_path_buf(),
                kind: kind_name,
                size: if kind.is_file() { meta.len() } else { 0 },
                mode: meta.permissions().mode(),
                mtime: meta.modified().unwrap(),
                target,
                hash: hasher.finish(),
            });
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn assert_clean(klon_path: &Path) {
    let status = git_ok(klon_path, &["status", "--porcelain"]);
    assert_eq!(status, "", "the klon must be clean");
}

#[test]
fn add_feature_on_the_10k_fixture() {
    let fx = Fixture::generate(10_000, 100, 1_000);
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
    let fx = Fixture::generate(200, 10, 20);
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
    let fx = Fixture::generate(50, 5, 5);
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
fn add_refuses_an_unknown_branch_and_a_path_inside_golden() {
    let fx = Fixture::generate(50, 5, 5);
    let out = klon(&fx.golden, &["add", "nope"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("branch not found"),
        "stderr: {}",
        stderr(&out)
    );

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
}

#[test]
fn add_under_claude_worktrees_inside_golden() {
    let fx = Fixture::generate(300, 10, 30);
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
    let fx = Fixture::generate(50, 5, 5);
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
    let fx = Fixture::generate(50, 5, 5);
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
    let fx = Fixture::generate(50, 5, 5);
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
    let fx = Fixture::generate(50, 5, 5);
    let before = git_ok(&fx.golden, &["worktree", "list", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature~0"]);
    assert!(
        !out.status.success(),
        "a revision expression is not a branch"
    );
    assert!(stderr(&out).contains("branch not found"));
    assert_eq!(
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"]),
        before
    );
}

#[test]
fn add_refuses_a_golden_path_with_a_newline_before_registration() {
    let fx = Fixture::generate(50, 5, 5);
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
    let fx = Fixture::generate(50, 5, 5);
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
    let fx = Fixture::generate(200, 10, 20);
    git_ok(&fx.golden, &["rm", "--cached", "d001/f1.txt"]);
    fs::write(fx.golden.join("d001/f1.txt"), "dirty replacement").unwrap();
    let before = git_ok(&fx.golden, &["status", "--porcelain"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_clean(&fx.default_klon_path());
    assert_eq!(
        fs::read_to_string(fx.default_klon_path().join("d001/f1.txt")).unwrap(),
        "tracked file 1\n"
    );
    assert_eq!(git_ok(&fx.golden, &["status", "--porcelain"]), before);
}

#[test]
fn add_uses_the_branch_when_a_tag_has_the_same_name() {
    let fx = Fixture::generate(50, 5, 5);
    git_ok(&fx.golden, &["tag", "feature", "main"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(
        git_ok(&fx.default_klon_path(), &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/feature"
    );
    assert_eq!(
        git_ok(&fx.default_klon_path(), &["rev-parse", "HEAD"]),
        git_ok(&fx.golden, &["rev-parse", "refs/heads/feature"])
    );
}

#[test]
fn add_with_a_split_index() {
    let fx = Fixture::generate(50, 5, 5);
    git_ok(&fx.golden, &["update-index", "--split-index"]);
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_clean(&fx.default_klon_path());
}

#[test]
fn failed_fill_removes_read_only_copied_directories() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::generate(50, 5, 5);
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
    let fx = Fixture::generate(50, 5, 5);
    let exclude = fx.golden.join(".git/info/exclude");
    fs::write(&exclude, b"/local-\xff").unwrap();
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(fs::read(exclude).unwrap(), b"/local-\xff\n/.klon/\n");
}

#[test]
fn add_checks_a_default_path_through_a_symlink() {
    let fx = Fixture::generate(50, 5, 5);
    std::os::unix::fs::symlink(&fx.golden, fx.golden.parent().unwrap().join("golden.wt")).unwrap();
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("inside the repository"));
    assert!(!fx.golden.join("feature").exists());
}

#[test]
fn add_refuses_the_git_common_directory_even_if_ignored() {
    let fx = Fixture::generate(50, 5, 5);
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
