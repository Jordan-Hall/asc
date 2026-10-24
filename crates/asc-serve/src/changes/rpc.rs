//! The `Changes` service: `ResolveChanges`, `Stack`, `Versions` and `SetReviewState`
//! (PLAN C19, SPEC §20.5).
//!
//! Every answer is read from the change index (`changes`, `change_versions`, `review_state`,
//! `mainline`) through indexed lookups; nothing scans and no message is parsed. The service shell
//! authorises every call before the handler runs (`Read`; `SetReviewState` needs `Admin` and then
//! the forge's internal principal, SPEC §20.5), bounds the request, and bounds the work: at most
//! [`MAX_REQUEST`] changes per call, [`MAX_STACK`] changes per stack, [`PAGE`] versions per page.
//! The other RPCs of the service belong to later units (C20 to C22, C50) and answer
//! `UNIMPLEMENTED` until they register.
//!
//! Choices where SPEC is silent:
//!
//! * `head` is the newest published version of the change, or the only visible changeset when
//!   nothing was published. `visible` lists the changesets of the change that no other changeset
//!   of it names as a predecessor, sorted; more than one means the change is divergent.
//! * `parent_current` is true when the head's first-parent chain starts at a head of the parent
//!   change (or the change has no parent).
//! * `Stack` runs from the bottom unlanded ancestor of the asked change through every descendant
//!   (forks included, depth first, parents before children, siblings by change id).
//!   `StackResponse.base` is the first parent of the bottom change's head (what the stack sits
//!   on). Looking a stack up by review number is the forge's mapping, so it is `UNIMPLEMENTED`.
//! * `SetReviewState` is an upsert that keeps the newer `updated`, so an out-of-order notifier
//!   event cannot roll a summary back. A change the index does not know is `NOT_FOUND`, and then
//!   nothing is written.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_core::path::RefName;
use asc_engine::history::HistoryApi;
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer, Credential};
use asc_rpc::changes::ReviewUpdates;
use asc_rpc::server::{CallCtx, ClientVersion, MAX_MESSAGE, MAX_STRING, RepoName, ServerInfo, wire};
use asc_rpc::services::changes_server::{Changes, ChangesServer};
use asc_rpc::{IdMode, RpcError};
use prost::Message;
use sqlx::{AnyConnection, Row};
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions as TonicExt, Request, Response, Status};

use super::index::{self, MAX_PREDS};
use super::{fixed, mergeable, overlaps, versions};
use crate::commit_graph_rpc::{commit_status, internal};
use crate::idmode::{Eng, IdCodec};
use crate::registry::{Extensions, Ns, Registry, RegistryError, RepoEntry};
use crate::update::PipelineHooks;

/// Changes and revisions one `ResolveChanges` call may name.
pub const MAX_REQUEST: usize = 256;
/// Changes in one stack (forks included).
pub const MAX_STACK: usize = 256;
/// Versions per `Versions` page.
pub const PAGE: usize = 200;
/// Review summaries one `SetReviewState` call may carry.
pub const MAX_REVIEW_UPDATES: usize = 1_000;
/// Longest accepted review summary, encoded.
const MAX_REVIEW_BYTES: usize = 16 * 1024;

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn index_status(e: index::Error) -> Status {
    internal("change index", &e)
}

fn sql_status(e: sqlx::Error) -> Status {
    internal("database", &e)
}

fn change_of(b: &[u8]) -> Result<ChangeId, Status> {
    <[u8; 16]>::try_from(b)
        .map(ChangeId::from_bytes)
        .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// The place the server's single `Changes` handler is registered, once (read by `build_routes`).
#[derive(Default)]
pub struct ChangesSlot(OnceLock<Arc<ChangesRpc>>);

impl ChangesSlot {
    /// Registers `rpc`; false when one is already registered.
    pub fn set(&self, rpc: Arc<ChangesRpc>) -> bool {
        self.0.set(rpc).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<ChangesRpc>> {
        self.0.get()
    }
}

/// The unit's one line in `register_all`: makes the handler available to `build_routes`.
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    let rpc = Arc::new(ChangesRpc {
        registry: registry.clone(),
        pipeline: ext.pipeline.clone(),
        interdiff: OnceLock::new(),
        restack: OnceLock::new(),
        request_merge: OnceLock::new(),
    });
    assert!(
        ext.changes.set(rpc),
        "the Changes handler is registered once"
    );
}

// ------------------------------------------------------------------------------ handler

/// What the index says about one change.
pub(super) struct Head {
    /// The newest published version, else the first visible changeset.
    pub(super) cs: ChangesetId,
    /// The newest version number, 0 when nothing was published.
    version: u32,
    pub(super) parent: Option<ChangeId>,
    /// Changesets of the change that no other one of it names as a predecessor.
    pub(super) visible: Vec<ChangesetId>,
}

/// The handler behind [`ChangesService`].
pub struct ChangesRpc {
    registry: Arc<Registry>,
    pipeline: Arc<PipelineHooks>,
    /// `InterDiff` (C20), set by [`super::interdiff::register`].
    pub(super) interdiff: OnceLock<Arc<super::interdiff::InterDiffer>>,
    /// `RestackChanges` (C22), set by [`super::restack::register`].
    pub(super) restack: OnceLock<Arc<super::restack::Restacker>>,
    /// `RequestMerge` (C50), set by [`super::request_merge::register`].
    pub(super) request_merge: OnceLock<Arc<super::request_merge::RequestMerger>>,
}

/// Reads the facts of changes for one call; remembers what it has read.
pub(super) struct Reader<'a> {
    pub(super) conn: &'a mut AnyConnection,
    pub(super) ids: &'a IdCodec,
    pub(super) engine: &'a Arc<Eng>,
    pub(super) ns: Ns,
    pub(super) land_ref: String,
    pub(super) heads: HashMap<ChangeId, Option<Arc<Head>>>,
}

impl Reader<'_> {
    pub(super) async fn head(&mut self, change: ChangeId) -> Result<Option<Arc<Head>>, Status> {
        if let Some(h) = self.heads.get(&change) {
            return Ok(h.clone());
        }
        let css = index::changesets_of(&mut *self.conn, self.ns, change, MAX_PREDS)
            .await
            .map_err(index_status)?;
        let latest = versions::latest(&mut *self.conn, self.ns, change)
            .await
            .map_err(|e| internal("versions", &e))?;
        let mut visible = if css.is_empty() {
            latest.iter().map(|l| l.cs).collect()
        } else {
            let rows = index::rows_of(&mut *self.conn, self.ns, &css)
                .await
                .map_err(index_status)?;
            let named: HashSet<ChangesetId> =
                rows.iter().flat_map(|r| r.preds.iter().copied()).collect();
            let heads: Vec<ChangesetId> =
                css.iter().copied().filter(|c| !named.contains(c)).collect();
            if heads.is_empty() { css } else { heads }
        };
        visible.sort();
        visible.dedup();
        let head = match (&latest, visible.first()) {
            (Some(l), _) => Some(Head {
                cs: l.cs,
                version: l.v,
                parent: l.parent_change,
                visible,
            }),
            (None, Some(&first)) => Some(Head {
                cs: first,
                version: 0,
                parent: None,
                visible,
            }),
            (None, None) => None,
        }
        .map(Arc::new);
        self.heads.insert(change, head.clone());
        Ok(head)
    }

    pub(super) async fn landed(&mut self, change: ChangeId) -> Result<Option<ChangesetId>, Status> {
        index::landed_as(&mut *self.conn, self.ns, change, &self.land_ref)
            .await
            .map_err(index_status)
    }

    pub(super) async fn review(
        &mut self,
        change: ChangeId,
    ) -> Result<Option<wire::ReviewState>, Status> {
        let row = sqlx::query("SELECT state FROM review_state WHERE ns = $1 AND change = $2")
            .bind(self.ns.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec())
            .fetch_optional(&mut *self.conn)
            .await
            .map_err(sql_status)?;
        let Some(row) = row else { return Ok(None) };
        let bytes: Vec<u8> = row.try_get("state").map_err(sql_status)?;
        match wire::ReviewState::decode(bytes.as_slice()) {
            Ok(r) => Ok(Some(r)),
            Err(e) => {
                // A damaged summary hides the review; the forge rewrites it on its next event.
                tracing::error!(error = %e, "damaged review_state row");
                Ok(None)
            }
        }
    }

    /// True when `head` still sits on the newest version of its parent change.
    pub(super) async fn parent_current(&mut self, head: &Head) -> Result<bool, Status> {
        Ok(match head.parent {
            None => true,
            Some(p) => match self.head(p).await? {
                None => false,
                Some(ph) => {
                    let cs = self
                        .engine
                        .get_changeset(head.cs)
                        .await
                        .map_err(commit_status)?;
                    cs.parents
                        .first()
                        .is_some_and(|f| ph.visible.contains(f) || *f == ph.cs)
                }
            },
        })
    }

    /// The `ChangeInfo` of `change`; `None` when the index does not know it.
    async fn info(&mut self, change: ChangeId) -> Result<Option<wire::ChangeInfo>, Status> {
        let Some(head) = self.head(change).await? else {
            return Ok(None);
        };
        let parent_current = self.parent_current(&head).await?;
        let landed = self.landed(change).await?;
        let review = self.review(change).await?;
        let git = self.ids.git_of(head.cs).await?;
        let mut visible = Vec::with_capacity(head.visible.len());
        for v in &head.visible {
            visible.push(self.ids.changeset_out(*v).await?);
        }
        Ok(Some(wire::ChangeInfo {
            change: Bytes::copy_from_slice(change.as_bytes()),
            head: self.ids.changeset_out(head.cs).await?,
            git: Bytes::copy_from_slice(git.as_bytes()),
            version: head.version,
            parent_change: head
                .parent
                .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                .unwrap_or_default(),
            parent_current,
            landed_as: match landed {
                Some(l) => self.ids.changeset_out(l).await?,
                None => Bytes::new(),
            },
            review,
            visible,
        }))
    }

    /// Changes whose newest version names `parent` as parent, sorted by id.
    pub(super) async fn children(&mut self, parent: ChangeId) -> Result<Vec<ChangeId>, Status> {
        let rows = sqlx::query(
            "SELECT DISTINCT change FROM change_versions WHERE ns = $1 AND parent_change = $2 \
             LIMIT $3",
        )
        .bind(self.ns.as_bytes().to_vec())
        .bind(parent.as_bytes().to_vec())
        .bind(i64::try_from(MAX_STACK + 1).unwrap_or(i64::MAX))
        .fetch_all(&mut *self.conn)
        .await
        .map_err(sql_status)?;
        let mut out = Vec::new();
        for r in &rows {
            let c = fixed::<16>(r, "change")
                .map(ChangeId::from_bytes)
                .ok_or_else(|| Status::data_loss("damaged change_versions row"))?;
            // Only the newest version counts: a change that moved elsewhere is not a child.
            if self
                .head(c)
                .await?
                .is_some_and(|h| h.parent == Some(parent))
            {
                out.push(c);
            }
        }
        out.sort();
        Ok(out)
    }
}

impl ChangesRpc {
    async fn open(&self, ctx: &CallCtx) -> Result<(Arc<RepoEntry>, IdCodec, Arc<Eng>), Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        Ok((entry, ids, engine))
    }

    /// `refs/heads/<trunk>` for `target` (the default branch when empty).
    fn land_ref(entry: &RepoEntry, target: &str) -> Result<(String, String), Status> {
        let name = if target.is_empty() {
            entry.default_branch.as_str()
        } else {
            target.strip_prefix("refs/heads/").unwrap_or(target)
        };
        let full = format!("refs/heads/{name}");
        if name.is_empty() || name.len() > MAX_STRING || RefName::new(full.as_bytes()).is_err() {
            return Err(Status::invalid_argument("invalid target branch"));
        }
        Ok((name.to_owned(), full))
    }

    async fn resolve_changes(
        &self,
        ctx: &CallCtx,
        req: wire::ResolveChangesRequest,
    ) -> Result<wire::ResolveChangesResponse, Status> {
        if req.changes.len() + req.revs.len() > MAX_REQUEST {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_REQUEST} changes and revisions per call"
            )));
        }
        let (entry, ids, engine) = self.open(ctx).await?;
        let mut wanted: Vec<ChangeId> = req
            .changes
            .iter()
            .map(|c| change_of(c))
            .collect::<Result<_, _>>()?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql_status)?;
        for spec in &req.revs {
            if spec.len() > MAX_STRING {
                return Err(Status::invalid_argument("revision is too long"));
            }
            let Some(cs) = ids
                .resolve(spec, &entry.default_branch, |e| {
                    Status::invalid_argument(e.to_string())
                })
                .await?
            else {
                continue;
            };
            let rows = index::rows_of(&mut conn, entry.ns, &[cs])
                .await
                .map_err(index_status)?;
            wanted.extend(rows.first().map(|r| r.change));
        }
        let (_, land_ref) = Self::land_ref(&entry, "")?;
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref,
            heads: HashMap::new(),
        };
        let mut seen = HashSet::new();
        let mut changes = Vec::new();
        for c in wanted {
            if seen.insert(c)
                && let Some(info) = reader.info(c).await?
            {
                changes.push(info);
            }
        }
        Ok(wire::ResolveChangesResponse { changes })
    }

    async fn stack(
        &self,
        ctx: &CallCtx,
        req: wire::StackRequest,
    ) -> Result<wire::StackResponse, Status> {
        use wire::stack_request::At;
        let (entry, ids, engine) = self.open(ctx).await?;
        let (trunk, land_ref) = Self::land_ref(&entry, &req.target)?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql_status)?;
        let anchor = match &req.at {
            Some(At::Change(c)) => change_of(c)?,
            Some(At::Rev(spec)) => {
                if spec.len() > MAX_STRING {
                    return Err(Status::invalid_argument("revision is too long"));
                }
                let cs = ids
                    .resolve(spec, &entry.default_branch, |e| {
                        Status::invalid_argument(e.to_string())
                    })
                    .await?
                    .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))?;
                index::rows_of(&mut conn, entry.ns, &[cs])
                    .await
                    .map_err(index_status)?
                    .first()
                    .map(|r| r.change)
                    .ok_or_else(|| Status::not_found("the revision is not a change"))?
            }
            Some(At::Review(_)) => {
                return Err(Status::unimplemented(
                    "a review number is the forge's mapping; ask by change or revision",
                ));
            }
            None => return Err(Status::invalid_argument("stack: say which change")),
        };
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref,
            heads: HashMap::new(),
        };
        if reader.head(anchor).await?.is_none() {
            return Err(Status::not_found("unknown change"));
        }
        // Down to the bottom unlanded ancestor.
        let mut seen = HashSet::from([anchor]);
        let mut root = anchor;
        loop {
            let head = reader.head(root).await?.expect("read above");
            let Some(parent) = head.parent else { break };
            if !seen.insert(parent) {
                return Err(Status::data_loss("the parent chain of a change loops"));
            }
            if seen.len() > MAX_STACK {
                return Err(Status::resource_exhausted(format!(
                    "a stack has at most {MAX_STACK} changes"
                )));
            }
            if reader.head(parent).await?.is_none() || reader.landed(parent).await?.is_some() {
                break;
            }
            root = parent;
        }
        // Up through every descendant: parents before children, siblings by id.
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        let mut todo = vec![root];
        while let Some(c) = todo.pop() {
            if !visited.insert(c) {
                continue;
            }
            if order.len() >= MAX_STACK {
                return Err(Status::resource_exhausted(format!(
                    "a stack has at most {MAX_STACK} changes"
                )));
            }
            order.push(c);
            let mut kids = reader.children(c).await?;
            kids.reverse();
            todo.extend(kids);
        }
        let mut changes = Vec::with_capacity(order.len());
        for c in &order {
            if let Some(info) = reader.info(*c).await? {
                changes.push(info);
            }
        }
        let bottom = reader.head(root).await?.expect("read above");
        let base = match engine
            .get_changeset(bottom.cs)
            .await
            .map_err(commit_status)?
            .parents
            .first()
        {
            Some(&p) => ids.changeset_out(p).await?,
            None => Bytes::new(),
        };
        Ok(wire::StackResponse {
            trunk,
            base,
            changes,
        })
    }

    async fn versions(
        &self,
        ctx: &CallCtx,
        req: wire::VersionsRequest,
    ) -> Result<wire::VersionsResponse, Status> {
        let change = change_of(&req.change)?;
        let after = if req.page_token.is_empty() {
            None
        } else if req.page_token.len() <= 10 {
            Some(
                req.page_token
                    .parse::<u32>()
                    .map_err(|_| Status::invalid_argument("malformed page token"))?,
            )
        } else {
            return Err(Status::invalid_argument("malformed page token"));
        };
        let (entry, ids, _) = self.open(ctx).await?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql_status)?;
        let list = versions::list(&mut conn, entry.ns, change, after, PAGE)
            .await
            .map_err(|e| internal("versions", &e))?;
        if list.is_empty() && after.is_none() {
            let known = index::changesets_of(&mut *conn, entry.ns, change, 1)
                .await
                .map_err(index_status)?;
            if known.is_empty() {
                return Err(Status::not_found("unknown change"));
            }
        }
        let mut out = Vec::with_capacity(list.len());
        for v in &list {
            let git = ids.git_of(v.cs).await?;
            out.push(wire::Version {
                v: v.v,
                cs: ids.changeset_out(v.cs).await?,
                git: Bytes::copy_from_slice(git.as_bytes()),
                parent_change: v
                    .parent_change
                    .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                    .unwrap_or_default(),
                at: v.at,
                actor: v.actor.clone(),
            });
        }
        let next_page_token = match list.last() {
            Some(l) if list.len() == PAGE => l.v.to_string(),
            _ => String::new(),
        };
        Ok(wire::VersionsResponse {
            versions: out,
            next_page_token,
        })
    }

    async fn mergeability(
        &self,
        ctx: &CallCtx,
        req: wire::MergeabilityRequest,
    ) -> Result<wire::MergeabilityResponse, Status> {
        if req.changes.is_empty() || req.changes.len() > mergeable::MAX_HEADS {
            return Err(Status::invalid_argument(format!(
                "ask for 1 to {} changes per call",
                mergeable::MAX_HEADS
            )));
        }
        let changes: Vec<ChangeId> = req
            .changes
            .iter()
            .map(|c| change_of(c))
            .collect::<Result<_, _>>()?;
        let (entry, ids, engine) = self.open(ctx).await?;
        let (_, land_ref) = Self::land_ref(&entry, &req.target)?;
        let tip = mergeable::trunk_tip(&self.registry, &entry, &land_ref).await?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql_status)?;
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref,
            heads: HashMap::new(),
        };
        let results = mergeable::mergeability(&mut reader, tip, &changes, now()).await?;
        Ok(wire::MergeabilityResponse { results })
    }

    async fn overlaps(
        &self,
        ctx: &CallCtx,
        req: wire::OverlapsRequest,
    ) -> Result<wire::OverlapsResponse, Status> {
        let (entry, ids, engine) = self.open(ctx).await?;
        let (_, land_ref) = Self::land_ref(&entry, &req.target)?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql_status)?;
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref,
            heads: HashMap::new(),
        };
        let overlaps = overlaps::overlaps(&mut reader, &req).await?;
        Ok(wire::OverlapsResponse { overlaps })
    }

    async fn set_review_state(
        &self,
        ctx: &CallCtx,
        req: wire::SetReviewStateRequest,
    ) -> Result<wire::SetReviewStateResponse, Status> {
        // Forgejo's internal token only (SPEC §20.5); `Admin` alone is not enough.
        if !self.pipeline.push_hooks().may_skip_pre(ctx) {
            return Err(Status::permission_denied(
                "SetReviewState needs the Forgejo internal token",
            ));
        }
        if req.changes.len() > MAX_REVIEW_UPDATES {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_REVIEW_UPDATES} review states per call"
            )));
        }
        let updates =
            ReviewUpdates::try_from(&req).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let stamp = now();
        let mut rows = Vec::with_capacity(updates.entries.len());
        for (change, state) in &updates.entries {
            let mut state = wire::ReviewState::from(state);
            if state.updated <= 0 {
                state.updated = stamp;
            }
            let bytes = state.encode_to_vec();
            if bytes.len() > MAX_REVIEW_BYTES {
                return Err(Status::invalid_argument("a review state is too large"));
            }
            rows.push((*change, bytes, state.updated));
        }
        // What each update does to the overlap index: an open request records the head's paths,
        // a closed or merged one forgets the change (SPEC §20.4).
        let engine = entry.backend.engine().clone();
        let mut plans = Vec::with_capacity(rows.len());
        for (change, state) in &updates.entries {
            let plan = match overlaps::lifecycle(&state.state) {
                overlaps::Lifecycle::Open => {
                    let mut conn = self.registry.pool().acquire().await.map_err(sql_status)?;
                    match overlaps::head_paths(&mut conn, &engine, entry.ns, *change).await? {
                        Some(paths) => overlaps::Plan::Record(paths),
                        None => overlaps::Plan::Keep,
                    }
                }
                overlaps::Lifecycle::Closed => overlaps::Plan::Close,
                overlaps::Lifecycle::Unknown => overlaps::Plan::Keep,
            };
            plans.push(plan);
        }
        let mut tx = self.registry.pool().begin().await.map_err(sql_status)?;
        let mut applied = Vec::with_capacity(rows.len());
        for (change, _, _) in &rows {
            let known = index::changesets_of(&mut *tx, entry.ns, *change, 1)
                .await
                .map_err(index_status)?;
            if known.is_empty() {
                return Err(Status::not_found(format!("unknown change {change}")));
            }
        }
        for ((change, bytes, updated), plan) in rows.into_iter().zip(plans) {
            let done = sqlx::query(
                "INSERT INTO review_state (ns, change, state, updated) VALUES ($1, $2, $3, $4)                  ON CONFLICT (ns, change) DO UPDATE SET state = excluded.state,                  updated = excluded.updated WHERE review_state.updated <= excluded.updated",
            )
            .bind(entry.ns.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec())
            .bind(bytes)
            .bind(updated)
            .execute(&mut *tx)
            .await
            .map_err(sql_status)?;
            // A stale update (an older `updated`) changed nothing and must not move the index.
            if done.rows_affected() == 0 {
                continue;
            }
            applied.push(change);
            match plan {
                overlaps::Plan::Record(paths) => {
                    overlaps::record(&mut tx, entry.ns, change, &paths)
                        .await
                        .map_err(sql_status)?;
                }
                overlaps::Plan::Close => {
                    overlaps::close(&mut tx, entry.ns, change)
                        .await
                        .map_err(sql_status)?;
                }
                overlaps::Plan::Keep => {}
            }
        }
        tx.commit().await.map_err(sql_status)?;
        // Stack watchers refresh the review state of the changes that really changed (C49).
        for change in applied {
            super::watch::emit(entry.ns, change, super::watch::StackEventKind::ReviewState);
        }
        Ok(wire::SetReviewStateResponse {})
    }
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Changes` tonic service.
pub struct ChangesService {
    rpc: Arc<ChangesRpc>,
    authorizer: Arc<dyn Authorizer>,
    info: ServerInfo,
}

impl ChangesService {
    pub fn new(rpc: Arc<ChangesRpc>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            rpc,
            authorizer,
            info,
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<ChangesServer<Self>, AuthInterceptor> {
        let server = ChangesServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Validates the repository, checks the client version and authorises `access`.
    async fn context(
        &self,
        meta: &MetadataMap,
        ext: &TonicExt,
        repo: Option<&wire::RepoRef>,
        access: Access,
    ) -> Result<CallCtx, Status> {
        let repo = repo.ok_or_else(|| Status::invalid_argument("missing repo"))?;
        let name = RepoName::parse(&repo.name)?;
        let id_mode =
            IdMode::from_wire(repo.id_mode).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let client = match meta.get("x-asc-client") {
            None => None,
            Some(v) => {
                let text = v
                    .to_str()
                    .map_err(|_| Status::invalid_argument("malformed x-asc-client"))?;
                ClientVersion::from_header(text)?
            }
        };
        if let (Some(min), Some(got)) = (self.info.min_client, client)
            && got < min
        {
            let mut info = asc_rpc::errors::ErrorInfo::default();
            info.paths.push(format!("minimum client version is {min}"));
            return Err(asc_rpc::server::rpc_status(&RpcError::ClientTooOld(info)));
        }
        let principal = self
            .authorizer
            .authorize_credential(ext.get::<Credential>(), name.as_str(), access)
            .await?;
        Ok(CallCtx {
            repo: name,
            id_mode,
            principal,
            op_id: None,
            client,
        })
    }
}

fn later(what: &str) -> Status {
    Status::unimplemented(format!("{what} is not available on this server yet"))
}

#[tonic::async_trait]
impl Changes for ChangesService {
    async fn resolve_changes(
        &self,
        request: Request<wire::ResolveChangesRequest>,
    ) -> Result<Response<wire::ResolveChangesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read)
            .await?;
        self.rpc.resolve_changes(&ctx, msg).await.map(Response::new)
    }

    async fn stack(
        &self,
        request: Request<wire::StackRequest>,
    ) -> Result<Response<wire::StackResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read)
            .await?;
        self.rpc.stack(&ctx, msg).await.map(Response::new)
    }

    async fn versions(
        &self,
        request: Request<wire::VersionsRequest>,
    ) -> Result<Response<wire::VersionsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read)
            .await?;
        self.rpc.versions(&ctx, msg).await.map(Response::new)
    }

    type InterDiffStream =
        Pin<Box<dyn futures::Stream<Item = Result<wire::DiffResponse, Status>> + Send + 'static>>;

    async fn inter_diff(
        &self,
        request: Request<wire::InterDiffRequest>,
    ) -> Result<Response<Self::InterDiffStream>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read)
            .await?;
        let Some(handler) = self.rpc.interdiff.get() else {
            return Err(later("InterDiff"));
        };
        handler.inter_diff(&ctx, msg).await.map(Response::new)
    }

    async fn mergeability(
        &self,
        request: Request<wire::MergeabilityRequest>,
    ) -> Result<Response<wire::MergeabilityResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read)
            .await?;
        self.rpc.mergeability(&ctx, msg).await.map(Response::new)
    }

    async fn overlaps(
        &self,
        request: Request<wire::OverlapsRequest>,
    ) -> Result<Response<wire::OverlapsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read)
            .await?;
        self.rpc.overlaps(&ctx, msg).await.map(Response::new)
    }

    async fn restack_changes(
        &self,
        request: Request<wire::RestackChangesRequest>,
    ) -> Result<Response<wire::RestackChangesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let mut ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write)
            .await?;
        ctx.op_id = super::restack::op_id_of(&meta)?;
        let Some(handler) = self.rpc.restack.get() else {
            return Err(later("RestackChanges"));
        };
        handler.restack_changes(&ctx, msg).await.map(Response::new)
    }

    async fn set_review_state(
        &self,
        request: Request<wire::SetReviewStateRequest>,
    ) -> Result<Response<wire::SetReviewStateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin)
            .await?;
        self.rpc
            .set_review_state(&ctx, msg)
            .await
            .map(Response::new)
    }

    async fn request_merge(
        &self,
        request: Request<wire::RequestMergeRequest>,
    ) -> Result<Response<wire::RequestMergeResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let mut ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write)
            .await?;
        ctx.op_id = super::request_merge::op_id_of(&meta)?;
        let Some(handler) = self.rpc.request_merge.get() else {
            return Err(later("RequestMerge"));
        };
        handler.request_merge(&ctx, msg).await.map(Response::new)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::change::{ChangeMeta, RewriteOp};
    use asc_rpc::auth::{BearerToken, StaticTokens};
    use asc_rpc::server::wire::RepoRef;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::update::PushHooks;

    const READER: &str = "reader-token";
    const ADMIN: &str = "admin-token";
    const FORGE: &str = "forge-token";

    struct Internal;
    impl PushHooks for Internal {
        fn may_skip_pre(&self, ctx: &CallCtx) -> bool {
            ctx.principal.id == crate::forgejo::INTERNAL_PRINCIPAL
        }
    }

    struct Fx {
        reg: Arc<Registry>,
        e: Arc<RepoEntry>,
        svc: ChangesService,
        css: Vec<ChangesetId>,
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    fn new_version(
        change: u8,
        cs: ChangesetId,
        parent: Option<u8>,
        at: i64,
    ) -> versions::NewVersion<'static> {
        versions::NewVersion {
            change: cid(change),
            cs,
            parent_change: parent.map(cid),
            at,
            actor: "alice",
            op_id: b"",
            force: false,
        }
    }

    /// Changes 1 <- 2 <- 3 and a fork 2 <- 4, one changeset each, all published as version 1.
    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        let a = commit(&e, &[], &[("a", b"a")]).await;
        let b = commit(&e, &[a], &[("b", b"b")]).await;
        let c = commit(&e, &[b], &[("c", b"c")]).await;
        let d = commit(&e, &[b], &[("d", b"d")]).await;
        let ext = Extensions::new();
        ext.pipeline
            .set_push_hooks(Arc::new(Internal))
            .ok()
            .unwrap();
        register(&ext, &reg);
        let auth = StaticTokens::new()
            .with_token(READER, "reader", Access::Read)
            .with_token(ADMIN, "admin", Access::Admin)
            .with_token(FORGE, crate::forgejo::INTERNAL_PRINCIPAL, Access::Admin);
        let svc = ChangesService::new(
            ext.changes.get().unwrap().clone(),
            Arc::new(auth),
            ServerInfo::default(),
        );
        let f = Fx {
            reg,
            e,
            svc,
            css: vec![a, b, c, d],
        };
        let idx = index::ChangeIndex::new(f.reg.clone());
        let mut tx = f.reg.refs().begin().await.unwrap();
        for (i, parent) in [(1u8, None), (2, Some(1u8)), (3, Some(2)), (4, Some(2))] {
            let cs = f.css[usize::from(i) - 1];
            idx.write_meta(
                &mut tx,
                f.e.key,
                &ChangeMeta::new(cs, cid(i), vec![], RewriteOp::New),
            )
            .await
            .unwrap();
            versions::append(
                &mut tx,
                f.e.ns,
                &new_version(i, cs, parent, 10 + i64::from(i)),
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        f
    }

    fn req<T>(m: T, token: &str) -> Request<T> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(token).unwrap()));
        r
    }

    fn repo_ref(mode: IdMode) -> Option<RepoRef> {
        Some(RepoRef {
            name: "o/r".into(),
            id_mode: mode.to_wire(),
        })
    }

    fn ids(v: &[wire::ChangeInfo]) -> Vec<u8> {
        v.iter().map(|c| c.change[0]).collect()
    }

    fn id_bytes(n: u8) -> Bytes {
        Bytes::copy_from_slice(cid(n).as_bytes())
    }

    fn stack_of(change: u8) -> wire::StackRequest {
        wire::StackRequest {
            repo: repo_ref(IdMode::Asc),
            target: String::new(),
            at: Some(wire::stack_request::At::Change(id_bytes(change))),
        }
    }

    fn resolve(
        mode: IdMode,
        changes: Vec<Bytes>,
        revs: Vec<String>,
    ) -> wire::ResolveChangesRequest {
        wire::ResolveChangesRequest {
            repo: repo_ref(mode),
            changes,
            revs,
        }
    }

    #[tokio::test]
    async fn resolve_changes_by_id_and_revision() {
        let f = fx().await;
        let got = f
            .svc
            .resolve_changes(req(
                resolve(
                    IdMode::Asc,
                    vec![id_bytes(3), id_bytes(9), id_bytes(3)],
                    vec![f.css[0].to_string()],
                ),
                READER,
            ))
            .await
            .unwrap()
            .into_inner();
        // The unknown change and the repeat are dropped; the revision names change 1.
        assert_eq!(ids(&got.changes), [3, 1]);
        let c3 = &got.changes[0];
        assert_eq!(c3.version, 1);
        assert_eq!(&c3.head[..], f.css[2].as_bytes());
        assert_eq!(&c3.parent_change[..], cid(2).as_bytes());
        assert!(c3.parent_current);
        assert_eq!(c3.visible.len(), 1);
        assert_eq!(c3.git.len(), 20);
        assert!(c3.landed_as.is_empty() && c3.review.is_none());
    }

    #[tokio::test]
    async fn resolve_changes_in_git_mode_speaks_git_ids() {
        let f = fx().await;
        let got = f
            .svc
            .resolve_changes(req(resolve(IdMode::Git, vec![id_bytes(1)], vec![]), READER))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(got.changes[0].head.len(), 20);
        assert_eq!(got.changes[0].head, got.changes[0].git);
    }

    #[tokio::test]
    async fn resolve_changes_is_bounded_and_validated() {
        let f = fx().await;
        let many = vec![id_bytes(1); MAX_REQUEST + 1];
        let e = f
            .svc
            .resolve_changes(req(resolve(IdMode::Asc, many, vec![]), READER))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let short = vec![Bytes::from_static(b"short")];
        let e = f
            .svc
            .resolve_changes(req(resolve(IdMode::Asc, short, vec![]), READER))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn every_call_is_authorised() {
        let f = fx().await;
        let msg = resolve(IdMode::Asc, vec![], vec![]);
        let e = f
            .svc
            .resolve_changes(Request::new(msg.clone()))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
        let e = f
            .svc
            .resolve_changes(req(msg, "wrong-token"))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
        let e = f
            .svc
            .stack(req(
                wire::StackRequest {
                    repo: None,
                    ..stack_of(1)
                },
                READER,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let set = wire::SetReviewStateRequest {
            repo: repo_ref(IdMode::Asc),
            ..Default::default()
        };
        // A read token may not write; an admin token without the forge's identity may not either.
        let e = f
            .svc
            .set_review_state(req(set.clone(), READER))
            .await
            .unwrap_err();
        assert_ne!(e.code(), tonic::Code::Ok);
        let e = f.svc.set_review_state(req(set, ADMIN)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn stack_runs_bottom_to_top_with_forks_from_any_member() {
        let f = fx().await;
        for from in [1, 2, 3, 4] {
            let got = f
                .svc
                .stack(req(stack_of(from), READER))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(ids(&got.changes), [1, 2, 3, 4], "from {from}");
            assert_eq!(got.trunk, "master");
            assert!(got.base.is_empty(), "change 1 is a root");
        }
        let by_rev = f
            .svc
            .stack(req(
                wire::StackRequest {
                    repo: repo_ref(IdMode::Asc),
                    target: "refs/heads/master".into(),
                    at: Some(wire::stack_request::At::Rev(f.css[3].to_string())),
                },
                READER,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ids(&by_rev.changes), [1, 2, 3, 4]);
        let e = f.svc.stack(req(stack_of(9), READER)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let e = f
            .svc
            .stack(req(
                wire::StackRequest {
                    target: "bad\nname".into(),
                    ..stack_of(1)
                },
                READER,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let e = f
            .svc
            .stack(req(
                wire::StackRequest {
                    at: Some(wire::stack_request::At::Review(7)),
                    ..stack_of(1)
                },
                READER,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn a_landed_parent_leaves_the_stack_and_a_moved_child_is_not_a_fork() {
        let f = fx().await;
        // Change 1 landed on master.
        sqlx::query(
            "INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, 'refs/heads/master', 0, $2)",
        )
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(f.css[0].as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
        let got = f
            .svc
            .stack(req(stack_of(3), READER))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ids(&got.changes), [2, 3, 4]);
        assert_eq!(&got.base[..], f.css[0].as_bytes());
        let one = f
            .svc
            .resolve_changes(req(resolve(IdMode::Asc, vec![id_bytes(1)], vec![]), READER))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(&one.changes[0].landed_as[..], f.css[0].as_bytes());
        // Change 4 is re-published on top of change 3: it is no longer a fork of 2.
        let idx = index::ChangeIndex::new(f.reg.clone());
        let d2 = commit(&f.e, &[f.css[2]], &[("d2", b"d2")]).await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            f.e.key,
            &ChangeMeta::new(d2, cid(4), vec![f.css[3]], RewriteOp::Restack),
        )
        .await
        .unwrap();
        versions::append(&mut tx, f.e.ns, &new_version(4, d2, Some(3), 99))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let got = f
            .svc
            .stack(req(stack_of(2), READER))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ids(&got.changes), [2, 3, 4]);
        let four = got.changes.last().unwrap();
        assert_eq!((four.version, four.parent_current), (2, true));
        assert_eq!(&four.parent_change[..], cid(3).as_bytes());
    }

    #[tokio::test]
    async fn a_parent_cycle_is_refused() {
        let f = fx().await;
        // Damaged data: change 1 claims change 3 as parent.
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
             VALUES ($1, $2, 2, $3, $4, 1, 'x', $5)",
        )
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(cid(1).as_bytes().to_vec())
        .bind(f.css[0].as_bytes().to_vec())
        .bind(cid(3).as_bytes().to_vec())
        .bind(Vec::<u8>::new())
        .execute(f.reg.pool())
        .await
        .unwrap();
        let e = f.svc.stack(req(stack_of(2), READER)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::DataLoss);
    }

    #[tokio::test]
    async fn divergence_and_stale_parents_show_in_the_info() {
        let f = fx().await;
        // Two unrelated rewrites of change 3: divergent.
        let idx = index::ChangeIndex::new(f.reg.clone());
        let x = commit(&f.e, &[f.css[1]], &[("x", b"x")]).await;
        let y = commit(&f.e, &[f.css[1]], &[("y", b"y")]).await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        for cs in [x, y] {
            idx.write_meta(
                &mut tx,
                f.e.key,
                &ChangeMeta::new(cs, cid(3), vec![f.css[2]], RewriteOp::Amend),
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        // Change 2 moves to a new version; change 3's head still sits on the old one.
        let b2 = commit(&f.e, &[f.css[0]], &[("b2", b"b2")]).await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            f.e.key,
            &ChangeMeta::new(b2, cid(2), vec![f.css[1]], RewriteOp::Amend),
        )
        .await
        .unwrap();
        versions::append(&mut tx, f.e.ns, &new_version(2, b2, Some(1), 50))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let got = f
            .svc
            .stack(req(stack_of(3), READER))
            .await
            .unwrap()
            .into_inner();
        let three = got.changes.iter().find(|c| c.change[0] == 3).unwrap();
        assert_eq!(three.visible.len(), 2, "divergent");
        assert!(!three.parent_current, "change 2 moved on");
        assert_eq!(three.version, 1);
        let two = got.changes.iter().find(|c| c.change[0] == 2).unwrap();
        assert_eq!((two.version, two.parent_current), (2, true));
    }

    #[tokio::test]
    async fn versions_are_paged_and_resolved_in_the_asked_mode() {
        let f = fx().await;
        let idx = index::ChangeIndex::new(f.reg.clone());
        let mut prev = f.css[2];
        let mut tx = f.reg.refs().begin().await.unwrap();
        for n in 0..PAGE + 5 {
            let next = commit(&f.e, &[f.css[1]], &[("v", n.to_string().as_bytes())]).await;
            idx.write_meta(
                &mut tx,
                f.e.key,
                &ChangeMeta::new(next, cid(3), vec![prev], RewriteOp::Amend),
            )
            .await
            .unwrap();
            versions::append(&mut tx, f.e.ns, &new_version(3, next, Some(2), 100))
                .await
                .unwrap();
            prev = next;
        }
        tx.commit().await.unwrap();
        let ask = |mode, token: &str| wire::VersionsRequest {
            repo: repo_ref(mode),
            change: id_bytes(3),
            page_token: token.to_owned(),
        };
        let p1 = f
            .svc
            .versions(req(ask(IdMode::Asc, ""), READER))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(p1.versions.len(), PAGE);
        assert_eq!(p1.versions[0].v, 1);
        assert_eq!(&p1.versions[0].cs[..], f.css[2].as_bytes());
        assert_eq!(&p1.versions[0].parent_change[..], cid(2).as_bytes());
        assert_eq!(
            (p1.versions[0].at, p1.versions[0].actor.as_str()),
            (13, "alice")
        );
        assert_eq!(p1.next_page_token, PAGE.to_string());
        let p2 = f
            .svc
            .versions(req(ask(IdMode::Git, &p1.next_page_token), READER))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(p2.versions.len(), 6);
        assert_eq!(p2.versions[0].v as usize, PAGE + 1);
        assert!(p2.next_page_token.is_empty());
        assert_eq!(p2.versions[0].cs.len(), 20, "git mode");
        for bad in ["x", "-1", "99999999999"] {
            let e = f
                .svc
                .versions(req(ask(IdMode::Asc, bad), READER))
                .await
                .unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{bad}");
        }
        let e = f
            .svc
            .versions(req(
                wire::VersionsRequest {
                    change: id_bytes(9),
                    ..ask(IdMode::Asc, "")
                },
                READER,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
    }

    fn state(number: u64, st: &str, updated: i64) -> wire::ReviewState {
        wire::ReviewState {
            forge: "forgejo".into(),
            number,
            url: format!("https://forge.example/o/r/pulls/{number}"),
            state: st.into(),
            approvals: 1,
            updated,
            ..Default::default()
        }
    }

    async fn set(f: &Fx, changes: &[u8], states: Vec<wire::ReviewState>) -> Result<(), Status> {
        f.svc
            .set_review_state(req(
                wire::SetReviewStateRequest {
                    repo: repo_ref(IdMode::Asc),
                    changes: changes.iter().map(|c| id_bytes(*c)).collect(),
                    states,
                },
                FORGE,
            ))
            .await
            .map(|_| ())
    }

    async fn review_of(f: &Fx, change: u8) -> Option<wire::ReviewState> {
        f.svc
            .resolve_changes(req(
                resolve(IdMode::Asc, vec![id_bytes(change)], vec![]),
                READER,
            ))
            .await
            .unwrap()
            .into_inner()
            .changes
            .remove(0)
            .review
    }

    #[tokio::test]
    async fn set_review_state_upserts_and_keeps_the_newest() {
        let f = fx().await;
        set(
            &f,
            &[1, 2],
            vec![state(7, "open", 100), state(8, "open", 100)],
        )
        .await
        .unwrap();
        assert_eq!(review_of(&f, 1).await.unwrap().number, 7);
        assert_eq!(review_of(&f, 3).await, None);
        set(&f, &[1], vec![state(7, "merged", 200)]).await.unwrap();
        assert_eq!(review_of(&f, 1).await.unwrap().state, "merged");
        // A late, older event does not roll the summary back.
        set(&f, &[1], vec![state(7, "open", 150)]).await.unwrap();
        assert_eq!(review_of(&f, 1).await.unwrap().state, "merged");
        // No stamp: the server clock is used.
        set(&f, &[3], vec![state(9, "open", 0)]).await.unwrap();
        assert!(review_of(&f, 3).await.unwrap().updated > 0);
    }

    #[tokio::test]
    async fn set_review_state_feeds_stack_watchers_only_for_applied_updates() {
        use super::super::watch::{StackEventKind, peek};
        let f = fx().await;
        let kinds = |n: u8| {
            peek(f.e.ns, 0)
                .events
                .iter()
                .filter(|e| e.change == cid(n))
                .map(|e| e.kind)
                .collect::<Vec<_>>()
        };
        // A rejected call (one unknown change) emits nothing.
        set(&f, &[1, 9], vec![state(1, "open", 1), state(2, "open", 1)])
            .await
            .unwrap_err();
        assert!(peek(f.e.ns, 0).events.is_empty());
        set(&f, &[1], vec![state(7, "merged", 200)]).await.unwrap();
        assert_eq!(kinds(1), vec![StackEventKind::ReviewState]);
        // A stale update changed nothing, so it is not an event either.
        set(&f, &[1], vec![state(7, "open", 150)]).await.unwrap();
        assert_eq!(kinds(1).len(), 1);
    }

    #[tokio::test]
    async fn set_review_state_validates_before_writing() {
        let f = fx().await;
        // One unknown change: nothing is written.
        let e = set(&f, &[1, 9], vec![state(1, "open", 1), state(2, "open", 1)])
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        assert_eq!(review_of(&f, 1).await, None);
        // Parallel lists must match.
        let e = set(&f, &[1, 2], vec![state(1, "open", 1)])
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // Oversized text.
        let mut big = state(1, "open", 1);
        big.url = "u".repeat(10_000);
        let e = set(&f, &[1], vec![big]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // Too many in one call.
        let n = MAX_REVIEW_UPDATES + 1;
        let e = set(&f, &vec![1; n], vec![state(1, "open", 1); n])
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn later_rpcs_answer_unimplemented() {
        let f = fx().await;
        let e = f
            .svc
            .request_merge(Request::new(wire::RequestMergeRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
    }
}
