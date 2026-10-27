//! A host service's claim about the person it acts for.
//!
//! The hub calls `asc serve` with its own service credential for work no user token covers. When
//! that work is a repair only a repository administrator may make, the hub also sends who the
//! signed-in person is and what its own permission model decided that person may do on the
//! subject of the call. The claim is four metadata entries:
//!
//! * `x-asc-forwarded-user`: the person, as `<user id>:<login>`;
//! * `x-asc-forwarded-role`: `read`, `write` or `admin`;
//! * `x-asc-forwarded-ts`: Unix seconds when the hub made it;
//! * `x-asc-forwarded-mac`: lower-case hex HMAC-SHA256, under a key only the hub and the server
//!   share, of `asc-forwarded-v1\n<user>\n<role>\n<subject>\n<ts>`.
//!   The subject is what the call is about: `owner/name` for a repository, `org:<name>`, `user:<login>`
//!   or `instance`.
//!
//! A client cannot make a claim: it lacks the key. A claim made for one subject, or more than
//! [`MAX_SKEW_SECS`] from the server's clock in either direction, is no claim for another call: the
//! window is the replay window, and a claim never grants more than its role. The server accepts the claim only on a call made
//! with the host service's credential; this module checks the claim itself.

use sha2::{Digest, Sha256};
use tonic::metadata::MetadataMap;

/// Metadata names of the claim.
pub const USER: &str = "x-asc-forwarded-user";
pub const ROLE: &str = "x-asc-forwarded-role";
pub const TS: &str = "x-asc-forwarded-ts";
pub const MAC: &str = "x-asc-forwarded-mac";
/// What the hub decided a person may do on a subject. Ordered: `Admin` includes `Write`, which
/// includes `Read`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    /// The spelling on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    /// Reads the wire spelling; anything else is no role.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// A verified claim: who, and what the hub allowed them on the subject it was signed for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// `<user id>:<login>`.
    pub user: String,
    pub role: Role,
}

impl Claim {
    /// Whether the claim covers `needed` (a claim never covers more than its role).
    #[must_use]
    pub fn allows(&self, needed: Role) -> bool {
        self.role >= needed
    }

    /// The login of the person.
    #[must_use]
    pub fn login(&self) -> &str {
        self.user.split_once(':').map_or(&self.user, |(_, l)| l)
    }
}
/// How far a claim's time may differ from the server's clock.
pub const MAX_SKEW_SECS: i64 = 60;
/// Shortest key accepted, in bytes.
pub const MIN_KEY: usize = 32;
const CONTEXT: &str = "asc-forwarded-v1";
const BLOCK: usize = 64;

/// The key the hub signs claims with, and the check of a claim.
#[derive(Clone)]
pub struct ForwardedVerifier {
    key: Vec<u8>,
}

impl std::fmt::Debug for ForwardedVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForwardedVerifier(<redacted>)")
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn payload(user: &str, role: &str, subject: &str, ts: &str) -> Vec<u8> {
    format!("{CONTEXT}\n{user}\n{role}\n{subject}\n{ts}").into_bytes()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

impl ForwardedVerifier {
    /// A verifier over `key`, at least [`MIN_KEY`] bytes.
    pub fn new(key: Vec<u8>) -> Option<Self> {
        (key.len() >= MIN_KEY).then_some(Self { key })
    }

    /// The metadata a hub sends for `user` as an administrator of `repo` at `ts`. The server never
    /// signs; this is for the hub's client library and for tests.
    pub fn sign(
        &self,
        user: &str,
        role: Role,
        subject: &str,
        ts: i64,
    ) -> Vec<(&'static str, String)> {
        let mac = hmac_sha256(
            &self.key,
            &payload(user, role.as_str(), subject, &ts.to_string()),
        );
        vec![
            (USER, user.to_owned()),
            (ROLE, role.as_str().to_owned()),
            (TS, ts.to_string()),
            (MAC, hex(&mac)),
        ]
    }

    /// The claim in `meta` for `subject`. `None` when it is absent, incomplete or repeated, signed
    /// under another key, made for another subject, has a role that is not `read`, `write` or
    /// `admin`, spells its time any way but the canonical decimal, or is outside the clock window.
    pub fn verify(&self, meta: &MetadataMap, subject: &str, now: i64) -> Option<Claim> {
        let one = |name: &str| -> Option<&str> {
            let mut all = meta.get_all(name).iter();
            let first = all.next()?;
            all.next().is_none().then(|| first.to_str().ok())?
        };
        let (user, role, ts, mac) = (one(USER)?, one(ROLE)?, one(TS)?, one(MAC)?);
        if user.is_empty() || user.len() > 300 || user.contains(['\n', '\r']) {
            return None;
        }
        let given = unhex(mac)?;
        // The MAC is checked before anything else of the claim is read.
        let want = hmac_sha256(&self.key, &payload(user, role, subject, ts));
        if !same(&given, &want) {
            return None;
        }
        let ts_n: i64 = ts.parse().ok().filter(|n: &i64| n.to_string() == ts)?;
        if (now - ts_n).abs() > MAX_SKEW_SECS {
            return None;
        }
        Some(Claim {
            user: user.to_owned(),
            role: Role::parse(role)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier() -> ForwardedVerifier {
        ForwardedVerifier::new(vec![7u8; 32]).unwrap()
    }

    fn meta(entries: &[(&'static str, String)]) -> MetadataMap {
        let mut m = MetadataMap::new();
        for (k, v) in entries {
            m.append(*k, v.parse().unwrap());
        }
        m
    }

    #[test]
    fn hmac_matches_the_published_test_vector() {
        // RFC 4231, test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_signed_claim_verifies_with_its_role_for_its_subject() {
        let v = verifier();
        for (role, subject) in [
            (Role::Read, "acme/app"),
            (Role::Write, "user:alice"),
            (Role::Admin, "org:acme"),
            (Role::Admin, "instance"),
        ] {
            let m = meta(&v.sign("7:alice", role, subject, 1_000));
            let c = v.verify(&m, subject, 1_030).unwrap();
            assert_eq!(
                (c.user.as_str(), c.role, c.login()),
                ("7:alice", role, "alice")
            );
        }
    }

    #[test]
    fn a_claim_never_covers_more_than_its_role() {
        let read = Claim {
            user: "7:alice".into(),
            role: Role::Read,
        };
        assert!(read.allows(Role::Read) && !read.allows(Role::Write) && !read.allows(Role::Admin));
        let write = Claim {
            role: Role::Write,
            ..read.clone()
        };
        assert!(
            write.allows(Role::Read) && write.allows(Role::Write) && !write.allows(Role::Admin)
        );
        let admin = Claim {
            role: Role::Admin,
            ..read
        };
        assert!(admin.allows(Role::Write) && admin.allows(Role::Admin));
    }

    #[test]
    fn a_claim_that_was_changed_or_replayed_elsewhere_is_no_claim() {
        let v = verifier();
        let good = v.sign("7:alice", Role::Write, "acme/app", 1_000);
        // Another subject, of any kind.
        for other in ["acme/other", "org:acme", "user:alice", "instance"] {
            assert!(v.verify(&meta(&good), other, 1_000).is_none(), "{other}");
        }
        // Another person with the old mac, and another role (a read claim made into admin).
        let mut other = good.clone();
        other[0].1 = "8:mallory".into();
        assert!(v.verify(&meta(&other), "acme/app", 1_000).is_none());
        let mut role = good.clone();
        role[1].1 = "admin".into();
        assert!(v.verify(&meta(&role), "acme/app", 1_000).is_none());
        // Too old, and from the future.
        assert!(
            v.verify(&meta(&good), "acme/app", 1_000 + MAX_SKEW_SECS + 1)
                .is_none()
        );
        assert!(
            v.verify(&meta(&good), "acme/app", 1_000 - MAX_SKEW_SECS - 1)
                .is_none()
        );
        assert!(
            v.verify(&meta(&good), "acme/app", 1_000 + MAX_SKEW_SECS)
                .is_some()
        );
        // Signed under another key.
        let theirs = ForwardedVerifier::new(vec![9u8; 32]).unwrap();
        assert!(theirs.verify(&meta(&good), "acme/app", 1_000).is_none());
        // A role the contract does not have, signed correctly, is still no claim.
        let key = vec![7u8; 32];
        let mac = hmac_sha256(&key, &payload("7:alice", "owner", "acme/app", "1000"));
        let odd = [
            (USER, "7:alice".to_owned()),
            (ROLE, "owner".to_owned()),
            (TS, "1000".to_owned()),
            (MAC, hex(&mac)),
        ];
        assert!(v.verify(&meta(&odd), "acme/app", 1_000).is_none());
        // A time spelled any way but the canonical decimal, even under a valid MAC.
        for spelled in ["01000", "+1000", "1000.0", " 1000"] {
            let mac = hmac_sha256(&key, &payload("7:alice", "write", "acme/app", spelled));
            let m = meta(&[
                (USER, "7:alice".to_owned()),
                (ROLE, "write".to_owned()),
                (TS, spelled.to_owned()),
                (MAC, hex(&mac)),
            ]);
            assert!(v.verify(&m, "acme/app", 1_000).is_none(), "{spelled:?}");
        }
        // A mac that is not hex, a missing entry, a repeated entry.
        let mut junk = good.clone();
        junk[3].1 = "zz".into();
        assert!(v.verify(&meta(&junk), "acme/app", 1_000).is_none());
        assert!(v.verify(&meta(&good[..3]), "acme/app", 1_000).is_none());
        let mut twice = good.clone();
        twice.push(good[0].clone());
        assert!(v.verify(&meta(&twice), "acme/app", 1_000).is_none());
        assert!(v.verify(&MetadataMap::new(), "acme/app", 1_000).is_none());
    }

    #[test]
    fn a_short_key_is_refused() {
        assert!(ForwardedVerifier::new(vec![1u8; MIN_KEY - 1]).is_none());
    }
}
