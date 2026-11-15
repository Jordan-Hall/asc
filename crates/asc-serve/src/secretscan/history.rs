//! The background scan of a whole history.
//!
//! The gate scans the newest [`MAX_CHANGESETS`](super::MAX_CHANGESETS) changesets of a history
//! import and puts the gap on the record (`secret.scan.incomplete`). [`Scanner::queue_history_scan`]
//! then queues a scan of everything that ref reaches. The scan reads the history in batches of
//! [`BATCH`] changesets, each under the limits of `[secrets]`, keeps the findings the normal way
//! ([`store::upsert`]) and leaves a `secret.scan.run` event when it ends. One such scan runs at a
//! time, server wide; a scan of the same tip is queued once. The queue is the task list of the
//! process: a restart drops what was waiting, and the `secret.scan.incomplete` event is what says
//! that a gap exists.

use std::collections::HashSet;
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId};
use asc_core::policy::SecretsPolicy;
use asc_engine::graph::LogQuery;
use futures::TryStreamExt;
use tonic::Status;

use super::scan::{FileRef, Scan};
use super::{BATCH, Scanner, added_files, store};
use asc_rpc::server::RepoName;

use crate::audit::{Actor, NewEvent, Outcome};
use crate::review::{internal, now, registry_status, sql};

/// Distinct file contents remembered across batches, so identical content is read once.
const MAX_SEEN: usize = 2_000_000;

/// What a history scan did.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub changesets: usize,
    pub files: u64,
    pub findings: usize,
    /// A batch stopped at a limit before it read every file.
    pub truncated: bool,
}

impl Scanner {
    /// Queues a scan of every changeset `tip` reaches in `repo`, under `policy`. Returns `false`
    /// when a scan of the same tip is already queued or running.
    pub fn queue_history_scan(
        self: &Arc<Self>,
        repo: RepoName,
        repo_id: i64,
        tip: ChangesetId,
        ref_name: String,
        policy: SecretsPolicy,
    ) -> bool {
        let first = match self.queued.lock() {
            Ok(mut q) => q.insert((repo_id, tip)),
            Err(_) => return false,
        };
        if !first {
            return false;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let ran = match me.background.acquire().await {
                Ok(_permit) => me.scan_history(&repo, repo_id, tip, &policy).await,
                Err(e) => Err(internal("secret history scan", &e)),
            };
            if let Ok(mut q) = me.queued.lock() {
                q.remove(&(repo_id, tip));
            }
            let mut event = NewEvent::new("secret.scan.run", Actor::system("secret-scan"))
                .repo(repo.as_str())
                .target(&ref_name)
                .detail("ref", &ref_name);
            match &ran {
                Ok(s) => {
                    event = event
                        .detail("changesets", s.changesets.to_string())
                        .detail("files", s.files.to_string())
                        .detail("findings", s.findings.to_string())
                        .detail("truncated", s.truncated.to_string());
                }
                Err(e) => {
                    tracing::error!(repo = %repo, ref_name, error = %e, "secret history scan failed");
                    event = event.outcome(Outcome::Failed);
                }
            }
            crate::audit::store::record_best_effort(me.registry.pool(), &event).await;
        });
        true
    }

    /// Scans the whole history of `tip`, newest first, and records the findings.
    async fn scan_history(
        &self,
        repo: &RepoName,
        repo_id: i64,
        tip: ChangesetId,
        policy: &SecretsPolicy,
    ) -> Result<Summary, Status> {
        let entry = self.registry.open(repo).await.map_err(registry_status)?;
        let engine = entry.backend.engine();
        let rules = Self::rules(policy)?;
        let key = store::repo_key(self.registry.pool(), repo_id, now())
            .await
            .map_err(sql)?;
        let graph = crate::policy::graph_of(&self.registry, &entry);
        let mut log = std::pin::pin!(graph.log(LogQuery {
            include: vec![tip],
            ..LogQuery::default()
        }));
        let mut summary = Summary::default();
        let mut seen: HashSet<FileId> = HashSet::new();
        let mut batch: Vec<ChangesetId> = Vec::with_capacity(BATCH);
        loop {
            let next = log
                .try_next()
                .await
                .map_err(|e| internal("secret history scan: log", &e))?;
            let done = next.is_none();
            batch.extend(next);
            if batch.len() < BATCH && !done {
                continue;
            }
            summary.changesets += batch.len();
            // Content read in an earlier batch is not read again.
            let files: Vec<FileRef> = added_files(engine, &batch)
                .await?
                .files
                .into_iter()
                .filter(|f| {
                    if seen.contains(&f.file) {
                        return false;
                    }
                    if seen.len() < MAX_SEEN {
                        seen.insert(f.file);
                    }
                    true
                })
                .collect();
            batch.clear();
            let mut scan = Scan::new(engine, &rules, policy, &self.cfg, &key);
            scan.feed(files).await?;
            let report = scan.finish();
            summary.files += u64::from(report.files_scanned);
            summary.truncated |= report.truncated;
            summary.findings += report.found.len();
            let new: Vec<store::NewFinding> = report
                .found
                .iter()
                .map(|f| store::NewFinding {
                    rule_id: f.rule_id.clone(),
                    severity: f.severity,
                    path: f.path.clone(),
                    line: f.line,
                    column: f.column,
                    changeset: f.changeset,
                    fingerprint: f.fingerprint.clone(),
                    preview: f.preview.clone(),
                })
                .collect();
            if !new.is_empty() {
                store::upsert(self.registry.pool(), repo_id, &new, now())
                    .await
                    .map_err(sql)?;
            }
            if done {
                return Ok(summary);
            }
        }
    }
}
