//! The pack file: many small stored values in one immutable object (PACKED-OBJECTS.md).
//!
//! ```text
//! header     : "ASCPACK1" or "ASCPACK2" (8) | count u32 LE | dictionary length u32 LE
//! dictionary : version 2 only: a zstd dictionary trained on this pack's entries
//! entries    : repeated [ class u8 | id 32 | stored length u32 LE | stored bytes ]
//! trailer    : blake3 of everything before it (32)
//! ```
//!
//! A version 1 entry holds exactly what `Objects::put` was given (`codec ‖ data`), and its
//! dictionary length is 0. A version 2 entry holds either that or codec `3`, the same data
//! compressed with the pack's dictionary, when that is smaller; [`decode_entry`] turns codec `3`
//! back into codec `0`, so a value read back is verified against its key as a loose one is.
//! Entries are sorted by (class code, id), so the same set of objects makes the same bytes, and
//! the index can be rebuilt from the packs.

use std::fmt;

use bytes::{BufMut, Bytes, BytesMut};

use zstd::dict::{DecoderDictionary, EncoderDictionary};

use crate::codec::{
    CodecError, PACK_DICT, ZSTD_LEVEL, decode_pack_dict, decode_stored_capped, encode_pack_dict,
    encode_stored,
};
use crate::key::{KeyClass, ObjKey};

pub const MAGIC: &[u8; 8] = b"ASCPACK1";
/// A pack with a trained dictionary.
pub const MAGIC_V2: &[u8; 8] = b"ASCPACK2";
/// The largest dictionary a pack may carry.
pub const MAX_DICT: usize = 1 << 20;
/// The largest value a codec `3` entry may decode to.
pub const MAX_ENTRY_DECODED: u64 = 64 << 20;
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
    #[error("pack dictionary is malformed")]
    BadDictionary,
}

/// When [`build_with`] trains a dictionary for a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DictOpts {
    /// The dictionary's size in bytes.
    pub size: usize,
    /// Fewer entries than this get no dictionary.
    pub min_objects: usize,
}

impl Default for DictOpts {
    fn default() -> Self {
        Self {
            size: 16 * 1024,
            min_objects: 128,
        }
    }
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

/// Builds a version 1 pack of `items`: sorted, one entry per key (the first of a repeated key
/// wins).
pub fn build(
    id: PackId,
    items: impl IntoIterator<Item = (ObjKey, Bytes)>,
) -> Result<BuiltPack, FormatError> {
    build_with(id, items, None)
}

/// Builds a pack of `items`. With `dict`, a dictionary is trained on the entries and the pack is
/// version 2 when the dictionary saves more than it costs; otherwise it is version 1.
pub fn build_with(
    id: PackId,
    items: impl IntoIterator<Item = (ObjKey, Bytes)>,
    dict: Option<&DictOpts>,
) -> Result<BuiltPack, FormatError> {
    let mut items: Vec<(ObjKey, Bytes)> = items.into_iter().collect();
    for (k, v) in &items {
        if !k.packable() || v.is_empty() || u32::try_from(v.len()).is_err() {
            return Err(FormatError::NotPackable(k.to_string()));
        }
    }
    items.sort_by_key(|(k, _)| sort_key(k));
    items.dedup_by(|b, a| a.0 == b.0);
    if u32::try_from(items.len()).is_err() {
        return Err(FormatError::Truncated);
    }
    if let Some(o) = dict
        && let Some((d, coded)) = with_dictionary(&items, o)
    {
        return Ok(write(id, MAGIC_V2, &d, &coded));
    }
    Ok(write(id, MAGIC, &[], &items))
}

/// Keys and stored values, in pack order.
type Items = Vec<(ObjKey, Bytes)>;

/// Trains a dictionary on `items` and codes each entry with it. `None` when there are too few
/// entries, training fails, or the dictionary does not pay for itself.
fn with_dictionary(
    items: &[(ObjKey, Bytes)],
    o: &DictOpts,
) -> Option<(Vec<u8>, Items)> {
    if items.len() < o.min_objects.max(8) || o.size < 256 || o.size > MAX_DICT {
        return None;
    }
    let data: Vec<Bytes> = items
        .iter()
        .map(|(_, v)| decode_stored_capped(v, MAX_ENTRY_DECODED).ok())
        .collect::<Option<_>>()?;
    let total: usize = data.iter().map(Bytes::len).sum();
    // zstd wants many times the dictionary's size in samples; far less trains noise.
    if total < o.size * 8 {
        return None;
    }
    let samples: Vec<&[u8]> = data.iter().map(|d| &d[..]).collect();
    let dict = zstd::dict::from_samples(&samples, o.size).ok()?;
    if dict.is_empty() || dict.len() > MAX_DICT {
        return None;
    }
    let enc = EncoderDictionary::copy(&dict, ZSTD_LEVEL);
    let mut saved = 0usize;
    let mut out = Vec::with_capacity(items.len());
    for ((k, v), d) in items.iter().zip(&data) {
        // A raw value may still shrink with plain zstd where the dictionary does not help.
        let plain = if v[0] == 0 {
            encode_stored(d)
        } else {
            v.clone()
        };
        let best = encode_pack_dict(&enc, d, plain.len()).unwrap_or(plain);
        saved += v.len().saturating_sub(best.len());
        out.push((*k, best));
    }
    (saved > dict.len() + 64).then_some((dict, out))
}

fn write(id: PackId, magic: &[u8; 8], dict: &[u8], items: &[(ObjKey, Bytes)]) -> BuiltPack {
    let body: usize = items.iter().map(|(_, v)| entry_size(v.len())).sum();
    let mut out = BytesMut::with_capacity(HEADER_LEN + dict.len() + body + TRAILER_LEN);
    out.put_slice(magic);
    out.put_u32_le(items.len() as u32);
    out.put_u32_le(dict.len() as u32);
    out.put_slice(dict);
    let mut entries = Vec::with_capacity(items.len());
    for (k, v) in items {
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
    BuiltPack {
        id,
        bytes: out.freeze(),
        entries,
    }
}

/// The version and dictionary length a pack header names.
pub fn header(head: &[u8]) -> Result<(u8, usize), FormatError> {
    if head.len() < HEADER_LEN {
        return Err(FormatError::Truncated);
    }
    let n = u32::from_le_bytes(
        head[12..16]
            .try_into()
            .map_err(|_| FormatError::Truncated)?,
    ) as usize;
    match &head[..8] {
        m if m == MAGIC && n == 0 => Ok((1, 0)),
        m if m == MAGIC_V2 && n > 0 && n <= MAX_DICT => Ok((2, n)),
        m if m == MAGIC || m == MAGIC_V2 => Err(FormatError::BadDictionary),
        _ => Err(FormatError::BadMagic),
    }
}

/// The dictionary of a whole pack, `None` for version 1.
pub fn dictionary(bytes: &[u8]) -> Result<Option<&[u8]>, FormatError> {
    let (_, n) = header(bytes)?;
    if n == 0 {
        return Ok(None);
    }
    bytes
        .get(HEADER_LEN..HEADER_LEN + n)
        .map(Some)
        .ok_or(FormatError::Truncated)
}

/// Turns a pack entry's stored bytes into a stored value any reader takes: codec `3` is decoded
/// with `dict` to codec `0`; anything else is returned as it is.
pub fn decode_entry(
    stored: Bytes,
    dict: Option<&DecoderDictionary<'_>>,
) -> Result<Bytes, CodecError> {
    if stored.first() != Some(&PACK_DICT) {
        return Ok(stored);
    }
    let d = dict.ok_or(CodecError::UnknownCodec(PACK_DICT))?;
    decode_pack_dict(&stored, d, MAX_ENTRY_DECODED)
}

/// Reads the entries of one whole pack, with its dictionary prepared once.
pub struct PackReader<'a> {
    bytes: &'a Bytes,
    dict: Option<DecoderDictionary<'static>>,
}

impl<'a> PackReader<'a> {
    /// A reader over a pack that [`parse`] accepted.
    pub fn new(bytes: &'a Bytes) -> Result<Self, FormatError> {
        let dict = dictionary(bytes)?.map(DecoderDictionary::copy);
        Ok(Self { bytes, dict })
    }

    /// The stored value of `e`, codec `3` decoded; `None` when it is out of range, the wrong
    /// length or does not decode.
    pub fn entry(&self, e: &PackEntry) -> Option<Bytes> {
        let s = slice(self.bytes, e)?;
        if s.len() != e.len as usize {
            return None;
        }
        decode_entry(s, self.dict.as_ref()).ok()
    }
}

/// Reads every entry header of a whole pack and checks the trailer. The values are not
/// decoded; `Objects` verification does that when one is read.
pub fn parse(bytes: &[u8]) -> Result<Vec<PackEntry>, FormatError> {
    if bytes.len() < HEADER_LEN + TRAILER_LEN {
        return Err(FormatError::Truncated);
    }
    let (_, dict) = header(bytes)?;
    let end = bytes.len() - TRAILER_LEN;
    if blake3::hash(&bytes[..end]).as_bytes() != &bytes[end..] {
        return Err(FormatError::Trailer);
    }
    let count = u32::from_le_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| FormatError::Truncated)?,
    );
    let mut at = HEADER_LEN + dict;
    if at > end {
        return Err(FormatError::Truncated);
    }
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

    /// Values shaped like trees and change records: much shared structure, little shared text.
    fn similar(n: u32) -> Vec<(ObjKey, Bytes)> {
        (0..n)
            .map(|i| {
                let d = format!(
                    "{{\"kind\":\"tree\",\"entries\":[{{\"name\":\"src/module_{i}.rs\",\"mode\":\"100644\",\"id\":\"{:064x}\"}},{{\"name\":\"tests/case_{i}.rs\",\"mode\":\"100644\",\"id\":\"{:064x}\"}}],\"author\":\"someone@example.invalid\",\"message\":\"change number {i}\"}}",
                    u64::from(i) * 7919,
                    u64::from(i) * 104_729
                );
                (ObjKey::File(FileId::of(d.as_bytes())), encode_stored(d.as_bytes()))
            })
            .collect()
    }

    fn opts() -> DictOpts {
        DictOpts {
            size: 4096,
            min_objects: 64,
        }
    }

    #[test]
    fn a_dictionary_pack_is_smaller_and_reads_back_every_value() {
        let items = similar(600);
        let v1 = build(PackId::random(), items.clone()).unwrap();
        let v2 = build_with(PackId::random(), items.clone(), Some(&opts())).unwrap();
        assert_eq!(&v2.bytes[..8], MAGIC_V2);
        assert!(
            v2.bytes.len() * 10 < v1.bytes.len() * 7,
            "dictionary pack {} vs plain {}",
            v2.bytes.len(),
            v1.bytes.len()
        );
        let parsed = parse(&v2.bytes).unwrap();
        assert_eq!(parsed, v2.entries);
        assert!(
            parsed
                .iter()
                .any(|e| slice(&v2.bytes, e).unwrap()[0] == PACK_DICT)
        );
        let r = PackReader::new(&v2.bytes).unwrap();
        for (k, v) in &items {
            let e = parsed.iter().find(|e| e.key == *k).unwrap();
            let got = r.entry(e).unwrap();
            assert_ne!(got[0], PACK_DICT, "codec 3 must not leave the pack");
            let want = decode_stored_capped(v, u64::MAX).unwrap();
            assert_eq!(decode_stored_capped(&got, u64::MAX).unwrap(), want);
        }
    }

    #[test]
    fn a_dictionary_pack_is_deterministic() {
        let a = build_with(PackId([1; 16]), similar(300), Some(&opts())).unwrap();
        let mut rev = similar(300);
        rev.reverse();
        let b = build_with(PackId([1; 16]), rev, Some(&opts())).unwrap();
        assert_eq!(a.bytes, b.bytes);
    }

    #[test]
    fn small_or_unhelpful_sets_stay_version_1() {
        let few = build_with(PackId::random(), similar(10), Some(&opts())).unwrap();
        assert_eq!(&few.bytes[..8], MAGIC);
        assert_eq!(dictionary(&few.bytes).unwrap(), None);
        // Random bytes share nothing a dictionary could learn.
        let noise: Vec<(ObjKey, Bytes)> = (0..300u32)
            .map(|i| {
                let d: Vec<u8> = (0..600u32)
                    .map(|j| {
                        blake3::hash(&[i.to_le_bytes(), j.to_le_bytes()].concat()).as_bytes()[0]
                    })
                    .collect();
                (ObjKey::File(FileId::of(&d)), encode_stored(&d))
            })
            .collect();
        let p = build_with(PackId::random(), noise, Some(&opts())).unwrap();
        assert_eq!(&p.bytes[..8], MAGIC);
    }

    #[test]
    fn headers_that_lie_about_the_dictionary_are_refused() {
        let v2 = build_with(PackId::random(), similar(300), Some(&opts())).unwrap();
        let mut h = v2.bytes[..HEADER_LEN].to_vec();
        assert_eq!(header(&h).unwrap().0, 2);
        h[12..16].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(header(&h), Err(FormatError::BadDictionary));
        h[12..16].copy_from_slice(&(MAX_DICT as u32 + 1).to_le_bytes());
        assert_eq!(header(&h), Err(FormatError::BadDictionary));
        let mut v1 = build(PackId::random(), items()).unwrap().bytes[..HEADER_LEN].to_vec();
        v1[12] = 1;
        assert_eq!(header(&v1), Err(FormatError::BadDictionary));
        assert_eq!(header(b"short"), Err(FormatError::Truncated));
    }

    #[test]
    fn a_flipped_byte_in_a_dictionary_pack_is_found() {
        let p = build_with(PackId::random(), similar(200), Some(&opts())).unwrap();
        assert_eq!(&p.bytes[..8], MAGIC_V2);
        for i in (0..p.bytes.len()).step_by(7) {
            let mut bad = p.bytes.to_vec();
            bad[i] ^= 0x40;
            assert!(parse(&bad).is_err(), "byte {i} went unnoticed");
        }
    }

    #[test]
    fn a_codec_3_entry_needs_its_dictionary_and_never_travels() {
        let p = build_with(PackId::random(), similar(300), Some(&opts())).unwrap();
        let e = p
            .entries
            .iter()
            .find(|e| slice(&p.bytes, e).unwrap()[0] == PACK_DICT)
            .unwrap();
        let raw = slice(&p.bytes, e).unwrap();
        assert!(crate::codec::check_codec(&raw).is_err());
        assert_eq!(
            decode_entry(raw.clone(), None),
            Err(CodecError::UnknownCodec(PACK_DICT))
        );
        let other =
            build_with(PackId::random(), similar(400).split_off(200), Some(&opts())).unwrap();
        let wrong = DecoderDictionary::copy(dictionary(&other.bytes).unwrap().unwrap());
        // A frame decoded with another pack's dictionary fails or yields bytes that do not
        // verify; either way it is not taken for the object.
        if let Ok(got) = decode_entry(raw, Some(&wrong)) {
            let d = decode_stored_capped(&got, u64::MAX).unwrap();
            assert_ne!(
                FileId::of(&d),
                match e.key {
                    ObjKey::File(f) => f,
                    _ => unreachable!(),
                }
            );
        }
    }

    #[test]
    fn ids_are_hex_and_strict() {
        let id = PackId::random();
        assert_eq!(PackId::parse_hex(&id.hex()), Some(id));
        assert_eq!(PackId::parse_hex(&id.hex().to_uppercase()), None);
        assert_eq!(PackId::parse_hex("abc"), None);
    }
}
