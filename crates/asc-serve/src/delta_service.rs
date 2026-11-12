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

use std::collections::HashSet;
use std::time::Duration;

use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, ChunkId, FileId, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Entry, Kind, Tree};
use asc_core::path::{Name, RepoPath};
use asc_store::delta::{Skip, pair_chunks};
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
}

impl Default for DeltaSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Secs(3600),
            min_size: ByteSize(1024),
            max_changesets: 5000,
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
    // Side-branch changesets already read this pass, over every merge and ref.
    let mut seen = HashSet::new();
    'refs: for r in refs {
        let mut at: i64 =
            sqlx::query_scalar("SELECT pos FROM delta_cursor WHERE ns = $1 AND ref = $2")
                .bind(ns.clone())
                .bind(r.clone())
                .fetch_optional(pool)
                .await
                .map_err(|e| sql("delta_cursor", e))?
                .unwrap_or(-1);
        while budget > 0 {
            let rows = sqlx::query(
                "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 AND pos > $3 \
                 ORDER BY pos LIMIT $4",
            )
            .bind(ns.clone())
            .bind(r.clone())
            .bind(at)
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
                // The first changeset of a pass has the whole budget; a merge whose side history
                // is larger than that is read as far as the budget goes and then passed.
                let fresh = budget == s.max_changesets as u64;
                let parents = one_changeset(pool, entry, s, cs, &purged, &mut rep).await?;
                if parents.len() > 1 {
                    let merged = parents[1..].to_vec();
                    let walk = Walk {
                        pool,
                        entry,
                        s,
                        r: &r,
                        purged: &purged,
                    };
                    let done = walk.run(merged, &mut seen, &mut budget, &mut rep).await?;
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
            todo.extend(one_changeset(pool, entry, self.s, id, self.purged, rep).await?);
        }
        Ok(true)
    }
}

/// Stores what `id` writes as deltas against its first parent: each file against the version
/// at the same path, each changed directory's tree against the same directory's old tree, and
/// each new chunk of a large file against the old version's chunk at the same place. Returns its
/// parents (none when the changeset is missing).
async fn one_changeset(
    pool: &AnyPool,
    entry: &RepoEntry,
    s: &DeltaSettings,
    id: ChangesetId,
    purged: &HashSet<ObjKey>,
    rep: &mut DeltaReport,
) -> Result<Vec<ChangesetId>, GcError> {
    let Some(cs) = changeset(entry, id).await? else {
        return Ok(Vec::new());
    };
    let Some(parent) = cs.parents.first() else {
        return Ok(Vec::new());
    };
    let Some(parent) = changeset(entry, *parent).await? else {
        return Ok(cs.parents.clone());
    };
    let mut d = Deltas {
        pool,
        entry,
        purged,
        rep,
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
        let Some(old) = lookup(entry, parent.root, from).await? else {
            continue;
        };
        if !matches!(old.kind, Kind::File | Kind::Exec) {
            continue;
        }
        let base = FileId::from_bytes(old.id);
        if base == *file {
            continue;
        }
        match (*size > CHUNK_THRESHOLD, old.size > CHUNK_THRESHOLD) {
            (false, false) => d.try_one(ObjKey::File(*file), ObjKey::File(base)).await?,
            (true, true) => d.chunks(*file, base).await?,
            _ => {}
        }
    }
    if cs.root != parent.root {
        d.try_one(ObjKey::Tree(cs.root), ObjKey::Tree(parent.root))
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
            d.try_one(ObjKey::Tree(n), ObjKey::Tree(o)).await?;
        }
    }
    Ok(cs.parents.clone())
}

/// The delta writes of one changeset.
struct Deltas<'a> {
    pool: &'a AnyPool,
    entry: &'a RepoEntry,
    purged: &'a HashSet<ObjKey>,
    rep: &'a mut DeltaReport,
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
            self.try_one(ObjKey::Chunk(c), ObjKey::Chunk(o)).await?;
        }
        Ok(())
    }

    async fn chunk_list(&self, f: FileId) -> Result<Option<ChunkList>, GcError> {
        let Some(b) = self.entry.objects.get(&ObjKey::ChunkList(f)).await? else {
            return Ok(None);
        };
        Ok(decode_envelope(&b).ok())
    }

    /// Stores `key` as a delta against `base` when that saves half.
    async fn try_one(&mut self, key: ObjKey, base: ObjKey) -> Result<(), GcError> {
        if key == base || self.purged.contains(&base) {
            return Ok(());
        }
        let (Some(kind), Some(id), Some(base_id)) =
            (row_kind(&key), key.delta_id(), base.delta_id())
        else {
            return Ok(());
        };
        let rep = &mut *self.rep;
        rep.candidates += 1;
        let planned = match self.entry.objects.plan_delta(key, base).await? {
            Ok(p) => p,
            Err(Skip::NoGain) => {
                rep.no_gain += 1;
                return Ok(());
            }
            Err(_) => {
                rep.skipped += 1;
                return Ok(());
            }
        };
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
        let t: DeltaSettings = toml::from_str("enabled = true\ninterval = \"15m\"").unwrap();
        assert!(t.enabled);
        assert_eq!(t.interval(), Duration::from_secs(900));
        assert!(toml::from_str::<DeltaSettings>("enable = true").is_err());
    }
}
