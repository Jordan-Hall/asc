//! The server's changeset graph over the `cs_graph` table (SPEC §8.2).
//!
//! One row per ingested changeset: its generation and its parents as concatenated 32-byte ids.
//! [`SqlGraph`] answers the engine's `GraphSource` from those rows, so ancestry checks never read
//! changeset objects; [`known`] and [`insert_nodes`] are the ingest side.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asc_core::id::ChangesetId;
use asc_core::object::Changeset;
use asc_engine::Engine;
use asc_engine::graph::{Error, GraphSource, Node, Result};
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};

use crate::registry::Ns;

/// Ids per `IN (...)` lookup; keeps statements under every driver's parameter limit.
const LOOKUP_BATCH: usize = 500;

fn sql_err(e: sqlx::Error) -> Error {
    Error::Remote(format!("cs_graph: {e}"))
}

/// Splits a `parents` column into ids; a length that is not a multiple of 32 is corrupt.
fn decode_parents(blob: &[u8]) -> Result<Vec<ChangesetId>> {
    if !blob.len().is_multiple_of(32) {
        return Err(Error::Remote(
            "cs_graph row has a damaged parents column".into(),
        ));
    }
    Ok(blob
        .chunks_exact(32)
        .map(|c| ChangesetId::from_bytes(c.try_into().expect("chunk is 32 bytes")))
        .collect())
}

fn placeholders(n: usize, from: usize) -> String {
    (0..n)
        .map(|i| format!("${}", from + i))
        .collect::<Vec<_>>()
        .join(",")
}

fn generation(row: &sqlx::any::AnyRow) -> Result<u32> {
    let g: i64 = row.try_get("gen").map_err(sql_err)?;
    u32::try_from(g).map_err(|_| Error::Remote("cs_graph generation out of range".into()))
}

/// The nodes of `ids` that are already in `cs_graph`, with their generations.
pub async fn known(
    pool: &AnyPool,
    ns: Ns,
    ids: &[ChangesetId],
) -> Result<HashMap<ChangesetId, u32>> {
    let mut out = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(LOOKUP_BATCH) {
        let sql = format!(
            "SELECT cs, gen FROM cs_graph WHERE ns = $1 AND cs IN ({})",
            placeholders(chunk.len(), 2)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(ns.as_bytes().to_vec());
        for id in chunk {
            q = q.bind(id.as_bytes().to_vec());
        }
        for row in q.fetch_all(pool).await.map_err(sql_err)? {
            let cs: Vec<u8> = row.try_get("cs").map_err(sql_err)?;
            let id = <[u8; 32]>::try_from(cs.as_slice())
                .map(ChangesetId::from_bytes)
                .map_err(|_| Error::Remote("cs_graph id is not 32 bytes".into()))?;
            out.insert(id, generation(&row)?);
        }
    }
    Ok(out)
}

/// One new graph row.
pub struct NewNode<'a> {
    pub id: ChangesetId,
    pub generation: u32,
    pub parents: &'a [ChangesetId],
}

/// Inserts `nodes` inside `tx`; rows that exist already are left alone.
pub async fn insert_nodes(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    nodes: &[NewNode<'_>],
) -> std::result::Result<(), sqlx::Error> {
    for n in nodes {
        let parents: Vec<u8> = n.parents.iter().flat_map(|p| *p.as_bytes()).collect();
        sqlx::query(
            "INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (ns, cs) DO NOTHING",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(n.id.as_bytes().to_vec())
        .bind(i64::from(n.generation))
        .bind(parents)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// A [`GraphSource`] over `cs_graph`; changeset bodies still come from the engine.
pub struct SqlGraph<O: Objects, R: Refs> {
    pool: AnyPool,
    ns: Ns,
    engine: Arc<Engine<O, R>>,
    reads: AtomicU64,
}

impl<O: Objects, R: Refs> SqlGraph<O, R> {
    pub fn new(pool: AnyPool, ns: Ns, engine: Arc<Engine<O, R>>) -> Self {
        Self {
            pool,
            ns,
            engine,
            reads: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl<O: Objects, R: Refs> GraphSource for SqlGraph<O, R> {
    async fn get(&self, id: ChangesetId) -> Result<Node> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let row = sqlx::query("SELECT gen, parents FROM cs_graph WHERE ns = $1 AND cs = $2")
            .bind(self.ns.as_bytes().to_vec())
            .bind(id.as_bytes().to_vec())
            .fetch_optional(&self.pool)
            .await
            .map_err(sql_err)?
            .ok_or(Error::Missing(id))?;
        let parents: Vec<u8> = row.try_get("parents").map_err(sql_err)?;
        Ok(Node {
            generation: generation(&row)?,
            parents: decode_parents(&parents)?,
        })
    }

    async fn get_many(&self, ids: &[ChangesetId]) -> Result<Vec<Node>> {
        let mut found: HashMap<ChangesetId, Node> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(LOOKUP_BATCH) {
            self.reads.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            let sql = format!(
                "SELECT cs, gen, parents FROM cs_graph WHERE ns = $1 AND cs IN ({})",
                placeholders(chunk.len(), 2)
            );
            let mut q =
                sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(self.ns.as_bytes().to_vec());
            for id in chunk {
                q = q.bind(id.as_bytes().to_vec());
            }
            for row in q.fetch_all(&self.pool).await.map_err(sql_err)? {
                let cs: Vec<u8> = row.try_get("cs").map_err(sql_err)?;
                let id = <[u8; 32]>::try_from(cs.as_slice())
                    .map(ChangesetId::from_bytes)
                    .map_err(|_| Error::Remote("cs_graph id is not 32 bytes".into()))?;
                let parents: Vec<u8> = row.try_get("parents").map_err(sql_err)?;
                found.insert(
                    id,
                    Node {
                        generation: generation(&row)?,
                        parents: decode_parents(&parents)?,
                    },
                );
            }
        }
        ids.iter()
            .map(|id| found.get(id).cloned().ok_or(Error::Missing(*id)))
            .collect()
    }

    async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.engine.get_changeset(id).await?)
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use asc_engine::graph::Graph;

    use super::*;
    use crate::migrate;
    use crate::registry::Registry;
    use asc_rpc::server::RepoName;

    fn id(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    #[tokio::test]
    async fn rows_round_trip_and_answer_ancestry() {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Registry::new(pool.clone(), "memory:///").unwrap();
        let entry = registry
            .create(&RepoName::parse("a/b").unwrap(), &Default::default())
            .await
            .unwrap();
        let (a, b, c) = (id(1), id(2), id(3));
        let mut tx = pool.begin().await.unwrap();
        insert_nodes(
            &mut tx,
            entry.ns,
            &[
                NewNode {
                    id: a,
                    generation: 1,
                    parents: &[],
                },
                NewNode {
                    id: b,
                    generation: 2,
                    parents: &[a],
                },
                NewNode {
                    id: c,
                    generation: 3,
                    parents: &[b, a],
                },
            ],
        )
        .await
        .unwrap();
        // A second insert of the same row is harmless.
        insert_nodes(
            &mut tx,
            entry.ns,
            &[NewNode {
                id: a,
                generation: 9,
                parents: &[],
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let got = known(&pool, entry.ns, &[a, c, id(9)]).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[&a], 1);

        let src = SqlGraph::new(pool, entry.ns, entry.backend.engine().clone());
        assert_eq!(src.get(c).await.unwrap().parents, vec![b, a]);
        assert!(matches!(src.get(id(9)).await, Err(Error::Missing(_))));
        let graph = Graph::new(src);
        assert!(graph.is_ancestor(a, c).await.unwrap());
        assert!(!graph.is_ancestor(c, a).await.unwrap());
    }
}
