//! `<klon>/.klon/env`: the per-klon environment contract from handoff §5.
//!
//! `add` writes the file once. `run`, `shell`, and `stop` read it back. Each
//! line is one `KEY=value` pair with no `export`, so a person can also read the
//! file with `set -a; . .klon/env`. A value that holds a character outside a
//! small safe set is wrapped in single quotes, so a path with a space survives
//! both the shell and the reader below.

use crate::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The per-klon directory. `add` adds `/.klon/` to `<common>/info/exclude`, so
/// everything below it stays invisible to `git status`.
pub const DIR: &str = ".klon";

/// `<klon>/.klon`.
pub fn dir(klon: &Path) -> PathBuf {
    klon.join(DIR)
}

/// `<klon>/.klon/env`.
pub fn file(klon: &Path) -> PathBuf {
    dir(klon).join("env")
}

/// `<klon>/.klon/tmp`: the `TMPDIR` of every command under `run`.
pub fn tmp_dir(klon: &Path) -> PathBuf {
    dir(klon).join("tmp")
}

/// `<klon>/.klon/hooks`: the `core.hooksPath` of the klon. C22 fills it with a
/// copy of the repository hooks. C16 only creates it, so the git config value
/// always names a directory that exists.
pub fn hooks_dir(klon: &Path) -> PathBuf {
    dir(klon).join("hooks")
}

/// The `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_n`, and `GIT_CONFIG_VALUE_n` set.
/// git reads it as config on the command line, so klon can set a key for one
/// klon without a write to any config file.
#[derive(Debug, Default)]
pub struct GitConfig {
    pairs: Vec<(String, String)>,
}

impl GitConfig {
    /// Add one key and value at the end of the set.
    pub fn push(&mut self, key: &str, value: &str) {
        self.pairs.push((key.to_string(), value.to_string()));
    }

    /// The set that `vars` already carries. A pair with a missing key or value
    /// stops the read, so a truncated set never shifts the later indexes.
    pub fn from_vars(vars: &[(String, String)]) -> GitConfig {
        let map: BTreeMap<&str, &str> =
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let count: usize = map
            .get("GIT_CONFIG_COUNT")
            .and_then(|text| text.parse().ok())
            .unwrap_or(0);
        let mut config = GitConfig::default();
        for index in 0..count {
            let key = map.get(format!("GIT_CONFIG_KEY_{index}").as_str()).copied();
            let value = map
                .get(format!("GIT_CONFIG_VALUE_{index}").as_str())
                .copied();
            match (key, value) {
                (Some(key), Some(value)) => config.push(key, value),
                _ => break,
            }
        }
        config
    }

    /// The variables that carry the set.
    pub fn vars(&self) -> Vec<(String, String)> {
        let mut out = vec![("GIT_CONFIG_COUNT".to_string(), self.pairs.len().to_string())];
        for (index, (key, value)) in self.pairs.iter().enumerate() {
            out.push((format!("GIT_CONFIG_KEY_{index}"), key.clone()));
            out.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
        }
        out
    }
}

/// `vars` with `extra` appended to its `GIT_CONFIG_*` set. `run` adds
/// `gc.auto=0` this way, so a command under `run` never starts a repack that
/// writes outside the paths the fence allows (handoff §5).
pub fn with_git_config(vars: &[(String, String)], extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut config = GitConfig::from_vars(vars);
    for (key, value) in extra {
        config.push(key, value);
    }
    let mut out: Vec<(String, String)> = vars
        .iter()
        .filter(|(key, _)| !key.starts_with("GIT_CONFIG_"))
        .cloned()
        .collect();
    out.extend(config.vars());
    out
}

/// The variables of a klon at `klon` with the branch `name` and the loopback
/// address `ip`. `HOME` is absent on purpose: the per-user caches are shared.
pub fn compose(name: &str, klon: &Path, ip: &str) -> Vec<(String, String)> {
    let mut config = GitConfig::default();
    config.push("core.hooksPath", &hooks_dir(klon).to_string_lossy());
    let mut vars = vec![
        ("KLON_NAME".to_string(), name.to_string()),
        ("KLON_IP".to_string(), ip.to_string()),
        ("HOST".to_string(), ip.to_string()),
        (
            "TMPDIR".to_string(),
            tmp_dir(klon).to_string_lossy().into_owned(),
        ),
        // C17 writes the jobserver fifo path here. Until then the variable
        // exists and is empty, so a reader never has to test for its absence.
        ("KLON_JOBSERVER".to_string(), String::new()),
    ];
    vars.extend(config.vars());
    vars
}

/// Create `.klon`, `.klon/tmp`, and `.klon/hooks`, then write `.klon/env`.
/// The answer is the variable list that the file now holds.
pub fn write(klon: &Path, name: &str, ip: &str) -> Result<Vec<(String, String)>> {
    for path in [dir(klon), tmp_dir(klon), hooks_dir(klon)] {
        fs::create_dir_all(&path).map_err(Error::io(format!("create {}", path.display())))?;
    }
    let vars = compose(name, klon, ip);
    let mut text = String::new();
    for (key, value) in &vars {
        text.push_str(key);
        text.push('=');
        text.push_str(&quote(value));
        text.push('\n');
    }
    let path = file(klon);
    fs::write(&path, text).map_err(Error::io(format!("write {}", path.display())))?;
    Ok(vars)
}

/// The variables of the klon at `klon`, in file order. A klon with no file
/// comes from an older klon version; the caller reports that.
pub fn read(klon: &Path) -> Result<Vec<(String, String)>> {
    let path = file(klon);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(Error::klon(format!(
            "{} is missing; the klon predates the envelope. Remove it and add it again",
            path.display()
        ))),
        Err(err) => Err(Error::io(format!("read {}", path.display()))(err)),
    }
}

/// The value of `key` in the klon's env file, or None when either is missing.
/// `list` reads `KLON_IP` this way and never fails on an old klon.
pub fn value(klon: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(file(klon)).ok()?;
    parse(&text)
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// Split the file into pairs. A blank line and a `#` comment are skipped. A
/// line with no `=` is skipped, so a hand-edited file never panics.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            vars.push((key.trim().to_string(), unquote(value)));
        }
    }
    vars
}

/// The characters a value may hold without quotes. Every one of them means
/// itself to a shell inside a `KEY=value` word.
fn plain(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:@%+,=-".contains(&b))
}

/// `value` in a form a shell reads back unchanged.
fn quote(value: &str) -> String {
    if plain(value) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The inverse of `quote`. An unquoted value comes back unchanged.
fn unquote(value: &str) -> String {
    if !value.starts_with('\'') {
        return value.to_string();
    }
    let mut out = String::new();
    let mut chars = value.chars();
    let mut inside = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => inside = !inside,
            '\\' if !inside => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_value_comes_back_unchanged() {
        for value in [
            "",
            "feature",
            "127.0.0.2",
            "/home/a b/c",
            "it's",
            "a\tb",
            "a'b'c",
            "$(echo x)",
        ] {
            let text = format!("K={}\n", quote(value));
            let back = parse(&text);
            assert_eq!(back, vec![("K".to_string(), value.to_string())], "{text}");
        }
    }

    #[test]
    fn a_plain_value_stays_unquoted() {
        assert_eq!(quote("127.0.0.2"), "127.0.0.2");
        assert_eq!(quote(""), "");
        assert_eq!(quote("/tmp/a/.klon/tmp"), "/tmp/a/.klon/tmp");
    }

    #[test]
    fn the_git_config_set_round_trips_and_appends() {
        let vars = compose("feature", Path::new("/w/feature"), "127.0.0.2");
        assert_eq!(
            vars.iter()
                .find(|(k, _)| k == "GIT_CONFIG_COUNT")
                .map(|(_, v)| v.as_str()),
            Some("1")
        );
        let grown = with_git_config(&vars, &[("gc.auto", "0")]);
        let map: BTreeMap<&str, &str> = grown
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(map.get("GIT_CONFIG_COUNT"), Some(&"2"));
        assert_eq!(map.get("GIT_CONFIG_KEY_0"), Some(&"core.hooksPath"));
        assert_eq!(map.get("GIT_CONFIG_KEY_1"), Some(&"gc.auto"));
        assert_eq!(map.get("GIT_CONFIG_VALUE_1"), Some(&"0"));
        // The append never duplicates a variable.
        assert_eq!(
            grown
                .iter()
                .filter(|(k, _)| k == "GIT_CONFIG_COUNT")
                .count(),
            1
        );
    }

    #[test]
    fn a_truncated_git_config_set_stops_at_the_gap() {
        let vars = vec![
            ("GIT_CONFIG_COUNT".to_string(), "3".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "a.b".to_string()),
            ("GIT_CONFIG_VALUE_0".to_string(), "1".to_string()),
        ];
        let grown = with_git_config(&vars, &[("gc.auto", "0")]);
        let map: BTreeMap<&str, &str> = grown
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(map.get("GIT_CONFIG_COUNT"), Some(&"2"));
        assert_eq!(map.get("GIT_CONFIG_KEY_1"), Some(&"gc.auto"));
    }
}
