//! Smaller `state.db` tables: graph metadata, the git map and per-workspace bookkeeping.

use super::{Error, Result, StateDb, fixed, now, pack, unpack};
use crate::home::WorkspaceId;

/// `cs_gen`: generation numbers and parents of local changesets (SPEC §6.3).
pub mod cs_gen {
    use asc_core::id::ChangesetId;
    use asc_store::refs::RepoKey;
    use sqlx::Row;

    use super::{Error, Result, StateDb, fixed};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct CsGen {
        pub cs: ChangesetId,
        pub gen_no: u64,
        pub parents: Vec<ChangesetId>,
    }

    /// Records rows; an existing row is kept (generations are immutable).
    pub async fn insert_many(db: &StateDb, repo: RepoKey, rows: &[CsGen]) -> Result<()> {
        let mut tx = db.begin().await?;
        for r in rows {
            let parents: Vec<u8> = r.parents.iter().flat_map(|p| *p.as_bytes()).collect();
            sqlx::query(
                "INSERT OR IGNORE INTO cs_gen (repo_id, cs, gen, parents) VALUES (?, ?, ?, ?)",
            )
            .bind(repo.0)
            .bind(r.cs.as_bytes().to_vec())
            .bind(r.gen_no as i64)
            .bind(parents)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get(db: &StateDb, repo: RepoKey, cs: ChangesetId) -> Result<Option<CsGen>> {
        let Some(row) = sqlx::query("SELECT gen, parents FROM cs_gen WHERE repo_id = ? AND cs = ?")
            .bind(repo.0)
            .bind(cs.as_bytes().to_vec())
            .fetch_optional(db.pool())
            .await?
        else {
            return Ok(None);
        };
        let gen_no: i64 = row.try_get("gen")?;
        let parents: Vec<u8> = row.try_get("parents")?;
        if !parents.len().is_multiple_of(32) {
            return Err(Error::Corrupt("cs_gen.parents"));
        }
        let parents = parents
            .chunks_exact(32)
            .map(|c| fixed(c.to_vec(), "cs_gen.parents").map(ChangesetId::from_bytes))
            .collect::<Result<_>>()?;
        Ok(Some(CsGen {
            cs,
            gen_no: u64::try_from(gen_no).map_err(|_| Error::Corrupt("cs_gen.gen"))?,
            parents,
        }))
    }
}

/// `history_boundary`: the oldest local changesets whose parents are not local (SPEC §6.3).
pub mod history_boundary {
    use asc_core::id::ChangesetId;
    use asc_store::refs::RepoKey;

    use super::{Result, StateDb};

    pub async fn insert(db: &StateDb, repo: RepoKey, cs: ChangesetId) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO history_boundary (repo_id, cs) VALUES (?, ?)")
            .bind(repo.0)
            .bind(cs.as_bytes().to_vec())
            .execute(db.pool())
            .await?;
        Ok(())
    }

    pub async fn contains(db: &StateDb, repo: RepoKey, cs: ChangesetId) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM history_boundary WHERE repo_id = ? AND cs = ?",
        )
        .bind(repo.0)
        .bind(cs.as_bytes().to_vec())
        .fetch_one(db.pool())
        .await?;
        Ok(n > 0)
    }

    pub async fn remove(db: &StateDb, repo: RepoKey, cs: ChangesetId) -> Result<()> {
        sqlx::query("DELETE FROM history_boundary WHERE repo_id = ? AND cs = ?")
            .bind(repo.0)
            .bind(cs.as_bytes().to_vec())
            .execute(db.pool())
            .await?;
        Ok(())
    }
}

/// `git_map`: `(git oid, kind) -> asc id` (SPEC §5).
pub mod git_map {
    use async_trait::async_trait;
    use asc_core::git::ObjectKind;
    use asc_core::id::GitOid;
    use asc_engine::derive::{Error as DeriveError, GitMapRow, GitMapSink};
    use asc_store::refs::RepoKey;
    use sqlx::Row;

    use super::{Error, Result, StateDb, fixed};

    fn kind_to_i64(k: ObjectKind) -> i64 {
        match k {
            ObjectKind::Blob => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Commit => 3,
            ObjectKind::Tag => 4,
        }
    }

    fn kind_from_i64(v: i64) -> Result<ObjectKind> {
        Ok(match v {
            1 => ObjectKind::Blob,
            2 => ObjectKind::Tree,
            3 => ObjectKind::Commit,
            4 => ObjectKind::Tag,
            _ => return Err(Error::Corrupt("git_map.kind")),
        })
    }

    /// Records rows in one transaction; recording a row twice is harmless.
    pub async fn insert_many(db: &StateDb, repo: RepoKey, rows: &[GitMapRow]) -> Result<()> {
        let mut tx = db.begin().await?;
        for r in rows {
            sqlx::query(
                "INSERT OR IGNORE INTO git_map (repo_id, git, kind, asc) VALUES (?, ?, ?, ?)",
            )
            .bind(repo.0)
            .bind(r.git.as_bytes().to_vec())
            .bind(kind_to_i64(r.kind))
            .bind(r.asc.to_vec())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn git_to_asc(
        db: &StateDb,
        repo: RepoKey,
        git: GitOid,
        kind: ObjectKind,
    ) -> Result<Option<[u8; 32]>> {
        let asc: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT asc FROM git_map WHERE repo_id = ? AND git = ? AND kind = ?")
                .bind(repo.0)
                .bind(git.as_bytes().to_vec())
                .bind(kind_to_i64(kind))
                .fetch_optional(db.pool())
                .await?;
        asc.map(|b| fixed(b, "git_map.asc")).transpose()
    }

    /// Every git object an asc id maps to (a file id can back several blobs only by collision;
    /// a changeset has one commit).
    pub async fn asc_to_git(
        db: &StateDb,
        repo: RepoKey,
        asc: &[u8; 32],
    ) -> Result<Vec<(GitOid, ObjectKind)>> {
        let rows = sqlx::query("SELECT git, kind FROM git_map WHERE repo_id = ? AND asc = ?")
            .bind(repo.0)
            .bind(asc.to_vec())
            .fetch_all(db.pool())
            .await?;
        rows.iter()
            .map(|r| {
                let git: Vec<u8> = r.try_get("git")?;
                Ok((
                    GitOid::from_bytes(fixed(git, "git_map.git")?),
                    kind_from_i64(r.try_get("kind")?)?,
                ))
            })
            .collect()
    }

    /// The `state.db` git map of one repository, as a [`GitMapSink`].
    #[derive(Clone, Debug)]
    pub struct RepoGitMap {
        pub db: StateDb,
        pub repo: RepoKey,
    }

    #[async_trait]
    impl GitMapSink for RepoGitMap {
        async fn put(&self, rows: &[GitMapRow]) -> asc_engine::derive::Result<()> {
            insert_many(&self.db, self.repo, rows)
                .await
                .map_err(|e| DeriveError::Sink(e.to_string()))
        }
    }
}

/// `entry_times`: placeholder time overrides (SPEC §10.1).
pub mod entry_times {
    use super::{Result, StateDb, WorkspaceId};

    pub async fn set(db: &StateDb, ws: WorkspaceId, path: &[u8], time: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO entry_times (ws_id, path, time) VALUES (?, ?, ?) \
             ON CONFLICT (ws_id, path) DO UPDATE SET time = excluded.time",
        )
        .bind(ws.to_string())
        .bind(path)
        .bind(time)
        .execute(db.pool())
        .await?;
        Ok(())
    }

    pub async fn get(db: &StateDb, ws: WorkspaceId, path: &[u8]) -> Result<Option<i64>> {
        Ok(
            sqlx::query_scalar("SELECT time FROM entry_times WHERE ws_id = ? AND path = ?")
                .bind(ws.to_string())
                .bind(path)
                .fetch_optional(db.pool())
                .await?,
        )
    }

    pub async fn remove(db: &StateDb, ws: WorkspaceId, path: &[u8]) -> Result<()> {
        sqlx::query("DELETE FROM entry_times WHERE ws_id = ? AND path = ?")
            .bind(ws.to_string())
            .bind(path)
            .execute(db.pool())
            .await?;
        Ok(())
    }
}

/// `journal`: change hints appended in batches (SPEC §9.2, §10.2.7).
pub mod journal {
    use sqlx::Row;

    use super::{Error, Result, StateDb, WorkspaceId};

    /// What happened to a path.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Op {
        Modified = 1,
        Created = 2,
        Deleted = 3,
        RenamedFrom = 4,
        DirRenamedFrom = 5,
        DirRenamedTo = 6,
    }

    impl Op {
        fn from_i64(v: i64) -> Result<Self> {
            Ok(match v {
                1 => Self::Modified,
                2 => Self::Created,
                3 => Self::Deleted,
                4 => Self::RenamedFrom,
                5 => Self::DirRenamedFrom,
                6 => Self::DirRenamedTo,
                _ => return Err(Error::Corrupt("journal.op")),
            })
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Entry {
        pub path: Vec<u8>,
        pub op: Op,
    }

    /// An entry with its sequence number.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Record {
        pub seq: i64,
        pub entry: Entry,
    }

    /// Appends a batch in one transaction; returns the last sequence number.
    pub async fn append(db: &StateDb, ws: WorkspaceId, batch: &[Entry]) -> Result<i64> {
        let wsid = ws.to_string();
        let mut tx = db.begin().await?;
        let mut seq: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM journal WHERE ws_id = ?")
                .bind(&wsid)
                .fetch_one(&mut *tx)
                .await?;
        for e in batch {
            seq += 1;
            sqlx::query("INSERT INTO journal (ws_id, seq, path, op) VALUES (?, ?, ?, ?)")
                .bind(&wsid)
                .bind(seq)
                .bind(&e.path)
                .bind(e.op as i64)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(seq)
    }

    /// Entries with `seq > after`, oldest first, at most `limit`.
    pub async fn since(
        db: &StateDb,
        ws: WorkspaceId,
        after: i64,
        limit: u32,
    ) -> Result<Vec<Record>> {
        let rows = sqlx::query(
            "SELECT seq, path, op FROM journal WHERE ws_id = ? AND seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(ws.to_string())
        .bind(after)
        .bind(i64::from(limit))
        .fetch_all(db.pool())
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Record {
                    seq: r.try_get("seq")?,
                    entry: Entry {
                        path: r.try_get("path")?,
                        op: Op::from_i64(r.try_get("op")?)?,
                    },
                })
            })
            .collect()
    }

    pub async fn clear(db: &StateDb, ws: WorkspaceId) -> Result<()> {
        sqlx::query("DELETE FROM journal WHERE ws_id = ?")
            .bind(ws.to_string())
            .execute(db.pool())
            .await?;
        Ok(())
    }
}

/// `switch_intent`: the record that lets an interrupted switch resume (SPEC §9.2).
pub mod switch_intent {
    use asc_core::id::TreeId;
    use sqlx::Row;

    use super::{Result, StateDb, WorkspaceId, fixed, now, pack, unpack};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SwitchIntent {
        pub from_root: TreeId,
        pub to_root: TreeId,
        pub to_head: String,
        /// Paths still to write.
        pub pending: Vec<Vec<u8>>,
        pub started: i64,
    }

    /// Records the intent, replacing any earlier one. `started` is stamped now.
    pub async fn set(db: &StateDb, ws: WorkspaceId, i: &SwitchIntent) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO switch_intent (ws_id, from_root, to_root, to_head, pending, \
             started) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(ws.to_string())
        .bind(i.from_root.as_bytes().to_vec())
        .bind(i.to_root.as_bytes().to_vec())
        .bind(&i.to_head)
        .bind(pack(i.pending.iter().map(Vec::as_slice)))
        .bind(now())
        .execute(db.pool())
        .await?;
        Ok(())
    }

    pub async fn get(db: &StateDb, ws: WorkspaceId) -> Result<Option<SwitchIntent>> {
        let Some(r) = sqlx::query(
            "SELECT from_root, to_root, to_head, pending, started FROM switch_intent \
             WHERE ws_id = ?",
        )
        .bind(ws.to_string())
        .fetch_optional(db.pool())
        .await?
        else {
            return Ok(None);
        };
        let pending: Vec<u8> = r.try_get("pending")?;
        Ok(Some(SwitchIntent {
            from_root: TreeId::from_bytes(fixed(r.try_get("from_root")?, "switch_intent.from")?),
            to_root: TreeId::from_bytes(fixed(r.try_get("to_root")?, "switch_intent.to")?),
            to_head: r.try_get("to_head")?,
            pending: unpack(&pending, "switch_intent.pending")?,
            started: r.try_get("started")?,
        }))
    }

    pub async fn clear(db: &StateDb, ws: WorkspaceId) -> Result<()> {
        sqlx::query("DELETE FROM switch_intent WHERE ws_id = ?")
            .bind(ws.to_string())
            .execute(db.pool())
            .await?;
        Ok(())
    }
}

/// `ws_ops`: pending `asc mv` renames only (SPEC §9.2).
pub mod ws_ops {
    use super::{Error, Result, StateDb, WorkspaceId, pack, unpack};

    /// `(from, to)` path pairs.
    pub type Renames = Vec<(Vec<u8>, Vec<u8>)>;

    pub async fn set_pending_renames(db: &StateDb, ws: WorkspaceId, r: &Renames) -> Result<()> {
        let flat = pack(r.iter().flat_map(|(a, b)| [a.as_slice(), b.as_slice()]));
        sqlx::query("INSERT OR REPLACE INTO ws_ops (ws_id, pending_renames) VALUES (?, ?)")
            .bind(ws.to_string())
            .bind(flat)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    pub async fn pending_renames(db: &StateDb, ws: WorkspaceId) -> Result<Renames> {
        let blob: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT pending_renames FROM ws_ops WHERE ws_id = ?")
                .bind(ws.to_string())
                .fetch_optional(db.pool())
                .await?;
        let items = unpack(&blob.unwrap_or_default(), "ws_ops.pending_renames")?;
        if !items.len().is_multiple_of(2) {
            return Err(Error::Corrupt("ws_ops.pending_renames"));
        }
        let mut it = items.into_iter();
        let mut out = Vec::new();
        while let (Some(a), Some(b)) = (it.next(), it.next()) {
            out.push((a, b));
        }
        Ok(out)
    }

    pub async fn clear(db: &StateDb, ws: WorkspaceId) -> Result<()> {
        sqlx::query("DELETE FROM ws_ops WHERE ws_id = ?")
            .bind(ws.to_string())
            .execute(db.pool())
            .await?;
        Ok(())
    }
}

/// `inodes`: the FUSE inode table (SPEC §9.2).
pub mod inodes {
    use super::{Result, StateDb, WorkspaceId};

    pub async fn assign(db: &StateDb, ws: WorkspaceId, ino: u64, path: &[u8]) -> Result<()> {
        sqlx::query("INSERT INTO inodes (ws_id, ino, path) VALUES (?, ?, ?)")
            .bind(ws.to_string())
            .bind(ino as i64)
            .bind(path)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    pub async fn path_of(db: &StateDb, ws: WorkspaceId, ino: u64) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT path FROM inodes WHERE ws_id = ? AND ino = ?")
                .bind(ws.to_string())
                .bind(ino as i64)
                .fetch_optional(db.pool())
                .await?,
        )
    }

    pub async fn ino_of(db: &StateDb, ws: WorkspaceId, path: &[u8]) -> Result<Option<u64>> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT ino FROM inodes WHERE ws_id = ? AND path = ?")
                .bind(ws.to_string())
                .bind(path)
                .fetch_optional(db.pool())
                .await?;
        Ok(v.map(|v| v as u64))
    }

    pub async fn remove(db: &StateDb, ws: WorkspaceId, ino: u64) -> Result<()> {
        sqlx::query("DELETE FROM inodes WHERE ws_id = ? AND ino = ?")
            .bind(ws.to_string())
            .bind(ino as i64)
            .execute(db.pool())
            .await?;
        Ok(())
    }
}
