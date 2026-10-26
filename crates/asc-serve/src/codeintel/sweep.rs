//! The weekly sweep (SPEC §23.5).
//!
//! Live ords are the union of the `tdocs` of the warmed heads: land-branch tips, open PR heads
//! and drafts touched in the last 30 days. An ord that has been dead for 14 days is tombstoned:
//! removed from the `files` and `chunks` indexes (its bit leaves the `indexed` bitmap in a new
//! generation, so no query sees it), dropped from the vector segments at their next merge
//! (merges read the tombstone flag), and its `d/tags.v1` and `d/chunks.v1` objects removed
//! (their keys are scoped to the namespace, so another namespace's copies are untouched).
//! Ords are never reused: `ci_docs` rows stay. Content that becomes reachable again loses its
//! tombstone and is enqueued as new work. The sweep is idempotent and runs on the leader only.
//!
//! A sweep that cannot compute the whole live set (a head's scope fails, or there are no heads
//! at all) changes nothing: tombstoning against a partial live set would delete live content.

use std::collections::BTreeMap;
use std::sync::Arc;

use asc_codeintel::index::manifest::{Cas, DropKind, Dropped, ManifestStore, Publisher};
use asc_codeintel::index::schema::IndexKind;
use asc_codeintel::index::segments::{Hash, get_indexed, put_indexed};
use asc_codeintel::scope::{OrdSource, ScopeEngine};
use asc_core::id::{ChangesetId, FileId};
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use sqlx::{AnyPool, Row};

use super::modelgen::{CiError, MAX_CAS, Result, cas_row, load_row, store_err};
use super::queue::{NewJob, Priority, Queue};
use super::worker::{chunks_key, ns_name, tags_key};

/// `ci_docs.flags` bit that marks a tombstoned doc (the skip flags use bits 0 to 6).
pub const TOMBSTONE: u8 = 0x80;
/// An ord must stay dead this long before it is tombstoned.
pub const DEAD_GRACE_MS: u64 = 14 * 24 * 3600 * 1000;
/// Drafts older than this are not heads.
pub const DRAFT_WINDOW_MS: u64 = 30 * 24 * 3600 * 1000;
/// The sweep runs this often.
pub const INTERVAL_MS: u64 = 7 * 24 * 3600 * 1000;
/// `ci_manifest.idx` of the sweep's memory.
pub const IDX_SWEEP: &str = "sweep";
/// How long a sweep holds the leader lease.
const LEASE_MS: u64 = 10 * 60 * 1000;
/// Most heads one sweep reads.
pub const MAX_HEADS: usize = 100_000;

/// Which kind of ref a head is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadKind {
    Land,
    Pr,
    Draft,
}

/// A changeset that keeps its content alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub rev: ChangesetId,
    pub kind: HeadKind,
    /// When the head last moved, in ms since the epoch (decides whether a draft counts).
    pub touched_ms: u64,
}

/// The heads of a namespace.
#[async_trait]
pub trait Heads: Send + Sync {
    async fn heads(&self, ns: &[u8]) -> Result<Vec<Head>>;
}

/// The ords a changeset's tree holds (`tdocs`).
#[async_trait]
pub trait Scopes: Send + Sync {
    async fn ords(&self, rev: ChangesetId) -> Result<RoaringBitmap>;
}

#[async_trait]
impl<O, R, S> Scopes for ScopeEngine<O, R, S>
where
    O: Objects + 'static,
    R: Refs + 'static,
    S: OrdSource + 'static,
{
    async fn ords(&self, rev: ChangesetId) -> Result<RoaringBitmap> {
        let t = self.scope(rev).await.map_err(store_err)?;
        Ok(t.bitmap().clone())
    }
}

/// True for a head that counts at `now_ms`.
pub fn is_live_head(h: &Head, now_ms: u64) -> bool {
    match h.kind {
        HeadKind::Land | HeadKind::Pr => true,
        HeadKind::Draft => h.touched_ms.saturating_add(DRAFT_WINDOW_MS) >= now_ms,
    }
}

/// Tombstoned ords of `ns`.
pub async fn tombstoned(pool: &AnyPool, ns: &[u8]) -> Result<RoaringBitmap> {
    let rows = sqlx::query(
        "SELECT CAST(ord AS BIGINT) AS ord FROM ci_docs WHERE ns = $1 AND flags >= 128",
    )
    .bind(ns.to_vec())
    .fetch_all(pool)
    .await?;
    let mut out = RoaringBitmap::new();
    for r in rows {
        let o: i64 = r.try_get("ord")?;
        out.insert(u32::try_from(o).map_err(|_| CiError::Corrupt("ord"))?);
    }
    Ok(out)
}

/// When each currently dead ord was first seen dead.
#[derive(Default, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SweepState {
    pub dead_since: Vec<(u32, u64)>,
}

/// What one sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Newly tombstoned ords.
    pub tombstoned: usize,
    /// Tombstoned ords that became reachable again and were enqueued.
    pub resurrected: usize,
    /// Dead ords still inside the 14-day grace.
    pub waiting: usize,
}

/// The sweep of one server.
pub struct Sweeper {
    pool: AnyPool,
    objects: Arc<dyn Objects>,
    manifests: Arc<dyn ManifestStore>,
    heads: Arc<dyn Heads>,
    scopes: Arc<dyn Scopes>,
    queue: Queue,
    holder: String,
}

impl Sweeper {
    pub fn new(
        pool: AnyPool,
        objects: Arc<dyn Objects>,
        manifests: Arc<dyn ManifestStore>,
        heads: Arc<dyn Heads>,
        scopes: Arc<dyn Scopes>,
        holder: &str,
    ) -> Self {
        Self {
            queue: Queue::new(pool.clone()),
            pool,
            objects,
            manifests,
            heads,
            scopes,
            holder: holder.to_owned(),
        }
    }

    fn publisher<'a>(&'a self, nsn: &'a str, kind: IndexKind) -> Publisher<'a> {
        Publisher {
            objects: &*self.objects,
            manifests: &*self.manifests,
            ns: nsn,
            kind,
            holder: &self.holder,
        }
    }

    async fn state(&self, ns: &[u8]) -> Result<(i64, SweepState)> {
        match load_row(&self.pool, ns, IDX_SWEEP).await? {
            None => Ok((0, SweepState::default())),
            Some((g, _, blob)) => Ok((
                g,
                serde_json::from_slice(&blob).map_err(|_| CiError::Corrupt("sweep state"))?,
            )),
        }
    }

    async fn save_state(&self, ns: &[u8], st: &SweepState) -> Result<()> {
        let blob = serde_json::to_vec(st).map_err(|_| CiError::Corrupt("sweep state"))?;
        for _ in 0..MAX_CAS {
            let (g, _) = self.state(ns).await?;
            if cas_row(&self.pool, ns, IDX_SWEEP, g, 0, &blob).await? {
                return Ok(());
            }
        }
        Err(CiError::Contended(MAX_CAS))
    }

    /// The union of the scopes of the live heads.
    async fn live_ords(&self, ns: &[u8], now_ms: u64) -> Result<RoaringBitmap> {
        let heads = self.heads.heads(ns).await?;
        if heads.len() > MAX_HEADS {
            return Err(CiError::Invalid("too many heads"));
        }
        let mut live = RoaringBitmap::new();
        let mut any = false;
        for h in heads.iter().filter(|h| is_live_head(h, now_ms)) {
            any = true;
            live |= self.scopes.ords(h.rev).await?;
        }
        if !any {
            // No head at all means no information, not an empty repository.
            return Err(CiError::Invalid("no live heads"));
        }
        Ok(live)
    }

    /// Runs one sweep. `None` when this node is not the namespace leader.
    pub async fn run(&self, ns: &[u8], now_ms: u64) -> Result<Option<SweepReport>> {
        let nsn = ns_name(ns)?;
        for kind in [IndexKind::Files, IndexKind::Chunks] {
            if !self.publisher(&nsn, kind).lease(now_ms, LEASE_MS).await? {
                return Ok(None);
            }
        }
        let live = self.live_ords(ns, now_ms).await?;

        let rows = sqlx::query(
            "SELECT CAST(ord AS BIGINT) AS ord, file, CAST(flags AS BIGINT) AS flags \
             FROM ci_docs WHERE ns = $1",
        )
        .bind(ns.to_vec())
        .fetch_all(&self.pool)
        .await?;
        let (_, old) = self.state(ns).await?;
        let old: BTreeMap<u32, u64> = old.dead_since.into_iter().collect();

        let mut since: BTreeMap<u32, u64> = BTreeMap::new();
        let mut doomed: Vec<(u32, FileId)> = Vec::new();
        let mut revived: Vec<(u32, FileId)> = Vec::new();
        let mut waiting = 0;
        for r in &rows {
            let ord =
                u32::try_from(r.try_get::<i64, _>("ord")?).map_err(|_| CiError::Corrupt("ord"))?;
            let flags = r.try_get::<i64, _>("flags")?;
            let file: Vec<u8> = r.try_get("file")?;
            let file = FileId::from_bytes(
                file.as_slice()
                    .try_into()
                    .map_err(|_| CiError::Corrupt("file id"))?,
            );
            let tomb = flags & i64::from(TOMBSTONE) != 0;
            if live.contains(ord) {
                if tomb {
                    revived.push((ord, file));
                }
                continue;
            }
            // Dead. A tombstoned ord is re-applied every sweep (idempotent), which finishes a
            // sweep that stopped half way.
            let first = *old.get(&ord).unwrap_or(&now_ms);
            since.insert(ord, first);
            if tomb || now_ms.saturating_sub(first) >= DEAD_GRACE_MS {
                doomed.push((ord, file));
            } else {
                waiting += 1;
            }
        }
        // Remember first-dead times before acting, so a crash does not restart the clock.
        self.save_state(
            ns,
            &SweepState {
                dead_since: since.into_iter().collect(),
            },
        )
        .await?;

        let mut report = SweepReport {
            waiting,
            ..SweepReport::default()
        };
        // Flag first: the intent survives a crash, and the next sweep re-applies the rest.
        for (ord, _) in &doomed {
            let n = sqlx::query(
                "UPDATE ci_docs SET flags = flags | 128 WHERE ns = $1 AND ord = $2 AND flags < 128",
            )
            .bind(ns.to_vec())
            .bind(i64::from(*ord))
            .execute(&self.pool)
            .await?
            .rows_affected();
            report.tombstoned += n as usize;
        }
        if !doomed.is_empty() {
            let set: RoaringBitmap = doomed.iter().map(|(o, _)| *o).collect();
            for kind in [IndexKind::Files, IndexKind::Chunks] {
                self.hide(&nsn, kind, &set, now_ms).await?;
            }
            // Derived keys are scoped to this namespace, so removing them cannot touch another
            // namespace's objects. A worker that re-indexes the same file here clears the
            // tombstone (`reserve_doc`) before it writes, so a row that is no longer tombstoned
            // after the delete may have had its fresh objects deleted: put it back under the
            // tombstone and queue it, and the job rewrites them.
            let mut raced: Vec<(u32, FileId)> = Vec::new();
            for (ord, file) in &doomed {
                for key in [tags_key(ns, file), chunks_key(ns, file)] {
                    self.objects.delete(&key).await.map_err(store_err)?;
                }
                if !self.is_tombstoned(ns, *ord).await? {
                    raced.push((*ord, *file));
                    continue;
                }
                sqlx::query("DELETE FROM ci_chunks WHERE ns = $1 AND doc_ord = $2")
                    .bind(ns.to_vec())
                    .bind(i64::from(*ord))
                    .execute(&self.pool)
                    .await?;
            }
            if !raced.is_empty() {
                for (ord, _) in &raced {
                    sqlx::query(
                        "UPDATE ci_docs SET flags = flags | 128 WHERE ns = $1 AND ord = $2 AND flags < 128",
                    )
                    .bind(ns.to_vec())
                    .bind(i64::from(*ord))
                    .execute(&self.pool)
                    .await?;
                }
                let set: RoaringBitmap = raced.iter().map(|(o, _)| *o).collect();
                for kind in [IndexKind::Files, IndexKind::Chunks] {
                    self.hide(&nsn, kind, &set, now_ms).await?;
                }
                let jobs: Vec<NewJob> = raced
                    .iter()
                    .map(|(_, f)| NewJob::file(f.as_bytes(), Priority::Backfill))
                    .collect();
                for part in jobs.chunks(super::queue::MAX_ENQUEUE) {
                    self.queue
                        .enqueue(ns, part, i64::try_from(now_ms / 1000).unwrap_or(i64::MAX))
                        .await?;
                }
            }
        }
        // Reachable again: clear the tombstone and index it as new work.
        if !revived.is_empty() {
            let jobs: Vec<NewJob> = revived
                .iter()
                .map(|(_, f)| NewJob::file(f.as_bytes(), Priority::Backfill))
                .collect();
            for (ord, _) in &revived {
                sqlx::query("UPDATE ci_docs SET flags = flags - 128 WHERE ns = $1 AND ord = $2 AND flags >= 128")
                    .bind(ns.to_vec())
                    .bind(i64::from(*ord))
                    .execute(&self.pool)
                    .await?;
            }
            for part in jobs.chunks(super::queue::MAX_ENQUEUE) {
                self.queue
                    .enqueue(ns, part, i64::try_from(now_ms / 1000).unwrap_or(i64::MAX))
                    .await?;
            }
            report.resurrected = revived.len();
        }
        Ok(Some(report))
    }

    async fn is_tombstoned(&self, ns: &[u8], ord: u32) -> Result<bool> {
        let n: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM ci_docs WHERE ns = $1 AND ord = $2 AND flags >= 128",
        )
        .bind(ns.to_vec())
        .bind(i64::from(ord))
        .fetch_one(&self.pool)
        .await?
        .try_get("n")?;
        Ok(n > 0)
    }

    /// Removes `set` from the `indexed` bitmap of a kind in a new generation. A no-op (no new
    /// generation) when none of the ords is visible.
    async fn hide(
        &self,
        nsn: &str,
        kind: IndexKind,
        set: &RoaringBitmap,
        now_ms: u64,
    ) -> Result<()> {
        let mut cur = self.manifests.load(nsn, kind).await?;
        for _ in 0..MAX_CAS {
            let Some(old) = cur.indexed else {
                return Ok(());
            };
            let mut bm = get_indexed(&*self.objects, nsn, kind, old).await?;
            if (&bm & set).is_empty() {
                return Ok(());
            }
            bm -= set;
            let new: Hash = put_indexed(&*self.objects, nsn, kind, &bm).await?;
            let mut next = cur.clone();
            next.generation = cur.generation + 1;
            next.indexed = Some(new);
            next.dropped.push(Dropped {
                kind: DropKind::Indexed,
                hash: old,
                at_ms: now_ms,
            });
            match self
                .manifests
                .cas(nsn, kind, cur.generation, next, &self.holder, now_ms)
                .await?
            {
                Cas::Won(_) => return Ok(()),
                Cas::Lost(m) => cur = m,
            }
        }
        Err(CiError::Contended(MAX_CAS))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use asc_codeintel::index::manifest::MemManifest;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;

    use super::*;
    use crate::codeintel::modelgen::tests::pool;
    use crate::codeintel::worker::{read_chunk_rows, read_tags};

    const NS: &[u8] = b"sweep-ns";
    const DAY: u64 = 24 * 3600 * 1000;

    struct Fixed(Vec<Head>);

    #[async_trait]
    impl Heads for Fixed {
        async fn heads(&self, _: &[u8]) -> Result<Vec<Head>> {
            Ok(self.0.clone())
        }
    }

    struct Table(HashMap<ChangesetId, RoaringBitmap>);

    #[async_trait]
    impl Scopes for Table {
        async fn ords(&self, rev: ChangesetId) -> Result<RoaringBitmap> {
            self.0
                .get(&rev)
                .cloned()
                .ok_or(CiError::Invalid("unknown rev"))
        }
    }

    fn rev(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    fn bits(v: &[u32]) -> RoaringBitmap {
        v.iter().copied().collect()
    }

    struct Rig {
        pool: AnyPool,
        objects: Arc<BucketObjects>,
        manifests: Arc<MemManifest>,
        files: Vec<FileId>,
    }

    /// Four docs (ords 0 to 3), all indexed in both indexes, with derived objects.
    async fn rig() -> Rig {
        let pool = pool().await;
        let objects = Arc::new(BucketObjects::memory());
        let manifests = Arc::new(MemManifest::new());
        let nsn = ns_name(NS).unwrap();
        let mut files = Vec::new();
        for ord in 0..4u32 {
            let f = FileId::of(&[ord as u8; 4]);
            files.push(f);
            sqlx::query(
                "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) VALUES ($1, $2, $3, 4, 0, $4)",
            )
            .bind(NS.to_vec())
            .bind(f.as_bytes().to_vec())
            .bind(i64::from(ord))
            .bind(vec![0u8; 12])
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO ci_chunks (ns, chunk_ord, doc_ord, lang, start_line, end_line, text_id) \
                 VALUES ($1, $2, $3, 'rust', 1, 1, $4)",
            )
            .bind(NS.to_vec())
            .bind(i64::from(ord) << 32)
            .bind(i64::from(ord))
            .bind(vec![ord as u8; 32])
            .execute(&pool)
            .await
            .unwrap();
            for key in [tags_key(NS, &f), chunks_key(NS, &f)] {
                objects.put(&key, encode_stored(b"[]")).await.unwrap();
            }
        }
        for kind in [IndexKind::Files, IndexKind::Chunks] {
            let h = put_indexed(&*objects, &nsn, kind, &bits(&[0, 1, 2, 3]))
                .await
                .unwrap();
            assert!(
                manifests
                    .lease(&nsn, kind, "node-a", 0, 1_000_000_000_000)
                    .await
                    .unwrap()
            );
            let mut m = manifests.load(&nsn, kind).await.unwrap();
            m.generation = 1;
            m.indexed = Some(h);
            assert!(matches!(
                manifests.cas(&nsn, kind, 0, m, "node-a", 0).await.unwrap(),
                Cas::Won(_)
            ));
        }
        Rig {
            pool,
            objects,
            manifests,
            files,
        }
    }

    fn sweeper(r: &Rig, heads: Vec<Head>) -> Sweeper {
        let scopes = Table(HashMap::from([
            (rev(1), bits(&[0])),    // land tip
            (rev(2), bits(&[0, 1])), // open PR head
            (rev(3), bits(&[0, 2])), // abandoned draft
            (rev(4), bits(&[0, 3])), // recent draft
        ]));
        Sweeper::new(
            r.pool.clone(),
            r.objects.clone(),
            r.manifests.clone(),
            Arc::new(Fixed(heads)),
            Arc::new(scopes),
            "node-a",
        )
    }

    fn head(n: u8, kind: HeadKind, touched_ms: u64) -> Head {
        Head {
            rev: rev(n),
            kind,
            touched_ms,
        }
    }

    async fn indexed(r: &Rig, kind: IndexKind) -> RoaringBitmap {
        let nsn = ns_name(NS).unwrap();
        let m = r.manifests.load(&nsn, kind).await.unwrap();
        get_indexed(&*r.objects, &nsn, kind, m.indexed.unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_abandoned_draft_is_tombstoned_after_the_grace_and_a_pr_head_keeps_its_ords() {
        let r = rig().await;
        let t0 = 100 * DAY;
        let heads = vec![
            head(1, HeadKind::Land, t0),
            head(2, HeadKind::Pr, t0 - 90 * DAY), // old, but an open PR head still counts
            head(3, HeadKind::Draft, t0 - 31 * DAY), // untouched for more than 30 days
            head(4, HeadKind::Draft, t0 - DAY),
        ];
        let s = sweeper(&r, heads);

        let rep = s.run(NS, t0).await.unwrap().unwrap();
        assert_eq!((rep.tombstoned, rep.waiting), (0, 1)); // ord 2 starts its 14 days
        assert_eq!(indexed(&r, IndexKind::Files).await, bits(&[0, 1, 2, 3]));

        let rep = s.run(NS, t0 + 14 * DAY - 1).await.unwrap().unwrap();
        assert_eq!(rep.tombstoned, 0);

        let rep = s.run(NS, t0 + 14 * DAY).await.unwrap().unwrap();
        assert_eq!(rep.tombstoned, 1);
        for kind in [IndexKind::Files, IndexKind::Chunks] {
            assert_eq!(indexed(&r, kind).await, bits(&[0, 1, 3]));
        }
        // Its derived objects are gone; the PR head's are not.
        assert!(
            r.objects
                .get(&tags_key(NS, &r.files[2]))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            r.objects
                .get(&chunks_key(NS, &r.files[2]))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            r.objects
                .get(&tags_key(NS, &r.files[1]))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(tombstoned(&r.pool, NS).await.unwrap(), bits(&[2]));
        // The ord stays: the row is kept so the ord is never handed out again.
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM ci_docs WHERE ns = $1")
            .bind(NS.to_vec())
            .fetch_one(&r.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 4);

        // Idempotent: another sweep writes no new generation.
        let nsn = ns_name(NS).unwrap();
        let g = r
            .manifests
            .load(&nsn, IndexKind::Files)
            .await
            .unwrap()
            .generation;
        let rep = s.run(NS, t0 + 15 * DAY).await.unwrap().unwrap();
        assert_eq!(rep.tombstoned, 0);
        assert_eq!(
            r.manifests
                .load(&nsn, IndexKind::Files)
                .await
                .unwrap()
                .generation,
            g
        );
    }

    #[tokio::test]
    async fn content_that_becomes_reachable_again_is_enqueued_as_new_work() {
        let r = rig().await;
        let t0 = 100 * DAY;
        let dead = sweeper(&r, vec![head(1, HeadKind::Land, t0)]);
        dead.run(NS, t0).await.unwrap();
        let rep = dead.run(NS, t0 + 14 * DAY).await.unwrap().unwrap();
        assert_eq!(rep.tombstoned, 3);
        assert_eq!(indexed(&r, IndexKind::Files).await, bits(&[0]));

        let back = sweeper(
            &r,
            vec![head(1, HeadKind::Land, t0), head(2, HeadKind::Pr, t0)],
        );
        let rep = back.run(NS, t0 + 15 * DAY).await.unwrap().unwrap();
        assert_eq!(rep.resurrected, 1);
        assert_eq!(tombstoned(&r.pool, NS).await.unwrap(), bits(&[2, 3]));
        let q = Queue::new(r.pool.clone());
        let jobs = q
            .lease(NS, 10, 60, 100 * 86_400 + 15 * 86_400)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, r.files[1].as_bytes().to_vec());
    }

    #[tokio::test]
    async fn a_sweep_without_the_whole_live_set_changes_nothing() {
        let r = rig().await;
        let t0 = 100 * DAY;
        // No heads at all.
        let s = sweeper(&r, vec![]);
        assert!(s.run(NS, t0 + 30 * DAY).await.is_err());
        // A head whose scope cannot be derived.
        let s = sweeper(
            &r,
            vec![head(1, HeadKind::Land, t0), head(9, HeadKind::Pr, t0)],
        );
        assert!(s.run(NS, t0 + 30 * DAY).await.is_err());
        assert_eq!(indexed(&r, IndexKind::Files).await, bits(&[0, 1, 2, 3]));
        assert!(tombstoned(&r.pool, NS).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_follower_does_not_sweep() {
        let r = rig().await;
        // The rig's lease is node-a's for a very long time; node-b cannot take it.
        let s = Sweeper::new(
            r.pool.clone(),
            r.objects.clone(),
            r.manifests.clone(),
            Arc::new(Fixed(vec![head(1, HeadKind::Land, 0)])),
            Arc::new(Table(HashMap::from([(rev(1), bits(&[0]))]))),
            "node-b",
        );
        assert!(s.run(NS, 5).await.unwrap().is_none());
    }

    const OTHER: &[u8] = b"other-tenant-ns";

    /// Gives `OTHER` a doc row and derived objects for the same file content as `files[2]`.
    async fn share_file_with_other_ns(r: &Rig) {
        let f = r.files[2];
        sqlx::query(
            "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) VALUES ($1, $2, 0, 4, 0, $3)",
        )
        .bind(OTHER.to_vec())
        .bind(f.as_bytes().to_vec())
        .bind(vec![0u8; 12])
        .execute(&r.pool)
        .await
        .unwrap();
        for key in [tags_key(OTHER, &f), chunks_key(OTHER, &f)] {
            r.objects.put(&key, encode_stored(b"[]")).await.unwrap();
        }
    }

    #[tokio::test]
    async fn sweeping_one_namespace_keeps_the_derived_objects_of_another_with_the_same_file() {
        let r = rig().await;
        share_file_with_other_ns(&r).await;
        let f = r.files[2];
        assert_ne!(tags_key(NS, &f), tags_key(OTHER, &f));
        assert_ne!(chunks_key(NS, &f), chunks_key(OTHER, &f));

        let t0 = 100 * DAY;
        let s = sweeper(
            &r,
            vec![
                head(1, HeadKind::Land, t0),
                head(2, HeadKind::Pr, t0),
                head(4, HeadKind::Draft, t0),
            ],
        );
        s.run(NS, t0).await.unwrap();
        let rep = s.run(NS, t0 + 14 * DAY).await.unwrap().unwrap();
        assert_eq!(rep.tombstoned, 1); // ord 2 in NS
        assert!(read_tags(&*r.objects, NS, &f).await.unwrap().is_none());
        assert!(
            read_chunk_rows(&*r.objects, NS, &f)
                .await
                .unwrap()
                .is_none()
        );
        // The other namespace still loads both.
        assert_eq!(
            read_tags(&*r.objects, OTHER, &f).await.unwrap(),
            Some(vec![])
        );
        assert_eq!(
            read_chunk_rows(&*r.objects, OTHER, &f).await.unwrap(),
            Some(vec![])
        );
    }

    /// Runs `before_delete` (a concurrent worker) just before the first object delete.
    struct Racing {
        inner: Arc<BucketObjects>,
        pool: AnyPool,
        fired: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Objects for Racing {
        async fn get(&self, key: &ObjKey) -> asc_store::objects::Result<Option<bytes::Bytes>> {
            self.inner.get(key).await
        }
        async fn get_stored(
            &self,
            key: &ObjKey,
        ) -> asc_store::objects::Result<Option<bytes::Bytes>> {
            self.inner.get_stored(key).await
        }
        async fn get_range(
            &self,
            key: &ObjKey,
            r: std::ops::Range<u64>,
        ) -> asc_store::objects::Result<Option<bytes::Bytes>> {
            self.inner.get_range(key, r).await
        }
        async fn put(&self, key: &ObjKey, stored: bytes::Bytes) -> asc_store::objects::Result<()> {
            self.inner.put(key, stored).await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> asc_store::objects::Result<Vec<bool>> {
            self.inner.has_many(keys).await
        }
        async fn delete(&self, key: &ObjKey) -> asc_store::objects::Result<()> {
            if matches!(key, ObjKey::Derived { name: "tags", .. })
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                // A worker re-indexes the file in this namespace: `reserve_doc` clears the
                // tombstone, then it writes the objects. The sweep's delete lands after.
                sqlx::query("UPDATE ci_docs SET flags = 0 WHERE ns = $1 AND ord = 2")
                    .bind(NS.to_vec())
                    .execute(&self.pool)
                    .await
                    .unwrap();
            }
            self.inner.delete(key).await
        }
        fn list(
            &self,
            prefix: asc_store::key::KeyPrefix,
        ) -> futures::stream::BoxStream<
            '_,
            asc_store::objects::Result<(ObjKey, std::time::SystemTime)>,
        > {
            self.inner.list(prefix)
        }
    }

    #[tokio::test]
    async fn a_reindex_racing_the_delete_is_tombstoned_again_and_requeued() {
        let r = rig().await;
        let t0 = 100 * DAY;
        let racing = Arc::new(Racing {
            inner: r.objects.clone(),
            pool: r.pool.clone(),
            fired: std::sync::atomic::AtomicBool::new(false),
        });
        let s = Sweeper::new(
            r.pool.clone(),
            racing,
            r.manifests.clone(),
            Arc::new(Fixed(vec![
                head(1, HeadKind::Land, t0),
                head(2, HeadKind::Pr, t0),
                head(4, HeadKind::Draft, t0),
            ])),
            Arc::new(Table(HashMap::from([
                (rev(1), bits(&[0])),
                (rev(2), bits(&[0, 1])),
                (rev(4), bits(&[0, 3])),
            ]))),
            "node-a",
        );
        s.run(NS, t0).await.unwrap();
        s.run(NS, t0 + 14 * DAY).await.unwrap();
        // The row is under the tombstone again, hidden, with its chunk rows kept, and a file job
        // is queued so the objects are rewritten.
        assert_eq!(tombstoned(&r.pool, NS).await.unwrap(), bits(&[2]));
        assert!(!indexed(&r, IndexKind::Files).await.contains(2));
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM ci_chunks WHERE ns = $1 AND doc_ord = 2")
            .bind(NS.to_vec())
            .fetch_one(&r.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 1);
        let jobs = Queue::new(r.pool.clone())
            .lease(NS, 10, 60, 100 * 86_400 + 15 * 86_400)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, r.files[2].as_bytes().to_vec());
    }

    #[test]
    fn drafts_count_for_thirty_days() {
        let d = head(1, HeadKind::Draft, 10 * DAY);
        assert!(is_live_head(&d, 40 * DAY));
        assert!(!is_live_head(&d, 40 * DAY + 1));
        assert!(is_live_head(&head(1, HeadKind::Pr, 0), 1000 * DAY));
    }
}
