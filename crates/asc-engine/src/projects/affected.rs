//! `Engine::affected` (SPEC §12A.7): merge base, leaf diff, two graphs, then the pure algebra.

use asc_core::object::Change;
use asc_graph::affected::{self, ChangeKind, ChangedPath, Query, Side};
use asc_graph::select::Selector;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::hash::fingerprint;
use super::{AffectedQuery, AffectedResult, Result};
use crate::Engine;
use crate::history::HistoryApi;

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The projects affected between `q.base` and `q.head`.
    ///
    /// Only the changed paths, the manifest sets and the manifests that changed are read.
    pub async fn affected(&self, q: &AffectedQuery) -> Result<AffectedResult> {
        let selectors = q
            .select
            .iter()
            .map(|s| Selector::parse(s))
            .collect::<Result<Vec<_>, _>>()?;
        let merge_base = if q.no_merge_base {
            q.base
        } else {
            HistoryApi::merge_base(self, q.head, &[q.base], false)
                .await?
                .first()
                .copied()
                .ok_or(super::Error::NoMergeBase {
                    base: q.base,
                    head: q.head,
                })?
        };
        let (m, h) = (
            self.get_changeset(merge_base).await?,
            self.get_changeset(q.head).await?,
        );
        let changed: Vec<ChangedPath> = self
            .leafdiff(m.root, h.root, None)
            .await?
            .into_iter()
            .map(|(path, c)| ChangedPath {
                path,
                kind: match c {
                    Change::Write { .. } => ChangeKind::Write,
                    Change::Delete => ChangeKind::Delete,
                    Change::Gitlink { .. } => ChangeKind::Gitlink,
                },
            })
            .collect();
        let (gb, gh) = (self.loaded(merge_base).await?, self.loaded(q.head).await?);
        let query = Query {
            target: q.target.as_deref(),
            ecosystems: &q.ecosystems,
            selectors: &selectors,
        };
        let mut a = affected::affected(
            self.projects.detectors,
            Side {
                graph: &gb.graph,
                set: &gb.set,
            },
            Side {
                graph: &gh.graph,
                set: &gh.set,
            },
            &changed,
            &query,
        )?;
        if q.with_hashes {
            let hashes = self
                .input_hashes(q.head, q.target.as_deref().unwrap_or(""))
                .await?;
            for p in &mut a.projects {
                p.input_hash = hashes
                    .0
                    .binary_search_by(|(k, _)| k.cmp(&p.project.key))
                    .ok()
                    .map(|i| fingerprint(hashes.0[i].1));
            }
        }
        Ok(AffectedResult {
            base: q.base,
            head: q.head,
            merge_base,
            all: a.all,
            projects: a.projects,
            removed: a.removed,
            unowned: a.unowned,
            issues: a.issues,
            changed_paths: changed.len(),
        })
    }
}
