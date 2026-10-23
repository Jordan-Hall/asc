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
}

impl Walker<'_> {
    fn spent(&self) -> bool {
        self.lookups >= MAX_LOOKUPS_PER_LAND
    }

    /// Whether `cs` landed on the trunk in this land or before (a `land_map` row for it).
    async fn is_landed(
        &mut self,
        conn: &mut AnyConnection,
        cs: ChangesetId,
    ) -> Result<bool, sqlx::Error> {
        if self.steps.contains(&cs) {
            return Ok(true);
        }
        if let Some(v) = self.landed.get(&cs) {
            return Ok(*v);
        }
        self.lookups += 1;
        let hit =
            sqlx::query("SELECT 1 FROM land_map_ref WHERE ns = $1 AND ref = $2 AND old_cs = $3              UNION ALL SELECT 1 FROM land_map WHERE ns = $1 AND ref = $2 AND old_cs = $3")
                .bind(self.ns.as_bytes().to_vec())
                .bind(self.trunk)
                .bind(cs.as_bytes().to_vec())
                .fetch_optional(&mut *conn)
                .await?
                .is_some();
        self.landed.insert(cs, hit);
        Ok(hit)
    }

    async fn on_mainline(
        &mut self,
        conn: &mut AnyConnection,
        cs: ChangesetId,
    ) -> Result<bool, sqlx::Error> {
        if let Some(v) = self.mainline.get(&cs) {
            return Ok(*v);
        }
        self.lookups += 1;
        let hit = sqlx::query("SELECT 1 FROM mainline WHERE ns = $1 AND ref = $2 AND cs = $3")
            .bind(self.ns.as_bytes().to_vec())
            .bind(self.trunk)
            .bind(cs.as_bytes().to_vec())
            .fetch_optional(&mut *conn)
            .await?
            .is_some();
        self.mainline.insert(cs, hit);
        Ok(hit)
    }

    /// Whether every changeset of `trunk..tip` has landed, and the stack is not empty. `None`
    /// when the lookup budget ran out.
    async fn stack_landed(
        &mut self,
        conn: &mut AnyConnection,
        tip: ChangesetId,
    ) -> Result<Option<bool>, sqlx::Error> {
        // The tip itself must have landed: a ref that merely sits on the trunk has no work.
        if !self.is_landed(conn, tip).await? {
            return Ok(Some(false));
        }
        let mut seen = HashSet::from([tip]);
        let mut todo = vec![tip];
        while let Some(cs) = todo.pop() {
            if self.spent() {
                return Ok(None);
            }
            self.lookups += 1;
            let Some(parents) = graph_parents(conn, self.ns, cs).await? else {
                return Ok(Some(false));
            };
            for p in parents {
                if !seen.insert(p) {
                    continue;
                }
                if self.is_landed(conn, p).await? {
                    todo.push(p);
                } else if !self.on_mainline(conn, p).await? {
                    return Ok(Some(false));
                }
            }
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
