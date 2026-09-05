//! `gh klon add <branch> [--path <p>]`: the `add` transaction from handoff §4, copy backend only.

use crate::backend::copy;
use crate::{config, git, paths, Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(clap::Args)]
pub struct Args {
    /// An existing local branch.
    pub branch: String,
    /// The klon path. Default: the `path` template from `.klon.toml`, else
    /// `../<repo>.wt/<branch>` next to golden. The template supports `{repo}` and `{branch}`.
    #[arg(long)]
    pub path: Option<PathBuf>,
}

/// Directories inside golden where a klon may live.
const ALLOWED_INSIDE_GOLDEN: &[&str] = &[".claude/worktrees", ".t3"];

pub fn run(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let common = git::common_dir(&cwd)?;
    check_git_path(&common)?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = worktrees
        .first()
        .map(|w| paths::absolute(&w.path))
        .ok_or_else(|| Error::klon("not inside a git repository"))??;
    let path = match &args.path {
        Some(p) => paths::absolute(p)?,
        None => config::load(&golden)?.resolve_path(&golden, &args.branch)?,
    };
    // Refuse unsupported paths before any repository mutation.
    for p in [&golden, &common, &path] {
        check_git_path(p)?;
    }
    if path.starts_with(&common) {
        return Err(Error::klon(
            "the destination is inside the git common directory",
        ));
    }
    check_path(&golden, &path)?;
    check_branch(&golden, &worktrees, &args.branch)?;

    // Step 2: git owns the admin entry. The path is empty or absent.
    git::run(
        &golden,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            "--lock",
            path.to_str().unwrap_or_default(),
        ],
    )?;
    let result = fill(&golden, &common, &worktrees, &path, &args.branch);
    if result.is_err() {
        // Step 11: leave no half-registered worktree, then report the original error.
        let p = path.to_str().unwrap_or_default();
        if let Err(err) = copy::make_removable(&path) {
            eprintln!("klon: cleanup: {err}");
        }
        git::run_quiet(&golden, &["worktree", "unlock", p]);
        if let Err(err) = git::run(&golden, &["worktree", "remove", "--force", p]) {
            eprintln!("klon: cleanup: {err}");
        }
    }
    result?;
    println!("{}", path.display());
    Ok(())
}

/// Git 2.34 has no NUL-delimited worktree list; reject ambiguous path names.
fn check_git_path(path: &Path) -> Result<()> {
    match path.to_str() {
        Some(text) if !text.contains(['\n', '\r']) => Ok(()),
        _ => Err(Error::klon(
            "repository and destination paths must be valid UTF-8 without newlines",
        )),
    }
}

/// Step 1: refuse a non-empty path and a path inside golden outside the allowed places.
fn check_path(golden: &Path, path: &Path) -> Result<()> {
    if path.is_file() || paths::is_non_empty_dir(path) {
        return Err(Error::klon(format!("path not empty: {}", path.display())));
    }
    if let Ok(rel) = path.strip_prefix(golden) {
        let allowed = ALLOWED_INSIDE_GOLDEN.iter().any(|d| rel.starts_with(d))
            || copy::Exclusions::new(golden, []).excludes(path, true);
        if !allowed {
            return Err(Error::klon(format!(
                "path {} is inside the repository; use .claude/worktrees, .t3, or a path that .klonignore excludes",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The branch must exist locally and must not be checked out anywhere.
fn check_branch(golden: &Path, worktrees: &[git::Worktree], branch: &str) -> Result<()> {
    if !git::local_branch_exists(golden, branch) {
        return Err(Error::klon(format!("branch not found: {branch}")));
    }
    let full = format!("refs/heads/{branch}");
    if let Some(w) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(&full))
    {
        return Err(Error::klon(format!(
            "branch {branch} is already checked out at {}",
            w.path.display()
        )));
    }
    Ok(())
}

/// Steps 3 to 10. Runs after git registered the worktree.
fn fill(
    golden: &Path,
    common: &Path,
    worktrees: &[git::Worktree],
    path: &Path,
    branch: &str,
) -> Result<()> {
    let admin_dir = read_admin_dir(path)?;
    exclude_klon_dir(common)?;

    // Step 4: copy golden minus .git, the destination, and every registered worktree.
    let others = worktrees
        .iter()
        .map(|w| paths::absolute(&w.path))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|p| p != golden);
    let exclude = copy::Exclusions::new(golden, others.chain(std::iter::once(path.to_path_buf())));
    copy::clone_tree(golden, path, &exclude)?;

    // Step 5: the .git file points at the admin entry that git created.
    fs::write(
        path.join(".git"),
        format!("gitdir: {}\n", admin_dir.display()),
    )
    .map_err(Error::io("write .git"))?;

    // Step 6: golden's index with a fresh mtime. `--no-checkout` wrote no index.
    let index = admin_dir.join("index");
    fs::copy(common.join("index"), &index).map_err(Error::io("copy the index"))?;
    // A split index refers to a shared file beside the original index.
    let shared = git::run(
        golden,
        &["rev-parse", "--path-format=absolute", "--shared-index-path"],
    )?;
    let shared = shared.strip_suffix('\n').unwrap_or(&shared);
    if !shared.is_empty() {
        let shared = Path::new(shared);
        let name = shared
            .file_name()
            .ok_or_else(|| Error::klon("invalid shared index path"))?;
        fs::copy(shared, admin_dir.join(name)).map_err(Error::io("copy the shared index"))?;
    }
    fs::File::open(&index)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .map_err(Error::io("touch the index"))?;

    // Step 7: shared config that makes the first `git status` cheap.
    for (key, value) in [
        ("core.checkStat", "minimal"),
        ("core.untrackedCache", "true"),
        ("index.version", "4"),
    ] {
        git::run(golden, &["config", key, value])?;
    }

    // Steps 8 to 10.
    git::run(path, &["checkout", "-q", "--force", branch])?;
    git::run(path, &["clean", "-fdq"])?;
    git::run(
        golden,
        &["worktree", "unlock", path.to_str().unwrap_or_default()],
    )?;
    Ok(())
}

/// Read `<path>/.git` and return `<common>/worktrees/<name>`.
fn read_admin_dir(path: &Path) -> Result<PathBuf> {
    let text = fs::read_to_string(path.join(".git")).map_err(Error::io("read .git"))?;
    text.strip_suffix('\n')
        .unwrap_or(&text)
        .strip_prefix("gitdir: ")
        .map(PathBuf::from)
        .ok_or_else(|| Error::klon(format!("unexpected .git file in {}", path.display())))
}

/// Step 3: append `/.klon/` to `<common>/info/exclude` once.
fn exclude_klon_dir(common: &Path) -> Result<()> {
    let file = common.join("info").join("exclude");
    let mut current = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(Error::io("read info/exclude")(err)),
    };
    if current
        .split(|b| *b == b'\n')
        .any(|line| line == b"/.klon/" || line == b"/.klon/\r")
    {
        return Ok(());
    }
    fs::create_dir_all(common.join("info")).map_err(Error::io("create info/"))?;
    if !current.is_empty() && !current.ends_with(b"\n") {
        current.push(b'\n');
    }
    current.extend_from_slice(b"/.klon/\n");
    fs::write(&file, current).map_err(Error::io("write info/exclude"))
}
