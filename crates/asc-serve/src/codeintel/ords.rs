//! The doc table of one namespace as the query side needs it (`ci_docs`, `ci_doc_langs`,
//! `ci_chunks`): `FileId` to ord and back, the flags of a doc, and its counts (SPEC §23.2.1,
//! PLAN I16).
//!
//! [`NsDocs`] is the [`OrdSource`] and the [`DocDirectory`] of the scope algebra and the exact
//! searcher. Both traits are synchronous, the database is not, so each lookup that misses the
//! cache bridges with `block_in_place`; the service refuses to run on a runtime that cannot do
//! that (see [`can_block`]). Reads are cached in bounded caches. A `FileId` the table does not
//! know is given the next dense ord at once and a `file` job is queued for it, because a scope
//! that holds the file needs its ord before the worker has seen the bytes (SPEC §23.2.3). The
//! row it leaves is a placeholder (`size` 0, no flags, versions all zero): it carries no claim
//! about the bytes, and the worker rewrites it with the real flags when it indexes the file.
//! Nothing here treats a placeholder as clean: [`DocInfo::indexed`] is false for it, and the
//! callers decide the secret flag from the bytes instead.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use asc_codeintel::doc::DocError;
use asc_codeintel::lang::Lang;
use asc_codeintel::scope::OrdSource;
use asc_codeintel::search::DocDirectory;
use asc_codeintel::skip::Flags;
use asc_core::id::FileId;
use roaring::RoaringBitmap;
use sqlx::{AnyPool, Row};
use tokio::runtime::{Handle, RuntimeFlavor};

use super::queue::{NewJob, Priority, Queue};
use crate::outbox::now;

/// Entries in each of the two lookup caches of a namespace.
pub const CACHE_ENTRIES: usize = 1 << 20;
/// Ords in one `IN (...)` statement.
const IN_BATCH: usize = 400;
/// Concurrent insert attempts that lose the unique index before a lookup gives up.
const ASSIGN_TRIES: usize = 16;
/// The most ords one [`NsDocs::info`] call reads.
pub const MAX_INFO: usize = 1 << 16;
/// How long the namespace counts stay valid.
const COUNTS_TTL: Duration = Duration::from_secs(30);
/// Bit of `ci_docs.flags` that marks a tombstoned doc (the sweep, `codeintel::sweep`).
const TOMBSTONE: i64 = 128;

/// True when the current runtime can run [`block_in_place`](tokio::task::block_in_place).
pub fn can_block() -> bool {
    Handle::try_current().is_ok_and(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread)
}

/// What the table says about one doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocInfo {
    pub file: FileId,
    /// The byte-decided skip flags (`asc_codeintel::skip::Flags` bits, tombstone bit removed).
    pub flags: u8,
    pub langs: Vec<Lang>,
    /// The worker has analysed the file: the row is not a placeholder.
    pub indexed: bool,
    pub tombstoned: bool,
}

impl DocInfo {
    /// `Some(false)` only when the row is the worker's and the secret flag is clear.
    pub fn secret(&self) -> Option<bool> {
        self.indexed.then(|| self.flags & Flags::SECRET.bits() != 0)
    }
}

/// Counts of one namespace, for the selectivity estimate and `IndexStatus`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub docs: u64,
    pub chunks: u64,
    /// The age of the oldest queued job, in seconds; zero when none waits.
    pub lag_secs: i64,
}

/// The doc table of one namespace.
pub struct NsDocs {
    pool: AnyPool,
    ns: Vec<u8>,
    queue: Queue,
    handle: Handle,
    by_file: quick_cache::sync::Cache<FileId, u32>,
    by_ord: quick_cache::sync::Cache<u32, FileId>,
    counts: std::sync::Mutex<Option<(Instant, Counts)>>,
    /// Files queued by this table: a test and metrics hook.
    queued: AtomicU64,
}

fn sql_err(context: &str, e: &dyn std::fmt::Display) {
    tracing::error!(%context, error = %e, "codeintel doc table error");
}

fn versions_zero() -> Vec<u8> {
    vec![0u8; 12]
}

impl NsDocs {
    /// The table of `ns` (the repository's 16-byte namespace id). Must be built inside a
    /// runtime: the handle is kept for the synchronous bridge.
    pub fn new(pool: AnyPool, ns: &[u8]) -> Self {
        Self {
            queue: Queue::new(pool.clone()),
            pool,
            ns: ns.to_vec(),
            handle: Handle::current(),
            by_file: quick_cache::sync::Cache::new(CACHE_ENTRIES),
            by_ord: quick_cache::sync::Cache::new(CACHE_ENTRIES),
            counts: std::sync::Mutex::new(None),
            queued: AtomicU64::new(0),
        }
    }

    pub fn ns(&self) -> &[u8] {
        &self.ns
    }

    /// Files this table has queued for indexing.
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    fn block<F: Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.handle.block_on(f))
    }

    async fn lookup(&self, id: &FileId) -> Result<Option<u32>, sqlx::Error> {
        if let Some(o) = self.by_file.get(id) {
            return Ok(Some(o));
        }
        let row = sqlx::query(
            "SELECT CAST(ord AS BIGINT) AS ord FROM ci_docs WHERE ns = $1 AND file = $2",
        )
        .bind(self.ns.clone())
        .bind(id.as_bytes().to_vec())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let ord: i64 = row.try_get("ord")?;
        let Ok(ord) = u32::try_from(ord) else {
            return Ok(None);
        };
        self.by_file.insert(*id, ord);
        self.by_ord.insert(ord, *id);
        Ok(Some(ord))
    }

    /// The ord of `id` when the table knows it; unlike [`Self::assign`] this adds nothing.
    pub async fn ord_of(&self, id: &FileId) -> Result<Option<u32>, sqlx::Error> {
        self.lookup(id).await
    }

    /// The ord of `id`, assigning the next dense ord when the table does not know it; the new
    /// doc gets a `file` job at interactive priority.
    pub async fn assign(&self, id: FileId) -> Result<u32, DocError> {
        for _ in 0..ASSIGN_TRIES {
            match self.lookup(&id).await {
                Ok(Some(o)) => return Ok(o),
                Ok(None) => {}
                Err(e) => {
                    sql_err("ord lookup", &e);
                    return Err(DocError::OrdsExhausted);
                }
            }
            // A concurrent insert of the same ord loses on the unique index and is retried.
            let inserted = sqlx::query(
                "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) \
                 VALUES ($1, $2, (SELECT COALESCE(MAX(ord) + 1, 0) FROM ci_docs WHERE ns = $1), 0, 0, $3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(self.ns.clone())
            .bind(id.as_bytes().to_vec())
            .bind(versions_zero())
            .execute(&self.pool)
            .await;
            match inserted {
                Ok(done) if done.rows_affected() > 0 => {
                    let job = NewJob::file(id.as_bytes(), Priority::Interactive);
                    match self.queue.enqueue(&self.ns, &[job], now()).await {
                        Ok(_) => {
                            self.queued.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => sql_err("file job", &e),
                    }
                }
                Ok(_) => {}
                Err(e) => sql_err("ord insert", &e),
            }
        }
        Err(DocError::OrdsExhausted)
    }

    /// The file of `ord`.
    pub async fn file(&self, ord: u32) -> Result<Option<FileId>, sqlx::Error> {
        if let Some(f) = self.by_ord.get(&ord) {
            return Ok(Some(f));
        }
        let row = sqlx::query("SELECT file FROM ci_docs WHERE ns = $1 AND ord = $2")
            .bind(self.ns.clone())
            .bind(i64::from(ord))
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let bytes: Vec<u8> = row.try_get("file")?;
        let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            return Ok(None);
        };
        let id = FileId::from_bytes(arr);
        self.by_ord.insert(ord, id);
        self.by_file.insert(id, ord);
        Ok(Some(id))
    }

    /// Rows and languages of `ords` (at most [`MAX_INFO`]; the rest are not read). An ord the
    /// table does not know is absent from the map.
    pub async fn info(&self, ords: &[u32]) -> Result<HashMap<u32, DocInfo>, sqlx::Error> {
        let mut out: HashMap<u32, DocInfo> = HashMap::new();
        for part in ords
            .iter()
            .take(MAX_INFO)
            .copied()
            .collect::<Vec<_>>()
            .chunks(IN_BATCH)
        {
            let marks: Vec<String> = (0..part.len()).map(|i| format!("${}", i + 2)).collect();
            let sql = format!(
                "SELECT CAST(ord AS BIGINT) AS ord, file, CAST(flags AS BIGINT) AS flags, \
                        CAST(size AS BIGINT) AS size, versions \
                 FROM ci_docs WHERE ns = $1 AND ord IN ({})",
                marks.join(", ")
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(self.ns.clone());
            for o in part {
                q = q.bind(i64::from(*o));
            }
            for r in q.fetch_all(&self.pool).await? {
                let ord: i64 = r.try_get("ord")?;
                let flags: i64 = r.try_get("flags")?;
                let size: i64 = r.try_get("size")?;
                let versions: Vec<u8> = r.try_get("versions")?;
                let file: Vec<u8> = r.try_get("file")?;
                let (Ok(ord), Ok(file)) =
                    (u32::try_from(ord), <[u8; 32]>::try_from(file.as_slice()))
                else {
                    continue;
                };
                let file = FileId::from_bytes(file);
                // The worker writes non-zero extractor versions with every row it analyses.
                let analysed = size > 0 || versions.iter().any(|b| *b != 0);
                out.insert(
                    ord,
                    DocInfo {
                        file,
                        flags: u8::try_from(flags & !TOMBSTONE & 0x7f).unwrap_or(0x7f),
                        langs: Vec::new(),
                        indexed: analysed,
                        tombstoned: flags & TOMBSTONE != 0,
                    },
                );
                self.by_ord.insert(ord, file);
                self.by_file.insert(file, ord);
            }
            let sql = format!(
                "SELECT CAST(ord AS BIGINT) AS ord, lang FROM ci_doc_langs \
                 WHERE ns = $1 AND ord IN ({})",
                marks.join(", ")
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(self.ns.clone());
            for o in part {
                q = q.bind(i64::from(*o));
            }
            for r in q.fetch_all(&self.pool).await? {
                let ord: i64 = r.try_get("ord")?;
                let name: String = r.try_get("lang")?;
                if let (Ok(ord), Some(l)) = (u32::try_from(ord), Lang::from_name(&name))
                    && let Some(d) = out.get_mut(&ord)
                {
                    d.langs.push(l);
                    d.langs.sort_unstable();
                }
            }
        }
        Ok(out)
    }

    /// The ords of `scope` that carry the tombstone bit.
    pub async fn tombstoned(&self, scope: &RoaringBitmap) -> Result<RoaringBitmap, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT CAST(ord AS BIGINT) AS ord FROM ci_docs WHERE ns = $1 AND flags >= $2",
        )
        .bind(self.ns.clone())
        .bind(TOMBSTONE)
        .fetch_all(&self.pool)
        .await?;
        let mut out = RoaringBitmap::new();
        for r in rows {
            let o: i64 = r.try_get("ord")?;
            if let Ok(o) = u32::try_from(o)
                && scope.contains(o)
            {
                out.insert(o);
            }
        }
        Ok(out)
    }

    /// Of the first `limit` ords of `scope` that are not tombstones, the ones with no language
    /// row (no grammar), for the word-boundary fallback of `refs`.
    pub async fn untagged(
        &self,
        scope: &RoaringBitmap,
        limit: usize,
        examine: usize,
    ) -> Result<Vec<u32>, sqlx::Error> {
        let ords: Vec<u32> = scope.iter().take(examine).collect();
        let mut out = Vec::new();
        for part in ords.chunks(IN_BATCH) {
            let marks: Vec<String> = (0..part.len()).map(|i| format!("${}", i + 2)).collect();
            let sql = format!(
                "SELECT DISTINCT CAST(ord AS BIGINT) AS ord FROM ci_doc_langs \
                 WHERE ns = $1 AND ord IN ({})",
                marks.join(", ")
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(self.ns.clone());
            for o in part {
                q = q.bind(i64::from(*o));
            }
            let tagged: std::collections::HashSet<u32> = q
                .fetch_all(&self.pool)
                .await?
                .iter()
                .filter_map(|r| r.try_get::<i64, _>("ord").ok())
                .filter_map(|o| u32::try_from(o).ok())
                .collect();
            for o in part {
                if !tagged.contains(o) {
                    out.push(*o);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Doc, chunk and queue counts of the namespace (cached for [`COUNTS_TTL`]).
    pub async fn counts(&self) -> Result<Counts, sqlx::Error> {
        {
            let g = self.counts.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((at, c)) = *g
                && at.elapsed() < COUNTS_TTL
            {
                return Ok(c);
            }
        }
        let n = |sql: &'static str| {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(self.ns.clone())
                .fetch_one(&self.pool)
        };
        let docs = n("SELECT CAST(COUNT(*) AS BIGINT) FROM ci_docs WHERE ns = $1").await?;
        let chunks = n("SELECT CAST(COUNT(*) AS BIGINT) FROM ci_chunks WHERE ns = $1").await?;
        let oldest: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(MIN(created_at) AS BIGINT) FROM ci_jobs WHERE ns = $1 AND state = 'queued'",
        )
        .bind(self.ns.clone())
        .fetch_one(&self.pool)
        .await?;
        let c = Counts {
            docs: u64::try_from(docs).unwrap_or(0),
            chunks: u64::try_from(chunks).unwrap_or(0),
            lag_secs: oldest.map_or(0, |t| now().saturating_sub(t).max(0)),
        };
        *self.counts.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), c));
        Ok(c)
    }

    /// Queues the files of `ords` at interactive priority, in the background.
    fn enqueue_background(&self, ords: Vec<u32>) {
        let (pool, ns, queue) = (self.pool.clone(), self.ns.clone(), self.queue.clone());
        self.handle.spawn(async move {
            let mut jobs = Vec::new();
            for part in ords.chunks(IN_BATCH) {
                let marks: Vec<String> = (0..part.len()).map(|i| format!("${}", i + 2)).collect();
                let sql = format!(
                    "SELECT file FROM ci_docs WHERE ns = $1 AND ord IN ({})",
                    marks.join(", ")
                );
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.clone());
                for o in part {
                    q = q.bind(i64::from(*o));
                }
                match q.fetch_all(&pool).await {
                    Ok(rows) => {
                        for r in rows {
                            if let Ok(f) = r.try_get::<Vec<u8>, _>("file") {
                                jobs.push(NewJob::file(&f, Priority::Interactive));
                            }
                        }
                    }
                    Err(e) => {
                        sql_err("interactive enqueue", &e);
                        return;
                    }
                }
            }
            if let Err(e) = queue.enqueue(&ns, &jobs, now()).await {
                sql_err("interactive enqueue", &e);
            }
        });
    }
}

impl OrdSource for NsDocs {
    fn ord_for(&self, id: FileId) -> Result<u32, DocError> {
        if let Some(o) = self.by_file.get(&id) {
            return Ok(o);
        }
        self.block(self.assign(id))
    }

    fn get(&self, id: &FileId) -> Option<u32> {
        if let Some(o) = self.by_file.get(id) {
            return Some(o);
        }
        match self.block(self.lookup(id)) {
            Ok(o) => o,
            Err(e) => {
                sql_err("ord lookup", &e);
                None
            }
        }
    }
}

impl DocDirectory for NsDocs {
    fn file_of(&self, ord: u32) -> Option<FileId> {
        if let Some(f) = self.by_ord.get(&ord) {
            return Some(f);
        }
        match self.block(self.file(ord)) {
            Ok(f) => f,
            Err(e) => {
                sql_err("file lookup", &e);
                None
            }
        }
    }

    fn enqueue_interactive(&self, ords: &RoaringBitmap) {
        self.enqueue_background(ords.iter().take(super::queue::MAX_ENQUEUE).collect());
    }
}
