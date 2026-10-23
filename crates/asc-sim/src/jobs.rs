//! Push, land, outbox, webhook and index work done by the replicas (SPEC 9, 12).

use asc_core::id::RefTarget;
use asc_serve::outbox::backoff_secs;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs};
use uuid::Uuid;

use crate::faults::CrashPoint;
use crate::memrefs::Committed;
use crate::model::{
    Cs, Fail, MASTER, MIRROR, PROTO_MAX, PROTO_MIN, R, REPO, SimKey, rerr, rn, viol,
};
use crate::world::{Job, OutRow, Phase, World};

/// What one land attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Land {
    Done,
    Conflict,
    Reingest,
    Skew,
    Prepared,
}

fn actor(who: &str) -> Actor {
    Actor {
        id: who.into(),
        via: "serve".into(),
    }
}

impl World {
    /// Starts an agent's change: some files (paths overlap across agents on purpose).
    pub fn spawn_job(&mut self, paths: u64) -> usize {
        let change = self.next_change;
        self.next_change += 1;
        let nfiles = 1 + self.rng.index(3);
        let mut files = Vec::new();
        let mut contents = Vec::new();
        for _ in 0..nfiles {
            let path = u8::try_from(self.rng.below(paths)).unwrap_or(0);
            if files.iter().any(|(p, _)| *p == path) {
                continue;
            }
            let body =
                format!("change {change} path {path} rev {}", self.rng.next_u64()).into_bytes();
            let SimKey::File(id) = SimKey::file(&body) else {
                continue;
            };
            files.push((path, id));
            contents.push(body);
        }
        let op_id = self.new_op();
        let proto = PROTO_MIN
            + u8::try_from(self.rng.below(u64::from(PROTO_MAX - PROTO_MIN + 1))).unwrap_or(0);
        self.jobs.push(Job {
            change,
            op_id,
            proto,
            files,
            contents,
            cs: None,
            phase: Phase::Ingest(0),
            attempts: 0,
            skewed: 0,
            prep: None,
            started_ms: self.clock.now_ms(),
            abandoned: false,
        });
        self.jobs.len() - 1
    }

    pub fn runnable_jobs(&self) -> Vec<usize> {
        (0..self.jobs.len())
            .filter(|&i| self.jobs[i].phase != Phase::Done && !self.jobs[i].abandoned)
            .collect()
    }

    /// One phase of one job, on a random live primary.
    pub async fn step_job(&mut self, ji: usize) -> R<()> {
        let prim = self.primaries();
        if prim.is_empty() {
            return Ok(());
        }
        let ri = prim[self.rng.index(prim.len())];
        let res = self.job_phase(ji, ri).await;
        match res {
            Ok(()) => Ok(()),
            Err(Fail::Transient) => {
                self.stats.retries += 1;
                self.jobs[ji].attempts += 1;
                Ok(())
            }
            Err(Fail::Crashed(p)) => {
                self.crash_replica(ri, p);
                self.jobs[ji].attempts += 1;
                self.jobs[ji].prep = None;
                Ok(())
            }
            Err(v) => Err(v),
        }
    }

    async fn job_phase(&mut self, ji: usize, ri: usize) -> R<()> {
        match self.jobs[ji].phase {
            Phase::Ingest(i) => self.ingest_phase(ji, i).await,
            Phase::Land => {
                let before = self.raw_refs.log_len();
                let out = self.land(ji, ri).await?;
                if out == Land::Skew && self.raw_refs.log_len() != before {
                    return viol("a version-skewed request had an effect");
                }
                self.note(3, ji as u64 * 8 + out as u64);
                match out {
                    Land::Done => {
                        let j = &mut self.jobs[ji];
                        j.phase = Phase::Done;
                        let c = j.change;
                        if !self.acked.contains(&c) {
                            self.acked.push(c);
                        }
                        self.stats.landed += 1;
                    }
                    Land::Conflict => self.stats.conflicts += 1,
                    Land::Prepared => {}
                    Land::Reingest => {
                        self.stats.reingests += 1;
                        self.jobs[ji].phase = Phase::Ingest(0);
                    }
                    Land::Skew => {
                        self.stats.skew_refusals += 1;
                        let j = &mut self.jobs[ji];
                        j.skewed += 1;
                        if j.skewed >= 2 {
                            j.proto = self.replicas[ri].proto_hi;
                        }
                    }
                }
                Ok(())
            }
            Phase::Done => Ok(()),
        }
    }

    /// `PushObjects`: files first, then the changeset (I1). A crash may stop it halfway.
    async fn ingest_phase(&mut self, ji: usize, i: usize) -> R<()> {
        let j = self.jobs[ji].clone();
        if i < j.files.len() {
            self.put_obj(SimKey::File(j.files[i].1), &j.contents[i])
                .await?;
            if self.crash.hit(CrashPoint::MidImport) {
                return Err(Fail::Crashed(CrashPoint::MidImport));
            }
            self.jobs[ji].phase = Phase::Ingest(i + 1);
            return Ok(());
        }
        let cs = Cs {
            parent: None,
            files: j.files.clone(),
            accepted: vec![],
            nonce: j.change,
        };
        self.put_obj(SimKey::Cs(cs.id()), &cs.encode()).await?;
        let jm = &mut self.jobs[ji];
        jm.cs = Some(cs);
        jm.phase = Phase::Land;
        Ok(())
    }

    /// Finds a committed change by `op_id` through the ref log (the dedupe table).
    async fn find_op(&self, op: Uuid) -> R<Option<Committed>> {
        let log = self
            .refs
            .log(REPO, &rn(MASTER), 100_000)
            .await
            .map_err(rerr)?;
        let Some(e) = log.into_iter().find(|e| e.op_id == op) else {
            return Ok(None);
        };
        Ok(self
            .raw_refs
            .committed()
            .into_iter()
            .find(|c| c.seq == e.seq))
    }

    /// `Land`: rebase the change onto the tip as a new first-parent commit, then CAS `master`.
    pub async fn land(&mut self, ji: usize, ri: usize) -> R<Land> {
        let job = self.jobs[ji].clone();
        let Some(change_cs) = job.cs.clone() else {
            return viol("land of a job without a changeset");
        };
        if !self.replicas[ri].supports(job.proto) {
            if self.bugs.skew_has_effect {
                self.refs_skew_bug().await;
            }
            return Ok(Land::Skew);
        }
        if !self.bugs.no_dedupe && self.find_op(job.op_id).await?.is_some() {
            return Ok(Land::Done);
        }
        if self.jobs[ji].prep.is_none() {
            // Phase one: read the tip, check the closure (I4), write the landing commit.
            let tip = self.refs.get(REPO, &rn(MASTER)).await.map_err(rerr)?;
            let need = {
                let mut v = change_cs.needs();
                v.push(SimKey::Cs(change_cs.id()));
                v
            };
            let have = self
                .objects
                .has_many(&need.iter().map(|k| k.obj()).collect::<Vec<_>>())
                .await
                .map_err(crate::model::oerr)?;
            if have.iter().any(|h| !h) {
                return Ok(Land::Reingest);
            }
            let tip_id = tip.as_ref().map(|e| match e.target {
                RefTarget::Changeset(c) => *c.as_bytes(),
                RefTarget::Tag(t) => *t.as_bytes(),
            });
            let l = Cs {
                parent: tip_id,
                files: change_cs.files.clone(),
                accepted: vec![job.change],
                nonce: job.op_id.as_u128() as u64 ^ u64::from(job.attempts),
            };
            self.put_obj(SimKey::Cs(l.id()), &l.encode()).await?;
            if self.crash.hit(CrashPoint::MidLand) {
                return Err(Fail::Crashed(CrashPoint::MidLand));
            }
            self.jobs[ji].prep = Some((tip_id, l));
            return Ok(Land::Prepared);
        }
        // Phase two, a later step (other agents may have landed in between): the CAS.
        let Some((tip_id, l)) = self.jobs[ji].prep.clone() else {
            return viol("prep vanished");
        };
        if self.crash.hit(CrashPoint::BeforeCas) {
            self.jobs[ji].prep = None;
            return Err(Fail::Crashed(CrashPoint::BeforeCas));
        }
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: rn(MASTER),
                expect: tip_id.map_or(Expect::Absent, |t| Expect::Value(Self::target(t))),
                new: Some(Self::target(l.id())),
                expires_at: None,
            }],
            actor: actor("agent"),
            reason: "land".into(),
            op_id: job.op_id,
        };
        match self.refs.update(REPO, &txn).await {
            Ok(()) => {}
            Err(RefError::Conflict(_)) => {
                self.jobs[ji].prep = None;
                return Ok(Land::Conflict);
            }
            Err(e) => return Err(rerr(e)),
        }
        self.jobs[ji].prep = None;
        if self.crash.hit(CrashPoint::AfterCasBeforeOutbox) {
            return Err(Fail::Crashed(CrashPoint::AfterCasBeforeOutbox));
        }
        self.enqueue_outbox();
        Ok(Land::Done)
    }

    /// The defect `skew_has_effect`: a refused request moves the ref anyway.
    async fn refs_skew_bug(&mut self) {
        let op = self.new_op();
        let t = Self::target(
            Cs {
                parent: None,
                files: vec![],
                accepted: vec![],
                nonce: op.as_u128() as u64,
            }
            .id(),
        );
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: rn("refs/skew/bug"),
                expect: Expect::Any,
                new: Some(t),
                expires_at: None,
            }],
            actor: actor("skew"),
            reason: "bug".into(),
            op_id: op,
        };
        let _ = self.raw_refs_update(&txn).await;
    }

    async fn raw_refs_update(&self, txn: &RefTxn) -> Result<(), RefError> {
        self.raw_refs.update(REPO, txn).await
    }

    // ---- outbox ----------------------------------------------------------------------------

    /// Inserts a row for every committed ref change that has none (create-if-absent).
    pub fn enqueue_outbox(&mut self) -> usize {
        let now = self.clock.now_ms();
        let mut added = 0;
        for c in self.raw_refs.committed() {
            let key = (c.name.to_string(), c.version);
            if let std::collections::btree_map::Entry::Vacant(v) = self.db.outbox.entry(key) {
                v.insert(OutRow {
                    op_id: c.op_id,
                    failures: 0,
                    due_ms: now,
                    lease_until_ms: 0,
                    delivered: false,
                    last_backoff_s: 0,
                });
                added += 1;
            }
        }
        added
    }

    /// The reconciler: finds ref changes whose outbox row was lost to a crash.
    pub fn reconcile_outbox(&mut self) -> usize {
        if self.bugs.skip_reconcile {
            return 0;
        }
        self.enqueue_outbox()
    }

    /// Delivers one due row from replica `ri`. The replica may die before it records the result.
    pub fn deliver_one(&mut self, ri: usize) {
        let now = self.clock.now_ms();
        let Some(key) = self
            .db
            .outbox
            .iter()
            .find(|(_, r)| !r.delivered && r.due_ms <= now && r.lease_until_ms <= now)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        if let Some(r) = self.db.outbox.get_mut(&key) {
            r.lease_until_ms = now + 30_000;
        }
        let res = self.forgejo.post_receive(&key, &mut self.rng);
        if !self.quiet && self.rng.chance(1, 25) {
            // Killed after the call and before the row was updated: the lease will lapse.
            self.crash_replica(ri, CrashPoint::AfterCasBeforeOutbox);
            self.note(4, 1);
            return;
        }
        let Some(r) = self.db.outbox.get_mut(&key) else {
            return;
        };
        r.lease_until_ms = 0;
        match res {
            Ok(()) => r.delivered = true,
            Err(_) => {
                r.failures += 1;
                r.last_backoff_s = backoff_secs(r.failures);
                r.due_ms = now + r.last_backoff_s * 1000;
            }
        }
        self.note(4, u64::from(res.is_ok()));
    }

    // ---- webhooks --------------------------------------------------------------------------

    /// Inbound push event from Forgejo: idempotent, monotonic in `seq`, objects before the ref.
    pub async fn webhook(
        &mut self,
        delivery: u64,
        seq: u64,
        tip: [u8; 32],
        objs: &[(SimKey, Vec<u8>)],
    ) -> R<()> {
        if self.db.webhook_seen.contains(&delivery) {
            self.stats.webhook_dups += 1;
            return Ok(());
        }
        // Monotonic in seq, decided from the ref itself (no separate counter to lose): the
        // mirror changeset's nonce is its event's seq - 1.
        let cur_ref = self.refs.get(REPO, &rn(MIRROR)).await.map_err(rerr)?;
        if let Some(e) = &cur_ref {
            let RefTarget::Changeset(c) = e.target else {
                return viol("the mirror ref is not a changeset");
            };
            let Some(cur) = self.get_cs(*c.as_bytes()).await? else {
                return viol("the mirror ref names a missing changeset");
            };
            if cur.nonce + 1 >= seq {
                self.db.webhook_seen.insert(delivery);
                self.stats.webhook_dups += 1;
                return Ok(());
            }
        }
        for (k, data) in objs {
            self.put_obj(*k, data).await?;
        }
        if self.crash.hit(CrashPoint::MidImport) {
            return Err(Fail::Crashed(CrashPoint::MidImport));
        }
        let cur = self.refs.get(REPO, &rn(MIRROR)).await.map_err(rerr)?;
        let cur_t = cur.as_ref().map(|e| e.target);
        if cur_t != Some(Self::target(tip)) {
            let txn = RefTxn {
                updates: vec![RefUpdate {
                    name: rn(MIRROR),
                    expect: cur_t.map_or(Expect::Absent, Expect::Value),
                    new: Some(Self::target(tip)),
                    expires_at: None,
                }],
                actor: actor("forgejo"),
                reason: "webhook".into(),
                op_id: Uuid::from_u128(u128::from(delivery) | (1 << 100)),
            };
            match self.refs.update(REPO, &txn).await {
                Ok(()) => {}
                Err(RefError::Conflict(_)) => return Err(Fail::Transient),
                Err(e) => return Err(rerr(e)),
            }
        }
        self.db.webhook_seen.insert(delivery);
        *self.db.webhook_effects.entry(delivery).or_insert(0) += 1;
        self.stats.webhook_applied += 1;
        Ok(())
    }

    // ---- index -------------------------------------------------------------------------------

    /// Walks the first-parent chain of `tip`, newest first.
    pub async fn chain(&self, tip: [u8; 32]) -> R<Vec<([u8; 32], Cs)>> {
        let mut out = Vec::new();
        let mut cur = Some(tip);
        while let Some(id) = cur {
            let Some(raw) = self.raw_objects_get(id).await? else {
                return viol(format!("the chain of master is missing {}", hex(&id)));
            };
            cur = raw.parent;
            out.push((id, raw));
            if out.len() > 100_000 {
                return viol("the chain of master does not end");
            }
        }
        Ok(out)
    }

    pub async fn raw_objects_get(&self, id: [u8; 32]) -> R<Option<Cs>> {
        if self.chk.verified.contains(&SimKey::Cs(id))
            && let Some(c) = self
                .chk
                .decoded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
        {
            return Ok(Some(c.clone()));
        }
        match self
            .raw_objects
            .get(&SimKey::Cs(id).obj())
            .await
            .map_err(crate::model::oerr)?
        {
            None => Ok(None),
            Some(b) => {
                let c = Cs::decode(&b);
                if let Some(c) = &c {
                    self.chk
                        .decoded
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(id, c.clone());
                }
                Ok(c)
            }
        }
    }

    /// Derives `mainline` and `path_changes` for landings not yet indexed, one at a time.
    pub async fn index_catch_up(&mut self, limit: usize) -> R<()> {
        let Some((tip, _)) = self.master_tip().await? else {
            return Ok(());
        };
        let chain = self.chain(tip).await?;
        let mut todo: Vec<_> = Vec::new();
        for (id, cs) in &chain {
            if Some(*id) == self.db.index.through {
                break;
            }
            todo.push((*id, cs.clone()));
        }
        if let Some(t) = self.db.index.through
            && !chain.iter().any(|(id, _)| *id == t)
        {
            return viol("the index points off the first-parent chain");
        }
        for (id, cs) in todo.into_iter().rev().take(limit) {
            self.db.index.mainline.push(id);
            for (path, _) in &cs.files {
                self.db
                    .index
                    .path_changes
                    .entry(*path)
                    .or_default()
                    .extend(cs.accepted.iter().copied());
            }
            self.db.index.through = Some(id);
        }
        Ok(())
    }
}

pub fn hex(b: &[u8; 32]) -> String {
    b.iter().take(6).map(|x| format!("{x:02x}")).collect()
}
