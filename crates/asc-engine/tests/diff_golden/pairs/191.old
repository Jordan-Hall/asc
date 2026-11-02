//! Canonical envelope codec (SPEC §2.2, §2.2a).
//!
//! Envelope: `b"as"` || kind || version || postcard(payload). Decoding is
//! strict: anything that does not re-encode to the exact input is rejected,
//! so `encode(decode(b)) == b` for every accepted `b`.

use serde::{Serialize, de::DeserializeOwned};

const MAGIC: [u8; 2] = *b"as";

/// Envelope kinds. Kind 5 is reserved (was `FileMeta`); 16-63 are derived values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EnvelopeKind {
    Tree = 1,
    Changeset = 2,
    TagRecord = 3,
    ChunkList = 4,
}

impl EnvelopeKind {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Tree,
            2 => Self::Changeset,
            3 => Self::TagRecord,
            4 => Self::ChunkList,
            _ => return None,
        })
    }
}

/// A type with a fixed envelope kind and version.
pub trait Enveloped: Serialize + DeserializeOwned {
    const KIND: EnvelopeKind;
    const VERSION: u8 = 1;

    /// Canonical envelope bytes.
    fn to_envelope(&self) -> Vec<u8> {
        encode_envelope(Self::KIND, Self::VERSION, self)
    }
}

/// Why decoding failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("bad envelope magic or truncated header")]
    BadMagic,
    #[error("unknown envelope kind {0}")]
    UnknownKind(u8),
    #[error("envelope kind {got:?} where {expected:?} was expected")]
    WrongKind {
        expected: EnvelopeKind,
        got: EnvelopeKind,
    },
    #[error("unknown version {version} for kind {kind:?}")]
    UnknownVersion { kind: EnvelopeKind, version: u8 },
    #[error("unknown payload version {0}")]
    UnknownPayloadVersion(u8),
    #[error("trailing bytes after payload")]
    Trailing,
    #[error("non-canonical encoding")]
    NonCanonical,
    #[error("payload: {0}")]
    Payload(String),
}

/// Encodes `value` as an envelope.
pub fn encode_envelope<T: Serialize>(kind: EnvelopeKind, version: u8, value: &T) -> Vec<u8> {
    let out = vec![MAGIC[0], MAGIC[1], kind as u8, version];
    postcard::to_extend(value, out).expect("in-memory encode cannot fail")
}

/// Decodes an envelope of type `T`, rejecting every malformed form.
pub fn decode_envelope<T: Enveloped>(bytes: &[u8]) -> Result<T, CodecError> {
    let [m0, m1, kind, version, payload @ ..] = bytes else {
        return Err(CodecError::BadMagic);
    };
    if [*m0, *m1] != MAGIC {
        return Err(CodecError::BadMagic);
    }
    let kind = EnvelopeKind::from_u8(*kind).ok_or(CodecError::UnknownKind(*kind))?;
    if kind != T::KIND {
        return Err(CodecError::WrongKind {
            expected: T::KIND,
            got: kind,
        });
    }
    if *version != T::VERSION {
        return Err(CodecError::UnknownVersion {
            kind,
            version: *version,
        });
    }
    let value = decode_payload::<T>(payload)?;
    if encode_envelope(T::KIND, T::VERSION, &value) != bytes {
        return Err(CodecError::NonCanonical);
    }
    Ok(value)
}

fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CodecError> {
    let (value, rest) =
        postcard::take_from_bytes(payload).map_err(|e| CodecError::Payload(e.to_string()))?;
    if rest.is_empty() {
        Ok(value)
    } else {
        Err(CodecError::Trailing)
    }
}

/// Encodes a non-envelope value: `version` || postcard(value) (SPEC §2.2a).
pub fn encode_versioned<T: Serialize>(version: u8, value: &T) -> Vec<u8> {
    let mut out = vec![version];
    out.extend(postcard::to_allocvec(value).expect("in-memory encode cannot fail"));
    out
}

/// Decodes a non-envelope value, requiring `version` and canonical bytes.
pub fn decode_versioned<T: Serialize + DeserializeOwned>(
    version: u8,
    bytes: &[u8],
) -> Result<T, CodecError> {
    let Some((v, payload)) = bytes.split_first() else {
        return Err(CodecError::Payload("empty".into()));
    };
    if *v != version {
        return Err(CodecError::UnknownPayloadVersion(*v));
    }
    let value = decode_payload::<T>(payload)?;
    if encode_versioned(version, &value) != bytes {
        return Err(CodecError::NonCanonical);
    }
    Ok(value)
}
