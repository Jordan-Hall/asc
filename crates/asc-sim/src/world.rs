//! The simulated deployment: replicas, store, ref store, the database tables, fake Forgejo and
//! the clock. Everything is single threaded and driven by one seeded RNG.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use bytes::Bytes;
use uuid::Uuid;

use crate::faults::{CrashPoint, CrashPoints, FaultCtl, FaultPlan, Faulty, FaultyRefs, SimClock};
use crate::memrefs::{MemRefs, SharedRefs};
use crate::model::{Bugs, Cs, MASTER, PROTO_MAX, PROTO_MIN, R, REPO, SimKey, oerr, rerr, rn, viol};
use crate::rng::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Primary,
    Cache,
}

/// One `asc serve` replica.
#[derive(Clone, Debug)]
pub struct Replica {
    pub role: Role,
    pub up: bool,
    pub down_until: u64,
    pub proto_lo: u8,
    pub proto_hi: u8,
    pub restarts: u32,
}

impl Replica {
    pub fn supports(&self, proto: u8) -> bool {
        (self.proto_lo..=self.proto_hi).contains(&proto)
    }
}

/// One undelivered `post_receive` (SPEC 9.5 outbox).
#[derive(Clone, Debug)]
pub struct OutRow {
    pub op_id: Uuid,
    pub failures: i64,
    pub due_ms: i64,
    pub lease_until_ms: i64,
    pub delivered: bool,
    pub last_backoff_s: i64,
}

/// The incrementally maintained `mainline` and `path_changes` (recomputed by the invariants).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathIndex {
    pub through: Option<[u8; 32]>,
    pub mainline: Vec<[u8; 32]>,
    pub path_changes: BTreeMap<u8, Vec<u64>>,
}

#[derive(Debug, Default)]
pub struct Db {
    pub outbox: BTreeMap<(String, u64), OutRow>,
    pub cs_graph: BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    pub git_map: BTreeMap<u64, [u8; 32]>,
    pub index: PathIndex,
    pub webhook_seen: BTreeSet<u64>,
    pub webhook_effects: BTreeMap<u64, u32>,
}

/// The fake Forgejo: records what the outbox delivers to it.
#[derive(Debug, Default)]
pub struct Forgejo {
    pub deliveries: BTreeMap<(String, u64), u32>,
    pub effects: BTreeMap<(String, u64), u32>,
}

/// Forgejo did not answer (or its answer was lost).
#[derive(Debug, Clone, Copy)]
pub struct Refused;

impl Forgejo {
    /// `post_receive` is idempotent on `(ref, version)`; the answer may still be lost.
    pub fn post_receive(&mut self, key: &(String, u64), rng: &mut Rng) -> Result<(), Refused> {
        *self.deliveries.entry(key.clone()).or_insert(0) += 1;
        if rng.chance(1, 5) {
            return Err(Refused);
        }
        self.effects.entry(key.clone()).or_insert(1);
        if rng.chance(1, 6) {
            Err(Refused)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ingest(usize),
    Land,
    Done,
}

/// One agent's change on its way from push to landed.
#[derive(Clone, Debug)]
pub struct Job {
    pub change: u64,
    pub op_id: Uuid,
    pub proto: u8,
    pub files: Vec<(u8, [u8; 32])>,
    pub contents: Vec<Vec<u8>>,
    pub cs: Option<Cs>,
    pub phase: Phase,
    pub attempts: u32,
    pub skewed: u32,
    pub started_ms: i64,
    pub abandoned: bool,
    /// Land phase one's result: the tip it saw and the landing commit it wrote.
    pub prep: Option<(Option<[u8; 32]>, Cs)>,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub landed: u64,
    pub conflicts: u64,
    pub retries: u64,
    pub crashes: u64,
    pub reingests: u64,
    pub skew_refusals: u64,
    pub fetches: u64,
    pub swept: u64,
    pub upstream_calls: u64,
    pub coalesced: u64,
    pub shed: u64,
    pub webhook_applied: u64,
    pub webhook_dups: u64,
}

pub struct World {
    pub seed: u64,
    pub rng: Rng,
    pub clock: Arc<SimClock>,
    pub ctl: Arc<FaultCtl>,
    pub crash: Arc<CrashPoints>,
    pub raw_objects: BucketObjects,
    pub objects: Faulty<BucketObjects>,
    pub raw_refs: Arc<MemRefs>,
    pub refs: FaultyRefs<SharedRefs>,
    pub born: BTreeMap<SimKey, i64>,
    pub replicas: Vec<Replica>,
    pub db: Db,
    pub forgejo: Forgejo,
    pub jobs: Vec<Job>,
    pub acked: Vec<u64>,
    pub garbage: Vec<SimKey>,
    pub bugs: Bugs,
    pub step: u64,
    pub trace: u64,
    pub stats: Stats,
    /// Set while draining: no new faults, no random replica deaths.
    pub quiet: bool,
    pub next_change: u64,
    pub upstream_calls: Vec<i64>,
    pub cache: BTreeMap<usize, crate::cache::Cache>,
    pub gc: crate::gc::Gc,
    pub(crate) chk: crate::invariants::Checked,
}

impl World {
    pub fn new(seed: u64, roles: &[Role], plan: FaultPlan, bugs: Bugs) -> Self {
        let clock = Arc::new(SimClock::default());
        let mut rng = Rng::new(seed);
        let ctl = FaultCtl::new(clock.clone(), rng.next_u64(), plan);
        let raw_objects = BucketObjects::memory();
        let raw_refs = MemRefs::new(clock.clone());
        Self {
            seed,
            rng,
            objects: Faulty::new(raw_objects.clone(), ctl.clone()),
            refs: FaultyRefs::new(SharedRefs(raw_refs.clone()), ctl.clone()),
            raw_objects,
            raw_refs,
            clock,
            ctl,
            crash: Arc::new(CrashPoints::default()),
            born: BTreeMap::new(),
            replicas: roles
                .iter()
                .map(|&role| Replica {
                    role,
                    up: true,
                    down_until: 0,
                    proto_lo: PROTO_MIN,
                    proto_hi: PROTO_MAX,
                    restarts: 0,
                })
                .collect(),
            db: Db::default(),
            forgejo: Forgejo::default(),
            jobs: Vec::new(),
            acked: Vec::new(),
            garbage: Vec::new(),
            bugs,
            step: 0,
            trace: 0xcbf2_9ce4_8422_2325,
            stats: Stats::default(),
            quiet: false,
            next_change: 1,
            upstream_calls: Vec::new(),
            cache: BTreeMap::new(),
            gc: crate::gc::Gc::default(),
            chk: crate::invariants::Checked::default(),
        }
    }

    /// Folds an event into the trace hash; two runs of one seed must end with the same hash.
    pub fn note(&mut self, tag: u64, val: u64) {
        for x in [self.step, tag, val] {
            self.trace = (self.trace ^ x).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn tick(&mut self) {
        self.step += 1;
        let ms = i64::try_from(self.rng.below(400)).unwrap_or(0);
        self.clock.advance_ms(ms);
        for i in 0..self.replicas.len() {
            if !self.replicas[i].up && self.replicas[i].down_until <= self.step {
                self.restart(i);
            }
        }
    }

    pub fn primaries(&self) -> Vec<usize> {
        (0..self.replicas.len())
            .filter(|&i| self.replicas[i].role == Role::Primary && self.replicas[i].up)
            .collect()
    }

    pub fn crash_replica(&mut self, i: usize, point: CrashPoint) {
        let down = 2 + self.rng.below(8);
        let r = &mut self.replicas[i];
        r.up = false;
        r.down_until = self.step + down;
        self.stats.crashes += 1;
        self.cache.remove(&i);
        if matches!(point, CrashPoint::MidGcMark | CrashPoint::MidGcSweep) {
            self.gc = crate::gc::Gc::default();
        }
        self.note(1, i as u64 * 16 + point as u64);
    }

    /// A replica comes back: it reconciles the outbox and catches the index up before serving.
    pub fn restart(&mut self, i: usize) {
        let r = &mut self.replicas[i];
        r.up = true;
        r.restarts += 1;
        self.note(2, i as u64);
    }

    // ---- object helpers -------------------------------------------------------------------

    pub async fn raw_has(&self, keys: &[SimKey]) -> R<Vec<bool>> {
        let ks: Vec<_> = keys.iter().map(|k| k.obj()).collect();
        self.raw_objects.has_many(&ks).await.map_err(oerr)
    }

    /// Writes one object. I1: everything it references is durable first.
    pub async fn put_obj(&mut self, key: SimKey, data: &[u8]) -> R<()> {
        if let (SimKey::Cs(_), false) = (key, self.bugs.skip_closure_order)
            && let Some(cs) = Cs::decode(data)
        {
            let need = cs.needs();
            if self.raw_has(&need).await?.iter().any(|h| !h) {
                return viol(format!("I1: {key:?} written before its closure"));
            }
        }
        let res = self
            .objects
            .put(&key.obj(), encode_stored(data))
            .await
            .map_err(oerr);
        // A lost response still wrote the object.
        if self.raw_has(&[key]).await?[0] {
            let now = self.clock.now_ms();
            self.born.entry(key).or_insert(now);
        }
        res
    }

    pub async fn get_obj(&self, key: SimKey) -> R<Option<Bytes>> {
        self.objects.get(&key.obj()).await.map_err(oerr)
    }

    pub async fn get_cs(&self, id: [u8; 32]) -> R<Option<Cs>> {
        match self.get_obj(SimKey::Cs(id)).await? {
            None => Ok(None),
            Some(b) => match Cs::decode(&b) {
                Some(c) => Ok(Some(c)),
                None => viol("a verified changeset does not decode"),
            },
        }
    }

    pub async fn master_tip(&self) -> R<Option<([u8; 32], u64)>> {
        let e = self.refs.get(REPO, &rn(MASTER)).await.map_err(rerr)?;
        Ok(e.map(|e| match e.target {
            RefTarget::Changeset(c) => (*c.as_bytes(), e.version),
            RefTarget::Tag(t) => (*t.as_bytes(), e.version),
        }))
    }

    pub fn target(id: [u8; 32]) -> RefTarget {
        RefTarget::Changeset(ChangesetId::from_bytes(id))
    }

    pub fn new_op(&mut self) -> Uuid {
        Uuid::from_u128(self.rng.u128())
    }
}
