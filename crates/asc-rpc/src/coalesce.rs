//! In-flight singleflight: concurrent misses on one key share a single fetch or derivation
//! (SPEC §16.3 "Coalescing").

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::FutureExt;
use futures::future::{BoxFuture, WeakShared};

type Flights<K, V> = Mutex<HashMap<K, (u64, WeakShared<BoxFuture<'static, V>>)>>;

/// Runs one computation per key at a time; every concurrent caller of the same key awaits it and
/// receives a clone of its result. The computation runs to completion as long as any caller
/// still waits (it is dropped when the last one gives up), and a later call after it finishes
/// starts a fresh one.
pub struct Coalesce<K, V> {
    flights: Arc<Flights<K, V>>,
    next: AtomicU64,
}

impl<K, V> Default for Coalesce<K, V> {
    fn default() -> Self {
        Self {
            flights: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        }
    }
}

/// Removes the flight when its future finishes or is dropped.
struct Done<K: Hash + Eq, V> {
    flights: Arc<Flights<K, V>>,
    key: K,
    id: u64,
}

impl<K: Hash + Eq, V> Drop for Done<K, V> {
    fn drop(&mut self) {
        let mut map = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        if map.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            map.remove(&self.key);
        }
    }
}

impl<K, V> Coalesce<K, V>
where
    K: Hash + Eq + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of computations in flight.
    pub fn in_flight(&self) -> usize {
        self.flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The result of `make()` for `key`, computed once for all concurrent callers.
    pub async fn run<F, Fut>(&self, key: K, make: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        let shared = {
            let mut map = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(f) = map.get(&key).and_then(|(_, w)| w.upgrade()) {
                f
            } else {
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                let done = Done {
                    flights: self.flights.clone(),
                    key: key.clone(),
                    id,
                };
                let work = make();
                let f = async move {
                    let _done = done;
                    work.await
                }
                .boxed()
                .shared();
                if let Some(weak) = f.downgrade() {
                    map.insert(key, (id, weak));
                }
                f
            }
        };
        shared.await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn hundred_callers_make_one_backend_read() {
        let c = Arc::new(Coalesce::<u32, usize>::new());
        let reads = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..100 {
            let (c, reads) = (c.clone(), reads.clone());
            tasks.push(tokio::spawn(async move {
                c.run(7, || async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    reads.fetch_add(1, Ordering::SeqCst) + 1
                })
                .await
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), 1);
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(c.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_finished_flight_is_not_reused() {
        let c = Coalesce::<u32, u32>::new();
        assert_eq!(c.run(1, || async { 10 }).await, 10);
        assert_eq!(c.run(1, || async { 11 }).await, 11);
    }

    #[tokio::test]
    async fn distinct_keys_run_independently() {
        let c = Coalesce::<u32, u32>::new();
        let (a, b) = futures::join!(c.run(1, || async { 1 }), c.run(2, || async { 2 }));
        assert_eq!((a, b), (1, 2));
    }

    #[tokio::test]
    async fn dropping_the_first_caller_does_not_cancel_the_others() {
        let c = Arc::new(Coalesce::<u32, u32>::new());
        let first = {
            let c = c.clone();
            tokio::spawn(async move {
                c.run(1, || async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    5
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let second = {
            let c = c.clone();
            tokio::spawn(async move { c.run(1, || async { 99 }).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        first.abort();
        assert_eq!(second.await.unwrap(), 5);
        assert_eq!(c.in_flight(), 0);
    }

    #[tokio::test]
    async fn abandoned_flights_are_removed() {
        let c = Arc::new(Coalesce::<u32, u32>::new());
        let t = {
            let c = c.clone();
            tokio::spawn(async move { c.run(1, std::future::pending::<u32>).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(c.in_flight(), 1);
        t.abort();
        let _ = t.await;
        assert_eq!(c.in_flight(), 0);
    }
}
