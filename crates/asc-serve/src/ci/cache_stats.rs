//! Counters of cache use per repository and day, and per change version: hits, misses, puts, bytes
//! served and milliseconds saved. Tables `cache_daily` and `cache_by_rev` (migration 0613).
//!
//! `day` is the unix time of 00:00 UTC. Counters are non-negative 64-bit integers kept as `BIGINT`,
//! so they saturate at `i64::MAX`, in memory and in SQL. Every statement binds its input and every
//! read is filtered by namespace.

use sqlx::{AnyConnection, AnyPool, Row};

use crate::registry::Ns;

const DAY: i64 = 86_400;
/// Days of daily rows kept.
pub const DAILY_DAYS: i64 = 180;
/// Days a change row is kept after its last update.
pub const REV_DAYS: i64 = 30;
/// Change rows kept per repository.
pub const MAX_REVS: i64 = 2000;
/// Daily rows `summary` returns, at most.
pub const MAX_SUMMARY_DAYS: usize = 90;
/// Rows one `prune` call deletes per statement.
const PRUNE_BATCH: i64 = 1000;
/// Repositories over the row cap one `prune` call trims.
const PRUNE_NS: i64 = 50;
/// Rows one `flush` transaction writes, at most.
pub const FLUSH_CHUNK: usize = 500;
/// Bytes of a change version id.
const CS_LEN: usize = 32;
const MAX: u64 = i64::MAX as u64;
const COLUMNS: [&str; 5] = ["hits", "misses", "puts", "bytes_served", "saved_ms"];

/// The five counters of one row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub hits: u64,
    pub misses: u64,
    pub puts: u64,
    pub bytes_served: u64,
    pub saved_ms: u64,
}

impl Counters {
    /// Adds `o` to `self`, each counter saturating at `u64::MAX`.
    pub fn add(&mut self, o: &Counters) {
        self.hits = self.hits.saturating_add(o.hits);
        self.misses = self.misses.saturating_add(o.misses);
        self.puts = self.puts.saturating_add(o.puts);
        self.bytes_served = self.bytes_served.saturating_add(o.bytes_served);
        self.saved_ms = self.saved_ms.saturating_add(o.saved_ms);
    }

    /// The counters as the stored `i64`s (clamped to `i64::MAX`).
    fn stored(&self) -> [i64; 5] {
        [
            self.hits,
            self.misses,
            self.puts,
            self.bytes_served,
            self.saved_ms,
        ]
        .map(|v| i64::try_from(v.min(MAX)).unwrap_or(i64::MAX))
    }

    fn from_row(r: &sqlx::any::AnyRow) -> Result<Self, sqlx::Error> {
        let get = |c: &str| -> Result<u64, sqlx::Error> {
            Ok(u64::try_from(r.try_get::<i64, _>(c)?).unwrap_or(0))
        };
        Ok(Self {
            hits: get("hits")?,
            misses: get("misses")?,
            puts: get("puts")?,
            bytes_served: get("bytes_served")?,
            saved_ms: get("saved_ms")?,
        })
    }
}

/// What one `prune` call deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub days: u64,
    pub revs: u64,
}

/// The `SET` list that adds `excluded` to `table`, each counter saturating at `i64::MAX`.
fn set_list(table: &str) -> String {
    COLUMNS
        .iter()
        .map(|col| {
            format!(
                "{col} = CASE WHEN {table}.{col} > 9223372036854775807 - excluded.{col} \
                 THEN 9223372036854775807 ELSE {table}.{col} + excluded.{col} END"
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn upsert_daily(
    conn: &mut AnyConnection,
    ns: Ns,
    day: i64,
    c: &Counters,
) -> Result<(), sqlx::Error> {
    let [h, m, p, b, s] = c.stored();
    let sql = format!(
        "INSERT INTO cache_daily (ns, day, hits, misses, puts, bytes_served, saved_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (ns, day) DO UPDATE SET {}",
        set_list("cache_daily")
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(ns.as_bytes().to_vec())
        .bind(day)
        .bind(h)
        .bind(m)
        .bind(p)
        .bind(b)
        .bind(s)
        .execute(conn)
        .await?;
    Ok(())
}

async fn upsert_rev(
    conn: &mut AnyConnection,
    ns: Ns,
    cs: &[u8],
    c: &Counters,
    now: i64,
) -> Result<(), sqlx::Error> {
    let [h, m, p, b, s] = c.stored();
    let sql = format!(
        "INSERT INTO cache_by_rev (ns, cs, hits, misses, puts, bytes_served, saved_ms, updated) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (ns, cs) DO UPDATE SET {}, \
         updated = excluded.updated",
        set_list("cache_by_rev")
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(ns.as_bytes().to_vec())
        .bind(cs.to_vec())
        .bind(h)
        .bind(m)
        .bind(p)
        .bind(b)
        .bind(s)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

/// Adds `c` to the row of `ns` and `day`.
pub async fn add_daily(pool: &AnyPool, ns: Ns, day: i64, c: &Counters) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    upsert_daily(&mut conn, ns, day, c).await
}

/// Adds `c` to the row of `ns` and change version `cs`, and sets its update time to `now`.
pub async fn add_rev(
    pool: &AnyPool,
    ns: Ns,
    cs: &[u8],
    c: &Counters,
    now: i64,
) -> Result<(), sqlx::Error> {
    if cs.len() != CS_LEN {
        return Ok(());
    }
    let mut conn = pool.acquire().await?;
    upsert_rev(&mut conn, ns, cs, c, now).await
}

/// Why a `flush` stopped, and what it had written by then.
#[derive(Debug)]
pub struct FlushError {
    /// Rows of the merged `daily` list (see [`merge_daily`]) already committed.
    pub written_daily: usize,
    /// Rows of the merged `revs` list (see [`merge_revs`]) already committed.
    pub written_revs: usize,
    pub source: sqlx::Error,
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cache stats flush failed after {} daily and {} change rows: {}",
            self.written_daily, self.written_revs, self.source
        )
    }
}

impl std::error::Error for FlushError {}

/// Sorts `daily` by (ns, day) and adds the counters of equal keys.
pub fn merge_daily(mut daily: Vec<(Ns, i64, Counters)>) -> Vec<(Ns, i64, Counters)> {
    daily.sort_by(|a, b| (a.0.as_bytes(), a.1).cmp(&(b.0.as_bytes(), b.1)));
    let mut out: Vec<(Ns, i64, Counters)> = Vec::with_capacity(daily.len());
    for (ns, day, c) in daily {
        match out.last_mut() {
            Some(l) if l.0 == ns && l.1 == day => l.2.add(&c),
            _ => out.push((ns, day, c)),
        }
    }
    out
}

/// Drops rows whose `cs` is not 32 bytes, sorts by (ns, cs) and adds the counters of equal keys.
pub fn merge_revs(mut revs: Vec<(Ns, Vec<u8>, Counters)>) -> Vec<(Ns, Vec<u8>, Counters)> {
    revs.retain(|r| r.1.len() == CS_LEN);
    revs.sort_by(|a, b| (a.0.as_bytes(), &a.1).cmp(&(b.0.as_bytes(), &b.1)));
    let mut out: Vec<(Ns, Vec<u8>, Counters)> = Vec::with_capacity(revs.len());
    for (ns, cs, c) in revs {
        match out.last_mut() {
            Some(l) if l.0 == ns && l.1 == cs => l.2.add(&c),
            _ => out.push((ns, cs, c)),
        }
    }
    out
}

/// Adds all rows. The lists are first merged ([`merge_daily`], [`merge_revs`]) so concurrent
/// flushers lock rows in one order, then written in transactions of at most [`FLUSH_CHUNK`] rows:
/// all daily rows, then all change rows. A failure part-way leaves the earlier chunks written;
/// the error says how many rows of the merged lists are in, so the caller keeps
/// `merge_daily(daily)[written_daily..]` and `merge_revs(revs)[written_revs..]` and does not count
/// a row twice.
pub async fn flush(
    pool: &AnyPool,
    daily: &[(Ns, i64, Counters)],
    revs: &[(Ns, Vec<u8>, Counters)],
    now: i64,
) -> Result<(), FlushError> {
    let daily = merge_daily(daily.to_vec());
    let revs = merge_revs(revs.to_vec());
    let (mut written_daily, mut written_revs) = (0, 0);
    let fail = |written_daily, written_revs, source| FlushError {
        written_daily,
        written_revs,
        source,
    };
    for chunk in daily.chunks(FLUSH_CHUNK) {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| fail(written_daily, written_revs, e))?;
        for (ns, day, c) in chunk {
            upsert_daily(&mut tx, *ns, *day, c)
                .await
                .map_err(|e| fail(written_daily, written_revs, e))?;
        }
        tx.commit()
            .await
            .map_err(|e| fail(written_daily, written_revs, e))?;
        written_daily += chunk.len();
    }
    for chunk in revs.chunks(FLUSH_CHUNK) {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| fail(written_daily, written_revs, e))?;
        for (ns, cs, c) in chunk {
            upsert_rev(&mut tx, *ns, cs, c, now)
                .await
                .map_err(|e| fail(written_daily, written_revs, e))?;
        }
        tx.commit()
            .await
            .map_err(|e| fail(written_daily, written_revs, e))?;
        written_revs += chunk.len();
    }
    Ok(())
}

/// Whether `change_versions` holds `cs` in `ns`.
pub async fn resolve_rev(pool: &AnyPool, ns: Ns, cs: &[u8]) -> Result<bool, sqlx::Error> {
    if cs.len() != CS_LEN {
        return Ok(false);
    }
    let row = sqlx::query("SELECT 1 AS one FROM change_versions WHERE ns = $1 AND cs = $2 LIMIT 1")
        .bind(ns.as_bytes().to_vec())
        .bind(cs.to_vec())
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// The daily rows of `ns` with `since_day <= day <= until_day` (both ends inclusive; `day` is unix
/// seconds at 00:00 UTC) and their sum. At most [`MAX_SUMMARY_DAYS`] rows: when the range holds more,
/// the newest are kept. Rows come oldest first.
pub async fn summary(
    pool: &AnyPool,
    ns: Ns,
    since_day: i64,
    until_day: i64,
) -> Result<(Counters, Vec<(i64, Counters)>), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT day, hits, misses, puts, bytes_served, saved_ms FROM cache_daily \
         WHERE ns = $1 AND day >= $2 AND day <= $3 ORDER BY day DESC LIMIT $4",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(since_day)
    .bind(until_day)
    .bind(MAX_SUMMARY_DAYS as i64)
    .fetch_all(pool)
    .await?;
    let mut total = Counters::default();
    let mut days = Vec::with_capacity(rows.len());
    for r in &rows {
        let c = Counters::from_row(r)?;
        total.add(&c);
        days.push((r.try_get::<i64, _>("day")?, c));
    }
    days.reverse();
    Ok((total, days))
}

/// The counters of `ns` for change version `cs`, if any.
pub async fn for_rev(pool: &AnyPool, ns: Ns, cs: &[u8]) -> Result<Option<Counters>, sqlx::Error> {
    if cs.len() != CS_LEN {
        return Ok(None);
    }
    sqlx::query(
        "SELECT hits, misses, puts, bytes_served, saved_ms FROM cache_by_rev \
         WHERE ns = $1 AND cs = $2",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(cs.to_vec())
    .fetch_optional(pool)
    .await?
    .map(|r| Counters::from_row(&r))
    .transpose()
}

/// Deletes daily rows older than [`DAILY_DAYS`], change rows older than [`REV_DAYS`], and change
/// rows beyond the newest [`MAX_REVS`] of a repository. One call deletes a bounded number of rows;
/// the next call continues.
pub async fn prune(pool: &AnyPool, now: i64) -> Result<Pruned, sqlx::Error> {
    let days = sqlx::query(
        "DELETE FROM cache_daily WHERE (ns, day) IN \
         (SELECT ns, day FROM cache_daily WHERE day < $1 LIMIT $2)",
    )
    .bind(now - DAILY_DAYS * DAY)
    .bind(PRUNE_BATCH)
    .execute(pool)
    .await?
    .rows_affected();
    let mut revs = sqlx::query(
        "DELETE FROM cache_by_rev WHERE (ns, cs) IN \
         (SELECT ns, cs FROM cache_by_rev WHERE updated < $1 LIMIT $2)",
    )
    .bind(now - REV_DAYS * DAY)
    .bind(PRUNE_BATCH)
    .execute(pool)
    .await?
    .rows_affected();
    let over = sqlx::query("SELECT ns FROM cache_by_rev GROUP BY ns HAVING COUNT(*) > $1 LIMIT $2")
        .bind(MAX_REVS)
        .bind(PRUNE_NS)
        .fetch_all(pool)
        .await?;
    for r in &over {
        let ns: Vec<u8> = r.try_get("ns")?;
        revs += sqlx::query(
            "DELETE FROM cache_by_rev WHERE ns = $1 AND cs IN \
             (SELECT cs FROM cache_by_rev WHERE ns = $2 ORDER BY updated DESC, cs DESC \
              LIMIT $3 OFFSET $4)",
        )
        .bind(ns.clone())
        .bind(ns)
        .bind(PRUNE_BATCH)
        .bind(MAX_REVS)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(Pruned { days, revs })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::migrate;

    async fn pool() -> AnyPool {
        let p = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&p).await.unwrap();
        p
    }

    fn c(hits: u64, misses: u64) -> Counters {
        Counters {
            hits,
            misses,
            puts: 1,
            bytes_served: 10,
            saved_ms: 100,
        }
    }

    async fn count(p: &AnyPool, ns: Ns, table: &str) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) AS n FROM {table} WHERE ns = $1"
        )))
        .bind(ns.as_bytes().to_vec())
        .fetch_one(p)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
    }

    #[tokio::test]
    async fn cache_stats_upsert_adds() {
        let p = pool().await;
        let ns = Ns::random();
        let sum = Counters {
            hits: 5,
            misses: 1,
            puts: 2,
            bytes_served: 20,
            saved_ms: 200,
        };
        add_daily(&p, ns, 0, &c(2, 1)).await.unwrap();
        add_daily(&p, ns, 0, &c(3, 0)).await.unwrap();
        let (t, days) = summary(&p, ns, 0, 0).await.unwrap();
        assert_eq!(days, vec![(0, sum)]);
        assert_eq!(t, sum);
        add_rev(&p, ns, &[1; 32], &c(2, 1), 10).await.unwrap();
        add_rev(&p, ns, &[1; 32], &c(3, 0), 20).await.unwrap();
        assert_eq!(for_rev(&p, ns, &[1; 32]).await.unwrap(), Some(sum));
        assert_eq!(for_rev(&p, ns, &[2; 32]).await.unwrap(), None);
        let upd: i64 = sqlx::query("SELECT updated FROM cache_by_rev")
            .fetch_one(&p)
            .await
            .unwrap()
            .try_get("updated")
            .unwrap();
        assert_eq!(upd, 20);
    }

    #[tokio::test]
    async fn cache_stats_flush_batches_and_namespaces_are_separate() {
        let p = pool().await;
        let (a, b) = (Ns::random(), Ns::random());
        flush(
            &p,
            &[(a, 5, c(1, 0)), (b, 5, c(7, 0)), (a, 6, c(2, 0))],
            &[(a, vec![9; 32], c(1, 0)), (b, vec![9; 32], c(4, 0))],
            1,
        )
        .await
        .unwrap();
        flush(&p, &[], &[], 1).await.unwrap();
        let (ta, da) = summary(&p, a, 0, 100).await.unwrap();
        assert_eq!((ta.hits, da.len()), (3, 2));
        let (tb, db) = summary(&p, b, 0, 100).await.unwrap();
        assert_eq!((tb.hits, db.len()), (7, 1));
        assert_eq!(for_rev(&p, a, &[9; 32]).await.unwrap().unwrap().hits, 1);
        assert_eq!(for_rev(&p, b, &[9; 32]).await.unwrap().unwrap().hits, 4);
        let (none, days) = summary(&p, Ns::random(), 0, 100).await.unwrap();
        assert_eq!((none, days.len()), (Counters::default(), 0));
    }

    #[tokio::test]
    async fn cache_stats_resolve_rev_refuses_other_namespace() {
        let p = pool().await;
        let (a, b) = (Ns::random(), Ns::random());
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
             VALUES ($1, $2, 1, $3, NULL, 0, 'x', $4)",
        )
        .bind(a.as_bytes().to_vec())
        .bind(vec![3u8; 16])
        .bind(vec![7u8; 32])
        .bind(vec![4u8; 16])
        .execute(&p)
        .await
        .unwrap();
        assert!(resolve_rev(&p, a, &[7; 32]).await.unwrap());
        assert!(!resolve_rev(&p, b, &[7; 32]).await.unwrap());
        assert!(!resolve_rev(&p, a, &[8; 32]).await.unwrap());
    }

    #[tokio::test]
    async fn cache_stats_summary_bounds_and_order() {
        let p = pool().await;
        let ns = Ns::random();
        let rows: Vec<_> = (0..100i64).rev().map(|d| (ns, d * DAY, c(1, 0))).collect();
        flush(&p, &rows, &[], 0).await.unwrap();
        let (t, days) = summary(&p, ns, 0, 99 * DAY).await.unwrap();
        assert_eq!(days.len(), MAX_SUMMARY_DAYS);
        assert_eq!(t.hits, 90);
        assert!(days.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(days[0].0, 10 * DAY);
        assert_eq!(days[89].0, 99 * DAY);
        let (t, days) = summary(&p, ns, 10 * DAY, 12 * DAY).await.unwrap();
        assert_eq!((t.hits, days.len()), (3, 3));
        assert_eq!(days[0].0, 10 * DAY);
        let (_, days) = summary(&p, ns, 12 * DAY, 10 * DAY).await.unwrap();
        assert!(days.is_empty());
    }

    #[tokio::test]
    async fn cache_stats_summary_keeps_the_newest_of_91_days() {
        let p = pool().await;
        let ns = Ns::random();
        let rows: Vec<_> = (0..91i64)
            .map(|d| (ns, d * DAY, c(d as u64 + 1, 0)))
            .collect();
        flush(&p, &rows, &[], 0).await.unwrap();
        let (t, days) = summary(&p, ns, 0, 90 * DAY).await.unwrap();
        assert_eq!(days.len(), 90);
        assert_eq!(days[0].0, DAY);
        assert_eq!(days[89].0, 90 * DAY);
        assert_eq!(days[89].1.hits, 91);
        assert_eq!(t.hits, (2..=91).sum::<u64>());
    }

    #[tokio::test]
    async fn cache_stats_flush_merges_duplicates_and_writes_in_chunks() {
        let p = pool().await;
        let (a, b) = (Ns::random(), Ns::random());
        let mut daily = vec![(b, 2, c(1, 0)), (a, 2, c(1, 0)), (b, 2, c(1, 0))];
        let merged = merge_daily(daily.clone());
        assert_eq!(merged.len(), 2);
        assert!(merged[0].0.as_bytes() < merged[1].0.as_bytes());
        for d in 0..(FLUSH_CHUNK as i64 * 2 + 3) {
            daily.push((a, 100 + d, c(1, 0)));
        }
        let revs = vec![
            (a, vec![2u8; 32], c(1, 0)),
            (a, vec![1u8; 32], c(1, 0)),
            (a, vec![2u8; 32], c(2, 0)),
            (a, vec![5u8; 31], c(9, 0)),
        ];
        let mr = merge_revs(revs.clone());
        assert_eq!(mr.len(), 2);
        assert_eq!((mr[0].1[0], mr[1].1[0], mr[1].2.hits), (1, 2, 3));
        flush(&p, &daily, &revs, 5).await.unwrap();
        assert_eq!(
            count(&p, a, "cache_daily").await,
            FLUSH_CHUNK as i64 * 2 + 4
        );
        assert_eq!(for_rev(&p, a, &[2; 32]).await.unwrap().unwrap().hits, 3);
        let (tb, _) = summary(&p, b, 0, 10).await.unwrap();
        assert_eq!(tb.hits, 2);
    }

    #[tokio::test]
    async fn cache_stats_flush_error_reports_what_was_written() {
        let p = pool().await;
        let a = Ns::random();
        let daily: Vec<_> = (0..(FLUSH_CHUNK as i64 + 1))
            .map(|d| (a, d, c(1, 0)))
            .collect();
        p.close().await;
        let e = flush(&p, &daily, &[], 0).await.unwrap_err();
        assert_eq!((e.written_daily, e.written_revs), (0, 0));
        assert!(!e.to_string().is_empty());
    }

    #[tokio::test]
    async fn cache_stats_refuses_a_change_id_that_is_not_32_bytes() {
        let p = pool().await;
        let ns = Ns::random();
        for cs in [&[1u8; 31][..], &[1u8; 33][..], &[][..]] {
            add_rev(&p, ns, cs, &c(1, 0), 0).await.unwrap();
            assert_eq!(for_rev(&p, ns, cs).await.unwrap(), None);
            assert!(!resolve_rev(&p, ns, cs).await.unwrap());
        }
        flush(&p, &[], &[(ns, vec![1; 31], c(1, 0))], 0)
            .await
            .unwrap();
        assert_eq!(count(&p, ns, "cache_by_rev").await, 0);
    }

    #[tokio::test]
    async fn cache_stats_prune_by_age_and_row_cap() {
        let p = pool().await;
        let ns = Ns::random();
        let now = 1_000 * DAY;
        add_daily(&p, ns, now - 181 * DAY, &c(1, 0)).await.unwrap();
        add_daily(&p, ns, now - 179 * DAY, &c(1, 0)).await.unwrap();
        add_rev(&p, ns, &[1; 32], &c(1, 0), now - 31 * DAY)
            .await
            .unwrap();
        add_rev(&p, ns, &[2; 32], &c(1, 0), now - 29 * DAY)
            .await
            .unwrap();
        let d = prune(&p, now).await.unwrap();
        assert_eq!(d, Pruned { days: 1, revs: 1 });
        assert_eq!(count(&p, ns, "cache_daily").await, 1);
        assert_eq!(count(&p, ns, "cache_by_rev").await, 1);

        // Row cap: 2005 more recent rows; the newest 2000 stay, another repository is untouched.
        let other = Ns::random();
        let key = |i: u32| {
            let mut cs = vec![0u8; 32];
            cs[..4].copy_from_slice(&i.to_be_bytes());
            cs
        };
        let mut tx = p.begin().await.unwrap();
        for i in 0..2005u32 {
            upsert_rev(&mut tx, ns, &key(i), &c(1, 0), now - 3000 + i64::from(i))
                .await
                .unwrap();
        }
        upsert_rev(&mut tx, other, &[1; 32], &c(1, 0), now - 5000)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        // The row from before is the oldest of 2006 and goes too.
        let d = prune(&p, now).await.unwrap();
        assert_eq!(d, Pruned { days: 0, revs: 6 });
        assert_eq!(count(&p, ns, "cache_by_rev").await, MAX_REVS);
        assert!(for_rev(&p, ns, &key(0)).await.unwrap().is_none());
        assert!(for_rev(&p, ns, &key(4)).await.unwrap().is_none());
        assert!(for_rev(&p, ns, &key(5)).await.unwrap().is_some());
        assert!(for_rev(&p, ns, &key(2004)).await.unwrap().is_some());
        assert!(for_rev(&p, other, &[1; 32]).await.unwrap().is_some());
        assert_eq!(prune(&p, now).await.unwrap(), Pruned::default());
    }

    #[tokio::test]
    async fn cache_stats_saturates_near_the_maximum() {
        let p = pool().await;
        let ns = Ns::random();
        let near = Counters {
            hits: MAX - 1,
            misses: u64::MAX,
            puts: 0,
            bytes_served: MAX,
            saved_ms: 1,
        };
        add_daily(&p, ns, 0, &near).await.unwrap();
        add_daily(&p, ns, 0, &c(5, 5)).await.unwrap();
        add_rev(&p, ns, &[1; 32], &near, 0).await.unwrap();
        add_rev(&p, ns, &[1; 32], &c(5, 5), 0).await.unwrap();
        let (t, _) = summary(&p, ns, 0, 0).await.unwrap();
        assert_eq!((t.hits, t.misses, t.bytes_served), (MAX, MAX, MAX));
        assert_eq!((t.puts, t.saved_ms), (1, 101));
        assert_eq!(for_rev(&p, ns, &[1; 32]).await.unwrap().unwrap(), t);
        let mut s = Counters {
            hits: u64::MAX,
            ..Counters::default()
        };
        s.add(&c(1, 0));
        assert_eq!(s.hits, u64::MAX);
    }
}
