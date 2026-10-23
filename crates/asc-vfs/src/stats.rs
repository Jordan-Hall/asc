//! Hydration counters per path and per process (SPEC §10.2 item 5, `asc debug hydrations`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use asc_core::path::RepoPath;

use crate::policy::image_name;

/// Distinct paths counted; further new paths only add to `dropped_paths`.
pub const MAX_PATHS: usize = 100_000;
/// Distinct process images counted; further new images only add to `dropped_processes`.
pub const MAX_PROCESSES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathStats {
    pub hydrations: u64,
    pub bytes: u64,
    pub last_read: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessStats {
    pub hydrations: u64,
    pub bytes: u64,
    /// Reads refused by the deny list.
    pub denied: u64,
    /// The last hydration by this image; dehydration leaves recently read paths alone.
    pub last_read: Option<SystemTime>,
}

#[derive(Default)]
struct Inner {
    paths: HashMap<RepoPath, PathStats>,
    processes: HashMap<String, ProcessStats>,
    dropped_paths: u64,
    dropped_processes: u64,
}

/// Thread-safe, bounded counters.
#[derive(Default)]
pub struct Stats {
    inner: Mutex<Inner>,
}

/// A copy of the counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub paths: Vec<(RepoPath, PathStats)>,
    pub processes: Vec<(String, ProcessStats)>,
    pub total_hydrations: u64,
    pub dropped_paths: u64,
    pub dropped_processes: u64,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Counters stay valid after a panic elsewhere.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn process<'a>(i: &'a mut Inner, image: &str) -> Option<&'a mut ProcessStats> {
        let key = image_name(image);
        if !i.processes.contains_key(&key) && i.processes.len() >= MAX_PROCESSES {
            i.dropped_processes += 1;
            return None;
        }
        Some(i.processes.entry(key).or_default())
    }

    /// Counts one hydration of `path` (`bytes` long) by `image`.
    pub fn record_hydration(&self, path: &RepoPath, image: &str, bytes: u64, at: SystemTime) {
        let mut i = self.lock();
        if let Some(p) = Self::process(&mut i, image) {
            p.hydrations += 1;
            p.bytes = p.bytes.saturating_add(bytes);
            p.last_read = Some(at);
        }
        if !i.paths.contains_key(path) && i.paths.len() >= MAX_PATHS {
            i.dropped_paths += 1;
            return;
        }
        let s = i.paths.entry(path.clone()).or_default();
        s.hydrations += 1;
        s.bytes = s.bytes.saturating_add(bytes);
        s.last_read = Some(at);
    }

    /// Counts one read refused by the deny list.
    pub fn record_denied(&self, image: &str) {
        let mut i = self.lock();
        if let Some(p) = Self::process(&mut i, image) {
            p.denied += 1;
        }
    }

    pub fn path(&self, path: &RepoPath) -> Option<PathStats> {
        self.lock().paths.get(path).copied()
    }

    pub fn process_stats(&self, image: &str) -> Option<ProcessStats> {
        self.lock().processes.get(&image_name(image)).copied()
    }

    /// When `image` last hydrated anything.
    pub fn last_read(&self, image: &str) -> Option<SystemTime> {
        self.process_stats(image).and_then(|p| p.last_read)
    }

    /// When `path` was last hydrated, for idle dehydration.
    pub fn last_read_of(&self, path: &RepoPath) -> Option<SystemTime> {
        self.path(path).and_then(|p| p.last_read)
    }

    /// Everything, paths and processes by descending hydration count.
    pub fn snapshot(&self) -> Snapshot {
        let i = self.lock();
        let mut paths: Vec<_> = i.paths.iter().map(|(k, v)| (k.clone(), *v)).collect();
        paths.sort_by(|a, b| b.1.hydrations.cmp(&a.1.hydrations).then(a.0.cmp(&b.0)));
        let mut processes: Vec<_> = i.processes.iter().map(|(k, v)| (k.clone(), *v)).collect();
        processes.sort_by(|a, b| b.1.hydrations.cmp(&a.1.hydrations).then(a.0.cmp(&b.0)));
        Snapshot {
            total_hydrations: processes.iter().map(|(_, p)| p.hydrations).sum(),
            paths,
            processes,
            dropped_paths: i.dropped_paths,
            dropped_processes: i.dropped_processes,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn counts_and_bounds() {
        let s = Stats::new();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let p = RepoPath::new("a/b").unwrap();
        s.record_hydration(&p, r"C:\x\Rg.exe", 5, t);
        s.record_hydration(&p, "rg.exe", 7, t + Duration::from_secs(1));
        assert_eq!(s.path(&p).unwrap().hydrations, 2);
        assert_eq!(s.process_stats("RG.EXE").unwrap().bytes, 12);
        assert_eq!(s.last_read("rg.exe"), Some(t + Duration::from_secs(1)));
        for i in 0..(MAX_PROCESSES + 5) {
            s.record_denied(&format!("p{i}"));
        }
        // `rg.exe` holds one slot.
        assert_eq!(s.snapshot().dropped_processes, 6);
    }
}
