//! Leases on a repository's long operations, kept in the database (`repo_leases`).
//!
//! Two instances that share a database must not run the same operation, such as a restack, on
//! the same repository at once. [`Lease::acquire`] takes the row for `(namespace, name)` when it
//! is free or expired; the holder renews it while it works and frees it when it is dropped. A
//! holder that dies stops renewing, and the lease passes once its time is up. Before an operation
//! writes, [`Lease::ensure_held`] checks the row still names this holder.

use std::time::Duration;

use sqlx::AnyPool;
use tokio::task::JoinHandle;

/// How long a lease lasts without a renewal.
pub const TTL_SECS: i64 = 60;
/// How often a held lease is renewed.
const RENEW_EVERY: Duration = Duration::from_secs(20);

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A held lease. Dropping it stops the renewals and frees the row.
pub struct Lease {
    pool: AnyPool,
    ns: Vec<u8>,
    name: String,
    holder: String,
    ttl: i64,
    renewer: JoinHandle<()>,
    released: bool,
}

impl Lease {
    /// Takes the lease on `(ns, name)` for [`TTL_SECS`]. `Ok(None)`: another holder has it.
    pub async fn acquire(
        pool: &AnyPool,
        ns: &[u8],
        name: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        Self::acquire_for(pool, ns, name, TTL_SECS, RENEW_EVERY).await
    }

    async fn acquire_for(
        pool: &AnyPool,
        ns: &[u8],
        name: &str,
        ttl: i64,
        renew_every: Duration,
    ) -> Result<Option<Self>, sqlx::Error> {
        let holder = uuid::Uuid::new_v4().simple().to_string();
        let t = now();
        let taken = sqlx::query(
            "INSERT INTO repo_leases (ns, name, holder, expires) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (ns, name) DO UPDATE SET holder = $3, expires = $4 \
             WHERE repo_leases.expires < $5",
        )
        .bind(ns.to_vec())
        .bind(name)
        .bind(&holder)
        .bind(t + ttl)
        .bind(t)
        .execute(pool)
        .await?
        .rows_affected();
        if taken == 0 {
            return Ok(None);
        }
        let renewer = {
            let (pool, ns, name, holder) =
                (pool.clone(), ns.to_vec(), name.to_owned(), holder.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(renew_every).await;
                    if !renew(&pool, &ns, &name, &holder, ttl)
                        .await
                        .unwrap_or(false)
                    {
                        return;
                    }
                }
            })
        };
        Ok(Some(Self {
            pool: pool.clone(),
            ns: ns.to_vec(),
            name: name.to_owned(),
            holder,
            ttl,
            renewer,
            released: false,
        }))
    }

    /// Extends the lease, and says whether this holder still has it. Call it before a write
    /// that must not overlap another holder's.
    pub async fn ensure_held(&self) -> Result<bool, sqlx::Error> {
        renew(&self.pool, &self.ns, &self.name, &self.holder, self.ttl).await
    }

    /// Frees the lease now.
    pub async fn release(mut self) {
        self.renewer.abort();
        self.released = true;
        free(&self.pool, &self.ns, &self.name, &self.holder).await;
    }
}

async fn renew(
    pool: &AnyPool,
    ns: &[u8],
    name: &str,
    holder: &str,
    ttl: i64,
) -> Result<bool, sqlx::Error> {
    let t = now();
    // A lease that ran out but was not taken by anyone else can still be kept.
    let n = sqlx::query(
        "UPDATE repo_leases SET expires = $4 WHERE ns = $1 AND name = $2 AND holder = $3",
    )
    .bind(ns.to_vec())
    .bind(name)
    .bind(holder)
    .bind(t + ttl)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

async fn free(pool: &AnyPool, ns: &[u8], name: &str, holder: &str) {
    let _ = sqlx::query("DELETE FROM repo_leases WHERE ns = $1 AND name = $2 AND holder = $3")
        .bind(ns.to_vec())
        .bind(name)
        .bind(holder)
        .execute(pool)
        .await;
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.renewer.abort();
        if self.released {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (pool, ns, name, holder) = (
                self.pool.clone(),
                self.ns.clone(),
                self.name.clone(),
                self.holder.clone(),
            );
            rt.spawn(async move { free(&pool, &ns, &name, &holder).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> AnyPool {
        let pool = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
        crate::migrate::migrate(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn only_one_holder_has_a_repository_at_a_time() {
        let pool = pool().await;
        let ns = [7u8; 16];
        let first = Lease::acquire(&pool, &ns, "restack")
            .await
            .unwrap()
            .unwrap();
        assert!(
            Lease::acquire(&pool, &ns, "restack")
                .await
                .unwrap()
                .is_none()
        );
        // Another repository, and another operation on the same one, are free.
        assert!(
            Lease::acquire(&pool, &[8u8; 16], "restack")
                .await
                .unwrap()
                .is_some()
        );
        assert!(Lease::acquire(&pool, &ns, "gc").await.unwrap().is_some());
        assert!(first.ensure_held().await.unwrap());
        first.release().await;
        let second = Lease::acquire(&pool, &ns, "restack").await.unwrap();
        assert!(second.is_some(), "freed on release");
    }

    #[tokio::test]
    async fn dropping_a_lease_frees_it() {
        let pool = pool().await;
        let ns = [1u8; 16];
        drop(
            Lease::acquire(&pool, &ns, "restack")
                .await
                .unwrap()
                .unwrap(),
        );
        let mut got = None;
        for _ in 0..50 {
            got = Lease::acquire(&pool, &ns, "restack").await.unwrap();
            if got.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn an_expired_lease_passes_to_the_next_holder_who_then_alone_holds_it() {
        let pool = pool().await;
        let ns = [2u8; 16];
        let dead = Lease::acquire(&pool, &ns, "restack")
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE repo_leases SET expires = 1")
            .execute(&pool)
            .await
            .unwrap();
        let next = Lease::acquire(&pool, &ns, "restack")
            .await
            .unwrap()
            .unwrap();
        assert!(!dead.ensure_held().await.unwrap(), "the old holder lost it");
        assert!(next.ensure_held().await.unwrap());
        // The old holder's drop does not free the new holder's lease.
        drop(dead);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            Lease::acquire(&pool, &ns, "restack")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_held_lease_is_renewed_in_the_background() {
        let pool = pool().await;
        let ns = [3u8; 16];
        let lease = Lease::acquire_for(&pool, &ns, "restack", 5, Duration::from_millis(30))
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE repo_leases SET expires = $1")
            .bind(now() + 1)
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let expires: i64 = sqlx::query_scalar("SELECT expires FROM repo_leases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(expires >= now() + 3, "renewed to a full lifetime");
        drop(lease);
    }
}
