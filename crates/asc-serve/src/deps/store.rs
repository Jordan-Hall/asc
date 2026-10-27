//! The cache, the findings and the last scan (`deps_cache`, `deps_findings`, `deps_scans`; migration 0560).

use std::collections::{BTreeMap, HashSet};

use asc_core::policy::DepSeverity;
use serde::{Deserialize, Serialize};
use sqlx::{AnyConnection, AnyPool, Row};

use super::model::Advisory;
use crate::audit::NewEvent;

/// Rows of the cache older than this are removed by [`prune`], whatever their TTL.
pub const CACHE_KEEP_SECS: i64 = 30 * 86_400;

// ------------------------------------------------------------------------------ cache

/// The cached text of `(kind, key)` when it was fetched at or after `now - max_age`.
pub async fn cache_get(
    pool: &AnyPool,
    kind: &str,
    key: &str,
    max_age_secs: u64,
    now: i64,
) -> Result<Option<String>, sqlx::Error> {
    let min = now.saturating_sub(i64::try_from(max_age_secs).unwrap_or(i64::MAX));
    sqlx::query_scalar(
        "SELECT value FROM deps_cache WHERE kind = $1 AND ckey = $2 AND fetched >= $3",
    )
    .bind(kind)
    .bind(key)
    .bind(min)
    .fetch_optional(pool)
    .await
}

pub async fn cache_put(
    pool: &AnyPool,
    kind: &str,
    key: &str,
    value: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO deps_cache (kind, ckey, value, fetched) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (kind, ckey) DO UPDATE SET value = $3, fetched = $4",
    )
    .bind(kind)
    .bind(key)
    .bind(value)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes cache rows older than [`CACHE_KEEP_SECS`]. Returns how many.
pub async fn prune(pool: &AnyPool, now: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM deps_cache WHERE fetched < $1")
        .bind(now - CACHE_KEEP_SECS)
        .execute(pool)
        .await?
        .rows_affected())
}

// ------------------------------------------------------------------------------ findings

/// The state of a finding. The numbers are the wire values of `DepFindingState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    Open = 1,
    Dismissed = 2,
    Fixed = 3,
}

impl State {
    pub fn from_code(c: i64) -> Option<Self> {
        Some(match c {
            1 => Self::Open,
            2 => Self::Dismissed,
            3 => Self::Fixed,
            _ => return None,
        })
    }

    /// The key `counts_by_state` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Dismissed => "dismissed",
            Self::Fixed => "fixed",
        }
    }
}

pub fn severity_code(s: Option<DepSeverity>) -> i64 {
    match s {
        None => 0,
        Some(DepSeverity::Low) => 1,
        Some(DepSeverity::Moderate) => 2,
        Some(DepSeverity::High) => 3,
        Some(DepSeverity::Critical) => 4,
    }
}

pub fn severity_of_code(c: i64) -> Option<DepSeverity> {
    match c {
        1 => Some(DepSeverity::Low),
        2 => Some(DepSeverity::Moderate),
        3 => Some(DepSeverity::High),
        4 => Some(DepSeverity::Critical),
        _ => None,
    }
}

#[derive(Serialize, Deserialize)]
struct Detail {
    aliases: Vec<String>,
    summary: String,
    cvss: f64,
    fixed: Vec<String>,
    url: String,
}

/// A finding to record.
#[derive(Clone, Debug)]
pub struct NewFinding {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub path: String,
    pub direct: bool,
    pub advisory: Advisory,
    pub fixed_in: String,
}

/// A finding as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct FindingRow {
    pub id: i64,
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub path: String,
    pub direct: bool,
    pub advisory: Advisory,
    pub state: State,
    pub fixed_in: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub dismissed_by: String,
    pub dismissed_at: i64,
    pub dismiss_reason: i64,
    pub note: String,
}

const COLS: &str = "id, ecosystem, name, version, path, advisory, detail, severity, direct, state, \
                    fixed_in, first_seen, last_seen, dismissed_by, dismissed_at, dismiss_reason, note";

fn row_of(r: &sqlx::any::AnyRow) -> Result<FindingRow, sqlx::Error> {
    let detail: String = r.try_get("detail")?;
    let d: Detail = serde_json::from_str(&detail).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let state: i64 = r.try_get("state")?;
    let severity: i64 = r.try_get("severity")?;
    Ok(FindingRow {
        id: r.try_get("id")?,
        ecosystem: r.try_get("ecosystem")?,
        name: r.try_get("name")?,
        version: r.try_get("version")?,
        path: r.try_get("path")?,
        direct: r.try_get::<i64, _>("direct")? != 0,
        advisory: Advisory {
            id: r.try_get("advisory")?,
            aliases: d.aliases,
            summary: d.summary,
            severity: severity_of_code(severity),
            cvss: d.cvss,
            fixed: d.fixed,
            url: d.url,
            affected: Vec::new(),
        },
        state: State::from_code(state)
            .ok_or_else(|| sqlx::Error::Decode("unknown finding state".into()))?,
        fixed_in: r.try_get("fixed_in")?,
        first_seen: r.try_get("first_seen")?,
        last_seen: r.try_get("last_seen")?,
        dismissed_by: r.try_get("dismissed_by")?,
        dismissed_at: r.try_get("dismissed_at")?,
        dismiss_reason: r.try_get("dismiss_reason")?,
        note: r.try_get("note")?,
    })
}

async fn next_id(conn: &mut AnyConnection, repo_id: i64) -> Result<i64, sqlx::Error> {
    let max: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM deps_findings WHERE repo_id = $1",
    )
    .bind(repo_id)
    .fetch_one(conn)
    .await?;
    Ok(max + 1)
}

/// Which package of which file a finding is about: `(ecosystem, name, version, path)`.
pub type PackageKey = (String, String, String, String);

/// What a scan of trunk found, to record.
pub struct Scan<'a> {
    pub found: &'a [NewFinding],
    /// Packages the scan saw but could not judge (lookup failed or no data): their findings stay as
    /// they were.
    pub unjudged: &'a HashSet<PackageKey>,
    /// Ecosystems the scan covered; findings of others are left alone.
    pub ecosystems: &'a HashSet<String>,
    /// The scan read every file there was. Otherwise nothing is marked fixed.
    pub complete: bool,
}

/// Records a scan of trunk. A finding already known keeps its id, state and first sighting and moves
/// `last_seen`; a fixed one that came back reopens; a new one is added open. When the scan was
/// complete, an open or dismissed finding of a covered ecosystem that this scan did not find, and
/// whose package it could judge, becomes fixed. Returns the rows of `found`, in order.
pub async fn record_scan(
    pool: &AnyPool,
    repo_id: i64,
    scan: &Scan<'_>,
    now: i64,
) -> Result<Vec<FindingRow>, sqlx::Error> {
    let mut out = Vec::with_capacity(scan.found.len());
    for f in scan.found {
        let detail = serde_json::to_string(&Detail {
            aliases: f.advisory.aliases.clone(),
            summary: f.advisory.summary.clone(),
            cvss: f.advisory.cvss,
            fixed: f.advisory.fixed.clone(),
            url: f.advisory.url.clone(),
        })
        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut tx = pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE deps_findings SET last_seen = $7, detail = $8, severity = $9, direct = $10, \
                 fixed_in = $11, \
                 state = CASE WHEN state = 3 THEN 1 ELSE state END, \
                 dismissed_by = CASE WHEN state = 3 THEN '' ELSE dismissed_by END, \
                 dismissed_at = CASE WHEN state = 3 THEN 0 ELSE dismissed_at END, \
                 dismiss_reason = CASE WHEN state = 3 THEN 0 ELSE dismiss_reason END, \
                 note = CASE WHEN state = 3 THEN '' ELSE note END \
                 WHERE repo_id = $1 AND ecosystem = $2 AND name = $3 AND version = $4 AND path = $5 \
                 AND advisory = $6",
            )
            .bind(repo_id)
            .bind(&f.ecosystem)
            .bind(&f.name)
            .bind(&f.version)
            .bind(&f.path)
            .bind(&f.advisory.id)
            .bind(now)
            .bind(&detail)
            .bind(severity_code(f.advisory.severity))
            .bind(i64::from(f.direct))
            .bind(&f.fixed_in)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                let id = next_id(&mut tx, repo_id).await?;
                let inserted = sqlx::query(
                    "INSERT INTO deps_findings (repo_id, id, ecosystem, name, version, path, advisory, \
                     detail, severity, direct, state, fixed_in, first_seen, last_seen) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 1, $11, $12, $12)",
                )
                .bind(repo_id)
                .bind(id)
                .bind(&f.ecosystem)
                .bind(&f.name)
                .bind(&f.version)
                .bind(&f.path)
                .bind(&f.advisory.id)
                .bind(&detail)
                .bind(severity_code(f.advisory.severity))
                .bind(i64::from(f.direct))
                .bind(&f.fixed_in)
                .bind(now)
                .execute(&mut *tx)
                .await;
                match inserted {
                    Ok(_) => {}
                    Err(sqlx::Error::Database(d)) if d.is_unique_violation() && attempts < 5 => {
                        drop(tx);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            tx.commit().await?;
            break;
        }
        let q = format!(
            "SELECT {COLS} FROM deps_findings WHERE repo_id = $1 AND ecosystem = $2 AND name = $3 \
             AND version = $4 AND path = $5 AND advisory = $6"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(repo_id)
            .bind(&f.ecosystem)
            .bind(&f.name)
            .bind(&f.version)
            .bind(&f.path)
            .bind(&f.advisory.id)
            .fetch_one(pool)
            .await?;
        out.push(row_of(&row)?);
    }
    if scan.complete {
        let still: HashSet<(&str, &str, &str, &str, &str)> = scan
            .found
            .iter()
            .map(|f| {
                (
                    f.ecosystem.as_str(),
                    f.name.as_str(),
                    f.version.as_str(),
                    f.path.as_str(),
                    f.advisory.id.as_str(),
                )
            })
            .collect();
        let rows = sqlx::query(
            "SELECT id, ecosystem, name, version, path, advisory FROM deps_findings \
             WHERE repo_id = $1 AND state IN (1, 2)",
        )
        .bind(repo_id)
        .fetch_all(pool)
        .await?;
        for r in rows {
            let (eco, name, version, path, adv): (String, String, String, String, String) = (
                r.try_get("ecosystem")?,
                r.try_get("name")?,
                r.try_get("version")?,
                r.try_get("path")?,
                r.try_get("advisory")?,
            );
            if !scan.ecosystems.contains(&eco)
                || still.contains(&(&eco, &name, &version, &path, &adv))
                || scan.unjudged.contains(&(
                    eco.clone(),
                    name.clone(),
                    version.clone(),
                    path.clone(),
                ))
            {
                continue;
            }
            let id: i64 = r.try_get("id")?;
            sqlx::query(
                "UPDATE deps_findings SET state = 3, last_seen = $3 WHERE repo_id = $1 AND id = $2",
            )
            .bind(repo_id)
            .bind(id)
            .bind(now)
            .execute(pool)
            .await?;
        }
    }
    Ok(out)
}

pub async fn get(pool: &AnyPool, repo_id: i64, id: i64) -> Result<Option<FindingRow>, sqlx::Error> {
    let q = format!("SELECT {COLS} FROM deps_findings WHERE repo_id = $1 AND id = $2");
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(repo_id)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(row_of)
        .transpose()
}

/// A page filter for [`list`].
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub states: Vec<State>,
    pub ecosystems: Vec<String>,
    pub min_severity: i64,
    pub name: String,
}

/// Findings of `repo_id`, newest first, `limit + 1` rows from the one after `after` (0 = start).
pub async fn list(
    pool: &AnyPool,
    repo_id: i64,
    filter: &Filter,
    after: i64,
    limit: usize,
) -> Result<Vec<FindingRow>, sqlx::Error> {
    // Placeholders: $1 repo, $2 cursor, $3 minimum severity, $4 name (empty: any), then lists.
    let mut next = 5;
    let mut conds = vec![
        "repo_id = $1".to_owned(),
        if after > 0 { "id < $2" } else { "$2 = 0" }.to_owned(),
        "severity >= $3".to_owned(),
        "($4 = '' OR name = $4)".to_owned(),
    ];
    if !filter.states.is_empty() {
        let ph: Vec<String> = (0..filter.states.len())
            .map(|i| format!("${}", next + i))
            .collect();
        next += filter.states.len();
        conds.push(format!("state IN ({})", ph.join(", ")));
    }
    if !filter.ecosystems.is_empty() {
        let ph: Vec<String> = (0..filter.ecosystems.len())
            .map(|i| format!("${}", next + i))
            .collect();
        conds.push(format!("ecosystem IN ({})", ph.join(", ")));
    }
    let q = format!(
        "SELECT {COLS} FROM deps_findings WHERE {} ORDER BY id DESC LIMIT {}",
        conds.join(" AND "),
        limit + 1
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(repo_id)
        .bind(after)
        .bind(filter.min_severity)
        .bind(filter.name.clone());
    for s in &filter.states {
        query = query.bind(*s as i64);
    }
    for e in &filter.ecosystems {
        query = query.bind(e.clone());
    }
    query.fetch_all(pool).await?.iter().map(row_of).collect()
}

/// How many findings of `repo_id` are in each state.
pub async fn counts(
    pool: &AnyPool,
    repo_id: i64,
) -> Result<BTreeMap<&'static str, u32>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT state, CAST(COUNT(*) AS BIGINT) AS n FROM deps_findings WHERE repo_id = $1 GROUP BY state",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    let mut out = BTreeMap::new();
    for s in [State::Open, State::Dismissed, State::Fixed] {
        out.insert(s.name(), 0);
    }
    for r in rows {
        let code: i64 = r.try_get("state")?;
        let n: i64 = r.try_get("n")?;
        if let Some(s) = State::from_code(code) {
            out.insert(s.name(), u32::try_from(n).unwrap_or(u32::MAX));
        }
    }
    Ok(out)
}

/// Moves finding `id` to `state` (open reopens). `None` when there is no such finding. The audit event
/// is appended in the same transaction.
#[allow(clippy::too_many_arguments)] // the columns of one transition, called from one place
pub async fn set_state(
    pool: &AnyPool,
    repo_id: i64,
    id: i64,
    state: State,
    reason: i64,
    note: &str,
    by: &str,
    now: i64,
    event: Option<&NewEvent>,
) -> Result<Option<FindingRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (by, at, reason) = if state == State::Open {
        (String::new(), 0, 0)
    } else {
        (by.to_owned(), now, reason)
    };
    let done = sqlx::query(
        "UPDATE deps_findings SET state = $3, note = $4, dismissed_by = $5, dismissed_at = $6, \
         dismiss_reason = $7 WHERE repo_id = $1 AND id = $2",
    )
    .bind(repo_id)
    .bind(id)
    .bind(state as i64)
    .bind(if state == State::Open { "" } else { note })
    .bind(by)
    .bind(at)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(None);
    }
    if let Some(e) = event {
        crate::audit::store::append_in_tx(&mut tx, e)
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    }
    tx.commit().await?;
    get(pool, repo_id, id).await
}

/// The advisories a maintainer dismissed for a package version of `repo_id`, as
/// `(ecosystem, name, version, advisory)`; the path does not matter to a landing.
pub async fn dismissed(
    pool: &AnyPool,
    repo_id: i64,
) -> Result<HashSet<(String, String, String, String)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT ecosystem, name, version, advisory FROM deps_findings WHERE repo_id = $1 AND state = 2 LIMIT 100000",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("ecosystem")?,
                r.try_get("name")?,
                r.try_get("version")?,
                r.try_get("advisory")?,
            ))
        })
        .collect()
}

// ------------------------------------------------------------------------------ scans

/// The last scan of trunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRow {
    pub at: i64,
    pub rev: Vec<u8>,
    pub lookup: i64,
    pub packages: i64,
    pub unknown: i64,
}

pub async fn put_scan(pool: &AnyPool, repo_id: i64, s: &ScanRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO deps_scans (repo_id, at, rev, lookup, packages, unknown) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (repo_id) DO UPDATE SET at = $2, rev = $3, lookup = $4, packages = $5, unknown = $6",
    )
    .bind(repo_id)
    .bind(s.at)
    .bind(s.rev.clone())
    .bind(s.lookup)
    .bind(s.packages)
    .bind(s.unknown)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn last_scan(pool: &AnyPool, repo_id: i64) -> Result<Option<ScanRow>, sqlx::Error> {
    let r =
        sqlx::query("SELECT at, rev, lookup, packages, unknown FROM deps_scans WHERE repo_id = $1")
            .bind(repo_id)
            .fetch_optional(pool)
            .await?;
    r.map(|r| {
        Ok(ScanRow {
            at: r.try_get("at")?,
            rev: r.try_get("rev")?,
            lookup: r.try_get("lookup")?,
            packages: r.try_get("packages")?,
            unknown: r.try_get("unknown")?,
        })
    })
    .transpose()
}
