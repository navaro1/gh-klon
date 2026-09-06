//! The exclude set and the path fixup pass (spec §7 C11, R3, R15, R39).
//!
//! Every test builds one small fixture, runs `gh klon add`, and reads the
//! result out of the klon. The pass runs inside `add`, so these tests exercise
//! the real transaction and not a private entry point.

mod common;

use common::{git_ok, klon, stderr, Fixture};
use std::fs;
use std::path::{Path, PathBuf};

const SEED: u64 = 61;

/// A fixture with a `feature` branch, plus the ignored files the test needs.
/// `build/` is already ignored by the generated `.gitignore`.
fn fixture() -> Fixture {
    Fixture::generate(SEED, 20, 2, 2, 3)
}

/// Run `add feature` and answer the klon path. The test fails when `add` does.
fn add(fx: &Fixture, extra: &[&str]) -> PathBuf {
    let mut args = vec!["add", "feature"];
    args.extend_from_slice(extra);
    let out = klon(&fx.golden, &args);
    assert!(out.status.success(), "add failed: {}", stderr(&out));
    fx.default_klon_path()
}

/// The lines of `<klon>/.klon/fixup.log`, or an empty list when it is absent.
fn log_lines(klon_path: &Path) -> Vec<String> {
    match fs::read_to_string(klon_path.join(".klon").join("fixup.log")) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// True when `text` names golden itself, and not only a sibling that starts
/// with the same characters. The default klon path is `<golden>.wt/<branch>`,
/// so a plain `contains` of golden's path is true of every rewritten line too.
fn names_golden(text: &str, golden: &Path) -> bool {
    let needle = golden.to_str().unwrap();
    let mut rest = text;
    while let Some(at) = rest.find(needle) {
        rest = &rest[at + needle.len()..];
        let next = rest.chars().next();
        if next.is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '.' | '-' | '_' | '~' | '+')) {
            return true;
        }
    }
    false
}

/// Append `lines` to golden's `.gitignore` and commit, so `git clean` in the
/// klon keeps the matching paths.
fn ignore_more(fx: &Fixture, lines: &str) {
    let file = fx.golden.join(".gitignore");
    let mut text = fs::read_to_string(&file).unwrap();
    text.push_str(lines);
    fs::write(&file, text).unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "ignore more"]);
    // The feature branch needs the same ignore rules, because `add` checks it out.
    git_ok(&fx.golden, &["checkout", "-q", "feature"]);
    git_ok(&fx.golden, &["merge", "-q", "main", "-m", "merge"]);
    git_ok(&fx.golden, &["checkout", "-q", "main"]);
}

// --- The exclude set (R39) -----------------------------------------------------

#[test]
fn a_klonignore_path_is_absent_from_the_klon() {
    let fx = fixture();
    fs::create_dir(fx.golden.join("build").join("cache")).unwrap();
    fs::write(fx.golden.join("build/cache/c.bin"), "cache\n").unwrap();
    fs::write(fx.golden.join(".klonignore"), "/build/cache/\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(klon_path.join("build/o0.bin").exists(), "build/ is cloned");
    assert!(
        !klon_path.join("build/cache").exists(),
        ".klonignore must keep build/cache out of the klon"
    );
}

#[test]
fn a_nested_git_directory_inside_an_ignored_directory_is_absent() {
    let fx = fixture();
    let nested = fx.golden.join("build").join("vendor");
    fs::create_dir_all(nested.join(".git")).unwrap();
    fs::write(nested.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(nested.join("code.txt"), "vendored\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("build/vendor/code.txt").exists(),
        "the vendored directory itself is cloned"
    );
    assert!(
        !klon_path.join("build/vendor/.git").exists(),
        "a nested .git must never reach the klon"
    );
}

#[test]
fn a_submodule_path_is_absent_from_the_klon() {
    let fx = fixture();
    // `vendor/` is ignored, so `git clean` keeps whatever the clone left there
    // and the test measures the exclude set alone.
    ignore_more(&fx, "/vendor/\n");
    // A `.gitmodules` entry alone is enough: the exclude set asks git for the
    // paths, so the test needs no second repository.
    fs::write(
        fx.golden.join(".gitmodules"),
        "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = ../lib.git\n",
    )
    .unwrap();
    fs::create_dir_all(fx.golden.join("vendor/lib")).unwrap();
    fs::write(fx.golden.join("vendor/lib/keep.txt"), "sub\n").unwrap();
    fs::write(fx.golden.join("vendor/other.txt"), "not a submodule\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("vendor/other.txt").exists(),
        "the rest of vendor/ is cloned"
    );
    assert!(
        !klon_path.join("vendor/lib").exists(),
        "a submodule path must stay out of the klon"
    );
}

#[test]
fn worktreeinclude_takes_back_a_klonignore_exclusion() {
    let fx = fixture();
    let cache = fx.golden.join("build").join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("c.bin"), "cache\n").unwrap();
    fs::create_dir(cache.join("keep")).unwrap();
    fs::write(cache.join("keep").join("k.bin"), "keep me\n").unwrap();
    fs::write(fx.golden.join(".klonignore"), "/build/cache/\n").unwrap();
    fs::write(fx.golden.join(".worktreeinclude"), "/build/cache/keep/\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("build/cache/keep/k.bin").exists(),
        ".worktreeinclude must take back the .klonignore exclusion"
    );
    assert!(
        !klon_path.join("build/cache/c.bin").exists(),
        "the rest of the excluded directory stays out"
    );
}

/// The submodule query must match only the `path` key. An unanchored pattern
/// also returns `submodule.<name>.url` and `.branch` when the name holds
/// `path`, and a branch named `main` would then drop golden's own `main/`.
#[test]
fn a_submodule_named_path_does_not_exclude_its_branch_name() {
    let fx = fixture();
    ignore_more(&fx, "/vendor/\n/main/\n");
    fs::write(
        fx.golden.join(".gitmodules"),
        "[submodule \"path-lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n\tbranch = main\n",
    )
    .unwrap();
    fs::create_dir_all(fx.golden.join("vendor/lib")).unwrap();
    fs::write(fx.golden.join("vendor/lib/keep.txt"), "sub\n").unwrap();
    fs::create_dir(fx.golden.join("main")).unwrap();
    fs::write(fx.golden.join("main/file.txt"), "not a submodule\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("main/file.txt").exists(),
        "the branch name of a submodule must not exclude a directory"
    );
    assert!(
        !klon_path.join("vendor/lib").exists(),
        "the submodule path itself stays out"
    );
}

/// A wildcard include must reach through the directories that `.klonignore`
/// excludes, or the walk prunes the directory that holds the wanted file.
#[test]
fn a_wildcard_worktreeinclude_reaches_a_deep_file() {
    let fx = fixture();
    let deep = fx.golden.join("build").join("deep").join("deeper");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("keep.txt"), "keep me\n").unwrap();
    fs::write(deep.join("drop.txt"), "drop me\n").unwrap();
    fs::write(fx.golden.join(".klonignore"), "/build/\n").unwrap();
    fs::write(fx.golden.join(".worktreeinclude"), "build/**/keep.txt\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("build/deep/deeper/keep.txt").exists(),
        "a wildcard include must reach through the excluded directories"
    );
    assert!(
        !klon_path.join("build/deep/deeper/drop.txt").exists(),
        "only the matching file comes back"
    );
}

/// gitignore keeps a trailing space when a backslash escapes it, so the raw
/// line is the pattern.
#[test]
fn a_worktreeinclude_line_keeps_an_escaped_trailing_space() {
    let fx = fixture();
    fs::write(fx.golden.join("build").join("keep "), "keep\n").unwrap();
    fs::write(fx.golden.join(".klonignore"), "/build/\n").unwrap();
    fs::write(fx.golden.join(".worktreeinclude"), "/build/keep\\ \n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("build/keep ").exists(),
        "an escaped trailing space names a real file"
    );
}

#[test]
fn worktreeinclude_does_not_take_back_a_nested_git() {
    let fx = fixture();
    let vendor = fx.golden.join("build").join("vendor");
    fs::create_dir_all(vendor.join(".git")).unwrap();
    fs::write(vendor.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(fx.golden.join(".worktreeinclude"), "/build/vendor/.git/\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        !klon_path.join("build/vendor/.git").exists(),
        "an include line must not take back the .git rule"
    );
}

// --- The rewrite rails (R15) ---------------------------------------------------

#[test]
fn a_small_text_file_is_rewritten_and_the_log_names_it() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    let file = fx.golden.join("build").join("config.yaml");
    // About 100 KB, with golden's path twice.
    let filler = "x".repeat(50_000);
    fs::write(
        &file,
        format!("storeDir: {golden_text}/store\n{filler}\ncacheDir: {golden_text}/cache\n{filler}"),
    )
    .unwrap();

    let klon_path = add(&fx, &[]);
    let rewritten = fs::read_to_string(klon_path.join("build/config.yaml")).unwrap();
    assert!(
        !names_golden(&rewritten, &fx.golden),
        "golden's path must be gone from the rewritten file"
    );
    assert!(
        rewritten.contains(&format!("storeDir: {}/store", klon_path.display())),
        "the klon path must replace it"
    );
    assert!(
        log_lines(&klon_path).contains(&"build/config.yaml 2".to_string()),
        ".klon/fixup.log must name the file and the count: {:?}",
        log_lines(&klon_path)
    );
}

#[test]
fn a_sibling_path_that_starts_with_goldens_name_is_not_rewritten() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    // `<golden>-docs` and `<golden>.wt` are two other trees, not golden.
    let text = format!("docs: {golden_text}-docs\nwt: {golden_text}.wt/other\n");
    fs::write(fx.golden.join("build").join("siblings.conf"), &text).unwrap();

    let klon_path = add(&fx, &[]);
    assert_eq!(
        fs::read_to_string(klon_path.join("build/siblings.conf")).unwrap(),
        text,
        "only a whole path component may move"
    );
}

#[test]
fn a_two_megabyte_text_file_is_not_rewritten() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    let big = format!("path {golden_text}\n{}", "y".repeat(2 * 1024 * 1024));
    fs::write(fx.golden.join("build").join("big.log"), &big).unwrap();

    let klon_path = add(&fx, &[]);
    let copied = fs::read_to_string(klon_path.join("build/big.log")).unwrap();
    assert!(
        names_golden(&copied, &fx.golden),
        "a file above 1 MB must keep golden's path"
    );
    assert!(
        !log_lines(&klon_path)
            .iter()
            .any(|l| l.starts_with("build/big.log")),
        "the log must not name a file the pass left alone"
    );
}

#[test]
fn a_sqlite_file_is_not_rewritten() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    // Valid UTF-8 and small: only the extension keeps the pass out.
    fs::write(
        fx.golden.join("build").join("state.sqlite"),
        format!("SQLite format 3 {golden_text}\n"),
    )
    .unwrap();

    let klon_path = add(&fx, &[]);
    let copied = fs::read_to_string(klon_path.join("build/state.sqlite")).unwrap();
    assert!(
        names_golden(&copied, &fx.golden),
        "a .sqlite file must keep golden's path"
    );
}

#[test]
fn a_binary_file_without_an_extension_is_not_rewritten() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap();
    let mut bytes = b"\x7fELF\x00\x00payload ".to_vec();
    bytes.extend_from_slice(golden_text.as_bytes());
    bytes.extend_from_slice(&[0u8, 0xff, 0xfe]);
    fs::write(fx.golden.join("build").join("program"), &bytes).unwrap();

    let klon_path = add(&fx, &[]);
    assert_eq!(
        fs::read(klon_path.join("build/program")).unwrap(),
        bytes,
        "a file with a NUL byte must stay byte for byte the same"
    );
}

/// The searcher stops at the first hit, so a NUL byte after the hit never
/// reaches it. The pass scans the whole file for a NUL of its own.
#[test]
fn a_file_with_a_nul_byte_after_the_hit_is_not_rewritten() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap();
    let mut bytes = format!("dir: {golden_text}\n").into_bytes();
    bytes.extend_from_slice(&[b'x'; 200]);
    bytes.push(0);
    bytes.extend_from_slice(&[b'y'; 200]);
    fs::write(fx.golden.join("build").join("late-nul.dat"), &bytes).unwrap();

    let klon_path = add(&fx, &[]);
    assert_eq!(
        fs::read(klon_path.join("build/late-nul.dat")).unwrap(),
        bytes,
        "a NUL anywhere in the file must stop the rewrite"
    );
}

/// R15 asks klon to rewrite the path. A refused write means the klon would
/// keep golden's path, so `add` must fail instead of reporting success.
#[test]
fn a_refused_rewrite_fails_the_add() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap();
    let locked = fx.golden.join("build").join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("conf.yaml"), format!("dir: {golden_text}\n")).unwrap();
    // A read-only directory refuses the temporary file that a rewrite renames.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let out = klon(&fx.golden, &["add", "feature"]);
    // Let the harness delete the fixture even after the test fails.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !out.status.success(),
        "add must fail when the pass cannot rewrite a file"
    );
    assert!(
        stderr(&out).contains("conf.yaml"),
        "the error must name the file: {}",
        stderr(&out)
    );
    assert!(
        !fx.default_klon_path().exists(),
        "the failed add must roll the klon back"
    );
}

#[test]
fn a_rewrite_keeps_the_mode_and_the_mtime() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    let file = fx.golden.join("build").join("run.sh");
    fs::write(&file, format!("#!/bin/sh\nexec {golden_text}/tool\n")).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::metadata(&file).unwrap();

    let klon_path = add(&fx, &[]);
    let after = fs::metadata(klon_path.join("build/run.sh")).unwrap();
    assert!(
        !names_golden(
            &fs::read_to_string(klon_path.join("build/run.sh")).unwrap(),
            &fx.golden
        ),
        "the file must be rewritten"
    );
    assert_eq!(
        after.permissions().mode() & 0o777,
        0o755,
        "the rewrite must keep the mode"
    );
    assert_eq!(
        after.modified().unwrap(),
        before.modified().unwrap(),
        "the rewrite must restore the mtime, so cargo and make see no change"
    );
}

#[test]
fn a_symlink_into_golden_is_rewritten() {
    let fx = fixture();
    let build = fx.golden.join("build");
    std::os::unix::fs::symlink(fx.golden.join("build").join("o0.bin"), build.join("into")).unwrap();
    std::os::unix::fs::symlink("o1.bin", build.join("beside")).unwrap();

    let klon_path = add(&fx, &[]);
    assert_eq!(
        fs::read_link(klon_path.join("build/into")).unwrap(),
        klon_path.join("build").join("o0.bin"),
        "a target inside golden must follow the klon"
    );
    assert_eq!(
        fs::read_link(klon_path.join("build/beside")).unwrap(),
        PathBuf::from("o1.bin"),
        "a relative target must stay unchanged"
    );
    assert!(
        log_lines(&klon_path).contains(&"build/into symlink".to_string()),
        "the log must name the rewritten symlink: {:?}",
        log_lines(&klon_path)
    );
}

#[test]
fn the_delete_list_is_removed_at_any_depth() {
    let fx = fixture();
    ignore_more(&fx, "/web/\n");
    let next = fx.golden.join("web").join(".next");
    fs::create_dir_all(next.join("cache").join("webpack")).unwrap();
    fs::write(next.join("cache").join("webpack").join("0.pack"), "cache\n").unwrap();
    fs::write(next.join("BUILD_ID"), "keep\n").unwrap();
    fs::write(fx.golden.join("web").join(".ninja_log"), "log\n").unwrap();
    fs::create_dir(fx.golden.join("web").join("deep")).unwrap();
    fs::write(fx.golden.join("web/deep/.ninja_deps"), "deps\n").unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        klon_path.join("web/.next/BUILD_ID").exists(),
        ".next itself stays"
    );
    for gone in ["web/.next/cache", "web/.ninja_log", "web/deep/.ninja_deps"] {
        assert!(
            !klon_path.join(gone).exists(),
            "{gone} must be deleted from the klon"
        );
    }
    let lines = log_lines(&klon_path);
    for gone in ["web/.next/cache", "web/.ninja_log", "web/deep/.ninja_deps"] {
        assert!(
            lines.contains(&format!("{gone} deleted")),
            "the log must name {gone}: {lines:?}"
        );
    }
}

// --- The controls --------------------------------------------------------------

#[test]
fn no_fixup_skips_every_change() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    let build = fx.golden.join("build");
    fs::write(build.join("config.yaml"), format!("dir: {golden_text}\n")).unwrap();
    std::os::unix::fs::symlink(fx.golden.join("build/o0.bin"), build.join("into")).unwrap();
    fs::write(build.join(".ninja_log"), "log\n").unwrap();

    let klon_path = add(&fx, &["--no-fixup"]);
    assert!(
        names_golden(
            &fs::read_to_string(klon_path.join("build/config.yaml")).unwrap(),
            &fx.golden
        ),
        "--no-fixup must leave the content alone"
    );
    assert_eq!(
        fs::read_link(klon_path.join("build/into")).unwrap(),
        fx.golden.join("build").join("o0.bin"),
        "--no-fixup must leave the symlink alone"
    );
    assert!(
        klon_path.join("build/.ninja_log").exists(),
        "--no-fixup must delete nothing"
    );
    assert!(
        log_lines(&klon_path).is_empty(),
        "--no-fixup must write no log"
    );
}

#[test]
fn a_fixup_skip_glob_keeps_a_file_out_of_the_rewrite() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    let build = fx.golden.join("build");
    fs::write(build.join("keep.conf"), format!("dir: {golden_text}\n")).unwrap();
    fs::write(build.join("fix.conf"), format!("dir: {golden_text}\n")).unwrap();
    fs::write(
        fx.golden.join(".klon.toml"),
        "[fixup]\nskip = [\"**/keep.conf\"]\n",
    )
    .unwrap();

    let klon_path = add(&fx, &[]);
    assert!(
        names_golden(
            &fs::read_to_string(klon_path.join("build/keep.conf")).unwrap(),
            &fx.golden
        ),
        "a [fixup] skip glob must keep the file out of the rewrite"
    );
    assert!(
        !names_golden(
            &fs::read_to_string(klon_path.join("build/fix.conf")).unwrap(),
            &fx.golden
        ),
        "a file outside the glob is still rewritten"
    );
}

#[test]
fn the_pass_leaves_tracked_files_alone() {
    let fx = fixture();
    let golden_text = fx.golden.to_str().unwrap().to_string();
    let tracked = fx.golden.join("tracked.txt");
    fs::write(&tracked, format!("golden lives at {golden_text}\n")).unwrap();
    git_ok(&fx.golden, &["add", "-A"]);
    git_ok(&fx.golden, &["commit", "-qm", "a tracked path"]);
    git_ok(&fx.golden, &["checkout", "-q", "feature"]);
    git_ok(&fx.golden, &["merge", "-q", "main", "-m", "merge"]);
    git_ok(&fx.golden, &["checkout", "-q", "main"]);

    let klon_path = add(&fx, &[]);
    assert!(
        names_golden(
            &fs::read_to_string(klon_path.join("tracked.txt")).unwrap(),
            &fx.golden
        ),
        "a tracked file must keep its bytes, or the klon would be dirty"
    );
    let status = git_ok(&klon_path, &["status", "--porcelain"]);
    assert_eq!(status, "", "the klon must stay clean after the pass");
}
