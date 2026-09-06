//! One file per command. The dispatcher in `main.rs` calls `run`.

pub mod add;
pub mod bench;
pub mod doctor;
pub mod init;
pub mod init_volume;
pub mod list;
pub mod pr;
pub mod prune;
pub mod rm;
pub mod run;
pub mod shell;
pub mod spare_build;
pub mod stop;
pub mod sync;
pub mod up;
pub mod warm;
