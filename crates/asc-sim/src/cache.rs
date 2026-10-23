//! The cache role (SPEC 16.3): read-through, single flight per key, per-key backoff after an
//! upstream failure, and a shed limit, so a dead upstream is not stampeded.

use std::collections::BTreeMap;

use bytes::Bytes;
use asc_serve::outbox::backoff_secs;
use asc_store::objects::Objects;

use crate::model::{Fail, R, SHED_PER_SEC, SimKey, oerr, viol};
use crate::world::{Role, World};

#[derive(Debug, Default, Clone)]
pub struct Cache {
    pub map: BTreeMap<SimKey, Bytes>,
    pub backoff: BTreeMap<SimKey, (i64, i64)>,
}

/// How a burst of reads for one key was answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    Hit,
    Fetched,
    Shed,
}

impl World {
    /// `n` clients ask replica `ri` for `key` at once; returns the answer all of them get.
    pub async fn cache_read_burst(&mut self, ri: usize, key: SimKey, n: usize) -> R<Served> {
        if self.replicas[ri].role != Role::Cache || !self.replicas[ri].up {
            return viol("a read was sent to a replica that is not a live cache");
        }
        let now = self.clock.now_ms();
        let hit = self.cache.get(&ri).and_then(|c| c.map.get(&key)).cloned();
        if let Some(b) = hit {
            self.check_cached(key, &b)?;
            return Ok(Served::Hit);
        }
        if self.bugs.no_coalesce {
            // The defect: every client's miss goes upstream, with no backoff and no limit.
            let mut last = Err(Fail::Transient);
            for _ in 0..n {
                last = self.upstream_get(key).await;
            }
            if matches!(last, Err(Fail::Transient)) {
                return Ok(Served::Shed);
            }
            return match last? {
                Some(b) => {
                    self.cache.entry(ri).or_default().map.insert(key, b);
                    Ok(Served::Fetched)
                }
                None => viol("a cached key is missing upstream"),
            };
        }
        let (fails, until) = self
            .cache
            .get(&ri)
            .and_then(|c| c.backoff.get(&key))
            .copied()
            .unwrap_or((0, 0));
        let recent = self
            .upstream_calls
            .iter()
            .filter(|&&t| t > now - 1000)
            .count();
        if until > now || recent >= SHED_PER_SEC {
            self.stats.shed += n as u64;
            return Ok(Served::Shed);
        }
        // One flight for the whole burst; the other n - 1 clients wait on it.
        self.stats.coalesced += n.saturating_sub(1) as u64;
        match self.upstream_get(key).await {
            Ok(Some(b)) => {
                let c = self.cache.entry(ri).or_default();
                c.backoff.remove(&key);
                c.map.insert(key, b);
                Ok(Served::Fetched)
            }
            Ok(None) => viol("a key the client was given is missing upstream"),
            Err(Fail::Transient) => {
                let f = fails + 1;
                let wait = backoff_secs(f) * 1000;
                self.cache
                    .entry(ri)
                    .or_default()
                    .backoff
                    .insert(key, (f, self.clock.now_ms() + wait));
                Ok(Served::Shed)
            }
            Err(e) => Err(e),
        }
    }

    async fn upstream_get(&mut self, key: SimKey) -> R<Option<Bytes>> {
        self.upstream_calls.push(self.clock.now_ms());
        self.stats.upstream_calls += 1;
        self.objects.get(&key.obj()).await.map_err(oerr)
    }

    fn check_cached(&self, key: SimKey, b: &Bytes) -> R<()> {
        // A cache only ever holds bytes that hash to their key.
        let ok = match key {
            SimKey::File(_) => SimKey::file(b) == key,
            SimKey::Cs(id) => crate::model::Cs::decode(b).is_some_and(|c| c.id() == id),
        };
        if ok {
            Ok(())
        } else {
            viol("a cache holds bytes that do not match their key")
        }
    }
}
