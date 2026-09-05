//! The conflict radar (spec §7 C24, handoff §6): what each klon hits when it merges
//! into base, and where two klons collide with each other.
//!
//! Git offers two forms of the merge preview. From version 2.38 `git merge-tree
//! --write-tree` runs a real merge and names every conflicted path. Below 2.38 only
//! the legacy three-argument form exists; it prints one section per changed path and
//! klon reads the conflict out of that text. `doctor` names the form in use.
//!
//! Every pair result is cached under `<common>/klon/radar`, keyed by the form and the
//! two object ids, so a second run with unchanged heads starts no `merge-tree`.

use crate::{config, git, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The version of the cache file format. A file with another value is ignored.
const CACHE_SCHEMA: &str = "klon.radar/1";

/// `merge-tree --write-tree` became the default mode in git 2.38.
const WRITE_TREE_SINCE: (u32, u32) = (2, 38);
/// `merge-tree --stdin` takes every pair in one process. It arrived in git 2.40.
const STDIN_SINCE: (u32, u32) = (2, 40);

// --- The form the installed git supports --------------------------------------

/// Which `merge-tree` form klon uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `git merge-tree --write-tree`, git 2.38 and above.
    /// `batch` is true from git 2.40, where `--stdin` takes every pair at once.
    WriteTree { batch: bool },
    /// `git merge-tree <merge-base> <a> <b>`, every git below 2.38.
    Legacy,
}

impl Form {
    /// The `doctor` label for this form.
    pub fn label(self) -> &'static str {
        match self {
            Form::WriteTree { .. } => "radar: merge-tree --write-tree",
            Form::Legacy => "radar: legacy merge-tree",
        }
    }

    /// A short tag that goes into the cache key. Two forms may disagree, so a
    /// cached result must never cross from one to the other.
    fn tag(self) -> &'static str {
        match self {
            Form::WriteTree { .. } => "write-tree",
            Form::Legacy => "legacy",
        }
    }
}

/// The form of the installed git. `git --version` runs once per process.
pub fn form(cwd: &Path) -> Form {
    static FORM: OnceLock<Form> = OnceLock::new();
    *FORM.get_or_init(|| match git::run(cwd, &["--version"]) {
        Ok(text) => from_version(&text),
        Err(_) => Form::Legacy,
    })
}

/// The line `doctor` prints for the radar.
// TODO(C4): `doctor` calls this once C4 lands the command.
#[allow(dead_code)]
pub fn doctor_row(cwd: &Path) -> String {
    form(cwd).label().to_string()
}

/// Read `git version 2.34.1` and pick the form. A line klon cannot read counts as
/// the legacy form, which every git supports.
fn from_version(text: &str) -> Form {
    let number = match text.split_whitespace().nth(2) {
        Some(number) => number,
        None => return Form::Legacy,
    };
    let mut parts = number.split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    if (major, minor) < WRITE_TREE_SINCE {
        Form::Legacy
    } else {
        Form::WriteTree {
            batch: (major, minor) >= STDIN_SINCE,
        }
    }
}

// --- What the radar reports ---------------------------------------------------

/// One klon the radar looks at.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    /// The branch name, or `(detached)`.
    pub branch: String,
    /// The full object id of HEAD. A klon without one has no commit to merge, so
    /// the radar leaves every column at `-` and `list` still shows the klon.
    pub head: Option<String>,
}

/// The three radar columns for one klon. `list --json` flattens it into each
/// klon of `klon.list/1`.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// `clean`, `N conflicts`, `behind M`, or `-` when klon could not measure it.
    pub vs_base: String,
    /// `clean`, `N conflicts with <branch>`, or `-`.
    pub vs_siblings: String,
    /// Commits in base that this klon lacks.
    pub behind: Option<usize>,
}

impl Row {
    /// The radar part of a `list` line: three columns after a pipe each.
    pub fn columns(&self) -> String {
        let behind = match self.behind {
            Some(n) => format!("behind {n}"),
            None => "-".to_string(),
        };
        format!("| {} | {} | {behind}", self.vs_base, self.vs_siblings)
    }

    /// The row klon shows when it cannot reach the radar at all.
    fn unknown() -> Row {
        Row {
            vs_base: "-".to_string(),
            vs_siblings: "-".to_string(),
            behind: None,
        }
    }
}

/// `1 conflict` or `N conflicts`.
fn conflicts(n: usize) -> String {
    if n == 1 {
        "1 conflict".to_string()
    } else {
        format!("{n} conflicts")
    }
}

/// Every klon in the repository, in `git worktree list` order. The main worktree is
/// not a klon and never appears.
pub fn targets(worktrees: &[git::Worktree]) -> Vec<Target> {
    worktrees
        .iter()
        .skip(1)
        .map(|worktree| Target {
            path: worktree.path.clone(),
            branch: worktree
                .branch
                .as_deref()
                .and_then(|b| b.strip_prefix("refs/heads/"))
                .unwrap_or("(detached)")
                .to_string(),
            head: worktree.head.clone(),
        })
        .collect()
}

/// The commit every klon measures against: `base` from `.klon.toml`, else the commit
/// the main worktree has checked out.
fn base_oid(golden: &Path) -> Result<String> {
    let named = config::load(golden).ok().and_then(|cfg| cfg.base);
    let rev = match &named {
        Some(name) => format!("{name}^{{commit}}"),
        None => "HEAD^{commit}".to_string(),
    };
    let out = git::run(golden, &["rev-parse", "--verify", "--quiet", &rev])?;
    let oid = out.trim();
    if oid.is_empty() {
        return Err(Error::klon(format!("cannot resolve the radar base {rev}")));
    }
    Ok(oid.to_string())
}

// --- The scan -----------------------------------------------------------------

/// The radar row of every target, in the same order.
///
/// klon never fails a command because the radar failed. A base it cannot resolve, a
/// git that refuses the merge preview, or a cache it cannot write each cost one
/// stderr line and leave a `-` in the affected column.
pub fn scan(golden: &Path, common: &Path, targets: &[Target]) -> Vec<Row> {
    if targets.is_empty() {
        return Vec::new();
    }
    let base = match base_oid(golden) {
        Ok(base) => base,
        Err(err) => {
            eprintln!("klon: the radar has no base: {err}; the columns show -");
            return vec![Row::unknown(); targets.len()];
        }
    };
    let form = form(golden);
    let dir = common.join("klon").join("radar");

    // A klon without a HEAD has no commit to merge. It keeps its place in the
    // output and takes no pair.
    let heads: Vec<(usize, &str)> = targets
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.head.as_deref().map(|head| (i, head)))
        .collect();
    let mut rows = vec![Row::unknown(); targets.len()];

    // One request per pair: every klon against base, then every pair of klons.
    let mut requests: Vec<Request> = heads
        .iter()
        .map(|(_, head)| Request::new(form, Kind::Base, &base, head))
        .collect();
    let mut sibling_index = Vec::new();
    for a in 0..heads.len() {
        for b in (a + 1)..heads.len() {
            sibling_index.push((a, b));
            requests.push(Request::new(form, Kind::Sibling, heads[a].1, heads[b].1));
        }
    }
    resolve(golden, &dir, form, &mut requests);

    // The `behind` count travels with the vs-base entry, so the cache covers it too.
    // Every column stands alone: one pair klon could not run leaves a `-` in its
    // own column and nowhere else.
    for (slot, (target, _)) in heads.iter().enumerate() {
        let (vs_base, behind) = match &requests[slot].result {
            Some(result) if result.conflicts > 0 => {
                (conflicts(result.conflicts), Some(result.behind))
            }
            Some(result) if result.behind > 0 => {
                (format!("behind {}", result.behind), Some(result.behind))
            }
            Some(result) => ("clean".to_string(), Some(result.behind)),
            None => ("-".to_string(), None),
        };
        rows[*target] = Row {
            vs_base,
            vs_siblings: "clean".to_string(),
            behind,
        };
    }
    // Each sibling pair reports into both of its klons.
    let mut against: Vec<Vec<(String, usize)>> = vec![Vec::new(); targets.len()];
    for (pair, (a, b)) in sibling_index.iter().enumerate() {
        let (left, right) = (heads[*a].0, heads[*b].0);
        let count = match &requests[heads.len() + pair].result {
            Some(result) if result.conflicts > 0 => result.conflicts,
            Some(_) => continue,
            None => {
                rows[left].vs_siblings = "-".to_string();
                rows[right].vs_siblings = "-".to_string();
                continue;
            }
        };
        against[left].push((targets[right].branch.clone(), count));
        against[right].push((targets[left].branch.clone(), count));
    }
    for (row, mut list) in rows.iter_mut().zip(against) {
        if list.is_empty() || row.vs_siblings == "-" {
            continue;
        }
        // The worst sibling first, then by branch name so the line never reorders.
        list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        row.vs_siblings = list
            .iter()
            .map(|(branch, n)| format!("{} with {branch}", conflicts(*n)))
            .collect::<Vec<_>>()
            .join(", ");
    }
    rows
}

/// The radar row of one klon, for `sync --check`.
pub fn scan_one(golden: &Path, common: &Path, targets: &[Target], which: usize) -> Row {
    scan(golden, common, targets)
        .into_iter()
        .nth(which)
        .unwrap_or_else(Row::unknown)
}

// --- Pair requests and the cache ----------------------------------------------

/// What a pair compares. The cache key holds it, so a base pair and a sibling pair
/// over the same two commits never share an entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Base,
    Sibling,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Base => "base",
            Kind::Sibling => "sibling",
        }
    }
}

/// One cached merge preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairResult {
    schema: String,
    /// The number of conflicted paths.
    conflicts: usize,
    /// The conflicted paths. Kept for `--json` and for a later `merge`.
    paths: Vec<String>,
    /// Commits the second side lacks from the first. Zero for a sibling pair.
    behind: usize,
}

/// One pair the scan must answer, with its cache key and its result.
struct Request {
    kind: Kind,
    /// The two commits, in command order.
    a: String,
    b: String,
    key: String,
    result: Option<PairResult>,
}

impl Request {
    fn new(form: Form, kind: Kind, a: &str, b: &str) -> Request {
        // A sibling pair is symmetric: order the ids so `a,b` and `b,a` share a key.
        let (a, b) = match kind {
            Kind::Sibling if b < a => (b, a),
            _ => (a, b),
        };
        Request {
            kind,
            a: a.to_string(),
            b: b.to_string(),
            key: cache_key(form, kind, a, b),
            result: None,
        }
    }
}

/// The cache file name: a hash of the schema, the form, the pair kind, and the ids.
fn cache_key(form: Form, kind: Kind, a: &str, b: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [CACHE_SCHEMA, form.tag(), kind.tag(), a, b] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    config::hex(&hasher.finalize())
}

/// One pair klon computed: the conflict count, the conflicted paths, and how far
/// behind the second commit is.
struct Computed {
    count: usize,
    paths: Vec<String>,
    behind: usize,
}

/// Fill every request: from the cache when the key is there, else from `merge-tree`.
fn resolve(golden: &Path, dir: &Path, form: Form, requests: &mut [Request]) {
    let mut missing = Vec::new();
    for (i, request) in requests.iter_mut().enumerate() {
        request.result = read_cache(dir, &request.key);
        if request.result.is_none() {
            missing.push(i);
        }
    }
    if missing.is_empty() {
        return;
    }
    let computed = match form {
        Form::WriteTree { batch: true } => batch_write_tree(golden, requests, &missing),
        _ => None,
    };
    let computed = match computed {
        Some(paths) => paths
            .into_iter()
            .zip(&missing)
            .map(|(paths, index)| {
                Some(Computed {
                    count: paths.len(),
                    paths,
                    behind: behind_of(golden, &requests[*index]),
                })
            })
            .collect(),
        None => compute_missing(golden, form, requests, &missing),
    };
    for (index, found) in missing.iter().zip(computed) {
        let found = match found {
            Some(found) => found,
            None => continue,
        };
        let result = PairResult {
            schema: CACHE_SCHEMA.to_string(),
            conflicts: found.count,
            paths: found.paths,
            behind: found.behind,
        };
        write_cache(dir, &requests[*index].key, &result);
        requests[*index].result = Some(result);
    }
}

/// klon spawns at most this many `merge-tree` processes at a time.
const MAX_WORKERS: usize = 16;

/// Compute every missing pair. Each pair is an independent `git` subprocess that
/// spends its time waiting, so klon spreads the pairs over a small pool of threads.
/// Five klons make fifteen pairs, and sequential process starts alone would take
/// most of the budget R23 gives the whole radar.
fn compute_missing(
    golden: &Path,
    form: Form,
    requests: &[Request],
    missing: &[usize],
) -> Vec<Option<Computed>> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, MAX_WORKERS)
        .min(missing.len());
    if workers <= 1 {
        return missing
            .iter()
            .map(|index| compute_one(golden, form, &requests[*index]))
            .collect();
    }
    let chunks: Vec<&[usize]> = missing.chunks(missing.len().div_ceil(workers)).collect();
    let mut out = Vec::with_capacity(missing.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                let chunk = *chunk;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|index| compute_one(golden, form, &requests[*index]))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        // A worker that panicked leaves its pairs unmeasured, never misaligned.
        for (chunk, handle) in chunks.iter().zip(handles) {
            match handle.join() {
                Ok(results) => out.extend(results),
                Err(_) => out.extend(std::iter::repeat_with(|| None).take(chunk.len())),
            }
        }
    });
    out
}

/// One pair through `merge-tree`. A pair klon cannot compute costs one stderr line.
fn compute_one(golden: &Path, form: Form, request: &Request) -> Option<Computed> {
    match one_pair(golden, form, request) {
        Ok((count, paths)) => Some(Computed {
            count,
            paths,
            behind: behind_of(golden, request),
        }),
        Err(err) => {
            eprintln!("klon: the radar cannot compare two commits: {err}");
            None
        }
    }
}

/// How far the klon of a base pair sits behind base. A sibling pair has no such
/// direction, so it reports zero.
fn behind_of(golden: &Path, request: &Request) -> usize {
    match request.kind {
        Kind::Base => behind_count(golden, &request.b, &request.a),
        Kind::Sibling => 0,
    }
}

/// Commits in `base` that `head` lacks.
fn behind_count(golden: &Path, head: &str, base: &str) -> usize {
    let range = format!("{head}..{base}");
    git::run(golden, &["rev-list", "--count", &range])
        .ok()
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0)
}

/// Read one cache entry. A missing, unreadable, or foreign file is a miss.
fn read_cache(dir: &Path, key: &str) -> Option<PairResult> {
    let text = std::fs::read_to_string(dir.join(format!("{key}.json"))).ok()?;
    let result: PairResult = serde_json::from_str(&text).ok()?;
    (result.schema == CACHE_SCHEMA).then_some(result)
}

/// Write one cache entry. A cache klon cannot write costs one stderr line and
/// nothing else: the next run recomputes the pair.
fn write_cache(dir: &Path, key: &str, result: &PairResult) {
    let file = dir.join(format!("{key}.json"));
    let write = std::fs::create_dir_all(dir)
        .and_then(|()| serde_json::to_vec(result).map_err(std::io::Error::other))
        .and_then(|bytes| std::fs::write(&file, bytes));
    if let Err(err) = write {
        eprintln!("klon: cannot write {}: {err}", file.display());
    }
}

// --- The two merge-tree forms -------------------------------------------------

/// The conflict count and the conflicted paths of one pair, through the form the
/// installed git supports. The count can exceed the path list when git reports a
/// conflict it does not name.
fn one_pair(golden: &Path, form: Form, request: &Request) -> Result<(usize, Vec<String>)> {
    match form {
        Form::WriteTree { .. } => write_tree_pair(golden, &request.a, &request.b),
        Form::Legacy => {
            let paths = legacy_pair(golden, &request.a, &request.b)?;
            Ok((paths.len(), paths))
        }
    }
}

/// The arguments of the modern per-pair form.
///
/// klon does not pass `--quiet`: it suppresses the very path list the radar reads,
/// and git only learned it in 2.50, well above the 2.38 floor of `--write-tree`.
fn write_tree_args<'a>(a: &'a str, b: &'a str) -> [&'a str; 7] {
    [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        "-z",
        a,
        b,
    ]
}

/// git 2.38 and above, one pair per process. Output is the merged tree id, a NUL,
/// then one NUL-terminated path per conflict. Exit code 1 means "conflicted".
fn write_tree_pair(golden: &Path, a: &str, b: &str) -> Result<(usize, Vec<String>)> {
    let (code, out) = git::run_input(golden, &write_tree_args(a, b), b"", &[0, 1])?;
    let paths = parse_write_tree(&out);
    // git found a conflict it did not name. Count it, and leave the path list empty.
    let count = if code == 1 { paths.len().max(1) } else { 0 };
    Ok((count, paths))
}

/// Read `<tree oid>NUL<path>NUL...` and keep the paths.
fn parse_write_tree(out: &[u8]) -> Vec<String> {
    out.split(|byte| *byte == 0)
        .skip(1)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

/// git 2.40 and above: every missing pair in one process through `--stdin`.
/// Returns `None` when the batch cannot run or its output does not parse, and the
/// caller then falls back to one process per pair.
fn batch_write_tree(
    golden: &Path,
    requests: &[Request],
    missing: &[usize],
) -> Option<Vec<Vec<String>>> {
    let mut input = String::new();
    for index in missing {
        input.push_str(&format!("{} {}\n", requests[*index].a, requests[*index].b));
    }
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        "--stdin",
    ];
    // `--stdin` exits 0 for a clean and for a conflicted merge alike.
    let (_, out) = git::run_input(golden, &args, input.as_bytes(), &[0]).ok()?;
    let parsed = parse_stdin(&out, missing.len());
    if parsed.is_none() {
        eprintln!("klon: cannot read `git merge-tree --stdin`; the radar runs one pair per call");
    }
    parsed
}

/// Read the `--stdin` record stream: per pair a status field (`0` conflicted, `1`
/// clean), the merged tree id, one field per conflicted path, and an empty field
/// that ends the record. `--no-messages` keeps the informational section out, so
/// the empty field is unambiguous. Any surprise returns `None`.
fn parse_stdin(out: &[u8], pairs: usize) -> Option<Vec<Vec<String>>> {
    let mut fields = out.split(|byte| *byte == 0);
    let mut all = Vec::with_capacity(pairs);
    for _ in 0..pairs {
        let status = fields.next()?;
        if status != b"0" && status != b"1" {
            return None;
        }
        fields.next()?; // The merged tree id.
        let mut paths = Vec::new();
        loop {
            let field = fields.next()?;
            if field.is_empty() {
                break;
            }
            paths.push(String::from_utf8_lossy(field).into_owned());
        }
        if (status == b"1") != paths.is_empty() {
            return None; // A clean merge names no path, a conflicted one names some.
        }
        all.push(paths);
    }
    // Nothing but the trailing separator may follow the last record.
    fields.all(|field| field.is_empty()).then_some(all)
}

/// Below git 2.38: `git merge-tree <merge-base> <a> <b>`.
fn legacy_pair(golden: &Path, a: &str, b: &str) -> Result<Vec<String>> {
    let merge_base = git::run(golden, &["merge-base", a, b])?.trim().to_string();
    let args = ["merge-tree", merge_base.as_str(), a, b];
    // The legacy form exits 0 whether or not the merge conflicts.
    let (_, out) = git::run_input(golden, &args, b"", &[0])?;
    Ok(parse_legacy(&String::from_utf8_lossy(&out)))
}

/// The eight section headers of the legacy form, from git's `builtin/merge-tree.c`.
const LEGACY_HEADERS: &[&str] = &[
    "merged",
    "added in remote",
    "added in both",
    "added in local",
    "removed in both",
    "changed in both",
    "removed in local",
    "removed in remote",
];

/// One section of legacy `merge-tree` output while klon reads it.
#[derive(Default)]
struct LegacySection<'a> {
    header: &'a str,
    path: Option<String>,
    /// The object id of each side that the section names.
    sides: BTreeMap<&'a str, String>,
    /// True once a `@@` hunk header appeared: git produced a text merge.
    hunk: bool,
    open_marker: bool,
    close_marker: bool,
}

/// Read the conflicted paths out of legacy `git merge-tree` output.
///
/// The output is a run of sections. A header sits at column 0 and names the outcome.
/// Indented `  <side> <mode> <oid> <path>` lines follow, then a unified diff of the
/// merged content. klon calls a section a conflict when one of three things holds:
///
/// * the diff adds a `<<<<<<<` line and a `>>>>>>>` line, the markers git writes into
///   a text merge it could not finish;
/// * both sides changed the file, they disagree, and git printed no diff at all,
///   which is what a binary file gives (git also warns on stderr);
/// * one side deleted the file while the other changed it, which a real merge reports
///   as a modify/delete conflict but this form resolves silently.
fn parse_legacy(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut section = LegacySection::default();
    for line in text.lines() {
        if LEGACY_HEADERS.contains(&line) {
            flush_legacy(&section, &mut paths);
            section = LegacySection {
                header: line,
                ..LegacySection::default()
            };
        } else if let Some((side, oid, path)) = legacy_entry(line) {
            section.path.get_or_insert_with(|| path.to_string());
            section.sides.insert(side, oid.to_string());
        } else if line.starts_with("@@ ") {
            section.hunk = true;
        } else if let Some(added) = line.strip_prefix('+') {
            section.open_marker |= added.starts_with("<<<<<<<");
            section.close_marker |= added.starts_with(">>>>>>>");
        }
    }
    flush_legacy(&section, &mut paths);
    paths
}

/// Read `  base   100644 <oid> <path>`. git writes the side name left-padded and the
/// path unquoted, so klon skips the padding and keeps every byte of the path.
fn legacy_entry(line: &str) -> Option<(&str, &str, &str)> {
    let (side, rest) = line.strip_prefix("  ")?.split_once(' ')?;
    if !matches!(side, "result" | "base" | "our" | "their") {
        return None;
    }
    let (mode, rest) = rest.trim_start_matches(' ').split_once(' ')?;
    if mode.len() != 6 || !mode.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (oid, path) = rest.split_once(' ')?;
    (!oid.is_empty() && !path.is_empty()).then_some((side, oid, path))
}

/// Add the section's path to `paths` when the section is a conflict.
fn flush_legacy(section: &LegacySection, paths: &mut Vec<String>) {
    let path = match &section.path {
        Some(path) => path,
        None => return,
    };
    let (base, ours, theirs) = (
        section.sides.get("base"),
        section.sides.get("our"),
        section.sides.get("their"),
    );
    let conflicted = if section.open_marker && section.close_marker {
        true
    } else {
        match section.header {
            "changed in both" | "added in both" => !section.hunk && ours != theirs,
            "removed in remote" => base.is_some() && base != ours,
            "removed in local" => base.is_some() && base != theirs,
            _ => false,
        }
    };
    if conflicted {
        paths.push(path.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_picks_the_form() {
        assert_eq!(from_version("git version 2.34.1\n"), Form::Legacy);
        assert_eq!(from_version("git version 2.37.9\n"), Form::Legacy);
        assert_eq!(
            from_version("git version 2.38.0\n"),
            Form::WriteTree { batch: false }
        );
        assert_eq!(
            from_version("git version 2.39.5 (Apple Git-154)\n"),
            Form::WriteTree { batch: false }
        );
        assert_eq!(
            from_version("git version 2.40.1\n"),
            Form::WriteTree { batch: true }
        );
        assert_eq!(
            from_version("git version 2.51.0\n"),
            Form::WriteTree { batch: true }
        );
        assert_eq!(
            from_version("git version 3.0.0\n"),
            Form::WriteTree { batch: true }
        );
        // A line klon cannot read falls back to the form every git has.
        assert_eq!(from_version("weird\n"), Form::Legacy);
        assert_eq!(from_version("git version next\n"), Form::Legacy);
    }

    #[test]
    fn doctor_names_each_form() {
        assert_eq!(Form::Legacy.label(), "radar: legacy merge-tree");
        assert_eq!(
            Form::WriteTree { batch: true }.label(),
            "radar: merge-tree --write-tree"
        );
    }

    #[test]
    fn the_cache_key_separates_every_input() {
        let legacy = Form::Legacy;
        let modern = Form::WriteTree { batch: false };
        let base = cache_key(legacy, Kind::Base, "aaa", "bbb");
        assert_ne!(base, cache_key(modern, Kind::Base, "aaa", "bbb"));
        assert_ne!(base, cache_key(legacy, Kind::Sibling, "aaa", "bbb"));
        assert_ne!(base, cache_key(legacy, Kind::Base, "aaa", "ccc"));
        assert_ne!(base, cache_key(legacy, Kind::Base, "ccc", "bbb"));
        // Two ids that only differ in where the separator falls stay apart.
        assert_ne!(
            cache_key(legacy, Kind::Base, "aa", "abbb"),
            cache_key(legacy, Kind::Base, "aaa", "bbb")
        );
    }

    #[test]
    fn a_sibling_pair_has_one_key_in_both_orders() {
        let form = Form::Legacy;
        let left = Request::new(form, Kind::Sibling, "bbb", "aaa");
        let right = Request::new(form, Kind::Sibling, "aaa", "bbb");
        assert_eq!(left.key, right.key);
        assert_eq!(left.a, "aaa");
        // A base pair keeps its order: base first, then the klon head.
        let base = Request::new(form, Kind::Base, "bbb", "aaa");
        assert_eq!(base.a, "bbb");
    }

    // --- The legacy parser, against output recorded from git 2.34.1 ---------

    const SAME_LINE: &str = "\
changed in both
  base   100644 83db48f84ec878fbfb30b46d16630e944e34f205 same.txt
  our    100644 73950990113369344b6a1561e099c8b01d437400 same.txt
  their  100644 520bd84d5ab550dc4d96b856a6003ab3e750441d same.txt
@@ -1,3 +1,7 @@
 line1
+<<<<<<< .our
 A-EDIT
+=======
+B-EDIT
+>>>>>>> .their
 line3
";

    #[test]
    fn legacy_finds_a_same_line_conflict() {
        assert_eq!(parse_legacy(SAME_LINE), vec!["same.txt".to_string()]);
    }

    #[test]
    fn legacy_passes_a_clean_automerge() {
        // Both sides changed the file, in hunks far apart. git merged it.
        let text = "\
changed in both
  base   100644 0ff3bbb9c8bba2291654cd64067fa417ff54c508 automerge.txt
  our    100644 f5dc1abfb45014e3b9bc0c27b9ad8e711d42cc21 automerge.txt
  their  100644 5d5d869e755cd2634b4d4cfaf1d64038cdfb77d5 automerge.txt
@@ -16,5 +16,5 @@
 16
 17
 18
-19
+B-EDIT
 20
";
        assert!(parse_legacy(text).is_empty());
    }

    #[test]
    fn legacy_passes_a_one_sided_change_and_a_clean_delete() {
        let text = "\
merged
  result 100644 f5dc1abfb45014e3b9bc0c27b9ad8e711d42cc21 other.txt
  our    100644 0ff3bbb9c8bba2291654cd64067fa417ff54c508 other.txt
@@ -1 +1 @@
-untouched
+changed
removed in remote
  base   100644 df967b96a579e45a18b8251732d16804b2e56a55 delme.txt
  our    100644 df967b96a579e45a18b8251732d16804b2e56a55 delme.txt
@@ -1 +0,0 @@
-base
";
        assert!(parse_legacy(text).is_empty());
    }

    #[test]
    fn legacy_finds_a_modify_delete_conflict() {
        // Our side changed the file, their side deleted it. The base and our ids
        // differ, and a real merge stops on that.
        let text = "\
removed in remote
  base   100644 df967b96a579e45a18b8251732d16804b2e56a55 gone.txt
  our    100644 7d0fbd899c2fc554e82fe2a51d9ee8f8f0a389cc gone.txt
@@ -1 +0,0 @@
-a changed
";
        assert_eq!(parse_legacy(text), vec!["gone.txt".to_string()]);
        let mirrored = text
            .replace("removed in remote", "removed in local")
            .replace("  our  ", "  their");
        assert_eq!(parse_legacy(&mirrored), vec!["gone.txt".to_string()]);
    }

    #[test]
    fn legacy_finds_an_add_add_conflict() {
        let text = "\
added in both
  our    100644 4e2940669539cf041b55347761902986c666ddad add-both.txt
  their  100644 83d0a58f1ba055e822ee900d83b86d9601efcc91 add-both.txt
@@ -1 +1,5 @@
+<<<<<<< .our
 add-both A
+=======
+add-both B
+>>>>>>> .their
";
        assert_eq!(parse_legacy(text), vec!["add-both.txt".to_string()]);
    }

    #[test]
    fn legacy_finds_a_binary_conflict_that_has_no_diff() {
        // git prints the section and warns on stderr, but writes no hunk.
        let text = "\
changed in both
  base   100644 b81139596fd1764fffacb13971ae505de12c3afe bin.dat
  our    100644 1d067d95503804efed9bfd90e3df76b5e32cf6f9 bin.dat
  their  100644 d189f5078b356911abf36ed983f1f034927c0dbe bin.dat
";
        assert_eq!(parse_legacy(text), vec!["bin.dat".to_string()]);
    }

    #[test]
    fn legacy_keeps_a_path_with_a_space_and_reports_every_section() {
        let text = format!(
            "{SAME_LINE}\
changed in both
  base   100644 f0f23074642919bb50ab5b6e4ec489706127f061 with space.txt
  our    100644 3b1896e9670045d19e7eb7343791642d1bad33a5 with space.txt
  their  100644 c9950ff67703dc0962a0ad0c6bc012fb4f04c4c3 with space.txt
@@ -1,3 +1,7 @@
 l1
+<<<<<<< .our
 A
+=======
+B
+>>>>>>> .their
 l3
"
        );
        assert_eq!(
            parse_legacy(&text),
            vec!["same.txt".to_string(), "with space.txt".to_string()]
        );
    }

    #[test]
    fn legacy_ignores_content_that_looks_like_a_section() {
        // Every diff line carries a prefix, so file content never sits at column 0.
        let text = "\
merged
  result 100644 f5dc1abfb45014e3b9bc0c27b9ad8e711d42cc21 notes.txt
  our    100644 0ff3bbb9c8bba2291654cd64067fa417ff54c508 notes.txt
@@ -1 +1,2 @@
 changed in both
+added in both
";
        assert!(parse_legacy(text).is_empty());
        assert_eq!(parse_legacy(""), Vec::<String>::new());
    }

    // --- The modern parsers -------------------------------------------------

    #[test]
    fn write_tree_args_hold_no_quiet_flag() {
        let args = write_tree_args("aaa", "bbb");
        assert_eq!(args[0], "merge-tree");
        assert!(args.contains(&"--write-tree"));
        assert!(args.contains(&"--name-only"));
        assert!(args.contains(&"-z"));
        // `--quiet` would suppress the path list the radar reads.
        assert!(!args.contains(&"--quiet"));
        assert_eq!(&args[5..], ["aaa", "bbb"]);
    }

    #[test]
    fn write_tree_output_drops_the_tree_id() {
        let clean = b"1234567890abcdef1234567890abcdef12345678\x00";
        assert!(parse_write_tree(clean).is_empty());
        let conflicted = b"1234567890abcdef1234567890abcdef12345678\x00a.txt\x00b/c.txt\x00";
        assert_eq!(parse_write_tree(conflicted), vec!["a.txt", "b/c.txt"]);
    }

    #[test]
    fn stdin_records_split_on_the_empty_field() {
        // A record is: status, merged tree id, one field per conflicted path, and
        // an empty field that ends it. Status 1 is clean, status 0 conflicted.
        let clean = b"1\x00aaa\x00\x00" as &[u8];
        assert_eq!(parse_stdin(clean, 1), Some(vec![vec![]]));
        let two = b"0\x00aaa\x00x.txt\x00y.txt\x00\x001\x00bbb\x00\x00" as &[u8];
        assert_eq!(
            parse_stdin(two, 2),
            Some(vec![vec!["x.txt".to_string(), "y.txt".to_string()], vec![]])
        );
    }

    #[test]
    fn a_stdin_stream_klon_cannot_read_gives_none() {
        // Too few records, a status field that is not 0 or 1, a clean record that
        // still names a path, and bytes left after the last record.
        assert_eq!(parse_stdin(b"1\x00aaa\x00\x00", 2), None);
        assert_eq!(parse_stdin(b"7\x00aaa\x00\x00", 1), None);
        assert_eq!(parse_stdin(b"1\x00aaa\x00x.txt\x00\x00", 1), None);
        assert_eq!(parse_stdin(b"0\x00aaa\x00x.txt\x00\x00junk\x00", 1), None);
        assert_eq!(parse_stdin(b"", 1), None);
    }

    #[test]
    fn a_row_prints_three_columns() {
        let row = Row {
            vs_base: "clean".to_string(),
            vs_siblings: "1 conflict with other".to_string(),
            behind: Some(3),
        };
        assert_eq!(row.columns(), "| clean | 1 conflict with other | behind 3");
        assert_eq!(Row::unknown().columns(), "| - | - | -");
        assert_eq!(conflicts(1), "1 conflict");
        assert_eq!(conflicts(2), "2 conflicts");
    }
}
