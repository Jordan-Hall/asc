//! The commit-cloud scenario (SPEC §17.2, §21.3.3, §21.3.4; PLAN B21): two machines of one user
//! share one cloud workspace through the real `asc_engine::branchless::sync` (merge plus CAS
//! retry), while the machines go offline and come back, responses are lost after a write, and a
//! third writer bumps the workspace version between the machines' `get` and `update`.
//!
//! Invariants, checked after every step: the workspace version never goes down; a failed sync
//! changes nothing locally; a sync that succeeds leaves the machine's visible heads equal to the
//! workspace's. After the drain (both machines online, no faults) both machines and the server
//! hold exactly the heads that no machine hid: nothing lost, nothing resurrected.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_engine::branchless::sync::{
    CloudDoc, CloudHead, CloudStore, CloudUpdate, Error, LocalState, StoreError, SyncOptions, sync,
};

use crate::model::{R, viol};
use crate::rng::Rng;
use crate::world::World;

/// Deliberate defects, to prove the invariants have teeth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CloudBug {
    #[default]
    None,
    /// A machine replaces the workspace with its own heads instead of merging (loses the other
    /// machine's heads).
    Overwrite,
}

/// Counters of one run.
#[derive(Clone, Debug, Default)]
pub struct CloudReport {
    pub steps: u64,
    pub syncs: u64,
    pub synced: u64,
    pub offline: u64,
    pub lost_responses: u64,
    pub version_conflicts: u64,
    pub gave_up: u64,
    pub trace: u64,
}

type Preds = HashMap<ChangesetId, Vec<ChangesetId>>;

#[derive(Default)]
struct Inner {
    version: u64,
    heads: BTreeMap<ChangesetId, CloudHead>,
    ops: HashMap<String, u64>,
    offline: bool,
    lose_next_update: bool,
    interfere: u32,
    offline_refused: u64,
    lost: u64,
    conflicts: u64,
}

/// The workspace on the server: version, heads and an idempotency record per `x-asc-op-id`.
#[derive(Default)]
struct SimCloud(Mutex<Inner>);

impl SimCloud {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn visible(&self) -> BTreeSet<ChangesetId> {
        visible(&self.inner().heads)
    }
}

#[async_trait]
impl CloudStore for SimCloud {
    async fn get(&self) -> Result<CloudDoc, StoreError> {
        let mut g = self.inner();
        if g.offline {
            g.offline_refused += 1;
            return Err(StoreError::Other("offline".into()));
        }
        let doc = CloudDoc {
            version: g.version,
            heads: g.heads.values().cloned().collect(),
        };
        if g.interfere > 0 {
            // Another writer lands after this read: the CAS below must lose.
            g.interfere -= 1;
            g.version += 1;
        }
        Ok(doc)
    }

    async fn update(&self, op_id: &str, u: &CloudUpdate) -> Result<u64, StoreError> {
        let mut g = self.inner();
        if g.offline {
            g.offline_refused += 1;
            return Err(StoreError::Other("offline".into()));
        }
        if let Some(v) = g.ops.get(op_id) {
            return Ok(*v);
        }
        if u.base_version != g.version {
            g.conflicts += 1;
            return Err(StoreError::VersionConflict { current: g.version });
        }
        for h in &u.add {
            g.heads.insert(h.cs, h.clone());
        }
        for c in &u.remove {
            g.heads.remove(c);
        }
        for c in &u.hide {
            if let Some(h) = g.heads.get_mut(c) {
                h.hidden_at = Some(1);
            }
        }
        for c in &u.unhide {
            if let Some(h) = g.heads.get_mut(c) {
                h.hidden_at = None;
            }
        }
        g.version += 1;
        let v = g.version;
        g.ops.insert(op_id.to_owned(), v);
        if g.lose_next_update {
            g.lose_next_update = false;
            g.lost += 1;
            return Err(StoreError::Other("response lost".into()));
        }
        Ok(v)
    }
}

fn visible(map: &BTreeMap<ChangesetId, CloudHead>) -> BTreeSet<ChangesetId> {
    map.values()
        .filter(|h| h.is_visible())
        .map(|h| h.cs)
        .collect()
}

fn cs(n: u32) -> ChangesetId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_le_bytes());
    ChangesetId::from_bytes(b)
}

fn head(n: u32) -> CloudHead {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&n.to_le_bytes());
    CloudHead {
        cs: cs(n),
        change: ChangeId::from_bytes(b),
        labels: vec![],
        added: i64::from(n),
        hidden_at: None,
    }
}

#[derive(Default)]
struct Machine {
    heads: BTreeMap<ChangesetId, CloudHead>,
    base: BTreeSet<ChangesetId>,
    offline: bool,
}

impl Machine {
    fn vis(&self) -> BTreeSet<ChangesetId> {
        visible(&self.heads)
    }

    /// One sync. `Ok(true)` when it finished; `Ok(false)` when it failed and changed nothing.
    async fn sync(
        &mut self,
        store: &SimCloud,
        preds: &Preds,
        bug: CloudBug,
    ) -> Result<bool, String> {
        if bug == CloudBug::Overwrite {
            return self.overwrite(store).await;
        }
        let state = LocalState {
            heads: self.heads.values().cloned().collect(),
            base: self.base.clone(),
            preds: preds.clone(),
        };
        let before = (self.heads.clone(), self.base.clone());
        match sync(store, &state, SyncOptions::default()).await {
            Ok(out) => {
                for h in out.local.add {
                    self.heads.insert(h.cs, h);
                }
                for c in out.local.hide {
                    if let Some(h) = self.heads.get_mut(&c) {
                        h.hidden_at = Some(2);
                    }
                }
                for c in out.local.unhide {
                    if let Some(h) = self.heads.get_mut(&c) {
                        h.hidden_at = None;
                    }
                }
                self.base = out.base;
                Ok(true)
            }
            Err(Error::Store(_) | Error::Conflict(_)) => {
                if before != (self.heads.clone(), self.base.clone()) {
                    return Err("a failed sync changed the local rows".into());
                }
                Ok(false)
            }
            Err(e) => Err(format!("sync: {e}")),
        }
    }

    /// The defect: push the local visible set over the workspace without merging.
    async fn overwrite(&mut self, store: &SimCloud) -> Result<bool, String> {
        let Ok(doc) = store.get().await else {
            return Ok(false);
        };
        let mine = self.vis();
        let u = CloudUpdate {
            base_version: doc.version,
            add: self
                .heads
                .values()
                .filter(|h| h.is_visible())
                .cloned()
                .collect(),
            remove: doc
                .heads
                .iter()
                .map(|h| h.cs)
                .filter(|c| !mine.contains(c))
                .collect(),
            ..CloudUpdate::default()
        };
        if store.update(&u.op_id(), &u).await.is_err() {
            return Ok(false);
        }
        self.base = mine;
        Ok(true)
    }
}

/// Runs `steps` random interleavings of two machines on one workspace, then drains and checks.
pub async fn run_cloud(
    seed: u64,
    steps: u64,
    bug: CloudBug,
    lose_responses: bool,
) -> Result<CloudReport, String> {
    let mut rng = Rng::new(seed ^ 0xC10D_C10D);
    cloud_with(&mut rng, steps, bug, lose_responses).await
}

async fn cloud_with(
    rng: &mut Rng,
    steps: u64,
    bug: CloudBug,
    lose_responses: bool,
) -> Result<CloudReport, String> {
    let store = SimCloud::default();
    let mut ms = [Machine::default(), Machine::default()];
    let mut preds: Preds = HashMap::new();
    let mut expect: BTreeSet<ChangesetId> = BTreeSet::new();
    let mut next = 1u32;
    let mut rep = CloudReport {
        trace: 0xcbf2_9ce4_8422_2325,
        ..CloudReport::default()
    };
    let mut last_version = 0;
    for step in 0..steps {
        rep.steps += 1;
        let m = rng.index(2);
        let roll = rng.below(100);
        match roll {
            0..=27 => {
                let n = next;
                next += 1;
                let vis: Vec<_> = ms[m].vis().into_iter().collect();
                if rng.chance(1, 2) && !vis.is_empty() {
                    let p = vis[rng.index(vis.len())];
                    if let Some(h) = ms[m].heads.get_mut(&p) {
                        h.hidden_at = Some(3);
                    }
                    preds.insert(cs(n), vec![p]);
                    expect.remove(&p);
                }
                ms[m].heads.insert(cs(n), head(n));
                expect.insert(cs(n));
            }
            28..=39 => {
                let vis: Vec<_> = ms[m].vis().into_iter().collect();
                if !vis.is_empty() {
                    let p = vis[rng.index(vis.len())];
                    if let Some(h) = ms[m].heads.get_mut(&p) {
                        h.hidden_at = Some(3);
                    }
                    expect.remove(&p);
                }
            }
            40..=51 => ms[m].offline = !ms[m].offline,
            _ => {
                rep.syncs += 1;
                {
                    let mut g = store.inner();
                    g.offline = ms[m].offline;
                    g.lose_next_update = lose_responses && rng.chance(1, 6);
                    g.interfere = if rng.chance(1, 4) {
                        1 + u32::try_from(rng.below(3)).unwrap_or(0)
                    } else {
                        0
                    };
                }
                let done = ms[m].sync(&store, &preds, bug).await?;
                {
                    // Faults armed for this sync end with it.
                    let mut g = store.inner();
                    g.lose_next_update = false;
                    g.interfere = 0;
                }
                if done {
                    rep.synced += 1;
                    if bug == CloudBug::None && ms[m].vis() != store.visible() {
                        return Err(format!(
                            "step {step}: machine {m} finished a sync but its visible heads differ from the workspace"
                        ));
                    }
                } else {
                    rep.gave_up += 1;
                }
            }
        }
        let v = store.inner().version;
        if v < last_version {
            return Err(format!("step {step}: the workspace version went down"));
        }
        last_version = v;
        let (a, b) = (ms[0].vis().len() as u64, ms[1].vis().len() as u64);
        for x in [step, roll, v, a, b] {
            rep.trace = (rep.trace ^ x).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    // Drain: reconnect, end the faults, sync until quiet.
    for m in &mut ms {
        m.offline = false;
    }
    {
        let mut g = store.inner();
        g.offline = false;
        g.lose_next_update = false;
        g.interfere = 0;
    }
    for _ in 0..3 {
        for m in &mut ms {
            if !m.sync(&store, &preds, bug).await? {
                return Err("a sync failed with no faults".into());
            }
        }
    }
    {
        let g = store.inner();
        rep.offline = g.offline_refused;
        rep.lost_responses = g.lost;
        rep.version_conflicts = g.conflicts;
    }
    let remote = store.visible();
    for (name, got) in [
        ("machine 0", ms[0].vis()),
        ("machine 1", ms[1].vis()),
        ("workspace", remote),
    ] {
        if got != expect {
            return Err(format!(
                "{name} did not converge: {} head(s) lost, {} resurrected {:?} (expected {})",
                expect.difference(&got).count(),
                got.difference(&expect).count(),
                got.difference(&expect)
                    .map(|c| c.as_bytes()[0])
                    .collect::<Vec<_>>(),
                expect.len()
            ));
        }
    }
    Ok(rep)
}

/// The scenario inside the world: the world's RNG drives it, the world's trace records it.
pub async fn cloud(w: &mut World) -> R<()> {
    let mut rng = w.rng.fork();
    match cloud_with(&mut rng, 1000, CloudBug::None, true).await {
        Ok(rep) => {
            w.step += rep.steps;
            w.note(0xC10D, rep.trace);
            if rep.synced == 0 {
                return viol("no cloud sync ever finished");
            }
            Ok(())
        }
        Err(e) => viol(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    #[test]
    fn cloud_two_machines_converge_after_1000_random_interleavings() {
        let mut offline = 0;
        let mut conflicts = 0;
        for seed in 1..=60 {
            let rep = block(run_cloud(seed, 1000, CloudBug::None, false))
                .unwrap_or_else(|e| panic!("ASC_SIM_SEED={seed} ASC_SIM_SCENARIO=cloud: {e}"));
            assert_eq!(rep.steps, 1000);
            offline += rep.offline;
            conflicts += rep.version_conflicts;
        }
        assert!(offline > 100, "machines never went offline: {offline}");
        assert!(conflicts > 20, "the CAS never lost a race: {conflicts}");
    }

    /// A response lost after a successful write leaves the machine's `cloud_base` stale. If the
    /// other machine hides the head before the next sync, the three-way merge would read the head
    /// as a local addition and unhide it on the server. `asc_engine::branchless::sync::sync`
    /// sends the same (idempotent) request again after an ambiguous update error, which settles
    /// the base within the same sync.
    #[test]
    fn cloud_lost_response_after_write_converges() {
        for seed in 1..=60 {
            let rep = block(run_cloud(seed, 1000, CloudBug::None, true))
                .unwrap_or_else(|e| panic!("ASC_SIM_SEED={seed} ASC_SIM_SCENARIO=cloud: {e}"));
            assert!(rep.steps == 1000);
        }
    }

    #[test]
    fn cloud_a_seed_replays_identically() {
        let a = block(run_cloud(7, 1000, CloudBug::None, false)).expect("a");
        let b = block(run_cloud(7, 1000, CloudBug::None, false)).expect("b");
        assert_eq!(a.trace, b.trace);
        let c = block(run_cloud(8, 1000, CloudBug::None, false)).expect("c");
        assert_ne!(a.trace, c.trace);
    }

    #[test]
    fn cloud_a_client_that_overwrites_the_workspace_is_caught() {
        let caught = (1..=20).any(|seed| {
            block(run_cloud(seed, 1000, CloudBug::Overwrite, false))
                .err()
                .is_some_and(|e| e.contains("did not converge"))
        });
        assert!(caught, "overwriting the workspace was never caught");
    }
}
