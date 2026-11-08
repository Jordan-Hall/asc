//! What a running scan has done, for `DepsService.ScanStatus` (docs/DEPENDENCIES.md "Progress").
//!
//! A scan registers itself when it starts and moves through three phases: reading the revision, asking the
//! advisory source and recording. Only the advisory phase has a known amount of work: one step per package version
//! asked (from the cache or OSV) and one per advisory record read, so the bar and the estimate there are real. The
//! estimate is the rate of the advisory phase when it has done something, else how long the last scan of the same
//! repository took on this process, else none. The state is in memory: a restart forgets it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The steps of the advisory phase. The lookup adds to `total` when it knows more work and to `done` as it goes.
#[derive(Debug, Default)]
pub struct Tick {
    done: AtomicU32,
    total: AtomicU32,
}

impl Tick {
    pub fn add_total(&self, n: usize) {
        self.total
            .fetch_add(u32::try_from(n).unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    pub fn add_done(&self, n: usize) {
        self.done
            .fetch_add(u32::try_from(n).unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    /// (done, total); `done` never says more than `total`.
    pub fn get(&self) -> (u32, u32) {
        let total = self.total.load(Ordering::Relaxed);
        (self.done.load(Ordering::Relaxed).min(total), total)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Reading,
    Advisories,
    Recording,
}

#[derive(Debug)]
struct Running {
    id: u64,
    started: i64,
    phase: Phase,
    phase_started: i64,
    tick: Arc<Tick>,
}

#[derive(Debug, Default)]
struct State {
    next: u64,
    running: HashMap<i64, Running>,
    took: HashMap<i64, i64>,
}

/// The scans running on this process, by repository.
#[derive(Debug, Default)]
pub struct Scans {
    state: Mutex<State>,
}

/// What `ScanStatus` answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub phase: Option<Phase>,
    pub done: u32,
    pub total: u32,
    pub started_at: i64,
    pub eta_secs: u32,
    pub permille: u32,
    pub last_took_secs: u32,
}

/// Most repositories whose last duration is kept.
const MAX_TOOK: usize = 4096;

impl Scans {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a scan of `repo` that starts at `now`. The newest scan of a repository is the one reported.
    pub fn start(self: &Arc<Self>, repo: i64, now: i64) -> Guard {
        let mut s = self.lock();
        s.next += 1;
        let id = s.next;
        let tick = Arc::new(Tick::default());
        s.running.insert(
            repo,
            Running {
                id,
                started: now,
                phase: Phase::Reading,
                phase_started: now,
                tick: tick.clone(),
            },
        );
        Guard {
            scans: self.clone(),
            repo,
            id,
            tick,
        }
    }

    fn set_phase(&self, repo: i64, id: u64, phase: Phase, now: i64) {
        if let Some(r) = self.lock().running.get_mut(&repo)
            && r.id == id
        {
            r.phase = phase;
            r.phase_started = now;
        }
    }

    fn end(&self, repo: i64, id: u64, finished: Option<i64>) {
        let mut s = self.lock();
        let Some(r) = s.running.get(&repo) else {
            return;
        };
        if r.id != id {
            return;
        }
        let started = r.started;
        s.running.remove(&repo);
        if let Some(now) = finished {
            if s.took.len() >= MAX_TOOK && !s.took.contains_key(&repo) {
                s.took.clear();
            }
            s.took.insert(repo, (now - started).max(0));
        }
    }

    /// The state of the newest running scan of `repo` at `now`.
    pub fn status(&self, repo: i64, now: i64) -> Status {
        let s = self.lock();
        let took = s.took.get(&repo).copied();
        let last_took_secs = took.map_or(0, secs);
        let Some(r) = s.running.get(&repo) else {
            return Status {
                last_took_secs,
                ..Status::default()
            };
        };
        let (done, total) = r.tick.get();
        let elapsed = (now - r.started).max(0);
        let permille = match r.phase {
            Phase::Reading => 0,
            Phase::Advisories if total == 0 => 100,
            Phase::Advisories => {
                100 + u32::try_from(u64::from(done) * 850 / u64::from(total)).unwrap_or(850)
            }
            Phase::Recording => 950,
        };
        let eta = if r.phase == Phase::Advisories && done > 0 && total > done {
            // The rate so far, at least a second of work for what is left.
            let spent = (now - r.phase_started).max(1);
            let left = i64::from(total - done);
            let done = i64::from(done);
            (spent * left + done - 1) / done
        } else if let Some(t) = took
            && t > elapsed
        {
            t - elapsed
        } else {
            0
        };
        Status {
            phase: Some(r.phase),
            done,
            total,
            started_at: r.started,
            eta_secs: secs(eta),
            permille,
            last_took_secs,
        }
    }
}

fn secs(n: i64) -> u32 {
    u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

/// A registered scan. Dropping it without [`Guard::finish`] (an error) forgets the scan and keeps the last duration.
#[derive(Debug)]
pub struct Guard {
    scans: Arc<Scans>,
    repo: i64,
    id: u64,
    tick: Arc<Tick>,
}

impl Guard {
    pub fn tick(&self) -> &Tick {
        &self.tick
    }

    pub fn phase(&self, phase: Phase, now: i64) {
        self.scans.set_phase(self.repo, self.id, phase, now);
    }

    /// The scan finished at `now`; its duration is the estimate of the next one.
    pub fn finish(self, now: i64) {
        self.scans.end(self.repo, self.id, Some(now));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.scans.end(self.repo, self.id, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_running() {
        let s = Arc::new(Scans::default());
        assert_eq!(s.status(1, 100), Status::default());
    }

    #[test]
    fn phases_bar_and_rate_estimate() {
        let s = Arc::new(Scans::default());
        let g = s.start(1, 100);
        let st = s.status(1, 103);
        assert_eq!(
            (st.phase, st.permille, st.eta_secs, st.started_at),
            (Some(Phase::Reading), 0, 0, 100)
        );
        g.phase(Phase::Advisories, 104);
        assert_eq!(s.status(1, 104).permille, 100, "no step known yet");
        g.tick().add_total(10);
        g.tick().add_done(4);
        let st = s.status(1, 108);
        assert_eq!((st.done, st.total), (4, 10));
        assert_eq!(st.permille, 100 + 4 * 85);
        assert_eq!(st.eta_secs, 6, "4 steps in 4 s, 6 left");
        g.phase(Phase::Recording, 110);
        assert_eq!(s.status(1, 110).permille, 950);
        assert_eq!(
            s.status(2, 110),
            Status::default(),
            "another repository is idle"
        );
        g.finish(112);
        let st = s.status(1, 200);
        assert_eq!((st.phase, st.last_took_secs), (None, 12));
    }

    #[test]
    fn history_estimates_before_the_rate_does() {
        let s = Arc::new(Scans::default());
        s.start(1, 0).finish(20);
        let _g = s.start(1, 100);
        assert_eq!(s.status(1, 105).eta_secs, 15, "the last scan took 20 s");
        assert_eq!(
            s.status(1, 130).eta_secs,
            0,
            "past the last duration there is no estimate"
        );
    }

    #[test]
    fn a_failed_scan_is_forgotten_and_keeps_the_history() {
        let s = Arc::new(Scans::default());
        s.start(1, 0).finish(9);
        drop(s.start(1, 50));
        let st = s.status(1, 60);
        assert_eq!((st.phase, st.last_took_secs), (None, 9));
    }

    #[test]
    fn the_newest_scan_is_reported_and_an_older_one_does_not_clear_it() {
        let s = Arc::new(Scans::default());
        let old = s.start(1, 0);
        let new = s.start(1, 5);
        old.finish(7);
        let st = s.status(1, 8);
        assert_eq!(
            st.started_at, 5,
            "the older scan ending leaves the newer one"
        );
        assert_eq!(st.last_took_secs, 0, "and records nothing for it");
        new.finish(9);
        assert_eq!(s.status(1, 9).last_took_secs, 4);
    }

    #[test]
    fn done_never_exceeds_total() {
        let t = Tick::default();
        t.add_total(2);
        t.add_done(5);
        assert_eq!(t.get(), (2, 2));
    }
}
