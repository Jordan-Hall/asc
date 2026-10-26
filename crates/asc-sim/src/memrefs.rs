//! In-memory `Refs` with a full commit history, for the simulation (the model of `SqlRefs`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_store::refs::{
    Actor, Expect, RefConflict, RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey,
};
use async_trait::async_trait;
use uuid::Uuid;

use crate::faults::SimClock;

/// One committed ref change, as the invariants see it.
#[derive(Clone, Debug)]
pub struct Committed {
    pub seq: i64,
    pub name: RefName,
    /// The ref's version after this change (a delete also takes a version).
    pub version: u64,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
    pub op_id: Uuid,
    pub at_ms: i64,
    pub actor: String,
}

#[derive(Default)]
struct Inner {
    refs: BTreeMap<RefName, RefEntry>,
    versions: BTreeMap<RefName, u64>,
    log: Vec<Committed>,
}

pub struct MemRefs {
    inner: Mutex<Inner>,
    clock: Arc<SimClock>,
}

impl MemRefs {
    pub fn new(clock: Arc<SimClock>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            clock,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every committed change, oldest first.
    pub fn committed(&self) -> Vec<Committed> {
        self.lock().log.clone()
    }

    /// Committed changes from index `start` on.
    pub fn committed_from(&self, start: usize) -> Vec<Committed> {
        self.lock().log.iter().skip(start).cloned().collect()
    }

    pub fn log_len(&self) -> usize {
        self.lock().log.len()
    }

    /// The current refs.
    pub fn all(&self) -> Vec<RefEntry> {
        self.lock().refs.values().cloned().collect()
    }
}

#[async_trait]
impl Refs for MemRefs {
    async fn get(&self, _repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>, RefError> {
        Ok(self.lock().refs.get(name).cloned())
    }

    async fn list(&self, _repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>, RefError> {
        Ok(self
            .lock()
            .refs
            .values()
            .filter(|e| e.name.as_bytes().starts_with(prefix.as_bytes()))
            .cloned()
            .collect())
    }

    async fn update(&self, _repo: RepoKey, txn: &RefTxn) -> Result<(), RefError> {
        let mut g = self.lock();
        let mut refs = g.refs.clone();
        let mut versions = g.versions.clone();
        let mut staged = Vec::new();
        for u in &txn.updates {
            let cur = refs.get(&u.name).map(|e| e.target);
            let ok = match u.expect {
                Expect::Any => true,
                Expect::Absent => cur.is_none(),
                Expect::Value(t) => cur == Some(t),
            };
            if !ok {
                return Err(RefConflict {
                    name: u.name.clone(),
                    actual: cur,
                }
                .into());
            }
            let v = versions.entry(u.name.clone()).or_insert(0);
            *v += 1;
            let version = *v;
            match u.new {
                Some(t) => {
                    refs.insert(
                        u.name.clone(),
                        RefEntry {
                            name: u.name.clone(),
                            target: t,
                            version,
                            expires_at: u.expires_at,
                        },
                    );
                }
                None => {
                    refs.remove(&u.name);
                }
            }
            staged.push((u.name.clone(), version, cur, u.new));
        }
        g.refs = refs;
        g.versions = versions;
        let now = self.clock.now_ms();
        for (name, version, old, new) in staged {
            let seq = i64::try_from(g.log.len()).unwrap_or(i64::MAX) + 1;
            g.log.push(Committed {
                seq,
                name,
                version,
                old,
                new,
                op_id: txn.op_id,
                at_ms: now,
                actor: txn.actor.id.clone(),
            });
        }
        Ok(())
    }

    async fn log(
        &self,
        _repo: RepoKey,
        name: &RefName,
        limit: u32,
    ) -> Result<Vec<RefLogEntry>, RefError> {
        let g = self.lock();
        Ok(g.log
            .iter()
            .rev()
            .filter(|c| &c.name == name)
            .take(limit as usize)
            .map(|c| RefLogEntry {
                seq: c.seq,
                name: c.name.clone(),
                old: c.old,
                new: c.new,
                actor: Actor {
                    id: c.actor.clone(),
                    via: "sim".into(),
                },
                reason: "sim".into(),
                op_id: c.op_id,
                at: c.at_ms / 1000,
            })
            .collect())
    }
}

/// A shared handle, so the faulty wrapper and the invariant checks see one store.
#[derive(Clone)]
pub struct SharedRefs(pub Arc<MemRefs>);

#[async_trait]
impl Refs for SharedRefs {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>, RefError> {
        self.0.get(repo, name).await
    }
    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>, RefError> {
        self.0.list(repo, prefix).await
    }
    async fn update(&self, repo: RepoKey, txn: &RefTxn) -> Result<(), RefError> {
        self.0.update(repo, txn).await
    }
    async fn log(
        &self,
        repo: RepoKey,
        name: &RefName,
        limit: u32,
    ) -> Result<Vec<RefLogEntry>, RefError> {
        self.0.log(repo, name, limit).await
    }
}
