//! `change_versions`: the version log of each change (SPEC §20.4, §20.5 flow 1).
//!
//! A version is the changeset a change had when it was published. Versions are GC roots (§3.6),
//! so a reviewer can always diff against an earlier one, and they never appear in `ListRefs`.
//!
//! [`append`] is the lease of flow 1: a new version is accepted only if the latest stored version
//! of the change is in the new changeset's `preds` closure (`CHANGE_DIVERGED` otherwise, unless
//! `force`). The insert is a compare-and-swap on the primary key `(ns, change, v)`, so two
//! concurrent publishes cannot both become version `v`. The walk over `preds` is bounded.

use std::collections::HashSet;

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_store::gc::{GcError, GcRootSource};
use asc_store::key::ObjKey;
use async_trait::async_trait;
use sqlx::any::Any;
use sqlx::{AnyConnection, AnyPool, Row, Transaction};

use super::fixed;
use super::index::rows_of;
use crate::registry::Ns;

/// Most changesets the `preds` walk of the lease visits.
pub const MAX_PREDS_WALK: usize = 4_096;
/// Largest page `list` returns.
pub const MAX_PAGE: usize = 1_000;
/// Longest actor string stored.
pub const MAX_ACTOR: usize = 256;
/// Longest op id stored.
pub const MAX_OP_ID: usize = 64;
/// Roots read per query by [`VersionRoots`].
const ROOT_PAGE: usize = 5_000;

/// Why a version read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("versions: database: {0}")]
    Sql(#[from] sqlx::Error),
    /// The latest version of the change is not in the new changeset's `preds` closure.
    #[error("change {change} moved on: its latest version is {latest}")]
    Diverged {
        change: ChangeId,
        latest: ChangesetId,
    },
    #[error("versions: {0}")]
    Invalid(&'static str),
    #[error("versions: damaged row: {0}")]
    Corrupt(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for tonic::Status {
    fn from(e: Error) -> Self {
        match e {
            Error::Diverged { latest, .. } => {
                crate::update::rpc(RpcError::ChangeDiverged(ErrorInfo {
                    actual: bytes::Bytes::copy_from_slice(latest.as_bytes()),
                    ..ErrorInfo::default()
                }))
            }
            Error::Invalid(m) => tonic::Status::invalid_argument(m),
            e => tonic::Status::internal(e.to_string()),
        }
    }
}

/// One stored version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub change: ChangeId,
    pub v: u32,
    pub cs: ChangesetId,
    pub parent_change: Option<ChangeId>,
    pub at: i64,
    pub actor: String,
    pub op_id: Vec<u8>,
}

/// A version to publish.
#[derive(Clone, Debug)]
pub struct NewVersion<'a> {
    pub change: ChangeId,
    pub cs: ChangesetId,
    pub parent_change: Option<ChangeId>,
    pub at: i64,
    pub actor: &'a str,
    pub op_id: &'a [u8],
    /// `--force`: skip the lease (the caller has already authorised it).
    pub force: bool,
}

/// What [`append`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appended {
    pub v: u32,
    /// False when the latest version already was this changeset (a retry).
    pub created: bool,
}

const COLUMNS: &str = "change, CAST(v AS BIGINT) AS v, cs, parent_change, at, actor, op_id";

fn version_of(r: &sqlx::any::AnyRow) -> Result<Version> {
    let parent: Option<Vec<u8>> = r.try_get("parent_change")?;
    let v: i64 = r.try_get("v")?;
    Ok(Version {
        change: ChangeId::from_bytes(fixed(r, "change").ok_or(Error::Corrupt("change"))?),
        v: u32::try_from(v).map_err(|_| Error::Corrupt("v"))?,
        cs: ChangesetId::from_bytes(fixed(r, "cs").ok_or(Error::Corrupt("cs"))?),
        parent_change: parent
            .map(|p| {
                <[u8; 16]>::try_from(p)
                    .map(ChangeId::from_bytes)
                    .map_err(|_| Error::Corrupt("parent_change"))
            })
            .transpose()?,
        at: r.try_get("at")?,
        actor: r.try_get("actor")?,
        op_id: r.try_get("op_id")?,
    })
}

/// The newest version of `change`.
pub async fn latest(db: &mut AnyConnection, ns: Ns, change: ChangeId) -> Result<Option<Version>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM change_versions WHERE ns = $1 AND change = $2 \
         ORDER BY v DESC LIMIT 1"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .fetch_optional(db)
        .await?;
    row.as_ref().map(version_of).transpose()
}

/// Versions of `change` after `after` (exclusive), oldest first, at most `limit`
/// (clamped to `1..=MAX_PAGE`). A page ending at `limit` rows means there may be more.
pub async fn list(
    db: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    after: Option<u32>,
    limit: usize,
) -> Result<Vec<Version>> {
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    let sql = format!(
        "SELECT {COLUMNS} FROM change_versions WHERE ns = $1 AND change = $2 AND v > $3 \
         ORDER BY v LIMIT $4"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .bind(i32::try_from(after.unwrap_or(0)).unwrap_or(i32::MAX))
        .bind(limit)
        .fetch_all(db)
        .await?;
    rows.iter().map(version_of).collect()
}

/// True when `target` is `from` or reachable from it through `preds` rows, visiting at most
/// [`MAX_PREDS_WALK`] changesets. A changeset without a row has no predecessors.
async fn in_preds_closure(
    db: &mut AnyConnection,
    ns: Ns,
    from: ChangesetId,
    target: ChangesetId,
) -> Result<bool> {
    let mut seen: HashSet<ChangesetId> = HashSet::from([from]);
    let mut frontier = vec![from];
    while !frontier.is_empty() {
        if frontier.contains(&target) {
            return Ok(true);
        }
        let mut next = Vec::new();
        for row in rows_of(&mut *db, ns, &frontier)
            .await
            .map_err(|e| match e {
                super::index::Error::Sql(e) => Error::Sql(e),
                _ => Error::Corrupt("changes row"),
            })?
        {
            for p in row.preds {
                if seen.len() < MAX_PREDS_WALK && seen.insert(p) {
                    next.push(p);
                }
            }
        }
        frontier = next;
    }
    Ok(false)
}

/// Publishes `nv` as the next version of its change (the lease and the CAS are described in the
/// module docs). A retry of the latest changeset returns the existing version.
pub async fn append(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    nv: &NewVersion<'_>,
) -> Result<Appended> {
    if nv.actor.len() > MAX_ACTOR {
        return Err(Error::Invalid("actor is too long"));
    }
    if nv.op_id.len() > MAX_OP_ID {
        return Err(Error::Invalid("op id is too long"));
    }
    if nv.parent_change == Some(nv.change) {
        return Err(Error::Invalid("a change cannot be its own parent"));
    }
    let last = latest(tx, ns, nv.change).await?;
    let next = match &last {
        Some(l) if l.cs == nv.cs => {
            return Ok(Appended {
                v: l.v,
                created: false,
            });
        }
        Some(l) => {
            if !nv.force && !in_preds_closure(tx, ns, nv.cs, l.cs).await? {
                return Err(Error::Diverged {
                    change: nv.change,
                    latest: l.cs,
                });
            }
            l.v.checked_add(1)
                .filter(|v| i32::try_from(*v).is_ok())
                .ok_or(Error::Invalid("too many versions"))?
        }
        None => 1,
    };
    let done = sqlx::query(
        "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(nv.change.as_bytes().to_vec())
    .bind(i32::try_from(next).unwrap_or(i32::MAX))
    .bind(nv.cs.as_bytes().to_vec())
    .bind(nv.parent_change.map(|p| p.as_bytes().to_vec()))
    .bind(nv.at)
    .bind(nv.actor.to_owned())
    .bind(nv.op_id.to_vec())
    .execute(&mut **tx)
    .await;
    match done {
        Ok(_) => Ok(Appended {
            v: next,
            created: true,
        }),
        // Someone else became version `next` first: the lease no longer holds.
        Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Err(Error::Diverged {
            change: nv.change,
            latest: last.map_or(nv.cs, |l| l.cs),
        }),
        Err(e) => Err(e.into()),
    }
}

/// Every changeset that is the version of some change, as GC roots (§3.6).
pub struct VersionRoots {
    pool: AnyPool,
}

impl VersionRoots {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl GcRootSource for VersionRoots {
    async fn roots(&self, ns: &[u8]) -> std::result::Result<Vec<ObjKey>, GcError> {
        let fail = |e: &dyn std::fmt::Display| GcError::Other(format!("change_versions: {e}"));
        let mut out = Vec::new();
        let mut after: Option<(Vec<u8>, i64)> = None;
        loop {
            let (sql, page) = match &after {
                None => (
                    "SELECT change, CAST(v AS BIGINT) AS v, cs FROM change_versions \
                     WHERE ns = $1 ORDER BY change, v LIMIT $2"
                        .to_owned(),
                    None,
                ),
                Some(p) => (
                    "SELECT change, CAST(v AS BIGINT) AS v, cs FROM change_versions \
                     WHERE ns = $1 AND (change > $3 OR (change = $3 AND v > $4)) \
                     ORDER BY change, v LIMIT $2"
                        .to_owned(),
                    Some(p.clone()),
                ),
            };
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(ns.to_vec())
                .bind(ROOT_PAGE as i64);
            if let Some((c, v)) = page {
                q = q.bind(c).bind(i32::try_from(v).unwrap_or(i32::MAX));
            }
            let rows = q.fetch_all(&self.pool).await.map_err(|e| fail(&e))?;
            for r in &rows {
                let cs = fixed(r, "cs").ok_or_else(|| fail(&"damaged cs"))?;
                out.push(ObjKey::Changeset(ChangesetId::from_bytes(cs)));
            }
            match rows.last() {
                Some(last) if rows.len() == ROOT_PAGE => {
                    after = Some((
                        last.try_get("change").map_err(|e| fail(&e))?,
                        last.try_get("v").map_err(|e| fail(&e))?,
                    ));
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use asc_core::change::{ChangeMeta, RewriteOp};
    use asc_store::refs::RepoKey;

    use super::*;
    use crate::changes::index::ChangeIndex;
    use crate::ingest::testutil::{registry, repo};
    use crate::registry::Registry;

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    struct Fx {
        reg: Arc<Registry>,
        ns: Ns,
        key: RepoKey,
        idx: ChangeIndex,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        Fx {
            idx: ChangeIndex::new(reg.clone()),
            ns: e.ns,
            key: e.key,
            reg,
        }
    }

    impl Fx {
        /// Indexes `new` as a rewrite of `preds` (a meta row the lease walks through).
        async fn meta(&self, change: u8, new: u8, preds: &[u8]) {
            let op = if preds.is_empty() {
                RewriteOp::New
            } else {
                RewriteOp::Amend
            };
            let m = ChangeMeta::new(
                cs(new),
                cid(change),
                preds.iter().map(|&p| cs(p)).collect(),
                op,
            );
            let mut tx = self.reg.refs().begin().await.unwrap();
            self.idx.write_meta(&mut tx, self.key, &m).await.unwrap();
            tx.commit().await.unwrap();
        }

        async fn publish(&self, change: u8, new: u8, force: bool) -> Result<Appended> {
            let mut tx = self.reg.refs().begin().await.unwrap();
            let r = append(
                &mut tx,
                self.ns,
                &NewVersion {
                    change: cid(change),
                    cs: cs(new),
                    parent_change: None,
                    at: 100,
                    actor: "alice",
                    op_id: &[1, 2, 3],
                    force,
                },
            )
            .await;
            if r.is_ok() {
                tx.commit().await.unwrap();
            }
            r
        }
    }

    #[tokio::test]
    async fn versions_follow_the_preds_chain_and_retries_are_idempotent() {
        let f = fx().await;
        f.meta(1, 1, &[]).await;
        f.meta(1, 2, &[1]).await;
        f.meta(1, 3, &[2]).await;
        let first = f.publish(1, 1, false).await.unwrap();
        assert_eq!(
            first,
            Appended {
                v: 1,
                created: true
            }
        );
        let again = f.publish(1, 1, false).await.unwrap();
        assert_eq!(
            again,
            Appended {
                v: 1,
                created: false
            }
        );
        assert_eq!(f.publish(1, 2, false).await.unwrap().v, 2);
        assert_eq!(f.publish(1, 3, false).await.unwrap().v, 3);
        let mut c = f.reg.pool().acquire().await.unwrap();
        let all = list(&mut c, f.ns, cid(1), None, 10).await.unwrap();
        assert_eq!(all.iter().map(|v| v.v).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(all[2].cs, cs(3));
        let page = list(&mut c, f.ns, cid(1), Some(1), 1).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].v, 2);
        assert_eq!(latest(&mut c, f.ns, cid(1)).await.unwrap().unwrap().v, 3);
    }

    #[tokio::test]
    async fn a_concurrent_rewrite_is_change_diverged_unless_forced() {
        let f = fx().await;
        f.meta(1, 1, &[]).await;
        f.meta(1, 2, &[1]).await;
        // Another author rewrote 1 into 5 without seeing 2.
        f.meta(1, 5, &[1]).await;
        assert_eq!(f.publish(1, 1, false).await.unwrap().v, 1);
        assert_eq!(f.publish(1, 2, false).await.unwrap().v, 2);
        let err = f.publish(1, 5, false).await.unwrap_err();
        assert!(
            matches!(err, Error::Diverged { latest, .. } if latest == cs(2)),
            "{err}"
        );
        let status: tonic::Status = err.into();
        assert!(status.message().contains("CHANGE_DIVERGED"), "{status:?}");
        // A changeset with no meta cannot prove it saw the latest version either.
        assert!(f.publish(1, 9, false).await.is_err());
        assert_eq!(f.publish(1, 5, true).await.unwrap().v, 3);
    }

    #[tokio::test]
    async fn the_version_cas_loses_to_a_concurrent_publish() {
        let f = fx().await;
        f.meta(1, 1, &[]).await;
        f.meta(1, 3, &[1]).await;
        f.publish(1, 1, false).await.unwrap();
        // A publish that read v1 as latest can no longer insert v2 once another one did.
        let mut tx = f.reg.refs().begin().await.unwrap();
        let last = latest(&mut tx, f.ns, cid(1)).await.unwrap().unwrap();
        assert_eq!(last.v, 1);
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
             VALUES ($1, $2, 2, $3, NULL, 1, 'bob', $4)",
        )
        .bind(f.ns.as_bytes().to_vec())
        .bind(cid(1).as_bytes().to_vec())
        .bind(cs(2).as_bytes().to_vec())
        .bind(vec![0u8])
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let err = f.publish(1, 3, false).await.unwrap_err();
        assert!(matches!(err, Error::Diverged { .. }), "{err}");
    }

    #[tokio::test]
    async fn bounds_are_enforced() {
        let f = fx().await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        let long = "x".repeat(MAX_ACTOR + 1);
        let nv = NewVersion {
            change: cid(1),
            cs: cs(1),
            parent_change: None,
            at: 0,
            actor: &long,
            op_id: &[],
            force: false,
        };
        let r = append(&mut tx, f.ns, &nv).await;
        assert!(matches!(r, Err(Error::Invalid(_))));
        let nv = NewVersion {
            actor: "a",
            parent_change: Some(cid(1)),
            ..nv
        };
        let r = append(&mut tx, f.ns, &nv).await;
        assert!(matches!(r, Err(Error::Invalid(_))));
    }

    #[tokio::test]
    async fn every_version_is_a_gc_root_and_only_for_its_namespace() {
        let f = fx().await;
        f.meta(1, 1, &[]).await;
        f.meta(1, 2, &[1]).await;
        f.meta(7, 7, &[]).await;
        f.publish(1, 1, false).await.unwrap();
        f.publish(1, 2, false).await.unwrap();
        f.publish(7, 7, false).await.unwrap();
        let roots = VersionRoots::new(f.reg.pool().clone());
        let mut got = roots.roots(f.ns.as_bytes()).await.unwrap();
        got.sort_by_key(ObjKey::path);
        let mut want = vec![
            ObjKey::Changeset(cs(1)),
            ObjKey::Changeset(cs(2)),
            ObjKey::Changeset(cs(7)),
        ];
        want.sort_by_key(ObjKey::path);
        assert_eq!(got, want);
        assert!(roots.roots(&[0u8; 16]).await.unwrap().is_empty());
    }
}
