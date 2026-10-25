//! Commit-cloud head sync: three-way merge and CAS retry (SPEC §21.3.4).
//!
//! The engine works on its own [`CloudDoc`] types and a [`CloudStore`] trait (get and
//! compare-and-swap update). The `cloud.proto` conversions and the RPC-backed store live in the
//! daemon, because asc-engine cannot use asc-rpc.
//!
//! The merge is a pure function ([`merge`]) over the local rows, the last synced set
//! (`cloud_base`) and the remote workspace. Everything read from the remote is untrusted, so it
//! is bounded in rows, heads, labels and predecessor steps before it is used.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use async_trait::async_trait;
use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;

/// Server `max_heads_per_workspace` (SPEC §21.3.2); a merged set above it is refused.
pub const MAX_HEADS: usize = 10_000;
/// Rows (visible and hidden) accepted in one remote workspace document.
pub const MAX_ROWS: usize = 100_000;
/// Labels per head, and bytes per label (SPEC §21.3.5).
pub const MAX_LABELS: usize = 8;
pub const MAX_LABEL_BYTES: usize = 64;
/// Predecessor steps followed per sync.
pub const MAX_PRED_STEPS: usize = 100_000;
/// Sync attempts before a version conflict is reported (SPEC §21.3.4 step 4).
pub const MAX_ATTEMPTS: usize = 3;

/// Failures from a sync.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The store failed.
    #[error("cloud store: {0}")]
    Store(String),
    /// Every attempt lost the CAS race.
    #[error("cloud workspace changed under {0} attempts (VERSION_CONFLICT)")]
    Conflict(usize),
    #[error("{0} exceeds the limit of {1}")]
    TooLarge(&'static str, usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A store failure, split so the sync can retry a lost CAS.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `VERSION_CONFLICT`; `current` is the server's version.
    #[error("version conflict (current {current})")]
    VersionConflict { current: u64 },
    #[error("{0}")]
    Other(String),
}

/// One head of a workspace, as the engine sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudHead {
    pub cs: ChangesetId,
    pub change: ChangeId,
    /// Display text only; they never become refs.
    pub labels: Vec<String>,
    pub added: i64,
    pub hidden_at: Option<i64>,
}

impl CloudHead {
    pub fn is_visible(&self) -> bool {
        self.hidden_at.is_none()
    }

    fn clean(mut self) -> Self {
        self.labels.retain(|l| valid_label(l));
        self.labels.truncate(MAX_LABELS);
        self
    }
}

fn valid_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= MAX_LABEL_BYTES
        && l.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

/// A workspace at one version, hidden rows included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudDoc {
    pub version: u64,
    pub heads: Vec<CloudHead>,
}

/// One `UpdateWorkspace` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudUpdate {
    pub base_version: u64,
    pub add: Vec<CloudHead>,
    pub remove: Vec<ChangesetId>,
    pub hide: Vec<ChangesetId>,
    pub unhide: Vec<ChangesetId>,
}

impl CloudUpdate {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty()
            && self.remove.is_empty()
            && self.hide.is_empty()
            && self.unhide.is_empty()
    }

    /// A stable id for the request, so a retried send is idempotent (`x-asc-op-id`).
    pub fn op_id(&self) -> String {
        let mut h = blake3::Hasher::new_derive_key("asc v1 cloud update");
        h.update(&self.base_version.to_le_bytes());
        let adds: Vec<ChangesetId> = self.add.iter().map(|x| x.cs).collect();
        for (tag, list) in [
            (b'a', &adds),
            (b'r', &self.remove),
            (b'h', &self.hide),
            (b'u', &self.unhide),
        ] {
            h.update(&[tag]);
            h.update(&(list.len() as u64).to_le_bytes());
            for cs in list {
                h.update(cs.as_bytes());
            }
        }
        h.finalize().to_hex().to_string()
    }
}

/// The remote workspace.
#[async_trait]
pub trait CloudStore: Send + Sync {
    /// The whole workspace, hidden rows included.
    async fn get(&self) -> Result<CloudDoc, StoreError>;
    /// Applies `update` if `update.base_version` is still current; returns the new version.
    async fn update(&self, op_id: &str, update: &CloudUpdate) -> Result<u64, StoreError>;
}

/// Local inputs to one sync.
#[derive(Clone, Debug, Default)]
pub struct LocalState {
    /// Every local `draft_heads` row.
    pub heads: Vec<CloudHead>,
    /// The visible set at the last successful sync (`cloud_base`).
    pub base: BTreeSet<ChangesetId>,
    /// Direct predecessors of heads, from their `ChangeMeta`.
    pub preds: HashMap<ChangesetId, Vec<ChangesetId>>,
}

#[derive(Clone, Copy, Debug)]
pub struct SyncOptions {
    pub max_attempts: usize,
    pub max_heads: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            max_heads: MAX_HEADS,
        }
    }
}

/// What the local rows must change after a sync.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalChanges {
    /// Remote heads to insert (their metadata is pulled next, SPEC §21.3.4 step 5).
    pub add: Vec<CloudHead>,
    /// Heads to set `hidden_at` on; they stay recoverable.
    pub hide: Vec<ChangesetId>,
    pub unhide: Vec<ChangesetId>,
}

/// The result of a merge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Merged {
    /// The merged visible set; it becomes the next `cloud_base`.
    pub visible: BTreeSet<ChangesetId>,
    pub local: LocalChanges,
    /// The request to send, without `base_version`.
    pub remote: CloudUpdate,
}

/// A finished sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub version: u64,
    pub attempts: usize,
    pub base: BTreeSet<ChangesetId>,
    pub local: LocalChanges,
}

fn index(heads: &[CloudHead]) -> BTreeMap<ChangesetId, CloudHead> {
    let mut out = BTreeMap::new();
    for h in heads {
        out.entry(h.cs).or_insert_with(|| h.clone().clean());
    }
    out
}

fn visible(map: &BTreeMap<ChangesetId, CloudHead>) -> BTreeSet<ChangesetId> {
    map.values()
        .filter(|h| h.is_visible())
        .map(|h| h.cs)
        .collect()
}

/// The three-way merge of SPEC §21.3.4 step 3.
///
/// A head removed on one side since `base` is removed on the other; otherwise the sets are
/// united. A remote head that has a local head in its predecessor closure replaces it.
pub fn merge(local: &LocalState, remote: &CloudDoc, max_heads: usize) -> Result<Merged> {
    if remote.heads.len() > MAX_ROWS {
        return Err(Error::TooLarge("remote workspace rows", MAX_ROWS));
    }
    let lmap = index(&local.heads);
    let rmap = index(&remote.heads);
    let (lv, rv) = (visible(&lmap), visible(&rmap));

    let mut merged: BTreeSet<ChangesetId> = lv
        .union(&rv)
        .copied()
        .filter(|c| !(local.base.contains(c) && !(lv.contains(c) && rv.contains(c))))
        .collect();

    let mut steps = 0usize;
    for &r in &rv {
        if !merged.contains(&r) {
            continue;
        }
        let mut seen = BTreeSet::new();
        let mut stack: Vec<ChangesetId> = local.preds.get(&r).cloned().unwrap_or_default();
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > MAX_PRED_STEPS {
                return Err(Error::TooLarge("predecessor steps", MAX_PRED_STEPS));
            }
            if p == r || !seen.insert(p) {
                continue;
            }
            if lv.contains(&p) {
                merged.remove(&p);
            }
            if let Some(next) = local.preds.get(&p) {
                stack.extend(next.iter().copied());
            }
        }
    }
    if merged.len() > max_heads {
        return Err(Error::TooLarge("cloud heads", max_heads));
    }

    let mut out = Merged {
        visible: merged.clone(),
        ..Merged::default()
    };
    for c in &merged {
        match lmap.get(c) {
            Some(h) if !h.is_visible() => out.local.unhide.push(*c),
            Some(_) => {}
            None => out.local.add.push(rmap[c].clone()),
        }
        if !rv.contains(c) {
            match rmap.get(c) {
                Some(_) => out.remote.unhide.push(*c),
                None => out.remote.add.push(lmap[c].clone()),
            }
        }
    }
    for c in lv.difference(&merged) {
        out.local.hide.push(*c);
    }
    for c in rv.difference(&merged) {
        if lv.contains(c) {
            out.remote.remove.push(*c);
        } else {
            out.remote.hide.push(*c);
        }
    }
    Ok(out)
}

/// Runs steps 2 to 4 of SPEC §21.3.4: get, merge, CAS update, and repeat on a lost race.
///
/// Upload (step 1) and the metadata pull (step 5) belong to the caller.
pub async fn sync<S: CloudStore>(
    store: &S,
    local: &LocalState,
    opts: SyncOptions,
) -> Result<Outcome> {
    let attempts = opts.max_attempts.max(1);
    for attempt in 1..=attempts {
        let doc = store.get().await.map_err(|e| Error::Store(e.to_string()))?;
        let merged = merge(local, &doc, opts.max_heads)?;
        let finish = |version| Outcome {
            version,
            attempts: attempt,
            base: merged.visible.clone(),
            local: merged.local.clone(),
        };
        if merged.remote.is_empty() {
            return Ok(finish(doc.version));
        }
        let update = CloudUpdate {
            base_version: doc.version,
            ..merged.remote.clone()
        };
        let op_id = update.op_id();
        let mut sent = store.update(&op_id, &update).await;
        if matches!(sent, Err(StoreError::Other(_))) {
            // The failure is ambiguous: the server may have applied the update and lost the
            // answer. Sending the same request again is safe, because the op id makes it
            // idempotent: an applied update answers with its version, an unapplied one is
            // applied or loses the CAS. Giving up here would leave `cloud_base` behind the
            // server, and a head another machine hid meanwhile would then read as a local
            // addition and come back.
            sent = store.update(&op_id, &update).await;
        }
        match sent {
            Ok(version) => return Ok(finish(version)),
            Err(StoreError::VersionConflict { .. }) => continue,
            Err(e) => return Err(Error::Store(e.to_string())),
        }
    }
    Err(Error::Conflict(attempts))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

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
    struct Mem {
        state: Mutex<(u64, BTreeMap<ChangesetId, CloudHead>)>,
        /// Concurrent writers to simulate: bump the version once per `get`, this many times.
        interfere: Mutex<u32>,
        ops: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CloudStore for Mem {
        async fn get(&self) -> Result<CloudDoc, StoreError> {
            let mut g = self.state.lock().unwrap();
            let doc = CloudDoc {
                version: g.0,
                heads: g.1.values().cloned().collect(),
            };
            let mut n = self.interfere.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                g.0 += 1;
            }
            Ok(doc)
        }

        async fn update(&self, op_id: &str, u: &CloudUpdate) -> Result<u64, StoreError> {
            let mut g = self.state.lock().unwrap();
            if u.base_version != g.0 {
                return Err(StoreError::VersionConflict { current: g.0 });
            }
            self.ops.lock().unwrap().push(op_id.to_owned());
            for h in &u.add {
                g.1.insert(h.cs, h.clone());
            }
            for c in &u.remove {
                g.1.remove(c);
            }
            for c in &u.hide {
                if let Some(h) = g.1.get_mut(c) {
                    h.hidden_at = Some(1);
                }
            }
            for c in &u.unhide {
                if let Some(h) = g.1.get_mut(c) {
                    h.hidden_at = None;
                }
            }
            g.0 += 1;
            Ok(g.0)
        }
    }

    type Preds = HashMap<ChangesetId, Vec<ChangesetId>>;

    #[derive(Default)]
    struct Machine {
        heads: BTreeMap<ChangesetId, CloudHead>,
        base: BTreeSet<ChangesetId>,
    }

    impl Machine {
        fn vis(&self) -> BTreeSet<ChangesetId> {
            visible(&self.heads)
        }

        async fn sync(&mut self, store: &Mem, preds: &Preds) {
            let state = LocalState {
                heads: self.heads.values().cloned().collect(),
                base: self.base.clone(),
                preds: preds.clone(),
            };
            let out = sync(store, &state, SyncOptions::default()).await.unwrap();
            for h in out.local.add {
                self.heads.insert(h.cs, h);
            }
            for c in out.local.hide {
                self.heads.get_mut(&c).unwrap().hidden_at = Some(2);
            }
            for c in out.local.unhide {
                self.heads.get_mut(&c).unwrap().hidden_at = None;
            }
            self.base = out.base;
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    #[tokio::test]
    async fn two_machines_converge_after_random_interleaving() {
        for seed in 1..=4u64 {
            let store = Mem::default();
            let mut ms = [Machine::default(), Machine::default()];
            let mut preds = Preds::new();
            let mut expect = BTreeSet::new();
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut next = 1u32;
            for _ in 0..1000 {
                let m = rng.next(2);
                match rng.next(4) {
                    0 | 1 => {
                        let n = next;
                        next += 1;
                        let vis: Vec<_> = ms[m].vis().into_iter().collect();
                        if rng.next(2) == 0 && !vis.is_empty() {
                            let p = vis[rng.next(vis.len())];
                            ms[m].heads.get_mut(&p).unwrap().hidden_at = Some(3);
                            preds.insert(cs(n), vec![p]);
                            expect.remove(&p);
                        }
                        ms[m].heads.insert(cs(n), head(n));
                        expect.insert(cs(n));
                    }
                    2 => {
                        let vis: Vec<_> = ms[m].vis().into_iter().collect();
                        if !vis.is_empty() {
                            let p = vis[rng.next(vis.len())];
                            ms[m].heads.get_mut(&p).unwrap().hidden_at = Some(3);
                            expect.remove(&p);
                        }
                    }
                    _ => ms[m].sync(&store, &preds).await,
                }
            }
            for _ in 0..2 {
                ms[0].sync(&store, &preds).await;
                ms[1].sync(&store, &preds).await;
            }
            let remote = visible(&store.state.lock().unwrap().1);
            assert_eq!(ms[0].vis(), expect, "seed {seed} machine 0");
            assert_eq!(ms[1].vis(), expect, "seed {seed} machine 1");
            assert_eq!(remote, expect, "seed {seed} remote");
        }
    }

    #[tokio::test]
    async fn retries_a_lost_race_and_reports_after_three() {
        let store = Mem::default();
        *store.interfere.lock().unwrap() = 2;
        let local = LocalState {
            heads: vec![head(1)],
            ..LocalState::default()
        };
        let out = sync(&store, &local, SyncOptions::default()).await.unwrap();
        assert_eq!(out.attempts, 3);
        assert_eq!(
            visible(&store.state.lock().unwrap().1),
            BTreeSet::from([cs(1)])
        );

        let store = Mem::default();
        *store.interfere.lock().unwrap() = 3;
        let err = sync(&store, &local, SyncOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(3)));
    }

    /// Applies updates like `Mem`, remembers answers by op id (the server's idempotency), and
    /// loses the answer of the first applied update.
    #[derive(Default)]
    struct Lossy {
        inner: Mem,
        answers: Mutex<HashMap<String, u64>>,
        lose: Mutex<bool>,
    }

    #[async_trait]
    impl CloudStore for Lossy {
        async fn get(&self) -> Result<CloudDoc, StoreError> {
            self.inner.get().await
        }

        async fn update(&self, op_id: &str, u: &CloudUpdate) -> Result<u64, StoreError> {
            if let Some(v) = self.answers.lock().unwrap().get(op_id) {
                return Ok(*v);
            }
            let v = self.inner.update(op_id, u).await?;
            self.answers.lock().unwrap().insert(op_id.to_owned(), v);
            if std::mem::take(&mut *self.lose.lock().unwrap()) {
                return Err(StoreError::Other("response lost".into()));
            }
            Ok(v)
        }
    }

    #[tokio::test]
    async fn a_lost_update_response_does_not_resurrect_a_head_hidden_elsewhere() {
        let store = Lossy::default();
        *store.lose.lock().unwrap() = true;
        let local = LocalState {
            heads: vec![head(1)],
            ..LocalState::default()
        };
        // The update lands, its answer is lost; the same request is sent again and settles.
        let out = sync(&store, &local, SyncOptions::default()).await.unwrap();
        assert_eq!(out.base, BTreeSet::from([cs(1)]));
        // Another machine hides the head.
        let hide = CloudUpdate {
            base_version: out.version,
            hide: vec![cs(1)],
            ..CloudUpdate::default()
        };
        store.update(&hide.op_id(), &hide).await.unwrap();
        // The next sync reads the head as hidden elsewhere, not as a new local one.
        let local = LocalState {
            heads: vec![head(1)],
            base: out.base,
            ..LocalState::default()
        };
        let out = sync(&store, &local, SyncOptions::default()).await.unwrap();
        assert_eq!(out.local.hide, vec![cs(1)]);
        assert!(visible(&index(&store.get().await.unwrap().heads)).is_empty());
    }

    #[tokio::test]
    async fn clean_sync_sends_nothing() {
        let store = Mem::default();
        let out = sync(&store, &LocalState::default(), SyncOptions::default())
            .await
            .unwrap();
        assert_eq!(out.attempts, 1);
        assert!(store.ops.lock().unwrap().is_empty());
    }

    #[test]
    fn remote_head_replaces_local_pred() {
        let local = LocalState {
            heads: vec![head(1)],
            preds: HashMap::from([(cs(2), vec![cs(3)]), (cs(3), vec![cs(1)])]),
            ..LocalState::default()
        };
        let remote = CloudDoc {
            version: 5,
            heads: vec![head(2)],
        };
        let m = merge(&local, &remote, MAX_HEADS).unwrap();
        assert_eq!(m.visible, BTreeSet::from([cs(2)]));
        assert_eq!(m.local.hide, vec![cs(1)]);
        assert!(m.remote.remove.is_empty() || m.remote.remove == vec![cs(1)]);
    }

    #[test]
    fn untrusted_remote_is_bounded_and_cleaned() {
        let mut h = head(1);
        h.labels = (0..20)
            .map(|i| format!("l{i}"))
            .chain(["bad label".to_owned()])
            .collect();
        let local = LocalState::default();
        let m = merge(
            &local,
            &CloudDoc {
                version: 1,
                heads: vec![h.clone(), h],
            },
            10,
        )
        .unwrap();
        assert_eq!(m.local.add.len(), 1);
        assert_eq!(m.local.add[0].labels.len(), MAX_LABELS);

        let many = CloudDoc {
            version: 1,
            heads: (0..11).map(head).collect(),
        };
        assert!(matches!(merge(&local, &many, 10), Err(Error::TooLarge(..))));
    }

    #[test]
    fn op_id_is_stable_and_distinguishes_requests() {
        let a = CloudUpdate {
            base_version: 1,
            add: vec![head(1)],
            ..CloudUpdate::default()
        };
        let b = CloudUpdate {
            base_version: 2,
            ..a.clone()
        };
        assert_eq!(a.op_id(), a.clone().op_id());
        assert_ne!(a.op_id(), b.op_id());
    }
}
