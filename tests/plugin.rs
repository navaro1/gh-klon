//! C28 acceptance tests: the Claude Code plugin hooks and the `--path-mode`
//! templates. The tests drive the hook scripts directly with the stdin JSON
//! from the documented contract (spec §2, consumer contract 2). A `gh` shim
//! on PATH forwards `gh klon ...` to the built binary.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{git_ok, klon, klon_env, stderr, stdout, Fixture, BIN};

const SEED: u64 = 42;

/// The hook scripts of this repository.
fn hook(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("plugin")
        .join("claude-code")
        .join("hooks")
        .join(name)
}

/// A directory with one `gh` executable that forwards `gh klon ...` to the
/// binary under test. The returned directory must outlive the hook calls.
fn gh_shim() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("shim tempdir");
    let shim = dir.path().join("gh");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\n[ \"$1\" = klon ] || {{\n    echo \"gh shim: only 'gh klon' is supported\" >&2\n    exit 64\n}}\nshift\nexec \"{}\" \"$@\"\n",
            BIN
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&shim, perms).unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

/// Run one hook script with the contract stdin JSON. The PATH holds the shim
/// first, so `gh klon` inside the hook reaches the binary under test.
fn run_hook(name: &str, shim: &Path, json: &str, extra: &[(&str, &OsStr)]) -> Output {
    let mut command = Command::new(hook(name));
    command
        .env(
            "PATH",
            format!(
                "{}:{}",
                shim.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("KLON_BIN", BIN)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in extra {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn the hook script");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(json.as_bytes())
        .expect("write the hook stdin");
    child.wait_with_output().expect("wait for the hook")
}

/// True when `path` is a registered worktree with `branch` checked out.
fn registered_with(golden: &Path, path: &Path, branch: &str) -> bool {
    let list = git_ok(golden, &["worktree", "list", "--porcelain"]);
    let block = list
        .split("\n\n")
        .find(|b| b.starts_with(&format!("worktree {}", path.display())));
    match block {
        Some(block) => block.lines().any(|l| l == format!("branch {branch}")),
        None => false,
    }
}

#[test]
fn the_create_hook_makes_a_klon_and_prints_its_path() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let (_shim_dir, shim) = gh_shim();
    let log = fx.golden.parent().unwrap().join("hook.log");
    let want = fx.golden.join(".claude").join("worktrees").join("test");

    let out = run_hook(
        "worktree-create.sh",
        &shim,
        &format!(
            "{{\"hook_event_name\":\"WorktreeCreate\",\"cwd\":\"{}\",\"name\":\"test\"}}",
            fx.golden.display()
        ),
        &[("KLON_HOOK_LOG", log.as_os_str())],
    );
    assert!(out.status.success(), "hook failed: {}", stderr(&out));
    // The contract: stdout is the path only.
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());
    assert!(!stdout(&out).contains('{'), "stdout must hold no JSON");

    // AC: `git worktree list` shows the path as a klon on branch worktree-test.
    assert!(
        registered_with(&fx.golden, &want, "refs/heads/worktree-test"),
        "the klon is not registered: {}",
        git_ok(&fx.golden, &["worktree", "list", "--porcelain"])
    );
    assert!(want.join(".git").is_file(), "the klon has no .git file");
    assert_eq!(
        git_ok(&want, &["status", "--porcelain"]),
        "",
        "the klon must be clean"
    );
    // The log line tells S3 that the hook ran.
    let log_text = fs::read_to_string(&log).expect("the hook log");
    assert!(log_text.contains("WorktreeCreate ok"), "log: {log_text}");
}

#[test]
fn the_remove_hook_removes_the_klon() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let (_shim_dir, shim) = gh_shim();
    let path = fx.golden.join(".claude").join("worktrees").join("gone");
    let out = klon(
        &fx.golden,
        &["add", "worktree-gone", "--path", path.to_str().unwrap()],
    );
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    let log = fx.golden.parent().unwrap().join("hook.log");

    let out = run_hook(
        "worktree-remove.sh",
        &shim,
        &format!(
            "{{\"hook_event_name\":\"WorktreeRemove\",\"cwd\":\"{}\",\"worktree_path\":\"{}\"}}",
            fx.golden.display(),
            path.display()
        ),
        &[("KLON_HOOK_LOG", log.as_os_str())],
    );
    assert!(out.status.success(), "hook failed: {}", stderr(&out));

    // AC: `git worktree list` no longer shows it, and the tree is gone.
    assert!(
        !git_ok(&fx.golden, &["worktree", "list", "--porcelain"])
            .contains(&path.display().to_string()),
        "the klon is still registered"
    );
    assert!(!path.exists(), "the klon directory is still there");
    let log_text = fs::read_to_string(&log).expect("the hook log");
    assert!(log_text.contains("WorktreeRemove ok"), "log: {log_text}");

    // An absent path is not a failure: the removal is already done.
    let out = run_hook(
        "worktree-remove.sh",
        &shim,
        &format!(
            "{{\"hook_event_name\":\"WorktreeRemove\",\"worktree_path\":\"{}\"}}",
            path.display()
        ),
        &[("KLON_HOOK_LOG", log.as_os_str())],
    );
    assert!(out.status.success(), "hook on an absent path failed");
    assert!(log_text.contains("WorktreeRemove ok"));
}

#[test]
fn a_failed_add_makes_the_create_hook_exit_non_zero() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let (_shim_dir, shim) = gh_shim();
    let body = |name: &str| {
        format!(
            "{{\"hook_event_name\":\"WorktreeCreate\",\"cwd\":\"{}\",\"name\":\"{name}\"}}",
            fx.golden.display()
        )
    };
    let first = run_hook("worktree-create.sh", &shim, &body("dup"), &[]);
    assert!(
        first.status.success(),
        "first hook failed: {}",
        stderr(&first)
    );

    // The second `add` refuses the branch that the first klon checks out.
    let second = run_hook("worktree-create.sh", &shim, &body("dup"), &[]);
    assert!(
        !second.status.success(),
        "the hook must propagate the add failure"
    );
    let err = stderr(&second);
    // The refusal order decides which rule fires first; both are `add` errors.
    // The contract needs the klon error on stderr and a non-zero exit.
    assert!(
        err.contains("klon:"),
        "stderr must carry the add error: {err}"
    );
    assert!(
        err.contains("gh klon add failed for worktree-dup"),
        "the hook must name the failed command: {err}"
    );
}

#[test]
fn the_sed_fallback_parses_the_hook_input() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let (_shim_dir, shim) = gh_shim();
    let want = fx.golden.join(".claude").join("worktrees").join("sedtest");
    let out = run_hook(
        "worktree-create.sh",
        &shim,
        &format!(
            "{{\"hook_event_name\":\"WorktreeCreate\",\"cwd\":\"{}\",\"name\":\"sedtest\"}}",
            fx.golden.display()
        ),
        &[("KLON_HOOK_NO_JQ", OsStr::new("1"))],
    );
    assert!(out.status.success(), "hook failed: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());
    assert!(registered_with(
        &fx.golden,
        &want,
        "refs/heads/worktree-sedtest"
    ));
}

#[test]
fn the_create_hook_lands_on_the_repository_root_from_a_subdirectory() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let (_shim_dir, shim) = gh_shim();
    let subdirectory = fx.golden.join("d000");
    let want = fx.golden.join(".claude").join("worktrees").join("subtest");

    let out = run_hook(
        "worktree-create.sh",
        &shim,
        &format!(
            "{{\"hook_event_name\":\"WorktreeCreate\",\"cwd\":\"{}\",\"name\":\"subtest\"}}",
            subdirectory.display()
        ),
        &[],
    );
    assert!(out.status.success(), "hook failed: {}", stderr(&out));
    // The klon lives under the repository root, not under the subdirectory.
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());
    assert!(
        registered_with(&fx.golden, &want, "refs/heads/worktree-subtest"),
        "the klon is not registered"
    );
    assert!(
        !subdirectory.join(".claude").exists(),
        "the subdirectory must stay empty of klons"
    );
}

#[test]
fn a_missing_cwd_makes_the_create_hook_fail_loudly() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let (_shim_dir, shim) = gh_shim();
    let absent = fx.golden.parent().unwrap().join("no-such-repo");

    let out = run_hook(
        "worktree-create.sh",
        &shim,
        &format!(
            "{{\"hook_event_name\":\"WorktreeCreate\",\"cwd\":\"{}\",\"name\":\"ghost\"}}",
            absent.display()
        ),
        &[],
    );
    assert!(
        !out.status.success(),
        "a cwd without a repository must fail"
    );
    let err = stderr(&out);
    assert!(
        err.contains("cannot find the git repository"),
        "the hook must name the missing repository: {err}"
    );
}

#[test]
fn add_with_the_claude_path_mode_uses_the_claude_convention() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let want = fx.golden.join(".claude").join("worktrees").join("x");

    let out = klon(&fx.golden, &["add", "x", "--path-mode", "claude"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());
    // AC: the branch is worktree-x.
    assert!(
        registered_with(&fx.golden, &want, "refs/heads/worktree-x"),
        "the klon is not registered on worktree-x"
    );
    assert_eq!(
        git_ok(&want, &["status", "--porcelain"]),
        "",
        "the klon must be clean"
    );

    // The claude mode renames the branch, so a second name is free.
    let out = klon(&fx.golden, &["add", "y", "--path-mode", "claude"]);
    assert!(out.status.success(), "second add failed: {}", stderr(&out));

    // `--path` is the explicit escape hatch and conflicts with the mode.
    let out = klon(
        &fx.golden,
        &["add", "z", "--path-mode", "claude", "--path", "p"],
    );
    assert!(
        !out.status.success(),
        "--path and --path-mode must conflict"
    );
}

#[test]
fn the_other_path_modes_set_the_documented_templates() {
    let fx = Fixture::generate(SEED, 100, 10, 5, 5);
    let tmp = fx.golden.parent().unwrap();

    // t3: `~/.t3/worktrees/{repo}/{branch}` with HOME redirected.
    let home = tmp.join("home");
    fs::create_dir(&home).unwrap();
    let out = klon_env(
        &fx.golden,
        &[("HOME", home.as_os_str())],
        &["add", "feature", "--path-mode", "t3"],
    );
    assert!(out.status.success(), "t3 add failed: {}", stderr(&out));
    let want = home
        .join(".t3")
        .join("worktrees")
        .join("golden")
        .join("feature");
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());
    assert!(registered_with(&fx.golden, &want, "refs/heads/feature"));

    // codex: `$CODEX_HOME/worktrees/{branch}`. The branch name stays the
    // argument, so each mode uses its own name here.
    let codex_home = tmp.join("codex-home");
    let out = klon_env(
        &fx.golden,
        &[("CODEX_HOME", codex_home.as_os_str())],
        &["add", "codexmode", "--path-mode", "codex"],
    );
    assert!(out.status.success(), "codex add failed: {}", stderr(&out));
    let want = codex_home.join("worktrees").join("codexmode");
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());

    // sibling: klon's own default, and it overrides a configured template.
    fs::write(fx.golden.join(".klon.toml"), "path = \"custom/{branch}\"\n").unwrap();
    let out = klon(&fx.golden, &["add", "sibmode", "--path-mode", "sibling"]);
    assert!(out.status.success(), "sibling add failed: {}", stderr(&out));
    let want = tmp.join("golden.wt").join("sibmode");
    assert_eq!(stdout(&out).trim(), want.to_str().unwrap());
    assert!(
        !tmp.join("custom").join("sibmode").exists(),
        "sibling must ignore the configured template"
    );

    // The mode needs a name: it is the worktree name, not optional.
    let out = klon(&fx.golden, &["add", "--path-mode", "claude"]);
    assert!(!out.status.success(), "the claude mode needs a name");
}
