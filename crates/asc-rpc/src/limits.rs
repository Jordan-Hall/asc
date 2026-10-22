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

/// Distinct keys one limiter tracks before it refuses new ones (bounds memory).
pub const MAX_KEYS: usize = 100_000;
/// Retry hint for a refused concurrency slot.
const SLOT_RETRY: Duration = Duration::from_millis(100);
/// Retry hint when a limiter holds too many keys.
const FULL_RETRY: Duration = Duration::from_secs(1);

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

    fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.capacity
    }
}

/// One token bucket per key.
pub struct KeyedLimiter<K> {
    rate: u64,
    burst: u64,
    buckets: Mutex<HashMap<K, TokenBucket>>,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    pub fn new(rate: u64, burst: u64) -> Self {
        Self {
            rate,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Charges `cost` to `key` now.
    pub fn check(&self, key: &K, cost: u64) -> Result<(), Shed> {
        self.check_at(key, cost, Instant::now())
    }

    pub fn check_at(&self, key: &K, cost: u64, now: Instant) -> Result<(), Shed> {
        let mut map = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if !map.contains_key(key) && map.len() >= MAX_KEYS {
            // Idle (full) buckets carry no information, so dropping them is lossless.
            map.retain(|_, b| !b.is_full(now));
            if map.len() >= MAX_KEYS {
                return Err(Shed::after(FULL_RETRY));
            }
        }
        map.entry(key.clone())
            .or_insert_with(|| TokenBucket::new(self.rate, self.burst, now))
            .try_take(cost, now)
    }

    /// Keys currently tracked.
    pub fn len(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
    fn keys_are_independent() {
        let l = KeyedLimiter::<u8>::new(1, 1);
        let t0 = Instant::now();
        l.check_at(&1, 1, t0).unwrap();
        assert!(l.check_at(&1, 1, t0).is_err());
        l.check_at(&2, 1, t0).unwrap();
    }

    #[test]
    fn a_full_limiter_prunes_idle_keys_then_refuses() {
        let l = KeyedLimiter::<u32>::new(1, 2);
        let t0 = Instant::now();
        for k in 0..MAX_KEYS as u32 {
            l.check_at(&k, 1, t0).unwrap();
        }
        // Every bucket is partly drained, so none can be dropped and the next key is refused.
        assert!(l.check_at(&u32::MAX, 1, t0).is_err());
        assert_eq!(l.len(), MAX_KEYS);
        // After the refill period they are idle, so they are pruned and the new key fits.
        l.check_at(&u32::MAX, 1, t0 + Duration::from_secs(5))
            .unwrap();
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
