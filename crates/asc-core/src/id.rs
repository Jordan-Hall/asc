//! Typed identifiers (SPEC §2.1): BLAKE3 derive-key ids and git object ids.

use std::fmt;
use std::str::FromStr;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};

/// Errors from parsing ids, prefixes and ref targets.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    /// Input is not a full-length hex id.
    #[error("invalid id: expected {expected} hex characters")]
    BadId { expected: usize },
    /// Prefix has fewer than [`HexPrefix::MIN_LEN`] hex characters.
    #[error("hex prefix too short: need at least {min} characters", min = HexPrefix::MIN_LEN)]
    PrefixTooShort,
    /// Prefix contains a non-hex character or is too long.
    #[error("invalid hex prefix")]
    BadPrefix,
    /// Ref target bytes are not 33 bytes with a known kind tag.
    #[error("invalid ref target encoding")]
    BadRefTarget,
}

fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], IdError> {
    let bad = IdError::BadId { expected: N * 2 };
    if s.len() != N * 2 {
        return Err(bad);
    }
    let v = HEXLOWER
        .decode(s.to_ascii_lowercase().as_bytes())
        .map_err(|_| bad)?;
    Ok(v.try_into().expect("length checked"))
}

macro_rules! id_newtype {
    ($(#[$m:meta])* $name:ident, $n:literal) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name([u8; $n]);

        impl $name {
            /// Wraps raw bytes.
            pub const fn from_bytes(b: [u8; $n]) -> Self { Self(b) }
            /// Borrows the raw bytes.
            pub const fn as_bytes(&self) -> &[u8; $n] { &self.0 }
            /// First 12 hex characters, for abbreviated display.
            pub fn short(&self) -> String { HEXLOWER.encode(&self.0[..6]) }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&HEXLOWER.encode(&self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.short())
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> { parse_hex(s).map(Self) }
        }
    };
}

id_newtype!(
    /// Identity of file content.
    FileId, 32
);
id_newtype!(
    /// Identity of a content-defined chunk.
    ChunkId, 32
);
id_newtype!(
    /// Identity of a canonical tree envelope.
    TreeId, 32
);
id_newtype!(
    /// Identity of a canonical changeset envelope.
    ChangesetId, 32
);
id_newtype!(
    /// Identity of a canonical tag envelope.
    TagId, 32
);
id_newtype!(
    /// A git SHA-1 object id; never an identity inside asc.
    GitOid, 20
);

/// Derive-key context strings (SPEC §2.1).
pub mod context {
    pub const FILE: &str = "asc v1 file";
    pub const CHUNK: &str = "asc v1 chunk";
    pub const TREE: &str = "asc v1 tree";
    pub const CHANGESET: &str = "asc v1 changeset";
    pub const TAG: &str = "asc v1 tag";
}

fn derive(ctx: &str, bytes: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key(ctx)
        .update(bytes)
        .finalize()
        .as_bytes()
}

impl FileId {
    /// Hashes a whole file held in memory.
    pub fn of(bytes: &[u8]) -> Self {
        Self(derive(context::FILE, bytes))
    }
}
impl ChunkId {
    /// Hashes one chunk.
    pub fn of(bytes: &[u8]) -> Self {
        Self(derive(context::CHUNK, bytes))
    }
}
impl TreeId {
    /// Hashes a canonical tree envelope.
    pub fn of(envelope: &[u8]) -> Self {
        Self(derive(context::TREE, envelope))
    }
}
impl ChangesetId {
    /// Hashes a canonical changeset envelope.
    pub fn of(envelope: &[u8]) -> Self {
        Self(derive(context::CHANGESET, envelope))
    }
}
impl TagId {
    /// Hashes a canonical tag envelope.
    pub fn of(envelope: &[u8]) -> Self {
        Self(derive(context::TAG, envelope))
    }
}

/// Streaming [`FileId`] hasher; constant memory regardless of file size.
pub struct FileHasher(blake3::Hasher);

impl FileHasher {
    /// Starts a new file hash.
    pub fn new() -> Self {
        Self(blake3::Hasher::new_derive_key(context::FILE))
    }
    /// Feeds more content.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }
    /// Finishes and returns the id.
    pub fn finalize(&self) -> FileId {
        FileId(*self.0.finalize().as_bytes())
    }
}

impl Default for FileHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// A hex prefix of at least 4 characters, for abbreviated id lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexPrefix {
    /// Whole bytes of the prefix.
    bytes: Vec<u8>,
    /// Trailing high nibble when the length is odd.
    nibble: Option<u8>,
}

impl HexPrefix {
    /// Minimum accepted prefix length in hex characters.
    pub const MIN_LEN: usize = 4;

    /// Parses a hex prefix (case-insensitive).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.len() < Self::MIN_LEN {
            return Err(IdError::PrefixTooShort);
        }
        if s.len() > 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(IdError::BadPrefix);
        }
        let digit = |c: u8| (c as char).to_digit(16).expect("checked") as u8;
        let b = s.as_bytes();
        let bytes = b
            .chunks_exact(2)
            .map(|p| (digit(p[0]) << 4) | digit(p[1]))
            .collect();
        let nibble = (b.len() % 2 == 1).then(|| digit(b[b.len() - 1]));
        Ok(Self { bytes, nibble })
    }

    /// True when `id` starts with this prefix.
    pub fn matches(&self, id: &[u8]) -> bool {
        let n = self.bytes.len();
        id.starts_with(&self.bytes)
            && self
                .nibble
                .is_none_or(|h| id.get(n).is_some_and(|b| b >> 4 == h))
    }
}

impl FromStr for HexPrefix {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::parse(s)
    }
}

/// What a ref points at; stored as 33 bytes, `kind ‖ id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RefTarget {
    Changeset(ChangesetId),
    Tag(TagId),
}

impl RefTarget {
    /// Encodes as `0x01|0x02 ‖ id`.
    pub fn to_bytes(&self) -> [u8; 33] {
        let (kind, id) = match self {
            Self::Changeset(c) => (1, c.as_bytes()),
            Self::Tag(t) => (2, t.as_bytes()),
        };
        let mut out = [0u8; 33];
        out[0] = kind;
        out[1..].copy_from_slice(id);
        out
    }

    /// Decodes the 33-byte form.
    pub fn from_bytes(b: &[u8]) -> Result<Self, IdError> {
        let [kind, id @ ..] = b else {
            return Err(IdError::BadRefTarget);
        };
        let id: [u8; 32] = id.try_into().map_err(|_| IdError::BadRefTarget)?;
        match kind {
            1 => Ok(Self::Changeset(ChangesetId(id))),
            2 => Ok(Self::Tag(TagId(id))),
            _ => Err(IdError::BadRefTarget),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../tests/golden/ids.txt");

    fn golden(name: &str) -> &'static str {
        GOLDEN
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
            .unwrap_or_else(|| panic!("missing golden {name}"))
            .trim()
    }

    #[test]
    fn context_strings_match_golden() {
        for (name, input) in [("empty", &b""[..]), ("hello", b"hello")] {
            let cases = [
                ("file", FileId::of(input).to_string()),
                ("chunk", ChunkId::of(input).to_string()),
                ("tree", TreeId::of(input).to_string()),
                ("changeset", ChangesetId::of(input).to_string()),
                ("tag", TagId::of(input).to_string()),
            ];
            for (kind, got) in cases {
                assert_eq!(got, golden(&format!("{kind}.{name}")), "{kind}.{name}");
            }
        }
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut h = FileHasher::new();
        for c in data.chunks(777) {
            h.update(c);
        }
        assert_eq!(h.finalize(), FileId::of(&data));
    }

    #[test]
    fn display_parse_short() {
        let id = FileId::of(b"x");
        let s = id.to_string();
        assert_eq!(s.len(), 64);
        assert_eq!(s.parse::<FileId>().unwrap(), id);
        assert_eq!(s.to_uppercase().parse::<FileId>().unwrap(), id);
        assert_eq!(id.short(), &s[..12]);
        assert!("abcd".parse::<FileId>().is_err());
        assert!(
            GitOid::from_bytes([7; 20])
                .to_string()
                .parse::<GitOid>()
                .is_ok()
        );
    }

    #[test]
    fn prefix_rules() {
        assert_eq!(HexPrefix::parse("abc"), Err(IdError::PrefixTooShort));
        assert_eq!(HexPrefix::parse("abcg"), Err(IdError::BadPrefix));
        let p = HexPrefix::parse("abcde").unwrap();
        assert!(p.matches(&[0xab, 0xcd, 0xe7]));
        assert!(!p.matches(&[0xab, 0xcd, 0xf7]));
        assert!(!p.matches(&[0xab, 0xcd]));
        assert!(HexPrefix::parse("ABCD").unwrap().matches(&[0xab, 0xcd, 1]));
    }

    #[test]
    fn ref_target_round_trip() {
        let t = RefTarget::Tag(TagId::of(b"t"));
        let c = RefTarget::Changeset(ChangesetId::of(b"c"));
        for r in [t, c] {
            assert_eq!(RefTarget::from_bytes(&r.to_bytes()).unwrap(), r);
        }
        assert!(RefTarget::from_bytes(&[3; 33]).is_err());
        assert!(RefTarget::from_bytes(&[1; 32]).is_err());
    }

    #[test]
    fn serde_raw_arrays() {
        let id = TreeId::of(b"t");
        let bytes = postcard::to_stdvec(&id).unwrap();
        assert_eq!(bytes, id.as_bytes());
        assert_eq!(postcard::from_bytes::<TreeId>(&bytes).unwrap(), id);
        let g = GitOid::from_bytes([9; 20]);
        assert_eq!(postcard::to_stdvec(&g).unwrap().len(), 20);
        let r = RefTarget::Tag(TagId::of(b"t"));
        let rb = postcard::to_stdvec(&r).unwrap();
        assert_eq!(postcard::from_bytes::<RefTarget>(&rb).unwrap(), r);
    }
}
