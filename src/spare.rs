//! The hot spare (spec §7 C9, R12, R40; handoff §4 "Hot spare").
//!
//! After `add`, `up`, and `rm`, klon starts one detached low-priority process,
//! `gh-klon spare-build <golden>`. That process clones golden into
//! `../<repo>.wt/.spare.tmp`, copies golden's index into `.spare.tmp/.klon/`,
//! writes `.spare.tmp/.klon/spare.json`, and renames the result to `.spare`.
//! The next `add` then renames `.spare` into place instead of cloning: one
//! `rename(2)`, whatever the size of the tree.
//!
//! Three rules keep the spare safe:
//!
//! 1. One lock, `../<repo>.wt/.spare.lock`, serializes the builder and every
//!    claim. A second builder exits at once. Two concurrent `add` calls use the
//!    spare at most once; the other clones directly.
//! 2. The tear check. The builder records the mtimes of golden's ignored
//!    directories before and after the clone. A difference means golden
//!    changed while the spare was made, so the spare may hold a torn build
//!    state. `add` deletes such a spare and clones directly.
//! 3. A stale spare (another HEAD) is still used. `git checkout --force`
//!    rewrites every tracked path that differs, and the ignored state is as
//!    warm as it was.
//!
//! The claim inside `add` runs after `git worktree add --no-checkout --detach
//! --lock` made the target: an empty directory with a `.git` file. The claim
//! moves that directory aside, to `.<name>.klon-claim` in the same parent,
//! then moves the spare to the target. Each move is one `rename`, so a crash
//! leaves either the target missing or the target populated, never both paths
//! gone. The stub stays beside the target, not inside the spare: a btrfs
//! snapshot spare is its own subvolume, and a plain directory cannot be
//! renamed into another subvolume. The worktree is locked through the whole
//! claim, so `git worktree prune` cannot drop the entry while the target is
//! away, and `doctor --repair` removes either state through `git worktree
//! remove --force`.

use crate::backend::{self, Exclusions};
use crate::{config, git, paths, process, time, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The format version of `spare.json`. A spare with a newer version is left
/// alone and not used.
pub const VERSION: u32 = 1;

/// The spare directory name under `../<repo>.wt`.
pub const DIR: &str = ".spare";
/// The builder's work directory. It becomes `.spare` with one rename.
const TMP: &str = ".spare.tmp";
/// The lock that serializes the builder and every claim.
const LOCK: &str = ".spare.lock";
/// The metadata file, under `<spare>/.klon/`.
const META: &str = "spare.json";
/// The suffix of the stub beside the target where the claim parks the empty
/// directory that `git worktree add` made.
const CLAIM_SUFFIX: &str = ".klon-claim";
/// The entries of `../<repo>.wt` that klon owns. `add` refuses them as a
/// destination, so the builder and the claim never delete a user's tree.
const RESERVED: [&str; 4] = [DIR, TMP, LOCK, ".trash"];

/// The paths of one repository's spare.
pub struct Layout {
    /// `../<repo>.wt`.
    pub root: PathBuf,
    /// `../<repo>.wt/.spare`.
    pub dir: PathBuf,
    tmp: PathBuf,
    lock: PathBuf,
}

impl Layout {
    pub fn of(golden: &Path) -> Layout {
        let root = paths::default_wt_root(golden);
        Layout {
            dir: root.join(DIR),
            tmp: root.join(TMP),
            lock: root.join(LOCK),
            root,
        }
    }

    fn meta(&self) -> PathBuf {
        self.dir.join(crate::envelope::env::DIR).join(META)
    }
}

/// `spare.json`: what the builder saw, so `add` can judge the spare.
#[derive(Debug, Serialize, Deserialize)]
pub struct Meta {
    pub version: u32,
    /// Golden's HEAD before the clone.
    pub head: String,
    /// The SHA-256 of `git status --porcelain` in golden before the clone.
    pub status_hash: String,
    /// The mtime of every ignored directory of golden before the clone, as
    /// `<seconds>.<nanoseconds>`, keyed by the path that git prints.
    pub top_mtimes_before: BTreeMap<String, String>,
    /// The same map after the clone. A difference marks the spare as torn.
    pub top_mtimes_after: BTreeMap<String, String>,
    /// The hash of the exclusion rules the clone followed: `.klonignore`,
    /// `.worktreeinclude`, and `.gitmodules`. A spare made under other rules
    /// may hold paths that the current rules leave out, and neither the
    /// checkout nor `git clean` removes an ignored path, so `add` discards
    /// such a spare.
    pub exclusions_hash: String,
    /// The backend that made the clone.
    pub backend: String,
    /// When the builder finished, RFC 3339 in UTC.
    pub created: String,
}

/// What the builder did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new `.spare` is in place.
    Built,
    /// A spare already existed, so nothing was built.
    Exists,
    /// Another builder or a claim holds the lock.
    Busy,
}

/// What `add` got from the spare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The spare now sits at the target path. Its `.klon/index` is golden's
    /// index of the moment the spare was made. The string names the backend
    /// that made the spare, which the `add` report carries.
    Used(String),
    /// No usable spare. `add` clones directly.
    Direct,
}

/// The name of the reserved entry of `../<repo>.wt` that `path` is or sits
/// under: `.spare`, `.spare.tmp`, `.spare.lock`, or `.trash`. None for every
/// other path. `add` refuses such a destination before any change.
pub fn reserved(golden: &Path, path: &Path) -> Option<&'static str> {
    let root = paths::default_wt_root(golden);
    RESERVED
        .into_iter()
        .find(|name| path.starts_with(root.join(name)))
}

/// The hash of the exclusion rules of golden: the bytes of `.klonignore`,
/// `.worktreeinclude`, and `.gitmodules`, each an empty string when absent
/// (C11 reads the three). A change to any of them changes the set of paths
/// that a clone leaves out.
fn exclusions_hash(golden: &Path) -> String {
    let mut hasher = Sha256::new();
    for name in [".klonignore", ".worktreeinclude", ".gitmodules"] {
        let bytes = fs::read(golden.join(name)).unwrap_or_default();
        hasher.update(bytes.len().to_le_bytes());
        hasher.update(&bytes);
    }
    config::hex(&hasher.finalize())
}

/// The stub beside `path` where the claim parks the empty target directory:
/// `<parent>/.<name>.klon-claim`. The same parent, so the rename never
/// crosses a filesystem or a btrfs subvolume.
fn claim_stub(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}{CLAIM_SUFFIX}"))
}

// --- Policy --------------------------------------------------------------------

/// True when a command may claim and build a spare. Three switches turn it
/// off, and any one of them wins: `--no-spare` on the call, `KLON_SPARE=0` in
/// the environment, and `spare = 0` in `.klon.toml`.
pub fn enabled(depth: Option<u32>, no_spare: bool) -> bool {
    !no_spare && std::env::var("KLON_SPARE").as_deref() != Ok("0") && depth != Some(0)
}

/// The `spare` depth of `<golden>/.klon.toml`, or None when the file is absent
/// or unreadable. `rm` and `up` call this after their own work, and a config
/// error must not fail a command that already completed.
pub fn configured_depth(golden: &Path) -> Option<u32> {
    config::load(golden).ok().and_then(|cfg| cfg.spare)
}

/// Start the detached builder after a command, when the policy allows it. A
/// failure to start costs one stderr line and never fails the command.
pub fn start_after(golden: &Path, depth: Option<u32>, no_spare: bool) {
    if !enabled(depth, no_spare) {
        return;
    }
    if let Err(err) = start_builder(golden) {
        eprintln!("klon: cannot start the spare builder: {err}");
    }
}

/// Start `gh-klon spare-build <golden>` detached at low priority, unless a
/// spare exists or a builder is already running. The call returns at once.
pub fn start_builder(golden: &Path) -> Result<()> {
    let layout = Layout::of(golden);
    if layout.dir.exists() {
        debug("a spare exists; no builder started");
        return Ok(());
    }
    fs::create_dir_all(&layout.root)
        .map_err(Error::io(format!("create {}", layout.root.display())))?;
    match Lock::try_acquire(&layout.lock)? {
        // The check is a courtesy: the builder takes the lock itself.
        None => {
            debug("a builder is running; no builder started");
            return Ok(());
        }
        Some(lock) => drop(lock),
    }
    process::spawn_detached_klon(
        &[OsStr::new("spare-build"), golden.as_os_str()],
        "spare builder",
    )
}

// --- The builder ---------------------------------------------------------------

/// The body of `gh-klon spare-build`. Take the lock without waiting, and build
/// a spare when none exists.
pub fn build(golden: &Path) -> Result<Outcome> {
    let golden = git::main_worktree(golden)?;
    let layout = Layout::of(&golden);
    fs::create_dir_all(&layout.root)
        .map_err(Error::io(format!("create {}", layout.root.display())))?;
    let Some(_lock) = Lock::try_acquire(&layout.lock)? else {
        return Ok(Outcome::Busy);
    };
    if layout.dir.exists() {
        return Ok(Outcome::Exists);
    }
    build_locked(&golden, &layout)?;
    Ok(Outcome::Built)
}

/// `bench` calls this before a timed `add`: wait for any builder, then build a
/// spare in this process when none exists.
pub fn ensure(golden: &Path) -> Result<()> {
    let golden = git::main_worktree(golden)?;
    let layout = Layout::of(&golden);
    fs::create_dir_all(&layout.root)
        .map_err(Error::io(format!("create {}", layout.root.display())))?;
    let _lock = Lock::acquire(&layout.lock)?;
    if layout.dir.exists() {
        return Ok(());
    }
    build_locked(&golden, &layout)
}

/// `bench` calls this after a timed `add`: wait until the builder that the
/// `add` started has finished, so the next sample runs on a quiet disk.
pub fn wait_for_builder(golden: &Path, timeout: Duration) -> Result<()> {
    let golden = git::main_worktree(golden)?;
    let layout = Layout::of(&golden);
    let started = Instant::now();
    loop {
        if layout.dir.exists() {
            // The builder renamed its result. Wait for it to let go of the lock.
            drop(Lock::acquire(&layout.lock)?);
            return Ok(());
        }
        // The builder takes the lock before any other step. A free lock after
        // the grace period means that no builder is alive; the next `ensure`
        // builds the spare in the bench process instead.
        if started.elapsed() > Duration::from_secs(1) && !layout.tmp.exists() {
            if let Some(lock) = Lock::try_acquire(&layout.lock)? {
                drop(lock);
                if !layout.dir.exists() {
                    return Ok(());
                }
            }
        }
        if started.elapsed() > timeout {
            return Err(Error::klon(format!(
                "the spare builder of {} did not finish in {} s",
                golden.display(),
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Build `.spare` while the caller holds the lock.
fn build_locked(golden: &Path, layout: &Layout) -> Result<()> {
    let common = git::common_dir(golden)?;
    let worktrees = git::worktree_list(golden)?;
    // The lock proves that no builder is alive, so a work directory that is
    // still there belongs to one that died. Any age. `add` refuses the path
    // as a destination; a registration from an older klon still stops the
    // delete here, because the lock proves nothing about ownership.
    if layout.tmp.exists() {
        if git::is_registered(golden, &layout.tmp) {
            return Err(Error::klon(format!(
                "{} is a registered worktree; the spare builder does not delete it",
                layout.tmp.display()
            )));
        }
        remove_tree(&layout.tmp)?;
    }
    let others = worktrees
        .iter()
        .map(|w| paths::absolute(&w.path))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|p| p != golden);
    let exclude = Exclusions::new(
        golden,
        others.chain([layout.tmp.clone(), layout.dir.clone()]),
    );

    let head = git::run(golden, &["rev-parse", "HEAD"])?.trim().to_string();
    let status = git::run(golden, &["status", "--porcelain"])?;
    let status_hash = config::hex(&Sha256::digest(status.as_bytes()));
    let before = top_mtimes(golden, &exclude)?;

    let choice = backend::select(golden, &common, Some(&layout.tmp), None)?;
    fs::create_dir(&layout.tmp).map_err(Error::io(format!("create {}", layout.tmp.display())))?;
    let filled =
        fill_tmp(golden, &common, layout, choice.backend.as_ref(), &exclude).and_then(|()| {
            let meta = Meta {
                version: VERSION,
                head,
                status_hash,
                top_mtimes_before: before,
                top_mtimes_after: top_mtimes(golden, &exclude)?,
                exclusions_hash: exclusions_hash(golden),
                backend: choice.backend.name().to_string(),
                created: time::now_rfc3339(),
            };
            let text = serde_json::to_string_pretty(&meta)
                .map_err(|err| Error::klon(format!("serialize spare.json: {err}")))?;
            let file = layout.tmp.join(crate::envelope::env::DIR).join(META);
            fs::write(&file, text).map_err(Error::io(format!("write {}", file.display())))?;
            fs::rename(&layout.tmp, &layout.dir).map_err(Error::io(format!(
                "rename {} to {}",
                layout.tmp.display(),
                layout.dir.display()
            )))
        });
    if let Err(err) = filled {
        if let Err(cleanup) = remove_tree(&layout.tmp) {
            eprintln!("klon: {cleanup}");
        }
        return Err(err);
    }
    Ok(())
}

/// The clone and the index copy into the work directory.
fn fill_tmp(
    golden: &Path,
    common: &Path,
    layout: &Layout,
    backend: &dyn backend::Backend,
    exclude: &Exclusions,
) -> Result<()> {
    backend.clone(golden, &layout.tmp, exclude)?;
    // The index goes in after the clone, so it describes every file that the
    // clone holds, and it gets a fresh mtime, so no entry is racy for git.
    let klon_dir = layout.tmp.join(crate::envelope::env::DIR);
    fs::create_dir_all(&klon_dir).map_err(Error::io(format!("create {}", klon_dir.display())))?;
    let index = klon_dir.join("index");
    fs::copy(common.join("index"), &index).map_err(Error::io("copy the index"))?;
    let shared = git::run(
        golden,
        &["rev-parse", "--path-format=absolute", "--shared-index-path"],
    )?;
    let shared = shared.trim();
    if !shared.is_empty() {
        let shared = Path::new(shared);
        let name = shared
            .file_name()
            .ok_or_else(|| Error::klon("invalid shared index path"))?;
        fs::copy(shared, klon_dir.join(name)).map_err(Error::io("copy the shared index"))?;
    }
    File::open(&index)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .map_err(Error::io("touch the index"))
}

/// The mtime of every ignored directory that the clone includes, keyed by the
/// path that `git ls-files --others --ignored --directory` prints. A directory
/// mtime changes when a direct child appears or goes, which is what a build
/// that starts or ends in golden does.
fn top_mtimes(golden: &Path, exclude: &Exclusions) -> Result<BTreeMap<String, String>> {
    let out = git::run(
        golden,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    let mut map = BTreeMap::new();
    for entry in out.split('\0').filter(|e| e.ends_with('/')) {
        let dir = golden.join(entry.trim_end_matches('/'));
        if exclude.excludes(&dir, true) {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(&dir) {
            map.insert(
                entry.to_string(),
                format!("{}.{:09}", meta.mtime(), meta.mtime_nsec()),
            );
        }
    }
    Ok(map)
}

/// macOS: put this process in the background band, which throttles its cpu
/// and disk use like `nice` and `ionice` do on Linux (handoff §4).
#[cfg(target_os = "macos")]
pub fn lower_priority() {
    // SAFETY: `setpriority` takes three integers and touches no memory of ours.
    let rc = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG) };
    if rc != 0 {
        eprintln!(
            "klon: cannot lower the priority: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Linux and the rest: the spawner already wrapped the builder in `nice` and
/// `ionice`.
#[cfg(not(target_os = "macos"))]
pub fn lower_priority() {}

// --- The claim -----------------------------------------------------------------

/// Move the spare to `path`, the empty directory that `git worktree add` made
/// for the klon whose admin entry is `admin_dir`. The answer says whether the
/// spare was used; on `Direct` the target is as `git worktree add` left it.
///
/// A torn spare is discarded here with a `spare torn` line, and so is a spare
/// made under other exclusion rules or one that this klon cannot read. A
/// spare from a newer klon is left alone. `wanted` is the `--backend` of the
/// call: a user who names a backend gets that backend, so a spare made by
/// another one is left for a call without the override.
pub fn claim(golden: &Path, path: &Path, admin_dir: &Path, wanted: Option<&str>) -> Result<Claim> {
    let layout = Layout::of(golden);
    if !layout.dir.exists() {
        return Ok(Claim::Direct);
    }
    let Some(_lock) = Lock::try_acquire(&layout.lock)? else {
        debug("the spare lock is busy; cloning directly");
        return Ok(Claim::Direct);
    };
    // Another `add` may have taken the spare while this one waited for the
    // list above.
    if !layout.dir.exists() {
        return Ok(Claim::Direct);
    }
    let meta = match read_meta(&layout) {
        Ok(meta) if meta.version > VERSION => {
            eprintln!(
                "klon: {} has version {}; this klon reads version {VERSION}; cloning directly",
                layout.dir.display(),
                meta.version
            );
            return Ok(Claim::Direct);
        }
        Ok(meta) if meta.top_mtimes_after != meta.top_mtimes_before => {
            eprintln!(
                "klon: spare torn: golden's ignored directories changed while the spare was built; cloning directly"
            );
            discard(golden, &layout);
            return Ok(Claim::Direct);
        }
        Ok(meta) if meta.exclusions_hash != exclusions_hash(golden) => {
            eprintln!(
                "klon: the spare predates a .klonignore change; deleting it and cloning directly"
            );
            discard(golden, &layout);
            return Ok(Claim::Direct);
        }
        Ok(meta) => meta,
        Err(err) => {
            eprintln!("klon: the spare is damaged ({err}); cloning directly");
            discard(golden, &layout);
            return Ok(Claim::Direct);
        }
    };
    if wanted.is_some_and(|name| name != meta.backend) {
        eprintln!(
            "klon: the spare was made by {}, not by --backend {}; cloning directly",
            meta.backend,
            wanted.unwrap_or_default()
        );
        return Ok(Claim::Direct);
    }
    let head = git::run(golden, &["rev-parse", "HEAD"])?;
    if head.trim() != meta.head {
        debug("the spare is stale; git checkout --force fixes the tracked files");
    }

    // The `.git` file is written into the spare first, so the target holds a
    // valid one from the instant the rename lands. The lock keeps another
    // `add` from writing its own admin entry in between.
    fs::write(
        layout.dir.join(".git"),
        format!("gitdir: {}\n", admin_dir.display()),
    )
    .map_err(Error::io("write .git into the spare"))?;
    let stub = claim_stub(path);
    // A stub from a claim of this path that died between its two renames.
    if let Err(err) = fs::remove_dir_all(&stub) {
        if err.kind() != ErrorKind::NotFound {
            return Err(Error::io(format!("remove {}", stub.display()))(err));
        }
    }
    crate::journal::pause_at("spare-claim");
    fs::rename(path, &stub).map_err(Error::io(format!("move {} aside", path.display())))?;
    crate::journal::pause_at("spare-moved");
    if let Err(err) = fs::rename(&layout.dir, path) {
        // Put the empty target back, so the transaction continues with a
        // direct clone.
        fs::rename(&stub, path).map_err(Error::io(format!(
            "restore {} after a failed claim ({err})",
            path.display()
        )))?;
        if err.raw_os_error() == Some(libc::EXDEV) {
            eprintln!(
                "klon: the spare is on another filesystem than {}; cloning directly",
                path.display()
            );
        } else {
            eprintln!("klon: cannot claim the spare ({err}); cloning directly");
        }
        return Ok(Claim::Direct);
    }
    // The stub holds the `.git` file that `git worktree add` wrote and nothing
    // else. A failure here leaves a stale stub that the next claim of this
    // path removes.
    if let Err(err) = fs::remove_dir_all(&stub) {
        eprintln!("klon: cannot remove {}: {err}", stub.display());
    }
    Ok(Claim::Used(meta.backend))
}

/// Move the spare's index files into the admin entry: `.klon/index` and any
/// `.klon/sharedindex.*` of a split index. The answer is false when the spare
/// brought no index, so the caller copies golden's instead.
pub fn take_index(path: &Path, admin_dir: &Path) -> Result<bool> {
    let klon_dir = path.join(crate::envelope::env::DIR);
    let index = klon_dir.join("index");
    if !index.is_file() {
        return Ok(false);
    }
    move_file(&index, &admin_dir.join("index"))?;
    let entries =
        fs::read_dir(&klon_dir).map_err(Error::io(format!("read {}", klon_dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("read {}", klon_dir.display())))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("sharedindex.") {
            move_file(&entry.path(), &admin_dir.join(&name))?;
        }
    }
    Ok(true)
}

/// Delete `<klon>/.klon`, which holds only what the builder and the claim left
/// there. `add` calls it before it writes the envelope.
pub fn drop_metadata(path: &Path) -> Result<()> {
    let klon_dir = path.join(crate::envelope::env::DIR);
    match fs::remove_dir_all(&klon_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("remove {}", klon_dir.display()))(err)),
    }
}

/// Rename `from` to `to`; copy and delete when the two sit on two filesystems.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            fs::copy(from, to).map_err(Error::io(format!("copy {}", from.display())))?;
            fs::remove_file(from).map_err(Error::io(format!("remove {}", from.display())))
        }
        Err(err) => Err(Error::io(format!("move {}", from.display()))(err)),
    }
}

fn read_meta(layout: &Layout) -> Result<Meta> {
    let file = layout.meta();
    let text = fs::read_to_string(&file).map_err(Error::io(format!("read {}", file.display())))?;
    serde_json::from_str(&text)
        .map_err(|err| Error::klon(format!("{} is not a spare record: {err}", file.display())))
}

/// Move a spare that `add` cannot use into `.trash` and delete it the way
/// `rm` deletes a klon: through the cached backend, so a btrfs subvolume
/// takes its one-ioctl delete, else the detached `rm -rf`. When the rename
/// fails, the delete runs in place.
fn discard(golden: &Path, layout: &Layout) {
    // `add` refuses `.spare` as a destination. A registration from an older
    // klon still stops the delete: a worktree may hold uncommitted work.
    if git::is_registered(golden, &layout.dir) {
        eprintln!(
            "klon: {} is a registered worktree; klon does not delete it",
            layout.dir.display()
        );
        return;
    }
    let trash = layout.root.join(".trash");
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let victim = trash.join(format!("{DIR}-{secs}-{}", std::process::id()));
    let moved = fs::create_dir_all(&trash).and_then(|()| fs::rename(&layout.dir, &victim));
    let result = match moved {
        Ok(()) => match git::common_dir_of_main(golden)
            .ok()
            .and_then(|common| backend::cached(&common))
        {
            Some(backend) => backend.delete(&victim),
            None => process::spawn_background_delete(&victim),
        },
        Err(_) => remove_tree(&layout.dir),
    };
    if let Err(err) = result {
        eprintln!("klon: cannot delete the spare: {err}");
    }
}

/// Delete a tree that klon made, read-only directories included.
fn remove_tree(path: &Path) -> Result<()> {
    if let Err(err) = backend::make_removable(path) {
        eprintln!("klon: {err}");
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(format!("remove {}", path.display()))(err)),
    }
}

/// One `KLON_DEBUG=1` line.
fn debug(message: &str) {
    if crate::debug() {
        eprintln!("klon: debug: spare: {message}");
    }
}

// --- The lock ------------------------------------------------------------------

/// An exclusive `flock` on `.spare.lock`. The file is never renamed or
/// deleted, so every holder locks one inode. Closing the descriptor releases
/// the lock, so a killed builder or `add` never blocks the next one.
struct Lock {
    file: File,
}

impl Lock {
    fn open(path: &Path) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .map_err(Error::io(format!("open {}", path.display())))
    }

    /// Take the lock, or answer None when another process holds it.
    fn try_acquire(path: &Path) -> Result<Option<Lock>> {
        let file = Lock::open(path)?;
        loop {
            // SAFETY: the descriptor is open and owned by `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Some(Lock { file }));
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                ErrorKind::Interrupted => continue,
                ErrorKind::WouldBlock => return Ok(None),
                _ => return Err(Error::io(format!("lock {}", path.display()))(err)),
            }
        }
    }

    /// Take the lock; wait for the holder when there is one.
    fn acquire(path: &Path) -> Result<Lock> {
        let file = Lock::open(path)?;
        loop {
            // SAFETY: the descriptor is open and owned by `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(Lock { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != ErrorKind::Interrupted {
                return Err(Error::io(format!("lock {}", path.display()))(err));
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still open; the close below releases the
        // lock anyway.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_switches_all_win() {
        // The environment is read live, so the test keeps it clear.
        std::env::remove_var("KLON_SPARE");
        assert!(enabled(None, false));
        assert!(enabled(Some(1), false));
        assert!(!enabled(Some(0), false));
        assert!(!enabled(None, true));
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let held = Lock::try_acquire(&path).unwrap().expect("a free lock");
        assert!(
            Lock::try_acquire(&path).unwrap().is_none(),
            "a second holder must be refused"
        );
        drop(held);
        assert!(Lock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn the_layout_sits_next_to_golden() {
        let layout = Layout::of(Path::new("/w/repo"));
        assert_eq!(layout.root, Path::new("/w/repo.wt"));
        assert_eq!(layout.dir, Path::new("/w/repo.wt/.spare"));
        assert_eq!(layout.tmp, Path::new("/w/repo.wt/.spare.tmp"));
        assert_eq!(
            layout.meta(),
            Path::new("/w/repo.wt/.spare/.klon/spare.json")
        );
    }

    #[test]
    fn the_reserved_entries_are_named_and_the_rest_are_not() {
        let golden = Path::new("/w/repo");
        assert_eq!(
            reserved(golden, Path::new("/w/repo.wt/.spare")),
            Some(".spare")
        );
        assert_eq!(
            reserved(golden, Path::new("/w/repo.wt/.spare.tmp/x")),
            Some(".spare.tmp")
        );
        assert_eq!(
            reserved(golden, Path::new("/w/repo.wt/.trash/f-1")),
            Some(".trash")
        );
        assert_eq!(reserved(golden, Path::new("/w/repo.wt/feature")), None);
        assert_eq!(reserved(golden, Path::new("/w/repo.wt/.spare-x")), None);
    }

    #[test]
    fn the_claim_stub_sits_beside_the_target() {
        let stub = claim_stub(Path::new("/w/repo.wt/feature"));
        assert_eq!(stub, Path::new("/w/repo.wt/.feature.klon-claim"));
        assert_eq!(stub.parent(), Path::new("/w/repo.wt/feature").parent());
    }

    #[test]
    fn a_meta_record_round_trips() {
        let mut before = BTreeMap::new();
        before.insert("build/".to_string(), "1.000000000".to_string());
        let meta = Meta {
            version: VERSION,
            head: "abc".to_string(),
            status_hash: "0".repeat(64),
            top_mtimes_before: before.clone(),
            top_mtimes_after: before,
            exclusions_hash: "0".repeat(64),
            backend: "copy".to_string(),
            created: time::now_rfc3339(),
        };
        let text = serde_json::to_string(&meta).unwrap();
        let read: Meta = serde_json::from_str(&text).unwrap();
        assert_eq!(read.version, 1);
        assert_eq!(read.top_mtimes_before, read.top_mtimes_after);
    }
}
