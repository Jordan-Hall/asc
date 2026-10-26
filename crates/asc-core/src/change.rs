//! Stable change identity (SPEC §20.2): `ChangeId`, `MetaId`, the content-addressed
//! `ChangeMeta` sidecar, `k`-`z` display and git import derivation.

use std::fmt;
use std::str::FromStr;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};

use crate::codec::{EnvelopeKind, Enveloped};
use crate::id::{ChangesetId, derive};

/// Derive-key contexts for change identity.
pub mod context {
    pub const CHANGE_META: &str = "asc v1 change meta";
    pub const CHANGE_GERRIT: &str = "asc v1 change gerrit";
}

/// Why a change id, prefix or meta was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChangeError {
    /// Not 32 letters in `k`-`z`.
    #[error("invalid change id: expected 32 letters in k-z")]
    BadId,
    /// Fewer than [`ChangePrefix::MIN_LEN`] letters.
    #[error("change prefix too short: need at least {min} letters", min = ChangePrefix::MIN_LEN)]
    PrefixTooShort,
    /// A letter outside `k`-`z`, or more than 32 letters.
    #[error("invalid change prefix")]
    BadPrefix,
    /// `ChangeMeta.version` is not 1.
    #[error("unknown change meta version {0}")]
    Version(u8),
    /// `preds` is not strictly ascending.
    #[error("change meta preds are not sorted and unique")]
    UnsortedPreds,
}

/// Stable identity of a change across rewrites; 16 bytes, random at creation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChangeId([u8; 16]);

/// Identity of a `ChangeMeta` envelope (key `m/`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MetaId([u8; 32]);

impl MetaId {
    /// Wraps raw bytes.
    pub const fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }
    /// Borrows the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    /// Hashes a canonical `ChangeMeta` envelope.
    pub fn of(envelope: &[u8]) -> Self {
        Self(derive(context::CHANGE_META, envelope))
    }
}

impl fmt::Display for MetaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&HEXLOWER.encode(&self.0))
    }
}

impl fmt::Debug for MetaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MetaId({})", HEXLOWER.encode(&self.0[..6]))
    }
}

/// Letter for hex digit `d`: jj's reverse-hex alphabet, `'z' - d`.
fn letter(d: u8) -> char {
    (b'z' - d) as char
}

/// Digit of a `k`-`z` letter.
fn digit(c: u8) -> Option<u8> {
    (b'k'..=b'z').contains(&c).then(|| b'z' - c)
}

impl ChangeId {
    /// Number of letters in the full form.
    pub const LEN: usize = 32;
    /// Number of letters in the short form.
    pub const SHORT_LEN: usize = 12;

    /// Wraps raw bytes (callers supply the randomness).
    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }
    /// Borrows the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
    /// First 12 letters, for abbreviated display.
    pub fn short(&self) -> String {
        self.to_string()[..Self::SHORT_LEN].to_owned()
    }
    /// Derives the id of a Gerrit `Change-Id:` trailer value, scoped by the first import ref.
    pub fn from_gerrit(scope: &[u8], value: &[u8]) -> Self {
        let mut input = Vec::with_capacity(scope.len() + 1 + value.len());
        input.extend_from_slice(scope);
        input.push(0);
        input.extend_from_slice(value);
        let h = derive(context::CHANGE_GERRIT, &input);
        Self(h[..16].try_into().expect("16 of 32 bytes"))
    }
    /// Decodes the value of a jj `change-id` commit header (32 letters in `k`-`z`).
    pub fn from_jj_header(value: &[u8]) -> Result<Self, ChangeError> {
        let s = std::str::from_utf8(value).map_err(|_| ChangeError::BadId)?;
        s.parse()
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::with_capacity(Self::LEN);
        for b in self.0 {
            s.push(letter(b >> 4));
            s.push(letter(b & 15));
        }
        f.write_str(&s)
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({})", self.short())
    }
}

impl FromStr for ChangeId {
    type Err = ChangeError;
    fn from_str(s: &str) -> Result<Self, ChangeError> {
        let b = s.as_bytes();
        if b.len() != Self::LEN {
            return Err(ChangeError::BadId);
        }
        let mut out = [0u8; 16];
        for (o, p) in out.iter_mut().zip(b.chunks_exact(2)) {
            let (Some(h), Some(l)) = (digit(p[0]), digit(p[1])) else {
                return Err(ChangeError::BadId);
            };
            *o = (h << 4) | l;
        }
        Ok(Self(out))
    }
}

/// A letter prefix of at least 4 characters, for abbreviated change lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangePrefix {
    /// Whole bytes of the prefix.
    bytes: Vec<u8>,
    /// Trailing high nibble when the length is odd.
    nibble: Option<u8>,
}

impl ChangePrefix {
    /// Minimum accepted prefix length in letters.
    pub const MIN_LEN: usize = 4;

    /// Parses a prefix of `k`-`z` letters.
    pub fn parse(s: &str) -> Result<Self, ChangeError> {
        let b = s.as_bytes();
        if b.len() < Self::MIN_LEN {
            return Err(ChangeError::PrefixTooShort);
        }
        if b.len() > ChangeId::LEN {
            return Err(ChangeError::BadPrefix);
        }
        let digits: Option<Vec<u8>> = b.iter().map(|&c| digit(c)).collect();
        let digits = digits.ok_or(ChangeError::BadPrefix)?;
        let bytes = digits.chunks_exact(2).map(|p| (p[0] << 4) | p[1]).collect();
        let nibble = (digits.len() % 2 == 1).then(|| digits[digits.len() - 1]);
        Ok(Self { bytes, nibble })
    }

    /// True when `id` starts with this prefix.
    pub fn matches(&self, id: &ChangeId) -> bool {
        let n = self.bytes.len();
        id.0.starts_with(&self.bytes)
            && self
                .nibble
                .is_none_or(|h| id.0.get(n).is_some_and(|b| b >> 4 == h))
    }
}

impl FromStr for ChangePrefix {
    type Err = ChangeError;
    fn from_str(s: &str) -> Result<Self, ChangeError> {
        Self::parse(s)
    }
}

/// The rewrite that produced a change version; the postcard variant index is the discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RewriteOp {
    New = 0,
    Amend = 1,
    Rebase = 2,
    Absorb = 3,
    Split = 4,
    Fold = 5,
    Squash = 6,
    Land = 7,
    Restack = 8,
    Import = 9,
}

/// Sidecar naming the changeset it describes and the change it belongs to.
///
/// Never part of the hashed changeset or the git commit (SPEC §20.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawChangeMeta")]
pub struct ChangeMeta {
    /// Always 1.
    pub version: u8,
    /// The changeset this describes.
    pub cs: ChangesetId,
    pub change: ChangeId,
    /// Sorted, deduplicated; empty for a new change.
    pub preds: Vec<ChangesetId>,
    pub op: RewriteOp,
}

#[derive(Deserialize)]
struct RawChangeMeta {
    version: u8,
    cs: ChangesetId,
    change: ChangeId,
    preds: Vec<ChangesetId>,
    op: RewriteOp,
}

impl TryFrom<RawChangeMeta> for ChangeMeta {
    type Error = ChangeError;
    fn try_from(r: RawChangeMeta) -> Result<Self, ChangeError> {
        if r.version != 1 {
            return Err(ChangeError::Version(r.version));
        }
        if !r.preds.windows(2).all(|w| w[0] < w[1]) {
            return Err(ChangeError::UnsortedPreds);
        }
        Ok(Self {
            version: r.version,
            cs: r.cs,
            change: r.change,
            preds: r.preds,
            op: r.op,
        })
    }
}

impl ChangeMeta {
    /// Builds a version-1 meta, sorting and deduplicating `preds`.
    pub fn new(
        cs: ChangesetId,
        change: ChangeId,
        mut preds: Vec<ChangesetId>,
        op: RewriteOp,
    ) -> Self {
        preds.sort();
        preds.dedup();
        Self {
            version: 1,
            cs,
            change,
            preds,
            op,
        }
    }

    /// Id of the canonical envelope (the `m/` key).
    pub fn id(&self) -> MetaId {
        MetaId::of(&self.to_envelope())
    }
}

impl Enveloped for ChangeMeta {
    const KIND: EnvelopeKind = EnvelopeKind::ChangeMeta;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{CodecError, decode_envelope, encode_envelope};

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::of(&[n])
    }

    fn sample() -> ChangeMeta {
        ChangeMeta::new(
            cs(1),
            ChangeId::from_bytes([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0, 1, 2, 3, 4, 5, 6, 7,
            ]),
            vec![cs(3), cs(2), cs(3)],
            RewriteOp::Amend,
        )
    }

    #[test]
    fn golden() {
        let m = sample();
        let new = ChangeMeta::new(cs(1), m.change, vec![], RewriteOp::New);
        let got = format!(
            "change.sample {}\nmeta.sample {}\nmeta.new {}\nenvelope.new {}\ngerrit {}\n",
            m.change,
            m.id(),
            new.id(),
            HEXLOWER.encode(&new.to_envelope()),
            ChangeId::from_gerrit(b"refs/heads/master", b"I0123abcd"),
        );
        let want = include_str!("../tests/golden/change.txt").replace("\r\n", "\n");
        assert_eq!(got, want);
    }

    #[test]
    fn display_and_parse() {
        let id = sample().change;
        let s = id.to_string();
        assert_eq!(s.len(), 32);
        assert!(s.bytes().all(|c| (b'k'..=b'z').contains(&c)));
        assert_eq!(&s[..4], "zyxw");
        assert_eq!(id.short(), s[..12]);
        assert_eq!(s.parse::<ChangeId>(), Ok(id));
        assert_eq!(ChangeId::from_jj_header(s.as_bytes()), Ok(id));
        for bad in [
            "",
            "zyxw",
            &s[..31],
            &s.replace('z', "a"),
            &s.to_uppercase(),
        ] {
            assert_eq!(bad.parse::<ChangeId>(), Err(ChangeError::BadId), "{bad}");
        }
        assert_eq!(
            ChangeId::from_jj_header(&[0xff; 32]),
            Err(ChangeError::BadId)
        );
    }

    #[test]
    fn prefixes() {
        let id = sample().change;
        let s = id.to_string();
        for n in 4..=32 {
            let p = ChangePrefix::parse(&s[..n]).unwrap();
            assert!(p.matches(&id), "{n}");
        }
        let other = ChangePrefix::parse(&format!("{}{}", &s[..5], "k")).unwrap();
        assert!(!other.matches(&id));
        assert_eq!(ChangePrefix::parse("zyx"), Err(ChangeError::PrefixTooShort));
        assert_eq!(ChangePrefix::parse("zyxa"), Err(ChangeError::BadPrefix));
        assert_eq!(
            ChangePrefix::parse(&"z".repeat(33)),
            Err(ChangeError::BadPrefix)
        );
    }

    #[test]
    fn envelope_round_trip() {
        let m = sample();
        assert_eq!(m.preds, {
            let mut p = vec![cs(2), cs(3)];
            p.sort();
            p
        });
        let bytes = m.to_envelope();
        assert_eq!(&bytes[..4], &[b'a', b's', 6, 1]);
        let back: ChangeMeta = decode_envelope(&bytes).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.to_envelope(), bytes);
        assert_eq!(MetaId::of(&bytes), m.id());
        assert_ne!(
            m.id(),
            ChangeMeta::new(cs(1), m.change, vec![], RewriteOp::New).id()
        );
    }

    #[test]
    fn rejects_invalid_meta() {
        let m = sample();
        let enc = |v: &ChangeMeta| encode_envelope(EnvelopeKind::ChangeMeta, 1, v);
        let mut v2 = m.clone();
        v2.version = 2;
        assert!(matches!(
            decode_envelope::<ChangeMeta>(&enc(&v2)),
            Err(CodecError::Payload(_))
        ));
        let mut unsorted = m.clone();
        unsorted.preds.reverse();
        assert!(decode_envelope::<ChangeMeta>(&enc(&unsorted)).is_err());
        let mut dup = m.clone();
        dup.preds = vec![cs(2), cs(2)];
        assert!(decode_envelope::<ChangeMeta>(&enc(&dup)).is_err());
        let good = m.to_envelope();
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            decode_envelope::<ChangeMeta>(&trailing),
            Err(CodecError::Trailing)
        );
        let mut wrong_version = good.clone();
        wrong_version[3] = 2;
        assert!(matches!(
            decode_envelope::<ChangeMeta>(&wrong_version),
            Err(CodecError::UnknownVersion { .. })
        ));
        // Unknown RewriteOp variant (10) is rejected.
        let mut bad_op = good;
        *bad_op.last_mut().unwrap() = 10;
        assert!(decode_envelope::<ChangeMeta>(&bad_op).is_err());
    }

    #[test]
    fn gerrit_derivation_is_scoped() {
        let a = ChangeId::from_gerrit(b"refs/heads/a", b"I1");
        assert_eq!(a, ChangeId::from_gerrit(b"refs/heads/a", b"I1"));
        assert_ne!(a, ChangeId::from_gerrit(b"refs/heads/b", b"I1"));
        assert_ne!(a, ChangeId::from_gerrit(b"refs/heads/a", b"I2"));
        // The separator keeps (scope, value) pairs unambiguous.
        assert_ne!(
            ChangeId::from_gerrit(b"ab", b"c"),
            ChangeId::from_gerrit(b"a", b"bc")
        );
    }
}
