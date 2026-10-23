//! Windows ProjFS backend (SPEC §10.2): the binding ([`ffi`]), the mount ([`provider`]), the
//! callbacks ([`enumerate`], [`data`], [`notify`]).
//!
//! Every path ProjFS hands a callback is untrusted: it is converted once, validated as a
//! `RepoPath`, and resolved against the base tree. No lock is held across a ProjFS call.

use std::future::Future;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};

use asc_core::path::RepoPath;
use asc_state::StateDb;
use asc_state::state::workspaces::Workspace;
use asc_wc::journal::{Journal, Op};
use asc_wc::times::entry_times_of;
use tokio::runtime::Handle;

pub mod data;
pub mod enumerate;
pub mod ffi;
pub mod notify;
pub mod provider;

pub use ffi::ProviderUnavailable;
pub use provider::{Config, ProjfsProvider};

/// The placeholder times of paths (SPEC §10.1).
pub trait TimeSource: Send + Sync {
    /// The time, in Unix seconds, of each path, in order.
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>>;
}

/// Where notifications append their change hints.
pub trait JournalSink: Send + Sync {
    /// Never blocks and never fails.
    fn record(&self, op: Op, path: &[u8]);
}

impl JournalSink for Journal {
    fn record(&self, op: Op, path: &[u8]) {
        Journal::record(self, op, path);
    }
}

/// [`TimeSource`] over `state.db`'s `entry_times` (U54v `times`).
pub struct DbTimes {
    pub db: StateDb,
    pub ws: Workspace,
    pub rt: Handle,
}

impl TimeSource for DbTimes {
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
        block(&self.rt, entry_times_of(&self.db, &self.ws, paths))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// Runs `f` to completion from a thread that may or may not be inside the runtime. ProjFS
/// callback threads are not; `DiskState` callers may be.
pub(crate) fn block<F>(rt: &Handle, f: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| rt.block_on(f))
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
        })
    } else {
        rt.block_on(f)
    }
}

/// Runs a callback body; a panic must not unwind into ProjFS.
pub(crate) fn guard(f: impl FnOnce() -> ffi::Hresult) -> ffi::Hresult {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(ffi::E_FAIL)
}

/// Converts a path ProjFS reports (backslash separated, relative to the root, `""` for the root)
/// to a [`RepoPath`]. `Ok(None)` is the root; `Err` is a path that must be refused.
pub(crate) fn rel_path(w: &str) -> Result<Option<RepoPath>, ()> {
    if w.is_empty() {
        return Ok(None);
    }
    if w.contains('/') {
        return Err(());
    }
    RepoPath::new(w.replace('\\', "/").into_bytes())
        .map(Some)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_converts_and_refuses() {
        assert_eq!(rel_path(""), Ok(None));
        assert_eq!(
            rel_path("src\\lib.rs").unwrap().unwrap().as_bytes(),
            b"src/lib.rs"
        );
        for bad in [
            "a/b", "\\a", "a\\\\b", "a\\", "..\\x", "a\\..\\b", "a\\.\\b",
        ] {
            assert!(rel_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn guard_turns_a_panic_into_failure() {
        assert_eq!(guard(|| panic!("boom")), ffi::E_FAIL);
        assert_eq!(guard(|| ffi::S_OK), ffi::S_OK);
    }
}

/// Tests against the real ProjFS. Each skips (and says so) where `Client-ProjFS` is not enabled.
#[cfg(test)]
mod fixture {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use asc_core::codec::Enveloped;
    use asc_core::id::{FileId, GitOid, TreeId};
    use asc_core::object::{Entry, Kind, Tree};
    use asc_core::path::Name;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;
    use asc_wc::redirect::Redirections;

    use super::*;
    use crate::core::{CoreFs, Options, Provider};
    use crate::policy::HydrationPolicy;

    pub(super) const FILES: usize = 1000;
    pub(super) const EPOCH_TIME: i64 = 1_700_000_000;
    pub(super) const WORKSPACE: [u8; 16] = [
        0x6b, 0x1f, 0x3a, 0x90, 0x11, 0x22, 0x4c, 0x33, 0x84, 0x55, 0x01, 0x02, 0x03, 0x04, 0x05,
        0x06,
    ];

    pub(super) struct FixedTimes;

    impl TimeSource for FixedTimes {
        fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
            Ok(vec![EPOCH_TIME; paths.len()])
        }
    }

    #[derive(Default)]
    pub(super) struct Recorded(Mutex<Vec<(Op, Vec<u8>)>>);

    impl JournalSink for Recorded {
        fn record(&self, op: Op, path: &[u8]) {
            self.0.lock().unwrap().push((op, path.to_vec()));
        }
    }

    impl Recorded {
        pub(super) fn has(&self, op: Op, path: &str) -> bool {
            self.0
                .lock()
                .unwrap()
                .iter()
                .any(|(o, p)| *o == op && p == path.as_bytes())
        }

        pub(super) fn any_under(&self, prefix: &str) -> bool {
            self.0
                .lock()
                .unwrap()
                .iter()
                .any(|(_, p)| p.starts_with(prefix.as_bytes()))
        }

        pub(super) fn wait_for(&self, op: Op, path: &str) -> bool {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                if self.has(op, path) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        }
    }

    pub(super) struct Fx {
        pub(super) rt: tokio::runtime::Runtime,
        pub(super) dir: tempfile::TempDir,
        pub(super) core: Arc<CoreFs>,
        pub(super) journal: Arc<Recorded>,
        pub(super) provider: ProjfsProvider,
    }

    impl Fx {
        pub(super) fn root(&self) -> std::path::PathBuf {
            self.dir.path().join("ws")
        }
    }

    pub(super) fn entry(name: &str, kind: Kind, id: [u8; 32], size: u64) -> Entry {
        Entry {
            name: Name::new(name).unwrap(),
            kind,
            id,
            size,
            git: GitOid::from_bytes([0; 20]),
        }
    }

    pub(super) fn body(i: usize) -> Vec<u8> {
        format!("file number {i}\n").into_bytes()
    }

    pub(super) async fn put(store: &BucketObjects, key: ObjKey, data: &[u8]) {
        store.put(&key, encode_stored(data)).await.unwrap();
    }

    pub(super) async fn tree(store: &BucketObjects, mut entries: Vec<Entry>) -> TreeId {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let t = Tree::new(entries, true).unwrap();
        put(store, ObjKey::Tree(t.id()), &t.to_envelope()).await;
        t.id()
    }

    /// `src/f0000.txt` .. `src/f0999.txt` and `README`.
    pub(super) async fn build(store: &BucketObjects) -> TreeId {
        let mut files = Vec::new();
        for i in 0..FILES {
            let data = body(i);
            let id = FileId::of(&data);
            put(store, ObjKey::File(id), &data).await;
            files.push(entry(
                &format!("f{i:04}.txt"),
                Kind::File,
                *id.as_bytes(),
                data.len() as u64,
            ));
        }
        let src = tree(store, files).await;
        let readme = b"read me\n";
        let rid = FileId::of(readme);
        put(store, ObjKey::File(rid), readme).await;
        tree(
            store,
            vec![
                entry("src", Kind::Dir, *src.as_bytes(), 0),
                entry("README", Kind::File, *rid.as_bytes(), readme.len() as u64),
            ],
        )
        .await
    }

    /// Mounts the fixture tree, or `None` (after saying why) where ProjFS is unavailable.
    pub(super) fn mount(deny: &str) -> Option<Fx> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(BucketObjects::memory());
        let root_tree = rt.block_on(build(&store));
        let red = Redirections::parse("node_modules\n");
        let opts = Options {
            redirections: red.clone(),
            policy: if deny.is_empty() {
                HydrationPolicy::allow_all()
            } else {
                HydrationPolicy::parse(deny)
            },
            ..Options::windows()
        };
        let core = Arc::new(CoreFs::new(store.clone(), root_tree, opts));
        let journal = Arc::new(Recorded::default());
        let cfg = Config {
            workspace: WORKSPACE,
            core: core.clone(),
            objects: store,
            local: None,
            times: Arc::new(FixedTimes),
            journal: journal.clone(),
            redirections: red,
            rt: rt.handle().clone(),
        };
        let provider = match ProjfsProvider::new(cfg) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping: {e}");
                return None;
            }
        };
        let fx = Fx {
            rt,
            dir: tempfile::tempdir().unwrap(),
            core,
            journal,
            provider,
        };
        if let Err(e) = fx.provider.mount(&fx.root()) {
            eprintln!("skipping: {e}");
            return None;
        }
        Some(fx)
    }

    pub(super) fn mtime(p: &Path) -> i64 {
        let t = std::fs::metadata(p).unwrap().modified().unwrap();
        i64::try_from(t.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
    }
}

#[cfg(test)]
mod basic {
    use super::fixture::*;
    use super::*;
    use crate::core::Provider;

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn enumeration_does_not_hydrate() {
        let Some(fx) = mount("") else { return };
        let names: Vec<_> = std::fs::read_dir(fx.root().join("src"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), FILES);
        assert_eq!(std::fs::read_dir(fx.root()).unwrap().count(), 2);
        // Sizes and times come from the tree, with no data fetched.
        let m = std::fs::metadata(fx.root().join("src").join("f0007.txt")).unwrap();
        assert_eq!(m.len(), body(7).len() as u64);
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 0);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn reading_one_file_hydrates_exactly_one() {
        let Some(fx) = mount("") else { return };
        let p = fx.root().join("src").join("f0042.txt");
        assert_eq!(std::fs::read(&p).unwrap(), body(42));
        let snap = fx.core.stats().snapshot();
        assert_eq!(snap.total_hydrations, 1);
        assert_eq!(snap.paths.len(), 1);
        // Case-insensitive lookup reaches the same tree.
        let upper = fx.root().join("SRC").join("F0043.TXT");
        assert_eq!(std::fs::read(upper).unwrap(), body(43));
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 2);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn times_are_the_epoch_time_before_and_after_hydration() {
        let Some(fx) = mount("") else { return };
        let p = fx.root().join("src").join("f0001.txt");
        assert_eq!(mtime(&p), EPOCH_TIME);
        let _ = std::fs::read(&p).unwrap();
        assert_eq!(mtime(&p), EPOCH_TIME);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn a_write_is_journaled() {
        let Some(fx) = mount("") else { return };
        let p = fx.root().join("src").join("f0002.txt");
        std::fs::write(&p, b"changed").unwrap();
        assert!(fx.journal.wait_for(Op::Modified, "src/f0002.txt"));
        let new = fx.root().join("src").join("new.txt");
        std::fs::write(&new, b"new").unwrap();
        assert!(fx.journal.wait_for(Op::Created, "src/new.txt"));
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn a_denied_process_gets_access_denied() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_str().unwrap().to_owned();
        let Some(fx) = mount(&name) else { return };
        let err = std::fs::read(fx.root().join("README")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 0);
        assert!(
            fx.core
                .stats()
                .process_stats(&name)
                .is_some_and(|s| s.denied >= 1)
        );
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn unmount_twice_fails_the_second_time() {
        let Some(fx) = mount("") else { return };
        fx.provider.unmount().unwrap();
        assert!(fx.provider.unmount().is_err());
        let _ = &fx.rt;
    }
}

#[cfg(test)]
mod redirect {
    use super::fixture::*;
    use super::*;

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn new_node_modules_not_journaled() {
        let Some(fx) = mount("") else { return };
        let nm = fx.root().join("node_modules").join("pkg");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("index.js"), b"x").unwrap();
        std::fs::write(fx.root().join("kept.txt"), b"y").unwrap();
        assert!(fx.journal.wait_for(Op::Created, "kept.txt"));
        assert!(!fx.journal.any_under("node_modules"));
        // The directory is a real one: it is not projected from the tree.
        assert!(nm.join("index.js").is_file());
    }
}
