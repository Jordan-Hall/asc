//! `Cloud.Purge` and cache eviction (SPEC §21.2.4, §21.3.7, PLAN B10).
//!
//! A purge removes a secret that reached the server in a draft. For each named changeset it
//!
//! 1. deletes the changeset's cloud heads (the owner's own; every owner's for a repo admin),
//! 2. lists the objects of the changeset in `gc_purge` (the changeset, the files and chunks it
//!    wrote and the trees on the paths it changed), which the next GC sweep deletes unless a ref
//!    or head still reaches them, without the 14-day age rule, and
//! 3. queues `cache_evict` outbox rows with the object ids, in the same transaction.
//!
//! The outbox worker ([`CacheEvict`]) sends the ids to every registered [`CacheTier`] and keeps
//! retrying the row, with the outbox backoff, until all of them acknowledge. Copies already
//! fetched to client machines cannot be recalled.
//!
//! Authorisation is decided before anything is read or written ([`Who`]). An owner may purge a
//! changeset only when it is a head of one of their own workspaces and it is unsubmitted: no
//! `change_versions` row names the changeset or its change, and no ref reaches it. A repo admin
//! may purge any changeset. A changeset that is refused for any reason, including "not yours" and
//! "does not exist", is reported the same way, so a purge is no oracle for other users' drafts.
//!
//! Choices where SPEC is silent:
//!
//! * "No shared-ref ancestry" is checked against every ref of the repository (shared refs, tags,
//!   user refs, agent refs): a draft anything else holds is not an unsubmitted draft. At most
//!   [`MAX_REACH_TIPS`] ref tips and [`MAX_REACH_VISITS`] graph visits are read; past either the
//!   owner's request is refused (fail closed) and an admin must purge.
//! * Eviction ids are the objects' layout paths (`f/ab/abcd...`), the same names `store/` and
//!   `cache/` use.
//! * `cache_evict` rows live in their own outbox lane (`repo_id` is the negated repository id),
//!   so a cache tier that is down never blocks the repository's `post_receive` rows.
//! * `Purge` reports every registered tier as unacked: delivery is asynchronous.
//! * A purge is at most [`MAX_PURGE_CS`] changesets and [`MAX_PURGE_KEYS`] objects; more is
//!   `RESOURCE_EXHAUSTED` and nothing changes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use asc_core::codec::decode_envelope;
use asc_core::content::CHUNK_THRESHOLD;
use asc_core::id::{ChangesetId, RefTarget, TreeId};
use asc_core::object::{Change, Changeset, ChunkList, Kind, TagRecord, Tree};
use asc_core::path::Name;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use sqlx::{AnyConnection, AnyPool, Row};
use tonic::Status;
use uuid::Uuid;

use crate::commit_graph_rpc::internal;
use crate::outbox::{Effect, EffectError, Job};
use crate::registry::{Registry, RepoEntry};

/// The outbox effect name of an eviction row.
pub const EFFECT: &str = "cache_evict";
/// Changesets one `Purge` names at most.
pub const MAX_PURGE_CS: usize = 100;
/// Objects one `Purge` lists at most.
pub const MAX_PURGE_KEYS: usize = 50_000;
/// Ids one `cache_evict` row carries at most (and a tier is sent at once).
pub const EVICT_IDS_PER_ROW: usize = 1_000;
/// Ref tips a reachability check reads at most.
pub const MAX_REACH_TIPS: usize = 11_000;
/// Changesets a reachability check visits at most.
pub const MAX_REACH_VISITS: usize = 200_000;
/// Heads of one changeset one purge deletes at most.
const MAX_HEAD_ROWS: i64 = 1_000;
/// Other changesets an owner's purge compares objects against at most.
const MAX_OTHER_CS: usize = 2_000;
/// Rows per `cs_graph` lookup.
const GRAPH_BATCH: usize = 100;
/// Rows per multi-row `gc_purge` insert.
const PURGE_ROWS: usize = 200;
/// Longest tier error kept in a row's `last_error`.
const MAX_TIER_ERROR: usize = 120;

fn sql(e: sqlx::Error) -> Status {
    internal("database", &e)
}

// ------------------------------------------------------------------------------ cache tiers

/// One cache the server must tell to drop objects: the CI host cache, `cacheproxy`, a
/// bucket/CDN cache. Eviction must be idempotent: a row is delivered until every tier answers.
#[async_trait]
pub trait CacheTier: Send + Sync {
    /// The name reported in `PurgeResponse.unacked_tiers`.
    fn name(&self) -> &str;
    /// Drops `ids` (object layout paths) of the namespace `ns` (hex). `Err` is retried.
    async fn evict(&self, ns: &str, ids: &[String]) -> Result<(), String>;
}

/// The registered tiers.
#[derive(Default)]
pub struct CacheTiers {
    tiers: RwLock<Vec<Arc<dyn CacheTier>>>,
}

impl CacheTiers {
    pub fn register(&self, tier: Arc<dyn CacheTier>) {
        self.tiers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tier);
    }

    fn snapshot(&self) -> Vec<Arc<dyn CacheTier>> {
        self.tiers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The names of the registered tiers.
    pub fn names(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|t| t.name().to_owned())
            .collect()
    }
}

/// The `cache_evict` outbox effect: sends the ids of a row to every tier.
pub struct CacheEvict {
    tiers: Arc<CacheTiers>,
}

impl CacheEvict {
    pub fn new(tiers: Arc<CacheTiers>) -> Self {
        Self { tiers }
    }
}

#[async_trait]
impl Effect for CacheEvict {
    fn kind(&self) -> &'static str {
        EFFECT
    }

    async fn run(&self, job: &Job) -> Result<(), EffectError> {
        let bad = |what: &str| EffectError(format!("cache_evict row: {what}"));
        let ns = job
            .doc
            .get("ns")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad("no ns"))?;
        let ids: Vec<String> = job
            .doc
            .get("ids")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .ok_or_else(|| bad("no ids"))?;
        if ids.is_empty() || ids.len() > EVICT_IDS_PER_ROW {
            return Err(bad("id count out of range"));
        }
        // A tier is never sent a name that is not an object path.
        if ids.iter().any(|i| ObjKey::parse(i).is_none()) {
            return Err(bad("an id is not an object path"));
        }
        if ns.len() != 32 || !ns.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("bad ns"));
        }
        // Every tier is tried, so one that is down does not hold back the others.
        let mut failed = Vec::new();
        for tier in self.tiers.snapshot() {
            if let Err(e) = tier.evict(ns, &ids).await {
                let e: String = e.chars().take(MAX_TIER_ERROR).collect();
                failed.push(format!("{}: {e}", tier.name()));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(EffectError(format!("unacked: {}", failed.join("; "))))
        }
    }
}

// ------------------------------------------------------------------------------ the purge

/// Who asks. A purge needs an owner identity or admin rights.
#[derive(Clone, Copy, Debug)]
pub struct Who {
    /// The verified Forgejo user id, when the credential carries one.
    pub owner: Option<i64>,
    /// Whether the credential has repo admin.
    pub admin: bool,
}

/// What a purge did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub purged: Vec<ChangesetId>,
    pub refused: Vec<ChangesetId>,
    /// Tiers that have not acknowledged yet (all registered tiers, at response time).
    pub unacked_tiers: Vec<String>,
}

/// A changeset that may be purged, with the head rows to delete.
struct Accepted {
    cs: ChangesetId,
    /// `(owner, workspace)` of each head row.
    heads: Vec<(i64, String)>,
}

/// The purge service.
pub struct Purger {
    registry: Arc<Registry>,
    tiers: Arc<CacheTiers>,
}

impl Purger {
    pub fn new(registry: Arc<Registry>, tiers: Arc<CacheTiers>) -> Self {
        Self { registry, tiers }
    }

    pub fn tiers(&self) -> &Arc<CacheTiers> {
        &self.tiers
    }

    /// Purges `css` from `entry`'s repository on behalf of `who`.
    pub async fn purge(
        &self,
        entry: &RepoEntry,
        who: Who,
        css: &[ChangesetId],
        at: i64,
    ) -> Result<Purged, Status> {
        if !who.admin && who.owner.is_none() {
            return Err(Status::permission_denied("purge needs a user identity"));
        }
        if css.is_empty() || css.len() > MAX_PURGE_CS {
            return Err(Status::invalid_argument(format!(
                "name 1 to {MAX_PURGE_CS} changesets"
            )));
        }
        let mut seen = HashSet::new();
        let css: Vec<ChangesetId> = css.iter().copied().filter(|c| seen.insert(*c)).collect();
        let pool = self.registry.pool();

        // Phase 1: decide, then read the objects. The connection is released before the
        // transaction opens (a one-connection pool would otherwise wait on itself).
        let first = {
            let mut conn = pool.acquire().await.map_err(sql)?;
            eligible(&mut conn, entry, who, &css).await?
        };
        let mut closures: HashMap<ChangesetId, Vec<ObjKey>> = HashMap::new();
        let mut total = 0usize;
        for a in &first.0 {
            let keys = closure(&*entry.objects, a.cs, &mut total).await?;
            closures.insert(a.cs, keys);
        }
        if !who.admin && !closures.is_empty() {
            // Objects are content-addressed: another draft or ingest may hold the same file or
            // tree. An owner's purge must not queue those for the early sweep (which skips the
            // age rule and the recent tier), or it could delete data others still want.
            let mut conn = pool.acquire().await.map_err(sql)?;
            let others = other_keys(&mut conn, entry, &css).await?;
            for keys in closures.values_mut() {
                keys.retain(|k| matches!(k, ObjKey::Changeset(_)) || !others.contains(k));
            }
        }

        // Phase 2: decide again inside the transaction, so a draft that was submitted or pushed
        // in between is not purged, then write everything at once.
        let mut tx = pool.begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, entry).await?;
        let (accepted, refused) = eligible(&mut *tx, entry, who, &css).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let mut ws: HashMap<(i64, String), i64> = HashMap::new();
        let mut keys: Vec<ObjKey> = Vec::new();
        let mut listed = HashSet::new();
        for a in &accepted {
            let Some(own) = closures.get(&a.cs) else {
                // Became eligible only after the objects were read: leave it for a retry.
                continue;
            };
            for (owner, name) in &a.heads {
                let gone = sqlx::query(
                    "DELETE FROM cloud_heads WHERE ns = $1 AND owner = $2 AND name = $3 AND cs = $4",
                )
                .bind(ns.clone())
                .bind(*owner)
                .bind(name.clone())
                .bind(a.cs.as_bytes().to_vec())
                .execute(&mut *tx)
                .await
                .map_err(sql)?
                .rows_affected();
                *ws.entry((*owner, name.clone())).or_default() +=
                    i64::try_from(gone).unwrap_or(i64::MAX);
            }
            for k in own {
                if listed.insert(*k) {
                    keys.push(*k);
                }
            }
        }
        for ((owner, name), n) in &ws {
            sqlx::query(
                "UPDATE cloud_ws SET heads = CASE WHEN heads >= $4 THEN heads - $4 ELSE 0 END, \
                 version = version + 1, updated = $5 \
                 WHERE ns = $1 AND owner = $2 AND name = $3",
            )
            .bind(ns.clone())
            .bind(*owner)
            .bind(name.clone())
            .bind(*n)
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        }
        record(&mut *tx, entry, &ns, &keys, at).await?;
        tx.commit().await.map_err(sql)?;

        let purged: Vec<ChangesetId> = accepted
            .iter()
            .map(|a| a.cs)
            .filter(|c| closures.contains_key(c))
            .collect();
        let mut refused = refused;
        refused.extend(
            accepted
                .iter()
                .map(|a| a.cs)
                .filter(|c| !closures.contains_key(c)),
        );
        tracing::info!(
            event = "cloud.purge",
            repo = %entry.name,
            by_admin = who.admin,
            owner = who.owner.unwrap_or(0),
            purged = purged.len(),
            refused = refused.len(),
            objects = keys.len(),
        );
        Ok(Purged {
            purged,
            refused,
            unacked_tiers: self.tiers.names(),
        })
    }
}

/// The objects of every other draft or not-yet-shared ingested changeset of the repository.
/// Past [`MAX_OTHER_CS`] changesets the request is refused (fail closed).
async fn other_keys(
    conn: &mut AnyConnection,
    entry: &RepoEntry,
    purging: &[ChangesetId],
) -> Result<HashSet<ObjKey>, Status> {
    let ns = entry.ns.as_bytes().to_vec();
    let rows = sqlx::query(
        "SELECT cs FROM cloud_heads WHERE ns = $1          UNION SELECT cs FROM cs_ledger WHERE ns = $1 AND shared = 0          LIMIT $2",
    )
    .bind(ns)
    .bind(i64::try_from(MAX_OTHER_CS + 1).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await
    .map_err(sql)?;
    if rows.len() > MAX_OTHER_CS {
        return Err(Status::failed_precondition(
            "too many other drafts to check for shared objects; ask a repository admin to purge",
        ));
    }
    let mut out = HashSet::new();
    for r in &rows {
        let raw: Vec<u8> = r.try_get("cs").map_err(sql)?;
        let cs = id_of(&raw).map_err(sql)?;
        if purging.contains(&cs) {
            continue;
        }
        // Each changeset is bounded separately: only the set matters here.
        let mut total = 0usize;
        out.extend(closure(&*entry.objects, cs, &mut total).await?);
    }
    Ok(out)
}

/// Writes the `gc_purge` rows and the `cache_evict` outbox rows of `keys`.
async fn record(
    tx: &mut AnyConnection,
    entry: &RepoEntry,
    ns: &[u8],
    keys: &[ObjKey],
    at: i64,
) -> Result<(), Status> {
    let ns_hex = entry.ns.hex();
    for chunk in keys.chunks(PURGE_ROWS) {
        let rows = (0..chunk.len())
            .map(|i| format!("($1, ${}, $2)", i + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "INSERT INTO gc_purge (ns, key, purged_at) VALUES {rows} \
             ON CONFLICT (ns, key) DO UPDATE SET purged_at = excluded.purged_at"
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(text))
            .bind(ns.to_vec())
            .bind(at);
        for k in chunk {
            q = q.bind(k.path());
        }
        q.execute(&mut *tx).await.map_err(sql)?;
    }
    for chunk in keys.chunks(EVICT_IDS_PER_ROW) {
        let ids: Vec<String> = chunk.iter().map(ObjKey::path).collect();
        let doc = serde_json::json!({ "effects": [EFFECT], "ns": ns_hex, "ids": ids });
        let doc = serde_json::to_vec(&doc)
            .map_err(|e| Status::internal(format!("eviction document: {e}")))?;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(-entry.key.0)
        .bind(Uuid::new_v4().as_bytes().to_vec())
        .bind(doc)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------ eligibility

/// Splits `css` into the changesets `who` may purge and the ones refused.
async fn eligible(
    conn: &mut AnyConnection,
    entry: &RepoEntry,
    who: Who,
    css: &[ChangesetId],
) -> Result<(Vec<Accepted>, Vec<ChangesetId>), Status> {
    let ns = entry.ns.as_bytes().to_vec();
    let mut accepted = Vec::new();
    let mut refused = Vec::new();
    // For an owner: the changesets still to be checked against the refs.
    let mut candidates: Vec<Accepted> = Vec::new();
    for &cs in css {
        let owner_filter = if who.admin { None } else { who.owner };
        let mut text = String::from("SELECT owner, name, change FROM cloud_heads WHERE ns = $1 AND cs = $2");
        if owner_filter.is_some() {
            text.push_str(" AND owner = $3");
        }
        text.push_str(" LIMIT ");
        text.push_str(&MAX_HEAD_ROWS.to_string());
        let mut q = sqlx::query(sqlx::AssertSqlSafe(text))
            .bind(ns.clone())
            .bind(cs.as_bytes().to_vec());
        if let Some(o) = owner_filter {
            q = q.bind(o);
        }
        let rows = q.fetch_all(&mut *conn).await.map_err(sql)?;
        let mut heads = Vec::new();
        let mut changes = Vec::new();
        for r in &rows {
            heads.push((
                r.try_get::<i64, _>("owner").map_err(sql)?,
                r.try_get::<String, _>("name").map_err(sql)?,
            ));
            changes.push(r.try_get::<Vec<u8>, _>("change").map_err(sql)?);
        }
        if who.admin {
            // An admin may purge any changeset the server knows: one with a head or a graph row.
            let known = !heads.is_empty()
                || sqlx::query("SELECT 1 FROM cs_graph WHERE ns = $1 AND cs = $2")
                    .bind(ns.clone())
                    .bind(cs.as_bytes().to_vec())
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(sql)?
                    .is_some();
            if known {
                accepted.push(Accepted { cs, heads });
            } else {
                refused.push(cs);
            }
            continue;
        }
        if heads.is_empty() || submitted(conn, &ns, cs, &changes).await? {
            refused.push(cs);
        } else {
            candidates.push(Accepted { cs, heads });
        }
    }
    if !candidates.is_empty() {
        let targets: Vec<ChangesetId> = candidates.iter().map(|a| a.cs).collect();
        // `None`: too many refs to read; fail closed.
        let held = reaches_a_ref(conn, entry, &targets).await?;
        for a in candidates {
            match &held {
                Some(h) if !h.contains(&a.cs) => accepted.push(a),
                _ => refused.push(a.cs),
            }
        }
    }
    Ok((accepted, refused))
}

/// Whether a `change_versions` row names the changeset or any of its changes.
async fn submitted(
    conn: &mut AnyConnection,
    ns: &[u8],
    cs: ChangesetId,
    changes: &[Vec<u8>],
) -> Result<bool, Status> {
    if sqlx::query("SELECT 1 FROM change_versions WHERE ns = $1 AND cs = $2 LIMIT 1")
        .bind(ns.to_vec())
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(&mut *conn)
        .await
        .map_err(sql)?
        .is_some()
    {
        return Ok(true);
    }
    for change in changes {
        if sqlx::query("SELECT 1 FROM change_versions WHERE ns = $1 AND change = $2 LIMIT 1")
            .bind(ns.to_vec())
            .bind(change.clone())
            .fetch_optional(&mut *conn)
            .await
            .map_err(sql)?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn id_of(b: &[u8]) -> Result<ChangesetId, sqlx::Error> {
    <[u8; 32]>::try_from(b)
        .map(ChangesetId::from_bytes)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

/// The members of `targets` that some ref of the repository reaches (a ref's tip is one of
/// them, or one of them is an ancestor of the tip). `None` when the check hit a cap.
async fn reaches_a_ref(
    conn: &mut AnyConnection,
    entry: &RepoEntry,
    targets: &[ChangesetId],
) -> Result<Option<HashSet<ChangesetId>>, Status> {
    let rows = sqlx::query("SELECT target FROM refs WHERE repo_id = $1 LIMIT $2")
        .bind(entry.key.0)
        .bind(i64::try_from(MAX_REACH_TIPS + 1).unwrap_or(i64::MAX))
        .fetch_all(&mut *conn)
        .await
        .map_err(sql)?;
    if rows.len() > MAX_REACH_TIPS {
        return Ok(None);
    }
    let mut tips: HashSet<ChangesetId> = HashSet::new();
    for r in &rows {
        let raw: Vec<u8> = r.try_get("target").map_err(sql)?;
        match RefTarget::from_bytes(&raw) {
            Ok(RefTarget::Changeset(c)) => {
                tips.insert(c);
            }
            Ok(RefTarget::Tag(t)) => {
                // A tag reaches what it names. A tag that cannot be read fails the check closed.
                let Ok(Some(data)) = entry.objects.get(&ObjKey::Tag(t)).await else {
                    return Ok(None);
                };
                let Ok(rec) = decode_envelope::<TagRecord>(&data) else {
                    return Ok(None);
                };
                tips.insert(rec.target);
            }
            Err(_) => return Ok(None),
        }
    }
    let want: HashSet<ChangesetId> = targets.iter().copied().collect();
    let mut reached: HashSet<ChangesetId> = tips.intersection(&want).copied().collect();
    // Generation floor: nothing below the lowest target can lead to a target.
    let ns = entry.ns.as_bytes().to_vec();
    let mut floor = i64::MAX;
    for chunk in targets.chunks(GRAPH_BATCH) {
        for (_, generation, _) in graph_rows(conn, &ns, chunk).await.map_err(sql)? {
            floor = floor.min(generation);
        }
    }
    let mut seen: HashSet<ChangesetId> = tips.clone();
    let mut frontier: Vec<ChangesetId> = tips.into_iter().collect();
    let mut visits = 0usize;
    while !frontier.is_empty() && reached.len() < want.len() {
        let mut next = Vec::new();
        for chunk in frontier.chunks(GRAPH_BATCH) {
            for (cs, generation, parents) in graph_rows(conn, &ns, chunk).await.map_err(sql)? {
                visits += 1;
                if visits > MAX_REACH_VISITS {
                    return Ok(None);
                }
                if want.contains(&cs) {
                    reached.insert(cs);
                }
                if generation <= floor {
                    continue;
                }
                for p in parents {
                    if seen.insert(p) {
                        next.push(p);
                    }
                }
            }
        }
        frontier = next;
    }
    Ok(Some(reached))
}

async fn graph_rows(
    conn: &mut AnyConnection,
    ns: &[u8],
    ids: &[ChangesetId],
) -> Result<Vec<(ChangesetId, i64, Vec<ChangesetId>)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks = (0..ids.len())
        .map(|i| format!("${}", i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("SELECT cs, gen, parents FROM cs_graph WHERE ns = $1 AND cs IN ({marks})");
    let mut q = sqlx::query(sqlx::AssertSqlSafe(text)).bind(ns.to_vec());
    for id in ids {
        q = q.bind(id.as_bytes().to_vec());
    }
    let mut out = Vec::new();
    for row in q.fetch_all(&mut *conn).await? {
        let cs = id_of(&row.try_get::<Vec<u8>, _>("cs")?)?;
        let generation: i64 = row.try_get("gen")?;
        let blob: Vec<u8> = row.try_get("parents")?;
        if !blob.len().is_multiple_of(32) {
            return Err(sqlx::Error::Decode("damaged cs_graph parents".into()));
        }
        let parents = blob
            .chunks_exact(32)
            .map(id_of)
            .collect::<Result<Vec<_>, _>>()?;
        out.push((cs, generation, parents));
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ closure

fn too_many() -> Status {
    Status::resource_exhausted(format!("a purge lists at most {MAX_PURGE_KEYS} objects"))
}

fn push(out: &mut Vec<ObjKey>, k: ObjKey, total: &mut usize) -> Result<(), Status> {
    *total += 1;
    if *total > MAX_PURGE_KEYS {
        return Err(too_many());
    }
    out.push(k);
    Ok(())
}

fn obj(e: asc_store::objects::Error) -> Status {
    internal("object store", &e)
}

/// The objects `cs` introduced: the changeset, the files and chunks it wrote, its root tree and
/// the trees on the paths it changed. Objects shared with other changesets are listed too; an
/// owner's purge drops those in [`other_keys`], and GC keeps whatever a strict root reaches. `total` counts the keys of the whole request.
async fn closure(
    objects: &dyn Objects,
    cs: ChangesetId,
    total: &mut usize,
) -> Result<Vec<ObjKey>, Status> {
    let mut out = vec![ObjKey::Changeset(cs)];
    *total += 1;
    if *total > MAX_PURGE_KEYS {
        return Err(too_many());
    }
    let Some(data) = objects.get(&ObjKey::Changeset(cs)).await.map_err(obj)? else {
        return Ok(out);
    };
    let changeset: Changeset =
        decode_envelope(&data).map_err(|e| internal("changeset", &e.to_string()))?;
    for (_, change) in changeset.changes.as_slice() {
        if let Change::Write { file, size, .. } = change {
            push(&mut out, ObjKey::File(*file), total)?;
            if *size > CHUNK_THRESHOLD {
                push(&mut out, ObjKey::ChunkList(*file), total)?;
                if let Some(list) = objects
                    .get(&ObjKey::ChunkList(*file))
                    .await
                    .map_err(obj)?
                {
                    let list: ChunkList = decode_envelope(&list)
                        .map_err(|e| internal("chunk list", &e.to_string()))?;
                    for (chunk, _) in &list.chunks {
                        push(&mut out, ObjKey::Chunk(*chunk), total)?;
                    }
                }
            }
        }
    }
    // The spine: the root and, for each changed path, the directories down to it.
    let mut trees: HashMap<TreeId, Option<Tree>> = HashMap::new();
    let mut listed: HashSet<TreeId> = HashSet::new();
    listed.insert(changeset.root);
    push(&mut out, ObjKey::Tree(changeset.root), total)?;
    for (path, _) in changeset.changes.as_slice() {
        let comps: Vec<&[u8]> = path.components().collect();
        let mut cur = changeset.root;
        for comp in comps.iter().take(comps.len().saturating_sub(1)) {
            if !trees.contains_key(&cur) {
                let tree = match objects.get(&ObjKey::Tree(cur)).await.map_err(obj)? {
                    Some(d) => Some(
                        decode_envelope::<Tree>(&d).map_err(|e| internal("tree", &e.to_string()))?,
                    ),
                    None => None,
                };
                trees.insert(cur, tree);
            }
            let Some(Some(tree)) = trees.get(&cur) else {
                break;
            };
            let Ok(name) = Name::new(comp.to_vec()) else {
                break;
            };
            match tree.get(&name) {
                Some(e) if e.kind == Kind::Dir => {
                    cur = TreeId::from_bytes(e.id);
                    if listed.insert(cur) {
                        push(&mut out, ObjKey::Tree(cur), total)?;
                    }
                }
                _ => break,
            }
        }
    }
    Ok(out)
}

/// Rows of `gc_purge` older than `before` for the namespace, removed in one statement.
pub(crate) async fn prune(pool: &AnyPool, ns: &[u8], before: i64) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM gc_purge WHERE ns = $1 AND purged_at < $2")
            .bind(ns.to_vec())
            .bind(before)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

#[cfg(test)]
#[path = "purge_tests.rs"]
mod tests;
