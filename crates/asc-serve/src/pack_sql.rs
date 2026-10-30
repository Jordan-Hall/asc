//! The pack index in the server's database (`packs`, `pack_objects`; docs/design/PACKED-OBJECTS.md).
//!
//! One [`SqlPackIndex`] per repository namespace. Rows are added only by `commit`, in one
//! transaction with the pack row, after the pack file is durable; a GC delete removes a row
//! (`forget`). Both tables are namespace tables (`admin.rs::NS_TABLES`), so a deleted repository
//! drops them.

use asc_store::key::{KeyClass, ObjKey};
use asc_store::objects::{Error, Result};
use asc_store::pack::format::ENTRY_HEADER_LEN;
use asc_store::pack::{PackDetail, PackEntry, PackId, PackIndex, PackInfo, PackLoc, PackState};
use async_trait::async_trait;
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};

use crate::registry::Ns;

/// Ids per `IN` list; keeps a statement far below every database's bind limit.
const IN_CHUNK: usize = 300;
/// Rows per multi-row insert (six binds each).
const INSERT_CHUNK: usize = 200;

fn sql(e: sqlx::Error) -> Error {
    Error::Index(e.to_string())
}

/// `$first, $first+1, … ` for `n` placeholders.
fn marks(first: usize, n: usize) -> String {
    (first..first + n)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pack_id(b: Vec<u8>) -> Result<PackId> {
    <[u8; 16]>::try_from(b.as_slice())
        .map(PackId)
        .map_err(|_| Error::Index("a pack id is not 16 bytes".into()))
}

fn entry_of(row: &AnyRow) -> Result<PackEntry> {
    let class: i64 = row.try_get("class").map_err(sql)?;
    let id: Vec<u8> = row.try_get("id").map_err(sql)?;
    let off: i64 = row.try_get("off").map_err(sql)?;
    let len: i64 = row.try_get("len").map_err(sql)?;
    let key = u8::try_from(class)
        .ok()
        .and_then(KeyClass::from_code)
        .zip(<[u8; 32]>::try_from(id.as_slice()).ok())
        .and_then(|(c, id)| ObjKey::from_pack(c, id))
        .ok_or_else(|| Error::Index("pack_objects holds a key that cannot be".into()))?;
    Ok(PackEntry {
        key,
        offset: u64::try_from(off).map_err(|_| Error::Index("negative offset".into()))?,
        len: u32::try_from(len).map_err(|_| Error::Index("bad length".into()))?,
    })
}

/// The index of one namespace.
pub struct SqlPackIndex {
    pool: AnyPool,
    ns: Ns,
}

impl SqlPackIndex {
    pub fn new(pool: AnyPool, ns: Ns) -> Self {
        Self { pool, ns }
    }

    fn ns_bytes(&self) -> Vec<u8> {
        self.ns.as_bytes().to_vec()
    }
}

fn state_of(row: &AnyRow) -> Result<PackState> {
    let s: i64 = row.try_get("state").map_err(sql)?;
    PackState::from_code(s).ok_or_else(|| Error::Index(format!("packs.state {s}")))
}

fn info_of(row: &AnyRow) -> Result<PackInfo> {
    let n = |c: &str| -> Result<u64> {
        let v: i64 = row.try_get(c).map_err(sql)?;
        u64::try_from(v).map_err(|_| Error::Index(format!("packs.{c} is negative")))
    };
    Ok(PackInfo {
        id: pack_id(row.try_get("pack").map_err(sql)?)?,
        objects: n("objects")?,
        bytes: n("bytes")?,
        created: row.try_get("created").map_err(sql)?,
        state: state_of(row)?,
        retired: row.try_get("retired").map_err(sql)?,
    })
}

#[async_trait]
impl PackIndex for SqlPackIndex {
    async fn any_live(&self) -> Result<bool> {
        // A caller may hold a transaction and the pool may be spent: do not wait long for a
        // connection to ask. "None" is then trusted for a moment, like any other answer.
        let wait = std::time::Duration::from_millis(250);
        let Ok(conn) = tokio::time::timeout(wait, self.pool.acquire()).await else {
            return Ok(false);
        };
        let mut conn = conn.map_err(sql)?;
        let row = sqlx::query("SELECT 1 AS one FROM packs WHERE ns = $1 AND state = 1 LIMIT 1")
            .bind(self.ns_bytes())
            .fetch_optional(&mut *conn)
            .await
            .map_err(sql)?;
        Ok(row.is_some())
    }

    async fn locate(&self, keys: &[ObjKey]) -> Result<Vec<Option<PackLoc>>> {
        let mut out = vec![None; keys.len()];
        // Group by class so the statement is `class = $2 AND id IN (...)`.
        let mut by_class: std::collections::BTreeMap<u8, Vec<(usize, [u8; 32])>> =
            Default::default();
        for (i, k) in keys.iter().enumerate() {
            by_class
                .entry(k.class().code())
                .or_default()
                .push((i, k.id32()));
        }
        for (class, items) in by_class {
            for chunk in items.chunks(IN_CHUNK) {
                let q = format!(
                    "SELECT id, pack, off, len FROM pack_objects \
                     WHERE ns = $1 AND class = $2 AND id IN ({})",
                    marks(3, chunk.len())
                );
                let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
                    .bind(self.ns_bytes())
                    .bind(i64::from(class));
                for (_, id) in chunk {
                    query = query.bind(id.to_vec());
                }
                let rows = query.fetch_all(&self.pool).await.map_err(sql)?;
                for row in rows {
                    let id: Vec<u8> = row.try_get("id").map_err(sql)?;
                    let off: i64 = row.try_get("off").map_err(sql)?;
                    let len: i64 = row.try_get("len").map_err(sql)?;
                    let loc = PackLoc {
                        pack: pack_id(row.try_get("pack").map_err(sql)?)?,
                        offset: u64::try_from(off).map_err(|_| Error::Index("offset".into()))?,
                        len: u64::try_from(len).map_err(|_| Error::Index("length".into()))?,
                    };
                    if let Some((i, _)) = chunk.iter().find(|(_, c)| c.as_slice() == id.as_slice())
                    {
                        out[*i] = Some(loc);
                    }
                }
            }
        }
        Ok(out)
    }

    async fn commit(
        &self,
        new: &PackInfo,
        entries: &[PackEntry],
        replaces: &[PackId],
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await.map_err(sql)?;
        let ns = self.ns_bytes();
        let to_i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        sqlx::query(
            "INSERT INTO packs (ns, pack, objects, bytes, created, state, retired) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ns.clone())
        .bind(new.id.0.to_vec())
        .bind(to_i(new.objects))
        .bind(to_i(new.bytes))
        .bind(new.created)
        .bind(new.state.code())
        .bind(new.retired)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
        let mut changed = 0u64;
        if replaces.is_empty() {
            for chunk in entries.chunks(INSERT_CHUNK) {
                let values = (0..chunk.len())
                    .map(|i| {
                        let b = i * 6;
                        format!(
                            "(${}, ${}, ${}, ${}, ${}, ${})",
                            b + 1,
                            b + 2,
                            b + 3,
                            b + 4,
                            b + 5,
                            b + 6
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let q = format!(
                    "INSERT INTO pack_objects (ns, class, id, pack, off, len) VALUES {values} \
                     ON CONFLICT (ns, class, id) DO NOTHING"
                );
                let mut query = sqlx::query(sqlx::AssertSqlSafe(q));
                for e in chunk {
                    query = query
                        .bind(ns.clone())
                        .bind(i64::from(e.key.class().code()))
                        .bind(e.key.id32().to_vec())
                        .bind(new.id.0.to_vec())
                        .bind(to_i(e.offset))
                        .bind(i64::from(e.len));
                }
                changed += query.execute(&mut *tx).await.map_err(sql)?.rows_affected();
            }
        } else {
            // Only rows that still point into a replaced pack move; a key GC deleted meanwhile
            // has no row and gets none.
            let q = format!(
                "UPDATE pack_objects SET pack = $1, off = $2, len = $3 \
                 WHERE ns = $4 AND class = $5 AND id = $6 AND pack IN ({})",
                marks(7, replaces.len())
            );
            for e in entries {
                let mut query = sqlx::query(sqlx::AssertSqlSafe(q.clone()))
                    .bind(new.id.0.to_vec())
                    .bind(to_i(e.offset))
                    .bind(i64::from(e.len))
                    .bind(ns.clone())
                    .bind(i64::from(e.key.class().code()))
                    .bind(e.key.id32().to_vec());
                for r in replaces {
                    query = query.bind(r.0.to_vec());
                }
                changed += query.execute(&mut *tx).await.map_err(sql)?.rows_affected();
            }
            let q = format!(
                "UPDATE packs SET state = 2, retired = $1 WHERE ns = $2 AND pack IN ({})",
                marks(3, replaces.len())
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(new.created)
                .bind(ns);
            for r in replaces {
                query = query.bind(r.0.to_vec());
            }
            query.execute(&mut *tx).await.map_err(sql)?;
        }
        tx.commit().await.map_err(sql)?;
        Ok(changed)
    }

    async fn retire(&self, ids: &[PackId], now: i64) -> Result<()> {
        for chunk in ids.chunks(IN_CHUNK) {
            let q = format!(
                "UPDATE packs SET state = 2, retired = $1 WHERE ns = $2 AND pack IN ({})",
                marks(3, chunk.len())
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(now)
                .bind(self.ns_bytes());
            for r in chunk {
                query = query.bind(r.0.to_vec());
            }
            query.execute(&self.pool).await.map_err(sql)?;
        }
        Ok(())
    }

    async fn forget(&self, keys: &[ObjKey]) -> Result<u64> {
        let mut by_class: std::collections::BTreeMap<u8, Vec<[u8; 32]>> = Default::default();
        for k in keys {
            by_class.entry(k.class().code()).or_default().push(k.id32());
        }
        let mut gone = 0;
        for (class, ids) in by_class {
            for chunk in ids.chunks(IN_CHUNK) {
                let q = format!(
                    "DELETE FROM pack_objects WHERE ns = $1 AND class = $2 AND id IN ({})",
                    marks(3, chunk.len())
                );
                let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
                    .bind(self.ns_bytes())
                    .bind(i64::from(class));
                for id in chunk {
                    query = query.bind(id.to_vec());
                }
                gone += query
                    .execute(&self.pool)
                    .await
                    .map_err(sql)?
                    .rows_affected();
            }
        }
        Ok(gone)
    }

    async fn packs(&self) -> Result<Vec<PackDetail>> {
        let rows = sqlx::query(
            "SELECT p.pack AS pack, p.objects AS objects, p.bytes AS bytes, p.created AS created, \
                    p.state AS state, p.retired AS retired, \
                    COUNT(o.id) AS n, CAST(COALESCE(SUM(o.len), 0) AS BIGINT) AS s \
             FROM packs p LEFT JOIN pack_objects o ON o.ns = p.ns AND o.pack = p.pack \
             WHERE p.ns = $1 \
             GROUP BY p.pack, p.objects, p.bytes, p.created, p.state, p.retired \
             ORDER BY p.created, p.pack",
        )
        .bind(self.ns_bytes())
        .fetch_all(&self.pool)
        .await
        .map_err(sql)?;
        rows.iter()
            .map(|row| {
                let n: i64 = row.try_get("n").map_err(sql)?;
                let s: i64 = row.try_get("s").map_err(sql)?;
                let n = u64::try_from(n).unwrap_or(0);
                Ok(PackDetail {
                    info: info_of(row)?,
                    live_objects: n,
                    live_bytes: u64::try_from(s).unwrap_or(0) + n * ENTRY_HEADER_LEN as u64,
                })
            })
            .collect()
    }

    async fn rows_of(&self, id: PackId) -> Result<Vec<PackEntry>> {
        let rows = sqlx::query(
            "SELECT class, id, off, len FROM pack_objects WHERE ns = $1 AND pack = $2 \
             ORDER BY class, id",
        )
        .bind(self.ns_bytes())
        .bind(id.0.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(sql)?;
        rows.iter().map(entry_of).collect()
    }

    async fn remove(&self, id: PackId) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(sql)?;
        for q in [
            "DELETE FROM pack_objects WHERE ns = $1 AND pack = $2",
            "DELETE FROM packs WHERE ns = $1 AND pack = $2",
        ] {
            sqlx::query(q)
                .bind(self.ns_bytes())
                .bind(id.0.to_vec())
                .execute(&mut *tx)
                .await
                .map_err(sql)?;
        }
        tx.commit().await.map_err(sql)
    }

    async fn list(
        &self,
        class: Option<KeyClass>,
        after: Option<(u8, [u8; 32])>,
        limit: usize,
    ) -> Result<Vec<(ObjKey, i64)>> {
        let mut q = String::from(
            "SELECT o.class AS class, o.id AS id, o.off AS off, o.len AS len, p.created AS created \
             FROM pack_objects o JOIN packs p ON p.ns = o.ns AND p.pack = o.pack \
             WHERE o.ns = $1",
        );
        let mut next = 2;
        if class.is_some() {
            q.push_str(&format!(" AND o.class = ${next}"));
            next += 1;
        }
        if after.is_some() {
            q.push_str(&format!(
                " AND (o.class > ${next} OR (o.class = ${next} AND o.id > ${}))",
                next + 1
            ));
            next += 2;
        }
        q.push_str(&format!(" ORDER BY o.class, o.id LIMIT ${next}"));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(self.ns_bytes());
        if let Some(c) = class {
            query = query.bind(i64::from(c.code()));
        }
        if let Some((c, id)) = after {
            query = query.bind(i64::from(c)).bind(id.to_vec());
        }
        let rows = query
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(sql)?;
        rows.iter()
            .map(|row| {
                let created: i64 = row.try_get("created").map_err(sql)?;
                Ok((entry_of(row)?.key, created))
            })
            .collect()
    }

    async fn rebuild(&self, packs: &[(PackInfo, Vec<PackEntry>)]) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(sql)?;
        let ns = self.ns_bytes();
        for t in ["pack_objects", "packs"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {t} WHERE ns = $1"
            )))
            .bind(ns.clone())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        let to_i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        for (info, entries) in packs {
            sqlx::query(
                "INSERT INTO packs (ns, pack, objects, bytes, created, state, retired) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(ns.clone())
            .bind(info.id.0.to_vec())
            .bind(to_i(info.objects))
            .bind(to_i(info.bytes))
            .bind(info.created)
            .bind(info.state.code())
            .bind(info.retired)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
            if info.state != PackState::Live {
                continue;
            }
            for e in entries {
                sqlx::query(
                    "INSERT INTO pack_objects (ns, class, id, pack, off, len) \
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (ns, class, id) DO NOTHING",
                )
                .bind(ns.clone())
                .bind(i64::from(e.key.class().code()))
                .bind(e.key.id32().to_vec())
                .bind(info.id.0.to_vec())
                .bind(to_i(e.offset))
                .bind(i64::from(e.len))
                .execute(&mut *tx)
                .await
                .map_err(sql)?;
            }
        }
        tx.commit().await.map_err(sql)
    }
}

/// Records on first use that a namespace holds packs, so a server that does not know the
/// feature can refuse it (see `Registry`), and refuses writes while a restore is staged.
pub struct SqlPackGate {
    pool: AnyPool,
    ns: Ns,
}

/// Store features this build understands. A namespace that lists another is refused at open.
pub const KNOWN_FEATURES: &[&str] = &[FEATURE_PACK1];
/// Set when a namespace first receives a pack file.
pub const FEATURE_PACK1: &str = "pack1";

impl SqlPackGate {
    pub fn new(pool: AnyPool, ns: Ns) -> Self {
        Self { pool, ns }
    }
}

#[async_trait]
impl asc_store::pack::layer::PackWriteGate for SqlPackGate {
    async fn before_write(&self) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        sqlx::query(
            "INSERT INTO store_features (ns, feature, since) VALUES ($1, $2, $3) \
             ON CONFLICT (ns, feature) DO NOTHING",
        )
        .bind(self.ns.as_bytes().to_vec())
        .bind(FEATURE_PACK1)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(sql)?;
        Ok(())
    }
}

/// The store features of `ns` this build does not know, and whether the namespace holds packs.
pub async fn store_features(
    pool: &AnyPool,
    ns: Ns,
) -> std::result::Result<(Vec<String>, bool), sqlx::Error> {
    let rows = sqlx::query("SELECT feature FROM store_features WHERE ns = $1")
        .bind(ns.as_bytes().to_vec())
        .fetch_all(pool)
        .await?;
    let mut out = Vec::new();
    let mut packed = false;
    for r in rows {
        let f: String = r.try_get("feature")?;
        packed |= f == FEATURE_PACK1;
        if !KNOWN_FEATURES.contains(&f.as_str()) {
            out.push(f);
        }
    }
    Ok((out, packed))
}
