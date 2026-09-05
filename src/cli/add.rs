//! `gh klon add <branch> [--path <p>]`: the `add` transaction from handoff §4, copy backend only.

use crate::backend::copy;
use crate::{git, paths, Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(clap::Args)]
pub struct Args {
    /// An existing local branch.
    pub branch: String,
    /// The klon path. Default: `../<repo>.wt/<branch>` next to golden.
    #[arg(long)]
    pub path: Option<PathBuf>,
}

/// Directories inside golden where a klon may live.
const ALLOWED_INSIDE_GOLDEN: &[&str] = &[".claude/worktrees", ".t3"];

pub fn run(args: Args) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = worktrees
        .first()
        .map(|w| paths::absolute(&w.path))
        .ok_or_else(|| Error::klon("not inside a git repository"))?;
    let common = git::common_dir(&cwd)?;

    let path = match &args.path {
        Some(p) => paths::absolute(p),
        None => paths::default_klon_path(&golden, &args.branch),
    };
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
        git::run_quiet(&golden, &["worktree", "unlock", p]);
        git::run_quiet(&golden, &["worktree", "remove", "--force", p]);
    }
    result?;
    println!("{}", path.display());
    Ok(())
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
    text.trim()
        .strip_prefix("gitdir:")
        .map(|p| PathBuf::from(p.trim()))
        .ok_or_else(|| Error::klon(format!("unexpected .git file in {}", path.display())))
}

/// Step 3: append `/.klon/` to `<common>/info/exclude` once.
fn exclude_klon_dir(common: &Path) -> Result<()> {
    let file = common.join("info").join("exclude");
    let current = fs::read_to_string(&file).unwrap_or_default();
    if current.lines().any(|l| l.trim() == "/.klon/") {
        return Ok(());
    }
    fs::create_dir_all(common.join("info")).map_err(Error::io("create info/"))?;
    let newline = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(&file, format!("{current}{newline}/.klon/\n"))
        .map_err(Error::io("write info/exclude"))
}
