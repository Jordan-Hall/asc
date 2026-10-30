//! The SQL side of usage. Every query takes the owner as a bound parameter.

use std::collections::BTreeMap;

use sqlx::{AnyPool, Row};

use super::{Period, RepoRow, Totals};
use crate::ai::clock;
use crate::settings::access::Subject;

/// Quota values; 0 = none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quota {
    pub storage_bytes: u64,
    pub agent_minutes: u64,
    pub ai_spend_micro_usd: u64,
    pub bandwidth_bytes: u64,
}

impl Quota {
    /// `self`, with the instance default for each measure `self` leaves at 0.
    pub fn over(self, default: Quota) -> Quota {
        let pick = |a: u64, b: u64| if a == 0 { b } else { a };
        Quota {
            storage_bytes: pick(self.storage_bytes, default.storage_bytes),
            agent_minutes: pick(self.agent_minutes, default.agent_minutes),
            ai_spend_micro_usd: pick(self.ai_spend_micro_usd, default.ai_spend_micro_usd),
            bandwidth_bytes: pick(self.bandwidth_bytes, default.bandwidth_bytes),
        }
    }
}

/// One row of a spend report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spend {
    pub group: String,
    pub label: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: u64,
}

fn u(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn i(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `(prefix, length)` of the repositories a subject owns; the whole server for the instance.
fn prefix(s: &Subject) -> (String, i64) {
    match s {
        Subject::Instance => (String::new(), 0),
        Subject::Org(n) | Subject::User(n) | Subject::Repo(n) => {
            let p = format!("{n}/");
            let l = i64::try_from(p.len()).unwrap_or(0);
            (p, l)
        }
    }
}

/// `(kind, name)` of the AI key owner a subject is; kind 0 is every owner.
fn key_owner(s: &Subject) -> (i64, String) {
    match s {
        Subject::Instance => (0, String::new()),
        Subject::User(n) => (1, n.clone()),
        Subject::Org(n) => (2, n.clone()),
        Subject::Repo(n) => (3, n.clone()),
    }
}

/// The person filter of run queries, a closed set of fragments.
const BY_PREFIX: &str = "SUBSTR(r.repo, 1, $2) = $1";
const BY_PERSON: &str = "(LOWER(r.starter) = $1 OR (r.starter = '' AND LOWER(r.created_by) = $1))";

#[derive(Clone)]
pub struct Store {
    pool: AnyPool,
}

impl Store {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    pub async fn report(
        &self,
        repo: &str,
        day: &str,
        storage: Option<u64>,
        bandwidth: u64,
        byo: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO usage_counters (repo, day, storage_bytes, bandwidth_bytes, byo_storage) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (repo, day) DO UPDATE SET \
             storage_bytes = CASE WHEN $6 = 1 THEN $3 ELSE usage_counters.storage_bytes END, \
             bandwidth_bytes = CASE WHEN usage_counters.bandwidth_bytes > 9223372036854775807 - $4 \
             THEN 9223372036854775807 ELSE usage_counters.bandwidth_bytes + $4 END, \
             byo_storage = CASE WHEN $6 = 1 THEN $5 ELSE usage_counters.byo_storage END",
        )
        .bind(repo.to_owned())
        .bind(day.to_owned())
        .bind(storage.map_or(0, i))
        .bind(i(bandwidth))
        .bind(i64::from(byo))
        .bind(i64::from(storage.is_some()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn sweep(&self, cutoff_day: &str) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query("DELETE FROM usage_counters WHERE day < $1")
            .bind(cutoff_day.to_owned())
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    // -------------------------------------------------------------------------- quotas

    pub async fn quota(&self, scope: &str) -> Result<Quota, sqlx::Error> {
        let row = sqlx::query(
            "SELECT storage_bytes, agent_minutes, ai_spend_micro_usd, bandwidth_bytes FROM usage_quotas WHERE scope = $1",
        )
        .bind(scope.to_owned())
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else {
            return Ok(Quota::default());
        };
        Ok(Quota {
            storage_bytes: u(r.try_get("storage_bytes")?),
            agent_minutes: u(r.try_get("agent_minutes")?),
            ai_spend_micro_usd: u(r.try_get("ai_spend_micro_usd")?),
            bandwidth_bytes: u(r.try_get("bandwidth_bytes")?),
        })
    }

    pub async fn set_quota(
        &self,
        scope: &str,
        q: Quota,
        now: i64,
        by: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM usage_quotas WHERE scope = $1")
            .bind(scope.to_owned())
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO usage_quotas (scope, storage_bytes, agent_minutes, ai_spend_micro_usd, bandwidth_bytes, updated, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(scope.to_owned())
        .bind(i(q.storage_bytes))
        .bind(i(q.agent_minutes))
        .bind(i(q.ai_spend_micro_usd))
        .bind(i(q.bandwidth_bytes))
        .bind(now)
        .bind(by.to_owned())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -------------------------------------------------------------------------- the four measures

    pub async fn totals(&self, s: &Subject, p: Period, now: i64) -> Result<Totals, sqlx::Error> {
        let (pre, len) = prefix(s);
        let first = clock::day(p.start);
        let last = clock::day(p.end - 1);
        let mut repos: BTreeMap<String, RepoRow> = BTreeMap::new();
        let mut t = Totals::default();
        fn slot<'a>(m: &'a mut BTreeMap<String, RepoRow>, r: &str) -> &'a mut RepoRow {
            m.entry(r.to_owned()).or_insert_with(|| RepoRow {
                repo: r.to_owned(),
                ..RepoRow::default()
            })
        }

        // Storage (a gauge: the latest report on or before the end of the period) and bandwidth.
        // A user's storage and bandwidth are the repositories they own; the board of a person
        // shows nothing about another owner's repositories.
        for r in sqlx::query(
            "SELECT c.repo, c.storage_bytes, c.byo_storage FROM usage_counters c \
             WHERE SUBSTR(c.repo, 1, $2) = $1 AND c.day = \
             (SELECT MAX(d.day) FROM usage_counters d WHERE d.repo = c.repo AND d.day <= $3)",
        )
        .bind(pre.clone())
        .bind(len)
        .bind(last.clone())
        .fetch_all(&self.pool)
        .await?
        {
            let repo: String = r.try_get("repo")?;
            let bytes = u(r.try_get("storage_bytes")?);
            let x = slot(&mut repos, &repo);
            if r.try_get::<i64, _>("byo_storage")? != 0 {
                t.byo_storage = true;
            } else {
                t.storage_bytes += bytes;
                x.storage_bytes = bytes;
            }
        }
        // Rows, not SUM: counters saturate at the largest signed number, and a sum of saturated
        // rows would overflow the database's integer.
        for r in sqlx::query(
            "SELECT repo, bandwidth_bytes AS b FROM usage_counters \
             WHERE SUBSTR(repo, 1, $2) = $1 AND day >= $3 AND day <= $4",
        )
        .bind(pre.clone())
        .bind(len)
        .bind(first.clone())
        .bind(last.clone())
        .fetch_all(&self.pool)
        .await?
        {
            let repo: String = r.try_get("repo")?;
            let b = u(r.try_get("b")?);
            t.bandwidth_bytes = t.bandwidth_bytes.saturating_add(b);
            let x = slot(&mut repos, &repo);
            x.bandwidth_bytes = x.bandwidth_bytes.saturating_add(b);
        }

        // Agent minutes: the running time of runs that started in the period.
        let (filter, who) = match s {
            Subject::User(n) => (BY_PERSON, n.clone()),
            _ => (BY_PREFIX, pre.clone()),
        };
        let minutes = format!(
            "SELECT r.repo, CAST(COALESCE(SUM(CASE WHEN r.finished > r.started THEN r.finished - r.started \
             WHEN r.finished = 0 THEN $5 - r.started ELSE 0 END), 0) AS BIGINT) AS secs \
             FROM agent_runs r WHERE {filter} AND r.started > 0 AND r.started >= $3 AND r.started < $4 GROUP BY r.repo"
        );
        for r in sqlx::query(sqlx::AssertSqlSafe(minutes))
            .bind(who.clone())
            .bind(len)
            .bind(p.start)
            .bind(p.end)
            .bind(now)
            .fetch_all(&self.pool)
            .await?
        {
            let repo: String = r.try_get("repo")?;
            let m = u(r.try_get("secs")?).div_ceil(60);
            t.agent_minutes += m;
            slot(&mut repos, &repo).agent_minutes = m;
        }

        // AI spend. An organisation or the server: the spend of every agent run on its
        // repositories. A person: the runs they started.
        let (kind, name) = key_owner(s);
        match s {
            Subject::User(n) => {
                for r in sqlx::query(
                    "SELECT r.repo, CAST(COALESCE(SUM(t.used_cost), 0) AS BIGINT) AS cost, \
                     CAST(COALESCE(SUM(CASE WHEN k.id IS NULL THEN 0 ELSE t.used_cost END), 0) AS BIGINT) AS byo \
                     FROM agent_runs r JOIN agent_tokens t ON t.id = r.token_id \
                     LEFT JOIN ai_keys k ON k.id = r.ai_key_id AND k.owner_kind = 1 AND k.owner_name = $1 \
                     WHERE (LOWER(r.starter) = $1 OR (r.starter = '' AND LOWER(r.created_by) = $1)) \
                     AND r.created >= $2 AND r.created < $3 GROUP BY r.repo",
                )
                .bind(n.clone())
                .bind(p.start)
                .bind(p.end)
                .fetch_all(&self.pool)
                .await?
                {
                    let repo: String = r.try_get("repo")?;
                    let c = u(r.try_get("cost")?);
                    t.ai_spend += c;
                    t.byo_ai_spend += u(r.try_get("byo")?);
                    slot(&mut repos, &repo).ai_spend = c;
                }
            }
            _ => {
                for r in sqlx::query(
                    "SELECT repo, CAST(COALESCE(SUM(cost), 0) AS BIGINT) AS cost FROM agent_spend \
                     WHERE SUBSTR(repo, 1, $2) = $1 AND day >= $3 AND day <= $4 GROUP BY repo",
                )
                .bind(pre.clone())
                .bind(len)
                .bind(first.clone())
                .bind(last.clone())
                .fetch_all(&self.pool)
                .await?
                {
                    let repo: String = r.try_get("repo")?;
                    let c = u(r.try_get("cost")?);
                    t.ai_spend += c;
                    slot(&mut repos, &repo).ai_spend = c;
                }
            }
        }
        // Keys that spent, and the spend on keys their owner brought.
        let r = sqlx::query(
            "SELECT CAST(COALESCE(SUM(cost_micro_usd), 0) AS BIGINT) AS cost, \
             CAST(COUNT(DISTINCT key_id) AS BIGINT) AS keys FROM ai_usage \
             WHERE (($1 = 0 AND owner_kind <> 4) OR owner_kind = $1) AND ($1 = 0 OR owner_name = $2) \
             AND day >= $3 AND day <= $4",
        )
        .bind(kind)
        .bind(name)
        .bind(first)
        .bind(last)
        .fetch_one(&self.pool)
        .await?;
        t.ai_keys = u32::try_from(r.try_get::<i64, _>("keys")?).unwrap_or(u32::MAX);
        if !matches!(s, Subject::User(_)) {
            t.byo_ai_spend = u(r.try_get("cost")?);
        }

        let mut rows: Vec<RepoRow> = repos.into_values().collect();
        rows.sort_by(|a, b| {
            (b.ai_spend, b.storage_bytes, &a.repo).cmp(&(a.ai_spend, a.storage_bytes, &b.repo))
        });
        rows.truncate(200);
        t.repos = rows;
        Ok(t)
    }

    // -------------------------------------------------------------------------- spend reports

    /// Spend per AI key of the owner (every owner for the instance).
    pub async fn spend_by_key(
        &self,
        s: &Subject,
        first: &str,
        last: &str,
    ) -> Result<Vec<Spend>, sqlx::Error> {
        let (kind, name) = key_owner(s);
        let rows = sqlx::query(
            "SELECT u.key_id AS id, COALESCE(k.label, '') AS label, \
             CAST(SUM(u.requests) AS BIGINT) AS req, CAST(SUM(u.input_tokens) AS BIGINT) AS inp, \
             CAST(SUM(u.output_tokens) AS BIGINT) AS outp, CAST(SUM(u.cost_micro_usd) AS BIGINT) AS cost \
             FROM ai_usage u LEFT JOIN ai_keys k ON k.id = u.key_id \
             WHERE ($1 = 0 OR (u.owner_kind = $1 AND u.owner_name = $2)) AND u.day >= $3 AND u.day <= $4 \
             GROUP BY u.key_id, k.label ORDER BY cost DESC, id LIMIT 500",
        )
        .bind(kind)
        .bind(name)
        .bind(first.to_owned())
        .bind(last.to_owned())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Spend {
                    group: r.try_get("id")?,
                    label: r.try_get("label")?,
                    requests: u(r.try_get("req")?),
                    input_tokens: u(r.try_get("inp")?),
                    output_tokens: u(r.try_get("outp")?),
                    cost: u(r.try_get("cost")?),
                })
            })
            .collect()
    }

    /// Spend of agent runs on an organisation's (or the server's) repositories, grouped by
    /// repository, day or month. `grp` is one of the three fixed SQL expressions below.
    pub async fn spend_by_repo_time(
        &self,
        s: &Subject,
        grp: Grouping,
        first: &str,
        last: &str,
    ) -> Result<Vec<Spend>, sqlx::Error> {
        let (pre, len) = prefix(s);
        let expr = match grp {
            Grouping::Repo => "repo",
            Grouping::Day => "day",
            Grouping::Month => "SUBSTR(day, 1, 7)",
        };
        let q = format!(
            "SELECT {expr} AS g, CAST(SUM(requests) AS BIGINT) AS req, CAST(SUM(cost) AS BIGINT) AS cost \
             FROM agent_spend WHERE SUBSTR(repo, 1, $2) = $1 AND day >= $3 AND day <= $4 \
             GROUP BY {expr} ORDER BY {expr} LIMIT 2000"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(pre)
            .bind(len)
            .bind(first.to_owned())
            .bind(last.to_owned())
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok(Spend {
                    group: r.try_get("g")?,
                    requests: u(r.try_get("req")?),
                    cost: u(r.try_get("cost")?),
                    ..Spend::default()
                })
            })
            .collect()
    }

    /// The cost of runs, one row per run: `(person, created, repo, cost)`. For a person, their
    /// own runs; for an organisation or the server, the runs on its repositories.
    pub async fn run_costs(
        &self,
        s: &Subject,
        since: i64,
        until: i64,
    ) -> Result<Vec<(String, i64, String, u64)>, sqlx::Error> {
        let (pre, len) = prefix(s);
        let (filter, who) = match s {
            Subject::User(n) => (BY_PERSON, n.clone()),
            _ => (BY_PREFIX, pre),
        };
        let q = format!(
            "SELECT LOWER(CASE WHEN r.starter = '' THEN r.created_by ELSE r.starter END) AS person, \
             r.created, r.repo, CAST(COALESCE(t.used_cost, 0) AS BIGINT) AS cost \
             FROM agent_runs r LEFT JOIN agent_tokens t ON t.id = r.token_id \
             WHERE {filter} AND r.created >= $3 AND r.created < $4 ORDER BY r.created LIMIT 50000"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(who)
            .bind(len)
            .bind(since)
            .bind(until)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get("person")?,
                    r.try_get("created")?,
                    r.try_get("repo")?,
                    u(r.try_get("cost")?),
                ))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    Repo,
    Day,
    Month,
}
