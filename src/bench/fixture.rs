//! The benchmark fixture generator (spec §7 C8; handoff §8).
//!
//! `bench` needs its own generator inside the binary, because the test harness
//! in `tests/common` belongs to the test targets. The two generators build the
//! same shape: `tracked_files` files spread over `dirs` directories, one
//! ignored `build/` directory, and a `feature` branch with a small diff. One
//! seed gives one repository: the same file bytes and the same commit dates, so
//! two hosts measure the same work.
//!
//! Every `git` call here isolates the configuration. A user's global config
//! must not change what the benchmark measures.

use super::manifest::Profile;
use crate::{Error, Result};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The branch that every cell checks out.
pub const BRANCH: &str = "feature";

/// The ignored directory. The correctness check compares it between golden and
/// the new tree: it is the warm build state that a plain `git worktree add`
/// leaves behind.
pub const IGNORED_DIR: &str = "build";

/// A generated repository inside a scratch directory. `Drop` removes the whole
/// directory, so an interrupted run leaves no gigabytes behind.
pub struct Fixture {
    root: PathBuf,
    golden: PathBuf,
}

impl Fixture {
    /// Build the profile below `base`. The directory name holds the pid, so two
    /// runs on one host never collide.
    pub fn build(base: &Path, name: &str, seed: u64, profile: &Profile) -> Result<Fixture> {
        let root = scratch_dir(base, name)?;
        let fixture = Fixture {
            golden: root.join("golden"),
            root,
        };
        fixture.generate(seed, profile)?;
        Ok(fixture)
    }

    /// The main checkout, left on `main`.
    pub fn golden(&self) -> &Path {
        &self.golden
    }

    /// The directory that holds golden. Every klon and every baseline worktree
    /// of a run lands below it, so both stay on golden's filesystem.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn generate(&self, seed: u64, profile: &Profile) -> Result<()> {
        let golden = &self.golden;
        create_dir(golden)?;
        for d in 0..profile.dirs {
            create_dir(&golden.join(dir_name(d)))?;
        }
        write_all((0..profile.tracked_files).collect(), |i| {
            let path = golden.join(tracked_rel(profile.dirs, i));
            fs::write(&path, tracked_body(seed, i)).map_err(Error::io(format!(
                "write the fixture file {}",
                path.display()
            )))
        })?;
        let build = golden.join(IGNORED_DIR);
        create_dir(&build)?;
        let bytes = profile.ignored_file_bytes;
        write_all((0..profile.ignored_files).collect(), |i| {
            let path = build.join(format!("o{i}.bin"));
            fs::write(&path, ignored_body(seed, i, bytes)).map_err(Error::io(format!(
                "write the fixture file {}",
                path.display()
            )))
        })?;
        fs::write(golden.join(".gitignore"), format!("/{IGNORED_DIR}/\n"))
            .map_err(Error::io("write the fixture .gitignore"))?;

        git(golden, &["init", "-q", "-b", "main"])?;
        git(golden, &["add", "-A"])?;
        commit(golden, "base", commit_time(seed, 0))?;

        git(golden, &["checkout", "-qb", BRANCH])?;
        for i in 0..profile.changed_files {
            let path = golden.join(tracked_rel(
                profile.dirs,
                i * 7 % profile.tracked_files.max(1),
            ));
            fs::write(&path, changed_body(seed, i)).map_err(Error::io(format!(
                "edit the fixture file {}",
                path.display()
            )))?;
        }
        for i in 0..profile.added_files {
            let path = golden.join(format!("new-{i}.txt"));
            fs::write(&path, added_body(seed, i)).map_err(Error::io(format!(
                "add the fixture file {}",
                path.display()
            )))?;
        }
        git(golden, &["add", "-A"])?;
        commit(golden, "feature", commit_time(seed, 1))?;
        git(golden, &["checkout", "-q", "main"])?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir_all(&self.root) {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("klon: bench: cannot remove {}: {err}", self.root.display());
            }
        }
    }
}

/// Run `write` for every index, over every core. The fixture is thousands of
/// small files, so a serial loop would dominate the run.
fn write_all(indexes: Vec<usize>, write: impl Fn(usize) -> Result<()> + Send + Sync) -> Result<()> {
    indexes
        .into_par_iter()
        .map(write)
        .reduce(|| Ok(()), |a: Result<()>, b| a.and(b))
}

fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(Error::io(format!("create {}", path.display())))
}

/// `<base>/klon-bench-<name>-<pid>-<n>`, a directory that does not exist yet.
fn scratch_dir(base: &Path, name: &str) -> Result<PathBuf> {
    create_dir(base)?;
    let pid = std::process::id();
    for n in 0..64u32 {
        let candidate = base.join(format!("klon-bench-{name}-{pid}-{n}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(Error::io(format!("create {}", candidate.display()))(err)),
        }
    }
    Err(Error::klon(format!(
        "cannot create a bench directory in {}",
        base.display()
    )))
}

// --- git ---------------------------------------------------------------------

/// Run `git -C <cwd> <args>` with an isolated identity and configuration.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = isolated_git(cwd, args)
        .output()
        .map_err(Error::io("run git for the fixture"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A `git` command with the fixture identity and no user configuration. The
/// runner spawns the timed `git` samples with it too, so a measured call and a
/// setup call see one configuration.
pub fn isolated_git(cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    isolate(&mut command);
    command
}

/// Give `command` the fixture identity and no user configuration.
pub fn isolate(command: &mut Command) {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "klon")
        .env("GIT_AUTHOR_EMAIL", "klon@example.com")
        .env("GIT_COMMITTER_NAME", "klon")
        .env("GIT_COMMITTER_EMAIL", "klon@example.com");
}

/// Commit with a pinned date, so one seed gives one set of commit ids.
fn commit(golden: &Path, message: &str, when: u64) -> Result<()> {
    let date = format!("@{when} +0000");
    let mut command = isolated_git(golden, &["commit", "-qm", message]);
    command
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date);
    let output = command
        .output()
        .map_err(Error::io("run git commit for the fixture"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

// --- Deterministic content ---------------------------------------------------

const IGNORED_SALT: u64 = 1 << 62;
const EDIT_SALT: u64 = 2 << 62;
const ADDED_SALT: u64 = 3 << 62;

/// SplitMix64. A tiny generator whose stream only depends on its state.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Thirty-two hex digits derived from `seed` and `salt`. The same pair repeats.
fn payload(seed: u64, salt: u64) -> String {
    let mut state = seed ^ salt.rotate_left(32);
    let a = splitmix64(&mut state);
    let b = splitmix64(&mut state);
    format!("{a:016x}{b:016x}")
}

/// The directory name of directory `d`.
fn dir_name(d: usize) -> String {
    format!("d{d:03}")
}

/// The path of tracked file `i`, relative to golden.
fn tracked_rel(dirs: usize, i: usize) -> String {
    format!("{}/f{i}.txt", dir_name(i % dirs.max(1)))
}

fn tracked_body(seed: u64, i: usize) -> String {
    format!("tracked file {i} {}\n", payload(seed, i as u64))
}

fn changed_body(seed: u64, i: usize) -> String {
    format!("feature edit {i} {}\n", payload(seed, EDIT_SALT + i as u64))
}

fn added_body(seed: u64, i: usize) -> String {
    format!(
        "added on feature {i} {}\n",
        payload(seed, ADDED_SALT + i as u64)
    )
}

/// `bytes` deterministic bytes for ignored file `i`. Build state is not text,
/// so the bytes are a pseudo-random stream: a compressing filesystem cannot
/// make the copy look cheaper than a real build tree.
fn ignored_body(seed: u64, i: usize, bytes: usize) -> Vec<u8> {
    let mut state = seed ^ (IGNORED_SALT + i as u64).rotate_left(32);
    let mut out = Vec::with_capacity(bytes);
    while out.len() < bytes {
        out.extend_from_slice(&splitmix64(&mut state).to_le_bytes());
    }
    out.truncate(bytes);
    out
}

/// A fixed commit time from the seed, in seconds. Tick 0 is the base commit and
/// tick 1 the feature commit.
fn commit_time(seed: u64, tick: u64) -> u64 {
    1_700_000_000 + (seed % 1_000) * 2 + tick
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Profile {
        Profile {
            tracked_files: 40,
            dirs: 4,
            ignored_files: 3,
            ignored_file_bytes: 512,
            changed_files: 2,
            added_files: 2,
        }
    }

    /// One seed gives one repository: the same bytes and the same commit ids.
    #[test]
    fn one_seed_gives_one_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = small();
        let a = Fixture::build(tmp.path(), "a", 7, &profile).unwrap();
        let b = Fixture::build(tmp.path(), "b", 7, &profile).unwrap();
        let c = Fixture::build(tmp.path(), "c", 8, &profile).unwrap();
        let head = |f: &Fixture| git(f.golden(), &["rev-parse", "feature"]).unwrap();
        assert_eq!(head(&a), head(&b), "one seed must give one commit id");
        assert_ne!(head(&a), head(&c), "another seed must give another id");
        assert_eq!(
            fs::read(a.golden().join("build/o1.bin")).unwrap(),
            fs::read(b.golden().join("build/o1.bin")).unwrap()
        );
    }

    #[test]
    fn the_shape_matches_the_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = small();
        let fx = Fixture::build(tmp.path(), "shape", 7, &profile).unwrap();
        let tracked = git(fx.golden(), &["ls-files"]).unwrap();
        // The tracked files plus .gitignore. `build/` is ignored.
        assert_eq!(tracked.lines().count(), profile.tracked_files + 1);
        assert!(!tracked.contains("build/"), "build/ must stay ignored");
        let ignored = fs::read_dir(fx.golden().join(IGNORED_DIR)).unwrap().count();
        assert_eq!(ignored, profile.ignored_files);
        assert_eq!(
            fs::metadata(fx.golden().join("build/o0.bin"))
                .unwrap()
                .len(),
            profile.ignored_file_bytes as u64
        );
        let status = git(fx.golden(), &["status", "--porcelain"]).unwrap();
        assert_eq!(status, "", "the fixture must be clean");
    }

    /// The feature branch changes the promised number of files and adds two.
    #[test]
    fn the_feature_branch_holds_the_promised_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = small();
        let fx = Fixture::build(tmp.path(), "diff", 7, &profile).unwrap();
        let names = git(fx.golden(), &["diff", "--name-only", "main", "feature"]).unwrap();
        assert_eq!(
            names.lines().count(),
            profile.changed_files + profile.added_files
        );
    }

    /// The scratch directory disappears with the fixture.
    #[test]
    fn drop_removes_the_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let root = {
            let fx = Fixture::build(tmp.path(), "drop", 7, &small()).unwrap();
            fx.root().to_path_buf()
        };
        assert!(!root.exists(), "{} must be gone", root.display());
    }
}
