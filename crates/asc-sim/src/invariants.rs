//! The invariants of SPEC 17.2, checked after every step. Any failure is a `Fail::Violation`.

use std::collections::{BTreeMap, BTreeSet};

use asc_core::id::RefTarget;
use asc_serve::outbox::backoff_secs;
use asc_store::objects::Objects;
use uuid::Uuid;

use crate::model::{Cs, MASTER, MIN_AGE_MS, R, SHED_PER_SEC, SimKey, viol};
use crate::world::{Phase, World};

/// State the checks carry between steps (so each step costs only what changed).
#[derive(Default)]
pub struct Checked {
    pub verified: BTreeSet<SimKey>,
    /// Decoded changesets (immutable, content addressed); used only while `verified` holds them.
    pub decoded: std::sync::Mutex<BTreeMap<[u8; 32], Cs>>,
    log_checked: usize,
    last_version: BTreeMap<String, u64>,
    ops_seen: BTreeSet<(String, Uuid)>,
}

impl World {
    pub async fn check_step(&mut self) -> R<()> {
        self.check_ref_log()?;
        self.check_closures().await?;
        self.check_master().await?;
        self.check_db().await?;
        self.check_upstream_rate()?;
        self.check_outbox_backoff()?;
        self.check_inflight().await?;
        Ok(())
    }

    /// `ref_log` versions strictly increase per ref; each `op_id` has one effect per ref.
    fn check_ref_log(&mut self) -> R<()> {
        let fresh = self.raw_refs.committed_from(self.chk.log_checked);
        for c in &fresh {
            let name = c.name.to_string();
            let last = self.chk.last_version.get(&name).copied().unwrap_or(0);
            if c.version <= last {
                return viol(format!(
                    "ref_log version of {name} went {last} -> {}",
                    c.version
                ));
            }
            self.chk.last_version.insert(name.clone(), c.version);
            if !self.chk.ops_seen.insert((name.clone(), c.op_id)) {
                return viol(format!("op_id {} has two effects on {name}", c.op_id));
            }
        }
        self.chk.log_checked += fresh.len();
        Ok(())
    }

    /// Every ref's closure exists (I4), and every stored object is what its key says.
    async fn check_closures(&mut self) -> R<()> {
        for e in self.raw_refs.all() {
            let RefTarget::Changeset(c) = e.target else {
                continue;
            };
            let mut stack = vec![SimKey::Cs(*c.as_bytes())];
            while let Some(k) = stack.pop() {
                if self.chk.verified.contains(&k) {
                    continue;
                }
                let Some(b) = self
                    .raw_objects
                    .get(&k.obj())
                    .await
                    .map_err(crate::model::oerr)?
                else {
                    return viol(format!("closure of {} is missing {k:?}", e.name));
                };
                if let SimKey::Cs(_) = k {
                    let Some(cs) = Cs::decode(&b) else {
                        return viol(format!("{k:?} does not decode"));
                    };
                    stack.extend(cs.needs());
                }
                self.chk.verified.insert(k);
            }
        }
        Ok(())
    }

    /// `master` is a linear first-parent history holding every acked change exactly once, and
    /// the incremental index equals a recomputation over the prefix it covers.
    async fn check_master(&mut self) -> R<()> {
        let Some((tip, _)) = self.master_tip_raw() else {
            if let Some(c) = self.acked.first() {
                return viol(format!("change {c} was acknowledged but master is empty"));
            }
            return Ok(());
        };
        let chain = self.chain(tip).await?;
        let mut seen = BTreeSet::new();
        for (_, cs) in &chain {
            for c in &cs.accepted {
                if !seen.insert(*c) {
                    return viol(format!("change {c} is on master twice"));
                }
            }
        }
        for c in &self.acked {
            if !seen.contains(c) {
                return viol(format!("acknowledged change {c} is not on master"));
            }
        }
        // Recompute mainline and path_changes up to the index's watermark.
        let idx = &self.db.index;
        let mut mainline = Vec::new();
        let mut path_changes: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
        if let Some(t) = idx.through {
            let Some(pos) = chain.iter().position(|(id, _)| *id == t) else {
                return viol("the index watermark is not on master's chain");
            };
            for (id, cs) in chain[pos..].iter().rev() {
                mainline.push(*id);
                for (p, _) in &cs.files {
                    path_changes
                        .entry(*p)
                        .or_default()
                        .extend(cs.accepted.iter().copied());
                }
            }
        }
        if mainline != idx.mainline || path_changes != idx.path_changes {
            return viol("mainline/path_changes differ from a recomputation");
        }
        Ok(())
    }

    fn master_tip_raw(&self) -> Option<([u8; 32], u64)> {
        self.raw_refs
            .all()
            .into_iter()
            .find(|e| e.name.as_bytes() == MASTER.as_bytes())
            .map(|e| match e.target {
                RefTarget::Changeset(c) => (*c.as_bytes(), e.version),
                RefTarget::Tag(t) => (*t.as_bytes(), e.version),
            })
    }

    /// I2 and I3: `git_map` implies `cs_graph` implies the parents' rows and the object.
    async fn check_db(&mut self) -> R<()> {
        for (g, cs) in &self.db.git_map {
            let Some(parents) = self.db.cs_graph.get(cs) else {
                return viol(format!("I3: git_map {g} has no cs_graph row"));
            };
            if self.raw_has(&[SimKey::Cs(*cs)]).await?.iter().any(|h| !h) {
                return viol(format!("I3: git_map {g} names a missing changeset"));
            }
            if parents.iter().any(|p| !self.db.cs_graph.contains_key(p)) {
                return viol("I2: a cs_graph row exists without its parent's row");
            }
        }
        Ok(())
    }

    /// The cache role's requests upstream never exceed the shed limit in any one second.
    fn check_upstream_rate(&self) -> R<()> {
        let t = &self.upstream_calls;
        let mut lo = 0;
        for hi in 0..t.len() {
            while t[hi] - t[lo] >= 1000 {
                lo += 1;
            }
            if hi - lo + 1 > SHED_PER_SEC {
                return viol(format!(
                    "{} upstream requests within one second (limit {SHED_PER_SEC})",
                    hi - lo + 1
                ));
            }
        }
        Ok(())
    }

    fn check_outbox_backoff(&self) -> R<()> {
        for (k, r) in &self.db.outbox {
            if r.failures > 0
                && (r.last_backoff_s != backoff_secs(r.failures) || r.last_backoff_s > 300)
            {
                return viol(format!("outbox row {k:?} has a wrong backoff"));
            }
        }
        Ok(())
    }

    /// An ingested push younger than `min_age` keeps its objects through any collection.
    async fn check_inflight(&mut self) -> R<()> {
        let now = self.clock.now_ms();
        for j in &self.jobs {
            if j.abandoned || j.phase != Phase::Land {
                continue;
            }
            let Some(cs) = &j.cs else { continue };
            if now - j.started_ms >= MIN_AGE_MS {
                continue;
            }
            let mut need = cs.needs();
            need.push(SimKey::Cs(cs.id()));
            if self.raw_has(&need).await?.iter().any(|h| !h) {
                return viol(format!(
                    "the objects of in-flight push {} were collected before min_age",
                    j.change
                ));
            }
        }
        Ok(())
    }

    /// After a drain: everything committed has been delivered, once.
    pub async fn check_final(&mut self) -> R<()> {
        self.check_step().await?;
        for c in self.raw_refs.committed() {
            let key = (c.name.to_string(), c.version);
            match self.db.outbox.get(&key) {
                Some(r) if r.delivered => {}
                _ => return viol(format!("ref change {key:?} never got its post_receive")),
            }
            if self.forgejo.effects.get(&key).copied() != Some(1) {
                return viol(format!("post_receive of {key:?} has no single effect"));
            }
        }
        if let Some((tip, _)) = self.master_tip_raw()
            && self.db.index.through != Some(tip)
        {
            return viol("the index never caught up with master");
        }
        for (d, n) in &self.db.webhook_effects {
            if *n > 1 {
                return viol(format!("webhook delivery {d} took effect {n} times"));
            }
        }
        Ok(())
    }
}
