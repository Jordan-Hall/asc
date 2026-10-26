//! File content ingest and chunking (SPEC §2.4).
//!
//! Files up to [`CHUNK_THRESHOLD`] are one inline object; larger files are cut
//! with FastCDC v2020 into a [`ChunkList`] plus chunks. `FileId` always covers
//! the whole content, so chunking never affects identity.

use std::io::{Read, Seek, SeekFrom};

use bytes::Bytes;
use fastcdc::v2020::{Error as CdcError, Normalization, StreamCDC};
use sha1::{Digest, Sha1};

use crate::id::{ChunkId, FileHasher, FileId, GitOid};
use crate::object::ChunkList;

/// Largest file stored inline: 4 MiB exactly is inline.
pub const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
const CHUNK_MIN: usize = 512 * 1024;
const CHUNK_AVG: usize = 1024 * 1024;
const CHUNK_MAX: usize = 4 * 1024 * 1024;

/// Errors from ingest and chunk assembly.
#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    /// Reading the source failed.
    #[error("read failed: {0}")]
    Io(#[from] std::io::Error),
    /// The chunker failed.
    #[error("chunking failed: {0}")]
    Chunker(String),
    /// The source ended before its measured size.
    #[error("source shorter than its measured size")]
    ShortRead,
    /// A chunk does not match its listed id or length.
    #[error("chunk {index} is corrupt")]
    CorruptChunk { index: usize },
    /// More chunks were pushed than the list holds.
    #[error("unexpected extra chunk")]
    ExtraChunk,
    /// The list was not fully consumed.
    #[error("chunk list incomplete: {got} of {want} chunks")]
    MissingChunks { got: usize, want: usize },
    /// The list lengths do not sum to its size.
    #[error("chunk list sizes do not sum to its size")]
    BadList,
    /// The assembled bytes do not hash to the expected `FileId`.
    #[error("assembled file does not match its id")]
    FileMismatch,
}

/// How a file's bytes are stored.
#[derive(Clone, Debug)]
pub enum Body {
    /// The whole content, at most [`CHUNK_THRESHOLD`] bytes.
    Inline(Bytes),
    /// A chunk list plus its chunks in order.
    Chunked {
        list: ChunkList,
        chunks: Vec<(ChunkId, Bytes)>,
    },
}

/// Result of ingesting one file: identities plus the bytes to store.
#[derive(Clone, Debug)]
pub struct Ingested {
    pub file: FileId,
    pub size: u64,
    /// Git blob SHA-1 (`"blob <size>\0" + content`).
    pub git: GitOid,
    pub body: Body,
}

/// The whole-file digests, fed in one pass.
struct Digests {
    file: FileHasher,
    git: Sha1,
}

impl Digests {
    fn new(size: u64) -> Self {
        let mut git = Sha1::new();
        git.update(format!("blob {size}\0").as_bytes());
        Self {
            file: FileHasher::new(),
            git,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.file.update(bytes);
        self.git.update(bytes);
    }
}

/// Ingests a seekable source. The size is measured first because the git blob
/// header needs it; the content is then read once. Inline bodies are buffered
/// (at most 4 MiB); larger files are chunked as they stream.
pub fn ingest<R: Read + Seek>(mut reader: R) -> Result<Ingested, ContentError> {
    let start = reader.stream_position()?;
    let size = reader.seek(SeekFrom::End(0))? - start;
    reader.seek(SeekFrom::Start(start))?;
    let mut d = Digests::new(size);
    let body = if size <= CHUNK_THRESHOLD {
        let mut buf = Vec::with_capacity(size as usize);
        reader.take(size).read_to_end(&mut buf)?;
        if buf.len() as u64 != size {
            return Err(ContentError::ShortRead);
        }
        d.update(&buf);
        Body::Inline(Bytes::from(buf))
    } else {
        chunk_stream(reader.take(size), size, &mut d)?
    };
    Ok(Ingested {
        file: d.file.finalize(),
        size,
        git: GitOid::from_bytes(d.git.finalize().into()),
        body,
    })
}

fn chunk_stream(r: impl Read, size: u64, d: &mut Digests) -> Result<Body, ContentError> {
    let cdc = StreamCDC::with_level(r, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX, Normalization::Level1);
    let (mut entries, mut chunks, mut seen) = (Vec::new(), Vec::new(), 0u64);
    for c in cdc {
        let c = c.map_err(|e| match e {
            CdcError::IoError(e) => ContentError::Io(e),
            e => ContentError::Chunker(e.to_string()),
        })?;
        d.update(&c.data);
        seen += c.data.len() as u64;
        let id = ChunkId::of(&c.data);
        entries.push((id, c.data.len() as u32));
        chunks.push((id, Bytes::from(c.data)));
    }
    if seen != size {
        return Err(ContentError::ShortRead);
    }
    Ok(Body::Chunked {
        list: ChunkList {
            size,
            chunks: entries,
        },
        chunks,
    })
}

/// Verifies a chunked file while it is read back: each chunk against its
/// listed id and length, then the whole against the `FileId`. Constant memory.
pub struct ChunkAssembler {
    list: ChunkList,
    want: FileId,
    next: usize,
    hasher: FileHasher,
}

impl ChunkAssembler {
    /// Starts assembly of `list`, expecting the result to hash to `want`.
    pub fn new(list: ChunkList, want: FileId) -> Result<Self, ContentError> {
        let sum: u64 = list.chunks.iter().map(|(_, n)| u64::from(*n)).sum();
        if sum != list.size {
            return Err(ContentError::BadList);
        }
        Ok(Self {
            list,
            want,
            next: 0,
            hasher: FileHasher::new(),
        })
    }

    /// Verifies and absorbs the next chunk, in list order.
    pub fn push(&mut self, data: &[u8]) -> Result<(), ContentError> {
        let index = self.next;
        let (id, len) = *self
            .list
            .chunks
            .get(index)
            .ok_or(ContentError::ExtraChunk)?;
        if data.len() != len as usize || ChunkId::of(data) != id {
            return Err(ContentError::CorruptChunk { index });
        }
        self.hasher.update(data);
        self.next += 1;
        Ok(())
    }

    /// Checks that every chunk arrived and the whole matches the `FileId`.
    pub fn finish(self) -> Result<FileId, ContentError> {
        let want = self.list.chunks.len();
        if self.next != want {
            return Err(ContentError::MissingChunks {
                got: self.next,
                want,
            });
        }
        let got = self.hasher.finalize();
        if got != self.want {
            return Err(ContentError::FileMismatch);
        }
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Deterministic xorshift64 bytes.
    fn pseudo(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.truncate(len);
        v
    }

    #[test]
    fn threshold_edges() {
        let at = ingest(Cursor::new(pseudo(CHUNK_THRESHOLD as usize))).unwrap();
        assert!(matches!(at.body, Body::Inline(_)));
        let over = ingest(Cursor::new(pseudo(CHUNK_THRESHOLD as usize + 1))).unwrap();
        assert!(matches!(over.body, Body::Chunked { .. }));
        let empty = ingest(Cursor::new(Vec::new())).unwrap();
        assert!(matches!(&empty.body, Body::Inline(b) if b.is_empty()));
        assert_eq!(
            empty.git.to_string(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn inline_digests() {
        let i = ingest(Cursor::new(b"hello\n".to_vec())).unwrap();
        assert_eq!(
            i.git.to_string(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_eq!(i.file, FileId::of(b"hello\n"));
        assert_eq!(i.size, 6);
    }

    fn chunked(data: &[u8]) -> (Ingested, ChunkList, Vec<(ChunkId, Bytes)>) {
        let i = ingest(Cursor::new(data.to_vec())).unwrap();
        let Body::Chunked { list, chunks } = i.body.clone() else {
            panic!("not chunked")
        };
        (i, list, chunks)
    }

    #[test]
    fn chunked_roundtrip_and_bounds() {
        let data = pseudo(20 * 1024 * 1024);
        let (i, list, chunks) = chunked(&data);
        assert_eq!(i.file, FileId::of(&data));
        assert_eq!(list.size, data.len() as u64);
        let last = list.chunks.len() - 1;
        for (n, (_, len)) in list.chunks.iter().enumerate() {
            let len = *len as usize;
            assert!(len <= CHUNK_MAX && (n == last || len >= CHUNK_MIN));
        }
        let mut a = ChunkAssembler::new(list, i.file).unwrap();
        for (_, b) in &chunks {
            a.push(b).unwrap();
        }
        assert_eq!(a.finish().unwrap(), i.file);
    }

    #[test]
    fn chunk_boundaries_are_stable() {
        let (_, list, _) = chunked(&pseudo(20 * 1024 * 1024));
        let mut h = blake3::Hasher::new();
        for (id, len) in &list.chunks {
            h.update(id.as_bytes()).update(&len.to_le_bytes());
        }
        assert_eq!((list.chunks.len(), h.finalize().to_hex().as_str()), GOLDEN);
    }

    #[test]
    fn corrupt_chunk_is_detected() {
        let (i, list, chunks) = chunked(&pseudo(6 * 1024 * 1024));
        let mut a = ChunkAssembler::new(list.clone(), i.file).unwrap();
        let mut bad = chunks[0].1.to_vec();
        bad[0] ^= 1;
        assert!(matches!(
            a.push(&bad),
            Err(ContentError::CorruptChunk { index: 0 })
        ));

        let mut a = ChunkAssembler::new(list.clone(), i.file).unwrap();
        a.push(&chunks[0].1).unwrap();
        assert!(matches!(
            a.finish(),
            Err(ContentError::MissingChunks { .. })
        ));

        let mut a = ChunkAssembler::new(list, FileId::of(b"other")).unwrap();
        for (_, b) in &chunks {
            a.push(b).unwrap();
        }
        assert!(matches!(a.finish(), Err(ContentError::FileMismatch)));
    }

    const GOLDEN: (usize, &str) = (
        17,
        "53d8c820f6e680332197b907646a53c5713ede17674728327279005c9f1f9b2d",
    );
}
