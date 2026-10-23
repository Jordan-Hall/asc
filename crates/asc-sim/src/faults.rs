//! Fault injection: the virtual clock, `Faulty<O>`, `FaultyRefs<R>` and the named crash points
//! (SPEC 17.2).

use std::ops::Range;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::path::RefName;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{Error as ObjError, Objects, Result as ObjResult};
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use futures::stream::BoxStream;

use crate::rng::Rng;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The simulated clock. Nothing in a run reads the wall clock; latency and waiting advance this.
#[derive(Debug)]
pub struct SimClock(AtomicI64);

impl Default for SimClock {
    fn default() -> Self {
        Self(AtomicI64::new(1_700_000_000_000))
    }
}

impl SimClock {
    pub fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    pub fn now_secs(&self) -> i64 {
        self.now_ms() / 1000
    }
    pub fn advance_ms(&self, ms: i64) {
        self.0.fetch_add(ms.max(0), Ordering::SeqCst);
    }
}

/// The crash points of SPEC 17.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrashPoint {
    BeforeCas,
    AfterCasBeforeOutbox,
    MidImport,
    MidLand,
    MidGcMark,
    MidGcSweep,
}

impl CrashPoint {
    pub const ALL: [CrashPoint; 6] = [
        Self::BeforeCas,
        Self::AfterCasBeforeOutbox,
        Self::MidImport,
        Self::MidLand,
        Self::MidGcMark,
        Self::MidGcSweep,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::BeforeCas => "before_cas",
            Self::AfterCasBeforeOutbox => "after_cas_before_outbox",
            Self::MidImport => "mid_import",
            Self::MidLand => "mid_land",
            Self::MidGcMark => "mid_gc_mark",
            Self::MidGcSweep => "mid_gc_sweep",
        }
    }
}

/// The armed crash points. A point fires when its countdown reaches zero, then disarms.
#[derive(Debug, Default)]
pub struct CrashPoints {
    armed: Mutex<Vec<(CrashPoint, u32)>>,
    fired: AtomicU64,
}

impl CrashPoints {
    /// Crashes at the `after_hits + 1`-th pass through `point`.
    pub fn arm(&self, point: CrashPoint, after_hits: u32) {
        lock(&self.armed).push((point, after_hits));
    }

    /// Called at each named point; `true` means the replica dies here.
    pub fn hit(&self, point: CrashPoint) -> bool {
        let mut armed = lock(&self.armed);
        let Some(i) = armed.iter().position(|(p, _)| *p == point) else {
            return false;
        };
        if armed[i].1 == 0 {
            armed.remove(i);
            self.fired.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            armed[i].1 -= 1;
            false
        }
    }

    pub fn clear(&self) {
        lock(&self.armed).clear();
    }

    pub fn fired(&self) -> u64 {
        self.fired.load(Ordering::SeqCst)
    }

    pub fn armed_count(&self) -> usize {
        lock(&self.armed).len()
    }
}

/// Probabilities (per mille) and latency of the injected faults.
#[derive(Clone, Copy, Debug, Default)]
pub struct FaultPlan {
    pub latency_max_ms: u64,
    pub unavailable_pm: u32,
    /// Writes only: the write succeeds, the response is lost.
    pub lost_response_pm: u32,
    /// Every call answers `UNAVAILABLE` (a partition from this store).
    pub down: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Pass,
    Unavailable,
    LostAfterWrite,
}

/// The shared fault state of one world.
#[derive(Debug)]
pub struct FaultCtl {
    plan: Mutex<FaultPlan>,
    rng: Mutex<Rng>,
    clock: Arc<SimClock>,
    pub unavailable: AtomicU64,
    pub lost: AtomicU64,
}

impl FaultCtl {
    pub fn new(clock: Arc<SimClock>, seed: u64, plan: FaultPlan) -> Arc<Self> {
        Arc::new(Self {
            plan: Mutex::new(plan),
            rng: Mutex::new(Rng::new(seed)),
            clock,
            unavailable: AtomicU64::new(0),
            lost: AtomicU64::new(0),
        })
    }

    pub fn set_plan(&self, plan: FaultPlan) {
        *lock(&self.plan) = plan;
    }

    pub fn plan(&self) -> FaultPlan {
        *lock(&self.plan)
    }

    pub fn decide(&self, write: bool) -> Decision {
        let plan = self.plan();
        let mut rng = lock(&self.rng);
        if plan.latency_max_ms > 0 {
            self.clock
                .advance_ms(i64::try_from(rng.below(plan.latency_max_ms + 1)).unwrap_or(0));
        }
        if plan.down {
            self.unavailable.fetch_add(1, Ordering::SeqCst);
            return Decision::Unavailable;
        }
        let roll = u32::try_from(rng.below(1000)).unwrap_or(0);
        if roll < plan.unavailable_pm {
            self.unavailable.fetch_add(1, Ordering::SeqCst);
            Decision::Unavailable
        } else if write && roll < plan.unavailable_pm + plan.lost_response_pm {
            self.lost.fetch_add(1, Ordering::SeqCst);
            Decision::LostAfterWrite
        } else {
            Decision::Pass
        }
    }
}

fn unavailable() -> ObjError {
    ObjError::Store(object_store::Error::Generic {
        store: "sim",
        source: "UNAVAILABLE".into(),
    })
}

/// Wraps any `Objects` with added latency, `UNAVAILABLE` and lost responses after a write.
pub struct Faulty<O: Objects> {
    inner: O,
    ctl: Arc<FaultCtl>,
}

impl<O: Objects> Faulty<O> {
    pub fn new(inner: O, ctl: Arc<FaultCtl>) -> Self {
        Self { inner, ctl }
    }

    fn read_gate(&self) -> ObjResult<()> {
        match self.ctl.decide(false) {
            Decision::Unavailable => Err(unavailable()),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl<O: Objects> Objects for Faulty<O> {
    async fn get(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        self.read_gate()?;
        self.inner.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        self.read_gate()?;
        self.inner.get_stored(key).await
    }
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> ObjResult<Option<Bytes>> {
        self.read_gate()?;
        self.inner.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> ObjResult<()> {
        match self.ctl.decide(true) {
            Decision::Unavailable => Err(unavailable()),
            Decision::LostAfterWrite => {
                self.inner.put(key, stored).await?;
                Err(unavailable())
            }
            Decision::Pass => self.inner.put(key, stored).await,
        }
    }
    async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
        self.read_gate()?;
        self.inner.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> ObjResult<()> {
        match self.ctl.decide(true) {
            Decision::Unavailable => Err(unavailable()),
            Decision::LostAfterWrite => {
                self.inner.delete(key).await?;
                Err(unavailable())
            }
            Decision::Pass => self.inner.delete(key).await,
        }
    }
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
        self.inner.list(prefix)
    }
}

fn refs_unavailable() -> RefError {
    RefError::Backend("UNAVAILABLE".into())
}

/// The same faults for the ref store; a lost response applies the transaction first.
pub struct FaultyRefs<R: Refs> {
    inner: R,
    ctl: Arc<FaultCtl>,
}

impl<R: Refs> FaultyRefs<R> {
    pub fn new(inner: R, ctl: Arc<FaultCtl>) -> Self {
        Self { inner, ctl }
    }
}

#[async_trait]
impl<R: Refs> Refs for FaultyRefs<R> {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>, RefError> {
        if self.ctl.decide(false) == Decision::Unavailable {
            return Err(refs_unavailable());
        }
        self.inner.get(repo, name).await
    }
    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>, RefError> {
        if self.ctl.decide(false) == Decision::Unavailable {
            return Err(refs_unavailable());
        }
        self.inner.list(repo, prefix).await
    }
    async fn update(&self, repo: RepoKey, txn: &RefTxn) -> Result<(), RefError> {
        match self.ctl.decide(true) {
            Decision::Unavailable => Err(refs_unavailable()),
            Decision::LostAfterWrite => {
                self.inner.update(repo, txn).await?;
                Err(refs_unavailable())
            }
            Decision::Pass => self.inner.update(repo, txn).await,
        }
    }
    async fn log(
        &self,
        repo: RepoKey,
        name: &RefName,
        limit: u32,
    ) -> Result<Vec<RefLogEntry>, RefError> {
        if self.ctl.decide(false) == Decision::Unavailable {
            return Err(refs_unavailable());
        }
        self.inner.log(repo, name, limit).await
    }
}
