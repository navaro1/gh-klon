//! The host-feature probe result (spec §4). Every optional host feature answers
//! with `Present`, `Absent`, or `Broken`. `doctor` prints one row per probe and
//! degrades with one line. C5 (backend) and C16 to C23 (fence, scope, jobserver,
//! netns) add a probe function and one table row; they never add a result type.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The outcome of one probe. `Present` carries the detail that `doctor` prints,
/// for example a version. `Absent` and `Broken` carry the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The feature works. The string is a version or another short detail.
    Present(String),
    /// The host does not have the feature. The string says why.
    Absent(String),
    /// The feature is installed but it does not work. The string says why.
    Broken(String),
}

/// One probe result in JSON: `{"status": "present", "detail": "..."}`.
#[derive(Serialize)]
pub struct Report<'a> {
    pub status: &'static str,
    pub detail: &'a str,
}

impl Status {
    /// The lowercase name of the case: `present`, `absent`, or `broken`.
    pub fn key(&self) -> &'static str {
        match self {
            Status::Present(_) => "present",
            Status::Absent(_) => "absent",
            Status::Broken(_) => "broken",
        }
    }

    /// The version or the reason.
    pub fn detail(&self) -> &str {
        match self {
            Status::Present(text) | Status::Absent(text) | Status::Broken(text) => text,
        }
    }

    /// The JSON shape of this result.
    pub fn report(&self) -> Report<'_> {
        Report {
            status: self.key(),
            detail: self.detail(),
        }
    }

    /// True for `Present`. Read by the chunks that select a feature.
    #[allow(dead_code)]
    pub fn present(&self) -> bool {
        matches!(self, Status::Present(_))
    }
}

/// The path of an executable named `name` in a PATH directory, or None.
pub fn tool_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| executable(&dir.join(name)))
}

/// `path` when it names an executable file, else None.
pub fn executable(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    let ok = meta.is_file() && meta.permissions().mode() & 0o111 != 0;
    ok.then(|| path.to_path_buf())
}

/// Run `program <args>` and return the first output line. A tool that is not on
/// PATH is `Absent`. A tool that fails or prints nothing is `Broken`, because it
/// is installed and does not answer.
pub fn version_of(program: &str, args: &[&str]) -> Status {
    match tool_path(program) {
        Some(path) => run_version(&path, args),
        None => Status::Absent(format!("{program} is not on PATH")),
    }
}

/// `version_of` for a tool that klon already located, for example one under
/// `$KLON_BTRFS_TOOLS`.
pub fn run_version(program: &Path, args: &[&str]) -> Status {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(err) => return Status::Broken(format!("cannot run {}: {err}", program.display())),
    };
    if !output.status.success() {
        return Status::Broken(format!(
            "{} {} exited with {}",
            program.display(),
            args.join(" "),
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    match text.lines().next().map(str::trim).filter(|l| !l.is_empty()) {
        Some(line) => Status::Present(line.to_string()),
        None => Status::Broken(format!("{} printed no version", program.display())),
    }
}
