//! The Linux write fence (handoff §5, R17): a Landlock ruleset that lets a
//! command under `run` read and execute everywhere and write only inside the
//! allow set: the klon, the git directories a commit needs, the temporary
//! directories, the per-user caches, a few device files, and the `[fence]
//! allow` entries of `.klon.toml`.
//!
//! The klon process builds the ruleset and opens every path once. The child
//! applies it after the fork and right before the exec (`Fence::child_step`),
//! so the klon process stays unfenced and the whole command tree inherits the
//! domain: Landlock follows `fork` and `exec`, and a nested ruleset can only
//! tighten it. `<common>` itself is never in the set, so `hooks/` and `config`
//! stay read-only. A commit never needs `packed-refs.lock` under `run`,
//! because the envelope sets `gc.auto=0`.
//!
//! One cost follows from the closed `<common>` root: git locks `packed-refs`
//! for every ref deletion, and the lock lives there, so `git branch -d`,
//! `git tag -d`, and a `fetch --prune` that drops a branch fail under the
//! fence with `Permission denied`. `run --no-fence` or a command outside
//! `run` covers them.

use crate::{config, git, paths, probe, Error, Result};
use landlock::{
    Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, RestrictSelfError,
    Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetError, RulesetStatus, ABI,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The highest ABI klon builds rules for. ABI 2 adds the right to link or
/// rename across directories, ABI 3 the right to truncate. Later ABIs add
/// rights klon does not handle: the device ioctl right of ABI 5 would deny
/// the terminal ioctls every shell needs.
const MAX_ABI: i32 = 3;

/// The version query of `landlock_create_ruleset(2)`: a null attribute and a
/// zero size with this flag return the ABI instead of a ruleset.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;

/// The kernel's answer to the version query.
enum Kernel {
    /// Landlock works and offers this ABI.
    Abi(i32),
    /// The kernel has Landlock but did not enable it at boot.
    NotEnabled,
    /// The kernel has no Landlock.
    NotImplemented,
}

impl Kernel {
    /// Ask the kernel once. The crate keeps its own probe private, and
    /// `doctor` needs the number, so klon makes the one syscall itself.
    fn query() -> Kernel {
        // SAFETY: a null attribute with a zero size is the documented form of
        // the version query. The call reads and writes no memory of ours.
        let version = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if version >= 0 {
            return Kernel::Abi(i32::try_from(version).unwrap_or(i32::MAX));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EOPNOTSUPP) => Kernel::NotEnabled,
            _ => Kernel::NotImplemented,
        }
    }

    /// The `doctor` row: the ABI, or why the fence is absent.
    fn status(&self) -> probe::Status {
        match self {
            Kernel::Abi(abi) => probe::Status::Present(format!("ABI {abi}")),
            Kernel::NotEnabled => probe::Status::Absent(
                "Landlock is built into the kernel but not enabled; \
                 add landlock to the lsm= boot parameter"
                    .to_string(),
            ),
            Kernel::NotImplemented => probe::Status::Absent(
                "the kernel has no Landlock; it needs Linux 5.13 or newer \
                 with CONFIG_SECURITY_LANDLOCK"
                    .to_string(),
            ),
        }
    }
}

/// The `doctor` row for Landlock.
pub fn probe() -> probe::Status {
    Kernel::query().status()
}

/// The rights every path keeps: read a file, list a directory, execute.
pub fn read_set(abi: ABI) -> BitFlags<AccessFs> {
    AccessFs::from_read(abi)
}

/// The rights the allow set adds: write, create, delete, and, where the ABI
/// offers them, link or rename across directories (ABI 2) and truncate
/// (ABI 3). Character and block device nodes stay out: no build needs `mknod`.
/// A kernel with ABI 1 denies every cross-directory rename or link inside a
/// sandbox, so cargo falls back to a copy there; that is the documented cost.
pub fn write_set(abi: ABI) -> BitFlags<AccessFs> {
    let mut set = AccessFs::WriteFile
        | AccessFs::MakeReg
        | AccessFs::MakeDir
        | AccessFs::MakeSym
        | AccessFs::MakeFifo
        | AccessFs::MakeSock
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir;
    if abi >= ABI::V2 {
        set |= AccessFs::Refer;
    }
    if abi >= ABI::V3 {
        set |= AccessFs::Truncate;
    }
    set
}

/// The git directories of one klon.
struct GitDirs {
    /// The main worktree, whose `.klon.toml` names the `[fence] allow` set.
    golden: PathBuf,
    /// `<common>`: the shared git directory.
    common: PathBuf,
    /// `<common>/worktrees/<name>`: the index, `HEAD`, and the reflog of the klon.
    admin: PathBuf,
}

impl GitDirs {
    /// One `git rev-parse` gives both directories. Golden is `<common>`'s
    /// parent in the usual layout; a separate git directory needs one more call.
    fn of(klon: &Path) -> Result<GitDirs> {
        let out = git::run(
            klon,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        )?;
        let mut lines = out.lines();
        let admin = PathBuf::from(
            lines
                .next()
                .ok_or_else(|| Error::klon("git rev-parse printed no git directory"))?,
        );
        let common = PathBuf::from(
            lines
                .next()
                .ok_or_else(|| Error::klon("git rev-parse printed no common directory"))?,
        );
        let golden = match common.parent() {
            Some(parent) if common.file_name().is_some_and(|name| name == ".git") => {
                parent.to_path_buf()
            }
            _ => git::main_worktree(klon)?,
        };
        // Canonical paths, so the `[fence] allow` checks compare like with like.
        Ok(GitDirs {
            golden: paths::absolute(&golden)?,
            common: paths::absolute(&common)?,
            admin,
        })
    }
}

/// One candidate of the allow set.
struct Candidate {
    path: PathBuf,
    /// Why it is there, for the debug line.
    why: String,
    /// Create the directory before the ruleset builds. The fence denies a
    /// `mkdir` in the parent later, so a cache a tool would create on first
    /// use must exist now. System directories and files are never created.
    create: bool,
}

/// The allow set of one klon, in the documented order (handoff §5). Every
/// entry is a candidate: `build` skips the ones that do not exist. The
/// `<common>` root is never here, so `hooks/` and `config` stay read-only.
fn allow_set(
    klon: &Path,
    tmpdir: Option<&str>,
    dirs: &GitDirs,
    allow: &[String],
    cgroup: Option<&Path>,
) -> Vec<Candidate> {
    let mut set: Vec<Candidate> = Vec::new();
    let mut push = |path: PathBuf, why: String, create: bool| {
        if !set.iter().any(|seen| seen.path == path) {
            set.push(Candidate { path, why, create });
        }
    };
    push(klon.to_path_buf(), "the klon".into(), false);
    // git creates `logs` and `rr-cache` on first use with a `mkdir` in
    // `<common>`, which the fence denies. `add` made `klon`; `objects` and
    // `refs` are part of every repository.
    for (name, create) in [
        ("objects", false),
        ("refs", false),
        ("logs", true),
        ("rr-cache", true),
        ("klon", true),
    ] {
        push(dirs.common.join(name), format!("git {name}"), create);
    }
    push(
        dirs.admin.clone(),
        "the worktree admin directory".into(),
        false,
    );
    push(
        dirs.common.join("packed-refs"),
        "git packed-refs".into(),
        false,
    );
    if let Some(tmp) = tmpdir.filter(|value| !value.is_empty()) {
        push(PathBuf::from(tmp), "TMPDIR".into(), true);
    }
    push(PathBuf::from("/tmp"), "/tmp".into(), false);
    push(PathBuf::from("/var/tmp"), "/var/tmp".into(), false);
    if let Some(dir) = env_path("XDG_RUNTIME_DIR") {
        push(dir, "XDG_RUNTIME_DIR".into(), false);
    }
    // The cgroupfs fallback of the scope (C20) joins its cgroup from inside
    // the child, which is fenced by then: `echo $$ > cgroup.procs`. Only that
    // file opens; `memory.high` and the rest of the cgroup stay read-only, so
    // a command cannot lift its own cap.
    if let Some(dir) = cgroup {
        push(
            dir.join("cgroup.procs"),
            "the scope cgroup.procs".into(),
            false,
        );
    }
    // The per-user caches. Each has an environment override and a default
    // under `HOME`; both are candidates, because a tool may use either. An
    // override is created because the user named it. A default is created
    // when its tool is on PATH, so the first `npm install` on a fresh home
    // finds its cache and no unused tool leaves a directory behind.
    let home = env_path("HOME");
    for (variable, default, tool, why) in CACHES {
        if let Some(dir) = env_path(variable) {
            push(dir, (*variable).to_string(), true);
        }
        if let Some(home) = &home {
            let wanted = tool.is_none_or(|tool| probe::tool_path(tool).is_some());
            push(home.join(default), (*why).to_string(), wanted);
        }
    }
    for device in ["/dev/null", "/dev/shm", "/dev/tty", "/dev/ptmx", "/dev/pts"] {
        push(PathBuf::from(device), device.to_string(), false);
    }
    // pasta (C23) runs inside the fence and writes `/proc/self/uid_map` to
    // set up its user namespace. The rule must cover all of `/proc`, because
    // Landlock pins directory inodes when klon adds the rule: `/proc/self`
    // would name the pid of klon, not of the child that runs pasta. No
    // repository path lives under `/proc`, so golden stays read-only, and
    // another user's entries keep their file permissions.
    push(PathBuf::from("/proc"), "/proc".into(), false);
    for entry in allow {
        match allow_entry(entry, klon, dirs) {
            Ok(path) => push(path, format!("[fence] allow {entry}"), false),
            Err(why) => eprintln!("klon: fence: skips [fence] allow entry {entry}: {why}"),
        }
    }
    set
}

/// The per-user caches: the environment override, the default under `HOME`,
/// the tool that owns the cache (None for a cache every tool shares), and the
/// name in the debug line. The pnpm store is its default location; `pnpm
/// store path` would cost a node start on every `run`, so klon does not ask,
/// and `[fence] allow` covers a store elsewhere.
const CACHES: &[(&str, &str, Option<&str>, &str)] = &[
    ("XDG_CACHE_HOME", ".cache", None, "~/.cache"),
    ("CARGO_HOME", ".cargo", Some("cargo"), "~/.cargo"),
    ("npm_config_cache", ".npm", Some("npm"), "~/.npm"),
    (
        "PNPM_HOME",
        ".local/share/pnpm",
        Some("pnpm"),
        "the pnpm store",
    ),
    ("NUGET_PACKAGES", ".nuget", Some("dotnet"), "~/.nuget"),
    (
        "GOCACHE",
        ".cache/go-build",
        Some("go"),
        "the go build cache",
    ),
    (
        "GOMODCACHE",
        "go/pkg/mod",
        Some("go"),
        "the go module cache",
    ),
    ("UV_CACHE_DIR", ".cache/uv", Some("uv"), "the uv cache"),
];

/// The value of an environment variable as a path, when it is set and not empty.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Resolve one `[fence] allow` entry. `~` is the home directory; a relative
/// path is relative to the klon. An entry that would open the fence is
/// refused with a reason: one that holds golden, `<common>`, the home
/// directory, or the klon's parent (the siblings live there), and one inside
/// `<common>`, where `hooks/` and `config` live. A repository must not turn
/// the fence off from inside a file it controls. A directory inside golden
/// stays legal: a repository may share one directory of its own.
fn allow_entry(entry: &str, klon: &Path, dirs: &GitDirs) -> std::result::Result<PathBuf, String> {
    let expanded = if entry == "~" || entry.starts_with("~/") {
        let home = env_path("HOME").ok_or("it uses ~ but HOME is not set")?;
        home.join(&entry[2.min(entry.len())..])
    } else {
        PathBuf::from(entry)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        klon.join(expanded)
    };
    let resolved = paths::absolute(&joined).map_err(|err| err.to_string())?;
    if resolved == Path::new("/") {
        return Err("it resolves to /".into());
    }
    if dirs.golden.starts_with(&resolved) {
        return Err("it holds golden".into());
    }
    if dirs.common.starts_with(&resolved) || resolved.starts_with(&dirs.common) {
        return Err("it holds or enters the git directory".into());
    }
    let home = env_path("HOME").and_then(|home| paths::absolute(&home).ok());
    if home.is_some_and(|home| home.starts_with(&resolved)) {
        return Err("it holds the home directory".into());
    }
    if resolved != klon && klon.starts_with(&resolved) {
        return Err("it holds the klon and its siblings".into());
    }
    Ok(resolved)
}

/// A built ruleset, ready for the child.
pub struct Fence {
    ruleset: RulesetCreated,
}

/// Build the fence of the klon at `klon`. `tmpdir` is the `TMPDIR` of the env
/// file, and `cgroup` the cgroup the scope made for this command, if any. The
/// answer is `None` when the kernel has no Landlock: one stderr line says so,
/// and the command runs without a fence (spec §5).
pub fn build(klon: &Path, tmpdir: Option<&str>, cgroup: Option<&Path>) -> Result<Option<Fence>> {
    let kernel = Kernel::query();
    let abi = match kernel {
        Kernel::Abi(abi) => ABI::from(abi.min(MAX_ABI)),
        _ => {
            eprintln!(
                "klon: fence: {}; the command runs without a write fence",
                kernel.status().detail()
            );
            return Ok(None);
        }
    };
    let dirs = GitDirs::of(klon)?;
    let allow = config::load(&dirs.golden)?
        .fence
        .and_then(|fence| fence.allow)
        .unwrap_or_default();
    let candidates = allow_set(klon, tmpdir, &dirs, &allow, cgroup);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(fence_error("handle the access set"))?
        .create()
        .map_err(fence_error("create the ruleset"))?
        .add_rule(PathBeneath::new(open(Path::new("/"))?, read_set(abi)))
        .map_err(fence_error("allow reads under /"))?;
    for Candidate { path, why, create } in candidates {
        // The klon process is not fenced, so it can still make the directory.
        if create {
            if let Err(err) = fs::create_dir_all(&path) {
                debug(|| format!("cannot create {why} at {}: {err}", path.display()));
            }
        }
        let fd = match PathFd::new(&path) {
            Ok(fd) => fd,
            Err(err) => {
                debug(|| format!("skip {why} at {}: {err}", path.display()));
                continue;
            }
        };
        // A rule on a file may carry only the file rights; the directory
        // rights would make the rule partial and the fence a best effort.
        let is_dir = fs::metadata(&path)
            .map(|meta| meta.is_dir())
            .unwrap_or(true);
        let access = if is_dir {
            write_set(abi)
        } else {
            write_set(abi) & AccessFs::from_file(abi)
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(fence_error(format!(
                "allow writes under {}",
                path.display()
            )))?;
        debug(|| format!("allow {why} at {}", path.display()));
    }
    Ok(Some(Fence { ruleset }))
}

impl Fence {
    /// The step the child runs after the fork and right before the exec. The
    /// descriptor is duplicated here, in the parent, so the child makes only
    /// two syscalls: `prctl(PR_SET_NO_NEW_PRIVS)` and `landlock_restrict_self`.
    /// The crate builds no string on either path, so the step allocates
    /// nothing after the fork. Only an errno survives the fork boundary, so
    /// the error is the raw code and `run` explains it.
    pub fn child_step(&self) -> Result<impl FnMut() -> io::Result<()> + Send + Sync + 'static> {
        let mut ruleset = Some(
            self.ruleset
                .try_clone()
                .map_err(Error::io("duplicate the fence descriptor"))?,
        );
        Ok(move || {
            let Some(ruleset) = ruleset.take() else {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            };
            match ruleset.restrict_self() {
                Ok(status) if status.ruleset == RulesetStatus::FullyEnforced => Ok(()),
                // The fence is on or it is off; a partial fence is not a fence.
                Ok(_) => Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
                Err(err) => Err(io::Error::from_raw_os_error(errno_of(&err))),
            }
        })
    }
}

/// The errno behind a failed `restrict_self`, for the child's answer.
fn errno_of(err: &RulesetError) -> i32 {
    match err {
        RulesetError::RestrictSelf(
            RestrictSelfError::RestrictSelfCall { source, .. }
            | RestrictSelfError::SetNoNewPrivsCall { source, .. },
        ) => source.raw_os_error().unwrap_or(libc::EPERM),
        _ => libc::EPERM,
    }
}

/// True when the spawn error can only come from the fence step. `execve`
/// never answers with these, so `run` can name the fence.
pub fn is_fence_errno(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP | libc::EBADF)
    )
}

/// An `O_PATH` descriptor of `path`, for a rule.
fn open(path: &Path) -> Result<PathFd> {
    PathFd::new(path).map_err(|err| Error::klon(format!("fence: {err}")))
}

fn fence_error(context: impl Into<String>) -> impl FnOnce(RulesetError) -> Error {
    let context = context.into();
    move |err| Error::klon(format!("fence: {context}: {err}"))
}

/// One stderr line under `KLON_DEBUG=1`, for the skipped and the allowed paths.
fn debug(line: impl FnOnce() -> String) {
    if std::env::var_os("KLON_DEBUG").is_some_and(|value| !value.is_empty() && value != "0") {
        eprintln!("klon: fence: {}", line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_write_set_follows_the_abi() {
        // AC: a forced ABI of 2 has no truncate right and keeps the write right.
        let v2 = write_set(ABI::V2);
        assert!(v2.contains(AccessFs::WriteFile));
        assert!(!v2.contains(AccessFs::Truncate));
        assert!(v2.contains(AccessFs::Refer));
        let v1 = write_set(ABI::V1);
        assert!(!v1.contains(AccessFs::Refer));
        assert!(!v1.contains(AccessFs::Truncate));
        let v3 = write_set(ABI::V3);
        assert!(v3.contains(AccessFs::Truncate));
        assert!(v3.contains(AccessFs::WriteFile));
        // No device nodes, ever.
        assert!(!v3.contains(AccessFs::MakeChar));
        assert!(!v3.contains(AccessFs::MakeBlock));
    }

    #[test]
    fn the_rules_stay_inside_the_handled_set() {
        // A rule with a right the ruleset does not handle is a kernel error,
        // so every write set must sit inside `from_all` of the same ABI, and
        // read and write must not overlap.
        for abi in [ABI::V1, ABI::V2, ABI::V3] {
            let all = AccessFs::from_all(abi);
            assert_eq!(write_set(abi) & !all, BitFlags::EMPTY, "{abi:?}");
            assert_eq!(read_set(abi) & write_set(abi), BitFlags::EMPTY, "{abi:?}");
        }
        // The cap keeps the fence at the tested ABI on a newer kernel.
        assert_eq!(ABI::from(MAX_ABI), ABI::V3);
        assert_eq!(ABI::from(9.min(MAX_ABI)), ABI::V3);
    }

    fn dirs(root: &Path) -> GitDirs {
        GitDirs {
            golden: root.join("golden"),
            common: root.join("golden").join(".git"),
            admin: root
                .join("golden")
                .join(".git")
                .join("worktrees")
                .join("feature"),
        }
    }

    #[test]
    fn the_allow_set_never_holds_the_common_root() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let klon = tmp.path().join("golden.wt").join("feature");
        let cgroup = Path::new("/sys/fs/cgroup/user.slice/klon-feature-1");
        let set = allow_set(&klon, Some("/tmp/x"), &dirs, &[], Some(cgroup));
        let paths: Vec<&Path> = set.iter().map(|c| c.path.as_path()).collect();
        // The scope's cgroup opens only through its `cgroup.procs` file.
        assert!(paths.contains(&cgroup.join("cgroup.procs").as_path()));
        assert!(!paths.contains(&cgroup));
        // System directories and the git directories that always exist are
        // never created; `logs`, `rr-cache`, and `TMPDIR` are.
        let created: Vec<&Path> = set
            .iter()
            .filter(|c| c.create)
            .map(|c| c.path.as_path())
            .collect();
        assert!(created.contains(&dirs.common.join("logs").as_path()));
        assert!(created.contains(&dirs.common.join("rr-cache").as_path()));
        assert!(created.contains(&Path::new("/tmp/x")));
        assert!(!created.contains(&Path::new("/tmp")));
        assert!(!created.contains(&Path::new("/dev/null")));
        assert!(!created.contains(&dirs.common.join("objects").as_path()));
        assert!(!created.contains(&klon.as_path()));
        assert!(paths.contains(&klon.as_path()));
        assert!(paths.contains(&dirs.common.join("objects").as_path()));
        assert!(paths.contains(&dirs.common.join("refs").as_path()));
        assert!(paths.contains(&dirs.common.join("logs").as_path()));
        assert!(paths.contains(&dirs.common.join("rr-cache").as_path()));
        assert!(paths.contains(&dirs.common.join("klon").as_path()));
        assert!(paths.contains(&dirs.admin.as_path()));
        assert!(paths.contains(&dirs.common.join("packed-refs").as_path()));
        assert!(paths.contains(&Path::new("/tmp/x")));
        assert!(paths.contains(&Path::new("/dev/null")));
        assert!(!paths.contains(&dirs.common.as_path()), "{paths:?}");
        assert!(!paths.contains(&dirs.golden.as_path()), "{paths:?}");
        assert!(!paths.contains(&dirs.common.join("hooks").as_path()));
        assert!(!paths.contains(&Path::new("/")));
    }

    #[test]
    fn an_allow_entry_resolves_against_the_klon_and_refuses_the_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let klon = tmp.path().join("golden.wt").join("feature");
        assert_eq!(
            allow_entry("../shared", &klon, &dirs).unwrap(),
            tmp.path().join("golden.wt").join("shared")
        );
        assert_eq!(
            allow_entry("/opt/cache", &klon, &dirs).unwrap(),
            PathBuf::from("/opt/cache")
        );
        assert!(allow_entry("/", &klon, &dirs).is_err());
        assert!(allow_entry("../../golden", &klon, &dirs).is_err());
        assert!(allow_entry("../../golden/.git", &klon, &dirs).is_err());
        // An ancestor opens everything below it, so it is refused too: the
        // parent of golden, the parent of the klon (the siblings live there),
        // and any entry inside `<common>`.
        assert!(allow_entry("../..", &klon, &dirs).is_err());
        assert!(allow_entry("..", &klon, &dirs).is_err());
        assert!(allow_entry("../../golden/.git/hooks", &klon, &dirs).is_err());
        assert!(allow_entry(tmp.path().to_str().unwrap(), &klon, &dirs).is_err());
        // The klon itself and a directory inside golden stay legal.
        assert_eq!(allow_entry(".", &klon, &dirs).unwrap(), klon);
        assert_eq!(
            allow_entry("../../golden/shared", &klon, &dirs).unwrap(),
            dirs.golden.join("shared")
        );
        if let Some(home) = env_path("HOME") {
            assert!(allow_entry(home.to_str().unwrap(), &klon, &dirs).is_err());
            assert!(allow_entry("~", &klon, &dirs).is_err());
            if let Some(parent) = home.parent().filter(|p| *p != Path::new("/")) {
                assert!(allow_entry(parent.to_str().unwrap(), &klon, &dirs).is_err());
            }
            assert_eq!(
                allow_entry("~/.local/share/x", &klon, &dirs).unwrap(),
                paths::absolute(&home.join(".local/share/x")).unwrap()
            );
        }
    }
}
