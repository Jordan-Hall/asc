//! The `Cloud` service: `GetWorkspace`, `UpdateWorkspace` and `ListWorkspaces` (SPEC §21.3.5,
//! `Purge`; PLAN B09, B10).
//!
//! A workspace is `(ns, owner, name)`; `owner` is the provider user id taken from the verified
//! identity ([`refpolicy::owner_of`]), never from the request. `UpdateWorkspace` is one
//! transaction: compare-and-swap of `cloud_ws.version`, head rows, the `heads` counter, the
//! workspace claim in `cloud_usage` and the `op_results` row. A retried `x-asc-op-id` returns the
//! stored version. Every authorisation decision is made before the handler runs.
//!
//! Choices where SPEC is silent:
//!
//! * An empty workspace name means `default`. A workspace that does not exist reads as version 0
//!   with no heads; `UpdateWorkspace` creates it at `base_version` 0 (version 1 after).
//! * `GetWorkspace(since_version = v)` with `v` equal to the current version answers with the
//!   version and no heads (nothing changed). Otherwise it returns the full set, ordered by
//!   changeset id, so `removed` stays empty (the client infers removal from absence against its
//!   `cloud_base`). The server keeps no tombstones.
//! * `owner` in a request is a decimal user id, or the caller's own login. Reading another
//!   user's workspace needs repo write (SPEC §21.2.5). Logins of other users are not resolved
//!   here (`INVALID_ARGUMENT`).
//! * `UpdateWorkspace` needs the cloud flag of the repository (`repos.cloud`) and a user
//!   identity; the added heads must already be indexed (ingested with a valid `m/` object). The
//!   server stamps `added`; the client's `added`, `hidden_at` and `change` are ignored.
//! * `cloud_ws` has one column beyond SPEC §21.2.3, `heads`, the count of its head rows.
//! * `ReportDevice` (SPEC §21.3.9) replaces every `cloud_device_drafts` row of one device in one
//!   transaction, so a retry is the same report and needs no op id. It keeps [`MAX_DEVICES`] devices
//!   per workspace (the least recently reported goes) and drops reports older than
//!   [`DEVICE_TTL_S`]. `GetWorkspace(include_devices)` names the devices of each head and, on its
//!   first page, lists the reported drafts that are not heads of the workspace; it always answers
//!   in full, as a report does not move the workspace version.
//! * A call the hub makes for a signed-in person with its forwarded claim acts as that person
//!   ([`acting::act_as`]): the hub hides and unhides the person's drafts from its Drafts page. The
//!   claim is the whole of its authority: another user's workspace needs a `write` claim and Purge
//!   as an administrator an `admin` claim; the hub's own credential is not asked again.

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer, Credential};
use asc_rpc::errors::ErrorInfo;
use asc_rpc::server::{
    CallCtx, ClientVersion, MAX_LIST, MAX_MESSAGE, RepoName, ServerInfo, rpc_status, wire,
};
use asc_rpc::services::cloud_server::{Cloud, CloudServer};
use asc_rpc::{IdMode, RpcError};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use sqlx::{AnyConnection, Row};
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions as TonicExt, Request, Response, Status};
use uuid::Uuid;

use super::Limits;
use super::expiry::CloudRoots;
use super::purge::{CacheEvict, CacheTiers, Purger, Who};
use super::quota::Quota;
use crate::authz::UserIdentity;
use crate::changes::index;
use crate::commit_graph_rpc::{internal, registry_status};
use crate::idmode::IdCodec;
use crate::refpolicy;
use crate::registry::{Extensions, Registry, RepoEntry};
use crate::review::acting;

/// Heads in one `GetWorkspace` page.
pub const PAGE: usize = 1_000;
/// Workspaces in one `ListWorkspaces` page, at most.
pub const MAX_LIST_PAGE: usize = 200;
/// Longest workspace name.
pub const MAX_NAME: usize = 64;
/// The name used when a request leaves it empty.
pub const DEFAULT_NAME: &str = "default";
/// Drafts in one `ReportDevice`, at most.
pub const MAX_REPORT: usize = 1_000;
/// Devices whose reports a workspace keeps.
pub const MAX_DEVICES: i64 = 8;
/// Age after which a device report is dropped: 30 days.
pub const DEVICE_TTL_S: i64 = 30 * 86_400;

/// Marks an `op_results` row as the result of `UpdateWorkspace`: the tag, then the version.
const OP_TAG: &[u8; 3] = b"cw1";
/// Tag of a `GetWorkspace` page token: the tag, then the last changeset id of the page.
const PAGE_TAG: &[u8; 2] = b"g1";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn sql(e: sqlx::Error) -> Status {
    internal("database", &e)
}

fn heads_limit() -> Status {
    rpc_status(&RpcError::CloudHeadsLimit(ErrorInfo::default()))
}

/// Validates a workspace name: `[A-Za-z0-9._-]{1,64}`; empty means [`DEFAULT_NAME`].
pub fn workspace_name(raw: &str) -> Result<&str, Status> {
    if raw.is_empty() {
        return Ok(DEFAULT_NAME);
    }
    let ok = raw.len() <= MAX_NAME
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(raw)
    } else {
        Err(Status::invalid_argument("invalid workspace name"))
    }
}

/// Which user a request names. `Ok(None)` is the caller (and the caller has no user identity).
/// A device name: `[A-Za-z0-9._-]{1,64}`.
pub fn device_name(raw: &str) -> Result<&str, Status> {
    let ok = !raw.is_empty()
        && raw.len() <= MAX_NAME
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(raw)
    } else {
        Err(Status::invalid_argument("invalid device name"))
    }
}

pub fn parse_owner(principal_id: &str, raw: &str) -> Result<Option<i64>, Status> {
    let me = UserIdentity::parse(principal_id).filter(UserIdentity::is_user);
    if raw.is_empty() {
        return me
            .map(|i| Some(i.user_id))
            .ok_or_else(|| Status::permission_denied("the commit cloud needs a user identity"));
    }
    if raw.len() <= 19 && raw.bytes().all(|b| b.is_ascii_digit()) {
        return match raw.parse::<i64>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(Status::invalid_argument("invalid owner")),
        };
    }
    match me {
        Some(i) if i.handle == raw => Ok(Some(i.user_id)),
        _ => Err(Status::invalid_argument(
            "owner must be a numeric user id or your own login",
        )),
    }
}

fn conflict(current: u64) -> Status {
    rpc_status(&asc_rpc::cloud::version_conflict(current))
}

fn page_token(cs: ChangesetId) -> String {
    let mut b = PAGE_TAG.to_vec();
    b.extend_from_slice(cs.as_bytes());
    URL_SAFE_NO_PAD.encode(b)
}

fn parse_page_token(t: &str) -> Result<Option<Vec<u8>>, Status> {
    if t.is_empty() {
        return Ok(None);
    }
    let bad = || Status::invalid_argument("bad page token");
    if t.len() > 128 {
        return Err(bad());
    }
    let b = URL_SAFE_NO_PAD.decode(t).map_err(|_| bad())?;
    match b.strip_prefix(PAGE_TAG.as_slice()) {
        Some(cs) if cs.len() == 32 => Ok(Some(cs.to_vec())),
        _ => Err(bad()),
    }
}

// ------------------------------------------------------------------------------ handler

/// The handler behind [`CloudService`], and the owner of the quota ledger other write paths
/// charge ([`CloudRpc::quota`]).
pub struct CloudRpc {
    registry: Arc<Registry>,
    quota: Quota,
    purger: Purger,
}

/// The place the server's single `Cloud` handler is registered, once (read by `build_routes`).
#[derive(Default)]
pub struct CloudSlot(OnceLock<Arc<CloudRpc>>);

impl CloudSlot {
    /// Registers `rpc`; false when one is already registered.
    pub fn set(&self, rpc: Arc<CloudRpc>) -> bool {
        self.0.set(rpc).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<CloudRpc>> {
        self.0.get()
    }
}

/// The unit's one line in `register_all`.
pub fn register(ext: &Extensions, registry: &Arc<Registry>, limits: Limits) {
    let rpc = Arc::new(CloudRpc::new(registry.clone(), Quota::local(limits)));
    // Cloud heads are GC roots and purged drafts are swept early (B10).
    ext.pipeline
        .gc_roots()
        .register(Arc::new(CloudRoots::new(registry.pool().clone())));
    assert!(ext.cloud.set(rpc), "the Cloud handler is registered once");
}

enum Outcome {
    Done(u64),
    /// The base version did not match; carries nothing, the caller reads the current one.
    Stale,
}

impl CloudRpc {
    pub fn new(registry: Arc<Registry>, quota: Quota) -> Self {
        let purger = Purger::new(registry.clone(), Arc::new(CacheTiers::default()));
        Self {
            registry,
            quota,
            purger,
        }
    }

    /// The cache tiers `Purge` evicts from; register one per configured cache.
    pub fn cache_tiers(&self) -> &Arc<CacheTiers> {
        self.purger.tiers()
    }

    /// The `cache_evict` outbox effect, for `Outbox::effects().register`.
    pub fn cache_evict_effect(&self) -> Arc<CacheEvict> {
        Arc::new(CacheEvict::new(self.purger.tiers().clone()))
    }

    /// `Cloud.Purge` (SPEC §21.3.7): `owner` is the caller's verified user id, `admin` whether
    /// the credential has repo admin. The purger decides what each of them may purge.
    pub async fn purge(
        &self,
        ctx: &CallCtx,
        owner: Option<i64>,
        admin: bool,
        req: wire::PurgeRequest,
    ) -> Result<wire::PurgeResponse, Status> {
        let (entry, ids) = self.open(ctx).await?;
        if req.cs.len() > super::purge::MAX_PURGE_CS {
            return Err(Status::invalid_argument(format!(
                "name at most {} changesets",
                super::purge::MAX_PURGE_CS
            )));
        }
        let css = Self::ids_in(&ids, &req.cs).await?;
        let done = self
            .purger
            .purge(&entry, Who { owner, admin }, &css, now())
            .await?;
        let mut refused = Vec::with_capacity(done.refused.len());
        for cs in done.refused {
            refused.push(ids.changeset_out(cs).await?);
        }
        Ok(wire::PurgeResponse {
            purged: u32::try_from(done.purged.len()).unwrap_or(u32::MAX),
            refused,
            unacked_tiers: done.unacked_tiers,
        })
    }

    /// The quota ledger, for `PutObjects`, `Ingest` and `ImportGitPush`.
    pub fn quota(&self) -> &Quota {
        &self.quota
    }

    async fn open(&self, ctx: &CallCtx) -> Result<(Arc<RepoEntry>, IdCodec), Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry);
        Ok((entry, ids))
    }

    async fn cloud_enabled(&self, entry: &RepoEntry) -> Result<bool, Status> {
        let row = sqlx::query(
            "SELECT CAST(CASE WHEN cloud THEN 1 ELSE 0 END AS BIGINT) AS c FROM repos WHERE id = $1",
        )
        .bind(entry.key.0)
        .fetch_one(self.registry.pool())
        .await
        .map_err(sql)?;
        Ok(row.try_get::<i64, _>("c").map_err(sql)? != 0)
    }

    /// The workspace row: `(version, heads)`.
    async fn row(
        conn: &mut AnyConnection,
        entry: &RepoEntry,
        owner: i64,
        name: &str,
    ) -> Result<Option<(u64, i64)>, Status> {
        let r = sqlx::query(
            "SELECT version, heads FROM cloud_ws WHERE ns = $1 AND owner = $2 AND name = $3",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(owner)
        .bind(name)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sql)?;
        let Some(r) = r else { return Ok(None) };
        let v: i64 = r.try_get("version").map_err(sql)?;
        let h: i64 = r.try_get("heads").map_err(sql)?;
        Ok(Some((u64::try_from(v).unwrap_or(0), h)))
    }

    pub async fn get_workspace(
        &self,
        ctx: &CallCtx,
        owner: i64,
        req: wire::GetWorkspaceRequest,
    ) -> Result<wire::GetWorkspaceResponse, Status> {
        let name = workspace_name(&req.name)?;
        if req.max_age_s < 0 {
            return Err(Status::invalid_argument("max_age_s is negative"));
        }
        let after = parse_page_token(&req.page_token)?;
        let (entry, ids) = self.open(ctx).await?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let devices = if req.include_devices {
            Some(Self::device_rows(&mut conn, &entry, owner, name).await?)
        } else {
            None
        };
        let Some((version, _)) = Self::row(&mut conn, &entry, owner, name).await? else {
            // No workspace yet: every reported draft is local.
            return Ok(wire::GetWorkspaceResponse {
                local: match (&devices, &after) {
                    (Some(d), None) => d.iter().map(DeviceRow::local).collect(),
                    _ => Vec::new(),
                },
                ..Default::default()
            });
        };
        if req.since_version == version && after.is_none() && devices.is_none() {
            return Ok(wire::GetWorkspaceResponse {
                version,
                ..Default::default()
            });
        }
        let cutoff = (req.max_age_s > 0).then(|| now().saturating_sub(req.max_age_s));
        let mut filter = String::from("ns = $1 AND owner = $2 AND name = $3");
        let mut next = 4;
        if after.is_some() {
            filter += &format!(" AND cs > ${next}");
            next += 1;
        }
        if !req.include_hidden {
            filter += " AND hidden_at IS NULL";
        }
        let age_filter = cutoff.map(|_| {
            let f = format!(" AND added >= ${next}");
            next += 1;
            f
        });
        let sql_text = format!(
            "SELECT cs, change, labels, added, CAST(hidden_at AS BIGINT) AS hidden_at \
             FROM cloud_heads WHERE {filter}{} ORDER BY cs LIMIT ${next}",
            age_filter.as_deref().unwrap_or("")
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_text))
            .bind(entry.ns.as_bytes().to_vec())
            .bind(owner)
            .bind(name.to_owned());
        if let Some(a) = &after {
            q = q.bind(a.clone());
        }
        if let Some(c) = cutoff {
            q = q.bind(c);
        }
        let rows = q
            .bind(i64::try_from(PAGE + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sql)?;
        let more = rows.len() > PAGE;
        let mut heads = Vec::with_capacity(rows.len().min(PAGE));
        let mut last = None;
        for r in rows.iter().take(PAGE) {
            let cs: Vec<u8> = r.try_get("cs").map_err(sql)?;
            let change: Vec<u8> = r.try_get("change").map_err(sql)?;
            let labels: String = r.try_get("labels").map_err(sql)?;
            let cs_id = ChangesetId::from_bytes(
                <[u8; 32]>::try_from(cs.as_slice())
                    .map_err(|_| internal("cloud_heads", &"cs column"))?,
            );
            last = Some(cs_id);
            heads.push(wire::CloudHead {
                cs: ids.changeset_out(cs_id).await?,
                change: Bytes::from(change),
                labels: serde_json::from_str(&labels)
                    .map_err(|e| internal("cloud_heads labels", &e))?,
                added: r.try_get("added").map_err(sql)?,
                hidden_at: r
                    .try_get::<Option<i64>, _>("hidden_at")
                    .map_err(sql)?
                    .unwrap_or(0),
                devices: devices
                    .iter()
                    .flatten()
                    .filter(|d| d.cs == cs)
                    .map(|d| d.device.clone())
                    .collect(),
            });
        }
        let local = match (&devices, &after) {
            (Some(d), None) => {
                let heads = Self::head_set(&mut conn, &entry, owner, name).await?;
                d.iter()
                    .filter(|d| !heads.contains(&d.cs))
                    .map(DeviceRow::local)
                    .collect()
            }
            _ => Vec::new(),
        };
        let omitted = match (cutoff, &after) {
            (Some(c), None) => {
                let hidden = if req.include_hidden {
                    ""
                } else {
                    " AND hidden_at IS NULL"
                };
                let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT COUNT(*) FROM cloud_heads \
                     WHERE ns = $1 AND owner = $2 AND name = $3 AND added < $4{hidden}"
                )))
                .bind(entry.ns.as_bytes().to_vec())
                .bind(owner)
                .bind(name.to_owned())
                .bind(c)
                .fetch_one(&mut *conn)
                .await
                .map_err(sql)?;
                u32::try_from(n).unwrap_or(u32::MAX)
            }
            _ => 0,
        };
        Ok(wire::GetWorkspaceResponse {
            version,
            heads,
            removed: Vec::new(),
            omitted,
            next_page_token: match (more, last) {
                (true, Some(cs)) => page_token(cs),
                _ => String::new(),
            },
            local,
        })
    }

    /// The live device reports of a workspace, newest draft first.
    async fn device_rows(
        conn: &mut AnyConnection,
        entry: &RepoEntry,
        owner: i64,
        name: &str,
    ) -> Result<Vec<DeviceRow>, Status> {
        let rows = sqlx::query(
            "SELECT device, cs, change, added, state, reported_at FROM cloud_device_drafts \
             WHERE ns = $1 AND owner = $2 AND name = $3 AND reported_at >= $4 \
             ORDER BY added DESC, device, cs",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(owner)
        .bind(name.to_owned())
        .bind(now().saturating_sub(DEVICE_TTL_S))
        .fetch_all(&mut *conn)
        .await
        .map_err(sql)?;
        rows.iter()
            .map(|r| {
                Ok(DeviceRow {
                    device: r.try_get("device").map_err(sql)?,
                    cs: r.try_get("cs").map_err(sql)?,
                    change: r.try_get("change").map_err(sql)?,
                    added: r.try_get("added").map_err(sql)?,
                    state: i32::try_from(r.try_get::<i64, _>("state").map_err(sql)?)
                        .unwrap_or_default(),
                    reported_at: r.try_get("reported_at").map_err(sql)?,
                })
            })
            .collect()
    }

    /// Every head of a workspace, visible or hidden, by native changeset id.
    async fn head_set(
        conn: &mut AnyConnection,
        entry: &RepoEntry,
        owner: i64,
        name: &str,
    ) -> Result<std::collections::HashSet<Vec<u8>>, Status> {
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT cs FROM cloud_heads WHERE ns = $1 AND owner = $2 AND name = $3",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(owner)
        .bind(name.to_owned())
        .fetch_all(&mut *conn)
        .await
        .map_err(sql)?;
        Ok(rows.into_iter().collect())
    }

    /// Replaces the report of one device (SPEC §21.3.9); returns the drafts kept.
    pub async fn report_device(
        &self,
        ctx: &CallCtx,
        owner: i64,
        req: wire::ReportDeviceRequest,
    ) -> Result<wire::ReportDeviceResponse, Status> {
        let name = workspace_name(&req.name)?.to_owned();
        let device = device_name(&req.device)?.to_owned();
        if req.drafts.len() > MAX_REPORT {
            return Err(Status::invalid_argument("too many drafts"));
        }
        let mut drafts: Vec<(Vec<u8>, Vec<u8>, i64, i64)> = Vec::with_capacity(req.drafts.len());
        let mut seen = std::collections::HashSet::new();
        for d in &req.drafts {
            if d.cs.len() != 32 || d.change.len() != 16 {
                return Err(Status::invalid_argument(
                    "a draft needs a native changeset id and a change id",
                ));
            }
            if d.added < 0 {
                return Err(Status::invalid_argument("added is negative"));
            }
            let state = wire::LocalDraftState::try_from(d.state)
                .ok()
                .filter(|s| *s != wire::LocalDraftState::Unspecified)
                .ok_or_else(|| Status::invalid_argument("unknown draft state"))?;
            if seen.insert(d.cs.clone()) {
                drafts.push((
                    d.cs.to_vec(),
                    d.change.to_vec(),
                    d.added,
                    i64::from(i32::from(state)),
                ));
            }
        }
        let (entry, _) = self.open(ctx).await?;
        if !self.cloud_enabled(&entry).await? {
            return Err(Status::failed_precondition(
                "the commit cloud is off for this repository",
            ));
        }
        let ns = entry.ns.as_bytes().to_vec();
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        // The workspace limit (§21.3.7) also bounds the names reports land under.
        self.quota
            .allow_report(&mut tx, entry.ns, owner, &name)
            .await?;
        sqlx::query(
            "DELETE FROM cloud_device_drafts WHERE ns = $1 AND owner = $2 AND name = $3 \
             AND (device = $4 OR reported_at < $5)",
        )
        .bind(ns.clone())
        .bind(owner)
        .bind(name.clone())
        .bind(device.clone())
        .bind(at.saturating_sub(DEVICE_TTL_S))
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
        for (cs, change, added, state) in &drafts {
            sqlx::query(
                "INSERT INTO cloud_device_drafts \
                 (ns, owner, name, device, cs, change, added, state, reported_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(ns.clone())
            .bind(owner)
            .bind(name.clone())
            .bind(device.clone())
            .bind(cs.clone())
            .bind(change.clone())
            .bind(*added)
            .bind(*state)
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        // Keep the devices reported most recently; this one counts as the newest.
        let others: Vec<String> = sqlx::query_scalar(
            "SELECT device FROM cloud_device_drafts \
             WHERE ns = $1 AND owner = $2 AND name = $3 AND device <> $4 \
             GROUP BY device ORDER BY MAX(reported_at) DESC, device",
        )
        .bind(ns.clone())
        .bind(owner)
        .bind(name.clone())
        .bind(device.clone())
        .fetch_all(&mut *tx)
        .await
        .map_err(sql)?;
        let keep = usize::try_from(MAX_DEVICES - 1).unwrap_or(0);
        for old in others.iter().skip(keep) {
            sqlx::query(
                "DELETE FROM cloud_device_drafts \
                 WHERE ns = $1 AND owner = $2 AND name = $3 AND device = $4",
            )
            .bind(ns.clone())
            .bind(owner)
            .bind(name.clone())
            .bind(old.clone())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        tx.commit().await.map_err(sql)?;
        Ok(wire::ReportDeviceResponse {
            kept: u32::try_from(drafts.len()).unwrap_or(u32::MAX),
        })
    }

    pub async fn list_workspaces(
        &self,
        ctx: &CallCtx,
        owner: i64,
        req: wire::ListWorkspacesRequest,
    ) -> Result<wire::ListWorkspacesResponse, Status> {
        let limit = match req.limit {
            0 => 100,
            n => usize::try_from(n).unwrap_or(usize::MAX).min(MAX_LIST_PAGE),
        };
        let after = if req.page_token.is_empty() {
            None
        } else {
            Some(workspace_name(&req.page_token)?.to_owned())
        };
        let (entry, _) = self.open(ctx).await?;
        let sql_text = format!(
            "SELECT name, version, heads, updated FROM cloud_ws \
             WHERE ns = $1 AND owner = $2{} ORDER BY name LIMIT {}",
            if after.is_some() {
                " AND name > $3"
            } else {
                ""
            },
            limit + 1
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql_text))
            .bind(entry.ns.as_bytes().to_vec())
            .bind(owner);
        if let Some(a) = after {
            q = q.bind(a);
        }
        let rows = q.fetch_all(self.registry.pool()).await.map_err(sql)?;
        let more = rows.len() > limit;
        let mut workspaces = Vec::new();
        for r in rows.iter().take(limit) {
            let version: i64 = r.try_get("version").map_err(sql)?;
            let heads: i64 = r.try_get("heads").map_err(sql)?;
            workspaces.push(wire::WorkspaceInfo {
                owner: owner.to_string(),
                name: r.try_get("name").map_err(sql)?,
                version: u64::try_from(version).unwrap_or(0),
                heads: u32::try_from(heads).unwrap_or(0),
                updated: r.try_get("updated").map_err(sql)?,
            });
        }
        let next_page_token = match (more, workspaces.last()) {
            (true, Some(w)) => w.name.clone(),
            _ => String::new(),
        };
        Ok(wire::ListWorkspacesResponse {
            workspaces,
            next_page_token,
        })
    }

    pub async fn update_workspace(
        &self,
        ctx: &CallCtx,
        owner: i64,
        req: wire::UpdateWorkspaceRequest,
    ) -> Result<wire::UpdateWorkspaceResponse, Status> {
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let name = workspace_name(&req.name)?.to_owned();
        for n in [
            req.add.len(),
            req.remove.len(),
            req.hide.len(),
            req.unhide.len(),
        ] {
            if n > MAX_LIST {
                return Err(Status::invalid_argument("too many entries"));
            }
        }
        let (entry, ids) = self.open(ctx).await?;
        if let Some(v) = self.stored(&entry, op_id).await? {
            return Ok(wire::UpdateWorkspaceResponse { version: v });
        }
        if !self.cloud_enabled(&entry).await? {
            return Err(Status::failed_precondition(
                "the commit cloud is off for this repository",
            ));
        }

        // Everything that reads the database or the id codec happens before the transaction.
        let mut adds: Vec<(ChangesetId, String)> = Vec::with_capacity(req.add.len());
        for h in &req.add {
            let labels = asc_rpc::cloud::labels(&h.labels)
                .map_err(|_| Status::invalid_argument("invalid labels"))?;
            let labels: Vec<&str> = labels.iter().map(|l| l.as_str()).collect();
            let json = serde_json::to_string(&labels).map_err(|e| internal("labels", &e))?;
            adds.push((ids.changeset_in(&h.cs).await?, json));
        }
        // The last entry for a changeset wins.
        let mut seen = std::collections::HashSet::new();
        adds.reverse();
        adds.retain(|(cs, _)| seen.insert(*cs));
        adds.reverse();
        let mut changes: std::collections::HashMap<ChangesetId, ChangeId> = Default::default();
        {
            let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
            let css: Vec<ChangesetId> = adds.iter().map(|a| a.0).collect();
            let rows = index::rows_of(&mut conn, entry.ns, &css)
                .await
                .map_err(|e| internal("change index", &e))?;
            for r in rows {
                changes.insert(r.cs, r.change);
            }
        }
        if adds.iter().any(|(cs, _)| !changes.contains_key(cs)) {
            return Err(Status::failed_precondition(
                "a head names a changeset that was not uploaded with its change metadata",
            ));
        }
        let remove = Self::ids_in(&ids, &req.remove).await?;
        let hide = Self::ids_in(&ids, &req.hide).await?;
        let unhide = Self::ids_in(&ids, &req.unhide).await?;

        match self
            .apply(
                &entry, owner, &name, op_id, &req, &adds, &changes, &remove, &hide, &unhide,
            )
            .await?
        {
            Outcome::Done(version) => Ok(wire::UpdateWorkspaceResponse { version }),
            Outcome::Stale => {
                // A concurrent retry of this very op may have won; its result stands.
                if let Some(v) = self.stored(&entry, op_id).await? {
                    return Ok(wire::UpdateWorkspaceResponse { version: v });
                }
                let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
                let current = Self::row(&mut conn, &entry, owner, &name)
                    .await?
                    .map_or(0, |r| r.0);
                Err(conflict(current))
            }
        }
    }

    async fn ids_in(ids: &IdCodec, wire_ids: &[Bytes]) -> Result<Vec<ChangesetId>, Status> {
        let mut out = Vec::with_capacity(wire_ids.len());
        for b in wire_ids {
            out.push(ids.changeset_in(b).await?);
        }
        Ok(out)
    }

    /// The version stored for `op_id`, if the op was an `UpdateWorkspace` of this repository.
    async fn stored(&self, entry: &RepoEntry, op_id: Uuid) -> Result<Option<u64>, Status> {
        let row = sqlx::query("SELECT repo_id, result FROM op_results WHERE op_id = $1")
            .bind(op_id.as_bytes().to_vec())
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?;
        let Some(row) = row else { return Ok(None) };
        let repo: i64 = row.try_get("repo_id").map_err(sql)?;
        if repo != entry.key.0 {
            return Err(Status::invalid_argument(
                "op id was used for another repository",
            ));
        }
        let bytes: Vec<u8> = row.try_get("result").map_err(sql)?;
        match bytes.strip_prefix(OP_TAG.as_slice()) {
            Some(v) if v.len() == 8 => Ok(Some(u64::from_be_bytes(v.try_into().expect("8")))),
            _ => Err(Status::invalid_argument(
                "op id was used for another operation",
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply(
        &self,
        entry: &RepoEntry,
        owner: i64,
        name: &str,
        op_id: Uuid,
        req: &wire::UpdateWorkspaceRequest,
        adds: &[(ChangesetId, String)],
        changes: &std::collections::HashMap<ChangesetId, ChangeId>,
        remove: &[ChangesetId],
        hide: &[ChangesetId],
        unhide: &[ChangesetId],
    ) -> Result<Outcome, Status> {
        let ns = entry.ns.as_bytes().to_vec();
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        // U48c: the namespace must still belong to this repository id and name.
        self.registry.guard(&mut tx, entry).await?;
        let (base, mut heads) = match Self::row(&mut tx, entry, owner, name).await? {
            Some((v, h)) => {
                if v != req.base_version {
                    return Ok(Outcome::Stale);
                }
                let done = sqlx::query(
                    "UPDATE cloud_ws SET version = version + 1, updated = $4 \
                     WHERE ns = $1 AND owner = $2 AND name = $3 AND version = $5",
                )
                .bind(ns.clone())
                .bind(owner)
                .bind(name)
                .bind(at)
                .bind(i64::try_from(v).map_err(|_| internal("version", &"overflow"))?)
                .execute(&mut *tx)
                .await
                .map_err(sql)?;
                if done.rows_affected() == 0 {
                    return Ok(Outcome::Stale);
                }
                (v, h)
            }
            None => {
                if req.base_version != 0 {
                    return Ok(Outcome::Stale);
                }
                self.quota.claim_workspace(&mut tx, entry.ns, owner).await?;
                let done = sqlx::query(
                    "INSERT INTO cloud_ws (ns, owner, name, version, updated, heads) \
                     VALUES ($1, $2, $3, 1, $4, 0) ON CONFLICT DO NOTHING",
                )
                .bind(ns.clone())
                .bind(owner)
                .bind(name)
                .bind(at)
                .execute(&mut *tx)
                .await
                .map_err(sql)?;
                if done.rows_affected() == 0 {
                    return Ok(Outcome::Stale);
                }
                // Created at version 1 above; the next version is `base + 1`.
                (0, 0)
            }
        };
        let version = base + 1;

        for (cs, labels) in adds {
            let change = changes[cs];
            let added = sqlx::query(
                "INSERT INTO cloud_heads (ns, owner, name, cs, change, labels, added, hidden_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, NULL) ON CONFLICT DO NOTHING",
            )
            .bind(ns.clone())
            .bind(owner)
            .bind(name)
            .bind(cs.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec())
            .bind(labels.as_str())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(sql)?
            .rows_affected();
            if added == 0 {
                sqlx::query(
                    "UPDATE cloud_heads SET labels = $5 \
                     WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4",
                )
                .bind(ns.clone())
                .bind(owner)
                .bind(name)
                .bind(cs.as_bytes().to_vec())
                .bind(labels.as_str())
                .execute(&mut *tx)
                .await
                .map_err(sql)?;
            } else {
                heads += 1;
            }
        }
        for cs in remove {
            let gone = sqlx::query(
                "DELETE FROM cloud_heads WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4",
            )
            .bind(ns.clone())
            .bind(owner)
            .bind(name)
            .bind(cs.as_bytes().to_vec())
            .execute(&mut *tx)
            .await
            .map_err(sql)?
            .rows_affected();
            heads -= i64::try_from(gone).unwrap_or(0);
        }
        for cs in hide {
            sqlx::query(
                "UPDATE cloud_heads SET hidden_at = $5 \
                 WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4 AND hidden_at IS NULL",
            )
            .bind(ns.clone())
            .bind(owner)
            .bind(name)
            .bind(cs.as_bytes().to_vec())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        for cs in unhide {
            sqlx::query(
                "UPDATE cloud_heads SET hidden_at = NULL \
                 WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4",
            )
            .bind(ns.clone())
            .bind(owner)
            .bind(name)
            .bind(cs.as_bytes().to_vec())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        if heads > i64::from(self.quota.limits().max_heads_per_workspace) {
            // Dropping `tx` rolls the whole update back.
            return Err(heads_limit());
        }
        sqlx::query("UPDATE cloud_ws SET heads = $4 WHERE ns = $1 AND owner = $2 AND name = $3")
            .bind(ns)
            .bind(owner)
            .bind(name)
            .bind(heads.max(0))
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        let mut result = OP_TAG.to_vec();
        result.extend_from_slice(&version.to_be_bytes());
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(result)
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        Ok(Outcome::Done(version))
    }
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Cloud` tonic service.
/// One reported draft of one device.
struct DeviceRow {
    device: String,
    cs: Vec<u8>,
    change: Vec<u8>,
    added: i64,
    state: i32,
    reported_at: i64,
}

impl DeviceRow {
    /// The draft as `GetWorkspace.local` lists it: no changeset id, as the server holds no object.
    fn local(&self) -> wire::LocalDraft {
        wire::LocalDraft {
            cs: Bytes::new(),
            change: Bytes::from(self.change.clone()),
            added: self.added,
            state: self.state,
            device: self.device.clone(),
            reported_at: self.reported_at,
        }
    }
}

pub struct CloudService {
    rpc: Arc<CloudRpc>,
    authorizer: Arc<dyn Authorizer>,
    info: ServerInfo,
}

impl CloudService {
    pub fn new(rpc: Arc<CloudRpc>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            rpc,
            authorizer,
            info,
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<CloudServer<Self>, AuthInterceptor> {
        let server = CloudServer::new(self)
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
        mutating: bool,
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
            let mut info = ErrorInfo::default();
            info.paths.push(format!("minimum client version is {min}"));
            return Err(rpc_status(&RpcError::ClientTooOld(info)));
        }
        let principal = self
            .authorizer
            .authorize_credential(ext.get::<Credential>(), name.as_str(), access)
            .await?;
        // A call the hub makes for a signed-in person acts as that person, within the claim.
        let (principal, admin) = acting::act_as(&*self.authorizer, principal, meta, &name, access)?;
        let op_id = match meta.get("x-asc-op-id") {
            None => None,
            Some(v) => Some(
                v.to_str()
                    .ok()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(|| Status::invalid_argument("malformed x-asc-op-id"))?,
            ),
        };
        if mutating && op_id.is_none() {
            return Err(Status::invalid_argument("mutating calls need x-asc-op-id"));
        }
        let admin = match admin.forwarded_claim() {
            Some(_) => admin,
            None => asc_rpc::auth::AdminProof::new(
                self.authorizer.clone(),
                ext.get::<Credential>().cloned(),
                name.as_str(),
            ),
        };
        Ok(CallCtx {
            repo: name,
            id_mode,
            principal,
            op_id,
            client,
            admin,
        })
    }

    /// The user whose workspaces a read names. Another user's needs repo write (SPEC §21.2.5).
    async fn read_owner(&self, ctx: &CallCtx, ext: &TonicExt, raw: &str) -> Result<i64, Status> {
        let owner = parse_owner(&ctx.principal.id, raw)?
            .ok_or_else(|| Status::permission_denied("no user identity"))?;
        if refpolicy::owner_of(&ctx.principal) != Some(owner)
            && !self.has(ext, ctx, Access::Write).await
        {
            return Err(Status::permission_denied(
                "reading another user's workspace needs write access",
            ));
        }
        Ok(owner)
    }

    /// Whether the caller holds `access` on the repository of `ctx`. A call made on the hub's claim
    /// holds what the claim's role covers and nothing more (the service credential is not asked).
    async fn has(&self, ext: &TonicExt, ctx: &CallCtx, access: Access) -> bool {
        if let Some(c) = ctx.admin.forwarded_claim() {
            return c.allows(acting::role_of(access));
        }
        self.authorizer
            .authorize_credential(ext.get::<Credential>(), ctx.repo.as_str(), access)
            .await
            .is_ok()
    }
}

#[tonic::async_trait]
impl Cloud for CloudService {
    async fn get_workspace(
        &self,
        request: Request<wire::GetWorkspaceRequest>,
    ) -> Result<Response<wire::GetWorkspaceResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let owner = self.read_owner(&ctx, &ext, &msg.owner).await?;
        self.rpc
            .get_workspace(&ctx, owner, msg)
            .await
            .map(Response::new)
    }

    async fn update_workspace(
        &self,
        request: Request<wire::UpdateWorkspaceRequest>,
    ) -> Result<Response<wire::UpdateWorkspaceResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, true)
            .await?;
        // Only the owner updates a workspace, and only a verified user identity is an owner.
        let owner = refpolicy::owner_of(&ctx.principal)
            .ok_or_else(|| Status::permission_denied("the commit cloud needs a user identity"))?;
        self.rpc
            .update_workspace(&ctx, owner, msg)
            .await
            .map(Response::new)
    }

    async fn list_workspaces(
        &self,
        request: Request<wire::ListWorkspacesRequest>,
    ) -> Result<Response<wire::ListWorkspacesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        let owner = self.read_owner(&ctx, &ext, &msg.owner).await?;
        self.rpc
            .list_workspaces(&ctx, owner, msg)
            .await
            .map(Response::new)
    }

    async fn report_device(
        &self,
        request: Request<wire::ReportDeviceRequest>,
    ) -> Result<Response<wire::ReportDeviceResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        // Only the owner reports their devices, and only a verified user identity is an owner.
        let owner = refpolicy::owner_of(&ctx.principal)
            .ok_or_else(|| Status::permission_denied("the commit cloud needs a user identity"))?;
        self.rpc
            .report_device(&ctx, owner, msg)
            .await
            .map(Response::new)
    }

    async fn purge(
        &self,
        request: Request<wire::PurgeRequest>,
    ) -> Result<Response<wire::PurgeResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        // Admin rights are asked of the authorizer; an answer other than yes means no.
        let admin = self.has(&ext, &ctx, Access::Admin).await;
        let owner = refpolicy::owner_of(&ctx.principal);
        if owner.is_none() && !admin {
            return Err(Status::permission_denied(
                "the commit cloud needs a user identity",
            ));
        }
        self.rpc
            .purge(&ctx, owner, admin, msg)
            .await
            .map(Response::new)
    }
}

#[cfg(test)]
mod tests {
    use asc_core::change::{ChangeMeta, RewriteOp};
    use asc_rpc::auth::{BearerToken, StaticTokens};
    use asc_rpc::cloud::conflict_version;
    use asc_rpc::server::wire::RepoRef;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    const ALICE: &str = "alice-token";
    const BOB: &str = "bob-token";
    const WRITER: &str = "writer-token";

    struct Fx {
        reg: Arc<Registry>,
        e: Arc<RepoEntry>,
        svc: CloudService,
        css: Vec<ChangesetId>,
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    async fn fx_with(limits: Limits) -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        sqlx::query("UPDATE repos SET cloud = TRUE WHERE id = $1")
            .bind(e.key.0)
            .execute(reg.pool())
            .await
            .unwrap();
        let mut css: Vec<ChangesetId> = Vec::new();
        let idx = index::ChangeIndex::new(reg.clone());
        for i in 1..=5u8 {
            let parents: Vec<ChangesetId> = css.last().copied().into_iter().collect();
            let cs = commit(&e, &parents, &[("f", &[i])]).await;
            let mut tx = reg.refs().begin().await.unwrap();
            idx.write_meta(
                &mut tx,
                e.key,
                &ChangeMeta::new(cs, cid(i), vec![], RewriteOp::New),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            css.push(cs);
        }
        let ext = Extensions::new();
        register(&ext, &reg, limits);
        let auth = StaticTokens::new()
            .with_token(ALICE, "42:alice", Access::Read)
            .with_token(BOB, "43:bob", Access::Read)
            .with_token(WRITER, "44:carol", Access::Write);
        let svc = CloudService::new(
            ext.cloud.get().unwrap().clone(),
            Arc::new(auth),
            ServerInfo::default(),
        );
        Fx { reg, e, svc, css }
    }

    async fn fx() -> Fx {
        fx_with(Limits::default()).await
    }

    fn req<T>(m: T, token: &str, op: bool) -> Request<T> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(token).unwrap()));
        if op {
            r.metadata_mut()
                .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
        }
        r
    }

    fn repo_ref() -> Option<RepoRef> {
        Some(RepoRef {
            name: "o/r".into(),
            id_mode: IdMode::Asc.to_wire(),
        })
    }

    fn head(cs: ChangesetId, labels: &[&str]) -> wire::CloudHead {
        wire::CloudHead {
            cs: Bytes::copy_from_slice(cs.as_bytes()),
            change: Bytes::copy_from_slice(&[0xee; 16]),
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            added: 5,
            hidden_at: 9,
            devices: Vec::new(),
        }
    }

    fn upd(name: &str, base: u64, add: Vec<wire::CloudHead>) -> wire::UpdateWorkspaceRequest {
        wire::UpdateWorkspaceRequest {
            repo: repo_ref(),
            name: name.into(),
            base_version: base,
            add,
            ..Default::default()
        }
    }

    async fn update(f: &Fx, token: &str, m: wire::UpdateWorkspaceRequest) -> Result<u64, Status> {
        f.svc
            .update_workspace(req(m, token, true))
            .await
            .map(|r| r.into_inner().version)
    }

    async fn get(f: &Fx, token: &str, owner: &str, name: &str) -> wire::GetWorkspaceResponse {
        f.svc
            .get_workspace(req(
                wire::GetWorkspaceRequest {
                    repo: repo_ref(),
                    owner: owner.into(),
                    name: name.into(),
                    ..Default::default()
                },
                token,
                false,
            ))
            .await
            .unwrap()
            .into_inner()
    }

    fn cs_bytes(cs: ChangesetId) -> Bytes {
        Bytes::copy_from_slice(cs.as_bytes())
    }

    fn rpc_code(s: &Status) -> String {
        asc_rpc::server::status_rpc_error(s)
            .map(|e| format!("{e:?}"))
            .unwrap_or_else(|| format!("{:?}", s.code()))
    }

    #[tokio::test]
    async fn a_workspace_is_created_updated_and_read_back_with_a_derived_change() {
        let f = fx().await;
        assert_eq!(get(&f, ALICE, "", "").await.version, 0);
        let v = update(
            &f,
            ALICE,
            upd("", 0, vec![head(f.css[0], &["wip", "a/b-1.x_"])]),
        )
        .await
        .unwrap();
        assert_eq!(v, 1);
        let g = get(&f, ALICE, "", "default").await;
        assert_eq!((g.version, g.heads.len()), (1, 1));
        // The client's change, added and hidden_at were ignored.
        assert_eq!(g.heads[0].change.as_ref(), cid(1).as_bytes());
        assert_eq!(g.heads[0].hidden_at, 0);
        assert!(g.heads[0].added > 100);
        assert_eq!(g.heads[0].labels, ["wip", "a/b-1.x_"]);
        // since_version equal to the current version: nothing to send.
        let same = f
            .svc
            .get_workspace(req(
                wire::GetWorkspaceRequest {
                    repo: repo_ref(),
                    since_version: 1,
                    ..Default::default()
                },
                ALICE,
                false,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((same.version, same.heads.len()), (1, 0));
        // Hide, then unhide, then remove.
        let mut m = upd("", 1, vec![head(f.css[1], &[])]);
        m.hide = vec![cs_bytes(f.css[0])];
        assert_eq!(update(&f, ALICE, m).await.unwrap(), 2);
        assert_eq!(get(&f, ALICE, "", "").await.heads.len(), 1);
        let mut m = upd("", 2, vec![]);
        m.unhide = vec![cs_bytes(f.css[0])];
        m.remove = vec![cs_bytes(f.css[1])];
        assert_eq!(update(&f, ALICE, m).await.unwrap(), 3);
        let g = get(&f, ALICE, "", "").await;
        assert_eq!(g.heads.len(), 1);
        assert_eq!(g.heads[0].cs.as_ref(), f.css[0].as_bytes());
        let l = f
            .svc
            .list_workspaces(req(
                wire::ListWorkspacesRequest {
                    repo: repo_ref(),
                    ..Default::default()
                },
                ALICE,
                false,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(l.workspaces.len(), 1);
        assert_eq!(
            (
                l.workspaces[0].heads,
                l.workspaces[0].version,
                l.workspaces[0].owner.as_str()
            ),
            (1, 3, "42")
        );
    }

    /// Calls `apply` with the entry opened before the repository was changed underneath it.
    async fn apply_stale(f: &Fx) -> Result<(), Status> {
        let cs = f.css[0];
        let adds = vec![(cs, "[]".to_owned())];
        let changes = std::collections::HashMap::from([(cs, cid(1))]);
        let m = upd("w", 0, vec![]);
        f.svc
            .rpc
            .apply(
                &f.e,
                42,
                "w",
                Uuid::new_v4(),
                &m,
                &adds,
                &changes,
                &[],
                &[],
                &[],
            )
            .await
            .map(|_| ())
    }

    async fn cloud_rows(f: &Fx) -> i64 {
        let mut n = 0;
        for t in ["cloud_ws", "cloud_heads", "cloud_usage"] {
            let c: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {t} WHERE ns = $1"
            )))
            .bind(f.e.ns.as_bytes().to_vec())
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
            n += c;
        }
        n
    }

    #[tokio::test]
    async fn an_apply_after_the_repository_was_deleted_or_renamed_writes_nothing() {
        let f = fx().await;
        sqlx::query("UPDATE repos SET name = 'o/moved' WHERE id = $1")
            .bind(f.e.key.0)
            .execute(f.reg.pool())
            .await
            .unwrap();
        let e = apply_stale(&f).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        assert_eq!(cloud_rows(&f).await, 0);

        sqlx::query("DELETE FROM repos WHERE id = $1")
            .bind(f.e.key.0)
            .execute(f.reg.pool())
            .await
            .unwrap();
        let e = apply_stale(&f).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        assert_eq!(cloud_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn a_concurrent_update_workspace_gets_version_conflict() {
        let f = fx().await;
        update(&f, ALICE, upd("w", 0, vec![head(f.css[0], &[])]))
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            update(&f, ALICE, upd("w", 1, vec![head(f.css[1], &[])])),
            update(&f, ALICE, upd("w", 1, vec![head(f.css[2], &[])])),
        );
        let (ok, err) = match (a, b) {
            (Ok(v), Err(e)) | (Err(e), Ok(v)) => (v, e),
            other => panic!("exactly one wins: {other:?}"),
        };
        assert_eq!(ok, 2);
        assert_eq!(err.code(), tonic::Code::Aborted);
        let current = asc_rpc::server::status_rpc_error(&err)
            .as_ref()
            .and_then(conflict_version);
        assert_eq!(current, Some(2));
        assert_eq!(get(&f, ALICE, "", "w").await.version, 2);
        // The loser retried at the current version succeeds.
        assert_eq!(
            update(&f, ALICE, upd("w", 2, vec![head(f.css[3], &[])]))
                .await
                .unwrap(),
            3
        );
        // A workspace that does not exist conflicts at version 0 for any other base.
        let e = update(&f, ALICE, upd("nope", 4, vec![])).await.unwrap_err();
        let missing = asc_rpc::server::status_rpc_error(&e)
            .as_ref()
            .and_then(conflict_version);
        assert_eq!(missing, Some(0));
    }

    #[tokio::test]
    async fn a_retried_op_id_returns_the_stored_result() {
        let f = fx().await;
        let m = upd("w", 0, vec![head(f.css[0], &[])]);
        let first = req(m.clone(), ALICE, true);
        let op = first.metadata().get("x-asc-op-id").unwrap().clone();
        let v1 = f.svc.update_workspace(first).await.unwrap().into_inner();
        let mut again = req(m, ALICE, false);
        again.metadata_mut().insert("x-asc-op-id", op);
        let v2 = f.svc.update_workspace(again).await.unwrap().into_inner();
        assert_eq!((v1.version, v2.version), (1, 1));
        assert_eq!(get(&f, ALICE, "", "w").await.version, 1);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM op_results")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn bad_input_is_refused_and_nothing_is_written() {
        let f = fx().await;
        let bad_labels: Vec<Vec<String>> = vec![
            vec!["has space".into()],
            vec!["x".repeat(65)],
            vec![String::new()],
            vec!["a".into(); 9],
            vec!["semi;colon".into()],
            vec!["caf\u{e9}".into()],
        ];
        for labels in bad_labels {
            let mut h = head(f.css[0], &[]);
            h.labels = labels;
            let e = update(&f, ALICE, upd("w", 0, vec![h])).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument);
        }
        for name in ["a b", "../x", &"n".repeat(65), "a/b"] {
            let e = update(&f, ALICE, upd(name, 0, vec![])).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{name}");
        }
        // An unknown changeset, a malformed one and a non-user caller.
        let ghost = ChangesetId::from_bytes([9; 32]);
        let e = update(&f, ALICE, upd("w", 0, vec![head(ghost, &[])]))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        let mut h = head(f.css[0], &[]);
        h.cs = Bytes::from_static(b"short");
        assert!(update(&f, ALICE, upd("w", 0, vec![h])).await.is_err());
        let auth = StaticTokens::new().with_token("t", "service", Access::Read);
        let ext = Extensions::new();
        register(&ext, &f.reg, Limits::default());
        let svc = CloudService::new(
            ext.cloud.get().unwrap().clone(),
            Arc::new(auth),
            ServerInfo::default(),
        );
        let e = svc
            .update_workspace(req(upd("w", 0, vec![]), "t", true))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_ws")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(rows, 0);
        // A mutating call needs an op id.
        let e = f
            .svc
            .update_workspace(req(upd("w", 0, vec![]), ALICE, false))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn the_seventeenth_workspace_and_the_heads_cap_are_refused() {
        let f = fx_with(Limits {
            max_heads_per_workspace: 2,
            ..Limits::default()
        })
        .await;
        for i in 0..16 {
            update(&f, ALICE, upd(&format!("w{i}"), 0, vec![]))
                .await
                .unwrap();
        }
        let e = update(&f, ALICE, upd("w16", 0, vec![])).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        assert!(rpc_code(&e).starts_with("CloudWorkspacesLimit"), "{e:?}");
        // Another user is unaffected, and an existing workspace still updates.
        update(&f, BOB, upd("w0", 0, vec![])).await.unwrap();
        let three = vec![
            head(f.css[0], &[]),
            head(f.css[1], &[]),
            head(f.css[2], &[]),
        ];
        let e = update(&f, ALICE, upd("w0", 1, three)).await.unwrap_err();
        assert!(rpc_code(&e).starts_with("CloudHeadsLimit"), "{e:?}");
        // The refused update left the workspace as it was.
        assert_eq!(get(&f, ALICE, "", "w0").await.version, 1);
        let two = vec![head(f.css[0], &[]), head(f.css[1], &[])];
        assert_eq!(update(&f, ALICE, upd("w0", 1, two)).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn reading_another_users_workspace_needs_repo_write() {
        let f = fx().await;
        update(&f, ALICE, upd("w", 0, vec![head(f.css[0], &[])]))
            .await
            .unwrap();
        let ask = |token: &'static str, owner: &'static str| {
            let f = &f;
            async move {
                f.svc
                    .get_workspace(req(
                        wire::GetWorkspaceRequest {
                            repo: repo_ref(),
                            owner: owner.into(),
                            name: "w".into(),
                            ..Default::default()
                        },
                        token,
                        false,
                    ))
                    .await
            }
        };
        assert_eq!(ask(ALICE, "42").await.unwrap().into_inner().heads.len(), 1);
        assert_eq!(
            ask(ALICE, "alice").await.unwrap().into_inner().heads.len(),
            1
        );
        assert_eq!(
            ask(BOB, "42").await.unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(ask(WRITER, "42").await.unwrap().into_inner().heads.len(), 1);
        assert_eq!(
            ask(BOB, "mallory").await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            ask(BOB, "-1").await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        // Bob's own, empty workspace is not Alice's.
        assert_eq!(get(&f, BOB, "", "w").await.version, 0);
    }

    #[tokio::test]
    async fn the_cloud_flag_gates_updates_and_page_tokens_resume() {
        let f = fx().await;
        let all: Vec<wire::CloudHead> = f.css.iter().map(|c| head(*c, &[])).collect();
        update(&f, ALICE, upd("w", 0, all)).await.unwrap();
        let g = get(&f, ALICE, "", "w").await;
        assert_eq!(g.heads.len(), 5);
        assert!(g.next_page_token.is_empty());
        let page = |token: String| {
            let f = &f;
            async move {
                f.svc
                    .get_workspace(req(
                        wire::GetWorkspaceRequest {
                            repo: repo_ref(),
                            name: "w".into(),
                            page_token: token,
                            ..Default::default()
                        },
                        ALICE,
                        false,
                    ))
                    .await
            }
        };
        assert_eq!(
            page("!!".into()).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let rest = page(page_token(f.css[1])).await.unwrap().into_inner();
        let bound = f.css[1].as_bytes().as_slice();
        assert!(rest.heads.iter().all(|h| h.cs.as_ref() > bound));
        assert_eq!(
            rest.heads.len(),
            f.css
                .iter()
                .filter(|c| c.as_bytes().as_slice() > bound)
                .count()
        );
        sqlx::query("UPDATE repos SET cloud = FALSE WHERE id = $1")
            .bind(f.e.key.0)
            .execute(f.reg.pool())
            .await
            .unwrap();
        let e = update(&f, ALICE, upd("w", 1, vec![])).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        // Reads still work.
        assert_eq!(get(&f, ALICE, "", "w").await.heads.len(), 5);
    }

    fn local(
        cs: ChangesetId,
        change: u8,
        added: i64,
        state: wire::LocalDraftState,
    ) -> wire::LocalDraft {
        wire::LocalDraft {
            cs: cs_bytes(cs),
            change: Bytes::copy_from_slice(&[change; 16]),
            added,
            state: state.into(),
            ..Default::default()
        }
    }

    async fn report(
        f: &Fx,
        token: &str,
        device: &str,
        drafts: Vec<wire::LocalDraft>,
    ) -> Result<u32, Status> {
        report_in(f, token, "", device, drafts).await
    }

    async fn report_in(
        f: &Fx,
        token: &str,
        name: &str,
        device: &str,
        drafts: Vec<wire::LocalDraft>,
    ) -> Result<u32, Status> {
        f.svc
            .report_device(req(
                wire::ReportDeviceRequest {
                    repo: repo_ref(),
                    name: name.into(),
                    device: device.into(),
                    drafts,
                },
                token,
                false,
            ))
            .await
            .map(|r| r.into_inner().kept)
    }

    async fn located(f: &Fx, token: &str, owner: &str) -> wire::GetWorkspaceResponse {
        f.svc
            .get_workspace(req(
                wire::GetWorkspaceRequest {
                    repo: repo_ref(),
                    owner: owner.into(),
                    include_devices: true,
                    since_version: 1,
                    ..Default::default()
                },
                token,
                false,
            ))
            .await
            .unwrap()
            .into_inner()
    }

    #[tokio::test]
    async fn device_reports_say_where_each_draft_is() {
        use wire::LocalDraftState as S;
        let f = fx().await;
        // Before any workspace exists, every reported draft is local.
        let lone = ChangesetId::from_bytes([0xab; 32]);
        assert_eq!(
            report(&f, ALICE, "laptop", vec![local(lone, 7, 50, S::TooLarge)])
                .await
                .unwrap(),
            1
        );
        let g = located(&f, ALICE, "").await;
        assert_eq!((g.version, g.heads.len(), g.local.len()), (0, 0, 1));
        assert_eq!(g.local[0].device, "laptop");
        assert_eq!(g.local[0].state, S::TooLarge as i32);
        assert!(
            g.local[0].cs.is_empty(),
            "no object id for a draft the server lacks"
        );
        assert_eq!(g.local[0].change.as_ref(), &[7u8; 16]);
        assert!(g.local[0].reported_at > 100);

        update(&f, ALICE, upd("", 0, vec![head(f.css[0], &[])]))
            .await
            .unwrap();
        // A second report of the same device replaces the first; a duplicate cs counts once.
        let two = vec![
            local(f.css[0], 1, 10, S::Uploaded),
            local(f.css[0], 1, 10, S::Uploaded),
            local(lone, 7, 60, S::Waiting),
        ];
        assert_eq!(report(&f, ALICE, "laptop", two).await.unwrap(), 2);
        assert_eq!(
            report(&f, ALICE, "desk", vec![local(f.css[0], 1, 10, S::Uploaded)])
                .await
                .unwrap(),
            1
        );
        // since_version is the current version, yet the devices are answered in full.
        let g = located(&f, ALICE, "").await;
        assert_eq!(g.heads.len(), 1);
        let mut devices = g.heads[0].devices.clone();
        devices.sort();
        assert_eq!(devices, ["desk", "laptop"]);
        assert_eq!(g.local.len(), 1);
        assert_eq!(
            (g.local[0].state, g.local[0].added),
            (S::Waiting as i32, 60)
        );
        // Without include_devices nothing changes for an old reader.
        let plain = get(&f, ALICE, "", "").await;
        assert!(plain.local.is_empty() && plain.heads[0].devices.is_empty());
        // Another user's locations need repo write, as their workspace does.
        assert_eq!(located(&f, WRITER, "42").await.local.len(), 1);
        let e = f
            .svc
            .get_workspace(req(
                wire::GetWorkspaceRequest {
                    repo: repo_ref(),
                    owner: "42".into(),
                    include_devices: true,
                    ..Default::default()
                },
                BOB,
                false,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        // Bob's report lands in Bob's workspace only.
        report(&f, BOB, "laptop", vec![local(lone, 9, 1, S::Failed)])
            .await
            .unwrap();
        assert_eq!(located(&f, ALICE, "").await.local.len(), 1);
        // An empty report clears the device.
        assert_eq!(report(&f, ALICE, "laptop", vec![]).await.unwrap(), 0);
        let g = located(&f, ALICE, "").await;
        assert_eq!(g.heads[0].devices, ["desk"]);
        assert!(g.local.is_empty());
    }

    #[tokio::test]
    async fn device_reports_are_bounded_and_validated() {
        use wire::LocalDraftState as S;
        let f = fx().await;
        let cs = ChangesetId::from_bytes([1; 32]);
        for bad in ["", "a b", "lap/top", &"d".repeat(65)] {
            let e = report(&f, ALICE, bad, vec![]).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{bad:?}");
        }
        let short = wire::LocalDraft {
            cs: Bytes::from_static(&[1; 20]),
            ..local(cs, 1, 1, S::Waiting)
        };
        let no_state = local(cs, 1, 1, S::Unspecified);
        let unknown = wire::LocalDraft {
            state: 99,
            ..local(cs, 1, 1, S::Waiting)
        };
        let negative = local(cs, 1, -1, S::Waiting);
        for d in [short, no_state, unknown, negative] {
            let e = report(&f, ALICE, "laptop", vec![d]).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument);
        }
        let many = (0..=MAX_REPORT)
            .map(|i| {
                let mut b = [0u8; 32];
                b[..8].copy_from_slice(&(i as u64).to_be_bytes());
                local(ChangesetId::from_bytes(b), 1, 1, S::Waiting)
            })
            .collect();
        assert_eq!(
            report(&f, ALICE, "laptop", many).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        // Nine devices: the least recently reported is dropped.
        for n in 0..9 {
            sqlx::query("UPDATE cloud_device_drafts SET reported_at = reported_at - 10")
                .execute(f.reg.pool())
                .await
                .unwrap();
            report(
                &f,
                ALICE,
                &format!("d{n}"),
                vec![local(cs, 1, 1, S::Waiting)],
            )
            .await
            .unwrap();
        }
        let g = located(&f, ALICE, "").await;
        let mut devices: Vec<&str> = g.local.iter().map(|d| d.device.as_str()).collect();
        devices.sort_unstable();
        assert_eq!(devices, ["d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8"]);
        // A report older than 30 days is not shown and is dropped by the next report.
        sqlx::query("UPDATE cloud_device_drafts SET reported_at = 1 WHERE device = 'd1'")
            .execute(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(located(&f, ALICE, "").await.local.len(), 7);
        report(&f, ALICE, "d2", vec![]).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_device_drafts")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(left, 6, "d1 expired and d2 emptied");
        // With the cloud off a report is refused.
        sqlx::query("UPDATE repos SET cloud = FALSE WHERE id = $1")
            .bind(f.e.key.0)
            .execute(f.reg.pool())
            .await
            .unwrap();
        let e = report(&f, ALICE, "laptop", vec![]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn reports_under_names_never_synced_are_bounded_by_the_workspace_limit() {
        use wire::LocalDraftState as S;
        let f = fx_with(Limits {
            max_workspaces_per_user: 2,
            ..Limits::default()
        })
        .await;
        let cs = ChangesetId::from_bytes([3; 32]);
        let one = || vec![local(cs, 1, 1, S::NotQueued)];
        report_in(&f, ALICE, "a", "laptop", one()).await.unwrap();
        report_in(&f, ALICE, "b", "laptop", one()).await.unwrap();
        let e = report_in(&f, ALICE, "c", "laptop", one())
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        assert!(rpc_code(&e).starts_with("CloudWorkspacesLimit"), "{e:?}");
        // A name already reported, another device on it, and another user still report.
        report_in(&f, ALICE, "a", "desk", one()).await.unwrap();
        report_in(&f, BOB, "c", "laptop", one()).await.unwrap();
        // A workspace that exists is already counted by the workspace quota.
        update(&f, ALICE, upd("c", 0, vec![])).await.unwrap();
        report_in(&f, ALICE, "c", "laptop", one()).await.unwrap();
        // Clearing a name frees its slot.
        report_in(&f, ALICE, "b", "laptop", vec![]).await.unwrap();
        report_in(&f, ALICE, "d", "laptop", one()).await.unwrap();
        let names: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT name) FROM cloud_device_drafts WHERE owner = 42",
        )
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
        assert_eq!(names, 3, "a, c and d");
    }

    #[tokio::test]
    async fn concurrent_reports_under_new_names_do_not_pass_the_limit_together() {
        use wire::LocalDraftState as S;
        let f = Arc::new(
            fx_with(Limits {
                max_workspaces_per_user: 2,
                ..Limits::default()
            })
            .await,
        );
        let cs = ChangesetId::from_bytes([4; 32]);
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let f = f.clone();
                tokio::spawn(async move {
                    report_in(
                        &f,
                        ALICE,
                        &format!("n{i}"),
                        "laptop",
                        vec![local(cs, 1, 1, S::Waiting)],
                    )
                    .await
                    .is_ok()
                })
            })
            .collect();
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap() {
                ok += 1;
            }
        }
        let names: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT name) FROM cloud_device_drafts WHERE owner = 42",
        )
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
        assert!(ok <= 2 && names <= 2, "{ok} accepted, {names} names");
    }
}
