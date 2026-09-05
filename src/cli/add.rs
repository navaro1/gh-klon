//! `gh klon add <branch> [--pr <n>] [--issue <n>] [--path <p>]`: the `add`
//! transaction from handoff §4, copy backend only.

use crate::backend::copy;
use crate::branch;
use crate::journal::{self, State};
use crate::{config, git, paths, repair, Error, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.add/1";

/// The only backend in v0. C5 replaces this with the probed backend name.
const BACKEND: &str = "copy";

#[derive(clap::Args)]
pub struct Args {
    /// A local branch, an `origin/<name>` remote branch, or the name of a
    /// new branch that klon creates from `base`.
    pub branch: Option<String>,
    /// Check out the head of pull request `<n>` as the branch `pr/<n>`.
    #[arg(long, conflicts_with_all = ["branch", "issue"])]
    pub pr: Option<u64>,
    /// Create the branch `<n>-<slug>` from the title of issue `<n>`.
    #[arg(long, conflicts_with = "branch")]
    pub issue: Option<u64>,
    /// The klon path. Default: the `path` template from `.klon.toml`, else
    /// `../<repo>.wt/<branch>` next to golden. The template supports `{repo}` and `{branch}`.
    #[arg(long)]
    pub path: Option<PathBuf>,
}

/// The `add --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    branch: &'a str,
    head: String,
    backend: &'static str,
    duration_ms: u64,
}

/// Directories inside golden where a klon may live.
const ALLOWED_INSIDE_GOLDEN: &[&str] = &[".claude/worktrees", ".t3"];

pub fn run(args: Args, json: bool) -> Result<()> {
    let started = Instant::now();
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    let common = git::common_dir(&cwd)?;
    check_git_path(&common)?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = worktrees
        .first()
        .map(|w| paths::absolute(&w.path))
        .ok_or_else(|| Error::klon("not inside a git repository"))??;
    // The branch form is resolved first: it names the default klon path and
    // may create the branch (handoff §4, git DWIM).
    let branch = resolve_branch(&golden, &args)?;
    let path = match &args.path {
        Some(p) => paths::absolute(p)?,
        None => config::load(&golden)?.resolve_path(&golden, &branch)?,
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
    // R6: a repeated `add` finishes the recovery of an interrupted one. This
    // runs before `check_path`, because an interrupted `add` leaves a `.git`
    // file in the destination and the check would refuse with `path not empty`.
    let worktrees = match recover_stale(&golden, &common, &path)? {
        // The recovery changed the register list, so read it again.
        true => git::worktree_list(&cwd)?,
        false => worktrees,
    };
    check_path(&golden, &path)?;
    // The refusal waits for the recovery above: an interrupted `add` may have
    // left the branch registered at the destination of this very run.
    refuse_checked_out(&worktrees, &branch)?;

    // Step 0: the journal entry precedes the first repository change.
    let mut record = journal::Record::start(&common, journal::Op::Add, &path, Some(&branch))?;

    // Step 2: git owns the admin entry. The path is empty or absent.
    if let Err(err) = git::run(
        &golden,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            "--lock",
            path.to_str().unwrap_or_default(),
        ],
    ) {
        // git registered nothing, so the entry has no work for `doctor`.
        if !git::is_registered(&golden, &path) {
            record.close()?;
        }
        return Err(err);
    }
    record.reach(State::Registered)?;

    let result = fill(&golden, &common, &worktrees, &path, &branch, &mut record);
    if result.is_err() && cleanup(&golden, &path) {
        // The rollback finished, so the entry has no work left either.
        record.close()?;
    }
    result?;
    record.reach(State::Ready)?;
    record.close()?;

    if json {
        let report = Report {
            schema: SCHEMA,
            path: &path,
            branch: &branch,
            head: git::run(&path, &["rev-parse", "HEAD"])?.trim().to_string(),
            backend: BACKEND,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
        );
    } else {
        println!("{}", path.display());
    }
    Ok(())
}

/// Close an open journal entry for this destination (R6, handoff §7). The
/// answer says whether the recovery changed anything. `add` reads only the
/// entry of its own path, so a future entry of another klon cannot block it.
fn recover_stale(golden: &Path, common: &Path, path: &Path) -> Result<bool> {
    let Some(entry) = journal::read(common, &journal::name_for(path))? else {
        return Ok(false);
    };
    let outcome = repair::entry(golden, common, &entry)?;
    for action in &outcome.actions {
        eprintln!("klon: recovery: {action}");
    }
    match outcome.failure {
        Some(err) => Err(err),
        None => Ok(true),
    }
}

/// Step 11: leave no half-registered worktree. True when the rollback finished
/// and the repository is back to the state before `add`.
fn cleanup(golden: &Path, path: &Path) -> bool {
    let text = path.to_str().unwrap_or_default();
    if let Err(err) = copy::make_removable(path) {
        eprintln!("klon: cleanup: {err}");
    }
    git::run_quiet(golden, &["worktree", "unlock", text]);
    match git::run(golden, &["worktree", "remove", "--force", text]) {
        Ok(_) => true,
        Err(err) => {
            eprintln!("klon: cleanup: {err}");
            eprintln!("klon: run gh klon doctor --repair to finish the cleanup");
            false
        }
    }
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

/// Resolve the branch form to a local branch name (handoff §4). The
/// resolution may create the branch: a tracking branch for `origin/<name>`,
/// `pr/<n>` for `--pr`, or a new branch from `base` for `--issue` and unknown
/// names.
fn resolve_branch(golden: &Path, args: &Args) -> Result<String> {
    if let Some(n) = args.pr {
        branch::resolve_pr(golden, n)
    } else if let Some(n) = args.issue {
        branch::resolve_issue(golden, n)
    } else {
        let name = args
            .branch
            .as_deref()
            .ok_or_else(|| Error::klon("name a branch, or use --pr or --issue"))?;
        branch::resolve(golden, name)
    }
}

/// Refuse a branch that another worktree has checked out.
fn refuse_checked_out(worktrees: &[git::Worktree], branch: &str) -> Result<()> {
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
    record: &mut journal::Record,
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
    record.reach(State::Cloned)?;

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
    record.reach(State::CheckedOut)?;
    // One status builds the untracked cache in the fresh index. Without it,
    // the first `rm` pays the build and misses its 100 ms budget (handoff §11).
    git::run(path, &["status", "--porcelain"])?;
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
