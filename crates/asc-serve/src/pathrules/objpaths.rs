//! `objpaths.v1`: which paths an object occurs at (SPEC §15.5.1).
//!
//! A restricted caller fetches a tree or file by id only when it occurs at some path the caller
//! can read. Content addressing makes that sound: an object readable at one path is the same
//! bytes everywhere, so serving it reveals nothing new. The index is derived from the history
//! and kept per repository; it is brought up to date before each restricted fetch, walking only
//! the changesets it has not seen and skipping every subtree already indexed at its path.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::Kind;
use asc_store::key::ObjKey;
use asc_store::refs::RepoKey;
use sqlx::{AnyPool, Row};
use tonic::Status;

use super::scope::Scope;
use crate::registry::RepoEntry;

const TREE: i16 = 0;
const FILE: i16 = 1;
/// Rows written per statement.
const BATCH: usize = 200;

/// The per-repository index, with one writer per repository at a time.
#[derive(Default)]
pub struct ObjPaths {
    locks: Mutex<HashMap<RepoKey, Arc<tokio::sync::Mutex<()>>>>,
}

fn sql(what: &str, e: impl std::fmt::Display) -> Status {
    tracing::error!(error = %e, what, "objpaths");
    Status::internal("internal error")
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
    p.extend_from_slice(dir);
    p.push(b'/');
    p.extend_from_slice(name);
    p
}

impl ObjPaths {
    /// `Ok` when `scope` may fetch every key: trees, files and chunk lists that occur at a
    /// readable path, and chunks (their ids are learned only from a chunk list).
    pub async fn require(
        &self,
        pool: &AnyPool,
        entry: &RepoEntry,
        scope: &Scope,
        keys: &[ObjKey],
    ) -> Result<(), Status> {
        self.catch_up(pool, entry).await?;
        for key in keys {
            let (kind, id) = match key {
                ObjKey::Tree(t) => (TREE, t.as_bytes().to_vec()),
                ObjKey::File(f) | ObjKey::ChunkList(f) => (FILE, f.as_bytes().to_vec()),
                // Chunk ids are learned only from a chunk list; changesets are served redacted;
                // a tag or change meta names no path.
                ObjKey::Chunk(_)
                | ObjKey::Changeset(_)
                | ObjKey::ChangeMeta(_)
                | ObjKey::Tag(_) => {
                    continue;
                }
                _ => {
                    return Err(Status::permission_denied(
                        "PATH_POLICY: this object is not served to a caller with restricted paths",
                    ));
                }
            };
            if !self.readable(pool, entry, scope, kind, &id).await? {
                return Err(Status::permission_denied(
                    "PATH_POLICY: the object occurs only under paths you cannot read",
                ));
            }
        }
        Ok(())
    }

    async fn readable(
        &self,
        pool: &AnyPool,
        entry: &RepoEntry,
        scope: &Scope,
        kind: i16,
        id: &[u8],
    ) -> Result<bool, Status> {
        let rows =
            sqlx::query("SELECT path FROM obj_paths WHERE ns = $1 AND kind = $2 AND obj = $3")
                .bind(entry.ns.as_bytes().to_vec())
                .bind(kind)
                .bind(id.to_vec())
                .fetch_all(pool)
                .await
                .map_err(|e| sql("obj_paths", e))?;
        for row in rows {
            let path: Vec<u8> = row.try_get("path").map_err(|e| sql("obj_paths", e))?;
            if scope.can_read(&path) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Indexes every changeset of the repository's graph not yet indexed, oldest first.
    pub async fn catch_up(&self, pool: &AnyPool, entry: &RepoEntry) -> Result<(), Status> {
        let lock = self
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(entry.key)
            .or_default()
            .clone();
        let _held = lock.lock().await;
        let ns = entry.ns.as_bytes().to_vec();
        let todo = sqlx::query(
            "SELECT cs FROM cs_graph g WHERE g.ns = $1 AND NOT EXISTS \
             (SELECT 1 FROM obj_paths_done d WHERE d.ns = g.ns AND d.cs = g.cs) ORDER BY g.gen",
        )
        .bind(ns.clone())
        .fetch_all(pool)
        .await
        .map_err(|e| sql("cs_graph", e))?;
        let mut seen = HashSet::new();
        for row in todo {
            let raw: Vec<u8> = row.try_get("cs").map_err(|e| sql("cs_graph", e))?;
            let cs = <[u8; 32]>::try_from(raw.as_slice())
                .map(ChangesetId::from_bytes)
                .map_err(|_| sql("cs_graph", "changeset id is not 32 bytes"))?;
            self.index(pool, entry, &ns, cs, &mut seen).await?;
        }
        Ok(())
    }

    async fn index(
        &self,
        pool: &AnyPool,
        entry: &RepoEntry,
        ns: &[u8],
        cs: ChangesetId,
        seen: &mut HashSet<(TreeId, Vec<u8>)>,
    ) -> Result<(), Status> {
        let engine = entry.backend.engine();
        let root = engine
            .get_changeset(cs)
            .await
            .map_err(|e| sql("changeset", e))?
            .root;
        // Post-order: (tree, path, whether its children are done).
        let mut stack = vec![(root, Vec::new(), false)];
        let mut rows: Vec<(i16, Vec<u8>, Vec<u8>)> = Vec::new();
        while let Some((tree, path, expanded)) = stack.pop() {
            if expanded {
                rows.push((TREE, tree.as_bytes().to_vec(), path.clone()));
                seen.insert((tree, path));
                if rows.len() >= BATCH {
                    flush(pool, ns, &mut rows).await?;
                }
                continue;
            }
            if seen.contains(&(tree, path.clone())) || indexed(pool, ns, tree, &path).await? {
                seen.insert((tree, path));
                continue;
            }
            let t = engine.get_tree(tree).await.map_err(|e| sql("tree", e))?;
            stack.push((tree, path.clone(), true));
            for en in t.entries() {
                let p = join(&path, en.name.as_bytes());
                match en.kind {
                    Kind::Dir => stack.push((TreeId::from_bytes(en.id), p, false)),
                    Kind::File | Kind::Exec | Kind::Symlink => rows.push((FILE, en.id.to_vec(), p)),
                    Kind::Gitlink => {}
                }
            }
            // Files go in before the tree row that vouches for them.
            if rows.len() >= BATCH {
                flush(pool, ns, &mut rows).await?;
            }
        }
        flush(pool, ns, &mut rows).await?;
        sqlx::query("INSERT INTO obj_paths_done (ns, cs) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(ns.to_vec())
            .bind(cs.as_bytes().to_vec())
            .execute(pool)
            .await
            .map_err(|e| sql("obj_paths_done", e))?;
        Ok(())
    }
}

async fn indexed(pool: &AnyPool, ns: &[u8], tree: TreeId, path: &[u8]) -> Result<bool, Status> {
    let row = sqlx::query(
        "SELECT 1 AS one FROM obj_paths WHERE ns = $1 AND kind = $2 AND obj = $3 AND path = $4",
    )
    .bind(ns.to_vec())
    .bind(TREE)
    .bind(tree.as_bytes().to_vec())
    .bind(path.to_vec())
    .fetch_optional(pool)
    .await
    .map_err(|e| sql("obj_paths", e))?;
    Ok(row.is_some())
}

/// Writes `rows` in order (a tree's row after its files') and empties it.
async fn flush(
    pool: &AnyPool,
    ns: &[u8],
    rows: &mut Vec<(i16, Vec<u8>, Vec<u8>)>,
) -> Result<(), Status> {
    for chunk in rows.chunks(BATCH) {
        let values: Vec<String> = (0..chunk.len())
            .map(|i| format!("($1, ${}, ${}, ${})", 2 + i * 3, 3 + i * 3, 4 + i * 3))
            .collect();
        let q = format!(
            "INSERT INTO obj_paths (ns, kind, obj, path) VALUES {} ON CONFLICT DO NOTHING",
            values.join(", ")
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.to_vec());
        for (kind, obj, path) in chunk {
            query = query.bind(*kind).bind(obj.clone()).bind(path.clone());
        }
        query.execute(pool).await.map_err(|e| sql("obj_paths", e))?;
    }
    rows.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;
    use asc_core::path::RepoPath;
    use asc_core::pathacl::{Level, PathAcl, Rule, Subject, Viewer};
    use asc_core::policy::{Kind as WhoKind, Who};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    fn scope() -> Scope {
        let rule = |dir: &str, level| Rule {
            dir: dir.into(),
            subject: Subject::parse("*").unwrap(),
            level,
        };
        let acl = PathAcl::new(&[
            rule("secret", Level::None),
            rule("secret/docs", Level::Read),
        ])
        .unwrap();
        Scope::new(
            Arc::new(acl),
            Viewer {
                who: Who::new(WhoKind::User, "bob"),
                teams: Default::default(),
                repo_level: Level::Write,
            },
        )
    }

    async fn graph(pool: &AnyPool, e: &RepoEntry, cs: ChangesetId, generation: i64) {
        sqlx::query("INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, $3, $4)")
            .bind(e.ns.as_bytes().to_vec())
            .bind(cs.as_bytes().to_vec())
            .bind(generation)
            .bind(Vec::<u8>::new())
            .execute(pool)
            .await
            .unwrap();
    }

    async fn id_at(e: &RepoEntry, cs: ChangesetId, path: &str) -> [u8; 32] {
        let engine = e.backend.engine();
        let root = engine.get_changeset(cs).await.unwrap().root;
        if path.is_empty() {
            return *root.as_bytes();
        }
        engine
            .lookup(root, &RepoPath::new(path).unwrap())
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn count(pool: &AnyPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn objects_are_served_only_where_they_occur_at_a_readable_path() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let pool = reg.pool();
        let c1 = commit(
            &e,
            &[],
            &[
                ("a", b"same\n"),
                ("secret/k", b"key\n"),
                ("secret/dup", b"same\n"),
                ("secret/docs/d", b"doc\n"),
            ],
        )
        .await;
        let c2 = commit(&e, &[c1], &[("secret/k", b"key2\n")]).await;
        graph(pool, &e, c1, 1).await;
        graph(pool, &e, c2, 2).await;
        let idx = ObjPaths::default();
        let s = scope();
        let tree = |id: [u8; 32]| ObjKey::Tree(TreeId::from_bytes(id));
        let file = |id: [u8; 32]| ObjKey::File(FileId::from_bytes(id));
        let ok = [
            tree(id_at(&e, c2, "").await),
            tree(id_at(&e, c2, "secret/docs").await),
            file(id_at(&e, c2, "a").await),
            // The same bytes sit under the restricted directory too.
            file(id_at(&e, c2, "secret/dup").await),
            file(id_at(&e, c1, "secret/docs/d").await),
            // Served redacted by the caller.
            ObjKey::Changeset(c2),
        ];
        for k in &ok {
            idx.require(pool, &e, &s, std::slice::from_ref(k))
                .await
                .unwrap();
        }
        let refused = [
            tree(id_at(&e, c1, "secret").await),
            tree(id_at(&e, c2, "secret").await),
            file(id_at(&e, c1, "secret/k").await),
            file(id_at(&e, c2, "secret/k").await),
        ];
        for k in &refused {
            let err = idx
                .require(pool, &e, &s, std::slice::from_ref(k))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{k:?}");
            assert!(err.message().starts_with("PATH_POLICY:"));
        }
        // One refused key refuses the whole request.
        assert!(
            idx.require(pool, &e, &s, &[ok[0], refused[0]])
                .await
                .is_err()
        );

        // Catching up again walks nothing new; a later changeset is picked up.
        let rows = count(pool, "obj_paths").await;
        idx.catch_up(pool, &e).await.unwrap();
        assert_eq!(count(pool, "obj_paths").await, rows);
        assert_eq!(count(pool, "obj_paths_done").await, 2);
        let c3 = commit(&e, &[c2], &[("b", b"new\n")]).await;
        graph(pool, &e, c3, 3).await;
        let b = file(id_at(&e, c3, "b").await);
        idx.require(pool, &e, &s, &[b]).await.unwrap();
        assert_eq!(count(pool, "obj_paths_done").await, 3);
    }
}
