//! The Cloud Files switch (SPEC §10.2b, the rules of §10.2 item 8): `switch`, `reset --hard`,
//! `restore`, through the shared engine ([`crate::win::engine`]).
//!
//! Cloud Files has no "update if clean" call that checks the data, so the rules are applied from
//! each path's probe:
//!
//! * A clean file (a placeholder still in sync) is rewritten in place: its identity and metadata
//!   move to the target and its data is dropped (`DEHYDRATE | MARK_IN_SYNC | VERIFY_IN_SYNC`), so
//!   the next read hydrates the new version. A write that lands between the probe and the update
//!   fails the in-sync check (`ERROR_CLOUD_FILE_NOT_IN_SYNC`) and the path counts as dirty.
//! * A dirty file (not a placeholder, out of sync, or a link the user made) is left alone in SAFE
//!   and MERGE; FORCE deletes it and puts a placeholder of the target in its place.
//! * A path with nothing on disk is created as a placeholder when its parent directory is listed.
//!   Below a directory that was never listed nothing is done: the directory's identity moves to
//!   the target tree, so its listing on demand is the new tree. A tracked path the user deleted
//!   (the old tree has it, the listed parent does not) is the tombstone of ProjFS: SAFE leaves it
//!   deleted, FORCE restores it.
//! * Directories the target adds are created as unlisted placeholders at the top-most missing
//!   level, so nothing below them is written until it is read.
//!
//! The switch's own deletions and renames pass the delete and rename callbacks (they come from
//! this process), so locked paths still move with a switch.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use asc_core::id::TreeId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_wc::intent::Replayed;
use asc_wc::switch::{Mode, Plan};
use asc_wc::times::Switched;
use windows::Win32::Storage::CloudFilters::{
    CF_UPDATE_FLAG_DEHYDRATE, CF_UPDATE_FLAG_MARK_IN_SYNC, CF_UPDATE_FLAG_VERIFY_IN_SYNC,
    CF_UPDATE_FLAGS,
};

use super::Ident;
use super::ffi::{self, Probe};
use super::populate::placeholder_of;
use super::provider::{CloudFilesProvider, Inner, lock, vfs_io};
use crate::core::{Node, Result, SwitchOutcome, VfsError};
use crate::win::block;
use crate::win::engine::{self, Apply, Out, Work};
pub use crate::win::engine::{MAX_THREADS, RETRY_DELAYS, Report, Request, Store};

/// `ERROR_CLOUD_FILE_NOT_IN_SYNC`: the file changed after it was probed.
const ERROR_NOT_IN_SYNC: i32 = 377;
/// `ERROR_DIR_NOT_EMPTY`.
const ERROR_DIR_NOT_EMPTY: i32 = 145;

/// What an ancestor directory lets the switch do below it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Anc {
    /// Listed: its children are on disk and each path is brought to the target.
    Open,
    /// Not listed (or just created): it lists the target tree on demand, so nothing below it is
    /// written.
    Closed,
}

fn is_tree(node: &Node) -> bool {
    matches!(node.kind, Kind::Dir | Kind::Gitlink)
}

/// A file with data of its own: not a placeholder, out of sync, or a link the user made.
fn is_dirty_file(p: &Probe) -> bool {
    p.is_foreign_reparse() || !p.is_placeholder() || !p.in_sync
}

/// One directory of the old tree: lower-case child name to the subtree id of a directory child.
type OldDir = std::sync::Arc<HashMap<Vec<u8>, Option<TreeId>>>;

fn lower(name: &[u8]) -> Vec<u8> {
    name.to_ascii_lowercase()
}

/// The state of one switch run.
struct Run<'a> {
    inner: &'a Inner,
    mode: Mode,
    time: i64,
    /// The tree the work tree was at before the switch, when known: what decides whether a
    /// missing path was deleted by the user.
    from: Option<TreeId>,
    /// Ancestors seen so far. Held across one `prepare`, so an ancestor is examined (and its
    /// placeholder made) once.
    anc: Mutex<HashMap<Option<RepoPath>, Anc>>,
    /// The lower-case child names of directories of `from`, by path; `None` where `from` has no
    /// such directory.
    old: Mutex<HashMap<Option<RepoPath>, Option<OldDir>>>,
}

impl<'a> Run<'a> {
    fn new(inner: &'a Inner, mode: Mode, time: i64, from: Option<TreeId>) -> Self {
        Self {
            inner,
            mode,
            time,
            from,
            anc: Mutex::new(HashMap::new()),
            old: Mutex::new(HashMap::new()),
        }
    }

    fn force(&self) -> bool {
        self.mode == Mode::Force
    }

    fn target(&self, path: &RepoPath) -> io::Result<Option<Node>> {
        self.inner.resolve(path)
    }

    /// The children of `dir` in the old tree, by lower-case name, with the tree id of each
    /// subdirectory; `None` where the old tree has no such directory (or is unknown).
    fn old_names(&self, dir: Option<&RepoPath>) -> io::Result<Option<OldDir>> {
        let Some(from) = self.from else {
            return Ok(None);
        };
        let key = dir.cloned();
        if let Some(v) = lock(&self.old).get(&key) {
            return Ok(v.clone());
        }
        let tree = match dir {
            None => Some(from),
            Some(d) => self
                .old_names(d.parent().as_ref())?
                .and_then(|m| m.get(&lower(d.file_name().as_bytes())).copied().flatten()),
        };
        let names = match tree {
            None => None,
            Some(t) => {
                let listing = block(
                    &self.inner.cfg.rt,
                    self.inner.cfg.core.read_tree_at(t, dir, false),
                )
                .map_err(vfs_io)?;
                Some(std::sync::Arc::new(
                    listing
                        .entries
                        .iter()
                        .map(|n| {
                            let sub =
                                (is_tree(n) && n.id != [0; 32]).then(|| TreeId::from_bytes(n.id));
                            (lower(n.path.file_name().as_bytes()), sub)
                        })
                        .collect(),
                ))
            }
        };
        lock(&self.old).insert(key, names.clone());
        Ok(names)
    }

    /// Whether the missing `path` is a deletion the user made: the old tree had it. Without the
    /// old tree, what the watcher heard decides.
    fn user_deleted(&self, path: &RepoPath) -> io::Result<bool> {
        if self.from.is_none() {
            return Ok(lock(&self.inner.deleted).contains(path));
        }
        Ok(self
            .old_names(path.parent().as_ref())?
            .is_some_and(|n| n.contains_key(&lower(path.file_name().as_bytes()))))
    }

    /// Creates the placeholder of `node` in its parent directory.
    fn create(&self, node: &Node) -> io::Result<()> {
        let Some(parent) = self.inner.abs(node.path.parent().as_ref()) else {
            return Ok(());
        };
        let Some(ph) = placeholder_of(node, self.time) else {
            return Ok(());
        };
        ffi::create_placeholders(&parent, &[ph]).map(|_| ())
    }

    /// Moves the identity of the directory placeholder at `abs` to `node`'s tree.
    fn retarget_dir(abs: &Path, p: &Probe, node: &Node) -> io::Result<()> {
        let want = Ident::of(node);
        if p.ident() == Some(want) {
            return Ok(());
        }
        let h = ffi::open(
            abs,
            windows::Win32::Storage::FileSystem::FILE_WRITE_DATA.0
                | windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES.0
                | windows::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES.0,
            ffi::SHARE_ALL,
        )?;
        ffi::update(&h, Some(&want.encode()), None, CF_UPDATE_FLAGS(0))
    }

    /// What the directory `dir` (whose parent is listed) lets the switch do below it. The second
    /// value says whether the answer may be remembered.
    fn examine(&self, dir: Option<&RepoPath>) -> io::Result<(Anc, bool)> {
        let Some(abs) = self.inner.abs(dir) else {
            // A link in an ancestor: nothing below is ours.
            return Ok((Anc::Closed, true));
        };
        let Some(d) = dir else {
            let p = ffi::probe(&abs)?;
            let st = if p.is_unpopulated() {
                Anc::Closed
            } else {
                Anc::Open
            };
            return Ok((st, true));
        };
        let target = self.target(d)?.filter(is_tree);
        match ffi::probe_opt(&abs)? {
            None => {
                let Some(node) = target else {
                    return Ok((Anc::Closed, false));
                };
                if !self.force() && self.user_deleted(d)? {
                    // A directory the user deleted stays deleted in SAFE and MERGE.
                    return Ok((Anc::Closed, true));
                }
                self.create(&node)?;
                Ok((Anc::Closed, true))
            }
            Some(p) if !p.is_dir() || p.is_foreign_reparse() => {
                // A file or link where a directory is expected: the path below waits for the file
                // to go (deletions run first).
                Ok((Anc::Closed, false))
            }
            Some(p) if p.is_placeholder() => {
                if let Some(node) = &target {
                    Self::retarget_dir(&abs, &p, node)?;
                }
                Ok((
                    if p.is_unpopulated() {
                        Anc::Closed
                    } else {
                        Anc::Open
                    },
                    true,
                ))
            }
            Some(_) => Ok((Anc::Open, true)),
        }
    }

    /// Prepares every ancestor of `path`, top-down; `false` when one is not listed, so nothing is
    /// to be written at `path`.
    fn prepare(&self, path: &RepoPath) -> io::Result<bool> {
        let mut memo = lock(&self.anc);
        let mut chain: Vec<Option<RepoPath>> = vec![None];
        let mut ups = Vec::new();
        let mut cur = path.parent();
        while let Some(a) = cur {
            cur = a.parent();
            ups.push(Some(a));
        }
        chain.extend(ups.into_iter().rev());
        for dir in chain {
            let st = match memo.get(&dir) {
                Some(s) => *s,
                None => {
                    let (s, keep) = self.examine(dir.as_ref())?;
                    if keep {
                        memo.insert(dir, s);
                    }
                    s
                }
            };
            if st == Anc::Closed {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Brings one path to the target version.
    fn apply_path(&self, path: &RepoPath, work: Work) -> io::Result<Out> {
        if !self.prepare(path)? {
            return Ok(Out::Done);
        }
        let Some(abs) = self.inner.abs(Some(path)) else {
            return Ok(Out::Done);
        };
        let target = self.target(path)?;
        let Some(p) = ffi::probe_opt(&abs)? else {
            let Some(node) = target else {
                return Ok(Out::Done);
            };
            if !self.force() && self.user_deleted(path)? {
                // The tombstone rule: a deletion the target changes is a dirty path.
                return Ok(Out::DirtySkipped);
            }
            return self.created(&node);
        };
        let Some(node) = target else {
            // Only a tracked path the plan deletes is removed; a path the target lacks is
            // untracked otherwise.
            if work != Work::Delete {
                return Ok(Out::Done);
            }
            return Ok(self.remove(&abs, &p));
        };
        if is_tree(&node) {
            if p.is_dir() && !p.is_foreign_reparse() {
                if p.is_placeholder() {
                    return Ok(match Self::retarget_dir(&abs, &p, &node) {
                        Ok(()) => Out::Done,
                        Err(e) => busy(&e),
                    });
                }
                return Ok(Out::Done);
            }
            // A file where the target has a directory.
            return self.then_create(self.remove(&abs, &p), &node);
        }
        if p.is_dir() && !p.is_foreign_reparse() {
            // A directory where the target has a file: its children were deleted first.
            let out = match std::fs::remove_dir(&abs) {
                Ok(()) => Out::Done,
                Err(e) if ffi::is_not_found(&e) => Out::Done,
                Err(e) if e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY) => Out::DirtySkipped,
                Err(e) => busy(&e),
            };
            self.inner.safe.forget(path);
            return self.then_create(out, &node);
        }
        if is_dirty_file(&p) {
            if !self.force() {
                return Ok(Out::DirtySkipped);
            }
            return self.then_create(self.delete_file(&abs, &p), &node);
        }
        Ok(self.rewrite(&abs, &p, &node))
    }

    fn created(&self, node: &Node) -> io::Result<Out> {
        match self.create(node) {
            Ok(()) => Ok(Out::Done),
            Err(e) if ffi::is_in_use(&e) => Ok(Out::InUse(e.to_string())),
            Err(e) => Err(e),
        }
    }

    fn then_create(&self, out: Out, node: &Node) -> io::Result<Out> {
        match out {
            Out::Done => self.created(node),
            other => Ok(other),
        }
    }

    /// A clean placeholder moves to `node`: new identity and metadata, data dropped unless the
    /// content is the same.
    fn rewrite(&self, abs: &Path, p: &Probe, node: &Node) -> Out {
        let want = Ident::of(node);
        let same = p.ident() == Some(want);
        let was_ro = match ffi::clear_read_only(abs, p.attrs) {
            Ok(v) => v,
            Err(e) => return busy(&e),
        };
        let r = ffi::open_exclusive(abs).and_then(|h| {
            let meta = ffi::metadata(false, node.size, self.time, node.read_only);
            let mut flags = CF_UPDATE_FLAG_MARK_IN_SYNC.0 | CF_UPDATE_FLAG_VERIFY_IN_SYNC.0;
            if !same {
                flags |= CF_UPDATE_FLAG_DEHYDRATE.0;
            }
            ffi::update(
                &h,
                Some(&want.encode()),
                Some(&meta),
                CF_UPDATE_FLAGS(flags),
            )
        });
        match r {
            Ok(()) => Out::Done,
            Err(e) => {
                if was_ro {
                    let _ = ffi::set_attributes(abs, p.attrs);
                }
                if e.raw_os_error() == Some(ERROR_NOT_IN_SYNC) {
                    Out::DirtySkipped
                } else {
                    busy(&e)
                }
            }
        }
    }

    /// Deletes a file or link (a link to a directory is removed as a directory entry).
    fn delete_file(&self, abs: &Path, p: &Probe) -> Out {
        if let Err(e) = ffi::clear_read_only(abs, p.attrs) {
            return busy(&e);
        }
        let r = if p.is_dir() {
            std::fs::remove_dir(abs)
        } else {
            std::fs::remove_file(abs)
        };
        match r {
            Ok(()) => Out::Done,
            Err(e) if ffi::is_not_found(&e) => Out::Done,
            Err(e) => busy(&e),
        }
    }

    /// Deletes a tracked path the target does not have.
    fn remove(&self, abs: &Path, p: &Probe) -> Out {
        if p.is_dir() && !p.is_foreign_reparse() {
            return match std::fs::remove_dir(abs) {
                Ok(()) => Out::Done,
                Err(e) if ffi::is_not_found(&e) => Out::Done,
                Err(e) if e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY) => Out::DirtySkipped,
                Err(e) => busy(&e),
            };
        }
        if is_dirty_file(p) && !self.force() {
            return Out::DirtySkipped;
        }
        self.delete_file(abs, p)
    }

    /// After deletions: directories the target no longer has are removed bottom-up when empty.
    fn drop_empty_dirs(&self, deleted: &[RepoPath]) {
        let mut dirs = std::collections::BTreeSet::new();
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
            let Some(abs) = self.inner.abs(Some(&d)) else {
                continue;
            };
            let Ok(Some(p)) = ffi::probe_opt(&abs) else {
                continue;
            };
            if !p.is_dir() || p.is_foreign_reparse() {
                continue;
            }
            // Non-empty (untracked content) is expected and fine.
            if std::fs::remove_dir(&abs).is_ok() {
                lock(&self.anc).remove(&Some(d.clone()));
            }
            self.inner.safe.forget(&d);
        }
    }
}

/// A failure that leaves the path pending: held open, or another error the retry may clear.
fn busy(e: &io::Error) -> Out {
    if ffi::is_in_use(e) {
        Out::InUse(format!("in use: {e}"))
    } else {
        Out::InUse(e.to_string())
    }
}

impl Apply for Run<'_> {
    fn apply(&self, path: &RepoPath, work: Work) -> io::Result<Out> {
        self.apply_path(path, work)
    }

    fn remove_empty_dirs(&self, deleted: &[RepoPath]) {
        self.drop_empty_dirs(deleted);
    }
}

impl CloudFilesProvider {
    /// Gives the provider the database that holds the switch intent. Without one a switch still
    /// works but is not crash safe.
    pub fn set_store(&self, store: Store) {
        *lock(&self.store) = Some(store);
    }

    fn intent_store(&self) -> Option<Store> {
        lock(&self.store).clone()
    }

    /// Runs the switch of `req` (SPEC §10.2 item 8). `times.time` is the time of every placeholder
    /// the switch writes.
    pub fn switch(&self, req: &Request<'_>, times: Switched) -> Result<Report> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let _live = inner.begin().ok_or(VfsError::NotMounted)?;
        let store = self.intent_store();
        engine::switch(store.as_ref(), &inner.cfg.core, &inner.cfg.rt, req, || {
            Run::new(&inner, req.mode, times.time, Some(req.from_root))
        })
    }

    /// Re-runs the stored intent: after a restart (the interrupted switch completes) and every
    /// 5 s for paths that were held open. `time` is the workspace's current epoch time. Applies
    /// the SAFE rules. `None` when there is no intent.
    pub fn resume(&self, time: i64) -> Result<Option<Replayed>> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let _live = inner.begin().ok_or(VfsError::NotMounted)?;
        let store = self.intent_store();
        engine::resume(store.as_ref(), &inner.cfg.core, &inner.cfg.rt, |from| {
            Run::new(&inner, Mode::Safe, time, Some(from))
        })
    }

    /// The `Provider::apply_switch` entry: SAFE rules, no extras, the base tree unchanged.
    pub(crate) fn apply_plan(&self, plan: &Plan, times: Switched) -> Result<SwitchOutcome> {
        let inner = self.inner().ok_or(VfsError::NotMounted)?;
        let _live = inner.begin().ok_or(VfsError::NotMounted)?;
        engine::apply_plan(&Run::new(&inner, Mode::Safe, times.time, None), plan)
    }
}
