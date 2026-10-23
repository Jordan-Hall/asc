//! The quota ledger (SPEC §21.3.7): `cloud_usage(ns, owner, bytes, workspaces)`.
//!
//! User-ref pushes and cloud ingest skip pre-receive, so the server charges bytes itself:
//! `PutObjects`, `Ingest` and `ImportGitPush` call [`Quota::charge`] inside their transaction
//! (a rollback refunds). Every counter moves by one conditional upsert, so a cap holds under
//! concurrent callers on both SQLite and Postgres without a read-then-write race. The forge's own
//! quota API is consulted through [`ForgeQuota`] before the local ledger moves.

use std::sync::Arc;

use async_trait::async_trait;
use asc_rpc::RpcError;
use asc_rpc::errors::ErrorInfo;
use asc_rpc::server::rpc_status;
use sqlx::AnyConnection;
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
