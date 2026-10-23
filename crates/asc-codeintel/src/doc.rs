//! Doc rows (SPEC §23.2.1): a dense `ord` per `FileId`, the byte-decided flags, and the
//! languages that the paths of a `FileId` give.

use crate::lang::{self, Lang};
use crate::secrets;
use crate::skip::{self, Flags};
use asc_core::id::FileId;
use std::collections::HashMap;

/// Most languages kept per doc (SPEC §23.2.1).
pub const MAX_LANGS: usize = 4;

/// Extractor versions recorded on a row; a bump re-extracts lazily (SPEC §23.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Versions {
    pub grams: u32,
    pub tags: u32,
    pub chunker: u32,
}

/// One `ci_docs` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocRow {
    pub ord: u32,
    pub size: u64,
    pub flags: Flags,
    pub versions: Versions,
}

impl DocRow {
    /// Builds the row from the file bytes alone: size, skip flags and the content secret
    /// scan. Path rules are not bytes, so they are applied with [`secrets::path_is_secret`].
    pub fn from_bytes(ord: u32, bytes: &[u8], versions: Versions) -> DocRow {
        let mut flags = skip::classify(bytes);
        if flags.indexable() && secrets::scan_content(bytes).is_secret() {
            flags.insert(Flags::SECRET);
        }
        DocRow {
            ord,
            size: bytes.len() as u64,
            flags,
            versions,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DocError {
    #[error("doc ordinal space exhausted")]
    OrdsExhausted,
}

/// Dense ordinal assignment for one namespace. The same `FileId` always gets the same
/// `ord`, at any number of paths.
#[derive(Debug, Default)]
pub struct OrdTable {
    ords: HashMap<FileId, u32>,
    next: u32,
}

impl OrdTable {
    /// Resumes from stored `(FileId, ord)` pairs. Ords must be dense and unique, else `None`.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (FileId, u32)>) -> Option<Self> {
        let mut ords = HashMap::new();
        for (f, o) in pairs {
            if ords.insert(f, o).is_some() {
                return None;
            }
        }
        let n = u32::try_from(ords.len()).ok()?;
        let mut seen = vec![false; ords.len()];
        for &o in ords.values() {
            let slot = seen.get_mut(o as usize)?;
            if *slot {
                return None;
            }
            *slot = true;
        }
        Some(OrdTable { ords, next: n })
    }

    /// The ord of `id`, assigning the next one if new. Returns `(ord, is_new)`.
    pub fn ord_for(&mut self, id: FileId) -> Result<(u32, bool), DocError> {
        if let Some(&o) = self.ords.get(&id) {
            return Ok((o, false));
        }
        let o = self.next;
        self.next = o.checked_add(1).ok_or(DocError::OrdsExhausted)?;
        self.ords.insert(id, o);
        Ok((o, true))
    }

    pub fn get(&self, id: &FileId) -> Option<u32> {
        self.ords.get(id).copied()
    }

    pub fn len(&self) -> usize {
        self.ords.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ords.is_empty()
    }
}

/// The distinct languages of one `FileId`, from every path that holds it (`ci_doc_langs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocLangs(Vec<Lang>);

impl DocLangs {
    /// Adds the language of `path` (from extension, else the shebang in `head`). Returns
    /// true when the set changed. A fifth distinct language is dropped, so rows stay <= 4.
    pub fn add_path(&mut self, path: &str, head: &[u8]) -> bool {
        let Some(l) = lang::detect(path, head) else {
            return false;
        };
        if self.0.contains(&l) || self.0.len() >= MAX_LANGS {
            return false;
        }
        self.0.push(l);
        self.0.sort_unstable();
        true
    }

    pub fn as_slice(&self) -> &[Lang] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_id_at_two_paths_gets_one_ord() {
        let mut t = OrdTable::default();
        let a = FileId::of(b"alpha");
        let b = FileId::of(b"beta");
        assert_eq!(t.ord_for(a), Ok((0, true)));
        assert_eq!(t.ord_for(b), Ok((1, true)));
        // Second path holding the same bytes: same ord, not new.
        assert_eq!(t.ord_for(a), Ok((0, false)));
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(&b), Some(1));
    }

    #[test]
    fn resume_requires_dense_unique_ords() {
        let a = FileId::of(b"a");
        let b = FileId::of(b"b");
        let mut t = OrdTable::from_pairs([(a, 1), (b, 0)]).unwrap();
        assert_eq!(t.ord_for(FileId::of(b"c")), Ok((2, true)));
        assert!(OrdTable::from_pairs([(a, 0), (b, 2)]).is_none());
        assert!(OrdTable::from_pairs([(a, 0), (b, 0)]).is_none());
        assert!(OrdTable::from_pairs([(a, 0), (a, 1)]).is_none());
    }

    #[test]
    fn row_flags_from_bytes_only() {
        let v = Versions {
            grams: 1,
            tags: 1,
            chunker: 1,
        };
        let ok = DocRow::from_bytes(0, b"fn a() {}\n", v);
        assert_eq!((ok.size, ok.flags), (10, Flags::default()));
        let bin = DocRow::from_bytes(1, b"a\0b", v);
        assert!(bin.flags.contains(Flags::BINARY) && !bin.flags.indexable());
        let sec = DocRow::from_bytes(2, b"k = AKIAIOSFODNN7EXAMPLE\n", v);
        assert!(sec.flags.contains(Flags::SECRET) && sec.flags.indexable());
    }

    #[test]
    fn languages_per_path_are_a_set_of_at_most_four() {
        let mut l = DocLangs::default();
        assert!(l.add_path("a/x.ts", b""));
        assert!(!l.add_path("b/y.ts", b""));
        assert!(!l.add_path("README.md", b""));
        assert!(l.add_path("z.tsx", b""));
        assert!(l.add_path("w", b"#!/usr/bin/env python3\n"));
        assert!(l.add_path("m.go", b""));
        assert!(!l.add_path("n.rs", b""));
        assert_eq!(l.as_slice().len(), MAX_LANGS);
    }
}
