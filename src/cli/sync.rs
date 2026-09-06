//! `gh klon sync <branch> [--merge | --onto <base>] [--fresh] [--all] [--check]
//! [--force]` (spec §7 C14, R30): bring one klon, or every klon, up to date.
//!
//! The steps:
//! 1. One `git fetch origin` for the whole common directory, also with `--all`.
//! 2. Fast-forward when the klon has no local commits of its own.
//! 3. Else `git rebase --autostash <upstream>`, or `git merge <upstream>` with
//!    `--merge`.
//! 4. Refuse a force-pushed upstream that the klon has unique commits against.
//! 5. `--fresh` removes the klon and makes it again from golden. `--check` is
//!    the C24 radar row and changes nothing.
//!
//! A branch with no upstream syncs onto `base` instead.
//!
//! `--json` prints one `klon.sync/1` document per klon, one per line. `--all`
//! over three klons gives three lines, so a reader parses each line on its own.

use crate::radar;
use crate::{branch, git, paths, process, spare, Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The JSON schema name. A field removal or a type change bumps the suffix.
pub const SCHEMA: &str = "klon.sync/1";

#[derive(clap::Args)]
pub struct Args {
    /// The branch of the klon to sync. `--all` replaces it.
    pub branch: Option<String>,
    /// Merge the upstream instead of rebasing onto it.
    #[arg(long, conflicts_with_all = ["onto", "fresh", "check"])]
    pub merge: bool,
    /// Sync onto this branch instead of the klon's upstream. klon takes
    /// `origin/<base>` when the remote-tracking branch exists, else `<base>`.
    #[arg(long, conflicts_with_all = ["fresh", "check"])]
    pub onto: Option<String>,
    /// Remove the klon and make it again from golden, on the same branch and
    /// at the same path. It refuses a dirty klon.
    #[arg(long, conflicts_with = "check")]
    pub fresh: bool,
    /// Sync every klon. One line per klon; a failure does not stop the rest.
    #[arg(long, conflicts_with = "branch")]
    pub all: bool,
    /// Print the radar row of the klon and change nothing.
    #[arg(long)]
    pub check: bool,
    /// Sync a klon whose upstream was force-pushed, even where the klon holds
    /// commits that the new upstream lacks.
    #[arg(long, conflicts_with = "check")]
    pub force: bool,
}

/// What klon did to one klon.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    FastForward,
    Rebase,
    Merge,
    Fresh,
    Check,
    UpToDate,
    Refused,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Action::FastForward => "fast-forward",
            Action::Rebase => "rebase",
            Action::Merge => "merge",
            Action::Fresh => "fresh",
            Action::Check => "check",
            Action::UpToDate => "up-to-date",
            Action::Refused => "refused",
        }
    }
}

/// One klon's result. It is also the `--json` document.
#[derive(Serialize)]
struct Outcome {
    schema: &'static str,
    branch: String,
    path: PathBuf,
    action: &'static str,
    head_before: Option<String>,
    head_after: Option<String>,
    /// A short human sentence: what klon did, or why it refused.
    message: String,
}

impl Outcome {
    fn print(&self, json: bool) -> Result<()> {
        if json {
            println!(
                "{}",
                serde_json::to_string(self)
                    .map_err(|err| Error::klon(format!("serialize the report: {err}")))?
            );
        } else {
            println!("{} {} {}", self.path.display(), self.branch, self.message);
        }
        Ok(())
    }
}

/// What every klon of one run shares.
struct Repo {
    golden: PathBuf,
    /// The golden branch, for a klon whose branch has no upstream.
    base: String,
}

pub fn run(args: Args, yes: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().map_err(Error::io("read the current directory"))?;
    // A reboot takes a klon volume down and every klon is then unreachable at
    // its old path, so the image goes back up before the first `git` call (C15).
    let cwd = crate::volume::ensure_attached(&cwd)?;
    let worktrees = git::worktree_list(&cwd)?;
    let golden = paths::absolute(
        &worktrees
            .first()
            .ok_or_else(|| Error::klon("not inside a git repository"))?
            .path,
    )?;
    let common = git::common_dir(&cwd)?;
    let targets = radar::targets(&worktrees);
    let chosen = select(&args, &targets)?;
    if args.check {
        return check(&golden, &common, &targets, &chosen, json);
    }

    // Step 1: the upstream tips this repository knew before the fetch. A
    // force-push is only visible against the tip that came before it, so the
    // read must precede the fetch.
    let before: Vec<Option<String>> = chosen
        .iter()
        .map(|&which| upstream_tip(&targets[which]))
        .collect();
    fetch_once(&golden, &targets, &chosen)?;

    let repo = Repo {
        base: branch::base(&golden)?,
        golden,
    };
    let mut failed = false;
    for (n, &which) in chosen.iter().enumerate() {
        let target = &targets[which];
        let result = one(&repo, target, before[n].clone(), &args, yes, json);
        match result {
            Ok(outcome) => outcome.print(json)?,
            // A single klon reports through the process exit: klon prints the
            // error and the shell sees the code. `--all` must reach every
            // klon, so the refusal becomes a row and the run ends non-zero.
            Err(err) if !args.all => return Err(err),
            Err(err) => {
                failed = true;
                Outcome {
                    schema: SCHEMA,
                    branch: target.branch.clone(),
                    path: paths::absolute(&target.path)?,
                    action: Action::Refused.name(),
                    head_before: head(&target.path),
                    head_after: head(&target.path),
                    message: format!("refused: {}", one_line(&err)),
                }
                .print(json)?;
            }
        }
    }
    match failed {
        // Every failing klon printed its own row, so the exit code carries
        // the news and klon prints no summary line.
        true => Err(Error::Exit(1)),
        false => Ok(()),
    }
}

/// The klons this run works on, in `git worktree list` order.
fn select(args: &Args, targets: &[radar::Target]) -> Result<Vec<usize>> {
    if args.all {
        return Ok((0..targets.len()).collect());
    }
    let name = args
        .branch
        .as_deref()
        .ok_or_else(|| Error::klon("name a branch, or pass --all"))?;
    let which = targets
        .iter()
        .position(|target| target.branch == name)
        .ok_or_else(|| Error::klon(format!("no klon has branch {name} checked out")))?;
    Ok(vec![which])
}

/// `--check`: the C24 radar row of each chosen klon. It runs no fetch and
/// changes nothing, so a dry run never moves a remote-tracking ref.
fn check(
    golden: &Path,
    common: &Path,
    targets: &[radar::Target],
    chosen: &[usize],
    json: bool,
) -> Result<()> {
    for &which in chosen {
        let row = radar::scan_one(golden, common, targets, which);
        let target = &targets[which];
        let path = paths::absolute(&target.path)?;
        if json {
            Outcome {
                schema: SCHEMA,
                branch: target.branch.clone(),
                path,
                action: Action::Check.name(),
                head_before: target.head.clone(),
                head_after: target.head.clone(),
                message: row.columns(),
            }
            .print(json)?;
        } else {
            println!("{} {} {}", path.display(), target.branch, row.columns());
        }
    }
    Ok(())
}

/// Step 1: one fetch for the whole common directory. Every klon shares
/// `refs/remotes/`, so one call updates the upstream of all of them.
fn fetch_once(golden: &Path, targets: &[radar::Target], chosen: &[usize]) -> Result<()> {
    let mut wanted: Vec<String> = vec!["origin".to_string()];
    // A branch may track another remote, for example a fork. That remote is
    // fetched too, once, so `sync` never reads a stale upstream ref.
    for &which in chosen {
        let target = &targets[which];
        if let Some(remote) = remote_of(&target.path, &target.branch) {
            if !wanted.contains(&remote) {
                wanted.push(remote);
            }
        }
    }
    let mut fetched = 0;
    for remote in &wanted {
        if git::run(golden, &["remote", "get-url", remote]).is_err() {
            continue;
        }
        git::run(golden, &["fetch", remote])?;
        fetched += 1;
    }
    if fetched == 0 {
        eprintln!("klon: no origin remote; sync skips the fetch");
    }
    Ok(())
}

/// Steps 2 to 5 for one klon.
fn one(
    repo: &Repo,
    target: &radar::Target,
    tip_before_fetch: Option<String>,
    args: &Args,
    yes: bool,
    json: bool,
) -> Result<Outcome> {
    let path = paths::absolute(&target.path)?;
    let head_before = head(&path);
    if args.fresh {
        return fresh(repo, target, &path, head_before, yes, json);
    }
    let upstream = upstream_of(&path, &target.branch);
    // `--onto <base>` replaces the upstream for this run. The user named the
    // target, so the force-push gate, which is about the upstream klon did not
    // choose, does not apply.
    let sync_to = match &args.onto {
        Some(base) => base_ref(&path, base)?,
        None => match &upstream {
            Some(upstream) => upstream.clone(),
            // A branch whose configured upstream is gone, for example after a
            // `fetch --prune` of a deleted remote branch, is not a branch
            // without an upstream. Rebasing it onto base would rewrite its
            // history onto something the user never named.
            None if tracks_a_remote(&path, &target.branch) => {
                return Err(Error::klon(format!(
                    "the upstream of {} is gone from the remote; pass --onto <base> to name a \
                     target, or drop the upstream with git branch --unset-upstream",
                    target.branch
                )))
            }
            // A branch with no upstream syncs onto base (R30).
            None => base_ref(&path, &repo.base)?,
        },
    };
    let tip = rev_parse(&path, &sync_to)?;
    if args.onto.is_none() {
        if let Some(upstream) = &upstream {
            refuse_force_push(repo, target, &path, upstream, &tip, tip_before_fetch, args)?;
        }
    }

    let action = if head_before.as_deref() == Some(tip.as_str()) {
        Action::UpToDate
    } else if is_ancestor(&path, "HEAD", &tip) == Some(true) {
        git::run(&path, &["merge", "--ff-only", &sync_to])?;
        Action::FastForward
    } else if args.merge {
        git::run(&path, &["merge", &sync_to])?;
        Action::Merge
    } else {
        git::run(&path, &["rebase", "--autostash", &sync_to])?;
        Action::Rebase
    };
    // The tip this klon is now in step with. The next run compares against it,
    // so a force-push that happens between two runs stays visible even where
    // the user fetched by hand in between. A `--onto` run syncs onto another
    // branch, and its tip says nothing about the upstream, so it records none.
    if args.onto.is_none() && upstream.is_some() {
        record_tip(repo, &target.branch, &tip);
    }
    let head_after = head(&path);
    let message = match action {
        Action::UpToDate => format!("up to date with {sync_to} at {}", short(&tip)),
        Action::FastForward => format!(
            "fast-forward {} onto {sync_to}",
            span(&head_before, &head_after)
        ),
        _ => format!(
            "{} {} onto {sync_to}",
            action.name(),
            span(&head_before, &head_after)
        ),
    };
    Ok(Outcome {
        schema: SCHEMA,
        branch: target.branch.clone(),
        path,
        action: action.name(),
        head_before,
        head_after,
        message,
    })
}

/// Step 4: refuse a force-pushed upstream.
///
/// A tip that came before and is no longer an ancestor of the new one means
/// the remote rewrote the branch. klon still proceeds where the klon holds no
/// commit of its own, because a fast-forward or a rebase then loses nothing.
///
/// The evidence has two forms, and klon takes the first that exists:
///
/// | Evidence | It catches |
/// |---|---|
/// | the tip klon recorded at the last sync of this branch | every rewrite since then, also one that another program fetched |
/// | the tip before this run's fetch, and the entry before the last one in the reflog of the upstream ref | the first sync of a branch, where klon has recorded nothing yet |
///
/// The record wins where it exists, and klon writes it after every sync. The
/// reflog would otherwise keep naming the pre-force tip after a `--force`
/// sync, and every later run would refuse again for a rewrite the user
/// already accepted.
fn refuse_force_push(
    repo: &Repo,
    target: &radar::Target,
    path: &Path,
    upstream: &str,
    tip: &str,
    tip_before_fetch: Option<String>,
    args: &Args,
) -> Result<()> {
    let known: Vec<String> = match recorded_tip(repo, &target.branch) {
        Some(recorded) => vec![recorded],
        None => tip_before_fetch
            .into_iter()
            .chain(reflog_previous(&repo.golden, upstream))
            .collect(),
    };
    let Some(old) = known
        .into_iter()
        .find(|old| old != tip && is_ancestor(path, old, tip) == Some(false))
    else {
        return Ok(());
    };
    let sync_to = upstream;
    let unique = git::run(path, &["rev-list", "--count", &format!("{sync_to}..HEAD")])?;
    let unique: u64 = unique.trim().parse().unwrap_or(0);
    if unique == 0 {
        eprintln!("klon: {sync_to} was force-pushed; this klon has no commit of its own");
        return Ok(());
    }
    if args.force {
        eprintln!(
            "klon: {sync_to} was force-pushed; --force syncs the {unique} local commits anyway"
        );
        return Ok(());
    }
    Err(Error::klon(format!(
        "{sync_to} was force-pushed away from {}; this klon has {unique} commit(s) that the new \
         upstream lacks. Inspect them with git log {sync_to}..HEAD, then pass --force to sync anyway",
        short(&old)
    )))
}

/// `--fresh`: remove the klon and make it again from golden, on the same
/// branch and at the same path.
///
/// The dirty check comes first. `rm` refuses a dirty klon on its own, but
/// `--fresh` must say what it is about, and it must never reach `rm --force`.
///
/// The rebuild takes no hot spare. A spare holds golden's ignored state of the
/// moment the builder ran, and `git checkout --force` rewrites only tracked
/// paths, so a spare made before golden's last build would give the klon the
/// old ignored state. `--fresh` promises golden's ignored state of now, so it
/// clones golden and starts a builder afterwards instead.
fn fresh(
    repo: &Repo,
    target: &radar::Target,
    path: &Path,
    head_before: Option<String>,
    yes: bool,
    json: bool,
) -> Result<Outcome> {
    if process::dirty(path)? {
        return Err(Error::klon(format!(
            "{} is dirty; --fresh would lose the changes. Commit or stash them first",
            path.display()
        )));
    }
    if target.branch == "(detached)" {
        return Err(Error::klon(format!(
            "{} has a detached HEAD; --fresh needs a branch to check out again",
            path.display()
        )));
    }
    // `rm` and `add` both read the current directory, and the klon is about to
    // go away. Golden is the one directory that outlives both.
    std::env::set_current_dir(&repo.golden).map_err(Error::io(format!(
        "enter {} for --fresh",
        repo.golden.display()
    )))?;
    // Neither the `rm` nor the `add` starts a builder: a builder that runs
    // during the clone only competes with it for the disk. One starts after.
    super::rm::run(
        super::rm::Args {
            branch: None,
            path: Some(path.to_path_buf()),
            merged: false,
            force: false,
            no_spare: true,
        },
        false,
    )?;
    let spawned = super::add::spawn(
        super::add::Args {
            branch: Some(target.branch.clone()),
            pr: None,
            issue: None,
            path: Some(path.to_path_buf()),
            backend: None,
            path_mode: None,
            no_fixup: false,
            no_spare: true,
            // `sync --fresh` rebuilds one klon that the `rm` above just took
            // away, so the count is the same and the disk budget has nothing
            // to decide (C29).
            evict: false,
            no_budget: true,
            command: Vec::new(),
        },
        yes,
        json,
    )?;
    spare::start_after(
        &repo.golden,
        spare::configured_depth(&repo.golden),
        false,
        None,
    );
    let head_after = head(&spawned.path);
    Ok(Outcome {
        schema: SCHEMA,
        branch: target.branch.clone(),
        action: Action::Fresh.name(),
        message: format!("fresh at {}", head_after.as_deref().map_or("-", short)),
        head_before,
        head_after,
        path: spawned.path,
    })
}

// --- Git helpers ---------------------------------------------------------------

/// The value the upstream ref held before its last update, from the reflog of
/// `refs/remotes/<remote>/<branch>`. A ref with one entry, and a repository
/// that logs no remote-ref update, both give None.
///
/// The read happens after the fetch. Where this run's fetch moved the ref, the
/// answer repeats the tip klon read before it; where another program fetched
/// the rewrite first, this is the only place the old tip survives.
fn reflog_previous(golden: &Path, upstream: &str) -> Option<String> {
    rev_parse(golden, &format!("{upstream}@{{1}}")).ok()
}

/// True when `branch` names a remote in its configuration. A branch that does
/// so has an upstream even where the remote-tracking ref is gone.
fn tracks_a_remote(path: &Path, branch: &str) -> bool {
    git::run(
        path,
        &["config", "--get", &format!("branch.{branch}.merge")],
    )
    .is_ok()
}

/// The remote that `branch` tracks, or None where it tracks none.
fn remote_of(path: &Path, branch: &str) -> Option<String> {
    let out = git::run(
        path,
        &["config", "--get", &format!("branch.{branch}.remote")],
    )
    .ok()?;
    let name = out.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The upstream of a klon's branch: `@{upstream}` when the branch tracks one,
/// else `origin/<branch>` when that remote-tracking branch exists.
fn upstream_of(path: &Path, name: &str) -> Option<String> {
    if let Ok(out) = git::run(
        path,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) {
        let upstream = out.trim();
        if !upstream.is_empty() {
            return Some(upstream.to_string());
        }
    }
    let remote = format!("origin/{name}");
    git::run(
        path,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote}"),
        ],
    )
    .is_ok()
    .then_some(remote)
}

/// The commit the upstream points at right now, before this run fetches.
fn upstream_tip(target: &radar::Target) -> Option<String> {
    let upstream = upstream_of(&target.path, &target.branch)?;
    rev_parse(&target.path, &upstream).ok()
}

/// `origin/<base>` when the remote-tracking branch exists, else the local
/// branch `<base>`.
fn base_ref(path: &Path, base: &str) -> Result<String> {
    let remote = format!("origin/{base}");
    for (rev, full) in [
        (remote.as_str(), format!("refs/remotes/{remote}")),
        (base, format!("refs/heads/{base}")),
    ] {
        if git::run(path, &["show-ref", "--verify", "--quiet", &full]).is_ok() {
            return Ok(rev.to_string());
        }
    }
    Err(Error::klon(format!(
        "sync has nothing to sync onto: neither origin/{base} nor the branch {base} exists"
    )))
}

/// The tip klon recorded at the last successful sync of `branch`.
fn recorded_tip(repo: &Repo, branch: &str) -> Option<String> {
    rev_parse(&repo.golden, &tip_ref(branch)).ok()
}

/// Record the tip this klon is now in step with. A repository can refuse the
/// ref name, for example where a branch `a` and a branch `a/b` both have a
/// klon, so the failure costs one line and never the sync.
fn record_tip(repo: &Repo, branch: &str, tip: &str) {
    if let Err(err) = git::run(&repo.golden, &["update-ref", &tip_ref(branch), tip]) {
        eprintln!(
            "klon: cannot record the upstream tip of {branch}: {}",
            one_line(&err)
        );
    }
}

/// `refs/klon/sync/<branch>`: klon's record of the upstream tip.
fn tip_ref(branch: &str) -> String {
    format!("refs/klon/sync/{branch}")
}

/// Some(true) when `a` is an ancestor of `b`, Some(false) when it is not, and
/// None when git could not answer, for example because the object is gone.
fn is_ancestor(dir: &Path, a: &str, b: &str) -> Option<bool> {
    match git::run(dir, &["merge-base", "--is-ancestor", a, b]) {
        Ok(_) => Some(true),
        Err(Error::Git { code: 1, .. }) => Some(false),
        Err(_) => None,
    }
}

fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    let out = git::run(dir, &["rev-parse", "--verify", "--quiet", rev])?;
    let oid = out.trim();
    match oid.is_empty() {
        true => Err(Error::klon(format!("cannot resolve {rev}"))),
        false => Ok(oid.to_string()),
    }
}

/// The object id of HEAD, or None where git cannot read it.
fn head(dir: &Path) -> Option<String> {
    rev_parse(dir, "HEAD").ok()
}

/// The first seven characters of an object id.
fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// `<before>..<after>` in short object ids.
fn span(before: &Option<String>, after: &Option<String>) -> String {
    let one = |oid: &Option<String>| oid.as_deref().map_or("-", short).to_string();
    format!("{}..{}", one(before), one(after))
}

/// An error as one line, for a `--all` row and for a warning.
fn one_line(err: &Error) -> String {
    err.to_string()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}
