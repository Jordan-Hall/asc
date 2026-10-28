//! Opaque, authenticated page tokens for the list RPCs.
//!
//! A token is `nonce || XChaCha20-Poly1305(payload)` in URL-safe base64. The associated data is the
//! scope the token was made for (the method and repository), the principal that asked and an
//! expiry, so a token read by a client says nothing, a changed byte is refused, and a token made
//! for another call, another repository or another caller is refused. Every refusal is the same
//! `invalid page token`. The payload is the cursor the endpoint always had (the last id of the page),
//! so nothing about paging order changes.
//!
//! The key is one random 32-byte secret kept in `server_secrets` under `page_key`, created by the
//! first replica that needs it and read by the others, so a token made by one replica opens on any
//! other. Before [`init`] has run (a unit test, a tool) a per-process random key is used.

use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use sqlx::AnyPool;
use tonic::Status;

/// How long a token is good for.
const TTL_SECS: u64 = 900;
const NONCE: usize = 24;
/// The longest token accepted, so a client cannot make the server decode a megabyte.
const MAX_TOKEN: usize = 512;

static KEY: OnceLock<[u8; 32]> = OnceLock::new();

fn key() -> &'static [u8; 32] {
    KEY.get_or_init(|| {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).expect("the operating system has randomness");
        k
    })
}

/// Loads (or creates) the shared page key. Call once at startup, after the migrations.
pub async fn init(pool: &AnyPool) -> Result<(), sqlx::Error> {
    let mut fresh = [0u8; 32];
    getrandom::fill(&mut fresh).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    sqlx::query(
        "INSERT INTO server_secrets (name, value) VALUES ('page_key', $1) \
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(fresh.to_vec())
    .execute(pool)
    .await?;
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT value FROM server_secrets WHERE name = 'page_key'")
            .fetch_one(pool)
            .await?;
    let k: [u8; 32] = stored
        .try_into()
        .map_err(|_| sqlx::Error::Protocol("the stored page key is not 32 bytes".into()))?;
    // A second call keeps the first value; the same database gives the same key anyway.
    let _ = KEY.set(k);
    Ok(())
}

fn cipher() -> XChaCha20Poly1305 {
    let k = blake3::derive_key("asc v1 list page token", key());
    XChaCha20Poly1305::new(&k.into())
}

fn aad(scope: &str, who: &str, expires: u64) -> Vec<u8> {
    let mut a = Vec::with_capacity(scope.len() + who.len() + 24);
    for part in [scope.as_bytes(), who.as_bytes()] {
        a.extend_from_slice(&(part.len() as u64).to_le_bytes());
        a.extend_from_slice(part);
    }
    a.extend_from_slice(&expires.to_le_bytes());
    a
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn refused() -> Status {
    Status::invalid_argument("invalid page token")
}

/// A token for `payload`, good for `scope` and `who` only.
pub fn seal(scope: &str, who: &str, payload: &str) -> String {
    let expires = now() + TTL_SECS;
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).expect("the operating system has randomness");
    let ct = cipher()
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: payload.as_bytes(),
                aad: &aad(scope, who, expires),
            },
        )
        .expect("sealing a short payload cannot fail");
    let mut raw = Vec::with_capacity(8 + NONCE + ct.len());
    raw.extend_from_slice(&expires.to_le_bytes());
    raw.extend_from_slice(&nonce);
    raw.extend_from_slice(&ct);
    URL_SAFE_NO_PAD.encode(raw)
}

/// The payload of `token`; every failure is the same refusal.
pub fn open(scope: &str, who: &str, token: &str) -> Result<String, Status> {
    if token.len() > MAX_TOKEN {
        return Err(refused());
    }
    let raw = URL_SAFE_NO_PAD.decode(token).map_err(|_| refused())?;
    if raw.len() < 8 + NONCE + 16 {
        return Err(refused());
    }
    let expires = u64::from_le_bytes(raw[..8].try_into().map_err(|_| refused())?);
    if expires < now() {
        return Err(refused());
    }
    let nonce: [u8; NONCE] = raw[8..8 + NONCE].try_into().map_err(|_| refused())?;
    let plain = cipher()
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &raw[8 + NONCE..],
                aad: &aad(scope, who, expires),
            },
        )
        .map_err(|_| refused())?;
    String::from_utf8(plain).map_err(|_| refused())
}

/// The numeric cursor of `token`; the empty token is `None`.
pub fn open_id(scope: &str, who: &str, token: &str) -> Result<Option<i64>, Status> {
    if token.is_empty() {
        return Ok(None);
    }
    open(scope, who, token)?
        .parse::<i64>()
        .map(Some)
        .map_err(|_| refused())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_opens_for_its_scope_and_caller_only() {
        let t = seal("Audit.List|o/r", "user:7", "1234");
        assert!(!t.contains("1234"), "the cursor is readable in {t}");
        assert_eq!(open("Audit.List|o/r", "user:7", &t).unwrap(), "1234");
        assert_eq!(open_id("Audit.List|o/r", "user:7", &t).unwrap(), Some(1234));
        for (scope, who) in [
            ("Audit.List|o/other", "user:7"),
            ("Ci.ListRuns|o/r", "user:7"),
            ("Audit.List|o/r", "user:8"),
        ] {
            assert_eq!(
                open(scope, who, &t).unwrap_err().message(),
                "invalid page token"
            );
        }
        assert_eq!(open_id("s", "w", "").unwrap(), None);
    }

    #[test]
    fn a_changed_late_or_foreign_token_is_refused_alike() {
        let t = seal("s", "w", "42");
        let mut bytes = t.clone().into_bytes();
        let mid = bytes.len() / 2;
        bytes[mid] = if bytes[mid] == b'A' { b'B' } else { b'A' };
        let flipped = String::from_utf8(bytes).unwrap();
        for bad in [flipped.as_str(), "42", "!!", "", &"A".repeat(2000)] {
            let e = open("s", "w", bad).unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument);
            assert_eq!(e.message(), "invalid page token", "{bad:?}");
        }
        // The expiry is part of what is authenticated: moving it forward breaks the token.
        let mut raw = URL_SAFE_NO_PAD.decode(&t).unwrap();
        raw[..8].copy_from_slice(&(now() + 10_000_000).to_le_bytes());
        assert!(open("s", "w", &URL_SAFE_NO_PAD.encode(raw)).is_err());
        // A token whose expiry has passed is refused even though it is genuine.
        let past = now() - 5;
        let mut nonce = [0u8; NONCE];
        getrandom::fill(&mut nonce).unwrap();
        let ct = cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: b"1",
                    aad: &aad("s", "w", past),
                },
            )
            .unwrap();
        let mut raw = past.to_le_bytes().to_vec();
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(&ct);
        assert!(open("s", "w", &URL_SAFE_NO_PAD.encode(raw)).is_err());
    }

    #[tokio::test]
    async fn the_key_is_shared_through_the_database() {
        let pool = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
        crate::migrate::migrate(&pool).await.unwrap();
        init(&pool).await.unwrap();
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT value FROM server_secrets WHERE name = 'page_key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.len(), 32);
        init(&pool).await.unwrap();
        let again: Vec<u8> =
            sqlx::query_scalar("SELECT value FROM server_secrets WHERE name = 'page_key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, again);
    }
}
