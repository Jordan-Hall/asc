//! The data the simulation stores: object keys, the changeset payload, errors and settings.

use asc_core::id::{ChangesetId, FileId};
use asc_core::path::RefName;
use asc_store::key::ObjKey;
use asc_store::objects::Error as ObjError;
use asc_store::refs::{RefError, RepoKey};

use crate::faults::CrashPoint;

pub const REPO: RepoKey = RepoKey(1);
pub const MASTER: &str = "refs/heads/master";
pub const MIRROR: &str = "refs/mirror/forgejo";
/// Objects younger than this are never swept (SPEC 3.5, `GcConfig::min_age`).
pub const MIN_AGE_MS: i64 = 600_000;
/// Requests per second a cache role may send upstream before it sheds (SPEC 16.3).
pub const SHED_PER_SEC: usize = 20;
/// Protocol versions the simulated clients and replicas speak.
pub const PROTO_MIN: u8 = 1;
pub const PROTO_MAX: u8 = 3;

/// Why a step did not complete.
#[derive(Debug, Clone)]
pub enum Fail {
    /// `UNAVAILABLE` or a lost response: retry later.
    Transient,
    /// The replica died at a named crash point.
    Crashed(CrashPoint),
    /// An invariant broke. The run stops and prints its seed.
    Violation(String),
}

pub type R<T> = Result<T, Fail>;

pub fn viol<T>(msg: impl Into<String>) -> R<T> {
    Err(Fail::Violation(msg.into()))
}

pub fn oerr(e: ObjError) -> Fail {
    match e {
        ObjError::Store(_) => Fail::Transient,
        other => Fail::Violation(format!("object store: {other}")),
    }
}

pub fn rerr(e: RefError) -> Fail {
    match e {
        RefError::Backend(_) => Fail::Transient,
        other => Fail::Violation(format!("ref store: {other}")),
    }
}

/// A well-known ref name; the names above are valid by construction.
pub fn rn(s: &str) -> RefName {
    match RefName::new(s) {
        Ok(n) => n,
        Err(e) => panic!("invalid sim ref name {s}: {e}"),
    }
}

/// The two object kinds the simulation writes, ordered so iteration is deterministic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SimKey {
    File([u8; 32]),
    Cs([u8; 32]),
}

impl SimKey {
    pub fn file(data: &[u8]) -> Self {
        Self::File(*FileId::of(data).as_bytes())
    }

    pub fn obj(self) -> ObjKey {
        match self {
            Self::File(b) => ObjKey::File(FileId::from_bytes(b)),
            Self::Cs(b) => ObjKey::Changeset(ChangesetId::from_bytes(b)),
        }
    }

    pub fn of_obj(k: &ObjKey) -> Option<Self> {
        match k {
            ObjKey::File(id) => Some(Self::File(*id.as_bytes())),
            ObjKey::Changeset(id) => Some(Self::Cs(*id.as_bytes())),
            _ => None,
        }
    }
}

/// A changeset in the simulation: a parent, the files it sets, and the changes it accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cs {
    pub parent: Option<[u8; 32]>,
    pub files: Vec<(u8, [u8; 32])>,
    pub accepted: Vec<u64>,
    pub nonce: u64,
}

impl Cs {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"SIM1".to_vec();
        match self.parent {
            Some(p) => {
                out.push(1);
                out.extend_from_slice(&p);
            }
            None => out.push(0),
        }
        out.push(u8::try_from(self.files.len()).unwrap_or(u8::MAX));
        for (path, f) in &self.files {
            out.push(*path);
            out.extend_from_slice(f);
        }
        out.push(u8::try_from(self.accepted.len()).unwrap_or(u8::MAX));
        for c in &self.accepted {
            out.extend_from_slice(&c.to_be_bytes());
        }
        out.extend_from_slice(&self.nonce.to_be_bytes());
        out
    }

    /// Parses untrusted bytes; any malformed input is `None`.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let mut r = Reader(b);
        if r.take(4)? != b"SIM1" {
            return None;
        }
        let parent = match r.byte()? {
            0 => None,
            1 => Some(r.arr32()?),
            _ => return None,
        };
        let nf = r.byte()?;
        let mut files = Vec::new();
        for _ in 0..nf {
            files.push((r.byte()?, r.arr32()?));
        }
        let na = r.byte()?;
        let mut accepted = Vec::new();
        for _ in 0..na {
            accepted.push(r.u64()?);
        }
        let nonce = r.u64()?;
        r.0.is_empty().then_some(Self {
            parent,
            files,
            accepted,
            nonce,
        })
    }

    pub fn id(&self) -> [u8; 32] {
        *ChangesetId::of(&self.encode()).as_bytes()
    }

    /// Every object this changeset needs (I1).
    pub fn needs(&self) -> Vec<SimKey> {
        let mut v: Vec<SimKey> = self.files.iter().map(|(_, f)| SimKey::File(*f)).collect();
        if let Some(p) = self.parent {
            v.push(SimKey::Cs(p));
        }
        v
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn arr32(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)?.try_into().ok().map(u64::from_be_bytes)
    }
}

/// Deliberate defects, used by the self-tests to prove the invariants notice them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bugs {
    /// A retried land with the same `op_id` lands again.
    pub no_dedupe: bool,
    /// The outbox reconciler never runs.
    pub skip_reconcile: bool,
    /// The sweep ignores `min_age` and the root rescan.
    pub gc_ignores_age: bool,
    /// A changeset is written before its parents and files (I1).
    pub skip_closure_order: bool,
    /// The cache role sends every miss upstream, with no coalescing, backoff or shed.
    pub no_coalesce: bool,
    /// A refused (version-skewed) request still moves the ref.
    pub skew_has_effect: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cs_round_trips() {
        let c = Cs {
            parent: Some([3; 32]),
            files: vec![(1, [2; 32]), (4, [5; 32])],
            accepted: vec![9, 10],
            nonce: 77,
        };
        assert_eq!(Cs::decode(&c.encode()), Some(c.clone()));
        let mut e = c.encode();
        e.push(0);
        assert_eq!(Cs::decode(&e), None);
        for n in 0..c.encode().len() {
            assert_eq!(Cs::decode(&c.encode()[..n]), None);
        }
    }
}
