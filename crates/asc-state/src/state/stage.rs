//! `stage`: asc's own staging area (SPEC §9.5). Set 0 is the workspace staging, an overlay on
//! HEAD with one row per path whose staged state differs from HEAD.

use asc_core::id::{FileId, GitOid};
use asc_core::object::Kind;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::{Error, Result, StateDb, Tx, fixed, now};
use crate::home::WorkspaceId;

/// A temporary stage set; never 0, which is the default set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SetId(i64);

impl SetId {
    /// `None` for 0 and negative ids.
    pub fn new(raw: i64) -> Option<Self> {
        (raw > 0).then_some(Self(raw))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// Which staging rows a call reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageSet {
    /// The workspace staging (set 0).
    Default,
    Temp(SetId),
}

impl StageSet {
    fn raw(self) -> i64 {
        match self {
            Self::Default => 0,
            Self::Temp(s) => s.0,
        }
    }
}

/// One staged path. `file: None` is a staged delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRow {
    pub path: Vec<u8>,
    /// 0 normal, 1-3 conflict stages.
    pub stage: u8,
    pub kind: Kind,
    pub file: Option<FileId>,
    pub size: u64,
    pub git: Option<GitOid>,
    pub intent_to_add: bool,
}

/// The entry of one path in a tree, as `rebase` needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub kind: Kind,
    pub file: FileId,
    pub size: u64,
    pub git: GitOid,
}

/// One entry of `leafdiff(from, to)`: the leaf of the path in each tree (`None` = absent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafDelta {
    pub path: Vec<u8>,
    pub from: Option<Leaf>,
    pub to: Option<Leaf>,
}

impl StageRow {
    /// The row that stages `leaf`, or a staged delete for `None`.
    pub fn from_leaf(path: Vec<u8>, leaf: Option<&Leaf>) -> Self {
        match leaf {
            Some(l) => Self {
                path,
                stage: 0,
                kind: l.kind,
                file: Some(l.file),
                size: l.size,
                git: Some(l.git),
                intent_to_add: false,
            },
            None => Self {
                path,
                stage: 0,
                kind: Kind::File,
                file: None,
                size: 0,
                git: None,
                intent_to_add: false,
            },
        }
    }

    /// Whether the row stages exactly `leaf` (`None` = a staged delete).
    fn equals(&self, leaf: Option<&Leaf>) -> bool {
        if self.stage != 0 || self.intent_to_add {
            return false;
        }
        match (self.file, leaf) {
            (None, None) => true,
            (Some(f), Some(l)) => {
                f == l.file && self.kind == l.kind && self.size == l.size && self.git == Some(l.git)
            }
            _ => false,
        }
    }
}

fn kind_from_i64(v: i64) -> Result<Kind> {
    Ok(match v {
        0 => Kind::File,
        1 => Kind::Exec,
        2 => Kind::Symlink,
        3 => Kind::Dir,
        4 => Kind::Gitlink,
        _ => return Err(Error::Corrupt("stage.kind")),
    })
}

/// The v1 tree format has no size limit (SPEC §0 decision 3), so a hostile tree can hold a size
/// above `i64::MAX`, and SQLite integers are signed. The column stores the 64-bit pattern of the
/// size, so every `u64` round-trips exactly. Rejecting such a size would leave the workspace
/// unable to rebase onto that HEAD, and truncating it would make a changed file look unchanged.
fn size_to_db(size: u64) -> i64 {
    i64::from_ne_bytes(size.to_ne_bytes())
}

fn size_from_db(size: i64) -> u64 {
    u64::from_ne_bytes(size.to_ne_bytes())
}

const COLS: &str = "path, stage, kind, file, size, git, intent_to_add";

fn decode(row: &AnyRow) -> Result<StageRow> {
    let file: Option<Vec<u8>> = row.try_get("file")?;
    let git: Option<Vec<u8>> = row.try_get("git")?;
    let stage: i64 = row.try_get("stage")?;
    let size: i64 = row.try_get("size")?;
    let ita: i64 = row.try_get("intent_to_add")?;
    Ok(StageRow {
        path: row.try_get("path")?,
        stage: u8::try_from(stage).map_err(|_| Error::Corrupt("stage.stage"))?,
        kind: kind_from_i64(row.try_get("kind")?)?,
        file: file
            .map(|b| fixed(b, "stage.file").map(FileId::from_bytes))
            .transpose()?,
        size: size_from_db(size),
        git: git
            .map(|b| fixed(b, "stage.git").map(GitOid::from_bytes))
            .transpose()?,
        intent_to_add: ita != 0,
    })
}

/// The highest conflict stage a row may hold (0 is a normal row).
const MAX_STAGE: u8 = 3;

async fn upsert_tx(tx: &mut Tx, ws: &str, set: i64, r: &StageRow) -> Result<()> {
    if r.stage > MAX_STAGE {
        return Err(Error::Corrupt("stage.stage"));
    }
    let size = size_to_db(r.size);
    sqlx::query(
        "INSERT INTO stage (ws_id, set_id, path, stage, kind, file, size, git, intent_to_add) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (ws_id, set_id, path, stage) DO UPDATE SET kind = excluded.kind, \
         file = excluded.file, size = excluded.size, git = excluded.git, \
         intent_to_add = excluded.intent_to_add",
    )
    .bind(ws)
    .bind(set)
    .bind(&r.path)
    .bind(i64::from(r.stage))
    .bind(r.kind as i64)
    .bind(r.file.map(|f| f.as_bytes().to_vec()))
    .bind(size)
    .bind(r.git.map(|g| g.as_bytes().to_vec()))
    .bind(i64::from(r.intent_to_add))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// All stages staged for one path.
pub async fn get(
    db: &StateDb,
    ws: WorkspaceId,
    set: StageSet,
    path: &[u8],
) -> Result<Vec<StageRow>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM stage WHERE ws_id = ? AND set_id = ? AND path = ? ORDER BY stage"
    )))
    .bind(ws.to_string())
    .bind(set.raw())
    .bind(path)
    .fetch_all(db.pool())
    .await?;
    rows.iter().map(decode).collect()
}

/// Every row of the set, in path order: the overlay on the HEAD tree. Its size is the number of
/// staged differences, not the size of the repository.
pub async fn overlay(db: &StateDb, ws: WorkspaceId, set: StageSet) -> Result<Vec<StageRow>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM stage WHERE ws_id = ? AND set_id = ? ORDER BY path, stage"
    )))
    .bind(ws.to_string())
    .bind(set.raw())
    .fetch_all(db.pool())
    .await?;
    rows.iter().map(decode).collect()
}

/// Inserts or replaces rows in one transaction.
pub async fn upsert(db: &StateDb, ws: WorkspaceId, set: StageSet, rows: &[StageRow]) -> Result<()> {
    let ws = ws.to_string();
    let mut tx = db.begin().await?;
    for r in rows {
        upsert_tx(&mut tx, &ws, set.raw(), r).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Replaces every row of the set with `rows` in one transaction. A failure leaves the old rows
/// in place; `clear` followed by `upsert` would leave the set empty or partial instead.
pub async fn replace(
    db: &StateDb,
    ws: WorkspaceId,
    set: StageSet,
    rows: &[StageRow],
) -> Result<()> {
    let mut tx = db.begin().await?;
    replace_tx(&mut tx, ws, set, rows).await?;
    tx.commit().await?;
    Ok(())
}

/// [`replace`] inside the caller's transaction, so it commits or rolls back with a ref move.
pub async fn replace_tx(tx: &mut Tx, ws: WorkspaceId, set: StageSet, rows: &[StageRow]) -> Result<()> {
    let ws = ws.to_string();
    sqlx::query("DELETE FROM stage WHERE ws_id = ? AND set_id = ?")
        .bind(&ws)
        .bind(set.raw())
        .execute(&mut **tx)
        .await?;
    for r in rows {
        upsert_tx(tx, &ws, set.raw(), r).await?;
    }
    Ok(())
}

/// Removes every stage of one path (the path is staged as in HEAD again).
pub async fn remove(db: &StateDb, ws: WorkspaceId, set: StageSet, path: &[u8]) -> Result<()> {
    sqlx::query("DELETE FROM stage WHERE ws_id = ? AND set_id = ? AND path = ?")
        .bind(ws.to_string())
        .bind(set.raw())
        .bind(path)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Removes every row of the set.
pub async fn clear(db: &StateDb, ws: WorkspaceId, set: StageSet) -> Result<()> {
    sqlx::query("DELETE FROM stage WHERE ws_id = ? AND set_id = ?")
        .bind(ws.to_string())
        .bind(set.raw())
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Copies set 0 into a new registered set so that a failed commit can restore it (SPEC §9.7
/// step 0). Drop the snapshot with [`super::stage_sets::drop_set`] when it is no longer needed.
pub async fn snapshot(db: &StateDb, ws: WorkspaceId) -> Result<SetId> {
    let wsid = ws.to_string();
    let mut tx = db.begin().await?;
    let set = super::stage_sets::register_tx(&mut tx, &wsid, None).await?;
    sqlx::query(
        "INSERT INTO stage (ws_id, set_id, path, stage, kind, file, size, git, intent_to_add) \
         SELECT ws_id, ?, path, stage, kind, file, size, git, intent_to_add \
         FROM stage WHERE ws_id = ? AND set_id = 0",
    )
    .bind(set.0)
    .bind(&wsid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(set)
}

/// Replaces set 0 with the rows of snapshot `from`.
pub async fn restore(db: &StateDb, ws: WorkspaceId, from: SetId) -> Result<()> {
    let wsid = ws.to_string();
    let mut tx = db.begin().await?;
    let known: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stage_sets WHERE ws_id = ? AND set_id = ?")
            .bind(&wsid)
            .bind(from.0)
            .fetch_one(&mut *tx)
            .await?;
    if known == 0 {
        return Err(Error::NotFound("stage set"));
    }
    sqlx::query("DELETE FROM stage WHERE ws_id = ? AND set_id = 0")
        .bind(&wsid)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO stage (ws_id, set_id, path, stage, kind, file, size, git, intent_to_add) \
         SELECT ws_id, 0, path, stage, kind, file, size, git, intent_to_add \
         FROM stage WHERE ws_id = ? AND set_id = ?",
    )
    .bind(&wsid)
    .bind(from.0)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Rewrites set 0 across a HEAD move so that `to_tree ⊕ rows'` equals `from_tree ⊕ rows`
/// (SPEC §9.5). `deltas` is `leafdiff(from_tree, to_tree)`, supplied by the caller so that this
/// crate does not depend on the tree walker. Costs O(deltas).
///
/// A path without a row gets a row holding the `from` leaf (or a staged delete); a path whose
/// single normal row now equals the `to` leaf loses it. Conflict and intent-to-add rows stay.
pub async fn rebase(db: &StateDb, ws: WorkspaceId, deltas: &[LeafDelta]) -> Result<()> {
    let mut tx = db.begin().await?;
    rebase_tx(&mut tx, ws, deltas).await?;
    tx.commit().await?;
    Ok(())
}

/// [`rebase`] inside the caller's transaction. A rebase is not idempotent (a row that equalled
/// the `to` leaf is dropped), so the caller must commit it together with the move of the base
/// it is relative to.
pub async fn rebase_tx(tx: &mut Tx, ws: WorkspaceId, deltas: &[LeafDelta]) -> Result<()> {
    let wsid = ws.to_string();
    for d in deltas {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM stage WHERE ws_id = ? AND set_id = 0 AND path = ?"
        )))
        .bind(&wsid)
        .bind(&d.path)
        .fetch_all(&mut **tx)
        .await?;
        let rows = rows.iter().map(decode).collect::<Result<Vec<_>>>()?;
        match rows.as_slice() {
            [] => {
                let row = StageRow::from_leaf(d.path.clone(), d.from.as_ref());
                upsert_tx(tx, &wsid, 0, &row).await?;
            }
            [only] if only.equals(d.to.as_ref()) => {
                sqlx::query("DELETE FROM stage WHERE ws_id = ? AND set_id = 0 AND path = ?")
                    .bind(&wsid)
                    .bind(&d.path)
                    .execute(&mut **tx)
                    .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn created_now() -> i64 {
    now()
}
