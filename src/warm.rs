//! The background warm of the `copy` backend (R36, spec §7 C12).
//!
//! On ext4 a whole-directory copy of a build tree takes 40 to 100 seconds. A
//! person does not need `target/` to read the code, so `add` copies the
//! tracked files and every small ignored directory inline, and hands the big
//! ones to a detached process. That process fills a staging directory beside
//! the real name and lands it with one rename, so a reader never sees a
//! half-filled `target/`.
//!
//! | Strategy | When | What happens |
//! |---|---|---|
//! | inline | a small directory | the clone copies it before `add` returns |
//! | copy | above `[copy] inline_limit`, or more than 2000 files | the warm process copies and renames it |
//! | reinstall | the directory is named in `[copy] reinstall` | the approved command runs inside the klon |
//!
//! `<klon>/.klon/warming.json` names the directories that have not landed.
//! `list` reads it and marks the klon `warming`; the warm process deletes an
//! entry after each rename and the file at the end.
//!
//! Only the `copy` backend warms. `btrfs-snapshot` and `reflink-walk` finish
//! the whole tree in milliseconds, so a background pass would only add risk.

use crate::backend::{copy, Exclusions};
use crate::config::{self, Config};
use crate::envelope::env;
use crate::{fixup, git, process, time, Error, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The format version of `warming.json`. Another version reads as no marker,
/// so an old klon never blocks a new `list`.
pub const VERSION: u32 = 1;

/// The marker file inside `<klon>/.klon`.
const MARKER: &str = "warming.json";

/// The stderr of the warm process, inside `<klon>/.klon`.
const LOG: &str = "warm.log";

/// The suffix of the staging directory. `add` teaches git to ignore it, so a
/// half-filled copy never shows up as an untracked path.
pub const STAGING_SUFFIX: &str = ".klon-warming";

/// A directory with more entries than this goes to the warm process whatever
/// its size. A node_modules tree is small in bytes and slow in files.
const MAX_INLINE_FILES: u64 = 2000;

/// What the warm process does with one top-level ignored directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Action {
    /// Copy it from golden and land it with one rename.
    Copy,
    /// Run this command inside the klon instead of copying. The user approved
    /// the command through the `.klon.toml` gate before `add` wrote the marker.
    Reinstall { command: String },
}

/// One directory and its action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub dir: String,
    #[serde(flatten)]
    pub action: Action,
}

/// `<klon>/.klon/warming.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub version: u32,
    /// The directories that have not landed yet.
    pub pending: Vec<String>,
    /// When the warm process started, RFC 3339 in UTC.
    pub started: String,
    /// What the warm process must still do. It keeps the reinstall commands,
    /// so the detached process never reads `.klon.toml` a second time and can
    /// never run a command the user did not approve.
    pub steps: Vec<Step>,
    /// `add --no-fixup` (R15). The warm process reads it here, because the
    /// switch belongs to the `add` that started it and no later command can
    /// know it.
    #[serde(default)]
    pub no_fixup: bool,
}

/// The path of the marker file.
fn marker_path(klon: &Path) -> PathBuf {
    env::dir(klon).join(MARKER)
}

/// The marker of `klon`, or None when it is absent, unreadable, or written by
/// another version.
pub fn marker(klon: &Path) -> Option<Marker> {
    let text = fs::read_to_string(marker_path(klon)).ok()?;
    let marker: Marker = serde_json::from_str(&text).ok()?;
    (marker.version == VERSION).then_some(marker)
}

/// The directories a klon still waits for. `list` shows `warming` while this
/// list is not empty.
pub fn pending(klon: &Path) -> Vec<String> {
    marker(klon).map(|m| m.pending).unwrap_or_default()
}

/// Write the marker through a temporary file and one rename, so a reader sees
/// either the old list or the new one.
fn write_marker(klon: &Path, state: &Marker) -> Result<()> {
    let path = marker_path(klon);
    let dir = env::dir(klon);
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let text = serde_json::to_string(state)
        .map_err(|err| Error::klon(format!("serialize the warm marker: {err}")))?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text).map_err(Error::io(format!("write {}", temp.display())))?;
    fs::rename(&temp, &path).map_err(Error::io(format!("write {}", path.display())))
}

// --- The plan ------------------------------------------------------------------

/// Decide what the warm process takes (R36).
///
/// `excludes` must be the clone's exclusions before the warm directories join
/// them, because the survey behind the decision is the same walk that the
/// free-space guard reads. `backend` is the selected backend name: only `copy`
/// warms.
pub fn plan(
    golden: &Path,
    excludes: &Exclusions,
    config: &Config,
    backend: &str,
) -> Result<Vec<Step>> {
    if backend != "copy" {
        return Ok(Vec::new());
    }
    let section = config.copy.as_ref();
    let limit = section.map_or(config::DEFAULT_INLINE_LIMIT, |c| c.inline_limit());
    let reinstall = section.and_then(|c| c.reinstall.as_ref());
    let survey = copy::survey(golden, excludes);
    let mut steps = Vec::new();
    for dir in ignored_dirs(golden)? {
        if let Some(command) = reinstall.and_then(|map| map.get(&dir)) {
            steps.push(Step {
                action: Action::Reinstall {
                    command: command.clone(),
                },
                dir,
            });
            continue;
        }
        let sizes = survey.dirs.get(&dir).copied().unwrap_or_default();
        if sizes.bytes > limit || sizes.files > MAX_INLINE_FILES {
            steps.push(Step {
                dir,
                action: Action::Copy,
            });
        }
    }
    Ok(steps)
}

/// The names of the top-level ignored directories of golden.
///
/// `--directory` collapses a fully ignored directory into one name, so the
/// list holds `target/` and not every file below it. A name with a separator
/// inside it names a directory below the top level, which the strategy does
/// not cover; klon's own `.klon` state never joins the list either.
fn ignored_dirs(golden: &Path) -> Result<Vec<String>> {
    let listed = git::run_input(
        golden,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
        b"",
        &[0],
    );
    // The plan is an optimization, never a rule. A repository that cannot list
    // its ignored directories still gets a klon: klon copies every one of them
    // inline, which is what it did before this strategy existed. One stderr
    // line reports the loss (spec §5).
    let out = match listed {
        Ok((_, out)) => out,
        Err(err) => {
            eprintln!("klon: cannot plan the copy strategy: {err}");
            eprintln!("klon: every ignored directory is copied before add returns");
            return Ok(Vec::new());
        }
    };
    let mut dirs = Vec::new();
    for record in out.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let Ok(name) = std::str::from_utf8(record) else {
            continue;
        };
        let Some(name) = name.strip_suffix('/') else {
            continue;
        };
        if name.is_empty() || name.contains('/') || name.starts_with(".klon") {
            continue;
        }
        dirs.push(name.to_string());
    }
    Ok(dirs)
}

// --- The start -----------------------------------------------------------------

/// Write the marker and start the detached warm process. The answer is the
/// list of directories the klon now waits for, which `add --json` reports.
///
/// The call never fails. It runs after the journal entry closed and after the
/// unlock, so a failure here would hand back an error together with a
/// registered klon that no repair can finish. A marker or a spawn that fails
/// therefore leaves no marker and draws two stderr lines naming the
/// directories the klon lacks. `add` still succeeds, because the transaction
/// it owns is complete.
pub fn start(golden: &Path, klon: &Path, steps: Vec<Step>, no_fixup: bool) -> Vec<String> {
    if steps.is_empty() {
        return Vec::new();
    }
    let pending: Vec<String> = steps.iter().map(|s| s.dir.clone()).collect();
    let state = Marker {
        version: VERSION,
        pending: pending.clone(),
        started: time::now_rfc3339(),
        steps,
        no_fixup,
    };
    let started = write_marker(klon, &state).and_then(|()| spawn(golden, klon));
    if let Err(err) = started {
        let _ = fs::remove_file(marker_path(klon));
        eprintln!("klon: {err}");
        eprintln!(
            "klon: {} did not fill; copy them by hand or add the klon again",
            pending.join(", ")
        );
        return Vec::new();
    }
    pending
}

/// Start `gh-klon warm <klon> <golden>` detached and at the lowest priority.
///
/// The process runs with the klon as its working directory and carries the
/// klon's `stop` tags, so `stop` and the live-process check of `rm` both see
/// it (R22). Its stderr lands in `<klon>/.klon/warm.log`, because a detached
/// process has no terminal to report to.
fn spawn(golden: &Path, klon: &Path) -> Result<()> {
    let log = env::dir(klon).join(LOG);
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(Error::io(format!("open {}", log.display())))?;
    let mut vars = Vec::new();
    if let Some(name) = env::value(klon, "KLON_NAME") {
        vars.push(("KLON_ID".to_string(), name));
        vars.push(("KLON_DIR".to_string(), klon.to_string_lossy().into_owned()));
    }
    let args: Vec<&OsStr> = vec![OsStr::new("warm"), klon.as_os_str(), golden.as_os_str()];
    process::spawn_detached_klon_with(
        &args,
        "background warm",
        process::Detached {
            cwd: Some(klon),
            log: Some(file),
            env: vars,
        },
    )
}

// --- The warm process ------------------------------------------------------------

/// The body of the hidden `gh-klon warm <klon> <golden>` subcommand.
///
/// It walks the steps in order. Each copy fills `<dir>.klon-warming`, takes
/// the C11 path fixup, and lands with one rename. A step that fails prints one
/// line into the log and the next step still runs, because a broken `target/`
/// must not cost the klon its `node_modules`.
pub fn run(klon: &Path, golden: &Path) -> Result<()> {
    let Some(mut state) = marker(klon) else {
        return Ok(());
    };
    let config = config::load(golden)?;
    let exclude = clone_exclusions(golden, klon);
    // The test-only pause fires once, before the first rename of the run.
    let mut test_pause = true;
    for step in state.steps.clone() {
        // `rm` can take the klon away at any point. Every step checks first,
        // so the warm process never writes into a directory that is on its way
        // to the trash.
        if !alive(klon) {
            return Ok(());
        }
        let outcome = match &step.action {
            Action::Copy => land(
                golden,
                klon,
                &step.dir,
                &exclude,
                &config,
                state.no_fixup,
                &mut test_pause,
            ),
            Action::Reinstall { command } => reinstall(klon, command),
        };
        match outcome {
            // A step that failed stays pending. `list` then keeps reporting
            // the klon as warming, which is the truth: the directory is not
            // there. `warm.log` names the reason.
            Err(err) => eprintln!("klon: warm {}: {err}", step.dir),
            Ok(()) => state.pending.retain(|name| name != &step.dir),
        }
        if alive(klon) {
            write_marker(klon, &state)?;
        }
    }
    if alive(klon) && state.pending.is_empty() {
        // The marker goes first, so the last write this process makes to the
        // klon happens before the cache warm and cannot invalidate it. The
        // marker lives in `.klon/`, which `info/exclude` hides, so git holds no
        // cache node for it today; the order keeps that from being load-bearing.
        let _ = fs::remove_file(marker_path(klon));
        rewarm_untracked_cache(klon);
    }
    Ok(())
}

/// Rebuild the untracked cache of the klon after the last directory landed
/// (R11, G2).
///
/// `add` warms the cache before this process starts, so the cache records the
/// root directory as it was without `target/` or `node_modules/`. The landing
/// rename changes the root mtime and invalidates that one node, and git 2.34
/// never writes the repair back, so every later `git status` reopens the root.
/// One forced status here leaves a cache that matches the finished tree.
///
/// A failure costs the klon nothing but that reopen, so the result is dropped.
fn rewarm_untracked_cache(klon: &Path) {
    let _ = git::run_env(
        klon,
        &["status", "--porcelain"],
        &[("GIT_FORCE_UNTRACKED_CACHE", "1")],
    );
}

/// The exclusion set of the original clone, rebuilt from the register.
///
/// A warm directory can hold another registered worktree, and copying one into
/// the klon would give it a second checkout. `add` built the same set before
/// its clone; the warm process reads the register again instead of carrying
/// the list in the marker, so a worktree registered since `add` is skipped too
/// (R39).
fn clone_exclusions(golden: &Path, klon: &Path) -> Exclusions {
    let others = git::worktree_list(golden)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| crate::paths::absolute(&w.path).ok())
        .filter(|p| p != golden);
    Exclusions::new(golden, others.chain(std::iter::once(klon.to_path_buf())))
}

/// True while the klon is still a worktree. `rm` renames the whole directory
/// into `.trash`, so the `.git` file disappears with it.
fn alive(klon: &Path) -> bool {
    klon.join(".git").exists()
}

/// Copy one ignored directory of golden into the klon through a staging name
/// and one rename. `pause_test` spends the test-only pause of
/// `wait_for_test_gate` before this call's rename, once per warm run.
fn land(
    golden: &Path,
    klon: &Path,
    dir: &str,
    exclude: &Exclusions,
    config: &Config,
    no_fixup: bool,
    pause_test: &mut bool,
) -> Result<()> {
    let source = golden.join(dir);
    let landed = klon.join(dir);
    let staging = klon.join(format!("{dir}{STAGING_SUFFIX}"));
    if !source.is_dir() {
        // Golden lost the directory while the klon waited for it. There is
        // nothing to copy and nothing to report.
        return Ok(());
    }
    if landed.exists() {
        return Err(Error::klon(format!(
            "{} already exists; the warm copy stops here",
            landed.display()
        )));
    }
    // The plan reads golden's ignore rules, and the klon has another branch
    // checked out. A directory that this branch tracks, or simply does not
    // ignore, must not land: `add` already ran `git clean`, so the klon would
    // keep the copy as untracked files and read dirty for ever (R3).
    if !ignored_in(klon, dir)? {
        return Err(Error::klon(format!(
            "{dir} is not ignored on this branch; the klon keeps it out"
        )));
    }
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(Error::io(format!("clear {}", staging.display())))?;
    }
    copy::copy_tree(&source, &staging, exclude)?;
    // C11 rewrites golden's path before the rename, under the name the
    // directory will carry, so the klon never holds a build tree that points
    // back at golden (R15). `--no-fixup` belongs to the `add` that planned
    // this step, so the marker carries it here.
    if !no_fixup {
        fixup::run_dir(golden, klon, config, &staging, &landed)?;
    }
    if *pause_test {
        *pause_test = false;
        wait_for_test_gate();
    }
    if !alive(klon) {
        let _ = fs::remove_dir_all(&staging);
        return Ok(());
    }
    fs::rename(&staging, &landed).map_err(Error::io(format!("land {} in {}", dir, klon.display())))
}

/// Test-only pause point, in the style of `KLON_TEST_PAUSE_AT` in the journal.
/// `KLON_TEST_WARM_PAUSE=<path>` holds the warm process just before its first
/// rename until the file at `<path>` exists, so a test can observe the window
/// between the filled staging copy and the landing. klon never sets this
/// variable itself and no command reads it for any other purpose. The wait
/// polls every 50 ms and gives up after 60 s, so a test that never writes the
/// file cannot hang the suite.
fn wait_for_test_gate() {
    let Some(gate) = std::env::var_os("KLON_TEST_WARM_PAUSE") else {
        return;
    };
    let gate = PathBuf::from(gate);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// True when the klon's own branch ignores the directory `dir`.
///
/// The query carries a trailing slash. `check-ignore` reads a bare name as a
/// file name whatever sits on disk, and a directory-only pattern such as
/// `/build/` never matches a file name; the slash is what makes git answer the
/// question that was asked. The path itself need not exist, which is the
/// point: the warm copy has not landed yet.
///
/// The exit code is the answer: 0 for an ignored path, 1 for one that is not.
fn ignored_in(klon: &Path, dir: &str) -> Result<bool> {
    let query = format!("{dir}/");
    let (code, _) = git::run_input(klon, &["check-ignore", "-q", "--", &query], b"", &[0, 1])?;
    Ok(code == 0)
}

/// Run one approved reinstall command inside the klon. The command owns the
/// directory, so klon neither stages nor renames anything.
fn reinstall(klon: &Path, command: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(klon)
        .stdin(Stdio::null())
        .status()
        .map_err(Error::io(format!("run {command}")))?;
    if status.success() {
        return Ok(());
    }
    Err(Error::klon(format!(
        "{command} failed with {}",
        status.code().map_or_else(
            || "a signal".to_string(),
            |code| format!("exit code {code}")
        )
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_round_trips_through_json() {
        let steps = vec![
            Step {
                dir: "target".to_string(),
                action: Action::Copy,
            },
            Step {
                dir: "node_modules".to_string(),
                action: Action::Reinstall {
                    command: "pnpm install".to_string(),
                },
            },
        ];
        let text = serde_json::to_string(&steps).expect("serialize");
        assert!(text.contains(r#""action":"copy""#), "{text}");
        assert!(text.contains(r#""command":"pnpm install""#), "{text}");
        let back: Vec<Step> = serde_json::from_str(&text).expect("parse");
        assert_eq!(back, steps);
    }
}
