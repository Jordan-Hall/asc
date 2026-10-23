//! Tantivy indexes of code intelligence (SPEC §23.2.4): the `files` and `chunks` schemas,
//! immutable segment packs in the bucket (`d/ciseg.v2/<scoped id>`), and the `ci_manifest`
//! generation CAS with its leader lease and `indexed` bitmaps (`d/ciidx.v2/<scoped id>`).
//! A scoped id hashes `(ns, kind, class, content hash)`, so no two indexes share an object.

pub mod manifest;
pub mod schema;
pub mod segments;

/// Failure of an index operation.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("tantivy: {0}")]
    Tantivy(String),
    #[error("io: {0}")]
    Io(String),
    #[error("store: {0}")]
    Store(String),
    #[error("invalid segment pack: {0}")]
    BadPack(&'static str),
    #[error("segment {0} failed its hash check")]
    HashMismatch(String),
    #[error("segment {0} is missing from the bucket")]
    Missing(String),
    #[error("limit exceeded: {0}")]
    Limit(&'static str),
    #[error("ord {0} was added twice to one segment")]
    DuplicateOrd(u32),
    #[error("not the leader of the namespace")]
    NotLeader,
    #[error("manifest is contended; gave up after {0} re-bases")]
    Contended(u32),
    #[error("path {0} is a symlink or not a directory")]
    Symlink(String),
}

impl From<tantivy::TantivyError> for IndexError {
    fn from(e: tantivy::TantivyError) -> Self {
        IndexError::Tantivy(e.to_string())
    }
}

impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        IndexError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;

#[cfg(test)]
mod tests;
