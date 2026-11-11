//! Listing, usage, clearing and eviction. Entries go least recently used first until the store
//! is under its size; a blob goes when no entry refers to it. A blob or temporary file that no
//! entry refers to yet may belong to a write in progress, so it is kept for [`GRACE`] seconds.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::key::ActionKey;
use crate::store::{Action, EntryInfo, Store, remove_readonly};
use crate::{now, unhex};

/// Seconds an unreferenced blob or temporary file is kept.
pub const GRACE: u64 = 3600;

/// What the store holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub entries: u64,
    pub blobs: u64,
    /// Blobs kept compressed (`<hex>.zst`).
    pub compressed: u64,
    /// Bytes of blobs and entries, as stored.
    pub bytes: u64,
}

/// What `gc` or `clear` removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GcReport {
    pub entries: u64,
    pub blobs: u64,
    pub freed: u64,
    pub before: Usage,
    pub after: Usage,
}

fn mtime(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Every file two levels below `dir` (`<2 hex>/<name>`), with its metadata.
fn sharded_files(dir: &Path) -> io::Result<Vec<(String, PathBuf, fs::Metadata)>> {
    let mut out = Vec::new();
    for shard in fs::read_dir(dir)? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for f in fs::read_dir(shard.path())? {
            let f = f?;
            let Ok(name) = f.file_name().into_string() else {
                continue;
            };
            let Ok(meta) = f.metadata() else { continue };
            if meta.is_file() {
                out.push((name, f.path(), meta));
            }
        }
    }
    Ok(out)
}

/// The blob id a `cas` file name stands for: `<hex>` or `<hex>.zst`.
fn blob_id(name: &str) -> Option<[u8; 32]> {
    unhex(name.strip_suffix(".zst").unwrap_or(name))
}

impl Action {
    /// Every blob the entry refers to.
    pub(crate) fn blobs(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.outputs
            .iter()
            .map(|o| o.blob.as_str())
            .chain(self.stdout.as_deref())
            .chain(self.stderr.as_deref())
            .filter_map(unhex)
    }
}

impl Store {
    /// Every readable entry, most recently used first.
    pub fn list(&self) -> io::Result<Vec<EntryInfo>> {
        let mut out = Vec::new();
        for (name, path, meta) in sharded_files(&self.ac())? {
            let Some(key) = ActionKey::parse(&name) else {
                continue;
            };
            let Ok(body) = fs::read(&path) else { continue };
            let Ok(action) = serde_json::from_slice::<Action>(&body) else {
                continue;
            };
            out.push(EntryInfo {
                key,
                action,
                last_use: mtime(&meta),
                entry_bytes: meta.len(),
            });
        }
        out.sort_by(|a, b| b.last_use.cmp(&a.last_use).then(a.key.cmp(&b.key)));
        Ok(out)
    }

    /// One entry by key or by a unique key prefix (at least 4 hex digits), without marking it
    /// used.
    pub fn find(&self, prefix: &str) -> io::Result<Option<EntryInfo>> {
        let p = prefix.to_ascii_lowercase();
        if p.len() < 4 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let mut hits = self
            .list()?
            .into_iter()
            .filter(|e| e.key.to_string().starts_with(&p));
        let first = hits.next();
        if hits.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{prefix}' matches more than one entry"),
            ));
        }
        Ok(first)
    }

    pub fn usage(&self) -> io::Result<Usage> {
        let entries = sharded_files(&self.ac())?;
        let blobs = sharded_files(&self.cas())?;
        Ok(Usage {
            entries: entries.len() as u64,
            blobs: blobs.len() as u64,
            compressed: blobs.iter().filter(|(n, _, _)| n.ends_with(".zst")).count() as u64,
            bytes: entries
                .iter()
                .chain(blobs.iter())
                .map(|(_, _, m)| m.len())
                .sum(),
        })
    }

    /// Removes the entries of `tool` (every entry when `None`) and the blobs left unused.
    pub fn clear(&self, tool: Option<&str>) -> io::Result<GcReport> {
        let before = self.usage()?;
        let mut report = GcReport {
            before,
            ..GcReport::default()
        };
        for (name, path, meta) in sharded_files(&self.ac())? {
            let matches = match tool {
                None => true,
                Some(t) => fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Action>(&b).ok())
                    .is_some_and(|a| a.tool == t),
            };
            if matches && ActionKey::parse(&name).is_some() && fs::remove_file(&path).is_ok() {
                report.entries += 1;
                report.freed += meta.len();
            }
        }
        // Blobs nobody refers to now go at once: clearing is asked for.
        self.sweep(&mut report, 0)?;
        report.after = self.usage()?;
        Ok(report)
    }

    /// Evicts least recently used entries until the store is at most `max` bytes, then removes
    /// blobs no entry refers to (after [`GRACE`]), stale temporary files and old events.
    pub fn gc(&self, max: u64) -> io::Result<GcReport> {
        let before = self.usage()?;
        let mut report = GcReport {
            before,
            ..GcReport::default()
        };
        // A blob is one file, or briefly two while it is being stored plain.
        let mut blob_files: HashMap<[u8; 32], Vec<(PathBuf, u64)>> = HashMap::new();
        for (n, path, m) in sharded_files(&self.cas())? {
            if let Some(id) = blob_id(&n) {
                blob_files.entry(id).or_default().push((path, m.len()));
            }
        }
        let mut entries = self.list()?;
        entries.reverse(); // least recently used first
        let mut refs: HashMap<[u8; 32], u32> = HashMap::new();
        for e in &entries {
            for b in e.action.blobs().collect::<HashSet<_>>() {
                *refs.entry(b).or_default() += 1;
            }
        }
        let mut total = before.bytes;
        for e in &entries {
            if total <= max {
                break;
            }
            if fs::remove_file(self.entry_path(&e.key)).is_err() {
                continue;
            }
            report.entries += 1;
            report.freed += e.entry_bytes;
            total = total.saturating_sub(e.entry_bytes);
            for b in e.action.blobs().collect::<HashSet<_>>() {
                let n = refs.entry(b).or_default();
                *n = n.saturating_sub(1);
                if *n == 0
                    && let Some(files) = blob_files.remove(&b)
                {
                    let mut removed = false;
                    for (path, size) in files {
                        if remove_readonly(&path).is_ok() {
                            removed = true;
                            report.freed += size;
                            total = total.saturating_sub(size);
                        }
                    }
                    if removed {
                        report.blobs += 1;
                    }
                }
            }
        }
        self.sweep(&mut report, GRACE)?;
        let cutoff = now().saturating_sub(GRACE);
        for f in fs::read_dir(self.root().join("tmp"))? {
            let f = f?;
            if f.metadata().is_ok_and(|m| mtime(&m) < cutoff) {
                let _ = remove_readonly(&f.path());
            }
        }
        self.prune_events()?;
        report.after = self.usage()?;
        Ok(report)
    }

    /// Runs [`Store::gc`] when the store is over `max`, at most once per [`GRACE`] across every
    /// process (`gc.stamp` records the last check). Adapters call it after a write.
    pub fn maybe_gc(&self, max: u64) -> io::Result<Option<GcReport>> {
        let stamp = self.root().join("gc.stamp");
        if fs::metadata(&stamp).is_ok_and(|m| mtime(&m) + GRACE > now()) {
            return Ok(None);
        }
        fs::write(&stamp, now().to_string())?;
        if self.usage()?.bytes <= max {
            return Ok(None);
        }
        self.gc(max).map(Some)
    }

    /// Removes blobs no entry refers to that are older than `grace` seconds.
    fn sweep(&self, report: &mut GcReport, grace: u64) -> io::Result<()> {
        let live: HashSet<[u8; 32]> = self.list()?.iter().flat_map(|e| e.action.blobs()).collect();
        let cutoff = now().saturating_sub(grace);
        for (name, path, meta) in sharded_files(&self.cas())? {
            let Some(id) = blob_id(&name) else { continue };
            if live.contains(&id) || (grace > 0 && mtime(&meta) >= cutoff) {
                continue;
            }
            if remove_readonly(&path).is_ok() {
                report.blobs += 1;
                report.freed += meta.len();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex;
    use crate::store::{Origin, Output, OutputKind};
    use std::time::{Duration, SystemTime};

    fn entry(s: &Store, tool: &str, n: u8, content: &[u8]) -> ActionKey {
        let (id, size) = s.put_blob(content).unwrap();
        let key = ActionKey::builder(tool).part("n", &[n]).finish();
        let mut a = Action::new(tool, &format!("e{n}"), Origin::default());
        a.outputs.push(Output {
            path: "out".into(),
            kind: OutputKind::File,
            size,
            blob: hex(&id),
        });
        s.put_action(&key, &a).unwrap();
        key
    }

    fn age(path: &Path, secs: u64) {
        let f = fs::File::options().write(true).open(path);
        let t = SystemTime::now() - Duration::from_secs(secs);
        match f {
            Ok(f) => f.set_modified(t).unwrap(),
            Err(_) => {
                // Read-only blob: make it writable for the test.
                let mut p = fs::metadata(path).unwrap().permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                p.set_readonly(false);
                fs::set_permissions(path, p).unwrap();
                fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
        }
    }

    #[test]
    fn gc_evicts_least_recently_used_and_shared_blobs_survive() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let old = entry(&s, "rustc", 1, &[1u8; 4000]);
        let shared = entry(&s, "rustc", 2, &[2u8; 4000]);
        let new = entry(&s, "vite", 3, &[2u8; 4000]); // same blob as `shared`
        age(&s.entry_path(&old), 300);
        age(&s.entry_path(&shared), 200);
        assert_eq!(s.list().unwrap()[0].key, new, "most recent first");

        let u = s.usage().unwrap();
        assert_eq!((u.entries, u.blobs), (3, 2));
        let r = s.gc(u.bytes - 1).unwrap();
        assert_eq!((r.entries, r.blobs), (1, 1), "{r:?}");
        assert!(s.get_action(&old).unwrap().is_none());

        // `shared` goes next but its blob is still used by `new`.
        let r = s.gc(r.after.bytes - 1).unwrap();
        assert_eq!((r.entries, r.blobs), (1, 0), "{r:?}");
        let a = s.get_action(&new).unwrap().unwrap();
        assert!(s.restore(&a, &d.path().join("w")).unwrap());
    }

    #[test]
    fn orphans_wait_out_the_grace_period_and_clear_takes_one_tool() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let (fresh, _) = s.put_blob(b"in flight").unwrap();
        let (stale, _) = s.put_blob(b"abandoned").unwrap();
        age(&s.blob_path(&stale), GRACE + 60);
        fs::write(s.root().join("tmp/x"), "partial").unwrap();
        age(&s.root().join("tmp/x"), GRACE + 60);
        let r = s.gc(u64::MAX).unwrap();
        assert_eq!((r.entries, r.blobs), (0, 1));
        assert!(s.has_blob(&fresh) && !s.has_blob(&stale));
        assert!(!s.root().join("tmp/x").exists());

        let a = entry(&s, "rustc", 1, b"a");
        let v = entry(&s, "vite", 2, b"v");
        let r = s.clear(Some("vite")).unwrap();
        assert_eq!(r.entries, 1);
        assert!(s.get_action(&a).unwrap().is_some() && s.get_action(&v).unwrap().is_none());
        let r = s.clear(None).unwrap();
        assert_eq!(r.after, Usage::default(), "{r:?}");
    }

    #[test]
    fn find_by_prefix() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let k = entry(&s, "t", 1, b"x");
        let text = k.to_string();
        assert_eq!(s.find(&text[..8]).unwrap().unwrap().key, k);
        assert!(s.find("zz").unwrap().is_none());
        assert!(s.find(&text[..3]).unwrap().is_none(), "too short");
    }

    #[test]
    fn maybe_gc_runs_once_per_period() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let k = entry(&s, "t", 1, &[7u8; 1000]);
        let r = s.maybe_gc(0).unwrap().expect("over the limit: runs");
        assert_eq!(r.entries, 1);
        entry(&s, "t", 2, &[8u8; 1000]);
        assert_eq!(s.maybe_gc(0).unwrap(), None, "checked within the hour");
        age(&s.root().join("gc.stamp"), GRACE + 1);
        assert_eq!(s.maybe_gc(u64::MAX).unwrap(), None, "under the limit");
        assert!(s.get_action(&k).unwrap().is_none());
    }
}
