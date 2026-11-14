//! `[delta]` settings and the delta pass (docs/design/DELTA-STORAGE.md).
//!
//! The pass reads each land branch's mainline from where it last stopped, and with each merge the
//! side-branch changesets it brought in. For every file a changeset writes, it takes the version
//! the same path (or the `copy_from` path) held in the first parent's tree as the base; for every
//! directory the changeset changed, the same directory's tree in the parent; for every new chunk
//! of a large file, the old version's chunk at the same place. It stores the new object as a delta
//! when that saves at least half. It runs under the collection lock ([`crate::gc::GcService`]),
//! records the base in `delta_bases` before it writes, and GC then keeps every recorded base as a
//! strict root.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, ChunkId, FileId, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Entry, Kind, Tree};
use asc_core::path::{Name, RepoPath};
use asc_store::delta::{PlanCache, Skip, pair_chunks};
use asc_store::gc::{GcError, GcRootSource};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use async_trait::async_trait;
use serde::Deserialize;
use sqlx::{AnyPool, Row};

use crate::config::{ByteSize, ConfigError, Secs};
use crate::registry::RepoEntry;

/// Largest file stored whole (SPEC §3.1); larger ones are chunked, and their chunks are deltas.
const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
/// Mainline rows read per query.
const PAGE: i64 = 200;
/// Most distinct ref targets read as tips. Beyond it the hot set is unknown, so the pass does
/// nothing for that repository rather than delta what a tip may hold.
const MAX_TIPS: usize = 50_000;
/// Decoded tip directories remembered per pass; the memo is cleared when it fills.
const MAX_TIP_TREES: usize = 4096;

/// Whether `pos` is at least `distance` mainline positions behind `newest`. A row landed after
/// `newest` was read (`pos > newest`) is never far.
fn far(newest: i64, pos: i64, distance: u64) -> bool {
    newest.saturating_sub(pos).max(0) as u64 >= distance
}

/// The directories of the ref tips' trees read so far this pass (a tree id names its content, so
/// one entry serves every tip and path that reaches it).
#[derive(Default)]
struct TipTrees(HashMap<TreeId, Option<Tree>>);

impl TipTrees {
    /// The entry at `path` under `root`; `None` when absent, below a file or unreadable.
    async fn entry_at(
        &mut self,
        entry: &RepoEntry,
        root: TreeId,
        path: &RepoPath,
    ) -> Result<Option<Entry>, GcError> {
        let mut dir = root;
        let mut comps = path.components().peekable();
        while let Some(comp) = comps.next() {
            if !self.0.contains_key(&dir) {
                if self.0.len() >= MAX_TIP_TREES {
                    self.0.clear();
                }
                let tree = match entry.objects.get(&ObjKey::Tree(dir)).await? {
                    Some(b) => decode_envelope::<Tree>(&b).ok(),
                    None => None,
                };
                self.0.insert(dir, tree);
            }
            let (Some(Some(tree)), Ok(name)) = (self.0.get(&dir), Name::new(comp)) else {
                return Ok(None);
            };
            let Some(e) = tree.get(&name).cloned() else {
                return Ok(None);
            };
            if comps.peek().is_none() {
                return Ok(Some(e));
            }
            if e.kind != Kind::Dir {
                return Ok(None);
            }
            dir = TreeId::from_bytes(e.id);
        }
        Ok(None)
    }
}

/// `[delta]`: delta storage. Off unless `enabled`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeltaSettings {
    /// Runs the delta pass. Reads resolve deltas whether or not this is set.
    pub enabled: bool,
    /// Time between passes.
    pub interval: Secs,
    /// Smaller files are left alone: a delta has a 33-byte header and costs a second read.
    pub min_size: ByteSize,
    /// Most changesets one pass reads per repository; the rest wait for the next pass.
    pub max_changesets: usize,
    /// A changeset becomes a delta candidate once it is this many mainline positions behind its
    /// ref's newest (or `cold_age` has passed). Tip versions stay whole for fast reads.
    pub cold_distance: u64,
    /// A changeset also becomes a candidate once its committer time is this old.
    pub cold_age: Secs,
    /// Leaves what any ref's tip holds whole (branches, stacks, tags and each land branch's newest
    /// mainline row). Not a setting: tests of the delta mechanics turn it off so the newest
    /// version of their fixtures can be a delta too. The tips are read once per pass, so a ref
    /// that moves during the pass is not seen (a revert that makes old content hot again costs
    /// only a slower read), and an object skipped as hot is not revisited: the cursor has moved on.
    #[serde(skip)]
    pub(crate) hot_guard: bool,
}

impl Default for DeltaSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Secs(3600),
            min_size: ByteSize(1024),
            max_changesets: 5000,
            cold_distance: 1000,
            cold_age: Secs(90 * 86_400),
            hot_guard: true,
        }
    }
}

impl DeltaSettings {
    /// The checked settings, or why they are refused.
    pub fn check(&self) -> Result<(), ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(format!("[delta] {m}")));
        if !(60..=7 * 86_400).contains(&self.interval.0) {
            return bad("interval must be 1m to 7d");
        }
        if !(64..=CHUNK_THRESHOLD).contains(&self.min_size.0) {
            return bad("min_size must be 64B to 4MiB");
        }
        if !(1..=1_000_000).contains(&self.max_changesets) {
            return bad("max_changesets must be 1 to 1000000");
        }
        if self.cold_distance > 10_000_000 {
            return bad("cold_distance must be 0 to 10000000");
        }
        if self.cold_age.0 > 3650 * 86_400 {
            return bad("cold_age must be 0 to 3650d");
        }
        Ok(())
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval.0)
    }
}

/// What one pass over one repository did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeltaReport {
    /// Mainline changesets read.
    pub changesets: u64,
    /// Changesets read on the side branches merges brought in.
    pub side: u64,
    /// Files, trees and chunks that had a base in the parent.
    pub candidates: u64,
    /// Objects now stored as deltas.
    pub written: u64,
    /// Of `written`, directory trees.
    pub trees: u64,
    /// Of `written`, large-file chunks.
    pub chunks: u64,
    /// Objects whose delta would not save half.
    pub no_gain: u64,
    /// Objects left whole because the same content is still at a land branch's tip.
    pub hot: u64,
    /// Objects skipped for another reason (packed, already a delta, base unusable).
    pub skipped: u64,
    /// Stored bytes of the written objects before and after.
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Deltas turned back into full values because their base was purged.
    pub expanded: u64,
}

impl DeltaReport {
    /// Of `written`, files stored whole (not trees or chunks).
    pub fn files(&self) -> u64 {
        self.written - self.trees - self.chunks
    }
}

/// What `delta status` shows for one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeltaStatus {
    /// Objects recorded as deltas.
    pub deltas: u64,
    /// Distinct bases they name.
    pub bases: u64,
    /// Mainline changesets not yet read, over all land branches.
    pub backlog: u64,
}

fn sql(what: &str, e: impl std::fmt::Display) -> GcError {
    GcError::Marks(format!("{what}: {e}"))
}

/// Every recorded base of one namespace, as strict GC roots.
pub struct DeltaRoots {
    pub pool: AnyPool,
    pub ns: Vec<u8>,
}

#[async_trait]
impl GcRootSource for DeltaRoots {
    async fn roots(&self, ns: &[u8]) -> Result<Vec<ObjKey>, GcError> {
        if ns != self.ns.as_slice() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query("SELECT DISTINCT kind, base FROM delta_bases WHERE ns = $1")
            .bind(self.ns.clone())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| sql("delta_bases", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let k: i16 = r.try_get(0).map_err(|e| sql("delta_bases", e))?;
            let b: Vec<u8> = r.try_get(1).map_err(|e| sql("delta_bases", e))?;
            out.push(row_key(k, &b)?);
        }
        Ok(out)
    }
}

/// The `kind` column of `delta_bases`: 0 file, 1 tree, 2 chunk. A delta and its base are always
/// of the same kind.
fn row_kind(key: &ObjKey) -> Option<i16> {
    match key {
        ObjKey::File(_) => Some(0),
        ObjKey::Tree(_) => Some(1),
        ObjKey::Chunk(_) => Some(2),
        _ => None,
    }
}

/// The key a `delta_bases` row names.
fn row_key(kind: i16, id: &[u8]) -> Result<ObjKey, GcError> {
    let a: [u8; 32] = id
        .try_into()
        .map_err(|_| GcError::Marks("delta_bases: an id is not 32 bytes".into()))?;
    match kind {
        0 => Ok(ObjKey::File(FileId::from_bytes(a))),
        1 => Ok(ObjKey::Tree(TreeId::from_bytes(a))),
        2 => Ok(ObjKey::Chunk(ChunkId::from_bytes(a))),
        k => Err(GcError::Marks(format!("delta_bases: unknown kind {k}"))),
    }
}

/// Before a sweep: every delta whose base an operator purged is turned back into a full value
/// and its row dropped, so the base is no longer a root and the purge can delete it.
pub async fn release_purged(pool: &AnyPool, entry: &RepoEntry) -> Result<u64, GcError> {
    let ns = entry.ns.as_bytes().to_vec();
    let purged: Vec<String> = sqlx::query_scalar("SELECT key FROM gc_purge WHERE ns = $1")
        .bind(ns.clone())
        .fetch_all(pool)
        .await
        .map_err(|e| sql("gc_purge", e))?;
    let mut expanded = 0;
    for key in purged.iter().filter_map(|k| ObjKey::parse(k)) {
        let (Some(kind), Some(base)) = (row_kind(&key), key.delta_id()) else {
            continue;
        };
        let deps: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT file FROM delta_bases WHERE ns = $1 AND base = $2 AND kind = $3",
        )
        .bind(ns.clone())
        .bind(base.to_vec())
        .bind(kind)
        .fetch_all(pool)
        .await
        .map_err(|e| sql("delta_bases", e))?;
        for d in deps {
            if entry.objects.expand_delta(row_key(kind, &d)?).await? {
                expanded += 1;
            }
            sqlx::query("DELETE FROM delta_bases WHERE ns = $1 AND file = $2")
                .bind(ns.clone())
                .bind(d)
                .execute(pool)
                .await
                .map_err(|e| sql("delta_bases", e))?;
        }
    }
    Ok(expanded)
}

/// Copies to a fork's namespace: the fork has no `delta_bases` rows, so each copied delta is
/// rewritten there in full. The fork's own delta pass makes deltas again.
pub async fn expand_copied(
    src: &RepoEntry,
    dst: &RepoEntry,
    keys: &[ObjKey],
) -> Result<u64, GcError> {
    let mut n = 0;
    for key in keys {
        if key.delta_id().is_none() || src.objects.delta_base_of(*key).await?.is_none() {
            continue;
        }
        let Some(full) = src.objects.get_stored(key).await? else {
            continue;
        };
        dst.objects.replace_loose(key, full).await?;
        n += 1;
    }
    Ok(n)
}

async fn changeset(entry: &RepoEntry, id: ChangesetId) -> Result<Option<Changeset>, GcError> {
    let key = ObjKey::Changeset(id);
    let Some(b) = entry.objects.get(&key).await? else {
        return Ok(None);
    };
    decode_envelope(&b)
        .map(Some)
        .map_err(|e| GcError::Undecodable {
            key,
            why: e.to_string(),
        })
}

/// The entry at `path` under `root`, or `None` when absent, below a file, or unreadable.
async fn lookup(
    entry: &RepoEntry,
    root: TreeId,
    path: &RepoPath,
) -> Result<Option<Entry>, GcError> {
    let mut tree_id = root;
    let mut comps = path.components().peekable();
    while let Some(comp) = comps.next() {
        let Some(b) = entry.objects.get(&ObjKey::Tree(tree_id)).await? else {
            return Ok(None);
        };
        let Ok(tree) = decode_envelope::<Tree>(&b) else {
            return Ok(None);
        };
        let Ok(name) = Name::new(comp) else {
            return Ok(None);
        };
        let Some(e) = tree.get(&name).cloned() else {
            return Ok(None);
        };
        if comps.peek().is_none() {
            return Ok(Some(e));
        }
        if e.kind != Kind::Dir {
            return Ok(None);
        }
        tree_id = TreeId::from_bytes(e.id);
    }
    Ok(None)
}

/// One pass over `entry`. The caller holds the collection lock.
pub async fn pass(
    pool: &AnyPool,
    entry: &RepoEntry,
    s: &DeltaSettings,
) -> Result<DeltaReport, GcError> {
    let ns = entry.ns.as_bytes().to_vec();
    let mut rep = DeltaReport::default();
    // A base an operator purged must not gain dependents.
    let purged: HashSet<ObjKey> =
        sqlx::query_scalar::<_, String>("SELECT key FROM gc_purge WHERE ns = $1")
            .bind(ns.clone())
            .fetch_all(pool)
            .await
            .map_err(|e| sql("gc_purge", e))?
            .iter()
            .filter_map(|k| ObjKey::parse(k))
            .collect();
    let refs: Vec<String> = sqlx::query_scalar("SELECT DISTINCT ref FROM mainline WHERE ns = $1")
        .bind(ns.clone())
        .fetch_all(pool)
        .await
        .map_err(|e| sql("mainline", e))?;
    let mut budget = s.max_changesets as u64;
    // The trees at every ref's tip and at each land branch's newest row, read once: what they
    // hold is hot.
    let mut tip_ids: HashSet<[u8; 32]> = HashSet::new();
    let mut newest: HashMap<String, i64> = HashMap::new();
    for r in &refs {
        let row = sqlx::query(
            "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 ORDER BY pos DESC LIMIT 1",
        )
        .bind(ns.clone())
        .bind(r.clone())
        .fetch_optional(pool)
        .await
        .map_err(|e| sql("mainline", e))?;
        let Some(row) = row else { continue };
        newest.insert(
            r.clone(),
            row.try_get("pos").map_err(|e| sql("mainline", e))?,
        );
        let cs: Vec<u8> = row.try_get("cs").map_err(|e| sql("mainline", e))?;
        let cs: [u8; 32] = cs
            .as_slice()
            .try_into()
            .map_err(|_| sql("mainline", "a changeset id is not 32 bytes"))?;
        tip_ids.insert(cs);
    }
    let mut tips: Vec<TreeId> = Vec::new();
    if s.hot_guard {
        let targets: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT DISTINCT target FROM refs WHERE repo_id = $1 LIMIT $2")
                .bind(entry.key.0)
                .bind(i64::try_from(MAX_TIPS + 1).unwrap_or(i64::MAX))
                .fetch_all(pool)
                .await
                .map_err(|e| sql("refs", e))?;
        if targets.len() > MAX_TIPS {
            return Ok(rep);
        }
        tip_ids.extend(
            targets
                .iter()
                .filter_map(|t| <[u8; 32]>::try_from(t.as_slice()).ok()),
        );
        let mut roots: HashSet<TreeId> = HashSet::new();
        for id in tip_ids {
            if let Some(c) = changeset(entry, ChangesetId::from_bytes(id)).await?
                && roots.insert(c.root)
            {
                tips.push(c.root);
            }
        }
    }
    let mut trees = TipTrees::default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    // Side-branch changesets already read this pass, over every merge and ref.
    let mut seen = HashSet::new();
    // Values and chains read this pass: the parent's tree or file is often the next plan's base.
    let mut cache = PlanCache::default();
    let ctx = Ctx {
        pool,
        entry,
        s,
        purged: &purged,
        tips: &tips,
    };
    'refs: for r in refs {
        let mut at: i64 =
            sqlx::query_scalar("SELECT pos FROM delta_cursor WHERE ns = $1 AND ref = $2")
                .bind(ns.clone())
                .bind(r.clone())
                .fetch_optional(pool)
                .await
                .map_err(|e| sql("delta_cursor", e))?
                .unwrap_or(-1);
        let newest = newest.get(&r).copied().unwrap_or(at);
        let mut hot_row = false;
        while budget > 0 && !hot_row {
            let rows = sqlx::query(
                "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 AND pos > $3 \
                 AND pos <= $4 ORDER BY pos LIMIT $5",
            )
            .bind(ns.clone())
            .bind(r.clone())
            .bind(at)
            .bind(newest)
            .bind(PAGE.min(budget as i64))
            .fetch_all(pool)
            .await
            .map_err(|e| sql("mainline", e))?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let pos: i64 = row.try_get("pos").map_err(|e| sql("mainline", e))?;
                let cs: Vec<u8> = row.try_get("cs").map_err(|e| sql("mainline", e))?;
                let cs: [u8; 32] = cs
                    .as_slice()
                    .try_into()
                    .map_err(|_| sql("mainline", "a changeset id is not 32 bytes"))?;
                let cs = ChangesetId::from_bytes(cs);
                // The mainline is read oldest first, so the first row still hot ends the ref's
                // pass; the cursor stays before it.
                let cold = far(newest, pos, s.cold_distance)
                    || match changeset(entry, cs).await? {
                        Some(c) => now.saturating_sub(c.committer.time()) >= s.cold_age.0 as i64,
                        None => true,
                    };
                if !cold {
                    hot_row = true;
                    break;
                }
                // The first changeset of a pass has the whole budget; a merge whose side history
                // is larger than that is read as far as the budget goes and then passed.
                let fresh = budget == s.max_changesets as u64;
                let parents = one_changeset(&ctx, cs, &mut rep, &mut cache, &mut trees).await?;
                if parents.len() > 1 {
                    let merged = parents[1..].to_vec();
                    let walk = Walk {
                        pool,
                        entry,
                        s,
                        r: &r,
                        purged: &purged,
                        tips: &tips,
                    };
                    let done = walk
                        .run(
                            merged,
                            &mut seen,
                            &mut budget,
                            &mut rep,
                            (&mut cache, &mut trees),
                        )
                        .await?;
                    if !done && !fresh {
                        // The cursor stays before the merge; the next pass starts with it.
                        set_cursor(pool, &ns, &r, at).await?;
                        break 'refs;
                    }
                }
                rep.changesets += 1;
                budget = budget.saturating_sub(1);
                at = pos;
            }
            set_cursor(pool, &ns, &r, at).await?;
        }
    }
    Ok(rep)
}

/// What one changeset's delta writes read: the pool, the repository, the settings, the purged
/// bases and the land branches' tip roots.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    pool: &'a AnyPool,
    entry: &'a RepoEntry,
    s: &'a DeltaSettings,
    purged: &'a HashSet<ObjKey>,
    tips: &'a [TreeId],
}

/// New-file bases tried per file.
const MAX_SIBLINGS: usize = 4;

/// The part of a file name after its last dot (none for a leading dot only), so `a.rs` pairs with
/// `b.rs` and names without one pair with each other.
fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&c| c == b'.') {
        Some(0) | None => b"",
        Some(i) => &name[i + 1..],
    }
}

async fn set_cursor(pool: &AnyPool, ns: &[u8], r: &str, pos: i64) -> Result<(), GcError> {
    sqlx::query(
        "INSERT INTO delta_cursor (ns, ref, pos) VALUES ($1, $2, $3) \
         ON CONFLICT (ns, ref) DO UPDATE SET pos = excluded.pos",
    )
    .bind(ns.to_vec())
    .bind(r.to_owned())
    .bind(pos)
    .execute(pool)
    .await
    .map_err(|e| sql("delta_cursor", e))?;
    Ok(())
}

/// The walk of the changesets a merge brought in: from its other parents back to `r`'s mainline,
/// each against its own first parent, so a version written on a side branch gets a base too.
struct Walk<'a> {
    pool: &'a AnyPool,
    entry: &'a RepoEntry,
    s: &'a DeltaSettings,
    r: &'a str,
    purged: &'a HashSet<ObjKey>,
    tips: &'a [TreeId],
}

impl Walk<'_> {
    /// Reads side changesets while `budget` lasts, each at most once per pass (`seen`). `true`
    /// when the walk reached the mainline everywhere, `false` when the budget ran out first.
    async fn run(
        &self,
        merged: Vec<ChangesetId>,
        seen: &mut HashSet<ChangesetId>,
        budget: &mut u64,
        rep: &mut DeltaReport,
        (cache, trees): (&mut PlanCache, &mut TipTrees),
    ) -> Result<bool, GcError> {
        let (pool, entry) = (self.pool, self.entry);
        let ns = entry.ns.as_bytes().to_vec();
        let mut todo = merged;
        while let Some(id) = todo.pop() {
            if seen.contains(&id) {
                continue;
            }
            if *budget == 0 {
                return Ok(false);
            }
            seen.insert(id);
            let on_mainline: Option<i64> = sqlx::query_scalar(
                "SELECT pos FROM mainline WHERE ns = $1 AND ref = $2 AND cs = $3",
            )
            .bind(ns.clone())
            .bind(self.r.to_owned())
            .bind(id.as_bytes().to_vec())
            .fetch_optional(pool)
            .await
            .map_err(|e| sql("mainline", e))?;
            if on_mainline.is_some() {
                continue;
            }
            *budget -= 1;
            rep.side += 1;
            todo.extend(
                one_changeset(
                    &Ctx {
                        pool,
                        entry,
                        s: self.s,
                        purged: self.purged,
                        tips: self.tips,
                    },
                    id,
                    rep,
                    cache,
                    trees,
                )
                .await?,
            );
        }
        Ok(true)
    }
}

/// Stores what `id` writes as deltas against its first parent: each file against the version
/// at the same path (or its copy source) in every parent, keeping the smallest, a new file
/// against similar files of the same directory, each changed directory's tree against the same directory's old tree, and
/// each new chunk of a large file against the old version's chunk at the same place. Returns its
/// parents (none when the changeset is missing).
async fn one_changeset(
    ctx: &Ctx<'_>,
    id: ChangesetId,
    rep: &mut DeltaReport,
    cache: &mut PlanCache,
    trees: &mut TipTrees,
) -> Result<Vec<ChangesetId>, GcError> {
    let Ctx {
        pool,
        entry,
        s,
        purged,
        tips,
    } = *ctx;
    let Some(cs) = changeset(entry, id).await? else {
        return Ok(Vec::new());
    };
    let Some(parent) = cs.parents.first() else {
        return Ok(Vec::new());
    };
    let Some(parent) = changeset(entry, *parent).await? else {
        return Ok(cs.parents.clone());
    };
    // A merge's file may come from any side: every parent's version is a candidate base.
    let mut roots = vec![parent.root];
    for p in &cs.parents[1..] {
        if let Some(c) = changeset(entry, *p).await? {
            roots.push(c.root);
        }
    }
    let mut d = Deltas {
        pool,
        entry,
        purged,
        tips,
        trees,
        rep,
        cache,
    };
    // Every directory above a changed path changed too; the root is handled on its own.
    let mut dirs: HashSet<RepoPath> = HashSet::new();
    for (path, change) in cs.changes.as_slice() {
        let mut up = path.parent();
        while let Some(p) = up {
            up = p.parent();
            if !dirs.insert(p) {
                break;
            }
        }
        let Change::Write {
            kind,
            file,
            size,
            copy_from,
            ..
        } = change
        else {
            continue;
        };
        if *kind == Kind::Symlink || *size < s.min_size.0 {
            continue;
        }
        let from = copy_from.as_ref().unwrap_or(path);
        let mut olds = Vec::new();
        for root in &roots {
            if let Some(old) = lookup(entry, *root, from).await?
                && matches!(old.kind, Kind::File | Kind::Exec)
            {
                olds.push(old);
            }
        }
        // A version some parent already had is not new storage.
        if olds.iter().any(|o| o.id == *file.as_bytes()) {
            continue;
        }
        let probe = Some((Some(path), file.as_bytes()));
        if *size > CHUNK_THRESHOLD {
            if let Some(old) = olds.first().filter(|o| o.size > CHUNK_THRESHOLD) {
                if d.is_hot(Some(path), file.as_bytes()).await? {
                    d.rep.hot += 1;
                } else {
                    d.chunks(*file, FileId::from_bytes(old.id)).await?;
                }
            }
            continue;
        }
        let bases: Vec<ObjKey> = if olds.is_empty() {
            d.siblings(parent.root, path, *size, s.min_size.0).await?
        } else {
            olds.iter()
                .filter(|o| o.size <= CHUNK_THRESHOLD)
                .map(|o| ObjKey::File(FileId::from_bytes(o.id)))
                .collect()
        };
        d.try_best(ObjKey::File(*file), &bases, probe).await?;
    }
    if cs.root != parent.root {
        d.try_one(
            ObjKey::Tree(cs.root),
            ObjKey::Tree(parent.root),
            Some((None, cs.root.as_bytes())),
        )
        .await?;
    }
    for dir in &dirs {
        let (Some(new), Some(old)) = (
            lookup(entry, cs.root, dir).await?,
            lookup(entry, parent.root, dir).await?,
        ) else {
            continue;
        };
        if new.kind == Kind::Dir && old.kind == Kind::Dir && new.id != old.id {
            let (n, o) = (TreeId::from_bytes(new.id), TreeId::from_bytes(old.id));
            d.try_one(ObjKey::Tree(n), ObjKey::Tree(o), Some((Some(dir), &new.id)))
                .await?;
        }
    }
    Ok(cs.parents.clone())
}

/// What a tip must not hold for an object to become a delta: its path (`None`: the root) and id.
type Probe<'p> = (Option<&'p RepoPath>, &'p [u8; 32]);

/// The delta writes of one changeset.
struct Deltas<'a> {
    pool: &'a AnyPool,
    entry: &'a RepoEntry,
    purged: &'a HashSet<ObjKey>,
    /// Roots of the ref tips.
    tips: &'a [TreeId],
    trees: &'a mut TipTrees,
    rep: &'a mut DeltaReport,
    cache: &'a mut PlanCache,
}

impl Deltas<'_> {
    /// Pairs the chunks of a large file's new version with the old version's: a chunk both share
    /// needs nothing, and each new chunk takes the next old chunk the new version dropped, in
    /// order between the chunks they share (content-defined boundaries line up again after an
    /// edit).
    async fn chunks(&mut self, file: FileId, base: FileId) -> Result<(), GcError> {
        let (Some(new), Some(old)) = (self.chunk_list(file).await?, self.chunk_list(base).await?)
        else {
            return Ok(());
        };
        for (c, o) in pair_chunks(&new, &old) {
            self.try_one(ObjKey::Chunk(c), ObjKey::Chunk(o), None)
                .await?;
        }
        Ok(())
    }

    async fn chunk_list(&self, f: FileId) -> Result<Option<ChunkList>, GcError> {
        let Some(b) = self.entry.objects.get(&ObjKey::ChunkList(f)).await? else {
            return Ok(None);
        };
        Ok(decode_envelope(&b).ok())
    }

    /// The bases a new file is tried against, as git's window search does: files of the first
    /// parent's same directory with the same extension and a size within half to double, the
    /// nearest sizes first, at most [`MAX_SIBLINGS`].
    async fn siblings(
        &self,
        root: TreeId,
        path: &RepoPath,
        size: u64,
        min_size: u64,
    ) -> Result<Vec<ObjKey>, GcError> {
        let dir = match path.parent() {
            Some(p) => match lookup(self.entry, root, &p).await? {
                Some(e) if e.kind == Kind::Dir => TreeId::from_bytes(e.id),
                _ => return Ok(Vec::new()),
            },
            None => root,
        };
        let Some(b) = self.entry.objects.get(&ObjKey::Tree(dir)).await? else {
            return Ok(Vec::new());
        };
        let Ok(tree) = decode_envelope::<Tree>(&b) else {
            return Ok(Vec::new());
        };
        let name = path.file_name();
        let ext = extension(name.as_bytes());
        let mut near: Vec<(u64, ObjKey)> = tree
            .entries()
            .iter()
            .filter(|e| {
                matches!(e.kind, Kind::File | Kind::Exec)
                    && e.size >= min_size
                    && e.size <= CHUNK_THRESHOLD
                    && e.size * 2 >= size
                    && e.size <= size * 2
                    && extension(e.name.as_bytes()) == ext
            })
            .map(|e| {
                (
                    e.size.abs_diff(size),
                    ObjKey::File(FileId::from_bytes(e.id)),
                )
            })
            .collect();
        near.sort_by_key(|(d, _)| *d);
        Ok(near
            .into_iter()
            .take(MAX_SIBLINGS)
            .map(|(_, k)| k)
            .collect())
    }

    /// Whether a land branch's tip holds the same content at `path` (`None`: the root): the same
    /// file id, or the same tree id for a directory.
    async fn is_hot(&mut self, path: Option<&RepoPath>, id: &[u8; 32]) -> Result<bool, GcError> {
        for tip in self.tips {
            let Some(path) = path else {
                if tip.as_bytes() == id {
                    return Ok(true);
                }
                continue;
            };
            if self
                .trees
                .entry_at(self.entry, *tip, path)
                .await?
                .is_some_and(|e| &e.id == id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Stores `key` as a delta against `base` when that saves half.
    async fn try_one(
        &mut self,
        key: ObjKey,
        base: ObjKey,
        probe: Option<Probe<'_>>,
    ) -> Result<(), GcError> {
        self.try_best(key, &[base], probe).await
    }

    /// Stores `key` as a delta against whichever of `bases` gives the smallest one, when that
    /// saves half. With a `probe`, an object the ref tips hold is left whole; the tips are asked
    /// only once the cheap tests have passed.
    async fn try_best(
        &mut self,
        key: ObjKey,
        bases: &[ObjKey],
        probe: Option<Probe<'_>>,
    ) -> Result<(), GcError> {
        let bases: Vec<ObjKey> = bases
            .iter()
            .copied()
            .filter(|b| *b != key && !self.purged.contains(b))
            .collect();
        if bases.is_empty() {
            return Ok(());
        }
        let (Some(kind), Some(id)) = (row_kind(&key), key.delta_id()) else {
            return Ok(());
        };
        let hot = match probe {
            Some((path, hid)) => self.is_hot(path, hid).await?,
            None => false,
        };
        let rep = &mut *self.rep;
        rep.candidates += 1;
        let outcome = if hot {
            Err(Skip::Hot)
        } else {
            self.entry
                .objects
                .plan_best(key, &bases, self.cache)
                .await?
        };
        let planned = match outcome {
            Ok(p) => p,
            Err(Skip::Hot) => {
                rep.hot += 1;
                return Ok(());
            }
            Err(Skip::NoGain) => {
                rep.no_gain += 1;
                return Ok(());
            }
            Err(_) => {
                rep.skipped += 1;
                return Ok(());
            }
        };
        let (base, base_id) = (planned.base, planned.base.delta_id().unwrap_or_default());
        // The row first: from here on GC keeps the base, whatever happens to the write.
        sqlx::query(
            "INSERT INTO delta_bases (ns, file, base, kind) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (ns, file) DO UPDATE SET base = excluded.base, kind = excluded.kind",
        )
        .bind(self.entry.ns.as_bytes().to_vec())
        .bind(id.to_vec())
        .bind(base_id.to_vec())
        .bind(kind)
        .execute(self.pool)
        .await
        .map_err(|e| sql("delta_bases", e))?;
        // The plan may have used a value remembered earlier in the pass: a base removed before
        // the row went in is gone for good, so the full value stays.
        if !self.entry.objects.has_many(&[base]).await?[0] {
            sqlx::query("DELETE FROM delta_bases WHERE ns = $1 AND file = $2")
                .bind(self.entry.ns.as_bytes().to_vec())
                .bind(id.to_vec())
                .execute(self.pool)
                .await
                .map_err(|e| sql("delta_bases", e))?;
            rep.skipped += 1;
            return Ok(());
        }
        self.entry.objects.store_delta(&planned).await?;
        rep.written += 1;
        match kind {
            1 => rep.trees += 1,
            2 => rep.chunks += 1,
            _ => {}
        }
        rep.bytes_before += planned.was;
        rep.bytes_after += planned.stored.len() as u64;
        Ok(())
    }
}

/// Rewrites every delta of `entry` as a full value and drops its row: the rollback of delta
/// storage. The cursors stay, so a pass that runs later does not redo old history.
pub async fn expand_all(pool: &AnyPool, entry: &RepoEntry) -> Result<u64, GcError> {
    let ns = entry.ns.as_bytes().to_vec();
    let rows = sqlx::query("SELECT kind, file FROM delta_bases WHERE ns = $1")
        .bind(ns.clone())
        .fetch_all(pool)
        .await
        .map_err(|e| sql("delta_bases", e))?;
    let mut n = 0;
    for r in rows {
        let k: i16 = r.try_get(0).map_err(|e| sql("delta_bases", e))?;
        let f: Vec<u8> = r.try_get(1).map_err(|e| sql("delta_bases", e))?;
        if entry.objects.expand_delta(row_key(k, &f)?).await? {
            n += 1;
        }
        sqlx::query("DELETE FROM delta_bases WHERE ns = $1 AND file = $2")
            .bind(ns.clone())
            .bind(f)
            .execute(pool)
            .await
            .map_err(|e| sql("delta_bases", e))?;
    }
    Ok(n)
}

/// The delta counts and backlog of `entry`.
pub async fn status(pool: &AnyPool, entry: &RepoEntry) -> Result<DeltaStatus, GcError> {
    let ns = entry.ns.as_bytes().to_vec();
    let row = sqlx::query(
        "SELECT COUNT(*) AS n, COUNT(DISTINCT base) AS b FROM delta_bases WHERE ns = $1",
    )
    .bind(ns.clone())
    .fetch_one(pool)
    .await
    .map_err(|e| sql("delta_bases", e))?;
    let backlog: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mainline m WHERE m.ns = $1 AND m.pos > COALESCE(\
         (SELECT c.pos FROM delta_cursor c WHERE c.ns = m.ns AND c.ref = m.ref), -1)",
    )
    .bind(ns)
    .fetch_one(pool)
    .await
    .map_err(|e| sql("mainline", e))?;
    Ok(DeltaStatus {
        deltas: row
            .try_get::<i64, _>("n")
            .map_err(|e| sql("delta_bases", e))? as u64,
        bases: row
            .try_get::<i64, _>("b")
            .map_err(|e| sql("delta_bases", e))? as u64,
        backlog: backlog as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_name_keys_of_their_own_kind() {
        let id = [3u8; 32];
        for (k, key) in [
            (0, ObjKey::File(FileId::from_bytes(id))),
            (1, ObjKey::Tree(TreeId::from_bytes(id))),
            (2, ObjKey::Chunk(ChunkId::from_bytes(id))),
        ] {
            assert_eq!(row_kind(&key), Some(k));
            assert_eq!(row_key(k, &id).unwrap(), key);
        }
        assert!(row_key(3, &id).is_err());
        assert!(row_key(0, &id[..31]).is_err());
        assert_eq!(row_kind(&ObjKey::ChunkList(FileId::from_bytes(id))), None);
    }

    #[tokio::test]
    async fn a_base_removed_after_a_remembered_plan_keeps_the_full_value() {
        use asc_store::codec::encode_stored;
        use asc_store::objects::Objects;
        let reg = crate::ingest::testutil::registry().await;
        let entry = crate::ingest::testutil::repo(&reg).await;
        let text = |edit: usize| -> Vec<u8> {
            (0..3000)
                .map(|i| match i == edit {
                    true => format!("fn edited_{i}() {{}}\n"),
                    false => format!("fn item_{i}() -> u32 {{ {i} }}\n"),
                })
                .collect::<String>()
                .into_bytes()
        };
        let mut keys = Vec::new();
        for v in [text(usize::MAX), text(1), text(2)] {
            let k = ObjKey::File(FileId::of(&v));
            entry.objects.put(&k, encode_stored(&v)).await.unwrap();
            keys.push(k);
        }
        let (base, a, b) = (keys[0], keys[1], keys[2]);
        let mut cache = PlanCache::default();
        let mut rep = DeltaReport::default();
        let purged = HashSet::new();
        // A plan of `a` remembers `base`; then `base` goes.
        assert!(
            entry
                .objects
                .plan_delta_cached(a, base, &mut cache)
                .await
                .unwrap()
                .is_ok()
        );
        entry.objects.delete(&base).await.unwrap();
        let mut d = Deltas {
            pool: reg.pool(),
            entry: &entry,
            purged: &purged,
            tips: &[],
            trees: &mut TipTrees::default(),
            rep: &mut rep,
            cache: &mut cache,
        };
        d.try_one(b, base, None).await.unwrap();
        assert_eq!((rep.candidates, rep.skipped, rep.written), (1, 1, 0));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delta_bases")
            .fetch_one(reg.pool())
            .await
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(entry.objects.delta_base_of(b).await.unwrap(), None);
        assert_eq!(entry.objects.get(&b).await.unwrap().unwrap(), text(2));
    }

    #[test]
    fn the_defaults_are_valid_and_nonsense_is_refused() {
        DeltaSettings::default().check().unwrap();
        assert!(!DeltaSettings::default().enabled);
        let s = |f: fn(&mut DeltaSettings)| {
            let mut s = DeltaSettings::default();
            f(&mut s);
            s.check().is_err()
        };
        assert!(s(|s| s.interval = Secs(1)));
        assert!(s(|s| s.min_size = ByteSize(1)));
        assert!(s(|s| s.min_size = ByteSize(CHUNK_THRESHOLD + 1)));
        assert!(s(|s| s.max_changesets = 0));
        assert!(s(|s| s.cold_distance = 10_000_001));
        assert!(s(|s| s.cold_age = Secs(3650 * 86_400 + 1)));
        assert!(!s(|s| s.cold_distance = 0));
        assert!(!s(|s| s.cold_age = Secs(0)));
        assert_eq!(DeltaSettings::default().cold_distance, 1000);
        assert_eq!(DeltaSettings::default().cold_age, Secs(90 * 86_400));
        let t: DeltaSettings = toml::from_str("enabled = true\ninterval = \"15m\"").unwrap();
        assert!(t.enabled);
        assert!(t.hot_guard);
        assert_eq!(t.interval(), Duration::from_secs(900));
        assert!(toml::from_str::<DeltaSettings>("enable = true").is_err());
    }
}

#[cfg(test)]
mod cold_tests {
    use super::*;
    use crate::ingest::testutil::{registry, repo, tree};
    use asc_core::sig::Signature;
    use asc_engine::commit::{NewChangeset, PathPolicy};

    const DAY: i64 = 86_400;

    /// The files one changeset writes and its committer time.
    type Step = (Vec<(&'static str, Vec<u8>)>, i64);

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A source file of about 90 KiB; `edit` changes one line.
    fn version(edit: usize) -> Vec<u8> {
        (0..3000)
            .map(|i| match i == edit {
                true => format!("fn edited_{i}() -> u32 {{ {i} }}\n"),
                false => format!("fn item_{i}() -> u32 {{ {} }}\n", i * 3),
            })
            .collect::<String>()
            .into_bytes()
    }

    /// A changeset committed at `time` that writes `files` over its first parent.
    async fn commit_at(
        e: &RepoEntry,
        parent: Option<ChangesetId>,
        files: &[(&str, &[u8])],
        time: i64,
    ) -> ChangesetId {
        let engine = e.backend.engine();
        let base = match parent {
            Some(p) => engine.get_changeset(p).await.unwrap().root,
            None => asc_engine::Engine::<
                asc_store::bucket::BucketObjects,
                asc_store::sqlrefs::SqlRefs,
            >::empty_tree_id(),
        };
        let root = tree(e, base, files).await;
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), time, 0).unwrap();
        engine
            .create_changeset(
                NewChangeset {
                    parents: parent.into_iter().collect(),
                    author: sig.clone(),
                    committer: sig,
                    message: b"m".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    /// `master` of `steps` (files, time), one changeset each, in the mainline.
    async fn history(
        reg: &crate::registry::Registry,
        e: &RepoEntry,
        steps: &[Step],
    ) -> Vec<ChangesetId> {
        let ns = e.ns.as_bytes().to_vec();
        let mut out: Vec<ChangesetId> = Vec::new();
        for (pos, (files, time)) in steps.iter().enumerate() {
            let files: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
            let cs = commit_at(e, out.last().copied(), &files, *time).await;
            sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4)")
                .bind(ns.clone())
                .bind("refs/heads/master")
                .bind(pos as i64)
                .bind(cs.as_bytes().to_vec())
                .execute(reg.pool())
                .await
                .unwrap();
            out.push(cs);
        }
        out
    }

    async fn cursor(reg: &crate::registry::Registry) -> Option<i64> {
        sqlx::query_scalar("SELECT pos FROM delta_cursor")
            .fetch_optional(reg.pool())
            .await
            .unwrap()
    }

    async fn rows(reg: &crate::registry::Registry) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM delta_bases")
            .fetch_one(reg.pool())
            .await
            .unwrap()
    }

    fn on(distance: u64) -> DeltaSettings {
        DeltaSettings {
            enabled: true,
            cold_distance: distance,
            ..DeltaSettings::default()
        }
    }

    fn three(time: i64) -> Vec<Step> {
        (0..3)
            .map(|i| {
                (
                    vec![("f", version(if i == 0 { usize::MAX } else { i }))],
                    time,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_recent_changeset_is_left_full() {
        let reg = registry().await;
        let e = repo(&reg).await;
        history(&reg, &e, &three(now())).await;
        let rep = pass(reg.pool(), &e, &on(1000)).await.unwrap();
        assert_eq!((rep.changesets, rep.candidates, rep.written), (0, 0, 0));
        assert_eq!((rows(&reg).await, cursor(&reg).await), (0, Some(-1)));
    }

    #[tokio::test]
    async fn a_changeset_past_the_distance_is_planned_and_the_cursor_stops_at_the_first_hot_row() {
        let reg = registry().await;
        let e = repo(&reg).await;
        history(&reg, &e, &three(now())).await;
        // Newest is 2: position 0 is 2 behind, 1 is 1 behind (hot).
        let rep = pass(reg.pool(), &e, &on(2)).await.unwrap();
        assert_eq!((rep.changesets, rep.written), (1, 0));
        assert_eq!(cursor(&reg).await, Some(0));
        // Resumes from there: position 1 is now cold, position 2 (the tip) is not.
        let rep = pass(reg.pool(), &e, &on(1)).await.unwrap();
        assert_eq!((rep.changesets, rep.written), (1, 1), "{rep:?}");
        assert_eq!(cursor(&reg).await, Some(1));
        assert_eq!(rows(&reg).await, 1);
    }

    #[tokio::test]
    async fn a_changeset_past_the_age_is_planned() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let old = now() - 100 * DAY;
        history(&reg, &e, &three(old)).await;
        // Within the distance, but past the 90 day age.
        let rep = pass(reg.pool(), &e, &on(1000)).await.unwrap();
        assert_eq!(rep.changesets, 3);
        assert_eq!(rep.written, 1, "{rep:?}");
        // The tip's own version stays whole.
        assert_eq!(rep.hot, 2, "{rep:?}");
    }

    #[tokio::test]
    async fn a_file_and_a_directory_still_at_the_tip_are_skipped() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let old = now() - 100 * DAY;
        let steps = vec![
            (vec![("d/f", version(usize::MAX))], old),
            (vec![("d/f", version(1))], old),
            (vec![("z", b"z".to_vec())], old),
        ];
        history(&reg, &e, &steps).await;
        let rep = pass(reg.pool(), &e, &on(1000)).await.unwrap();
        assert_eq!(rows(&reg).await, 0, "{rep:?}");
        // The file d/f, its directory d, and the tip's own root.
        assert_eq!((rep.hot, rep.written), (3, 0), "{rep:?}");
    }

    #[tokio::test]
    async fn a_non_land_branch_tip_keeps_its_content_whole() {
        let reg = registry().await;
        let e = repo(&reg).await;
        let old = now() - 100 * DAY;
        let steps: Vec<Step> = (0..4)
            .map(|i| {
                (
                    vec![("f", version(if i == 0 { usize::MAX } else { i }))],
                    old,
                )
            })
            .collect();
        let ids = history(&reg, &e, &steps).await;
        // A branch outside the mainline table, at the second changeset.
        sqlx::query("INSERT INTO refs (repo_id, name, target, version) VALUES ($1, $2, $3, 1)")
            .bind(e.key.0)
            .bind("refs/heads/topic")
            .bind(ids[1].as_bytes().to_vec())
            .execute(reg.pool())
            .await
            .unwrap();
        let rep = pass(reg.pool(), &e, &on(1000)).await.unwrap();
        // Version 1 (file and root) and the mainline tip (file and root) are hot; only version 2
        // becomes a delta.
        assert_eq!((rep.written, rep.hot), (1, 4), "{rep:?}");
        assert_eq!(rows(&reg).await, 1);
    }

    #[test]
    fn a_row_landed_after_the_snapshot_is_never_far() {
        assert!(far(10, 0, 10));
        assert!(!far(10, 1, 10));
        // `newest` was read before this row landed: no wrap to a huge distance.
        assert!(!far(2, 5, 1));
        assert!(!far(i64::MIN, i64::MAX, 1));
    }
}
