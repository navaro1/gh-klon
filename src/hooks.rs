//! Per-tree repository hooks (spec §7 C22, R20). `add` copies the repository
//! hooks into `<klon>/.klon/hooks`, and every command under `run` points
//! `core.hooksPath` there through `GIT_CONFIG_*`. An agent that edits a hook
//! inside its klon therefore cannot touch golden or a sibling klon, and the
//! fence keeps `<common>/hooks` read-only (handoff §5).
//!
//! Plain `git` outside `run` sees the copy only when the repository already
//! opted into per-worktree config (`extensions.worktreeConfig = true` in the
//! shared config): `add` then also writes `core.hooksPath` into the klon's
//! `config.worktree`. klon never turns the extension on itself, because the
//! extension changes which config file every worktree reads, and that is the
//! user's decision. `doctor` reports the state instead.

use crate::envelope::env;
use crate::{git, probe, Error, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The directory the repository keeps its hooks in: `core.hooksPath` when the
/// config sets one, else `<common>/hooks`. Git runs the hooks of golden in
/// golden's own directory, so a relative `core.hooksPath` resolves against
/// golden here.
fn source(golden: &Path) -> Result<PathBuf> {
    let configured = match git::run(golden, &["config", "--get", "core.hooksPath"]) {
        Ok(text) => Some(text.trim().to_string()),
        // Exit 1 means the key is unset.
        Err(Error::Git { code: 1, .. }) => None,
        Err(err) => return Err(err),
    };
    match configured.filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = Path::new(&value);
            Ok(if path.is_absolute() {
                path.to_path_buf()
            } else {
                golden.join(path)
            })
        }
        None => Ok(git::common_dir(golden)?.join("hooks")),
    }
}

/// Copy the repository hooks into the klon. Only executable regular files
/// survive the copy, and git's `*.sample` examples never do. The copy keeps
/// the modes, so a hook that ran in golden runs in the klon.
///
/// A failure costs one stderr line: git treats a hooks directory with no such
/// file as "no hook", so the klon stays usable, and a missing hook copy never
/// blocks the `add` itself.
pub fn copy_repository_hooks(golden: &Path, klon: &Path) {
    if let Err(err) = copy_all(golden, klon) {
        eprintln!("klon: {err}; the klon runs without repository hooks");
    }
}

fn copy_all(golden: &Path, klon: &Path) -> Result<()> {
    let source = source(golden)?;
    let target = env::hooks_dir(klon);
    // A klon that a hot spare served brings the hooks of the day the spare
    // was built, so the copy starts empty and always ends up with today's
    // repository hooks.
    match fs::remove_dir_all(&target) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(Error::io(format!("clear {}", target.display()))(err)),
    }
    fs::create_dir_all(&target).map_err(Error::io(format!("create {}", target.display())))?;
    let entries = match fs::read_dir(&source) {
        Ok(entries) => entries,
        // A repository without a hooks directory gives the klon no hooks.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io(format!("read {}", source.display()))(err)),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => return Err(Error::io(format!("read {}", source.display()))(err)),
        };
        if entry.file_name().to_string_lossy().ends_with(".sample") {
            continue;
        }
        let from = entry.path();
        let meta = match fs::metadata(&from) {
            Ok(meta) => meta,
            Err(err) => {
                eprintln!("klon: cannot read the hook {}: {err}", from.display());
                continue;
            }
        };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        if let Err(err) = fs::copy(&from, target.join(entry.file_name())) {
            eprintln!("klon: cannot copy the hook {}: {err}", from.display());
        }
    }
    Ok(())
}

/// Give plain `git` in the klon the per-tree hooks too, when the repository
/// already opted into per-worktree config. The value lands in
/// `<common>/worktrees/<name>/config.worktree`, so golden and the sibling
/// klons keep their own hooks. A failure costs one stderr line and leaves
/// `run` as the only way to reach the copies.
pub fn export_to_plain_git(golden: &Path, klon: &Path) {
    if !matches!(
        git::config_bool(golden, "extensions.worktreeConfig"),
        Ok(Some(true))
    ) {
        return;
    }
    let path = env::hooks_dir(klon);
    if let Err(err) = git::run(
        klon,
        &[
            "config",
            "--worktree",
            "core.hooksPath",
            &path.to_string_lossy(),
        ],
    ) {
        eprintln!("klon: plain git in this klon will not use its hooks: {err}");
    }
}

/// The `hooks` row of `doctor` (C22). `Present` when plain git in a new klon
/// also uses the per-tree hooks, with the reason when it does not.
pub fn doctor_row(golden: &Path) -> probe::Status {
    match git::config_bool(golden, "extensions.worktreeConfig") {
        Ok(Some(true)) => probe::Status::Present(
            "extensions.worktreeConfig is on; plain git in a klon uses its own hooks".to_string(),
        ),
        Ok(_) => probe::Status::Absent(
            "per-tree hooks apply under run only; enable extensions.worktreeConfig for plain git"
                .to_string(),
        ),
        Err(err) => probe::Status::Broken(err.to_string()),
    }
}
