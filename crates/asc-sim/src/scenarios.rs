//! The scenarios of SPEC 17.2. Each one builds a world from a seed, drives it step by step with
//! the invariants checked after every step, drains it, and checks the end state.

use crate::faults::{CrashPoint, FaultPlan};
use crate::import::{GitCommit, Importer, history};
use crate::model::{Bugs, Cs, Fail, MIN_AGE_MS, MIRROR, R, REPO, SimKey, rerr, rn, viol};
use crate::world::{Phase, Role, World};

use asc_core::id::RefTarget;
use asc_store::refs::Refs;

/// Every scenario, in the order the runner uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    ConcurrentLand,
    WebhookStorm,
    GcTraffic,
    OutboxRestart,
    VersionSkew,
    CacheStampede,
    ImportCrash,
}

impl Scenario {
    pub const ALL: [Scenario; 7] = [
        Self::ConcurrentLand,
        Self::WebhookStorm,
        Self::GcTraffic,
        Self::OutboxRestart,
        Self::VersionSkew,
        Self::CacheStampede,
        Self::ImportCrash,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ConcurrentLand => "concurrent_land",
            Self::WebhookStorm => "webhook_storm",
            Self::GcTraffic => "gc_traffic",
            Self::OutboxRestart => "outbox_restart",
            Self::VersionSkew => "version_skew",
            Self::CacheStampede => "cache_stampede",
            Self::ImportCrash => "import_crash",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.name() == s)
    }
}

/// Counters a finished run reports.
#[derive(Clone, Debug)]
pub struct Report {
    pub scenario: Scenario,
    pub seed: u64,
    pub steps: u64,
    /// Hash of every event; a replay of the seed must give the same value.
    pub trace: u64,
    pub landed: u64,
    pub crashes: u64,
    pub unavailable: u64,
    pub lost: u64,
    pub conflicts: u64,
    pub upstream_calls: u64,
}

type Tip = [u8; 32];
type Obj = (SimKey, Vec<u8>);

fn flaky() -> FaultPlan {
    FaultPlan {
        latency_max_ms: 40,
        unavailable_pm: 30,
        lost_response_pm: 30,
        down: false,
    }
}

fn soft<T>(r: R<T>) -> R<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Fail::Transient) => Ok(None),
        Err(e) => Err(e),
    }
}

impl World {
    /// Background work of a live replica: outbox reconciler, index derivation, delivery.
    pub async fn maintenance(&mut self) -> R<()> {
        let prim = self.primaries();
        if prim.is_empty() {
            return Ok(());
        }
        let ri = prim[self.rng.index(prim.len())];
        self.reconcile_outbox();
        soft(self.index_catch_up(2).await)?;
        self.deliver_one(ri);
        Ok(())
    }

    /// A client fetch: walks `master` and must never meet a missing object.
    pub async fn fetch(&mut self) -> R<()> {
        self.stats.fetches += 1;
        let Some(Some((tip, _))) = soft(self.master_tip().await)? else {
            return Ok(());
        };
        let mut cur = Some(tip);
        while let Some(id) = cur {
            let Some(got) = soft(self.get_obj(SimKey::Cs(id)).await)? else {
                return Ok(());
            };
            let Some(b) = got else {
                return viol(format!(
                    "a fetch of master met a missing changeset {}",
                    crate::jobs::hex(&id)
                ));
            };
            let Some(cs) = Cs::decode(&b) else {
                return viol("a fetched changeset does not decode");
            };
            for (_, f) in &cs.files {
                if let Some(None) = soft(self.get_obj(SimKey::File(*f)).await)? {
                    return viol("a fetch of master met a missing file");
                }
            }
            cur = cs.parent;
        }
        Ok(())
    }

    /// One random step of mixed traffic.
    pub async fn traffic_step(&mut self, gc: bool, max_jobs: usize) -> R<()> {
        self.tick();
        let roll = self.rng.below(100);
        match roll {
            0..=64 => {
                let run = self.runnable_jobs();
                if !run.is_empty() {
                    let ji = run[self.rng.index(run.len())];
                    self.step_job(ji).await?;
                }
            }
            65..=74 => self.maintenance().await?,
            75..=82 => self.fetch().await?,
            83..=89 => {
                let prim = self.primaries();
                if gc && !prim.is_empty() {
                    let ri = prim[self.rng.index(prim.len())];
                    self.gc_step(ri).await?;
                } else {
                    self.maintenance().await?;
                }
            }
            90..=93 if self.jobs.len() < max_jobs => {
                self.spawn_job(6);
            }
            94..=95 => {
                let prim = self.primaries();
                if prim.len() > 1 {
                    let ri = prim[self.rng.index(prim.len())];
                    let p = CrashPoint::ALL[self.rng.index(CrashPoint::ALL.len())];
                    self.crash_replica(ri, p);
                }
            }
            _ => {}
        }
        self.check_step().await
    }

    /// Ends the faults, finishes every job, delivers the outbox, and checks the end state.
    pub async fn drain(&mut self) -> R<()> {
        self.quiet = true;
        self.ctl.set_plan(FaultPlan::default());
        self.crash.clear();
        for i in 0..self.replicas.len() {
            if !self.replicas[i].up {
                self.restart(i);
            }
        }
        for _ in 0..20_000 {
            let run = self.runnable_jobs();
            if run.is_empty() {
                break;
            }
            self.tick();
            let ji = run[self.rng.index(run.len())];
            self.step_job(ji).await?;
            self.check_step().await?;
        }
        if let Some(j) = self.runnable_jobs().first() {
            return viol(format!("job {} never finished", self.jobs[*j].change));
        }
        self.reconcile_outbox();
        let Some(&ri) = self.primaries().first() else {
            return viol("no primary after the drain");
        };
        for _ in 0..20_000 {
            self.index_catch_up(100).await?;
            if self.db.outbox.values().all(|r| r.delivered) {
                break;
            }
            self.clock.advance_ms(30_000);
            self.deliver_one(ri);
        }
        self.check_final().await
    }

    fn report(&self, scenario: Scenario) -> Report {
        use std::sync::atomic::Ordering::SeqCst;
        Report {
            scenario,
            seed: self.seed,
            steps: self.step,
            trace: self.trace,
            landed: self.stats.landed,
            crashes: self.stats.crashes,
            unavailable: self.ctl.unavailable.load(SeqCst),
            lost: self.ctl.lost.load(SeqCst),
            conflicts: self.stats.conflicts,
            upstream_calls: self.stats.upstream_calls,
        }
    }
}

pub async fn run(scenario: Scenario, seed: u64, bugs: Bugs) -> Result<Report, (u64, Fail)> {
    let mut w = match scenario {
        Scenario::ConcurrentLand | Scenario::GcTraffic | Scenario::OutboxRestart => {
            World::new(seed, &[Role::Primary, Role::Primary], flaky(), bugs)
        }
        Scenario::WebhookStorm => World::new(seed, &[Role::Primary, Role::Primary], flaky(), bugs),
        Scenario::VersionSkew => World::new(
            seed,
            &[Role::Primary, Role::Primary, Role::Primary],
            FaultPlan::default(),
            bugs,
        ),
        Scenario::CacheStampede => World::new(
            seed,
            &[Role::Primary, Role::Cache, Role::Cache],
            FaultPlan::default(),
            bugs,
        ),
        Scenario::ImportCrash => World::new(seed, &[Role::Primary, Role::Primary], flaky(), bugs),
    };
    let res = match scenario {
        Scenario::ConcurrentLand => concurrent_land(&mut w).await,
        Scenario::WebhookStorm => webhook_storm(&mut w).await,
        Scenario::GcTraffic => gc_traffic(&mut w).await,
        Scenario::OutboxRestart => outbox_restart(&mut w).await,
        Scenario::VersionSkew => version_skew(&mut w).await,
        Scenario::CacheStampede => cache_stampede(&mut w).await,
        Scenario::ImportCrash => import_crash(&mut w, bugs).await,
    };
    match res {
        Ok(()) => Ok(w.report(scenario)),
        Err(e) => Err((w.step, e)),
    }
}

/// 20 agents push and land at once with overlapping paths, faults and crashes on.
async fn concurrent_land(w: &mut World) -> R<()> {
    for p in [
        CrashPoint::BeforeCas,
        CrashPoint::AfterCasBeforeOutbox,
        CrashPoint::MidImport,
        CrashPoint::MidLand,
    ] {
        let after = u32::try_from(w.rng.below(40)).unwrap_or(0);
        w.crash.arm(p, after);
    }
    for _ in 0..20 {
        w.spawn_job(6);
    }
    for _ in 0..500 {
        w.traffic_step(false, 40).await?;
    }
    w.drain().await?;
    if w.stats.landed < 20 {
        return viol(format!("only {} of 20 agents landed", w.stats.landed));
    }
    Ok(())
}

/// Forgejo webhooks with duplicates and reordering; the mirror ref ends at the newest event.
async fn webhook_storm(w: &mut World) -> R<()> {
    let n = 12 + w.rng.index(16);
    let mut events: Vec<(Tip, Vec<Obj>)> = Vec::new();
    let mut parent = None;
    let mut objs: Vec<Obj> = Vec::new();
    for i in 0..n {
        let body = format!("fj file {i} {}", w.rng.next_u64()).into_bytes();
        let SimKey::File(fid) = SimKey::file(&body) else {
            continue;
        };
        objs.push((SimKey::File(fid), body));
        let cs = Cs {
            parent,
            files: vec![(0, fid)],
            accepted: vec![],
            nonce: i as u64,
        };
        objs.push((SimKey::Cs(cs.id()), cs.encode()));
        parent = Some(cs.id());
        events.push((cs.id(), objs.clone()));
    }
    // Deliveries: (delivery id, seq). Each event arrives 1-4 times, in a random order.
    let mut queue: Vec<(u64, u64)> = Vec::new();
    let mut did = 1;
    for seq in 0..n as u64 {
        for _ in 0..1 + w.rng.index(4) {
            queue.push((did, seq + 1));
            did += 1;
        }
    }
    w.rng.shuffle(&mut queue);
    // A duplicate keeps its event's delivery id, so Forgejo's own retry is a true duplicate.
    for (id, seq) in queue.iter_mut() {
        *id = *seq * 100 + (*id % 3);
    }
    w.crash.arm(CrashPoint::MidImport, 3);
    for _ in 0..20 {
        w.spawn_job(4);
    }
    let mut pending: std::collections::VecDeque<(u64, u64)> = queue.into();
    let mut guard = 0;
    while let Some((d, seq)) = pending.pop_front() {
        guard += 1;
        if guard > 5000 {
            return viol("webhooks are never accepted");
        }
        w.tick();
        let (tip, objs) = &events[(seq - 1) as usize];
        let prim = w.primaries();
        if prim.is_empty() {
            pending.push_back((d, seq));
            continue;
        }
        let ri = prim[w.rng.index(prim.len())];
        match w.webhook(d, seq, *tip, objs).await {
            Ok(()) => {}
            Err(Fail::Transient) => pending.push_back((d, seq)),
            Err(Fail::Crashed(p)) => {
                w.crash_replica(ri, p);
                pending.push_back((d, seq));
            }
            Err(e) => return Err(e),
        }
        if w.rng.chance(1, 4) {
            let run = w.runnable_jobs();
            if let Some(&ji) = run.first() {
                w.step_job(ji).await?;
            }
        }
        w.check_step().await?;
    }
    w.drain().await?;
    let want = events[n - 1].0;
    let got = w.refs.get(REPO, &rn(MIRROR)).await.map_err(rerr);
    // The drain turned faults off, so this read is reliable.
    match got? {
        Some(e) if e.target == RefTarget::Changeset(asc_core::id::ChangesetId::from_bytes(want)) => {
            Ok(())
        }
        other => viol(format!(
            "the mirror ref ended at {other:?}, not the newest event"
        )),
    }
}

/// Pushes, lands and fetches while GC marks and sweeps; garbage must go, live data must stay.
async fn gc_traffic(w: &mut World) -> R<()> {
    w.crash.arm(
        CrashPoint::MidGcMark,
        u32::try_from(w.rng.below(30)).unwrap_or(0),
    );
    w.crash.arm(
        CrashPoint::MidGcSweep,
        u32::try_from(w.rng.below(10)).unwrap_or(0),
    );
    // Abandoned pushes: ingested, never landed. After min_age they are garbage.
    let k = 3 + w.rng.index(4);
    for _ in 0..k {
        let ji = w.spawn_job(6);
        w.quiet = true;
        let faults = w.ctl.plan();
        w.ctl.set_plan(FaultPlan::default());
        while w.jobs[ji].phase != Phase::Land {
            w.step_job(ji).await?;
        }
        w.ctl.set_plan(faults);
        w.quiet = false;
        w.jobs[ji].abandoned = true;
        let cs = w.jobs[ji].cs.clone();
        if let Some(cs) = cs {
            w.garbage.push(SimKey::Cs(cs.id()));
            w.garbage.extend(cs.needs());
        }
    }
    w.clock.advance_ms(2 * MIN_AGE_MS);
    for _ in 0..12 {
        w.spawn_job(6);
    }
    for i in 0..600 {
        w.traffic_step(true, 30).await?;
        if i == 300 {
            // Another idle stretch: pushes that have not landed fall past min_age (they re-push).
            w.clock.advance_ms(MIN_AGE_MS + 1);
        }
    }
    w.drain().await?;
    let Some(&ri) = w.primaries().first() else {
        return viol("no primary after the drain");
    };
    w.gc = crate::gc::Gc::default();
    w.gc_to_completion(ri).await?;
    w.check_step().await?;
    for g in w.garbage.clone() {
        if w.raw_has(&[g]).await?[0] {
            // Shared content with a live object is not garbage; abandoned pushes are unique.
            return viol(format!("garbage {g:?} was not swept"));
        }
    }
    Ok(())
}

/// Replicas die while delivering the outbox; every committed ref change is still delivered.
async fn outbox_restart(w: &mut World) -> R<()> {
    w.crash.arm(CrashPoint::AfterCasBeforeOutbox, 0);
    w.crash.arm(CrashPoint::AfterCasBeforeOutbox, 3);
    w.crash.arm(CrashPoint::AfterCasBeforeOutbox, 6);
    for _ in 0..12 {
        w.spawn_job(6);
    }
    for _ in 0..200 {
        w.traffic_step(false, 12).await?;
    }
    // Delivery under repeated replica deaths.
    for _ in 0..300 {
        w.tick();
        let prim = w.primaries();
        if prim.is_empty() {
            continue;
        }
        let ri = prim[w.rng.index(prim.len())];
        w.deliver_one(ri);
        w.check_step().await?;
    }
    w.drain().await
}

/// A rolling upgrade: clients and replicas speak different versions; refusals have no effect.
async fn version_skew(w: &mut World) -> R<()> {
    w.replicas[0].proto_lo = 2;
    w.replicas[1].proto_hi = 2;
    for _ in 0..24 {
        w.spawn_job(6);
    }
    for i in 0..400 {
        w.traffic_step(false, 30).await?;
        match i {
            100 => w.replicas[1].proto_lo = 2,
            200 => w.replicas[2].proto_lo = 2,
            300 => w.replicas[1].proto_hi = 3,
            _ => {}
        }
    }
    w.drain().await?;
    Ok(())
}

/// A cache role in front of a dead upstream must not stampede it, and must recover.
async fn cache_stampede(w: &mut World) -> R<()> {
    for _ in 0..10 {
        w.spawn_job(6);
    }
    w.drain().await?;
    let Some((tip, _)) = w.master_tip().await? else {
        return viol("nothing landed");
    };
    let mut keys = Vec::new();
    for (id, cs) in w.chain(tip).await? {
        keys.push(SimKey::Cs(id));
        keys.extend(
            cs.needs()
                .into_iter()
                .filter(|k| matches!(k, SimKey::File(_))),
        );
    }
    w.quiet = false;
    w.ctl.set_plan(FaultPlan {
        down: true,
        ..FaultPlan::default()
    });
    for _ in 0..300 {
        w.tick();
        let caches: Vec<usize> = (0..w.replicas.len())
            .filter(|&i| w.replicas[i].role == Role::Cache && w.replicas[i].up)
            .collect();
        if caches.is_empty() {
            continue;
        }
        let ri = caches[w.rng.index(caches.len())];
        let key = keys[w.rng.index(keys.len())];
        let n = 1 + w.rng.index(80);
        w.cache_read_burst(ri, key, n).await?;
        if w.rng.chance(1, 60) {
            w.crash_replica(ri, CrashPoint::BeforeCas);
        }
        w.check_step().await?;
    }
    // The upstream returns: after the backoff every key can be read.
    w.ctl.set_plan(FaultPlan::default());
    for i in 0..w.replicas.len() {
        if !w.replicas[i].up {
            w.restart(i);
        }
    }
    w.clock.advance_ms(10 * 60_000);
    let ri = (0..w.replicas.len())
        .find(|&i| w.replicas[i].role == Role::Cache)
        .unwrap_or(1);
    for k in keys {
        w.clock.advance_ms(60);
        if w.cache_read_burst(ri, k, 3).await? == crate::cache::Served::Shed {
            // The shed limit is per second; a short wait clears it.
            w.clock.advance_ms(1000);
            if w.cache_read_burst(ri, k, 3).await? == crate::cache::Served::Shed {
                return viol("a cache never recovered after the upstream came back");
            }
        }
        w.check_step().await?;
    }
    Ok(())
}

/// A git import killed at random object writes, rerun by two importers, equals a clean import.
async fn import_crash(w: &mut World, bugs: Bugs) -> R<()> {
    let n = 5 + w.rng.index(8);
    let commits: Vec<GitCommit> = history(&mut w.rng.fork(), n);
    for _ in 0..4 {
        let after = u32::try_from(w.rng.below(6 * n as u64)).unwrap_or(0);
        w.crash.arm(CrashPoint::MidImport, after);
    }
    let mut imps = [Importer::default(), Importer::default()];
    for _ in 0..20_000 {
        if w.import_done(&commits) {
            break;
        }
        w.tick();
        let which = w.rng.index(2);
        match w.import_step(&mut imps[which], &commits).await {
            Ok(()) => {}
            Err(Fail::Transient) => {}
            Err(Fail::Crashed(_)) => {
                w.stats.crashes += 1;
                imps[which] = Importer::default();
            }
            Err(e) => return Err(e),
        }
        w.check_step().await?;
    }
    if !w.import_done(&commits) {
        return viol("the import never finished");
    }
    let got = w.fingerprint().await?;
    let mut clean = World::new(w.seed, &[Role::Primary], FaultPlan::default(), bugs);
    let mut imp = Importer::default();
    while !clean.import_done(&commits) {
        clean.import_step(&mut imp, &commits).await?;
    }
    if got != clean.fingerprint().await? {
        return viol("a crashed-and-rerun import differs from an uninterrupted one");
    }
    Ok(())
}
