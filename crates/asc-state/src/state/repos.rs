//! `repos`: one row per repository known to the machine (SPEC §9.2).

use asc_store::refs::RepoKey;
use sqlx::Row;
use sqlx::any::AnyRow;

use super::{Error, Result, StateDb};
use crate::home::RepoId;

/// How a repository reaches its remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteKind {
    LocalOnly,
    Server,
    Bucket,
    GitRemote,
    /// The dedicated row that holds a bucket's `--refs local` refs (SPEC §3.4).
    BucketLocalRefs,
}

impl RemoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local-only",
            Self::Server => "server",
            Self::Bucket => "bucket",
            Self::GitRemote => "git-remote",
            Self::BucketLocalRefs => "bucket-local-refs",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "local-only" => Self::LocalOnly,
            "server" => Self::Server,
            "bucket" => Self::Bucket,
            "git-remote" => Self::GitRemote,
            "bucket-local-refs" => Self::BucketLocalRefs,
            _ => return Err(Error::Corrupt("repos.remote_kind")),
        })
    }
}

/// A repository to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRepo {
    pub id: RepoId,
    pub name: String,
    /// The repository namespace (SPEC §2.1); random for local-only repositories.
    pub ns: Vec<u8>,
    pub remote_url: Option<String>,
    pub remote_kind: RemoteKind,
    pub default_branch: String,
    pub windows_safe: bool,
}

/// A stored `repos` row. `key` is its rowid, the `repo_id` of every other table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub key: RepoKey,
    pub repo: NewRepo,
}

const COLS: &str = "rowid, id, name, ns, remote_url, remote_kind, default_branch, windows_safe";

fn decode(row: &AnyRow) -> Result<Repo> {
    let id: String = row.try_get("id")?;
    let kind: String = row.try_get("remote_kind")?;
    let safe: i64 = row.try_get("windows_safe")?;
    Ok(Repo {
        key: RepoKey(row.try_get("rowid")?),
        repo: NewRepo {
            id: id.parse().map_err(|_| Error::Corrupt("repos.id"))?,
            name: row.try_get("name")?,
            ns: row.try_get("ns")?,
            remote_url: row.try_get("remote_url")?,
            remote_kind: RemoteKind::parse(&kind)?,
            default_branch: row.try_get("default_branch")?,
            windows_safe: safe != 0,
        },
    })
}

async fn insert_tx(tx: &mut super::Tx, r: &NewRepo) -> Result<RepoKey> {
    let key: i64 = sqlx::query_scalar(
        "INSERT INTO repos (id, name, ns, remote_url, remote_kind, default_branch, windows_safe) \
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING rowid",
    )
    .bind(r.id.to_string())
    .bind(&r.name)
    .bind(&r.ns)
    .bind(&r.remote_url)
    .bind(r.remote_kind.as_str())
    .bind(&r.default_branch)
    .bind(i64::from(r.windows_safe))
    .fetch_one(&mut **tx)
    .await?;
    Ok(RepoKey(key))
}

/// Registers a repository.
pub async fn insert(db: &StateDb, r: &NewRepo) -> Result<RepoKey> {
    let mut tx = db.begin().await?;
    let key = insert_tx(&mut tx, r).await?;
    tx.commit().await?;
    Ok(key)
}

/// The repository with this key.
pub async fn get(db: &StateDb, key: RepoKey) -> Result<Option<Repo>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM repos WHERE rowid = ?"
    )))
    .bind(key.0)
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(decode).transpose()
}

/// Repositories with this name, oldest first. Bucket-refs rows are not repositories and are
/// left out.
pub async fn by_name(db: &StateDb, name: &str) -> Result<Vec<Repo>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM repos WHERE name = ? AND remote_kind <> ? ORDER BY rowid"
    )))
    .bind(name)
    .bind(RemoteKind::BucketLocalRefs.as_str())
    .fetch_all(db.pool())
    .await?;
    rows.iter().map(decode).collect()
}

/// Gets or inserts the dedicated `bucket-local-refs` row of the bucket at `url`
/// (SPEC §3.4). The row's `name` is the normalised bucket URL; `ns` stays empty until the first
/// push learns the bucket's namespace.
pub async fn bucket_refs_row(db: &StateDb, url: &str) -> Result<RepoKey> {
    let mut tx = db.begin().await?;
    let found: Option<i64> =
        sqlx::query_scalar("SELECT rowid FROM repos WHERE remote_kind = ? AND remote_url = ?")
            .bind(RemoteKind::BucketLocalRefs.as_str())
            .bind(url)
            .fetch_optional(&mut *tx)
            .await?;
    let key = match found {
        Some(k) => RepoKey(k),
        None => {
            let row = NewRepo {
                id: RepoId::new(),
                name: url.to_owned(),
                ns: Vec::new(),
                remote_url: Some(url.to_owned()),
                remote_kind: RemoteKind::BucketLocalRefs,
                default_branch: "master".to_owned(),
                windows_safe: false,
            };
            insert_tx(&mut tx, &row).await?
        }
    };
    tx.commit().await?;
    Ok(key)
}
