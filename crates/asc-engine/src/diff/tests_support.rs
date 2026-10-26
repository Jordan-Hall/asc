//! Shared test fixtures: an in-memory content source.

use std::collections::HashMap;

use asc_core::id::FileId;
use asc_core::object::Entry;
use async_trait::async_trait;
use bytes::Bytes;

use super::{ContentSource, Error, Result};

/// File bytes by `FileId`, for diffs that need no object store.
#[derive(Default)]
pub struct MemSource(HashMap<[u8; 32], Bytes>);

impl MemSource {
    pub fn add(&mut self, content: &[u8]) {
        self.0.insert(
            *FileId::of(content).as_bytes(),
            Bytes::copy_from_slice(content),
        );
    }
}

#[async_trait]
impl ContentSource for MemSource {
    async fn read(&self, entry: &Entry) -> Result<Bytes> {
        self.0
            .get(&entry.id)
            .cloned()
            .ok_or(Error::MissingFile(FileId::from_bytes(entry.id)))
    }
}
