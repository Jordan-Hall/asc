//! The production [`ChangeSource`] of the `codeintel.enqueue` outbox effect (SPEC §23.5, PLAN I13).
//!
//! After an update commits, the effect asks which files the update made new. [`RegistryChanges`]
//! answers from the repository's own engine: it walks the changesets the new ref targets reach,
//! stopping at the targets the refs left and at the tips of the repository's other refs, and
//! returns the `Write` `FileId`s of their `changes`. It reads changesets only, never trees, so the
//! cost is the size of the update, not of the repository.
//!
//! * **Bounded.** One delivery looks at [`MAX_CHANGESETS`] changesets and returns at most
//!   [`MAX_ENQUEUE`] files. A first push of a long history is cut there (and logged); older
//!   revisions are indexed on demand (SPEC §23.5).
//! * **Idempotent.** The same update gives the same files; the queue ignores a job it has.
//! * **Gone is done.** A repository that no longer exists, or is not a ready repository, has
//!   nothing to index: the effect succeeds with no files rather than blocking the repository's
//!   outbox rows forever.
//! * **Tags and gaps.** A target that is not a changeset (a tag) is skipped. An ancestor the store
//!   lacks is skipped; a new target it lacks is an error, retried with the outbox row.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::Change;
use asc_rpc::server::RepoName;
use async_trait::async_trait;
use sqlx::Row;

use super::backfill::{SqlTips, TipSource};
use super::queue::{ChangeSource, EnqueueEffect, MAX_ENQUEUE, Written};
use crate::outbox::{Effect, EffectError};
use crate::registry::{Registry, RegistryError, RepoEntry};

/// Changesets one delivery examines at most.
pub const MAX_CHANGESETS: usize = 4096;

/// [`ChangeSource`] over the repository registry.
pub struct RegistryChanges {
    registry: Arc<Registry>,
}

impl RegistryChanges {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// The open repository `repo_id`, or `None` when it is gone or still being copied.
    pub(crate) async fn entry(&self, repo_id: i64) -> Result<Option<Arc<RepoEntry>>, EffectError> {
        let row = sqlx::query("SELECT name FROM repos WHERE id = $1")
            .bind(repo_id)
            .fetch_optional(self.registry.pool())
            .await
            .map_err(|e| EffectError(format!("repository lookup: {e}")))?;
        let Some(row) = row else { return Ok(None) };
        let name: String = row
            .try_get("name")
            .map_err(|e| EffectError(format!("repository lookup: {e}")))?;
        let name = RepoName::parse(&name)
            .map_err(|s| EffectError(format!("repository name: {}", s.message())))?;
        match self.registry.open(&name).await {
            Ok(entry) if entry.key.0 == repo_id => Ok(Some(entry)),
            // Renamed since the row was read; the next delivery sees the new name.
            Ok(_) => Err(EffectError("repository changed while it was read".into())),
            Err(RegistryError::NotFound(_)) => Ok(None),
            Err(e) => Err(EffectError(format!("repository open: {e}"))),
        }
    }
}

/// The changesets among `targets` (33-byte ref targets); tags and malformed entries are skipped.
fn changesets(targets: &[Vec<u8>]) -> Vec<ChangesetId> {
    targets
        .iter()
        .filter_map(|t| match RefTarget::from_bytes(t) {
            Ok(RefTarget::Changeset(c)) => Some(c),
            _ => None,
        })
        .collect()
}

#[async_trait]
impl ChangeSource for RegistryChanges {
    async fn written(&self, repo_id: i64, targets: &[Vec<u8>]) -> Result<Written, EffectError> {
        self.written_since(repo_id, targets, &[]).await
    }

    async fn written_since(
        &self,
        repo_id: i64,
        targets: &[Vec<u8>],
        old: &[Vec<u8>],
    ) -> Result<Written, EffectError> {
        let Some(entry) = self.entry(repo_id).await? else {
            return Ok(Written::default());
        };
        let new = changesets(targets);
        // What was already reachable: where the refs were, and where the other refs are.
        let mut stop: HashSet<ChangesetId> = changesets(old).into_iter().collect();
        let tips = SqlTips::new(self.registry.pool().clone())
            .tips(repo_id)
            .await
            .map_err(|e| EffectError(format!("ref list: {e}")))?;
        stop.extend(tips.iter().map(|t| t.rev));
        for n in &new {
            stop.remove(n);
        }

        let engine = entry.backend.engine();
        let mut todo: VecDeque<ChangesetId> = new.iter().copied().collect();
        let mut seen: HashSet<ChangesetId> = HashSet::new();
        let mut files: Vec<Vec<u8>> = Vec::new();
        let mut file_seen: HashSet<FileId> = HashSet::new();
        let mut walked = 0usize;
        'walk: while let Some(id) = todo.pop_front() {
            if stop.contains(&id) || !seen.insert(id) {
                continue;
            }
            if walked >= MAX_CHANGESETS {
                tracing::warn!(
                    repo_id,
                    walked,
                    "codeintel enqueue stopped at the changeset bound"
                );
                break;
            }
            walked += 1;
            let cs = match engine.get_changeset(id).await {
                Ok(cs) => cs,
                // History this repository does not hold is not new work; a new target the store
                // lacks is an error, retried with the outbox row.
                Err(asc_engine::commit::Error::MissingChangeset(_)) if !new.contains(&id) => {
                    continue;
                }
                Err(e) => return Err(EffectError(format!("changeset {id}: {e}"))),
            };
            for (_, change) in cs.changes.as_slice() {
                if let Change::Write { file, .. } = change
                    && file_seen.insert(*file)
                {
                    if files.len() >= MAX_ENQUEUE {
                        tracing::warn!(repo_id, "codeintel enqueue stopped at the file bound");
                        break 'walk;
                    }
                    files.push(file.as_bytes().to_vec());
                }
            }
            todo.extend(cs.parents.iter().copied());
        }
        Ok(Written {
            ns: entry.ns.as_bytes().to_vec(),
            files,
        })
    }
}

/// The `codeintel.enqueue` effect over `registry`, which `serve()` registers in the outbox when
/// `[codeintel] enabled`.
pub fn enqueue_effect(registry: &Arc<Registry>) -> Arc<dyn Effect> {
    Arc::new(EnqueueEffect::new(
        registry.pool().clone(),
        Arc::new(RegistryChanges::new(Arc::clone(registry))),
    ))
}
