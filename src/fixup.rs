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
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// Workers on the walk. The pass reads every byte of the build tree, so it
/// takes the measured optimum of the clone walk (handoff §4 "Backends").
const WORKERS: usize = 4;

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
    let roots = ignored_roots(klon)?;
    let Some((first, rest)) = roots.split_first() else {
        return Ok(Summary::default());
    };
    let pass = Pass::new(golden, klon, config)?;
    let found = Shared::default();
    pass.walk(first, rest, &found);

    let mut summary = Summary::default();
    let mut log = found.changes.into_inner().expect("fixup lock");
    for path in found.doomed.into_inner().expect("fixup lock") {
        let removed = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match removed {
            Ok(()) => {
                summary.deleted += 1;
                log.push(Change::deleted(pass.relative(&path)));
            }
            Err(err) => eprintln!("klon: fixup: delete {}: {err}", path.display()),
        }
    }
    for change in &log {
        match change.kind {
            Kind::File => summary.files += 1,
            Kind::Symlink => summary.symlinks += 1,
            Kind::Deleted => {}
        }
    }
    if !log.is_empty() {
        // The workers finish in no fixed order, so the log is sorted. A stable
        // file makes two runs on one tree comparable.
        log.sort_by(|a, b| a.line.cmp(&b.line));
        write_log(klon, &log)?;
    }
    Ok(summary)
}

/// What the workers found, and what the caller must still delete.
#[derive(Default)]
struct Shared {
    changes: Mutex<Vec<Change>>,
    doomed: Mutex<Vec<PathBuf>>,
}

/// One line of the log, with the kind that the summary counts.
struct Change {
    kind: Kind,
    line: String,
}

enum Kind {
    File,
    Symlink,
    Deleted,
}

impl Change {
    fn file(relative: &str, count: usize) -> Change {
        Change {
            kind: Kind::File,
            line: format!("{relative} {count}"),
        }
    }

    fn symlink(relative: &str) -> Change {
        Change {
            kind: Kind::Symlink,
            line: format!("{relative} symlink"),
        }
    }

    fn deleted(relative: String) -> Change {
        Change {
            kind: Kind::Deleted,
            line: format!("{relative} deleted"),
        }
    }
}

/// One configured pass over one klon.
struct Pass {
    /// Golden's absolute path, the search needle.
    needle: String,
    /// The klon's absolute path, the replacement.
    replacement: String,
    klon: PathBuf,
    /// The `[fixup] skip` globs, compiled against the klon root.
    skip: ignore::gitignore::Gitignore,
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
            skip,
        })
    }

    /// Walk every ignored entry with `WORKERS` threads and fix what it finds.
    /// The pass reads every byte of the build tree, so it sits where the clone
    /// walk sits: on four workers (handoff §4).
    fn walk(&self, first: &Path, rest: &[PathBuf], found: &Shared) {
        let mut builder = WalkBuilder::new(first);
        for root in rest {
            builder.add(root);
        }
        builder
            // The roots are already known to be ignored, so every gitignore
            // filter is off and nothing below them is hidden from the walk.
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .threads(WORKERS);
        builder.build_parallel().run(|| {
            // One searcher per worker: a `Searcher` holds a line buffer and is
            // not shareable.
            let mut searcher = SearcherBuilder::new()
                // `quit` stops at the first NUL byte, so a binary file never
                // reaches the rewrite and never reports a hit.
                .binary_detection(BinaryDetection::quit(0))
                .line_number(false)
                .build();
            Box::new(move |entry| self.visit(entry, &mut searcher, found))
        });
    }

    /// Handle one walk entry on one worker.
    fn visit(
        &self,
        entry: std::result::Result<ignore::DirEntry, ignore::Error>,
        searcher: &mut Searcher,
        found: &Shared,
    ) -> ignore::WalkState {
        let entry = match entry {
            Ok(entry) => entry,
            // A vanished file is not a reason to stop: another process may own
            // the ignored tree. Report it and continue.
            Err(err) => {
                eprintln!("klon: fixup: {err}");
                return ignore::WalkState::Continue;
            }
        };
        let path = entry.path();
        if on_delete_list(path) {
            found
                .doomed
                .lock()
                .expect("fixup lock")
                .push(path.to_path_buf());
            // The caller deletes it, so no worker may descend into it.
            return ignore::WalkState::Skip;
        }
        if path
            .as_os_str()
            .as_encoded_bytes()
            .ends_with(TEMP_SUFFIX.as_bytes())
        {
            return ignore::WalkState::Continue;
        }
        let Some(kind) = entry.file_type() else {
            return ignore::WalkState::Continue;
        };
        let outcome = if kind.is_symlink() {
            self.fix_symlink(path)
        } else if kind.is_file() {
            // The walk already stat-ed the entry; a second stat per file costs
            // one syscall for nothing on a big build tree.
            match entry.metadata() {
                Ok(meta) => self.fix_file(path, &meta, searcher),
                Err(err) => Err(Error::klon(format!("stat {}: {err}", self.relative(path)))),
            }
        } else {
            Ok(None)
        };
        match outcome {
            Ok(None) => {}
            Ok(Some(change)) => found.changes.lock().expect("fixup lock").push(change),
            Err(err) => eprintln!("klon: fixup: {err}"),
        }
        ignore::WalkState::Continue
    }

    /// Rewrite one file when it passes every rail. The answer is the log line.
    ///
    /// The rails run from the cheapest to the dearest: the name, then the size
    /// from the walk's own stat, then one read, then the search. A build tree
    /// holds tens of thousands of files, so the order decides the cost.
    fn fix_file(
        &self,
        path: &Path,
        meta: &fs::Metadata,
        searcher: &mut Searcher,
    ) -> Result<Option<Change>> {
        if skipped_extension(path) || meta.len() > MAX_BYTES {
            return Ok(None);
        }
        let relative = self.relative(path);
        if self.skipped(&relative) {
            return Ok(None);
        }
        let bytes = fs::read(path).map_err(Error::io(format!("read {relative}")))?;
        // Almost every file in a build tree never names golden. One `memmem`
        // scan settles those, and only a candidate pays for the text test.
        if memchr::memmem::find(&bytes, self.needle.as_bytes()).is_none() {
            return Ok(None);
        }
        if !holds_needle(searcher, self.needle.as_bytes(), &bytes, &relative)? {
            return Ok(None);
        }
        // The searcher already refused a NUL byte. This refuses every other
        // byte sequence that is not text, so the splice cannot cut a codepoint.
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(None);
        };
        let (rewritten, count) = replace_paths(&text, &self.needle, &self.replacement);
        if count == 0 {
            return Ok(None);
        }
        replace_content(path, rewritten.as_bytes(), meta)?;
        Ok(Some(Change::file(&relative, count)))
    }

    /// Point a symlink that resolves into golden at the same place in the klon.
    fn fix_symlink(&self, path: &Path) -> Result<Option<Change>> {
        let relative = self.relative(path);
        if self.skipped(&relative) {
            return Ok(None);
        }
        let target = fs::read_link(path).map_err(Error::io(format!("readlink {relative}")))?;
        let Some(text) = target.to_str() else {
            return Ok(None);
        };
        // Only a target that starts at golden's root moves. A relative target
        // already follows the klon, and a sibling such as `<golden>-docs` is
        // another tree.
        let rest = match text.strip_prefix(&self.needle) {
            Some(rest) if !continues_name(rest) => rest,
            _ => return Ok(None),
        };
        let meta = fs::symlink_metadata(path).map_err(Error::io(format!("stat {relative}")))?;
        let new = format!("{}{rest}", self.replacement);
        fs::remove_file(path).map_err(Error::io(format!("replace {relative}")))?;
        std::os::unix::fs::symlink(&new, path).map_err(Error::io(format!("relink {relative}")))?;
        crate::backend::set_symlink_times(path, &meta)?;
        Ok(Some(Change::symlink(&relative)))
    }

    /// True when a `[fixup] skip` glob names this path.
    fn skipped(&self, relative: &str) -> bool {
        self.skip
            .matched_path_or_any_parents(relative, false)
            .is_ignore()
    }

    /// The path of `entry` relative to the klon, for the log and the messages.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.klon)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

/// True when `grep-searcher` reads `bytes` as text and finds `needle` there.
/// The caller already holds the bytes, so the search opens no second file.
fn holds_needle(
    searcher: &mut Searcher,
    needle: &[u8],
    bytes: &[u8],
    relative: &str,
) -> Result<bool> {
    let mut found = Found::default();
    searcher
        .search_slice(Fixed(needle), bytes, &mut found)
        .map_err(Error::io(format!("search {relative}")))?;
    Ok(found.hit && !found.binary)
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
fn write_log(klon: &Path, lines: &[Change]) -> Result<()> {
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
    for change in lines {
        text.push_str(&change.line);
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

/// Replace every occurrence of `needle` in `text` that ends at a path
/// boundary. The answer is the new text and the number of replacements.
///
/// A plain fixed-string replace is not safe here. Golden's path is a string
/// prefix of every sibling that starts with the same name: with golden at
/// `/w/proj`, a plain replace would turn `/w/proj-docs` into
/// `<klon>-docs` and would rewrite the klon's own path inside golden's default
/// worktree root `/w/proj.wt`. Only an occurrence that ends the path component
/// names golden itself.
fn replace_paths(text: &str, needle: &str, replacement: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    let mut rest = text;
    while let Some(at) = rest.find(needle) {
        out.push_str(&rest[..at]);
        let after = &rest[at + needle.len()..];
        if continues_name(after) {
            out.push_str(needle);
        } else {
            out.push_str(replacement);
            count += 1;
        }
        rest = after;
    }
    out.push_str(rest);
    (out, count)
}

/// True when `rest` opens with a character that a file name can continue with,
/// so the text before it is only the start of another name. A separator, a
/// quote, a space, or the end of the text closes the name instead.
fn continues_name(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '~' | '+' | '@'))
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
///
/// The search itself is `memchr::memmem`, which `grep-searcher` already pulls
/// in. A hand-written byte loop looked simpler and was 12 times slower than
/// `grep -r` on a 20k-file build tree, because a test binary is unoptimized
/// while `memmem` and `std` are not.
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
        let found = memchr::memmem::find(&haystack[at..], self.0)
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

    /// The klon path of the default template starts with golden's path plus
    /// `.wt`, so a plain fixed-string replace would rewrite golden's own name
    /// inside the replacement it just wrote. The boundary rule stops that.
    #[test]
    fn the_replace_moves_only_a_whole_path() {
        let golden = "/w/proj";
        let klon = "/w/proj.wt/feature";
        let cases = [
            ("dir: /w/proj/store\n", "dir: /w/proj.wt/feature/store\n", 1),
            ("dir: /w/proj\n", "dir: /w/proj.wt/feature\n", 1),
            ("\"/w/proj\"", "\"/w/proj.wt/feature\"", 1),
            (
                "a=/w/proj:b=/w/proj/x",
                "a=/w/proj.wt/feature:b=/w/proj.wt/feature/x",
                2,
            ),
            // A sibling directory keeps its name.
            ("/w/proj-docs/readme", "/w/proj-docs/readme", 0),
            ("/w/project/readme", "/w/project/readme", 0),
            ("/w/proj.bak", "/w/proj.bak", 0),
            // The klon path itself must survive a second pass unchanged.
            ("/w/proj.wt/feature/x", "/w/proj.wt/feature/x", 0),
            ("nothing here", "nothing here", 0),
        ];
        for (input, want, count) in cases {
            let (out, found) = replace_paths(input, golden, klon);
            assert_eq!((out.as_str(), found), (want, count), "input {input}");
        }
    }

    #[test]
    fn the_searcher_reports_a_needle_in_a_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.yaml");
        fs::write(&file, "dir: /home/x/golden\nmore\n").unwrap();
        let mut found = Found::default();
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(false)
            .build();
        let outcome = searcher.search_path(Fixed(b"/home/x/golden"), &file, &mut found);
        assert!(outcome.is_ok(), "the search failed: {outcome:?}");
        assert!(found.hit, "the searcher must report the needle");
        assert!(!found.binary);
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
