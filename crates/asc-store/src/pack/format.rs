//! The pack file: many small stored values in one immutable object (PACKED-OBJECTS.md).
//!
//! ```text
//! header  : "ASCPACK1" (8) | count u32 LE | reserved u32 (0)
//! entries : repeated [ class u8 | id 32 | stored length u32 LE | stored bytes ]
//! trailer : blake3 of everything before it (32)
//! ```
//!
//! An entry holds exactly what `Objects::put` was given (`codec ‖ data`), so a value read back
//! is verified against its key as a loose one is. Entries are sorted by (class code, id), so
//! the same set of objects makes the same bytes, and the index can be rebuilt from the packs.

use std::fmt;

use bytes::{BufMut, Bytes, BytesMut};

use crate::key::{KeyClass, ObjKey};

pub const MAGIC: &[u8; 8] = b"ASCPACK1";
pub const HEADER_LEN: usize = 16;
/// Class, id and stored length that precede each value.
pub const ENTRY_HEADER_LEN: usize = 1 + 32 + 4;
pub const TRAILER_LEN: usize = 32;

/// Why a pack could not be built or read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("not a pack (bad magic)")]
    BadMagic,
    #[error("pack is truncated")]
    Truncated,
    #[error("pack checksum does not match")]
    Trailer,
    #[error("pack entry {0} is malformed")]
    BadEntry(usize),
    #[error("pack entries are not in order at {0}")]
    Unsorted(usize),
    #[error("{0} cannot be packed")]
    NotPackable(String),
}

/// 16 random bytes; the pack's file name is its lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackId(pub [u8; 16]);

impl PackId {
    pub fn random() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    pub fn hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.0)
    }

    pub fn parse_hex(s: &str) -> Option<Self> {
        if s.len() != 32 || s.bytes().any(|c| c.is_ascii_uppercase()) {
            return None;
        }
        data_encoding::HEXLOWER
            .decode(s.as_bytes())
            .ok()?
            .try_into()
            .ok()
            .map(Self)
    }
}

impl fmt::Display for PackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for PackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PackId({})", self.hex())
    }
}

/// Where one stored value sits inside a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackEntry {
    pub key: ObjKey,
    /// Of the stored bytes, after the entry header.
    pub offset: u64,
    pub len: u32,
}

/// A pack ready to write.
#[derive(Clone, Debug)]
pub struct BuiltPack {
    pub id: PackId,
    pub bytes: Bytes,
    pub entries: Vec<PackEntry>,
}

impl BuiltPack {
    /// The trailer, which is also the checksum of everything before it.
    pub fn checksum(&self) -> [u8; 32] {
        let n = self.bytes.len();
        self.bytes[n - TRAILER_LEN..].try_into().unwrap_or([0; 32])
    }
}

fn sort_key(k: &ObjKey) -> (u8, [u8; 32]) {
    (k.class().code(), k.id32())
}

/// Size of an entry holding `stored` bytes.
pub const fn entry_size(stored: usize) -> usize {
    ENTRY_HEADER_LEN + stored
}

/// Builds a pack of `items`: sorted, one entry per key (the first of a repeated key wins).
pub fn build(
    id: PackId,
    items: impl IntoIterator<Item = (ObjKey, Bytes)>,
) -> Result<BuiltPack, FormatError> {
    let mut items: Vec<(ObjKey, Bytes)> = items.into_iter().collect();
    for (k, v) in &items {
        if !k.packable() || v.is_empty() || u32::try_from(v.len()).is_err() {
            return Err(FormatError::NotPackable(k.to_string()));
        }
    }
    items.sort_by_key(|(k, _)| sort_key(k));
    items.dedup_by(|b, a| a.0 == b.0);
    let body: usize = items.iter().map(|(_, v)| entry_size(v.len())).sum();
    let mut out = BytesMut::with_capacity(HEADER_LEN + body + TRAILER_LEN);
    out.put_slice(MAGIC);
    out.put_u32_le(u32::try_from(items.len()).map_err(|_| FormatError::Truncated)?);
    out.put_u32_le(0);
    let mut entries = Vec::with_capacity(items.len());
    for (k, v) in &items {
        out.put_u8(k.class().code());
        out.put_slice(&k.id32());
        out.put_u32_le(v.len() as u32);
        entries.push(PackEntry {
            key: *k,
            offset: out.len() as u64,
            len: v.len() as u32,
        });
        out.put_slice(v);
    }
    let sum = blake3::hash(&out);
    out.put_slice(sum.as_bytes());
    Ok(BuiltPack {
        id,
        bytes: out.freeze(),
        entries,
    })
}

/// Reads every entry header of a whole pack and checks the trailer. The values are not
/// decoded; `Objects` verification does that when one is read.
pub fn parse(bytes: &[u8]) -> Result<Vec<PackEntry>, FormatError> {
    if bytes.len() < HEADER_LEN + TRAILER_LEN {
        return Err(FormatError::Truncated);
    }
    if &bytes[..8] != MAGIC {
        return Err(FormatError::BadMagic);
    }
    let end = bytes.len() - TRAILER_LEN;
    if blake3::hash(&bytes[..end]).as_bytes() != &bytes[end..] {
        return Err(FormatError::Trailer);
    }
    let count = u32::from_le_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| FormatError::Truncated)?,
    );
    let mut at = HEADER_LEN;
    let mut out = Vec::with_capacity((count as usize).min(1 << 20));
    let mut last: Option<(u8, [u8; 32])> = None;
    for i in 0..count as usize {
        if at + ENTRY_HEADER_LEN > end {
            return Err(FormatError::BadEntry(i));
        }
        let class = KeyClass::from_code(bytes[at]).ok_or(FormatError::BadEntry(i))?;
        let id: [u8; 32] = bytes[at + 1..at + 33]
            .try_into()
            .map_err(|_| FormatError::BadEntry(i))?;
        let len = u32::from_le_bytes(
            bytes[at + 33..at + 37]
                .try_into()
                .map_err(|_| FormatError::BadEntry(i))?,
        );
        let key = ObjKey::from_pack(class, id).ok_or(FormatError::BadEntry(i))?;
        let offset = (at + ENTRY_HEADER_LEN) as u64;
        if len == 0 || offset + u64::from(len) > end as u64 {
            return Err(FormatError::BadEntry(i));
        }
        let sk = (class.code(), id);
        if last.is_some_and(|l| l >= sk) {
            return Err(FormatError::Unsorted(i));
        }
        last = Some(sk);
        out.push(PackEntry { key, offset, len });
        at = offset as usize + len as usize;
    }
    if at != end {
        return Err(FormatError::BadEntry(count as usize));
    }
    Ok(out)
}

/// The stored bytes of `e` inside the whole pack `bytes`.
pub fn slice(bytes: &Bytes, e: &PackEntry) -> Option<Bytes> {
    let start = usize::try_from(e.offset).ok()?;
    let end = start.checked_add(e.len as usize)?;
    (end <= bytes.len().saturating_sub(TRAILER_LEN)).then(|| bytes.slice(start..end))
}

#[cfg(test)]
mod tests {
    use asc_core::id::{ChangesetId, FileId, GitOid, TagId, TreeId};

    use super::*;
    use crate::codec::encode_stored;

    fn items() -> Vec<(ObjKey, Bytes)> {
        let v = |d: &[u8]| encode_stored(d);
        vec![
            (ObjKey::File(FileId::of(b"file")), v(b"file")),
            (ObjKey::Tree(TreeId::of(b"tree")), v(b"tree")),
            (ObjKey::Changeset(ChangesetId::of(b"cs")), v(b"cs")),
            (ObjKey::Tag(TagId::of(b"tag")), v(b"tag")),
            (
                ObjKey::GitTreeOverride(GitOid::from_bytes([7; 20])),
                v(b"override"),
            ),
            (ObjKey::ChunkList(FileId::of(b"list")), v(b"list")),
        ]
    }

    #[test]
    fn a_pack_round_trips_every_packable_class() {
        let id = PackId::random();
        let p = build(id, items()).unwrap();
        let parsed = parse(&p.bytes).unwrap();
        assert_eq!(parsed, p.entries);
        assert_eq!(parsed.len(), 6);
        for (k, v) in items() {
            let e = parsed.iter().find(|e| e.key == k).unwrap();
            assert_eq!(slice(&p.bytes, e).unwrap(), v);
        }
        assert_eq!(
            p.checksum(),
            *blake3::hash(&p.bytes[..p.bytes.len() - 32]).as_bytes()
        );
    }

    #[test]
    fn the_same_set_makes_the_same_bytes_in_any_order() {
        let id = PackId::random();
        let a = build(id, items()).unwrap();
        let mut rev = items();
        rev.reverse();
        rev.push(items()[0].clone());
        let b = build(id, rev).unwrap();
        assert_eq!(a.bytes, b.bytes);
    }

    #[test]
    fn a_flipped_byte_anywhere_is_found() {
        let p = build(PackId::random(), items()).unwrap();
        for i in 0..p.bytes.len() {
            let mut bad = p.bytes.to_vec();
            bad[i] ^= 0x40;
            assert!(parse(&bad).is_err(), "byte {i} went unnoticed");
        }
        assert_eq!(
            parse(&p.bytes[..p.bytes.len() - 1]),
            Err(FormatError::Trailer)
        );
        assert_eq!(parse(&p.bytes[..10]), Err(FormatError::Truncated));
    }

    #[test]
    fn unpackable_keys_and_empty_values_are_refused() {
        let chunk = ObjKey::Chunk(asc_core::id::ChunkId::of(b"c"));
        assert!(matches!(
            build(PackId::random(), [(chunk, encode_stored(b"c"))]),
            Err(FormatError::NotPackable(_))
        ));
        let k = ObjKey::File(FileId::of(b"x"));
        assert!(build(PackId::random(), [(k, Bytes::new())]).is_err());
    }

    #[test]
    fn ids_are_hex_and_strict() {
        let id = PackId::random();
        assert_eq!(PackId::parse_hex(&id.hex()), Some(id));
        assert_eq!(PackId::parse_hex(&id.hex().to_uppercase()), None);
        assert_eq!(PackId::parse_hex("abc"), None);
    }
}
