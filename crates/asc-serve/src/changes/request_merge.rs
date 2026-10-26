//! `RequestMerge` (PLAN C50, SPEC §20.5 flow 5 and the `changes.proto` comment).
//!
//! `RequestMerge(change, now)` asks for "bottom..change" of a stack to land. The server does not
//! decide who may merge what: with a forge attached it forwards the request to the forge's queue
//! (C39) over the internal channel (the one that carries proc-receive, in the other direction)
//! and relays the answer, so the CLI never calls a forge API (D9). With no forge attached,
//! `now = true` lands the prefix directly and `now = false` is `STATE_REFUSED`.
//!
//! * **Authority.** The service shell has checked write access. A forwarded request carries the
//!   user from the authenticated principal, never from the request, and only a principal that is
//!   a real forge user (id above zero) may be forwarded: the forge's own internal principal and
//!   anonymous callers are `PERMISSION_DENIED`, because the queue acts as that user.
//! * **Fail closed.** With a forge attached the server never lands by itself: an unreachable
//!   forge, a forge that answers with an error status or an answer that does not parse is
//!   `UNAVAILABLE` or `INTERNAL`, and no ref moves. The answer is untrusted: its state must be
//!   known, `pending` may only name changes of the prefix below the requested one, and the
//!   reason is cleaned and bounded.
//! * **The prefix.** From the requested change down its parent chain to the first change that is
//!   landed, unknown to the index, or has no parent, at most [`MAX_PREFIX`] changes, listed
//!   bottom first. A divergent change in the prefix is refused in either mode.
//! * **Direct land (no forge).** One `UpdateRefs(LAND)` on the default branch with the head of
//!   the requested change as the new tip, hooks run, so the same policy as `asc push --land`
//!   binds, and the land is a pushrebase (§8.5), atomic for the whole prefix. A prefix whose
//!   heads do not sit on each other (a change above the bottom is not on the newest version of
//!   its parent) is refused: the stack needs `asc sync` first. A land error (conflict, needs a
//!   client rebase) is returned as the error `UpdateRefs` gives, with its details. A change that
//!   is already landed answers `STATE_LANDED` and does nothing. The call needs `x-asc-op-id`.
//!
//! Choices where SPEC is silent:
//!
//! * The forge endpoint is `POST /api/internal/asc/request-merge/{owner}/{repo}` with the
//!   internal token. The body is `{user_id, user_name, change, now, prefix: [{change, git}]}`
//!   (changes in their 32-letter form, `git` the head's commit id, bottom first) and the answer
//!   is `{state: "queued" | "landed" | "refused", pending: [change], reason}`. The `x-asc-op-id`
//!   header is relayed when the caller sent one.
//! * The target is the repository's default branch (the request names none).

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use asc_core::change::ChangeId;
use asc_core::id::RefTarget;
use asc_rpc::IdMode;
use asc_rpc::changes::{MergeOutcome, MergeRequestState};
use asc_rpc::server::{CallCtx, MAX_STRING, RepoName, wire};
use async_trait::async_trait;
use tonic::Status;
use tonic::metadata::MetadataMap;
use uuid::Uuid;

use super::rpc::{MAX_STACK, Reader};
use crate::authz::UserIdentity;
use crate::idmode::IdCodec;
use crate::registry::{Extensions, Registry, RegistryError};
use crate::update::{ServerRefs, internal};

/// Most changes one request may merge (a stack's bound).
pub const MAX_PREFIX: usize = MAX_STACK;
/// Longest reason relayed to a client.
pub const MAX_REASON: usize = 4096;

// ------------------------------------------------------------------------------ the forge

/// One change of the prefix as the forge needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixEntry {
    pub change: ChangeId,
    /// The head's git commit id, 40 hex digits.
    pub git: String,
}

/// What the forge is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeAsk {
    /// The forge user the request acts for (from the authenticated principal).
    pub user_id: i64,
    pub user_name: String,
    /// The requested change: the top of `prefix`.
    pub change: ChangeId,
    pub now: bool,
    /// Bottom to top.
    pub prefix: Vec<PrefixEntry>,
}

/// Why the forge's queue could not answer.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Unreachable, a failure status or an oversized answer.
    #[error("the forge could not be reached")]
    Unavailable,
    /// The forge refused the caller (401 or 403); carries the forge's user-safe message.
    #[error("{0}")]
    Denied(String),
    /// The forge's answer did not parse.
    #[error("the forge sent an invalid answer")]
    Invalid,
}

/// The forge's merge queue (C39), as the server calls it.
#[async_trait]
pub trait MergeQueue: Send + Sync {
    async fn request_merge(
        &self,
        repo: &RepoName,
        ask: &MergeAsk,
        op_id: Option<Uuid>,
    ) -> Result<MergeOutcome, QueueError>;
}

/// A user-safe reason: printable, bounded.
pub fn clean_reason(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c == '\n' || !c.is_control() { c } else { ' ' })
        .take(MAX_REASON)
        .collect();
    out.truncate(out.trim_end().len());
    out
}

/// The JSON body sent to the forge.
pub fn forge_body(ask: &MergeAsk) -> serde_json::Value {
    serde_json::json!({
        "user_id": ask.user_id,
        "user_name": ask.user_name,
        "change": ask.change.to_string(),
        "now": ask.now,
        "prefix": ask.prefix.iter().map(|p| serde_json::json!({
            "change": p.change.to_string(),
            "git": p.git,
        })).collect::<Vec<_>>(),
    })
}

/// Parses the forge's `200` answer. Nothing in it is trusted beyond its shape here; the handler
/// checks it against the prefix.
pub fn parse_forge_answer(body: &[u8]) -> Result<MergeOutcome, QueueError> {
    #[derive(serde::Deserialize, Default)]
    struct Answer {
        #[serde(default)]
        state: String,
        #[serde(default)]
        pending: Vec<String>,
        #[serde(default)]
        reason: String,
    }
    let a: Answer = serde_json::from_slice(body).map_err(|_| QueueError::Invalid)?;
    let state = match a.state.as_str() {
        "queued" => MergeRequestState::Queued,
        "landed" => MergeRequestState::Landed,
        "refused" => MergeRequestState::Refused,
        _ => return Err(QueueError::Invalid),
    };
    if a.pending.len() > MAX_PREFIX {
        return Err(QueueError::Invalid);
    }
    let pending = a
        .pending
        .iter()
        .map(|s| ChangeId::from_str(s).map_err(|_| QueueError::Invalid))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MergeOutcome {
        state,
        pending,
        reason: clean_reason(&a.reason),
    })
}

// ------------------------------------------------------------------------------ the handler

/// The `RequestMerge` handler behind the `Changes` service.
pub struct RequestMerger {
    registry: Arc<Registry>,
    refs: ServerRefs,
    /// The forge's queue; unset when no forge is attached.
    queue: OnceLock<Arc<dyn MergeQueue>>,
}

/// The unit's one line in `register_all` (through [`super::register`]): hands the `Changes`
/// handler its `RequestMerge` implementation.
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    let handler = Arc::new(RequestMerger {
        registry: registry.clone(),
        refs: ServerRefs::new(registry.clone(), ext.pipeline.clone()),
        queue: OnceLock::new(),
    });
    let changes = ext
        .changes
        .get()
        .expect("the Changes handler is registered before RequestMerge");
    assert!(
        changes.request_merge.set(handler).is_ok(),
        "RequestMerge is registered once"
    );
}

/// Attaches the forge's queue (called by `forgejo::register` once Forgejo is configured). When
/// the `Changes` handler serves no `RequestMerge` there is nothing to attach to; attaching twice
/// is an error. A server that has a queue never lands by itself.
pub fn attach_queue(ext: &Extensions, queue: Arc<dyn MergeQueue>) -> Result<(), &'static str> {
    match ext.changes.get().and_then(|c| c.request_merge.get()) {
        None => Ok(()),
        Some(h) => h
            .queue
            .set(queue)
            .map_err(|_| "the merge queue is attached once"),
    }
}

/// The `x-asc-op-id` header, when present.
pub(super) fn op_id_of(meta: &MetadataMap) -> Result<Option<Uuid>, Status> {
    meta.get("x-asc-op-id")
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| Status::invalid_argument("malformed x-asc-op-id"))
        })
        .transpose()
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn answer(
    state: MergeRequestState,
    pending: Vec<ChangeId>,
    reason: String,
) -> wire::RequestMergeResponse {
    wire::RequestMergeResponse::from(&MergeOutcome {
        state,
        pending,
        reason,
    })
}

fn refused(reason: impl Into<String>) -> wire::RequestMergeResponse {
    answer(MergeRequestState::Refused, Vec::new(), reason.into())
}

/// One change of the prefix with what the server needs of it.
struct Link {
    change: ChangeId,
    head: Arc<super::rpc::Head>,
}

impl RequestMerger {
    pub async fn request_merge(
        &self,
        ctx: &CallCtx,
        req: wire::RequestMergeRequest,
    ) -> Result<wire::RequestMergeResponse, Status> {
        let change = <[u8; 16]>::try_from(req.change.as_ref())
            .map(ChangeId::from_bytes)
            .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))?;
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        let land_ref = format!("refs/heads/{}", entry.default_branch);
        if entry.default_branch.is_empty()
            || entry.default_branch.len() > MAX_STRING
            || asc_core::path::RefName::new(land_ref.as_bytes()).is_err()
        {
            return Err(Status::failed_precondition("the default branch is invalid"));
        }
        let mut conn = self
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| internal("database", &e))?;
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref: land_ref.clone(),
            heads: Default::default(),
        };
        if reader.head(change).await?.is_none() {
            return Err(Status::not_found("unknown change"));
        }
        if reader.landed(change).await?.is_some() {
            return Ok(answer(
                MergeRequestState::Landed,
                Vec::new(),
                "already landed".into(),
            ));
        }
        let links = Self::prefix(&mut reader, change).await?;
        if let Some(l) = links.iter().find(|l| l.head.visible.len() > 1) {
            return Ok(refused(format!(
                "change {} is divergent; resolve it first",
                l.change
            )));
        }

        if let Some(queue) = self.queue.get() {
            // A forge is attached: it decides, and nothing below lands by itself.
            let identity = UserIdentity::of(&ctx.principal)
                .filter(|i| i.user_id > 0)
                .ok_or_else(|| {
                    Status::permission_denied("merging needs a forge user; this caller is not one")
                })?;
            let mut prefix = Vec::with_capacity(links.len());
            for l in &links {
                prefix.push(PrefixEntry {
                    change: l.change,
                    git: ids.git_of(l.head.cs).await?.to_string(),
                });
            }
            let ask = MergeAsk {
                user_id: identity.user_id,
                user_name: identity.handle,
                change,
                now: req.now,
                prefix,
            };
            let got = queue
                .request_merge(&ctx.repo, &ask, ctx.op_id)
                .await
                .map_err(|e| match e {
                    QueueError::Unavailable => {
                        Status::unavailable("the forge could not be reached; try again")
                    }
                    QueueError::Denied(m) => Status::permission_denied(clean_reason(&m)),
                    QueueError::Invalid => {
                        internal("request merge", &"the forge sent an invalid answer")
                    }
                })?;
            return Self::checked(&links, got);
        }

        // No forge attached.
        if !req.now {
            return Ok(refused(
                "no forge is attached to queue the merge; ask for it to land now instead",
            ));
        }
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        // Only published versions land: a change with no `change_versions` row is an
        // unpublished draft (the head read falls back to its changeset at version 0).
        for l in &links {
            let published = super::versions::latest(&mut *reader.conn, reader.ns, l.change)
                .await
                .map_err(|e| internal("versions", &e))?;
            if published.is_none_or(|p| p.cs != l.head.cs) {
                return Ok(refused(format!(
                    "change {} has no published version; publish it first",
                    l.change
                )));
            }
        }
        for l in links.iter().skip(1) {
            if !reader.parent_current(&l.head).await? {
                return Ok(refused(format!(
                    "change {} is not on the newest version of its parent; sync the stack first",
                    l.change
                )));
            }
        }
        // The land takes its own connections; holding this one could starve a small pool.
        drop(conn);
        let top = links.last().expect("the prefix holds the requested change");
        let update = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: IdMode::Asc as i32,
            }),
            updates: vec![wire::RefUpdate {
                name: land_ref,
                expect: Some(wire::ref_update::Expect::Any(true)),
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(top.head.cs))),
                delete: false,
            }],
            mode: wire::UpdateMode::Land as i32,
            hooks: wire::HookMode::Run as i32,
            ..wire::UpdateRefsRequest::default()
        };
        let mut land_ctx = ctx.clone();
        land_ctx.op_id = Some(op_id);
        land_ctx.id_mode = IdMode::Asc;
        let done = self.refs.update_rewriting(&land_ctx, update, None).await?;
        Ok(answer(
            MergeRequestState::Landed,
            Vec::new(),
            clean_reason(&done.message),
        ))
    }

    /// The changes from the bottom unlanded ancestor of `change` up to `change`.
    async fn prefix(reader: &mut Reader<'_>, change: ChangeId) -> Result<Vec<Link>, Status> {
        let mut seen = HashSet::from([change]);
        let mut links = Vec::new();
        let mut cur = change;
        loop {
            let head = reader
                .head(cur)
                .await?
                .ok_or_else(|| Status::not_found("unknown change"))?;
            let parent = head.parent;
            links.push(Link { change: cur, head });
            let Some(p) = parent else { break };
            if !seen.insert(p) {
                return Err(Status::data_loss("the parent chain of a change loops"));
            }
            if seen.len() > MAX_PREFIX {
                return Err(Status::resource_exhausted(format!(
                    "a merge request covers at most {MAX_PREFIX} changes"
                )));
            }
            if reader.head(p).await?.is_none() || reader.landed(p).await?.is_some() {
                break;
            }
            cur = p;
        }
        links.reverse();
        Ok(links)
    }

    /// The forge's answer, checked against the prefix before it reaches the client.
    fn checked(links: &[Link], got: MergeOutcome) -> Result<wire::RequestMergeResponse, Status> {
        let below: HashSet<ChangeId> = links[..links.len() - 1].iter().map(|l| l.change).collect();
        let mut seen = HashSet::new();
        if got
            .pending
            .iter()
            .any(|p| !below.contains(p) || !seen.insert(*p))
        {
            return Err(internal(
                "request merge",
                &"the forge named a pending change outside the prefix",
            ));
        }
        Ok(answer(got.state, got.pending, clean_reason(&got.reason)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use asc_core::change::{ChangeMeta, RewriteOp};
    use asc_core::id::ChangesetId;
    use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
    use asc_rpc::server::ServerInfo;
    use asc_rpc::services::changes_server::Changes;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs as _};
    use tonic::Request;

    use super::super::rpc::ChangesService;
    use super::super::{index, versions};
    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::read::ReadHooks;
    use crate::registry::RepoEntry;
    use crate::update::PushHooks;

    const READER: &str = "reader-token";
    const WRITER: &str = "writer-token";
    const MEMBER: &str = "member-token";
    const FORGE: &str = "forge-token";

    struct Internal;
    impl PushHooks for Internal {
        fn may_skip_pre(&self, ctx: &CallCtx) -> bool {
            crate::authz::is_host_service(&ctx.principal.id)
        }
    }

    /// A forge queue that records what it is asked and answers what it is told.
    #[derive(Default)]
    struct FakeQueue {
        asked: Mutex<Vec<MergeAsk>>,
        ops: Mutex<Vec<Option<Uuid>>>,
        reply: Mutex<Option<Result<MergeOutcome, u8>>>,
    }

    impl FakeQueue {
        fn replying(r: Result<MergeOutcome, u8>) -> Arc<Self> {
            let q = Self::default();
            *q.reply.lock().unwrap() = Some(r);
            Arc::new(q)
        }
        fn asked(&self) -> Vec<MergeAsk> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MergeQueue for FakeQueue {
        async fn request_merge(
            &self,
            _repo: &RepoName,
            ask: &MergeAsk,
            op_id: Option<Uuid>,
        ) -> Result<MergeOutcome, QueueError> {
            self.asked.lock().unwrap().push(ask.clone());
            self.ops.lock().unwrap().push(op_id);
            match self.reply.lock().unwrap().clone().expect("a reply is set") {
                Ok(o) => Ok(o),
                Err(0) => Err(QueueError::Unavailable),
                Err(1) => Err(QueueError::Denied("not yours\u{1b}[31m".into())),
                Err(_) => Err(QueueError::Invalid),
            }
        }
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    fn cbytes(n: u8) -> bytes::Bytes {
        bytes::Bytes::copy_from_slice(cid(n).as_bytes())
    }

    struct Fx {
        reg: Arc<Registry>,
        e: Arc<RepoEntry>,
        ext: Extensions,
        svc: ChangesService,
        /// The trunk's first commit.
        a: ChangesetId,
        /// Heads of changes 1 and 2 (a stack on `a`), 3 (alone on `a`) and 4 (on change 2 but
        /// built on a stale head of it).
        c: [ChangesetId; 4],
    }

    async fn publish(
        reg: &Arc<Registry>,
        e: &RepoEntry,
        n: u8,
        cs: ChangesetId,
        parent: Option<u8>,
    ) {
        let idx = index::ChangeIndex::new(reg.clone());
        let mut tx = reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            e.key,
            &ChangeMeta::new(cs, cid(n), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        versions::append(
            &mut tx,
            e.ns,
            &versions::NewVersion {
                change: cid(n),
                cs,
                parent_change: parent.map(cid),
                at: 5,
                actor: "alice",
                op_id: b"",
                force: false,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        let a = commit(&e, &[], &[("a", b"a\n")]).await;
        let c1 = commit(&e, &[a], &[("one", b"1\n")]).await;
        let c2 = commit(&e, &[c1], &[("two", b"2\n")]).await;
        let c3 = commit(&e, &[a], &[("three", b"3\n")]).await;
        // Change 4 names change 2 as its parent, but sits on `a`, not on change 2's head.
        let c4 = commit(&e, &[a], &[("four", b"4\n")]).await;
        let ext = Extensions::new();
        ext.pipeline
            .set_push_hooks(Arc::new(Internal))
            .ok()
            .unwrap();
        let hooks = Arc::new(ReadHooks::default());
        super::super::register(&ext, &reg, &hooks);
        let auth = StaticTokens::new()
            .with_token(READER, "reader", Access::Read)
            .with_token(WRITER, "writer", Access::Write)
            .with_token(MEMBER, "7:alice", Access::Write)
            .with_token(FORGE, crate::authz::HOST_SERVICE_PRINCIPAL, Access::Admin);
        let svc = ChangesService::new(
            ext.changes.get().unwrap().clone(),
            Arc::new(auth),
            ServerInfo::default(),
        );
        publish(&reg, &e, 1, c1, None).await;
        publish(&reg, &e, 2, c2, Some(1)).await;
        publish(&reg, &e, 3, c3, None).await;
        publish(&reg, &e, 4, c4, Some(2)).await;
        let f = Fx {
            reg,
            e,
            ext,
            svc,
            a,
            c: [c1, c2, c3, c4],
        };
        f.set_ref("refs/heads/master", a).await;
        f
    }

    fn msg(change: u8, now: bool) -> wire::RequestMergeRequest {
        wire::RequestMergeRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc as i32,
            }),
            change: cbytes(change),
            now,
        }
    }

    fn call(
        m: wire::RequestMergeRequest,
        token: &str,
        op: Option<Uuid>,
    ) -> Request<wire::RequestMergeRequest> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(token).unwrap()));
        if let Some(op) = op {
            r.metadata_mut()
                .insert("x-asc-op-id", op.to_string().parse().unwrap());
        }
        r
    }

    impl Fx {
        async fn run(
            &self,
            change: u8,
            now: bool,
            token: &str,
        ) -> Result<wire::RequestMergeResponse, Status> {
            self.svc
                .request_merge(call(msg(change, now), token, Some(Uuid::new_v4())))
                .await
                .map(tonic::Response::into_inner)
        }

        fn attach(&self, q: &Arc<FakeQueue>) {
            let q: Arc<dyn MergeQueue> = q.clone();
            attach_queue(&self.ext, q).unwrap();
        }

        async fn set_ref(&self, name: &str, cs: ChangesetId) {
            let txn = RefTxn {
                updates: vec![RefUpdate {
                    name: asc_core::path::RefName::new(name.as_bytes()).unwrap(),
                    expect: Expect::Any,
                    new: Some(RefTarget::Changeset(cs)),
                    expires_at: None,
                }],
                actor: Actor {
                    id: "t".into(),
                    via: "test".into(),
                },
                reason: "test".into(),
                op_id: Uuid::new_v4(),
            };
            self.reg.refs().update(self.e.key, &txn).await.unwrap();
        }

        async fn master(&self) -> Option<RefTarget> {
            self.reg
                .refs()
                .get(
                    self.e.key,
                    &asc_core::path::RefName::new(b"refs/heads/master").unwrap(),
                )
                .await
                .unwrap()
                .map(|e| e.target)
        }
    }

    fn outcome(state: MergeRequestState, pending: &[u8]) -> MergeOutcome {
        MergeOutcome {
            state,
            pending: pending.iter().map(|n| cid(*n)).collect(),
            reason: String::new(),
        }
    }

    fn state_of(r: &wire::RequestMergeResponse) -> MergeRequestState {
        MergeOutcome::try_from(r).unwrap().state
    }

    #[tokio::test]
    async fn without_a_forge_now_false_is_refused_and_nothing_moves() {
        let f = fx().await;
        let r = f.run(2, false, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Refused);
        assert!(!r.reason.is_empty());
        assert!(r.pending.is_empty());
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn without_a_forge_now_true_lands_the_prefix_atomically() {
        let f = fx().await;
        let r = f.run(2, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Landed);
        // Changes 1 and 2 sit on the trunk tip, so the land fast-forwards to change 2's head.
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.c[1])));
        // Asking again is idempotent: it is landed, and nothing else moves.
        let again = f.run(2, true, WRITER).await.unwrap();
        assert_eq!(state_of(&again), MergeRequestState::Landed);
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.c[1])));
    }

    #[tokio::test]
    async fn a_land_covers_only_bottom_to_the_requested_change() {
        let f = fx().await;
        let r = f.run(1, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Landed);
        // Change 2 sits above the request and stays open.
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.c[0])));
    }

    #[tokio::test]
    async fn a_stack_that_is_not_on_its_parent_is_refused_not_landed() {
        let f = fx().await;
        let r = f.run(4, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Refused);
        assert!(r.reason.contains("sync"));
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn an_unpublished_change_in_the_prefix_is_refused_not_landed() {
        let f = fx().await;
        // Change 5 has a changeset and a meta row but no published version.
        let c5 = commit(&f.e, &[f.a], &[("five", b"5\n")]).await;
        let c6 = commit(&f.e, &[c5], &[("six", b"6\n")]).await;
        let idx = index::ChangeIndex::new(f.reg.clone());
        let mut tx = f.reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            f.e.key,
            &ChangeMeta::new(c5, cid(5), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        publish(&f.reg, &f.e, 6, c6, Some(5)).await;
        let r = f.run(6, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Refused);
        assert!(r.reason.contains("no published version"), "{}", r.reason);
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
        // The unpublished change alone is refused the same way.
        let r = f.run(5, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Refused);
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn a_direct_land_needs_an_op_id() {
        let f = fx().await;
        let e = f
            .svc
            .request_merge(call(msg(1, true), WRITER, None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn a_reader_cannot_request_a_merge() {
        let f = fx().await;
        for now in [false, true] {
            let e = f.run(1, now, READER).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied);
        }
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn bad_requests_are_rejected() {
        let f = fx().await;
        let mut m = msg(1, true);
        m.change = bytes::Bytes::from_static(b"short");
        let e = f
            .svc
            .request_merge(call(m, WRITER, Some(Uuid::new_v4())))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let e = f.run(99, true, WRITER).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        // A repository the caller named wrongly.
        let mut m = msg(1, true);
        m.repo.as_mut().unwrap().name = "o/none".into();
        let e = f
            .svc
            .request_merge(call(m, WRITER, Some(Uuid::new_v4())))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_divergent_change_is_refused_in_both_modes() {
        let f = fx().await;
        // A second, unrelated changeset under change 3's id with no version link: divergent.
        let twin = commit(&f.e, &[f.a], &[("twin", b"t\n")]).await;
        let idx = index::ChangeIndex::new(f.reg.clone());
        let mut tx = f.reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            f.e.key,
            &ChangeMeta::new(twin, cid(3), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let q = FakeQueue::replying(Ok(outcome(MergeRequestState::Queued, &[])));
        let r = f.run(3, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Refused);
        assert!(r.reason.contains("divergent"));
        f.attach(&q);
        let r = f.run(3, false, MEMBER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Refused);
        assert!(
            q.asked().is_empty(),
            "a divergent change never reaches the forge"
        );
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn with_a_forge_the_request_is_forwarded_and_nothing_lands_here() {
        let f = fx().await;
        let q = FakeQueue::replying(Ok(outcome(MergeRequestState::Queued, &[1])));
        f.attach(&q);
        let op = Uuid::new_v4();
        let r = f
            .svc
            .request_merge(call(msg(2, true), MEMBER, Some(op)))
            .await
            .unwrap()
            .into_inner();
        let got = MergeOutcome::try_from(&r).unwrap();
        assert_eq!(got.state, MergeRequestState::Queued);
        assert_eq!(got.pending, vec![cid(1)]);
        // The forge was asked for bottom..change, as the authenticated user, with the op id.
        let asked = q.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].user_id, 7);
        assert_eq!(asked[0].user_name, "alice");
        assert_eq!(asked[0].change, cid(2));
        assert!(asked[0].now);
        let chain: Vec<ChangeId> = asked[0].prefix.iter().map(|p| p.change).collect();
        assert_eq!(chain, vec![cid(1), cid(2)]);
        assert!(asked[0].prefix.iter().all(|p| p.git.len() == 40));
        assert_eq!(*q.ops.lock().unwrap(), vec![Some(op)]);
        // Even with now = true the server did not land by itself.
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn with_a_forge_a_landed_parent_is_not_part_of_the_prefix() {
        let f = fx().await;
        // Change 1 lands (no forge yet), so it is the trunk's, not the stack's.
        let r = f.run(1, true, WRITER).await.unwrap();
        assert_eq!(state_of(&r), MergeRequestState::Landed);
        let q = FakeQueue::replying(Ok(outcome(MergeRequestState::Queued, &[])));
        f.attach(&q);
        f.run(2, false, MEMBER).await.unwrap();
        let chain: Vec<ChangeId> = q.asked()[0].prefix.iter().map(|p| p.change).collect();
        assert_eq!(chain, vec![cid(2)]);
    }

    #[tokio::test]
    async fn with_a_forge_only_a_forge_user_may_ask() {
        let f = fx().await;
        let q = FakeQueue::replying(Ok(outcome(MergeRequestState::Queued, &[])));
        f.attach(&q);
        // A token whose principal is not a forge user, and the forge's own principal.
        for token in [WRITER, FORGE] {
            let e = f.run(2, false, token).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied, "{token}");
        }
        assert!(q.asked().is_empty());
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn an_unreachable_forge_fails_closed_without_landing() {
        let f = fx().await;
        let q = FakeQueue::replying(Err(0));
        f.attach(&q);
        for now in [false, true] {
            let e = f.run(2, now, MEMBER).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::Unavailable);
        }
        assert_eq!(f.master().await, Some(RefTarget::Changeset(f.a)));
    }

    #[tokio::test]
    async fn the_forges_refusal_of_the_caller_is_relayed_cleaned() {
        let f = fx().await;
        let q = FakeQueue::replying(Err(1));
        f.attach(&q);
        let e = f.run(2, true, MEMBER).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        assert!(!e.message().contains('\u{1b}'));
        let q2 = FakeQueue::replying(Err(2));
        let f2 = fx().await;
        f2.attach(&q2);
        let e = f2.run(2, true, MEMBER).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Internal);
    }

    #[tokio::test]
    async fn an_answer_naming_a_change_outside_the_prefix_is_not_relayed() {
        // 3 is not in change 2's prefix; 2 is the requested change itself; 1 twice.
        for pending in [&[3u8][..], &[2], &[1, 1]] {
            let f = fx().await;
            let q = FakeQueue::replying(Ok(outcome(MergeRequestState::Queued, pending)));
            f.attach(&q);
            let e = f.run(2, false, MEMBER).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::Internal, "{pending:?}");
        }
    }

    #[tokio::test]
    async fn the_forges_reason_is_bounded_and_printable() {
        let f = fx().await;
        let mut o = outcome(MergeRequestState::Refused, &[]);
        o.reason = format!("no\u{1b}[0m{}", "x".repeat(MAX_REASON * 2));
        let q = FakeQueue::replying(Ok(o));
        f.attach(&q);
        let r = f.run(1, false, MEMBER).await.unwrap();
        assert!(r.reason.chars().count() <= MAX_REASON);
        assert!(!r.reason.contains('\u{1b}'));
    }

    #[test]
    fn the_queue_attaches_once() {
        let ext = Extensions::new();
        // Nothing serves RequestMerge yet: attaching is a no-op.
        let q: Arc<dyn MergeQueue> = FakeQueue::replying(Err(0));
        assert!(attach_queue(&ext, q).is_ok());
    }

    #[test]
    fn the_forge_body_has_the_documented_shape() {
        let ask = MergeAsk {
            user_id: 7,
            user_name: "alice".into(),
            change: cid(2),
            now: true,
            prefix: vec![
                PrefixEntry {
                    change: cid(1),
                    git: "a".repeat(40),
                },
                PrefixEntry {
                    change: cid(2),
                    git: "b".repeat(40),
                },
            ],
        };
        let v = forge_body(&ask);
        assert_eq!(v["user_id"], 7);
        assert_eq!(v["user_name"], "alice");
        assert_eq!(v["change"], cid(2).to_string());
        assert_eq!(v["now"], true);
        assert_eq!(v["prefix"][0]["change"], cid(1).to_string());
        assert_eq!(v["prefix"][1]["git"], "b".repeat(40));
        assert_eq!(v["prefix"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_forge_answer_is_parsed_strictly() {
        let c1 = cid(1).to_string();
        let ok = format!(r#"{{"state":"queued","pending":["{c1}"],"reason":"waiting\u001b"}}"#);
        let got = parse_forge_answer(ok.as_bytes()).unwrap();
        assert_eq!(got.state, MergeRequestState::Queued);
        assert_eq!(got.pending, vec![cid(1)]);
        assert!(!got.reason.contains('\u{1b}'));
        for bad in [
            r#"{"state":"maybe"}"#.to_owned(),
            r#"{}"#.to_owned(),
            "not json".to_owned(),
            r#"{"state":"queued","pending":["short"]}"#.to_owned(),
            format!(
                r#"{{"state":"queued","pending":[{}]}}"#,
                vec![format!("\"{c1}\""); MAX_PREFIX + 1].join(",")
            ),
        ] {
            assert!(
                matches!(parse_forge_answer(bad.as_bytes()), Err(QueueError::Invalid)),
                "{bad}"
            );
        }
    }
}
