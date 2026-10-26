//! The `Compare` RPC (PLAN C30b, SPEC §20.6): ahead and behind of two revisions, a page of
//! changesets per side and the changed files, from [`asc_engine::history::compare`].
//!
//! `ahead` counts what `b` has that `a` lacks, `behind` the reverse; a count stops at the
//! engine's cap. `three_dot` takes the files from the merge base to `b`, and fails with
//! `FAILED_PRECONDITION` when the two have none. The response has no truncation flag, so the
//! file list stops at the engine's file cap. Both page tokens are untrusted: they are
//! size-bounded here and decoded strictly by the engine. Access (`Access::Read`) is checked by
//! the service before this runs.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::object::{Entry, Kind};
use asc_engine::diff::TreeChange;
use asc_engine::history::compare::{self, Comparison, MAX_TOKEN_BYTES, Query};
use asc_rpc::IdMode;
use asc_rpc::server::{CallCtx, MAX_STRING, wire};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use tonic::Status;

use crate::commit_graph_rpc::{commit_status, data_loss, internal, registry_status, store_status};
use crate::history::linehist_rpc::ServerLineHistory;
use crate::idmode::{Eng, IdCodec};
use crate::registry::{Registry, RepoEntry};

/// Longest accepted token, base64 of [`MAX_TOKEN_BYTES`].
const MAX_TOKEN_CHARS: usize = MAX_TOKEN_BYTES.div_ceil(3) * 4;
/// Concurrent changeset reads while building entries.
const FANOUT: usize = 16;

/// The `compare` method of the `insights` handler group.
pub struct ServerCompare {
    registry: Arc<Registry>,
}

fn compare_status(e: compare::Error) -> Status {
    use compare::Error as E;
    match e {
        e @ E::BadToken => Status::invalid_argument(e.to_string()),
        e @ E::NoMergeBase => Status::failed_precondition(e.to_string()),
        E::Graph(e) => match e {
            asc_engine::graph::Error::Missing(id) => {
                Status::not_found(format!("changeset {id} is missing"))
            }
            asc_engine::graph::Error::Query(m) => Status::invalid_argument(m),
            asc_engine::graph::Error::Engine(e) => commit_status(e),
            e => internal("compare", &e),
        },
        E::Commit(e) => commit_status(e),
        E::Tree(e) => commit_status(asc_engine::commit::Error::Tree(e)),
        E::Diff(e) => match e {
            asc_engine::diff::Error::Store(e) => store_status(e),
            e @ (asc_engine::diff::Error::MissingFile(_) | asc_engine::diff::Error::Codec(_)) => {
                data_loss("file", &e)
            }
            e => internal("compare", &e),
        },
    }
}

fn token_in(t: &str) -> Result<Option<Vec<u8>>, Status> {
    if t.is_empty() {
        Ok(None)
    } else if t.len() > MAX_TOKEN_CHARS {
        Err(Status::invalid_argument("page token is too long"))
    } else {
        URL_SAFE_NO_PAD
            .decode(t)
            .map(Some)
            .map_err(|_| Status::invalid_argument("malformed page token"))
    }
}

fn token_out(t: &Option<Vec<u8>>) -> String {
    t.as_ref()
        .map_or_else(String::new, |t| URL_SAFE_NO_PAD.encode(t))
}

fn path_change(mode: IdMode, c: &TreeChange) -> wire::PathChange {
    let side = |e: &Option<Entry>| match e {
        Some(e) if e.kind != Kind::Gitlink => (
            match mode {
                IdMode::Asc => Bytes::copy_from_slice(&e.id),
                IdMode::Git => Bytes::copy_from_slice(e.git.as_bytes()),
            },
            e.size,
        ),
        _ => (Bytes::new(), 0),
    };
    let git = |e: &Option<Entry>| {
        e.as_ref()
            .map_or_else(Bytes::new, |e| Bytes::copy_from_slice(e.git.as_bytes()))
    };
    let (old_id, old_size) = side(&c.old);
    let (new_id, new_size) = side(&c.new);
    wire::PathChange {
        path: Bytes::copy_from_slice(c.path.as_bytes()),
        status: c.status.letter().to_string(),
        old_path: if c.old_path == c.path {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(c.old_path.as_bytes())
        },
        old_mode: c.old_mode(),
        new_mode: c.new_mode(),
        old_git: git(&c.old),
        new_git: git(&c.new),
        old_id,
        new_id,
        old_size,
        new_size,
        similarity: c.status.score().map_or(0, u32::from),
    }
}

impl ServerCompare {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    async fn rev(entry: &RepoEntry, ids: &IdCodec, spec: &str) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let shown = if spec.is_empty() { "HEAD" } else { spec };
        ids.resolve(shown, &entry.default_branch, |e| {
            Status::invalid_argument(e.to_string())
        })
        .await?
        .ok_or_else(|| Status::not_found(format!("revision {shown} does not exist")))
    }

    async fn entries(
        engine: &Eng,
        ids: &IdCodec,
        list: &[ChangesetId],
    ) -> Result<Vec<wire::LogEntry>, Status> {
        let mut out = Vec::with_capacity(list.len());
        for group in list.chunks(FANOUT) {
            out.extend(
                futures::future::try_join_all(
                    group
                        .iter()
                        .map(|&id| ServerLineHistory::entry_message(engine, ids, id)),
                )
                .await?,
            );
        }
        Ok(out)
    }

    /// One bounded comparison.
    pub async fn compare(
        &self,
        ctx: &CallCtx,
        req: wire::CompareRequest,
    ) -> Result<wire::CompareResponse, Status> {
        let ahead_page = token_in(&req.ahead_page_token)?;
        let behind_page = token_in(&req.behind_page_token)?;
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        let a = Self::rev(&entry, &ids, &req.a).await?;
        let b = Self::rev(&entry, &ids, &req.b).await?;
        let got: Comparison = engine
            .compare(&Query {
                a,
                b,
                three_dot: req.three_dot,
                limit: req.limit as usize,
                ahead_page,
                behind_page,
            })
            .await
            .map_err(compare_status)?;
        let ahead_entries = Self::entries(&engine, &ids, &got.ahead.ids).await?;
        let behind_entries = Self::entries(&engine, &ids, &got.behind.ids).await?;
        let merge_base = match got.merge_base {
            Some(id) => ids.changeset_out(id).await?,
            None => Bytes::new(),
        };
        Ok(wire::CompareResponse {
            merge_base,
            ahead: got.ahead.total,
            behind: got.behind.total,
            ahead_entries,
            behind_entries,
            ahead_next: token_out(&got.ahead.next),
            behind_next: token_out(&got.behind.next),
            files: got
                .files
                .iter()
                .map(|c| path_change(ctx.id_mode, c))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    struct Fx {
        handler: ServerCompare,
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        Fx {
            handler: ServerCompare::new(reg.clone()),
            reg,
            entry,
        }
    }

    fn ctx(mode: IdMode) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: mode,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
        }
    }

    fn req(a: &str, b: &str) -> wire::CompareRequest {
        wire::CompareRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            a: a.into(),
            b: b.into(),
            ..wire::CompareRequest::default()
        }
    }

    async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: asc_core::path::RefName::new(name.as_bytes().to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(asc_core::id::RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::new_v4(),
        };
        f.reg.refs().update(f.entry.key, &txn).await.unwrap();
    }

    /// base, then master gains two changesets and topic gains three.
    async fn fork(f: &Fx) -> (ChangesetId, Vec<ChangesetId>) {
        let base = commit(&f.entry, &[], &[("f", b"base\n")]).await;
        let mut m = vec![base];
        for i in 0..2 {
            let s = format!("m{i}\n");
            m.push(commit(&f.entry, &[*m.last().unwrap()], &[("m", s.as_bytes())]).await);
        }
        let mut t = vec![base];
        for i in 0..3 {
            let s = format!("t{i}\n");
            t.push(commit(&f.entry, &[*t.last().unwrap()], &[("t", s.as_bytes())]).await);
        }
        set_ref(f, "refs/heads/master", *m.last().unwrap()).await;
        set_ref(f, "refs/heads/topic", *t.last().unwrap()).await;
        (base, t)
    }

    #[tokio::test]
    async fn reports_ahead_behind_merge_base_and_files() {
        let f = fx().await;
        let (base, t) = fork(&f).await;
        let mut r = req("master", "topic");
        r.three_dot = true;
        let got = f.handler.compare(&ctx(IdMode::Asc), r).await.unwrap();
        assert_eq!((got.ahead, got.behind), (3, 2));
        assert_eq!(got.merge_base.to_vec(), base.as_bytes().to_vec());
        assert_eq!(got.ahead_entries.len(), 3);
        assert_eq!(
            got.ahead_entries[0].id.to_vec(),
            t.last().unwrap().as_bytes().to_vec()
        );
        assert_eq!(got.behind_entries.len(), 2);
        assert!(got.ahead_entries[0].r#gen > 0);
        assert!(got.ahead_next.is_empty() && got.behind_next.is_empty());
        let paths: Vec<_> = got.files.iter().map(|c| c.path.to_vec()).collect();
        assert_eq!(paths, vec![b"t".to_vec()]);
        // Two dots also list the files master changed.
        let two = f
            .handler
            .compare(&ctx(IdMode::Asc), req("master", "topic"))
            .await
            .unwrap();
        let paths: Vec<_> = two.files.iter().map(|c| c.path.to_vec()).collect();
        assert_eq!(paths, vec![b"m".to_vec(), b"t".to_vec()]);
    }

    #[tokio::test]
    async fn pages_a_side_with_its_token() {
        let f = fx().await;
        fork(&f).await;
        let mut r = req("master", "topic");
        r.limit = 2;
        let first = f
            .handler
            .compare(&ctx(IdMode::Asc), r.clone())
            .await
            .unwrap();
        assert_eq!(first.ahead_entries.len(), 2);
        assert_eq!(first.ahead, 3);
        assert!(!first.ahead_next.is_empty());
        r.ahead_page_token = first.ahead_next.clone();
        let second = f.handler.compare(&ctx(IdMode::Asc), r).await.unwrap();
        assert_eq!(second.ahead_entries.len(), 1);
        assert!(second.ahead_next.is_empty());
    }

    #[tokio::test]
    async fn empty_revision_means_head_and_git_mode_answers() {
        let f = fx().await;
        fork(&f).await;
        let got = f
            .handler
            .compare(&ctx(IdMode::Git), req("", "master"))
            .await
            .unwrap();
        assert_eq!((got.ahead, got.behind), (0, 0));
        assert!(got.files.is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let f = fx().await;
        fork(&f).await;
        let code = |r: Result<wire::CompareResponse, Status>| r.unwrap_err().code();
        let c = ctx(IdMode::Asc);
        assert_eq!(
            code(f.handler.compare(&c, req("master", "nope")).await),
            tonic::Code::NotFound
        );
        let mut r = req("master", "topic");
        r.ahead_page_token = "!!!".into();
        assert_eq!(
            code(f.handler.compare(&c, r).await),
            tonic::Code::InvalidArgument
        );
        let mut r = req("master", "topic");
        r.behind_page_token = "A".repeat(MAX_TOKEN_CHARS + 1);
        assert_eq!(
            code(f.handler.compare(&c, r).await),
            tonic::Code::InvalidArgument
        );
        let mut r = req("master", "topic");
        r.ahead_page_token = URL_SAFE_NO_PAD.encode([9u8; 7]);
        assert_eq!(
            code(f.handler.compare(&c, r).await),
            tonic::Code::InvalidArgument
        );
        let long = "x".repeat(MAX_STRING + 1);
        assert_eq!(
            code(f.handler.compare(&c, req("master", &long)).await),
            tonic::Code::InvalidArgument
        );
    }
}
