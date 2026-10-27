//! Findings and the per-repository fingerprint key (`secret_findings`, `secret_scan_keys`;
//! migration 0520).
//!
//! Nothing here ever holds a secret value: a finding carries a keyed fingerprint and a masked
//! preview, both computed by the caller.

use std::collections::BTreeMap;

use asc_core::id::ChangesetId;
use asc_core::secrets::Severity;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::{AnyConnection, AnyPool, Row};

use crate::audit::NewEvent;

/// The state of a finding. The numbers are the wire values of `SecretFindingState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    Open = 1,
    Revoked = 2,
    FalsePositive = 3,
    AcceptedRisk = 4,
}

impl State {
    pub fn from_code(c: i64) -> Option<Self> {
        Some(match c {
            1 => Self::Open,
            2 => Self::Revoked,
            3 => Self::FalsePositive,
            4 => Self::AcceptedRisk,
            _ => return None,
        })
    }

    /// The key `counts_by_state` uses: the enum name without the prefix, lower case.
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Revoked => "revoked",
            Self::FalsePositive => "false_positive",
            Self::AcceptedRisk => "accepted_risk",
        }
    }
}

/// A finding to record.
#[derive(Clone, Debug)]
pub struct NewFinding {
    pub rule_id: String,
    pub severity: Severity,
    pub path: Vec<u8>,
    pub line: u32,
    pub column: u32,
    pub changeset: ChangesetId,
    pub fingerprint: String,
    pub preview: String,
}

/// A finding as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindingRow {
    pub id: i64,
    pub rule_id: String,
    pub severity: Severity,
    pub state: State,
    pub path: Vec<u8>,
    pub line: i64,
    pub column: i64,
    pub changeset: Vec<u8>,
    pub change: Vec<u8>,
    pub fingerprint: String,
    pub preview: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub resolved_by: String,
    pub resolved_at: i64,
    pub note: String,
}

const COLS: &str = "id, rule_id, severity, state, path, line, col, changeset, change, fingerprint, \
                    preview, first_seen, last_seen, resolved_by, resolved_at, note";

fn row_of(r: &sqlx::any::AnyRow) -> Result<FindingRow, sqlx::Error> {
    let severity: i64 = r.try_get("severity")?;
    let state: i64 = r.try_get("state")?;
    Ok(FindingRow {
        id: r.try_get("id")?,
        rule_id: r.try_get("rule_id")?,
        severity: i32::try_from(severity)
            .ok()
            .and_then(Severity::from_number)
            .ok_or_else(|| sqlx::Error::Decode("unknown severity".into()))?,
        state: State::from_code(state)
            .ok_or_else(|| sqlx::Error::Decode("unknown state".into()))?,
        path: r.try_get("path")?,
        line: r.try_get("line")?,
        column: r.try_get("col")?,
        changeset: r.try_get("changeset")?,
        change: r.try_get("change")?,
        fingerprint: r.try_get("fingerprint")?,
        preview: r.try_get("preview")?,
        first_seen: r.try_get("first_seen")?,
        last_seen: r.try_get("last_seen")?,
        resolved_by: r.try_get("resolved_by")?,
        resolved_at: r.try_get("resolved_at")?,
        note: r.try_get("note")?,
    })
}

/// The key fingerprints of `repo_id` are keyed with; created on first use. A key per repository
/// keeps one repository's fingerprints from matching another's, and from being guessed offline
/// without the database.
pub async fn repo_key(pool: &AnyPool, repo_id: i64, now: i64) -> Result<[u8; 32], sqlx::Error> {
    async fn read(pool: &AnyPool, repo_id: i64) -> Result<Option<[u8; 32]>, sqlx::Error> {
        let raw: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT hmac_key FROM secret_scan_keys WHERE repo_id = $1")
                .bind(repo_id)
                .fetch_optional(pool)
                .await?;
        raw.map(|v| {
            <[u8; 32]>::try_from(v).map_err(|_| sqlx::Error::Decode("damaged scan key".into()))
        })
        .transpose()
    }
    if let Some(k) = read(pool, repo_id).await? {
        return Ok(k);
    }
    let mut fresh = [0u8; 32];
    getrandom::fill(&mut fresh).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    sqlx::query(
        "INSERT INTO secret_scan_keys (repo_id, hmac_key, created) VALUES ($1, $2, $3) \
         ON CONFLICT (repo_id) DO NOTHING",
    )
    .bind(repo_id)
    .bind(fresh.to_vec())
    .bind(now)
    .execute(pool)
    .await?;
    // Whoever inserted first wins; everyone reads the stored one.
    read(pool, repo_id)
        .await?
        .ok_or_else(|| sqlx::Error::Decode("scan key missing after insert".into()))
}

/// `HMAC-SHA256(key, rule id || 0x00 || value)` as 64 lower-case hex characters.
pub fn fingerprint(key: &[u8; 32], rule_id: &str, value: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(rule_id.as_bytes());
    mac.update(&[0]);
    mac.update(value);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn next_id(conn: &mut AnyConnection, repo_id: i64) -> Result<i64, sqlx::Error> {
    let max: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM secret_findings WHERE repo_id = $1",
    )
    .bind(repo_id)
    .fetch_one(conn)
    .await?;
    Ok(max + 1)
}

/// Records `findings` of repository `repo_id`: a finding whose (fingerprint, path) is known keeps its
/// id, state and first sighting and only moves its location and `last_seen`; a new one is added
/// open. Returns the stored rows in the order given.
pub async fn upsert(
    pool: &AnyPool,
    repo_id: i64,
    findings: &[NewFinding],
    now: i64,
) -> Result<Vec<FindingRow>, sqlx::Error> {
    let mut out = Vec::with_capacity(findings.len());
    for f in findings {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut tx = pool.begin().await?;
            let updated = sqlx::query(
                "UPDATE secret_findings SET last_seen = $4, line = $5, col = $6 \
                 WHERE repo_id = $1 AND fingerprint = $2 AND path = $3",
            )
            .bind(repo_id)
            .bind(&f.fingerprint)
            .bind(f.path.clone())
            .bind(now)
            .bind(i64::from(f.line))
            .bind(i64::from(f.column))
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                let id = next_id(&mut tx, repo_id).await?;
                let inserted = sqlx::query(
                    "INSERT INTO secret_findings (repo_id, id, rule_id, severity, state, path, line, \
                     col, changeset, change, fingerprint, preview, first_seen, last_seen) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $13)",
                )
                .bind(repo_id)
                .bind(id)
                .bind(&f.rule_id)
                .bind(i64::from(f.severity.number()))
                .bind(State::Open as i64)
                .bind(f.path.clone())
                .bind(i64::from(f.line))
                .bind(i64::from(f.column))
                .bind(f.changeset.as_bytes().to_vec())
                .bind(Vec::<u8>::new())
                .bind(&f.fingerprint)
                .bind(&f.preview)
                .bind(now)
                .execute(&mut *tx)
                .await;
                match inserted {
                    Ok(_) => {}
                    // Another writer took the id or recorded the same finding first: retry.
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
            "SELECT {COLS} FROM secret_findings WHERE repo_id = $1 AND fingerprint = $2 AND path = $3"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(repo_id)
            .bind(&f.fingerprint)
            .bind(f.path.clone())
            .fetch_one(pool)
            .await?;
        out.push(row_of(&row)?);
    }
    Ok(out)
}

pub async fn get(pool: &AnyPool, repo_id: i64, id: i64) -> Result<Option<FindingRow>, sqlx::Error> {
    let q = format!("SELECT {COLS} FROM secret_findings WHERE repo_id = $1 AND id = $2");
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
    pub rule_ids: Vec<String>,
    pub min_severity: Option<Severity>,
}

/// Findings of `repo_id`, newest first, `limit + 1` rows from the one after `after` (0 = start).
pub async fn list(
    pool: &AnyPool,
    repo_id: i64,
    filter: &Filter,
    after: i64,
    limit: usize,
) -> Result<Vec<FindingRow>, sqlx::Error> {
    // Placeholders: $1 repo, $2 cursor, then states, rules, severity.
    let mut next = 3;
    let mut conds = vec!["repo_id = $1".to_owned()];
    if after > 0 {
        conds.push("id < $2".into());
    } else {
        conds.push("$2 = 0".into());
    }
    if !filter.states.is_empty() {
        let ph: Vec<String> = (0..filter.states.len())
            .map(|i| format!("${}", next + i))
            .collect();
        next += filter.states.len();
        conds.push(format!("state IN ({})", ph.join(", ")));
    }
    if !filter.rule_ids.is_empty() {
        let ph: Vec<String> = (0..filter.rule_ids.len())
            .map(|i| format!("${}", next + i))
            .collect();
        next += filter.rule_ids.len();
        conds.push(format!("rule_id IN ({})", ph.join(", ")));
    }
    if filter.min_severity.is_some() {
        conds.push(format!("severity >= ${next}"));
    }
    let q = format!(
        "SELECT {COLS} FROM secret_findings WHERE {} ORDER BY id DESC LIMIT {}",
        conds.join(" AND "),
        limit + 1
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(repo_id)
        .bind(after);
    for s in &filter.states {
        query = query.bind(*s as i64);
    }
    for r in &filter.rule_ids {
        query = query.bind(r.clone());
    }
    if let Some(s) = filter.min_severity {
        query = query.bind(i64::from(s.number()));
    }
    query.fetch_all(pool).await?.iter().map(row_of).collect()
}

/// How many findings of `repo_id` are in each state.
pub async fn counts(
    pool: &AnyPool,
    repo_id: i64,
) -> Result<BTreeMap<&'static str, u32>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT state, CAST(COUNT(*) AS BIGINT) AS n FROM secret_findings WHERE repo_id = $1 GROUP BY state",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    let mut out = BTreeMap::new();
    for s in [
        State::Open,
        State::Revoked,
        State::FalsePositive,
        State::AcceptedRisk,
    ] {
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

/// Moves finding `id` to `state`; reopening clears who resolved it and when. `None` when there is
/// no such finding. The audit event, if any, is appended in the same transaction.
#[allow(clippy::too_many_arguments)] // the columns of one transition, called from one place
pub async fn set_state(
    pool: &AnyPool,
    repo_id: i64,
    id: i64,
    state: State,
    note: &str,
    by: &str,
    now: i64,
    event: Option<&NewEvent>,
) -> Result<Option<FindingRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (resolved_by, resolved_at) = if state == State::Open {
        (String::new(), 0)
    } else {
        (by.to_owned(), now)
    };
    let done = sqlx::query(
        "UPDATE secret_findings SET state = $3, note = $4, resolved_by = $5, resolved_at = $6 \
         WHERE repo_id = $1 AND id = $2",
    )
    .bind(repo_id)
    .bind(id)
    .bind(state as i64)
    .bind(note)
    .bind(resolved_by)
    .bind(resolved_at)
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

/// How many findings of `repo_id` are open at or above `min`.
pub async fn open_count(pool: &AnyPool, repo_id: i64, min: Severity) -> Result<u64, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM secret_findings WHERE repo_id = $1 AND state = $2 AND severity >= $3",
    )
    .bind(repo_id)
    .bind(State::Open as i64)
    .bind(i64::from(min.number()))
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(n).unwrap_or(0))
}
