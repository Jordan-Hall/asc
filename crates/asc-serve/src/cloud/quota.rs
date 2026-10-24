//! The quota ledger (SPEC §21.3.7): `cloud_usage(ns, owner, bytes, workspaces)`.
//!
//! User-ref pushes and cloud ingest skip pre-receive, so the server charges bytes itself:
//! `PutObjects`, `Ingest` and `ImportGitPush` call [`Quota::charge`] inside their transaction
//! (a rollback refunds). Every counter moves by one conditional upsert, so a cap holds under
//! concurrent callers on both SQLite and Postgres without a read-then-write race. The forge's own
//! quota API is consulted through [`ForgeQuota`] before the local ledger moves.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::object::{Change, Changeset, TagRecord};
use asc_rpc::RpcError;
use asc_rpc::errors::ErrorInfo;
use asc_rpc::server::rpc_status;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use sqlx::{AnyConnection, AnyPool, Row};
use tonic::Status;

use super::Limits;
use crate::registry::Ns;

/// Largest cap honoured, so `bytes + charge` can never overflow an `i64`.
const MAX_CAP: u64 = 1 << 61;

/// The forge's quota check (Forgejo quota API). Asked before the local ledger moves.
#[async_trait]
pub trait ForgeQuota: Send + Sync {
    /// Whether `owner` may store `bytes` more. An `Err` means the forge could not answer; the
    /// charge then fails closed with `UNAVAILABLE`.
    async fn admit(&self, owner: i64, bytes: u64) -> Result<bool, String>;
}

/// No forge quota: only the local ledger applies.
pub struct NoForgeQuota;

#[async_trait]
impl ForgeQuota for NoForgeQuota {
    async fn admit(&self, _owner: i64, _bytes: u64) -> Result<bool, String> {
        Ok(true)
    }
}

/// The ledger and its caps.
#[derive(Clone)]
pub struct Quota {
    limits: Limits,
    forge: Arc<dyn ForgeQuota>,
}

fn exceeded(what: &str) -> Status {
    let mut info = ErrorInfo::default();
    info.paths.push(what.to_owned());
    rpc_status(&RpcError::QuotaExceeded(info))
}

fn workspaces_limit() -> Status {
    rpc_status(&RpcError::CloudWorkspacesLimit(ErrorInfo::default()))
}

fn db(context: &str, e: &sqlx::Error) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn owner_ok(owner: i64) -> Result<(), Status> {
    if owner > 0 {
        Ok(())
    } else {
        Err(Status::permission_denied("quota needs a user identity"))
    }
}

impl Quota {
    pub fn new(limits: Limits, forge: Arc<dyn ForgeQuota>) -> Self {
        Self { limits, forge }
    }

    /// Local ledger only.
    pub fn local(limits: Limits) -> Self {
        Self::new(limits, Arc::new(NoForgeQuota))
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Charges `bytes` to `owner` in `ns`; `QUOTA_EXCEEDED` when the cap would be crossed (and
    /// nothing is charged). Call it inside the transaction that stores the bytes.
    pub async fn charge(
        &self,
        conn: &mut AnyConnection,
        ns: Ns,
        owner: i64,
        bytes: u64,
    ) -> Result<(), Status> {
        owner_ok(owner)?;
        let cap = self.limits.max_user_bytes.min(MAX_CAP);
        if bytes > cap {
            return Err(exceeded("user byte quota"));
        }
        if bytes == 0 {
            return Ok(());
        }
        match self.forge.admit(owner, bytes).await {
            Ok(true) => {}
            Ok(false) => return Err(exceeded("forge quota")),
            Err(e) => {
                tracing::warn!(error = %e, "forge quota API failed");
                return Err(Status::unavailable("quota service unavailable"));
            }
        }
        // Both fit in an i64: `bytes <= cap <= 2^61`.
        let (bytes, cap) = (bytes as i64, cap as i64);
        let done = sqlx::query(
            "INSERT INTO cloud_usage (ns, owner, bytes, workspaces) VALUES ($1, $2, $3, 0) \
             ON CONFLICT (ns, owner) DO UPDATE SET bytes = cloud_usage.bytes + $3 \
             WHERE cloud_usage.bytes + $3 <= $4",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(owner)
        .bind(bytes)
        .bind(cap)
        .execute(&mut *conn)
        .await
        .map_err(|e| db("cloud_usage", &e))?;
        if done.rows_affected() == 0 {
            return Err(exceeded("user byte quota"));
        }
        Ok(())
    }

    /// Returns `bytes` to `owner` (a purge or an expiry). The counter never goes below zero.
    pub async fn credit(
        &self,
        conn: &mut AnyConnection,
        ns: Ns,
        owner: i64,
        bytes: u64,
    ) -> Result<(), Status> {
        owner_ok(owner)?;
        let bytes = bytes.min(MAX_CAP) as i64;
        sqlx::query(
            "UPDATE cloud_usage SET bytes = CASE WHEN bytes > $3 THEN bytes - $3 ELSE 0 END \
             WHERE ns = $1 AND owner = $2",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(owner)
        .bind(bytes)
        .execute(&mut *conn)
        .await
        .map_err(|e| db("cloud_usage", &e))?;
        Ok(())
    }

    /// Counts one new workspace for `owner`; `CLOUD_WORKSPACES_LIMIT` at the cap.
    pub async fn claim_workspace(
        &self,
        conn: &mut AnyConnection,
        ns: Ns,
        owner: i64,
    ) -> Result<(), Status> {
        owner_ok(owner)?;
        let max = i64::from(self.limits.max_workspaces_per_user);
        if max == 0 {
            return Err(workspaces_limit());
        }
        let done = sqlx::query(
            "INSERT INTO cloud_usage (ns, owner, bytes, workspaces) VALUES ($1, $2, 0, 1) \
             ON CONFLICT (ns, owner) DO UPDATE SET workspaces = cloud_usage.workspaces + 1 \
             WHERE cloud_usage.workspaces < $3",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(owner)
        .bind(max)
        .execute(&mut *conn)
        .await
        .map_err(|e| db("cloud_usage", &e))?;
        if done.rows_affected() == 0 {
            return Err(workspaces_limit());
        }
        Ok(())
    }

    /// Uncounts a workspace that was deleted.
    pub async fn release_workspace(
        &self,
        conn: &mut AnyConnection,
        ns: Ns,
        owner: i64,
    ) -> Result<(), Status> {
        owner_ok(owner)?;
        sqlx::query(
            "UPDATE cloud_usage SET workspaces = workspaces - 1 \
             WHERE ns = $1 AND owner = $2 AND workspaces > 0",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(owner)
        .execute(&mut *conn)
        .await
        .map_err(|e| db("cloud_usage", &e))?;
        Ok(())
    }
}

// ------------------------------------------------------------------------------ user quota

/// Bytes charged per changeset, whatever it contains (its object and its graph row).
pub const CHANGESET_BASE_COST: u64 = 512;
/// Bytes charged per entry of a changeset's `changes`, on top of the path and the content.
pub const CHANGE_BASE_COST: u64 = 64;
/// Most unsettled changesets one walk collects.
const MAX_WALK: usize = 200_000;
/// Ids per `IN (...)` lookup of a walk.
const WALK_BATCH: usize = 400;
/// Rows per multi-row insert.
const INSERT_BATCH: usize = 200;

/// The logical cost of one changeset (SPEC §21.3.7): a fixed charge, its message, and for each
/// change a fixed charge, the path and, for a write, the file's size. Computed from the stored
/// object, whose `size`s ingest has verified against `file_meta`, so a client cannot understate it.
pub fn changeset_cost(cs: &Changeset) -> u64 {
    let mut total = CHANGESET_BASE_COST.saturating_add(cs.message.len() as u64);
    for (path, change) in cs.changes.as_slice() {
        total = total
            .saturating_add(CHANGE_BASE_COST)
            .saturating_add(path.as_bytes().len() as u64);
        if let Change::Write { size, .. } = change {
            total = total.saturating_add(*size);
        }
    }
    total
}

fn placeholders(n: usize, from: usize) -> String {
    (0..n)
        .map(|i| format!("${}", from + i))
        .collect::<Vec<_>>()
        .join(",")
}

fn clamp(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// Records what an ingest or import added: one `cs_ledger` row per new changeset, with its cost
/// and the verified owner id of the principal that ingested it (0 for none). Runs in the
/// transaction that inserts the graph rows, so a changeset is never in the graph without a row
/// (and so never free to adopt). A row that exists is left alone.
pub async fn record(
    conn: &mut AnyConnection,
    ns: Ns,
    ingester: i64,
    rows: &[(ChangesetId, u64)],
) -> Result<(), sqlx::Error> {
    for (id, bytes) in rows {
        sqlx::query(
            "INSERT INTO cs_ledger (ns, cs, bytes, ingester, shared) VALUES ($1, $2, $3, $4, 0) \
             ON CONFLICT (ns, cs) DO NOTHING",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(id.as_bytes().to_vec())
        .bind(clamp(*bytes))
        .bind(ingester)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The ledgered changesets reachable from `roots` that are neither shared nor claimed by `owner`
/// (`0` claims nothing), with their costs. The walk follows `cs_graph` parents and stops at a
/// shared or claimed changeset (everything below it is settled already) and at one with no ledger
/// row (written by land or mirror, or before the ledger: free). No object is read.
async fn unsettled(
    conn: &mut AnyConnection,
    ns: Ns,
    owner: i64,
    roots: &[ChangesetId],
) -> Result<Vec<(ChangesetId, u64)>, Status> {
    let mut seen: HashSet<ChangesetId> = roots.iter().copied().collect();
    let mut frontier: Vec<ChangesetId> = seen.iter().copied().collect();
    let mut out = Vec::new();
    while !frontier.is_empty() {
        let at = frontier.len().saturating_sub(WALK_BATCH);
        let batch = frontier.split_off(at);
        let sql = format!(
            "SELECT l.cs AS cs, l.bytes AS bytes, CAST(l.shared AS BIGINT) AS shared, \
             g.parents AS parents, \
             (SELECT COUNT(*) FROM cs_claim c WHERE c.ns = l.ns AND c.owner = $2 AND c.cs = l.cs) \
             AS claimed \
             FROM cs_ledger l JOIN cs_graph g ON g.ns = l.ns AND g.cs = l.cs \
             WHERE l.ns = $1 AND l.cs IN ({})",
            placeholders(batch.len(), 3)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ns.as_bytes().to_vec())
            .bind(owner);
        for id in &batch {
            q = q.bind(id.as_bytes().to_vec());
        }
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| db("cs_ledger", &e))?;
        for row in rows {
            let bad = || Status::internal("internal error");
            let cs: Vec<u8> = row.try_get("cs").map_err(|e| db("cs_ledger", &e))?;
            let bytes: i64 = row.try_get("bytes").map_err(|e| db("cs_ledger", &e))?;
            let shared: i64 = row.try_get("shared").map_err(|e| db("cs_ledger", &e))?;
            let claimed: i64 = row.try_get("claimed").map_err(|e| db("cs_ledger", &e))?;
            let parents: Vec<u8> = row.try_get("parents").map_err(|e| db("cs_graph", &e))?;
            if shared != 0 || claimed != 0 {
                continue;
            }
            if out.len() >= MAX_WALK {
                return Err(exceeded("too many unclaimed changesets"));
            }
            let id = <[u8; 32]>::try_from(cs.as_slice())
                .map(ChangesetId::from_bytes)
                .map_err(|_| bad())?;
            out.push((id, u64::try_from(bytes).unwrap_or(0)));
            if !parents.len().is_multiple_of(32) {
                return Err(bad());
            }
            for p in parents.chunks_exact(32) {
                let p = ChangesetId::from_bytes(p.try_into().expect("chunk is 32 bytes"));
                if seen.insert(p) {
                    frontier.push(p);
                }
            }
        }
    }
    Ok(out)
}

/// Marks the closure of `roots` shared: a shared ref made it reachable, so it costs no user
/// anything from now on. Runs in the transaction that moves the shared ref.
pub async fn mark_shared(
    conn: &mut AnyConnection,
    ns: Ns,
    roots: &[ChangesetId],
) -> Result<(), Status> {
    let ids = unsettled(conn, ns, 0, roots).await?;
    for chunk in ids.chunks(WALK_BATCH) {
        let sql = format!(
            "UPDATE cs_ledger SET shared = 1 WHERE ns = $1 AND cs IN ({})",
            placeholders(chunk.len(), 2)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.as_bytes().to_vec());
        for (id, _) in chunk {
            q = q.bind(id.as_bytes().to_vec());
        }
        q.execute(&mut *conn)
            .await
            .map_err(|e| db("cs_ledger", &e))?;
    }
    Ok(())
}

/// The changesets a set of ref targets name: a changeset is its own root, a tag its target.
pub async fn roots_of<O: Objects>(
    objects: &O,
    targets: &[RefTarget],
) -> Result<Vec<ChangesetId>, Status> {
    let internal = |what: &str, e: &dyn std::fmt::Display| {
        tracing::error!(error = %e, what, "quota root");
        Status::internal("internal error")
    };
    let mut roots = Vec::with_capacity(targets.len());
    for t in targets {
        match *t {
            RefTarget::Changeset(c) => roots.push(c),
            RefTarget::Tag(t) => {
                let data = objects
                    .get(&ObjKey::Tag(t))
                    .await
                    .map_err(|e| internal("tag read", &e))?
                    .ok_or_else(|| internal("tag read", &"the tag is not stored"))?;
                let tag: TagRecord =
                    decode_envelope(&data).map_err(|e| internal("tag decode", &e))?;
                roots.push(tag.target);
            }
        }
    }
    Ok(roots)
}

/// The per-user quota of user-ref pushes and cloud ingest (SPEC §21.3.7): `[cloud] max_user_bytes`
/// through the [`Quota`] ledger, and `[cloud] max_user_refs`. The pipeline owns one, so no push
/// hook can leave a user-ref push uncharged.
#[derive(Clone)]
pub struct UserQuota {
    quota: Quota,
    max_user_refs: usize,
}

static DEFAULT: LazyLock<UserQuota> = LazyLock::new(UserQuota::default);

impl Default for UserQuota {
    /// The SPEC defaults (1,000 refs, 10 GiB), local ledger only.
    fn default() -> Self {
        Self::new(
            Quota::local(Limits::default()),
            crate::refauthz::DEFAULT_MAX_USER_REFS,
        )
    }
}

impl UserQuota {
    pub fn new(quota: Quota, max_user_refs: usize) -> Self {
        Self {
            quota,
            max_user_refs,
        }
    }

    /// The defaults, shared.
    pub fn defaults() -> &'static Self {
        &DEFAULT
    }

    /// `[cloud] max_user_refs`: user refs per user per repository.
    pub fn max_user_refs(&self) -> usize {
        self.max_user_refs
    }

    pub fn quota(&self) -> &Quota {
        &self.quota
    }

    /// Asked BEFORE anything is ingested: the bytes `owner` may still add in `ns`, or
    /// `QUOTA_EXCEEDED` when none are left. The ingest refuses a push that needs more than this
    /// before it records a row.
    pub async fn remaining(&self, pool: &AnyPool, ns: Ns, owner: i64) -> Result<u64, Status> {
        owner_ok(owner)?;
        let cap = self.quota.limits().max_user_bytes.min(MAX_CAP);
        let used: Option<i64> =
            sqlx::query_scalar("SELECT bytes FROM cloud_usage WHERE ns = $1 AND owner = $2")
                .bind(ns.as_bytes().to_vec())
                .bind(owner)
                .fetch_optional(pool)
                .await
                .map_err(|e| db("cloud_usage", &e))?;
        let used = u64::try_from(used.unwrap_or(0)).unwrap_or(0);
        if used >= cap {
            return Err(exceeded("user byte quota"));
        }
        Ok(cap - used)
    }

    /// Charges `owner` for what a user ref newly makes reachable: every changeset in the closure
    /// of `roots` that is not shared and that `owner` has not paid for, whoever ingested it. The
    /// bytes go through [`Quota::charge`] and the claims are recorded in the same transaction, so
    /// a refusal, or a rollback of the ref update, charges nothing. Returns the bytes charged.
    pub async fn claim(
        &self,
        conn: &mut AnyConnection,
        ns: Ns,
        owner: i64,
        roots: &[ChangesetId],
    ) -> Result<u64, Status> {
        owner_ok(owner)?;
        let owed = unsettled(conn, ns, owner, roots).await?;
        let total = owed.iter().fold(0u64, |a, (_, b)| a.saturating_add(*b));
        self.quota.charge(conn, ns, owner, total).await?;
        for chunk in owed.chunks(INSERT_BATCH) {
            let rows = (0..chunk.len())
                .map(|i| format!("($1, $2, ${})", i + 3))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "INSERT INTO cs_claim (ns, owner, cs) VALUES {rows} ON CONFLICT DO NOTHING"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(ns.as_bytes().to_vec())
                .bind(owner);
            for (id, _) in chunk {
                q = q.bind(id.as_bytes().to_vec());
            }
            q.execute(&mut *conn)
                .await
                .map_err(|e| db("cs_claim", &e))?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::*;
    use crate::ingest::testutil::{registry, repo};

    fn code(s: &Status) -> String {
        asc_rpc::server::status_rpc_error(s)
            .map(|e| format!("{e:?}"))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn put_objects_over_quota_gets_quota_exceeded_and_charges_nothing() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let q = Quota::local(Limits {
            max_user_bytes: 100,
            ..Limits::default()
        });
        let mut c = reg.pool().acquire().await.unwrap();
        q.charge(&mut c, e.ns, 7, 60).await.unwrap();
        let err = q.charge(&mut c, e.ns, 7, 41).await.unwrap_err();
        assert!(code(&err).starts_with("QuotaExceeded"), "{err:?}");
        // Exactly at the cap is allowed; the refused charge left the ledger alone.
        q.charge(&mut c, e.ns, 7, 40).await.unwrap();
        assert!(q.charge(&mut c, e.ns, 7, 1).await.is_err());
        // One user's usage never touches another's.
        q.charge(&mut c, e.ns, 8, 100).await.unwrap();
        // A single charge above the cap is refused even for a fresh user.
        assert!(q.charge(&mut c, e.ns, 9, 101).await.is_err());
        let used: i64 = sqlx::query("SELECT bytes FROM cloud_usage WHERE ns = $1 AND owner = 7")
            .bind(e.ns.as_bytes().to_vec())
            .fetch_one(&mut *c)
            .await
            .unwrap()
            .get("bytes");
        assert_eq!(used, 100);
        q.credit(&mut c, e.ns, 7, 1_000).await.unwrap();
        q.charge(&mut c, e.ns, 7, 100).await.unwrap();
    }

    #[tokio::test]
    async fn an_identity_is_required_and_the_forge_can_refuse() {
        struct Deny;
        #[async_trait]
        impl ForgeQuota for Deny {
            async fn admit(&self, owner: i64, _: u64) -> Result<bool, String> {
                if owner == 1 {
                    Ok(false)
                } else {
                    Err("down".into())
                }
            }
        }
        let reg = registry().await;
        let e = repo(&reg).await;
        let q = Quota::new(Limits::default(), Arc::new(Deny));
        let mut c = reg.pool().acquire().await.unwrap();
        assert_eq!(
            q.charge(&mut c, e.ns, 0, 1).await.unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
        let err = q.charge(&mut c, e.ns, 1, 1).await.unwrap_err();
        assert!(code(&err).starts_with("QuotaExceeded"));
        assert_eq!(
            q.charge(&mut c, e.ns, 2, 1).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
    }

    #[tokio::test]
    async fn the_seventeenth_workspace_is_refused() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let q = Quota::local(Limits::default());
        let mut c = reg.pool().acquire().await.unwrap();
        for _ in 0..16 {
            q.claim_workspace(&mut c, e.ns, 5).await.unwrap();
        }
        let err = q.claim_workspace(&mut c, e.ns, 5).await.unwrap_err();
        assert!(code(&err).starts_with("CloudWorkspacesLimit"), "{err:?}");
        q.release_workspace(&mut c, e.ns, 5).await.unwrap();
        q.claim_workspace(&mut c, e.ns, 5).await.unwrap();
        q.claim_workspace(&mut c, e.ns, 6).await.unwrap();
    }
}
