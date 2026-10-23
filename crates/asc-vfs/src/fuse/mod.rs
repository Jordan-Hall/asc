//! Linux FUSE backend (SPEC §10.3): [`inodes`] is the persisted inode table, [`overlay`] holds
//! what the user changed, [`passthrough`] hands clean files to the kernel, and [`fs`] is the
//! `fuser` file system and the [`core::Provider`](crate::core::Provider) over it.
//!
//! Every name, path and handle the kernel sends is untrusted: names are validated once, paths are
//! built as `RepoPath`s, and the overlay is only reached through `SafeRoot`, so a symlink in an
//! ancestor is never followed.

use std::future::Future;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use asc_core::path::RepoPath;
use asc_wc::journal::Op;
use tokio::runtime::Handle;

pub mod fs;
pub mod inodes;
pub mod overlay;
pub mod passthrough;

pub use fs::{Config, FuseProvider};

/// The note `asc switch`, `pull` and `reset` print when a process watches the mount (SPEC §10.3
/// [audit: watchers-virtual-switch]).
pub const WATCH_NOTE: &str =
    "note: file watchers do not see this switch; restart watch processes or use polling";

/// Most `/proc` entries the watcher scan looks at.
const MAX_SCAN_PROCS: usize = 200_000;
/// Most descriptors the watcher scan examines in total.
const MAX_SCAN_FDS: usize = 2_000_000;
/// Longest `fdinfo` text read.
const MAX_FDINFO: u64 = 1 << 20;

/// The placeholder times of paths (SPEC §10.1).
pub trait TimeSource: Send + Sync {
    /// The time, in Unix seconds, of each path, in order.
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>>;
}

/// Where the file system appends its change hints. Every write passes through the daemon, so the
/// hints are exact.
pub trait JournalSink: Send + Sync {
    /// Never blocks and never fails.
    fn record(&self, op: Op, path: &[u8]);
}

impl JournalSink for asc_wc::journal::Journal {
    fn record(&self, op: Op, path: &[u8]) {
        asc_wc::journal::Journal::record(self, op, path);
    }
}

/// Runs `f` to completion from a thread that may or may not be inside the runtime. FUSE worker
/// threads are not; `DiskState` callers may be.
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

/// Whether one `/proc/<pid>/fdinfo/<fd>` text is an inotify descriptor with a watch on the file
/// system whose kernel device number is `sdev`.
pub(crate) fn fdinfo_watches(text: &str, sdev: u64) -> bool {
    text.lines().any(|l| {
        l.strip_prefix("inotify ").is_some_and(|rest| {
            rest.split_whitespace()
                .filter_map(|w| w.strip_prefix("sdev:"))
                .any(|v| u64::from_str_radix(v, 16) == Ok(sdev))
        })
    })
}

/// The kernel's internal device number of a `stat` device number.
fn kernel_dev(dev: u64) -> u64 {
    (u64::from(rustix::fs::major(dev)) << 20) | u64::from(rustix::fs::minor(dev))
}

/// The processes holding an inotify watch on the file system of `mount`, from `/proc/*/fdinfo`.
/// Processes that cannot be examined are skipped; the scan is bounded.
pub fn inotify_users(mount: &Path) -> io::Result<Vec<u32>> {
    use std::io::Read;
    let sdev = kernel_dev(std::fs::metadata(mount)?.dev());
    let mut pids = Vec::new();
    let mut fds = 0usize;
    for (n, e) in std::fs::read_dir("/proc")?.flatten().enumerate() {
        if n >= MAX_SCAN_PROCS || fds >= MAX_SCAN_FDS {
            break;
        }
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let dir = e.path();
        let Ok(rd) = std::fs::read_dir(dir.join("fd")) else {
            continue;
        };
        for fd in rd.flatten() {
            fds += 1;
            if fds >= MAX_SCAN_FDS {
                break;
            }
            let is_inotify = std::fs::read_link(fd.path())
                .is_ok_and(|t| t.as_os_str().as_encoded_bytes() == b"anon_inode:inotify");
            if !is_inotify {
                continue;
            }
            let mut text = String::new();
            let read = std::fs::File::open(dir.join("fdinfo").join(fd.file_name()))
                .and_then(|f| f.take(MAX_FDINFO).read_to_string(&mut text));
            if read.is_ok() && fdinfo_watches(&text, sdev) {
                pids.push(pid);
                break;
            }
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdinfo_matches_only_inotify_on_the_device() {
        let text = "pos:\t0\nflags:\t02000000\nmnt_id:\t12\nino:\t5f\n\
                    inotify wd:1 ino:2a sdev:2c mask:fc6 ignored_mask:0 fhandle-bytes:8\n\
                    inotify wd:2 ino:2b sdev:7 mask:fc6 ignored_mask:0 fhandle-bytes:8\n";
        assert!(fdinfo_watches(text, 0x2c));
        assert!(fdinfo_watches(text, 7));
        assert!(!fdinfo_watches(text, 0x2d));
        assert!(!fdinfo_watches(
            "tfd: 5 events: 19 data: 5 pos:0 ino:2c sdev:2c",
            0x2c
        ));
        assert!(!fdinfo_watches("", 1));
    }

    #[test]
    fn kernel_device_numbers_pack_major_over_minor() {
        // glibc's encoding of major 0, minor 44 is 44; the kernel's is the same for major 0.
        assert_eq!(kernel_dev(44), 44);
        // major 8, minor 1 (sda1): glibc 0x801, kernel 8 << 20 | 1.
        assert_eq!(kernel_dev(0x801), (8 << 20) | 1);
    }
}

/// Tests against a real mount. Each skips (and says so) where `/dev/fuse` cannot be mounted.
#[cfg(test)]
mod fixture {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use asc_core::codec::Enveloped;
    use asc_core::id::{FileId, GitOid, TreeId};
    use asc_core::object::{Entry, Kind, Tree};
    use asc_core::path::Name;
    use asc_store::bucket::BucketObjects;
    use asc_store::cachedir::CacheDir;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;
    use asc_wc::redirect::Redirections;

    use super::inodes::MemInodes;
    use super::*;
    use crate::core::{CoreFs, Options, Provider};
    use crate::policy::HydrationPolicy;

    pub(super) const EPOCH_TIME: i64 = 1_700_000_000;

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
        pub(super) fn ops(&self) -> Vec<(Op, String)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(o, p)| (*o, String::from_utf8_lossy(p).into_owned()))
                .collect()
        }
    }

    pub(super) struct Fx {
        pub(super) provider: Option<FuseProvider>,
        pub(super) core: Arc<CoreFs>,
        pub(super) journal: Arc<Recorded>,
        pub(super) inodes: Arc<MemInodes>,
        pub(super) store: Arc<BucketObjects>,
        pub(super) tree1: TreeId,
        pub(super) tree2: TreeId,
        pub(super) deny: String,
        pub(super) tmp: tempfile::TempDir,
        pub(super) rt: tokio::runtime::Runtime,
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

    async fn put(store: &BucketObjects, key: ObjKey, data: &[u8]) {
        store.put(&key, encode_stored(data)).await.unwrap();
    }

    async fn tree(store: &BucketObjects, mut entries: Vec<Entry>) -> TreeId {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let t = Tree::new(entries, true).unwrap();
        put(store, ObjKey::Tree(t.id()), &t.to_envelope()).await;
        t.id()
    }

    async fn blob(store: &BucketObjects, name: &str, kind: Kind, data: &[u8]) -> Entry {
        let id = FileId::of(data);
        put(store, ObjKey::File(id), data).await;
        entry(name, kind, *id.as_bytes(), data.len() as u64)
    }

    /// `README`, `run.sh`, `link`, `src/{a.txt,b.txt,sub/deep.txt}`; the second tree changes
    /// `a.txt` and `deep.txt`, drops `b.txt` and adds `c.txt`.
    async fn build(store: &BucketObjects) -> (TreeId, TreeId) {
        let mut roots = Vec::new();
        for second in [false, true] {
            let a: &[u8] = if second {
                b"alpha two, longer\n"
            } else {
                b"alpha\n"
            };
            let deep: &[u8] = if second { b"deep two\n" } else { b"deep\n" };
            let sub = tree(store, vec![blob(store, "deep.txt", Kind::File, deep).await]).await;
            let mut src = vec![
                blob(store, "a.txt", Kind::File, a).await,
                entry("sub", Kind::Dir, *sub.as_bytes(), 0),
            ];
            if second {
                src.push(blob(store, "c.txt", Kind::File, b"gamma\n").await);
            } else {
                src.push(blob(store, "b.txt", Kind::File, b"beta\n").await);
            }
            let src = tree(store, src).await;
            roots.push(
                tree(
                    store,
                    vec![
                        blob(store, "README", Kind::File, b"read me\n").await,
                        blob(store, "run.sh", Kind::Exec, b"#!/bin/sh\n").await,
                        blob(store, "link", Kind::Symlink, b"README").await,
                        entry("src", Kind::Dir, *src.as_bytes(), 0),
                    ],
                )
                .await,
            );
        }
        (roots[0], roots[1])
    }

    impl Fx {
        pub(super) fn root(&self) -> PathBuf {
            self.tmp.path().join("ws")
        }

        pub(super) fn path(&self, rel: &str) -> PathBuf {
            self.root().join(rel)
        }

        fn cache(&self) -> CacheDir {
            CacheDir::open(self.tmp.path().join("cache"), self.tmp.path().join("ctmp")).unwrap()
        }

        fn config(&self) -> Config {
            Config {
                core: self.core.clone(),
                local: Some(self.cache()),
                inodes: self.inodes.clone(),
                overlay: self.tmp.path().join("overlay"),
                redirect: self.tmp.path().join("redirect"),
                redirections: Redirections::parse("node_modules\n"),
                times: Arc::new(FixedTimes),
                journal: self.journal.clone(),
                rt: self.rt.handle().clone(),
                root_time: EPOCH_TIME,
                passthrough: true,
            }
        }

        /// Puts `README` and `src/a.txt` into the local content cache.
        fn warm(&self) {
            let cache = self.cache();
            for data in [&b"read me\n"[..], b"alpha\n"] {
                let id = FileId::of(data);
                self.rt
                    .block_on(cache.put(&ObjKey::File(id), encode_stored(data)))
                    .unwrap();
            }
        }

        /// Unmounts and mounts again over the same overlay and inode table.
        pub(super) fn remount(&mut self) -> bool {
            self.provider = None;
            self.start()
        }

        fn start(&mut self) -> bool {
            self.core = Arc::new(CoreFs::new(
                self.store.clone(),
                self.tree1,
                Options {
                    redirections: Redirections::parse("node_modules\n"),
                    policy: if self.deny.is_empty() {
                        HydrationPolicy::allow_all()
                    } else {
                        HydrationPolicy::parse(&self.deny)
                    },
                    ..Options::default()
                },
            ));
            let p = match FuseProvider::new(self.config()) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("skipping: {e}");
                    return false;
                }
            };
            if let Err(e) = p.mount(&self.root()) {
                eprintln!("skipping: {e}");
                return false;
            }
            self.provider = Some(p);
            true
        }

        pub(super) fn provider(&self) -> &FuseProvider {
            self.provider.as_ref().unwrap()
        }
    }

    /// Mounts the fixture tree, or `None` (after saying why) where FUSE is unavailable.
    pub(super) fn mount(deny: &str) -> Option<Fx> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(BucketObjects::memory());
        let (tree1, tree2) = rt.block_on(build(&store));
        let core = Arc::new(CoreFs::new(store.clone(), tree1, Options::default()));
        let mut fx = Fx {
            provider: None,
            core,
            journal: Arc::new(Recorded::default()),
            inodes: Arc::new(MemInodes::default()),
            store,
            tree1,
            tree2,
            deny: deny.to_owned(),
            tmp: tempfile::tempdir().unwrap(),
            rt,
        };
        fx.warm();
        fx.start().then_some(fx)
    }

    pub(super) fn wait_for(f: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        f()
    }
}

#[cfg(test)]
mod mounted {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use asc_core::id::{FileId, GitOid};
    use asc_core::object::{Change, Kind};
    use asc_wc::switch::Plan;
    use asc_wc::times::Switched;
    use asc_wc::vstatus::{DiskKind, PathState};

    use super::fixture::*;
    use super::*;
    use crate::core::Provider;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn getattr_uses_tree_metadata_and_epoch_times() {
        let Some(fx) = mount("") else { return };
        assert_eq!(names(&fx.root()), ["README", "link", "run.sh", "src"]);
        assert_eq!(names(&fx.path("src")), ["a.txt", "b.txt", "sub"]);
        let m = std::fs::metadata(fx.path("src/a.txt")).unwrap();
        assert_eq!(m.len(), 6);
        assert_eq!(m.mtime(), EPOCH_TIME);
        assert_eq!(m.mode() & 0o7777, 0o644);
        let x = std::fs::metadata(fx.path("run.sh")).unwrap();
        assert_eq!(x.mode() & 0o7777, 0o755);
        assert!(std::fs::metadata(fx.path("src")).unwrap().is_dir());
        assert_eq!(
            std::fs::read_link(fx.path("link")).unwrap(),
            std::path::Path::new("README")
        );
        // Listing and `stat` do not hydrate anything.
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 0);
        assert_eq!(fx.provider().overlay_bytes().unwrap(), 0);
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn inode_numbers_are_stable_across_remount() {
        let Some(mut fx) = mount("") else { return };
        let paths = ["README", "src", "src/a.txt", "src/sub/deep.txt"];
        let before: Vec<u64> = paths
            .iter()
            .map(|p| std::fs::metadata(fx.path(p)).unwrap().ino())
            .collect();
        assert!(before.iter().all(|&i| i > 1));
        let mut uniq = before.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), before.len());
        std::fs::write(fx.path("made.txt"), b"kept").unwrap();
        let made = std::fs::metadata(fx.path("made.txt")).unwrap().ino();
        std::fs::remove_file(fx.path("src/b.txt")).unwrap();
        if !fx.remount() {
            return;
        }
        let after: Vec<u64> = paths
            .iter()
            .map(|p| std::fs::metadata(fx.path(p)).unwrap().ino())
            .collect();
        assert_eq!(before, after);
        assert_eq!(std::fs::metadata(fx.path("made.txt")).unwrap().ino(), made);
        // The overlay outlives the mount: the new file and the deletion are still there.
        assert_eq!(std::fs::read(fx.path("made.txt")).unwrap(), b"kept");
        assert_eq!(names(&fx.path("src")), ["a.txt", "sub"]);
        // New names never reuse a number.
        std::fs::write(fx.path("later.txt"), b"x").unwrap();
        let later = std::fs::metadata(fx.path("later.txt")).unwrap().ino();
        assert!(!before.contains(&later) && later != made);
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn passthrough_read_costs_no_overlay_bytes() {
        let Some(fx) = mount("") else { return };
        // `README` is in the local cache, `src/b.txt` is not.
        assert_eq!(std::fs::read(fx.path("README")).unwrap(), b"read me\n");
        assert_eq!(fx.provider().overlay_bytes().unwrap(), 0);
        let (usable, registered) = fx.provider().passthrough_stats();
        if usable {
            assert!(
                registered >= 1,
                "passthrough negotiated but nothing registered"
            );
        } else {
            eprintln!("note: kernel passthrough is not available here");
        }
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 1);
        assert_eq!(std::fs::read(fx.path("src/b.txt")).unwrap(), b"beta\n");
        assert_eq!(fx.provider().overlay_bytes().unwrap(), 0);
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 2);
        // A partial read at an offset.
        let f = std::fs::File::open(fx.path("src/a.txt")).unwrap();
        let mut buf = [0u8; 3];
        std::os::unix::fs::FileExt::read_exact_at(&f, &mut buf, 2).unwrap();
        assert_eq!(&buf, b"pha");
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn a_denied_process_gets_eacces() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_str().unwrap().to_owned();
        let Some(fx) = mount(&name) else { return };
        let err = std::fs::read(fx.path("src/b.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let err = std::fs::read(fx.path("README")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fx.core.stats().snapshot().total_hydrations, 0);
        assert!(
            fx.core
                .stats()
                .process_stats(&name)
                .is_some_and(|s| s.denied >= 2)
        );
        // Listing and `stat` stay allowed.
        assert_eq!(std::fs::metadata(fx.path("README")).unwrap().len(), 8);
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn the_journal_is_exact() {
        let Some(fx) = mount("") else { return };
        std::fs::write(fx.path("f1"), b"data").unwrap();
        std::fs::rename(fx.path("f1"), fx.path("f2")).unwrap();
        std::fs::remove_file(fx.path("f2")).unwrap();
        std::fs::create_dir(fx.path("dd")).unwrap();
        std::fs::remove_dir(fx.path("dd")).unwrap();
        let want: Vec<(Op, String)> = vec![
            (Op::Created, "f1".to_owned()),
            (Op::RenamedFrom, "f1".to_owned()),
            (Op::Created, "f2".to_owned()),
            (Op::Deleted, "f2".to_owned()),
            (Op::Created, "dd".to_owned()),
            (Op::Deleted, "dd".to_owned()),
        ];
        assert_eq!(fx.journal.ops(), want);
        // Created and removed again: nothing is left in the overlay.
        assert!(fx.provider().whiteouts().unwrap().is_empty());
        assert_eq!(names(&fx.root()), ["README", "link", "run.sh", "src"]);
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn overlay_holds_exactly_what_changed() {
        let Some(fx) = mount("") else { return };
        // Copy-up on write: the old bytes survive an append.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(fx.path("README"))
            .unwrap();
        f.write_all(b"more\n").unwrap();
        drop(f);
        assert_eq!(
            std::fs::read(fx.path("README")).unwrap(),
            b"read me\nmore\n"
        );
        // Truncating write.
        std::fs::write(fx.path("src/a.txt"), b"new").unwrap();
        assert_eq!(std::fs::read(fx.path("src/a.txt")).unwrap(), b"new");
        assert_eq!(std::fs::metadata(fx.path("src/a.txt")).unwrap().len(), 3);
        // The mode of a base file can change.
        std::fs::set_permissions(fx.path("run.sh"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(
            std::fs::metadata(fx.path("run.sh")).unwrap().mode() & 0o7777,
            0o700
        );
        // Unlink hides the base entry.
        std::fs::remove_file(fx.path("src/b.txt")).unwrap();
        assert_eq!(names(&fx.path("src")), ["a.txt", "sub"]);
        assert!(!fx.path("src/b.txt").exists());
        // A non-empty directory cannot be removed.
        let err = std::fs::remove_dir(fx.path("src/sub")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty);
        // Renaming a base directory moves everything below it.
        std::fs::rename(fx.path("src/sub"), fx.path("moved")).unwrap();
        assert_eq!(std::fs::read(fx.path("moved/deep.txt")).unwrap(), b"deep\n");
        assert!(!fx.path("src/sub").exists());
        // Symlinks and new directories.
        std::os::unix::fs::symlink("README", fx.path("ln")).unwrap();
        assert_eq!(
            std::fs::read_link(fx.path("ln")).unwrap(),
            std::path::Path::new("README")
        );
        std::fs::create_dir(fx.path("d1")).unwrap();
        std::fs::write(fx.path("d1/x"), b"x").unwrap();
        assert_eq!(
            std::fs::create_dir(fx.path("d1")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        // Delete and recreate a base file: the new one wins and the old one stays hidden.
        std::fs::remove_file(fx.path("link")).unwrap();
        std::fs::write(fx.path("link"), b"file now").unwrap();
        assert_eq!(std::fs::read(fx.path("link")).unwrap(), b"file now");

        let mut white: Vec<String> = fx
            .provider()
            .whiteouts()
            .unwrap()
            .iter()
            .map(|p| String::from_utf8(p.as_bytes().to_vec()).unwrap())
            .collect();
        white.sort();
        assert_eq!(white, ["link", "src/b.txt", "src/sub"]);
        // Only the bytes of changed files are held: README(13) + a.txt(3) + run.sh(10) +
        // deep.txt(5) + x(1) + file now(8).
        assert_eq!(
            fx.provider().overlay_bytes().unwrap(),
            13 + 3 + 10 + 5 + 1 + 8
        );

        // The on-disk view the status code reads.
        let d = fx.provider().disk_state();
        assert_eq!(d.state(&rp("README")).unwrap(), PathState::Full);
        assert_eq!(d.state(&rp("src/b.txt")).unwrap(), PathState::Tombstone);
        assert_eq!(
            d.state(&rp("src/sub/deep.txt")).unwrap(),
            PathState::Tombstone
        );
        assert_eq!(d.state(&rp("moved/deep.txt")).unwrap(), PathState::Full);
        assert_eq!(d.state(&rp("nothing")).unwrap(), PathState::Missing);
        let st = d.stat(&rp("run.sh")).unwrap().unwrap();
        assert_eq!(st.kind, DiskKind::File { exec: Some(true) });
        assert_eq!(d.read(&rp("d1/x"), 100).unwrap(), b"x");
        assert!(d.read(&rp("d1/x"), 0).is_err());
        let listed: Vec<String> = d
            .list(&rp("d1"))
            .unwrap()
            .iter()
            .map(|e| String::from_utf8(e.name.as_bytes().to_vec()).unwrap())
            .collect();
        assert_eq!(listed, ["x"]);

        let ops = fx.journal.ops();
        for (op, p) in [
            (Op::Modified, "README"),
            (Op::Modified, "src/a.txt"),
            (Op::Modified, "run.sh"),
            (Op::Deleted, "src/b.txt"),
            (Op::DirRenamedFrom, "src/sub"),
            (Op::DirRenamedTo, "moved"),
            (Op::Created, "ln"),
            (Op::Created, "d1/x"),
            (Op::Deleted, "link"),
            (Op::Created, "link"),
        ] {
            assert!(
                ops.contains(&(op, p.to_owned())),
                "missing {op:?} {p}: {ops:?}"
            );
        }
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn redirected_names_are_symlinks_to_the_redirect_dir() {
        let Some(fx) = mount("") else { return };
        let nm = fx.path("node_modules");
        let target = std::fs::read_link(&nm).unwrap();
        assert!(target.starts_with(fx.tmp.path().join("redirect")));
        assert!(target.is_dir());
        // Writes through the link land in the redirect dir and are not journaled.
        std::fs::create_dir_all(nm.join("pkg")).unwrap();
        std::fs::write(nm.join("pkg/index.js"), b"x").unwrap();
        assert!(target.join("pkg/index.js").is_file());
        assert!(fx.journal.ops().is_empty());
        assert!(names(&fx.root()).contains(&"node_modules".to_owned()));
        // Creating the name itself is refused: it already exists.
        let err = std::fs::create_dir(&nm).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn switch_invalidation_is_visible_to_stat() {
        let Some(fx) = mount("") else { return };
        // The kernel caches these for an hour.
        assert_eq!(std::fs::metadata(fx.path("src/a.txt")).unwrap().len(), 6);
        assert_eq!(std::fs::read(fx.path("src/b.txt")).unwrap(), b"beta\n");
        assert_eq!(
            std::fs::read(fx.path("src/sub/deep.txt")).unwrap(),
            b"deep\n"
        );
        // A local edit that the switch overwrites (force).
        std::fs::write(fx.path("src/sub/deep.txt"), b"mine").unwrap();
        assert!(!fx.path("src/c.txt").exists());

        let write = |data: &[u8]| Change::Write {
            kind: Kind::File,
            file: FileId::of(data),
            size: data.len() as u64,
            git: GitOid::from_bytes([0; 20]),
            copy_from: None,
        };
        let plan = Plan {
            updates: vec![
                (rp("src/a.txt"), write(b"alpha two, longer\n")),
                (rp("src/sub/deep.txt"), write(b"deep two\n")),
                (rp("src/c.txt"), write(b"gamma\n")),
            ],
            deletes: vec![rp("src/b.txt")],
            ..Plan::default()
        };
        fx.core.set_root(fx.tree2);
        let out = fx
            .provider()
            .apply_switch(
                &plan,
                Switched {
                    epoch: 2,
                    time: EPOCH_TIME + 10,
                },
            )
            .unwrap();
        assert!(out.pending.is_empty());

        assert_eq!(std::fs::metadata(fx.path("src/a.txt")).unwrap().len(), 18);
        assert_eq!(
            std::fs::read(fx.path("src/a.txt")).unwrap(),
            b"alpha two, longer\n"
        );
        assert_eq!(
            std::fs::metadata(fx.path("src/b.txt")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read(fx.path("src/c.txt")).unwrap(), b"gamma\n");
        assert_eq!(names(&fx.path("src")), ["a.txt", "c.txt", "sub"]);
        assert_eq!(
            std::fs::read(fx.path("src/sub/deep.txt")).unwrap(),
            b"deep two\n"
        );
        assert_eq!(
            fx.provider()
                .disk_state()
                .state(&rp("src/sub/deep.txt"))
                .unwrap(),
            PathState::Virtual
        );
        // A plan with conflicts is refused.
        let bad = Plan {
            conflicts: vec![rp("src/a.txt")],
            ..Plan::default()
        };
        assert!(
            fx.provider()
                .apply_switch(&bad, Switched { epoch: 3, time: 0 })
                .is_err()
        );
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn the_watcher_notice_is_printed_when_a_process_holds_an_inotify_watch() {
        use rustix::fs::inotify::{CreateFlags, WatchFlags, add_watch, init};
        let Some(fx) = mount("") else { return };
        assert_eq!(fx.provider().switch_note(), None);
        let fd = init(CreateFlags::CLOEXEC).unwrap();
        add_watch(&fd, fx.root(), WatchFlags::MODIFY | WatchFlags::CREATE).unwrap();
        assert!(wait_for(|| fx.provider().switch_note().is_some()));
        assert_eq!(fx.provider().switch_note().as_deref(), Some(WATCH_NOTE));
        assert_eq!(
            WATCH_NOTE,
            "note: file watchers do not see this switch; restart watch processes or use polling"
        );
        drop(fd);
        assert!(wait_for(|| fx.provider().switch_note().is_none()));
    }

    #[test]
    #[ignore = "needs /dev/fuse"]
    fn unmount_twice_fails_the_second_time() {
        let Some(fx) = mount("") else { return };
        fx.provider().unmount().unwrap();
        assert!(fx.provider().unmount().is_err());
        assert!(!fx.path("README").exists());
    }
}
