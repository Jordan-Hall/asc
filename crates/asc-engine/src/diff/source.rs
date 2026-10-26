//! Where diff content and attributes come from.

use asc_core::codec::decode_envelope;
use asc_core::content::ChunkAssembler;
use asc_core::id::FileId;
use asc_core::object::{ChunkList, Entry};
use asc_core::path::RepoPath;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};

use super::{Error, Result};
use crate::Engine;

/// Fetches file bytes lazily; the diff reads a side only when it must.
#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Bytes of a file, executable or symlink entry.
    async fn read(&self, entry: &Entry) -> Result<Bytes>;
}

#[async_trait]
impl<O: Objects, R: Refs> ContentSource for Engine<O, R> {
    async fn read(&self, entry: &Entry) -> Result<Bytes> {
        self.read_file(FileId::from_bytes(entry.id), entry.size)
            .await
    }
}

/// Largest chunked file `read_file` assembles in memory. A tree entry's size is remote input;
/// diff, blame, merge and derive all need the whole content at once, so a larger file is
/// refused with [`Error::FileTooLarge`] instead of read.
pub const MAX_READ_FILE_BYTES: u64 = 1 << 30;

/// Buffer reserved up front for a chunked read; the rest grows as verified chunks arrive.
const READ_FILE_PREALLOC: u64 = 64 << 20;

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Reads a whole file: one object when inline, else its chunks in order.
    pub async fn read_file(&self, file: FileId, size: u64) -> Result<Bytes> {
        if size <= asc_core::content::CHUNK_THRESHOLD {
            return self
                .objects
                .get(&ObjKey::File(file))
                .await?
                .ok_or(Error::MissingFile(file));
        }
        // The whole file is held in memory: refuse a claimed size past the bound before anything
        // is fetched, and reserve only a bounded start so a forged size costs nothing.
        if size > MAX_READ_FILE_BYTES {
            return Err(Error::FileTooLarge(file, size));
        }
        let list = self
            .objects
            .get(&ObjKey::ChunkList(file))
            .await?
            .ok_or(Error::MissingFile(file))?;
        let list = decode_envelope::<ChunkList>(&list)?;
        // Chunk lists are not verified when fetched: bind the list to `file` and `size`.
        if list.size != size {
            return Err(Error::MissingFile(file));
        }
        let chunks = list.chunks.clone();
        let mut assembler =
            ChunkAssembler::new(list, file).map_err(|_| Error::MissingFile(file))?;
        let mut out = BytesMut::with_capacity(size.min(READ_FILE_PREALLOC) as usize);
        for (id, len) in &chunks {
            let chunk = self
                .objects
                .get(&ObjKey::Chunk(*id))
                .await?
                .ok_or(Error::MissingFile(file))?;
            if chunk.len() != *len as usize {
                return Err(Error::MissingFile(file));
            }
            assembler
                .push(&chunk)
                .map_err(|_| Error::MissingFile(file))?;
            out.extend_from_slice(&chunk);
        }
        assembler.finish().map_err(|_| Error::MissingFile(file))?;
        Ok(out.freeze())
    }
}

/// The `diff` attribute of a path (SPEC §2.4.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DiffAttr {
    /// Not mentioned: binary is decided by content.
    #[default]
    Unspecified,
    /// `diff`: always text.
    Text,
    /// `-diff` (also `binary`): always binary.
    Binary,
    /// `diff=<driver>`: content decides, unless the driver converts it to text.
    Driver(String),
}

/// Attribute lookups for diff. The real implementation reads `.ascattributes` and
/// `.gitattributes`; the diff only needs the answers.
#[async_trait]
pub trait Attributes: Send + Sync {
    /// The `diff` attribute of `path`.
    fn diff_attr(&self, path: &RepoPath) -> DiffAttr;

    /// Runs the `textconv` of `driver` over `content`; `None` when the driver has none.
    async fn textconv(
        &self,
        driver: &str,
        path: &RepoPath,
        content: &Bytes,
    ) -> Result<Option<Bytes>>;
}

/// No attributes: binary detection by content only.
pub struct NoAttributes;

#[async_trait]
impl Attributes for NoAttributes {
    fn diff_attr(&self, _: &RepoPath) -> DiffAttr {
        DiffAttr::Unspecified
    }

    async fn textconv(&self, _: &str, _: &RepoPath, _: &Bytes) -> Result<Option<Bytes>> {
        Ok(None)
    }
}
