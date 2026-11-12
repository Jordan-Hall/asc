//! `prefetch_profiles`: learned prefetch profiles (SPEC §10.2 item 5b). A profile is named after
//! the build target or plugin entry that read the paths (`vite:app`, `rspack:web`, a name given
//! to `asc prefetch --record`); `asc prefetch --profile` and the warm after a switch or clone
//! read them back.
//!
//! Bounded: a profile keeps at most [`MAX_PATHS`] paths (the least named and oldest go first) and
//! a repository at most [`MAX_PROFILES`] profiles (the least recently learned goes first).

use asc_core::path::RepoPath;
use asc_store::refs::RepoKey;
use sqlx::Row;

use crate::state::{Error, StateDb};

/// Most paths one profile keeps.
pub const MAX_PATHS: usize = 50_000;
/// Most profiles one repository keeps.
pub const MAX_PROFILES: usize = 64;
/// Longest profile name, in bytes.
pub const MAX_NAME_LEN: usize = 200;

/// Failures from the profile table.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error(transparent)]
    State(#[from] Error),
    #[error("a profile name is 1 to {MAX_NAME_LEN} printable bytes without spaces")]
    BadName,
    #[error("a learn names at most {MAX_PATHS} paths")]
    TooMany,
}

impl From<sqlx::Error> for ProfileError {
    fn from(e: sqlx::Error) -> Self {
        Self::State(e.into())
    }
}

pub type Result<T, E = ProfileError> = std::result::Result<T, E>;

/// One profile in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub paths: u64,
    /// Unix seconds of the last learn.
    pub last_used: i64,
}

/// A name is printable ASCII or UTF-8 without whitespace or control characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Merges `paths` into the profile `name` (or replaces it with them when `replace`), stamped
/// `now`. Answers the number of paths the profile holds afterwards.
pub async fn learn(
    db: &StateDb,
    repo: RepoKey,
    name: &str,
    paths: &[RepoPath],
    replace: bool,
    now: i64,
) -> Result<u64> {
    if !valid_name(name) {
        return Err(ProfileError::BadName);
    }
    if paths.len() > MAX_PATHS {
        return Err(ProfileError::TooMany);
    }
    let mut tx = db.begin().await?;
    if replace {
        sqlx::query("DELETE FROM prefetch_profiles WHERE repo_id = ? AND name = ?")
            .bind(repo.0)
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }
    for p in paths {
        sqlx::query(
            "INSERT INTO prefetch_profiles (repo_id, name, path, hits, last_used) \
             VALUES (?, ?, ?, 1, ?) \
             ON CONFLICT (repo_id, name, path) DO UPDATE SET hits = prefetch_profiles.hits + 1, \
             last_used = excluded.last_used",
        )
        .bind(repo.0)
        .bind(name)
        .bind(p.as_bytes().to_vec())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    // The profile's own bound: drop the least named, then the oldest.
    let held: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM prefetch_profiles WHERE repo_id = ? AND name = ?")
            .bind(repo.0)
            .bind(name)
            .fetch_one(&mut *tx)
            .await?;
    let over = (held as usize).saturating_sub(MAX_PATHS);
    if over > 0 {
        sqlx::query(
            "DELETE FROM prefetch_profiles WHERE repo_id = ? AND name = ? AND path IN \
             (SELECT path FROM prefetch_profiles WHERE repo_id = ? AND name = ? \
              ORDER BY hits ASC, last_used ASC, path ASC LIMIT ?)",
        )
        .bind(repo.0)
        .bind(name)
        .bind(repo.0)
        .bind(name)
        .bind(over as i64)
        .execute(&mut *tx)
        .await?;
    }
    // The repository's bound: drop the profiles learned least recently.
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM prefetch_profiles WHERE repo_id = ? GROUP BY name \
         ORDER BY MAX(last_used) DESC, name ASC",
    )
    .bind(repo.0)
    .fetch_all(&mut *tx)
    .await?;
    for old in names.iter().skip(MAX_PROFILES) {
        sqlx::query("DELETE FROM prefetch_profiles WHERE repo_id = ? AND name = ?")
            .bind(repo.0)
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((held as usize).min(MAX_PATHS) as u64)
}

/// The paths of the profile `name`, most named first; empty when there is no such profile.
pub async fn paths(db: &StateDb, repo: RepoKey, name: &str) -> Result<Vec<RepoPath>> {
    let rows = sqlx::query(
        "SELECT path FROM prefetch_profiles WHERE repo_id = ? AND name = ? \
         ORDER BY hits DESC, path ASC LIMIT ?",
    )
    .bind(repo.0)
    .bind(name)
    .bind(MAX_PATHS as i64)
    .fetch_all(db.pool())
    .await?;
    rows.iter()
        .map(|r| {
            let b: Vec<u8> = r.try_get("path")?;
            RepoPath::new(b).map_err(|_| Error::Corrupt("prefetch_profiles.path").into())
        })
        .collect()
}

/// Every profile of the repository, most recently learned first.
pub async fn list(db: &StateDb, repo: RepoKey) -> Result<Vec<Profile>> {
    let rows = sqlx::query(
        "SELECT name, COUNT(*) AS n, MAX(last_used) AS used FROM prefetch_profiles \
         WHERE repo_id = ? GROUP BY name ORDER BY MAX(last_used) DESC, name ASC",
    )
    .bind(repo.0)
    .fetch_all(db.pool())
    .await?;
    rows.iter()
        .map(|r| {
            Ok(Profile {
                name: r.try_get("name")?,
                paths: r.try_get::<i64, _>("n")?.max(0) as u64,
                last_used: r.try_get("used")?,
            })
        })
        .collect()
}

/// The names of the profiles learned at or after `since` (unix seconds), most recent first.
pub async fn recent(db: &StateDb, repo: RepoKey, since: i64) -> Result<Vec<String>> {
    Ok(list(db, repo)
        .await?
        .into_iter()
        .filter(|p| p.last_used >= since)
        .map(|p| p.name)
        .collect())
}

/// A new clone of `remote_url` (the repository `to`) takes the profiles other clones of the same
/// remote on this machine learned, so its first warm already knows what the builds read. Answers
/// the paths copied; the bounds hold as they did in the sources.
pub async fn adopt(db: &StateDb, to: RepoKey, remote_url: &str) -> Result<u64> {
    let r = sqlx::query(
        "INSERT OR IGNORE INTO prefetch_profiles (repo_id, name, path, hits, last_used)          SELECT ?, name, path, MAX(hits), MAX(last_used) FROM prefetch_profiles          WHERE repo_id IN (SELECT rowid FROM repos WHERE remote_url = ? AND rowid <> ?)          GROUP BY name, path          ORDER BY MAX(last_used) DESC LIMIT ?",
    )
    .bind(to.0)
    .bind(remote_url)
    .bind(to.0)
    .bind((MAX_PATHS * MAX_PROFILES) as i64)
    .execute(db.pool())
    .await?;
    Ok(r.rows_affected())
}

/// Removes the profile `name`; answers whether it existed.
pub async fn forget(db: &StateDb, repo: RepoKey, name: &str) -> Result<bool> {
    let r = sqlx::query("DELETE FROM prefetch_profiles WHERE repo_id = ? AND name = ?")
        .bind(repo.0)
        .bind(name)
        .execute(db.pool())
        .await?;
    Ok(r.rows_affected() > 0)
}

#[cfg(test)]
mod tests;
