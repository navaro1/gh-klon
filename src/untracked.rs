//! The untracked cache of a spare's index (R11, R12; G1).
//!
//! git keeps the untracked cache in the `UNTR` extension of the index. The
//! extension starts with an identity string, `Location <worktree>, system
//! <uname>`, and git drops the whole cache when that string differs from the
//! worktree it runs in (`ident_in_untracked` in `dir.c`). A spare is built at
//! `../<repo>.wt/.spare.tmp` and renamed twice before it serves an `add`, so a
//! cache built there names the wrong location, and the first `git status` in
//! the klon would scan all 100k paths again.
//!
//! `relocate` rewrites that one string in the index bytes: the identity, the
//! extension size, and the checksum trailer that git verifies on every read.
//! Every index entry stays as it was, so this is not the entry splice of G4.
//! An index that does not parse, or one without the extension, is left alone
//! and the caller keeps the bytes it had.

use sha1::Digest;
use std::path::Path;

/// The result of `relocate`: whether the bytes changed.
#[derive(Debug, PartialEq, Eq)]
pub enum Relocated {
    /// The identity now names `worktree`. The trailer is recomputed.
    Patched,
    /// No `UNTR` extension: there is no cache to relocate.
    NoCache,
    /// The bytes are not an index this code reads. Nothing changed.
    Unreadable,
}

/// Point the untracked cache in `bytes` at `worktree`.
///
/// `worktree` must be the real path of the klon, the way git computes it
/// (`strbuf_realpath`): a symlink in the path would make the check fail and
/// git would rebuild the cache, which is a slow first `status`, never a wrong
/// one.
pub fn relocate(bytes: &mut Vec<u8>, worktree: &Path) -> Relocated {
    let Some(layout) = parse(bytes) else {
        return Relocated::Unreadable;
    };
    let Some((untr_at, untr_len)) = layout.untr else {
        return Relocated::NoCache;
    };
    // The identity: a varint length, then the string.
    let data = &bytes[untr_at..untr_at + untr_len];
    let Some((ident_len, varint_len)) = decode_varint(data) else {
        return Relocated::Unreadable;
    };
    if varint_len + ident_len > data.len() {
        return Relocated::Unreadable;
    }
    let old_ident = &data[varint_len..varint_len + ident_len];
    // Keep git's own system suffix; only the location changes.
    let Some(comma) = find(old_ident, b", system ") else {
        return Relocated::Unreadable;
    };
    let mut ident = Vec::with_capacity(ident_len + 64);
    ident.extend_from_slice(b"Location ");
    ident.extend_from_slice(worktree.as_os_str().as_encoded_bytes());
    ident.extend_from_slice(&old_ident[comma..]);
    let mut head = encode_varint(ident.len());
    head.extend_from_slice(&ident);
    let old_head_len = varint_len + ident_len;
    let delta = head.len() as i64 - old_head_len as i64;

    // Splice the new identity in and fix the extension size.
    bytes.splice(untr_at..untr_at + old_head_len, head);
    let new_len = (untr_len as i64 + delta) as u32;
    bytes[untr_at - 4..untr_at].copy_from_slice(&new_len.to_be_bytes());

    // `EOIE` holds a hash over every extension header before it, so a changed
    // size changes that hash too. The offset it holds is unchanged: the
    // entries are.
    if let Some(eoie_at) = layout.eoie {
        let eoie_at = (eoie_at as i64 + delta) as usize;
        let mut hasher = sha1::Sha1::new();
        let mut at = layout.extensions_at;
        while at + 8 <= eoie_at - 8 {
            hasher.update(&bytes[at..at + 8]);
            let size = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            at += 8 + size;
        }
        let digest = hasher.finalize();
        bytes[eoie_at + 4..eoie_at + 4 + 20].copy_from_slice(&digest);
    }

    // The trailer: the hash of everything before it.
    let body = bytes.len() - layout.hash_len;
    match layout.hash_len {
        20 => {
            let digest = sha1::Sha1::digest(&bytes[..body]);
            bytes[body..].copy_from_slice(&digest);
        }
        _ => {
            let digest = sha2::Sha256::digest(&bytes[..body]);
            bytes[body..].copy_from_slice(&digest);
        }
    }
    Relocated::Patched
}

/// Where the parts of an index sit.
struct Layout {
    /// The first byte after the last entry.
    extensions_at: usize,
    /// The `UNTR` data: its start and length.
    untr: Option<(usize, usize)>,
    /// The start of the `EOIE` data, when the extension is present.
    eoie: Option<usize>,
    /// 20 for SHA-1, 32 for SHA-256.
    hash_len: usize,
}

/// Walk the entries and the extension headers. SHA-1 is tried first; a
/// SHA-256 index leaves the SHA-1 walk off the end and is tried second.
fn parse(bytes: &[u8]) -> Option<Layout> {
    parse_with(bytes, 20).or_else(|| parse_with(bytes, 32))
}

fn parse_with(bytes: &[u8], hash_len: usize) -> Option<Layout> {
    if bytes.len() < 12 + hash_len || &bytes[..4] != b"DIRC" {
        return None;
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    let count = u32::from_be_bytes(bytes[8..12].try_into().ok()?) as usize;
    if !(2..=4).contains(&version) {
        return None;
    }
    let end = bytes.len() - hash_len;
    let mut at = 12;
    for _ in 0..count {
        let start = at;
        // ctime, mtime, dev, ino, mode, uid, gid, size: 40 bytes; the object
        // name; two flag bytes; two more when the extended flag is set.
        let flags_at = start + 40 + hash_len;
        if flags_at + 2 > end {
            return None;
        }
        let flags = u16::from_be_bytes(bytes[flags_at..flags_at + 2].try_into().ok()?);
        at = flags_at + 2;
        if version >= 3 && flags & 0x4000 != 0 {
            at += 2;
        }
        if version == 4 {
            // A varint says how much of the previous name to strip.
            let (_, n) = decode_varint(&bytes[at..end])?;
            at += n;
        }
        let nul = bytes[at..end].iter().position(|b| *b == 0)?;
        at += nul + 1;
        if version < 4 {
            // The entry is padded with NULs to a multiple of eight bytes.
            let len = at - start;
            at = start + len.div_ceil(8) * 8;
        }
        if at > end {
            return None;
        }
    }
    let extensions_at = at;
    let mut untr = None;
    let mut eoie = None;
    while at + 8 <= end {
        let sig = &bytes[at..at + 4];
        if !sig.iter().all(|b| b.is_ascii_uppercase()) {
            return None;
        }
        let size = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        if at + 8 + size > end {
            return None;
        }
        match sig {
            b"UNTR" => untr = Some((at + 8, size)),
            b"EOIE" => eoie = Some(at + 8),
            _ => {}
        }
        at += 8 + size;
    }
    if at != end {
        return None;
    }
    Some(Layout {
        extensions_at,
        untr,
        eoie,
        hash_len,
    })
}

/// git's varint (`varint.c`): seven bits per byte, high bit set on every byte
/// but the last, and each continuation adds one. The answer is the value and
/// the number of bytes read.
fn decode_varint(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut at = 0;
    let mut c = *bytes.get(at)?;
    at += 1;
    let mut value = (c & 127) as usize;
    while c & 128 != 0 {
        value = value.checked_add(1)?;
        if value >> (usize::BITS - 7) != 0 {
            return None;
        }
        c = *bytes.get(at)?;
        at += 1;
        value = (value << 7) + (c & 127) as usize;
    }
    Some((value, at))
}

fn encode_varint(mut value: usize) -> Vec<u8> {
    let mut out = vec![(value & 127) as u8];
    value >>= 7;
    while value != 0 {
        value -= 1;
        out.push(128 | (value & 127) as u8);
        value >>= 7;
    }
    out.reverse();
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// What one `git status --porcelain=v1 -z --untracked-files=normal` document
/// says that `add` needs (G1).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The `?? <path>` entries: the untracked, non-ignored paths, with a
    /// fully untracked directory as one name with a trailing slash. Bytes,
    /// because a name on Unix is not always UTF-8.
    pub untracked: Vec<Vec<u8>>,
    /// True when a `.gitignore` is modified, added, deleted, or staged: the
    /// ignore rules of that tree differ from its commit, so a list of its
    /// untracked paths says nothing about another commit's rules.
    pub rules_dirty: bool,
}

/// Read a `-z` porcelain document. A rename or copy entry carries a second
/// path after its own NUL, which the parser skips.
pub fn scan_porcelain(status: &[u8]) -> Scan {
    let mut scan = Scan::default();
    let mut fields = status.split(|b| *b == 0);
    while let Some(entry) = fields.next() {
        if entry.len() < 3 {
            continue;
        }
        let (code, path) = entry.split_at(3);
        if matches!(code[0], b'R' | b'C') || matches!(code[1], b'R' | b'C') {
            fields.next();
        }
        if code == b"?? " {
            scan.untracked.push(path.to_vec());
        } else if path.ends_with(b"/.gitignore") || path == b".gitignore" {
            scan.rules_dirty = true;
        }
    }
    scan
}

/// True when `path` names a `.gitignore` at any depth.
pub fn is_ignore_file(path: &[u8]) -> bool {
    path == b".gitignore" || path.ends_with(b"/.gitignore")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_varint_round_trips_like_git() {
        for value in [0usize, 1, 127, 128, 129, 255, 256, 16383, 16384, 100_000] {
            let bytes = encode_varint(value);
            assert_eq!(decode_varint(&bytes), Some((value, bytes.len())), "{value}");
        }
        // The two-byte boundary: git encodes 128 as 0x80 0x00.
        assert_eq!(encode_varint(128), vec![0x80, 0x00]);
        assert_eq!(encode_varint(127), vec![0x7f]);
    }

    /// A version 2 index with one entry and one UNTR extension, built by hand.
    fn index_with_ident(ident: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        let start = bytes.len();
        bytes.extend_from_slice(&[0u8; 40]);
        bytes.extend_from_slice(&[0xabu8; 20]);
        bytes.extend_from_slice(&5u16.to_be_bytes());
        bytes.extend_from_slice(b"a.txt\0");
        while (bytes.len() - start) % 8 != 0 {
            bytes.push(0);
        }
        let mut data = encode_varint(ident.len());
        data.extend_from_slice(ident);
        data.extend_from_slice(&[0u8; 44]);
        data.extend_from_slice(b"\0");
        bytes.extend_from_slice(b"UNTR");
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&data);
        let digest = sha1::Sha1::digest(&bytes);
        bytes.extend_from_slice(&digest);
        bytes
    }

    #[test]
    fn relocate_rewrites_the_location_and_the_trailer() {
        let mut bytes = index_with_ident(b"Location /old/place, system Linux");
        let before = bytes.clone();
        assert_eq!(
            relocate(&mut bytes, Path::new("/new/much/longer/place")),
            Relocated::Patched
        );
        assert_ne!(bytes, before);
        assert!(find(&bytes, b"Location /new/much/longer/place, system Linux").is_some());
        assert!(find(&bytes, b"/old/place").is_none());
        // The trailer is the SHA-1 of the rest.
        let body = bytes.len() - 20;
        assert_eq!(
            &bytes[body..],
            sha1::Sha1::digest(&bytes[..body]).as_slice()
        );
        // The layout still parses and the size field matches.
        let layout = parse(&bytes).expect("parses");
        let (at, len) = layout.untr.expect("UNTR");
        assert_eq!(at + len, body);
    }

    #[test]
    fn relocate_leaves_an_index_without_the_extension_alone() {
        let mut bytes = index_with_ident(b"Location /old/place, system Linux");
        // Drop the extension: keep the entries, recompute the trailer.
        let layout = parse(&bytes).unwrap();
        bytes.truncate(layout.extensions_at);
        let digest = sha1::Sha1::digest(&bytes);
        bytes.extend_from_slice(&digest);
        let before = bytes.clone();
        assert_eq!(relocate(&mut bytes, Path::new("/x")), Relocated::NoCache);
        assert_eq!(bytes, before);
    }

    #[test]
    fn the_porcelain_parser_keeps_the_untracked_entries_and_skips_rename_sources() {
        let status = b"?? new.txt\0 M edited.txt\0R  moved.txt\0old.txt\0?? dir/\0A  added.txt\0";
        let scan = scan_porcelain(status);
        assert_eq!(scan.untracked, vec![b"new.txt".to_vec(), b"dir/".to_vec()]);
        assert!(!scan.rules_dirty);
        assert_eq!(scan_porcelain(b""), Scan::default());
        // A non-UTF-8 name survives as bytes.
        assert_eq!(
            scan_porcelain(b"?? a\xffb\0").untracked,
            vec![b"a\xffb".to_vec()]
        );
        // A changed ignore file marks the rules dirty; an untracked one does
        // not, because the untracked list already reflects it.
        assert!(scan_porcelain(b" M sub/.gitignore\0").rules_dirty);
        assert!(scan_porcelain(b"D  .gitignore\0").rules_dirty);
        assert!(!scan_porcelain(b"?? .gitignore\0").rules_dirty);
        assert!(is_ignore_file(b"a/b/.gitignore") && !is_ignore_file(b"a/.gitignore.bak"));
    }

    #[test]
    fn relocate_refuses_bytes_that_are_not_an_index() {
        let mut bytes = b"not an index at all".to_vec();
        let before = bytes.clone();
        assert_eq!(relocate(&mut bytes, Path::new("/x")), Relocated::Unreadable);
        assert_eq!(bytes, before);
    }
}
