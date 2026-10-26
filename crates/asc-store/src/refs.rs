//! The `Refs` trait and its transaction types (SPEC 3.2, 3.4).

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use async_trait::async_trait;
use uuid::Uuid;

/// Numeric id of a repository in the ref store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoKey(pub i64);

/// Who made a change, and through which surface (`cli`, `serve`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    pub id: String,
    pub via: String,
}

/// What a ref must hold for an update to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Any,
    Absent,
    Value(RefTarget),
}

/// One ref change. `new: None` deletes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: RefName,
    pub expect: Expect,
    pub new: Option<RefTarget>,
    /// Unix seconds; the ref is reaped after this (SPEC 8.8). `None` clears any expiry.
    pub expires_at: Option<i64>,
}

/// An atomic batch of ref changes with its audit context.
#[derive(Clone, Debug)]
pub struct RefTxn {
    pub updates: Vec<RefUpdate>,
    pub actor: Actor,
    pub reason: String,
    pub op_id: Uuid,
}

/// A stored ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefEntry {
    pub name: RefName,
    pub target: RefTarget,
    pub version: u64,
    pub expires_at: Option<i64>,
}

/// One row of the ref log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefLogEntry {
    pub seq: i64,
    pub name: RefName,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
    pub actor: Actor,
    pub reason: String,
    pub op_id: Uuid,
    /// Unix seconds.
    pub at: i64,
}

/// An update whose expectation did not hold; the whole transaction was rolled back.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("ref {name} changed concurrently (now {actual:?})")]
pub struct RefConflict {
    pub name: RefName,
    /// The current value, `None` if absent.
    pub actual: Option<RefTarget>,
}

#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error(transparent)]
    Conflict(#[from] RefConflict),
    /// Names are stored as text; a non-UTF-8 ref name cannot be stored.
    #[error("ref name is not valid UTF-8: {0}")]
    NonUtf8Name(RefName),
    /// A stored row does not decode.
    #[error("corrupt ref row: {0}")]
    Corrupt(String),
    #[error("ref backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T, E = RefError> = std::result::Result<T, E>;

/// Compare-and-swap ref storage.
#[async_trait]
pub trait Refs: Send + Sync {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>>;
    /// Refs whose name starts with `prefix`, ordered by name.
    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>>;
    /// Applies every update or none; a failed expectation is `RefError::Conflict`.
    async fn update(&self, repo: RepoKey, txn: &RefTxn) -> Result<()>;
    /// Newest first.
    async fn log(&self, repo: RepoKey, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>>;
}
