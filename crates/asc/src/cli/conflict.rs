//! What `merge`, `cherry-pick` and `revert` share (SPEC §4.4): the engine's merge options (conflict
//! style, `-X`, merge attributes, configured drivers), the safety checks before the work tree is
//! touched, materialising a merge result (files with markers, stages 1/2/3 in the staging rows),
//! git's `CONFLICT (...)` messages, and writing the commit a replay produces.
//!
//! The engine merges trees; nothing here reads the work tree except to refuse overwriting local
//! work, and every work tree path comes from `SafeRoot` (CARRY: work tree access).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use bstr::ByteSlice;
use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{Gen, index_commit_git};
use asc_engine::merge::{
    ConflictKind, ConflictStyle, DriverRequest, DriverResult, Favor, Labels, MergeAttr,
    MergeAttributes, MergeDrivers, MergeOptions, MergeOutcome,
};
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_state::state::tables::cs_gen;
use asc_state::state::{git_map, workspaces};
use asc_store::refs::{Expect, RefTxn, RefUpdate};
use asc_wc::SafeRoot;
use asc_wc::status;
use gix_attributes::glob::pattern::Case;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_attributes::{Search, StateRef};

use super::commit::{ident, stagebuild};
use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::restore::{WcParts, actor, sync_worktree};
use crate::ext::{self, OpId};

/// Largest merged file a driver may hand back.
const MAX_DRIVER_OUTPUT: u64 = 128 * 1024 * 1024;

pub fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// Which command is merging, for the wording of its refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Merge,
    /// A fast-forward `merge`: staged changes elsewhere are fine.
    FastForward,
    Pick,
    Revert,
}

impl Op {
    fn command(self) -> &'static str {
        match self {
            Self::Merge | Self::FastForward => "merge",
            Self::Pick => "cherry-pick",
            Self::Revert => "revert",
        }
    }

    /// The last lines and the exit code of a refusal.
    fn failure(self) -> (&'static str, u8) {
        match self {
            Self::Merge => ("Merge with strategy ort failed.", 2),
            Self::FastForward => ("", 1),
            Self::Pick => ("fatal: cherry-pick failed", 128),
            Self::Revert => ("fatal: revert failed", 128),
        }
    }
}

/// The three trees of a merge.
#[derive(Clone, Copy, Debug)]
pub struct Sides {
    pub base: TreeId,
    pub ours: TreeId,
    pub theirs: TreeId,
}

/// Whether `advice.<key>` allows the hint (on unless set to false).
pub fn advice(ctx: &Ctx, key: &str) -> bool {
    ctx.config.bool(&format!("advice.{key}")) != Some(false)
}

/// `-X ours|theirs`; other strategy options are refused rather than ignored.
pub fn favor_of(xopts: &[String]) -> Result<Favor> {
    let mut favor = Favor::None;
    for x in xopts {
        favor = match x.as_str() {
            "ours" => Favor::Ours,
            "theirs" => Favor::Theirs,
            other => return Err(fatal(format!("asc: strategy option '{other}' is not supported (only ours and theirs)"))),
        };
    }
    Ok(favor)
}

// ---------------------------------------------------------------- merge attributes and drivers

/// Merge attributes looked up before the merge for the paths both sides changed.
struct PreparedAttrs(HashMap<RepoPath, MergeAttr>);

impl MergeAttributes for PreparedAttrs {
    fn merge_attr(&self, path: &RepoPath) -> MergeAttr {
        self.0.get(path).cloned().unwrap_or_default()
    }
}

/// Reads the `merge` attribute of each path from `.ascattributes` / `.gitattributes` (root to leaf,
/// the first of the two per directory), `<common>/info/attributes` and `core.attributesFile`.
fn prepare_attrs(ctx: &Ctx, ws: &WsCtx, paths: &[RepoPath]) -> HashMap<RepoPath, MergeAttr> {
    let mut out = HashMap::new();
    if paths.is_empty() {
        return out;
    }
    let root = ws.projection.toplevel().to_owned();
    let gate = SafeRoot::new(&root);
    let mut collection = MetadataCollection::default();
    let mut buf = Vec::new();
    let Ok(base) = Search::new_globals(ctx.config.path("core.attributesFile"), &mut buf, &mut collection) else {
        return out;
    };
    let info = ws.projection.common_dir().join("info").join("attributes");
    let info = info.is_file().then_some(info);
    let mut dirs: HashMap<Vec<u8>, Rc<Search>> = HashMap::new();
    let mut outcome = Outcome::default();
    for path in paths {
        let bytes = path.as_bytes();
        let dir = bytes.iter().rposition(|&c| c == b'/').map_or(&[][..], |i| &bytes[..i]);
        // Directories root first, so each inherits its parent's patterns.
        let mut chain: Vec<&[u8]> = vec![&[][..]];
        for (i, &c) in dir.iter().enumerate() {
            if c == b'/' {
                chain.push(&dir[..i]);
            }
        }
        if !dir.is_empty() {
            chain.push(dir);
        }
        let mut search = base.clone();
        for d in chain {
            if let Some(found) = dirs.get(d) {
                search = (**found).clone();
                continue;
            }
            let regular = |name: &str| -> Option<PathBuf> {
                let rel = if d.is_empty() { name.as_bytes().to_vec() } else { [d, b"/", name.as_bytes()].concat() };
                let abs = gate.resolve(&RepoPath::new(rel).ok()?)?;
                std::fs::symlink_metadata(&abs).is_ok_and(|m| m.is_file()).then_some(abs)
            };
            if info.is_some() && !d.is_empty() {
                search.pop_pattern_list();
            }
            if let Some(file) = regular(".ascattributes").or_else(|| regular(".gitattributes")) {
                let _ = search.add_patterns_file(file, false, Some(&root), &mut buf, &mut collection, true);
            }
            if let Some(i) = &info {
                let _ = search.add_patterns_file(i.clone(), false, None, &mut buf, &mut collection, true);
            }
            dirs.insert(d.to_vec(), Rc::new(search.clone()));
        }
        outcome.initialize_with_selection(&collection, ["merge"]);
        search.pattern_matching_relative_path(bytes.as_bstr(), Case::Sensitive, Some(false), &mut outcome);
        let state = outcome.iter_selected().find(|m| m.assignment.name.as_str() == "merge").map(|m| m.assignment.state);
        let attr = match state {
            Some(StateRef::Set) => MergeAttr::Text,
            Some(StateRef::Unset) => MergeAttr::Unset,
            Some(StateRef::Value(v)) => match v.as_bstr().to_str_lossy().as_ref() {
                "text" => MergeAttr::Text,
                "binary" => MergeAttr::Binary,
                "union" => MergeAttr::Union,
                "ours" => MergeAttr::Ours,
                other => MergeAttr::Driver(other.to_owned()),
            },
            _ => MergeAttr::Unspecified,
        };
        if attr != MergeAttr::Unspecified {
            out.insert(path.clone(), attr);
        }
    }
    out
}

/// Configured `merge.<name>.driver` commands, run through the shell as git does (SPEC §9.4.1).
struct ShellDrivers {
    shell: PathBuf,
    commands: HashMap<String, String>,
    tmp: PathBuf,
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[async_trait]
impl MergeDrivers for ShellDrivers {
    async fn run(
        &self,
        driver: &str,
        req: &DriverRequest<'_>,
    ) -> asc_engine::merge::Result<Option<DriverResult>> {
        let Some(template) = self.commands.get(driver) else { return Ok(None) };
        let failed = |message: String| asc_engine::merge::Error::Driver { driver: driver.to_owned(), message };
        let dir = self.tmp.join(format!("merge-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).map_err(|e| failed(e.to_string()))?;
        let guard = DirGuard(dir.clone());
        let (o, a, b) = (dir.join("O"), dir.join("A"), dir.join("B"));
        for (file, bytes) in [(&o, req.base), (&a, req.ours), (&b, req.theirs)] {
            std::fs::write(file, bytes).map_err(|e| failed(e.to_string()))?;
        }
        let path = String::from_utf8_lossy(req.path.as_bytes()).into_owned();
        let arg = |p: &PathBuf| sh_quote(&p.to_string_lossy());
        let mut command = String::new();
        let mut chars = template.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                command.push(c);
                continue;
            }
            match chars.next() {
                Some('O') => command.push_str(&arg(&o)),
                Some('A') => command.push_str(&arg(&a)),
                Some('B') => command.push_str(&arg(&b)),
                Some('L') => command.push_str(&req.marker_size.to_string()),
                Some('P') => command.push_str(&sh_quote(&path)),
                Some('S') => command.push_str(&sh_quote(&req.labels.base)),
                Some('X') => command.push_str(&sh_quote(&req.labels.ours)),
                Some('Y') => command.push_str(&sh_quote(&req.labels.theirs)),
                Some('%') => command.push('%'),
                Some(other) => {
                    command.push('%');
                    command.push(other);
                }
                None => command.push('%'),
            }
        }
        let mut child = crate::child::command(&self.shell).map_err(|m| failed(format!("{m:?}")))?;
        child.arg("-c").arg(&command).current_dir(&dir);
        child.stdin(std::process::Stdio::null());
        let status = tokio::task::spawn_blocking(move || child.status())
            .await
            .map_err(|e| failed(e.to_string()))?
            .map_err(|e| failed(e.to_string()))?;
        let code = status.code().unwrap_or(255);
        if !(0..128).contains(&code) {
            return Err(failed(format!("exited with status {code}")));
        }
        let meta = std::fs::metadata(&a).map_err(|e| failed(e.to_string()))?;
        if meta.len() > MAX_DRIVER_OUTPUT {
            return Err(failed("result is too large".into()));
        }
        let content = std::fs::read(&a).map_err(|e| failed(e.to_string()))?;
        drop(guard);
        Ok(Some(DriverResult { content, clean: code == 0 }))
    }
}

struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The paths both sides changed relative to the base, and (for the messages) whether the two
/// results differ: `Auto-merging` is printed for those that do.
async fn both_changed(ws: &WsCtx, sides: &Sides) -> Result<(Vec<RepoPath>, BTreeSet<RepoPath>)> {
    let ours: BTreeMap<RepoPath, Change> = ws.engine.leafdiff(sides.base, sides.ours, None).await?.into_iter().collect();
    let theirs = ws.engine.leafdiff(sides.base, sides.theirs, None).await?;
    let (mut both, mut differ) = (Vec::new(), BTreeSet::new());
    for (path, t) in theirs {
        let Some(o) = ours.get(&path) else { continue };
        both.push(path.clone());
        let same = match (o, &t) {
            (Change::Write { kind: k1, file: f1, .. }, Change::Write { kind: k2, file: f2, .. }) => k1 == k2 && f1 == f2,
            (Change::Delete, Change::Delete) => true,
            (Change::Gitlink { commit: a }, Change::Gitlink { commit: b }) => a == b,
            _ => false,
        };
        if !same && matches!((o, &t), (Change::Write { .. }, Change::Write { .. })) {
            differ.insert(path);
        }
    }
    Ok((both, differ))
}

/// The merge options for one run: conflict style, favour, labels, attributes and drivers.
pub async fn merge_options(
    ctx: &Ctx,
    ws: &WsCtx,
    sides: &Sides,
    favor: Favor,
    labels: Labels,
) -> Result<(MergeOptions, BTreeSet<RepoPath>)> {
    let style = match ctx.config.get("merge.conflictStyle") {
        None => ConflictStyle::default(),
        Some(s) => ConflictStyle::parse(&s).ok_or_else(|| fatal(format!("unknown style '{s}' given for 'merge.conflictstyle'")))?,
    };
    let (both, differ) = both_changed(ws, sides).await?;
    let attrs = prepare_attrs(ctx, ws, &both);
    let mut commands = HashMap::new();
    for attr in attrs.values() {
        if let MergeAttr::Driver(name) = attr
            && let Some(cmd) = ctx.config.get(&format!("merge.{name}.driver"))
        {
            commands.insert(name.clone(), cmd);
        }
    }
    let drivers: Arc<dyn MergeDrivers> = if commands.is_empty() {
        Arc::new(asc_engine::merge::NoDrivers)
    } else {
        let _ = ctx.real_git();
        Arc::new(ShellDrivers { shell: ctx.shell(), commands, tmp: ws.home.tmp().to_owned() })
    };
    let opts = MergeOptions {
        style,
        favor,
        labels,
        attrs: Arc::new(PreparedAttrs(attrs)),
        drivers,
        ..MergeOptions::default()
    };
    Ok((opts, differ))
}

// ---------------------------------------------------------------- messages

/// git's output for the paths a merge touched specially, sorted by path: `Auto-merging` for a
/// file both sides changed, then the `CONFLICT (...)` line. `ours` and `theirs` are the names
/// printed in the modify/delete line.
pub fn messages(outcome: &MergeOutcome, differ: &BTreeSet<RepoPath>, ours: &str, theirs: &str) -> Vec<String> {
    let mut by_path: BTreeMap<&RepoPath, Vec<String>> = BTreeMap::new();
    for p in differ {
        by_path.entry(p).or_default().push(format!("Auto-merging {}", p));
    }
    for c in &outcome.conflicts {
        let p = &c.path;
        let lines = by_path.entry(&c.path).or_default();
        match c.kind {
            ConflictKind::Content | ConflictKind::Binary | ConflictKind::AddAdd => {
                if c.kind == ConflictKind::Binary {
                    lines.insert(0, format!("warning: Cannot merge binary files: {p} ({ours} vs. {theirs})"));
                }
                if !lines.iter().any(|l| l.starts_with("Auto-merging")) {
                    lines.push(format!("Auto-merging {p}"));
                }
                let kind = if c.stages.base.is_none() { "add/add" } else { "content" };
                lines.push(format!("CONFLICT ({kind}): Merge conflict in {p}"));
            }
            ConflictKind::ModifyDelete => {
                let (deleted, modified) = if c.stages.ours.is_none() { (ours, theirs) } else { (theirs, ours) };
                lines.push(format!(
                    "CONFLICT (modify/delete): {p} deleted in {deleted} and modified in {modified}.  Version {modified} of {p} left in tree."
                ));
            }
            ConflictKind::FileDir | ConflictKind::CaseFold => {
                lines.push(format!("CONFLICT (file/directory): {p}"));
            }
        }
    }
    by_path.into_values().flatten().collect()
}

/// Prints the merge messages to stdout.
pub fn print_messages(lines: &[String]) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    for l in lines {
        let _ = writeln!(out, "{l}");
    }
}

/// The `# Conflicts:` block git appends to `MERGE_MSG`, with the comment character.
pub fn conflicts_block(outcome: &MergeOutcome, comment: u8) -> Vec<u8> {
    let c = comment as char;
    let mut paths: Vec<&RepoPath> = outcome.conflicts.iter().map(|x| &x.path).collect();
    paths.sort();
    paths.dedup();
    let mut out = format!("\n{c} Conflicts:\n").into_bytes();
    for p in paths {
        out.extend_from_slice(format!("{c}\t").as_bytes());
        out.extend_from_slice(p.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Refuses a conflict kind asc cannot materialise the way git does.
pub fn refuse_unsupported(outcome: &MergeOutcome) -> Result<()> {
    if let Some(c) = outcome.conflicts.iter().find(|c| matches!(c.kind, ConflictKind::FileDir | ConflictKind::CaseFold)) {
        return Err(fatal(format!(
            "asc: a file/directory or case conflict at '{}' is not supported yet; nothing was changed",
            c.path
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------- safety checks

pub fn leaf_row(path: &RepoPath, stage_no: u8, e: &Entry) -> StageRow {
    StageRow {
        path: path.as_bytes().to_vec(),
        stage: stage_no,
        kind: e.kind,
        file: Some(FileId::from_bytes(e.id)),
        size: e.size,
        git: Some(e.git),
        intent_to_add: false,
    }
}

fn paths_text(prefix: &str, paths: &BTreeSet<RepoPath>) -> String {
    paths.iter().map(|p| format!("{prefix}{}\n", p)).collect()
}

/// Refuses to merge over local work (git's `unpack_trees` and `ort` checks), before anything is
/// written. `current` is the tree the merge starts from (the index tree when earlier no-commit
/// replays are staged); `allow_staged` skips the staged-changes refusal for that case.
pub async fn precheck(
    ctx: &Ctx,
    ws: &WsCtx,
    op: Op,
    current: TreeId,
    outcome: &MergeOutcome,
    allow_staged: bool,
) -> Result<()> {
    let cmd = op.command();
    let (tail, exit_code) = op.failure();
    let tail = if tail.is_empty() { String::new() } else { format!("{tail}\n") };
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let staged: BTreeSet<RepoPath> = rows.iter().filter_map(|r| RepoPath::new(r.path.clone()).ok()).collect();
    if !allow_staged && op != Op::FastForward && !staged.is_empty() {
        let text = match op {
            Op::Merge => format!(
                "error: Your local changes to the following files would be overwritten by merge:\n{}{tail}",
                paths_text("  ", &staged)
            ),
            _ => {
                let hint = if advice(ctx, "commitBeforeMerge") {
                    "hint: commit your changes or stash them to proceed.\n"
                } else {
                    ""
                };
                format!("error: your local changes would be overwritten by {cmd}.\n{hint}{tail}")
            }
        };
        return Err(CliError::Raw(text.trim_end().to_owned(), exit_code));
    }
    let mut touched: BTreeSet<RepoPath> = ws
        .engine
        .leafdiff(current, outcome.root, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    touched.extend(outcome.conflicts.iter().map(|c| c.path.clone()));
    if touched.is_empty() {
        return Ok(());
    }
    let fresh = workspaces::get(&ws.db, ws.workspace.ws.id)
        .await?
        .ok_or_else(|| fatal("asc: this workspace is not registered in state.db"))?;
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &fresh).await.map_err(fatal)?;
    let mut dirty: BTreeSet<RepoPath> = st.unstaged.iter().map(|c| c.path.clone()).collect();
    if op == Op::FastForward {
        dirty.extend(staged.iter().cloned());
    }
    let overlap: BTreeSet<RepoPath> = touched.intersection(&dirty).cloned().collect();
    if !overlap.is_empty() {
        let text = format!(
            "error: Your local changes to the following files would be overwritten by merge:\n{}Please commit your changes or stash them before you merge.\nAborting\n{}",
            paths_text("\t", &overlap),
            tail
        );
        return Err(CliError::Raw(text.trim_end().to_owned(), exit_code));
    }
    let gate = SafeRoot::new(ws.projection.toplevel());
    let mut untracked = BTreeSet::new();
    for p in &touched {
        if ws.engine.lookup(current, p).await?.is_some() || ws.engine.lookup(outcome.root, p).await?.is_none() {
            continue;
        }
        if gate.resolve(p).is_some_and(|abs| std::fs::symlink_metadata(abs).is_ok()) {
            untracked.insert(p.clone());
        }
    }
    if !untracked.is_empty() {
        let text = format!(
            "error: The following untracked working tree files would be overwritten by merge:\n{}Please move or remove them before you merge.\nAborting\n{}",
            paths_text("\t", &untracked),
            tail
        );
        return Err(CliError::Raw(text.trim_end().to_owned(), exit_code));
    }
    Ok(())
}

// ---------------------------------------------------------------- materialising

/// Registers the git ids of `root`'s trees and of the files `current -> root` changed, so
/// `ls-tree`, export and hooks find them.
pub async fn index_result(ws: &WsCtx, current: TreeId, root: TreeId) -> Result<()> {
    let changes = ws.engine.leafdiff(current, root, None).await?;
    stagebuild::index_spine(ws, root, &changes).await
}

/// Writes the merge result to the work tree and the staging rows: the files at every changed
/// path (conflict markers included), stage 0 rows for clean changes against HEAD, and stages
/// 1/2/3 for conflicts. HEAD does not move.
pub async fn materialise(
    ctx: &Ctx,
    ws: &WsCtx,
    head_root: TreeId,
    current: TreeId,
    outcome: &MergeOutcome,
) -> Result<()> {
    let mut touched: BTreeSet<RepoPath> = ws
        .engine
        .leafdiff(current, outcome.root, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    touched.extend(outcome.conflicts.iter().map(|c| c.path.clone()));
    index_result(ws, current, outcome.root).await?;
    // Rows relative to HEAD: everything the result differs from HEAD at, conflicts as stages.
    let conflicted: BTreeSet<&RepoPath> = outcome.conflicts.iter().map(|c| &c.path).collect();
    let mut rows = Vec::new();
    let mut all: BTreeSet<RepoPath> = ws.engine.leafdiff(head_root, outcome.root, None).await?.into_iter().map(|(p, _)| p).collect();
    all.extend(outcome.conflicts.iter().map(|c| c.path.clone()));
    for path in &all {
        if conflicted.contains(path) {
            continue;
        }
        let head = ws.engine.lookup(head_root, path).await?;
        let merged = ws.engine.lookup(outcome.root, path).await?;
        match (&head, &merged) {
            (_, Some(m)) if m.kind != Kind::Dir => rows.push(leaf_row(path, 0, m)),
            (Some(h), None) if h.kind != Kind::Dir => rows.push(StageRow::from_leaf(path.as_bytes().to_vec(), None)),
            _ => {}
        }
    }
    for c in &outcome.conflicts {
        for (n, e) in [(1u8, &c.stages.base), (2, &c.stages.ours), (3, &c.stages.theirs)] {
            if let Some(e) = e {
                rows.push(leaf_row(&c.path, n, e));
            }
        }
    }
    let paths: Vec<RepoPath> = touched.into_iter().collect();
    sync_worktree(ctx, outcome.root, &paths, true).await?;
    stage::replace(&ws.db, ws.workspace.ws.id, StageSet::Default, &rows).await?;
    ext::index_sink().on_stage_changed(ws.workspace.ws.id, &[]).await.map_err(fatal)?;
    Ok(())
}

/// The git tree id of `root`, for `AUTO_MERGE`.
pub async fn git_tree_id(ws: &WsCtx, root: TreeId) -> Result<GitOid> {
    Ok(asc_core::git::tree_oid(ws.engine.get_tree(root).await?.as_ref()))
}

/// The git id of `commit`, registering it in the git map first.
pub async fn git_id(ws: &WsCtx, commit: ChangesetId) -> Result<GitOid> {
    let sink = git_map::RepoGitMap { db: ws.db.clone(), repo: ws.repo.key };
    index_commit_git(&ws.engine, commit, &sink).await.map_err(fatal)?;
    ws.engine.git_commit_of(commit).await.map_err(fatal)
}

/// The changeset a git commit id names in this repository.
pub async fn changeset_of(ws: &WsCtx, oid: GitOid) -> Result<ChangesetId> {
    git_map::git_to_asc(&ws.db, ws.repo.key, oid, asc_core::git::ObjectKind::Commit)
        .await?
        .map(ChangesetId::from_bytes)
        .ok_or_else(|| fatal(format!("asc: unknown commit {oid}")))
}

// ---------------------------------------------------------------- committing

/// Who commits: the committer, and the author when it is not a replayed one.
pub fn identities(ctx: &Ctx) -> Result<ident::Idents> {
    ident::resolve(ctx, None, None, None)
}

/// What a replay or a merge writes.
pub struct NewCommit {
    pub parents: Vec<ChangesetId>,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,
    pub root: TreeId,
}

/// Stores the changeset and derives what the rest of asc needs (generation, git ids); returns it
/// with its git id. The ref does not move.
pub async fn write_commit(ws: &WsCtx, new: NewCommit) -> Result<(ChangesetId, GitOid)> {
    let engine = &ws.engine;
    let parents = new.parents.clone();
    let root = new.root;
    let first = match parents.first() {
        Some(&p) => engine.get_changeset(p).await?.root,
        None => super::common::CliEngine::empty_tree_id(),
    };
    let cs = NewChangeset {
        parents: new.parents,
        author: new.author,
        committer: new.committer,
        message: new.message,
        extra: Vec::new(),
        root,
        copies: Vec::new(),
    };
    let policy = PathPolicy::Native { windows_safe: ws.repo.repo.windows_safe };
    let id = engine.create_changeset(cs, policy).await?;
    let gen_no = engine.get_derived::<Gen>(id).await.map_err(fatal)?;
    engine.warm(id).await.map_err(fatal)?;
    cs_gen::insert_many(
        &ws.db,
        ws.repo.key,
        &[cs_gen::CsGen { cs: id, gen_no: u64::from(gen_no.generation), parents }],
    )
    .await?;
    index_result(ws, first, root).await?;
    let git = git_id(ws, id).await?;
    Ok((id, git))
}

/// Moves HEAD's branch (or a detached HEAD) from `old` to `new`, with the reflog text.
pub async fn move_head(
    ctx: &Ctx,
    ws: &WsCtx,
    old: Option<ChangesetId>,
    new: ChangesetId,
    reflog: &str,
    staging: StagingAfterMove,
    committer: Option<&Signature>,
) -> Result<OpId> {
    let name = match ws.projection.read_head()? {
        Head::Branch(b) => RefName::new(format!("refs/heads/{b}").into_bytes()),
        Head::Detached(_) => RefName::new_onelevel(b"HEAD".to_vec()),
    }
    .map_err(fatal)?;
    let mut who = actor();
    if let Some(c) = committer {
        who.id = String::from_utf8_lossy(c.email()).into_owned();
    }
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name,
            expect: old.map_or(Expect::Absent, |o| Expect::Value(RefTarget::Changeset(o))),
            new: Some(RefTarget::Changeset(new)),
            expires_at: None,
        }],
        actor: who,
        reason: String::new(),
        op_id: uuid::Uuid::now_v7(),
    };
    let op = OpId(txn.op_id);
    ctx.update_local_refs(txn, reflog, staging).await?;
    Ok(op)
}

/// Tells the sinks about a changeset a command wrote (SPEC §20.3): the rewrite record
/// (`New`, no predecessors) and the synthetic index.
pub async fn announce(ws: &WsCtx, id: ChangesetId, op: OpId) -> Result<()> {
    ext::rewrite_sink().record(id, &[], RewriteOp::New, op);
    ext::index_sink().on_commit(ws.workspace.ws.id, id).await.map_err(fatal)
}

/// `[branch abc1234] subject`: the label git prints for a commit on the current branch.
pub fn branch_label(ws: &WsCtx) -> Result<String> {
    Ok(match ws.projection.read_head()? {
        Head::Branch(b) => b,
        Head::Detached(_) => "detached HEAD".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_engine::merge::{Conflict, Stages};

    fn conflict(path: &str, kind: ConflictKind, stages: Stages) -> Conflict {
        Conflict { path: RepoPath::new(path.as_bytes().to_vec()).unwrap(), kind, stages, marked_id: None }
    }

    fn entry() -> Entry {
        Entry { name: asc_core::path::Name::new(b"f".to_vec()).unwrap(), kind: Kind::File, id: [1; 32], size: 1, git: GitOid::from_bytes([2; 20]) }
    }

    #[test]
    fn messages_follow_gits_wording_and_path_order() {
        let outcome = MergeOutcome {
            root: TreeId::from_bytes([0; 32]),
            conflicts: vec![
                conflict("k", ConflictKind::ModifyDelete, Stages { base: Some(entry()), ours: Some(entry()), theirs: None }),
                conflict("f", ConflictKind::Content, Stages { base: Some(entry()), ours: Some(entry()), theirs: Some(entry()) }),
            ],
        };
        let lines = messages(&outcome, &BTreeSet::new(), "HEAD", "side");
        assert_eq!(
            lines,
            [
                "Auto-merging f",
                "CONFLICT (content): Merge conflict in f",
                "CONFLICT (modify/delete): k deleted in side and modified in HEAD.  Version HEAD of k left in tree.",
            ]
        );
    }

    #[test]
    fn a_conflict_block_lists_each_path_once_with_the_comment_char() {
        let st = Stages { base: None, ours: Some(entry()), theirs: Some(entry()) };
        let outcome = MergeOutcome {
            root: TreeId::from_bytes([0; 32]),
            conflicts: vec![conflict("b", ConflictKind::AddAdd, st.clone()), conflict("a", ConflictKind::AddAdd, st)],
        };
        assert_eq!(conflicts_block(&outcome, b'#'), b"\n# Conflicts:\n#\ta\n#\tb\n");
    }

    #[test]
    fn strategy_options_other_than_ours_and_theirs_are_refused() {
        assert_eq!(favor_of(&["theirs".into()]).unwrap(), Favor::Theirs);
        assert!(favor_of(&["patience".into()]).is_err());
        assert_eq!(favor_of(&[]).unwrap(), Favor::None);
    }

    #[test]
    fn driver_arguments_are_shell_quoted() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }
}
