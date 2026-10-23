//! The local degraded code intelligence index (SPEC §23.2.5, unit I19).
//!
//! A repository that is local-only, or whose server is unreachable, has no server index. The
//! helper `asc codeintel worker` (a separate process, never the daemon) builds a degraded one
//! from the `HEAD` of a workspace with the same `asc-codeintel` code the server uses, under
//! hard limits:
//!
//! * resident memory at most [`MAX_RSS`] (the tantivy writer heap is [`WRITER_HEAP`]; files
//!   are read and analysed in batches of at most [`BATCH_BYTES`]),
//! * [`THREADS`] analysis threads at low priority,
//! * no network: file bytes come from `store/` and `cache/` only, never a remote tier,
//! * disk only inside `codeintel.local.maxSize` ([`DEFAULT_MAX_SIZE`]), least recently used
//!   revision first,
//! * started only when the workspace is idle ([`IDLE_AFTER`]) and gone [`EXIT_AFTER`] after its
//!   last job.
//!
//! The index lives in `$ASC_HOME/ci/<repo-id>/` ([`store`]). The daemon and `asc grep` only read
//! it ([`query`]); `coverage` says how much of a revision is indexed. Every byte read back from
//! disk is untrusted: sizes are capped, ids and paths are re-validated, segment packs are
//! hash-checked by `asc-codeintel`, and file contents are checked against their `FileId`.

use std::time::Duration;

pub mod build;
pub mod cli;
pub mod query;
pub mod routed;
pub mod start;
pub mod store;
pub mod worker;

/// Resident memory the helper stays under.
pub const MAX_RSS: u64 = 256 << 20;
/// Memory the tantivy writer may use.
pub const WRITER_HEAP: usize = 64 << 20;
/// Analysis threads.
pub const THREADS: usize = 2;
/// Most file bytes one build batch holds.
pub const BATCH_BYTES: u64 = 24 << 20;
/// Most files one build batch (hence one segment) holds.
pub const BATCH_FILES: usize = 2_048;
/// Default for `codeintel.local.maxSize`.
pub const DEFAULT_MAX_SIZE: u64 = 512 << 20;
/// The helper exits this long after its last job.
pub const EXIT_AFTER: Duration = Duration::from_secs(60);
/// A workspace whose `HEAD` and index files were last written longer ago than this is idle.
pub const IDLE_AFTER: Duration = Duration::from_secs(5);
/// A reader the daemon opened is closed after this long without a query.
pub const READER_IDLE: Duration = Duration::from_secs(300);
/// The helper polls `HEAD` this often while it waits for a job.
pub const POLL: Duration = Duration::from_millis(500);

/// Failure of a local index operation.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("io: {0}")]
    Io(String),
    #[error("corrupt local index: {0}")]
    Corrupt(&'static str),
    #[error("{0}")]
    Index(String),
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for LocalError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<asc_codeintel::index::IndexError> for LocalError {
    fn from(e: asc_codeintel::index::IndexError) -> Self {
        Self::Index(e.to_string())
    }
}

pub type Result<T, E = LocalError> = std::result::Result<T, E>;

/// Lower-case hex of `b`.
pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// The 32 bytes of 64 hex digits, or `None`.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        let b = [7u8; 32];
        assert_eq!(unhex32(&hex(&b)), Some(b));
        assert_eq!(unhex32("zz"), None);
        assert_eq!(
            unhex32(&"A".repeat(64)),
            None,
            "upper case is not canonical"
        );
        assert_eq!(unhex32(&"a".repeat(63)), None);
    }

    #[test]
    fn limits_match_the_spec() {
        assert_eq!(MAX_RSS, 256 << 20);
        assert_eq!(WRITER_HEAP, 64 << 20);
        assert_eq!(THREADS, 2);
        assert_eq!(DEFAULT_MAX_SIZE, 512 << 20);
        assert_eq!(EXIT_AFTER, Duration::from_secs(60));
        assert_eq!(READER_IDLE, Duration::from_secs(300));
        assert!(BATCH_BYTES + (WRITER_HEAP as u64) * 2 < MAX_RSS);
    }
}
