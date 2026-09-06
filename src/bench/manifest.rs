//! The versioned benchmark manifest (spec §7 C8, R14; handoff §8).
//!
//! `bench/manifests/v1.toml` fixes the fixture seed and shape, the cells, the
//! run counts, the timer points, and the pass rule before a run starts. The
//! file is embedded at build time, so a binary always carries the manifest it
//! was built with and a result can never come from an edited copy on disk.
//!
//! `fixture_hash` is a hash of the seed and of every profile. A changed seed or
//! a changed shape gives a new hash, so a reader can tell at once whether two
//! result files measure the same repository.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The manifest version this binary understands.
pub const VERSION: u32 = 1;

/// The path of the manifest inside the repository, for the report.
pub const PATH: &str = "bench/manifests/v1.toml";

/// The manifest text, embedded at build time.
const SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench/manifests/v1.toml"
));

/// The whole manifest.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub seed: u64,
    pub runs: Runs,
    pub profiles: BTreeMap<String, Profile>,
    pub cells: Vec<Cell>,
    /// True when `KLON_BENCH_SMOKE=1` replaced every profile with the tiny
    /// shape below. The report says so, and the fixture hash differs, so a
    /// smoke result can never pass for a measurement.
    #[serde(skip)]
    pub smoke: bool,
}

/// The shape that `KLON_BENCH_SMOKE=1` gives every profile. It builds in about
/// a second, which is what the test suite needs: the tests prove the shape of a
/// report and the void path, not the speed of a laptop.
const SMOKE: Profile = Profile {
    tracked_files: 200,
    dirs: 5,
    ignored_files: 20,
    ignored_file_bytes: 4096,
    changed_files: 5,
    added_files: 2,
};

/// The sample counts.
#[derive(Debug, Deserialize)]
pub struct Runs {
    pub warm: u32,
    pub cold: u32,
    pub release_warm: u32,
    pub release_cold: u32,
    pub steady_calls: u32,
}

/// One generated repository shape.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Profile {
    pub tracked_files: usize,
    pub dirs: usize,
    pub ignored_files: usize,
    pub ignored_file_bytes: usize,
    pub changed_files: usize,
    pub added_files: usize,
}

/// What a cell measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// M1: create a tree.
    Add,
    /// M4: `git status` in a fresh tree.
    Status,
    /// M6: remove a tree.
    Rm,
}

/// One benchmark cell.
#[derive(Debug, Clone, Deserialize)]
pub struct Cell {
    pub name: String,
    pub metric: String,
    pub profile: String,
    pub action: Action,
    /// Where the timer starts and stops, in words. The report copies it.
    pub timer: String,
    /// The pass rule: the p50 budget of the primary series, in milliseconds.
    pub pass_p50_ms: u64,
    /// The p50 budget of the steady series, for M4.
    #[serde(default)]
    pub pass_steady_p50_ms: Option<u64>,
    /// The value that `KLON_FIXTURE` must hold before this cell may run. A cell
    /// without it always runs.
    #[serde(default)]
    pub requires_fixture: Option<String>,
}

impl Manifest {
    /// Parse the embedded manifest, and shrink it when `KLON_BENCH_SMOKE=1`
    /// asks for a smoke run.
    pub fn load() -> Result<Manifest> {
        let mut manifest = Manifest::parse(SOURCE)?;
        if std::env::var("KLON_BENCH_SMOKE").as_deref() == Ok("1") {
            manifest.shrink_to_smoke();
        }
        Ok(manifest)
    }

    /// Put the smoke shape in place of every profile.
    pub fn shrink_to_smoke(&mut self) {
        self.smoke = true;
        for profile in self.profiles.values_mut() {
            *profile = SMOKE;
        }
    }

    /// The profile of `cell`. `parse` proved that it exists.
    pub fn profile_of(&self, cell: &Cell) -> Profile {
        self.profiles[&cell.profile]
    }

    /// Parse `text`. A version this binary does not know fails closed, like the
    /// journal and the probe cache.
    pub fn parse(text: &str) -> Result<Manifest> {
        let manifest: Manifest = toml::from_str(text)
            .map_err(|err| Error::klon(format!("read the bench manifest: {err}")))?;
        if manifest.version != VERSION {
            return Err(Error::klon(format!(
                "unknown bench manifest version {}; this klon reads version {VERSION}",
                manifest.version
            )));
        }
        for cell in &manifest.cells {
            if !manifest.profiles.contains_key(&cell.profile) {
                return Err(Error::klon(format!(
                    "cell {} names the unknown profile {}",
                    cell.name, cell.profile
                )));
            }
        }
        Ok(manifest)
    }

    /// The cell named `name`.
    pub fn cell(&self, name: &str) -> Result<&Cell> {
        self.cells
            .iter()
            .find(|cell| cell.name == name)
            .ok_or_else(|| {
                let names: Vec<&str> = self.cells.iter().map(|c| c.name.as_str()).collect();
                Error::klon(format!(
                    "unknown cell {name}; the manifest holds {}",
                    names.join(", ")
                ))
            })
    }

    /// The warm and cold sample counts for this run. `KLON_BENCH_RUNS`
    /// overrides both, so the test suite can run a cell in seconds. The report
    /// records the count that was used.
    pub fn run_counts(&self, release: bool) -> (u32, u32) {
        if let Some(runs) = override_runs() {
            return (runs, runs);
        }
        if release {
            (self.runs.release_warm, self.runs.release_cold)
        } else {
            (self.runs.warm, self.runs.cold)
        }
    }

    /// Sixteen hex digits over the seed and every profile. A changed seed or a
    /// changed shape gives a new hash (R14).
    pub fn fixture_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("klon.bench manifest {}\n", self.version));
        hasher.update(format!("seed {}\n", self.seed));
        // `profiles` is a BTreeMap, so the order is the name order on every host.
        for (name, p) in &self.profiles {
            hasher.update(format!(
                "profile {name} {} {} {} {} {} {}\n",
                p.tracked_files,
                p.dirs,
                p.ignored_files,
                p.ignored_file_bytes,
                p.changed_files,
                p.added_files
            ));
        }
        let digest = hasher.finalize();
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The `KLON_BENCH_RUNS` override, when it names a count of one or more.
fn override_runs() -> Option<u32> {
    std::env::var("KLON_BENCH_RUNS")
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|runs| *runs > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with one profile and one cell. The tests edit one field of it.
    fn source(seed: u64, tracked: usize) -> String {
        format!(
            r#"
version = 1
seed = {seed}
[runs]
warm = 10
cold = 5
release_warm = 30
release_cold = 10
steady_calls = 3
[profiles.p10k]
tracked_files = {tracked}
dirs = 100
ignored_files = 10
ignored_file_bytes = 100
changed_files = 2
added_files = 2
[[cells]]
name = "m1-add-10k"
metric = "M1"
profile = "p10k"
action = "add"
timer = "process"
pass_p50_ms = 1000
"#
        )
    }

    #[test]
    fn the_embedded_manifest_parses() {
        let manifest = Manifest::load().expect("the embedded manifest must parse");
        assert_eq!(manifest.version, VERSION);
        assert_eq!(manifest.runs.warm, 10, "the development run is 10 warm");
        assert_eq!(manifest.runs.cold, 5, "the development run is 5 cold");
        assert_eq!(manifest.runs.release_warm, 30);
        assert_eq!(manifest.runs.release_cold, 10);
        let names: Vec<&str> = manifest.cells.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["m1-add-10k", "m1-add-100k", "m4-status-100k", "m6-rm-100k"]
        );
    }

    /// The C8 acceptance line: a changed seed changes the fixture hash.
    #[test]
    fn a_changed_seed_changes_the_fixture_hash() {
        let first = Manifest::parse(&source(1, 10_000)).unwrap();
        let second = Manifest::parse(&source(2, 10_000)).unwrap();
        assert_ne!(first.fixture_hash(), second.fixture_hash());
        // The same text gives the same hash on every run.
        let again = Manifest::parse(&source(1, 10_000)).unwrap();
        assert_eq!(first.fixture_hash(), again.fixture_hash());
        assert_eq!(first.fixture_hash().len(), 16);
    }

    /// A changed shape is as important as a changed seed: it also gives a new
    /// repository, so it must give a new hash.
    #[test]
    fn a_changed_shape_changes_the_fixture_hash() {
        let first = Manifest::parse(&source(1, 10_000)).unwrap();
        let second = Manifest::parse(&source(1, 20_000)).unwrap();
        assert_ne!(first.fixture_hash(), second.fixture_hash());
    }

    /// A smoke run gives a new shape, so it gives a new fixture hash. A reader
    /// can never mistake a smoke result for a measurement.
    #[test]
    fn a_smoke_run_changes_the_shape_and_the_hash() {
        let full = Manifest::parse(&source(1, 10_000)).unwrap();
        let mut smoke = Manifest::parse(&source(1, 10_000)).unwrap();
        smoke.shrink_to_smoke();
        assert!(!full.smoke);
        assert!(smoke.smoke);
        assert_ne!(full.fixture_hash(), smoke.fixture_hash());
        assert_eq!(smoke.profiles["p10k"].tracked_files, SMOKE.tracked_files);
    }

    #[test]
    fn a_future_manifest_version_fails_closed() {
        let text = source(1, 10).replace("version = 1", "version = 99");
        let err = Manifest::parse(&text).expect_err("version 99 must fail");
        assert!(
            err.to_string()
                .contains("unknown bench manifest version 99"),
            "unexpected error {err}"
        );
    }

    #[test]
    fn a_cell_with_an_unknown_profile_is_refused() {
        let text = source(1, 10).replace(r#"profile = "p10k""#, r#"profile = "p1m""#);
        let err = Manifest::parse(&text).expect_err("an unknown profile must fail");
        assert!(
            err.to_string().contains("unknown profile p1m"),
            "unexpected error {err}"
        );
    }

    #[test]
    fn an_unknown_cell_name_lists_the_known_ones() {
        let manifest = Manifest::load().unwrap();
        let err = manifest
            .cell("m9-nothing")
            .expect_err("an unknown cell fails");
        let text = err.to_string();
        assert!(text.contains("unknown cell m9-nothing"), "found {text}");
        assert!(text.contains("m1-add-10k"), "found {text}");
    }
}
