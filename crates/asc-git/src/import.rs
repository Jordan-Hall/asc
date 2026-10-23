//! Git import (SPEC §14.1, §14.4).
//!
//! Commits stream in topological order through a bounded window. Per window, workers read
//! the git trees and plan the first-parent diff, blobs are hashed in parallel, and
//! changesets are then built one at a time. A commit's kind-3 git-map row is written last
//! (I3), so a crash re-imports the commit and produces identical bytes.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use asc_core::codec::{CodecError, Enveloped};
use asc_core::content::{Body, ContentError, Ingested, ingest};
use asc_core::git::{
    GitError, ObjectKind, canonicalise_commit, canonicalise_tag, export_commit, export_tag,
    object_oid, parse_commit, parse_tag, render_tree, tree_oid,
};
use asc_core::id::{ChangesetId, FileId, GitOid, TagId, TreeId};
use asc_core::object::{AscGitExtra, Entry, Kind, ObjectError, Tree};
use asc_core::path::{MAX_TREE_DEPTH, Name, PathError};
use asc_engine::Engine;
use asc_engine::commit::{self, PathPolicy};
use asc_engine::derive::{self, Derived, Gen, GitCommit, GitMapRow, GitMapSink};
use asc_engine::tree::{self, MAX_WALK_PATH_BYTES};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use data_encoding::BASE64;

use crate::import_change;
use crate::odb::{Odb, OdbError};
use asc_engine::change::ChangeRows;

/// Entries kept in each of the in-memory blob and commit caches.
const CACHE_ITEMS: usize = 65_536;

/// Why an import failed.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Derive(#[from] derive::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("object {oid} is a {found:?}, expected a {expected:?}")]
    WrongKind {
        oid: GitOid,
        found: ObjectKind,
        expected: ObjectKind,
    },
    #[error("tree {0} is malformed: {1}")]
    BadTree(GitOid, &'static str),
    #[error("tree {0} has an entry name asc cannot hold: {1}")]
    BadName(GitOid, PathError),
    /// Raised while parsing one tree, which does not know its commit; [`prepare`] turns it into
    /// [`ImportError::CommitNameTooLong`].
    #[error(
        "tree {tree} has an entry name of {len} bytes, past the {MAX_WALK_PATH_BYTES} byte limit"
    )]
    TreeNameTooLong { tree: GitOid, len: usize },
    #[error(
        "commit {commit} is refused: its tree {tree} has an entry name of {len} bytes, \
         past the {MAX_WALK_PATH_BYTES} byte limit"
    )]
    CommitNameTooLong {
        commit: GitOid,
        tree: GitOid,
        len: usize,
    },
    #[error("tree {0} nests deeper than {MAX_TREE_DEPTH} levels")]
    TreeTooDeep(GitOid),
    #[error("git tree {oid} does not describe the asc tree built from it: {source}")]
    BadOverride {
        oid: GitOid,
        source: crate::tree_override::OverrideError,
    },
    #[error("tree {0} is an empty directory, which asc cannot represent")]
    EmptyDir(GitOid),
    #[error("commit {0} has an unknown asc header version {1}: upgrade asc")]
    UpgradeAsc(GitOid, u8),
    #[error("commit {0} has a malformed asc header")]
    BadAscHeader(GitOid),
    #[error("commit {commit} does not round-trip to its git bytes: {source}")]
    RoundTrip { commit: GitOid, source: GitError },
    #[error("tag {tag} does not round-trip to its git bytes: {source}")]
    TagRoundTrip { tag: GitOid, source: GitError },
    #[error("git commit of {changeset} is {derived}, expected {expected}")]
    CommitMismatch {
        changeset: ChangesetId,
        derived: GitOid,
        expected: GitOid,
    },
    #[error("parent commit {0} is not imported")]
    ParentMissing(GitOid),
    #[error("internal: plan for {0} disagrees with the parent tree")]
    Inconsistent(GitOid),
    #[error("stored object does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Meta(#[from] import_change::Error),
}

pub type Result<T, E = ImportError> = std::result::Result<T, E>;

/// The git map as the importer sees it: row sink plus lookup (for incremental import).
#[async_trait]
pub trait ImportMap: GitMapSink {
    /// The asc id the map holds for `git` of `kind`.
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>>;
}

/// Tuning knobs; the defaults suit a laptop.
#[derive(Clone, Copy, Debug)]
pub struct ImportPolicy {
    /// Commits planned per window.
    pub window: usize,
    /// Blobs read and hashed per parallel batch (bounds buffered blob bytes).
    pub batch: usize,
    /// Worker threads for tree planning and blob hashing.
    pub workers: usize,
}

impl Default for ImportPolicy {
    fn default() -> Self {
        let workers = std::thread::available_parallelism().map_or(4, usize::from);
        Self {
            window: 128,
            batch: workers * 4,
            workers,
        }
    }
}

/// A ref to import: its name (for reports) and the object it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    pub name: Vec<u8>,
    pub oid: GitOid,
}

/// A ref that asc cannot represent (SPEC §14.1 step 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedRef {
    pub name: Vec<u8>,
    pub oid: GitOid,
    /// What it points to: `tree`, `blob` or `tag`.
    pub target: &'static str,
}

impl SkippedRef {
    /// The warning line SPEC §14.1 prescribes.
    pub fn warning(&self) -> String {
        format!(
            "warning: asc: skipped tag '{}' (points to a {})",
            String::from_utf8_lossy(&self.name),
            self.target
        )
    }
}

/// What one `import_commits` call did.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// Commits newly imported.
    pub commits: usize,
    /// Blobs newly hashed and stored.
    pub blobs: usize,
    /// Trees built (including non-canonical ones).
    pub trees: usize,
    /// Non-canonical git trees stored as overrides.
    pub overrides: usize,
    /// Refs and the changeset they now name (tags are peeled).
    pub refs: Vec<(Vec<u8>, ChangesetId)>,
    /// Annotated tags stored as `TagRecord`s.
    pub tags: Vec<(Vec<u8>, TagId)>,
    /// Refs that point at a tree, a blob or a tag of a tag.
    pub skipped_refs: Vec<SkippedRef>,
    /// Tags that failed parsing or the round trip, with the reason.
    pub rejected_tags: Vec<(Vec<u8>, GitOid, String)>,
}

// ------------------------------------------------------------------ git trees

/// A git tree split into asc entries (zero ids), sorted by name.
struct GitTree {
    tree: Tree,
    /// The raw body, kept only when its canonical render differs.
    raw_if_odd: Option<Vec<u8>>,
}

fn kind_of(mode: u32) -> Option<Kind> {
    match mode & 0o170_000 {
        0o100_000 => Some(if mode & 0o100 != 0 {
            Kind::Exec
        } else {
            Kind::File
        }),
        0o120_000 => Some(Kind::Symlink),
        0o040_000 => Some(Kind::Dir),
        0o160_000 => Some(Kind::Gitlink),
        _ => None,
    }
}

fn parse_git_tree(oid: GitOid, body: Vec<u8>) -> Result<GitTree> {
    let bad = |why| ImportError::BadTree(oid, why);
    let mut entries = Vec::new();
    let mut rest = &body[..];
    while !rest.is_empty() {
        let sp = rest.iter().position(|&c| c == b' ').ok_or(bad("no mode"))?;
        let mode = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|m| u32::from_str_radix(m, 8).ok())
            .and_then(|m| kind_of(m).map(|_| m))
            .ok_or(bad("unsupported mode"))?;
        rest = &rest[sp + 1..];
        let nul = rest
            .iter()
            .position(|&c| c == 0)
            .ok_or(bad("no name end"))?;
        // Git trees are untrusted: bound the name before copying it, as `get_tree` does on read.
        if nul > MAX_WALK_PATH_BYTES {
            return Err(ImportError::TreeNameTooLong {
                tree: oid,
                len: nul,
            });
        }
        let name = Name::new(&rest[..nul]).map_err(|e| ImportError::BadName(oid, e))?;
        rest = &rest[nul + 1..];
        let git = rest
            .get(..20)
            .and_then(|b| <[u8; 20]>::try_from(b).ok())
            .map(GitOid::from_bytes)
            .ok_or(bad("truncated entry"))?;
        rest = &rest[20..];
        entries.push(Entry {
            name,
            kind: kind_of(mode).ok_or(bad("unsupported mode"))?,
            id: [0; 32],
            size: 0,
            git,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if entries.windows(2).any(|w| w[0].name == w[1].name) {
        return Err(bad("duplicate entry name"));
    }
    let tree = Tree::new(entries, true)?;
    let odd = render_tree(&tree) != body;
    Ok(GitTree {
        tree,
        raw_if_odd: odd.then_some(body),
    })
}

fn read_git_tree(odb: &Odb, oid: GitOid) -> Result<GitTree> {
    let (found, body) = odb.read_object_verified(oid)?;
    if found != ObjectKind::Tree {
        return Err(ImportError::WrongKind {
            oid,
            found,
            expected: ObjectKind::Tree,
        });
    }
    parse_git_tree(oid, body)
}

fn read_raw_commit(odb: &Odb, oid: GitOid) -> Result<Vec<u8>> {
    let (found, raw) = odb.read_object_verified(oid)?;
    if found != ObjectKind::Commit {
        return Err(ImportError::WrongKind {
            oid,
            found,
            expected: ObjectKind::Commit,
        });
    }
    Ok(raw)
}

/// The `tree` and `parent` headers of a raw commit, without parsing the rest.
fn commit_links(raw: &[u8]) -> Result<(GitOid, Vec<GitOid>)> {
    let bad = || ImportError::Git(GitError::Commit("bad tree or parent header"));
    let mut tree = None;
    let mut parents = Vec::new();
    for line in raw.split(|&c| c == b'\n') {
        let hex = |v: &[u8]| {
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.parse::<GitOid>().ok())
                .ok_or_else(bad)
        };
        if let Some(v) = line.strip_prefix(b"tree ") {
            tree = Some(hex(v)?);
        } else if let Some(v) = line.strip_prefix(b"parent ") {
            parents.push(hex(v)?);
        } else {
            break;
        }
    }
    Ok((tree.ok_or_else(bad)?, parents))
}

/// Fails with `upgrade asc` if the commit has an `asc` header of an unknown version (SPEC §2.2a).
fn check_asc_version(oid: GitOid, raw: &[u8]) -> Result<()> {
    for line in raw.split(|&c| c == b'\n') {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix(b"asc ") {
            let first = BASE64
                .decode(v)
                .ok()
                .and_then(|b| b.first().copied())
                .ok_or(ImportError::BadAscHeader(oid))?;
            if first != AscGitExtra::VERSION {
                return Err(ImportError::UpgradeAsc(oid, first));
            }
        }
    }
    Ok(())
}

/// An override must list exactly the entries of the asc tree it is stored for (U33): a mismatch
/// means the git tree does not describe the asc tree, which is a hard error.
fn check_override(git: GitOid, raw: &[u8], tree: &Tree) -> Result<()> {
    crate::tree_override::verify(raw, tree)
        .map_err(|source| ImportError::BadOverride { oid: git, source })
}

// ----------------------------------------------------------------------- plan

/// What a directory of the new tree needs, relative to the first parent's git tree.
struct DirPlan {
    git: GitOid,
    /// The tree equals the parent's: nothing to build.
    same: bool,
    /// Raw body of a non-canonical tree, stored as an override.
    raw_override: Option<Vec<u8>>,
    entries: Vec<(Name, Plan)>,
}

enum Plan {
    /// Equal to the parent's entry of that name.
    Same,
    Blob(Kind, GitOid),
    Gitlink(GitOid),
    Dir(Box<DirPlan>),
}

impl Drop for DirPlan {
    /// Drops the plan iteratively; the default glue recurses once per level.
    fn drop(&mut self) {
        let mut work = std::mem::take(&mut self.entries);
        while let Some((_, plan)) = work.pop() {
            if let Plan::Dir(mut sub) = plan {
                work.append(&mut sub.entries);
            }
        }
    }
}

/// One directory being planned: its entries so far and the next one to look at.
struct PlanFrame {
    oid: GitOid,
    new: GitTree,
    old: Option<GitTree>,
    next: usize,
    entries: Vec<(Name, Plan)>,
}

/// Opens a directory for planning, or returns its finished plan when it equals the parent's.
fn open_dir(
    odb: &Odb,
    oid: GitOid,
    parent: Option<GitOid>,
    is_root: bool,
) -> Result<std::result::Result<PlanFrame, DirPlan>> {
    if parent == Some(oid) {
        return Ok(Err(DirPlan {
            git: oid,
            same: true,
            raw_override: None,
            entries: Vec::new(),
        }));
    }
    let new = read_git_tree(odb, oid)?;
    if new.tree.entries().is_empty() && !is_root {
        return Err(ImportError::EmptyDir(oid));
    }
    let old = parent.map(|p| read_git_tree(odb, p)).transpose()?;
    let entries = Vec::with_capacity(new.tree.entries().len());
    Ok(Ok(PlanFrame {
        oid,
        new,
        old,
        next: 0,
        entries,
    }))
}

/// Plans a directory and those below it with an explicit stack, so a crafted deep tree cannot
/// overflow the call stack. Nesting past [`MAX_TREE_DEPTH`] levels fails with `TreeTooDeep`.
/// (`RepoPath` has no length limit that would bound the depth indirectly.)
fn plan_dir(
    odb: &Odb,
    oid: GitOid,
    parent: Option<GitOid>,
    is_root: bool,
    blobs: &mut Vec<GitOid>,
) -> Result<DirPlan> {
    let mut stack = match open_dir(odb, oid, parent, is_root)? {
        Ok(frame) => vec![frame],
        Err(done) => return Ok(done),
    };
    loop {
        let Some(top) = stack.last_mut() else {
            unreachable!("the loop returns when the last frame finishes")
        };
        let Some(e) = top.new.tree.entries().get(top.next) else {
            let frame = stack.pop().ok_or(ImportError::Inconsistent(oid))?;
            let done = DirPlan {
                git: frame.oid,
                same: false,
                raw_override: frame.new.raw_if_odd,
                entries: frame.entries,
            };
            match stack.last_mut() {
                None => return Ok(done),
                Some(parent_frame) => {
                    let name = parent_frame.new.tree.entries()[parent_frame.next - 1]
                        .name
                        .clone();
                    parent_frame.entries.push((name, Plan::Dir(Box::new(done))));
                    continue;
                }
            }
        };
        top.next += 1;
        let before = top.old.as_ref().and_then(|t| t.tree.get(&e.name));
        match before {
            Some(b) if b.kind == e.kind && b.git == e.git => {
                top.entries.push((e.name.clone(), Plan::Same));
            }
            _ => match e.kind {
                Kind::Dir => {
                    let under = before.filter(|b| b.kind == Kind::Dir).map(|b| b.git);
                    let child = e.git;
                    if stack.len() > MAX_TREE_DEPTH {
                        return Err(ImportError::TreeTooDeep(child));
                    }
                    match open_dir(odb, child, under, false)? {
                        Ok(frame) => stack.push(frame),
                        Err(done) => {
                            let Some(f) = stack.last_mut() else { continue };
                            let name = f.new.tree.entries()[f.next - 1].name.clone();
                            f.entries.push((name, Plan::Dir(Box::new(done))));
                        }
                    }
                }
                Kind::Gitlink => top.entries.push((e.name.clone(), Plan::Gitlink(e.git))),
                kind => {
                    blobs.push(e.git);
                    top.entries.push((e.name.clone(), Plan::Blob(kind, e.git)));
                }
            },
        }
    }
}

/// A commit read and planned, ready to build.
struct Prepared {
    oid: GitOid,
    raw: Vec<u8>,
    parents: Vec<GitOid>,
    plan: DirPlan,
    blobs: Vec<GitOid>,
}

fn prepare(odb: &Odb, oid: GitOid) -> Result<Prepared> {
    let raw = read_raw_commit(odb, oid)?;
    check_asc_version(oid, &raw)?;
    let parsed = parse_commit(&raw)?;
    let parent_tree = match parsed.parents.first() {
        Some(&p) => Some(commit_links(&read_raw_commit(odb, p)?)?.0),
        None => None,
    };
    let mut blobs = Vec::new();
    let plan = plan_dir(odb, parsed.tree, parent_tree, true, &mut blobs).map_err(|e| match e {
        ImportError::TreeNameTooLong { tree, len } => ImportError::CommitNameTooLong {
            commit: oid,
            tree,
            len,
        },
        other => other,
    })?;
    Ok(Prepared {
        oid,
        raw,
        parents: parsed.parents,
        plan,
        blobs,
    })
}

fn hash_blob(odb: &Odb, oid: GitOid) -> Result<(GitOid, Ingested)> {
    let got = odb.with_blob_verified(oid, |bytes| ingest(Cursor::new(bytes)))??;
    debug_assert_eq!(got.git, oid);
    Ok((oid, got))
}

/// Blob bytes buffered at once while hashing (each blob costs about twice its size in flight).
const BATCH_BYTES: u64 = 32 << 20;
/// Threads that read and chunk blobs at once. A packed blob can sit at the end of a delta chain
/// whose bases are far larger than the blob, and each thread decodes its own chain.
const HASH_WORKERS: usize = 3;

/// Splits `want` into batches of at most `count` blobs and about [`BATCH_BYTES`] of content;
/// a blob larger than the budget is a batch of its own.
fn byte_batches(odb: &Odb, want: &[GitOid], count: usize) -> Vec<Vec<GitOid>> {
    let mut out = Vec::new();
    let mut cur: Vec<GitOid> = Vec::new();
    let mut bytes = 0u64;
    for &oid in want {
        let size = odb.blob_size(oid).unwrap_or(BATCH_BYTES);
        if !cur.is_empty() && (cur.len() >= count || bytes + size > BATCH_BYTES) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        cur.push(oid);
        bytes += size;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Runs `f` over `items` on one thread per fork; results keep the input order.
fn par_map<T: Sync, R: Send>(
    forks: &mut [Odb],
    items: &[T],
    f: impl Fn(&Odb, &T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, R)> = Vec::with_capacity(items.len());
    std::thread::scope(|s| {
        let handles: Vec<_> = forks
            .iter_mut()
            .take(items.len())
            .map(|odb| {
                let (next, f) = (&next, &f);
                s.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        out.push((i, f(odb, item)));
                    }
                    out
                })
            })
            .collect();
        for h in handles {
            match h.join() {
                Ok(part) => done.extend(part),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    done.sort_by_key(|&(i, _)| i);
    done.into_iter().map(|(_, r)| r).collect()
}

// ------------------------------------------------------------------- importer

/// Rows and counters gathered while one changeset is built.
#[derive(Default)]
struct Built {
    rows: Vec<GitMapRow>,
    trees: usize,
    overrides: usize,
}

/// Imports git history into an asc repository.
pub struct Importer<'a, O: Objects, R: Refs> {
    engine: &'a Engine<O, R>,
    objects: Arc<O>,
    odb: &'a Odb,
    forks: Vec<Odb>,
    map: &'a dyn ImportMap,
    policy: ImportPolicy,
    /// Recently hashed blobs: git oid to file id and size.
    blobs: quick_cache::sync::Cache<GitOid, (FileId, u64)>,
    /// Recently imported commits.
    commits: quick_cache::sync::Cache<GitOid, ChangesetId>,
    /// Where `ChangeMeta` rows are indexed (SPEC §20.2); the meta objects are written either way.
    rows: Option<&'a dyn ChangeRows>,
    /// Names of the refs that reached commits in the current call, in sorted order.
    scopes: Vec<Vec<u8>>,
}

impl<'a, O: Objects, R: Refs> Importer<'a, O, R> {
    /// `objects` must be the store `engine` was built over.
    pub fn new(
        engine: &'a Engine<O, R>,
        objects: Arc<O>,
        odb: &'a Odb,
        map: &'a dyn ImportMap,
        policy: ImportPolicy,
    ) -> Self {
        let workers = policy.workers.max(1);
        Self {
            engine,
            objects,
            odb,
            forks: (0..workers).map(|_| odb.fork()).collect(),
            map,
            policy,
            blobs: quick_cache::sync::Cache::new(CACHE_ITEMS),
            commits: quick_cache::sync::Cache::new(CACHE_ITEMS),
            rows: None,
            scopes: Vec::new(),
        }
    }

    /// Indexes the `ChangeMeta` of each imported commit in `rows` as well (C45).
    #[must_use]
    pub fn with_change_rows(mut self, rows: &'a dyn ChangeRows) -> Self {
        self.rows = Some(rows);
        self
    }

    /// Imports every commit reachable from `tips` that the git map does not know yet, then
    /// the annotated tags among them. Safe to rerun after a crash (SPEC §14.4).
    pub async fn import_commits(&mut self, tips: &[Tip]) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut roots = Vec::new();
        let mut jobs = Vec::new();
        let mut named: Vec<(Vec<u8>, GitOid)> = Vec::new();
        for tip in tips {
            let before = roots.len();
            self.classify(tip, &mut roots, &mut jobs, &mut report)?;
            named.extend(roots[before..].iter().map(|&r| (tip.name.clone(), r)));
        }
        // A commit's scope is the first ref, in sorted name order, that reaches it (SPEC §20.2).
        named.sort_by(|a, b| a.0.cmp(&b.0));
        self.scopes.clear();
        let mut scoped = Vec::with_capacity(named.len());
        for (name, oid) in named {
            if self.scopes.last() != Some(&name) {
                self.scopes.push(name);
            }
            scoped.push((oid, self.scopes.len() - 1));
        }
        let order = self.discover(&scoped).await?;
        for window in order.chunks(self.policy.window.max(1)) {
            self.import_window(window, &mut report).await?;
        }
        for (tip, job) in jobs {
            self.import_tag(&tip, job, &mut report).await?;
        }
        for tip in tips {
            if report.refs.iter().all(|(n, _)| *n != tip.name)
                && let Some(id) = self.commit_id(tip.oid).await?
            {
                report.refs.push((tip.name.clone(), id));
            }
        }
        Ok(report)
    }

    /// Sorts a tip into walk roots, tag jobs and skipped refs.
    fn classify(
        &self,
        tip: &Tip,
        roots: &mut Vec<GitOid>,
        jobs: &mut Vec<(Tip, TagJob)>,
        report: &mut ImportReport,
    ) -> Result<()> {
        let skip = |report: &mut ImportReport, target| {
            report.skipped_refs.push(SkippedRef {
                name: tip.name.clone(),
                oid: tip.oid,
                target,
            });
        };
        let (kind, body) = self.odb.read_object_verified(tip.oid)?;
        match kind {
            ObjectKind::Commit => roots.push(tip.oid),
            ObjectKind::Tree => skip(report, "tree"),
            ObjectKind::Blob => skip(report, "blob"),
            ObjectKind::Tag => match parse_tag(&body) {
                Err(e) => {
                    report
                        .rejected_tags
                        .push((tip.name.clone(), tip.oid, e.to_string()));
                }
                Ok(p) => match p.object_type.as_slice() {
                    b"commit" => {
                        roots.push(p.object);
                        jobs.push((
                            tip.clone(),
                            TagJob {
                                raw: body,
                                target: p.object,
                            },
                        ));
                    }
                    b"tree" => skip(report, "tree"),
                    b"blob" => skip(report, "blob"),
                    b"tag" => skip(report, "tag"),
                    _ => report.rejected_tags.push((
                        tip.name.clone(),
                        tip.oid,
                        "unknown target type".into(),
                    )),
                },
            },
        }
        Ok(())
    }

    async fn commit_id(&self, git: GitOid) -> Result<Option<ChangesetId>> {
        if let Some(id) = self.commits.get(&git) {
            return Ok(Some(id));
        }
        Ok(self
            .map
            .get(git, ObjectKind::Commit)
            .await?
            .map(ChangesetId::from_bytes))
    }

    /// Unknown commits reachable from `roots`, parents first (iterative post-order DFS).
    async fn discover(&self, roots: &[(GitOid, usize)]) -> Result<Vec<(GitOid, usize)>> {
        struct Frame {
            oid: GitOid,
            scope: usize,
            parents: Vec<GitOid>,
            next: usize,
        }
        let mut seen: HashSet<GitOid> = HashSet::new();
        let mut order = Vec::new();
        let mut stack: Vec<Frame> = Vec::new();
        for &(root, scope) in roots {
            if !seen.insert(root) || self.commit_id(root).await?.is_some() {
                continue;
            }
            let raw = read_raw_commit(self.odb, root)?;
            stack.push(Frame {
                oid: root,
                scope,
                parents: commit_links(&raw)?.1,
                next: 0,
            });
            while let Some(top) = stack.last_mut() {
                if let Some(&p) = top.parents.get(top.next) {
                    top.next += 1;
                    if seen.insert(p) && self.commit_id(p).await?.is_none() {
                        let raw = read_raw_commit(self.odb, p)?;
                        stack.push(Frame {
                            oid: p,
                            scope,
                            parents: commit_links(&raw)?.1,
                            next: 0,
                        });
                    }
                } else {
                    order.push((top.oid, top.scope));
                    stack.pop();
                }
            }
        }
        Ok(order)
    }

    async fn import_window(
        &mut self,
        window: &[(GitOid, usize)],
        report: &mut ImportReport,
    ) -> Result<()> {
        let prepared = par_map(&mut self.forks, window, |odb, &(oid, _)| prepare(odb, oid))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let hashed = self.hash_blobs(&prepared, report).await?;
        for (p, &(_, scope)) in prepared.into_iter().zip(window) {
            self.build_commit(p, scope, &hashed, report).await?;
        }
        Ok(())
    }

    /// Hashes and stores the window's blobs that are not cached; returns every blob's id.
    async fn hash_blobs(
        &mut self,
        prepared: &[Prepared],
        report: &mut ImportReport,
    ) -> Result<HashMap<GitOid, (FileId, u64)>> {
        let mut known = HashMap::new();
        let mut want = Vec::new();
        let mut seen = HashSet::new();
        for oid in prepared.iter().flat_map(|p| p.blobs.iter().copied()) {
            if !seen.insert(oid) {
                continue;
            }
            match self.blobs.get(&oid) {
                Some(v) => {
                    known.insert(oid, v);
                }
                None => want.push(oid),
            }
        }
        for batch in byte_batches(self.odb, &want, self.policy.batch.max(1)) {
            let n = HASH_WORKERS.min(self.forks.len());
            let done = par_map(&mut self.forks[..n], &batch, |odb, &oid| {
                hash_blob(odb, oid)
            })
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
            let mut rows = Vec::with_capacity(done.len());
            for (oid, got) in done {
                self.store_content(&got).await?;
                rows.push(GitMapRow {
                    git: oid,
                    kind: ObjectKind::Blob,
                    asc: *got.file.as_bytes(),
                });
                known.insert(oid, (got.file, got.size));
                self.blobs.insert(oid, (got.file, got.size));
            }
            self.map.put(&rows).await?;
            report.blobs += rows.len();
        }
        Ok(known)
    }

    /// Stores a file's bytes: chunks and the chunk list first (I1).
    async fn store_content(&self, got: &Ingested) -> Result<()> {
        match &got.body {
            Body::Inline(bytes) => {
                self.objects
                    .put(&ObjKey::File(got.file), encode_stored(bytes))
                    .await?;
            }
            Body::Chunked { list, chunks } => {
                for (id, bytes) in chunks {
                    self.objects
                        .put(&ObjKey::Chunk(*id), encode_stored(bytes))
                        .await?;
                }
                self.objects
                    .put(
                        &ObjKey::ChunkList(got.file),
                        encode_stored(&list.to_envelope()),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Builds the asc tree for `plan` on top of `base`, bottom-up, and stores it.
    ///
    /// Iterative, with one frame per directory, so stack use does not depend on tree depth.
    async fn build_dir(
        &self,
        plan: &DirPlan,
        base: Option<Arc<Tree>>,
        is_root: bool,
        hashed: &HashMap<GitOid, (FileId, u64)>,
        built: &mut Built,
    ) -> Result<(TreeId, u64)> {
        struct Frame<'p> {
            plan: &'p DirPlan,
            base: Option<Arc<Tree>>,
            is_root: bool,
            next: usize,
            entries: Vec<Entry>,
            /// Child base trees fetched ahead of use, by plan index.
            loaded: HashMap<usize, Arc<Tree>>,
        }
        let frame = |plan, base, is_root| Frame {
            plan,
            base,
            is_root,
            next: 0,
            entries: Vec::with_capacity(plan.entries.len()),
            loaded: HashMap::new(),
        };
        let mut stack = vec![frame(plan, base, is_root)];
        loop {
            let depth = stack.len();
            let Some(top) = stack.last_mut() else {
                return Err(ImportError::Inconsistent(plan.git));
            };
            let cur = top.plan;
            let Some((name, step)) = cur.entries.get(top.next) else {
                let Some(done) = stack.pop() else {
                    return Err(ImportError::Inconsistent(plan.git));
                };
                let (id, size) = self
                    .finish_dir(done.plan, done.entries, done.is_root, built)
                    .await?;
                let Some(parent) = stack.last_mut() else {
                    return Ok((id, size));
                };
                let (name, step) = &parent.plan.entries[parent.next - 1];
                let Plan::Dir(sub) = step else {
                    return Err(ImportError::Inconsistent(parent.plan.git));
                };
                parent.entries.push(Entry {
                    name: name.clone(),
                    kind: Kind::Dir,
                    id: *id.as_bytes(),
                    size,
                    git: sub.git,
                });
                continue;
            };
            top.next += 1;
            let git = cur.git;
            let entry = match step {
                Plan::Same => top
                    .base
                    .as_ref()
                    .and_then(|b| b.get(name))
                    .cloned()
                    .ok_or(ImportError::Inconsistent(git))?,
                Plan::Blob(kind, oid) => {
                    let &(file, size) = hashed.get(oid).ok_or(ImportError::Inconsistent(git))?;
                    Entry {
                        name: name.clone(),
                        kind: *kind,
                        id: *file.as_bytes(),
                        size,
                        git: *oid,
                    }
                }
                Plan::Gitlink(oid) => Entry {
                    name: name.clone(),
                    kind: Kind::Gitlink,
                    id: [0; 32],
                    size: 0,
                    git: *oid,
                },
                Plan::Dir(sub) => {
                    let top_base = top.base.clone();
                    if depth > MAX_TREE_DEPTH {
                        return Err(ImportError::TreeTooDeep(sub.git));
                    }
                    let idx = top.next - 1;
                    if !top.loaded.contains_key(&idx) {
                        // Read this child's base tree and those of the next siblings together,
                        // at most LOAD_WINDOW in flight.
                        let base_dir = |n: &Name| {
                            top_base
                                .as_ref()
                                .and_then(|b| b.get(n))
                                .filter(|e| e.kind == Kind::Dir)
                                .map(|e| TreeId::from_bytes(e.id))
                        };
                        let end = (idx + tree::LOAD_WINDOW).min(cur.entries.len());
                        let want: Vec<(usize, TreeId)> = (idx..end)
                            .filter(|&j| matches!(cur.entries[j].1, Plan::Dir(_)))
                            .filter_map(|j| base_dir(&cur.entries[j].0).map(|id| (j, id)))
                            .collect();
                        let ids: Vec<TreeId> = want.iter().map(|w| w.1).collect();
                        let trees = self.engine.get_trees(&ids).await?;
                        if let Some(top) = stack.last_mut() {
                            for ((j, _), t) in want.into_iter().zip(trees) {
                                top.loaded.insert(j, t);
                            }
                        }
                    }
                    let under = stack.last_mut().and_then(|t| t.loaded.remove(&idx));
                    stack.push(frame(sub, under, false));
                    continue;
                }
            };
            top.entries.push(entry);
        }
    }

    /// Stores one built directory: its tree, its override (verified) and its git map row.
    async fn finish_dir(
        &self,
        plan: &DirPlan,
        entries: Vec<Entry>,
        is_root: bool,
        built: &mut Built,
    ) -> Result<(TreeId, u64)> {
        let size = entries.iter().fold(0u64, |s, e| s.saturating_add(e.size));
        let tree = Tree::new(entries, is_root)?;
        if let Some(raw) = &plan.raw_override {
            check_override(plan.git, raw, &tree)?;
        }
        let id = self.engine.put_tree(tree).await?;
        if let Some(raw) = &plan.raw_override {
            self.objects
                .put(&ObjKey::GitTreeOverride(plan.git), encode_stored(raw))
                .await?;
            built.overrides += 1;
        }
        built.trees += 1;
        built.rows.push(GitMapRow {
            git: plan.git,
            kind: ObjectKind::Tree,
            asc: *id.as_bytes(),
        });
        Ok((id, size))
    }

    /// Builds, validates and stores one changeset; the commit row is written last (I3).
    async fn build_commit(
        &self,
        p: Prepared,
        scope: usize,
        hashed: &HashMap<GitOid, (FileId, u64)>,
        report: &mut ImportReport,
    ) -> Result<()> {
        let mut parents = Vec::with_capacity(p.parents.len());
        for &g in &p.parents {
            parents.push(
                self.commit_id(g)
                    .await?
                    .ok_or(ImportError::ParentMissing(g))?,
            );
        }
        let base_root = match parents.first() {
            Some(&first) => self.engine.get_changeset(first).await?.root,
            None => Engine::<O, R>::empty_tree_id(),
        };
        let mut built = Built::default();
        let root = if p.plan.same {
            base_root
        } else {
            let base = self.engine.get_tree(base_root).await?;
            self.build_dir(&p.plan, Some(base), true, hashed, &mut built)
                .await?
                .0
        };
        let changes = self.engine.leafdiff(base_root, root, None).await?;
        let cs = canonicalise_commit(&p.raw, root, parents, changes)?;
        self.engine
            .validate_changeset(&cs, PathPolicy::Import)
            .await?;
        let round = export_commit(&cs, p.plan.git, &p.parents).map_err(|source| {
            ImportError::RoundTrip {
                commit: p.oid,
                source,
            }
        })?;
        if round != p.raw {
            return Err(ImportError::RoundTrip {
                commit: p.oid,
                source: GitError::RawMismatch("export differs from the original bytes"),
            });
        }
        let envelope = cs.to_envelope();
        let id = ChangesetId::of(&envelope);
        self.objects
            .put(&ObjKey::Changeset(id), encode_stored(&envelope))
            .await?;
        self.derive_and_check(id, root, &p).await?;
        let scope = self.scopes.get(scope).map_or(&[][..], Vec::as_slice);
        import_change::write_meta(&*self.objects, self.rows, id, &p.raw, scope).await?;
        self.map.put(&built.rows).await?;
        self.map
            .put(&[GitMapRow {
                git: p.oid,
                kind: ObjectKind::Commit,
                asc: *id.as_bytes(),
            }])
            .await?;
        self.commits.insert(p.oid, id);
        report.commits += 1;
        report.trees += built.trees;
        report.overrides += built.overrides;
        Ok(())
    }

    /// Writes the eager derived values and asserts `gitcommit == oid` (SPEC §14.1 step 5).
    ///
    /// The engine derives `gitcommit` from the canonical render of the root tree, which is
    /// wrong when the root is a non-canonical git tree; its value is then written here from
    /// the verified original.
    async fn derive_and_check(&self, id: ChangesetId, root: TreeId, p: &Prepared) -> Result<()> {
        self.engine.get_derived::<Gen>(id).await?;
        let canonical_root = tree_oid(&*self.engine.get_tree(root).await?) == p.plan.git;
        let derived = if canonical_root {
            self.engine.get_derived::<GitCommit>(id).await?.commit
        } else {
            let value = GitCommit {
                commit: object_oid(ObjectKind::Commit, &p.raw),
            };
            let key = ObjKey::Derived {
                name: GitCommit::NAME,
                version: GitCommit::VERSION,
                id: *id.as_bytes(),
            };
            self.objects
                .put(&key, encode_stored(&value.encode()))
                .await?;
            value.commit
        };
        if derived != p.oid {
            return Err(ImportError::CommitMismatch {
                changeset: id,
                derived,
                expected: p.oid,
            });
        }
        Ok(())
    }

    /// Stores an annotated tag of a commit as a `TagRecord`.
    async fn import_tag(&self, tip: &Tip, job: TagJob, report: &mut ImportReport) -> Result<()> {
        let target = self
            .commit_id(job.target)
            .await?
            .ok_or(ImportError::ParentMissing(job.target))?;
        let record = canonicalise_tag(&job.raw, target).and_then(|t| {
            let round = export_tag(&t, job.target)?;
            if round == job.raw {
                Ok(t)
            } else {
                Err(GitError::RawMismatch(
                    "export differs from the original bytes",
                ))
            }
        });
        match record {
            Err(e) => report
                .rejected_tags
                .push((tip.name.clone(), tip.oid, e.to_string())),
            Ok(t) => {
                let envelope = t.to_envelope();
                let id = TagId::of(&envelope);
                self.objects
                    .put(&ObjKey::Tag(id), encode_stored(&envelope))
                    .await?;
                self.map
                    .put(&[GitMapRow {
                        git: tip.oid,
                        kind: ObjectKind::Tag,
                        asc: *id.as_bytes(),
                    }])
                    .await?;
                report.tags.push((tip.name.clone(), id));
                report.refs.push((tip.name.clone(), target));
            }
        }
        Ok(())
    }
}

/// An annotated tag of a commit, waiting for its target.
struct TagJob {
    raw: Vec<u8>,
    target: GitOid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::git::ObjectKind;

    fn entry(name: &str, seed: u8) -> Entry {
        Entry {
            name: Name::new(name).unwrap(),
            kind: Kind::File,
            id: [seed; 32],
            size: 1,
            git: GitOid::from_bytes([seed; 20]),
        }
    }

    #[test]
    fn override_that_does_not_match_the_tree_is_a_hard_error() {
        let tree = Tree::new(vec![entry("a", 1), entry("b", 2)], true).unwrap();
        let other = Tree::new(vec![entry("a", 1), entry("c", 2)], true).unwrap();
        let canon = render_tree(&tree);
        check_override(GitOid::from_bytes([9; 20]), &canon, &tree).unwrap();
        assert!(matches!(
            check_override(GitOid::from_bytes([9; 20]), &canon, &other),
            Err(ImportError::BadOverride { .. })
        ));
    }

    /// Writes a chain of `levels` nested directories and returns the root tree oid.
    fn chain(odb: &Odb, levels: usize) -> GitOid {
        let blob = odb.write_loose(ObjectKind::Blob, b"x").unwrap();
        let mut body = [b"100644 f ".as_slice(), blob.as_bytes()].concat();
        let mut oid = odb.write_loose(ObjectKind::Tree, &body).unwrap();
        for _ in 1..levels {
            body = [b"40000 d ".as_slice(), oid.as_bytes()].concat();
            oid = odb.write_loose(ObjectKind::Tree, &body).unwrap();
        }
        oid
    }

    fn plan_on_small_stack(levels: usize) -> Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let odb = Odb::open(dir.path()).unwrap();
        let root = chain(&odb, levels);
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || plan_dir(&odb, root, None, true, &mut Vec::new()).map(|_| ()))
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn plan_rejects_3000_levels_without_overflowing() {
        assert!(matches!(
            plan_on_small_stack(3000),
            Err(ImportError::TreeTooDeep(_))
        ));
    }

    #[test]
    fn plan_accepts_100_levels() {
        plan_on_small_stack(100).unwrap();
    }
}
