//! The git map: `(git oid, kind) -> asc id` rows (SPEC §5).

use async_trait::async_trait;
use asc_core::git::{ObjectKind, tree_oid};
use asc_core::id::{GitOid, TreeId};
use asc_core::object::Kind;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::Result;
use crate::Engine;

/// One git-map row. `asc` is the 32-byte id of the file, tree or changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitMapRow {
    pub git: GitOid,
    pub kind: ObjectKind,
    pub asc: [u8; 32],
}

/// Receives git-map rows. Implemented by `state.db` and the server.
#[async_trait]
pub trait GitMapSink: Send + Sync {
    /// Records rows; recording a row twice is harmless.
    async fn put(&self, rows: &[GitMapRow]) -> Result<()>;
}

/// Emits rows for one tree: the tree itself and each file and subtree entry.
///
/// Costs one tree read. It does not descend, so an ingest calls it for each new tree along
/// the changed spines only. Gitlinks have no asc object and produce no row.
pub async fn index_tree_git<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    id: TreeId,
    sink: &dyn GitMapSink,
) -> Result<()> {
    let tree = e.get_tree(id).await?;
    let mut rows = vec![GitMapRow {
        git: tree_oid(&tree),
        kind: ObjectKind::Tree,
        asc: *id.as_bytes(),
    }];
    for entry in tree.entries() {
        let kind = match entry.kind {
            Kind::Dir => ObjectKind::Tree,
            Kind::Gitlink => continue,
            Kind::File | Kind::Exec | Kind::Symlink => ObjectKind::Blob,
        };
        rows.push(GitMapRow {
            git: entry.git,
            kind,
            asc: entry.id,
        });
    }
    sink.put(&rows).await
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::derive::testutil::*;

    /// Collects rows in memory.
    #[derive(Default)]
    pub struct VecSink(pub Mutex<Vec<GitMapRow>>);

    #[async_trait]
    impl GitMapSink for VecSink {
        async fn put(&self, rows: &[GitMapRow]) -> Result<()> {
            self.0.lock().unwrap().extend_from_slice(rows);
            Ok(())
        }
    }

    #[tokio::test]
    async fn tree_rows_cover_the_tree_and_its_entries_only() {
        let e = engine();
        let cs = commit_on(&e, &[], b"hello").await;
        let root = e.get_changeset(cs).await.unwrap().root;
        let sink = VecSink::default();
        index_tree_git(&e, root, &sink).await.unwrap();
        let rows = sink.0.lock().unwrap().clone();
        let tree = e.get_tree(root).await.unwrap();
        assert_eq!(rows.len(), 1 + tree.entries().len());
        assert_eq!(rows[0].kind, ObjectKind::Tree);
        assert_eq!(rows[0].git, tree_oid(&tree));
        let f = tree
            .entries()
            .iter()
            .find(|x| x.kind == Kind::File)
            .unwrap();
        let d = tree.entries().iter().find(|x| x.kind == Kind::Dir).unwrap();
        assert!(rows.contains(&GitMapRow {
            git: d.git,
            kind: ObjectKind::Tree,
            asc: d.id
        }));
        assert!(rows.contains(&GitMapRow {
            git: f.git,
            kind: ObjectKind::Blob,
            asc: f.id
        }));
    }
}
