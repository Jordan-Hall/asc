//! `workspaces`: mounts and materialised working copies (SPEC §9.2, §9.3).

use asc_core::id::ChangesetId;
use asc_store::refs::RepoKey;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::{Error, Result, StateDb, Tx, fixed, now};
use crate::home::WorkspaceId;

/// How the working copy is realised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Virtual,
    Plain,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Virtual => "virtual",
            Self::Plain => "plain",
        }
    }
}

/// A workspace to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorkspace {
    pub id: WorkspaceId,
    pub repo: RepoKey,
    /// The requested path.
    pub path: String,
    /// The mount root; differs from `path` for nested paths (SPEC §9.3).
    pub mount: String,
    pub mode: Mode,
    /// The checked-out ref name, or empty when detached.
    pub head: String,
    pub base_cs: Option<ChangesetId>,
    pub sparse: Option<String>,
    pub ephemeral_until: Option<i64>,
}

/// A stored workspace row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub ws: NewWorkspace,
    pub epoch: i64,
    /// Unix seconds of the last epoch bump.
    pub epoch_time: i64,
    pub locked: Option<String>,
    pub created: i64,
}

const COLS: &str = "id, repo_id, path, mount, mode, head, base_cs, epoch, epoch_time, sparse, \
                    ephemeral_until, locked, created";

fn decode(row: &AnyRow) -> Result<Workspace> {
    let id: String = row.try_get("id")?;
    let mode: String = row.try_get("mode")?;
    let base: Option<Vec<u8>> = row.try_get("base_cs")?;
    Ok(Workspace {
        ws: NewWorkspace {
            id: id.parse().map_err(|_| Error::Corrupt("workspaces.id"))?,
            repo: RepoKey(row.try_get("repo_id")?),
            path: row.try_get("path")?,
            mount: row.try_get("mount")?,
            mode: match mode.as_str() {
                "virtual" => Mode::Virtual,
                "plain" => Mode::Plain,
                _ => return Err(Error::Corrupt("workspaces.mode")),
            },
            head: row.try_get("head")?,
            base_cs: base
                .map(|b| fixed(b, "workspaces.base_cs").map(ChangesetId::from_bytes))
                .transpose()?,
            sparse: row.try_get("sparse")?,
            ephemeral_until: row.try_get("ephemeral_until")?,
        },
        epoch: row.try_get("epoch")?,
        epoch_time: row.try_get("epoch_time")?,
        locked: row.try_get("locked")?,
        created: row.try_get("created")?,
    })
}

async fn one(db: &StateDb, filter: &str, arg: &str) -> Result<Option<Workspace>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workspaces WHERE {filter}"
    )))
    .bind(arg)
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(decode).transpose()
}

/// Registers a workspace at epoch 0.
pub async fn insert(db: &StateDb, w: &NewWorkspace) -> Result<()> {
    let t = now();
    sqlx::query(
        "INSERT INTO workspaces (id, repo_id, path, mount, mode, head, base_cs, epoch, epoch_time, \
         sparse, ephemeral_until, created) VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?)",
    )
    .bind(w.id.to_string())
    .bind(w.repo.0)
    .bind(&w.path)
    .bind(&w.mount)
    .bind(w.mode.as_str())
    .bind(&w.head)
    .bind(w.base_cs.map(|c| c.as_bytes().to_vec()))
    .bind(t)
    .bind(&w.sparse)
    .bind(w.ephemeral_until)
    .bind(t)
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn get(db: &StateDb, id: WorkspaceId) -> Result<Option<Workspace>> {
    one(db, "id = ?", &id.to_string()).await
}

/// By the requested path.
pub async fn by_path(db: &StateDb, path: &str) -> Result<Option<Workspace>> {
    one(db, "path = ?", path).await
}

/// By mount root. Nested workspaces share no mount, so at most one row matches.
pub async fn by_mount(db: &StateDb, mount: &str) -> Result<Option<Workspace>> {
    one(db, "mount = ?", mount).await
}

/// Moves HEAD and the base changeset together.
pub async fn update_head_base(
    db: &StateDb,
    id: WorkspaceId,
    head: &str,
    base: Option<ChangesetId>,
) -> Result<()> {
    let mut tx = db.begin().await?;
    update_head_base_tx(&mut tx, id, head, base).await?;
    tx.commit().await?;
    Ok(())
}

/// [`update_head_base`] inside the caller's transaction, so a ref move, the stage rebase and the
/// new base commit or roll back together.
pub async fn update_head_base_tx(
    tx: &mut Tx,
    id: WorkspaceId,
    head: &str,
    base: Option<ChangesetId>,
) -> Result<()> {
    let done = sqlx::query("UPDATE workspaces SET head = ?, base_cs = ? WHERE id = ?")
        .bind(head)
        .bind(base.map(|c| c.as_bytes().to_vec()))
        .bind(id.to_string())
        .execute(&mut **tx)
        .await?;
    if done.rows_affected() == 0 {
        return Err(Error::NotFound("workspace"));
    }
    Ok(())
}

/// The base changeset of the workspace as the caller's transaction sees it: the commit the
/// stage rows are relative to. `None` for an unborn HEAD.
pub async fn base_cs_tx(tx: &mut Tx, id: WorkspaceId) -> Result<Option<ChangesetId>> {
    let row: Option<Option<Vec<u8>>> =
        sqlx::query_scalar("SELECT base_cs FROM workspaces WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut **tx)
            .await?;
    let Some(base) = row else {
        return Err(Error::NotFound("workspace"));
    };
    base.map(|b| fixed(b, "workspaces.base_cs").map(ChangesetId::from_bytes))
        .transpose()
}

/// Increments the epoch and stamps its time; returns the new epoch.
pub async fn bump_epoch(db: &StateDb, id: WorkspaceId) -> Result<i64> {
    sqlx::query_scalar(
        "UPDATE workspaces SET epoch = epoch + 1, epoch_time = ? WHERE id = ? RETURNING epoch",
    )
    .bind(now())
    .bind(id.to_string())
    .fetch_optional(db.pool())
    .await?
    .ok_or(Error::NotFound("workspace"))
}

/// Workspaces of one repository, or of all when `repo` is `None`, oldest first.
pub async fn list(db: &StateDb, repo: Option<RepoKey>) -> Result<Vec<Workspace>> {
    let rows = match repo {
        Some(r) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {COLS} FROM workspaces WHERE repo_id = ? ORDER BY created, id"
            )))
            .bind(r.0)
            .fetch_all(db.pool())
            .await?
        }
        None => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {COLS} FROM workspaces ORDER BY created, id"
            )))
            .fetch_all(db.pool())
            .await?
        }
    };
    rows.iter().map(decode).collect()
}

/// The workspace of `repo` that has `branch` (a short name) checked out: git's "branch in use"
/// rule (SPEC §9.3).
pub async fn holding_branch(
    db: &StateDb,
    repo: RepoKey,
    branch: &str,
) -> Result<Option<Workspace>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM workspaces WHERE repo_id = ? AND head = ? ORDER BY created, id LIMIT 1"
    )))
    .bind(repo.0)
    .bind(format!("refs/heads/{branch}"))
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(decode).transpose()
}

/// Deletes the workspace and every per-workspace row.
pub async fn remove(db: &StateDb, id: WorkspaceId) -> Result<()> {
    let id = id.to_string();
    let mut tx = db.begin().await?;
    for table in [
        "stage",
        "stage_sets",
        "entry_times",
        "journal",
        "switch_intent",
        "ws_ops",
        "inodes",
        "workspaces",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {} = ?",
            if table == "workspaces" { "id" } else { "ws_id" }
        )))
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
