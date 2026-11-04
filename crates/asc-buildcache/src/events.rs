//! The event log: one JSON line per hit, miss or write, a file per UTC day, kept
//! [`KEEP_DAYS`] days. Each line is one `write` to a file opened for appending, so processes
//! writing at once never interleave inside a line.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::key::ActionKey;
use crate::now;
use crate::store::{Origin, Store};

/// Days of events kept.
pub const KEEP_DAYS: u64 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Hit,
    Miss,
    Put,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Seconds since the Unix epoch.
    pub t: u64,
    pub kind: EventKind,
    pub tool: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Bytes restored (hit) or stored (put).
    #[serde(default)]
    pub size: u64,
}

impl Event {
    pub fn new(kind: EventKind, tool: &str, key: &ActionKey, origin: &Origin, size: u64) -> Self {
        Event {
            t: now(),
            kind,
            tool: tool.to_owned(),
            key: key.to_string(),
            worktree: origin.worktree.clone(),
            workflow: origin.workflow.clone(),
            size,
        }
    }
}

/// Counts for one tool.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolStats {
    pub hits: u64,
    pub misses: u64,
    pub puts: u64,
    /// Bytes restored instead of rebuilt.
    pub restored: u64,
}

impl ToolStats {
    /// Hits over lookups, 0 to 1.
    pub fn hit_rate(&self) -> f64 {
        let n = self.hits + self.misses;
        if n == 0 {
            0.0
        } else {
            self.hits as f64 / n as f64
        }
    }
}

/// `yyyy-mm-dd` (UTC) of a Unix time.
pub(crate) fn day(t: u64) -> String {
    // Days to civil date (H. Hinnant).
    let z = (t / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

impl Store {
    /// Appends `event`. The log is advisory: a failure to write it never fails a build.
    pub fn record(&self, event: &Event) {
        let Ok(mut line) = serde_json::to_vec(event) else {
            return;
        };
        line.push(b'\n');
        let path = self.events_dir().join(format!("{}.jsonl", day(event.t)));
        if let Ok(mut f) = fs::File::options().create(true).append(true).open(path) {
            let _ = f.write_all(&line);
        }
    }

    /// Every event at or after `since`, oldest first. Lines that do not parse are skipped.
    pub fn events(&self, since: u64) -> io::Result<Vec<Event>> {
        let first = day(since);
        let mut names: Vec<String> = fs::read_dir(self.events_dir())?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".jsonl") && n[..n.len() - 6] >= *first)
            .collect();
        names.sort();
        let mut out = Vec::new();
        for n in names {
            let Ok(f) = fs::File::open(self.events_dir().join(n)) else {
                continue;
            };
            for line in io::BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                if let Ok(e) = serde_json::from_str::<Event>(&line)
                    && e.t >= since
                {
                    out.push(e);
                }
            }
        }
        out.sort_by_key(|e| e.t);
        Ok(out)
    }

    /// Counts per tool since `since`.
    pub fn stats(&self, since: u64) -> io::Result<BTreeMap<String, ToolStats>> {
        let mut out: BTreeMap<String, ToolStats> = BTreeMap::new();
        for e in self.events(since)? {
            let s = out.entry(e.tool).or_default();
            match e.kind {
                EventKind::Hit => {
                    s.hits += 1;
                    s.restored += e.size;
                }
                EventKind::Miss => s.misses += 1,
                EventKind::Put => s.puts += 1,
            }
        }
        Ok(out)
    }

    /// The events of one entry, oldest first: which work trees and workflows wrote and used it.
    pub fn events_for(&self, key: &ActionKey) -> io::Result<Vec<Event>> {
        let key = key.to_string();
        let since = now().saturating_sub(KEEP_DAYS * 86_400);
        Ok(self
            .events(since)?
            .into_iter()
            .filter(|e| e.key == key)
            .collect())
    }

    /// Removes day files older than [`KEEP_DAYS`]; returns how many.
    pub(crate) fn prune_events(&self) -> io::Result<usize> {
        let oldest = day(now().saturating_sub(KEEP_DAYS * 86_400));
        let mut n = 0;
        for e in fs::read_dir(self.events_dir())? {
            let e = e?;
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if name.ends_with(".jsonl") && name[..name.len() - 6] < *oldest {
                fs::remove_file(e.path())?;
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(951_782_400), "2000-02-29");
        assert_eq!(day(1_798_675_200), "2026-12-31");
    }

    #[test]
    fn stats_count_per_tool() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let k = ActionKey::builder("t").finish();
        let wt = Origin {
            worktree: Some("/w/one".into()),
            workflow: None,
        };
        assert_eq!(s.lookup("rustc", &k, &wt).unwrap(), None);
        let mut hit = Event::new(EventKind::Hit, "rustc", &k, &wt, 10);
        s.record(&hit);
        hit.worktree = Some("/w/two".into());
        s.record(&hit);
        s.record(&Event::new(
            EventKind::Put,
            "vite",
            &k,
            &Origin::default(),
            5,
        ));
        let st = s.stats(0).unwrap();
        assert_eq!(
            st["rustc"],
            ToolStats {
                hits: 2,
                misses: 1,
                puts: 0,
                restored: 20
            }
        );
        assert!((st["rustc"].hit_rate() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(st["vite"].puts, 1);
        let used: Vec<_> = s
            .events_for(&k)
            .unwrap()
            .into_iter()
            .filter_map(|e| e.worktree)
            .collect();
        assert_eq!(used, ["/w/one", "/w/one", "/w/two"]);

        fs::write(s.events_dir().join("2000-01-01.jsonl"), "{}\n").unwrap();
        assert_eq!(s.prune_events().unwrap(), 1);
    }
}
