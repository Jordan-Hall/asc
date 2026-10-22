//! Errors for the embedding crate.

/// Embedding failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The model licence is not on the allow-list.
    #[error("licence {0:?} is not allowed (MIT, Apache-2.0 only)")]
    Licence(String),
    /// A model file does not match its pinned sha256.
    #[error("model file {name} sha256 mismatch: expected {expected}, got {actual}")]
    Digest {
        /// File role.
        name: &'static str,
        /// Pinned digest.
        expected: String,
        /// Computed digest.
        actual: String,
    },
    /// The model id is not in the registry.
    #[error("unknown model {0:?}")]
    UnknownModel(String),
    /// Invalid calibration input or table.
    #[error("calibration: {0}")]
    Calibration(String),
    /// A protocol frame is malformed or over its bound.
    #[error("protocol: {0}")]
    Protocol(String),
    /// The embedding backend failed.
    #[error("embedding backend: {0}")]
    Backend(String),
    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
