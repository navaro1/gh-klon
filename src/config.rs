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

/// What `add` does when a klon would cross the disk budget. Read by the budget chunk.
#[allow(dead_code)]
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
}

/// The `[fixup]` table. Read by the fixup chunk.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct Fixup {
    pub skip: Option<Vec<String>>,
}

/// The `[hardlink]` table. Read by the v2 hardlink backend.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct Hardlink {
    pub paths: Option<Vec<String>>,
}

/// The whole `.klon.toml`. Every key is optional (handoff §3).
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// The golden branch. The radar measures every klon against it.
    pub base: Option<String>,
    /// The path template for new klons.
    pub path: Option<String>,
    /// Above this size `add` applies `disk_budget_action`. Read by the budget chunk.
    #[allow(dead_code)]
    pub disk_budget: Option<String>,
    /// `refuse` or `hibernate`. Read by the budget chunk.
    #[allow(dead_code)]
    pub disk_budget_action: Option<BudgetAction>,
    /// Hot-spare pool depth. Read by the spare chunk.
    #[allow(dead_code)]
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
    /// Paths the fixup pass must not rewrite. Read by the fixup chunk.
    #[allow(dead_code)]
    pub fixup: Option<Fixup>,
    /// Paths to hardlink in v2. Read by the hardlink backend.
    #[allow(dead_code)]
    pub hardlink: Option<Hardlink>,
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
    "hardlink",
];

/// The keys klon knows inside each table, as `(table, keys)`.
const KNOWN_TABLES: &[(&str, &[&str])] = &[
    ("warm", &["steps"]),
    ("proof", &["steps"]),
    ("fence", &["allow"]),
    ("copy", &["reinstall"]),
    ("fixup", &["skip"]),
    ("hardlink", &["paths"]),
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
        let filled = fill_template(template, golden, branch)?;
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
}

fn refused(template: &str, what: &str) -> Error {
    Error::klon(format!(
        "refuses path template {template}: it resolves to {what}"
    ))
}

/// Replace the placeholders klon supports: `{repo}` and `{branch}`.
fn fill_template(template: &str, golden: &Path, branch: &str) -> Result<String> {
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
            name => {
                return Err(Error::klon(format!(
                    "path template uses unknown placeholder {{{name}}}; klon supports {{repo}} and {{branch}}"
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

/// Lower-case hexadecimal. The config hash and the radar cache key share it.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
