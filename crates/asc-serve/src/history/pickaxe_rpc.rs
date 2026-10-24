//! The pickaxe path of the `Log` RPC (PLAN C30c, SPEC §20.5): the changesets that changed how
//! often a string, or a regular expression, occurs in one file, from
//! [`asc_engine::history::pickaxe`].
//!
//! A pickaxe needs exactly one path (`paths` has one entry); without it the request is refused,
//! since the search would read every file of the history. The walk is bounded by `scan_budget`
//! candidates (default 10,000, at most 50,000) and `limit` changesets per page. When the budget
//! runs out the last message has `truncated` and a `resume_token`; when `limit` fills first it
//! carries a `next_page_token` (the same kind of token). Tokens are untrusted: size-bounded,
//! decoded strictly by the engine, and their id is read through the store. Access
//! (`Access::Read`) is checked by the service before this runs. Tokens are accepted in
//! `resume_token` only.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::commit;
use asc_engine::history::linehist::MAX_FILE_BYTES;
use asc_engine::history::pickaxe::{self, MAX_NEEDLE_BYTES, Query, Stop};
use asc_rpc::server::{CallCtx, MAX_STRING, RpcStream, wire};
use futures::stream::{self, StreamExt};
use tonic::Status;

use crate::commit_graph_rpc::{commit_status, data_loss, internal, registry_status, store_status};
use crate::history::linehist_rpc::ServerLineHistory;
use crate::idmode::IdCodec;
use crate::read::ReadHooks;
use crate::registry::{Registry, RepoEntry};

/// Entries per response message.
const ROWS_PER_MESSAGE: usize = 100;
/// Longest accepted token, base64.
const MAX_TOKEN_CHARS: usize = 256;
/// Longest accepted path.
const MAX_PATH_BYTES: usize = 4096;
/// Whole-file bytes a call holds at once: one file version and its parent's.
const TEXT_RESERVE: u64 = 2 * MAX_FILE_BYTES;

/// The pickaxe form of `Log`, for the history handler group.
pub struct ServerPickaxe {
    registry: Arc<Registry>,
    hooks: Arc<ReadHooks>,
}

/// True when a `Log` request asks for a pickaxe search.
pub fn is_pickaxe(req: &wire::LogRequest) -> bool {
    !req.pickaxe.is_empty() || req.pickaxe_regex
}

fn pickaxe_status(e: pickaxe::Error) -> Status {
    use pickaxe::Error as E;
    match e {
        e @ (E::PathRequired
        | E::EmptyNeedle
        | E::NeedleTooLong(_)
        | E::BadPattern(_)
        | E::BadToken) => Status::invalid_argument(e.to_string()),
        E::Commit(e) => commit_status(e),
        E::Tree(e) => commit_status(commit::Error::Tree(e)),
        E::Diff(e) => match e {
            asc_engine::diff::Error::Store(e) => store_status(e),
            e @ (asc_engine::diff::Error::MissingFile(_) | asc_engine::diff::Error::Codec(_)) => {
                data_loss("file", &e)
            }
            e @ asc_engine::diff::Error::FileTooLarge(..) => {
                Status::failed_precondition(e.to_string())
            }
            e => internal("pickaxe", &e),
        },
    }
}

impl ServerPickaxe {
    pub fn new(registry: Arc<Registry>, hooks: Arc<ReadHooks>) -> Self {
        Self { registry, hooks }
    }

    async fn query(
        entry: &RepoEntry,
        ids: &IdCodec,
        req: &wire::LogRequest,
    ) -> Result<Query, Status> {
        if req.pickaxe.len() > MAX_NEEDLE_BYTES {
            return Err(Status::invalid_argument("pickaxe is too long"));
        }
        // The path is required: reject before any work, whatever else is in the request.
        let path = match req.paths.as_slice() {
            [] => {
                return Err(Status::invalid_argument(
                    "a pickaxe search needs a path (paths); it cannot walk every file",
                ));
            }
            [p] if p.len() <= MAX_PATH_BYTES => {
                RepoPath::new(p.as_bytes()).map_err(|e| Status::invalid_argument(e.to_string()))?
            }
            [_] => return Err(Status::invalid_argument("path is too long")),
            _ => {
                return Err(Status::invalid_argument(
                    "a pickaxe search takes exactly one path",
                ));
            }
        };
        if req.include.len() > 1 || !req.exclude.is_empty() {
            return Err(Status::invalid_argument(
                "a pickaxe search takes at most one start revision and no exclusions",
            ));
        }
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
            let spec = req.include.first().map_or("HEAD", String::as_str);
            let spec = if spec.is_empty() { "HEAD" } else { spec };
            if spec.len() > MAX_STRING {
                return Err(Status::invalid_argument("revision is too long"));
            }
            ids.resolve(spec, &entry.default_branch, |e| {
                Status::invalid_argument(e.to_string())
            })
            .await?
            .ok_or_else(|| Status::not_found(format!("revision {spec} does not exist")))?
        };
        Ok(Query {
            rev,
            path: Some(path),
            needle: req.pickaxe.to_vec(),
            regex: req.pickaxe_regex,
            limit: req.limit as usize,
            scan_budget: req.scan_budget,
            resume,
        })
    }

    /// One bounded page of pickaxe results as response messages.
    pub async fn log(
        &self,
        ctx: &CallCtx,
        req: wire::LogRequest,
    ) -> Result<RpcStream<wire::LogResponse>, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        let q = Self::query(&entry, &ids, &req).await?;
        let page = {
            let _guard = self
                .hooks
                .text_budget
                .acquire(TEXT_RESERVE)
                .await
                .map_err(|_| {
                    Status::resource_exhausted("text memory budget exhausted; retry shortly")
                })?;
            engine.pickaxe(&q).await.map_err(pickaxe_status)?
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
                    .map(|&id| ServerLineHistory::entry_message(&engine, &ids, id)),
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
    use bytes::Bytes;
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
    use futures::stream::TryStreamExt;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    struct Fx {
        handler: ServerPickaxe,
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        Fx {
            handler: ServerPickaxe::new(reg.clone(), Arc::new(ReadHooks::default())),
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

    fn req(needle: &str) -> wire::LogRequest {
        wire::LogRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            paths: vec!["f".into()],
            pickaxe: Bytes::copy_from_slice(needle.as_bytes()),
            ..wire::LogRequest::default()
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

    async fn call(f: &Fx, r: wire::LogRequest) -> Result<Vec<wire::LogResponse>, Status> {
        f.handler.log(&ctx(), r).await?.try_collect().await
    }

    fn ids_of(rs: &[wire::LogResponse]) -> Vec<Vec<u8>> {
        rs.iter()
            .flat_map(|r| r.entries.iter().map(|e| e.id.to_vec()))
            .collect()
    }

    /// Five versions of `f`; `needle` is added by the first and the fourth.
    async fn history(f: &Fx) -> Vec<ChangesetId> {
        let texts: [&[u8]; 5] = [
            b"needle\nb\n",
            b"needle\nB\n",
            b"needle\nC\n",
            b"needle\nC\nneedle\n",
            b"needle\nD\nneedle\n",
        ];
        let mut ids: Vec<ChangesetId> = Vec::new();
        for t in texts {
            let parents: Vec<ChangesetId> = ids.last().copied().into_iter().collect();
            ids.push(commit(&f.entry, &parents, &[("f", t)]).await);
        }
        set_master(f, *ids.last().unwrap()).await;
        ids
    }

    fn bytes_of(ids: &[ChangesetId], idx: &[usize]) -> Vec<Vec<u8>> {
        idx.iter().map(|&i| ids[i].as_bytes().to_vec()).collect()
    }

    #[tokio::test]
    async fn returns_the_changesets_that_changed_the_match_count() {
        let f = fx().await;
        let ids = history(&f).await;
        let got = call(&f, req("needle")).await.unwrap();
        assert_eq!(ids_of(&got), bytes_of(&ids, &[3, 0]));
        let last = got.last().unwrap();
        assert_eq!(last.count, 2);
        assert!(!last.truncated);
        assert!(got[0].entries[0].r#gen > 0);
    }

    #[tokio::test]
    async fn budget_exhaustion_returns_truncated_with_a_resume_token() {
        let f = fx().await;
        let ids = history(&f).await;
        let mut r = req("needle");
        r.scan_budget = 2;
        let first = call(&f, r.clone()).await.unwrap();
        let last = first.last().unwrap();
        assert!(last.truncated);
        assert!(!last.resume_token.is_empty());
        assert!(last.next_page_token.is_empty());
        let mut found = ids_of(&first);
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
        assert_eq!(found, bytes_of(&ids, &[3, 0]));
    }

    #[tokio::test]
    async fn limit_returns_a_page_token() {
        let f = fx().await;
        history(&f).await;
        let mut r = req("needle");
        r.limit = 1;
        let got = call(&f, r).await.unwrap();
        assert_eq!(ids_of(&got).len(), 1);
        let last = got.last().unwrap();
        assert!(!last.truncated);
        assert!(!last.next_page_token.is_empty());
    }

    #[tokio::test]
    async fn pickaxe_without_a_path_is_rejected() {
        let f = fx().await;
        history(&f).await;
        let code = |r: Result<Vec<wire::LogResponse>, Status>| r.unwrap_err().code();
        let mut r = req("needle");
        r.paths.clear();
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.paths = vec!["f".into(), "g".into()];
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.paths = vec!["../x".into()];
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let f = fx().await;
        history(&f).await;
        let code = |r: Result<Vec<wire::LogResponse>, Status>| r.unwrap_err().code();
        let mut r = req("(");
        r.pickaxe_regex = true;
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.resume_token = "!!!".into();
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.resume_token = "A".repeat(MAX_TOKEN_CHARS + 1);
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.resume_token = URL_SAFE_NO_PAD.encode([1u8; 50]);
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.pickaxe = Bytes::from(vec![b'x'; MAX_NEEDLE_BYTES + 1]);
        assert_eq!(code(call(&f, r).await), tonic::Code::InvalidArgument);
        let mut r = req("needle");
        r.include = vec!["no-such-branch".into()];
        assert!(call(&f, r).await.is_err());
        assert!(is_pickaxe(&req("x")));
    }
}
