//! Listing refs and loading what the atoms need: the refs under the patterns' prefixes, the
//! reachability filters, then only as much of each object as the format reads.

use asc_core::git::{ObjectKind, export_commit, export_tag, object_oid, tree_oid};
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::object::TagRecord;
use asc_core::path::RepoPath;
use asc_engine::derive::GitMapRow;
use asc_engine::history::HistoryApi;
use asc_git::projection::Head;
use asc_state::state::git_map;
use asc_store::refs::{RefEntry, Refs as _};
use asc_wc::SafeRoot;

use super::item::{Item, Obj, ObjKind};
use super::parse::{Atom, Kind, Part};
use super::select;
use super::value::{Env, Level};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::format::mailmap::Mailmap;
use crate::cli::log::refs;
use crate::cli::log::walk::Hist;

/// The reachability and identity filters of `for-each-ref`/`branch`/`tag`.
#[derive(Clone, Debug, Default)]
pub struct Filters {
    /// Git ids; a ref matches when it, or the object it peels to, has one of them.
    pub points_at: Vec<GitOid>,
    pub merged: Vec<ChangesetId>,
    pub no_merged: Vec<ChangesetId>,
    pub contains: Vec<ChangesetId>,
    pub no_contains: Vec<ChangesetId>,
}

impl Filters {
    fn commit_filters(&self) -> bool {
        !(self.merged.is_empty() && self.no_merged.is_empty() && self.contains.is_empty() && self.no_contains.is_empty())
    }
}

/// Which refs to list.
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub patterns: Vec<String>,
    pub excludes: Vec<String>,
    pub ignore_case: bool,
    /// `for-each-ref` matches names as paths; `branch` and `tag` match short names plainly.
    pub as_path: bool,
    /// `--include-root-refs`: `HEAD` and the `*_HEAD` pseudorefs.
    pub include_root: bool,
    pub filters: Filters,
}

pub(super) fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

fn matches(q: &Query, patterns: &[String], name: &str) -> bool {
    if q.as_path {
        select::match_as_path(patterns, name, q.ignore_case)
    } else {
        select::match_plain(patterns, name, q.ignore_case)
    }
}

fn listed(q: &Query, name: &str) -> bool {
    (q.patterns.is_empty() || matches(q, &q.patterns, name))
        && !(!q.excludes.is_empty() && matches(q, &q.excludes, name))
}

/// The commit export of a changeset (git's raw bytes).
pub async fn commit_raw(ws: &WsCtx, cs: ChangesetId) -> Result<Vec<u8>> {
    let changeset = ws.engine.get_changeset(cs).await?;
    let root = ws.engine.get_tree(changeset.root).await?;
    let mut parents = Vec::with_capacity(changeset.parents.len());
    for p in &changeset.parents {
        parents.push(ws.engine.git_commit_of(*p).await.map_err(fatal)?);
    }
    export_commit(&changeset, tree_oid(&root), &parents).map_err(fatal)
}

async fn commit_obj(ws: &WsCtx, cs: ChangesetId, level: Level) -> Result<Obj> {
    let oid = ws.engine.git_commit_of(cs).await.map_err(fatal)?;
    let raw = if level >= Level::Raw { commit_raw(ws, cs).await? } else { Vec::new() };
    Ok(Obj { kind: ObjKind::Commit, oid, raw })
}

async fn tag_obj(ws: &WsCtx, rec: &TagRecord) -> Result<Obj> {
    let target = ws.engine.git_commit_of(rec.target).await.map_err(fatal)?;
    let raw = export_tag(rec, target).map_err(fatal)?;
    Ok(Obj { kind: ObjKind::Tag, oid: object_oid(ObjectKind::Tag, &raw), raw })
}

/// The highest level any atom wants of the ref's object (`deref` false) or its peeled object.
fn level_of(atoms: &[&Atom], deref: bool) -> Level {
    atoms.iter().filter(|a| a.deref == deref).map(|a| a.level()).max().unwrap_or(Level::None)
}

/// The pseudorefs `--include-root-refs` adds, as far as asc keeps them.
async fn root_refs(ws: &WsCtx) -> Result<Vec<(Item, Option<RefTarget>)>> {
    let mut out = Vec::new();
    if let Head::Branch(branch) = ws.projection.read_head()? {
        let full = format!("refs/heads/{branch}");
        if let Ok(name) = asc_core::path::RefName::new(full.clone().into_bytes()) {
            if let Some(entry) = ws.db.refs().get(ws.repo.key, &name).await? {
                let mut item = Item::new("HEAD");
                item.symref = Some(full);
                out.push((item, Some(entry.target)));
            }
        }
    } else if let Some(cs) = ws.head_commit().await? {
        out.push((Item::new("HEAD"), Some(RefTarget::Changeset(cs))));
    }
    let git_dir = ws.projection.git_dir().to_owned();
    for name in ["ORIG_HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        let Ok(text) = std::fs::read_to_string(git_dir.join(name)) else { continue };
        let Some(oid) = text.get(..40).and_then(|h| h.parse::<GitOid>().ok()) else { continue };
        if let Some(asc) = git_map::git_to_asc(&ws.db, ws.repo.key, oid, ObjectKind::Commit).await? {
            out.push((Item::new(name), Some(RefTarget::Changeset(ChangesetId::from_bytes(asc)))));
        }
    }
    Ok(out)
}

/// A listed ref with its tag record, before filtering.
struct Candidate {
    item: Item,
    record: Option<TagRecord>,
}

async fn candidates(ws: &WsCtx, q: &Query) -> Result<Vec<Candidate>> {
    let mut entries: Vec<(String, RefTarget)> = Vec::new();
    let prefixes = if q.ignore_case { vec!["refs/".to_owned()] } else { select::prefixes(&q.patterns) };
    for prefix in prefixes {
        let found: Vec<RefEntry> = ws.db.refs().list(ws.repo.key, &prefix).await?;
        entries.extend(found.into_iter().map(|e| (String::from_utf8_lossy(e.name.as_bytes()).into_owned(), e.target)));
    }
    let mut roots = Vec::new();
    if q.include_root {
        roots = root_refs(ws).await?;
    }
    let head = match ws.projection.read_head()? {
        Head::Branch(b) => Some(format!("refs/heads/{b}")),
        Head::Detached(_) => None,
    };
    let mut out = Vec::new();
    let all = entries.into_iter().map(|(n, t)| (Item::new(n), Some(t))).chain(roots);
    for (mut item, target) in all {
        if !listed(q, &item.name) {
            continue;
        }
        let Some(target) = target else { continue };
        item.head = head.as_deref() == Some(item.name.as_str());
        item.target = Some(target);
        let record = match target {
            RefTarget::Changeset(c) => {
                item.commit = Some(c);
                None
            }
            RefTarget::Tag(t) => {
                let rec = refs::tag_record(ws, t).await?;
                item.commit = Some(rec.target);
                item.tag = Some(t);
                Some(rec)
            }
        };
        out.push(Candidate { item, record });
    }
    Ok(out)
}

async fn reachable(api: &dyn HistoryApi, ancestor: ChangesetId, of: ChangesetId) -> Result<bool> {
    api.is_ancestor(ancestor, of).await.map_err(fatal)
}

/// Whether a ref passes `--merged`, `--no-merged`, `--contains` and `--no-contains`.
async fn passes(api: &dyn HistoryApi, f: &Filters, tip: ChangesetId) -> Result<bool> {
    if !f.merged.is_empty() {
        let mut any = false;
        for b in &f.merged {
            any = any || reachable(api, tip, *b).await?;
        }
        if !any {
            return Ok(false);
        }
    }
    for b in &f.no_merged {
        if reachable(api, tip, *b).await? {
            return Ok(false);
        }
    }
    if !f.contains.is_empty() {
        let mut any = false;
        for c in &f.contains {
            any = any || reachable(api, *c, tip).await?;
        }
        if !any {
            return Ok(false);
        }
    }
    for c in &f.no_contains {
        if reachable(api, *c, tip).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Lists the refs `q` selects, with the object facts `atoms` read.
pub async fn collect(ws: &WsCtx, hist: &Hist, q: &Query, atoms: &[&Atom]) -> Result<Vec<Item>> {
    let mut cands = candidates(ws, q).await?;
    let api = hist.api(ws);
    if q.filters.commit_filters() {
        let mut kept = Vec::with_capacity(cands.len());
        for c in cands {
            if let Some(tip) = c.item.commit {
                if passes(api, &q.filters, tip).await? {
                    kept.push(c);
                }
            }
        }
        cands = kept;
    }
    let points_at = !q.filters.points_at.is_empty();
    let (mut own, mut peeled) = (level_of(atoms, false), level_of(atoms, true));
    if points_at {
        own = own.max(Level::Id);
        peeled = peeled.max(Level::Id);
    }
    let mut mapped: Vec<GitMapRow> = Vec::new();
    let mut items = Vec::with_capacity(cands.len());
    for Candidate { mut item, record } in cands {
        match (&record, item.commit) {
            (Some(rec), _) => {
                if own > Level::None {
                    item.obj = Some(tag_obj(ws, rec).await?);
                }
                if peeled > Level::None || points_at {
                    item.deref = Some(commit_obj(ws, rec.target, peeled).await?);
                }
            }
            (None, Some(cs)) => {
                if own > Level::None {
                    item.obj = Some(commit_obj(ws, cs, own).await?);
                }
            }
            (None, None) => {}
        }
        if points_at {
            let hits = |o: &Option<Obj>| o.as_ref().is_some_and(|o| q.filters.points_at.contains(&o.oid));
            if !(hits(&item.obj) || (record.is_some() && hits(&item.deref))) {
                continue;
            }
        }
        if let (Some(t), Some(o)) = (item.tag, &item.obj) {
            mapped.push(GitMapRow { git: o.oid, kind: ObjectKind::Tag, asc: *t.as_bytes() });
        }
        if let (Some(c), Some(o)) = (item.commit, item.deref.as_ref().or(item.obj.as_ref())) {
            if o.kind == ObjKind::Commit {
                mapped.push(GitMapRow { git: o.oid, kind: ObjectKind::Commit, asc: *c.as_bytes() });
            }
        }
        items.push(item);
    }
    if !mapped.is_empty() {
        git_map::insert_many(&ws.db, ws.repo.key, &mapped).await?;
    }
    Ok(items)
}

/// The evaluation environment for `atoms`.
pub fn env(ctx: &Ctx, ws: &WsCtx, atoms: &[&Atom], color: bool) -> Env {
    let abbrev = ctx
        .config
        .get("core.abbrev")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(7, |n| n.clamp(4, 40));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let wants_map = atoms.iter().any(|a| {
        matches!(
            a.kind,
            Kind::Person { part: Part::Name { mailmap: true } | Part::Email { mailmap: true, .. }, .. }
        )
    });
    let mailmap = if wants_map { load_mailmap(ctx, ws) } else { None };
    Env { abbrev, now, color, mailmap }
}

fn load_mailmap(ctx: &Ctx, ws: &WsCtx) -> Option<Mailmap> {
    let bytes = match ctx.config.path("mailmap.file") {
        Some(p) => std::fs::read(p).ok()?,
        None => {
            let path = RepoPath::new(".mailmap").ok()?;
            let abs = SafeRoot::new(ws.projection.toplevel()).resolve(&path)?;
            std::fs::read(abs).ok()?
        }
    };
    Some(Mailmap::parse(&bytes))
}
