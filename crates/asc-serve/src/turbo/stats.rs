//! The recorder of cache use (hits, misses, puts, bytes served, milliseconds saved) per repository
//! namespace and day, and per change version. The request path only adds to a bounded in-memory
//! map; a background task writes the map to `cache_daily` and `cache_by_rev` (`ci::cache_stats`).
//!
//! The authoriser's answer carries no namespace, so the recorder keeps a small cache from
//! repository name to namespace (4096 names; an answer is valid for 60 s, a "no such repository"
//! answer for 30 s, and at most 1024 of those are kept, so made-up names cannot crowd real ones
//! out). The flusher task fills it, in batches, off the request path. A request for a name the
//! cache does not know yet, or no longer holds, is not counted: the name is queued for the next
//! tick. For a newly created repository the worst case is about 60 s uncounted (a "no such
//! repository" answer of up to 30 s, then up to one 30 s tick); the first request after an answer
//! expires can go uncounted too. A name that is not a repository never takes a daily key. A name
//! lookup cut short by its timeout drops its queue; counting resumes at the next request.
//!
//! For up to 60 s after a rename, requests for the old name, if a new repository takes it, are
//! counted for the renamed repository; after a delete they go to a namespace nobody reads. The
//! admin paths do not reach the recorder (TurboState is not plumbed there), so nothing clears it.
//!
//! A hit is counted when the response starts, so an aborted download still counts. A commit that
//! succeeded but reported failure is written again by the next flush and counted twice (inherent
//! to additive upserts), as is a write cancelled by the flush timeout at the moment it commits.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sqlx::{AnyPool, Row};
use tokio_util::sync::CancellationToken;

use crate::ci::cache_stats::{self, Counters, FLUSH_CHUNK};
use crate::registry::Ns;

/// (namespace, day) keys held at once, at most.
pub(super) const MAX_DAILY_KEYS: usize = 4096;
/// Change versions held per repository, at most.
pub(super) const MAX_REVS_PER_REPO: usize = 64;
/// Change versions held in all, at most.
const MAX_REV_KEYS: usize = MAX_DAILY_KEYS;
/// Names the name-to-namespace cache holds (and names it has queued), at most.
const MAX_NAMES: usize = 4096;
/// How long a name-to-namespace answer is used.
const NAME_TTL: Duration = Duration::from_secs(60);
/// How long a "no such repository" answer is used.
const NAME_NEG_TTL: Duration = Duration::from_secs(30);
/// "No such repository" answers kept, at most.
const MAX_NEG_NAMES: usize = 1024;
/// A used answer older than this is looked up again on the next tick.
const NAME_REFRESH: Duration = Duration::from_secs(30);
/// Names or change versions per lookup statement, at most.
const LOOKUP_CHUNK: usize = 500;
const DAY: i64 = 86_400;
const FLUSH_EVERY: Duration = Duration::from_secs(30);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
const FLUSH_LIMIT: Duration = Duration::from_secs(20);
const FINAL_FLUSH: Duration = Duration::from_secs(5);

/// One counted request.
#[derive(Clone, Copy, Debug)]
pub(super) enum Event {
    Hit { bytes: u64, saved_ms: u64 },
    Miss,
    Put,
}

impl Event {
    fn counters(self) -> Counters {
        match self {
            Event::Hit { bytes, saved_ms } => Counters {
                hits: 1,
                bytes_served: bytes,
                saved_ms,
                ..Counters::default()
            },
            Event::Miss => Counters {
                misses: 1,
                ..Counters::default()
            },
            Event::Put => Counters {
                puts: 1,
                ..Counters::default()
            },
        }
    }
}

type Daily = (Ns, i64, Counters);
type Rev = (Ns, [u8; 32], Counters);

#[derive(Default)]
struct Inner {
    daily: HashMap<(Ns, i64), Counters>,
    revs: HashMap<(Ns, [u8; 32]), Counters>,
    per_repo: HashMap<Ns, usize>,
}

impl Inner {
    fn add_daily(&mut self, ns: Ns, day: i64, c: &Counters, dropped: &AtomicU64) {
        let full = self.daily.len() >= MAX_DAILY_KEYS;
        if let Some(row) = self.daily.get_mut(&(ns, day)) {
            row.add(c);
        } else if full {
            dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.daily.insert((ns, day), *c);
        }
    }

    /// A change version over a cap is not kept; its event is in the daily row all the same.
    fn add_rev(&mut self, ns: Ns, cs: [u8; 32], c: &Counters, rev_dropped: &AtomicU64) {
        if let Some(row) = self.revs.get_mut(&(ns, cs)) {
            row.add(c);
            return;
        }
        let held = self.per_repo.get(&ns).copied().unwrap_or(0);
        if held >= MAX_REVS_PER_REPO || self.revs.len() >= MAX_REV_KEYS {
            rev_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.revs.insert((ns, cs), *c);
        *self.per_repo.entry(ns).or_default() += 1;
    }
}

struct NameEntry {
    ns: Option<Ns>,
    at: Instant,
    used: bool,
}

impl NameEntry {
    /// A "no such repository" answer lives half as long, so a new repository is found sooner.
    fn fresh(&self, now: Instant) -> bool {
        let ttl = if self.ns.is_some() {
            NAME_TTL
        } else {
            NAME_NEG_TTL
        };
        now.saturating_duration_since(self.at) < ttl
    }
}

/// Repository name to namespace, as last read from `repos`.
#[derive(Default)]
struct Names {
    cache: HashMap<String, NameEntry>,
    wanted: HashSet<String>,
    /// Entries of `cache` that say "no such repository".
    neg: usize,
}

impl Names {
    /// The namespace of `repo` when a fresh answer says it exists. Otherwise queues the name
    /// (when there is room) and answers none.
    fn lookup(&mut self, repo: &str, now: Instant) -> Option<Ns> {
        if let Some(e) = self.cache.get_mut(repo)
            && e.fresh(now)
        {
            e.used = true;
            return e.ns;
        }
        if self.wanted.len() < MAX_NAMES && !self.wanted.contains(repo) {
            self.wanted.insert(repo.to_owned());
        }
        None
    }

    /// The names to look up now: the queued ones and the used ones that are getting old.
    fn take_refresh(&mut self, now: Instant) -> Vec<String> {
        self.cache.retain(|_, e| e.fresh(now));
        self.neg = self.cache.values().filter(|e| e.ns.is_none()).count();
        let mut out: Vec<String> = self.wanted.drain().collect();
        for (name, e) in &mut self.cache {
            if e.used && e.ns.is_some() && now.saturating_duration_since(e.at) >= NAME_REFRESH {
                e.used = false;
                out.push(name.clone());
            }
        }
        out
    }

    fn fill(&mut self, name: String, ns: Option<Ns>, now: Instant) {
        let known = self.cache.get(&name).map(|e| e.ns.is_none());
        if known.is_none() && self.cache.len() >= MAX_NAMES {
            // Full: made-up names must not crowd out real ones. A real answer evicts a negative
            // one; a negative answer is dropped.
            let victim = if ns.is_some() && self.neg > 0 {
                self.cache
                    .iter()
                    .find(|(_, e)| e.ns.is_none())
                    .map(|(k, _)| k.clone())
            } else {
                None
            };
            let Some(victim) = victim else { return };
            self.cache.remove(&victim);
            self.neg -= 1;
        }
        if ns.is_none() && known != Some(true) && self.neg >= MAX_NEG_NAMES {
            return;
        }
        if known == Some(true) {
            self.neg -= 1;
        }
        if ns.is_none() {
            self.neg += 1;
        }
        self.cache.insert(
            name,
            NameEntry {
                ns,
                at: now,
                used: false,
            },
        );
    }

    fn retry(&mut self, names: &[String]) {
        for n in names {
            if self.wanted.len() < MAX_NAMES {
                self.wanted.insert(n.clone());
            }
        }
    }
}

/// What one flush round holds, so that a round cut short knows what is written.
struct Work {
    /// Sorted rows, written in chunks.
    daily: Vec<Daily>,
    done_daily: usize,
    /// Change rows not yet checked against `change_versions`.
    /// A chunk leaves here only after its check returns, so a cut loses no row.
    pending: Vec<(Ns, Vec<Rev>)>,
    /// Change rows that exist, sorted, written in chunks.
    ready: Vec<Rev>,
    done_ready: usize,
    /// Change rows whose check failed.
    retry_revs: Vec<Rev>,
}

impl Work {
    fn new(mut daily: Vec<Daily>, revs: Vec<Rev>) -> Self {
        daily.sort_by(|a, b| (a.0.as_bytes(), a.1).cmp(&(b.0.as_bytes(), b.1)));
        let mut by_ns: HashMap<Ns, Vec<Rev>> = HashMap::new();
        for r in revs {
            by_ns.entry(r.0).or_default().push(r);
        }
        Self {
            daily,
            done_daily: 0,
            pending: by_ns.into_iter().collect(),
            ready: Vec::new(),
            done_ready: 0,
            retry_revs: Vec::new(),
        }
    }

    /// The rows not known written, to be counted again next round. A rev row that does not exist
    /// is gone for good.
    fn leftovers(&mut self) -> (Vec<Daily>, Vec<Rev>) {
        let daily = self.daily.split_off(self.done_daily.min(self.daily.len()));
        let mut revs: Vec<Rev> = std::mem::take(&mut self.pending)
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .collect();
        revs.extend(self.ready.split_off(self.done_ready.min(self.ready.len())));
        revs.append(&mut self.retry_revs);
        (daily, revs)
    }
}

/// The counts not yet written.
#[derive(Default)]
pub(super) struct Recorder {
    inner: Mutex<Inner>,
    names: Mutex<Names>,
    /// Events with no daily row and no room for one.
    dropped: AtomicU64,
    /// Events with no change row and no room for one.
    rev_dropped: AtomicU64,
}

impl Recorder {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_names(&self) -> std::sync::MutexGuard<'_, Names> {
        self.names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Counts `ev` for repository `repo` on the day of `now`, and for change version `rev` when
    /// given. Constant time; never fails. A repository the name cache does not know is not
    /// counted.
    pub(super) fn record(&self, repo: &str, now: i64, rev: Option<[u8; 32]>, ev: Event) {
        let Some(ns) = self.lock_names().lookup(repo, Instant::now()) else {
            return;
        };
        let c = ev.counters();
        let mut g = self.lock();
        g.add_daily(ns, now - now.rem_euclid(DAY), &c, &self.dropped);
        if let Some(cs) = rev {
            g.add_rev(ns, cs, &c, &self.rev_dropped);
        }
    }

    /// Rows held: (namespace-day rows, change rows).
    #[cfg(test)]
    pub(super) fn sizes(&self) -> (usize, usize) {
        let g = self.lock();
        (g.daily.len(), g.revs.len())
    }

    /// The sum of all daily rows held.
    #[cfg(test)]
    pub(super) fn totals(&self) -> Counters {
        let mut t = Counters::default();
        for c in self.lock().daily.values() {
            t.add(c);
        }
        t
    }

    /// Tells the cache that `name` is repository `ns`.
    #[cfg(test)]
    pub(super) fn know(&self, name: &str, ns: Ns) {
        self.lock_names()
            .fill(name.to_owned(), Some(ns), Instant::now());
    }

    fn take(&self) -> (Vec<Daily>, Vec<Rev>) {
        let mut g = self.lock();
        let g = std::mem::take(&mut *g);
        (
            g.daily.into_iter().map(|((n, d), c)| (n, d, c)).collect(),
            g.revs.into_iter().map(|((n, s), c)| (n, s, c)).collect(),
        )
    }

    /// Puts rows back after a failed or cut short write, under the same caps.
    fn restore(&self, daily: Vec<Daily>, revs: Vec<Rev>) {
        let mut g = self.lock();
        for (ns, day, c) in &daily {
            g.add_daily(*ns, *day, c, &self.dropped);
        }
        for (ns, cs, c) in &revs {
            g.add_rev(*ns, *cs, c, &self.rev_dropped);
        }
    }

    /// Events lost to the caps since the last call, for the log.
    fn take_dropped(&self) -> (u64, u64) {
        (
            self.dropped.swap(0, Ordering::Relaxed),
            self.rev_dropped.swap(0, Ordering::Relaxed),
        )
    }

    /// Looks up the queued names and the used names that are getting old, in batches.
    pub(super) async fn refresh_names(&self, pool: &AnyPool) {
        let names = self.lock_names().take_refresh(Instant::now());
        for chunk in names.chunks(LOOKUP_CHUNK) {
            match lookup_names(pool, chunk).await {
                Ok(found) => {
                    let now = Instant::now();
                    let mut g = self.lock_names();
                    for n in chunk {
                        g.fill(n.clone(), found.get(n).copied(), now);
                    }
                }
                Err(e) => {
                    tracing::warn!(event = "turbo.cache_stats_lookup_failed", error = %e);
                    self.lock_names().retry(chunk);
                }
            }
        }
    }

    /// Writes everything counted so far, within `limit`. Rows not known written (after an
    /// error or the limit) stay for the next round.
    pub(super) async fn flush(&self, pool: &AnyPool, now: i64, limit: Duration) {
        let (dropped, rev_dropped) = self.take_dropped();
        if dropped + rev_dropped > 0 {
            tracing::warn!(
                event = "turbo.cache_stats_dropped",
                dropped,
                rev_dropped,
                "cache counts over the caps were not kept"
            );
        }
        let (daily, revs) = self.take();
        if daily.is_empty() && revs.is_empty() {
            return;
        }
        let mut work = Work::new(daily, revs);
        if tokio::time::timeout(limit, write(pool, &mut work, now))
            .await
            .is_err()
        {
            tracing::warn!(event = "turbo.cache_stats_flush_timed_out");
        }
        let (daily, revs) = work.leftovers();
        if !daily.is_empty() || !revs.is_empty() {
            self.restore(daily, revs);
        }
    }

    /// Flushes every [`FLUSH_EVERY`] (within [`FLUSH_LIMIT`]) and prunes every [`PRUNE_EVERY`]
    /// until `stop` is cancelled, then flushes once more within [`FINAL_FLUSH`].
    pub(super) async fn run(&self, pool: AnyPool, stop: CancellationToken, now: fn() -> i64) {
        let mut tick =
            tokio::time::interval_at(tokio::time::Instant::now() + FLUSH_EVERY, FLUSH_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_prune = tokio::time::Instant::now();
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                _ = tick.tick() => {}
            }
            if tokio::time::timeout(FLUSH_LIMIT, self.refresh_names(&pool))
                .await
                .is_err()
            {
                tracing::warn!(event = "turbo.cache_stats_lookup_timed_out");
            }
            self.flush(&pool, now(), FLUSH_LIMIT).await;
            if last_prune.elapsed() >= PRUNE_EVERY {
                last_prune = tokio::time::Instant::now();
                match tokio::time::timeout(FLUSH_LIMIT, cache_stats::prune(&pool, now())).await {
                    Ok(Ok(p)) if p.days + p.revs > 0 => tracing::info!(
                        event = "turbo.cache_stats_pruned",
                        days = p.days,
                        revs = p.revs
                    ),
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(event = "turbo.cache_stats_prune_failed", error = %e);
                    }
                    Err(_) => tracing::warn!(event = "turbo.cache_stats_prune_timed_out"),
                }
            }
        }
        self.flush(&pool, now(), FINAL_FLUSH).await;
    }
}

/// `($1, $2, ...)` placeholders from `first` for `n` values.
fn placeholders(first: usize, n: usize) -> String {
    (first..first + n)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The namespaces of the repositories among `names` (at most [`LOOKUP_CHUNK`]).
async fn lookup_names(
    pool: &AnyPool,
    names: &[String],
) -> Result<HashMap<String, Ns>, sqlx::Error> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT name, ns FROM repos WHERE name IN ({})",
        placeholders(1, names.len())
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for n in names {
        q = q.bind(n.clone());
    }
    let mut out = HashMap::new();
    for r in q.fetch_all(pool).await? {
        let name: String = r.try_get("name")?;
        let ns: Vec<u8> = r.try_get("ns")?;
        if let Some(ns) = Ns::from_bytes(&ns) {
            out.insert(name, ns);
        }
    }
    Ok(out)
}

/// The change versions among `cs` (at most [`LOOKUP_CHUNK`]) that `change_versions` holds in `ns`
/// (the condition of `cache_stats::resolve_rev`, for many at once).
async fn existing_revs(
    pool: &AnyPool,
    ns: Ns,
    cs: &[[u8; 32]],
) -> Result<HashSet<[u8; 32]>, sqlx::Error> {
    if cs.is_empty() {
        return Ok(HashSet::new());
    }
    let sql = format!(
        "SELECT DISTINCT cs FROM change_versions WHERE ns = $1 AND cs IN ({})",
        placeholders(2, cs.len())
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.as_bytes().to_vec());
    for c in cs {
        q = q.bind(c.to_vec());
    }
    let mut out = HashSet::new();
    for r in q.fetch_all(pool).await? {
        let v: Vec<u8> = r.try_get("cs")?;
        if let Ok(a) = <[u8; 32]>::try_from(v) {
            out.insert(a);
        }
    }
    Ok(out)
}

/// Writes the daily rows, then the change rows that exist in their namespace, a chunk at a time.
/// `work` says after each step what is written. A failed write stops the round.
async fn write(pool: &AnyPool, work: &mut Work, now: i64) {
    while work.done_daily < work.daily.len() {
        let end = (work.done_daily + FLUSH_CHUNK).min(work.daily.len());
        if let Err(e) = cache_stats::flush(pool, &work.daily[work.done_daily..end], &[], now).await
        {
            tracing::warn!(event = "turbo.cache_stats_flush_failed", error = %e);
            return;
        }
        work.done_daily = end;
    }
    // Resolve the change versions, one statement per namespace and chunk.
    // A chunk moves only after its lookup returns; nothing is awaited in between.
    while let Some((ns, rows)) = work.pending.last() {
        let ns = *ns;
        let take = rows.len().min(LOOKUP_CHUNK);
        let keys: Vec<[u8; 32]> = rows[rows.len() - take..].iter().map(|r| r.1).collect();
        let found = existing_revs(pool, ns, &keys).await;
        let Some((_, rows)) = work.pending.last_mut() else {
            break;
        };
        let chunk = rows.split_off(rows.len() - take);
        if rows.is_empty() {
            work.pending.pop();
        }
        match found {
            Ok(found) => work
                .ready
                .extend(chunk.into_iter().filter(|r| found.contains(&r.1))),
            Err(e) => {
                tracing::warn!(event = "turbo.cache_stats_rev_lookup_failed", error = %e);
                work.retry_revs.extend(chunk);
            }
        }
    }
    work.ready
        .sort_by(|a, b| (a.0.as_bytes(), a.1).cmp(&(b.0.as_bytes(), b.1)));
    while work.done_ready < work.ready.len() {
        let end = (work.done_ready + FLUSH_CHUNK).min(work.ready.len());
        let rows: Vec<(Ns, Vec<u8>, Counters)> = work.ready[work.done_ready..end]
            .iter()
            .map(|(ns, cs, c)| (*ns, cs.to_vec(), *c))
            .collect();
        if let Err(e) = cache_stats::flush(pool, &[], &rows, now).await {
            tracing::warn!(event = "turbo.cache_stats_flush_failed", error = %e);
            return;
        }
        work.done_ready = end;
    }
}

/// The change version of a request: the `x-asc-rev` header when it is exactly 64 lowercase hex
/// characters, else none. With several such headers the first one counts.
pub(super) fn rev_of(headers: &axum::http::HeaderMap) -> Option<[u8; 32]> {
    let v = headers.get("x-asc-rev")?.to_str().ok()?;
    if v.len() != 64 || !super::rev_ok(v) {
        return None;
    }
    let mut out = [0u8; 32];
    for (o, p) in out.iter_mut().zip(v.as_bytes().chunks(2)) {
        let nib = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        *o = nib(p[0]) << 4 | nib(p[1]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use super::*;
    use crate::migrate;

    const T: i64 = 5 * DAY + 77;
    const LIMIT: Duration = Duration::from_secs(20);

    async fn pool() -> AnyPool {
        let p = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&p).await.unwrap();
        p
    }

    async fn repo(p: &AnyPool, r: &Recorder, name: &str) -> Ns {
        let ns = Ns::random();
        sqlx::query("INSERT INTO repos (name, ns, created) VALUES ($1, $2, 0)")
            .bind(name.to_owned())
            .bind(ns.as_bytes().to_vec())
            .execute(p)
            .await
            .unwrap();
        r.know(name, ns);
        ns
    }

    async fn change(p: &AnyPool, ns: Ns, cs: u8) {
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
             VALUES ($1, $2, 1, $3, NULL, 0, 'x', $4)",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(vec![3u8; 16])
        .bind(vec![cs; 32])
        .bind(vec![4u8; 16])
        .execute(p)
        .await
        .unwrap();
    }

    fn hit(bytes: u64) -> Event {
        Event::Hit { bytes, saved_ms: 7 }
    }

    #[test]
    fn rev_of_takes_only_64_lowercase_hex_and_the_first_header() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(rev_of(&h), None);
        for bad in [
            "",
            "abc",
            &"A".repeat(64),
            &"g".repeat(64),
            &"a".repeat(40),
            &"a".repeat(65),
        ] {
            h.insert("x-asc-rev", bad.parse().unwrap());
            assert_eq!(rev_of(&h), None, "{bad}");
        }
        h.insert(
            "x-asc-rev",
            format!("{}1f", "0".repeat(62)).parse().unwrap(),
        );
        let mut want = [0u8; 32];
        want[31] = 0x1f;
        assert_eq!(rev_of(&h), Some(want));
        h.append("x-asc-rev", "ab".repeat(32).parse().unwrap());
        assert_eq!(rev_of(&h), Some(want), "the first header wins");
    }

    #[test]
    fn unknown_names_take_no_daily_key_and_the_queue_is_bounded() {
        let r = Recorder::default();
        for i in 0..MAX_NAMES + 10 {
            r.record(&format!("o/junk{i}"), T, None, Event::Miss);
        }
        assert_eq!(r.sizes(), (0, 0));
        assert_eq!(r.lock_names().wanted.len(), MAX_NAMES);
    }

    #[test]
    fn made_up_names_cannot_keep_real_ones_out_and_expire_in_30_s() {
        let t0 = Instant::now();
        let mut n = Names::default();
        for i in 0..MAX_NAMES {
            n.fill(format!("o/none{i}"), None, t0);
        }
        assert_eq!(n.neg, MAX_NEG_NAMES);
        assert_eq!(n.cache.len(), MAX_NEG_NAMES);
        // Real names still fit, and fill the cache up to its size.
        let ns = Ns::random();
        for i in 0..MAX_NAMES - MAX_NEG_NAMES {
            n.fill(format!("o/real{i}"), Some(ns), t0);
        }
        assert_eq!(n.cache.len(), MAX_NAMES);
        // When full, a real name evicts a negative one; a negative one is dropped.
        n.fill("o/late".into(), Some(ns), t0);
        assert_eq!(n.lookup("o/late", t0), Some(ns));
        assert_eq!(n.cache.len(), MAX_NAMES);
        assert_eq!(n.neg, MAX_NEG_NAMES - 1);
        n.fill("o/more-none".into(), None, t0);
        assert!(!n.cache.contains_key("o/more-none"));
        // A negative answer is used for under 30 s.
        assert_eq!(n.lookup("o/none0", t0 + Duration::from_secs(31)), None);
        assert!(n.wanted.contains("o/none0") || !n.cache.contains_key("o/none0"));
        let mut m = Names::default();
        m.fill("o/x".into(), None, t0);
        m.lookup("o/x", t0 + Duration::from_secs(29));
        assert!(m.wanted.is_empty());
        m.lookup("o/x", t0 + Duration::from_secs(31));
        assert!(m.wanted.contains("o/x"));
    }

    #[test]
    fn a_name_answer_expires_and_a_negative_answer_is_kept() {
        let mut n = Names::default();
        let t0 = Instant::now();
        let ns = Ns::random();
        n.fill("o/r".into(), Some(ns), t0);
        n.fill("o/none".into(), None, t0);
        assert_eq!(n.lookup("o/r", t0 + Duration::from_secs(59)), Some(ns));
        assert_eq!(n.lookup("o/none", t0 + Duration::from_secs(29)), None);
        assert!(n.wanted.is_empty());
        assert_eq!(n.lookup("o/r", t0 + Duration::from_secs(61)), None);
        assert!(n.wanted.contains("o/r"));
        // A used, ageing answer is looked up again; an unused one is not.
        let mut n = Names::default();
        n.fill("o/used".into(), Some(ns), t0);
        n.fill("o/idle".into(), Some(ns), t0);
        n.lookup("o/used", t0);
        let got = n.take_refresh(t0 + Duration::from_secs(31));
        assert_eq!(got, vec!["o/used".to_owned()]);
    }

    #[tokio::test]
    async fn the_flusher_learns_a_repository_in_batches() {
        let r = Recorder::default();
        let p = pool().await;
        let ns = Ns::random();
        sqlx::query("INSERT INTO repos (name, ns, created) VALUES ('o/r', $1, 0)")
            .bind(ns.as_bytes().to_vec())
            .execute(&p)
            .await
            .unwrap();
        r.record("o/r", T, None, Event::Miss);
        r.record("o/ghost", T, None, Event::Miss);
        assert_eq!(r.sizes(), (0, 0));
        r.refresh_names(&p).await;
        r.record("o/r", T, None, Event::Miss);
        r.record("o/ghost", T, None, Event::Miss);
        assert_eq!(r.sizes(), (1, 0));
        assert_eq!(r.lock_names().cache["o/ghost"].ns, None);
        // More names than one statement holds.
        for i in 0..LOOKUP_CHUNK + 5 {
            r.record(&format!("o/n{i}"), T, None, Event::Miss);
        }
        r.refresh_names(&p).await;
        assert_eq!(r.lock_names().cache.len(), LOOKUP_CHUNK + 5 + 2);
    }

    #[test]
    fn the_daily_map_is_capped_and_a_known_key_still_counts() {
        let r = Recorder::default();
        let first = Ns::random();
        r.know("o/first", first);
        for i in 0..MAX_DAILY_KEYS as i64 - 1 {
            r.record("o/first", i * DAY, None, Event::Miss);
        }
        let other = Ns::random();
        r.know("o/other", other);
        r.record("o/other", 0, None, Event::Miss);
        r.record("o/other", DAY, None, Event::Miss);
        r.record("o/first", 0, None, Event::Put);
        assert_eq!(r.take_dropped(), (1, 0));
        let (daily, _) = r.take();
        assert_eq!(daily.len(), MAX_DAILY_KEYS);
        let row = daily.iter().find(|d| d.0 == first && d.1 == 0).unwrap();
        assert_eq!((row.2.misses, row.2.puts), (1, 1));
    }

    #[test]
    fn restoring_into_a_full_map_raises_the_dropped_counter() {
        let r = Recorder::default();
        let ns = Ns::random();
        r.know("o/r", ns);
        for i in 0..MAX_DAILY_KEYS as i64 {
            r.record("o/r", i * DAY, None, Event::Miss);
        }
        let c = Counters {
            hits: 1,
            ..Counters::default()
        };
        r.restore(
            vec![(ns, 0, c), (ns, MAX_DAILY_KEYS as i64 * DAY, c)],
            vec![],
        );
        assert_eq!(r.take_dropped(), (1, 0));
        let (daily, _) = r.take();
        assert_eq!(daily.len(), MAX_DAILY_KEYS);
        assert_eq!(daily.iter().find(|d| d.1 == 0).unwrap().2.hits, 1);
    }

    #[test]
    fn revs_are_capped_per_repository_and_the_event_stays_in_the_daily_row() {
        let r = Recorder::default();
        let (a, b) = (Ns::random(), Ns::random());
        r.know("o/r", a);
        r.know("o/other", b);
        for i in 0..=MAX_REVS_PER_REPO {
            r.record("o/r", T, Some([i as u8; 32]), Event::Miss);
        }
        r.record("o/r", T, Some([0; 32]), Event::Put);
        r.record("o/other", T, Some([200; 32]), Event::Miss);
        assert_eq!(r.take_dropped(), (0, 1));
        let (daily, revs) = r.take();
        assert_eq!(revs.iter().filter(|x| x.0 == a).count(), MAX_REVS_PER_REPO);
        assert_eq!(revs.iter().filter(|x| x.0 == b).count(), 1);
        let d = daily.iter().find(|d| d.0 == a).unwrap();
        assert_eq!((d.2.misses, d.2.puts), (MAX_REVS_PER_REPO as u64 + 1, 1));
        let z = revs.iter().find(|x| x.0 == a && x.1 == [0; 32]).unwrap();
        assert_eq!((z.2.misses, z.2.puts), (1, 1));
    }

    #[test]
    fn leftovers_are_exactly_the_rows_not_known_written() {
        let ns = Ns::random();
        let c = |h| Counters {
            hits: h,
            ..Counters::default()
        };
        let daily = vec![(ns, 3, c(1)), (ns, 1, c(2)), (ns, 2, c(8))];
        let revs = vec![(ns, [2; 32], c(1)), (ns, [1; 32], c(2))];
        for done in 0..=3 {
            let mut w = Work::new(daily.clone(), revs.clone());
            w.done_daily = done;
            let (d, r) = w.leftovers();
            assert_eq!(d.len(), 3 - done);
            assert_eq!(r.len(), 2);
        }
        let mut w = Work::new(daily, revs);
        w.done_daily = 3;
        w.ready = std::mem::take(&mut w.pending)
            .into_iter()
            .flat_map(|p| p.1)
            .collect();
        w.done_ready = 1;
        w.retry_revs.push((ns, [9; 32], c(5)));
        let (d, r) = w.leftovers();
        assert!(d.is_empty());
        assert_eq!(r.iter().map(|x| x.2.hits).sum::<u64>(), 7);
        assert_eq!(r.len(), 2);
    }

    #[tokio::test]
    async fn a_rev_lookup_cut_short_loses_and_doubles_no_row() {
        let p = pool().await;
        let (a, b) = (Ns::random(), Ns::random());
        let c = Counters {
            hits: 1,
            ..Counters::default()
        };
        let mut revs = Vec::new();
        for (ns, n) in [(a, LOOKUP_CHUNK + 200), (b, 5)] {
            for i in 0..n {
                let mut cs = [0u8; 32];
                cs[..4].copy_from_slice(&(i as u32).to_be_bytes());
                let mut id = vec![3u8; 16];
                id[..4].copy_from_slice(&(i as u32).to_be_bytes());
                sqlx::query(
                    "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id)                      VALUES ($1, $2, 1, $3, NULL, 0, 'x', $2)",
                )
                .bind(ns.as_bytes().to_vec())
                .bind(id)
                .bind(cs.to_vec())
                .execute(&p)
                .await
                .unwrap();
                revs.push((ns, cs, c));
            }
        }
        let total = revs.len();
        for limit in [
            Duration::ZERO,
            Duration::from_micros(400),
            Duration::from_millis(4),
            Duration::from_secs(20),
        ] {
            let mut w = Work::new(Vec::new(), revs.clone());
            let _ = tokio::time::timeout(limit, write(&p, &mut w, 1)).await;
            let held = w.ready.len()
                + w.retry_revs.len()
                + w.pending.iter().map(|g| g.1.len()).sum::<usize>();
            assert_eq!(held, total, "{limit:?}");
            let done = w.done_ready;
            let (_, left) = w.leftovers();
            assert_eq!(left.len() + done, total, "{limit:?}");
            let keys: HashSet<_> = left.iter().map(|r| (r.0, r.1)).collect();
            assert_eq!(keys.len(), left.len(), "no row twice");
        }
    }

    #[tokio::test]
    async fn a_failed_flush_keeps_the_rows_and_a_later_one_writes_them_once() {
        let r = Recorder::default();
        let p = pool().await;
        let ns = repo(&p, &r, "o/r").await;
        r.record("o/r", T, None, hit(10));
        p.close().await;
        r.flush(&p, T, LIMIT).await;
        r.record("o/r", T, None, hit(10));
        // The row of the failed flush is back, with the new event added once.
        assert_eq!(r.lock().daily[&(ns, 5 * DAY)].hits, 2);
        let p2 = pool().await;
        sqlx::query("INSERT INTO repos (name, ns, created) VALUES ('o/r', $1, 0)")
            .bind(ns.as_bytes().to_vec())
            .execute(&p2)
            .await
            .unwrap();
        r.flush(&p2, T, LIMIT).await;
        let (t, _) = cache_stats::summary(&p2, ns, 0, 9 * DAY).await.unwrap();
        assert_eq!(t.hits, 2);
        assert_eq!(r.sizes(), (0, 0));
    }

    #[tokio::test]
    async fn a_failed_rev_check_keeps_the_rev_row_and_the_daily_row_is_written() {
        let r = Recorder::default();
        let p = pool().await;
        let ns = repo(&p, &r, "o/r").await;
        change(&p, ns, 4).await;
        r.record("o/r", T, Some([4; 32]), hit(5));
        sqlx::query("ALTER TABLE change_versions RENAME TO change_versions_gone")
            .execute(&p)
            .await
            .unwrap();
        r.flush(&p, T, LIMIT).await;
        let (t, _) = cache_stats::summary(&p, ns, 0, 9 * DAY).await.unwrap();
        assert_eq!(t.hits, 1);
        assert_eq!(r.sizes(), (0, 1));
        sqlx::query("ALTER TABLE change_versions_gone RENAME TO change_versions")
            .execute(&p)
            .await
            .unwrap();
        r.flush(&p, T, LIMIT).await;
        assert_eq!(
            cache_stats::for_rev(&p, ns, &[4; 32])
                .await
                .unwrap()
                .map(|c| c.hits),
            Some(1)
        );
        assert_eq!(r.sizes(), (0, 0));
    }

    #[tokio::test]
    async fn flush_writes_only_revs_that_exist_in_the_same_repository() {
        let r = Recorder::default();
        let p = pool().await;
        let ns = repo(&p, &r, "o/r").await;
        let other = repo(&p, &r, "o/s").await;
        change(&p, ns, 1).await;
        change(&p, other, 2).await;
        r.record("o/r", T, Some([1; 32]), hit(5));
        r.record("o/r", T, Some([2; 32]), hit(5));
        r.record("o/r", T, Some([3; 32]), hit(5));
        r.flush(&p, T, LIMIT).await;
        let (t, days) = cache_stats::summary(&p, ns, 0, 9 * DAY).await.unwrap();
        assert_eq!(
            (t.hits, t.bytes_served, t.saved_ms, days.len()),
            (3, 15, 21, 1)
        );
        assert_eq!(days[0].0, 5 * DAY);
        let got = |ns, cs: u8| {
            let p = p.clone();
            async move { cache_stats::for_rev(&p, ns, &[cs; 32]).await.unwrap() }
        };
        assert_eq!(got(ns, 1).await.map(|c| c.hits), Some(1));
        assert_eq!(got(ns, 2).await, None);
        assert_eq!(got(ns, 3).await, None);
        assert_eq!(got(other, 2).await, None);
        assert_eq!(r.sizes(), (0, 0));
    }

    #[tokio::test]
    async fn a_flush_cut_short_by_its_limit_restores_what_is_not_written() {
        let r = Recorder::default();
        let p = pool().await;
        let ns = repo(&p, &r, "o/r").await;
        r.record("o/r", T, None, hit(5));
        r.flush(&p, T, Duration::ZERO).await;
        let (t, _) = cache_stats::summary(&p, ns, 0, 9 * DAY).await.unwrap();
        // Nothing was written and the row is back, or it was written and is not. A write cut at
        // the instant it commits is both (the documented double count), never neither.
        let total = t.hits + r.lock().daily.values().map(|c| c.hits).sum::<u64>();
        assert!((1..=2).contains(&total), "{total}");
    }

    #[tokio::test]
    async fn recording_during_a_flush_loses_nothing() {
        let r = Arc::new(Recorder::default());
        let p = pool().await;
        let ns = repo(&p, &r, "o/r").await;
        const N: u64 = 2000;
        let writer = {
            let r = r.clone();
            tokio::spawn(async move {
                for i in 0..N {
                    r.record("o/r", T, None, Event::Put);
                    if i % 100 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        };
        let flusher = {
            let (r, p) = (r.clone(), p.clone());
            tokio::spawn(async move {
                for _ in 0..20 {
                    r.flush(&p, T, LIMIT).await;
                    tokio::task::yield_now().await;
                }
            })
        };
        writer.await.unwrap();
        flusher.await.unwrap();
        r.flush(&p, T, LIMIT).await;
        let (t, _) = cache_stats::summary(&p, ns, 0, 9 * DAY).await.unwrap();
        assert_eq!(t.puts, N);
    }

    #[tokio::test]
    async fn shutdown_does_one_final_flush() {
        let r = Arc::new(Recorder::default());
        let p = pool().await;
        let ns = repo(&p, &r, "o/r").await;
        r.record("o/r", T, None, Event::Put);
        let stop = CancellationToken::new();
        let task = tokio::spawn({
            let (r, p, stop) = (r.clone(), p.clone(), stop.clone());
            async move { r.run(p, stop, || T).await }
        });
        stop.cancel();
        task.await.unwrap();
        let (t, _) = cache_stats::summary(&p, ns, 0, 9 * DAY).await.unwrap();
        assert_eq!(t.puts, 1);
    }
}
