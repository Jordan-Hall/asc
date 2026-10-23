//! The `changes` table: which change a changeset belongs to and which `ChangeMeta` says so.
//!
//! [`ChangeIndex`] is the server's [`IngestObserver`]. `on_ingested` indexes every `ChangeMeta`
//! (`ObjKey::ChangeMeta`) the ingest closure carries, after checking that the meta decodes, that
//! its `cs` is a changeset of this push (a meta for an already stored changeset is accepted only
//! as an identical retry of its row), and that its fields are within bounds. The first meta
//! indexed for a changeset wins (SPEC §20.2). `on_rewritten` writes a
//! `ChangeMeta` and a row for each changeset a land or merge created from an indexed one, in the
//! transaction of `land_map`. [`ChangeIndex::write_meta`] is the same write for the changesets
//! the server creates itself (`Commit`, M18).
//!
//! All stored bytes and all pushed metas are untrusted: objects are read through
//! `Objects::get` (BLAKE3-verified, size-capped), counts are bounded, and no query scans.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::change::{ChangeId, ChangeMeta, MetaId, RewriteOp};
use asc_core::codec::{Enveloped, decode_envelope};
use asc_core::id::ChangesetId;
use asc_engine::sync::changes::LogMeta;
use asc_rpc::log_meta::set_change_meta;
use asc_rpc::server::wire;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::RepoKey;
use sqlx::any::{Any, AnyRow};
use sqlx::{Executor, Row, Transaction};
use tonic::Status;

use super::{fixed, placeholders};
use crate::ingest::{IngestObserver, ObserverError};
use crate::read::LogEntryEnricher;
use crate::registry::{Ns, Registry, RepoEntry};

/// Most `ChangeMeta` objects one ingest may carry.
pub const MAX_METAS: usize = 100_000;
/// Most `preds` one meta may name (a fold of this many changes is already absurd).
pub const MAX_PREDS: usize = 1_024;
/// Ids per `IN (...)` lookup.
const LOOKUP_BATCH: usize = 500;
/// Metas fetched from the object store at once.
const FETCH_PARALLEL: usize = 16;

/// Why an index read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("changes: database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("changes: invalid object {key}: {why}")]
    Invalid { key: String, why: String },
    #[error("changes: object store: {0}")]
    Store(String),
    #[error("changes: damaged row: {0}")]
    Corrupt(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for ObserverError {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid { key, why } => ObserverError::Invalid { key, why },
            Error::Sql(e) => ObserverError::Sql(e),
            e => ObserverError::Failed(e.to_string()),
        }
    }
}

fn invalid(id: MetaId, why: impl Into<String>) -> Error {
    Error::Invalid {
        key: ObjKey::ChangeMeta(id).path(),
        why: why.into(),
    }
}

/// One `changes` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRow {
    pub cs: ChangesetId,
    pub change: ChangeId,
    pub meta: MetaId,
    pub op: i64,
    pub preds: Vec<ChangesetId>,
}

fn pack(preds: &[ChangesetId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(preds.len() * 32);
    for p in preds {
        out.extend_from_slice(p.as_bytes());
    }
    out
}

fn unpack(b: &[u8]) -> Result<Vec<ChangesetId>> {
    if !b.len().is_multiple_of(32) || b.len() / 32 > MAX_PREDS {
        return Err(Error::Corrupt("preds column"));
    }
    Ok(b.chunks_exact(32)
        .map(|c| ChangesetId::from_bytes(c.try_into().expect("32 bytes")))
        .collect())
}

fn row_of(r: &AnyRow) -> Result<ChangeRow> {
    let preds: Vec<u8> = r.try_get("preds")?;
    Ok(ChangeRow {
        cs: ChangesetId::from_bytes(fixed(r, "cs").ok_or(Error::Corrupt("cs"))?),
        change: ChangeId::from_bytes(fixed(r, "change").ok_or(Error::Corrupt("change"))?),
        meta: MetaId::from_bytes(fixed(r, "meta").ok_or(Error::Corrupt("meta"))?),
        op: r.try_get("op")?,
        preds: unpack(&preds)?,
    })
}

const ROW_COLUMNS: &str = "cs, change, meta, CAST(op AS BIGINT) AS op, preds";

/// The rows of `cs` (those that have one), at most `LOOKUP_BATCH` ids per statement.
pub async fn rows_of(
    db: &mut sqlx::AnyConnection,
    ns: Ns,
    cs: &[ChangesetId],
) -> Result<Vec<ChangeRow>> {
    let mut out = Vec::new();
    for chunk in cs.chunks(LOOKUP_BATCH) {
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM changes WHERE ns = $1 AND cs IN ({})",
            placeholders(2, chunk.len())
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.as_bytes().to_vec());
        for c in chunk {
            q = q.bind(c.as_bytes().to_vec());
        }
        for r in q.fetch_all(&mut *db).await? {
            out.push(row_of(&r)?);
        }
    }
    Ok(out)
}

/// The changesets of `change`, at most `limit`, through `changes_by_change`.
pub async fn changesets_of<'e, E>(
    db: E,
    ns: Ns,
    change: ChangeId,
    limit: usize,
) -> Result<Vec<ChangesetId>>
where
    E: Executor<'e, Database = Any>,
{
    let limit = i64::try_from(limit.clamp(1, MAX_PREDS)).unwrap_or(1);
    let rows = sqlx::query("SELECT cs FROM changes WHERE ns = $1 AND change = $2 LIMIT $3")
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .bind(limit)
        .fetch_all(db)
        .await?;
    rows.iter()
        .map(|r| {
            fixed(r, "cs")
                .map(ChangesetId::from_bytes)
                .ok_or(Error::Corrupt("cs"))
        })
        .collect()
}

/// The changeset of `change` that sits on the mainline of `land_ref`: one indexed join
/// (`changes_by_change` into `mainline_by_cs`), never a scan of messages (Scion lesson 23).
pub async fn landed_as<'e, E>(
    db: E,
    ns: Ns,
    change: ChangeId,
    land_ref: &str,
) -> Result<Option<ChangesetId>>
where
    E: Executor<'e, Database = Any>,
{
    let row = sqlx::query(LANDED_AS)
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .bind(land_ref.to_owned())
        .fetch_optional(db)
        .await?;
    row.map(|r| {
        fixed(&r, "cs")
            .map(ChangesetId::from_bytes)
            .ok_or(Error::Corrupt("cs"))
    })
    .transpose()
}

pub(crate) const LANDED_AS: &str = "SELECT c.cs AS cs FROM changes c \
     JOIN mainline m ON m.ns = c.ns AND m.ref = $3 AND m.cs = c.cs \
     WHERE c.ns = $1 AND c.change = $2 LIMIT 1";

/// Inserts a row unless the changeset already has one (the first meta wins). True if written.
async fn insert_row(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    id: MetaId,
    m: &ChangeMeta,
) -> Result<bool> {
    let done = sqlx::query(
        "INSERT INTO changes (ns, cs, change, meta, op, preds) VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (ns, cs) DO NOTHING",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(m.cs.as_bytes().to_vec())
    .bind(m.change.as_bytes().to_vec())
    .bind(id.as_bytes().to_vec())
    .bind(m.op as i16)
    .bind(pack(&m.preds))
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// The changesets of `ids` that are in `cs_graph`.
async fn stored(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    ids: &[ChangesetId],
) -> Result<HashSet<ChangesetId>> {
    let mut out = HashSet::with_capacity(ids.len());
    for chunk in ids.chunks(LOOKUP_BATCH) {
        let sql = format!(
            "SELECT cs FROM cs_graph WHERE ns = $1 AND cs IN ({})",
            placeholders(2, chunk.len())
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.as_bytes().to_vec());
        for c in chunk {
            q = q.bind(c.as_bytes().to_vec());
        }
        for r in q.fetch_all(&mut **tx).await? {
            out.insert(ChangesetId::from_bytes(
                fixed(&r, "cs").ok_or(Error::Corrupt("cs_graph.cs"))?,
            ));
        }
    }
    Ok(out)
}

/// Checks the fields of a decoded meta that the codec does not.
fn check_meta(id: MetaId, m: &ChangeMeta) -> Result<()> {
    if m.preds.len() > MAX_PREDS {
        return Err(invalid(id, format!("more than {MAX_PREDS} preds")));
    }
    if m.preds.contains(&m.cs) {
        return Err(invalid(id, "a changeset cannot be its own predecessor"));
    }
    if m.op == RewriteOp::New && !m.preds.is_empty() {
        return Err(invalid(id, "a new change has no predecessors"));
    }
    Ok(())
}

/// The change index: the server's `IngestObserver` and `LogEntryEnricher` for stack metadata.
pub struct ChangeIndex {
    registry: Arc<Registry>,
}

impl ChangeIndex {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    fn objects(&self, ns: Ns) -> BucketObjects {
        let (store, root) = self.registry.namespace(ns);
        BucketObjects::new(store, root)
    }

    /// The namespace of `repo`, read inside `tx`.
    async fn ns_of(tx: &mut Transaction<'_, Any>, repo: RepoKey) -> Result<Ns> {
        let row = sqlx::query("SELECT ns FROM repos WHERE id = $1")
            .bind(repo.0)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Corrupt("repository row is gone"))?;
        let b: Vec<u8> = row.try_get("ns")?;
        Ns::from_bytes(&b).ok_or(Error::Corrupt("repos.ns"))
    }

    /// Validates and indexes the metas named by `ids`. `new` are the changesets of this push.
    pub async fn index_metas(
        &self,
        tx: &mut Transaction<'_, Any>,
        ns: Ns,
        ids: &[MetaId],
        new: &[ChangesetId],
    ) -> Result<usize> {
        use futures::stream::{StreamExt, TryStreamExt};
        let mut seen = HashSet::new();
        let ids: Vec<MetaId> = ids.iter().copied().filter(|i| seen.insert(*i)).collect();
        if ids.len() > MAX_METAS {
            return Err(Error::Invalid {
                key: "m/".into(),
                why: format!("more than {MAX_METAS} change metas in one push"),
            });
        }
        let objects = self.objects(ns);
        let metas: Vec<(MetaId, ChangeMeta)> = futures::stream::iter(ids)
            .map(|id| {
                let objects = &objects;
                async move {
                    let data = objects
                        .get(&ObjKey::ChangeMeta(id))
                        .await
                        .map_err(|e| match e {
                            asc_store::objects::Error::Corrupt(_) => {
                                invalid(id, "the object does not match its key")
                            }
                            e => Error::Store(e.to_string()),
                        })?
                        .ok_or_else(|| invalid(id, "the object is not stored"))?;
                    let m: ChangeMeta = decode_envelope(&data)
                        .map_err(|e| invalid(id, format!("undecodable change meta: {e}")))?;
                    check_meta(id, &m)?;
                    Ok::<_, Error>((id, m))
                }
            })
            .buffered(FETCH_PARALLEL)
            .try_collect()
            .await?;
        // A meta is indexed only for a changeset of THIS push (SPEC §20.2): pushed changesets
        // are new to the store, so no one else has a row for them. For an already stored
        // changeset a meta is accepted only as an idempotent retry, identical to the stored row;
        // anything else would claim a change id, versions or preds for someone else's commit,
        // and the first writer would win forever.
        let in_push: HashSet<ChangesetId> = new.iter().copied().collect();
        let unknown: Vec<ChangesetId> = metas
            .iter()
            .map(|(_, m)| m.cs)
            .filter(|c| !in_push.contains(c))
            .collect();
        let known = stored(tx, ns, &unknown).await?;
        let existing: HashMap<ChangesetId, ChangeRow> = rows_of(tx, ns, &unknown)
            .await?
            .into_iter()
            .map(|r| (r.cs, r))
            .collect();
        let mut written = 0;
        for (id, m) in &metas {
            if in_push.contains(&m.cs) {
                // The first meta indexed for a changeset wins; a different one is not indexed.
                written += usize::from(insert_row(tx, ns, *id, m).await?);
                continue;
            }
            if !known.contains(&m.cs) {
                return Err(invalid(
                    *id,
                    format!("changeset {} is not in the push closure", m.cs),
                ));
            }
            let same = existing.get(&m.cs).is_some_and(|r| {
                r.meta == *id && r.change == m.change && r.op == m.op as i64 && r.preds == m.preds
            });
            if !same {
                return Err(invalid(
                    *id,
                    format!(
                        "changeset {} is already stored and not part of this push; this meta cannot claim it",
                        m.cs
                    ),
                ));
            }
        }
        Ok(written)
    }

    /// Stores `meta` and indexes it, for a changeset the server created itself. The object is
    /// written first, so a row never names a missing object. Returns the meta id and whether a
    /// row was written (false: the changeset already had a meta).
    pub async fn write_meta(
        &self,
        tx: &mut Transaction<'_, Any>,
        repo: RepoKey,
        meta: &ChangeMeta,
    ) -> Result<(MetaId, bool)> {
        let ns = Self::ns_of(tx, repo).await?;
        self.write_meta_in(tx, ns, meta).await
    }

    async fn write_meta_in(
        &self,
        tx: &mut Transaction<'_, Any>,
        ns: Ns,
        meta: &ChangeMeta,
    ) -> Result<(MetaId, bool)> {
        let id = meta.id();
        check_meta(id, meta)?;
        self.objects(ns)
            .put(&ObjKey::ChangeMeta(id), encode_stored(&meta.to_envelope()))
            .await
            .map_err(|e| Error::Store(e.to_string()))?;
        let wrote = insert_row(tx, ns, id, meta).await?;
        Ok((id, wrote))
    }

    /// Writes the metas of rewritten changesets: each `new` continues the change of its `old`
    /// with `preds = [old]`. Several `old` mapped to one `new` (a squash) give one meta with all
    /// of them as preds and the change of the last `old` that has one (the PR's head). Olds
    /// without a row (plain git history) get no meta.
    pub async fn write_rewrites(
        &self,
        tx: &mut Transaction<'_, Any>,
        repo: RepoKey,
        pairs: &[(ChangesetId, ChangesetId)],
        op: RewriteOp,
    ) -> Result<usize> {
        if pairs.is_empty() {
            return Ok(0);
        }
        let ns = Self::ns_of(tx, repo).await?;
        let olds: Vec<ChangesetId> = pairs.iter().map(|p| p.0).collect();
        let rows: HashMap<ChangesetId, ChangeRow> = rows_of(tx, ns, &olds)
            .await?
            .into_iter()
            .map(|r| (r.cs, r))
            .collect();
        let mut grouped: Vec<(ChangesetId, Vec<ChangesetId>)> = Vec::new();
        let mut at: HashMap<ChangesetId, usize> = HashMap::new();
        for &(old, new) in pairs {
            let i = *at.entry(new).or_insert_with(|| {
                grouped.push((new, Vec::new()));
                grouped.len() - 1
            });
            grouped[i].1.push(old);
        }
        let mut written = 0;
        for (new, olds) in grouped {
            let Some(change) = olds
                .iter()
                .rev()
                .find_map(|o| rows.get(o))
                .map(|r| r.change)
            else {
                continue;
            };
            let preds: Vec<ChangesetId> = olds.into_iter().filter(|o| *o != new).collect();
            if preds.is_empty() {
                continue;
            }
            let meta = ChangeMeta::new(new, change, preds, op);
            written += usize::from(self.write_meta_in(tx, ns, &meta).await?.1);
        }
        Ok(written)
    }
}

#[async_trait]
impl IngestObserver for ChangeIndex {
    async fn on_ingested(
        &self,
        tx: &mut Transaction<'_, Any>,
        repo: RepoKey,
        new: &[ChangesetId],
        closure: &[ObjKey],
    ) -> std::result::Result<(), ObserverError> {
        let metas: Vec<MetaId> = closure
            .iter()
            .filter_map(|k| match k {
                ObjKey::ChangeMeta(m) => Some(*m),
                _ => None,
            })
            .collect();
        if metas.is_empty() {
            return Ok(());
        }
        let ns = Self::ns_of(tx, repo).await?;
        self.index_metas(tx, ns, &metas, new).await?;
        Ok(())
    }

    async fn on_rewritten(
        &self,
        tx: &mut Transaction<'_, Any>,
        repo: RepoKey,
        pairs: &[(ChangesetId, ChangesetId)],
        kind: RewriteOp,
    ) -> std::result::Result<(), ObserverError> {
        self.write_rewrites(tx, repo, pairs, kind).await?;
        Ok(())
    }
}

#[async_trait]
impl LogEntryEnricher for ChangeIndex {
    async fn enrich(
        &self,
        repo: &RepoEntry,
        ids: &[ChangesetId],
        entries: &mut [wire::LogEntry],
    ) -> std::result::Result<(), Status> {
        let n = entries.len().min(ids.len());
        if n == 0 {
            return Ok(());
        }
        let mut conn = self
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let rows = rows_of(&mut conn, repo.ns, &ids[..n])
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let by_cs: HashMap<ChangesetId, &ChangeRow> = rows.iter().map(|r| (r.cs, r)).collect();
        for (entry, id) in entries.iter_mut().zip(ids) {
            if let Some(r) = by_cs.get(id) {
                set_change_meta(
                    entry,
                    &LogMeta {
                        id: *id,
                        change: Some(r.change),
                        meta: Some(r.meta),
                    },
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_sql::{NewNode, insert_nodes};
    use crate::ingest::testutil::{commit, registry, repo};

    struct Fx {
        reg: Arc<Registry>,
        e: Arc<RepoEntry>,
        idx: ChangeIndex,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        let idx = ChangeIndex::new(reg.clone());
        Fx { reg, e, idx }
    }

    impl Fx {
        async fn graph(&self, tx: &mut Transaction<'_, Any>, ids: &[ChangesetId]) {
            let nodes: Vec<NewNode<'_>> = ids
                .iter()
                .map(|&id| NewNode {
                    id,
                    generation: 1,
                    parents: &[],
                })
                .collect();
            insert_nodes(tx, self.e.ns, &nodes).await.unwrap();
        }

        async fn put(&self, m: &ChangeMeta) -> MetaId {
            let id = m.id();
            self.e
                .objects
                .put(&ObjKey::ChangeMeta(id), encode_stored(&m.to_envelope()))
                .await
                .unwrap();
            id
        }

        async fn rows(&self, cs: &[ChangesetId]) -> Vec<ChangeRow> {
            let mut c = self.reg.pool().acquire().await.unwrap();
            rows_of(&mut c, self.e.ns, cs).await.unwrap()
        }
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    #[tokio::test]
    async fn a_meta_whose_cs_is_not_in_the_push_closure_is_rejected() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let stray = ChangesetId::from_bytes([9; 32]);
        let m = f
            .put(&ChangeMeta::new(stray, cid(1), vec![], RewriteOp::New))
            .await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a]).await;
        let err = f
            .idx
            .on_ingested(
                &mut tx,
                f.e.key,
                &[a],
                &[ObjKey::Changeset(a), ObjKey::ChangeMeta(m)],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ObserverError::Invalid { .. }), "{err}");
        drop(tx);
        assert!(f.rows(&[stray]).await.is_empty());
    }

    #[tokio::test]
    async fn a_valid_meta_is_indexed_and_the_first_one_wins() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let first = f
            .put(&ChangeMeta::new(a, cid(1), vec![], RewriteOp::New))
            .await;
        let second = f
            .put(&ChangeMeta::new(a, cid(2), vec![], RewriteOp::New))
            .await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a]).await;
        f.idx
            .on_ingested(&mut tx, f.e.key, &[a], &[ObjKey::ChangeMeta(first)])
            .await
            .unwrap();
        // A second, different meta for the same changeset of the push is stored but not indexed.
        f.idx
            .on_ingested(&mut tx, f.e.key, &[a], &[ObjKey::ChangeMeta(second)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let rows = f.rows(&[a]).await;
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].change, rows[0].meta), (cid(1), first));
        let mut c = f.reg.pool().acquire().await.unwrap();
        assert_eq!(
            changesets_of(&mut *c, f.e.ns, cid(1), 10).await.unwrap(),
            vec![a]
        );
    }

    #[tokio::test]
    async fn a_meta_cannot_claim_a_changeset_stored_by_another_push() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let b = commit(&f.e, &[], &[("b", b"b")]).await;
        let c = commit(&f.e, &[], &[("c", b"c")]).await;
        let mine = f
            .put(&ChangeMeta::new(a, cid(1), vec![], RewriteOp::New))
            .await;
        // User A's push indexes `a`; `c` was stored without a meta.
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a, c]).await;
        f.idx
            .on_ingested(&mut tx, f.e.key, &[a], &[ObjKey::ChangeMeta(mine)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let before = f.rows(&[a]).await;
        // User B's push (new = [b]) carries a different meta for A's `a`, and one for `c`.
        let squat_a = f
            .put(&ChangeMeta::new(a, cid(2), vec![b], RewriteOp::Amend))
            .await;
        let squat_c = f
            .put(&ChangeMeta::new(c, cid(3), vec![], RewriteOp::New))
            .await;
        for squat in [squat_a, squat_c] {
            let mut tx = f.reg.refs().begin().await.unwrap();
            f.graph(&mut tx, &[b]).await;
            let err = f
                .idx
                .on_ingested(&mut tx, f.e.key, &[b], &[ObjKey::ChangeMeta(squat)])
                .await
                .unwrap_err();
            assert!(matches!(err, ObserverError::Invalid { .. }), "{err}");
            drop(tx);
        }
        assert_eq!(f.rows(&[a]).await, before, "A's row is unchanged");
        assert!(f.rows(&[c]).await.is_empty(), "no row was created for c");
    }

    #[tokio::test]
    async fn an_identical_meta_for_a_stored_changeset_is_an_idempotent_retry() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let m = f
            .put(&ChangeMeta::new(a, cid(1), vec![], RewriteOp::New))
            .await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a]).await;
        f.idx
            .on_ingested(&mut tx, f.e.key, &[a], &[ObjKey::ChangeMeta(m)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let before = f.rows(&[a]).await;
        // The retry: `a` is stored now, so it is not new to the push.
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.idx
            .on_ingested(&mut tx, f.e.key, &[], &[ObjKey::ChangeMeta(m)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(f.rows(&[a]).await, before);
    }

    #[tokio::test]
    async fn missing_and_malformed_metas_are_rejected() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let absent = ChangeMeta::new(a, cid(1), vec![], RewriteOp::New).id();
        let bad = ChangeMeta::new(a, cid(3), vec![a], RewriteOp::Amend);
        let bad = f.put(&bad).await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a]).await;
        for id in [absent, bad] {
            let err = f
                .idx
                .on_ingested(&mut tx, f.e.key, &[a], &[ObjKey::ChangeMeta(id)])
                .await
                .unwrap_err();
            assert!(matches!(err, ObserverError::Invalid { .. }), "{err}");
        }
    }

    #[tokio::test]
    async fn rewrites_continue_the_change_and_squash_uses_the_head() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let b = commit(&f.e, &[a], &[("b", b"b")]).await;
        let (a2, b2, s) = (
            ChangesetId::from_bytes([0xa2; 32]),
            ChangesetId::from_bytes([0xb2; 32]),
            ChangesetId::from_bytes([0x55; 32]),
        );
        let ma = f
            .put(&ChangeMeta::new(a, cid(1), vec![], RewriteOp::New))
            .await;
        let mb = f
            .put(&ChangeMeta::new(b, cid(2), vec![], RewriteOp::New))
            .await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a, b]).await;
        f.idx
            .on_ingested(
                &mut tx,
                f.e.key,
                &[a, b],
                &[ObjKey::ChangeMeta(ma), ObjKey::ChangeMeta(mb)],
            )
            .await
            .unwrap();
        f.idx
            .on_rewritten(&mut tx, f.e.key, &[(a, a2), (b, b2)], RewriteOp::Land)
            .await
            .unwrap();
        f.idx
            .on_rewritten(&mut tx, f.e.key, &[(a, s), (b, s)], RewriteOp::Squash)
            .await
            .unwrap();
        // Plain git history has no row, so its rewrite gets no meta.
        let plain = ChangesetId::from_bytes([0x77; 32]);
        let plain2 = ChangesetId::from_bytes([0x78; 32]);
        f.idx
            .on_rewritten(&mut tx, f.e.key, &[(plain, plain2)], RewriteOp::Land)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let rows = f.rows(&[a2, b2, s, plain2]).await;
        let by: HashMap<_, _> = rows.iter().map(|r| (r.cs, r)).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!((by[&a2].change, by[&a2].preds.clone()), (cid(1), vec![a]));
        assert_eq!(by[&b2].change, cid(2));
        assert_eq!(by[&s].change, cid(2));
        assert_eq!(by[&s].preds.len(), 2);
        // The meta objects exist and verify.
        let got =
            f.e.objects
                .get(&ObjKey::ChangeMeta(by[&a2].meta))
                .await
                .unwrap()
                .unwrap();
        let m: ChangeMeta = decode_envelope(&got).unwrap();
        assert_eq!((m.cs, m.op), (a2, RewriteOp::Land));
    }

    #[tokio::test]
    async fn the_enricher_fills_change_and_meta() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let b = commit(&f.e, &[], &[("b", b"b")]).await;
        let m = ChangeMeta::new(a, cid(4), vec![], RewriteOp::New);
        let id = f.put(&m).await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a, b]).await;
        f.idx
            .on_ingested(&mut tx, f.e.key, &[a, b], &[ObjKey::ChangeMeta(id)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut entries = vec![wire::LogEntry::default(), wire::LogEntry::default()];
        f.idx.enrich(&f.e, &[a, b], &mut entries).await.unwrap();
        assert_eq!(&entries[0].change[..], cid(4).as_bytes());
        assert_eq!(&entries[0].meta[..], id.as_bytes());
        assert!(entries[1].change.is_empty() && entries[1].meta.is_empty());
    }

    #[tokio::test]
    async fn landed_as_uses_indexes_only() {
        let f = fx().await;
        let a = commit(&f.e, &[], &[("a", b"a")]).await;
        let id = f
            .put(&ChangeMeta::new(a, cid(5), vec![], RewriteOp::New))
            .await;
        let mut tx = f.reg.refs().begin().await.unwrap();
        f.graph(&mut tx, &[a]).await;
        f.idx
            .on_ingested(&mut tx, f.e.key, &[a], &[ObjKey::ChangeMeta(id)])
            .await
            .unwrap();
        sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, 0, $3)")
            .bind(f.e.ns.as_bytes().to_vec())
            .bind("refs/heads/master")
            .bind(a.as_bytes().to_vec())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut c = f.reg.pool().acquire().await.unwrap();
        let ns = f.e.ns;
        assert_eq!(
            landed_as(&mut *c, ns, cid(5), "refs/heads/master")
                .await
                .unwrap(),
            Some(a)
        );
        assert_eq!(
            landed_as(&mut *c, ns, cid(5), "refs/heads/other")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            landed_as(&mut *c, ns, cid(6), "refs/heads/master")
                .await
                .unwrap(),
            None
        );
        let plan = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {LANDED_AS}"
        )))
        .bind(ns.as_bytes().to_vec())
        .bind(cid(5).as_bytes().to_vec())
        .bind("refs/heads/master")
        .fetch_all(&mut *c)
        .await
        .unwrap();
        let details: Vec<String> = plan.iter().map(|r| r.get::<String, _>("detail")).collect();
        assert!(
            details.iter().all(|d| !d.starts_with("SCAN")),
            "sequential scan in {details:?}"
        );
        assert!(
            details.iter().any(|d| d.contains("changes_by_change")),
            "{details:?}"
        );
    }
}
