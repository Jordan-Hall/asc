//! The file side of symbol queries on the server (SPEC §23.3, PLAN I16): [`FileSource`] over one
//! namespace and one revision.
//!
//! The symbol engine is synchronous and asks for bytes, paths and flags one file at a time, so
//! it runs on a blocking thread and each answer is fetched with `Handle::block_on`. Every byte
//! it reads is hash-verified (`Objects::get_range` checks the key). A file that is missing,
//! corrupt, over the 1 MiB skip bound or chunked is "unavailable": the engine skips it.
//!
//! Secret flags fail closed: the flag of an analysed doc comes from `ci_docs`; a doc the worker
//! has not analysed yet is scanned from its bytes; if neither is possible the flag is unknown
//! and the renderer redacts the symbol.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use asc_codeintel::doc::{DocRow, Versions};
use asc_codeintel::lang::{self, Lang};
use asc_codeintel::search::{DocDirectory, Searcher};
use asc_codeintel::skip::{self, Flags};
use asc_codeintel::symbols::{FileSource, Result, SymbolError};
use asc_codeintel::tags::{self, Tag};
use asc_core::id::{FileId, TreeId};
use asc_core::path::RepoPath;
use asc_store::bucket::BucketObjects;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::sqlrefs::SqlRefs;
use roaring::RoaringBitmap;
use tokio::runtime::Handle;

use super::ords::{DocInfo, NsDocs};
use super::worker::read_tags;
use crate::idmode::Eng;

/// Paths of one doc the engine may ask for.
const MAX_DOC_PATHS: usize = 64;
/// Docs whose rows the source remembers.
const MAX_INFO_CACHE: usize = 4096;

pub type NsSearcher = Searcher<BucketObjects, SqlRefs, NsDocs, NsDocs>;

fn src<E: std::fmt::Display>(e: E) -> SymbolError {
    SymbolError::Source(e.to_string())
}

/// The bytes of `file` up to the skip bound, `None` when absent, corrupt or over the bound.
pub async fn file_bytes(objects: &dyn Objects, file: &FileId) -> Result<Option<Bytes>> {
    match objects
        .get_range(&ObjKey::File(*file), 0..skip::MAX_BYTES as u64 + 1)
        .await
    {
        Ok(Some(b)) if b.len() <= skip::MAX_BYTES => Ok(Some(b)),
        Ok(_) => Ok(None),
        Err(objects::Error::Corrupt(_)) | Err(objects::Error::Codec(..)) => Ok(None),
        Err(e) => Err(src(e)),
    }
}

/// The secret flag of `bytes`: the byte-decided scan every doc row is built with.
pub fn secret_of(bytes: &[u8]) -> bool {
    DocRow::from_bytes(0, bytes, Versions::default())
        .flags
        .contains(Flags::SECRET)
}

/// One namespace at one revision, for the symbol engine.
pub struct ServerFiles {
    pub handle: Handle,
    pub searcher: Arc<NsSearcher>,
    pub engine: Arc<Eng>,
    pub objects: Arc<BucketObjects>,
    pub docs: Arc<NsDocs>,
    pub root: TreeId,
    /// Ords the files index holds at the pinned generation.
    pub indexed: RoaringBitmap,
    pub info: Mutex<HashMap<u32, Option<DocInfo>>>,
}

impl ServerFiles {
    fn doc(&self, ord: u32) -> Option<DocInfo> {
        {
            let g = self.info.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(d) = g.get(&ord) {
                return d.clone();
            }
        }
        let found = self
            .handle
            .block_on(self.docs.info(&[ord]))
            .ok()
            .and_then(|mut m| m.remove(&ord));
        let mut g = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() >= MAX_INFO_CACHE {
            g.clear();
        }
        g.insert(ord, found.clone());
        found
    }

    fn file(&self, ord: u32) -> Option<FileId> {
        self.docs.file_of(ord)
    }
}

impl FileSource for ServerFiles {
    fn paths(&self, ord: u32) -> Vec<Vec<u8>> {
        match self.handle.block_on(
            self.searcher
                .scope_engine()
                .locate(self.root, ord, MAX_DOC_PATHS),
        ) {
            Ok(p) => p.into_iter().map(|p| p.as_bytes().to_vec()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn lang(&self, ord: u32) -> Option<Lang> {
        if let Some(l) = self.doc(ord).and_then(|d| d.langs.first().copied()) {
            return Some(l);
        }
        let paths = self.paths(ord);
        let first = paths.first()?;
        lang::detect(&String::from_utf8_lossy(first), b"")
    }

    fn bytes(&self, ord: u32) -> Result<Option<Arc<[u8]>>> {
        let Some(file) = self.file(ord) else {
            return Ok(None);
        };
        Ok(self
            .handle
            .block_on(file_bytes(&*self.objects, &file))?
            .map(|b| Arc::from(&b[..])))
    }

    fn read_path(&self, path: &[u8]) -> Result<Option<Arc<[u8]>>> {
        let Ok(p) = RepoPath::new(path.to_vec()) else {
            return Ok(None);
        };
        let entry = self
            .handle
            .block_on(self.engine.lookup(self.root, &p))
            .map_err(src)?;
        let Some(e) = entry else { return Ok(None) };
        if !matches!(e.kind, asc_core::object::Kind::File | asc_core::object::Kind::Exec) {
            return Ok(None);
        }
        Ok(self
            .handle
            .block_on(file_bytes(&*self.objects, &FileId::from_bytes(e.id)))?
            .map(|b| Arc::from(&b[..])))
    }

    fn untagged(&self, scope: &RoaringBitmap, limit: usize) -> Vec<u32> {
        let examine = limit.saturating_mul(8).max(limit);
        self.handle
            .block_on(self.docs.untagged(scope, limit, examine))
            .unwrap_or_default()
    }

    fn secret(&self, ord: u32) -> Option<bool> {
        if self.indexed.contains(ord)
            && let Some(s) = self.doc(ord).and_then(|d| d.secret())
        {
            return Some(s);
        }
        // Not analysed yet: decide from the bytes. Unreadable means unknown, hence flagged.
        let file = self.file(ord)?;
        let bytes = self
            .handle
            .block_on(file_bytes(&*self.objects, &file))
            .ok()??;
        Some(secret_of(&bytes))
    }

    fn tags(&self, ord: u32) -> Result<Option<Arc<Vec<Tag>>>> {
        let Some(file) = self.file(ord) else {
            return Ok(None);
        };
        let ns = self.docs.ns().to_vec();
        if let Ok(Some(t)) = self
            .handle
            .block_on(read_tags(&*self.objects, &ns, &file))
        {
            return Ok(Some(Arc::new(t)));
        }
        let Some(lang) = self.lang(ord) else {
            return Ok(None);
        };
        let Some(b) = self.bytes(ord)? else {
            return Ok(None);
        };
        Ok(tags::extract(lang, &b).ok().map(Arc::new))
    }
}
