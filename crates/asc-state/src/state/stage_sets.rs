//! `stage_sets`: temporary stage sets named by a `GIT_INDEX_FILE` path (SPEC §9.5).

use sqlx::Row;

use super::stage::{self, SetId, StageRow, StageSet};
use super::{Error, Result, StateDb, Tx};
use crate::home::WorkspaceId;

/// A registered temporary set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageSetInfo {
    pub ws: WorkspaceId,
    pub set: SetId,
    pub index_path: String,
    pub created: i64,
}

/// Allocates the next set id of `ws`. `index_path` of `None` names an internal snapshot.
pub(super) async fn register_tx(tx: &mut Tx, ws: &str, index_path: Option<&str>) -> Result<SetId> {
    let next: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(set_id), 0) + 1 FROM stage_sets WHERE ws_id = ?")
            .bind(ws)
            .fetch_one(&mut **tx)
            .await?;
    let path = index_path.map_or_else(|| format!("snapshot:{ws}:{next}"), str::to_owned);
    sqlx::query("INSERT INTO stage_sets (ws_id, set_id, index_path, created) VALUES (?, ?, ?, ?)")
        .bind(ws)
        .bind(next)
        .bind(path)
        .bind(stage::created_now())
        .execute(&mut **tx)
        .await?;
    SetId::new(next).ok_or(Error::Corrupt("stage_sets.set_id"))
}

/// Registers an empty set for `index_path`.
pub async fn register(db: &StateDb, ws: WorkspaceId, index_path: &str) -> Result<SetId> {
    let mut tx = db.begin().await?;
    let set = register_tx(&mut tx, &ws.to_string(), Some(index_path)).await?;
    tx.commit().await?;
    Ok(set)
}

/// The set registered for `index_path`.
pub async fn lookup_by_path(db: &StateDb, index_path: &str) -> Result<Option<StageSetInfo>> {
    let row = sqlx::query(
        "SELECT ws_id, set_id, index_path, created FROM stage_sets WHERE index_path = ?",
    )
    .bind(index_path)
    .fetch_optional(db.pool())
    .await?;
    row.map(|r| {
        let ws: String = r.try_get("ws_id")?;
        Ok(StageSetInfo {
            ws: ws.parse().map_err(|_| Error::Corrupt("stage_sets.ws_id"))?,
            set: SetId::new(r.try_get("set_id")?).ok_or(Error::Corrupt("stage_sets.set_id"))?,
            index_path: r.try_get("index_path")?,
            created: r.try_get("created")?,
        })
    })
    .transpose()
}

/// Registers `index_path` and fills it with `rows`, which the caller parsed from an existing
/// index file.
pub async fn import(
    db: &StateDb,
    ws: WorkspaceId,
    index_path: &str,
    rows: &[StageRow],
) -> Result<SetId> {
    let set = register(db, ws, index_path).await?;
    stage::upsert(db, ws, StageSet::Temp(set), rows).await?;
    Ok(set)
}

/// Sets registered for the workspace, oldest first.
pub async fn list(db: &StateDb, ws: WorkspaceId) -> Result<Vec<StageSetInfo>> {
    let rows = sqlx::query(
        "SELECT set_id, index_path, created FROM stage_sets WHERE ws_id = ? ORDER BY set_id",
    )
    .bind(ws.to_string())
    .fetch_all(db.pool())
    .await?;
    rows.iter()
        .map(|r| {
            Ok(StageSetInfo {
                ws,
                set: SetId::new(r.try_get("set_id")?).ok_or(Error::Corrupt("stage_sets.set_id"))?,
                index_path: r.try_get("index_path")?,
                created: r.try_get("created")?,
            })
        })
        .collect()
}

/// Drops a set and its rows. Set 0 has no `SetId`, so it cannot be dropped.
pub async fn drop_set(db: &StateDb, ws: WorkspaceId, set: SetId) -> Result<()> {
    let ws = ws.to_string();
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM stage WHERE ws_id = ? AND set_id = ?")
        .bind(&ws)
        .bind(set.get())
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM stage_sets WHERE ws_id = ? AND set_id = ?")
        .bind(&ws)
        .bind(set.get())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
