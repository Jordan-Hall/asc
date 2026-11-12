//! Stored-value codec: `codec: u8 ‖ data` (SPEC §3.1).
//!
//! Codec `2` is a delta, used only at rest in the server's bucket (docs/design/DELTA-STORAGE.md):
//! `2 ‖ base id (32 bytes) ‖ one zstd frame` compressed with the base's bytes as a raw content
//! dictionary. The base is an object of the delta's own kind (a file, a tree or a chunk). It never travels: [`check_codec`] refuses it, and the bucket turns it back
//! into codec `0` or `1` before anyone reads it.
//!
//! Codec `3` exists only inside a version 2 pack (docs/design/PACKED-OBJECTS.md, "Dictionaries"):
//! `3 ‖ one zstd frame` compressed with the dictionary trained for that pack. It never travels
//! either: the pack layer turns it into codec `0` as it reads the entry.

use bytes::{BufMut, Bytes, BytesMut};

const RAW: u8 = 0;
const ZSTD: u8 = 1;
const DELTA: u8 = 2;
/// A pack entry compressed with its pack's trained dictionary.
pub const PACK_DICT: u8 = 3;
pub(crate) const ZSTD_LEVEL: i32 = 3;
/// zstd level of a delta frame. Deltas are written by a background pass, not on the push path, so
/// they can afford more effort: over every same-path pair of a 22,704-pair history, level 9 stored
/// 24.6 MB where level 3 stored 129 MB, at 2.8 times the encode time and no slower decode; levels
/// above 9 saved under 13% more at 2 to 12 times the encode time.
pub(crate) const DELTA_LEVEL: i32 = 9;
/// Bytes before a delta's zstd frame: the codec byte and the base id.
const DELTA_HEADER: usize = 1 + 32;

/// A stored value that cannot be decoded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("empty stored value")]
    Empty,
    #[error("unknown codec byte {0}")]
    UnknownCodec(u8),
    #[error("zstd frame is invalid or lacks a sane content size")]
    BadFrame,
    #[error("decoded size exceeds the {0} byte limit")]
    TooLarge(u64),
    /// A delta whose base is missing, damaged, or more than [`MAX_DELTA_DEPTH`] deltas deep.
    #[error("delta base {} is missing, damaged or too deep", .0.path())]
    BadBase(Box<crate::key::ObjKey>),
    /// A delta stored under a kind of key that is never a delta.
    #[error("a delta is stored under a key that cannot hold one")]
    DeltaKind,
}

/// The most deltas a read follows before it reaches a full value.
pub const MAX_DELTA_DEPTH: u8 = 8;

/// Encodes `data` as stored bytes; zstd only when the frame is under 90% of raw.
pub fn encode_stored(data: &[u8]) -> Bytes {
    if let Ok(z) = zstd::bulk::compress(data, ZSTD_LEVEL)
        && z.len() * 10 < data.len() * 9
    {
        let mut out = BytesMut::with_capacity(1 + z.len());
        out.put_u8(ZSTD);
        out.extend_from_slice(&z);
        return out.freeze();
    }
    let mut out = BytesMut::with_capacity(1 + data.len());
    out.put_u8(RAW);
    out.extend_from_slice(data);
    out.freeze()
}

/// Encodes `data` as a delta against `base` (whose id is `base_id`), or `None` when the delta is
/// not under half of `encode_stored(data)`: a delta costs a second read, so it must pay for it.
pub fn encode_delta(
    base_id: &[u8; 32],
    base: &[u8],
    data: &[u8],
    plain_len: usize,
) -> Option<Bytes> {
    let mut c = zstd::bulk::Compressor::with_dictionary(DELTA_LEVEL, base).ok()?;
    c.include_contentsize(true).ok()?;
    let z = c.compress(data).ok()?;
    if (DELTA_HEADER + z.len()) * 2 >= plain_len {
        return None;
    }
    let mut out = BytesMut::with_capacity(DELTA_HEADER + z.len());
    out.put_u8(DELTA);
    out.extend_from_slice(base_id);
    out.extend_from_slice(&z);
    Some(out.freeze())
}

/// The base id of a delta, or `None` for any other stored value.
pub fn delta_base(stored: &[u8]) -> Option<[u8; 32]> {
    if stored.first() != Some(&DELTA) {
        return None;
    }
    stored.get(1..DELTA_HEADER)?.try_into().ok()
}

/// Decodes a delta given its base's bytes, rejecting values larger than `max_decoded`.
pub fn decode_delta(stored: &[u8], base: &[u8], max_decoded: u64) -> Result<Bytes, CodecError> {
    if delta_base(stored).is_none() {
        return Err(CodecError::BadFrame);
    }
    let body = &stored[DELTA_HEADER..];
    let size = zstd::zstd_safe::get_frame_content_size(body)
        .ok()
        .flatten()
        .ok_or(CodecError::BadFrame)?;
    if size > max_decoded {
        return Err(CodecError::TooLarge(max_decoded));
    }
    let mut d =
        zstd::bulk::Decompressor::with_dictionary(base).map_err(|_| CodecError::BadFrame)?;
    let out = d
        .decompress(body, size as usize)
        .map_err(|_| CodecError::BadFrame)?;
    if out.len() as u64 != size {
        return Err(CodecError::BadFrame);
    }
    Ok(out.into())
}

/// Encodes `data` as codec `3` with the pack dictionary `dict`, or `None` when the result is not
/// smaller than `than` bytes.
pub fn encode_pack_dict(
    dict: &zstd::dict::EncoderDictionary<'_>,
    data: &[u8],
    than: usize,
) -> Option<Bytes> {
    let mut c = zstd::bulk::Compressor::with_prepared_dictionary(dict).ok()?;
    c.include_contentsize(true).ok()?;
    c.include_checksum(false).ok()?;
    c.include_dictid(false).ok()?;
    let z = c.compress(data).ok()?;
    if 1 + z.len() >= than {
        return None;
    }
    let mut out = BytesMut::with_capacity(1 + z.len());
    out.put_u8(PACK_DICT);
    out.extend_from_slice(&z);
    Some(out.freeze())
}

/// Decodes a codec `3` pack entry with its pack's dictionary into a codec `0` stored value,
/// rejecting values larger than `max_decoded`.
pub fn decode_pack_dict(
    stored: &[u8],
    dict: &zstd::dict::DecoderDictionary<'_>,
    max_decoded: u64,
) -> Result<Bytes, CodecError> {
    if stored.first() != Some(&PACK_DICT) {
        return Err(CodecError::BadFrame);
    }
    let body = &stored[1..];
    let size = zstd::zstd_safe::get_frame_content_size(body)
        .ok()
        .flatten()
        .ok_or(CodecError::BadFrame)?;
    if size > max_decoded {
        return Err(CodecError::TooLarge(max_decoded));
    }
    let mut d = zstd::bulk::Decompressor::with_prepared_dictionary(dict)
        .map_err(|_| CodecError::BadFrame)?;
    let mut out = Vec::with_capacity(1 + size as usize);
    out.push(RAW);
    let data = d
        .decompress(body, size as usize)
        .map_err(|_| CodecError::BadFrame)?;
    if data.len() as u64 != size {
        return Err(CodecError::BadFrame);
    }
    out.extend_from_slice(&data);
    Ok(out.into())
}

/// Checks the codec byte only, without decoding.
pub fn check_codec(stored: &[u8]) -> Result<(), CodecError> {
    match stored.first() {
        None => Err(CodecError::Empty),
        Some(&(RAW | ZSTD)) => Ok(()),
        Some(&c) => Err(CodecError::UnknownCodec(c)),
    }
}

/// Decodes stored bytes to the original data, rejecting values larger than `max_decoded`.
/// Raw values are sliced, not copied; zstd frames must declare a content size within the limit.
pub fn decode_stored_capped(stored: &Bytes, max_decoded: u64) -> Result<Bytes, CodecError> {
    check_codec(stored)?;
    let body = stored.slice(1..);
    if stored[0] == RAW {
        if body.len() as u64 > max_decoded {
            return Err(CodecError::TooLarge(max_decoded));
        }
        return Ok(body);
    }
    let size = zstd::zstd_safe::get_frame_content_size(&body)
        .ok()
        .flatten()
        .ok_or(CodecError::BadFrame)?;
    if size > max_decoded {
        return Err(CodecError::TooLarge(max_decoded));
    }
    let out = zstd::bulk::decompress(&body, size as usize).map_err(|_| CodecError::BadFrame)?;
    if out.len() as u64 != size {
        return Err(CodecError::BadFrame);
    }
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u64 = 256 * 1024 * 1024;

    #[test]
    fn compressible_uses_zstd() {
        let data = vec![7u8; 10_000];
        let s = encode_stored(&data);
        assert_eq!(s[0], ZSTD);
        assert!(s.len() < 1_000);
        assert_eq!(decode_stored_capped(&s, MAX).unwrap(), data);
    }

    #[test]
    fn a_delta_frame_is_at_the_delta_level_and_no_larger_than_level_3() {
        let text = |edit: usize| -> Vec<u8> {
            (0..4000)
                .map(|i| match i {
                    _ if i % 997 == edit => format!("let changed_{i} = {};\n", i * 7),
                    _ => format!("let value_{i} = compute({i}, {});\n", i % 13),
                })
                .collect::<String>()
                .into_bytes()
        };
        let (base, data) = (text(usize::MAX), text(3));
        let plain = encode_stored(&data).len();
        let d = encode_delta(&[1; 32], &base, &data, plain).unwrap();
        let frame = &d[DELTA_HEADER..];
        let mut low = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &base).unwrap();
        low.include_contentsize(true).unwrap();
        let low = low.compress(&data).unwrap();
        assert!(frame.len() <= low.len(), "{} > {}", frame.len(), low.len());
        let mut high = zstd::bulk::Compressor::with_dictionary(DELTA_LEVEL, &base).unwrap();
        high.include_contentsize(true).unwrap();
        assert_eq!(frame, &high.compress(&data).unwrap()[..]);
        assert_eq!(delta_base(&d), Some([1; 32]));
    }

    #[test]
    fn incompressible_and_tiny_stay_raw() {
        let mut noise = vec![0u8; 4096];
        blake3::Hasher::new()
            .update(b"noise")
            .finalize_xof()
            .fill(&mut noise);
        for data in [&noise[..], b"", b"hi"] {
            let s = encode_stored(data);
            assert_eq!(s[0], RAW);
            assert_eq!(&s[1..], data);
            assert_eq!(decode_stored_capped(&s, MAX).unwrap(), data);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            decode_stored_capped(&Bytes::new(), MAX),
            Err(CodecError::Empty)
        );
        assert_eq!(
            decode_stored_capped(&Bytes::from_static(&[9, 1]), MAX),
            Err(CodecError::UnknownCodec(9))
        );
        assert_eq!(
            decode_stored_capped(&Bytes::from_static(&[1, 1, 2, 3]), MAX),
            Err(CodecError::BadFrame)
        );
    }

    #[test]
    fn rejects_oversize() {
        let s = encode_stored(&vec![7u8; 10_000]);
        assert_eq!(
            decode_stored_capped(&s, 9_999),
            Err(CodecError::TooLarge(9_999))
        );
        assert!(decode_stored_capped(&s, 10_000).is_ok());
        let raw = Bytes::from_static(&[0, 1, 2, 3]);
        assert_eq!(decode_stored_capped(&raw, 2), Err(CodecError::TooLarge(2)));
    }

    fn source(lines: usize, edit: usize) -> Vec<u8> {
        (0..lines)
            .map(|i| {
                if i == edit {
                    format!(
                        "let changed_{i} = compute({i}) + 1;
"
                    )
                } else {
                    format!(
                        "let value_{i} = compute({i}) * {};
",
                        i % 7
                    )
                }
            })
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn a_small_edit_is_stored_as_a_small_delta() {
        let base = source(2000, usize::MAX);
        let next = source(2000, 1234);
        let id = [7u8; 32];
        let plain = encode_stored(&next);
        let d = encode_delta(&id, &base, &next, plain.len()).expect("a delta pays");
        assert!(d.len() * 10 < plain.len(), "{} vs {}", d.len(), plain.len());
        assert_eq!(delta_base(&d), Some(id));
        assert_eq!(decode_delta(&d, &base, MAX).unwrap(), next);
        // A delta never passes as a wire value.
        assert_eq!(check_codec(&d), Err(CodecError::UnknownCodec(2)));
        assert_eq!(delta_base(&plain), None);
    }

    #[test]
    fn an_unrelated_base_gives_no_delta() {
        let mut noise = vec![0u8; 8192];
        blake3::Hasher::new()
            .update(b"other")
            .finalize_xof()
            .fill(&mut noise);
        let next = source(300, usize::MAX);
        let plain = encode_stored(&next);
        assert!(encode_delta(&[1; 32], &noise, &next, plain.len()).is_none());
    }

    #[test]
    fn a_delta_decodes_only_with_its_base_and_within_the_cap() {
        let base = source(500, usize::MAX);
        let next = source(500, 10);
        let d = encode_delta(&[3; 32], &base, &next, encode_stored(&next).len()).unwrap();
        assert!(decode_delta(&d, b"wrong base", MAX).map_or(true, |b| b != next));
        assert_eq!(decode_delta(&d, &base, 10), Err(CodecError::TooLarge(10)));
        assert_eq!(
            decode_delta(&d[..20], &base, MAX),
            Err(CodecError::BadFrame)
        );
        assert_eq!(
            decode_delta(&[1, 2, 3], &base, MAX),
            Err(CodecError::BadFrame)
        );
    }
}
