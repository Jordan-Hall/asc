//! The `LineHistory` RPC (PLAN C30a, SPEC §20.6): the changesets that changed a line range of
//! one file, newest first, from [`asc_engine::history::linehist`].
//!
//! The walk is bounded three ways: `scan_budget` candidates (default 10,000, at most 50,000),
//! `limit` changesets per page, and the size of one file version. When the budget runs out the
//! last message has `truncated` and a `resume_token`; when `limit` fills first it carries a
//! `next_page_token` (the same kind of token). Both tokens are untrusted on the way back in:
//! they are size-bounded and decoded strictly by the engine, and every id in them is read
//! through the store. Access (`Access::Read`) is checked by the service before this runs.
//! Lines are zero-based, the range is half open and `end_line == 0` means the end of the file.
//! Tokens are accepted in `resume_token` only.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::commit;
use asc_engine::derive::Gen;
use asc_engine::history::HistoryApi;
use asc_engine::history::linehist::{self, MAX_FILE_BYTES, Query, Stop};
use asc_rpc::server::{CallCtx, MAX_STRING, RpcStream, wire};
use futures::stream::{self, StreamExt};
use tonic::Status;

use crate::commit_graph_rpc::{commit_status, data_loss, internal, registry_status, store_status};
use crate::idmode::{Eng, IdCodec};
use crate::read::ReadHooks;
use crate::registry::{Registry, RepoEntry};

/// Entries per response message.
const ROWS_PER_MESSAGE: usize = 100;
/// Longest accepted token, base64.
const MAX_TOKEN_CHARS: usize = 8192;
/// Longest accepted path.
const MAX_PATH_BYTES: usize = 4096;
/// Whole-file bytes a call holds at once: one file version and its parent's.
const TEXT_RESERVE: u64 = 2 * MAX_FILE_BYTES;

/// The `line_history` method of the `insights` handler group.
pub struct ServerLineHistory {
    registry: Arc<Registry>,
    hooks: Arc<ReadHooks>,
}

fn line_status(e: linehist::Error) -> Status {
    use linehist::Error as E;
    match e {
        E::NotFound(p) => Status::not_found(format!("{p} is not a file at that revision")),
        e @ E::BadRange { .. } => Status::out_of_range(e.to_string()),
        e @ (E::Binary | E::TooLarge(_)) => Status::failed_precondition(e.to_string()),
        e @ E::BadToken => Status::invalid_argument(e.to_string()),
        E::Commit(e) => commit_status(e),
        E::Tree(e) => commit_status(commit::Error::Tree(e)),
        E::Blame(e) => internal("line history", &e),
        E::Diff(e) => match e {
            asc_engine::diff::Error::Store(e) => store_status(e),
            e @ (asc_engine::diff::Error::MissingFile(_) | asc_engine::diff::Error::Codec(_)) => {
                data_loss("file", &e)
            }
            e @ asc_engine::diff::Error::FileTooLarge(..) => {
                Status::failed_precondition(e.to_string())
            }
            e => internal("line history", &e),
        },
    }
}

impl ServerLineHistory {
    pub fn new(registry: Arc<Registry>, hooks: Arc<ReadHooks>) -> Self {
        Self { registry, hooks }
    }

    pub(crate) async fn entry_message(
        engine: &Eng,
        ids: &IdCodec,
        id: ChangesetId,
    ) -> Result<wire::LogEntry, Status> {
        let cs = engine.get_changeset(id).await.map_err(commit_status)?;
        let generation = engine
            .get_derived::<Gen>(id)
            .await
            .map_err(|e| internal("generation", &e))?
            .generation;
        let git = ids.git_of(id).await?;
        Ok(wire::LogEntry {
            id: ids.changeset_out(id).await?,
            git: Bytes::copy_from_slice(git.as_bytes()),
            parents: ids.changesets_out(&cs.parents).await?,
            author: Some(wire::Signature::from(&cs.author)),
            committer: Some(wire::Signature::from(&cs.committer)),
            message: Bytes::copy_from_slice(&cs.message),
            r#gen: generation,
            ..wire::LogEntry::default()
        })
    }

    async fn query(
        entry: &RepoEntry,
        engine: &Eng,
        ids: &IdCodec,
        req: &wire::LineHistoryRequest,
    ) -> Result<Query, Status> {
        if req.path.len() > MAX_PATH_BYTES {
            return Err(Status::invalid_argument("path is too long"));
        }
        let path =
            RepoPath::new(&req.path[..]).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resume = if req.resume_token.is_empty() {
            None
        } else if req.resume_token.len() > MAX_TOKEN_CHARS {
            return Err(Status::invalid_argument("resume token is too long"));
        } else {
            Some(
                URL_SAFE_NO_PAD
                    .decode(&req.resume_token)
                    .map_err(|_| Status::invalid_argument("malformed resume token"))?,
            )
        };
        let rev = if resume.is_some() {
            // The token carries the position, so the revision is not resolved.
            ChangesetId::from_bytes([0; 32])
        } else {
            if req.rev.len() > MAX_STRING {
                return Err(Status::invalid_argument("revision is too long"));
            }
            let spec = if req.rev.is_empty() { "HEAD" } else { &req.rev };
            let spec = ids.rev_in(spec, &entry.default_branch).await?;
            engine
                .resolve(std::slice::from_ref(&spec))
                .await
                .map_err(|e| Status::invalid_argument(e.to_string()))?
                .into_iter()
                .next()
                .flatten()
                .ok_or_else(|| Status::not_found(format!("revision {} does not exist", req.rev)))?
        };
        Ok(Query {
            rev,
            path,
            start: req.start_line,
            end: req.end_line,
            limit: req.limit as usize,
            scan_budget: req.scan_budget,
            resume,
        })
    }

    /// One bounded page of line history as response messages.
    pub async fn line_history(
        &self,
        ctx: &CallCtx,
        req: wire::LineHistoryRequest,
    ) -> Result<RpcStream<wire::LogResponse>, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry);
        let engine = entry.backend.engine().clone();
        let q = Self::query(&entry, &engine, &ids, &req).await?;
        let page = {
            let _guard = self
                .hooks
                .text_budget
                .acquire(TEXT_RESERVE)
                .await
                .map_err(|_| {
                    Status::resource_exhausted("text memory budget exhausted; retry shortly")
                })?;
            engine.line_history(&q).await.map_err(line_status)?
        };
        let (truncated, resume, next) = match page.stop {
            Stop::Done => (false, String::new(), String::new()),
            Stop::Limit(t) => (false, String::new(), URL_SAFE_NO_PAD.encode(t)),
            Stop::Budget(t) => (true, URL_SAFE_NO_PAD.encode(t), String::new()),
        };
        let count = page.hits.len() as u64;
        let mut messages = Vec::new();
        for group in page.hits.chunks(ROWS_PER_MESSAGE) {
            let entries = futures::future::try_join_all(
                group
                    .iter()
                    .map(|&id| Self::entry_message(&engine, &ids, id)),
            )
            .await?;
            messages.push(wire::LogResponse {
                entries,
                ..wire::LogResponse::default()
            });
        }
        if messages.is_empty() {
            messages.push(wire::LogResponse::default());
        }
        if let Some(last) = messages.last_mut() {
            last.count = count;
            last.truncated = truncated;
            last.resume_token = resume;
            last.next_page_token = next;
        }
        Ok(stream::iter(messages.into_iter().map(Ok)).boxed())
    }
}

#[cfg(test)]
mod tests {
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
    use futures::stream::TryStreamExt;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    struct Fx {
        handler: ServerLineHistory,
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        Fx {
            handler: ServerLineHistory::new(reg.clone(), Arc::new(ReadHooks::default())),
            reg,
            entry,
        }
    }

    fn ctx() -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
        }
    }

    fn req(start: u32, end: u32) -> wire::LineHistoryRequest {
        wire::LineHistoryRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            path: Bytes::from_static(b"f"),
            start_line: start,
            end_line: end,
            ..wire::LineHistoryRequest::default()
        }
    }

    async fn set_master(f: &Fx, cs: ChangesetId) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: asc_core::path::RefName::new(b"refs/heads/master".to_vec()).unwrap(),
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

    async fn call(f: &Fx, r: wire::LineHistoryRequest) -> Result<Vec<wire::LogResponse>, Status> {
        f.handler.line_history(&ctx(), r).await?.try_collect().await
    }

    fn ids_of(rs: &[wire::LogResponse]) -> Vec<Vec<u8>> {
        rs.iter()
            .flat_map(|r| r.entries.iter().map(|e| e.id.to_vec()))
            .collect()
    }

    /// Five versions of `f`; only the first line changes after the first one.
    async fn history(f: &Fx) -> Vec<ChangesetId> {
        let mut ids = vec![commit(&f.entry, &[], &[("f", b"a\nb\nc\n")]).await];
        for i in 0..4 {
            let text = format!("a{i}\nb\nc\n");
            let p = *ids.last().unwrap();
            ids.push(commit(&f.entry, &[p], &[("f", text.as_bytes())]).await);
        }
        set_master(f, *ids.last().unwrap()).await;
        ids
    }

    #[tokio::test]
    async fn returns_the_changesets_that_touched_the_range() {
        let f = fx().await;
        let ids = history(&f).await;
        let got = call(&f, req(0, 1)).await.unwrap();
        let want: Vec<_> = ids.iter().rev().map(|i| i.as_bytes().to_vec()).collect();
        assert_eq!(ids_of(&got), want);
        let last = got.last().unwrap();
        assert_eq!(last.count, 5);
        assert!(!last.truncated);
        assert!(last.resume_token.is_empty());
        assert!(got[0].entries[0].r#gen > 0);
        // Line 2 was only ever written by the first changeset.
        let got = call(&f, req(2, 3)).await.unwrap();
        assert_eq!(ids_of(&got), vec![ids[0].as_bytes().to_vec()]);
    }

    #[tokio::test]
    async fn exhausting_scan_budget_returns_truncated_with_a_resume_token() {
        let f = fx().await;
        let ids = history(&f).await;
        let mut r = req(0, 1);
        r.scan_budget = 2;
        let first = call(&f, r.clone()).await.unwrap();
        let last = first.last().unwrap();
        assert!(last.truncated);
        assert!(!last.resume_token.is_empty());
        let mut found = ids_of(&first);
        assert_eq!(found.len(), 2);
        let mut r2 = r.clone();
        r2.resume_token = last.resume_token.clone();
        let mut guard = 0;
        loop {
            let got = call(&f, r2.clone()).await.unwrap();
            found.extend(ids_of(&got));
            let last = got.last().unwrap();
            if !last.truncated {
                break;
            }
            r2.resume_token = last.resume_token.clone();
            guard += 1;
            assert!(guard < 10);
        }
        let want: Vec<_> = ids.iter().rev().map(|i| i.as_bytes().to_vec()).collect();
        assert_eq!(found, want);
    }

    #[tokio::test]
    async fn limit_returns_a_page_token() {
        let f = fx().await;
        history(&f).await;
        let mut r = req(0, 1);
        r.limit = 2;
        let got = call(&f, r.clone()).await.unwrap();
        assert_eq!(ids_of(&got).len(), 2);
        let last = got.last().unwrap();
        assert!(!last.truncated);
        assert!(!last.next_page_token.is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let f = fx().await;
        history(&f).await;
        let code = |r: Result<Vec<wire::LogResponse>, Status>| r.unwrap_err().code();
        assert_eq!(code(call(&f, req(0, 99)).await), tonic::Code::OutOfRange);
        let mut r = req(0, 1);
        r.path = Bytes::from_static(b"nope");
        assert_eq!(code(call(&f, r).await), tonic::Code::NotFound);
        let mut r = req(0, 1);
        r.path = Bytes::from_static(b"../x");
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req(0, 1);
        r.resume_token = "!!!".into();
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req(0, 1);
        r.resume_token = "A".repeat(MAX_TOKEN_CHARS + 1);
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req(0, 1);
        r.resume_token = URL_SAFE_NO_PAD.encode([1u8; 50]);
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req(0, 1);
        r.rev = "no-such-branch".into();
        assert!(call(&f, r).await.is_err());
    }
}
