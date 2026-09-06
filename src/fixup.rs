//! Path fixup (R15, handoff §9): after the checkout, rewrite golden's absolute
//! path to the klon's path inside the ignored directories.
//!
//! A build tree carries absolute paths. `node_modules/.modules.yaml` names the
//! pnpm store, `obj/*.nuget.g.props` names the NuGet cache, `pyvenv.cfg` names
//! the interpreter, and `CMakeCache.txt` names the build directory. A clone
//! that keeps those paths points the klon back at golden. klon therefore runs
//! one fixed-string search over the ignored entries and rewrites the hits.
//!
//! The pass is generic, so it needs rails. klon rewrites a file only when the
//! file is
//!
//! | Rail | Reason |
//! |---|---|
//! | at most 1 MB | a big artifact is a database or an image, not a config |
//! | free of a NUL byte | `grep-searcher` stops on binary content |
//! | valid UTF-8 | a byte splice inside a multi-byte sequence would corrupt it |
//! | outside the skip list | a known binary extension, or a `[fixup] skip` glob |
//!
//! klon also rewrites a symlink whose target points into golden (handoff §12
//! Q8), and deletes `.next/cache`, `.ninja_log`, and `.ninja_deps`, which hold
//! absolute paths that no rewrite makes valid (handoff §9).
//!
//! Only ignored entries are walked, so the pass never touches a tracked file
//! and `core.checkStat=minimal` stays valid. Each rewritten file keeps its mode
//! and its mtime, because cargo and make compare mtimes.

use crate::config::Config;
use crate::{git, Error, Result};
use grep_matcher::{Match, Matcher, NoCaptures, NoError};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The biggest file klon rewrites (R15). A bigger one is an artifact, not a
/// configuration file.
const MAX_BYTES: u64 = 1024 * 1024;

/// Extensions that the rewrite never opens. The list is the one from handoff
/// §2 plus the Rust and archive artifacts that a `target/` directory holds.
/// Any extension that starts with `sqlite` is skipped too, which covers
/// `.sqlite3`, `.sqlite-wal`, and `.sqlite-shm`.
const SKIP_EXTENSIONS: &[&str] = &[
    "a", "bin", "class", "db", "dylib", "gz", "jpeg", "jpg", "o", "pack", "png", "pyc", "rlib",
    "rmeta", "so", "tar", "wasm", "zip",
];

/// The name of the rewrite log inside the klon.
const LOG: &str = "fixup.log";

/// The suffix of the temporary file that a rewrite renames over the original.
const TEMP_SUFFIX: &str = ".klon-fixup.tmp";

/// What one pass changed. `add` prints nothing; `.klon/fixup.log` is the record.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Files whose content klon rewrote.
    pub files: u64,
    /// Symlinks whose target klon rewrote.
    pub symlinks: u64,
    /// Entries from the delete list that klon removed.
    pub deleted: u64,
}

/// Rewrite golden's path inside the ignored entries of `klon`.
///
/// `add` calls this after `git clean`, so every remaining untracked entry is an
/// ignored one. A destination that equals golden is a no-op.
pub fn run(golden: &Path, klon: &Path, config: &Config) -> Result<Summary> {
    if golden == klon {
        return Ok(Summary::default());
    }
    let pass = Pass::new(golden, klon, config)?;
    let mut summary = Summary::default();
    let mut log = Vec::new();
    for root in ignored_roots(klon)? {
        pass.walk(&root, &mut summary, &mut log)?;
    }
    if !log.is_empty() {
        write_log(klon, &log)?;
    }
    Ok(summary)
}

/// One configured pass over one klon.
struct Pass {
    /// Golden's absolute path, the search needle.
    needle: String,
    /// The klon's absolute path, the replacement.
    replacement: String,
    klon: PathBuf,
    /// The `[fixup] skip` globs, compiled against the klon root.
    skip: Option<ignore::gitignore::Gitignore>,
    /// One reusable searcher. The walk is single threaded, so a cell is enough.
    searcher: RefCell<Searcher>,
}

impl Pass {
    fn new(golden: &Path, klon: &Path, config: &Config) -> Result<Pass> {
        let needle = utf8(golden)?.to_string();
        let replacement = utf8(klon)?.to_string();
        let mut builder = ignore::gitignore::GitignoreBuilder::new(klon);
        if let Some(globs) = config.fixup.as_ref().and_then(|f| f.skip.as_ref()) {
            for glob in globs {
                builder
                    .add_line(None, glob)
                    .map_err(|err| Error::klon(format!("[fixup] skip: {glob}: {err}")))?;
            }
        }
        let skip = builder
            .build()
            .map_err(|err| Error::klon(format!("[fixup] skip: {err}")))?;
        Ok(Pass {
            needle,
            replacement,
            klon: klon.to_path_buf(),
            skip: Some(skip),
            // `quit` stops at the first NUL byte, so a binary file never
            // reaches the rewrite and never reports a hit.
            searcher: RefCell::new(
                SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(0))
                    .line_number(false)
                    .build(),
            ),
        })
    }

    /// Walk one ignored entry and fix every file, symlink, and delete target
    /// below it. `root` is absolute.
    fn walk(&self, root: &Path, summary: &mut Summary, log: &mut Vec<String>) -> Result<()> {
        let doomed: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let collect = Arc::clone(&doomed);
        let walk = WalkBuilder::new(root)
            // The entries are already known to be ignored, so every gitignore
            // filter is off and nothing below the root is hidden from the walk.
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .filter_entry(move |entry| {
                if on_delete_list(entry.path()) {
                    collect
                        .lock()
                        .expect("fixup lock")
                        .push(entry.path().to_path_buf());
                    return false;
                }
                true
            })
            .build();
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                // A vanished file is not a reason to stop: another process may
                // own the ignored tree. Report it and continue.
                Err(err) => {
                    eprintln!("klon: fixup: {err}");
                    continue;
                }
            };
            let path = entry.path();
            if path.to_string_lossy().ends_with(TEMP_SUFFIX) {
                continue;
            }
            let Some(kind) = entry.file_type() else {
                continue;
            };
            let outcome = if kind.is_symlink() {
                self.fix_symlink(path)
            } else if kind.is_file() {
                self.fix_file(path)
            } else {
                Ok(None)
            };
            match outcome {
                Ok(None) => {}
                Ok(Some(line)) => {
                    if kind.is_symlink() {
                        summary.symlinks += 1;
                    } else {
                        summary.files += 1;
                    }
                    log.push(line);
                }
                Err(err) => eprintln!("klon: fixup: {err}"),
            }
        }
        let doomed = std::mem::take(&mut *doomed.lock().expect("fixup lock"));
        for path in doomed {
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match removed {
                Ok(()) => {
                    summary.deleted += 1;
                    log.push(format!("{} deleted", self.relative(&path)));
                }
                Err(err) => eprintln!("klon: fixup: delete {}: {err}", path.display()),
            }
        }
        Ok(())
    }

    /// Rewrite one file when it passes every rail. The answer is the log line.
    fn fix_file(&self, path: &Path) -> Result<Option<String>> {
        let relative = self.relative(path);
        if self.skipped(&relative) || skipped_extension(path) {
            return Ok(None);
        }
        let meta = fs::symlink_metadata(path).map_err(Error::io(format!("stat {relative}")))?;
        if meta.len() > MAX_BYTES {
            return Ok(None);
        }
        if !self.holds_needle(path)? {
            return Ok(None);
        }
        let bytes = fs::read(path).map_err(Error::io(format!("read {relative}")))?;
        // The searcher already refused a NUL byte. This refuses every other
        // byte sequence that is not text, so the splice cannot cut a codepoint.
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(None);
        };
        let count = text.matches(&self.needle).count();
        if count == 0 {
            return Ok(None);
        }
        let rewritten = text.replace(&self.needle, &self.replacement);
        replace_content(path, rewritten.as_bytes(), &meta)?;
        Ok(Some(format!("{relative} {count}")))
    }

    /// True when `grep-searcher` found the needle in a file it read as text.
    fn holds_needle(&self, path: &Path) -> Result<bool> {
        let mut found = Found::default();
        let mut searcher = self.searcher.borrow_mut();
        searcher
            .search_path(Fixed(self.needle.as_bytes()), path, &mut found)
            .map_err(Error::io(format!("search {}", self.relative(path))))?;
        Ok(found.hit && !found.binary)
    }

    /// Point a symlink that resolves into golden at the same place in the klon.
    fn fix_symlink(&self, path: &Path) -> Result<Option<String>> {
        let relative = self.relative(path);
        if self.skipped(&relative) {
            return Ok(None);
        }
        let target = fs::read_link(path).map_err(Error::io(format!("readlink {relative}")))?;
        let Some(text) = target.to_str() else {
            return Ok(None);
        };
        // Only a target that starts at golden's root moves. A relative target
        // already follows the klon.
        let rest = match text.strip_prefix(&self.needle) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
            _ => return Ok(None),
        };
        let meta = fs::symlink_metadata(path).map_err(Error::io(format!("stat {relative}")))?;
        let new = format!("{}{rest}", self.replacement);
        fs::remove_file(path).map_err(Error::io(format!("replace {relative}")))?;
        std::os::unix::fs::symlink(&new, path).map_err(Error::io(format!("relink {relative}")))?;
        crate::backend::set_symlink_times(path, &meta)?;
        Ok(Some(format!("{relative} symlink")))
    }

    /// True when a `[fixup] skip` glob names this path.
    fn skipped(&self, relative: &str) -> bool {
        self.skip
            .as_ref()
            .is_some_and(|s| s.matched_path_or_any_parents(relative, false).is_ignore())
    }

    /// The path of `entry` relative to the klon, for the log and the messages.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.klon)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

/// The top-level ignored entries of `klon`. `--directory` collapses a fully
/// ignored directory into one name, so the walk starts at `target/` instead of
/// at every file below it.
fn ignored_roots(klon: &Path) -> Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let (_, out) = git::run_input(
        klon,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
        b"",
        &[0],
    )?;
    let mut roots = Vec::new();
    for name in out.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let name = Path::new(std::ffi::OsStr::from_bytes(name));
        // klon's own state, including the log this pass writes.
        if name.starts_with(".klon") {
            continue;
        }
        roots.push(klon.join(name));
    }
    Ok(roots)
}

/// Append the log lines to `<klon>/.klon/fixup.log`.
fn write_log(klon: &Path, lines: &[String]) -> Result<()> {
    use std::io::Write;
    let dir = klon.join(".klon");
    fs::create_dir_all(&dir).map_err(Error::io(format!("create {}", dir.display())))?;
    let path = dir.join(LOG);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(Error::io(format!("open {}", path.display())))?;
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    file.write_all(text.as_bytes())
        .map_err(Error::io(format!("write {}", path.display())))
}

/// Write `bytes` over `path` through a temporary file and one rename, then
/// restore the mode and the mtime of `meta`.
///
/// The rename matters: a build artifact can be a hardlink into a shared store,
/// as every file that pnpm links out of its store is. An in-place write would
/// change that store for every project on the host. The rename gives the klon
/// its own inode and leaves the shared one alone (R4).
fn replace_content(path: &Path, bytes: &[u8], meta: &fs::Metadata) -> Result<()> {
    let mut name = path.as_os_str().to_os_string();
    name.push(TEMP_SUFFIX);
    let temp = PathBuf::from(name);
    let write = || -> io::Result<()> {
        fs::write(&temp, bytes)?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(meta.permissions().mode()))
    };
    if let Err(err) = write() {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(format!("write {}", temp.display()))(err));
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(format!("rewrite {}", path.display()))(err));
    }
    crate::backend::set_times(path, meta)
}

/// True when `path` is `.next/cache`, `.ninja_log`, or `.ninja_deps`.
/// Those three hold absolute paths that no rewrite makes valid (handoff §9).
fn on_delete_list(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    if name == ".ninja_log" || name == ".ninja_deps" {
        return true;
    }
    name == "cache" && path.parent().and_then(Path::file_name) == Some(".next".as_ref())
}

/// True when the extension of `path` names a known binary format.
fn skipped_extension(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = extension.to_ascii_lowercase();
    lower.starts_with("sqlite") || SKIP_EXTENSIONS.contains(&lower.as_str())
}

/// A path as UTF-8. `add` already refuses a repository path that is not.
fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::klon(format!("path is not valid UTF-8: {}", path.display())))
}

// --- The fixed-string search ---------------------------------------------------

/// A `grep-searcher` matcher for one fixed string. klon searches for one
/// absolute path, so a regular expression engine would only add a dependency.
#[derive(Clone, Copy)]
struct Fixed<'a>(&'a [u8]);

impl Matcher for Fixed<'_> {
    type Captures = NoCaptures;
    type Error = NoError;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> std::result::Result<Option<Match>, Self::Error> {
        if at > haystack.len() {
            return Ok(None);
        }
        if self.0.is_empty() {
            return Ok(Some(Match::new(at, at)));
        }
        let window = &haystack[at..];
        let found = window
            .windows(self.0.len())
            .position(|slice| slice == self.0)
            .map(|start| Match::new(at + start, at + start + self.0.len()));
        Ok(found)
    }

    fn new_captures(&self) -> std::result::Result<NoCaptures, Self::Error> {
        Ok(NoCaptures::new())
    }
}

/// Whether the search saw the needle, and whether it stopped on binary content.
#[derive(Default)]
struct Found {
    hit: bool,
    binary: bool,
}

impl Sink for &mut Found {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        _matched: &SinkMatch<'_>,
    ) -> std::result::Result<bool, io::Error> {
        self.hit = true;
        // One hit answers the question, so the search stops here.
        Ok(false)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _offset: u64,
    ) -> std::result::Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delete_list_holds_three_names() {
        assert!(on_delete_list(Path::new("/k/.ninja_log")));
        assert!(on_delete_list(Path::new("/k/deep/.ninja_deps")));
        assert!(on_delete_list(Path::new("/k/.next/cache")));
        assert!(on_delete_list(Path::new("/k/web/.next/cache")));
        // A `cache` directory outside `.next` stays.
        assert!(!on_delete_list(Path::new("/k/target/cache")));
        assert!(!on_delete_list(Path::new("/k/.next/server")));
    }

    #[test]
    fn a_known_binary_extension_is_skipped() {
        for name in [
            "a.sqlite",
            "a.sqlite3",
            "a.sqlite-wal",
            "a.db",
            "a.o",
            "a.rlib",
            "a.so",
            "a.PNG",
            "a.zip",
        ] {
            assert!(skipped_extension(Path::new(name)), "{name} must be skipped");
        }
        for name in ["a.json", "a.yaml", "a.d", "a.toml", "plain", "a.cfg"] {
            assert!(!skipped_extension(Path::new(name)), "{name} must be read");
        }
    }

    #[test]
    fn the_fixed_matcher_finds_every_position() {
        let matcher = Fixed(b"ab");
        assert_eq!(
            matcher.find_at(b"xxabyy", 0).unwrap(),
            Some(Match::new(2, 4))
        );
        assert_eq!(matcher.find_at(b"xxabyy", 3).unwrap(), None);
        assert_eq!(matcher.find_at(b"xx", 0).unwrap(), None);
        // An offset past the end answers "no match" instead of a panic.
        assert_eq!(matcher.find_at(b"xx", 9).unwrap(), None);
    }
}
