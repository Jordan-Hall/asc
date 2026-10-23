//! Status of a virtual work tree (SPEC §10.2.6-7) without walking it. Only the dirty candidates
//! (journal hints, staged paths, and what directory renames and tombstones expand to) are looked
//! at, through [`DiskState`], which the OS providers implement. Paths that were never materialised
//! are clean by construction. The journal is hints only: a path is reported when the disk says it
//! differs, never because a hint says so.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;

use asc_core::git::BlobOidHasher;
use asc_core::id::GitOid;
use asc_core::object::Kind;
use asc_core::path::{MAX_TREE_DEPTH, Name, RepoPath};

use crate::journal::{Entry, Op};
use crate::redirect::Redirections;
use crate::status::{ChangeKind, PathChange, Status, Untracked};

/// Files larger than this are reported modified without being hashed (the provider's `read` is
/// whole-file).
pub const MAX_HASH_BYTES: u64 = 512 * 1024 * 1024;
/// Directory entries one `vstatus` walk of renamed directories visits at most.
pub const MAX_WALK: usize = 1_000_000;

/// What a path is on disk (ProjFS `PrjGetOnDiskFileState`, FUSE overlay, plain files).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathState {
    /// Never materialised: no on-disk entry, served from the tree.
    Virtual,
    /// A placeholder with no data: clean.
    Placeholder,
    /// A placeholder whose data was fetched and not changed: clean.
    Hydrated,
    /// A real file or directory (created, or a placeholder the user wrote to).
    Full,
    /// A local deletion of a path the tree has.
    Tombstone,
    /// Nothing there and no record of one.
    Missing,
}

impl PathState {
    /// Whether a directory in this state exists as a real entry that may hold on-disk children.
    fn materialised(self) -> bool {
        matches!(self, Self::Placeholder | Self::Hydrated | Self::Full)
    }
}

/// The type of an on-disk entry (not following links).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskKind {
    /// `exec` is `None` where the file system has no exec bit.
    File {
        exec: Option<bool>,
    },
    Symlink,
    Dir,
    /// FIFO, device or socket: never read.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskStat {
    pub kind: DiskKind,
    pub size: u64,
}

/// An on-disk (non-virtual) child of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Name,
    pub dir: bool,
}

/// The work tree as the provider sees it. Every path is untrusted; implementations resolve it
/// through `SafeRoot` and never follow a link in an ancestor.
pub trait DiskState {
    fn state(&self, path: &RepoPath) -> io::Result<PathState>;
    /// `None` when nothing is there.
    fn stat(&self, path: &RepoPath) -> io::Result<Option<DiskStat>>;
    /// The bytes the path would be ingested as (after line-ending conversion), or a symlink's
    /// target. At most `max` bytes; longer content is an error.
    fn read(&self, path: &RepoPath, max: u64) -> io::Result<Vec<u8>>;
    /// The on-disk children of a materialised directory; virtual-only children are not listed.
    fn list(&self, dir: &RepoPath) -> io::Result<Vec<DirEntry>>;
}

/// A tracked leaf of the effective (staged) tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub kind: Kind,
    pub git: GitOid,
}

/// Everything [`compute`] needs besides the disk.
pub struct Input<'a> {
    /// Tracked leaves of the effective tree (the index): the base tree plus staged changes.
    pub effective: &'a BTreeMap<RepoPath, Tracked>,
    /// Staged paths; they are candidates even without a hint.
    pub staged: &'a BTreeSet<RepoPath>,
    /// Journal hints since the last clean status.
    pub journal: &'a [Entry],
    pub redirections: &'a Redirections,
    /// Ignore rules: `(path, is_dir)`.
    pub ignored: &'a dyn Fn(&RepoPath, bool) -> bool,
}

/// Computes the unstaged changes and the untracked paths of a virtual work tree. `staged`,
/// `unmerged` and `shadowed` of the result are left empty: they come from the stage rows.
pub fn compute(input: &Input<'_>, disk: &dyn DiskState) -> io::Result<Status> {
    let mut run = Run {
        input,
        disk,
        candidates: BTreeSet::new(),
        deleted: BTreeSet::new(),
        loose: BTreeSet::new(),
        walk: Vec::new(),
        materialised: HashMap::new(),
        hidden: HashMap::new(),
    };
    run.collect()?;
    run.walk_renamed()?;
    let mut changes: BTreeMap<RepoPath, ChangeKind> = run
        .deleted
        .iter()
        .map(|p| (p.clone(), ChangeKind::Deleted))
        .collect();
    for p in std::mem::take(&mut run.candidates) {
        if changes.contains_key(&p) {
            continue;
        }
        if let Some(t) = input.effective.get(&p)
            && let Some(kind) = run.check(&p, t)?
        {
            changes.insert(p, kind);
        }
    }
    let untracked = run.untracked()?;
    Ok(Status {
        unstaged: changes
            .into_iter()
            .map(|(path, kind)| PathChange { path, kind })
            .collect(),
        untracked,
        ..Status::default()
    })
}

struct Run<'a> {
    input: &'a Input<'a>,
    disk: &'a dyn DiskState,
    /// Tracked paths to compare with the disk.
    candidates: BTreeSet<RepoPath>,
    /// Tracked paths known deleted (directory tombstones).
    deleted: BTreeSet<RepoPath>,
    /// Untracked paths that may exist.
    loose: BTreeSet<RepoPath>,
    /// Directories whose on-disk subtree is walked (`dir-renamed-to`, created directories).
    walk: Vec<RepoPath>,
    /// Memo: directory exists as a real entry.
    materialised: HashMap<RepoPath, bool>,
    /// Memo: directory is ignored or inside an ignored directory.
    hidden: HashMap<RepoPath, bool>,
}

impl Run<'_> {
    fn tracked_under(&self, dir: &RepoPath) -> impl Iterator<Item = &RepoPath> {
        let d = dir.as_bytes();
        self.input
            .effective
            .range(dir.clone()..)
            .map(|(k, _)| k)
            .take_while(move |k| k.as_bytes().starts_with(d))
            .filter(move |k| k.starts_with_dir(dir))
    }

    fn collect(&mut self) -> io::Result<()> {
        let input = self.input;
        for p in input.staged {
            if input.effective.contains_key(p) {
                self.candidates.insert(p.clone());
            }
        }
        for e in input.journal {
            let Ok(path) = RepoPath::new(e.path.as_slice()) else {
                continue;
            };
            if input.redirections.covers(&path) {
                continue;
            }
            match e.op {
                Op::Modified | Op::Created | Op::RenamedFrom => self.file_hint(path),
                Op::Deleted => {
                    if input.effective.contains_key(&path) {
                        self.candidates.insert(path);
                        continue;
                    }
                    let under: Vec<RepoPath> = self.tracked_under(&path).cloned().collect();
                    if under.is_empty() {
                        continue;
                    }
                    // A deleted directory: a tombstone (or nothing) removes everything below it.
                    // If something is there again, each path is checked on its own.
                    match self.disk.state(&path)? {
                        PathState::Tombstone | PathState::Missing => self.deleted.extend(under),
                        _ => self.candidates.extend(under),
                    }
                }
                Op::DirRenamedFrom => {
                    // Everything the base tree had below the source is gone unless the disk still
                    // has it.
                    let under: Vec<RepoPath> = self.tracked_under(&path).cloned().collect();
                    self.candidates.extend(under);
                }
                Op::DirRenamedTo => self.walk.push(path),
            }
        }
        Ok(())
    }

    fn file_hint(&mut self, path: RepoPath) {
        if self.input.effective.contains_key(&path) {
            self.candidates.insert(path);
        } else {
            self.loose.insert(path);
        }
    }

    /// Lists the on-disk subtrees of `dir-renamed-to` and created directories as new files.
    fn walk_renamed(&mut self) -> io::Result<()> {
        let mut seen = 0usize;
        let mut stack: Vec<(RepoPath, usize)> = std::mem::take(&mut self.walk)
            .into_iter()
            .map(|d| (d, 0))
            .collect();
        while let Some((dir, depth)) = stack.pop() {
            if depth > MAX_TREE_DEPTH {
                continue;
            }
            let list = match self.disk.state(&dir)? {
                s if s.materialised() => self.disk.list(&dir)?,
                _ => continue,
            };
            for entry in list {
                seen += 1;
                if seen > MAX_WALK {
                    return Err(io::Error::other(
                        "renamed directories hold too many entries",
                    ));
                }
                if self
                    .input
                    .redirections
                    .is_redirected_name(entry.name.as_bytes())
                {
                    continue;
                }
                let child = dir.join(&entry.name);
                if entry.dir {
                    stack.push((child, depth + 1));
                } else {
                    self.file_hint(child);
                }
            }
        }
        Ok(())
    }

    /// Compares one tracked path with the disk.
    fn check(&self, p: &RepoPath, t: &Tracked) -> io::Result<Option<ChangeKind>> {
        match self.disk.state(p)? {
            PathState::Virtual | PathState::Placeholder | PathState::Hydrated => Ok(None),
            PathState::Tombstone | PathState::Missing => Ok(Some(ChangeKind::Deleted)),
            PathState::Full => {
                let Some(stat) = self.disk.stat(p)? else {
                    return Ok(Some(ChangeKind::Deleted));
                };
                self.compare(p, t, stat)
            }
        }
    }

    fn compare(&self, p: &RepoPath, t: &Tracked, stat: DiskStat) -> io::Result<Option<ChangeKind>> {
        let changed = Ok(Some(ChangeKind::Modified));
        let retyped = Ok(Some(ChangeKind::TypeChanged));
        match (t.kind, stat.kind) {
            // A submodule's directory is the sub-repository's business.
            (Kind::Gitlink, DiskKind::Dir) => Ok(None),
            (Kind::File | Kind::Exec, DiskKind::File { exec }) => {
                if let Some(x) = exec
                    && x != (t.kind == Kind::Exec)
                {
                    return changed;
                }
                self.same_content(p, t, stat.size)
            }
            (Kind::Symlink, DiskKind::Symlink) => self.same_content(p, t, stat.size),
            _ => retyped,
        }
    }

    fn same_content(&self, p: &RepoPath, t: &Tracked, size: u64) -> io::Result<Option<ChangeKind>> {
        if size > MAX_HASH_BYTES {
            return Ok(Some(ChangeKind::Modified));
        }
        let data = self.disk.read(p, MAX_HASH_BYTES)?;
        let mut h = BlobOidHasher::new(data.len() as u64);
        h.update(&data);
        let oid = h.finalize().map_err(io::Error::other)?;
        Ok((oid != t.git).then_some(ChangeKind::Modified))
    }

    /// Whether `dir` exists as a real entry (memoised).
    fn is_materialised(&mut self, dir: &RepoPath) -> io::Result<bool> {
        if let Some(&m) = self.materialised.get(dir) {
            return Ok(m);
        }
        let m = self.disk.state(dir)?.materialised();
        self.materialised.insert(dir.clone(), m);
        Ok(m)
    }

    /// Whether `dir` or an ancestor is ignored (memoised).
    fn is_hidden(&mut self, dir: &RepoPath) -> bool {
        if let Some(&h) = self.hidden.get(dir) {
            return h;
        }
        let h = (self.input.ignored)(dir, true) || dir.parent().is_some_and(|p| self.is_hidden(&p));
        self.hidden.insert(dir.clone(), h);
        h
    }

    /// Whether the path or any ancestor directory is not a real, visible entry.
    fn out_of_view(&mut self, p: &RepoPath) -> io::Result<bool> {
        let mut cur = p.parent();
        while let Some(d) = cur {
            if !self.is_materialised(&d)? {
                return Ok(true);
            }
            cur = d.parent();
        }
        Ok(p.parent().is_some_and(|d| self.is_hidden(&d)))
    }

    /// The untracked entries: files on disk that the tree lacks, collapsed to the topmost
    /// directory that holds nothing tracked.
    fn untracked(&mut self) -> io::Result<Vec<Untracked>> {
        let mut out: BTreeMap<RepoPath, bool> = BTreeMap::new();
        let loose = std::mem::take(&mut self.loose);
        for p in loose {
            if self.input.effective.contains_key(&p) || self.out_of_view(&p)? {
                continue;
            }
            if self.disk.state(&p)? != PathState::Full {
                continue;
            }
            match self.disk.stat(&p)?.map(|s| s.kind) {
                Some(DiskKind::File { .. } | DiskKind::Symlink) => {}
                _ => continue,
            }
            if (self.input.ignored)(&p, false) {
                continue;
            }
            let top = self.topmost_untracked_dir(&p);
            match top {
                Some(d) => out.insert(d, true),
                None => out.insert(p, false),
            };
        }
        // A directory entry hides the files below it.
        let dirs: Vec<RepoPath> = out
            .iter()
            .filter(|(_, d)| **d)
            .map(|(p, _)| p.clone())
            .collect();
        Ok(out
            .into_iter()
            .filter(|(p, _)| !dirs.iter().any(|d| p.starts_with_dir(d)))
            .map(|(path, dir)| Untracked { path, dir })
            .collect())
    }

    /// The shortest ancestor directory of `p` with no tracked path below it.
    fn topmost_untracked_dir(&self, p: &RepoPath) -> Option<RepoPath> {
        let mut chain = Vec::new();
        let mut cur = p.parent();
        while let Some(d) = cur {
            cur = d.parent();
            chain.push(d);
        }
        chain.into_iter().rev().find(|d| {
            self.tracked_under(d).next().is_none() && !self.input.effective.contains_key(d)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn oid(data: &[u8]) -> GitOid {
        let mut h = BlobOidHasher::new(data.len() as u64);
        h.update(data);
        h.finalize().unwrap()
    }

    #[derive(Clone)]
    struct Item {
        state: PathState,
        kind: DiskKind,
        data: Vec<u8>,
    }

    /// A fake provider: entries not listed are `Virtual` when `virtual_default`, else `Missing`.
    #[derive(Default)]
    struct Fake {
        items: BTreeMap<RepoPath, Item>,
        reads: Cell<usize>,
    }

    impl Fake {
        fn put(&mut self, path: &str, state: PathState, kind: DiskKind, data: &[u8]) {
            self.items.insert(
                p(path),
                Item {
                    state,
                    kind,
                    data: data.to_vec(),
                },
            );
        }
        fn file(&mut self, path: &str, state: PathState, data: &[u8]) {
            self.put(path, state, DiskKind::File { exec: None }, data);
        }
        fn dir(&mut self, path: &str, state: PathState) {
            self.put(path, state, DiskKind::Dir, b"");
        }
    }

    impl DiskState for Fake {
        fn state(&self, path: &RepoPath) -> io::Result<PathState> {
            Ok(self.items.get(path).map_or(PathState::Missing, |i| i.state))
        }
        fn stat(&self, path: &RepoPath) -> io::Result<Option<DiskStat>> {
            Ok(self.items.get(path).map(|i| DiskStat {
                kind: i.kind,
                size: i.data.len() as u64,
            }))
        }
        fn read(&self, path: &RepoPath, _: u64) -> io::Result<Vec<u8>> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.items[path].data.clone())
        }
        fn list(&self, dir: &RepoPath) -> io::Result<Vec<DirEntry>> {
            Ok(self
                .items
                .iter()
                .filter(|(k, i)| k.parent().as_ref() == Some(dir) && i.state.materialised())
                .map(|(k, i)| DirEntry {
                    name: k.file_name(),
                    dir: i.kind == DiskKind::Dir,
                })
                .collect())
        }
    }

    fn tracked(files: &[(&str, &[u8])]) -> BTreeMap<RepoPath, Tracked> {
        files
            .iter()
            .map(|(n, d)| {
                (
                    p(n),
                    Tracked {
                        kind: Kind::File,
                        git: oid(d),
                    },
                )
            })
            .collect()
    }

    fn hints(list: &[(Op, &str)]) -> Vec<Entry> {
        list.iter()
            .map(|(op, s)| Entry {
                path: s.as_bytes().to_vec(),
                op: *op,
            })
            .collect()
    }

    fn run(
        effective: &BTreeMap<RepoPath, Tracked>,
        staged: &[&str],
        journal: &[Entry],
        redirs: &str,
        disk: &Fake,
    ) -> Status {
        let staged: BTreeSet<RepoPath> = staged.iter().map(|s| p(s)).collect();
        let r = Redirections::parse(redirs);
        let ignored = |q: &RepoPath, _| q.as_bytes().ends_with(b".log");
        compute(
            &Input {
                effective,
                staged: &staged,
                journal,
                redirections: &r,
                ignored: &ignored,
            },
            disk,
        )
        .unwrap()
    }

    fn changes(s: &Status) -> Vec<(String, char)> {
        s.unstaged
            .iter()
            .map(|c| (c.path.to_string(), c.kind.letter()))
            .collect()
    }

    fn untracked(s: &Status) -> Vec<(String, bool)> {
        s.untracked
            .iter()
            .map(|u| (u.path.to_string(), u.dir))
            .collect()
    }

    #[test]
    fn every_on_disk_state() {
        let eff = tracked(&[
            ("virt", b"v"),
            ("ph", b"p"),
            ("hyd", b"h"),
            ("same", b"s"),
            ("edit", b"old"),
            ("tomb", b"t"),
            ("gone", b"g"),
            ("link", b"l"),
        ]);
        let mut d = Fake::default();
        d.file("virt", PathState::Virtual, b"");
        d.file("ph", PathState::Placeholder, b"zzz");
        d.file("hyd", PathState::Hydrated, b"zzz");
        d.file("same", PathState::Full, b"s");
        d.file("edit", PathState::Full, b"new");
        d.file("tomb", PathState::Tombstone, b"");
        d.put("link", PathState::Full, DiskKind::Symlink, b"target");
        let j = hints(&[
            (Op::Modified, "virt"),
            (Op::Modified, "ph"),
            (Op::Modified, "hyd"),
            (Op::Modified, "same"),
            (Op::Modified, "edit"),
            (Op::Deleted, "tomb"),
            (Op::Deleted, "gone"),
            (Op::Modified, "link"),
        ]);
        let s = run(&eff, &[], &j, "", &d);
        assert_eq!(
            changes(&s),
            [
                ("edit".to_string(), 'M'),
                ("gone".to_string(), 'D'),
                ("link".to_string(), 'T'),
                ("tomb".to_string(), 'D'),
            ]
        );
        assert!(s.untracked.is_empty());
        // Only the Full files were read.
        assert_eq!(d.reads.get(), 2);
    }

    #[test]
    fn clean_tree_without_hints_reads_nothing() {
        let eff = tracked(&[("a", b"1"), ("b/c", b"2")]);
        let d = Fake::default();
        let s = run(&eff, &[], &[], "", &d);
        assert_eq!(s, Status::default());
        assert_eq!(d.reads.get(), 0);
    }

    #[test]
    fn staged_paths_are_candidates_and_exec_bit_counts() {
        let mut eff = tracked(&[("a", b"1")]);
        eff.get_mut(&p("a")).unwrap().kind = Kind::Exec;
        let mut d = Fake::default();
        d.put(
            "a",
            PathState::Full,
            DiskKind::File { exec: Some(false) },
            b"1",
        );
        let s = run(&eff, &["a"], &[], "", &d);
        assert_eq!(changes(&s), [("a".to_string(), 'M')]);
        d.put(
            "a",
            PathState::Full,
            DiskKind::File { exec: Some(true) },
            b"1",
        );
        assert!(run(&eff, &["a"], &[], "", &d).unstaged.is_empty());
    }

    #[test]
    fn untracked_only_in_materialised_dirs_and_not_redirected_or_ignored() {
        let eff = tracked(&[("src/a", b"1")]);
        let mut d = Fake::default();
        d.dir("src", PathState::Placeholder);
        d.file("src/new.rs", PathState::Full, b"x");
        d.file("src/debug.log", PathState::Full, b"x");
        d.file("top.txt", PathState::Full, b"x");
        // `virt` was never enumerated, so a stray hint under it is not trusted.
        d.file("virt/x", PathState::Full, b"x");
        d.dir("node_modules", PathState::Full);
        d.file("node_modules/pkg", PathState::Full, b"x");
        d.dir("fresh", PathState::Full);
        d.file("fresh/one", PathState::Full, b"x");
        d.file("fresh/two", PathState::Full, b"x");
        let j = hints(&[
            (Op::Created, "src/new.rs"),
            (Op::Created, "src/debug.log"),
            (Op::Created, "top.txt"),
            (Op::Created, "virt/x"),
            (Op::Created, "node_modules/pkg"),
            (Op::Created, "fresh/one"),
            (Op::Created, "fresh/two"),
            (Op::Created, "../escape"),
        ]);
        let s = run(&eff, &[], &j, "node_modules\n", &d);
        assert!(s.unstaged.is_empty());
        assert_eq!(
            untracked(&s),
            [
                ("fresh".to_string(), true),
                ("src/new.rs".to_string(), false),
                ("top.txt".to_string(), false),
            ]
        );
    }

    #[test]
    fn dir_rename_expands_to_deletes_and_new_files() {
        let eff = tracked(&[("old/a", b"1"), ("old/sub/b", b"2"), ("keep", b"3")]);
        let mut d = Fake::default();
        d.dir("new", PathState::Full);
        d.file("new/a", PathState::Full, b"1");
        d.dir("new/sub", PathState::Full);
        d.file("new/sub/b", PathState::Full, b"2");
        d.dir("new/node_modules", PathState::Full);
        d.file("new/node_modules/x", PathState::Full, b"2");
        let j = hints(&[(Op::DirRenamedFrom, "old"), (Op::DirRenamedTo, "new")]);
        let s = run(&eff, &[], &j, "node_modules", &d);
        assert_eq!(
            changes(&s),
            [("old/a".to_string(), 'D'), ("old/sub/b".to_string(), 'D')]
        );
        assert_eq!(untracked(&s), [("new".to_string(), true)]);
    }

    #[test]
    fn dir_rename_onto_a_tracked_path_compares_content() {
        let eff = tracked(&[("old/a", b"1"), ("dst/a", b"orig")]);
        let mut d = Fake::default();
        d.dir("dst", PathState::Full);
        d.file("dst/a", PathState::Full, b"from old");
        let j = hints(&[(Op::DirRenamedFrom, "old"), (Op::DirRenamedTo, "dst")]);
        let s = run(&eff, &[], &j, "", &d);
        assert_eq!(
            changes(&s),
            [("dst/a".to_string(), 'M'), ("old/a".to_string(), 'D')]
        );
        assert!(s.untracked.is_empty());
    }

    #[test]
    fn dir_tombstone_expands_to_every_tracked_path_below() {
        let eff = tracked(&[("t/x", b"1"), ("t/y/z", b"2"), ("t2/k", b"3"), ("u", b"4")]);
        let mut d = Fake::default();
        d.dir("t", PathState::Tombstone);
        let j = hints(&[(Op::Deleted, "t")]);
        let s = run(&eff, &[], &j, "", &d);
        assert_eq!(
            changes(&s),
            [("t/x".to_string(), 'D'), ("t/y/z".to_string(), 'D')]
        );
        assert_eq!(d.reads.get(), 0);
    }

    #[test]
    fn deleted_directory_that_is_back_is_checked_path_by_path() {
        let eff = tracked(&[("t/x", b"1"), ("t/y", b"2")]);
        let mut d = Fake::default();
        d.dir("t", PathState::Placeholder);
        d.file("t/x", PathState::Hydrated, b"1");
        d.file("t/y", PathState::Full, b"changed");
        let j = hints(&[(Op::Deleted, "t")]);
        let s = run(&eff, &[], &j, "", &d);
        assert_eq!(changes(&s), [("t/y".to_string(), 'M')]);
    }

    #[test]
    fn oversized_file_is_modified_without_reading() {
        struct Big;
        impl DiskState for Big {
            fn state(&self, _: &RepoPath) -> io::Result<PathState> {
                Ok(PathState::Full)
            }
            fn stat(&self, _: &RepoPath) -> io::Result<Option<DiskStat>> {
                Ok(Some(DiskStat {
                    kind: DiskKind::File { exec: None },
                    size: MAX_HASH_BYTES + 1,
                }))
            }
            fn read(&self, _: &RepoPath, _: u64) -> io::Result<Vec<u8>> {
                panic!("must not read");
            }
            fn list(&self, _: &RepoPath) -> io::Result<Vec<DirEntry>> {
                Ok(Vec::new())
            }
        }
        let eff = tracked(&[("big", b"x")]);
        let j = hints(&[(Op::Modified, "big")]);
        let r = Redirections::default();
        let staged = BTreeSet::new();
        let ignored = |_: &RepoPath, _| false;
        let s = compute(
            &Input {
                effective: &eff,
                staged: &staged,
                journal: &j,
                redirections: &r,
                ignored: &ignored,
            },
            &Big,
        )
        .unwrap();
        assert_eq!(changes(&s), [("big".to_string(), 'M')]);
    }
}
