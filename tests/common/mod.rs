//! Shared test harness (spec §7 C1): a deterministic fixture generator, a manifest
//! walker, and a plain `git worktree add` oracle. Every test file starts with
//! `mod common;` and reuses these helpers instead of private copies.

use std::collections::BTreeSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

pub const BIN: &str = env!("CARGO_BIN_EXE_gh-klon");

/// Run `git -C <cwd> <args>` with an isolated identity and config.
pub fn git(cwd: &Path, args: &[&str]) -> Output {
    git_env(cwd, args, &[])
}

fn git_env(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "klon")
        .env("GIT_AUTHOR_EMAIL", "klon@example.com")
        .env("GIT_COMMITTER_NAME", "klon")
        .env("GIT_COMMITTER_EMAIL", "klon@example.com");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("run git")
}

pub fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn klon(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run gh-klon")
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// --- Deterministic content ---------------------------------------------------

const IGNORED_SALT: u64 = 1 << 62;
const EDIT_SALT: u64 = 2 << 62;
const ADDED_SALT: u64 = 3 << 62;

/// SplitMix64. A tiny generator whose stream only depends on its state.
fn splitmix64(state: &mut u64) -> u64 {
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

/// The path of tracked file `i`, relative to the root.
fn tracked_rel(dirs: usize, i: usize) -> String {
    format!("d{:03}/f{}.txt", i % dirs, i)
}

/// The bytes that `main` gives tracked file `i`.
fn tracked_body(seed: u64, i: usize) -> String {
    format!("tracked file {i} {}\n", payload(seed, i as u64))
}

/// The bytes of ignored file `i` in `build/`, written three times for bulk.
fn ignored_body(seed: u64, i: usize) -> String {
    format!("object {i} {}\n", payload(seed, IGNORED_SALT + i as u64)).repeat(3)
}

/// A fixed commit time derived from the seed, in seconds. Tick 0 is the base
/// commit and tick 1 the feature commit. Pinned dates make the same seed give
/// the same commit shas, so two same-seed repositories are comparable.
fn commit_time(seed: u64, tick: u64) -> u64 {
    1_700_000_000 + (seed % 1_000) * 2 + tick
}

fn commit_ok(golden: &Path, message: &str, when: u64) {
    let date = format!("@{when} +0000");
    let out = git_env(
        golden,
        &["commit", "-qm", message],
        &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
    );
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// --- Fixture -----------------------------------------------------------------

/// A generated repository: `main`, a `feature` branch, and an ignored `build/`
/// directory. The same seed and counts give the same file bytes and the same
/// commit shas, so two fixtures compare equal without the wall clock.
pub struct Fixture {
    _tmp: tempfile::TempDir,
    /// The main checkout, left on `main`.
    pub golden: PathBuf,
    pub seed: u64,
    dirs: usize,
    /// Paths that `feature` changes or adds, relative to the root.
    pub diff_paths: BTreeSet<String>,
}

impl Fixture {
    /// `tracked_files` files spread over `dirs` directories, `ignored_files`
    /// files in `build/`, and `diff_paths` paths that `feature` changes or adds.
    pub fn generate(
        seed: u64,
        tracked_files: usize,
        dirs: usize,
        ignored_files: usize,
        diff_paths: usize,
    ) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let golden = tmp.path().join("golden");
        fs::create_dir(&golden).unwrap();
        for i in 0..tracked_files {
            let dir = golden.join(format!("d{:03}", i % dirs));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("f{i}.txt")), tracked_body(seed, i)).unwrap();
        }
        fs::write(golden.join("f2.txt"), "root file 2\n").unwrap();
        let build = golden.join("build");
        fs::create_dir(&build).unwrap();
        for i in 0..ignored_files {
            fs::write(build.join(format!("o{i}.bin")), ignored_body(seed, i)).unwrap();
        }
        fs::write(golden.join(".gitignore"), "/build/\n").unwrap();
        git_ok(&golden, &["init", "-q", "-b", "main"]);
        git_ok(&golden, &["add", "-A"]);
        commit_ok(&golden, "base", commit_time(seed, 0));

        git_ok(&golden, &["checkout", "-qb", "feature"]);
        let mut diff_set = BTreeSet::new();
        for i in 0..diff_paths {
            let rel = tracked_rel(dirs, i * 7);
            fs::write(
                golden.join(&rel),
                format!("feature edit {i} {}\n", payload(seed, EDIT_SALT + i as u64)),
            )
            .unwrap();
            diff_set.insert(rel);
        }
        fs::write(golden.join("f2.txt"), "root file 2 on feature\n").unwrap();
        diff_set.insert("f2.txt".into());
        for (k, name) in ["new-a.txt", "d000/new-b.txt"].into_iter().enumerate() {
            fs::write(
                golden.join(name),
                format!(
                    "added on feature {}\n",
                    payload(seed, ADDED_SALT + k as u64)
                ),
            )
            .unwrap();
            diff_set.insert(name.into());
        }
        git_ok(&golden, &["add", "-A"]);
        commit_ok(&golden, "feature", commit_time(seed, 1));
        git_ok(&golden, &["checkout", "-q", "main"]);
        // Let the coarse filesystem clock move past every fixture mtime.
        std::thread::sleep(Duration::from_millis(20));
        Fixture {
            _tmp: tmp,
            golden,
            seed,
            dirs,
            diff_paths: diff_set,
        }
    }

    pub fn default_klon_path(&self) -> PathBuf {
        self.golden
            .parent()
            .unwrap()
            .join("golden.wt")
            .join("feature")
    }

    /// A worktree made by plain `git worktree add` at `<tmp>/oracle/<branch>`.
    /// The oracle shows what unmodified git produces for the same branch.
    pub fn oracle_worktree_add(&self, branch: &str) -> PathBuf {
        let path = self.golden.parent().unwrap().join("oracle").join(branch);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        git_ok(
            &self.golden,
            &["worktree", "add", path.to_str().unwrap(), branch],
        );
        path
    }

    /// The path of tracked file `i`, relative to the root.
    pub fn tracked_rel(&self, i: usize) -> String {
        tracked_rel(self.dirs, i)
    }

    /// The bytes that `main` gives tracked file `i`.
    pub fn tracked_content(&self, i: usize) -> String {
        tracked_body(self.seed, i)
    }
}

// --- Manifest ----------------------------------------------------------------

/// (path, type, size, mode, mtime, symlink target, content hash), sorted by path.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: &'static str,
    pub size: u64,
    pub mode: u32,
    pub mtime: SystemTime,
    pub target: Option<PathBuf>,
    pub hash: u64,
}

/// `Entry` without `mtime`, for comparisons that must ignore the wall clock.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryNoTimes {
    pub path: PathBuf,
    pub kind: &'static str,
    pub size: u64,
    pub mode: u32,
    pub target: Option<PathBuf>,
    pub hash: u64,
}

impl Entry {
    fn without_times(self) -> EntryNoTimes {
        EntryNoTimes {
            path: self.path,
            kind: self.kind,
            size: self.size,
            mode: self.mode,
            target: self.target,
            hash: self.hash,
        }
    }
}

/// Every entry below `root`, with paths relative to `root`. Strips nothing.
pub fn manifest(root: &Path) -> Vec<Entry> {
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

/// `manifest` with the timestamps removed.
pub fn manifest_without_times(root: &Path) -> Vec<EntryNoTimes> {
    manifest(root)
        .into_iter()
        .map(Entry::without_times)
        .collect()
}

/// Give every entry below `root`, symlinks included, one fixed mtime. Tests use
/// it to compare two trees without wall-clock noise.
pub fn freeze_times(root: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let spec = libc::timespec {
        tv_sec: 1_700_000_000,
        tv_nsec: 0,
    };
    let times = [spec, spec];
    fn set(path: &Path, times: &[libc::timespec; 2]) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is NUL-terminated and `times` holds two timespec values.
        let rc = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c_path.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(rc, 0, "set the mtime of {}", path.display());
    }
    fn walk(dir: &Path, times: &[libc::timespec; 2]) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            set(&path, times);
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(&path, times);
            }
        }
    }
    set(root, &times);
    walk(root, &times);
}

// --- Oracle and parity -------------------------------------------------------

/// Assert that the klon is clean: `git status --porcelain` prints nothing.
pub fn assert_clean(klon_path: &Path) {
    let status = git_ok(klon_path, &["status", "--porcelain"]);
    assert_eq!(status, "", "the klon must be clean");
}

struct WorktreeState {
    kinds: BTreeSet<String>,
    branch: String,
    head: String,
}

fn worktree_state(dir: &Path) -> WorktreeState {
    let target = fs::canonicalize(dir).unwrap();
    let list = git_ok(dir, &["worktree", "list", "--porcelain"]);
    for block in list.split("\n\n") {
        let mut kinds = BTreeSet::new();
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
                kinds.insert("worktree".to_string());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = Some(h.to_string());
                kinds.insert("HEAD".to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.to_string());
                kinds.insert("branch".to_string());
            } else if !line.is_empty() {
                let kind = line.split(' ').next().unwrap_or(line).to_string();
                kinds.insert(kind);
            }
        }
        let found = path.is_some_and(|p| fs::canonicalize(&p).is_ok_and(|c| c == target));
        if found {
            return WorktreeState {
                kinds,
                branch: branch.expect("a worktree with a branch"),
                head: head.expect("a worktree with a HEAD"),
            };
        }
    }
    panic!(
        "{} is missing from git worktree list --porcelain",
        dir.display()
    );
}

/// Assert that two worktrees agree: the same `git worktree list --porcelain`
/// shape, the same HEAD and branch, the same tracked tree hash, and two clean
/// trees. The trees may live in two repositories built from the same seed.
/// A one-byte difference between a worktree and its branch tree fails here.
pub fn assert_worktree_parity(klon: &Path, oracle: &Path) {
    let left = worktree_state(klon);
    let right = worktree_state(oracle);
    assert_eq!(
        left.kinds, right.kinds,
        "porcelain shape differs: {:?} vs {:?}",
        left.kinds, right.kinds
    );
    assert_eq!(
        left.branch, right.branch,
        "branch differs: {} vs {}",
        left.branch, right.branch
    );
    assert_eq!(
        left.head, right.head,
        "HEAD differs: {} vs {}",
        left.head, right.head
    );
    assert_eq!(
        git_ok(klon, &["rev-parse", "HEAD^{tree}"]),
        git_ok(oracle, &["rev-parse", "HEAD^{tree}"]),
        "tracked tree hash differs"
    );
    for dir in [klon, oracle] {
        let status = git_ok(dir, &["status", "--porcelain"]);
        assert_eq!(status, "", "worktree {} must be clean", dir.display());
        assert!(
            git(dir, &["diff", "--quiet", "HEAD"]).status.success(),
            "worktree {} differs from HEAD",
            dir.display()
        );
    }
}
