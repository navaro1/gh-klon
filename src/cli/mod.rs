//! One file per command. The dispatcher in `main.rs` calls `run`.

pub mod add;
pub mod doctor;
pub mod list;
pub mod pr;
pub mod prune;
pub mod rm;
pub mod sync;
pub mod up;
