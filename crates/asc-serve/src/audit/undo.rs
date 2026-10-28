//! The ref log and ref-level undo (`Stacks.ListRefLog`, `Stacks.UndoRef`).
//!
//! Every ref change writes a `ref_log` row. [`RefLog::undo`] puts a ref back to the value it had
//! before one of those rows, under four guards that hold for every ref:
//!
//! 1. **Own operation.** The row's actor is the caller. Nobody undoes another person's push.
//! 2. **Newest row.** The row is the latest of its ref. If anything moved the ref since (or moved
//!    it away and back), the answer is `VERSION_CONFLICT` and nothing changes; the compare-and-swap
//!    inside the transaction closes the race between the check and the write.
//! 3. **Target exists.** The value to restore is still in the repository (a garbage-collected
//!    changeset cannot come back).
//! 4. **Idempotent.** A retried `x-asc-op-id` returns the first answer.
//!
//! A *protected* ref is any ref outside the caller's own user refs: the trunk, release branches,
//! tags, shared and server-managed refs. Undoing one is the roll-back of an update the caller just
//! made (a land, say), not a push: it needs repository admin and the row must be younger than
//! `[audit] undo_window_secs` (600 by default; 0 switches it off). Otherwise the answer is
//! `REF_UNDO_PROTECTED` and the way back is `Stacks.RevertLanded`, which makes new changes.
//!
//! The undo is itself a ref update: it writes a `ref_log` row (`via = "undo"`), an audit event
//! `repo.ref.undo`, and the outbox row the forge adapter listens to. It can be undone in turn by
//! the same rules. When the undone update was a land, the landing records of that update
//! (`land_map_ref`, the mainline index of the branch and the landed markers of the actor's user
//! refs) are removed in the same transaction, so the changes show as not landed again.

use std::collections::HashMap;
use std::sync::Arc;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{CallCtx, wire};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor as RefActor, Expect, RefError, RefTxn, RefUpdate, Refs};
use bytes::Bytes;
use prost::Message;
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};
use tonic::Status;
use uuid::Uuid;

use super::actor::actor_of;
use super::catalogue::plain;
use super::{NewEvent, Outcome, store};
use crate::mainline;
use crate::refpolicy::{self, Class, RepoPolicy, USER_NS, UserRefWriter};
use crate::registry::{Registry, RepoEntry, RepoMode};
use crate::update::{
    EffectRef, OutboxEffects, PipelineHooks, conflict, info, internal, rpc, target_hex,
};

/// Tag of the `op_results` value of an undo, so another operation's id is not read as one.
const OP_TAG: &[u8; 4] = b"und1";
/// Entries in one `ListRefLog` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 50;
/// Entries in one `ListRefLog` page, at most.
pub const MAX_PAGE: usize = 200;
/// Longest `reason`, in bytes.
pub const MAX_REASON: usize = 256;
/// What `ref_log.via` records for an undo.
pub const VIA: &str = "undo";

/// Who asks.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The principal id the ref log records.
    pub id: String,
    /// The numeric owner of the caller's user refs, if the caller is a user.
    pub owner: Option<i64>,
    /// Repository admin.
    pub maintainer: bool,
}

/// A `ref_log` row.
#[derive(Clone, Debug)]
pub struct LogRow {
    pub seq: i64,
    pub name: String,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
    pub actor: String,
    pub via: String,
    pub op_id: Uuid,
    pub at: i64,
}

/// Why a row cannot be undone by `caller`, as a short fixed phrase, or `None` when it can.
/// `newest` is the sequence number of the newest row of the same ref, `now` unix seconds,
/// `window` the undo window for protected refs in seconds.
pub fn blocked_reason(
    row: &LogRow,
    newest: i64,
    caller: &Caller,
    now: i64,
    window: i64,
) -> Option<&'static str> {
    if row.old == row.new {
        return Some("nothing to undo");
    }
    if row.actor != caller.id {
        return Some("not your operation");
    }
    if row.seq != newest {
        return Some("the ref changed since");
    }
    if row.name.starts_with(USER_NS) {
        if !caller
            .owner
            .is_some_and(|o| refpolicy::owns_user_ref(o, &row.name))
        {
            return Some("not your ref");
        }
        return None;
    }
    if !caller.maintainer {
        return Some("a protected ref needs a maintainer");
    }
    if window <= 0 {
        return Some("undo of protected refs is off");
    }
    if now.saturating_sub(row.at) > window {
        return Some("outside the undo window of a protected ref");
    }
    None
}

fn target_of(bytes: Option<Vec<u8>>) -> Result<Option<RefTarget>, Status> {
    bytes
        .map(|b| RefTarget::from_bytes(&b).map_err(|e| internal("ref_log", &e)))
        .transpose()
}

fn row_of(r: &AnyRow) -> Result<LogRow, Status> {
    let sql = |e: sqlx::Error| internal("ref_log", &e);
    let op: Vec<u8> = r.try_get("op_id").map_err(sql)?;
    Ok(LogRow {
        seq: r.try_get("seq").map_err(sql)?,
        name: r.try_get("name").map_err(sql)?,
        old: target_of(r.try_get("old").map_err(sql)?)?,
        new: target_of(r.try_get("new").map_err(sql)?)?,
        actor: r.try_get("actor").map_err(sql)?,
        via: r.try_get("via").map_err(sql)?,
        op_id: Uuid::from_slice(&op).map_err(|e| internal("ref_log", &e))?,
        at: r.try_get("at").map_err(sql)?,
    })
}

const ROW_COLUMNS: &str = "seq, name, old, new, actor, via, op_id, at";

/// How a ref is named to `owner`: an own user ref as `refs/heads/<n>`, another user's not at all.
pub fn client_name(owner: Option<i64>, stored: &str) -> Option<String> {
    match refpolicy::parse_user_ref(stored) {
        Some((o, branch)) => (Some(o) == owner).then(|| format!("refs/heads/{branch}")),
        None if stored.starts_with(USER_NS) => None,
        None => Some(stored.to_owned()),
    }
}

enum Bind {
    Text(String),
    Int(i64),
}

/// Adds a bind and returns its placeholder.
fn bind(binds: &mut Vec<Bind>, b: Bind) -> String {
    binds.push(b);
    format!("${}", binds.len())
}

/// The ref log and ref undo of one server.
pub struct RefLog {
    registry: Arc<Registry>,
    hooks: Arc<PipelineHooks>,
    window_secs: i64,
}

impl RefLog {
    pub fn new(registry: Arc<Registry>, hooks: Arc<PipelineHooks>, window_secs: u64) -> Self {
        Self {
            registry,
            hooks,
            window_secs: i64::try_from(window_secs).unwrap_or(i64::MAX),
        }
    }

    fn pool(&self) -> &AnyPool {
        self.registry.pool()
    }

    // -------------------------------------------------------------------------- listing

    /// `Stacks.ListRefLog`.
    pub async fn list(
        &self,
        ctx: &CallCtx,
        caller: &Caller,
        req: wire::ListRefLogRequest,
    ) -> Result<wire::ListRefLogResponse, Status> {
        let bad = |m: &str| Status::invalid_argument(m.to_owned());
        if req.since < 0 || req.until < 0 || (req.until > 0 && req.since >= req.until) {
            return Err(bad("invalid time range"));
        }
        if req.actor.len() > 255 || req.actor.chars().any(char::is_control) {
            return Err(bad("invalid actor"));
        }
        if req.r#ref.len() > 1024 || req.r#ref.chars().any(char::is_control) {
            return Err(bad("invalid ref"));
        }
        let page_scope = format!("Stacks.ListRefLog|{}", ctx.repo);
        let after = crate::pagetoken::open_id(&page_scope, &ctx.principal.id, &req.page_token)?;
        let limit = match req.limit {
            0 => DEFAULT_PAGE,
            n => usize::try_from(n).unwrap_or(MAX_PAGE).min(MAX_PAGE),
        };
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(crate::commit_graph_rpc::registry_status)?;
        let policy = RepoPolicy::load(self.pool(), entry.key.0).await?;

        let mut binds: Vec<Bind> = vec![Bind::Int(entry.key.0)];
        let mut sql = String::from("repo_id = $1");
        // Which refs: one name, a prefix, or all the caller may see.
        if !req.r#ref.is_empty() {
            if let Some(prefix) = req.r#ref.strip_suffix('/') {
                let mut alts = vec![format!("{prefix}/")];
                if let (Some(rest), Some(owner)) =
                    (req.r#ref.strip_prefix("refs/heads/"), caller.owner)
                    && policy.branchless()
                {
                    alts.push(format!("{}heads/{rest}", refpolicy::user_prefix(owner)));
                }
                let mut ors = Vec::new();
                for a in alts {
                    let len = bind(
                        &mut binds,
                        Bind::Int(i64::try_from(a.chars().count()).unwrap_or(0)),
                    );
                    let text = bind(&mut binds, Bind::Text(a));
                    ors.push(format!("substr(name, 1, CAST({len} AS INTEGER)) = {text}"));
                }
                sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
            } else {
                let stored = match policy.classify(caller.owner, &req.r#ref)? {
                    Class::User { stored, .. } => stored,
                    _ => req.r#ref.clone(),
                };
                let p = bind(&mut binds, Bind::Text(stored));
                sql.push_str(&format!(" AND name = {p}"));
            }
        }
        // Other users' refs are never listed.
        let ns_len = bind(
            &mut binds,
            Bind::Int(i64::try_from(USER_NS.len()).unwrap_or(0)),
        );
        let ns = bind(&mut binds, Bind::Text(USER_NS.to_owned()));
        match caller.owner {
            Some(o) => {
                let own = refpolicy::user_prefix(o);
                let len = bind(
                    &mut binds,
                    Bind::Int(i64::try_from(own.chars().count()).unwrap_or(0)),
                );
                let text = bind(&mut binds, Bind::Text(own));
                sql.push_str(&format!(
                    " AND (substr(name, 1, CAST({ns_len} AS INTEGER)) <> {ns} OR substr(name, 1, CAST({len} AS INTEGER)) = {text})"
                ));
            }
            None => sql.push_str(&format!(
                " AND substr(name, 1, CAST({ns_len} AS INTEGER)) <> {ns}"
            )),
        }
        if !req.actor.is_empty() {
            let exact = bind(&mut binds, Bind::Text(req.actor.clone()));
            let suffix = format!(
                "%:{}",
                req.actor
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            let like = bind(&mut binds, Bind::Text(suffix));
            let pat = bind(&mut binds, Bind::Text(format!("pat:{}", req.actor)));
            sql.push_str(&format!(
                " AND (actor = {exact} OR actor = {pat} OR actor LIKE {like} ESCAPE '\\')"
            ));
        }
        if req.since > 0 {
            let p = bind(&mut binds, Bind::Int(req.since));
            sql.push_str(&format!(" AND at >= {p}"));
        }
        if req.until > 0 {
            let p = bind(&mut binds, Bind::Int(req.until));
            sql.push_str(&format!(" AND at < {p}"));
        }
        if let Some(a) = after {
            let p = bind(&mut binds, Bind::Int(a));
            sql.push_str(&format!(" AND seq < {p}"));
        }
        let lim = bind(
            &mut binds,
            Bind::Int(i64::try_from(limit + 1).unwrap_or(i64::MAX)),
        );
        let q =
            format!("SELECT {ROW_COLUMNS} FROM ref_log WHERE {sql} ORDER BY seq DESC LIMIT {lim}");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q));
        for b in binds {
            query = match b {
                Bind::Text(s) => query.bind(s),
                Bind::Int(i) => query.bind(i),
            };
        }
        let rows = query
            .fetch_all(self.pool())
            .await
            .map_err(|e| internal("ref_log", &e))?;
        let mut logs = rows.iter().map(row_of).collect::<Result<Vec<_>, _>>()?;
        let next_page_token = if logs.len() > limit {
            logs.truncate(limit);
            logs.last()
                .map(|r| crate::pagetoken::seal(&page_scope, &ctx.principal.id, &r.seq.to_string()))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let newest = self.newest_seqs(entry.key.0, &logs).await?;
        let now = crate::update::now();
        let entries = logs
            .iter()
            .filter_map(|r| {
                let name = client_name(caller.owner, &r.name)?;
                let newest = newest.get(&r.name).copied().unwrap_or(r.seq);
                let blocked = blocked_reason(r, newest, caller, now, self.window_secs);
                Some(wire::RefLogEntry {
                    seq: u64::try_from(r.seq).unwrap_or(0),
                    r#ref: name,
                    old: r.old.as_ref().map(wire::RefTarget::from),
                    new: r.new.as_ref().map(wire::RefTarget::from),
                    time: r.at,
                    actor: actor_of(&r.actor).name,
                    via: r.via.clone(),
                    op_id: r.op_id.to_string(),
                    undoable: blocked.is_none(),
                    undo_blocked_reason: blocked.unwrap_or_default().to_owned(),
                })
            })
            .collect();
        Ok(wire::ListRefLogResponse {
            entries,
            next_page_token,
        })
    }

    /// The newest `ref_log` sequence number of each ref in `rows`.
    async fn newest_seqs(
        &self,
        repo: i64,
        rows: &[LogRow],
    ) -> Result<HashMap<String, i64>, Status> {
        let mut names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let mut out = HashMap::new();
        for chunk in names.chunks(100) {
            let marks = crate::review::placeholders(2, chunk.len());
            let q = format!(
                "SELECT name, CAST(MAX(seq) AS BIGINT) AS newest FROM ref_log \
                 WHERE repo_id = $1 AND name IN ({marks}) GROUP BY name"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(repo);
            for n in chunk {
                query = query.bind((*n).to_owned());
            }
            for r in query
                .fetch_all(self.pool())
                .await
                .map_err(|e| internal("ref_log", &e))?
            {
                out.insert(
                    r.try_get::<String, _>("name")
                        .map_err(|e| internal("ref_log", &e))?,
                    r.try_get::<i64, _>("newest")
                        .map_err(|e| internal("ref_log", &e))?,
                );
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------------------- undo

    /// A stored answer for `op_id`, if the call already ran.
    async fn stored(
        &self,
        repo: i64,
        op_id: Uuid,
    ) -> Result<Option<wire::UndoRefResponse>, Status> {
        let row = sqlx::query("SELECT repo_id, result FROM op_results WHERE op_id = $1")
            .bind(op_id.as_bytes().to_vec())
            .fetch_optional(self.pool())
            .await
            .map_err(|e| internal("op_results", &e))?;
        let Some(row) = row else { return Ok(None) };
        let stored_repo: i64 = row
            .try_get("repo_id")
            .map_err(|e| internal("op_results", &e))?;
        let result: Vec<u8> = row
            .try_get("result")
            .map_err(|e| internal("op_results", &e))?;
        match result.strip_prefix(OP_TAG.as_slice()) {
            Some(body) if stored_repo == repo => wire::UndoRefResponse::decode(body)
                .map(Some)
                .map_err(|e| internal("op_results", &e)),
            _ => Err(Status::invalid_argument(
                "op id was used for another operation",
            )),
        }
    }

    /// `Stacks.UndoRef`. A refusal for lack of the right is audited as a denied `repo.ref.undo`.
    pub async fn undo(
        &self,
        ctx: &CallCtx,
        caller: &Caller,
        req: wire::UndoRefRequest,
    ) -> Result<wire::UndoRefResponse, Status> {
        let shown = req.r#ref.clone();
        let result = self.undo_checked(ctx, caller, req).await;
        if let Err(e) = &result
            && e.code() == tonic::Code::PermissionDenied
        {
            let event = NewEvent::new("repo.ref.undo", actor_of(&ctx.principal.id))
                .repo(ctx.repo.as_str())
                .target(&shown)
                .outcome(Outcome::Denied)
                .detail("ref", shown.as_str());
            store::record_best_effort(self.pool(), &event).await;
        }
        result
    }

    async fn undo_checked(
        &self,
        ctx: &CallCtx,
        caller: &Caller,
        req: wire::UndoRefRequest,
    ) -> Result<wire::UndoRefResponse, Status> {
        let bad = |m: &str| Status::invalid_argument(m.to_owned());
        let op_id = ctx
            .op_id
            .ok_or_else(|| bad("mutating calls need x-asc-op-id"))?;
        if req.seq == 0 || i64::try_from(req.seq).is_err() {
            return Err(bad("seq is the sequence number of a ref log entry"));
        }
        if req.reason.len() > MAX_REASON {
            return Err(bad("reason is at most 256 bytes"));
        }
        if req.r#ref.is_empty() || req.r#ref.len() > 1024 || req.r#ref.chars().any(char::is_control)
        {
            return Err(bad("invalid ref"));
        }
        let reason = plain(&req.reason);
        let seq = i64::try_from(req.seq).unwrap_or(i64::MAX);
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(crate::commit_graph_rpc::registry_status)?;
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }
        if let Some(done) = self.stored(entry.key.0, op_id).await? {
            return Ok(done);
        }
        let policy = RepoPolicy::load(self.pool(), entry.key.0).await?;
        let stored_name = match policy.classify(caller.owner, &req.r#ref)? {
            Class::User { stored, .. } => stored,
            _ => req.r#ref.clone(),
        };
        let ref_name = RefName::new(stored_name.as_bytes()).map_err(|e| bad(&e.to_string()))?;

        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {ROW_COLUMNS} FROM ref_log WHERE repo_id = $1 AND seq = $2"
        )))
        .bind(entry.key.0)
        .bind(seq)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| internal("ref_log", &e))?;
        let log = match row.as_ref().map(row_of).transpose()? {
            Some(l) if l.name == stored_name => l,
            Some(_) => return Err(Status::not_found("no such ref log entry for this ref")),
            None => {
                let oldest: Option<i64> =
                    sqlx::query_scalar("SELECT MIN(seq) FROM ref_log WHERE repo_id = $1")
                        .bind(entry.key.0)
                        .fetch_one(self.pool())
                        .await
                        .map_err(|e| internal("ref_log", &e))?;
                return Err(match oldest {
                    Some(o) if seq < o => {
                        rpc(RpcError::RefLogExpired(info(vec![req.r#ref.clone()])))
                    }
                    _ => Status::not_found("no such ref log entry for this ref"),
                });
            }
        };
        let newest = self
            .newest_seqs(entry.key.0, std::slice::from_ref(&log))
            .await?
            .get(&log.name)
            .copied()
            .unwrap_or(log.seq);
        let now = crate::update::now();
        if let Some(why) = blocked_reason(&log, newest, caller, now, self.window_secs) {
            return Err(refusal(why, &req.r#ref));
        }
        // What the ref holds now, and what it will hold.
        let current = self
            .registry
            .refs()
            .get(entry.key, &ref_name)
            .await
            .map_err(|e| internal("refs", &e))?
            .map(|e| e.target);
        if current != log.new {
            return Err(rpc(RpcError::VersionConflict(info(vec![
                req.r#ref.clone(),
            ]))));
        }
        let restored = log.old;
        if let Some(RefTarget::Changeset(c)) = restored {
            let present = entry
                .objects
                .has_many(&[ObjKey::Changeset(c)])
                .await
                .map_err(|e| internal("objects", &e))?;
            if present != [true] {
                return Err(Status::failed_precondition(
                    "the value to restore is no longer in the repository",
                ));
            }
        }
        let result = wire::RefResult {
            name: req.r#ref.clone(),
            old: current.as_ref().map(wire::RefTarget::from),
            new: restored.as_ref().map(wire::RefTarget::from),
            user: stored_name.starts_with(USER_NS),
        };
        if req.dry_run {
            return Ok(wire::UndoRefResponse {
                result: Some(result),
                new_seq: 0,
                applied: false,
            });
        }

        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: ref_name.clone(),
                expect: current.map_or(Expect::Absent, Expect::Value),
                new: restored,
                expires_at: None,
            }],
            actor: RefActor {
                id: ctx.principal.id.clone(),
                via: VIA.into(),
            },
            reason: format!("undo {seq}: {reason}"),
            op_id,
        };
        let mut response = wire::UndoRefResponse {
            result: Some(result),
            new_seq: 0,
            applied: true,
        };
        match self
            .commit(
                &entry,
                &txn,
                &log,
                &reason,
                seq,
                &stored_name,
                &mut response,
            )
            .await
        {
            Ok(()) => Ok(response),
            Err(e) => {
                // A retry of this op that committed in the meantime is not an error.
                if let Some(done) = self.stored(entry.key.0, op_id).await? {
                    return Ok(done);
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit(
        &self,
        entry: &RepoEntry,
        txn: &RefTxn,
        log: &LogRow,
        reason: &str,
        seq: i64,
        stored_name: &str,
        response: &mut wire::UndoRefResponse,
    ) -> Result<(), Status> {
        let refs = self.registry.refs();
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        self.registry.guard(&mut tx, entry).await?;
        // The row must still be the newest of its ref in this transaction.
        let newest: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(MAX(seq) AS BIGINT) FROM ref_log WHERE repo_id = $1 AND name = $2",
        )
        .bind(entry.key.0)
        .bind(stored_name)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| internal("ref_log", &e))?;
        if newest != Some(log.seq) {
            return Err(rpc(RpcError::VersionConflict(info(vec![
                stored_name.to_owned(),
            ]))));
        }
        match refs.update_in_tx(&mut tx, entry.key, txn).await {
            Ok(()) => {}
            Err(RefError::Conflict(c)) => return Err(conflict(c)),
            Err(e @ RefError::NonUtf8Name(_)) => {
                return Err(Status::invalid_argument(e.to_string()));
            }
            Err(e) => return Err(internal("refs", &e)),
        }
        refpolicy::check_user_ref_writes(
            &mut tx,
            entry.key,
            txn,
            UserRefWriter::Client,
            self.hooks.user_quota().max_user_refs(),
        )
        .await?;
        let was_land = unwind_land(&mut tx, entry, log, stored_name).await?;
        let new_seq: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(MAX(seq) AS BIGINT) FROM ref_log WHERE repo_id = $1 AND op_id = $2",
        )
        .bind(entry.key.0)
        .bind(txn.op_id.as_bytes().to_vec())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| internal("ref_log", &e))?;
        response.new_seq = u64::try_from(new_seq.unwrap_or(0)).unwrap_or(0);

        let at = crate::update::now();
        let mut stored = OP_TAG.to_vec();
        stored.extend_from_slice(&response.encode_to_vec());
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(txn.op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(stored)
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("op_results", &e))?;
        let shared = !stored_name.starts_with(USER_NS);
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: if shared {
                self.hooks.push_hooks().outbox_effects()
            } else {
                &[]
            },
            pusher: &txn.actor.id,
            refs: if shared {
                vec![EffectRef {
                    name: stored_name.to_owned(),
                    old: log.new.as_ref().map(target_hex),
                    new: log.old.as_ref().map(target_hex),
                }]
            } else {
                Vec::new()
            },
        })
        .map_err(|e| internal("outbox", &e))?;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(entry.key.0)
        .bind(txn.op_id.as_bytes().to_vec())
        .bind(outbox)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("outbox", &e))?;
        let event = NewEvent::new("repo.ref.undo", actor_of(&txn.actor.id))
            .repo(entry.name.as_str())
            .target(
                &response
                    .result
                    .as_ref()
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
            )
            .detail(
                "ref",
                response
                    .result
                    .as_ref()
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
            )
            .detail("old", log.new.as_ref().map(target_hex).unwrap_or_default())
            .detail("new", log.old.as_ref().map(target_hex).unwrap_or_default())
            .detail("undone_seq", seq.to_string())
            .detail("undone_actor", actor_of(&log.actor).name)
            .detail("undone_op", log.op_id.to_string())
            .detail("reason", reason)
            .detail("op_id", txn.op_id.to_string());
        store::append_in_tx(&mut tx, &event)
            .await
            .map_err(|e| internal("audit", &e))?;
        tx.commit().await.map_err(|e| internal("commit", &e))?;
        tracing::info!(repo = %entry.name, seq, land = was_land, "ref update undone");
        Ok(())
    }
}

/// The error for a blocked entry. The reason is one of the fixed phrases of [`blocked_reason`].
fn refusal(why: &'static str, shown: &str) -> Status {
    match why {
        "not your operation" | "not your ref" => {
            Status::permission_denied(format!("{why}: only the actor of an update can undo it"))
        }
        "the ref changed since" => rpc(RpcError::VersionConflict(info(vec![shown.to_owned()]))),
        "nothing to undo" => Status::failed_precondition(why),
        _ => {
            let typed = rpc(RpcError::RefUndoProtected(info(vec![shown.to_owned()])));
            // The typed code and details stay; the message says which rule applied.
            Status::with_details_and_metadata(
                typed.code(),
                format!("{why} (Stacks.RevertLanded makes new changes instead)"),
                Bytes::copy_from_slice(typed.details()),
                typed.metadata().clone(),
            )
        }
    }
}

/// Removes what a land left besides the ref: the landing map rows of the update, the mainline
/// index above the restored tip and the landed markers set at that time. Returns whether the
/// undone update was a land.
async fn unwind_land(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    entry: &RepoEntry,
    log: &LogRow,
    stored_name: &str,
) -> Result<bool, Status> {
    let ns = entry.ns.as_bytes().to_vec();
    let sql = |e: sqlx::Error| internal("land records", &e);
    let mut removed = 0;
    for table in ["land_map_ref", "land_map"] {
        let q = format!("DELETE FROM {table} WHERE ns = $1 AND ref = $2 AND op_id = $3");
        removed += sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(ns.clone())
            .bind(stored_name)
            .bind(log.op_id.as_bytes().to_vec())
            .execute(&mut **tx)
            .await
            .map_err(sql)?
            .rows_affected();
    }
    if removed == 0 {
        return Ok(false);
    }
    // The branch index goes back to the restored tip; when that tip is not indexed (or the ref
    // did not exist) the branch index is emptied and the next land rebuilds it.
    let base = match log.old {
        Some(RefTarget::Changeset(c)) => Some(c),
        _ => None,
    };
    let applied = match base {
        Some(_) => mainline::apply(tx, entry.ns, stored_name, base, &[]).await,
        None => Err(mainline::Error::Corrupt("no restored tip")),
    };
    if applied.is_err() {
        mainline::apply(tx, entry.ns, stored_name, None, &[])
            .await
            .map_err(|e| internal("mainline", &e))?;
    }
    if let Some(owner) = refpolicy::owner_of(&asc_rpc::auth::Principal {
        id: log.actor.clone(),
    }) {
        sqlx::query(
            "UPDATE user_ref_meta SET landed_at = NULL WHERE ns = $1 AND owner = $2 AND landed_at >= $3",
        )
        .bind(ns)
        .bind(owner)
        .bind(log.at.saturating_sub(5))
        .execute(&mut **tx)
        .await
        .map_err(sql)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::id::ChangesetId;

    fn cs(n: u8) -> RefTarget {
        RefTarget::Changeset(ChangesetId::from_bytes([n; 32]))
    }

    fn row(name: &str, actor: &str, at: i64) -> LogRow {
        LogRow {
            seq: 5,
            name: name.into(),
            old: Some(cs(1)),
            new: Some(cs(2)),
            actor: actor.into(),
            via: "asc-push".into(),
            op_id: Uuid::nil(),
            at,
        }
    }

    fn me(maintainer: bool) -> Caller {
        Caller {
            id: "7:alice".into(),
            owner: Some(7),
            maintainer,
        }
    }

    #[test]
    fn a_user_ref_is_undone_by_its_owner_only() {
        let own = row("refs/users/7/heads/x", "7:alice", 100);
        assert_eq!(blocked_reason(&own, 5, &me(false), 100_000, 600), None);
        let theirs = row("refs/users/7/heads/x", "8:bob", 100);
        assert_eq!(
            blocked_reason(&theirs, 5, &me(true), 100, 600),
            Some("not your operation")
        );
        // Alice's own operation on somebody else's ref cannot happen, and is refused anyway.
        let other = row("refs/users/9/heads/x", "7:alice", 100);
        assert_eq!(
            blocked_reason(&other, 5, &me(true), 100, 600),
            Some("not your ref")
        );
    }

    #[test]
    fn a_moved_ref_is_refused() {
        let r = row("refs/users/7/heads/x", "7:alice", 100);
        assert_eq!(
            blocked_reason(&r, 6, &me(false), 100, 600),
            Some("the ref changed since")
        );
    }

    #[test]
    fn a_noop_has_nothing_to_undo() {
        let mut r = row("refs/users/7/heads/x", "7:alice", 100);
        r.new = r.old;
        assert_eq!(
            blocked_reason(&r, 5, &me(false), 100, 600),
            Some("nothing to undo")
        );
    }

    #[test]
    fn a_protected_ref_needs_a_maintainer_and_the_window() {
        let r = row("refs/heads/master", "7:alice", 1_000);
        assert_eq!(
            blocked_reason(&r, 5, &me(false), 1_010, 600),
            Some("a protected ref needs a maintainer")
        );
        assert_eq!(blocked_reason(&r, 5, &me(true), 1_600, 600), None);
        assert_eq!(
            blocked_reason(&r, 5, &me(true), 1_601, 600),
            Some("outside the undo window of a protected ref")
        );
        assert_eq!(
            blocked_reason(&r, 5, &me(true), 1_001, 0),
            Some("undo of protected refs is off")
        );
        // The actor still has to be the caller on a protected ref.
        let theirs = row("refs/heads/master", "8:bob", 1_000);
        assert_eq!(
            blocked_reason(&theirs, 5, &me(true), 1_001, 600),
            Some("not your operation")
        );
    }

    #[test]
    fn refs_are_named_to_their_owner() {
        assert_eq!(
            client_name(Some(7), "refs/users/7/heads/feat/x").as_deref(),
            Some("refs/heads/feat/x")
        );
        assert_eq!(client_name(Some(7), "refs/users/8/heads/x"), None);
        assert_eq!(client_name(None, "refs/users/8/heads/x"), None);
        assert_eq!(
            client_name(Some(7), "refs/heads/master").as_deref(),
            Some("refs/heads/master")
        );
        assert_eq!(client_name(Some(7), "refs/users/junk"), None);
    }
}
