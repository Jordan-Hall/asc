//! One entry and its blobs as a single artefact for the server tier (docs/designs/build-cache.md
//! §3), and the queue of entries waiting to be uploaded.
//!
//! ```text
//! zstd(  "ASCBC01\n"  key[32]  u32 entry-length  entry JSON
//!        u32 blob-count  { id[32]  u64 length  bytes }*  )
//! ```
//!
//! Unpacking stores each blob under the id of the bytes actually received and refuses the
//! artefact when that differs from the id it declares, when a blob the entry needs is absent, or
//! when the artefact carries a blob the entry does not use.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};

use crate::key::ActionKey;
use crate::store::{Action, Store};
use crate::{hex, unhex};

const MAGIC: &[u8; 8] = b"ASCBC01\n";
/// Largest entry document in an artefact.
const MAX_ENTRY: u32 = 4 << 20;
/// Most bytes an artefact may unpack to.
pub const MAX_UNPACKED: u64 = 4 << 30;

fn bad(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}

fn read_u32(r: &mut dyn Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut dyn Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

impl Store {
    /// Writes the entry `key` and its blobs to `w`. `Ok(false)` when the entry or one of its
    /// blobs is gone.
    pub fn pack(&self, key: &ActionKey, w: &mut dyn Write) -> io::Result<bool> {
        let Ok(body) = fs::read(self.entry_path(key)) else {
            return Ok(false);
        };
        let Ok(action) = serde_json::from_slice::<Action>(&body) else {
            return Ok(false);
        };
        let blobs: BTreeSet<[u8; 32]> = action.blobs().collect();
        if blobs.iter().any(|b| !self.has_blob(b)) {
            return Ok(false);
        }
        let mut z = zstd::Encoder::new(w, 3)?;
        z.write_all(MAGIC)?;
        z.write_all(&key.0)?;
        z.write_all(&(body.len() as u32).to_le_bytes())?;
        z.write_all(&body)?;
        z.write_all(&(blobs.len() as u32).to_le_bytes())?;
        for id in &blobs {
            let mut f = match fs::File::open(self.blob_path(id)) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            };
            let len = f.metadata()?.len();
            z.write_all(id)?;
            z.write_all(&len.to_le_bytes())?;
            let copied = io::copy(&mut (&mut f).take(len), &mut z)?;
            if copied != len {
                return Err(bad("a blob changed while it was packed"));
            }
        }
        z.finish()?;
        Ok(true)
    }

    /// Reads an artefact made by [`Store::pack`] for `expected` into the store and returns its
    /// entry. Nothing is written for an entry whose artefact is refused.
    pub fn unpack(&self, expected: &ActionKey, r: &mut dyn Read) -> io::Result<Action> {
        let mut z = zstd::Decoder::new(r)?.take(MAX_UNPACKED);
        let mut magic = [0u8; 8];
        z.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(bad("not a build cache artefact"));
        }
        let mut key = [0u8; 32];
        z.read_exact(&mut key)?;
        if key != expected.0 {
            return Err(bad("the artefact is for another entry"));
        }
        let n = read_u32(&mut z)?;
        if n > MAX_ENTRY {
            return Err(bad("the entry is too large"));
        }
        let mut body = vec![0u8; n as usize];
        z.read_exact(&mut body)?;
        let action: Action =
            serde_json::from_slice(&body).map_err(|e| bad(format!("bad entry: {e}")))?;
        for o in &action.outputs {
            crate::store::check_rel(&o.path)?;
        }
        let mut want: BTreeSet<[u8; 32]> = action.blobs().collect();
        let declared = action
            .outputs
            .iter()
            .map(|o| o.blob.as_str())
            .chain(action.stdout.as_deref())
            .chain(action.stderr.as_deref())
            .count();
        if action.blobs().count() != declared {
            return Err(bad("a blob id is not 64 hex digits"));
        }
        let count = read_u32(&mut z)?;
        if count as usize != want.len() {
            return Err(bad("the artefact's blobs do not match its entry"));
        }
        for _ in 0..count {
            let mut id = [0u8; 32];
            z.read_exact(&mut id)?;
            let len = read_u64(&mut z)?;
            if !want.remove(&id) {
                return Err(bad("the artefact carries a blob its entry does not use"));
            }
            let mut part = (&mut z).take(len);
            let (got, size) = self.put_blob_reader(&mut part)?;
            if size != len {
                return Err(bad("the artefact is truncated"));
            }
            if got != id {
                return Err(bad(format!(
                    "blob {} arrived with other content ({})",
                    hex(&id),
                    hex(&got)
                )));
            }
        }
        if !want.is_empty() {
            return Err(bad("the artefact lacks a blob of its entry"));
        }
        self.put_action(expected, &action)?;
        Ok(action)
    }

    /// Marks `key` for upload by `asc cache push`.
    pub fn queue_push(&self, key: &ActionKey) -> io::Result<()> {
        let dir = self.root().join("outbox");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(key.to_string()), b"")
    }

    /// The keys waiting for upload.
    pub fn queued(&self) -> io::Result<Vec<ActionKey>> {
        let dir = self.root().join("outbox");
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out: Vec<ActionKey> = rd
            .filter_map(|e| ActionKey::parse(&e.ok()?.file_name().into_string().ok()?))
            .collect();
        out.sort();
        Ok(out)
    }

    pub fn unqueue(&self, key: &ActionKey) -> io::Result<()> {
        match fs::remove_file(self.root().join("outbox").join(key.to_string())) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// A key from its artefact name on the server (`asc-<64 hex>`).
pub fn key_of_artifact(name: &str) -> Option<ActionKey> {
    name.strip_prefix("asc-").and_then(unhex).map(ActionKey)
}

/// The artefact name of `key` on the server: turbo hashes are `[A-Za-z0-9_-]`, at most 128.
pub fn artifact_name(key: &ActionKey) -> String {
    format!("asc-{key}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Origin, Output, OutputKind};

    fn seeded() -> (tempfile::TempDir, Store, ActionKey, Vec<u8>) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("a")).unwrap();
        let w = d.path().join("w");
        fs::create_dir_all(w.join("out")).unwrap();
        fs::write(w.join("out/x"), vec![3u8; 70_000]).unwrap();
        fs::write(w.join("out/y"), b"y").unwrap();
        let key = ActionKey::builder("t").part("p", b"1").finish();
        let mut a = Action::new("t", "lbl", Origin::default());
        a.outputs = s.put_outputs(&w, &["out".into()]).unwrap();
        a.stdout = Some(hex(&s.put_blob(b"warning: x").unwrap().0));
        s.put_action(&key, &a).unwrap();
        let mut packed = Vec::new();
        assert!(s.pack(&key, &mut packed).unwrap());
        (d, s, key, packed)
    }

    #[test]
    fn pack_round_trips_into_another_store() {
        let (d, s, key, packed) = seeded();
        assert!(packed.len() < 2000, "compressed: {}", packed.len());
        let other = Store::open(d.path().join("b")).unwrap();
        let a = other.unpack(&key, &mut &packed[..]).unwrap();
        assert_eq!(Some(a.clone()), s.get_action(&key).unwrap());
        assert!(other.restore(&a, &d.path().join("w2")).unwrap());
        assert_eq!(fs::read(d.path().join("w2/out/y")).unwrap(), b"y");
        let mut out = Vec::new();
        other
            .copy_blob_to(a.stdout.as_deref().unwrap(), &mut out)
            .unwrap();
        assert_eq!(out, b"warning: x");
    }

    #[test]
    fn unpack_refuses_tampering() {
        let (d, _s, key, packed) = seeded();
        let other = Store::open(d.path().join("b")).unwrap();
        let wrong = ActionKey::builder("t").finish();
        assert!(other.unpack(&wrong, &mut &packed[..]).is_err());

        // Flip one byte of a blob's content inside the stream.
        let mut raw = zstd::decode_all(&packed[..]).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 1;
        let tampered = zstd::encode_all(&raw[..], 3).unwrap();
        let e = other.unpack(&key, &mut &tampered[..]).unwrap_err();
        assert!(e.to_string().contains("other content"), "{e}");
        assert!(other.get_action(&key).unwrap().is_none());

        let cut = zstd::encode_all(&raw[..n - 10], 3).unwrap();
        assert!(other.unpack(&key, &mut &cut[..]).is_err());
        assert!(other.unpack(&key, &mut &b"junk"[..]).is_err());
    }

    #[test]
    fn unpack_refuses_escaping_paths() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("a")).unwrap();
        let (id, _) = s.put_blob(b"x").unwrap();
        let key = ActionKey::builder("t").finish();
        let a = Action {
            outputs: vec![Output {
                path: "../../x".into(),
                kind: OutputKind::File,
                size: 1,
                blob: hex(&id),
            }],
            ..Action::new("t", "", Origin::default())
        };
        let body = serde_json::to_vec(&a).unwrap();
        let mut raw = Vec::new();
        raw.extend_from_slice(MAGIC);
        raw.extend_from_slice(&key.0);
        raw.extend_from_slice(&(body.len() as u32).to_le_bytes());
        raw.extend_from_slice(&body);
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&id);
        raw.extend_from_slice(&1u64.to_le_bytes());
        raw.push(b'x');
        let z = zstd::encode_all(&raw[..], 3).unwrap();
        assert!(s.unpack(&key, &mut &z[..]).is_err());
    }

    #[test]
    fn the_outbox_queues_keys() {
        let (_d, s, key, _) = seeded();
        assert!(s.queued().unwrap().is_empty());
        s.queue_push(&key).unwrap();
        s.queue_push(&key).unwrap();
        assert_eq!(s.queued().unwrap(), [key]);
        s.unqueue(&key).unwrap();
        s.unqueue(&key).unwrap();
        assert!(s.queued().unwrap().is_empty());
        assert_eq!(key_of_artifact(&artifact_name(&key)), Some(key));
        assert!(artifact_name(&key).len() <= 128);
    }
}
