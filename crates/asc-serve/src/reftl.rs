//! User-ref lifetime: the landed marker and its expiry (SPEC §8.8, §21.2.4, §21.2.4a; PLAN B08c).
//!
//! * [`mark_landed`] runs inside the land transaction. It looks at the landing author's user refs
//!   only (at most `max_user_refs`), asks whether every changeset of `trunk..tip` has landed, and
//!   sets `user_ref_meta.landed_at`. It never reads another owner's refs and never scans `refs`.
//! * [`delete_reachability`] answers the `push --delete` question: which changes of the deleted
//!   ref's range stay reachable from the owner's other heads. It reads the owner's rows only.
//! * [`LandedReaper`] deletes a user ref `landed_ref_days` after its marker, through the normal
//!   `UpdateRefs` pipeline. A ref that was updated after it landed is kept and its marker cleared.
//!
//! `agent/*` user refs keep their `expires_at` TTL, which `expire::Expirer` handles.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::auth::Principal;
use asc_rpc::server::RepoName;
use sqlx::any::Any;
use sqlx::{AnyConnection, AnyPool, Row, Transaction};

use crate::expire::Expirer;
use crate::refauthz::DEFAULT_MAX_USER_REFS;
use crate::refpolicy::{self, USER_NS};
use crate::registry::{Ns, Registry};

/// `[cloud] landed_ref_days` default; `0` disables the expiry.
pub const DEFAULT_LANDED_REF_DAYS: u32 = 7;
/// Marker rows one reaper run handles at most (SPEC §21.2.4: batches of 1,000).
pub const REAP_BATCH: i64 = 1_000;
/// Database lookups one land spends on markers; the rest of the refs wait for the next land.
pub const MAX_LOOKUPS_PER_LAND: usize = 50_000;
/// Parents one changeset may have for the marker walk; a changeset over it is unknown, not landed.
pub const MAX_PARENTS_PER_NODE: usize = 1_000;
/// Tips a delete-reachability check considers (1,000 user refs plus 10,000 cloud heads).
pub const MAX_REACH_TIPS: usize = 11_000;
/// Changesets a delete-reachability walk visits at most.
pub const MAX_REACH_VISITS: usize = 200_000;
/// Rows per `cs_graph` lookup.
const GRAPH_BATCH: usize = 100;
const DAY: i64 = 86_400;

/// What one [`mark_landed`] call did; tests read it to bound the work.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LandedScan {
    /// `refs` rows read (all of them the landing author's).
    pub refs_read: usize,
    /// Refs whose marker is now set (new or already set).
    pub marked: usize,
    /// Database lookups spent walking stacks.
    pub lookups: usize,
    /// The lookup budget ran out before every ref was checked.
    pub truncated: bool,
}

fn upper_bound(prefix: &str) -> String {
    // `/` is followed by `0` in ASCII: the exclusive end of the prefix range.
    let mut s = prefix.trim_end_matches('/').to_owned();
    s.push('0');
    s
}

async fn graph_parents(
    conn: &mut AnyConnection,
    ns: Ns,
    cs: ChangesetId,
) -> Result<Option<Vec<ChangesetId>>, sqlx::Error> {
    let row = sqlx::query("SELECT parents FROM cs_graph WHERE ns = $1 AND cs = $2")
        .bind(ns.as_bytes().to_vec())
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(&mut *conn)
        .await?;
    row.map(|r| decode_parents(&r.try_get::<Vec<u8>, _>("parents")?))
        .transpose()
}

fn id_of(b: &[u8]) -> Result<ChangesetId, sqlx::Error> {
    <[u8; 32]>::try_from(b)
        .map(ChangesetId::from_bytes)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

fn decode_parents(blob: &[u8]) -> Result<Vec<ChangesetId>, sqlx::Error> {
    if !blob.len().is_multiple_of(32) {
        return Err(sqlx::Error::Decode("damaged cs_graph parents".into()));
    }
    blob.chunks_exact(32).map(id_of).collect()
}

struct Walker<'a> {
    ns: Ns,
    trunk: &'a str,
    steps: HashSet<ChangesetId>,
    landed: HashMap<ChangesetId, bool>,
    mainline: HashMap<ChangesetId, bool>,
    lookups: usize,
    /// Lookups the whole call may spend ([`MAX_LOOKUPS_PER_LAND`]).
    budget: usize,
    /// Parents one changeset may have ([`MAX_PARENTS_PER_NODE`]).
    max_parents: usize,
    /// The last [`Self::stack_landed`] stopped at a changeset with too many parents.
    over_parents: bool,
}

/// `$first, $first + 1, ...` placeholders for `n` bound ids.
fn placeholders(first: usize, n: usize) -> String {
    (first..first + n)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Walker<'_> {
    fn spent(&self) -> bool {
        self.lookups >= self.budget
    }

    /// Counts `n` lookups against the budget; `false` (and nothing counted) when they do not fit.
    fn charge(&mut self, n: usize) -> bool {
        if self.lookups.saturating_add(n) > self.budget {
            return false;
        }
        self.lookups += n;
        true
    }

    /// Whether `cs` landed on the trunk in this land or before; call [`Self::fill_landed`] first.
    fn is_landed(&self, cs: ChangesetId) -> bool {
        self.steps.contains(&cs) || self.landed.get(&cs).copied().unwrap_or(false)
    }

    /// Whether `cs` is on the trunk's mainline; call [`Self::fill_mainline`] first.
    fn on_mainline(&self, cs: ChangesetId) -> bool {
        self.mainline.get(&cs).copied().unwrap_or(false)
    }

    /// Looks up which of `ids` landed on the trunk (a `land_map` row for it), `GRAPH_BATCH` per
    /// query, and charges one lookup per id. `false` when the budget ran out first.
    async fn fill_landed(
        &mut self,
        conn: &mut AnyConnection,
        ids: &[ChangesetId],
    ) -> Result<bool, sqlx::Error> {
        let mut todo: Vec<ChangesetId> = Vec::new();
        for cs in ids {
            if !self.steps.contains(cs) && !self.landed.contains_key(cs) && !todo.contains(cs) {
                todo.push(*cs);
            }
        }
        for chunk in todo.chunks(GRAPH_BATCH) {
            if !self.charge(chunk.len()) {
                return Ok(false);
            }
            let marks = placeholders(3, chunk.len());
            let sql = format!(
                "SELECT old_cs FROM land_map_ref WHERE ns = $1 AND ref = $2 AND old_cs IN ({marks})                  UNION ALL SELECT old_cs FROM land_map WHERE ns = $1 AND ref = $2                  AND old_cs IN ({marks})"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(self.ns.as_bytes().to_vec())
                .bind(self.trunk);
            for cs in chunk {
                q = q.bind(cs.as_bytes().to_vec());
            }
            let mut hits = HashSet::new();
            for row in q.fetch_all(&mut *conn).await? {
                hits.insert(id_of(&row.try_get::<Vec<u8>, _>("old_cs")?)?);
            }
            for cs in chunk {
                self.landed.insert(*cs, hits.contains(cs));
            }
        }
        Ok(true)
    }

    /// Looks up which of `ids` are on the trunk's mainline, like [`Self::fill_landed`].
    async fn fill_mainline(
        &mut self,
        conn: &mut AnyConnection,
        ids: &[ChangesetId],
    ) -> Result<bool, sqlx::Error> {
        let mut todo: Vec<ChangesetId> = Vec::new();
        for cs in ids {
            if !self.mainline.contains_key(cs) && !todo.contains(cs) {
                todo.push(*cs);
            }
        }
        for chunk in todo.chunks(GRAPH_BATCH) {
            if !self.charge(chunk.len()) {
                return Ok(false);
            }
            let marks = placeholders(3, chunk.len());
            let sql =
                format!("SELECT cs FROM mainline WHERE ns = $1 AND ref = $2 AND cs IN ({marks})");
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(self.ns.as_bytes().to_vec())
                .bind(self.trunk);
            for cs in chunk {
                q = q.bind(cs.as_bytes().to_vec());
            }
            let mut hits = HashSet::new();
            for row in q.fetch_all(&mut *conn).await? {
                hits.insert(id_of(&row.try_get::<Vec<u8>, _>("cs")?)?);
            }
            for cs in chunk {
                self.mainline.insert(*cs, hits.contains(cs));
            }
        }
        Ok(true)
    }

    /// Whether every changeset of `trunk..tip` has landed, and the stack is not empty. `None`
    /// (unknown, never landed) when the lookup budget ran out or a changeset has more than
    /// `max_parents` parents.
    async fn stack_landed(
        &mut self,
        conn: &mut AnyConnection,
        tip: ChangesetId,
    ) -> Result<Option<bool>, sqlx::Error> {
        self.over_parents = false;
        // The tip itself must have landed: a ref that merely sits on the trunk has no work.
        if !self.fill_landed(conn, &[tip]).await? {
            return Ok(None);
        }
        if !self.is_landed(tip) {
            return Ok(Some(false));
        }
        let mut seen = HashSet::from([tip]);
        let mut todo = vec![tip];
        while let Some(cs) = todo.pop() {
            if !self.charge(1) {
                return Ok(None);
            }
            let Some(parents) = graph_parents(conn, self.ns, cs).await? else {
                return Ok(Some(false));
            };
            if parents.len() > self.max_parents {
                self.over_parents = true;
                return Ok(None);
            }
            let fresh: Vec<ChangesetId> = parents.into_iter().filter(|p| seen.insert(*p)).collect();
            if !self.fill_landed(conn, &fresh).await? {
                return Ok(None);
            }
            let (landed, rest): (Vec<_>, Vec<_>) =
                fresh.into_iter().partition(|p| self.is_landed(*p));
            if !self.fill_mainline(conn, &rest).await? {
                return Ok(None);
            }
            if rest.iter().any(|p| !self.on_mainline(*p)) {
                return Ok(Some(false));
            }
            todo.extend(landed);
        }
        Ok(Some(true))
    }
}

/// Sets `user_ref_meta.landed_at = at` on every user ref of `actor` whose stack has landed.
///
/// Call it in the land transaction after the `land_map` rows and the mainline steps of the land
/// are written. `landed` are the changesets this land put on `trunk` (its mainline steps, oldest
/// first; those up to and including `old_tip` were on the trunk before and are dropped);
/// `pairs` the (old, new) rewrites. A marker already set keeps its first time, so a landed ref is
/// not kept alive by later lands. An actor with no numeric id owns no user refs.
#[allow(clippy::too_many_arguments)]
pub async fn mark_landed(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    repo_id: i64,
    trunk: &str,
    actor: &str,
    landed: &[ChangesetId],
    old_tip: Option<ChangesetId>,
    pairs: &[(ChangesetId, ChangesetId)],
    at: i64,
    max_refs: usize,
) -> Result<LandedScan, sqlx::Error> {
    mark_landed_within(
        tx,
        ns,
        repo_id,
        trunk,
        actor,
        landed,
        old_tip,
        pairs,
        at,
        max_refs,
        MAX_LOOKUPS_PER_LAND,
        MAX_PARENTS_PER_NODE,
    )
    .await
}

/// [`mark_landed`] with its two caps as arguments: the lookup `budget` and the parents one
/// changeset may have (`max_parents`).
#[allow(clippy::too_many_arguments)]
async fn mark_landed_within(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    repo_id: i64,
    trunk: &str,
    actor: &str,
    landed: &[ChangesetId],
    old_tip: Option<ChangesetId>,
    pairs: &[(ChangesetId, ChangesetId)],
    at: i64,
    max_refs: usize,
    budget: usize,
    max_parents: usize,
) -> Result<LandedScan, sqlx::Error> {
    let mut scan = LandedScan::default();
    let Some(owner) = refpolicy::owner_of(&Principal { id: actor.into() }) else {
        return Ok(scan);
    };
    let prefix = format!("{USER_NS}{owner}/heads/");
    let rows = sqlx::query(
        "SELECT name, target FROM refs WHERE repo_id = $1 AND name >= $2 AND name < $3 \
         ORDER BY name LIMIT $4",
    )
    .bind(repo_id)
    .bind(&prefix)
    .bind(upper_bound(&prefix))
    .bind(i64::try_from(max_refs).unwrap_or(i64::MAX))
    .fetch_all(&mut **tx)
    .await?;
    scan.refs_read = rows.len();
    let fresh = match old_tip.and_then(|t| landed.iter().position(|c| *c == t)) {
        Some(i) => &landed[i + 1..],
        None => landed,
    };
    let mut walker = Walker {
        ns,
        trunk,
        steps: fresh
            .iter()
            .copied()
            .chain(pairs.iter().map(|(old, _)| *old))
            .collect(),
        landed: HashMap::new(),
        mainline: HashMap::new(),
        lookups: 0,
        budget,
        max_parents,
        over_parents: false,
    };
    for row in &rows {
        let name: String = row.try_get("name")?;
        let target: Vec<u8> = row.try_get("target")?;
        let (Ok(RefTarget::Changeset(tip)), Some((_, branch))) = (
            RefTarget::from_bytes(&target),
            refpolicy::parse_user_ref(&name),
        ) else {
            continue;
        };
        if walker.spent() {
            scan.truncated = true;
            break;
        }
        match walker.stack_landed(tx, tip).await? {
            Some(true) => {}
            Some(false) => continue,
            // Too many parents: this ref stays unknown; the others are still checked.
            None if walker.over_parents => continue,
            None => {
                scan.truncated = true;
                break;
            }
        }
        sqlx::query(
            "INSERT INTO user_ref_meta (ns, owner, name, landed_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (ns, owner, name) DO UPDATE \
             SET landed_at = COALESCE(user_ref_meta.landed_at, excluded.landed_at)",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(owner)
        .bind(branch)
        .bind(at)
        .execute(&mut **tx)
        .await?;
        scan.marked += 1;
    }
    scan.lookups = walker.lookups;
    Ok(scan)
}

/// The answer of [`delete_reachability`].
#[derive(Debug, PartialEq, Eq)]
pub enum Reach {
    /// The changes of the range that no other head of the owner reaches.
    Unreachable(Vec<ChangesetId>),
    /// More than [`MAX_REACH_TIPS`] tips (or [`MAX_REACH_VISITS`] visits): the PR stays open.
    CapExceeded,
}

/// What a [`delete_reachability`] call read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReachStats {
    /// `refs` rows read, all of them the owner's.
    pub refs_read: usize,
    pub visits: usize,
}

/// For `push --delete` of `deleted` (a stored user-ref name of `owner`): which of `range` (the
/// changes of the deleted ref's range, within the stack-length cap) the owner's other heads still
/// reach. `cloud_tips` are the owner's `cloud_heads` (at most 10,000), supplied by the caller. The
/// owner's other user refs are read by prefix; no other owner's rows are touched.
pub async fn delete_reachability(
    pool: &AnyPool,
    ns: Ns,
    repo_id: i64,
    owner: i64,
    deleted: &str,
    range: &[ChangesetId],
    cloud_tips: &[ChangesetId],
) -> Result<(Reach, ReachStats), sqlx::Error> {
    let mut stats = ReachStats::default();
    if cloud_tips.len() > MAX_REACH_TIPS {
        return Ok((Reach::CapExceeded, stats));
    }
    let prefix = format!("{USER_NS}{owner}/heads/");
    let limit = i64::try_from(DEFAULT_MAX_USER_REFS + 2).unwrap_or(i64::MAX);
    let mut conn = pool.acquire().await?;
    let rows = sqlx::query(
        "SELECT name, target FROM refs WHERE repo_id = $1 AND name >= $2 AND name < $3 \
         ORDER BY name LIMIT $4",
    )
    .bind(repo_id)
    .bind(&prefix)
    .bind(upper_bound(&prefix))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    stats.refs_read = rows.len();
    let mut tips: HashSet<ChangesetId> = cloud_tips.iter().copied().collect();
    for row in &rows {
        let name: String = row.try_get("name")?;
        if name == deleted {
            continue;
        }
        let target: Vec<u8> = row.try_get("target")?;
        if let Ok(RefTarget::Changeset(c)) = RefTarget::from_bytes(&target) {
            tips.insert(c);
        }
    }
    if tips.len() > MAX_REACH_TIPS {
        return Ok((Reach::CapExceeded, stats));
    }
    // Walk down from the other tips. Everything reachable is collected; a node whose generation
    // is below the lowest of the range cannot reach it, so the walk stops there.
    let mut floor = i64::MAX;
    for chunk in range.chunks(GRAPH_BATCH) {
        for (_, generation, _) in graph_rows(&mut conn, ns, chunk).await? {
            floor = floor.min(generation);
        }
    }
    let want: HashSet<ChangesetId> = range.iter().copied().collect();
    let mut reached: HashSet<ChangesetId> = HashSet::new();
    let mut seen: HashSet<ChangesetId> = tips.clone();
    let mut frontier: Vec<ChangesetId> = tips.into_iter().collect();
    while !frontier.is_empty() && reached.len() < want.len() {
        let mut next = Vec::new();
        for chunk in frontier.chunks(GRAPH_BATCH) {
            for (cs, generation, parents) in graph_rows(&mut conn, ns, chunk).await? {
                stats.visits += 1;
                if stats.visits > MAX_REACH_VISITS {
                    return Ok((Reach::CapExceeded, stats));
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
    let gone = range
        .iter()
        .filter(|c| !reached.contains(c))
        .copied()
        .collect();
    Ok((Reach::Unreachable(gone), stats))
}

async fn graph_rows(
    conn: &mut AnyConnection,
    ns: Ns,
    ids: &[ChangesetId],
) -> Result<Vec<(ChangesetId, i64, Vec<ChangesetId>)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks = (0..ids.len())
        .map(|i| format!("${}", i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT cs, gen, parents FROM cs_graph WHERE ns = $1 AND cs IN ({marks})");
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.as_bytes().to_vec());
    for id in ids {
        q = q.bind(id.as_bytes().to_vec());
    }
    let mut out = Vec::new();
    for row in q.fetch_all(&mut *conn).await? {
        let cs: Vec<u8> = row.try_get("cs")?;
        let cs = id_of(&cs)?;
        let generation: i64 = row.try_get("gen")?;
        let parents = decode_parents(&row.try_get::<Vec<u8>, _>("parents")?)?;
        out.push((cs, generation, parents));
    }
    Ok(out)
}

/// What one [`LandedReaper::run_once`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub deleted: usize,
    /// Refs updated after they landed: kept, marker cleared.
    pub cleared: usize,
    /// Markers whose ref was already gone: removed.
    pub orphaned: usize,
    /// Refs left alone because they moved during the run or a mirror repo owns them.
    pub kept: usize,
    pub failed: usize,
}

/// Deletes user refs `landed_ref_days` after their landed marker.
pub struct LandedReaper {
    registry: Arc<Registry>,
    expirer: Arc<Expirer>,
    days: u32,
}

impl LandedReaper {
    /// `days == 0` disables the expiry.
    pub fn new(registry: Arc<Registry>, expirer: Arc<Expirer>, days: u32) -> Self {
        Self {
            registry,
            expirer,
            days,
        }
    }

    /// Deletes the refs whose marker is at least `landed_ref_days` old at `at`. Reads at most
    /// [`REAP_BATCH`] markers through `user_ref_meta (ns, landed_at)`; the rest wait for the next
    /// run.
    pub async fn run_once(&self, at: i64) -> Result<ReapReport, sqlx::Error> {
        let mut report = ReapReport::default();
        if self.days == 0 {
            return Ok(report);
        }
        let cutoff = at.saturating_sub(i64::from(self.days).saturating_mul(DAY));
        let rows = sqlx::query(
            "SELECT ns, owner, name, landed_at FROM user_ref_meta \
             WHERE landed_at IS NOT NULL AND landed_at <= $1 \
             ORDER BY landed_at, ns, owner, name LIMIT $2",
        )
        .bind(cutoff)
        .bind(REAP_BATCH)
        .fetch_all(self.registry.pool())
        .await?;
        let mut repos: HashMap<Vec<u8>, Vec<(i64, String)>> = HashMap::new();
        for row in &rows {
            let ns: Vec<u8> = row.try_get("ns")?;
            let owner: i64 = row.try_get("owner")?;
            let branch: String = row.try_get("name")?;
            let landed_at: i64 = row.try_get("landed_at")?;
            if !repos.contains_key(&ns) {
                let found = sqlx::query("SELECT id, name FROM repos WHERE ns = $1")
                    .bind(ns.clone())
                    .fetch_all(self.registry.pool())
                    .await?;
                let list = found
                    .iter()
                    .filter_map(|r| Some((r.try_get("id").ok()?, r.try_get("name").ok()?)))
                    .collect();
                repos.insert(ns.clone(), list);
            }
            let stored = format!("{USER_NS}{owner}/heads/{branch}");
            let mut any_repo = false;
            for (repo_id, repo) in &repos[&ns] {
                any_repo = true;
                self.reap_one(*repo_id, repo, &stored, landed_at, at, &mut report)
                    .await;
            }
            if !any_repo {
                self.drop_marker(&ns, owner, &branch).await;
                report.orphaned += 1;
            }
            // The marker goes when the ref is gone or was kept for a newer update; a failure or
            // a lost race leaves it for the next run.
            if self.ref_gone_everywhere(&repos[&ns], &stored).await {
                self.drop_marker(&ns, owner, &branch).await;
            }
        }
        Ok(report)
    }

    async fn ref_gone_everywhere(&self, repos: &[(i64, String)], stored: &str) -> bool {
        for (repo_id, _) in repos {
            let n: Result<i64, _> =
                sqlx::query_scalar("SELECT COUNT(*) FROM refs WHERE repo_id = $1 AND name = $2")
                    .bind(*repo_id)
                    .bind(stored)
                    .fetch_one(self.registry.pool())
                    .await;
            if n.map_or(true, |n| n > 0) {
                return false;
            }
        }
        true
    }

    async fn drop_marker(&self, ns: &[u8], owner: i64, branch: &str) {
        let r = sqlx::query("DELETE FROM user_ref_meta WHERE ns = $1 AND owner = $2 AND name = $3")
            .bind(ns.to_vec())
            .bind(owner)
            .bind(branch)
            .execute(self.registry.pool())
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "landed marker: cannot remove a row");
        }
    }

    async fn clear_marker(&self, repo_id: i64, stored: &str) {
        let Some((owner, branch)) = refpolicy::parse_user_ref(stored) else {
            return;
        };
        let r = sqlx::query(
            "UPDATE user_ref_meta SET landed_at = NULL WHERE owner = $1 AND name = $2 \
             AND ns = (SELECT ns FROM repos WHERE id = $3)",
        )
        .bind(owner)
        .bind(branch)
        .bind(repo_id)
        .execute(self.registry.pool())
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "landed marker: cannot clear a row");
        }
    }

    async fn reap_one(
        &self,
        repo_id: i64,
        repo: &str,
        stored: &str,
        landed_at: i64,
        at: i64,
        report: &mut ReapReport,
    ) {
        let (Ok(name), Ok(ref_name)) = (RepoName::parse(repo), RefName::new(stored.as_bytes()))
        else {
            report.failed += 1;
            return;
        };
        let entry = match self.registry.open(&name).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(%repo, error = %e, "landed expiry: cannot open the repository");
                report.failed += 1;
                return;
            }
        };
        if entry.mode == crate::registry::RepoMode::Mirror {
            report.kept += 1;
            return;
        }
        use asc_store::refs::Refs as _;
        let current = match self.registry.refs().get(entry.key, &ref_name).await {
            Ok(Some(e)) => e,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(%repo, error = %e, "landed expiry: cannot read a ref");
                report.failed += 1;
                return;
            }
        };
        // Work pushed to the ref after it landed is not ours to delete.
        let last: Result<Option<i64>, _> = sqlx::query_scalar(
            "SELECT at FROM ref_log WHERE repo_id = $1 AND name = $2 ORDER BY seq DESC LIMIT 1",
        )
        .bind(repo_id)
        .bind(stored)
        .fetch_optional(self.registry.pool())
        .await;
        match last {
            Ok(Some(t)) if t > landed_at => {
                self.clear_marker(repo_id, stored).await;
                report.cleared += 1;
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(%repo, error = %e, "landed expiry: cannot read the ref log");
                report.failed += 1;
                return;
            }
        }
        // The internal delete path only removes user refs whose expiry has passed, so the landed
        // marker's time becomes the ref's expiry, set only while the ref still names the scanned
        // target (a push that moved it in between wins).
        let armed = sqlx::query(
            "UPDATE refs SET expires_at = $1 WHERE repo_id = $2 AND name = $3 AND target = $4",
        )
        .bind(at)
        .bind(repo_id)
        .bind(stored)
        .bind(current.target.to_bytes().to_vec())
        .execute(self.registry.pool())
        .await;
        match armed {
            Ok(r) if r.rows_affected() == 1 => {}
            Ok(_) => {
                report.kept += 1;
                return;
            }
            Err(e) => {
                tracing::warn!(%repo, error = %e, "landed expiry: cannot arm the expiry");
                report.failed += 1;
                return;
            }
        }
        match self
            .expirer
            .delete_exact(&name, stored, &current.target, at)
            .await
        {
            Ok(()) => report.deleted += 1,
            Err(s) if s.code() == tonic::Code::Aborted => report.kept += 1,
            Err(s) => {
                tracing::warn!(%repo, code = ?s.code(), "landed expiry: a deletion failed");
                report.failed += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::migrate;

    fn id(n: u32) -> ChangesetId {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        ChangesetId::from_bytes(b)
    }

    struct Db {
        pool: AnyPool,
        ns: Ns,
    }

    async fn db() -> Db {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        Db {
            pool,
            ns: Ns::random(),
        }
    }

    impl Db {
        async fn node(&self, cs: u32, parents: &[u32]) {
            let blob: Vec<u8> = parents
                .iter()
                .flat_map(|p| id(*p).as_bytes().to_vec())
                .collect();
            sqlx::query("INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, 1, $3)")
                .bind(self.ns.as_bytes().to_vec())
                .bind(id(cs).as_bytes().to_vec())
                .bind(blob)
                .execute(&self.pool)
                .await
                .unwrap();
        }

        async fn landed(&self, cs: u32) {
            sqlx::query(
                "INSERT INTO land_map (ns, old_cs, new_cs, ref, ref_log_seq, op_id) \
                 VALUES ($1, $2, $2, 'refs/heads/main', 1, $3)",
            )
            .bind(self.ns.as_bytes().to_vec())
            .bind(id(cs).as_bytes().to_vec())
            .bind(vec![0u8; 16])
            .execute(&self.pool)
            .await
            .unwrap();
        }

        async fn user_ref(&self, branch: &str, tip: u32) {
            sqlx::query("INSERT INTO refs (repo_id, name, target, version) VALUES (1, $1, $2, 1)")
                .bind(format!("{USER_NS}42/heads/{branch}"))
                .bind(RefTarget::Changeset(id(tip)).to_bytes().to_vec())
                .execute(&self.pool)
                .await
                .unwrap();
        }

        fn walker(&self, budget: usize, max_parents: usize) -> Walker<'static> {
            Walker {
                ns: self.ns,
                trunk: "refs/heads/main",
                steps: HashSet::new(),
                landed: HashMap::new(),
                mainline: HashMap::new(),
                lookups: 0,
                budget,
                max_parents,
                over_parents: false,
            }
        }

        async fn mark(&self, budget: usize, max_parents: usize) -> (LandedScan, Vec<String>) {
            let mut tx = self.pool.begin().await.unwrap();
            let scan = mark_landed_within(
                &mut tx,
                self.ns,
                1,
                "refs/heads/main",
                "42:alice",
                &[],
                None,
                &[],
                100,
                10,
                budget,
                max_parents,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            let marked = sqlx::query("SELECT name FROM user_ref_meta WHERE landed_at IS NOT NULL")
                .fetch_all(&self.pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.try_get("name").unwrap())
                .collect();
            (scan, marked)
        }
    }

    #[tokio::test]
    async fn fill_landed_reads_legacy_and_new_rows_of_the_trunk_only() {
        let d = db().await;
        // Legacy `land_map` (written before 0106) and `land_map_ref` rows, on the trunk and on
        // another ref.
        for (table, cs, refname) in [
            ("land_map", 1_u32, "refs/heads/main"),
            ("land_map", 2, "refs/heads/other"),
            ("land_map_ref", 3, "refs/heads/main"),
            ("land_map_ref", 4, "refs/heads/other"),
        ] {
            let sql = format!(
                "INSERT INTO {table} (ns, old_cs, new_cs, ref, ref_log_seq, op_id) \
                 VALUES ($1, $2, $2, $3, 1, $4)"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(d.ns.as_bytes().to_vec())
                .bind(id(cs).as_bytes().to_vec())
                .bind(refname)
                .bind(vec![0u8; 16])
                .execute(&d.pool)
                .await
                .unwrap();
        }
        let mut w = d.walker(100, 10_000);
        let mut conn = d.pool.acquire().await.unwrap();
        let ids = [id(1), id(2), id(3), id(4), id(5)];
        assert!(w.fill_landed(&mut conn, &ids).await.unwrap());
        let got: Vec<bool> = ids.iter().map(|cs| w.landed[cs]).collect();
        assert_eq!(got, [true, false, true, false, false]);
    }

    #[tokio::test]
    async fn a_node_with_more_parents_than_the_budget_stops_at_the_budget() {
        let d = db().await;
        let parents: Vec<u32> = (10..510).collect();
        d.landed(1).await;
        d.node(1, &parents).await;
        let mut w = d.walker(20, 10_000);
        let mut conn = d.pool.acquire().await.unwrap();
        assert_eq!(w.stack_landed(&mut conn, id(1)).await.unwrap(), None);
        // The tip (1) and the graph row (1) are counted; the 500 parents do not fit in what is left.
        assert_eq!(w.lookups, 2);
    }

    #[tokio::test]
    async fn a_node_over_the_parent_cap_is_unknown_without_parent_lookups() {
        let d = db().await;
        let parents: Vec<u32> = (10..20).collect();
        d.landed(1).await;
        d.node(1, &parents).await;
        let mut w = d.walker(1_000, 9);
        let mut conn = d.pool.acquire().await.unwrap();
        assert_eq!(w.stack_landed(&mut conn, id(1)).await.unwrap(), None);
        assert!(w.over_parents);
        // The tip lookup and the graph lookup only.
        assert_eq!(w.lookups, 2);
    }

    #[tokio::test]
    async fn parents_at_the_cap_are_batched_and_decided() {
        let d = db().await;
        let parents: Vec<u32> = (10..20).collect();
        d.landed(1).await;
        d.node(1, &parents).await;
        for p in &parents {
            d.landed(*p).await;
            d.node(*p, &[]).await;
        }
        let mut w = d.walker(1_000, 10);
        let mut conn = d.pool.acquire().await.unwrap();
        assert_eq!(w.stack_landed(&mut conn, id(1)).await.unwrap(), Some(true));
        // Tip, its graph row, 10 landed ids, then one graph row per parent.
        assert_eq!(w.lookups, 2 + parents.len() + parents.len());
    }

    #[tokio::test]
    async fn a_graph_over_the_budget_is_unknown_and_the_ref_is_not_marked() {
        let d = db().await;
        // A landed chain 1 <- 2 <- ... <- 10 on top of a trunk changeset 11.
        for n in 1..=10 {
            d.landed(n).await;
            d.node(n, &[n + 1]).await;
        }
        d.node(11, &[]).await;
        sqlx::query(
            "INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, 'refs/heads/main', 1, $2)",
        )
        .bind(d.ns.as_bytes().to_vec())
        .bind(id(11).as_bytes().to_vec())
        .execute(&d.pool)
        .await
        .unwrap();
        d.user_ref("chain", 1).await;
        let (scan, marked) = d.mark(6, 1_000).await;
        assert!(scan.truncated);
        assert!(marked.is_empty(), "{marked:?}");
        assert!(scan.lookups <= 6);
        // The same graph within the budget marks the ref.
        let (scan, marked) = d.mark(10_000, 1_000).await;
        assert!(!scan.truncated);
        assert_eq!(marked, ["chain"]);
    }

    #[tokio::test]
    async fn a_node_over_the_parent_cap_never_marks_and_does_not_stop_the_scan() {
        let d = db().await;
        d.landed(1).await;
        d.node(1, &[20, 21, 22]).await;
        d.landed(2).await;
        d.node(2, &[]).await;
        d.user_ref("a-wide", 1).await;
        d.user_ref("b-narrow", 2).await;
        let (scan, marked) = d.mark(10_000, 2).await;
        assert!(!scan.truncated);
        assert_eq!(marked, ["b-narrow"]);
    }
}
