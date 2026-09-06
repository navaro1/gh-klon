//! `gh klon bench` (spec §7 C8, R14; handoff §8).
//!
//! The benchmark has four parts:
//!
//! | Module | Role |
//! |---|---|
//! | `manifest` | The versioned manifest: the seed, the shapes, the cells, the run counts, the timer points, and the pass rule |
//! | `fixture` | The deterministic repository generator: a file tree, a cargo workspace, or a pnpm project |
//! | `disk` | The unique bytes of one idle tree, for M5 |
//! | `runner` | The measured run: the samples, the random order, and the correctness check |
//! | `report` | The record shapes, the percentiles, the environment record, and the table |
//!
//! Nothing here changes a user's repository. A run builds its own fixture in a
//! scratch directory and removes it again.

pub mod disk;
pub mod fixture;
pub mod manifest;
pub mod report;
pub mod runner;
