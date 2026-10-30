//! Encryption of a backup file before it is uploaded.
//!
//! A file is cut into 4 MiB chunks. Each chunk is sealed with XChaCha20-Poly1305 under a key
//! derived (HKDF-SHA256) from the backup key; the nonce is a 16-byte random prefix, stored in the
//! file header, followed by the chunk counter. The additional data says whether the chunk is the
//! last one, so a file cannot be cut short or have chunks reordered or dropped without the open
//! failing. Layout: `"ASCB1\0"`, the 16-byte prefix, the chunks.

use std::io::{Read, Write};
use std::path::Path;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

/// Plaintext bytes per chunk.
pub const CHUNK: usize = 4 * 1024 * 1024;
const MAGIC: &[u8; 6] = b"ASCB1\0";
const PREFIX: usize = 16;
const TAG: usize = 16;
const HEADER: usize = MAGIC.len() + PREFIX;
const CONTEXT: &[u8] = b"asc-backup-stream-v1";

/// Why a file could not be sealed or opened. The text carries no key material.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("the file is not an encrypted backup file")]
    NotEncrypted,
    #[error(
        "the file does not open with this key: it is damaged, truncated or sealed under another key"
    )]
    Open,
}

/// A new file, mode 0600, never over an existing one.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

/// The size an encrypted file has for `plain` bytes.
pub fn sealed_len(plain: u64) -> u64 {
    let chunks = plain.div_ceil(CHUNK as u64).max(1);
    HEADER as u64 + plain + chunks * TAG as u64
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    let mut okm = [0u8; 32];
    Hkdf::<Sha256>::new(None, key)
        .expand(CONTEXT, &mut okm)
        .expect("32 bytes is a valid output length");
    XChaCha20Poly1305::new(&okm.into())
}

fn nonce(prefix: &[u8; PREFIX], counter: u64) -> XNonce {
    let mut n = [0u8; 24];
    n[..PREFIX].copy_from_slice(prefix);
    n[PREFIX..].copy_from_slice(&counter.to_be_bytes());
    n.into()
}

/// Reads up to `buf.len()` bytes, fewer only at the end of the input.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = r.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// Seals `src` into `dst`.
pub fn encrypt_file(key: &[u8; 32], src: &Path, dst: &Path) -> Result<u64, CryptoError> {
    let c = cipher(key);
    let mut input = std::fs::File::open(src)?;
    let mut out = create_private_file(dst)?;
    let mut prefix = [0u8; PREFIX];
    getrandom::fill(&mut prefix).expect("random source");
    out.write_all(MAGIC)?;
    out.write_all(&prefix)?;
    let mut cur = vec![0u8; CHUNK];
    let mut next = vec![0u8; CHUNK];
    let mut n = fill(&mut input, &mut cur)?;
    let mut counter = 0u64;
    let mut total = HEADER as u64;
    loop {
        let m = if n == CHUNK {
            fill(&mut input, &mut next)?
        } else {
            0
        };
        let last = m == 0;
        let sealed = c
            .encrypt(
                &nonce(&prefix, counter),
                Payload {
                    msg: &cur[..n],
                    aad: &[u8::from(last)],
                },
            )
            .map_err(|_| CryptoError::Open)?;
        out.write_all(&sealed)?;
        total += sealed.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        n = m;
        counter += 1;
    }
    out.sync_all()?;
    Ok(total)
}

/// Opens `src` into `dst`. A damaged, truncated or foreign file is an error and `dst` is removed.
pub fn decrypt_file(key: &[u8; 32], src: &Path, dst: &Path) -> Result<u64, CryptoError> {
    let r = decrypt_inner(key, src, dst);
    if r.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    r
}

fn decrypt_inner(key: &[u8; 32], src: &Path, dst: &Path) -> Result<u64, CryptoError> {
    let c = cipher(key);
    let mut input = std::fs::File::open(src)?;
    let mut head = [0u8; HEADER];
    if fill(&mut input, &mut head)? != HEADER || &head[..MAGIC.len()] != MAGIC {
        return Err(CryptoError::NotEncrypted);
    }
    let mut prefix = [0u8; PREFIX];
    prefix.copy_from_slice(&head[MAGIC.len()..]);
    let mut out = create_private_file(dst)?;
    let size = CHUNK + TAG;
    let mut cur = vec![0u8; size];
    let mut next = vec![0u8; size];
    let mut n = fill(&mut input, &mut cur)?;
    let mut counter = 0u64;
    let mut total = 0u64;
    loop {
        if n < TAG {
            return Err(CryptoError::Open);
        }
        let m = if n == size {
            fill(&mut input, &mut next)?
        } else {
            0
        };
        let last = m == 0;
        let plain = c
            .decrypt(
                &nonce(&prefix, counter),
                Payload {
                    msg: &cur[..n],
                    aad: &[u8::from(last)],
                },
            )
            .map_err(|_| CryptoError::Open)?;
        out.write_all(&plain)?;
        total += plain.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        n = m;
        counter += 1;
    }
    out.sync_all()?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(len: usize) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &data).unwrap();
        let key = [7u8; 32];
        let sealed = encrypt_file(&key, &a, &b).unwrap();
        assert_eq!(sealed, sealed_len(len as u64), "{len}");
        assert_eq!(std::fs::metadata(&b).unwrap().len(), sealed);
        assert_eq!(decrypt_file(&key, &b, &c).unwrap(), len as u64);
        assert_eq!(std::fs::read(&c).unwrap(), data);
    }

    #[test]
    fn files_of_every_shape_round_trip() {
        for len in [
            0,
            1,
            100,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            2 * CHUNK,
            2 * CHUNK + 5,
        ] {
            roundtrip(len);
        }
    }

    #[test]
    fn a_wrong_key_a_cut_file_and_a_flipped_byte_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        std::fs::write(&a, vec![9u8; CHUNK + 10]).unwrap();
        let key = [1u8; 32];
        encrypt_file(&key, &a, &b).unwrap();
        assert!(matches!(
            decrypt_file(&[2u8; 32], &b, &c),
            Err(CryptoError::Open)
        ));
        assert!(!c.exists(), "a failed open leaves nothing behind");
        let sealed = std::fs::read(&b).unwrap();
        // Dropping the last chunk is caught by the last-chunk flag.
        std::fs::write(&b, &sealed[..HEADER + CHUNK + TAG]).unwrap();
        assert!(decrypt_file(&key, &b, &c).is_err());
        let mut bad = sealed.clone();
        bad[HEADER + 3] ^= 1;
        std::fs::write(&b, &bad).unwrap();
        assert!(decrypt_file(&key, &b, &c).is_err());
        std::fs::write(&b, b"plain text").unwrap();
        assert!(matches!(
            decrypt_file(&key, &b, &c),
            Err(CryptoError::NotEncrypted)
        ));
    }
}
