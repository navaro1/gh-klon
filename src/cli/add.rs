//! `gh klon add <branch> [--pr <n>] [--issue <n>] [--path <p>] [--path-mode <m>] [--backend <b>]`:
//! the `add` transaction from handoff §4. The probed backend fills the working
//! directory (C5).

use crate::backend::{self, Backend, Exclusions};
use crate::branch;
use crate::envelope::{env, slots};
use crate::journal::{self, State};
use crate::{config, fixup, git, paths, repair, spare, Error, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// The `KLON_DEBUG=1` timing lines: one per step of the transaction, so a
/// reader can see where the time goes.
struct Steps {
    on: bool,
    last: Instant,
}

impl Steps {
    fn new() -> Steps {
        Steps {
            on: crate::debug(),
            last: Instant::now(),
        }
    }

    /// Print the time since the previous mark under `step`, then restart.
    fn mark(&mut self, step: &str) {
        if self.on {
            eprintln!(
                "klon: debug: add {step} {:.1} ms",
                self.last.elapsed().as_secs_f64() * 1000.0
            );
        }
        self.last = Instant::now();
    }
}

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.add/1";

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
    /// `../<repo>.wt/<branch>` next to golden. The template supports `{repo}`, `{branch}`,
    /// and `{name}`.
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Use this clone backend instead of the probed one, for example
    /// `reflink-walk` or `copy`.
    #[arg(long)]
    pub backend: Option<String>,
    /// The path convention of a host harness (research record §19). It sets
    /// the path template and, for `claude`, renames the branch to
    /// `worktree-<name>`. `--path` is the explicit escape hatch, so the two
    /// conflict.
    #[arg(
        long,
        value_enum,
        requires = "branch",
        conflicts_with_all = ["path", "pr", "issue"]
    )]
    pub path_mode: Option<config::PathMode>,
    /// Skip the path fixup pass over the ignored directories (R15). The klon
    /// then keeps golden's absolute paths in its build artifacts.
    #[arg(long)]
    pub no_fixup: bool,
    /// Skip the hot spare for this call: clone directly and start no builder.
    #[arg(long)]
    pub no_spare: bool,
    /// A command to run in the new klon, after `--`. `add` runs it through
    /// `run` and exits with its exit code.
    #[arg(last = true, num_args = 1.., allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// The `add --json` document.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    path: &'a Path,
    branch: &'a str,
    head: String,
    /// The backend that filled the tree: the spare's own when the spare
    /// served this `add`, else the selected one.
    backend: &'a str,
    /// True when the hot spare served this `add` (C9).
    spare: bool,
    duration_ms: u64,
}

/// What `fill` may take from the hot spare (C9).
struct SparePolicy<'a> {
    /// False when a switch turned the spare off for this call.
    on: bool,
    /// The `--backend` override. A spare made by another backend is skipped.
    wanted: Option<&'a str>,
}

/// Directories inside golden where a klon may live.
const ALLOWED_INSIDE_GOLDEN: &[&str] = &[".claude/worktrees", ".t3"];

pub fn run(args: Args, json: bool) -> Result<()> {
    // The wrapped command owns stdout, so its output and the report would share
    // one stream and no reader could parse the result as one document. The
    // refusal comes before every repository change.
    if json && !args.command.is_empty() {
        return Err(Error::klon(
            "--json is not available for add with a command after --; the command owns stdout",
        ));
    }
    let started = Instant::now();
    let mut steps = Steps::new();
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    // A reboot takes a klon volume down and leaves golden's symlink dangling,
    // so the image goes back up before the first `git` call (C15, S1 §9.4). A
    // repository without a volume pays one failed stat per ancestor of the
    // working directory and starts no process.
    crate::volume::ensure_attached(&cwd)?;
    let common = git::common_dir(&cwd)?;
    check_git_path(&common)?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = worktrees
        .first()
        .map(|w| paths::absolute(&w.path))
        .ok_or_else(|| Error::klon("not inside a git repository"))??;
    // The branch form is resolved first: it names the default klon path and
    // may create the branch (handoff §4, git DWIM). The claude mode renames
    // the branch first: the argument is the worktree name (research §19).
    let branch = match args.path_mode {
        Some(config::PathMode::Claude) => {
            let raw = args.branch.as_deref().ok_or_else(|| {
                Error::klon(
                    "--path-mode claude names the worktree, as in: add x --path-mode claude",
                )
            })?;
            branch::resolve(&golden, &format!("worktree-{raw}"))?
        }
        _ => resolve_branch(&golden, &args)?,
    };
    // One load: the path template and the `[fixup] skip` globs come from the
    // same file, and a second load would repeat its warning lines.
    let config = config::load(&golden)?;
    let use_spare = spare::enabled(config.spare, args.no_spare);
    let path = match &args.path {
        Some(p) => paths::absolute(p)?,
        None => match args.path_mode {
            Some(mode) => config::resolve_filled(
                &golden,
                &mode.template(),
                &branch,
                &klon_name(&args, &branch),
            )?,
            None => config.resolve_path(&golden, &branch)?,
        },
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

    // The probe writes a fixture next to golden and can fail, so it runs before
    // the journal entry and before the first repository change (R5). The
    // destination decides whether a block-sharing backend can reach it.
    let choice = backend::select(&golden, &common, Some(&path), args.backend.as_deref())?;
    steps.mark("resolve-and-probe");

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
    steps.mark("register");

    let result = fill(
        &golden,
        &common,
        &worktrees,
        &path,
        &branch,
        choice.backend.as_ref(),
        &config,
        args.no_fixup,
        SparePolicy {
            on: use_spare,
            wanted: args.backend.as_deref(),
        },
        &mut record,
        &mut steps,
    );
    if result.is_err() && cleanup(&golden, &path) {
        // The rollback finished, so the entry has no work left either.
        record.close()?;
    }
    let spare_backend = result?;
    record.reach(State::Ready)?;
    record.close()?;

    // Step 8: the next spare. The builder is detached and low priority, and a
    // failure to start it costs one line, never the klon.
    if use_spare {
        spare::start_after(&golden, config.spare, args.no_spare);
        steps.mark("spare-start");
    }

    if json {
        let report = Report {
            schema: SCHEMA,
            path: &path,
            branch: &branch,
            head: git::run(&path, &["rev-parse", "HEAD"])?.trim().to_string(),
            backend: spare_backend.as_deref().unwrap_or(choice.backend.name()),
            spare: spare_backend.is_some(),
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
    // Step 12: `add <branch> -- <cmd>` is `add` and then `run`. The exit code
    // of the command becomes the exit code of `add`.
    if !args.command.is_empty() {
        return super::run::exec(&path, &args.command);
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
    // The loopback address goes back before the directory does. A crash here
    // still frees it, because the allocator drops a slot whose path is gone.
    if let Ok(common) = git::common_dir_of_main(golden) {
        if let Err(err) = slots::release(&common, path) {
            eprintln!("klon: cleanup: {err}");
        }
    }
    if let Err(err) = backend::make_removable(path) {
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

/// Step 1: refuse a non-empty path, a path that klon reserves for its spare
/// and its trash, and a path inside golden outside the allowed places.
fn check_path(golden: &Path, path: &Path) -> Result<()> {
    if path.is_file() || paths::is_non_empty_dir(path) {
        return Err(Error::klon(format!("path not empty: {}", path.display())));
    }
    if let Some(name) = spare::reserved(golden, path) {
        return Err(Error::klon(format!(
            "path {} is reserved: klon keeps its spare and its trash under {name}",
            path.display()
        )));
    }
    if let Ok(rel) = path.strip_prefix(golden) {
        let allowed = ALLOWED_INSIDE_GOLDEN.iter().any(|d| rel.starts_with(d))
            || Exclusions::new(golden, []).excludes(path, true);
        if !allowed {
            return Err(Error::klon(format!(
                "path {} is inside the repository; use .claude/worktrees, .t3, or a path that .klonignore excludes",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The `{name}` of the template: the raw `add` argument in the claude mode,
/// the branch everywhere else, where no separate name exists.
fn klon_name(args: &Args, branch: &str) -> String {
    match args.path_mode {
        Some(config::PathMode::Claude) => args.branch.clone().unwrap_or_else(|| branch.to_string()),
        _ => branch.to_string(),
    }
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

/// Steps 3 to 10. Runs after git registered the worktree. The answer names
/// the backend of the hot spare when the spare filled the working directory.
#[allow(clippy::too_many_arguments)]
fn fill(
    golden: &Path,
    common: &Path,
    worktrees: &[git::Worktree],
    path: &Path,
    branch: &str,
    backend: &dyn Backend,
    config: &config::Config,
    no_fixup: bool,
    policy: SparePolicy,
    record: &mut journal::Record,
    steps: &mut Steps,
) -> Result<Option<String>> {
    let admin_dir = read_admin_dir(path)?;
    exclude_klon_dir(common)?;

    // Step 4: the spare, when one is ready (C9); else clone golden minus .git,
    // the destination, and every registered worktree.
    let spare_backend = match policy.on {
        true => match spare::claim(golden, path, &admin_dir, policy.wanted)? {
            spare::Claim::Used(name) => Some(name),
            spare::Claim::Direct => None,
        },
        false => None,
    };
    let used_spare = spare_backend.is_some();
    if !used_spare {
        let others = worktrees
            .iter()
            .map(|w| paths::absolute(&w.path))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| p != golden);
        let exclude = Exclusions::new(golden, others.chain(std::iter::once(path.to_path_buf())));
        backend.clone(golden, path, &exclude)?;
    }
    steps.mark(if used_spare { "claim" } else { "clone" });

    // Step 5: the .git file points at the admin entry that git created.
    fs::write(
        path.join(".git"),
        format!("gitdir: {}\n", admin_dir.display()),
    )
    .map_err(Error::io("write .git"))?;
    record.reach(State::Cloned)?;

    // Step 6: an index with a fresh mtime. `--no-checkout` wrote no index. The
    // spare brings the index of the moment it was made, which describes its
    // files exactly; a direct clone takes golden's.
    let index = admin_dir.join("index");
    let from_spare = used_spare && spare::take_index(path, &admin_dir)?;
    if !from_spare {
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
    }
    fs::File::open(&index)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .map_err(Error::io("touch the index"))?;
    if used_spare {
        // The builder's record and the claim's stub must not reach the klon.
        spare::drop_metadata(path)?;
    }
    steps.mark("index");

    // Step 7: shared config that makes the first `git status` cheap.
    ensure_config(golden)?;
    steps.mark("config");

    // Steps 8 to 10.
    git::run(path, &["checkout", "-q", "--force", branch])?;
    steps.mark("checkout");
    git::run(path, &["clean", "-fdq"])?;
    steps.mark("clean");
    // Step 5b: the ignored directories still name golden. `git clean` ran
    // first, so every entry that the pass walks is an ignored one (handoff §4).
    // It runs before the envelope, so the pass never reads the new `.klon/`.
    if !no_fixup {
        fixup::run(golden, path, config)?;
        steps.mark("fixup");
    }

    // Step 10b: the envelope contract (handoff §5). `/.klon/` is already in
    // `info/exclude`, so the new directory keeps the klon clean for git. It is
    // written before the status below, so the untracked cache already knows it.
    let ip = slots::allocate(common, branch, path)?;
    env::write(path, branch, &ip)?;
    steps.mark("env");

    // The state comes after the envelope, not before it. `doctor --repair`
    // reads `checked-out` as "only the unlock is left" and finishes there, so a
    // klon that reached that state without an env file would stay half made:
    // `run`, `shell`, and `stop` all need the file. A crash before this line
    // leaves the state `cloned`, and the repair rolls the whole `add` back.
    record.reach(State::CheckedOut)?;

    // One status builds the untracked cache in the fresh index. Without it,
    // the first `rm` pays the build and misses its 100 ms budget (handoff §11).
    git::run(path, &["status", "--porcelain"])?;
    steps.mark("status-warm");
    git::run(
        golden,
        &["worktree", "unlock", path.to_str().unwrap_or_default()],
    )?;
    steps.mark("unlock");
    Ok(spare_backend)
}

/// The three keys that make the first `git status` cheap (handoff §4). One
/// read tells which of them golden lacks; a key that already holds its value
/// is not rewritten, so two concurrent `add` calls do not fight over git's
/// `config.lock`, and every `add` after the first spawns no `git config`.
fn ensure_config(golden: &Path) -> Result<()> {
    const WANTED: [(&str, &str); 3] = [
        ("core.checkStat", "minimal"),
        ("core.untrackedCache", "true"),
        ("index.version", "4"),
    ];
    let current = match git::run(
        golden,
        &[
            "config",
            "--get-regexp",
            "^(core\\.checkstat|core\\.untrackedcache|index\\.version)$",
        ],
    ) {
        Ok(text) => text,
        // Exit 1 means that no key matched.
        Err(Error::Git { code: 1, .. }) => String::new(),
        Err(err) => return Err(err),
    };
    for (key, value) in WANTED {
        let wanted = format!("{} {value}", key.to_ascii_lowercase());
        if !current.lines().any(|line| line == wanted) {
            git::run(golden, &["config", key, value])?;
        }
    }
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
