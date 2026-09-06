//! `.klon.toml`: the loader, the path template, and the command approval gate (R16, R39).

use crate::{Error, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// The path template klon uses when `.klon.toml` names none. It resolves against golden.
pub const DEFAULT_PATH_TEMPLATE: &str = "../{repo}.wt/{branch}";

/// The path convention of a host harness (research record §19). `add
/// --path-mode` sets the path template from this table; the `claude` mode
/// also renames the branch to `worktree-<name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PathMode {
    /// `../{repo}.wt/{branch}` next to golden: klon's own convention.
    #[value(name = "sibling")]
    Sibling,
    /// `{repo}/.claude/worktrees/{name}` with the branch `worktree-{name}`:
    /// the Claude Code convention. The `add` argument is the name.
    #[value(name = "claude")]
    Claude,
    /// `~/.t3/worktrees/{repo}/{branch}`: the t3 code convention.
    #[value(name = "t3")]
    T3,
    /// `$CODEX_HOME/worktrees/{branch}`, default `~/.codex`: the Codex
    /// convention. Codex itself detaches; klon keeps a branch, because its
    /// `rm`, `sync`, and `merge` commands key on branches.
    #[value(name = "codex")]
    Codex,
}

impl PathMode {
    /// The path template that the mode sets. `~` expands to `$HOME`.
    pub fn template(self) -> String {
        match self {
            PathMode::Sibling => DEFAULT_PATH_TEMPLATE.to_string(),
            PathMode::Claude => ".claude/worktrees/{name}".to_string(),
            PathMode::T3 => "~/.t3/worktrees/{repo}/{branch}".to_string(),
            PathMode::Codex => {
                let home = std::env::var("CODEX_HOME").unwrap_or_else(|_| "~/.codex".to_string());
                format!("{home}/worktrees/{{branch}}")
            }
        }
    }
}

/// What `add` does when a klon would cross the disk budget (C29).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BudgetAction {
    Refuse,
    Hibernate,
}

/// A `[warm]` or `[proof]` table.
#[derive(Debug, Default, Deserialize)]
pub struct Section {
    pub steps: Option<Vec<String>>,
}

/// The `[fence]` table. Read by the envelope chunk.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct Fence {
    pub allow: Option<Vec<String>>,
}

/// The `[copy]` table.
#[derive(Debug, Default, Deserialize)]
pub struct CopySection {
    /// Directory name to command: the command runs inside the klon instead of a copy.
    pub reinstall: Option<BTreeMap<String, String>>,
    /// The size above which a top-level ignored directory moves to the
    /// background warm process, as a byte count or a suffixed size such as
    /// `"1M"`. The default is `DEFAULT_INLINE_LIMIT`.
    pub inline_limit: Option<String>,
}

/// The `[fixup]` table: gitignore-syntax globs that the path fixup pass leaves
/// alone (R15). They resolve against the klon root.
#[derive(Debug, Default, Deserialize)]
pub struct Fixup {
    pub skip: Option<Vec<String>>,
}

/// How `gh klon merge` joins a klon's branch to base (C25, R24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ff {
    /// Write a merge commit every time. This is the default.
    NoFf,
    /// Move base forward only when the branch already holds it. A branch that
    /// needs a merge commit fails instead.
    FfOnly,
}

impl Ff {
    /// The `git merge` flag of the mode.
    pub fn flag(self) -> &'static str {
        match self {
            Ff::NoFf => "--no-ff",
            Ff::FfOnly => "--ff-only",
        }
    }

    /// The name in `.klon.toml` and in the `mode` field of `klon.merge/1`.
    pub fn name(self) -> &'static str {
        match self {
            Ff::NoFf => "no-ff",
            Ff::FfOnly => "ff-only",
        }
    }
}

/// The `[merge]` table (C25).
#[derive(Debug, Default, Deserialize)]
pub struct MergeSection {
    /// `no-ff` or `ff-only`. The `merge` flags win over this key.
    pub ff: Option<Ff>,
}

/// The `[hardlink]` table. Read by the v2 hardlink backend.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct Hardlink {
    pub paths: Option<Vec<String>>,
}

/// The `[netns]` table. Read by the netns part of `run` and `shell`.
#[derive(Debug, Default, Deserialize)]
pub struct Netns {
    /// TCP ports pasta maps from the klon's loopback address into the namespace.
    pub ports: Option<Vec<u16>>,
}

/// The whole `.klon.toml`. Every key is optional (handoff §3).
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// The golden branch, read by the radar and the branch forms in `add`
    /// and `rm --merged`.
    pub base: Option<String>,
    /// The path template for new klons.
    pub path: Option<String>,
    /// Above this size `add` applies `disk_budget_action` (C29). A number with
    /// an optional K, M, G, or T, for example "40G".
    pub disk_budget: Option<String>,
    /// `refuse` or `hibernate` (C29).
    pub disk_budget_action: Option<BudgetAction>,
    /// Hot-spare pool depth (C9). `0` disables the spare; v0 knows one depth.
    pub spare: Option<u32>,
    /// Steps `gh klon up` runs in golden. Needs approval.
    pub warm: Option<Section>,
    /// Steps `gh klon check` runs in v0.2. Needs approval.
    pub proof: Option<Section>,
    /// Extra writable paths under `run`. Read by the envelope chunk.
    #[allow(dead_code)]
    pub fence: Option<Fence>,
    /// Per-directory strategy of the copy backend. `reinstall` needs approval.
    pub copy: Option<CopySection>,
    /// Paths the fixup pass must not rewrite.
    pub fixup: Option<Fixup>,
    /// How `merge` joins a branch to base (C25).
    pub merge: Option<MergeSection>,
    /// Paths to hardlink in v2. Read by the hardlink backend.
    #[allow(dead_code)]
    pub hardlink: Option<Hardlink>,
    /// The `[netns]` table: the TCP ports of `run --netns`.
    pub netns: Option<Netns>,
    /// The SHA-256 of the raw file bytes. `None` when the file is absent.
    hash: Option<String>,
}

/// The top-level keys klon knows. Anything else draws one warning line.
const KNOWN_TOP: &[&str] = &[
    "base",
    "path",
    "disk_budget",
    "disk_budget_action",
    "spare",
    "warm",
    "proof",
    "fence",
    "copy",
    "fixup",
    "merge",
    "hardlink",
    "netns",
];

/// The keys klon knows inside each table, as `(table, keys)`.
const KNOWN_TABLES: &[(&str, &[&str])] = &[
    ("warm", &["steps"]),
    ("proof", &["steps"]),
    ("fence", &["allow"]),
    ("copy", &["reinstall", "inline_limit"]),
    ("fixup", &["skip"]),
    ("merge", &["ff"]),
    ("hardlink", &["paths"]),
    ("netns", &["ports"]),
];

/// Read and parse `<golden>/.klon.toml`. A missing file is an empty config.
pub fn load(golden: &Path) -> Result<Config> {
    let file = golden.join(".klon.toml");
    let bytes = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => return Err(Error::io(format!("read {}", file.display()))(err)),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::klon(format!("{} must be valid UTF-8", file.display())))?;
    let value: toml::Value =
        toml::from_str(text).map_err(|err| Error::klon(format!("{}: {err}", file.display())))?;
    warn_unknown_keys(&file, &value);
    let mut config = Config::deserialize(value)
        .map_err(|err| Error::klon(format!("{}: {err}", file.display())))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    config.hash = Some(hex(&hasher.finalize()));
    Ok(config)
}

/// One warning line for every key klon does not know. An unknown key never fails the load.
fn warn_unknown_keys(file: &Path, value: &toml::Value) {
    let mut unknown: Vec<String> = Vec::new();
    if let Some(table) = value.as_table() {
        for key in table.keys() {
            if !KNOWN_TOP.contains(&key.as_str()) {
                unknown.push(key.clone());
            }
        }
        for (table_name, known) in KNOWN_TABLES {
            if let Some(inner) = table.get(*table_name).and_then(|v| v.as_table()) {
                for key in inner.keys() {
                    if !known.contains(&key.as_str()) {
                        unknown.push(format!("{table_name}.{key}"));
                    }
                }
            }
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        eprintln!(
            "klon: {}: ignoring unknown keys: {}",
            file.display(),
            unknown.join(", ")
        );
    }
}

impl Config {
    /// The commands this file asks klon to run, as `(key, command)` pairs in file order.
    fn commands(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(steps) = self.warm.as_ref().and_then(|w| w.steps.as_ref()) {
            for step in steps {
                out.push(("warm.steps".to_string(), step.clone()));
            }
        }
        if let Some(steps) = self.proof.as_ref().and_then(|p| p.steps.as_ref()) {
            for step in steps {
                out.push(("proof.steps".to_string(), step.clone()));
            }
        }
        if let Some(reinstall) = self.copy.as_ref().and_then(|c| c.reinstall.as_ref()) {
            for (dir, command) in reinstall {
                out.push((format!("copy.reinstall[{dir}]"), command.clone()));
            }
        }
        out
    }

    /// The approval gate (R16). Before klon uses a command under `keys`, the user must
    /// approve this file's content hash once. `--yes` approves without a prompt.
    /// A run without a terminal and without `--yes` refuses before anything runs.
    pub fn ensure_approved(&self, yes: bool, keys: &[&str]) -> Result<()> {
        let commands: Vec<_> = self
            .commands()
            .into_iter()
            .filter(|(key, _)| keys.iter().any(|name| key.starts_with(name)))
            .collect();
        if commands.is_empty() {
            return Ok(());
        }
        let hash = self
            .hash
            .as_deref()
            .ok_or_else(|| Error::klon("internal: no content hash for a config with commands"))?;
        let mut approvals = read_approvals();
        if approvals.approval.iter().any(|a| a.hash == hash) {
            return Ok(());
        }
        eprintln!("klon: .klon.toml asks to run:");
        for (key, command) in &commands {
            eprintln!("  {key}: {command}");
        }
        let approved = if yes {
            true
        } else if std::io::stdin().is_terminal() {
            eprint!("Approve? [y/N] ");
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(Error::io("read the approval answer"))?;
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        } else {
            false
        };
        if !approved {
            return Err(Error::klon(
                "needs approval: .klon.toml holds repository commands; pass --yes or approve at the prompt",
            ));
        }
        approvals.approval.push(Approval {
            hash: hash.to_string(),
        });
        write_approvals(&approvals)
    }

    /// Resolve the `path` template for `branch`. A relative template resolves against golden.
    /// Klon refuses a template that resolves to `/`, the home directory, or the repository.
    pub fn resolve_path(&self, golden: &Path, branch: &str) -> Result<PathBuf> {
        let template = self.path.as_deref().unwrap_or(DEFAULT_PATH_TEMPLATE);
        resolve_filled(golden, template, branch, branch)
    }
}

/// Resolve a path template against golden. The modes call this with their own
/// template; `{name}` is the raw `add` argument and equals `branch` outside
/// the `claude` mode.
pub fn resolve_filled(golden: &Path, template: &str, branch: &str, name: &str) -> Result<PathBuf> {
    let filled = expand_tilde(&fill_template(template, golden, branch, name)?)?;
    let filled = PathBuf::from(&filled);
    let joined = if filled.is_absolute() {
        filled
    } else {
        golden.join(filled)
    };
    let resolved = crate::paths::absolute(&joined)?;
    if resolved == Path::new("/") {
        return Err(refused(template, "/"));
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && resolved == crate::paths::absolute(Path::new(&home))? {
            return Err(refused(template, "the home directory"));
        }
    }
    if resolved == golden {
        return Err(refused(template, "the repository root"));
    }
    Ok(resolved)
}

fn refused(template: &str, what: &str) -> Error {
    Error::klon(format!(
        "refuses path template {template}: it resolves to {what}"
    ))
}

/// Expand a leading `~` or `~/` to `$HOME`. The `t3` and `codex` templates use it.
fn expand_tilde(filled: &str) -> Result<String> {
    if !(filled == "~" || filled.starts_with("~/")) {
        return Ok(filled.to_string());
    }
    let home = std::env::var("HOME")
        .map_err(|_| Error::klon("the path template uses ~ but HOME is not set"))?;
    if home.is_empty() {
        return Err(Error::klon("the path template uses ~ but HOME is empty"));
    }
    Ok(format!("{home}{}", &filled[1..]))
}

/// Replace the placeholders klon supports: `{repo}`, `{branch}`, and `{name}`.
fn fill_template(template: &str, golden: &Path, branch: &str, name: &str) -> Result<String> {
    let repo = golden
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let mut filled = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        filled.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find('}').ok_or_else(|| {
            Error::klon(format!("path template has an unclosed '{{': {template}"))
        })?;
        match &tail[1..end] {
            "repo" => filled.push_str(&repo),
            "branch" => filled.push_str(branch),
            "name" => filled.push_str(name),
            other => {
                return Err(Error::klon(format!(
                    "path template uses unknown placeholder {{{other}}}; klon supports {{repo}}, {{branch}}, and {{name}}"
                )))
            }
        }
        rest = &tail[end + 1..];
    }
    filled.push_str(rest);
    Ok(filled)
}

/// One approved content hash.
#[derive(Debug, Deserialize)]
struct Approval {
    hash: String,
}

/// The `approvals.toml` store.
#[derive(Debug, Default, Deserialize)]
struct Approvals {
    #[serde(default)]
    approval: Vec<Approval>,
}

/// `<config home>/klon/approvals.toml`. `KLON_CONFIG_HOME` overrides the home so tests
/// stay out of the user's directory.
fn approvals_path() -> PathBuf {
    config_home().join("klon").join("approvals.toml")
}

/// The klon config home: `KLON_CONFIG_HOME`, else `XDG_CONFIG_HOME`, else `~/.config`.
fn config_home() -> PathBuf {
    for name in ["KLON_CONFIG_HOME", "XDG_CONFIG_HOME"] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
}

/// Read the approval store. A missing or unreadable file holds no approvals.
fn read_approvals() -> Approvals {
    toml::from_str(&fs::read_to_string(approvals_path()).unwrap_or_default()).unwrap_or_default()
}

fn write_approvals(approvals: &Approvals) -> Result<()> {
    let file = approvals_path();
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(Error::io(format!("create {}", parent.display())))?;
    }
    let mut text = String::from("# Approved .klon.toml content hashes. Written by gh-klon.\n");
    for approval in &approvals.approval {
        text.push_str(&format!("[[approval]]\nhash = \"{}\"\n", approval.hash));
    }
    fs::write(&file, text).map_err(Error::io(format!("write {}", file.display())))
}

/// The default `[copy] inline_limit`: 64 MiB. A top-level ignored directory
/// above it goes to the background warm process, so `add` returns before the
/// big copy finishes (R36).
pub const DEFAULT_INLINE_LIMIT: u64 = 64 * 1024 * 1024;

impl CopySection {
    /// The `inline_limit` in bytes. An unreadable value draws one warning line
    /// and takes the default, because a size is a preference, not a rule.
    pub fn inline_limit(&self) -> u64 {
        let Some(text) = self.inline_limit.as_deref() else {
            return DEFAULT_INLINE_LIMIT;
        };
        match parse_size(text) {
            Some(bytes) => bytes,
            None => {
                eprintln!(
                    "klon: .klon.toml: [copy] inline_limit {text} is not a size; using {DEFAULT_INLINE_LIMIT} bytes"
                );
                DEFAULT_INLINE_LIMIT
            }
        }
    }
}

/// Read `12`, `64K`, `1M`, or `2G` as a byte count. The suffix is binary, as
/// every other size in klon is, and the case does not matter. A trailing `B`
/// is accepted, so `1MB` and `1M` agree.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let body = lower.strip_suffix('b').unwrap_or(&lower);
    let (digits, scale) = match body.strip_suffix(['k', 'm', 'g', 't']) {
        Some(digits) => {
            let unit = body.as_bytes()[body.len() - 1];
            let power = match unit {
                b'k' => 1,
                b'm' => 2,
                b'g' => 3,
                _ => 4,
            };
            (digits, 1024u64.checked_pow(power)?)
        }
        None => (body, 1),
    };
    let digits = digits.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(scale)
}

/// Lower-case hexadecimal. The config hash and the radar cache key share it.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
