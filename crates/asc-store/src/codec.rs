//! Stored-value codec: `codec: u8 ‖ data` (SPEC §3.1).

use bytes::{BufMut, Bytes, BytesMut};

const RAW: u8 = 0;
const ZSTD: u8 = 1;
const ZSTD_LEVEL: i32 = 3;

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
}

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
}
