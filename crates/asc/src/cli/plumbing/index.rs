//! The index as git sees it: HEAD's tree overlaid with the rows of the stage set the invocation
//! works on (`GIT_INDEX_FILE`, SPEC §9.5), streamed in git's path order.

use std::collections::{HashMap, VecDeque};
use std::iter::Peekable;

use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{Entry, Kind};
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_state::state::stage_sets;

use futures::TryStreamExt as _;

use super::objects::git_order;
use crate::cli::add::spec::Spec;
use crate::cli::add::target::{Edit, Staged, Target};
use crate::cli::common::{CliEngine, Ctx, Result, WsCtx};

/// One index entry.
#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub path: Vec<u8>,
    pub kind: Kind,
    pub oid: GitOid,
    pub stage: u8,
    pub size: u64,
    pub file: Option<FileId>,
    /// An intent-to-add entry.
    pub ita: bool,
}

impl IndexEntry {
    /// The git file mode.
    pub fn mode(&self) -> u32 {
        mode_of(self.kind)
    }
}

/// The git file mode of a tree entry kind.
pub fn mode_of(kind: Kind) -> u32 {
    match kind {
        Kind::File => 0o100_644,
        Kind::Exec => 0o100_755,
        Kind::Symlink => 0o120_000,
        Kind::Dir => 0o040_000,
        Kind::Gitlink => 0o160_000,
    }
}

/// The stage set this invocation works on, with HEAD's tree.
pub struct Index {
    pub target: Target,
    pub set: StageSet,
    /// HEAD's tree: the stage rows overlay it.
    pub root: TreeId,
    /// A temporary set whose index file does not exist yet. git starts such an index empty, not
    /// at HEAD, so reads see nothing until [`Index::seed`] makes the rows say so.
    fresh: bool,
}

impl Index {
    pub async fn open(ctx: &Ctx) -> Result<Self> {
        let ws = ctx.ws()?;
        let target = Target::open(ctx).await?;
        let root = match ws.head_commit().await? {
            Some(c) => ws.engine.get_changeset(c).await?.root,
            None => CliEngine::empty_tree_id(),
        };
        let mut fresh = false;
        let set = if target.is_default() {
            StageSet::Default
        } else {
            // `Target::open` registered the set under the absolute path of `GIT_INDEX_FILE`.
            let file = ctx.env.index_file.as_ref().map(|f| if f.is_absolute() { f.clone() } else { ctx.cwd.join(f) });
            fresh = file.as_ref().is_some_and(|f| !f.exists());
            let key = file.map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            match stage_sets::lookup_by_path(&ws.db, &key).await? {
                Some(info) => StageSet::Temp(info.set),
                None => StageSet::Default,
            }
        };
        let mut index = Self { target, set, root, fresh };
        // A set that has rows already took its shape from a command that rendered the file.
        index.fresh = index.fresh && index.rows(ws).await?.is_empty();
        Ok(index)
    }

    /// The tree the rows overlay when reading: nothing for an index that does not exist yet.
    pub fn base(&self) -> TreeId {
        if self.fresh { CliEngine::empty_tree_id() } else { self.root }
    }

    /// Applies `edits` whose staged content the work tree may not hold (`--cacheinfo`,
    /// `read-tree`). The temporary set's file needs no stat. Set 0 is mirrored with every
    /// entry stat-dirty: `Target::apply` reports its edits as paths whose files equal the staged
    /// content, which the synthetic index (SPEC §9.4) then records with the file's real stat, so
    /// a later `status` would trust the staged oid for a file that differs.
    pub async fn apply_untrusted(&mut self, ctx: &Ctx, edits: Vec<Edit>) -> Result<()> {
        if !self.target.is_default() {
            return self.target.apply(ctx, edits).await;
        }
        let ws = ctx.ws()?;
        let id = ws.workspace.ws.id;
        let mut existing: HashMap<Vec<u8>, Vec<StageRow>> = HashMap::new();
        for row in self.rows(ws).await? {
            existing.entry(row.path.clone()).or_default().push(row);
        }
        let (mut removes, mut upserts) = (Vec::new(), Vec::new());
        for edit in &edits {
            let key = edit.path().as_bytes().to_vec();
            let rows = existing.get(&key);
            let conflicted = rows.is_some_and(|r| r.iter().any(|x| x.stage > 0));
            let head = self.target.head_leaf(ws, edit.path()).await?;
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
                    existing.insert(key, vec![row]);
                }
                None if rows.is_some() => {
                    removes.push(key.clone());
                    existing.remove(&key);
                }
                None => {}
            }
        }
        for path in &removes {
            stage::remove(&ws.db, id, self.set, path).await?;
        }
        stage::upsert(&ws.db, id, self.set, &upserts).await?;
        // The target's copy of the rows is stale now.
        self.target = Target::open(ctx).await?;
        crate::ext::index_sink()
            .on_stage_changed(id, &[])
            .await
            .map_err(|e| crate::cli::common::CliError::Fatal(e.to_string()))
    }

    /// What `path` is staged as; nothing is, in an index that does not exist yet.
    pub async fn staged(&self, ws: &WsCtx, path: &asc_core::path::RepoPath) -> Result<Staged> {
        if self.fresh {
            return Ok(Staged::Absent);
        }
        self.target.staged(ws, path).await
    }

    /// Before a command changes the index: a missing index file means an empty index, so the
    /// set drops every path of HEAD and the file is written (SPEC §9.5, "a missing file starts
    /// an empty set (git semantics)").
    pub async fn seed(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.fresh {
            return Ok(());
        }
        let ws = ctx.ws()?;
        let mut drops = Vec::new();
        let mut walk = ws.engine.walk(self.root, None);
        while let Some((path, _)) = walk.try_next().await? {
            drops.push(Edit::Drop(path));
        }
        drop(walk);
        self.target.apply(ctx, drops).await?;
        self.fresh = false;
        Ok(())
    }

    /// Every row of the set, by path then stage.
    pub async fn rows(&self, ws: &WsCtx) -> Result<Vec<StageRow>> {
        let mut rows = stage::overlay(&ws.db, ws.workspace.ws.id, self.set).await?;
        rows.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
        Ok(rows)
    }

    /// The entries under `spec` in path order.
    pub async fn entries<'a>(&self, ws: &'a WsCtx, spec: Option<&'a Spec>) -> Result<Entries<'a>> {
        let rows = self.rows(ws).await?;
        let mut groups: Vec<Vec<StageRow>> = Vec::new();
        for row in rows {
            match groups.last_mut() {
                Some(last) if last[0].path == row.path => last.push(row),
                _ => groups.push(vec![row]),
            }
        }
        let mut walk = Entries {
            ws,
            spec,
            frames: Vec::new(),
            head: None,
            rows: groups.into_iter().peekable(),
            pending: VecDeque::new(),
        };
        if !self.fresh {
            walk.enter(Vec::new(), self.root).await?;
        }
        Ok(walk)
    }
}

struct Frame {
    prefix: Vec<u8>,
    entries: Vec<Entry>,
    at: usize,
}

/// A streaming merge of HEAD's tree and the stage rows.
pub struct Entries<'a> {
    ws: &'a WsCtx,
    spec: Option<&'a Spec>,
    frames: Vec<Frame>,
    head: Option<(Vec<u8>, Entry)>,
    rows: Peekable<std::vec::IntoIter<Vec<StageRow>>>,
    pending: VecDeque<IndexEntry>,
}

impl Entries<'_> {
    async fn enter(&mut self, prefix: Vec<u8>, id: TreeId) -> Result<()> {
        let tree = self.ws.engine.get_tree(id).await?;
        let entries: Vec<Entry> = git_order(&tree).into_iter().cloned().collect();
        self.frames.push(Frame { prefix, entries, at: 0 });
        Ok(())
    }

    /// The next leaf of HEAD's tree under the spec.
    async fn next_head(&mut self) -> Result<Option<(Vec<u8>, Entry)>> {
        loop {
            let Some(frame) = self.frames.last_mut() else { return Ok(None) };
            let Some(entry) = frame.entries.get(frame.at).cloned() else {
                self.frames.pop();
                continue;
            };
            frame.at += 1;
            let mut path = frame.prefix.clone();
            path.extend_from_slice(entry.name.as_bytes());
            if entry.kind == Kind::Dir {
                if self.spec.is_none_or(|s| s.may_contain_dir(&path)) {
                    path.push(b'/');
                    self.enter(path, TreeId::from_bytes(entry.id)).await?;
                }
                continue;
            }
            if self.spec.is_none_or(|s| s.matches(&path)) {
                return Ok(Some((path, entry)));
            }
        }
    }

    fn from_head(path: Vec<u8>, entry: &Entry) -> IndexEntry {
        IndexEntry {
            path,
            kind: entry.kind,
            oid: entry.git,
            stage: 0,
            size: entry.size,
            file: (entry.kind != Kind::Gitlink).then(|| FileId::from_bytes(entry.id)),
            ita: false,
        }
    }

    fn from_rows(&self, group: Vec<StageRow>) -> Vec<IndexEntry> {
        if self.spec.is_some_and(|s| !s.matches(&group[0].path)) {
            return Vec::new();
        }
        group
            .into_iter()
            .filter_map(|r| {
                Some(IndexEntry {
                    path: r.path,
                    kind: r.kind,
                    oid: r.git?,
                    stage: r.stage,
                    size: r.size,
                    file: r.file,
                    ita: r.intent_to_add,
                })
            })
            .collect()
    }

    /// The next entry in path order, stages of one path together.
    pub async fn next(&mut self) -> Result<Option<IndexEntry>> {
        loop {
            if let Some(entry) = self.pending.pop_front() {
                return Ok(Some(entry));
            }
            if self.head.is_none() {
                self.head = self.next_head().await?;
            }
            let row_path = self.rows.peek().map(|g| g[0].path.clone());
            let order = match (&self.head, &row_path) {
                (None, None) => return Ok(None),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((h, _)), Some(r)) => h.as_slice().cmp(r.as_slice()),
            };
            match order {
                std::cmp::Ordering::Less => {
                    if let Some((path, entry)) = self.head.take() {
                        return Ok(Some(Self::from_head(path, &entry)));
                    }
                }
                std::cmp::Ordering::Greater => {
                    if let Some(group) = self.rows.next() {
                        self.pending.extend(self.from_rows(group));
                    }
                }
                std::cmp::Ordering::Equal => {
                    self.head = None;
                    if let Some(group) = self.rows.next() {
                        self.pending.extend(self.from_rows(group));
                    }
                }
            }
        }
    }
}
