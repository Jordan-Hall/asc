//! The `Objects` trait and consumer-side verification (SPEC §3.2).

use std::ops::Range;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::{ChangesetId, ChunkId, FileId, TagId, TreeId};
use futures::stream::BoxStream;

use crate::codec::CodecError;
use crate::key::{KeyPrefix, ObjKey};

/// Storage errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Stored bytes do not match the key; the caller treats the object as missing.
    #[error("corrupt object {0}")]
    Corrupt(ObjKey),
    /// The stored value has an unusable codec byte or frame.
    #[error("bad stored value for {0}: {1}")]
    Codec(ObjKey, CodecError),
    /// `get_range` on a key that is not a file or chunk.
    #[error("range reads are not supported for {0}")]
    RangeUnsupported(ObjKey),
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    #[error("invalid store url: {0}")]
    Url(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Content-addressed object storage. Values are stored as `codec ‖ data`.
#[async_trait]
pub trait Objects: Send + Sync {
    /// Decoded data, verified against the key (`File`, `Chunk`, `Tree`, `Changeset`, `Tag`).
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>>;
    /// Stored bytes (`codec ‖ data`), neither decoded nor verified.
    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>>;
    /// A verified slice of decoded data, clamped to its length. `File` and `Chunk` only.
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>>;
    /// Writes stored bytes (`codec ‖ data`) if absent; an existing object is `Ok`.
    async fn put(&self, key: &ObjKey, stored: Bytes) -> Result<()>;
    /// Presence of each key, in order.
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>>;
    /// Removes an object; absent is `Ok`. GC only.
    async fn delete(&self, key: &ObjKey) -> Result<()>;
    /// Lists keys under a prefix with their modification time. GC only.
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>>;
}

/// Checks decoded `data` against `key` for verifiable kinds; other kinds pass.
///
/// `ChunkList` is not identity-bearing (it is checked when the file is assembled).
pub fn verify(key: &ObjKey, data: &[u8]) -> Result<()> {
    let ok = match key {
        ObjKey::File(id) => FileId::of(data) == *id,
        ObjKey::Chunk(id) => ChunkId::of(data) == *id,
        ObjKey::Tree(id) => TreeId::of(data) == *id,
        ObjKey::Changeset(id) => ChangesetId::of(data) == *id,
        ObjKey::Tag(id) => TagId::of(data) == *id,
        ObjKey::ChunkList(_)
        | ObjKey::GitTreeOverride(_)
        | ObjKey::Derived { .. }
        | ObjKey::Sha256Alias(_) => true,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Corrupt(*key))
    }
}

/// Decodes stored bytes and verifies the result.
pub fn decode_verified(key: &ObjKey, stored: &Bytes) -> Result<Bytes> {
    let data = crate::codec::decode_stored(stored).map_err(|e| Error::Codec(*key, e))?;
    verify(key, &data)?;
    Ok(data)
}

/// Clamps a requested range to `len` and slices.
pub fn slice_range(data: &Bytes, r: Range<u64>) -> Bytes {
    let len = data.len() as u64;
    let start = r.start.min(len) as usize;
    let end = r.end.clamp(r.start.min(len), len) as usize;
    data.slice(start..end)
}

#[cfg(test)]
mod tests {
    use asc_core::id::GitOid;

    use super::*;

    #[test]
    fn verify_kinds() {
        let data = b"hello";
        assert!(verify(&ObjKey::File(FileId::of(data)), data).is_ok());
        assert!(verify(&ObjKey::Chunk(ChunkId::of(data)), data).is_ok());
        assert!(verify(&ObjKey::Tree(TreeId::of(data)), data).is_ok());
        assert!(verify(&ObjKey::Changeset(ChangesetId::of(data)), data).is_ok());
        assert!(verify(&ObjKey::Tag(TagId::of(data)), data).is_ok());
        let bad = ObjKey::File(FileId::of(b"other"));
        assert!(matches!(verify(&bad, data), Err(Error::Corrupt(k)) if k == bad));
        // File and chunk ids use different contexts.
        assert!(
            verify(
                &ObjKey::Chunk(ChunkId::from_bytes(*FileId::of(data).as_bytes())),
                data
            )
            .is_err()
        );
        let unchecked = [
            ObjKey::ChunkList(FileId::of(b"x")),
            ObjKey::GitTreeOverride(GitOid::from_bytes([1; 20])),
            ObjKey::Sha256Alias([2; 32]),
        ];
        for k in unchecked {
            assert!(verify(&k, data).is_ok());
        }
    }

    #[test]
    fn range_clamps() {
        let d = Bytes::from_static(b"0123456789");
        assert_eq!(&slice_range(&d, 2..5)[..], b"234");
        assert_eq!(&slice_range(&d, 8..99)[..], b"89");
        assert!(slice_range(&d, 20..30).is_empty());
        let reversed = Range { start: 5, end: 2 };
        assert!(slice_range(&d, reversed).is_empty());
    }
}
