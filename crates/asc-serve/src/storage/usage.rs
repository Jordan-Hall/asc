//! What a repository keeps in storage and how much was read from it (`Storage.GetRepoUsage`).
//!
//! * **Stored bytes by kind** come from a scan of the repository's namespace in every place it
//!   can hold objects (its target, the fallback, the instance bucket), classified by the key's
//!   directory. Objects inside packs are classified by the SQL pack index, and pack bytes the
//!   index does not name (retired objects not yet collected) count as `other`. A scan lists the
//!   whole namespace, so it runs in the background and its result is kept in `storage_usage`;
//!   the call answers with the last result and starts a new scan when none exists, when the last
//!   one is a day old, or when asked (at most every [`MIN_RESCAN`]).
//! * **Bytes read this month** are counted by [`Metered`], which wraps the store of every
//!   repository the registry opens: each successful read adds the length of the range it returned
//!   to [`ReadMeter`], which is written to `storage_reads(ns, month)` every [`FLUSH_EVERY`] and
//!   before a usage answer, so the answer includes reads up to the call.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use sqlx::{AnyPool, Row};

use crate::ai::clock;
use crate::registry::Ns;

/// How often counted reads are written to the database.
pub const FLUSH_EVERY: Duration = Duration::from_secs(60);
/// The shortest time between two scans of one repository asked for by a caller.
pub const MIN_RESCAN: i64 = 5 * 60;
/// A result older than this is scanned again on the next call.
pub const STALE_AFTER: i64 = 24 * 60 * 60;

/// What stored bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Changesets, trees, tags, change metadata and git tree overrides.
    History,
    /// Whole files.
    Files,
    /// Files stored in chunks: chunk lists and chunks.
    LargeFiles,
    /// Derived data: search and code intelligence indexes.
    Indexes,
    /// CI artifacts and caches.
    Ci,
    /// Anything else (pack bytes no live object uses, probe leftovers).
    Other,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::History,
        Kind::Files,
        Kind::LargeFiles,
        Kind::Indexes,
        Kind::Ci,
        Kind::Other,
    ];

    /// The stored code. Never reused.
    pub const fn code(self) -> i64 {
        match self {
            Kind::History => 1,
            Kind::Files => 2,
            Kind::LargeFiles => 3,
            Kind::Indexes => 4,
            Kind::Ci => 5,
            Kind::Other => 6,
        }
    }

    pub fn from_code(c: i64) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.code() == c)
    }

    /// The kind of an object directory under the namespace root (`asc_store::key::KeyClass::dir`).
    fn of_dir(dir: &str) -> Kind {
        match dir {
            "c" | "t" | "g" | "m" | "o" => Kind::History,
            "f" => Kind::Files,
            "l" | "k" => Kind::LargeFiles,
            "d" => Kind::Indexes,
            "actions" => Kind::Ci,
            _ => Kind::Other,
        }
    }

    /// The kind of a packed object by its class code (`asc_store::key::KeyClass::code`).
    fn of_class(code: i64) -> Kind {
        match code {
            1 => Kind::Files,
            2 | 3 => Kind::LargeFiles,
            4..=8 => Kind::History,
            9 => Kind::Indexes,
            _ => Kind::Other,
        }
    }
}

/// Bytes and objects of one kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub bytes: u64,
    pub objects: u64,
}

/// A repository's usage as answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Every kind, in [`Kind::ALL`] order; zero when nothing of it is stored.
    pub kinds: Vec<(Kind, Tally)>,
    /// When the stored bytes were scanned (unix seconds); 0 when never.
    pub computed_at: i64,
    /// A scan is running; its result replaces `kinds` when it ends.
    pub computing: bool,
    /// `YYYY-MM` (UTC) of `read_bytes`.
    pub month: String,
    pub read_bytes: u64,
    /// Where new objects of the repository go; `""`: the instance default.
    pub target_id: String,
    /// An earlier place that still holds objects; `Some("")`: the instance default.
    pub fallback: Option<String>,
}

impl Report {
    pub fn total(&self) -> u64 {
        self.kinds
            .iter()
            .fold(0u64, |n, (_, t)| n.saturating_add(t.bytes))
    }
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn unclamp(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

// ------------------------------------------------------------------------------ reads

/// Bytes read per namespace since the last flush.
#[derive(Default)]
pub struct ReadMeter {
    pending: Mutex<HashMap<Ns, u64>>,
}

impl fmt::Debug for ReadMeter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadMeter").finish_non_exhaustive()
    }
}

impl ReadMeter {
    pub fn add(&self, ns: Ns, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let n = p.entry(ns).or_default();
        *n = n.saturating_add(bytes);
    }

    /// Writes the counted reads to `storage_reads` under the month of `now`. What could not be
    /// written is counted again, so a failed flush loses nothing.
    pub async fn flush(&self, pool: &AnyPool, now: i64) -> Result<(), sqlx::Error> {
        let drained: Vec<(Ns, u64)> = {
            let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            p.drain().collect()
        };
        let month = clock::month(now);
        let mut left = drained.into_iter();
        while let Some((ns, bytes)) = left.next() {
            let r = sqlx::query(
                "INSERT INTO storage_reads (ns, month, bytes) VALUES ($1, $2, $3) \
                 ON CONFLICT (ns, month) DO UPDATE SET bytes = storage_reads.bytes + excluded.bytes",
            )
            .bind(ns.as_bytes().to_vec())
            .bind(month.clone())
            .bind(clamp(bytes))
            .execute(pool)
            .await;
            if let Err(e) = r {
                self.add(ns, bytes);
                for (ns, bytes) in left {
                    self.add(ns, bytes);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// The bytes of `ns` not yet flushed.
    pub fn pending(&self, ns: Ns) -> u64 {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&ns)
            .copied()
            .unwrap_or(0)
    }
}

/// A store that counts the bytes read from it for one namespace. Everything else passes through.
#[derive(Clone)]
pub struct Metered {
    inner: Arc<dyn ObjectStore>,
    ns: Ns,
    meter: Arc<ReadMeter>,
}

impl Metered {
    pub fn new(inner: Arc<dyn ObjectStore>, ns: Ns, meter: Arc<ReadMeter>) -> Self {
        Self { inner, ns, meter }
    }
}

impl fmt::Debug for Metered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metered").field("ns", &self.ns).finish()
    }
}

impl fmt::Display for Metered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Metered({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for Metered {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let head = options.head;
        let r = self.inner.get_opts(location, options).await?;
        if !head {
            self.meter
                .add(self.ns, r.range.end.saturating_sub(r.range.start));
        }
        Ok(r)
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        let r = self.inner.get_ranges(location, ranges).await?;
        self.meter.add(
            self.ns,
            r.iter()
                .map(|b| b.len() as u64)
                .fold(0, u64::saturating_add),
        );
        Ok(r)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

// ------------------------------------------------------------------------------ stored bytes

/// Scans the namespace at `root` of `store` and the pack index of `ns`.
pub async fn scan(
    pool: &AnyPool,
    ns: Ns,
    store: &dyn ObjectStore,
    root: &Path,
) -> Result<HashMap<Kind, Tally>, String> {
    let mut out: HashMap<Kind, Tally> = HashMap::new();
    let mut add = |k: Kind, bytes: u64, objects: u64| {
        let t = out.entry(k).or_default();
        t.bytes = t.bytes.saturating_add(bytes);
        t.objects = t.objects.saturating_add(objects);
    };
    let depth = root.parts().count();
    let mut pack_bytes = 0u64;
    let mut listing = store.list(Some(root));
    while let Some(meta) = listing.next().await {
        let meta = meta.map_err(|_| "the storage could not be listed".to_owned())?;
        let dir = meta
            .location
            .parts()
            .nth(depth)
            .map(|p| p.as_ref().to_owned())
            .unwrap_or_default();
        if dir == "pack" {
            pack_bytes = pack_bytes.saturating_add(meta.size);
        } else {
            add(Kind::of_dir(&dir), meta.size, 1);
        }
    }
    let rows = sqlx::query(
        "SELECT class, COUNT(*) AS n, CAST(SUM(len) AS BIGINT) AS bytes FROM pack_objects \
         WHERE ns = $1 GROUP BY class",
    )
    .bind(ns.as_bytes().to_vec())
    .fetch_all(pool)
    .await
    .map_err(|_| "the pack index could not be read".to_owned())?;
    let mut indexed = 0u64;
    for r in rows {
        let class: i64 = r.try_get("class").unwrap_or(0);
        let n = unclamp(r.try_get("n").unwrap_or(0));
        let bytes = unclamp(r.try_get("bytes").unwrap_or(0));
        indexed = indexed.saturating_add(bytes);
        add(Kind::of_class(class), bytes, n);
    }
    add(Kind::Other, pack_bytes.saturating_sub(indexed), 0);
    Ok(out)
}

/// Replaces the stored result of `ns`.
pub async fn save(
    pool: &AnyPool,
    ns: Ns,
    kinds: &HashMap<Kind, Tally>,
    at: i64,
) -> Result<(), sqlx::Error> {
    let key = ns.as_bytes().to_vec();
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM storage_usage WHERE ns = $1")
        .bind(key.clone())
        .execute(&mut *tx)
        .await?;
    for (k, t) in kinds {
        sqlx::query(
            "INSERT INTO storage_usage (ns, kind, bytes, objects, computed_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(key.clone())
        .bind(k.code())
        .bind(clamp(t.bytes))
        .bind(clamp(t.objects))
        .bind(at)
        .execute(&mut *tx)
        .await?;
    }
    // A repository with nothing stored still has a result: one empty row says when.
    if kinds.is_empty() {
        sqlx::query(
            "INSERT INTO storage_usage (ns, kind, bytes, objects, computed_at) VALUES ($1, $2, 0, 0, $3)",
        )
        .bind(key)
        .bind(Kind::Other.code())
        .bind(at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// The stored result of `ns` and the reads of the month of `now`.
pub async fn load(pool: &AnyPool, ns: Ns, now: i64) -> Result<Report, sqlx::Error> {
    let key = ns.as_bytes().to_vec();
    let rows =
        sqlx::query("SELECT kind, bytes, objects, computed_at FROM storage_usage WHERE ns = $1")
            .bind(key.clone())
            .fetch_all(pool)
            .await?;
    let mut got: HashMap<Kind, Tally> = HashMap::new();
    let mut computed_at = 0;
    for r in rows {
        computed_at = r.try_get::<i64, _>("computed_at")?;
        if let Some(k) = Kind::from_code(r.try_get("kind")?) {
            got.insert(
                k,
                Tally {
                    bytes: unclamp(r.try_get("bytes")?),
                    objects: unclamp(r.try_get("objects")?),
                },
            );
        }
    }
    let month = clock::month(now);
    let read: Option<i64> =
        sqlx::query_scalar("SELECT bytes FROM storage_reads WHERE ns = $1 AND month = $2")
            .bind(key)
            .bind(month.clone())
            .fetch_optional(pool)
            .await?;
    Ok(Report {
        kinds: Kind::ALL
            .into_iter()
            .map(|k| (k, got.get(&k).copied().unwrap_or_default()))
            .collect(),
        computed_at,
        computing: false,
        month,
        read_bytes: unclamp(read.unwrap_or(0)),
        ..Report::default()
    })
}

/// Whether a call should start a scan.
pub fn wants_scan(computed_at: i64, refresh: bool, now: i64) -> bool {
    computed_at == 0
        || now.saturating_sub(computed_at) >= STALE_AFTER
        || refresh && now.saturating_sub(computed_at) >= MIN_RESCAN
}

/// The namespaces being scanned, so one repository has one scan at a time.
#[derive(Default)]
pub struct Scans {
    running: Mutex<HashSet<Ns>>,
}

impl Scans {
    /// Claims `ns`; false when a scan of it is already running.
    pub fn claim(&self, ns: Ns) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ns)
    }

    pub fn release(&self, ns: Ns) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ns);
    }

    pub fn running(&self, ns: Ns) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;

    fn ns(b: u8) -> Ns {
        Ns::from_bytes(&[b; 16]).unwrap()
    }

    #[test]
    fn directories_and_classes_map_to_kinds() {
        for (d, k) in [
            ("c", Kind::History),
            ("t", Kind::History),
            ("g", Kind::History),
            ("m", Kind::History),
            ("o", Kind::History),
            ("f", Kind::Files),
            ("l", Kind::LargeFiles),
            ("k", Kind::LargeFiles),
            ("d", Kind::Indexes),
            ("actions", Kind::Ci),
            (".asc-probe", Kind::Other),
        ] {
            assert_eq!(Kind::of_dir(d), k, "{d}");
        }
        use asc_store::key::KeyClass as C;
        for (c, k) in [
            (C::File, Kind::Files),
            (C::ChunkList, Kind::LargeFiles),
            (C::Chunk, Kind::LargeFiles),
            (C::Tree, Kind::History),
            (C::Changeset, Kind::History),
            (C::Tag, Kind::History),
            (C::GitTreeOverride, Kind::History),
            (C::ChangeMeta, Kind::History),
            (C::Derived, Kind::Indexes),
        ] {
            assert_eq!(Kind::of_class(i64::from(c.code())), k, "{c:?}");
            assert_eq!(Kind::of_dir(c.dir()), k, "{c:?}");
        }
        for k in Kind::ALL {
            assert_eq!(Kind::from_code(k.code()), Some(k));
        }
        assert_eq!(Kind::from_code(0), None);
    }

    #[test]
    fn a_scan_is_wanted_when_none_ran_when_stale_or_when_asked_after_the_pause() {
        let now = 1_000_000;
        assert!(wants_scan(0, false, now));
        assert!(!wants_scan(now - 60, false, now));
        assert!(!wants_scan(now - 60, true, now), "asked too soon");
        assert!(wants_scan(now - MIN_RESCAN, true, now));
        assert!(wants_scan(now - STALE_AFTER, false, now));
    }

    #[tokio::test]
    async fn reads_are_counted_per_namespace_and_heads_and_writes_are_not() {
        let meter = Arc::new(ReadMeter::default());
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let a = Metered::new(inner.clone(), ns(1), meter.clone());
        let b = Metered::new(inner, ns(2), meter.clone());
        let p = Path::from("ns/x/f/ab/abcd");
        a.put(&p, PutPayload::from_static(b"0123456789"))
            .await
            .unwrap();
        assert_eq!(meter.pending(ns(1)), 0, "a write is not a read");
        a.head(&p).await.unwrap();
        assert_eq!(meter.pending(ns(1)), 0, "a head reads no bytes");
        a.get(&p).await.unwrap().bytes().await.unwrap();
        assert_eq!(meter.pending(ns(1)), 10);
        a.get_range(&p, 2..5).await.unwrap();
        assert_eq!(meter.pending(ns(1)), 13);
        b.get_ranges(&p, &[0..1, 4..8]).await.unwrap();
        assert_eq!(meter.pending(ns(2)), 5);
        assert!(a.get(&Path::from("ns/x/none")).await.is_err());
        assert_eq!(meter.pending(ns(1)), 13, "a failed read counts nothing");
    }
}
