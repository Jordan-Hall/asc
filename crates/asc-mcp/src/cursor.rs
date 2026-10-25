//! HMAC-sealed cursors (SPEC 22.5 "Keys and sealing", 22.6.2 "Cursors").
//!
//! A cursor is `base64url(key_id[4] || payload_json || mac[32])`. The payload binds the tool, a
//! digest of the call's arguments (without `cursor` and `max_tokens`), a position and an expiry.
//! The MAC is HMAC-SHA256 over `key_id || payload_json` under the key the id names. The first key
//! of the [`KeyRing`] signs; every key verifies. A cursor from another call, another tool, another
//! key ring (a restart on stdio uses a fresh random key) or past its expiry is rejected with one
//! indistinguishable error whose JSON-RPC code is -32602. Cursors hold ids and offsets, never
//! tokens.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// JSON-RPC "invalid params": the code every cursor failure maps to.
pub const INVALID_PARAMS: i32 = -32602;

/// How long a cursor stays valid: 1 h, the longest TTL of SPEC 22.5.
pub const CURSOR_TTL_SECS: u64 = 3600;

/// Argument names that do not take part in the digest: they change between pages.
const PAGING_ARGS: [&str; 2] = ["cursor", "max_tokens"];

const KEY_LEN: usize = 32;
const KID_LEN: usize = 4;
const MAC_LEN: usize = 32;
/// A cursor longer than this is refused before any decoding.
const MAX_CURSOR_CHARS: usize = 4096;

/// Where a continuation resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Position {
    /// Entry index in a list, log, grep, tree or blame stream.
    Index { index: u64 },
    /// `read_file`: the byte offset of the next line and its 1-based line number.
    File { offset: u64, line: u64 },
    /// `diff`: the file index in path order and the first hunk of that file still to send.
    Diff { file: u64, hunk: u64 },
    /// A paged RPC's own `page_token`/`after`, wrapped.
    Page { token: String },
}

/// Every cursor failure. The text never says which check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid cursor")]
pub struct CursorError;

impl CursorError {
    /// The JSON-RPC error code (-32602).
    #[must_use]
    pub const fn code(self) -> i32 {
        INVALID_PARAMS
    }
}

/// A key ring could not be built.
#[derive(Debug, thiserror::Error)]
pub enum KeyRingError {
    /// A key was not exactly 32 bytes.
    #[error("cursor key must be exactly {KEY_LEN} bytes")]
    BadLength,
    /// The ring has no key.
    #[error("cursor key ring is empty")]
    Empty,
    /// The system random source failed.
    #[error("no random source: {0}")]
    Random(String),
}

struct Key {
    id: [u8; KID_LEN],
    bytes: [u8; KEY_LEN],
}

impl Key {
    fn new(bytes: [u8; KEY_LEN]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut id = [0u8; KID_LEN];
        id.copy_from_slice(&digest[..KID_LEN]);
        Self { id, bytes }
    }

    fn mac(&self, msg: &[u8]) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.bytes).expect("HMAC takes any key length");
        mac.update(msg);
        mac
    }
}

/// The keys that seal and verify cursors. The first key signs, all keys verify.
pub struct KeyRing {
    keys: Vec<Key>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        f.debug_struct("KeyRing").field("keys", &self.keys.len()).finish()
    }
}

impl KeyRing {
    /// One random key: the stdio default, so a cursor does not outlive the process.
    ///
    /// # Errors
    /// [`KeyRingError::Random`] when the system random source fails.
    pub fn random() -> Result<Self, KeyRingError> {
        let mut k = [0u8; KEY_LEN];
        getrandom::fill(&mut k).map_err(|e| KeyRingError::Random(e.to_string()))?;
        Ok(Self { keys: vec![Key::new(k)] })
    }

    /// A ring from raw key bytes, signing key first (HTTP: the contents of `[mcp] hmac_keys`).
    ///
    /// # Errors
    /// [`KeyRingError::Empty`] for no keys, [`KeyRingError::BadLength`] for a key not 32 bytes.
    pub fn from_keys<K: AsRef<[u8]>>(keys: &[K]) -> Result<Self, KeyRingError> {
        if keys.is_empty() {
            return Err(KeyRingError::Empty);
        }
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let bytes: [u8; KEY_LEN] = k.as_ref().try_into().map_err(|_| KeyRingError::BadLength)?;
            out.push(Key::new(bytes));
        }
        Ok(Self { keys: out })
    }

    /// Seal a cursor for `tool` and `args` at `position`, valid for [`CURSOR_TTL_SECS`] from `now`
    /// (Unix seconds).
    #[must_use]
    pub fn seal_at(&self, tool: &str, args: &Value, position: Position, now: u64) -> String {
        let payload = Payload {
            t: tool.to_owned(),
            a: arg_digest(tool, args),
            p: position,
            x: now.saturating_add(CURSOR_TTL_SECS),
        };
        let json = serde_json::to_vec(&payload).expect("a cursor payload always serialises");
        let key = &self.keys[0];
        let mut msg = Vec::with_capacity(KID_LEN + json.len() + MAC_LEN);
        msg.extend_from_slice(&key.id);
        msg.extend_from_slice(&json);
        let tag = key.mac(&msg).finalize().into_bytes();
        msg.extend_from_slice(&tag);
        URL_SAFE_NO_PAD.encode(msg)
    }

    /// [`Self::seal_at`] with the system clock.
    #[must_use]
    pub fn seal(&self, tool: &str, args: &Value, position: Position) -> String {
        self.seal_at(tool, args, position, unix_now())
    }

    /// Check a cursor for `tool` and `args` at `now` and return its position.
    ///
    /// # Errors
    /// [`CursorError`] for a cursor that is malformed, unsigned by any key of the ring, expired,
    /// or issued for another tool or other arguments.
    pub fn open_at(&self, tool: &str, args: &Value, cursor: &str, now: u64) -> Result<Position, CursorError> {
        if cursor.len() > MAX_CURSOR_CHARS {
            return Err(CursorError);
        }
        let raw = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| CursorError)?;
        if raw.len() <= KID_LEN + MAC_LEN {
            return Err(CursorError);
        }
        let (msg, tag) = raw.split_at(raw.len() - MAC_LEN);
        let kid = &msg[..KID_LEN];
        // Try every key with this id; a collision on 4 bytes just costs another MAC.
        let ok = self
            .keys
            .iter()
            .filter(|k| k.id == kid)
            .any(|k| k.mac(msg).verify_slice(tag).is_ok());
        if !ok {
            return Err(CursorError);
        }
        let payload: Payload = serde_json::from_slice(&msg[KID_LEN..]).map_err(|_| CursorError)?;
        if now > payload.x || payload.t != tool || payload.a != arg_digest(tool, args) {
            return Err(CursorError);
        }
        Ok(payload.p)
    }

    /// [`Self::open_at`] with the system clock.
    ///
    /// # Errors
    /// As [`Self::open_at`].
    pub fn open(&self, tool: &str, args: &Value, cursor: &str) -> Result<Position, CursorError> {
        self.open_at(tool, args, cursor, unix_now())
    }
}

#[derive(Serialize, Deserialize)]
struct Payload {
    t: String,
    a: String,
    p: Position,
    x: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// SHA-256 (hex) of the tool name and the canonical arguments without `cursor` and `max_tokens`.
#[must_use]
pub fn arg_digest(tool: &str, args: &Value) -> String {
    let mut canon = String::new();
    canon_json(args, true, &mut canon);
    let mut h = Sha256::new();
    h.update((tool.len() as u64).to_be_bytes());
    h.update(tool.as_bytes());
    h.update(canon.as_bytes());
    h.finalize().iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn canon_json(v: &Value, top: bool, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().filter(|k| !(top && PAGING_ARGS.contains(&k.as_str()))).collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canon_json(&m[k], false, out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canon_json(e, false, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ring(b: u8) -> KeyRing {
        KeyRing::from_keys(&[[b; 32]]).unwrap()
    }

    fn pos() -> Position {
        Position::File { offset: 4096, line: 120 }
    }

    #[test]
    fn cursor_round_trips_and_ignores_paging_args() {
        let r = ring(1);
        let a = json!({"path": "a.rs", "max_tokens": 100});
        let c = r.seal_at("read_file", &a, pos(), 1000);
        let b = json!({"max_tokens": 5000, "path": "a.rs", "cursor": c});
        assert_eq!(r.open_at("read_file", &b, &c, 1001).unwrap(), pos());
    }

    #[test]
    fn cursor_from_another_call_is_invalid_params() {
        let r = ring(1);
        let c = r.seal_at("read_file", &json!({"path": "a"}), pos(), 0);
        let e = r.open_at("read_file", &json!({"path": "b"}), &c, 1).unwrap_err();
        assert_eq!(e.code(), -32602);
        assert!(r.open_at("grep", &json!({"path": "a"}), &c, 1).is_err());
    }

    #[test]
    fn cursor_after_restart_or_with_foreign_key_is_invalid_params() {
        let a = json!({});
        let c = ring(1).seal_at("log", &a, Position::Index { index: 3 }, 0);
        assert_eq!(ring(2).open_at("log", &a, &c, 1).unwrap_err().code(), -32602);
        let fresh = KeyRing::random().unwrap();
        assert!(fresh.open_at("log", &a, &c, 1).is_err());
    }

    #[test]
    fn key_ring_signs_with_first_and_verifies_with_all() {
        let a = json!({});
        let old = ring(1);
        let c_old = old.seal_at("log", &a, Position::Index { index: 1 }, 0);
        let rotated = KeyRing::from_keys(&[[2u8; 32], [1u8; 32]]).unwrap();
        assert_eq!(rotated.open_at("log", &a, &c_old, 1).unwrap(), Position::Index { index: 1 });
        let c_new = rotated.seal_at("log", &a, Position::Index { index: 2 }, 0);
        assert!(old.open_at("log", &a, &c_new, 1).is_err());
        assert!(ring(2).open_at("log", &a, &c_new, 1).is_ok());
    }

    #[test]
    fn tampered_truncated_expired_and_oversized_cursors_fail() {
        let r = ring(1);
        let a = json!({});
        let c = r.seal_at("log", &a, Position::Index { index: 1 }, 100);
        let mut raw = URL_SAFE_NO_PAD.decode(&c).unwrap();
        let mid = raw.len() / 2;
        raw[mid] ^= 1;
        assert!(r.open_at("log", &a, &URL_SAFE_NO_PAD.encode(&raw), 101).is_err());
        assert!(r.open_at("log", &a, &c[..c.len() - 4], 101).is_err());
        assert!(r.open_at("log", &a, "", 101).is_err());
        assert!(r.open_at("log", &a, "!!!", 101).is_err());
        assert!(r.open_at("log", &a, &"A".repeat(5000), 101).is_err());
        assert!(r.open_at("log", &a, &c, 100 + CURSOR_TTL_SECS).is_ok());
        assert!(r.open_at("log", &a, &c, 101 + CURSOR_TTL_SECS).is_err());
    }

    #[test]
    fn key_ring_rejects_bad_keys_and_hides_material() {
        assert!(matches!(KeyRing::from_keys::<[u8; 32]>(&[]), Err(KeyRingError::Empty)));
        assert!(matches!(KeyRing::from_keys(&[vec![0u8; 31]]), Err(KeyRingError::BadLength)));
        assert!(!format!("{:?}", ring(7)).contains('7'));
    }

    #[test]
    fn digest_is_independent_of_key_order() {
        let x = arg_digest("t", &json!({"a": 1, "b": {"c": 2, "d": 3}}));
        let y = arg_digest("t", &json!({"b": {"d": 3, "c": 2}, "a": 1}));
        assert_eq!(x, y);
        assert_ne!(x, arg_digest("t", &json!({"a": 2, "b": {"c": 2, "d": 3}})));
    }
}
