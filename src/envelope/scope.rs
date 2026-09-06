//! The resource scope of one command, one implementation per system.
//!
//! Linux caps memory and tasks with cgroup v2 (C20). macOS has no cgroup: C21
//! polls the footprint of the process group and sends SIGTERM above a
//! threshold. Until C21 lands, the macOS scope is empty and every function
//! below answers "absent" with the reason.

#[cfg(target_os = "linux")]
#[path = "scope_linux.rs"]
mod imp;

/// The macOS placeholder. C21 replaces it with the footprint poll.
#[cfg(not(target_os = "linux"))]
mod imp {
    use crate::envelope::Envelope;
    use crate::probe;
    use std::path::{Path, PathBuf};

    /// Nothing to clean up while no scope exists.
    pub struct Scope;

    impl Scope {
        /// No cgroup exists here, so the fence has nothing to open.
        pub fn cgroup(&self) -> Option<&Path> {
            None
        }
    }

    /// The reason every row below reports.
    const WHY: &str = "cgroup v2 is a Linux feature; C21 adds the macOS scope";

    pub fn apply(_envelope: &mut Envelope) -> Scope {
        Scope
    }

    pub fn klon_cgroups(_pids: &[u32], _name: &str) -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn kill(_dir: &Path) -> bool {
        false
    }

    pub fn systemd_status() -> probe::Status {
        probe::Status::Absent(WHY.to_string())
    }

    pub fn controllers_status() -> probe::Status {
        probe::Status::Absent(WHY.to_string())
    }

    pub fn scope_status(_common: &Path) -> probe::Status {
        probe::Status::Absent(WHY.to_string())
    }
}

pub use imp::*;
