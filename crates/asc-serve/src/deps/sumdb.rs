//! Verifying an answer of the Go checksum database.
//!
//! `GET /lookup/<module>@<version>` returns the record (the `go.sum` lines of that module version), the
//! signed tree head it was logged under, and nothing that proves the record is in that tree. Believing the
//! lines would be believing whoever answered. This module checks what Go's own client checks:
//!
//! 1. the signed note: the tree head (`go.sum database tree`, size, root hash) carries an Ed25519
//!    signature by the database's key (`golang.org/x/mod/sumdb/note` format), whose name and key hash match
//!    the configured verifier key;
//! 2. inclusion: the record's leaf hash (`SHA-256(0x00 || record)`), at the record number the lookup gives,
//!    hashes up to the signed root, using tiles (`/tile/8/<level>/<n>`) of the log for the nodes of the
//!    proof (RFC 6962 / `golang.org/x/mod/sumdb/tlog`).
//!
//! Everything here is pure: tiles are fetched by the caller from the list [`tiles_needed`] gives and handed
//! back to [`verify_inclusion`].

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

/// The key of `sum.golang.org`, in the `name+hash+key` form of `golang.org/x/mod/sumdb/note`.
pub const DEFAULT_KEY: &str =
    "sum.golang.org+033de0ae+Ac4zctda0e5eza+HJyk9SxEdh+s3Ux18htTTAD8OuAn8";

/// Tiles are 8 hashes high, 256 wide.
const TILE_H: u32 = 8;
const TILE_W: u64 = 1 << TILE_H;

/// Most tiles one verification may ask for.
pub const MAX_TILES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    hash: [u8; 4],
    pubkey: [u8; 32],
}

/// Reads a verifier key, checking that the hash in its text is the one its name and key give.
pub fn parse_key(text: &str) -> Result<Key, String> {
    let mut it = text.splitn(3, '+');
    let (name, hash, enc) = (it.next(), it.next(), it.next());
    let (Some(name), Some(hash), Some(enc)) = (name, hash, enc) else {
        return Err("a sumdb key is name+hash+key".into());
    };
    if name.is_empty() || name.contains(char::is_whitespace) || hash.len() != 8 {
        return Err("a sumdb key is name+hash+key".into());
    }
    let want = u32::from_str_radix(hash, 16)
        .map_err(|_| "the hash of a sumdb key is not hex".to_owned())?;
    let raw = B64
        .decode(enc)
        .map_err(|_| "the key of a sumdb key is not base64".to_owned())?;
    let [alg, key @ ..] = raw.as_slice() else {
        return Err("the key of a sumdb key is empty".into());
    };
    if *alg != 1 || key.len() != 32 {
        return Err("a sumdb key is an Ed25519 key (algorithm 1, 32 bytes)".into());
    }
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(key);
    let got = key_hash(name, &pubkey);
    if u32::from_be_bytes(got) != want {
        return Err("the hash of the sumdb key is not the hash of its name and key".into());
    }
    Ok(Key {
        name: name.to_owned(),
        hash: got,
        pubkey,
    })
}

fn key_hash(name: &str, pubkey: &[u8; 32]) -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(b"\n");
    h.update([1u8]);
    h.update(pubkey);
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

/// A parsed `lookup` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    /// The record number: the leaf index in the log.
    pub id: u64,
    /// The record as logged: its lines, each ending in a newline.
    pub record: String,
    /// The signed text: `go.sum database tree\n<size>\n<root>\n`.
    pub note: String,
    pub size: u64,
    pub root: [u8; 32],
    /// `(name, signature line payload)` of every signature on the note.
    pub signatures: Vec<(String, Vec<u8>)>,
}

pub fn parse_lookup(body: &[u8]) -> Result<Lookup, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the answer is not UTF-8".to_owned())?;
    let (id_line, rest) = text.split_once('\n').ok_or("the answer is truncated")?;
    let id: u64 = id_line
        .parse()
        .map_err(|_| "the record number is not a number".to_owned())?;
    let (record, rest) = rest
        .split_once("\n\n")
        .map(|(r, rest)| (format!("{r}\n"), rest))
        .ok_or("the answer has no signed note")?;
    let (note, sigs) = rest
        .split_once("\n\n")
        .ok_or("the signed note is truncated")?;
    let note = format!("{note}\n");
    let mut lines = note.lines();
    if lines.next() != Some("go.sum database tree") {
        return Err("the signed note is not a go.sum database tree head".into());
    }
    let size: u64 = lines
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or("the tree size is not a number")?;
    let root_b64 = lines.next().ok_or("the tree head has no root")?;
    if lines.next().is_some() {
        return Err("the tree head has more lines than it should".into());
    }
    let root_v = B64
        .decode(root_b64)
        .map_err(|_| "the tree root is not base64".to_owned())?;
    let root: [u8; 32] = root_v
        .try_into()
        .map_err(|_| "the tree root is not 32 bytes".to_owned())?;
    let mut signatures = Vec::new();
    for l in sigs.lines().filter(|l| !l.is_empty()) {
        let l = l
            .strip_prefix('\u{2014}')
            .and_then(|r| r.strip_prefix(' '))
            .ok_or("a signature line does not start with an em dash")?;
        let (name, sig) = l
            .split_once(' ')
            .ok_or("a signature line has no signature")?;
        let sig = B64
            .decode(sig)
            .map_err(|_| "a signature is not base64".to_owned())?;
        signatures.push((name.to_owned(), sig));
    }
    if signatures.is_empty() {
        return Err("the tree head is not signed".into());
    }
    if id >= size {
        return Err("the record is not in the tree the head describes".into());
    }
    Ok(Lookup {
        id,
        record,
        note,
        size,
        root,
        signatures,
    })
}

/// The tree head carries a valid signature by `key`.
pub fn verify_note(l: &Lookup, key: &Key) -> Result<(), String> {
    let mut seen = false;
    for (name, sig) in &l.signatures {
        if *name != key.name {
            continue;
        }
        seen = true;
        if sig.len() != 4 + 64 || sig[..4] != key.hash {
            continue;
        }
        let pk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key.pubkey);
        if pk.verify(l.note.as_bytes(), &sig[4..]).is_ok() {
            return Ok(());
        }
    }
    Err(if seen {
        "the signature of the checksum database's tree head does not verify".into()
    } else {
        format!("the tree head is not signed by {}", key.name)
    })
}

/// A tile of the log: height 8, `level`, number `n`, with `width` hashes (256 when full).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    pub level: u32,
    pub n: u64,
    pub width: u64,
}

impl TileId {
    /// The path segments of `/tile/8/<level>/<n>[.p/<width>]`.
    pub fn path(&self) -> Vec<String> {
        let mut parts = vec![
            "tile".to_owned(),
            TILE_H.to_string(),
            self.level.to_string(),
        ];
        let mut n = self.n;
        let mut digits = vec![format!("{:03}", n % 1000)];
        while n >= 1000 {
            n /= 1000;
            digits.push(format!("x{:03}", n % 1000));
        }
        digits.reverse();
        let mut last = digits.pop().unwrap_or_default();
        if self.width < TILE_W {
            last.push_str(".p");
            parts.extend(digits);
            parts.push(last);
            parts.push(self.width.to_string());
        } else {
            parts.extend(digits);
            parts.push(last);
        }
        parts
    }
}

fn node_hash(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(l);
    h.update(r);
    h.finalize().into()
}

/// The hash of a record: `SHA-256(0x00 || data)`.
pub fn record_hash(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(data);
    h.finalize().into()
}

/// The tile that holds the node at `level` (counted from the leaves) and `index`, in a tree of `size`
/// leaves, and where in it the node's bottom row starts.
fn locate(size: u64, level: u32, index: u64) -> Result<(TileId, u64, u64), String> {
    let tl = level / TILE_H;
    let r = level % TILE_H;
    let row_count = size.checked_shr(TILE_H * tl).unwrap_or(0);
    let start = index
        .checked_shl(r)
        .ok_or("a node of the proof is out of range")?;
    let span = 1u64 << r;
    let n = start / TILE_W;
    let off = start % TILE_W;
    let have = row_count.saturating_sub(n * TILE_W).min(TILE_W);
    if have == 0 || off + span > have {
        return Err("a node of the proof is not in the tree".into());
    }
    Ok((
        TileId {
            level: tl,
            n,
            width: have,
        },
        off,
        span,
    ))
}

/// Walks the proof of leaf `id` in a tree of `size`, asking `get(level, index)` for stored nodes.
fn root_with<F>(size: u64, id: u64, leaf: [u8; 32], get: &mut F) -> Result<[u8; 32], String>
where
    F: FnMut(u32, u64) -> Result<[u8; 32], String>,
{
    fn mth<F: FnMut(u32, u64) -> Result<[u8; 32], String>>(
        a: u64,
        b: u64,
        get: &mut F,
    ) -> Result<[u8; 32], String> {
        let n = b - a;
        if n.is_power_of_two() && a.is_multiple_of(n) {
            return get(n.trailing_zeros(), a / n);
        }
        let k = 1u64 << (63 - (n - 1).leading_zeros());
        let l = mth(a, a + k, get)?;
        let r = mth(a + k, b, get)?;
        Ok(node_hash(&l, &r))
    }
    fn up<F: FnMut(u32, u64) -> Result<[u8; 32], String>>(
        m: u64,
        a: u64,
        b: u64,
        leaf: [u8; 32],
        get: &mut F,
    ) -> Result<[u8; 32], String> {
        let n = b - a;
        if n == 1 {
            return Ok(leaf);
        }
        let k = 1u64 << (63 - (n - 1).leading_zeros());
        if m < a + k {
            let l = up(m, a, a + k, leaf, get)?;
            let r = mth(a + k, b, get)?;
            Ok(node_hash(&l, &r))
        } else {
            let l = mth(a, a + k, get)?;
            let r = up(m, a + k, b, leaf, get)?;
            Ok(node_hash(&l, &r))
        }
    }
    if size == 0 || id >= size || size > 1 << 62 {
        return Err("the tree size is not usable".into());
    }
    up(id, 0, size, leaf, get)
}

/// The tiles the inclusion proof of the lookup needs.
pub fn tiles_needed(l: &Lookup) -> Result<Vec<TileId>, String> {
    let mut out: Vec<TileId> = Vec::new();
    let mut get = |level: u32, index: u64| -> Result<[u8; 32], String> {
        let (t, _, _) = locate(l.size, level, index)?;
        if !out.contains(&t) {
            out.push(t);
        }
        Ok([0u8; 32])
    };
    root_with(l.size, l.id, [0u8; 32], &mut get)?;
    if out.len() > MAX_TILES {
        return Err("the proof needs more tiles than are read".into());
    }
    Ok(out)
}

/// The record is in the tree whose root the note signs, given the tiles of [`tiles_needed`].
pub fn verify_inclusion(l: &Lookup, tiles: &HashMap<TileId, Vec<u8>>) -> Result<(), String> {
    let mut get = |level: u32, index: u64| -> Result<[u8; 32], String> {
        let (t, off, span) = locate(l.size, level, index)?;
        let data = tiles.get(&t).ok_or("a tile of the proof was not fetched")?;
        if data.len() as u64 != t.width * 32 {
            return Err("a tile of the log is not the size it should be".into());
        }
        let mut row: Vec<[u8; 32]> = (off..off + span)
            .map(|i| {
                let at = usize::try_from(i).unwrap_or(0) * 32;
                let mut h = [0u8; 32];
                h.copy_from_slice(&data[at..at + 32]);
                h
            })
            .collect();
        while row.len() > 1 {
            row = row.chunks(2).map(|p| node_hash(&p[0], &p[1])).collect();
        }
        Ok(row[0])
    };
    let leaf = record_hash(l.record.as_bytes());
    let root = root_with(l.size, l.id, leaf, &mut get)?;
    if root != l.root {
        return Err("the record is not in the tree the checksum database signed".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "sumdb/tests.rs"]
mod tests;
