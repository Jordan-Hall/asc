//! Mark and sweep as a state machine (SPEC 3.5), so pushes and fetches interleave with it and a
//! crash can stop it at `mid_gc_mark` or `mid_gc_sweep`.

use std::collections::BTreeSet;

use asc_store::key::KeyPrefix;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::StreamExt;

use crate::faults::CrashPoint;
use crate::model::{Cs, Fail, MIN_AGE_MS, R, REPO, SimKey, oerr, rerr, viol};
use crate::world::World;

#[derive(Debug, Default)]
pub enum Gc {
    #[default]
    Idle,
    Marking {
        start_ms: i64,
        queue: Vec<SimKey>,
        marked: BTreeSet<SimKey>,
    },
    Sweeping {
        start_ms: i64,
        marked: BTreeSet<SimKey>,
        todo: Vec<SimKey>,
    },
}

impl World {
    pub fn gc_busy(&self) -> bool {
        !matches!(self.gc, Gc::Idle)
    }

    /// One unit of GC work on a live primary `ri`.
    pub async fn gc_step(&mut self, ri: usize) -> R<()> {
        let res = self.gc_step_inner().await;
        match res {
            Err(Fail::Crashed(p)) => {
                self.crash_replica(ri, p);
                Ok(())
            }
            Err(Fail::Transient) => Ok(()),
            other => other,
        }
    }

    async fn roots(&self) -> R<Vec<SimKey>> {
        let all = self.refs.list(REPO, "").await.map_err(rerr)?;
        Ok(all
            .into_iter()
            .filter_map(|e| match e.target {
                asc_core::id::RefTarget::Changeset(c) => Some(SimKey::Cs(*c.as_bytes())),
                asc_core::id::RefTarget::Tag(_) => None,
            })
            .collect())
    }

    async fn refs_of(&self, k: SimKey) -> R<Vec<SimKey>> {
        match k {
            SimKey::File(_) => Ok(vec![]),
            SimKey::Cs(id) => {
                // The mark reads the object itself: verified, decoded, never trusted blindly.
                let Some(b) = self
                    .objects
                    .get(&SimKey::Cs(id).obj())
                    .await
                    .map_err(oerr)?
                else {
                    return Ok(vec![]);
                };
                Ok(Cs::decode(&b).map(|c| c.needs()).unwrap_or_default())
            }
        }
    }

    async fn gc_step_inner(&mut self) -> R<()> {
        match std::mem::take(&mut self.gc) {
            Gc::Idle => {
                let roots = self.roots().await?;
                self.gc = Gc::Marking {
                    start_ms: self.clock.now_ms(),
                    queue: roots,
                    marked: BTreeSet::new(),
                };
                Ok(())
            }
            Gc::Marking {
                start_ms,
                mut queue,
                mut marked,
            } => {
                for _ in 0..3 {
                    let Some(k) = queue.pop() else { break };
                    if !marked.insert(k) {
                        continue;
                    }
                    let kids = match self.refs_of(k).await {
                        Ok(v) => v,
                        Err(e) => {
                            // Put the state back so the next step retries this key.
                            marked.remove(&k);
                            queue.push(k);
                            self.gc = Gc::Marking {
                                start_ms,
                                queue,
                                marked,
                            };
                            return Err(e);
                        }
                    };
                    queue.extend(kids.into_iter().filter(|c| !marked.contains(c)));
                    if self.crash.hit(CrashPoint::MidGcMark) {
                        // A partial mark is discarded; the state stays Idle.
                        return Err(Fail::Crashed(CrashPoint::MidGcMark));
                    }
                }
                if queue.is_empty() {
                    let todo = self.sweep_candidates(start_ms, &marked).await?;
                    self.gc = Gc::Sweeping {
                        start_ms,
                        marked,
                        todo,
                    };
                } else {
                    self.gc = Gc::Marking {
                        start_ms,
                        queue,
                        marked,
                    };
                }
                Ok(())
            }
            Gc::Sweeping {
                start_ms,
                mut marked,
                mut todo,
            } => {
                if !self.bugs.gc_ignores_age {
                    // Refs that moved after the mark: mark their new closure before deleting.
                    let mut q = self.roots().await?;
                    while let Some(k) = q.pop() {
                        if marked.insert(k) {
                            q.extend(self.refs_of(k).await?);
                        }
                    }
                }
                for _ in 0..3 {
                    let Some(k) = todo.pop() else { break };
                    if marked.contains(&k) {
                        continue;
                    }
                    if self.crash.hit(CrashPoint::MidGcSweep) {
                        return Err(Fail::Crashed(CrashPoint::MidGcSweep));
                    }
                    if self.objects.delete(&k.obj()).await.is_ok() {
                        self.born.remove(&k);
                        self.stats.swept += 1;
                        self.chk.verified.clear();
                    } else {
                        todo.push(k);
                        break;
                    }
                }
                if !todo.is_empty() {
                    self.gc = Gc::Sweeping {
                        start_ms,
                        marked,
                        todo,
                    };
                }
                Ok(())
            }
        }
    }

    /// Unmarked objects old enough to sweep. `min_age` protects objects of in-flight pushes.
    async fn sweep_candidates(&self, start_ms: i64, marked: &BTreeSet<SimKey>) -> R<Vec<SimKey>> {
        let mut out = Vec::new();
        let mut s = self.raw_objects.list(KeyPrefix::all());
        while let Some(item) = s.next().await {
            let (k, _) = item.map_err(oerr)?;
            let Some(sk) = SimKey::of_obj(&k) else {
                return viol(format!(
                    "the store holds a key the simulation never wrote: {k}"
                ));
            };
            let born = self.born.get(&sk).copied().unwrap_or(0);
            let old = self.bugs.gc_ignores_age || born <= start_ms - MIN_AGE_MS;
            if old && !marked.contains(&sk) {
                out.push(sk);
            }
        }
        Ok(out)
    }

    /// Runs a whole collection without interleaving (used to settle at the end of a run).
    pub async fn gc_to_completion(&mut self, ri: usize) -> R<()> {
        for _ in 0..10_000 {
            self.gc_step(ri).await?;
            if !self.gc_busy() {
                return Ok(());
            }
        }
        viol("gc does not finish")
    }
}
