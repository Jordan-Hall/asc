//! An entry and its blobs as separate pieces, for the server tier's file-level routes
//! (docs/designs/build-cache.md §3.1): the entry document, each blob zstd-compressed and checked
//! against its id, and accepting a fetched entry once every blob it names is in the store. A
//! build that changed one output file sends that one file; a machine that holds the others
//! fetches only the changed one.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};

use crate::hex;
use crate::key::ActionKey;
use crate::store::{Action, Store};

/// Largest entry document accepted from the server.
pub const MAX_ENTRY_DOC: usize = 4 << 20;
/// Largest blob sent or fetched on its own (uncompressed). A larger output keeps its entry on
/// the artefact route.
pub const MAX_FILE: u64 = 512 << 20;

/// An entry document as stored, with the blobs it names.
pub type EntryDoc = (Vec<u8>, Vec<[u8; 32]>);

fn bad(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.into())
}

/// Parses an entry document and checks what [`Store::unpack`] checks of one: safe output paths
/// and well-formed blob ids. Returns the entry and the blobs it names.
pub fn parse_entry(body: &[u8]) -> io::Result<(Action, BTreeSet<[u8; 32]>)> {
    if body.len() > MAX_ENTRY_DOC {
        return Err(bad("the entry is too large"));
    }
    let action: Action =
        serde_json::from_slice(body).map_err(|e| bad(format!("bad entry: {e}")))?;
    crate::store::check_paths(&action)?;
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
    let blobs = action.blobs().collect();
    Ok((action, blobs))
}

impl Store {
    /// The entry document of `key` as stored, with its blobs. `None` when the entry or one of
    /// its blobs is gone.
    pub fn entry_doc(&self, key: &ActionKey) -> io::Result<Option<EntryDoc>> {
        let body = match fs::read(self.entry_path(key)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let Ok((_, blobs)) = parse_entry(&body) else {
            return Ok(None);
        };
        if blobs.iter().any(|b| !self.has_blob(b)) {
            return Ok(None);
        }
        Ok(Some((body, blobs.into_iter().collect())))
    }

    /// Blob `id`, zstd-compressed. `None` when it is gone or over [`MAX_FILE`].
    pub fn blob_zstd(&self, id: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        let f = match fs::File::open(self.blob_path(id)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if f.metadata()?.len() > MAX_FILE {
            return Ok(None);
        }
        zstd::encode_all(f, 3).map(Some)
    }

    /// Stores a zstd-compressed blob fetched for `id`. Refused when its content is not `id`'s or
    /// is over [`MAX_FILE`]; returns the uncompressed size.
    pub fn put_blob_zstd(&self, id: &[u8; 32], compressed: &[u8]) -> io::Result<u64> {
        let mut z = zstd::Decoder::new(compressed)?.take(MAX_FILE + 1);
        let (got, size) = self.put_blob_reader(&mut z)?;
        if size > MAX_FILE {
            return Err(bad("the file is too large"));
        }
        if &got != id {
            return Err(bad(format!(
                "file {} arrived with other content ({})",
                hex(id),
                hex(&got)
            )));
        }
        Ok(size)
    }

    /// Stores a fetched entry document under `key` once every blob it names is in the store.
    pub fn accept_entry(&self, key: &ActionKey, body: &[u8]) -> io::Result<Action> {
        let (action, blobs) = parse_entry(body)?;
        if let Some(b) = blobs.iter().find(|b| !self.has_blob(b)) {
            return Err(bad(format!("the entry's file {} is missing", hex(b))));
        }
        self.put_action(key, &action)?;
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Origin;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("bc")).unwrap();
        (d, s)
    }

    fn saved(s: &Store, d: &std::path::Path, files: &[(&str, &[u8])]) -> ActionKey {
        let base = d.join("w");
        for (p, b) in files {
            let p = base.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b).unwrap();
        }
        let rels: Vec<String> = files.iter().map(|(p, _)| (*p).to_owned()).collect();
        let mut a = Action::new("vite", "web", Origin::default());
        a.outputs = s.put_outputs(&base, &rels).unwrap();
        let key = ActionKey([7; 32]);
        s.put_action(&key, &a).unwrap();
        key
    }

    #[test]
    fn a_blob_round_trips_compressed_and_is_checked_against_its_id() {
        let (_d, s) = store();
        let (id, _) = s.put_blob(b"export default 1;\n").unwrap();
        let z = s.blob_zstd(&id).unwrap().unwrap();
        let (_d2, t) = store();
        assert_eq!(t.put_blob_zstd(&id, &z).unwrap(), 18);
        assert!(t.has_blob(&id));
        let other = zstd::encode_all(&b"tampered"[..], 3).unwrap();
        let err = t.put_blob_zstd(&id, &other).unwrap_err();
        assert!(err.to_string().contains("other content"), "{err}");
        assert!(t.put_blob_zstd(&id, b"not zstd").is_err());
        assert!(s.blob_zstd(&[9; 32]).unwrap().is_none());
    }

    #[test]
    fn an_entry_is_accepted_only_with_all_its_files() {
        let (d, s) = store();
        let key = saved(&s, d.path(), &[("dist/a.js", b"a"), ("dist/b.js", b"b")]);
        let (doc, blobs) = s.entry_doc(&key).unwrap().unwrap();
        assert_eq!(blobs.len(), 2);
        let (_d2, t) = store();
        let err = t.accept_entry(&key, &doc).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        for b in &blobs {
            t.put_blob_zstd(b, &s.blob_zstd(b).unwrap().unwrap())
                .unwrap();
        }
        let a = t.accept_entry(&key, &doc).unwrap();
        assert_eq!(a.outputs.len(), 2);
        assert_eq!(t.get_action(&key).unwrap().unwrap(), a);
        // A missing blob makes the local entry unusable for sending.
        fs::remove_file(s.blob_path(&blobs[0])).unwrap();
        assert!(s.entry_doc(&key).unwrap().is_none());
    }

    #[test]
    fn an_unsafe_entry_is_refused() {
        let id = hex(&[1; 32]);
        for body in [
            format!(
                r#"{{"tool":"t","outputs":[{{"path":"../x","kind":"file","size":1,"blob":"{id}"}}]}}"#
            ),
            format!(
                r#"{{"tool":"t","outputs":[{{"path":"/abs","kind":"file","size":1,"blob":"{id}"}}]}}"#
            ),
            r#"{"tool":"t","outputs":[{"path":"x","kind":"file","size":1,"blob":"zz"}]}"#
                .to_owned(),
            "{".to_owned(),
        ] {
            assert!(parse_entry(body.as_bytes()).is_err(), "{body}");
        }
        assert!(parse_entry(&vec![b' '; MAX_ENTRY_DOC + 1]).is_err());
    }
}
