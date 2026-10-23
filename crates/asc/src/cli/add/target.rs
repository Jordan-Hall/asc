//! The stage set a staging command works on (SPEC §9.5): set 0, or the temporary set a foreign
//! `GIT_INDEX_FILE` names. A set holds rows over the HEAD tree; this module reads the effective
//! state (HEAD overlaid with the rows), applies edits, and tells whoever mirrors the set: the
//! synthetic index sink for set 0, the index file itself for a temporary set.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use asc_core::git::ObjectKind;
use asc_core::id::{FileId, TreeId};
use asc_core::object::{Entry, Kind};
use asc_core::path::RepoPath;
use asc_engine::tree::Pathspec as TreeSpec;
use asc_git::index::{
    self, EntryFlags, FileMode, FileStat, IndexEntry, IndexFile, RenderOptions, Stage, StatSource,
    StatTime,
};
use asc_state::WorkspaceId;
use asc_state::state::stage::{self, Leaf, SetId, StageRow, StageSet};
use asc_state::state::{git_map, stage_sets};
use futures::TryStreamExt;

use super::spec::Spec;
use crate::cli::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use crate::ext;

/// A temporary stage set is dropped after this long (SPEC §9.5).
const TEMP_SET_LIFETIME_SECS: i64 = 24 * 60 * 60;

/// What a path is staged as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Staged {
    /// Not in the staged tree (never was, or a staged delete).
    Absent,
    Leaf {
        leaf: Leaf,
        ita: bool,
    },
    /// Conflict stages 1-3.
    Unmerged,
}

/// A tracked path: staged, with the HEAD entry it overlays.
#[derive(Clone, Debug)]
pub struct Tracked {
    pub staged: Staged,
    pub head: Option<Leaf>,
}

/// One change to the staged tree.
#[derive(Clone, Debug)]
pub enum Edit {
    /// Stage `leaf` (as intent-to-add when `ita`).
    Put {
        path: RepoPath,
        leaf: Leaf,
        ita: bool,
    },
    /// Remove the path from the staged tree.
    Drop(RepoPath),
}

impl Edit {
    pub fn path(&self) -> &RepoPath {
        match self {
            Self::Put { path, .. } | Self::Drop(path) => path,
        }
    }
}

pub struct Target {
    set: StageSet,
    /// The index file re-rendered after each change; only for a temporary set.
    file: Option<PathBuf>,
    root: TreeId,
    /// Every row of the set, by path.
    rows: BTreeMap<Vec<u8>, Vec<StageRow>>,
}

fn leaf_of(e: &Entry) -> Leaf {
    Leaf {
        kind: e.kind,
        file: FileId::from_bytes(e.id),
        size: e.size,
        git: e.git,
    }
}

fn row_state(rows: &[StageRow]) -> Staged {
    if rows.iter().any(|r| r.stage > 0) {
        return Staged::Unmerged;
    }
    match rows.iter().find(|r| r.stage == 0) {
        Some(StageRow {
            file: Some(file),
            git: Some(git),
            kind,
            size,
            intent_to_add,
            ..
        }) => Staged::Leaf {
            leaf: Leaf {
                kind: *kind,
                file: *file,
                size: *size,
                git: *git,
            },
            ita: *intent_to_add,
        },
        _ => Staged::Absent,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            s.to_ascii_lowercase()
        } else {
            s
        }
    };
    norm(a) == norm(b)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn tree_spec(prefixes: &[Vec<u8>]) -> Result<Option<TreeSpec>> {
    if prefixes.is_empty() {
        return Ok(None);
    }
    TreeSpec::parse(prefixes)
        .map(Some)
        .map_err(|e| CliError::Fatal(e.to_string()))
}

impl Target {
    /// Resolves the stage set of this invocation from `GIT_INDEX_FILE` (SPEC §9.5).
    pub async fn open(ctx: &Ctx) -> Result<Self> {
        let ws = ctx.ws()?;
        let root = match ws.head_commit().await? {
            Some(c) => ws.engine.get_changeset(c).await?.root,
            None => CliEngine::empty_tree_id(),
        };
        let foreign = ctx.env.index_file.as_ref().map(|f| {
            if f.is_absolute() {
                f.clone()
            } else {
                ctx.cwd.join(f)
            }
        });
        let foreign = foreign.filter(|f| !same_file(f, &ws.projection.git_dir().join("index")));
        let (set, file) = match foreign {
            None => (StageSet::Default, None),
            Some(file) => (StageSet::Temp(temp_set(ws, root, &file).await?), Some(file)),
        };
        let mut rows: BTreeMap<Vec<u8>, Vec<StageRow>> = BTreeMap::new();
        for r in stage::overlay(&ws.db, ws.workspace.ws.id, set).await? {
            rows.entry(r.path.clone()).or_default().push(r);
        }
        Ok(Self {
            set,
            file,
            root,
            rows,
        })
    }

    /// Whether this is the workspace staging (set 0).
    pub fn is_default(&self) -> bool {
        self.set == StageSet::Default
    }

    /// The HEAD entry at `path`, if it is a leaf.
    pub async fn head_leaf(&self, ws: &WsCtx, path: &RepoPath) -> Result<Option<Leaf>> {
        let entry = ws.engine.lookup(self.root, path).await?;
        Ok(entry.filter(|e| e.kind != Kind::Dir).map(|e| leaf_of(&e)))
    }

    /// What `path` is staged as: its rows, else HEAD.
    pub async fn staged(&self, ws: &WsCtx, path: &RepoPath) -> Result<Staged> {
        if let Some(rows) = self.rows.get(path.as_bytes()) {
            return Ok(row_state(rows));
        }
        Ok(match self.head_leaf(ws, path).await? {
            Some(leaf) => Staged::Leaf { leaf, ita: false },
            None => Staged::Absent,
        })
    }

    /// Calls `f` for every tracked path selected by `spec` (staged, not a staged delete):
    /// HEAD's entries in tree order, then the rows of paths HEAD lacks. HEAD subtrees outside
    /// the spec's prefixes are never read.
    pub async fn tracked(
        &self,
        ws: &WsCtx,
        spec: &Spec,
        mut f: impl FnMut(&RepoPath, Tracked) -> Result<()>,
    ) -> Result<()> {
        let tree_spec = tree_spec(&spec.prefixes())?;
        let mut seen: HashSet<&[u8]> = HashSet::new();
        let mut walk = ws.engine.walk(self.root, tree_spec.as_ref());
        while let Some((path, entry)) = walk.try_next().await? {
            if !spec.matches(path.as_bytes()) {
                continue;
            }
            let head = leaf_of(&entry);
            let staged = match self.rows.get_key_value(path.as_bytes()) {
                Some((key, rows)) => {
                    seen.insert(key);
                    row_state(rows)
                }
                None => Staged::Leaf {
                    leaf: head.clone(),
                    ita: false,
                },
            };
            if staged != Staged::Absent {
                f(
                    &path,
                    Tracked {
                        staged,
                        head: Some(head),
                    },
                )?;
            }
        }
        drop(walk);
        for (key, rows) in &self.rows {
            if seen.contains(key.as_slice()) || !spec.matches(key) {
                continue;
            }
            let Ok(path) = RepoPath::new(key.clone()) else {
                continue;
            };
            // A row over a path HEAD lacks, or over a directory that lost its files.
            let head = self.head_leaf(ws, &path).await?;
            let staged = row_state(rows);
            if staged != Staged::Absent {
                f(&path, Tracked { staged, head })?;
            }
        }
        Ok(())
    }

    /// Applies `edits` to the set in one step, then tells the mirror of the set.
    pub async fn apply(&mut self, ctx: &Ctx, edits: Vec<Edit>) -> Result<()> {
        let ws = ctx.ws()?;
        let (id, mut removes, mut upserts) = (ws.workspace.ws.id, Vec::new(), Vec::new());
        for edit in &edits {
            let key = edit.path().as_bytes().to_vec();
            let existing = self.rows.get(&key);
            let conflicted = existing.is_some_and(|r| r.iter().any(|x| x.stage > 0));
            let head = self.head_leaf(ws, edit.path()).await?;
            let row = match edit {
                Edit::Put { leaf, ita, .. } if *ita || head.as_ref() != Some(leaf) => {
                    let mut row = StageRow::from_leaf(key.clone(), Some(leaf));
                    row.intent_to_add = *ita;
                    Some(row)
                }
                Edit::Drop(_) if head.is_some() => Some(StageRow::from_leaf(key.clone(), None)),
                _ => None,
            };
            match row {
                Some(row) => {
                    if conflicted {
                        removes.push(key.clone());
                    }
                    upserts.push(row.clone());
                    self.rows.insert(key, vec![row]);
                }
                None if existing.is_some() => {
                    removes.push(key.clone());
                    self.rows.remove(&key);
                }
                None => {}
            }
        }
        for path in &removes {
            stage::remove(&ws.db, id, self.set, path).await?;
        }
        stage::upsert(&ws.db, id, self.set, &upserts).await?;
        let paths: Vec<RepoPath> = edits.iter().map(|e| e.path().clone()).collect();
        self.mirror(ctx, id, &paths).await
    }

    async fn mirror(&self, ctx: &Ctx, ws: WorkspaceId, paths: &[RepoPath]) -> Result<()> {
        match &self.file {
            None => ext::index_sink()
                .on_stage_changed(ws, paths)
                .await
                .map_err(|e| CliError::Fatal(e.to_string())),
            Some(file) => self.render(ctx, file).await,
        }
    }

    /// Writes the effective index of the set to `file` under git's lock protocol.
    async fn render(&self, ctx: &Ctx, file: &Path) -> Result<()> {
        let ws = ctx.ws()?;
        let mut entries: Vec<(RepoPath, Entry)> = Vec::new();
        let mut walk = ws.engine.walk(self.root, None);
        while let Some((path, entry)) = walk.try_next().await? {
            if !self.rows.contains_key(path.as_bytes()) {
                entries.push((path, entry));
            }
        }
        drop(walk);
        let mut conflicts = Vec::new();
        let mut intents: HashSet<&[u8]> = HashSet::new();
        for (key, rows) in &self.rows {
            let Ok(path) = RepoPath::new(key.clone()) else {
                continue;
            };
            for r in rows {
                let (Some(file_id), Some(git)) = (r.file, r.git) else {
                    continue;
                };
                if r.stage > 0 {
                    conflicts.push((path.clone(), r));
                    continue;
                }
                intents.extend(r.intent_to_add.then_some(key.as_slice()));
                entries.push((
                    path.clone(),
                    Entry {
                        name: path.file_name(),
                        kind: r.kind,
                        id: *file_id.as_bytes(),
                        size: r.size,
                        git,
                    },
                ));
            }
        }
        let mut out = index::render(
            entries.into_iter(),
            &ZeroStat,
            &RenderOptions {
                cache_tree: false,
                ..RenderOptions::default()
            },
        );
        for e in &mut out.entries {
            e.flags.intent_to_add = intents.contains(e.path.as_bytes());
        }
        for (path, r) in conflicts {
            let Some(mode) = mode_of(r.kind) else {
                continue;
            };
            out.entries.push(IndexEntry {
                path,
                mode,
                oid: r.git.unwrap_or_else(|| asc_core::git::blob_oid(b"")),
                size: r.size as u32,
                ctime: StatTime::default(),
                mtime: StatTime::default(),
                dev: 0,
                ino: 0,
                uid: 0,
                gid: 0,
                flags: EntryFlags {
                    stage: match r.stage {
                        1 => Stage::Base,
                        2 => Stage::Ours,
                        _ => Stage::Theirs,
                    },
                    ..EntryFlags::default()
                },
            });
        }
        out.entries
            .sort_by(|a, b| a.path.cmp(&b.path).then(a.flags.stage.cmp(&b.flags.stage)));
        index::write(file, &out).map_err(|e| CliError::Fatal(e.to_string()))
    }
}

fn mode_of(kind: Kind) -> Option<FileMode> {
    Some(match kind {
        Kind::File => FileMode::File,
        Kind::Exec => FileMode::Exec,
        Kind::Symlink => FileMode::Symlink,
        Kind::Gitlink => FileMode::Gitlink,
        Kind::Dir => return None,
    })
}

/// Entries carry no stat data, so git re-reads every file it compares: the rendered file is
/// not a stat cache for work-tree content asc did not read.
struct ZeroStat;

impl StatSource for ZeroStat {
    fn stat(&self, _: &RepoPath, entry: &Entry) -> FileStat {
        FileStat {
            size: entry.size as u32,
            ..FileStat::default()
        }
    }
}

/// The registered set of `file`: reused while the file exists and is fresh, else started
/// again; an existing unregistered file is imported (SPEC §9.5).
async fn temp_set(ws: &WsCtx, root: TreeId, file: &Path) -> Result<SetId> {
    let key = file.to_string_lossy().into_owned();
    let id = ws.workspace.ws.id;
    if let Some(info) = stage_sets::lookup_by_path(&ws.db, &key).await? {
        if info.ws != id {
            return Err(CliError::Fatal(format!(
                "asc: '{key}' is the index of another workspace"
            )));
        }
        if file.exists() && now_secs() - info.created < TEMP_SET_LIFETIME_SECS {
            return Ok(info.set);
        }
        stage_sets::drop_set(&ws.db, id, info.set).await?;
    }
    if !file.is_file() {
        return Ok(stage_sets::register(&ws.db, id, &key).await?);
    }
    let rows = import_rows(ws, root, file).await?;
    Ok(stage_sets::import(&ws.db, id, &key, &rows).await?)
}

/// The rows that make HEAD's tree the index in `file`.
async fn import_rows(ws: &WsCtx, root: TreeId, file: &Path) -> Result<Vec<StageRow>> {
    let index =
        IndexFile::read(file).map_err(|e| CliError::Fatal(format!("index file corrupt: {e}")))?;
    let mut head: HashMap<Vec<u8>, Leaf> = HashMap::new();
    let mut walk = ws.engine.walk(root, None);
    while let Some((path, entry)) = walk.try_next().await? {
        head.insert(path.as_bytes().to_vec(), leaf_of(&entry));
    }
    drop(walk);
    let mut rows = Vec::new();
    let mut present: HashSet<&[u8]> = HashSet::new();
    for e in &index.entries {
        let (kind, file_id) = match e.mode {
            FileMode::Gitlink => (Kind::Gitlink, FileId::from_bytes([0; 32])),
            mode => {
                let asc = git_map::git_to_asc(&ws.db, ws.repo.key, e.oid, ObjectKind::Blob)
                    .await?
                    .ok_or_else(|| CliError::Fatal(format!("unable to read {}", e.oid)))?;
                let kind = match mode {
                    FileMode::Exec => Kind::Exec,
                    FileMode::Symlink => Kind::Symlink,
                    _ => Kind::File,
                };
                (kind, FileId::from_bytes(asc))
            }
        };
        let stage = match e.flags.stage {
            Stage::Normal => 0,
            Stage::Base => 1,
            Stage::Ours => 2,
            Stage::Theirs => 3,
        };
        present.insert(e.path.as_bytes());
        let same = head
            .get(e.path.as_bytes())
            .is_some_and(|h| h.kind == kind && h.git == e.oid);
        if stage == 0 && !e.flags.intent_to_add && same {
            continue;
        }
        rows.push(StageRow {
            path: e.path.as_bytes().to_vec(),
            stage,
            kind,
            file: Some(file_id),
            size: u64::from(e.size),
            git: Some(e.oid),
            intent_to_add: e.flags.intent_to_add,
        });
    }
    rows.extend(
        head.keys()
            .filter(|p| !present.contains(p.as_slice()))
            .map(|p| StageRow::from_leaf(p.clone(), None)),
    );
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, file: Option<u8>) -> StageRow {
        StageRow {
            path: path.as_bytes().to_vec(),
            stage: 0,
            kind: Kind::File,
            file: file.map(|b| FileId::from_bytes([b; 32])),
            size: 1,
            git: file.map(|_| asc_core::git::blob_oid(b"x")),
            intent_to_add: false,
        }
    }

    #[test]
    fn row_states() {
        assert_eq!(row_state(&[row("a", None)]), Staged::Absent);
        assert!(matches!(
            row_state(&[row("a", Some(1))]),
            Staged::Leaf { ita: false, .. }
        ));
        let mut conflict = row("a", Some(1));
        conflict.stage = 2;
        assert_eq!(row_state(&[conflict]), Staged::Unmerged);
    }

    #[test]
    fn foreign_index_comparison() {
        assert!(same_file(
            Path::new("/g/index"),
            Path::new("/g//index".replace("//", "/").as_str())
        ));
        assert!(!same_file(Path::new("/g/index"), Path::new("/g/other")));
    }
}
