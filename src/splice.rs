//! A byte splice of a git index (spec §7 G4; R2, R12).
//!
//! `git checkout -q --force <branch>` does three jobs in a klon: it writes the
//! working-tree files that differ between the spare's commit and the branch, it
//! replaces the index with one that matches the branch, and it moves `HEAD`.
//! On the 100k fixture the first job writes 22 files and the second rewrites
//! the whole index, which costs 355 to 423 ms (G1 §5.7). This module
//! does the second job by hand: it copies the entry bytes of every path the
//! branch leaves alone and re-emits only the entries the branch changes.
//!
//! The rule of the module is that **a splice that is not certainly correct
//! refuses**, and the caller then runs `git checkout` as before. Every refusal
//! carries a short reason for the `KLON_DEBUG=1` line. The module refuses an
//! index it cannot read, an extension it does not know, an entry at a merge
//! stage or with an extended flag, and a change it cannot place.
//!
//! Two extensions need care.
//!
//! - `TREE`, the cached tree, is **dropped**. A stale cached tree makes `git
//!   commit` write a wrong tree, and a correct patch of it means re-emitting a
//!   recursive structure. git treats a missing cached tree as "nothing cached"
//!   and rebuilds it from the entries, which are the truth, so dropping is the
//!   one choice that cannot be silently wrong. The first `git write-tree` in
//!   the klon pays for the rebuild: 0.76 s against 0.04 s on the 100k fixture,
//!   and 0.04 s every time after it. That is off the `add` path.
//! - `IEOT`, the entry offset table, is **recomputed**. Its blocks are byte
//!   offsets of entries, so a splice moves them. Dropping it would cost every
//!   later git command the threaded index read that G2 turned on, so the
//!   splice emits fresh blocks over the entries it wrote.
//!
//! `EOIE` is recomputed as well: it holds the offset of the end of the entries
//! and a hash over the extension headers, and both change. `UNTR` moves
//! through `untracked::retarget`, so the splice also does the work that
//! `spare::take_index` would otherwise do in a second pass over the same
//! bytes.

use crate::untracked;
use sha1::Digest;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The most paths a splice will place. A bigger diff is a bigger share of the
/// tree, where `git checkout` is the better tool and the fallback is cheap.
pub const MAX_CHANGES: usize = 1000;

/// Why a splice refused. The caller prints it under `KLON_DEBUG=1` and runs
/// `git checkout`.
pub type Refused = &'static str;

/// The stat fields of an index entry, from the file the caller just wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    ctime: (u32, u32),
    mtime: (u32, u32),
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

impl Stat {
    /// The fields git records, each truncated to 32 bits the way
    /// `fill_stat_data` in `read-cache.c` truncates them.
    pub fn of(meta: &std::fs::Metadata) -> Stat {
        Stat {
            ctime: (meta.ctime() as u32, meta.ctime_nsec() as u32),
            mtime: (meta.mtime() as u32, meta.mtime_nsec() as u32),
            dev: meta.dev() as u32,
            ino: meta.ino() as u32,
            uid: meta.uid(),
            gid: meta.gid(),
            size: meta.size() as u32,
        }
    }

    fn write(&self, out: &mut [u8]) {
        let fields = [
            self.ctime.0,
            self.ctime.1,
            self.mtime.0,
            self.mtime.1,
            self.dev,
            self.ino,
        ];
        for (i, value) in fields.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&value.to_be_bytes());
        }
        // Offset 24 is the mode, which the caller already wrote.
        out[28..32].copy_from_slice(&self.uid.to_be_bytes());
        out[32..36].copy_from_slice(&self.gid.to_be_bytes());
        out[36..40].copy_from_slice(&self.size.to_be_bytes());
    }
}

/// What the branch does to one path, as `git diff-tree -r --raw` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The path, as the index spells it: relative, `/` separated, no NUL.
    pub path: Vec<u8>,
    /// The mode and object id the branch holds, or None when the branch drops
    /// the path.
    pub to: Option<Target>,
}

/// The state a changed path has on the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// git's canonical mode: `0o100644`, `0o100755`, or `0o120000`.
    pub mode: u32,
    /// The object id, raw, as long as the index hash.
    pub oid: Vec<u8>,
}

/// A spliced index, complete but for the stat data of the paths the caller is
/// about to write and the checksum trailer.
pub struct Spliced {
    bytes: Vec<u8>,
    /// The offset of each change's 40-byte stat block, in the order of the
    /// `changes` slice that made this. A dropped path has none.
    stat_at: Vec<Option<usize>>,
    hash_len: usize,
}

impl Spliced {
    /// Record the stat data of the file the caller wrote for change `i`.
    pub fn set_stat(&mut self, i: usize, stat: &Stat) {
        if let Some(at) = self.stat_at[i] {
            stat.write(&mut self.bytes[at..at + 40]);
        }
    }

    /// The finished index bytes, with the checksum git verifies on every read.
    /// `plan` refuses every index but a SHA-1 one, so the trailer is a SHA-1.
    pub fn finish(mut self) -> Vec<u8> {
        let body = self.bytes.len() - self.hash_len;
        let digest = sha1::Sha1::digest(&self.bytes[..body]);
        self.bytes[body..].copy_from_slice(&digest);
        self.bytes
    }
}

/// The extensions the splice knows. Anything else refuses: a `link` (split
/// index) or `sdir` (sparse index) is not even ignorable, and `REUC` and
/// `FSMN` carry state that a splice would leave stale.
fn known(sig: &[u8; 4]) -> bool {
    matches!(sig, b"TREE" | b"UNTR" | b"EOIE" | b"IEOT")
}

/// Build the new index for `bytes` under `changes`, with the untracked cache
/// pointed at `worktree`.
///
/// `changes` must name distinct paths. The answer carries no stat data yet:
/// the caller writes the files, then calls `set_stat` for each change and
/// `finish`. Nothing here reads or writes a file.
pub fn plan(bytes: &[u8], changes: &[Change], worktree: &Path) -> Result<Spliced, Refused> {
    if changes.len() > MAX_CHANGES {
        return Err("the diff is larger than the splice places");
    }
    let layout = untracked::parse(bytes).ok_or("the index does not parse")?;
    for ext in &layout.exts {
        if !known(&ext.sig) {
            return Err("the index carries an extension the splice does not know");
        }
    }
    let hash_len = layout.hash_len;
    // `EOIE` holds a hash of the extension headers in the repository's own
    // algorithm. Every repository this klon meets is SHA-1 on git 2.34, and a
    // splice that guessed the other one would write a hash git rejects, so a
    // SHA-256 index takes the checkout.
    if hash_len != 20 {
        return Err("the index is not a SHA-1 index");
    }
    for change in changes {
        if let Some(target) = &change.to {
            if target.oid.len() != hash_len {
                return Err("a change carries an object id of the wrong length");
            }
            if !matches!(target.mode, 0o100_644 | 0o100_755 | 0o120_000) {
                return Err("a change carries a mode the splice does not write");
            }
        }
        if change.path.is_empty() || change.path.contains(&0) {
            return Err("a change carries a path the index cannot hold");
        }
    }

    // The changes in index order, so the walk below meets each one once.
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by(|a, b| changes[*a].path.cmp(&changes[*b].path));
    for pair in order.windows(2) {
        if changes[pair[0]].path == changes[pair[1]].path {
            return Err("two changes name the same path");
        }
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 4096);
    out.extend_from_slice(&bytes[..12]);
    let mut stat_at = vec![None; changes.len()];
    let mut offsets: Vec<u32> = Vec::with_capacity(layout.count);
    // `path` holds the full path of the entry the walk is on, and so, at the
    // top of each turn, of the previous entry in the old index. `prev_new` is
    // the same for the new index. Version 4 spells each path against the
    // previous one, so an entry may be copied byte for byte only while the two
    // agree; before version 4 every entry spells its own path and always may.
    let mut prev_new: Vec<u8> = Vec::new();
    let mut path: Vec<u8> = Vec::new();
    let mut next = 0usize;
    let mut run: Option<Run> = None;
    let mut at = 12;
    let end = bytes.len() - hash_len;

    for _ in 0..layout.count {
        let start = at;
        let mut in_sync = layout.version != 4 || prev_new == path;
        let (mode, flags, name_at) = read_head(bytes, start, hash_len, layout.version, end)?;
        at = name_at;
        // The path, then the end of the entry.
        if layout.version == 4 {
            let (strip, n) = untracked::decode_varint(&bytes[at..end]).ok_or("a broken entry")?;
            at += n;
            if strip > path.len() {
                return Err("an entry strips more than the previous path holds");
            }
            let nul = bytes[at..end]
                .iter()
                .position(|b| *b == 0)
                .ok_or("an unterminated path")?;
            path.truncate(path.len() - strip);
            path.extend_from_slice(&bytes[at..at + nul]);
            at += nul + 1;
        } else {
            let nul = bytes[at..end]
                .iter()
                .position(|b| *b == 0)
                .ok_or("an unterminated path")?;
            path.clear();
            path.extend_from_slice(&bytes[at..at + nul]);
            at += nul + 1;
            at = start + (at - start).div_ceil(8) * 8;
        }
        if at > end {
            return Err("an entry runs past the end");
        }
        // git reads the name length from the flags below 0xfff, so a length
        // that disagrees with the NUL means the two readers would differ.
        let declared = (flags & 0x0fff) as usize;
        if declared != 0x0fff && declared != path.len() {
            return Err("an entry's name length disagrees with its path");
        }

        // A merge stage, an assume-valid mark, or an extended flag all change
        // what a checkout does with an entry, and a submodule needs a whole
        // subsystem. The splice refuses them all rather than decide.
        if flags & 0xf000 != 0 {
            return Err("an entry carries a stage or an extended flag");
        }
        if mode == 0o160_000 {
            return Err("the index holds a submodule");
        }

        // Every insert that sorts before this path goes first.
        while next < order.len() && changes[order[next]].path < path {
            let i = order[next];
            let target = changes[i]
                .to
                .as_ref()
                .ok_or("the branch drops a path the index does not hold")?;
            flush(&mut out, &mut run, bytes);
            let block_start = offsets.len() % BLOCK == 0;
            offsets.push(out.len() as u32);
            stat_at[i] = Some(emit(
                &mut out,
                layout.version,
                &prev_new,
                &changes[i].path,
                target,
                block_start,
            ));
            prev_new.clear();
            prev_new.extend_from_slice(&changes[i].path);
            // The entry before this one is no longer the one the old bytes
            // spell their path against, so those bytes cannot be copied.
            in_sync = false;
            next += 1;
        }
        // The first entry of a block spells its whole path, whatever stands
        // before it, so the copied bytes of the old index will not do.
        let block_start = offsets.len() % BLOCK == 0;
        in_sync = in_sync && !(layout.version == 4 && block_start);

        let change = (next < order.len() && changes[order[next]].path == path).then(|| {
            let i = order[next];
            next += 1;
            i
        });
        match change {
            // The branch drops the path: the entry is not emitted.
            Some(i) if changes[i].to.is_none() => {
                flush(&mut out, &mut run, bytes);
                continue;
            }
            // The branch holds another blob or mode at the same path.
            Some(i) => {
                flush(&mut out, &mut run, bytes);
                let target = changes[i].to.as_ref().expect("matched above");
                offsets.push(out.len() as u32);
                stat_at[i] = Some(emit(
                    &mut out,
                    layout.version,
                    &prev_new,
                    &path,
                    target,
                    block_start,
                ));
            }
            // The branch leaves the path alone, and the entry before it is the
            // one it was, so the old bytes still say the same thing.
            None if in_sync => match &mut run {
                Some(open) => {
                    offsets.push((open.at + (start - open.from)) as u32);
                    open.to = at;
                }
                None => {
                    offsets.push(out.len() as u32);
                    run = Some(Run {
                        from: start,
                        to: at,
                        at: out.len(),
                    });
                }
            },
            // The previous path differs, so the entry is spelled again, with
            // the stat data and the object id it already had.
            None => {
                flush(&mut out, &mut run, bytes);
                offsets.push(out.len() as u32);
                let target = Target {
                    mode,
                    oid: bytes[start + 40..start + 40 + hash_len].to_vec(),
                };
                let stat = emit(
                    &mut out,
                    layout.version,
                    &prev_new,
                    &path,
                    &target,
                    block_start,
                );
                out[stat..stat + 40].copy_from_slice(&bytes[start..start + 40]);
            }
        }
        prev_new.clear();
        prev_new.extend_from_slice(&path);
    }
    flush(&mut out, &mut run, bytes);
    // Every change left over sorts after the last entry.
    while next < order.len() {
        let i = order[next];
        let target = changes[i]
            .to
            .as_ref()
            .ok_or("the branch drops a path the index does not hold")?;
        let block_start = offsets.len() % BLOCK == 0;
        offsets.push(out.len() as u32);
        stat_at[i] = Some(emit(
            &mut out,
            layout.version,
            &prev_new,
            &changes[i].path,
            target,
            block_start,
        ));
        prev_new.clear();
        prev_new.extend_from_slice(&changes[i].path);
        next += 1;
    }
    if at != layout.extensions_at {
        return Err("the entry walk did not end where the extensions start");
    }
    let count = u32::try_from(offsets.len()).map_err(|_| "too many entries")?;
    out[8..12].copy_from_slice(&count.to_be_bytes());
    let extensions_at = out.len();

    // The extensions. `TREE` goes; `IEOT` and `EOIE` are rebuilt last, in the
    // order git writes them.
    for ext in &layout.exts {
        match &ext.sig {
            b"TREE" | b"IEOT" | b"EOIE" => continue,
            b"UNTR" => {
                let data = untracked::retarget(&bytes[ext.at..ext.at + ext.size], worktree)
                    .ok_or("the untracked cache does not parse")?;
                push_ext(&mut out, b"UNTR", &data);
            }
            _ => return Err("the index carries an extension the splice does not know"),
        }
    }
    if layout.exts.iter().any(|e| &e.sig == b"IEOT") {
        push_ext(&mut out, b"IEOT", &ieot(&offsets));
    }
    if layout.exts.iter().any(|e| &e.sig == b"EOIE") {
        let mut data = Vec::with_capacity(4 + hash_len);
        data.extend_from_slice(
            &(u32::try_from(extensions_at).map_err(|_| "index too large")?).to_be_bytes(),
        );
        // The hash covers the signature and the size of every extension before
        // this one, and nothing else.
        let mut hasher = sha1::Sha1::new();
        let mut walk = extensions_at;
        while walk < out.len() {
            hasher.update(&out[walk..walk + 8]);
            let size = u32::from_be_bytes(out[walk + 4..walk + 8].try_into().unwrap()) as usize;
            walk += 8 + size;
        }
        data.extend_from_slice(&hasher.finalize());
        push_ext(&mut out, b"EOIE", &data);
    }
    out.extend(std::iter::repeat_n(0u8, hash_len));
    Ok(Spliced {
        bytes: out,
        stat_at,
        hash_len,
    })
}

/// The mode, the flags, and the offset of the path of the entry at `start`.
fn read_head(
    bytes: &[u8],
    start: usize,
    hash_len: usize,
    version: u32,
    end: usize,
) -> Result<(u32, u16, usize), Refused> {
    let flags_at = start + 40 + hash_len;
    if flags_at + 2 > end {
        return Err("an entry runs past the end");
    }
    let mode = u32::from_be_bytes(bytes[start + 24..start + 28].try_into().unwrap());
    let flags = u16::from_be_bytes(bytes[flags_at..flags_at + 2].try_into().unwrap());
    let mut at = flags_at + 2;
    if version >= 3 && flags & 0x4000 != 0 {
        at += 2;
    }
    Ok((mode, flags, at))
}

/// A run of entries that the branch leaves alone, whose bytes say the same
/// thing in the new index: where it starts in the old bytes, where it will
/// land in the new ones, and where it ends so far.
struct Run {
    from: usize,
    to: usize,
    at: usize,
}

/// Copy the pending run of untouched entry bytes in one move.
fn flush(out: &mut Vec<u8>, run: &mut Option<Run>, bytes: &[u8]) {
    if let Some(run) = run.take() {
        debug_assert_eq!(out.len(), run.at);
        out.extend_from_slice(&bytes[run.from..run.to]);
    }
}

/// Write one entry. The answer is the offset of its 40-byte stat block, which
/// the caller fills once it has written the file.
fn emit(
    out: &mut Vec<u8>,
    version: u32,
    prev: &[u8],
    path: &[u8],
    target: &Target,
    block_start: bool,
) -> usize {
    let start = out.len();
    out.extend(std::iter::repeat_n(0u8, 40));
    out[start + 24..start + 28].copy_from_slice(&target.mode.to_be_bytes());
    out.extend_from_slice(&target.oid);
    let name_len = u16::try_from(path.len()).unwrap_or(0x0fff).min(0x0fff);
    out.extend_from_slice(&name_len.to_be_bytes());
    if version == 4 {
        // Strip what the previous path holds past the common prefix, then
        // spell the rest, the way `ce_write_entry` does.
        //
        // The first entry of an `IEOT` block shares nothing with the previous
        // path, because git's threaded reader starts each block with no
        // previous name at all (`load_cache_entries_thread` passes NULL).
        // git's writer forces the same by breaking the first byte of the
        // previous name, which makes the common prefix empty; the entry then
        // strips the whole previous path and spells its own. Both readers get
        // the same name from that, and only from that.
        let common = match block_start {
            true => 0,
            false => prev.iter().zip(path).take_while(|(a, b)| a == b).count(),
        };
        out.extend_from_slice(&untracked::encode_varint(prev.len() - common));
        out.extend_from_slice(&path[common..]);
        out.push(0);
    } else {
        out.extend_from_slice(path);
        out.push(0);
        while (out.len() - start) % 8 != 0 {
            out.push(0);
        }
    }
    start
}

/// One extension: the signature, the size, the body.
fn push_ext(out: &mut Vec<u8>, sig: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(sig);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

/// The `IEOT` body: a version word, then one (offset, count) pair per block of
/// `BLOCK` entries. git reads the entries of each block on its own thread and
/// starts every block with no previous name, so the blocks here must be the
/// ones the emit forced a whole path at: `BLOCK` entries each, and the rest in
/// the last one.
fn ieot(offsets: &[u32]) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + offsets.len().div_ceil(BLOCK) * 8);
    data.extend_from_slice(&1u32.to_be_bytes());
    let mut at = 0;
    while at < offsets.len() {
        let n = BLOCK.min(offsets.len() - at);
        data.extend_from_slice(&offsets[at].to_be_bytes());
        data.extend_from_slice(&(n as u32).to_be_bytes());
        at += n;
    }
    data
}

/// The entries per `IEOT` block. Any partition works for git's reader, which
/// hands whole blocks to threads; this one is small enough to keep every
/// thread of a big index busy and big enough that the whole-path entry at each
/// boundary costs nothing worth counting.
const BLOCK: usize = 500;

// --- The checkout ---------------------------------------------------------

/// What the spliced checkout did.
pub enum Done {
    /// The klon holds the branch: its files, its index, and its `HEAD`.
    Spliced,
    /// The splice did not run and changed nothing. The caller writes the index
    /// bytes it holds and runs `git checkout`. The string says why.
    Refused(Refused),
}

/// The name of the temporary index that drives `git checkout-index`.
const SMALL: &str = "index.klon-splice";

/// Do what `git checkout -q --force <branch>` does to a klon that a spare
/// filled, without letting git rewrite the whole index (G4).
///
/// `index` is the spare's index, read but not yet written anywhere; `from` is
/// the commit the spare was made from, and the caller has already proved that
/// the spare's files and that index both match it. `real` is the canonical
/// path of the klon, which the untracked cache must name.
///
/// The three jobs of a checkout, in order:
///
/// 1. `git diff-tree` names every path that differs between the two commits.
///    `git checkout-index` writes the ones the branch holds, through a small
///    temporary index that carries only them, so git applies every filter,
///    permission, and symlink rule of a real checkout on 22 entries instead of
///    100,000. The paths the branch drops go by hand, with their empty parents.
/// 2. The index is spliced: every untouched entry keeps its bytes.
/// 3. `HEAD` becomes a symbolic ref to the branch, with the reflog line that
///    `git checkout` writes.
///
/// A refusal changes nothing at all and costs one `git diff-tree`. A failure
/// after the file writes is a real error: the caller rolls the `add` back.
pub fn checkout(
    klon: &Path,
    admin_dir: &Path,
    branch: &str,
    changes: &[Change],
    index: &[u8],
    real: &Path,
) -> crate::Result<Done> {
    for change in changes {
        // The attributes of a path decide how git writes it, and a branch that
        // changes them changes what a write means part way through.
        if change.path == b".gitattributes" || change.path.ends_with(b"/.gitattributes") {
            return Ok(Done::Refused("the diff changes the attributes of the tree"));
        }
        // A submodule needs a whole subsystem of its own.
        if change.to.as_ref().is_some_and(|to| to.mode == 0o160_000) {
            return Ok(Done::Refused("the diff names a submodule"));
        }
    }
    let mut step = Step::new();
    let mut spliced = match plan(index, changes, real) {
        Ok(spliced) => spliced,
        Err(why) => return Ok(Done::Refused(why)),
    };
    step.mark("plan");

    // Job 1a: the paths the branch drops, and every parent they leave empty.
    //
    // The removals go first, and the order is load bearing. The working tree
    // is still the one the spare's index describes, so every path resolves
    // through the real directories that index names. Afterwards it is not: a
    // branch that replaces the tracked `dir/child` with a symbolic link `dir`
    // to somewhere outside the klon would have `checkout-index` install that
    // link first, and a removal of `dir/child` would then follow it and delete
    // a file in golden or anywhere else. Removing first also settles every
    // file-against-directory swap, because git creates what it needs after the
    // old shape is gone.
    for change in changes.iter().filter(|c| c.to.is_none()) {
        remove(klon, &change.path)?;
    }
    // Job 1b: the paths the branch holds, written by git itself.
    let small = admin_dir.join(SMALL);
    let written: Vec<&Change> = changes.iter().filter(|c| c.to.is_some()).collect();
    if !written.is_empty() {
        std::fs::write(&small, small_index(&written))
            .map_err(crate::Error::io(format!("write {}", small.display())))?;
        let out = crate::git::run_bytes_env(
            klon,
            &["checkout-index", "--force", "--all"].map(std::ffi::OsStr::new),
            &[("GIT_INDEX_FILE", small.as_os_str())],
        );
        let _ = std::fs::remove_file(&small);
        out?;
    }
    step.mark("files");

    // Job 2: the stat data of what job 1 wrote, then the index.
    for (i, change) in changes.iter().enumerate() {
        let Some(_) = &change.to else { continue };
        let file = klon.join(std::ffi::OsStr::from_bytes(&change.path));
        let meta = std::fs::symlink_metadata(&file)
            .map_err(crate::Error::io(format!("read {}", file.display())))?;
        spliced.set_stat(i, &Stat::of(&meta));
    }
    let bytes = spliced.finish();
    step.mark("checksum");
    let target = admin_dir.join("index");
    let temp = admin_dir.join("index.klon-tmp");
    std::fs::write(&temp, bytes).map_err(crate::Error::io(format!("write {}", temp.display())))?;
    std::fs::rename(&temp, &target)
        .map_err(crate::Error::io(format!("move {}", temp.display())))?;
    step.mark("write");

    // Job 3: `HEAD`. The message is the one `git checkout` writes, so `git
    // checkout -` and `@{-1}` read the reflog of this klon the same way.
    let was = std::fs::read_to_string(admin_dir.join("HEAD")).unwrap_or_default();
    let was = was.trim();
    let was = was.strip_prefix("ref: refs/heads/").unwrap_or(was);
    let message = format!("checkout: moving from {was} to {branch}");
    let reference = format!("refs/heads/{branch}");
    crate::git::run(klon, &["symbolic-ref", "-m", &message, "HEAD", &reference])?;
    step.mark("head");
    Ok(Done::Spliced)
}

/// The `KLON_DEBUG=1` timing lines inside the splice, so a reader of the
/// per-step lines of `add` can see which part of it costs what.
struct Step {
    on: bool,
    last: std::time::Instant,
}

impl Step {
    fn new() -> Step {
        Step {
            on: crate::debug(),
            last: std::time::Instant::now(),
        }
    }

    fn mark(&mut self, name: &str) {
        if self.on {
            eprintln!(
                "klon: debug: add splice-{name} {:.1} ms",
                self.last.elapsed().as_secs_f64() * 1000.0
            );
        }
        self.last = std::time::Instant::now();
    }
}

/// Every path that differs between the two commits, with the mode and object
/// id the branch holds. `--raw` says both in the same line, so `add` runs one
/// `git diff-tree` for the two shortcuts that read it: the recorded lists of
/// G1 compare the names, and the splice writes the entries.
pub fn diff(klon: &Path, from: &str, reference: &str) -> Result<Vec<Change>, Refused> {
    let out = crate::git::run_bytes_env(
        klon,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--raw",
            "--no-renames",
            from,
            reference,
        ]
        .map(std::ffi::OsStr::new),
        &[],
    )
    .map_err(|_| "the two commits do not diff")?;
    // Each record is `:<srcmode> <dstmode> <srcoid> <dstoid> <status>` and
    // then, after a NUL, the path.
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        let path = fields.next().ok_or("a diff record without a path")?;
        let meta = std::str::from_utf8(meta).map_err(|_| "a diff record that is not text")?;
        let meta = meta
            .strip_prefix(':')
            .ok_or("a diff record without a mode")?;
        let mut parts = meta.split(' ');
        let mut next = || parts.next().ok_or("a short diff record");
        let src = next()?;
        let dst = next()?;
        let _src_oid = next()?;
        let dst_oid = next()?;
        let status = next()?;
        let _ = src;
        let to = match status {
            "D" => None,
            "A" | "M" | "T" => Some(Target {
                mode: u32::from_str_radix(dst, 8).map_err(|_| "a diff record with a bad mode")?,
                oid: decode_hex(dst_oid).ok_or("a diff record with a bad object id")?,
            }),
            _ => return Err("a diff status the splice does not know"),
        };
        changes.push(Change {
            path: path.to_vec(),
            to,
        });
    }
    Ok(changes)
}

/// An index of version 2 holding only `changes`, in path order, with no
/// extensions. `git checkout-index --all` reads it and writes exactly those
/// files, so git owns every conversion rule and the read costs 22 entries
/// instead of 100,000.
fn small_index(changes: &[&Change]) -> Vec<u8> {
    let mut order: Vec<&&Change> = changes.iter().collect();
    order.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = Vec::with_capacity(64 + order.len() * 96);
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(order.len() as u32).to_be_bytes());
    for change in &order {
        let target = change.to.as_ref().expect("the caller filtered");
        emit(&mut out, 2, b"", &change.path, target, true);
    }
    let digest = sha1::Sha1::digest(&out);
    out.extend_from_slice(&digest);
    out
}

/// Remove one path the branch drops, then every parent directory it leaves
/// empty, the way `unlink_entry` and `remove_scheduled_dirs` do. A parent that
/// still holds anything stays: `rmdir` refuses it, which is the answer.
fn remove(klon: &Path, path: &[u8]) -> crate::Result<()> {
    let file = klon.join(std::ffi::OsStr::from_bytes(path));
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(crate::Error::io(format!("remove {}", file.display()))(err)),
    }
    let mut dir = file.parent();
    while let Some(at) = dir {
        if at == klon || std::fs::remove_dir(at).is_err() {
            break;
        }
        dir = at.parent();
    }
    Ok(())
}

/// A hex object id as raw bytes. None when the text is not hex, or is the
/// all-zero id that a diff writes for a side that holds nothing.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || text.is_empty() {
        return None;
    }
    let bytes: Option<Vec<u8>> = text
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect();
    bytes.filter(|bytes| bytes.iter().any(|b| *b != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Vec<u8> {
        vec![byte; 20]
    }

    /// Build an index of `paths` by hand, in the version given.
    fn index(version: u32, paths: &[&str], untr: bool, ieot: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&(paths.len() as u32).to_be_bytes());
        let mut prev: &[u8] = b"";
        let mut offsets = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            offsets.push(out.len() as u32);
            let target = Target {
                mode: 0o100_644,
                oid: oid((i % 250) as u8 + 1),
            };
            let at = emit(
                &mut out,
                version,
                prev,
                path.as_bytes(),
                &target,
                i % BLOCK == 0,
            );
            // A plausible stat block, so a copied entry is visibly copied.
            Stat {
                ctime: (100 + i as u32, 0),
                mtime: (200 + i as u32, 0),
                dev: 7,
                ino: 900 + i as u32,
                uid: 1000,
                gid: 1000,
                size: 3 + i as u32,
            }
            .write(&mut out[at..at + 40]);
            prev = path.as_bytes();
        }
        let extensions_at = out.len();
        if untr {
            let mut data = untracked::encode_varint(b"Location /old, system Linux".len());
            data.extend_from_slice(b"Location /old, system Linux");
            data.extend_from_slice(&[0u8; 44]);
            data.push(0);
            push_ext(&mut out, b"UNTR", &data);
        }
        if ieot {
            push_ext(&mut out, b"IEOT", &super::ieot(&offsets));
        }
        let mut data = (extensions_at as u32).to_be_bytes().to_vec();
        let mut hasher = sha1::Sha1::new();
        let mut walk = extensions_at;
        while walk < out.len() {
            hasher.update(&out[walk..walk + 8]);
            let size = u32::from_be_bytes(out[walk + 4..walk + 8].try_into().unwrap()) as usize;
            walk += 8 + size;
        }
        data.extend_from_slice(&hasher.finalize());
        push_ext(&mut out, b"EOIE", &data);
        let digest = sha1::Sha1::digest(&out);
        out.extend_from_slice(&digest);
        out
    }

    /// The paths an index holds, in order, read back through the parser.
    fn paths_of(bytes: &[u8]) -> Vec<String> {
        let layout = untracked::parse(bytes).expect("parses");
        let end = bytes.len() - layout.hash_len;
        let mut at = 12;
        let mut prev: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        for _ in 0..layout.count {
            let start = at;
            let (_, flags, name_at) = read_head(bytes, start, 20, layout.version, end).unwrap();
            at = name_at;
            let mut path = Vec::new();
            if layout.version == 4 {
                let (strip, n) = untracked::decode_varint(&bytes[at..end]).unwrap();
                at += n;
                let nul = bytes[at..end].iter().position(|b| *b == 0).unwrap();
                path.extend_from_slice(&prev[..prev.len() - strip]);
                path.extend_from_slice(&bytes[at..at + nul]);
                at += nul + 1;
            } else {
                let nul = bytes[at..end].iter().position(|b| *b == 0).unwrap();
                path.extend_from_slice(&bytes[at..at + nul]);
                at += nul + 1;
                at = start + (at - start).div_ceil(8) * 8;
            }
            assert_eq!((flags & 0x0fff) as usize, path.len());
            prev.clone_from(&path);
            out.push(String::from_utf8(path).unwrap());
            let _ = start;
        }
        assert_eq!(at, layout.extensions_at, "the walk ends at the extensions");
        out
    }

    /// The paths an index holds, read the way `load_cache_entries_thread`
    /// reads them: every `IEOT` block starts with no previous name, so the
    /// first entry of a block must spell its whole path. A block table that
    /// does not match the entries desynchronises git's reader, which then
    /// dies with `malformed name field` or `unknown index entry format`.
    fn paths_of_threaded(bytes: &[u8]) -> Vec<String> {
        let layout = untracked::parse(bytes).expect("parses");
        let ext = layout
            .exts
            .iter()
            .find(|e| &e.sig == b"IEOT")
            .expect("IEOT is present");
        let data = &bytes[ext.at..ext.at + ext.size];
        assert_eq!(u32::from_be_bytes(data[..4].try_into().unwrap()), 1);
        let end = bytes.len() - layout.hash_len;
        let mut out = Vec::new();
        let mut walk = 4;
        while walk < data.len() {
            let mut at = u32::from_be_bytes(data[walk..walk + 4].try_into().unwrap()) as usize;
            let nr = u32::from_be_bytes(data[walk + 4..walk + 8].try_into().unwrap()) as usize;
            walk += 8;
            // The block starts with an empty previous name, as git does.
            let mut prev: Vec<u8> = Vec::new();
            for _ in 0..nr {
                let start = at;
                let (_, flags, name_at) = read_head(bytes, start, 20, layout.version, end).unwrap();
                at = name_at;
                let mut path = Vec::new();
                if layout.version == 4 {
                    let (strip, n) = untracked::decode_varint(&bytes[at..end]).unwrap();
                    at += n;
                    // git's threaded reader has no previous name at a block
                    // start, so it copies nothing however big the strip is.
                    let copy = prev.len().saturating_sub(strip);
                    path.extend_from_slice(&prev[..copy]);
                    let nul = bytes[at..end].iter().position(|b| *b == 0).unwrap();
                    path.extend_from_slice(&bytes[at..at + nul]);
                    at += nul + 1;
                } else {
                    let nul = bytes[at..end].iter().position(|b| *b == 0).unwrap();
                    path.extend_from_slice(&bytes[at..at + nul]);
                    at += nul + 1;
                    at = start + (at - start).div_ceil(8) * 8;
                }
                // git reads the whole length from the flags, so it must agree.
                assert_eq!((flags & 0x0fff) as usize, path.len());
                prev.clone_from(&path);
                out.push(String::from_utf8(path).unwrap());
            }
        }
        out
    }

    fn change(path: &str, to: Option<u8>) -> Change {
        Change {
            path: path.as_bytes().to_vec(),
            to: to.map(|byte| Target {
                mode: 0o100_644,
                oid: oid(byte),
            }),
        }
    }

    /// The trailer and every extension of a spliced index must read back.
    fn check(bytes: &[u8]) -> untracked::Layout {
        let layout = untracked::parse(bytes).expect("the spliced index parses");
        let body = bytes.len() - layout.hash_len;
        assert_eq!(
            &bytes[body..],
            sha1::Sha1::digest(&bytes[..body]).as_slice(),
            "the trailer covers the whole buffer"
        );
        layout
    }

    #[test]
    fn a_modified_path_keeps_every_other_entry_byte_for_byte() {
        for version in [2u32, 3, 4] {
            let before = index(
                version,
                &["a.txt", "b/c.txt", "b/d.txt", "z.txt"],
                true,
                true,
            );
            let changes = [change("b/c.txt", Some(0x77))];
            let mut spliced = plan(&before, &changes, Path::new("/new")).expect("splices");
            spliced.set_stat(
                0,
                &Stat {
                    mtime: (555, 6),
                    size: 42,
                    ..Stat::default()
                },
            );
            let after = spliced.finish();
            check(&after);
            assert_eq!(
                paths_of(&after),
                vec!["a.txt", "b/c.txt", "b/d.txt", "z.txt"],
                "version {version}"
            );
            // The new object id and the new stat data landed.
            assert!(
                after.windows(20).any(|w| w == oid(0x77).as_slice()),
                "version {version}"
            );
            assert!(!after.windows(20).any(|w| w == oid(2).as_slice()));
            // The untracked cache moved with it.
            assert!(after
                .windows(9)
                .any(|w| w == b"/new, sys".as_slice() || w == b"Location ".as_slice()));
            assert!(!after.windows(4).any(|w| w == b"/old".as_slice()));

            // The point of the splice: every other entry keeps its bytes. One
            // path changed and kept its length, so the entry region has the
            // same size and differs only inside that one entry.
            let old = untracked::parse(&before).unwrap();
            let new = untracked::parse(&after).unwrap();
            assert_eq!(new.extensions_at, old.extensions_at, "version {version}");
            let differ: Vec<usize> = (12..old.extensions_at)
                .filter(|i| before[*i] != after[*i])
                .collect();
            assert!(!differ.is_empty(), "version {version}: something changed");
            let (first, last) = (differ[0], differ[differ.len() - 1]);
            assert!(
                last - first < 62,
                "version {version}: {} bytes changed, from {first} to {last}, \
                 which is more than one entry's fixed fields",
                differ.len()
            );
        }
    }

    #[test]
    fn an_added_path_lands_in_order_and_the_next_entry_is_respelled() {
        for version in [2u32, 3, 4] {
            let before = index(version, &["a.txt", "b/c.txt", "z.txt"], true, true);
            let changes = [change("b/b.txt", Some(9)), change("zz.txt", Some(10))];
            let mut spliced = plan(&before, &changes, Path::new("/new")).expect("splices");
            spliced.set_stat(0, &Stat::default());
            spliced.set_stat(1, &Stat::default());
            let after = spliced.finish();
            let layout = check(&after);
            assert_eq!(layout.count, 5, "version {version}");
            assert_eq!(
                paths_of(&after),
                vec!["a.txt", "b/b.txt", "b/c.txt", "z.txt", "zz.txt"],
                "version {version}"
            );
        }
    }

    #[test]
    fn a_dropped_path_goes_and_the_next_entry_is_respelled() {
        for version in [2u32, 3, 4] {
            let before = index(
                version,
                &["a.txt", "b/c.txt", "b/d.txt", "z.txt"],
                true,
                true,
            );
            let changes = [change("b/c.txt", None)];
            let spliced = plan(&before, &changes, Path::new("/new")).expect("splices");
            let after = spliced.finish();
            let layout = check(&after);
            assert_eq!(layout.count, 3, "version {version}");
            assert_eq!(
                paths_of(&after),
                vec!["a.txt", "b/d.txt", "z.txt"],
                "version {version}"
            );
        }
    }

    #[test]
    fn the_first_and_the_last_entry_can_change_too() {
        for version in [2u32, 3, 4] {
            let before = index(version, &["a.txt", "m.txt", "z.txt"], true, true);
            let changes = [
                change("a.txt", None),
                change("z.txt", Some(4)),
                change("0.txt", Some(5)),
            ];
            let mut spliced = plan(&before, &changes, Path::new("/new")).expect("splices");
            for i in 0..3 {
                spliced.set_stat(i, &Stat::default());
            }
            let after = spliced.finish();
            check(&after);
            assert_eq!(
                paths_of(&after),
                vec!["0.txt", "m.txt", "z.txt"],
                "version {version}"
            );
        }
    }

    /// An index big enough for more than one `IEOT` block. git's threaded
    /// reader starts every block with no previous name, so a version 4 index
    /// whose block boundaries carry a prefix reads as nonsense on a 100k tree
    /// and reads correctly on a small one. The 100k fixture caught that; this
    /// test catches it in a second.
    #[test]
    fn every_block_of_a_version_4_index_reads_on_its_own() {
        let names: Vec<String> = (0..BLOCK * 3 + 7)
            .map(|i| format!("d{:03}/f{i:06}.txt", i % 97))
            .collect();
        let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        for version in [2u32, 4] {
            let before = index(version, &sorted, true, true);
            // A modify, a drop, and an insert, spread over the blocks.
            let changes = [
                change(sorted[BLOCK], Some(0x31)),
                change(sorted[BLOCK * 2 - 1], None),
                change("aaa-first.txt", Some(0x32)),
                change("zzz-last.txt", Some(0x33)),
            ];
            let mut spliced = plan(&before, &changes, Path::new("/new")).expect("splices");
            for i in 0..changes.len() {
                spliced.set_stat(i, &Stat::default());
            }
            let after = spliced.finish();
            check(&after);
            let mut want: Vec<String> = sorted.iter().map(|s| (*s).to_string()).collect();
            want.retain(|p| p != sorted[BLOCK * 2 - 1]);
            want.push("aaa-first.txt".into());
            want.push("zzz-last.txt".into());
            want.sort();
            assert_eq!(paths_of(&after), want, "version {version}: one reader");
            assert_eq!(
                paths_of_threaded(&after),
                want,
                "version {version}: the threaded reader"
            );
        }
    }

    #[test]
    fn the_offset_table_names_the_new_places() {
        let before = index(4, &["a.txt", "b.txt", "c.txt"], true, true);
        let changes = [change("0.txt", Some(6))];
        let mut spliced = plan(&before, &changes, Path::new("/new")).expect("splices");
        spliced.set_stat(0, &Stat::default());
        let after = spliced.finish();
        let layout = check(&after);
        let ext = layout
            .exts
            .iter()
            .find(|e| &e.sig == b"IEOT")
            .expect("IEOT survives");
        let data = &after[ext.at..ext.at + ext.size];
        assert_eq!(u32::from_be_bytes(data[..4].try_into().unwrap()), 1);
        let mut total = 0u32;
        let mut walk = 4;
        let mut first = None;
        while walk < data.len() {
            let offset = u32::from_be_bytes(data[walk..walk + 4].try_into().unwrap());
            total += u32::from_be_bytes(data[walk + 4..walk + 8].try_into().unwrap());
            first.get_or_insert(offset);
            walk += 8;
        }
        assert_eq!(total, 4, "every entry is covered");
        assert_eq!(first, Some(12), "the first block starts at the first entry");
    }

    #[test]
    fn the_cached_tree_is_dropped_and_the_end_marker_is_rebuilt() {
        let mut before = index(4, &["a.txt", "b.txt"], true, true);
        // Put a TREE extension in front of the others and rebuild the file.
        let layout = untracked::parse(&before).unwrap();
        let head = before[..layout.extensions_at].to_vec();
        let rest = before[layout.extensions_at..before.len() - 20].to_vec();
        before.clear();
        before.extend_from_slice(&head);
        push_ext(&mut before, b"TREE", b"\x002 0\n01234567890123456789");
        before.extend_from_slice(&rest);
        // The EOIE hash of the rebuilt file is stale, which is fine: the
        // splice recomputes it and never reads it.
        let digest = sha1::Sha1::digest(&before);
        before.extend_from_slice(&digest);

        let spliced = plan(&before, &[change("a.txt", None)], Path::new("/new")).expect("splices");
        let after = spliced.finish();
        let layout = check(&after);
        assert!(
            !layout.exts.iter().any(|e| &e.sig == b"TREE"),
            "the cached tree is dropped"
        );
        let eoie = layout.exts.iter().find(|e| &e.sig == b"EOIE").unwrap();
        let offset = u32::from_be_bytes(after[eoie.at..eoie.at + 4].try_into().unwrap()) as usize;
        assert_eq!(offset, layout.extensions_at, "the end marker is correct");
        // The hash covers the headers of the extensions before it.
        let mut hasher = sha1::Sha1::new();
        let mut walk = layout.extensions_at;
        while walk < eoie.at - 8 {
            hasher.update(&after[walk..walk + 8]);
            let size = u32::from_be_bytes(after[walk + 4..walk + 8].try_into().unwrap()) as usize;
            walk += 8 + size;
        }
        assert_eq!(
            &after[eoie.at + 4..eoie.at + 24],
            hasher.finalize().as_slice()
        );
    }

    #[test]
    fn the_splice_refuses_what_it_cannot_certainly_do() {
        let plain = index(4, &["a.txt", "b.txt"], true, true);
        // An index that is not one.
        assert!(plan(b"not an index", &[], Path::new("/new")).is_err());
        // Two changes on one path.
        assert_eq!(
            plan(
                &plain,
                &[change("a.txt", Some(1)), change("a.txt", Some(2))],
                Path::new("/new")
            )
            .err(),
            Some("two changes name the same path")
        );
        // A drop of a path the index does not hold.
        assert_eq!(
            plan(&plain, &[change("gone.txt", None)], Path::new("/new")).err(),
            Some("the branch drops a path the index does not hold")
        );
        // A submodule and a mode the splice does not write.
        assert_eq!(
            plan(
                &plain,
                &[Change {
                    path: b"sub".to_vec(),
                    to: Some(Target {
                        mode: 0o160_000,
                        oid: oid(3)
                    }),
                }],
                Path::new("/new")
            )
            .err(),
            Some("a change carries a mode the splice does not write")
        );
        // A diff bigger than the splice places.
        let many: Vec<Change> = (0..MAX_CHANGES + 1)
            .map(|i| change(&format!("x{i}.txt"), Some(1)))
            .collect();
        assert_eq!(
            plan(&plain, &many, Path::new("/new")).err(),
            Some("the diff is larger than the splice places")
        );
    }

    #[test]
    fn an_extension_the_splice_does_not_know_refuses() {
        let mut before = index(4, &["a.txt"], false, false);
        let layout = untracked::parse(&before).unwrap();
        before.truncate(layout.extensions_at);
        push_ext(&mut before, b"REUC", b"junk");
        let digest = sha1::Sha1::digest(&before);
        before.extend_from_slice(&digest);
        assert_eq!(
            plan(&before, &[change("a.txt", Some(2))], Path::new("/new")).err(),
            Some("the index carries an extension the splice does not know")
        );
    }

    #[test]
    fn a_merge_stage_refuses() {
        let mut before = index(2, &["a.txt", "b.txt"], false, false);
        // Set stage 2 on the first entry, then fix the trailer.
        let flags_at = 12 + 40 + 20;
        let flags = u16::from_be_bytes(before[flags_at..flags_at + 2].try_into().unwrap());
        before[flags_at..flags_at + 2].copy_from_slice(&(flags | 0x2000).to_be_bytes());
        let body = before.len() - 20;
        let digest = sha1::Sha1::digest(&before[..body]);
        before[body..].copy_from_slice(&digest);
        assert_eq!(
            plan(&before, &[change("b.txt", Some(9))], Path::new("/new")).err(),
            Some("an entry carries a stage or an extended flag")
        );
    }

    #[test]
    fn the_untracked_cache_is_kept_when_the_worktree_is_the_same() {
        // No UNTR at all: the splice still works and writes none.
        let before = index(4, &["a.txt"], false, true);
        let spliced = plan(&before, &[change("a.txt", None)], Path::new("/new")).expect("splices");
        let after = spliced.finish();
        let layout = check(&after);
        assert_eq!(layout.count, 0);
        assert!(!layout.exts.iter().any(|e| &e.sig == b"UNTR"));
    }
}
