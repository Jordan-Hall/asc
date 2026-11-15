//! The hook between the landing gate and the dependency scanner.
//!
//! The gate (`policy::gate`) knows the policy; the scanner knows advisories. This trait is all they
//! share: the gate asks whether an update adds a dependency the policy forbids, and the scanner answers
//! with the lines of the refusal, or none. When the scanner is not running (`[deps] enabled = false`)
//! the slot is empty and a policy that blocks on advisories refuses every update that touches a
//! dependency file, because it cannot check it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use asc_core::id::ChangesetId;
use asc_core::policy::DepsPolicy;
use async_trait::async_trait;
use bytes::Bytes;
use tonic::Status;

use super::hub::{
    Assessed, DepsHub, IntelView, MAX_GATE_FILES, MAX_GATE_PACKAGES, findings_of, summarize,
};
use super::intel::FeedStatus;
use super::model::{Dep, Ecosystem, Lookup};
use super::osv::Query;
use super::parse::{Format, MAX_FILE_BYTES, parse_file};
use super::scan::distinct;
use super::status::Move;
use super::store::{self, NewFinding};
use crate::registry::RepoEntry;
use crate::review::sql;

/// Most lines of a refusal.
pub const MAX_LINES: usize = 20;

/// What the gate asks.
pub struct CheckRequest<'a> {
    pub entry: &'a RepoEntry,
    /// The changeset the ref moves to.
    pub new: ChangesetId,
    /// What the ref was (or trunk was, for a new ref); the dependencies it had are not "added".
    pub base: Option<ChangesetId>,
    /// The paths the update changes.
    pub touched: &'a BTreeSet<Vec<u8>>,
    pub policy: DepsPolicy,
}

#[async_trait]
pub trait DepsCheck: Send + Sync {
    /// The lines of a refusal (the first says why, the rest name the packages), or none when the update
    /// is fine. An error is an internal failure, which refuses the update as well.
    async fn check(&self, req: &CheckRequest<'_>) -> Result<Vec<String>, Status>;
}

/// The place the scanner puts itself, once, for the gate to find.
#[derive(Default)]
pub struct DepsGateSlot(OnceLock<Arc<dyn DepsCheck>>);

impl DepsGateSlot {
    pub fn set(&self, c: Arc<dyn DepsCheck>) -> bool {
        self.0.set(c).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<dyn DepsCheck>> {
        self.0.get()
    }
}

/// The dependency files among `touched`.
pub fn dependency_files(touched: &BTreeSet<Vec<u8>>) -> Vec<String> {
    touched
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| Format::of(super::model::base_of(p)).is_some())
        .map(str::to_owned)
        .collect()
}

// ------------------------------------------------------------------------------ report mode

// Everything below is for the `Status` call. The refusing check above and `DepsHub::check` (the landing
// path) do not use it and are not changed by it.

/// Most changesets between trunk and a change head that status reads the touched files of.
pub const MAX_REPORT_CHANGESETS: usize = 500;
/// How long a complete report is kept, in seconds.
pub const REPORT_TTL_SECS: i64 = 600;
/// How long a report that could not judge everything is kept, in seconds.
pub const INCOMPLETE_TTL_SECS: i64 = 30;
/// Most reports kept; the least recently used goes first.
pub const REPORT_CAPACITY: usize = 1000;

/// What a change adds to trunk, as a summary: the gate's check in report mode, which names the added
/// advisories instead of refusing. Only counts and the package moves are kept, not the findings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeReport {
    /// The change writes at least one dependency file.
    pub touches_deps: bool,
    /// The advisories of the package versions the change adds (dismissed and ignored ones are left out,
    /// as in a scan), counted per severity; every severity has a key.
    pub open_by_severity: BTreeMap<String, u32>,
    /// The package moves that close them, at most `status::MAX_MOVES`.
    pub moves: Vec<Move>,
    /// Some added package versions could not be judged (a lookup failed, a file could not be read, or
    /// there were more than the limits allow), so a lack of findings is not a clean answer.
    pub incomplete: bool,
}

impl ChangeReport {
    fn untouched() -> Self {
        Self {
            touches_deps: false,
            open_by_severity: BTreeMap::new(),
            moves: Vec::new(),
            incomplete: false,
        }
    }

    fn unjudged() -> Self {
        Self {
            touches_deps: true,
            open_by_severity: BTreeMap::new(),
            moves: Vec::new(),
            incomplete: true,
        }
    }

    /// Whether the change adds any open finding.
    pub fn has_findings(&self) -> bool {
        self.open_by_severity.values().any(|n| *n > 0)
    }

    fn ttl(&self) -> i64 {
        if self.incomplete {
            INCOMPLETE_TTL_SECS
        } else {
            REPORT_TTL_SECS
        }
    }
}

/// The dependencies of `new` that `old` does not have (compared by ecosystem, name and version); a
/// dependency without a version is never added.
pub fn introduced(new: Vec<Dep>, old: &[Dep]) -> Vec<Dep> {
    let had: HashSet<_> = old.iter().map(Dep::key).collect();
    new.into_iter()
        .filter(|d| !d.version.is_empty() && !had.contains(&d.key()))
        .collect()
}

/// What the feeds say, as one string: when it differs, an advisory may be judged differently.
pub fn intel_version(view: &IntelView) -> String {
    let one = |s: &FeedStatus| format!("{:?}/{:?}/{:?}", s.state, s.version, s.date);
    format!("{}|{}", one(&view.kev), one(&view.epss))
}

/// Repository, head, trunk it was compared with, feed version and dismissal epoch.
type ReportKey = (i64, ChangesetId, Option<ChangesetId>, String, u64);

struct Held {
    report: ChangeReport,
    at: i64,
    used: u64,
}

impl Held {
    fn fresh(&self, now: i64) -> bool {
        now.saturating_sub(self.at) <= self.report.ttl()
    }
}

#[derive(Default)]
struct Reports {
    tick: u64,
    map: HashMap<ReportKey, Held>,
}

/// The reports of changes, in memory only: per (repository, head, trunk it was compared with, feed
/// version, dismissal epoch), for [`REPORT_TTL_SECS`] ([`INCOMPLETE_TTL_SECS`] when the report could not
/// judge everything), at most [`REPORT_CAPACITY`]. One computation per key runs at a time.
#[derive(Default)]
pub struct ChangeCache {
    inner: Mutex<Reports>,
    computed: AtomicU64,
    /// Bumped by every dismissal change, so a report made before it is never served after it.
    epoch: AtomicU64,
    flights: Mutex<HashMap<ReportKey, Arc<tokio::sync::Mutex<()>>>>,
}

/// The right to compute one key; the entry leaves the table when the last holder or waiter is done.
struct Flight<'a> {
    cache: &'a ChangeCache,
    key: ReportKey,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut flights = self
            .cache
            .flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The table holds one handle and this struct one; more means another call waits on it.
        if Arc::strong_count(&self.lock) <= 2 {
            flights.remove(&self.key);
        }
    }
}

impl ChangeCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Reports> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, key: &ReportKey, now: i64) -> Option<ChangeReport> {
        let mut g = self.lock();
        g.tick += 1;
        let tick = g.tick;
        match g.map.get_mut(key) {
            Some(h) if h.fresh(now) => {
                h.used = tick;
                Some(h.report.clone())
            }
            Some(_) => {
                g.map.remove(key);
                None
            }
            None => None,
        }
    }

    fn put(&self, key: ReportKey, now: i64, report: ChangeReport) {
        let mut g = self.lock();
        g.tick += 1;
        let used = g.tick;
        if g.map.len() >= REPORT_CAPACITY && !g.map.contains_key(&key) {
            g.map.retain(|_, h| h.fresh(now));
        }
        while g.map.len() >= REPORT_CAPACITY && !g.map.contains_key(&key) {
            let oldest = g
                .map
                .iter()
                .min_by_key(|(_, h)| h.used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => g.map.remove(&k),
                None => break,
            };
        }
        g.map.insert(
            key,
            Held {
                report,
                at: now,
                used,
            },
        );
    }

    fn flight(&self, key: &ReportKey) -> Flight<'_> {
        let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = flights.entry(key.clone()).or_default().clone();
        Flight {
            cache: self,
            key: key.clone(),
            lock,
        }
    }

    /// A finding was dismissed or reopened: reports made until now are no longer served.
    pub fn dismissals_changed(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// How many computations are in flight or waited for.
    pub fn in_flight(&self) -> usize {
        self.flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// How many reports were computed, not served from memory.
    pub fn computed(&self) -> u64 {
        self.computed.load(Ordering::Relaxed)
    }

    /// How many reports are held.
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl DepsHub {
    /// The report of the change whose head is `head` against `base` (trunk), from memory when it is
    /// there and fresh. A miss takes a scan permit and runs once per key: calls for the same key wait for
    /// the first and share its answer. The whole computation is held to the gate's timeout; past it the
    /// answer is "could not judge".
    pub(crate) async fn report_cached(
        &self,
        entry: &RepoEntry,
        head: ChangesetId,
        base: Option<ChangesetId>,
    ) -> Result<ChangeReport, Status> {
        let key = (
            entry.key.0,
            head,
            base,
            intel_version(&self.intel_view()),
            self.change_cache.epoch.load(Ordering::Relaxed),
        );
        if let Some(r) = self.change_cache.get(&key, self.now()) {
            return Ok(r);
        }
        let flight = self.change_cache.flight(&key);
        let _turn = flight.lock.lock().await;
        if let Some(r) = self.change_cache.get(&key, self.now()) {
            return Ok(r);
        }
        let _permit = self.permit().await?;
        let timeout = std::time::Duration::from_secs(self.cfg.gate_timeout_secs);
        let report =
            match tokio::time::timeout(timeout, self.report_change(entry, head, base)).await {
                Ok(r) => r?,
                Err(_) => ChangeReport::unjudged(),
            };
        self.change_cache.computed.fetch_add(1, Ordering::Relaxed);
        self.change_cache.put(key, self.now(), report.clone());
        Ok(report)
    }

    /// The gate's check in report mode: reads the dependency files the change writes (the changesets
    /// between `base` and `head`), and looks up the package versions they add over `base`. Reads only;
    /// nothing is recorded.
    pub(crate) async fn report_change(
        &self,
        entry: &RepoEntry,
        head: ChangesetId,
        base: Option<ChangesetId>,
    ) -> Result<ChangeReport, Status> {
        let Some(ids) = self
            .view
            .range(
                entry,
                head,
                base.into_iter().collect(),
                MAX_REPORT_CHANGESETS,
            )
            .await?
        else {
            return Ok(ChangeReport::unjudged());
        };
        let mut touched: BTreeSet<Vec<u8>> = BTreeSet::new();
        for id in ids {
            for p in self.view.changed_paths(entry, id).await? {
                touched.insert(p.into_bytes());
            }
        }
        let files = dependency_files(&touched);
        if files.is_empty() {
            return Ok(ChangeReport::untouched());
        }
        if files.len() > MAX_GATE_FILES {
            return Ok(ChangeReport::unjudged());
        }
        let new_root = self.view.root_of(entry, head).await?;
        let old_root = match base {
            Some(b) => Some(self.view.root_of(entry, b).await?),
            None => None,
        };
        let mut incomplete = false;
        let mut added: Vec<Dep> = Vec::new();
        for path in &files {
            let parse = |bytes: &Bytes| match parse_file(path, bytes) {
                Some(Ok(p)) => Ok(p.deps),
                Some(Err(e)) => Err(e),
                None => Ok(Vec::new()),
            };
            let new = match self
                .view
                .read(entry, new_root, path, MAX_FILE_BYTES as u64)
                .await
            {
                Ok(Some(b)) => parse(&b),
                Ok(None) => continue,
                Err(s) if s.code() == tonic::Code::FailedPrecondition => {
                    Err(s.message().to_owned())
                }
                Err(s) => return Err(s),
            };
            let Ok(new) = new else {
                incomplete = true;
                continue;
            };
            // The old file must be read to say what is added; when it cannot be, the file says nothing.
            let old: Vec<Dep> = match old_root {
                Some(root) => match self
                    .view
                    .read(entry, root, path, MAX_FILE_BYTES as u64)
                    .await
                {
                    Ok(Some(b)) => match parse(&b) {
                        Ok(deps) => deps,
                        Err(_) => {
                            incomplete = true;
                            continue;
                        }
                    },
                    Ok(None) => Vec::new(),
                    Err(_) => {
                        incomplete = true;
                        continue;
                    }
                },
                None => Vec::new(),
            };
            added.extend(introduced(new, &old));
        }
        let (pkgs, capped) = distinct(&added, MAX_GATE_PACKAGES);
        let queries: Vec<Query> = pkgs
            .iter()
            .map(|(eco, name, version)| Query {
                eco: *eco,
                name: name.clone(),
                version: version.clone(),
            })
            .collect();
        let lookups = self.osv.lookup(&queries, false).await.lookups;
        let answers: HashMap<(Ecosystem, String, String), Lookup> =
            pkgs.into_iter().zip(lookups).collect();
        let assessed = Assessed {
            answers,
            errors: Vec::new(),
            capped,
            source: "change",
        };
        let loaded = self.repo_config(entry, base).await?;
        let (found, unjudged) = findings_of(&added, &assessed, &loaded.config, &self.today());
        let dismissed = store::dismissed(self.registry.pool(), entry.key.0)
            .await
            .map_err(sql)?;
        let found: Vec<NewFinding> = found
            .into_iter()
            .filter(|f| {
                !dismissed.contains(&(
                    f.ecosystem.clone(),
                    f.name.clone(),
                    f.version.clone(),
                    f.advisory.id.clone(),
                ))
            })
            .collect();
        Ok(summarize(
            &found,
            incomplete || capped || !unjudged.is_empty(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dependency_files_are_picked_out_of_the_touched_paths() {
        let touched: BTreeSet<Vec<u8>> = [
            "src/main.rs",
            "Cargo.lock",
            "web/package-lock.json",
            "docs/requirements-dev.txt",
            "README.md",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .chain([vec![0xff, 0xfe]])
        .collect();
        assert_eq!(
            dependency_files(&touched),
            [
                "Cargo.lock",
                "docs/requirements-dev.txt",
                "web/package-lock.json"
            ]
        );
    }

    #[test]
    fn the_slot_is_set_once() {
        struct Never;
        #[async_trait]
        impl DepsCheck for Never {
            async fn check(&self, _: &CheckRequest<'_>) -> Result<Vec<String>, Status> {
                Ok(Vec::new())
            }
        }
        let slot = DepsGateSlot::default();
        assert!(slot.get().is_none());
        assert!(slot.set(Arc::new(Never)));
        assert!(!slot.set(Arc::new(Never)));
        assert!(slot.get().is_some());
    }
}
