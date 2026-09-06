//! Subprocess wrapper around the installed `git`. klon never reimplements plumbing.

use crate::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Run `git -C <cwd> <args>` and return its stdout. A non-zero exit becomes `Error::Git`.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(Error::io("run git"))?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| Error::klon("git output must be valid UTF-8"))
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Run `git -C <cwd> <args>` with `input` on stdin and return the exit code and the
/// raw stdout. An exit code outside `ok` becomes `Error::Git`.
///
/// The radar needs all three parts: raw bytes because `merge-tree` prints file
/// content that is not always UTF-8, `-z` output that is NUL separated, and exit
/// code 1 because that is how `merge-tree --write-tree` reports a conflict.
pub fn run_input(cwd: &Path, args: &[&str], input: &[u8], ok: &[i32]) -> Result<(i32, Vec<u8>)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::io("run git"))?;
    // Write stdin from a second thread. git can fill the stdout pipe while klon
    // still writes, and a single-threaded write-then-read would deadlock there.
    let mut sink = child.stdin.take().expect("stdin is piped");
    let payload = input.to_vec();
    let writer = std::thread::spawn(move || sink.write_all(&payload));
    let output = child
        .wait_with_output()
        .map_err(Error::io("read the git output"))?;
    let _ = writer.join();
    let code = output.status.code().unwrap_or(1);
    if ok.contains(&code) {
        Ok((code, output.stdout))
    } else {
        Err(Error::Git {
            code,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Run `git` and ignore the result. Used on the cleanup path only.
pub fn run_quiet(cwd: &Path, args: &[&str]) {
    let _ = run(cwd, args);
}

/// Set `key` to `value` in the shared repository config, unless it already
/// holds that value. `merge` (C25) writes its four keys this way.
///
/// The read comes first for two reasons. A key that already agrees needs no
/// `git config` process, and a repeated command then takes no `config.lock`,
/// so two klon commands at once never fight over it.
pub fn ensure_config(cwd: &Path, key: &str, value: &str) -> Result<()> {
    match run(cwd, &["config", "--get", key]) {
        Ok(current) if current.trim_end_matches('\n') == value => return Ok(()),
        // Exit 1 means that the key is unset. Every other failure is real.
        Ok(_) | Err(Error::Git { code: 1, .. }) => {}
        Err(err) => return Err(err),
    }
    run(cwd, &["config", key, value]).map(|_| ())
}

/// The `(major, minor)` version of the installed git, or `(0, 0)` when klon
/// cannot read the line. One `git --version` runs per process.
pub fn version(cwd: &Path) -> (u32, u32) {
    static VERSION: OnceLock<(u32, u32)> = OnceLock::new();
    *VERSION.get_or_init(|| match run(cwd, &["--version"]) {
        Ok(text) => parse_version(&text),
        Err(_) => (0, 0),
    })
}

/// Read `git version 2.34.1` as `(2, 34)`. A line klon cannot read gives
/// `(0, 0)`, which every feature test then treats as "too old".
pub fn parse_version(text: &str) -> (u32, u32) {
    let Some(number) = text.split_whitespace().nth(2) else {
        return (0, 0);
    };
    let mut parts = number.split('.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

/// One block of `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    /// `refs/heads/<name>` when the worktree has a branch checked out.
    pub branch: Option<String>,
    /// The full object id of HEAD. Absent while the worktree has no commit.
    pub head: Option<String>,
    /// True when the entry is locked (`locked` or `locked <reason>`).
    pub locked: bool,
}

/// Parse `git worktree list --porcelain`. The first entry is the main worktree.
pub fn worktree_list(cwd: &Path) -> Result<Vec<Worktree>> {
    let text = run(cwd, &["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    for block in text.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        let mut head = None;
        let mut locked = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.to_string());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = Some(h.to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                locked = true;
            }
        }
        if let Some(path) = path {
            list.push(Worktree {
                path,
                branch,
                head,
                locked,
            });
        }
    }
    Ok(list)
}

/// The absolute path of the main worktree: the first `git worktree list` entry.
pub fn main_worktree(cwd: &Path) -> Result<PathBuf> {
    let path = worktree_list(cwd)?
        .first()
        .map(|w| w.path.clone())
        .ok_or_else(|| Error::klon("not inside a git repository"))?;
    crate::paths::absolute(&path)
}

/// The absolute common directory: the output of `git rev-parse --git-common-dir`.
pub fn common_dir(cwd: &Path) -> Result<PathBuf> {
    let out = run(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(out.strip_suffix('\n').unwrap_or(&out)))
}

/// The common directory of the repository whose main worktree is `golden`,
/// without a subprocess where the layout allows one. `rm` must return inside
/// 100 ms (R8), and one `git` process costs 10 to 50 ms while other builds run.
///
/// The main worktree holds either a `.git` directory, which is the common
/// directory itself, or a `.git` file that names it. The file form comes from
/// `git init --separate-git-dir` and from a submodule. Every other shape falls
/// back to `git rev-parse`, so the answer is never a guess.
pub fn common_dir_of_main(golden: &Path) -> Result<PathBuf> {
    let dot_git = golden.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(meta) if meta.is_dir() => return Ok(dot_git),
        Ok(meta) if meta.is_file() => {
            if let Some(dir) = read_gitdir_file(&dot_git, golden) {
                return Ok(dir);
            }
        }
        _ => {}
    }
    common_dir(golden)
}

/// The value of a boolean key of the shared config, or None when the key is
/// unset. A value in the global or system config does not count here: git
/// honours an extension such as `extensions.worktreeConfig` only from the
/// shared config, so a caller that writes or reports on that feature must
/// read the key from there too.
pub fn config_bool(cwd: &Path, key: &str) -> Result<Option<bool>> {
    match run(cwd, &["config", "--local", "--bool", "--get", key]) {
        Ok(text) => Ok(Some(text.trim() == "true")),
        // Exit 1 means the key is unset.
        Err(Error::Git { code: 1, .. }) => Ok(None),
        Err(err) => Err(err),
    }
}

/// The path in a `gitdir: <path>` file, made absolute against `base`.
fn read_gitdir_file(file: &Path, base: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let target = text
        .trim_end_matches(['\n', '\r'])
        .strip_prefix("gitdir: ")?;
    if target.is_empty() {
        return None;
    }
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

/// True when git still lists `path` as a worktree of the repository at `cwd`.
/// Both paths are made absolute first, so a symlinked parent still matches.
pub fn is_registered(cwd: &Path, path: &Path) -> bool {
    let Ok(list) = worktree_list(cwd) else {
        return false;
    };
    list.iter()
        .any(|w| w.path == path || crate::paths::absolute(&w.path).is_ok_and(|p| p == path))
}

/// True when `refs/heads/<branch>` exists.
pub fn local_branch_exists(cwd: &Path, branch: &str) -> bool {
    let rev = format!("refs/heads/{branch}");
    run(cwd, &["show-ref", "--verify", "--quiet", &rev]).is_ok()
}
