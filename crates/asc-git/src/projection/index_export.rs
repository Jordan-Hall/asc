//! The synthetic index export for turbo (SPEC §9.4): HEAD's tree metadata overlaid with stage
//! set 0, rendered through [`crate::index`] (the only index writer).
//!
//! [`render_head`] streams the tree once. [`patch`] and [`update`] change only the paths that
//! differ from the previous export, so a one-path commit reads the trees along that path and no
//! others. An `asc-index-base` file beside `index` names the root tree the index was rendered
//! from; it is written after the index, so a crash between the two re-applies a diff.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use asc_core::git::blob_oid;
use asc_core::id::{GitOid, TreeId};
use asc_core::object::{Entry, Kind};
use asc_core::path::{PathError, RepoPath};
use asc_engine::Engine;
use asc_engine::tree;
use asc_state::state::stage::StageRow;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use data_encoding::HEXLOWER;
use futures::TryStreamExt;

use crate::index::{
    CacheTree, EntryFlags, FileMode, FileStat, IndexEntry, IndexError, IndexFile, Stage,
    StatSource, StatTime,
};

use super::{ProjectionError, write_locked};

/// Marker file in the gitdir: the root tree id (hex) the index was rendered from.
pub const BASE_FILE: &str = "asc-index-base";
/// Entries must be older than the index by this much to not read as racily clean.
const RACY_MARGIN: Duration = Duration::from_secs(2);

/// Failures exporting the index.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Marker(#[from] ProjectionError),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ExportError>;

/// The `asc.turboIndex` policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurboIndex {
    /// On when the root tree has `turbo.json` or the workspace is plain.
    #[default]
    Auto,
    On,
    Off,
}

impl TurboIndex {
    /// Parses the config value; anything but a boolean word is `Auto`.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("true" | "yes" | "on" | "1") => Self::On,
            Some("false" | "no" | "off" | "0") => Self::Off,
            _ => Self::Auto,
        }
    }

    /// Whether the workspace gets an `index` file.
    pub fn enabled(self, root_has_turbo_json: bool, plain: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => root_has_turbo_json || plain,
        }
    }
}

/// Marks the paths that get the skip-worktree bit (sparse cone, unrepresentable names).
pub type SkipFn<'a> = dyn Fn(&RepoPath) -> bool + Sync + 'a;

/// Render options; like [`RenderOptions`](crate::index::RenderOptions) but `Sync`, so the
/// export futures can be sent between threads.
pub struct ExportOptions<'a> {
    /// The clock; `None` reads the system clock.
    pub now: Option<SystemTime>,
    /// Build the TREE extension when every entry allows it (default on).
    pub cache_tree: bool,
    pub skip_worktree: Option<&'a SkipFn<'a>>,
}

impl Default for ExportOptions<'_> {
    fn default() -> Self {
        Self {
            now: None,
            cache_tree: true,
            skip_worktree: None,
        }
    }
}

/// An index rendered earlier, with the root tree it was rendered from and the paths that had
/// stage rows then (a cleared row must be re-resolved even when HEAD did not change there).
pub struct Basis {
    pub index: IndexFile,
    /// `None` when only the index is known (written by a checkout): it is kept as a stat cache
    /// but the tree is rendered whole.
    pub root: Option<TreeId>,
    pub staged: Vec<RepoPath>,
}

/// Gives an entry whose mode and oid equal the previous export's the previous stat, so a stat
/// recorded when the file was known to match survives; anything else asks `inner`.
struct Inherit<'a> {
    prev: &'a [IndexEntry],
    inner: &'a (dyn StatSource + Sync),
}

impl StatSource for Inherit<'_> {
    fn stat(&self, path: &RepoPath, entry: &Entry) -> FileStat {
        let at = self
            .prev
            .partition_point(|e| e.path.as_bytes() < path.as_bytes());
        if let Some(p) = self.prev.get(at)
            && p.path == *path
            && p.flags.stage == Stage::Normal
            && Some(p.mode) == mode_of(entry.kind)
            && p.oid == entry.git
        {
            return FileStat {
                ctime: p.ctime,
                mtime: p.mtime,
                dev: p.dev,
                ino: p.ino,
                uid: p.uid,
                gid: p.gid,
                size: p.size,
            };
        }
        self.inner.stat(path, entry)
    }
}

/// Reads the base marker: the root tree id, then one hex-encoded staged path per line. `None`
/// when absent or malformed.
pub fn read_base(gitdir: &Path) -> Option<(TreeId, Vec<RepoPath>)> {
    let text = std::fs::read_to_string(gitdir.join(BASE_FILE)).ok()?;
    let mut lines = text.lines();
    let root = lines.next()?.trim().parse().ok()?;
    let staged = lines
        .map(|l| RepoPath::new(HEXLOWER.decode(l.trim().as_bytes()).ok()?).ok())
        .collect::<Option<Vec<_>>>()?;
    Some((root, staged))
}

/// Records `root` and the paths of `rows` as the state the index in `gitdir` was rendered from.
pub fn write_base(gitdir: &Path, root: TreeId, rows: &[StageRow]) -> Result<()> {
    let paths: BTreeSet<&[u8]> = rows.iter().map(|r| r.path.as_slice()).collect();
    let mut text = format!("{root}\n");
    for p in paths {
        text.push_str(&HEXLOWER.encode(p));
        text.push('\n');
    }
    Ok(write_locked(&gitdir.join(BASE_FILE), text.as_bytes())?)
}

/// Writes `index` and then the base marker.
pub fn store(gitdir: &Path, index: &IndexFile, root: TreeId, rows: &[StageRow]) -> Result<PathBuf> {
    let path = gitdir.join("index");
    crate::index::write(&path, index)?;
    write_base(gitdir, root, rows)?;
    Ok(path)
}

/// Stage rows grouped by path.
struct ByPath<'a>(HashMap<&'a [u8], Vec<&'a StageRow>>);

impl<'a> ByPath<'a> {
    fn new(rows: &'a [StageRow]) -> Self {
        let mut map: HashMap<&[u8], Vec<&StageRow>> = HashMap::new();
        for r in rows {
            map.entry(r.path.as_slice()).or_default().push(r);
        }
        Self(map)
    }

    fn get(&self, path: &RepoPath) -> &[&'a StageRow] {
        self.0.get(path.as_bytes()).map_or(&[], Vec::as_slice)
    }
}

fn mode_of(kind: Kind) -> Option<FileMode> {
    match kind {
        Kind::File => Some(FileMode::File),
        Kind::Exec => Some(FileMode::Exec),
        Kind::Symlink => Some(FileMode::Symlink),
        Kind::Gitlink => Some(FileMode::Gitlink),
        Kind::Dir => None,
    }
}

struct Leaf {
    kind: Kind,
    id: [u8; 32],
    size: u64,
    git: GitOid,
    stage: Stage,
    ita: bool,
}

fn entry_of(
    path: &RepoPath,
    leaf: Leaf,
    stat: &(dyn StatSource + Sync),
    skip: Option<&SkipFn<'_>>,
) -> Option<IndexEntry> {
    let mode = mode_of(leaf.kind)?;
    let st = if mode == FileMode::Gitlink || leaf.stage != Stage::Normal {
        FileStat {
            size: leaf.size as u32,
            ..FileStat::default()
        }
    } else {
        let e = Entry {
            name: path.file_name(),
            kind: leaf.kind,
            id: leaf.id,
            size: leaf.size,
            git: leaf.git,
        };
        stat.stat(path, &e)
    };
    Some(IndexEntry {
        path: path.clone(),
        mode,
        oid: leaf.git,
        size: st.size,
        ctime: st.ctime,
        mtime: st.mtime,
        dev: st.dev,
        ino: st.ino,
        uid: st.uid,
        gid: st.gid,
        flags: EntryFlags {
            stage: leaf.stage,
            skip_worktree: skip.is_some_and(|f| f(path)),
            intent_to_add: leaf.ita,
        },
    })
}

/// The index entries of one path: its stage rows when it has any (conflict stages as git writes
/// them, a staged delete as nothing), else HEAD's entry.
fn resolve(
    path: &RepoPath,
    head: Option<&Entry>,
    rows: &[&StageRow],
    stat: &(dyn StatSource + Sync),
    skip: Option<&SkipFn<'_>>,
) -> Vec<IndexEntry> {
    if rows.is_empty() {
        return head
            .and_then(|e| {
                let leaf = Leaf {
                    kind: e.kind,
                    id: e.id,
                    size: e.size,
                    git: e.git,
                    stage: Stage::Normal,
                    ita: false,
                };
                entry_of(path, leaf, stat, skip)
            })
            .into_iter()
            .collect();
    }
    let mut out: Vec<IndexEntry> = rows
        .iter()
        .filter_map(|r| {
            let stage = match r.stage {
                0 => Stage::Normal,
                1 => Stage::Base,
                2 => Stage::Ours,
                3 => Stage::Theirs,
                _ => return None,
            };
            r.file?; // a staged delete leaves no entry
            let git = match r.git {
                Some(g) => g,
                None if r.intent_to_add => blob_oid(b""),
                None => return None,
            };
            let leaf = Leaf {
                kind: r.kind,
                id: *r.file?.as_bytes(),
                size: r.size,
                git,
                stage,
                ita: r.intent_to_add && stage == Stage::Normal,
            };
            entry_of(path, leaf, stat, skip)
        })
        .collect();
    out.sort_by_key(|e| e.flags.stage);
    out
}

/// Sorts `entries`, sets the racy-safe mtime and builds the cache tree when every entry is a
/// plain stage-0 one (git rebuilds the TREE extension itself otherwise).
fn finish(mut entries: Vec<IndexEntry>, opts: &ExportOptions<'_>) -> IndexFile {
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.flags.stage.cmp(&b.flags.stage)));
    let newest = entries
        .iter()
        .map(|e| e.ctime.max(e.mtime))
        .max()
        .unwrap_or(StatTime::default());
    let now = opts.now.unwrap_or_else(SystemTime::now);
    let plain = entries
        .iter()
        .all(|e| e.flags.stage == Stage::Normal && !e.flags.intent_to_add);
    IndexFile {
        tree: (opts.cache_tree && plain).then(|| CacheTree::from_entries(&entries)),
        entries,
        mtime: Some(now.max(newest.to_system_time() + RACY_MARGIN)),
    }
}

/// Renders HEAD's tree at `root` overlaid with `rows` (stage set 0), streaming the tree. `prev`
/// is the previous index (sorted), whose stat is kept for entries that did not change.
pub async fn render_head<O: Objects, R: Refs>(
    engine: &Engine<O, R>,
    root: TreeId,
    rows: &[StageRow],
    prev: &[IndexEntry],
    stat: &(dyn StatSource + Sync),
    opts: &ExportOptions<'_>,
) -> Result<IndexFile> {
    let stat = &Inherit { prev, inner: stat };
    let by_path = ByPath::new(rows);
    let mut out = Vec::new();
    let mut walk = engine.walk(root, None);
    while let Some((path, entry)) = walk.try_next().await? {
        if by_path.get(&path).is_empty() {
            out.extend(resolve(&path, Some(&entry), &[], stat, opts.skip_worktree));
        }
    }
    let mut seen = HashSet::new();
    for r in rows {
        if seen.insert(r.path.as_slice()) {
            let path = RepoPath::new(r.path.clone())?;
            out.extend(resolve(
                &path,
                None,
                by_path.get(&path),
                stat,
                opts.skip_worktree,
            ));
        }
    }
    Ok(finish(out, opts))
}

/// Replaces the entries of the given paths in `prev` with their resolved entries (none removes
/// the path). Touches no tree.
pub fn patch(
    prev: IndexFile,
    resolved: Vec<(RepoPath, Vec<IndexEntry>)>,
    opts: &ExportOptions<'_>,
) -> IndexFile {
    let gone: HashSet<&[u8]> = resolved.iter().map(|(p, _)| p.as_bytes()).collect();
    let mut entries = prev.entries;
    entries.retain(|e| !gone.contains(e.path.as_bytes()));
    entries.extend(resolved.into_iter().flat_map(|(_, e)| e));
    finish(entries, opts)
}

/// [`patch`] for `changed` paths of the tree at `root`: stage rows win, else HEAD's entry.
/// Reads one tree per path component, only for paths without a row.
pub async fn patch_head<O: Objects, R: Refs>(
    engine: &Engine<O, R>,
    prev: IndexFile,
    root: TreeId,
    changed: &BTreeSet<RepoPath>,
    rows: &[StageRow],
    stat: &(dyn StatSource + Sync),
    opts: &ExportOptions<'_>,
) -> Result<IndexFile> {
    let stat = &Inherit {
        prev: &prev.entries,
        inner: stat,
    };
    let by_path = ByPath::new(rows);
    let mut resolved = Vec::with_capacity(changed.len());
    for path in changed {
        let staged = by_path.get(path);
        let head = if staged.is_empty() {
            engine.lookup(root, path).await?
        } else {
            None
        };
        let entries = resolve(path, head.as_ref(), staged, stat, opts.skip_worktree);
        resolved.push((path.clone(), entries));
    }
    Ok(patch(prev, resolved, opts))
}

/// Brings the export to HEAD `root` plus `rows`. With a [`Basis`] only the paths that differ
/// between its root and `root`, plus `stage_paths`, are recomputed; without one, or when the
/// difference is too large to list, the tree is rendered whole.
pub async fn update<O: Objects, R: Refs>(
    engine: &Engine<O, R>,
    basis: Option<Basis>,
    root: TreeId,
    stage_paths: &[RepoPath],
    rows: &[StageRow],
    stat: &(dyn StatSource + Sync),
    opts: &ExportOptions<'_>,
) -> Result<IndexFile> {
    let Some(basis) = basis else {
        return render_head(engine, root, rows, &[], stat, opts).await;
    };
    let Some(base_root) = basis.root else {
        return render_head(engine, root, rows, &basis.index.entries, stat, opts).await;
    };
    let mut changed: BTreeSet<RepoPath> = stage_paths.iter().cloned().collect();
    changed.extend(basis.staged);
    for r in rows {
        changed.insert(RepoPath::new(r.path.clone())?);
    }
    match engine.leafdiff(base_root, root, None).await {
        Ok(diff) => changed.extend(diff.into_iter().map(|(p, _)| p)),
        Err(tree::Error::TooLarge(_) | tree::Error::OutputTooLarge(_)) => {
            return render_head(engine, root, rows, &basis.index.entries, stat, opts).await;
        }
        Err(e) => return Err(e.into()),
    }
    patch_head(engine, basis.index, root, &changed, rows, stat, opts).await
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bytes::Bytes;
    use asc_core::content::{Body, ingest};
    use asc_core::id::FileId;
    use asc_core::object::Change;
    use asc_core::path::RefName;
    use asc_store::bucket::BucketObjects;
    use asc_store::key::{KeyPrefix, ObjKey};
    use asc_store::objects::Result as ObjResult;
    use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, RepoKey};
    use futures::stream::BoxStream;

    use super::*;

    struct NoRefs;

    #[async_trait]
    impl Refs for NoRefs {
        async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
            Ok(None)
        }
        async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
            Ok(Vec::new())
        }
        async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
            Err(RefError::Corrupt("unused".into()))
        }
        async fn log(
            &self,
            _: RepoKey,
            _: &RefName,
            _: u32,
        ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
            Ok(Vec::new())
        }
    }

    /// Counts tree reads.
    struct Counting {
        inner: BucketObjects,
        trees: AtomicUsize,
    }

    #[async_trait]
    impl Objects for Counting {
        async fn get(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
            if matches!(key, ObjKey::Tree(_)) {
                self.trees.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.get(key).await
        }
        async fn get_stored(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
            self.inner.get_stored(key).await
        }
        async fn get_range(
            &self,
            key: &ObjKey,
            r: std::ops::Range<u64>,
        ) -> ObjResult<Option<Bytes>> {
            self.inner.get_range(key, r).await
        }
        async fn put(&self, key: &ObjKey, stored: Bytes) -> ObjResult<()> {
            self.inner.put(key, stored).await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
            self.inner.has_many(keys).await
        }
        async fn delete(&self, key: &ObjKey) -> ObjResult<()> {
            self.inner.delete(key).await
        }
        fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
            self.inner.list(prefix)
        }
    }

    type Eng = Engine<Counting, NoRefs>;

    fn engine() -> (Arc<Counting>, Eng) {
        let objects = Arc::new(Counting {
            inner: BucketObjects::memory(),
            trees: AtomicUsize::new(0),
        });
        (
            objects.clone(),
            Engine::new(objects, Arc::new(NoRefs), RepoKey(1)),
        )
    }

    /// Stat that reports the entry size and one fixed time.
    struct Fixed;

    impl StatSource for Fixed {
        fn stat(&self, _: &RepoPath, e: &Entry) -> FileStat {
            let t = StatTime {
                secs: 1_700_000_000,
                nsecs: 0,
            };
            FileStat {
                ctime: t,
                mtime: t,
                size: e.size as u32,
                ..FileStat::default()
            }
        }
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).expect("path")
    }

    fn write(content: &[u8]) -> (Change, FileId, u64) {
        let got = ingest(Cursor::new(content)).expect("ingest");
        assert!(matches!(got.body, Body::Inline(_)));
        let change = Change::write(Kind::File, got.file, got.size, got.git, None).expect("change");
        (change, got.file, got.size)
    }

    async fn tree_of(eng: &Eng, base: TreeId, files: &[(&str, &[u8])]) -> TreeId {
        let changes: Vec<_> = files.iter().map(|(p, c)| (rp(p), write(c).0)).collect();
        eng.apply(base, &changes).await.expect("apply")
    }

    fn row(path: &str, content: &[u8]) -> StageRow {
        let (_, file, size) = write(content);
        StageRow {
            path: path.as_bytes().to_vec(),
            stage: 0,
            kind: Kind::File,
            file: Some(file),
            size,
            git: Some(blob_oid(content)),
            intent_to_add: false,
        }
    }

    fn deleted(path: &str) -> StageRow {
        StageRow::from_leaf(path.as_bytes().to_vec(), None)
    }

    fn lines(idx: &IndexFile) -> Vec<String> {
        idx.entries
            .iter()
            .map(|e| {
                let mode = match e.mode {
                    FileMode::File => "100644",
                    FileMode::Exec => "100755",
                    FileMode::Symlink => "120000",
                    FileMode::Gitlink => "160000",
                };
                let stage = match e.flags.stage {
                    Stage::Normal => 0,
                    Stage::Base => 1,
                    Stage::Ours => 2,
                    Stage::Theirs => 3,
                };
                format!(
                    "{mode} {} {stage}\t{}",
                    e.oid,
                    String::from_utf8_lossy(e.path.as_bytes())
                )
            })
            .collect()
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git") // lint:allow-raw-git
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).expect("utf8")
    }

    fn put(dir: &Path, name: &str, body: &[u8]) {
        let f = dir.join(name);
        std::fs::create_dir_all(f.parent().expect("parent")).expect("mkdir");
        std::fs::write(f, body).expect("write");
    }

    #[test]
    fn policy_follows_turbo_json_and_mode() {
        assert!(TurboIndex::Auto.enabled(true, false));
        assert!(TurboIndex::Auto.enabled(false, true));
        assert!(!TurboIndex::Auto.enabled(false, false));
        assert!(TurboIndex::On.enabled(false, false));
        assert!(!TurboIndex::Off.enabled(true, true));
        assert_eq!(TurboIndex::parse(Some("False")), TurboIndex::Off);
        assert_eq!(TurboIndex::parse(Some("auto")), TurboIndex::Auto);
        assert_eq!(TurboIndex::parse(None), TurboIndex::Auto);
    }

    #[tokio::test]
    async fn staged_new_delete_and_modify_match_git_ls_files() {
        let (_, eng) = engine();
        let head = tree_of(
            &eng,
            Eng::empty_tree_id(),
            &[
                ("a.txt", b"one\n"),
                ("b.txt", b"two\n"),
                ("d/c.txt", b"three\n"),
            ],
        )
        .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "user.email", "t@example.com"]);
        put(p, "a.txt", b"one\n");
        put(p, "b.txt", b"two\n");
        put(p, "d/c.txt", b"three\n");
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "init"]);
        put(p, "n.txt", b"new\n");
        put(p, "a.txt", b"one changed\n");
        git(p, &["add", "n.txt", "a.txt"]);
        git(p, &["rm", "-q", "--cached", "b.txt"]);

        let rows = [
            row("a.txt", b"one changed\n"),
            deleted("b.txt"),
            row("n.txt", b"new\n"),
        ];
        let idx = render_head(&eng, head, &rows, &[], &Fixed, &ExportOptions::default())
            .await
            .expect("render");
        let expected: Vec<String> = git(p, &["ls-files", "-s"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines(&idx), expected);

        let path = dir.path().join("synthetic-index");
        crate::index::write(&path, &idx).expect("write");
        let back = IndexFile::read(&path).expect("gix-index reads it");
        assert_eq!(lines(&back), expected);
        let newest = back.entries.iter().map(|e| e.mtime).max().expect("entries");
        assert!(
            back.mtime.expect("mtime") >= newest.to_system_time() + RACY_MARGIN,
            "no racy entry"
        );
    }

    #[tokio::test]
    async fn patch_equals_a_full_render() {
        let (_, eng) = engine();
        let one = tree_of(
            &eng,
            Eng::empty_tree_id(),
            &[("a.txt", b"1"), ("d/x", b"2"), ("d/y", b"3")],
        )
        .await;
        let two = tree_of(&eng, one, &[("d/x", b"22"), ("e/z", b"4")]).await;
        let opts = ExportOptions::default();
        let first_rows = [row("a.txt", b"staged")];
        let first = render_head(&eng, one, &first_rows, &[], &Fixed, &opts)
            .await
            .expect("render");
        // `a.txt` is unstaged and `d/y` staged away between the two exports.
        let rows = [deleted("d/y")];
        let basis = Basis {
            index: first,
            root: Some(one),
            staged: vec![rp("a.txt")],
        };
        let patched = update(
            &eng,
            Some(basis),
            two,
            &[rp("a.txt"), rp("d/y")],
            &rows,
            &Fixed,
            &opts,
        )
        .await
        .expect("update");
        let whole = render_head(&eng, two, &rows, &[], &Fixed, &opts)
            .await
            .expect("render");
        assert_eq!(patched.entries, whole.entries);
        assert_eq!(patched.tree, whole.tree);
    }

    #[tokio::test]
    async fn one_path_patch_reads_no_unchanged_trees() {
        let (objects, eng) = engine();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for d in 0..100 {
            for f in 0..100 {
                files.push((
                    format!("pkg{d:03}/src/f{f:03}.ts"),
                    format!("{d}-{f}").into_bytes(),
                ));
            }
        }
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_slice()))
            .collect();
        let one = tree_of(&eng, Eng::empty_tree_id(), &refs).await;
        let two = tree_of(&eng, one, &[("pkg042/src/f007.ts", b"changed")]).await;
        let opts = ExportOptions::default();
        let first = render_head(&eng, one, &[], &[], &Fixed, &opts)
            .await
            .expect("render");
        assert_eq!(first.entries.len(), 10_000);

        // A fresh engine has an empty tree cache, so every read reaches the store.
        let fresh = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
        objects.trees.store(0, Ordering::SeqCst);
        let basis = Basis {
            index: first,
            root: Some(one),
            staged: vec![rp("a.txt")],
        };
        let patched = update(&fresh, Some(basis), two, &[], &[], &Fixed, &opts)
            .await
            .expect("update");
        let reads = objects.trees.load(Ordering::SeqCst);
        // Roots, `pkg042` and `pkg042/src` of both trees; the other 99 packages stay unread.
        assert!(reads <= 6, "read {reads} trees");
        let hit = patched
            .entries
            .iter()
            .find(|e| e.path == rp("pkg042/src/f007.ts"))
            .expect("entry");
        assert_eq!(hit.oid, blob_oid(b"changed"));
        assert_eq!(patched.entries.len(), 10_000);
    }

    #[tokio::test]
    async fn unchanged_entries_keep_their_recorded_stat() {
        struct Later;
        impl StatSource for Later {
            fn stat(&self, _: &RepoPath, e: &Entry) -> FileStat {
                let t = StatTime {
                    secs: 1_800_000_000,
                    nsecs: 0,
                };
                FileStat {
                    ctime: t,
                    mtime: t,
                    size: e.size as u32,
                    ..FileStat::default()
                }
            }
        }
        let (_, eng) = engine();
        let one = tree_of(&eng, Eng::empty_tree_id(), &[("a", b"1"), ("b", b"2")]).await;
        let two = tree_of(&eng, one, &[("b", b"22")]).await;
        let opts = ExportOptions::default();
        let first = render_head(&eng, one, &[], &[], &Fixed, &opts)
            .await
            .expect("render");
        let kept = Basis {
            index: first.clone(),
            root: Some(one),
            staged: Vec::new(),
        };
        let patched = update(&eng, Some(kept), two, &[], &[], &Later, &opts)
            .await
            .expect("update");
        assert_eq!(
            patched.entries[0].mtime, first.entries[0].mtime,
            "a is unchanged"
        );
        assert_ne!(
            patched.entries[1].mtime, first.entries[1].mtime,
            "b changed"
        );
        // Without a base tree the old index only serves as the stat cache.
        let cache = Basis {
            index: first.clone(),
            root: None,
            staged: Vec::new(),
        };
        let whole = update(&eng, Some(cache), two, &[], &[], &Later, &opts)
            .await
            .expect("update");
        assert_eq!(whole.entries, patched.entries);
    }

    #[tokio::test]
    async fn conflicts_and_intent_to_add_are_written() {
        let (_, eng) = engine();
        let head = tree_of(
            &eng,
            Eng::empty_tree_id(),
            &[("c.txt", b"base"), ("k.txt", b"keep")],
        )
        .await;
        let mut rows = Vec::new();
        for (n, body) in [(1u8, &b"base"[..]), (2, b"ours"), (3, b"theirs")] {
            rows.push(StageRow {
                stage: n,
                ..row("c.txt", body)
            });
        }
        let ita = StageRow {
            intent_to_add: true,
            git: None,
            size: 0,
            ..row("i.txt", b"")
        };
        rows.push(ita);
        let idx = render_head(&eng, head, &rows, &[], &Fixed, &ExportOptions::default())
            .await
            .expect("render");
        let got: Vec<_> = idx
            .entries
            .iter()
            .map(|e| (e.path.clone(), e.flags.stage))
            .collect();
        assert_eq!(
            got,
            [
                (rp("c.txt"), Stage::Base),
                (rp("c.txt"), Stage::Ours),
                (rp("c.txt"), Stage::Theirs),
                (rp("i.txt"), Stage::Normal),
                (rp("k.txt"), Stage::Normal),
            ]
        );
        let i = &idx.entries[3];
        assert!(i.flags.intent_to_add);
        assert_eq!(i.oid, blob_oid(b""));
        assert!(idx.tree.is_none(), "git rebuilds the TREE extension itself");
    }

    #[tokio::test]
    async fn gitlinks_and_skip_worktree_are_kept() {
        let (_, eng) = engine();
        let sub = GitOid::from_bytes([7; 20]);
        let link = Change::Gitlink { commit: sub };
        let head = eng
            .apply(
                Eng::empty_tree_id(),
                &[(rp("sub"), link), (rp("out/f"), write(b"x").0)],
            )
            .await
            .expect("apply");
        let skip = |p: &RepoPath| p.as_bytes().starts_with(b"out/");
        let opts = ExportOptions {
            skip_worktree: Some(&skip),
            ..ExportOptions::default()
        };
        let idx = render_head(&eng, head, &[], &[], &Fixed, &opts)
            .await
            .expect("render");
        assert_eq!(idx.entries[0].path, rp("out/f"));
        assert!(idx.entries[0].flags.skip_worktree);
        assert_eq!(idx.entries[1].mode, FileMode::Gitlink);
        assert_eq!(idx.entries[1].oid, sub);
        assert!(!idx.entries[1].flags.skip_worktree);
    }

    #[test]
    fn base_marker_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_base(dir.path()), None);
        let root = Eng::empty_tree_id();
        write_base(dir.path(), root, &[row("a/b", b"x")]).expect("write");
        assert_eq!(read_base(dir.path()), Some((root, vec![rp("a/b")])));
    }
}
