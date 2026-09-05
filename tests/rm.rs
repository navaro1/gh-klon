//! C3 acceptance tests for `gh-klon rm` and `gh klon prune`. C1 moves the
//! fixture helpers to `tests/common`; the copies here go away then.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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

/// A generated repository: `main`, a `feature` branch that edits one file,
/// and an ignored `build/` directory.
struct Fixture {
    _tmp: tempfile::TempDir,
    golden: PathBuf,
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
        fs::write(golden.join("d000").join("f0.txt"), "edited on feature\n").unwrap();
        git_ok(&golden, &["commit", "-qam", "feature"]);
        git_ok(&golden, &["checkout", "-q", "main"]);
        // Let the coarse filesystem clock move past every fixture mtime.
        std::thread::sleep(Duration::from_millis(20));
        Fixture { _tmp: tmp, golden }
    }

    fn klon_path(&self) -> PathBuf {
        self.golden
            .parent()
            .unwrap()
            .join("golden.wt")
            .join("feature")
    }

    fn wt_root(&self) -> PathBuf {
        self.golden.parent().unwrap().join("golden.wt")
    }

    fn trash(&self) -> PathBuf {
        self.wt_root().join(".trash")
    }
}

/// `add feature` and assert that it worked.
fn add_feature(fx: &Fixture) -> PathBuf {
    let out = klon(&fx.golden, &["add", "feature"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    fx.klon_path()
}

fn worktree_list(golden: &Path) -> String {
    git_ok(golden, &["worktree", "list", "--porcelain"])
}

/// Poll `cond` until it holds or the timeout passes.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

/// True when the trash directory is empty or gone.
fn trash_is_empty(trash: &Path) -> bool {
    match fs::read_dir(trash) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

#[test]
fn rm_refuses_a_dirty_klon_and_force_removes_it() {
    let fx = Fixture::generate(200, 10, 20);
    let klon_path = add_feature(&fx);
    fs::write(klon_path.join("d000").join("f0.txt"), "dirty\n").unwrap();

    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(!out.status.success(), "rm of a dirty klon must fail");
    assert!(
        stderr(&out).contains("dirty"),
        "stderr must say dirty: {}",
        stderr(&out)
    );
    assert!(klon_path.exists(), "the tree must still exist");
    assert!(
        worktree_list(&fx.golden).contains(&klon_path.to_string_lossy().to_string()),
        "the klon must stay registered"
    );

    let out = klon(&fx.golden, &["rm", "--force", "feature"]);
    assert!(out.status.success(), "rm --force failed: {}", stderr(&out));
    assert!(!klon_path.exists(), "the tree must be gone");
    assert!(
        !worktree_list(&fx.golden).contains(&klon_path.to_string_lossy().to_string()),
        "the klon must be unregistered"
    );
    // The branch survives: rm never deletes it.
    let branches = git_ok(&fx.golden, &["branch", "--list", "feature"]);
    assert!(
        !branches.trim().is_empty(),
        "the branch must still be listed"
    );
    // The background delete must drain the trash within 30 s.
    let trash = fx.trash();
    assert!(
        wait_until(|| trash_is_empty(&trash), Duration::from_secs(30)),
        "the trash directory must drain within 30 s"
    );
}

#[test]
fn rm_refuses_the_repository_root_the_home_and_an_unresolved_template() {
    let fx = Fixture::generate(50, 5, 5);
    let before = worktree_list(&fx.golden);

    // The main worktree is the repository root, by branch and by path.
    let out = klon(&fx.golden, &["rm", "main"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("repository root"),
        "stderr: {}",
        stderr(&out)
    );
    let out = klon(&fx.golden, &["rm", "--path", fx.golden.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("repository root"),
        "stderr: {}",
        stderr(&out)
    );

    // The home directory, even though no klon lives there.
    let home = std::env::var("HOME").expect("HOME must be set");
    let out = klon(&fx.golden, &["rm", "--path", &home]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("home directory"),
        "stderr: {}",
        stderr(&out)
    );

    // A template that nothing substituted.
    let out = klon(&fx.golden, &["rm", "--path", "../golden.wt/{branch}"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("template"),
        "stderr: {}",
        stderr(&out)
    );

    // Bad requests: neither or both of branch and path.
    let out = klon(&fx.golden, &["rm"]);
    assert!(!out.status.success(), "rm with no target must fail");
    let out = klon(&fx.golden, &["rm", "feature", "--path", "x"]);
    assert!(!out.status.success(), "rm with two targets must fail");

    // No refusal changed anything.
    assert_eq!(worktree_list(&fx.golden), before);
    assert!(!fx.wt_root().exists(), "a refused rm must create nothing");
}

#[test]
fn rm_refuses_a_klon_with_a_live_process() {
    let fx = Fixture::generate(50, 5, 5);
    let klon_path = add_feature(&fx);
    let mut sleep = Command::new("sleep")
        .arg("30")
        .current_dir(&klon_path)
        .spawn()
        .expect("spawn sleep in the klon");

    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(!out.status.success(), "rm with a live process must fail");
    assert!(
        stderr(&out).contains("live process"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(klon_path.exists(), "the tree must still exist");

    sleep.kill().expect("kill sleep");
    sleep.wait().expect("reap sleep");

    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
    assert!(!klon_path.exists(), "the tree must be gone");
}

#[test]
fn rm_by_path_removes_the_same_klon_as_by_branch() {
    let fx = Fixture::generate(50, 5, 5);
    let klon_path = add_feature(&fx);
    let out = klon(&fx.golden, &["rm", "--path", klon_path.to_str().unwrap()]);
    assert!(out.status.success(), "rm --path failed: {}", stderr(&out));
    assert!(!klon_path.exists());
    assert!(
        !worktree_list(&fx.golden).contains(&klon_path.to_string_lossy().to_string()),
        "the klon must be unregistered"
    );

    // A second klon goes away the same way by branch.
    let klon_path = add_feature(&fx);
    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
    assert!(!klon_path.exists());
}

#[test]
fn rm_refuses_a_locked_klon_even_with_force() {
    let fx = Fixture::generate(50, 5, 5);
    let klon_path = add_feature(&fx);
    git_ok(
        &fx.golden,
        &["worktree", "lock", klon_path.to_str().unwrap()],
    );

    let out = klon(&fx.golden, &["rm", "--force", "feature"]);
    assert!(!out.status.success(), "rm of a locked klon must fail");
    assert!(stderr(&out).contains("locked"), "stderr: {}", stderr(&out));
    assert!(klon_path.exists(), "the tree must still exist");

    git_ok(
        &fx.golden,
        &["worktree", "unlock", klon_path.to_str().unwrap()],
    );
    let out = klon(&fx.golden, &["rm", "feature"]);
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
}

#[test]
fn rm_returns_within_100_ms_on_the_10k_fixture() {
    let fx = Fixture::generate(10_000, 100, 1_000);
    // Three runs on fresh klons; the minimum tolerates a loaded host.
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let klon_path = add_feature(&fx);
        let t0 = Instant::now();
        let out = klon(&fx.golden, &["rm", "feature"]);
        let elapsed = t0.elapsed();
        assert!(out.status.success(), "rm failed: {}", stderr(&out));
        assert!(!klon_path.exists());
        best = best.min(elapsed);
    }
    assert!(
        best < Duration::from_millis(100),
        "the fastest rm took {best:?}; the budget is 100 ms"
    );
    let trash = fx.trash();
    assert!(
        wait_until(|| trash_is_empty(&trash), Duration::from_secs(30)),
        "the trash directory must drain within 30 s"
    );
}

#[test]
fn prune_drops_a_stale_worktree_and_drains_the_trash() {
    let fx = Fixture::generate(50, 5, 5);
    let klon_path = add_feature(&fx);

    // A registration whose directory vanished by hand is stale.
    fs::remove_dir_all(&klon_path).unwrap();
    let out = klon(&fx.golden, &["prune"]);
    assert!(out.status.success(), "prune failed: {}", stderr(&out));
    assert!(
        !worktree_list(&fx.golden).contains(&klon_path.to_string_lossy().to_string()),
        "the stale entry must be gone"
    );

    // prune drains whatever sits in the trash, in the background.
    let junk = fx.trash().join("junk-1");
    fs::create_dir_all(&junk).unwrap();
    fs::write(junk.join("a.bin"), "junk\n".repeat(1_000)).unwrap();
    let out = klon(&fx.golden, &["prune"]);
    assert!(out.status.success(), "prune failed: {}", stderr(&out));
    assert!(
        wait_until(|| trash_is_empty(&fx.trash()), Duration::from_secs(30)),
        "the trash directory must drain within 30 s"
    );
}

#[test]
fn rm_deletes_the_trash_when_priority_tools_are_missing() {
    // A PATH without setsid and ionice must not leave the delete undone:
    // klon checks every optional tool before it composes the command.
    let fx = Fixture::generate(50, 5, 5);
    let fake_bin = fx.golden.parent().unwrap().join("fakebin");
    fs::create_dir_all(&fake_bin).unwrap();
    let mut looked_up = 0;
    for tool in ["git", "rm", "nice"] {
        if let Some(real) = tool_in_path(tool) {
            std::os::unix::fs::symlink(&real, fake_bin.join(tool)).unwrap();
            looked_up += 1;
        }
    }
    if looked_up < 3 {
        println!("skip: git, rm, or nice is not on PATH");
        return;
    }

    let klon_path = add_feature(&fx);
    let out = Command::new(BIN)
        .args(["rm", "feature"])
        .current_dir(&fx.golden)
        .env("PATH", &fake_bin)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run gh-klon");
    assert!(out.status.success(), "rm failed: {}", stderr(&out));
    assert!(
        stderr(&out).contains("missing"),
        "stderr must name the missing tools: {}",
        stderr(&out)
    );
    assert!(!klon_path.exists(), "the tree must be gone");
    let trash = fx.trash();
    assert!(
        wait_until(|| trash_is_empty(&trash), Duration::from_secs(30)),
        "the trash directory must drain without setsid and ionice"
    );
}

/// The first real path of `tool` on the current PATH, or None.
fn tool_in_path(tool: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(tool))
        .find(|path| path.is_file())
}
