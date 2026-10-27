//! A host service's claim about the person it acts for.
//!
//! The hub calls `asc serve` with its own service credential for work no user token covers. When
//! that work is a repair only a repository administrator may make, the hub also sends who the
//! signed-in person is and that it checked that person is an administrator of the repository.
//! The claim is four metadata entries:
//!
//! * `x-asc-forwarded-user`: the person, as `<user id>:<login>`;
//! * `x-asc-forwarded-role`: `admin`;
//! * `x-asc-forwarded-ts`: Unix seconds when the hub made it;
//! * `x-asc-forwarded-mac`: lower-case hex HMAC-SHA256, under a key only the hub and the server
//!   share, of `asc-forwarded-v1\n<user>\n<role>\n<repository>\n<ts>`.
//!
//! A client cannot make a claim: it lacks the key. A claim made for one repository or older than
//! [`MAX_SKEW_SECS`] is no claim for another. The server accepts the claim only on a call made
//! with the host service's credential; this module checks the claim itself.

use sha2::{Digest, Sha256};
use tonic::metadata::MetadataMap;

/// Metadata names of the claim.
pub const USER: &str = "x-asc-forwarded-user";
pub const ROLE: &str = "x-asc-forwarded-role";
pub const TS: &str = "x-asc-forwarded-ts";
pub const MAC: &str = "x-asc-forwarded-mac";
/// The only role a claim can carry.
pub const ADMIN: &str = "admin";
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

fn payload(user: &str, role: &str, repo: &str, ts: i64) -> Vec<u8> {
    format!("{CONTEXT}\n{user}\n{role}\n{repo}\n{ts}").into_bytes()
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
    pub fn sign(&self, user: &str, repo: &str, ts: i64) -> Vec<(&'static str, String)> {
        let mac = hmac_sha256(&self.key, &payload(user, ADMIN, repo, ts));
        vec![
            (USER, user.to_owned()),
            (ROLE, ADMIN.to_owned()),
            (TS, ts.to_string()),
            (MAC, hex(&mac)),
        ]
    }

    /// The person a valid claim names: `<user id>:<login>`. `None` when the claim is absent,
    /// incomplete, signed under another key, made for another repository, not an admin claim, or
    /// outside the clock window.
    pub fn verify(&self, meta: &MetadataMap, repo: &str, now: i64) -> Option<String> {
        let one = |name: &str| -> Option<&str> {
            let mut all = meta.get_all(name).iter();
            let first = all.next()?;
            all.next().is_none().then(|| first.to_str().ok())?
        };
        let (user, role, ts, mac) = (one(USER)?, one(ROLE)?, one(TS)?, one(MAC)?);
        if role != ADMIN || user.is_empty() || user.len() > 300 || user.contains(['\n', '\r']) {
            return None;
        }
        let ts_n: i64 = ts.parse().ok()?;
        if (now - ts_n).abs() > MAX_SKEW_SECS {
            return None;
        }
        let given = unhex(mac)?;
        let want = hmac_sha256(&self.key, &payload(user, role, repo, ts_n));
        same(&given, &want).then(|| user.to_owned())
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
    fn a_signed_claim_for_the_repository_verifies() {
        let v = verifier();
        let m = meta(&v.sign("7:alice", "acme/app", 1_000));
        assert_eq!(v.verify(&m, "acme/app", 1_030).as_deref(), Some("7:alice"));
    }

    #[test]
    fn a_claim_that_was_changed_or_replayed_elsewhere_is_no_claim() {
        let v = verifier();
        let good = v.sign("7:alice", "acme/app", 1_000);
        // Another repository.
        assert!(v.verify(&meta(&good), "acme/other", 1_000).is_none());
        // Another person with the old mac.
        let mut other = good.clone();
        other[0].1 = "8:mallory".into();
        assert!(v.verify(&meta(&other), "acme/app", 1_000).is_none());
        // Another role.
        let mut role = good.clone();
        role[1].1 = "owner".into();
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
        // Signed under another key.
        let theirs = ForwardedVerifier::new(vec![9u8; 32]).unwrap();
        assert!(theirs.verify(&meta(&good), "acme/app", 1_000).is_none());
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
