//! The benchmark fixture generator (spec §7 C8, C31; handoff §8).
//!
//! `bench` needs its own generator inside the binary, because the test harness
//! in `tests/common` belongs to the test targets. One seed gives one
//! repository: the same file bytes and the same commit dates, so two hosts
//! measure the same work.
//!
//! Three kinds of repository:
//!
//! | Kind | Shape | Ignored state | Cells |
//! |---|---|---|---|
//! | `synthetic` | `tracked_files` files over `dirs` directories, a `feature` branch with a small diff | `build/`, pseudo-random bytes | M1, M2, M4, M5, M6 |
//! | `rust` | A cargo workspace of `crates` member crates, each with `functions` generated functions | `target/` | M3, M12 |
//! | `pnpm` | One package that depends on a tarball inside the fixture | `node_modules/` | M3 |
//!
//! The two ecosystem kinds put `feature` on the same commit as `main`. A source
//! edit between the branches would make the first build in a klon compile that
//! crate again, and M3 asks what a klon compiles when nothing changed.
//!
//! Every `git` call here isolates the configuration. A user's global config
//! must not change what the benchmark measures.

use super::manifest::Profile;
use crate::{probe, Error, Result};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The branch that every cell checks out.
pub const BRANCH: &str = "feature";

/// The ignored directory of the synthetic kind. The correctness check compares
/// it between golden and the new tree: it is the warm build state that a plain
/// `git worktree add` leaves behind.
pub const IGNORED_DIR: &str = "build";

/// Which repository a cell measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The generated file tree of C8.
    Synthetic,
    /// A cargo workspace.
    Rust,
    /// A pnpm project with a vendored tarball.
    Pnpm,
}

impl Kind {
    /// The name that the manifest and the report use.
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Synthetic => "synthetic",
            Kind::Rust => "rust",
            Kind::Pnpm => "pnpm",
        }
    }

    /// The directories that `.gitignore` lists. The correctness check compares
    /// them, and M5 counts the bytes below them.
    pub fn ignored_dirs(self) -> &'static [&'static str] {
        match self {
            Kind::Synthetic => &[IGNORED_DIR],
            Kind::Rust => &["target"],
            Kind::Pnpm => &["node_modules"],
        }
    }

    /// True when a klon of this kind holds an ignored state that the path fixup
    /// pass rewrites. Such a tree can never equal golden byte for byte, so its
    /// cell rests on the metric it measures instead (handoff §9).
    pub fn fixup_rewrites_ignored_state(self) -> bool {
        !matches!(self, Kind::Synthetic)
    }

    /// The programs a cell of this kind needs. A host without one skips the
    /// cell with a reason instead of failing the run.
    pub fn tools(self) -> &'static [&'static str] {
        match self {
            Kind::Synthetic => &[],
            Kind::Rust => &["cargo"],
            Kind::Pnpm => &["pnpm", "node", "tar"],
        }
    }
}

/// The shape of an ecosystem fixture. The synthetic kind ignores it.
///
/// It is not part of `fixture_hash`: that hash covers the seed and the profiles
/// alone, so a C8 result and a C31 result of the same profile still compare.
/// Every record prints its own `fixture_shape`, so a reader can still tell two
/// ecosystem runs apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub struct Shape {
    /// The member crates of the cargo workspace.
    pub crates: usize,
    /// The generated functions per crate. They give `rustc` real work.
    pub functions: usize,
}

/// Everything that decides what one fixture holds. Two cells with one recipe
/// share one generated repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Recipe {
    pub kind: Kind,
    pub profile: Profile,
    pub shape: Shape,
    /// True when the generator runs the ecosystem build once in golden, so a
    /// klon of it starts warm. An M12 fixture stays cold: every builder must do
    /// real work.
    pub warm: bool,
}

impl Recipe {
    /// A short name for the scratch directory and for the run log.
    pub fn key(&self, profile_name: &str) -> String {
        format!(
            "{profile_name}-{}-{}x{}-{}",
            self.kind.tag(),
            self.shape.crates,
            self.shape.functions,
            if self.warm { "warm" } else { "cold" }
        )
    }
}

/// A generated repository inside a scratch directory. `Drop` removes the whole
/// directory, so an interrupted run leaves no gigabytes behind.
pub struct Fixture {
    root: PathBuf,
    golden: PathBuf,
    recipe: Recipe,
}

impl Fixture {
    /// Build the recipe below `base`. The directory name holds the pid, so two
    /// runs on one host never collide.
    pub fn build(base: &Path, name: &str, seed: u64, recipe: &Recipe) -> Result<Fixture> {
        let root = scratch_dir(base, name)?;
        let fixture = Fixture {
            golden: root.join("golden"),
            root,
            recipe: *recipe,
        };
        match recipe.kind {
            Kind::Synthetic => fixture.generate(seed, &recipe.profile)?,
            Kind::Rust => fixture.generate_rust(seed, recipe)?,
            Kind::Pnpm => fixture.generate_pnpm(recipe)?,
        }
        Ok(fixture)
    }

    /// The main checkout, left on `main`.
    pub fn golden(&self) -> &Path {
        &self.golden
    }

    /// The directory that holds golden. Every klon and every baseline worktree
    /// of a run lands below it, so both stay on golden's filesystem.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn kind(&self) -> Kind {
        self.recipe.kind
    }

    /// The pnpm store. It sits beside golden, not inside it, so golden, a klon,
    /// and a baseline worktree all reach the same store. A store inside the
    /// ignored state would leave the baseline with nothing to install from, and
    /// the comparison would measure the missing store instead of the tool.
    pub fn store(&self) -> PathBuf {
        self.root.join("pnpm-store")
    }

    fn generate(&self, seed: u64, profile: &Profile) -> Result<()> {
        let golden = &self.golden;
        create_dir(golden)?;
        for d in 0..profile.dirs {
            create_dir(&golden.join(dir_name(d)))?;
        }
        write_all((0..profile.tracked_files).collect(), |i| {
            let path = golden.join(tracked_rel(profile.dirs, i));
            fs::write(&path, tracked_body(seed, i)).map_err(Error::io(format!(
                "write the fixture file {}",
                path.display()
            )))
        })?;
        let build = golden.join(IGNORED_DIR);
        create_dir(&build)?;
        let bytes = profile.ignored_file_bytes;
        write_all((0..profile.ignored_files).collect(), |i| {
            let path = build.join(format!("o{i}.bin"));
            fs::write(&path, ignored_body(seed, i, bytes)).map_err(Error::io(format!(
                "write the fixture file {}",
                path.display()
            )))
        })?;
        fs::write(golden.join(".gitignore"), format!("/{IGNORED_DIR}/\n"))
            .map_err(Error::io("write the fixture .gitignore"))?;

        git(golden, &["init", "-q", "-b", "main"])?;
        git(golden, &["add", "-A"])?;
        commit(golden, "base", commit_time(seed, 0))?;

        git(golden, &["checkout", "-qb", BRANCH])?;
        for i in 0..profile.changed_files {
            let path = golden.join(tracked_rel(
                profile.dirs,
                i * 7 % profile.tracked_files.max(1),
            ));
            fs::write(&path, changed_body(seed, i)).map_err(Error::io(format!(
                "edit the fixture file {}",
                path.display()
            )))?;
        }
        for i in 0..profile.added_files {
            let path = golden.join(format!("new-{i}.txt"));
            fs::write(&path, added_body(seed, i)).map_err(Error::io(format!(
                "add the fixture file {}",
                path.display()
            )))?;
        }
        git(golden, &["add", "-A"])?;
        commit(golden, "feature", commit_time(seed, 1))?;
        git(golden, &["checkout", "-q", "main"])?;
        Ok(())
    }

    /// A cargo workspace of `shape.crates` member crates. Every crate holds
    /// `shape.functions` generated functions, which is what gives `rustc` real
    /// work: an M12 run needs a solo build that already uses every token, or
    /// the six-way run would beat it for free.
    ///
    /// The crates depend on nothing, so one build fills the whole token pool.
    /// Nothing reaches a registry: every command carries `--offline`.
    fn generate_rust(&self, seed: u64, recipe: &Recipe) -> Result<()> {
        let golden = &self.golden;
        create_dir(golden)?;
        let names: Vec<String> = (0..recipe.shape.crates).map(crate_name).collect();
        let members: Vec<String> = names
            .iter()
            .map(|name| format!("    \"{name}\","))
            .collect();
        fs::write(
            golden.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\n{}\n]\nresolver = \"2\"\n",
                members.join("\n")
            ),
        )
        .map_err(Error::io("write the workspace manifest"))?;
        for (index, name) in names.iter().enumerate() {
            let dir = golden.join(name);
            create_dir(&dir.join("src"))?;
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .map_err(Error::io("write a crate manifest"))?;
            fs::write(
                dir.join("src").join("lib.rs"),
                crate_body(seed, index, recipe.shape.functions),
            )
            .map_err(Error::io("write a crate source"))?;
        }
        // `Cargo.lock` is tracked, so a klon never resolves anything again.
        fs::write(golden.join(".gitignore"), "/target/\n")
            .map_err(Error::io("write the fixture .gitignore"))?;
        self.commit_ecosystem(seed)?;
        if recipe.warm {
            let cargo = cargo().ok_or_else(|| Error::klon("cargo is not on PATH"))?;
            let out = build_command(Kind::Rust, &cargo, golden, &self.store(), true)
                .output()
                .map_err(Error::io("run the warm cargo build"))?;
            if !out.status.success() {
                return Err(Error::klon(format!(
                    "the warm cargo build failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            // The warm build writes `Cargo.lock`. Commit it, or `git clean`
            // would delete it from every klon.
            self.commit_ecosystem(seed)?;
        }
        Ok(())
    }

    /// One package that depends on a tarball inside the fixture, installed with
    /// pnpm. The store sits beside golden, so a klon and a baseline worktree
    /// both reach it (handoff §9).
    fn generate_pnpm(&self, recipe: &Recipe) -> Result<()> {
        let golden = &self.golden;
        create_dir(golden)?;
        fs::write(golden.join("package.json"), PACKAGE_JSON)
            .map_err(Error::io("write package.json"))?;
        fs::write(golden.join(".gitignore"), "node_modules/\n")
            .map_err(Error::io("write the fixture .gitignore"))?;
        pack(golden)?;
        self.commit_ecosystem(0)?;
        if recipe.warm {
            let pnpm = pnpm().ok_or_else(|| Error::klon("pnpm is not on PATH"))?;
            let out = build_command(Kind::Pnpm, &pnpm, golden, &self.store(), true)
                .output()
                .map_err(Error::io("run the warm pnpm install"))?;
            if !out.status.success() {
                return Err(Error::klon(format!(
                    "the warm pnpm install failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            // `pnpm-lock.yaml` must be tracked: `--frozen-lockfile` needs it,
            // and `git clean` would delete an untracked one from every klon.
            self.commit_ecosystem(0)?;
        }
        Ok(())
    }

    /// Commit everything and put `feature` on the same commit as `main`.
    ///
    /// An ecosystem fixture carries no diff between the two branches. A source
    /// edit would make the first build in a klon compile that crate again, and
    /// M3 asks what a klon compiles when nothing changed.
    fn commit_ecosystem(&self, seed: u64) -> Result<()> {
        let golden = &self.golden;
        if !golden.join(".git").exists() {
            git(golden, &["init", "-q", "-b", "main"])?;
        }
        git(golden, &["add", "-A"])?;
        // An empty commit would fail. Nothing changed means nothing to do.
        if git(golden, &["diff", "--cached", "--quiet"]).is_err() {
            commit(golden, "base", commit_time(seed, 0))?;
        }
        git(golden, &["branch", "-f", BRANCH, "main"])?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir_all(&self.root) {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("klon: bench: cannot remove {}: {err}", self.root.display());
            }
        }
    }
}

/// Run `write` for every index, over every core. The fixture is thousands of
/// small files, so a serial loop would dominate the run.
fn write_all(indexes: Vec<usize>, write: impl Fn(usize) -> Result<()> + Send + Sync) -> Result<()> {
    indexes
        .into_par_iter()
        .map(write)
        .reduce(|| Ok(()), |a: Result<()>, b| a.and(b))
}

fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(Error::io(format!("create {}", path.display())))
}

/// `<base>/klon-bench-<name>-<pid>-<n>`, a directory that does not exist yet.
fn scratch_dir(base: &Path, name: &str) -> Result<PathBuf> {
    create_dir(base)?;
    let pid = std::process::id();
    for n in 0..64u32 {
        let candidate = base.join(format!("klon-bench-{name}-{pid}-{n}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(Error::io(format!("create {}", candidate.display()))(err)),
        }
    }
    Err(Error::klon(format!(
        "cannot create a bench directory in {}",
        base.display()
    )))
}

// --- git ---------------------------------------------------------------------

/// Run `git -C <cwd> <args>` with an isolated identity and configuration.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = isolated_git(cwd, args)
        .output()
        .map_err(Error::io("run git for the fixture"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A `git` command with the fixture identity and no user configuration. The
/// runner spawns the timed `git` samples with it too, so a measured call and a
/// setup call see one configuration.
pub fn isolated_git(cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    isolate(&mut command);
    command
}

/// Give `command` the fixture identity and no user configuration.
pub fn isolate(command: &mut Command) {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "klon")
        .env("GIT_AUTHOR_EMAIL", "klon@example.com")
        .env("GIT_COMMITTER_NAME", "klon")
        .env("GIT_COMMITTER_EMAIL", "klon@example.com");
}

/// Commit with a pinned date, so one seed gives one set of commit ids.
fn commit(golden: &Path, message: &str, when: u64) -> Result<()> {
    let date = format!("@{when} +0000");
    let mut command = isolated_git(golden, &["commit", "-qm", message]);
    command
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date);
    let output = command
        .output()
        .map_err(Error::io("run git commit for the fixture"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Git {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

// --- The ecosystem fixtures ---------------------------------------------------

/// The package that the pnpm fixture installs. The dependency is a tarball
/// inside the fixture, so the install never reaches a registry.
const PACKAGE_JSON: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "private": true,
  "dependencies": { "leftpad": "file:./vendor/leftpad-1.0.0.tgz" }
}
"#;

/// Environment variables that leak from a `cargo test` parent into a nested
/// build and change where it writes or how it links. Every build the bench
/// starts clears them, so a `cargo test` run and a shell run measure the same
/// build.
const CARGO_LEAKS: &[&str] = &[
    "CARGO",
    "CARGO_BUILD_JOBS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_MAKEFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_UNSTABLE_SPARSE_REGISTRY",
    "MAKEFLAGS",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
];

/// The crate directory and package name of member `index`.
fn crate_name(index: usize) -> String {
    format!("kc{index:03}")
}

/// `functions` generated functions. The body of each one is derived from the
/// seed, so one seed gives one workspace on every host.
fn crate_body(seed: u64, index: usize, functions: usize) -> String {
    let mut out = String::new();
    for f in 0..functions {
        let salt = payload(seed, (index as u64) << 20 | f as u64);
        out.push_str(&format!(
            "pub fn f{f}(x: u64) -> u64 {{\n    let k = 0x{}u64;\n    x.wrapping_mul(k).rotate_left({}) ^ k\n}}\n",
            &salt[..16],
            f % 63 + 1
        ));
    }
    out
}

/// Write a tarball of one tiny package into `vendor/`. `tar` is part of every
/// supported host, and a tarball dependency lands in the pnpm store, which a
/// `file:` directory dependency does not.
///
/// The flags are the portable ones only. `--sort`, `--owner=0`, `--group=0`,
/// and `--mtime` belong to GNU tar; the BSD tar that macOS ships rejects them,
/// and the fixture would fail to build on a host that has pnpm. The staged
/// files carry a pinned mtime instead, which both tars record, so the archive
/// stays as repeatable as a portable call allows. Its bytes are outside
/// `fixture_hash` either way: the cell counts installed packages, not bytes.
fn pack(golden: &Path) -> Result<()> {
    let vendor = golden.join("vendor");
    let stage = vendor.join("package");
    create_dir(&stage)?;
    fs::write(
        stage.join("package.json"),
        "{ \"name\": \"leftpad\", \"version\": \"1.0.0\", \"main\": \"index.js\" }\n",
    )
    .map_err(Error::io("write the vendored package.json"))?;
    fs::write(
        stage.join("index.js"),
        "module.exports = function (s) { return ' ' + s; };\n",
    )
    .map_err(Error::io("write the vendored index.js"))?;
    let pinned = filetime::FileTime::from_unix_time(1_577_836_800, 0);
    for name in ["package.json", "index.js"] {
        filetime::set_file_mtime(stage.join(name), pinned)
            .map_err(Error::io("re-time the vendored package"))?;
    }
    filetime::set_file_mtime(&stage, pinned).map_err(Error::io("re-time the tar directory"))?;
    let out = Command::new("tar")
        .current_dir(&vendor)
        .args(["-czf", "leftpad-1.0.0.tgz", "package"])
        .output()
        .map_err(Error::io("run tar for the fixture package"))?;
    if !out.status.success() {
        return Err(Error::klon(format!(
            "tar cannot write the fixture package: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    fs::remove_dir_all(&stage).map_err(Error::io("remove the tar staging directory"))
}

/// The `cargo` on PATH.
pub fn cargo() -> Option<PathBuf> {
    probe::tool_path("cargo")
}

/// The `pnpm` on PATH, or the one the pnpm installer puts under `HOME`. pnpm
/// installs itself outside PATH on the development laptop (handoff §11).
pub fn pnpm() -> Option<PathBuf> {
    if let Some(found) = probe::tool_path("pnpm") {
        return Some(found);
    }
    let home = std::env::var_os("HOME")?;
    probe::executable(&Path::new(&home).join(".local/share/pnpm/pnpm"))
}

/// The program named `name`, with the extra places klon knows about.
pub fn tool(name: &str) -> Option<PathBuf> {
    match name {
        "pnpm" => pnpm(),
        other => probe::tool_path(other),
    }
}

/// The build that an M3 or an M12 cell measures.
///
/// `first` is the run in golden that fills the ignored state. It resolves and
/// writes the lock file; every later run reads it. Both are offline: the
/// fixture depends on nothing outside itself.
pub fn build_command(
    kind: Kind,
    program: &Path,
    tree: &Path,
    store: &Path,
    first: bool,
) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(tree)
        // A coloured `Compiling` line carries escape codes that no plain text
        // match survives, and the unit count is a text match.
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1");
    for name in CARGO_LEAKS {
        command.env_remove(name);
    }
    match kind {
        Kind::Synthetic => {
            // A synthetic cell builds nothing. `true` keeps the signature total.
            command = Command::new("true");
        }
        Kind::Rust => {
            command.args(["build", "--offline"]);
            if let Some(jobs) = smoke_jobs() {
                command.args(["-j", &jobs.to_string()]);
            }
        }
        Kind::Pnpm => {
            command.args(["install", "--offline", "--child-concurrency=1"]);
            if !first {
                command.arg("--frozen-lockfile");
            }
            command
                .arg("--store-dir")
                .arg(store)
                // `CI=1` keeps pnpm from asking whether it may rebuild
                // `node_modules`.
                .env("CI", "1")
                .env("UV_THREADPOOL_SIZE", "2");
        }
    }
    command
}

/// The job cap that a smoke build takes, or None for a measured run.
///
/// A smoke run measures nothing: it proves the shape of a report. It runs
/// inside `cargo test`, beside the other test binaries of this suite and beside
/// the builds of whoever else is on the machine, so it must not claim every
/// core. Two jobs is the same cap that the C11 zero-compile tests take, and for
/// the same reason.
///
/// A measured run takes no cap. Bounding a real M12 build is the jobserver's
/// work, and that is exactly what the cell measures.
fn smoke_jobs() -> Option<usize> {
    (std::env::var("KLON_BENCH_SMOKE").as_deref() == Ok("1")).then_some(2)
}

/// The units that one build compiled, from its combined output.
///
/// cargo prints one `Compiling` line per crate it builds. pnpm prints one
/// `Progress:` line whose `downloaded` and `added` counts are the packages it
/// fetched or linked; a run that did neither reports zero.
pub fn units_compiled(kind: Kind, output: &str) -> u64 {
    match kind {
        Kind::Synthetic => 0,
        Kind::Rust => output
            .lines()
            .filter(|line| line.contains("Compiling "))
            .count() as u64,
        Kind::Pnpm => pnpm_units(output),
    }
}

/// The packages that pnpm fetched or linked. pnpm rewrites its progress line
/// in place with a carriage return, so the scan splits on both line endings and
/// keeps the last count it finds.
fn pnpm_units(output: &str) -> u64 {
    let mut found: Option<u64> = None;
    for line in output.split(['\n', '\r']) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Progress: ") {
            let downloaded = field_after(rest, "downloaded").unwrap_or(0);
            let added = field_after(rest, "added").unwrap_or(0);
            found = Some(downloaded + added);
        } else if let Some(rest) = line.strip_prefix("Packages: ") {
            // `Packages: +3 -1`. Only the additions count as work.
            let added = rest
                .split_whitespace()
                .find_map(|word| word.strip_prefix('+'))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            found = found.or(Some(added));
        }
    }
    found.unwrap_or(0)
}

/// The number that follows `name` in a `key n, key n` list.
fn field_after(text: &str, name: &str) -> Option<u64> {
    let mut words = text.split([' ', ',']).filter(|word| !word.is_empty());
    while let Some(word) = words.next() {
        if word == name {
            return words.next()?.parse().ok();
        }
    }
    None
}

// --- Deterministic content ---------------------------------------------------

const IGNORED_SALT: u64 = 1 << 62;
const EDIT_SALT: u64 = 2 << 62;
const ADDED_SALT: u64 = 3 << 62;

/// SplitMix64. A tiny generator whose stream only depends on its state.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Thirty-two hex digits derived from `seed` and `salt`. The same pair repeats.
fn payload(seed: u64, salt: u64) -> String {
    let mut state = seed ^ salt.rotate_left(32);
    let a = splitmix64(&mut state);
    let b = splitmix64(&mut state);
    format!("{a:016x}{b:016x}")
}

/// The directory name of directory `d`.
fn dir_name(d: usize) -> String {
    format!("d{d:03}")
}

/// The path of tracked file `i`, relative to golden.
fn tracked_rel(dirs: usize, i: usize) -> String {
    format!("{}/f{i}.txt", dir_name(i % dirs.max(1)))
}

fn tracked_body(seed: u64, i: usize) -> String {
    format!("tracked file {i} {}\n", payload(seed, i as u64))
}

fn changed_body(seed: u64, i: usize) -> String {
    format!("feature edit {i} {}\n", payload(seed, EDIT_SALT + i as u64))
}

fn added_body(seed: u64, i: usize) -> String {
    format!(
        "added on feature {i} {}\n",
        payload(seed, ADDED_SALT + i as u64)
    )
}

/// `bytes` deterministic bytes for ignored file `i`. Build state is not text,
/// so the bytes are a pseudo-random stream: a compressing filesystem cannot
/// make the copy look cheaper than a real build tree.
fn ignored_body(seed: u64, i: usize, bytes: usize) -> Vec<u8> {
    let mut state = seed ^ (IGNORED_SALT + i as u64).rotate_left(32);
    let mut out = Vec::with_capacity(bytes);
    while out.len() < bytes {
        out.extend_from_slice(&splitmix64(&mut state).to_le_bytes());
    }
    out.truncate(bytes);
    out
}

/// A fixed commit time from the seed, in seconds. Tick 0 is the base commit and
/// tick 1 the feature commit.
fn commit_time(seed: u64, tick: u64) -> u64 {
    1_700_000_000 + (seed % 1_000) * 2 + tick
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_profile() -> Profile {
        Profile {
            tracked_files: 40,
            dirs: 4,
            ignored_files: 3,
            ignored_file_bytes: 512,
            changed_files: 2,
            added_files: 2,
        }
    }

    fn small() -> Recipe {
        Recipe {
            kind: Kind::Synthetic,
            profile: small_profile(),
            shape: Shape {
                crates: 1,
                functions: 1,
            },
            warm: false,
        }
    }

    /// A cargo workspace of two crates. `warm` decides whether golden holds a
    /// built `target/`.
    fn rust(warm: bool) -> Recipe {
        Recipe {
            kind: Kind::Rust,
            profile: small_profile(),
            shape: Shape {
                crates: 2,
                functions: 3,
            },
            warm,
        }
    }

    /// One seed gives one repository: the same bytes and the same commit ids.
    #[test]
    fn one_seed_gives_one_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = small();
        let a = Fixture::build(tmp.path(), "a", 7, &profile).unwrap();
        let b = Fixture::build(tmp.path(), "b", 7, &profile).unwrap();
        let c = Fixture::build(tmp.path(), "c", 8, &profile).unwrap();
        let head = |f: &Fixture| git(f.golden(), &["rev-parse", "feature"]).unwrap();
        assert_eq!(head(&a), head(&b), "one seed must give one commit id");
        assert_ne!(head(&a), head(&c), "another seed must give another id");
        assert_eq!(
            fs::read(a.golden().join("build/o1.bin")).unwrap(),
            fs::read(b.golden().join("build/o1.bin")).unwrap()
        );
    }

    #[test]
    fn the_shape_matches_the_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = small();
        let profile = recipe.profile;
        let fx = Fixture::build(tmp.path(), "shape", 7, &recipe).unwrap();
        let tracked = git(fx.golden(), &["ls-files"]).unwrap();
        // The tracked files plus .gitignore. `build/` is ignored.
        assert_eq!(tracked.lines().count(), profile.tracked_files + 1);
        assert!(!tracked.contains("build/"), "build/ must stay ignored");
        let ignored = fs::read_dir(fx.golden().join(IGNORED_DIR)).unwrap().count();
        assert_eq!(ignored, profile.ignored_files);
        assert_eq!(
            fs::metadata(fx.golden().join("build/o0.bin"))
                .unwrap()
                .len(),
            profile.ignored_file_bytes as u64
        );
        let status = git(fx.golden(), &["status", "--porcelain"]).unwrap();
        assert_eq!(status, "", "the fixture must be clean");
    }

    /// The feature branch changes the promised number of files and adds two.
    #[test]
    fn the_feature_branch_holds_the_promised_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = small();
        let fx = Fixture::build(tmp.path(), "diff", 7, &recipe).unwrap();
        let names = git(fx.golden(), &["diff", "--name-only", "main", "feature"]).unwrap();
        assert_eq!(
            names.lines().count(),
            recipe.profile.changed_files + recipe.profile.added_files
        );
    }

    /// A rust fixture is a cargo workspace whose `feature` branch sits on the
    /// same commit as `main`. A diff between them would make the first build in
    /// a klon compile the edited crate again, and M3 asks for zero.
    #[test]
    fn a_rust_fixture_is_a_workspace_with_no_branch_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = rust(false);
        let fx = Fixture::build(tmp.path(), "rust", 7, &recipe).unwrap();
        let golden = fx.golden();
        assert_eq!(fx.kind(), Kind::Rust);
        assert_eq!(Kind::Rust.ignored_dirs(), ["target"]);
        let members = fs::read_to_string(golden.join("Cargo.toml")).unwrap();
        assert!(members.contains("\"kc000\","), "found {members}");
        assert!(members.contains("\"kc001\","), "found {members}");
        let body = fs::read_to_string(golden.join("kc001/src/lib.rs")).unwrap();
        assert_eq!(
            body.lines().filter(|l| l.starts_with("pub fn f")).count(),
            recipe.shape.functions
        );
        // One seed gives one workspace, and two crates differ.
        let other = fs::read_to_string(golden.join("kc000/src/lib.rs")).unwrap();
        assert_ne!(body, other, "each crate gets its own bodies");

        let main = git(golden, &["rev-parse", "main"]).unwrap();
        let feature = git(golden, &["rev-parse", BRANCH]).unwrap();
        assert_eq!(main, feature, "an ecosystem fixture carries no branch diff");
        assert!(
            !golden.join("target").exists(),
            "a cold fixture holds no build output"
        );
        assert_eq!(
            git(golden, &["status", "--porcelain"]).unwrap(),
            "",
            "the fixture must be clean"
        );
    }

    /// A warm rust fixture holds a built `target/`, and its lock file is
    /// tracked so `git clean` in a klon cannot delete it.
    #[test]
    fn a_warm_rust_fixture_holds_a_built_target() {
        if cargo().is_none() {
            println!("skipped: cargo is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let fx = Fixture::build(tmp.path(), "warm", 7, &rust(true)).unwrap();
        let golden = fx.golden();
        assert!(golden.join("target").is_dir(), "the warm build must run");
        let tracked = git(golden, &["ls-files"]).unwrap();
        assert!(tracked.contains("Cargo.lock"), "found {tracked}");
        assert!(!tracked.contains("target/"), "target/ must stay ignored");
        assert_eq!(git(golden, &["status", "--porcelain"]).unwrap(), "");
    }

    /// The unit count reads what each tool prints.
    #[test]
    fn the_unit_count_reads_the_build_output() {
        let cargo_out = "   Compiling kc000 v0.1.0 (/x/kc000)\n\
                         \x20  Compiling kc001 v0.1.0 (/x/kc001)\n\
                         \x20   Finished `dev` profile in 0.30s\n";
        assert_eq!(units_compiled(Kind::Rust, cargo_out), 2);
        assert_eq!(units_compiled(Kind::Rust, "    Finished in 0.01s\n"), 0);

        // pnpm rewrites the progress line in place with a carriage return.
        let busy = "Packages: +1\r\nProgress: resolved 1, reused 0, downloaded 1, added 1, done\n";
        assert_eq!(units_compiled(Kind::Pnpm, busy), 2);
        let idle =
            "Lockfile is up to date\rProgress: resolved 1, reused 1, downloaded 0, added 0, done\n";
        assert_eq!(units_compiled(Kind::Pnpm, idle), 0);
        assert_eq!(units_compiled(Kind::Pnpm, "Already up to date\n"), 0);
        // A `Packages:` line alone still counts.
        assert_eq!(units_compiled(Kind::Pnpm, "Packages: +3 -1\n"), 3);
        assert_eq!(units_compiled(Kind::Synthetic, "Compiling x\n"), 0);
    }

    /// Two recipes of one profile give two fixture keys, so the runner builds
    /// one repository for each instead of reusing the wrong one.
    #[test]
    fn a_recipe_key_names_every_part_of_the_shape() {
        assert_eq!(small().key("p10k"), "p10k-synthetic-1x1-cold");
        assert_eq!(rust(true).key("p10k"), "p10k-rust-2x3-warm");
        assert_ne!(rust(true).key("p10k"), rust(false).key("p10k"));
    }

    /// The scratch directory disappears with the fixture.
    #[test]
    fn drop_removes_the_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let root = {
            let fx = Fixture::build(tmp.path(), "drop", 7, &small()).unwrap();
            fx.root().to_path_buf()
        };
        assert!(!root.exists(), "{} must be gone", root.display());
    }
}
