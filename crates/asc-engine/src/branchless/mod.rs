//! Branchless drafts (SPEC §21): visibility, hide and unhide, and the commit-cloud engine.
//!
//! The engine never touches `state.db`. The local `draft_heads` rows are reached only through
//! [`DraftRows`], which the daemon implements over asc-state, so asc-engine takes no dependency on
//! asc-state.

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use async_trait::async_trait;

use crate::stack;

pub mod guard;
pub mod live;
pub mod sync;
pub mod upload;
pub mod visible;

/// Failures from branchless visibility.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Stack(#[from] stack::Error),
    #[error(transparent)]
    Graph(#[from] crate::graph::Error),
    /// The draft table failed.
    #[error("draft rows: {0}")]
    Rows(String),
    #[error("{0} is not a draft")]
    NotDraft(ChangesetId),
    #[error("{0} is not a visible draft")]
    NotVisible(ChangesetId),
    #[error("{0} exceeds the limit of {1}")]
    TooLarge(&'static str, usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// One `draft_heads` row (SPEC §21.2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftHead {
    pub cs: ChangesetId,
    pub change: ChangeId,
    pub added: i64,
    pub hidden_at: Option<i64>,
}

impl DraftHead {
    pub fn is_visible(&self) -> bool {
        self.hidden_at.is_none()
    }
}

/// Read access to the local draft heads. Maintenance (hide, unhide, upload queue) runs in the
/// caller's transaction and is not part of this trait.
#[async_trait]
pub trait DraftRows: Send + Sync {
    /// Visible heads (`hidden_at IS NULL`), oldest first, at most `n`.
    async fn visible_heads(&self, n: usize) -> Result<Vec<DraftHead>>;
    /// Hidden heads, most recently hidden first, at most `n`.
    async fn hidden_heads(&self, n: usize) -> Result<Vec<DraftHead>>;
}
