//! The ProjFS switch (SPEC §10.2 item 8): `switch`, `reset --hard`, `restore`.
//!
//! The daemon passes a [`Request`]: the plan from `asc_wc::switch::plan` (updates, deletes and
//! conflicts, already checked against the dirty set by the switch rules of step 3), the FORCE
//! extras `D` (dirty paths in the command's pathspec), and the two roots. [`ProjfsProvider::switch`]
//! then runs steps 1, 2 and 4 to 6 of the algorithm; the caller records the new epoch first
//! (`asc_wc::times::record_switch`, whose [`Switched`] it passes in) and rewrites the index and
//! `HEAD` afterwards.
//!
//! * The intent (`from_root`, `to_root`, the work set `W`) is persisted before anything is
//!   touched, and the base tree moves to `to_root` before the first path is applied, so a path
//!   that becomes virtual again is served from the new tree. A crash at any point is repaired by
//!   [`ProjfsProvider::resume`], which re-runs the intent; every step is idempotent.
//! * Per path of `W`: SAFE/MERGE updates clean placeholders with `PrjUpdateFileIfNeeded`
//!   (`ALLOW_DIRTY_METADATA | ALLOW_READ_ONLY`, never `ALLOW_DIRTY_DATA`); FORCE deletes the
//!   placeholder, hydrated, dirty or full file with every `ALLOW_*` data flag, so the path becomes
//!   virtual and enumerates from the target tree; a tombstone is cleared with `ALLOW_TOMBSTONE`
//!   (FORCE, or the target has no such entry), and so is a tombstoned ancestor directory.
//!   Untracked full files are never touched.
//! * A path held open (sharing violation) is retried at 100, 300 and 900 ms, then stays in the
//!   intent as pending; [`ProjfsProvider::resume`] retries pending paths (the daemon calls it every
//!   5 s).
//! * Afterwards the negative path cache is cleared, and every path the switch adds whose parent is
//!   on disk is opened with no access, so ProjFS creates its placeholder (no hydration) and
//!   `ReadDirectoryChangesW` watchers see `FILE_ACTION_ADDED` (step 4a).
//!
//! Recovery applies the SAFE rules: it never destroys a file with local data, so a crashed FORCE
//! switch leaves such files alone and the command is simply run again.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

#[cfg(test)]
use asc_core::id::TreeId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
#[cfg(test)]
use asc_wc::intent;
use asc_wc::intent::Replayed;
use asc_wc::switch::{Mode, Plan};
use asc_wc::times::Switched;

use super::data::{CONTENT_FILE, CONTENT_TREE, content_id};
use super::enumerate::basic_info;
use super::ffi::{self, ID_LEN, PlaceholderInfo, VersionInfo, update};
use super::provider::{Inner, ProjfsProvider, lock};
use super::reconcile::{is_dirty, is_tombstone, rel_wide, state_bits};
use crate::core::{Node, Result, SwitchOutcome, VfsError};
use crate::win::block;
use crate::win::engine::{self, Apply, Out, Work, vfs};
pub use crate::win::engine::{MAX_THREADS, RETRY_DELAYS, Report, Request, Store};

const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// `ALLOW_DIRTY_METADATA | ALLOW_READ_ONLY`.
const SAFE_FLAGS: u32 = update::ALLOW_DIRTY_METADATA | update::ALLOW_READ_ONLY;
/// `SAFE_FLAGS | ALLOW_DIRTY_DATA`.
const FORCE_FLAGS: u32 = SAFE_FLAGS | update::ALLOW_DIRTY_DATA;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Anc {
    Absent,
    Present,
    Tomb,
}

/// The state of one switch run.
struct Run<'a> {
    inner: &'a Inner,
    ctx: usize,
    mode: Mode,
    time: i64,
    anc: Mutex<HashMap<RepoPath, Anc>>,
    added: Mutex<Vec<RepoPath>>,
}

fn is_sharing(hr: ffi::Hresult) -> bool {
    hr == ffi::ERROR_SHARING_VIOLATION
}

fn describe(hr: ffi::Hresult, cause: u32) -> String {
    format!("HRESULT {:#010x} (cause {cause})", hr as u32)
}

/// The placeholder a node projects as, at time `t`.
pub(crate) fn placeholder_info(workspace: &[u8; 16], node: &Node, t: i64) -> PlaceholderInfo {
    let mut info = PlaceholderInfo::zeroed();
    info.basic = basic_info(node, t);
    let mut provider_id = [0u8; ID_LEN];
    provider_id[..16].copy_from_slice(workspace);
    info.version = VersionInfo {
        provider_id,
        content_id: match node.kind {
            Kind::Dir | Kind::Gitlink => content_id(CONTENT_TREE, &node.id),
            _ => content_id(CONTENT_FILE, &node.id),
        },
    };
    info
}

fn is_tree(node: &Node) -> bool {
    matches!(node.kind, Kind::Dir | Kind::Gitlink)
}

impl<'a> Run<'a> {
    fn new(inner: &'a Inner, mode: Mode, time: i64) -> Self {
        Self {
            inner,
            ctx: inner.ctx.load(Ordering::SeqCst),
            mode,
            time,
            anc: Mutex::new(HashMap::new()),
            added: Mutex::new(Vec::new()),
        }
    }

    fn force(&self) -> bool {
        self.mode == Mode::Force
    }

    fn flags(&self) -> u32 {
        if self.force() {
            FORCE_FLAGS
        } else {
            SAFE_FLAGS
        }
    }

    fn target(&self, path: &RepoPath) -> io::Result<Option<Node>> {
        block(&self.inner.cfg.rt, self.inner.cfg.core.resolve(path)).map_err(vfs)
    }

    fn finish((hr, cause): (ffi::Hresult, u32)) -> Out {
        use ffi::failure as f;
        if hr == ffi::S_OK || hr == ffi::ERROR_FILE_NOT_FOUND || hr == ffi::ERROR_PATH_NOT_FOUND {
            Out::Done
        } else if cause & (f::DIRTY_DATA | f::DIRTY_METADATA | f::TOMBSTONE | f::READ_ONLY) != 0
            || hr == ffi::ERROR_DIR_NOT_EMPTY
        {
            Out::DirtySkipped
        } else {
            Out::InUse(describe(hr, cause))
        }
    }

    fn delete(&self, rel: &[u16], flags: u32) -> (ffi::Hresult, u32) {
        self.inner.api.delete(self.ctx, rel, flags)
    }

    fn note_added(&self, path: &RepoPath) {
        lock(&self.added).push(path.clone());
    }

    /// A path with nothing on disk may sit below a tombstoned directory. The nearest ancestor that
    /// exists decides: a tombstone is cleared when FORCE says so or the target has no such
    /// directory, so the path then enumerates from the new tree.
    fn clear_ancestor_tombstone(&self, path: &RepoPath) -> io::Result<()> {
        let mut cur = path.parent();
        while let Some(a) = cur {
            let known = lock(&self.anc).get(&a).copied();
            let st = match known {
                Some(s) => s,
                None => {
                    let s = match state_bits(self.inner, &a)? {
                        None => Anc::Absent,
                        Some(b) if is_tombstone(b) => Anc::Tomb,
                        Some(_) => Anc::Present,
                    };
                    lock(&self.anc).insert(a.clone(), s);
                    s
                }
            };
            match st {
                Anc::Present => return Ok(()),
                Anc::Absent => cur = a.parent(),
                Anc::Tomb => {
                    if self.force() || self.target(&a)?.is_none() {
                        if let Some(rel) = rel_wide(&a) {
                            let _ = self.delete(&rel, update::ALLOW_TOMBSTONE);
                        }
                        lock(&self.anc).insert(a, Anc::Absent);
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Brings one path to the target version.
    fn apply_path(&self, path: &RepoPath, work: Work) -> io::Result<Out> {
        let inner = self.inner;
        let target = self.target(path)?;
        let Some(rel) = rel_wide(path) else {
            return Ok(Out::Done);
        };
        let Some(bits) = state_bits(inner, path)? else {
            // Virtual, absent, or outside the work tree. Nothing on disk to rewrite.
            self.clear_ancestor_tombstone(path)?;
            if target.is_some() {
                self.note_added(path);
            }
            return Ok(Out::Done);
        };
        if is_tombstone(bits) {
            if self.force() || target.is_none() {
                let out = Self::finish(self.delete(&rel, update::ALLOW_TOMBSTONE));
                if target.is_some() && matches!(out, Out::Done) {
                    self.note_added(path);
                }
                return Ok(out);
            }
            // SAFE and MERGE: a deletion the target changes is a dirty path (step 3).
            return Ok(Out::DirtySkipped);
        }
        let abs = inner
            .safe
            .resolve(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let on_disk_dir = std::fs::symlink_metadata(&abs).is_ok_and(|m| {
            m.is_dir() && m.file_attributes() & ffi::FILE_ATTRIBUTE_REPARSE_POINT == 0
        });
        let Some(node) = target else {
            // The target has no such entry. Only a tracked path the plan deletes is removed; a
            // dirty path without a target entry is untracked.
            if work != Work::Delete {
                return Ok(Out::Done);
            }
            return Ok(self.remove(&rel, &abs, bits));
        };
        if is_tree(&node) {
            if on_disk_dir {
                let info = placeholder_info(&inner.cfg.workspace, &node, self.time);
                return Ok(Self::finish(
                    inner.api.update(self.ctx, &rel, &info, SAFE_FLAGS),
                ));
            }
            // A file on disk where the target has a directory.
            return Ok(self.remove(&rel, &abs, bits));
        }
        if on_disk_dir {
            // A directory on disk where the target has a file: its children were deleted first.
            return Ok(Self::finish(self.delete(&rel, self.flags())));
        }
        if self.force() {
            let out = self.replace(&rel, &abs);
            if matches!(out, Out::Done) {
                self.note_added(path);
            }
            return Ok(out);
        }
        let info = placeholder_info(&inner.cfg.workspace, &node, self.time);
        let out = Self::finish(inner.api.update(self.ctx, &rel, &info, SAFE_FLAGS));
        if matches!(out, Out::DirtySkipped) && !is_dirty(bits) {
            // A clean placeholder refused for another reason (read-only data): in use.
            return Ok(Out::InUse("update refused".into()));
        }
        Ok(out)
    }

    /// Deletes a tracked path the target does not have.
    fn remove(&self, rel: &[u16], abs: &std::path::Path, _bits: u32) -> Out {
        let hr = self.delete(rel, self.flags());
        let out = Self::finish(hr);
        if self.force() && matches!(out, Out::InUse(_)) && !is_sharing(hr.0) {
            return self.delete_by_hand(rel, abs, out);
        }
        out
    }

    /// FORCE: the path becomes virtual again. A file the placeholder API will not delete (a full
    /// file) is removed by hand and its tombstone cleared.
    fn replace(&self, rel: &[u16], abs: &std::path::Path) -> Out {
        let hr = self.delete(rel, FORCE_FLAGS);
        let out = Self::finish(hr);
        if matches!(out, Out::InUse(_)) && !is_sharing(hr.0) {
            return self.delete_by_hand(rel, abs, out);
        }
        out
    }

    fn delete_by_hand(&self, rel: &[u16], abs: &std::path::Path, was: Out) -> Out {
        if let Ok(m) = std::fs::symlink_metadata(abs) {
            let mut perms = m.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(abs, perms);
            }
        }
        match std::fs::remove_file(abs) {
            Ok(()) => {
                // Removing a file that came from a placeholder leaves a tombstone.
                let _ = self.delete(rel, update::ALLOW_TOMBSTONE);
                Out::Done
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Out::Done,
            Err(e) if e.raw_os_error() == Some(32) => Out::InUse(format!("in use: {e}")),
            Err(_) => was,
        }
    }

    /// After deletions: directories the target no longer has are removed bottom-up when empty.
    fn drop_empty_dirs(&self, deleted: &[RepoPath]) {
        let mut dirs = BTreeSet::new();
        for p in deleted {
            let mut cur = p.parent();
            while let Some(d) = cur {
                cur = d.parent();
                dirs.insert(d);
            }
        }
        let mut dirs: Vec<RepoPath> = dirs.into_iter().collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            if self.target(&d).ok().flatten().is_some() {
                continue;
            }
            let Ok(Some(bits)) = state_bits(self.inner, &d) else {
                continue;
            };
            if is_tombstone(bits) {
                continue;
            }
            if let Some(rel) = rel_wide(&d) {
                // Non-empty (untracked content) is expected and fine.
                let _ = self.delete(&rel, self.flags());
            }
            self.inner.safe.forget(&d);
        }
    }

    /// Step 4a: opens each added path with no access so ProjFS creates the placeholder and
    /// watchers see it.
    fn make_visible(&self) {
        let mut added = std::mem::take(&mut *lock(&self.added));
        added.sort();
        added.dedup();
        let mut parents: HashMap<RepoPath, bool> = HashMap::new();
        for p in added {
            let on_disk = match p.parent() {
                None => true,
                Some(par) => *parents.entry(par.clone()).or_insert_with(
                    || matches!(state_bits(self.inner, &par), Ok(Some(b)) if !is_tombstone(b)),
                ),
            };
            if !on_disk {
                continue;
            }
            let Some(abs) = self.inner.safe.resolve(&p) else {
                continue;
            };
            let _ = std::fs::OpenOptions::new()
                .access_mode(0)
                .share_mode(7)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(abs);
        }
    }
}

impl Apply for Run<'_> {
    fn apply(&self, path: &RepoPath, work: Work) -> io::Result<Out> {
        self.apply_path(path, work)
    }

    fn remove_empty_dirs(&self, deleted: &[RepoPath]) {
        self.drop_empty_dirs(deleted);
    }

    /// Placeholders created by the lookups must not be hidden by stale negative entries; then
    /// step 4a.
    fn finish(&self) {
        let _ = self.inner.api.clear_negative(self.ctx);
        self.make_visible();
    }
}

impl ProjfsProvider {
    /// Gives the provider the database that holds the switch intent. Without one a switch still
    /// works but is not crash safe.
    pub fn set_store(&self, store: Store) {
        *lock(&self.store) = Some(store);
    }

    fn intent_store(&self) -> Option<Store> {
        lock(&self.store).clone()
    }

    /// Runs the switch of `req` (SPEC §10.2 item 8). `times.time` is the time of every placeholder
    /// the switch rewrites.
    pub fn switch(&self, req: &Request<'_>, times: Switched) -> Result<Report> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let _live = inner.begin().ok_or(VfsError::NotMounted)?;
        let store = self.intent_store();
        engine::switch(store.as_ref(), &inner.cfg.core, &inner.cfg.rt, req, || {
            Run::new(&inner, req.mode, times.time)
        })
    }

    /// Re-runs the stored intent: after a restart (the interrupted switch completes) and every
    /// 5 s for paths that were held open. `time` is the workspace's current epoch time. Applies
    /// the SAFE rules. `None` when there is no intent.
    pub fn resume(&self, time: i64) -> Result<Option<Replayed>> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let _live = inner.begin().ok_or(VfsError::NotMounted)?;
        let store = self.intent_store();
        engine::resume(store.as_ref(), &inner.cfg.core, &inner.cfg.rt, |_| {
            Run::new(&inner, Mode::Safe, time)
        })
    }

    /// The `Provider::apply_switch` entry: SAFE rules, no extras, the base tree unchanged.
    pub(crate) fn apply_plan(&self, plan: &Plan, times: Switched) -> Result<SwitchOutcome> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let _live = inner.begin().ok_or(VfsError::NotMounted)?;
        engine::apply_plan(&Run::new(&inner, Mode::Safe, times.time), plan)
    }
}

/// A real ProjFS instance over a tree built in memory, for the tests of this module tree.
#[cfg(test)]
pub(crate) mod lab {
    use std::ops::Deref;
    use std::sync::Arc;

    use super::*;
    use crate::core::{CoreFs, Provider};
    use crate::win::testkit::WORKSPACE;
    pub(crate) use crate::win::testkit::{Files, Holder, Kit, p};

    /// A mounted ProjFS root over the kit's tree; the kit is reached through `Deref`.
    pub(crate) struct Lab {
        kit: Kit,
        pub(crate) core: Arc<CoreFs>,
        pub(crate) provider: ProjfsProvider,
        guid: [u8; 16],
    }

    impl Deref for Lab {
        type Target = Kit;

        fn deref(&self) -> &Kit {
            &self.kit
        }
    }

    impl Lab {
        /// Mounts a workspace with `files`, or `None` (saying why) where ProjFS is unavailable.
        pub(crate) fn new(files: Files<'_>) -> Option<Self> {
            Self::build(WORKSPACE, files, &[], "")
        }

        pub(crate) fn with_guid(guid: [u8; 16], files: Files<'_>) -> Option<Self> {
            Self::build(guid, files, &[], "")
        }

        /// As `new`, with the objects of `missing` left out of the store (an offline fetch).
        pub(crate) fn offline(files: Files<'_>, missing: &[&str]) -> Option<Self> {
            Self::build(WORKSPACE, files, missing, "")
        }

        fn build(guid: [u8; 16], files: Files<'_>, missing: &[&str], deny: &str) -> Option<Self> {
            let kit = Kit::new(deny);
            let root = kit.tree_missing(files, missing);
            let core = kit.core(root);
            let provider = match ProjfsProvider::new(kit.config(guid, core.clone())) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("skipping: {e}");
                    return None;
                }
            };
            let lab = Self {
                kit,
                core,
                provider,
                guid,
            };
            if let Err(e) = lab.provider.mount(&lab.root()) {
                eprintln!("skipping: {e}");
                return None;
            }
            lab.provider.set_store(lab.store_handle());
            Some(lab)
        }

        /// The daemon restarting: the provider stops and a new one starts over the same root, with
        /// the base tree `base`.
        pub(crate) fn restart(&mut self, base: TreeId) {
            self.provider.unmount().unwrap();
            self.core = self.kit.core(base);
            self.provider =
                ProjfsProvider::new(self.kit.config(self.guid, self.core.clone())).unwrap();
            self.provider.mount(&self.root()).unwrap();
            self.provider.set_store(self.store_handle());
        }

        /// A switch from `old` to `new` at time `t`, stamping every changed path as the daemon does.
        pub(crate) fn run(
            &self,
            old: Files<'_>,
            new: Files<'_>,
            mode: Mode,
            extra: &[&str],
            t: i64,
        ) -> Result<Report> {
            let (plan, extra) = self.stamp(old, new, extra, t);
            let (from, to) = (self.tree(old), self.tree(new));
            self.provider.switch(
                &Request {
                    mode,
                    plan: &plan,
                    extra: &extra,
                    from_root: from,
                    to_root: to,
                    to_head: "refs/heads/master",
                },
                Switched { epoch: 2, time: t },
            )
        }

        /// Reads every file so its placeholder is hydrated.
        pub(crate) fn hydrate(&self, files: Files<'_>) {
            for (path, data) in files {
                assert_eq!(&std::fs::read(self.at(path)).unwrap(), data, "{path}");
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::lab::*;
    use super::*;
    use crate::win::testkit::{EPOCH_TIME, mtime};

    pub(crate) const T: i64 = EPOCH_TIME + 100;

    pub(crate) const V1: &[(&str, &[u8])] = &[
        ("a.txt", b"alpha"),
        ("b.txt", b"bravo"),
        ("c.txt", b"charlie"),
        ("d.txt", b"delta"),
        ("e.txt", b"echo"),
        ("sub/x.txt", b"xray"),
        ("src/main.rs", b"fn main() {}"),
    ];

    pub(crate) const V2: &[(&str, &[u8])] = &[
        ("a.txt", b"alpha two"),
        ("c.txt", b"charlie"),
        ("d.txt", b"delta"),
        ("e.txt", b"echo"),
        ("src/main.rs", b"fn main() {}"),
        ("new/y.txt", b"why"),
        ("z.txt", b"zulu"),
    ];

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn stored_intent(lab: &Lab) -> Option<asc_wc::intent::SwitchIntent> {
        lab.rt.block_on(intent::load(&lab.db, lab.ws)).unwrap()
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn reset_hard_same_tree() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        // Modified, read-only, tombstoned, overwritten (full), and an untracked file.
        std::fs::write(lab.at("a.txt"), b"changed").unwrap();
        let mut perm = std::fs::metadata(lab.at("b.txt")).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(lab.at("b.txt"), perm).unwrap();
        std::fs::remove_file(lab.at("c.txt")).unwrap();
        {
            let mut f = std::fs::File::create(lab.at("d.txt")).unwrap();
            f.write_all(b"scribble").unwrap();
        }
        std::fs::write(lab.at("mine.txt"), b"untracked").unwrap();

        let dirty = ["a.txt", "b.txt", "c.txt", "d.txt", "mine.txt"];
        let rep = lab.run(V1, V1, Mode::Force, &dirty, T).unwrap();
        assert!(rep.pending.is_empty(), "{rep:?}");
        assert!(rep.errors.is_empty(), "{rep:?}");
        for (path, data) in V1 {
            assert_eq!(&std::fs::read(lab.at(path)).unwrap(), data, "{path}");
        }
        assert!(
            !std::fs::metadata(lab.at("b.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert_eq!(std::fs::read(lab.at("mine.txt")).unwrap(), b"untracked");
        // Restored files carry the switch time; untouched ones keep the clone time.
        assert_eq!(mtime(&lab.at("a.txt")), T);
        assert_eq!(mtime(&lab.at("c.txt")), T);
        assert_eq!(mtime(&lab.at("e.txt")), EPOCH_TIME);
        assert!(stored_intent(&lab).is_none(), "the intent is cleared");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn restore_deleted_file() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        std::fs::remove_file(lab.at("c.txt")).unwrap();
        assert!(!lab.at("c.txt").exists());
        let rep = lab.run(V1, V1, Mode::Force, &["c.txt"], T).unwrap();
        assert!(rep.pending.is_empty(), "{rep:?}");
        assert_eq!(std::fs::read(lab.at("c.txt")).unwrap(), b"charlie");
        assert_eq!(mtime(&lab.at("c.txt")), T);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn restore_deleted_directory() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        std::fs::remove_dir_all(lab.at("sub")).unwrap();
        assert!(!lab.at("sub").exists());
        let rep = lab.run(V1, V1, Mode::Force, &["sub/x.txt"], T).unwrap();
        assert!(rep.pending.is_empty(), "{rep:?}");
        assert_eq!(std::fs::read(lab.at("sub/x.txt")).unwrap(), b"xray");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn safe_switch_updates_adds_and_deletes() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let rep = lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert!(
            rep.pending.is_empty() && rep.dirty_skipped.is_empty(),
            "{rep:?}"
        );
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"alpha two");
        assert!(!lab.at("b.txt").exists());
        assert!(
            !lab.at("sub").exists(),
            "a directory the target lacks is removed"
        );
        assert_eq!(std::fs::read(lab.at("new/y.txt")).unwrap(), b"why");
        assert_eq!(std::fs::read(lab.at("z.txt")).unwrap(), b"zulu");
        assert_eq!(
            names(&lab.root()),
            ["a.txt", "c.txt", "d.txt", "e.txt", "new", "src", "z.txt"]
        );
        assert_eq!(mtime(&lab.at("a.txt")), T);
        assert_eq!(mtime(&lab.at("c.txt")), EPOCH_TIME);
        assert!(stored_intent(&lab).is_none());
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn safe_mode_leaves_a_modified_file_alone() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        std::fs::write(lab.at("a.txt"), b"mine").unwrap();
        // The caller's step 3 would have refused this; the provider still never destroys data.
        let rep = lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert_eq!(rep.dirty_skipped, [p("a.txt")], "{rep:?}");
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(lab.at("z.txt")).unwrap(), b"zulu");
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn a_plan_with_conflicts_is_refused() {
        let Some(lab) = Lab::new(V1) else { return };
        let mut plan = lab.plan(V1, V2);
        plan.conflicts.push(p("a.txt"));
        let err = lab
            .provider
            .apply_plan(&plan, Switched { epoch: 2, time: T })
            .unwrap_err();
        assert!(matches!(err, VfsError::Conflicts));
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn watcher_sees_added_file() {
        let Some(lab) = Lab::new(V1) else { return };
        // `src` is on disk once it has been opened.
        assert_eq!(names(&lab.at("src")), ["main.rs"]);
        let w = crate::projfs::ffi::watch::Watcher::new(&lab.at("src")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut v2 = V1.to_vec();
        v2.push(("src/lib.rs", b"pub fn f() {}"));
        let rep = lab.run(V1, &v2, Mode::Safe, &[], T).unwrap();
        assert!(rep.pending.is_empty(), "{rep:?}");
        assert!(
            w.wait_for(
                crate::projfs::ffi::watch::ADDED,
                "lib.rs",
                std::time::Duration::from_secs(10)
            ),
            "no FILE_ACTION_ADDED for src/lib.rs"
        );
        // Creating the placeholder did not hydrate anything.
        assert_eq!(lab.core.stats().snapshot().total_hydrations, 0);
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn held_open_file_is_pending_then_cleared() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let holder = Holder::new(&lab.at("a.txt"));
        let rep = lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert_eq!(rep.pending, [p("a.txt")], "{rep:?}");
        // Everything else was applied, and only the held path stays in the intent.
        assert_eq!(std::fs::read(lab.at("z.txt")).unwrap(), b"zulu");
        assert!(!lab.at("b.txt").exists());
        let intent = stored_intent(&lab).expect("the intent stays while a path is pending");
        assert_eq!(intent.pending, [b"a.txt".to_vec()]);
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"alpha");

        holder.release();
        let r = lab.provider.resume(T).unwrap().unwrap();
        assert!(r.complete(), "{r:?}");
        assert_eq!(std::fs::read(lab.at("a.txt")).unwrap(), b"alpha two");
        assert!(stored_intent(&lab).is_none());
    }
}
