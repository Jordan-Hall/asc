//! Client retries (SPEC §16.3): exponential backoff with full jitter, `retry-after-ms`, and a
//! per-process budget of 10% retries over a 10 s window.
//!
//! Only `UNAVAILABLE`, `DEADLINE_EXCEEDED` and `RESOURCE_EXHAUSTED` are retried, and the last
//! only when the server calls it `RATE_LIMITED` (or says nothing): `QUOTA_EXCEEDED` and the
//! other limits never clear by waiting. A mutating call without its `x-asc-op-id` is never
//! retried.

use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tonic::{Code, Status};

use crate::errors::RpcError;
use crate::server::status_rpc_error;

/// Trailer carrying the server's retry hint, in milliseconds.
pub const RETRY_AFTER_MS: &str = "retry-after-ms";

/// How a call is retried.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub base: Duration,
    pub cap: Duration,
    /// Total attempts, the first included.
    pub max_attempts: u32,
    /// A uniform sample in `[0, 1)`; replaced in tests.
    pub jitter: fn() -> f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(200),
            cap: Duration::from_secs(10),
            max_attempts: 5,
            jitter: random_unit,
        }
    }
}

fn random_unit() -> f64 {
    let b = uuid::Uuid::new_v4().into_bytes();
    let n = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) >> 11;
    n as f64 / (1u64 << 53) as f64
}

impl RetryPolicy {
    /// The wait before attempt `failed + 1`, given `failed` attempts so far (at least 1). The
    /// jittered backoff is raised to the server's hint, and the hint is capped at `cap` so a
    /// hostile server cannot park the client.
    pub fn delay(&self, failed: u32, hint: Option<Duration>) -> Duration {
        let exp = failed.saturating_sub(1).min(20);
        let ceiling = self.base.saturating_mul(1 << exp).min(self.cap);
        let jittered = ceiling.mul_f64((self.jitter)().clamp(0.0, 1.0));
        jittered.max(hint.unwrap_or_default().min(self.cap))
    }
}

/// Whether `status` is worth another attempt.
pub fn is_retryable(status: &Status) -> bool {
    match status.code() {
        Code::Unavailable | Code::DeadlineExceeded => true,
        Code::ResourceExhausted => match status_rpc_error(status) {
            None => true,
            Some(e) => matches!(e, RpcError::RateLimited(_)),
        },
        _ => false,
    }
}

/// The server's `retry-after-ms` hint.
pub fn retry_after(status: &Status) -> Option<Duration> {
    let ms: u64 = status
        .metadata()
        .get(RETRY_AFTER_MS)?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    Some(Duration::from_millis(ms))
}

const WINDOW_SECS: u64 = 10;

#[derive(Clone, Copy, Default)]
struct Slot {
    second: u64,
    requests: u32,
    retries: u32,
}

/// Retries may be at most 10% of requests in a 10 s sliding window; beyond that calls fail fast.
pub struct RetryBudget {
    start: Instant,
    slots: Mutex<[Slot; WINDOW_SECS as usize]>,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl RetryBudget {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            slots: Mutex::new([Slot::default(); WINDOW_SECS as usize]),
        }
    }

    /// Counts a first attempt.
    pub fn record_request(&self) {
        self.record_request_at(Instant::now());
    }

    /// Takes a retry from the budget, or returns false.
    pub fn try_retry(&self) -> bool {
        self.try_retry_at(Instant::now())
    }

    pub fn record_request_at(&self, now: Instant) {
        let sec = self.second(now);
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let s = Self::slot(&mut slots, sec);
        s.requests = s.requests.saturating_add(1);
    }

    pub fn try_retry_at(&self, now: Instant) -> bool {
        let sec = self.second(now);
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        Self::slot(&mut slots, sec); // reset a stale slot before summing
        let (mut requests, mut retries) = (0u64, 0u64);
        for s in slots.iter().filter(|s| sec.saturating_sub(s.second) < WINDOW_SECS) {
            requests += u64::from(s.requests);
            retries += u64::from(s.retries);
        }
        if (retries + 1) * 10 > requests {
            return false;
        }
        let s = Self::slot(&mut slots, sec);
        s.retries = s.retries.saturating_add(1);
        true
    }

    fn second(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.start).as_secs()
    }

    fn slot(slots: &mut [Slot; WINDOW_SECS as usize], sec: u64) -> &mut Slot {
        let s = &mut slots[(sec % WINDOW_SECS) as usize];
        if s.second != sec {
            *s = Slot {
                second: sec,
                ..Slot::default()
            };
        }
        s
    }
}

/// Runs `call` until it succeeds, fails with a status that is not retryable, or runs out of
/// attempts or budget. `replay_safe` is false for a mutating call that has no `x-asc-op-id`.
pub async fn retry<T, F, Fut>(
    policy: &RetryPolicy,
    budget: &RetryBudget,
    replay_safe: bool,
    mut call: F,
) -> Result<T, Status>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Status>>,
{
    budget.record_request();
    let mut failed = 0u32;
    loop {
        let status = match call().await {
            Ok(v) => return Ok(v),
            Err(s) => s,
        };
        failed += 1;
        if !replay_safe
            || !is_retryable(&status)
            || failed >= policy.max_attempts
            || !budget.try_retry()
        {
            return Err(status);
        }
        tokio::time::sleep(policy.delay(failed, retry_after(&status))).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn fast() -> RetryPolicy {
        RetryPolicy {
            base: Duration::from_millis(1),
            cap: Duration::from_millis(50),
            max_attempts: 5,
            jitter: || 1.0,
        }
    }

    fn with_hint(code: Code, ms: u64) -> Status {
        let mut s = Status::new(code, "x");
        s.metadata_mut()
            .insert(RETRY_AFTER_MS, ms.to_string().parse().unwrap());
        s
    }

    /// A budget that has seen `n` requests, so retries are allowed.
    fn funded(n: u32) -> RetryBudget {
        let b = RetryBudget::new();
        for _ in 0..n {
            b.record_request();
        }
        b
    }

    #[test]
    fn delay_is_full_jitter_up_to_the_exponential_ceiling() {
        let p = RetryPolicy {
            jitter: || 0.5,
            ..RetryPolicy::default()
        };
        assert_eq!(p.delay(1, None), Duration::from_millis(100));
        assert_eq!(p.delay(2, None), Duration::from_millis(200));
        assert_eq!(p.delay(30, None), Duration::from_secs(5));
        let zero = RetryPolicy {
            jitter: || 0.0,
            ..RetryPolicy::default()
        };
        assert_eq!(zero.delay(3, None), Duration::ZERO);
    }

    #[test]
    fn the_hint_raises_the_delay_but_is_capped() {
        let p = RetryPolicy {
            jitter: || 0.0,
            ..RetryPolicy::default()
        };
        assert_eq!(p.delay(1, Some(Duration::from_secs(2))), Duration::from_secs(2));
        assert_eq!(p.delay(1, Some(Duration::from_secs(3600))), Duration::from_secs(10));
    }

    #[test]
    fn real_jitter_stays_in_range() {
        for _ in 0..1000 {
            let u = random_unit();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn only_transient_codes_are_retryable() {
        assert!(is_retryable(&Status::unavailable("x")));
        assert!(is_retryable(&Status::deadline_exceeded("x")));
        assert!(is_retryable(&Status::resource_exhausted("x")));
        assert!(!is_retryable(&Status::not_found("x")));
        assert!(!is_retryable(&Status::aborted("x")));
        assert!(!is_retryable(&Status::permission_denied("x")));
        let quota = crate::server::rpc_status(&RpcError::QuotaExceeded(Default::default()));
        assert!(!is_retryable(&quota));
        let limited = crate::server::rpc_status(&RpcError::RateLimited(Default::default()));
        assert!(is_retryable(&limited));
    }

    #[test]
    fn budget_allows_ten_percent() {
        let b = funded(100);
        let allowed = (0..50).filter(|_| b.try_retry()).count();
        assert_eq!(allowed, 10);
    }

    #[test]
    fn budget_window_slides() {
        let b = RetryBudget::new();
        let t0 = b.start;
        for _ in 0..10 {
            b.record_request_at(t0);
        }
        assert!(b.try_retry_at(t0));
        assert!(!b.try_retry_at(t0));
        // Eleven seconds later the old requests and retries have left the window.
        let later = t0 + Duration::from_secs(11);
        assert!(!b.try_retry_at(later));
        for _ in 0..10 {
            b.record_request_at(later);
        }
        assert!(b.try_retry_at(later));
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_to_success() {
        let calls = AtomicU32::new(0);
        let budget = funded(100);
        let out = retry(&fast(), &budget, true, || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(Status::unavailable("down"))
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn attempts_stop_at_the_maximum() {
        let calls = AtomicU32::new(0);
        let budget = funded(1000);
        let out: Result<(), _> = retry(&fast(), &budget, true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Status::unavailable("down"))
        })
        .await;
        assert_eq!(out.unwrap_err().code(), Code::Unavailable);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn the_client_honours_retry_after() {
        let calls = AtomicU32::new(0);
        let budget = funded(100);
        let started = Instant::now();
        let out = retry(&fast(), &budget, true, || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(with_hint(Code::ResourceExhausted, 40))
            } else {
                Ok(())
            }
        })
        .await;
        out.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn a_failing_fake_exhausts_the_budget() {
        let calls = AtomicU32::new(0);
        let budget = RetryBudget::new();
        let policy = RetryPolicy {
            max_attempts: 5,
            jitter: || 0.0,
            ..fast()
        };
        for _ in 0..100 {
            let _ = retry::<(), _, _>(&policy, &budget, true, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Status::unavailable("down"))
            })
            .await;
        }
        // 100 first attempts plus at most 10% of 100 retries.
        let total = calls.load(Ordering::SeqCst);
        assert!(total <= 110, "{total} attempts");
        assert!(total >= 100);
    }

    #[tokio::test]
    async fn a_mutating_call_without_an_op_id_is_not_retried() {
        let calls = AtomicU32::new(0);
        let budget = funded(100);
        let out: Result<(), _> = retry(&fast(), &budget, false, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Status::unavailable("down"))
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn permanent_errors_fail_at_once() {
        let calls = AtomicU32::new(0);
        let budget = funded(100);
        let out: Result<(), _> = retry(&fast(), &budget, true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Status::not_found("gone"))
        })
        .await;
        assert_eq!(out.unwrap_err().code(), Code::NotFound);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
