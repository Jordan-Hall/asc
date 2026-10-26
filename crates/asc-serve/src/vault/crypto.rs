//! Sealing and opening one secret (docs/BUILDOUT.md, "Secrets").
//!
//! Stored form: `generation (u32 BE) || nonce (24) || ciphertext || tag (16)`.
//!
//! The data key is HKDF-SHA256 over the master key of that generation with the context as the
//! `info`, and the same context (plus the generation) is the associated data, so a sealed value
//! cannot be moved to another kind, owner or row, and the generation in the header cannot be
//! rewritten. The nonce is 24 random bytes per value (XChaCha20 makes random nonces safe).

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use super::VaultError;

/// Fixed prefix of every context.
const DOMAIN: &[u8] = b"asc-vault-v1";
/// Nonce length of XChaCha20-Poly1305.
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// Header length: the generation.
pub const HEADER_LEN: usize = 4;
/// Largest plaintext one value may hold.
pub const MAX_SECRET: usize = 16 * 1024;

/// What binds a sealed value to its place: the kind of secret, the owner it belongs to and the
/// row that holds it.
#[derive(Clone, Copy, Debug)]
pub struct SecretContext<'a> {
    /// `"storage"`, `"ai-key"`, ...
    pub kind: &'a str,
    /// `"user:alice"`, `"org:acme"`, `"repo:acme/app"`.
    pub owner: &'a str,
    /// The row id.
    pub id: &'a str,
}

impl SecretContext<'_> {
    /// `"asc-vault-v1" || len(kind) || kind || len(owner) || owner || len(id) || id`, each length
    /// a u16, so no two contexts encode alike.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DOMAIN.len() + 6 + self.kind.len() + self.owner.len() + self.id.len());
        out.extend_from_slice(DOMAIN);
        for part in [self.kind, self.owner, self.id] {
            let len = u16::try_from(part.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&part.as_bytes()[..usize::from(len)]);
        }
        out
    }
}

fn data_key(master: &[u8; 32], context: &[u8]) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let mut okm = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, master)
        .expand(context, okm.as_mut())
        .map_err(|_| VaultError::Crypto)?;
    Ok(okm)
}

fn aad(context: &[u8], generation: u32) -> Vec<u8> {
    let mut aad = context.to_vec();
    aad.extend_from_slice(&generation.to_be_bytes());
    aad
}

/// Seals `plaintext` under `master` (generation `generation`).
pub fn seal(
    master: &[u8; 32],
    generation: u32,
    ctx: &SecretContext<'_>,
    plaintext: &[u8],
) -> Result<Vec<u8>, VaultError> {
    if plaintext.len() > MAX_SECRET {
        return Err(VaultError::TooLarge);
    }
    let context = ctx.encode();
    let key = data_key(master, &context)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| VaultError::Crypto)?;
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| VaultError::Crypto)?;
    let ct = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &aad(&context, generation),
            },
        )
        .map_err(|_| VaultError::Crypto)?;
    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(&generation.to_be_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// The generation a sealed value says it was sealed under.
pub fn generation_of(sealed: &[u8]) -> Option<u32> {
    let head: [u8; HEADER_LEN] = sealed.get(..HEADER_LEN)?.try_into().ok()?;
    Some(u32::from_be_bytes(head))
}

/// Opens a sealed value with the master key of its generation. Any change to the value, the
/// context or the generation fails with [`VaultError::Corrupt`].
pub fn open(
    master: &[u8; 32],
    ctx: &SecretContext<'_>,
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let generation = generation_of(sealed).ok_or(VaultError::Corrupt)?;
    if sealed.len() < HEADER_LEN + NONCE_LEN + TAG_LEN {
        return Err(VaultError::Corrupt);
    }
    let nonce: [u8; NONCE_LEN] = sealed[HEADER_LEN..HEADER_LEN + NONCE_LEN]
        .try_into()
        .map_err(|_| VaultError::Corrupt)?;
    let context = ctx.encode();
    let key = data_key(master, &context)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| VaultError::Crypto)?;
    let plain = cipher
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &sealed[HEADER_LEN + NONCE_LEN..],
                aad: &aad(&context, generation),
            },
        )
        .map_err(|_| VaultError::Corrupt)?;
    Ok(Zeroizing::new(plain))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: [u8; 32] = [7; 32];

    fn ctx<'a>(kind: &'a str, owner: &'a str, id: &'a str) -> SecretContext<'a> {
        SecretContext { kind, owner, id }
    }

    #[test]
    fn round_trip() {
        let c = ctx("storage", "user:alice", "st_1");
        let sealed = seal(&MASTER, 3, &c, b"s3cr3t").unwrap();
        assert_eq!(generation_of(&sealed), Some(3));
        assert_eq!(sealed.len(), HEADER_LEN + NONCE_LEN + 6 + TAG_LEN);
        assert!(!sealed.windows(6).any(|w| w == b"s3cr3t"));
        assert_eq!(&open(&MASTER, &c, &sealed).unwrap()[..], b"s3cr3t");
        // An empty secret seals too.
        let empty = seal(&MASTER, 1, &c, b"").unwrap();
        assert!(open(&MASTER, &c, &empty).unwrap().is_empty());
    }

    #[test]
    fn nonces_are_fresh() {
        let c = ctx("storage", "user:alice", "st_1");
        let a = seal(&MASTER, 1, &c, b"x").unwrap();
        let b = seal(&MASTER, 1, &c, b"x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn another_owner_row_or_kind_fails() {
        let c = ctx("storage", "user:alice", "st_1");
        let sealed = seal(&MASTER, 1, &c, b"secret").unwrap();
        for other in [
            ctx("storage", "user:bob", "st_1"),
            ctx("storage", "user:alice", "st_2"),
            ctx("ai-key", "user:alice", "st_1"),
            // Boundaries move without changing the concatenation.
            ctx("storag", "euser:alice", "st_1"),
        ] {
            assert!(matches!(open(&MASTER, &other, &sealed), Err(VaultError::Corrupt)));
        }
    }

    #[test]
    fn any_flipped_bit_fails() {
        let c = ctx("storage", "user:alice", "st_1");
        let sealed = seal(&MASTER, 1, &c, b"secret").unwrap();
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert!(open(&MASTER, &c, &bad).is_err(), "byte {i}");
        }
        assert!(matches!(open(&MASTER, &c, &sealed[..10]), Err(VaultError::Corrupt)));
        assert!(matches!(open(&MASTER, &c, &[]), Err(VaultError::Corrupt)));
    }

    #[test]
    fn another_master_key_fails() {
        let c = ctx("storage", "user:alice", "st_1");
        let sealed = seal(&MASTER, 1, &c, b"secret").unwrap();
        assert!(open(&[8; 32], &c, &sealed).is_err());
    }

    #[test]
    fn oversized_secrets_are_refused() {
        let c = ctx("storage", "user:alice", "st_1");
        assert!(matches!(
            seal(&MASTER, 1, &c, &vec![0; MAX_SECRET + 1]),
            Err(VaultError::TooLarge)
        ));
        assert!(seal(&MASTER, 1, &c, &vec![0; MAX_SECRET]).is_ok());
    }
}
