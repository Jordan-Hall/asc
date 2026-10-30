//! `Review.DraftContext`: what a draft of a new change is written from.
//!
//! The hub's "Draft from the diff" writes What, Why, How and Risk with the person's own AI key.
//! asc supplies the facts, from the changesets in `base..head`: their subjects, the files they
//! touch with line counts (summed per path), the size, and the risk with its reasons, the same
//! classification the review brief uses. The patch text is read with `Repo.Diff`; nothing here
//! is generated, so there is nothing to leak that the person could not already read.

use std::collections::BTreeMap;

use asc_core::id::ChangesetId;
use asc_engine::graph::LogQuery;
use asc_rpc::server::{CallCtx, wire};
use bytes::Bytes;
use futures::TryStreamExt as _;
use tonic::Status;

use super::{Reviews, brief};
use crate::idmode::IdCodec;

/// Changesets walked at most.
const MAX_CHANGESETS: usize = 500;
/// Subjects returned.
const MAX_MESSAGES: usize = 50;
/// Bytes of one subject.
const MAX_SUBJECT: usize = 200;
/// Files returned.
const MAX_FILES: usize = 200;
/// A revision spec at most this long.
const MAX_SPEC: usize = 1024;

impl Reviews {
    /// `Review.DraftContext`.
    pub async fn draft_context(
        &self,
        ctx: &CallCtx,
        req: wire::DraftContextRequest,
    ) -> Result<wire::DraftContextResponse, Status> {
        if req.base.len() > MAX_SPEC || req.head.len() > MAX_SPEC {
            return Err(Status::invalid_argument("a revision is too long"));
        }
        let entry = self.open(ctx).await?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let head_spec = if req.head.is_empty() {
            "HEAD"
        } else {
            req.head.as_str()
        };
        let base_spec = if req.base.is_empty() {
            entry.default_branch.as_str()
        } else {
            req.base.as_str()
        };
        let mut found = Vec::with_capacity(2);
        for spec in [head_spec, base_spec] {
            found.push(
                ids.resolve(spec, &entry.default_branch, |e| {
                    super::internal("history", &e)
                })
                .await?
                .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))?,
            );
        }
        let (head, base) = (found[0], found[1]);
        let engine = entry.backend.engine();
        let graph = crate::policy::graph_of(&self.registry, &entry);
        let list: Vec<ChangesetId> = graph
            .log(LogQuery {
                include: vec![head],
                exclude: vec![base],
                limit: Some(MAX_CHANGESETS + 1),
                ..LogQuery::default()
            })
            .try_collect()
            .await
            .map_err(|e| super::internal("history", &e))?;
        let mut out = wire::DraftContextResponse {
            nothing_to_compare: list.is_empty(),
            truncated: list.len() > MAX_CHANGESETS,
            ..Default::default()
        };
        let mut per_path: BTreeMap<Vec<u8>, (u32, u32, bool)> = BTreeMap::new();
        for cs in list.iter().take(MAX_CHANGESETS) {
            out.changesets += 1;
            if out.messages.len() < MAX_MESSAGES {
                let body = engine
                    .get_changeset(*cs)
                    .await
                    .map_err(|e| super::internal("changeset", &e))?;
                let subject = String::from_utf8_lossy(&body.message);
                let line = crate::notify::one_line(
                    subject.lines().next().unwrap_or_default(),
                    MAX_SUBJECT,
                );
                if !line.is_empty() {
                    out.messages.push(line);
                }
            }
            let st = engine
                .diffstat(*cs)
                .await
                .map_err(|e| super::internal("diffstat", &e))?;
            out.truncated |= st.truncated;
            for f in &st.files {
                let e = per_path.entry(f.path.as_bytes().to_vec()).or_default();
                e.0 = e.0.saturating_add(f.added);
                e.1 = e.1.saturating_add(f.removed);
                e.2 |= f.binary;
            }
        }
        let all: Vec<wire::ChangeFileStat> = per_path
            .into_iter()
            .map(|(path, (added, removed, binary))| wire::ChangeFileStat {
                path: Bytes::from(path),
                added,
                removed,
                binary,
            })
            .collect();
        out.added = all.iter().map(|f| f.added).fold(0, u32::saturating_add);
        out.removed = all.iter().map(|f| f.removed).fold(0, u32::saturating_add);
        out.risk = Some(brief::classify(&all, out.added, out.removed));
        out.truncated |= all.len() > MAX_FILES;
        out.files = all.into_iter().take(MAX_FILES).collect();
        Ok(out)
    }
}
