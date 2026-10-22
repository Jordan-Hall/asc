//! Admission-control primitives (SPEC §16.3): token buckets, keyed limiters and keyed
//! concurrency counters. Everything is synchronous and non-blocking, so an overloaded server
//! sheds a request at once instead of queueing it.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A refused request and when to try again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("over limit; retry after {} ms", .retry_after.as_millis())]
pub struct Shed {
    pub retry_after: Duration,
}

impl Shed {
    pub fn after(retry_after: Duration) -> Self {
        Self { retry_after }
    }
}

/// Distinct keys one limiter tracks before a full table applies its [`OnFull`] policy (bounds
/// memory).
pub const MAX_KEYS: usize = 100_000;
/// Retry hint for a refused concurrency slot.
const SLOT_RETRY: Duration = Duration::from_millis(100);
/// A full table drops its idle buckets at most this often, so a flood of new keys cannot make
/// every insert scan the table.
const PRUNE_EVERY: Duration = Duration::from_secs(1);

/// A token bucket refilling `rate` tokens per second up to `burst`.
#[derive(Clone, Debug)]
pub struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket. Zero `rate` or `burst` are treated as 1.
    pub fn new(rate: u64, burst: u64, now: Instant) -> Self {
        let capacity = burst.max(1) as f64;
        Self {
            rate: rate.max(1) as f64,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate).min(self.capacity);
        self.last = self.last.max(now);
    }

    /// Takes `cost` tokens (capped at the burst size) or says when enough will exist.
    pub fn try_take(&mut self, cost: u64, now: Instant) -> Result<(), Shed> {
        self.refill(now);
        let cost = (cost as f64).min(self.capacity);
        if self.tokens >= cost {
            self.tokens -= cost;
            return Ok(());
        }
        let wait = (cost - self.tokens) / self.rate;
        Err(Shed::after(
            Duration::from_secs_f64(wait).max(Duration::from_millis(1)),
        ))
    }

    /// Charges `cost` tokens unconditionally. The balance may go negative (down to one burst of
    /// debt), so later takes are refused until the debt has refilled. For bytes that already
    /// exist and cannot be refused, such as a response that is being streamed.
    pub fn debit(&mut self, cost: u64, now: Instant) {
        self.refill(now);
        self.tokens = (self.tokens - cost as f64).max(-self.capacity);
    }

    fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.capacity
    }
}

/// What a [`KeyedLimiter`] does with a new key once it tracks its maximum of keys that are not
/// idle. It never refuses the key outright.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnFull {
    /// Charge the new key to one shared overflow bucket and leave every tracked bucket alone.
    /// For keys that an attacker can mint freely: a flood only drains the overflow bucket.
    Overflow,
    /// Evict the least recently seen bucket. For keys that are costly to forge.
    EvictOldest,
}

struct Entry {
    bucket: TokenBucket,
    seen: Instant,
}

struct Table<K> {
    map: HashMap<K, Entry>,
    overflow: Option<TokenBucket>,
    last_prune: Option<Instant>,
}

/// One token bucket per key, in a table of bounded size.
pub struct KeyedLimiter<K> {
    rate: u64,
    burst: u64,
    max_keys: usize,
    on_full: OnFull,
    table: Mutex<Table<K>>,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    /// A limiter of up to [`MAX_KEYS`] keys that overflows into a shared bucket when full.
    pub fn new(rate: u64, burst: u64) -> Self {
        Self::bounded(rate, burst, MAX_KEYS, OnFull::Overflow)
    }

    pub fn bounded(rate: u64, burst: u64, max_keys: usize, on_full: OnFull) -> Self {
        Self {
            rate,
            burst,
            max_keys: max_keys.max(1),
            on_full,
            table: Mutex::new(Table {
                map: HashMap::new(),
                overflow: None,
                last_prune: None,
            }),
        }
    }

    /// Charges `cost` to `key` now.
    pub fn check(&self, key: &K, cost: u64) -> Result<(), Shed> {
        self.check_at(key, cost, Instant::now())
    }

    pub fn check_at(&self, key: &K, cost: u64, now: Instant) -> Result<(), Shed> {
        self.with_bucket(key, now, |b| b.try_take(cost, now))
    }

    /// Charges `cost` to `key` even when that overdraws it (see [`TokenBucket::debit`]).
    pub fn debit(&self, key: &K, cost: u64) {
        let now = Instant::now();
        self.with_bucket(key, now, |b| b.debit(cost, now));
    }

    fn with_bucket<R>(&self, key: &K, now: Instant, f: impl FnOnce(&mut TokenBucket) -> R) -> R {
        let mut guard = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let t = &mut *guard;
        if let Some(e) = t.map.get_mut(key) {
            e.seen = now;
            return f(&mut e.bucket);
        }
        if t.map.len() >= self.max_keys {
            // Idle (full) buckets carry no information, so dropping them is lossless.
            let due = t
                .last_prune
                .is_none_or(|p| now.saturating_duration_since(p) >= PRUNE_EVERY);
            if due {
                t.last_prune = Some(now);
                t.map.retain(|_, e| !e.bucket.is_full(now));
            }
            if t.map.len() >= self.max_keys {
                match self.on_full {
                    OnFull::Overflow => {
                        let b = t
                            .overflow
                            .get_or_insert_with(|| TokenBucket::new(self.rate, self.burst, now));
                        return f(b);
                    }
                    OnFull::EvictOldest => {
                        let oldest = t
                            .map
                            .iter()
                            .min_by_key(|(_, e)| e.seen)
                            .map(|(k, _)| k.clone());
                        if let Some(k) = oldest {
                            t.map.remove(&k);
                        }
                    }
                }
            }
        }
        let e = t.map.entry(key.clone()).or_insert_with(|| Entry {
            bucket: TokenBucket::new(self.rate, self.burst, now),
            seen: now,
        });
        f(&mut e.bucket)
    }

    /// Keys currently tracked (the overflow bucket is not a key).
    pub fn len(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-flight counts per key. Entries exist only while a slot is held, so memory is bounded by
/// the number of in-flight requests.
pub struct KeyedCounter<K> {
    counts: Mutex<HashMap<K, u32>>,
}

impl<K> Default for KeyedCounter<K> {
    fn default() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Hash + Eq + Clone> KeyedCounter<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a slot for `key` unless `limit` are already held.
    pub fn try_acquire(self: &Arc<Self>, key: &K, limit: u32) -> Result<CounterGuard<K>, Shed> {
        let mut map = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let n = map.entry(key.clone()).or_insert(0);
        if *n >= limit {
            return Err(Shed::after(SLOT_RETRY));
        }
        *n += 1;
        Ok(CounterGuard {
            owner: self.clone(),
            key: key.clone(),
        })
    }

    /// Slots held by `key`.
    pub fn held(&self, key: &K) -> u32 {
        let map = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(key).copied().unwrap_or(0)
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Returns its slot on drop.
pub struct CounterGuard<K: Hash + Eq + Clone> {
    owner: Arc<KeyedCounter<K>>,
    key: K,
}

impl<K: Hash + Eq + Clone> Drop for CounterGuard<K> {
    fn drop(&mut self) {
        let mut map = self
            .owner
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = map.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_refill() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10, 3, t0);
        for _ in 0..3 {
            b.try_take(1, t0).unwrap();
        }
        let shed = b.try_take(1, t0).unwrap_err();
        assert!(shed.retry_after >= Duration::from_millis(90));
        assert!(shed.retry_after <= Duration::from_millis(110));
        b.try_take(1, t0 + Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn oversized_cost_is_capped_at_the_burst() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10, 5, t0);
        b.try_take(1_000, t0).unwrap();
        assert!(b.try_take(1, t0).is_err());
    }

    #[test]
    fn debit_overdraws_and_refuses_until_repaid() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10, 5, t0);
        b.debit(1_000, t0);
        assert!(b.try_take(1, t0).is_err());
        // Debt is capped at one burst, so it is repaid within burst/rate plus the cost.
        b.try_take(1, t0 + Duration::from_millis(600)).unwrap();
    }

    #[test]
    fn keys_are_independent() {
        let l = KeyedLimiter::<u8>::new(1, 1);
        let t0 = Instant::now();
        l.check_at(&1, 1, t0).unwrap();
        assert!(l.check_at(&1, 1, t0).is_err());
        l.check_at(&2, 1, t0).unwrap();
    }

    #[test]
    fn a_full_limiter_prunes_idle_keys_then_overflows_to_a_shared_bucket() {
        let l = KeyedLimiter::<u32>::bounded(1, 2, 8, OnFull::Overflow);
        let t0 = Instant::now();
        for k in 0..8 {
            l.check_at(&k, 1, t0).unwrap();
        }
        // Every bucket is partly drained, so none can be dropped. New keys are not refused
        // outright: they share one overflow bucket (burst 2) and tracked keys are untouched.
        l.check_at(&100, 1, t0).unwrap();
        l.check_at(&101, 1, t0).unwrap();
        assert!(l.check_at(&102, 1, t0).is_err(), "the overflow is shared");
        assert_eq!(l.len(), 8);
        l.check_at(&0, 1, t0).unwrap();
        // After the refill period they are idle, so they are pruned and the new key fits.
        l.check_at(&u32::MAX, 1, t0 + Duration::from_secs(5))
            .unwrap();
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn a_full_evicting_limiter_admits_a_new_key_by_dropping_the_least_recent() {
        let l = KeyedLimiter::<u32>::bounded(1, 2, 3, OnFull::EvictOldest);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        for (i, k) in [1u32, 2, 3].into_iter().enumerate() {
            l.check_at(&k, 1, at(i as u64)).unwrap();
        }
        // Key 1 is the least recently seen once key 2 is touched.
        l.check_at(&2, 1, at(10)).unwrap();
        l.check_at(&4, 1, at(11)).unwrap();
        assert_eq!(l.len(), 3);
        // Key 1 was evicted and starts afresh; key 2 kept its drained bucket.
        l.check_at(&1, 1, at(12)).unwrap();
        assert!(l.check_at(&2, 1, at(12)).is_err());
    }

    #[test]
    fn pruning_a_full_table_is_throttled() {
        let l = KeyedLimiter::<u32>::bounded(1, 2, 2, OnFull::Overflow);
        let t0 = Instant::now();
        l.check_at(&1, 1, t0).unwrap();
        l.check_at(&2, 1, t0).unwrap();
        // First new key at t0 prunes (nothing idle). Both buckets then become idle, but a new
        // key within the throttle window does not scan again.
        l.check_at(&3, 1, t0).unwrap();
        let soon = t0 + Duration::from_millis(900);
        l.check_at(&4, 1, soon).unwrap();
        assert_eq!(l.len(), 2);
        // Past the window the idle keys go.
        l.check_at(&5, 1, t0 + Duration::from_secs(5)).unwrap();
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn counter_sheds_over_the_limit_and_frees_on_drop() {
        let c = Arc::new(KeyedCounter::<&str>::new());
        let a = c.try_acquire(&"k", 2).unwrap();
        let _b = c.try_acquire(&"k", 2).unwrap();
        assert!(c.try_acquire(&"k", 2).is_err());
        c.try_acquire(&"other", 2).unwrap();
        drop(a);
        assert_eq!(c.held(&"k"), 1);
        let _again = c.try_acquire(&"k", 2).unwrap();
    }

    #[test]
    fn counter_entries_vanish_at_zero() {
        let c = Arc::new(KeyedCounter::<u8>::new());
        drop(c.try_acquire(&1, 1).unwrap());
        assert_eq!(c.tracked(), 0);
    }
}
