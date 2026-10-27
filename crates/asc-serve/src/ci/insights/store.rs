//! The data behind the `CiInsights` service: test cases, failure groups, flaky verdicts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use asc_ci::insights as ci;
use asc_core::policy::CiPolicy;
use asc_core::secrets::RuleSet;
use asc_rpc::server::{CallCtx, wire};
use bytes::Bytes;
use prost::Message as _;
use sqlx::any::AnyRow;
use sqlx::{AnyConnection, AnyPool, Row};
use tonic::Status;
use uuid::Uuid;

use super::{
    DEFAULT_FLAKY, DEFAULT_GROUPS, DEFAULT_OCCURRENCES, DEFAULT_WINDOW_DAYS, Insights, MAX_CASES,
    MAX_FLAKY, MAX_GROUPS, MAX_NAME, MAX_NOTE, MAX_OCCURRENCES, MAX_RECOMPUTE,
    MAX_REPORT_BYTES, MAX_WINDOW_DAYS,
};
use crate::audit::{self, NewEvent};
use crate::registry::{Ns, RepoEntry};
use crate::review::{
    WriteError, next_id, now, page_limit, placeholders, retry_conflict, sql as db,
};

const PASSED: i64 = wire::TestOutcome::Passed as i64;
const FAILED: i64 = wire::TestOutcome::Failed as i64;
const SKIPPED: i64 = wire::TestOutcome::Skipped as i64;
const ERROR: i64 = wire::TestOutcome::Error as i64;
const OPEN: i64 = wire::FailureGroupState::Open as i64;
const QUARANTINED: i64 = wire::FailureGroupState::Quarantined as i64;
const RESOLVED: i64 = wire::FailureGroupState::Resolved as i64;
const DAY: i64 = 86_400;
/// Days of results the flaky verdict reads.
const VERDICT_DAYS: i64 = 180;
/// Most results of one test the verdict reads.
const MAX_OBSERVATIONS: i64 = 20_000;
/// Tag of the `op_results` value of `SetFailureGroupState`.
const OP_TAG: &[u8; 4] = b"fgs1";

fn bad(m: &str) -> Status {
    Status::invalid_argument(m.to_owned())
}

fn ns_bytes(ns: Ns) -> Vec<u8> {
    ns.as_bytes().to_vec()
}

fn is_failure(outcome: i64) -> bool {
    outcome == FAILED || outcome == ERROR
}

/// Masks the values the secret rules find in `text` with `***`.
fn mask_secrets(rules: &RuleSet, text: &str) -> String {
    let scanned = rules.scan(text.as_bytes());
    if scanned.hits.is_empty() {
        return text.to_owned();
    }
    let mut ranges: Vec<std::ops::Range<usize>> = scanned.hits.into_iter().map(|h| h.value).collect();
    ranges.sort_by_key(|r| r.start);
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    for r in ranges {
        if r.start < at || r.end > text.len() {
            continue;
        }
        out.extend_from_slice(&text.as_bytes()[at..r.start]);
        out.extend_from_slice(b"***");
        at = r.end;
    }
    out.extend_from_slice(&text.as_bytes()[at..]);
    String::from_utf8_lossy(&out).into_owned()
}

/// A case after validation and normalisation, ready to store.
struct Prepared {
    name: String,
    outcome: i64,
    attempt: i64,
    duration_ms: i64,
    message: String,
    /// Hex signature, title and location of a failure.
    failure: Option<(String, String, String)>,
}

fn outcome_of(n: i32) -> Result<i64, Status> {
    match wire::TestOutcome::try_from(n) {
        Ok(wire::TestOutcome::Unspecified) | Err(_) => Err(bad("a test case needs an outcome")),
        Ok(o) => Ok(o as i64),
    }
}

/// Validates the cases of a report.
fn validate(req: &wire::ReportTestCasesRequest) -> Result<(), Status> {
    if req.run_id == 0 || req.job_id == 0 || i64::try_from(req.run_id).is_err() || i64::try_from(req.job_id).is_err() {
        return Err(bad("run_id and job_id are required"));
    }
    if req.cases.len() > MAX_CASES {
        return Err(bad("at most 5000 test cases per call"));
    }
    if req.head.len() > 64 {
        return Err(bad("head is at most 64 bytes"));
    }
    let mut bytes = 0usize;
    for c in &req.cases {
        outcome_of(c.outcome)?;
        if c.name.len() > MAX_NAME || c.name.chars().any(char::is_control) {
            return Err(bad("a test name is at most 512 bytes of text"));
        }
        if c.name.is_empty() && !is_failure(outcome_of(c.outcome)?) {
            return Err(bad("only a failure can have no test name (a job-level failure)"));
        }
        bytes += c.name.len() + c.message.len() + 16;
    }
    if bytes > MAX_REPORT_BYTES {
        return Err(bad("a report is at most 16 MiB"));
    }
    Ok(())
}

/// Masks, cuts and normalises the cases (CPU work, off the async threads).
fn prepare(rules: &RuleSet, job: &str, cases: Vec<wire::TestCase>) -> Vec<Prepared> {
    cases
        .into_iter()
        .map(|c| {
            let outcome = outcome_of(c.outcome).unwrap_or(ERROR);
            let message = if is_failure(outcome) {
                mask_secrets(rules, &ci::truncate_message(&c.message))
            } else {
                String::new()
            };
            let failure = is_failure(outcome).then(|| {
                let n = ci::normalise(&message);
                let sig = ci::signature(job, &c.name, &n);
                (ci::signature_hex(&sig), n.title, n.location)
            });
            Prepared {
                name: c.name,
                outcome,
                attempt: i64::from(c.attempt.max(1)),
                duration_ms: i64::from(c.duration_ms),
                message,
                failure,
            }
        })
        .collect()
}

impl Insights {
    async fn open(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry
            .open(&ctx.repo)
            .await
            .map_err(crate::commit_graph_rpc::registry_status)
    }

    /// The `[ci]` policy in force and the secret rules of the repository.
    async fn rules(&self, entry: &RepoEntry) -> Result<(CiPolicy, RuleSet), Status> {
        let loaded = self.policy.in_force(entry).await?;
        Ok(match loaded.policy() {
            Some(p) => (
                p.ci,
                RuleSet::with_custom(&p.secrets.rules).unwrap_or_else(|_| RuleSet::builtin()),
            ),
            None => (CiPolicy::default(), RuleSet::builtin()),
        })
    }

    /// Whether the `[ci]` policy of the repository allows quarantine, and its flaky threshold.
    pub(crate) async fn ci_policy(&self, entry: &RepoEntry) -> Result<CiPolicy, Status> {
        Ok(self.rules(entry).await?.0)
    }

    // -------------------------------------------------------------------------- report

    /// `CiInsights.ReportTestCases`.
    pub async fn report(
        &self,
        ctx: &CallCtx,
        req: wire::ReportTestCasesRequest,
    ) -> Result<wire::ReportTestCasesResponse, Status> {
        validate(&req)?;
        let entry = self.open(ctx).await?;
        let (policy, rules) = self.rules(&entry).await?;
        let (run, job) = (req.run_id as i64, req.job_id as i64);
        let (job_name, _) = self.job_of(&entry, run, job).await?;
        let cases = req.cases.clone();
        let prepared = {
            let job_name = job_name.clone();
            tokio::task::spawn_blocking(move || prepare(&rules, &job_name, cases))
                .await
                .map_err(|e| crate::review::internal("report", &e))?
        };
        let head = req.head.to_vec();
        let threshold = policy.flaky_threshold;
        let out = retry_conflict(|| self.try_report(&entry, run, job, &head, &prepared, threshold)).await?;
        Ok(out)
    }

    async fn job_of(&self, entry: &RepoEntry, run: i64, job: i64) -> Result<(String, Vec<u8>), Status> {
        let mut conn = self.registry.pool().acquire().await.map_err(db)?;
        job_row(&mut conn, entry.ns, run, job).await
    }

    async fn try_report(
        &self,
        entry: &RepoEntry,
        run: i64,
        job: i64,
        head: &[u8],
        cases: &[Prepared],
        threshold: f64,
    ) -> Result<wire::ReportTestCasesResponse, WriteError> {
        let ns = entry.ns;
        let nsb = ns_bytes(ns);
        let at = now();
        let mut tx = self.registry.pool().begin().await?;
        self.registry.guard(&mut tx, entry).await?;
        let (job_name, change) = job_row(&mut tx, ns, run, job).await?;
        let version: i64 = sqlx::query_scalar("SELECT version FROM check_runs WHERE ns = $1 AND id = $2")
            .bind(nsb.clone())
            .bind(run)
            .fetch_one(&mut *tx)
            .await?;
        let head = if !head.is_empty() {
            head.to_vec()
        } else if !change.is_empty() && version > 0 {
            sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT cs FROM change_versions WHERE ns = $1 AND change = $2 AND v = $3",
            )
            .bind(nsb.clone())
            .bind(change.clone())
            .bind(version)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default()
        } else {
            Vec::new()
        };

        // A report replaces what the same (run, job) reported before.
        let before: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT group_id FROM test_cases WHERE ns = $1 AND run_id = $2 AND job_id = $3 AND group_id <> 0",
        )
        .bind(nsb.clone())
        .bind(run)
        .bind(job)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM test_cases WHERE ns = $1 AND run_id = $2 AND job_id = $3")
            .bind(nsb.clone())
            .bind(run)
            .bind(job)
            .execute(&mut *tx)
            .await?;

        let mut groups: HashMap<String, i64> = HashMap::new();
        let mut touched: BTreeSet<i64> = BTreeSet::new();
        for (seq, c) in cases.iter().enumerate() {
            let group = match &c.failure {
                Some((sig, title, location)) => {
                    let id = match groups.get(sig) {
                        Some(id) => *id,
                        None => {
                            let id = upsert_group(&mut tx, ns, sig, title, location, &c.name, &job_name, at).await?;
                            groups.insert(sig.clone(), id);
                            id
                        }
                    };
                    touched.insert(id);
                    id
                }
                None => 0,
            };
            sqlx::query(
                "INSERT INTO test_cases (ns, run_id, job_id, seq, job_name, name, outcome, attempt, \
                 duration_ms, group_id, message, change, version, head, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
            )
            .bind(nsb.clone())
            .bind(run)
            .bind(job)
            .bind(i64::try_from(seq).unwrap_or(i64::MAX))
            .bind(job_name.clone())
            .bind(c.name.clone())
            .bind(c.outcome)
            .bind(c.attempt)
            .bind(c.duration_ms)
            .bind(group)
            .bind(c.message.clone())
            .bind(change.clone())
            .bind(version)
            .bind(head.clone())
            .bind(at)
            .execute(&mut *tx)
            .await?;
        }
        // A group that lost its last occurrence to this replacement and was never triaged goes away.
        for old in before.iter().filter(|g| !touched.contains(g)) {
            sqlx::query(
                "DELETE FROM failure_groups WHERE ns = $1 AND id = $2 AND state = $3 \
                 AND NOT EXISTS (SELECT 1 FROM test_cases WHERE ns = $1 AND group_id = $2)",
            )
            .bind(nsb.clone())
            .bind(*old)
            .bind(OPEN)
            .execute(&mut *tx)
            .await?;
        }
        self.recompute_flaky(&mut tx, ns, &job_name, cases, threshold, at).await?;
        tx.commit().await?;
        Ok(wire::ReportTestCasesResponse {
            stored: u32::try_from(cases.len()).unwrap_or(u32::MAX),
            groups_touched: u32::try_from(touched.len()).unwrap_or(u32::MAX),
        })
    }

    /// Recomputes the flaky verdict of the tests that have both verdicts in their history.
    async fn recompute_flaky(
        &self,
        conn: &mut AnyConnection,
        ns: Ns,
        job_name: &str,
        cases: &[Prepared],
        threshold: f64,
        at: i64,
    ) -> Result<(), sqlx::Error> {
        let nsb = ns_bytes(ns);
        let since = at - VERDICT_DAYS * DAY;
        let names: BTreeSet<&str> = cases
            .iter()
            .filter(|c| !c.name.is_empty() && c.outcome != SKIPPED)
            .map(|c| c.name.as_str())
            .collect();
        let mut candidates: BTreeSet<String> = cases
            .iter()
            .filter(|c| !c.name.is_empty() && is_failure(c.outcome))
            .map(|c| c.name.clone())
            .collect();
        let all: Vec<&str> = names.iter().copied().collect();
        for chunk in all.chunks(100) {
            // Tests that failed some time before, and tests that are flagged now.
            for q in [
                "SELECT DISTINCT name FROM test_cases WHERE ns = $1 AND job_name = $2 AND at >= $3 \
                 AND outcome IN (2, 4) AND name IN",
                "SELECT test AS name FROM flaky_tests WHERE ns = $1 AND job_name = $2 AND $3 >= 0 \
                 AND flaky = 1 AND test IN",
            ] {
                let sql = format!("{q} ({})", placeholders(4, chunk.len()));
                let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(nsb.clone())
                    .bind(job_name.to_owned())
                    .bind(since);
                for n in chunk {
                    query = query.bind((*n).to_owned());
                }
                for r in query.fetch_all(&mut *conn).await? {
                    candidates.insert(r.try_get::<String, _>("name")?);
                }
            }
        }
        for test in candidates.into_iter().take(MAX_RECOMPUTE) {
            let rows = sqlx::query(
                "SELECT run_id, at, attempt, outcome, head FROM test_cases \
                 WHERE ns = $1 AND job_name = $2 AND name = $3 AND at >= $4 AND outcome <> $5 \
                 ORDER BY at, run_id, attempt LIMIT $6",
            )
            .bind(nsb.clone())
            .bind(job_name.to_owned())
            .bind(test.clone())
            .bind(since)
            .bind(SKIPPED)
            .bind(MAX_OBSERVATIONS)
            .fetch_all(&mut *conn)
            .await?;
            let mut obs = Vec::with_capacity(rows.len());
            for r in &rows {
                let outcome: i64 = r.try_get("outcome")?;
                obs.push(ci::Observation {
                    run: u64::try_from(r.try_get::<i64, _>("run_id")?).unwrap_or(0),
                    at: r.try_get("at")?,
                    attempt: u32::try_from(r.try_get::<i64, _>("attempt")?).unwrap_or(1),
                    passed: outcome == PASSED,
                    head: r.try_get("head")?,
                });
            }
            let stats = ci::analyse(&obs, self.cfg.flaky_window_runs as usize, threshold);
            sqlx::query(
                "INSERT INTO flaky_tests (ns, job_name, test, runs, failures, flips, first_flip, last_flip, flaky, updated) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (ns, job_name, test) DO UPDATE SET runs = excluded.runs, failures = excluded.failures, \
                 flips = excluded.flips, first_flip = excluded.first_flip, last_flip = excluded.last_flip, \
                 flaky = excluded.flaky, updated = excluded.updated",
            )
            .bind(nsb.clone())
            .bind(job_name.to_owned())
            .bind(test.clone())
            .bind(i64::from(stats.runs))
            .bind(i64::from(stats.failures))
            .bind(i64::from(stats.flips))
            .bind(stats.first_flip.unwrap_or(0))
            .bind(stats.last_flip.unwrap_or(0))
            .bind(i64::from(stats.flaky))
            .bind(at)
            .execute(&mut *conn)
            .await?;
            sqlx::query("DELETE FROM flaky_daily WHERE ns = $1 AND job_name = $2 AND test = $3")
                .bind(nsb.clone())
                .bind(job_name.to_owned())
                .bind(test.clone())
                .execute(&mut *conn)
                .await?;
            for b in &stats.buckets {
                sqlx::query(
                    "INSERT INTO flaky_daily (ns, job_name, test, day, runs, failures, flips) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(nsb.clone())
                .bind(job_name.to_owned())
                .bind(test.clone())
                .bind(b.start)
                .bind(i64::from(b.runs))
                .bind(i64::from(b.failures))
                .bind(i64::from(b.flips))
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------- groups

    /// `CiInsights.ListFailureGroups`.
    pub async fn list_groups(
        &self,
        ctx: &CallCtx,
        req: wire::ListFailureGroupsRequest,
    ) -> Result<wire::ListFailureGroupsResponse, Status> {
        if req.job_name.len() > MAX_NAME || req.test_name.len() > MAX_NAME {
            return Err(bad("job_name and test_name are at most 512 bytes"));
        }
        if req.since < 0 {
            return Err(bad("since is a unix time"));
        }
        let mut states = Vec::new();
        for s in &req.states {
            match wire::FailureGroupState::try_from(*s) {
                Ok(wire::FailureGroupState::Unspecified) => {}
                Ok(s) => states.push(s as i64),
                Err(_) => return Err(bad("unknown state")),
            }
        }
        let cursor = parse_cursor(&req.page_token, 2)?;
        let limit = page_limit(req.limit, DEFAULT_GROUPS, MAX_GROUPS);
        let entry = self.open(ctx).await?;
        let nsb = ns_bytes(entry.ns);
        let mut binds: Vec<Bind> = vec![Bind::Blob(nsb.clone())];
        let mut cond = String::from("g.ns = $1");
        if !states.is_empty() {
            let ps: Vec<String> = states.iter().map(|s| push(&mut binds, Bind::Int(*s))).collect();
            cond.push_str(&format!(" AND g.state IN ({})", ps.join(", ")));
        }
        if !req.job_name.is_empty() {
            let p = push(&mut binds, Bind::Text(req.job_name.clone()));
            cond.push_str(&format!(" AND g.job_name = {p}"));
        }
        if !req.test_name.is_empty() {
            let p = push(&mut binds, Bind::Text(req.test_name.clone()));
            cond.push_str(&format!(" AND g.test_name = {p}"));
        }
        if req.since > 0 {
            let p = push(&mut binds, Bind::Int(req.since));
            cond.push_str(&format!(" AND g.last_seen >= {p}"));
        }
        if req.flaky_only {
            cond.push_str(
                " AND EXISTS (SELECT 1 FROM flaky_tests f WHERE f.ns = g.ns AND f.job_name = g.job_name \
                 AND f.test = g.test_name AND f.flaky = 1)",
            );
        }
        if let Some(c) = cursor {
            let (a, b) = (push(&mut binds, Bind::Int(c[0])), push(&mut binds, Bind::Int(c[1])));
            cond.push_str(&format!(" AND (g.last_seen < {a} OR (g.last_seen = {a} AND g.id < {b}))"));
        }
        let lim = push(&mut binds, Bind::Int(i64::try_from(limit + 1).unwrap_or(i64::MAX)));
        let q = format!(
            "SELECT g.id, g.signature, g.title, g.test_name, g.job_name, g.state, g.note, g.first_seen, g.last_seen, \
             CAST(COALESCE((SELECT f.flaky FROM flaky_tests f WHERE f.ns = g.ns AND f.job_name = g.job_name \
             AND f.test = g.test_name), 0) AS BIGINT) AS flaky \
             FROM failure_groups g WHERE {cond} ORDER BY g.last_seen DESC, g.id DESC LIMIT {lim}"
        );
        let rows = run(self.registry.pool(), q, binds).await.map_err(db)?;
        let mut groups = rows.iter().map(group_of).collect::<Result<Vec<_>, _>>().map_err(db)?;
        let next_page_token = if groups.len() > limit {
            groups.truncate(limit);
            groups
                .last()
                .map(|g| format!("{}:{}", g.last_seen, g.id))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut conn = self.registry.pool().acquire().await.map_err(db)?;
        fill_counts(&mut conn, entry.ns, &mut groups).await.map_err(db)?;
        Ok(wire::ListFailureGroupsResponse {
            groups,
            next_page_token,
        })
    }

    /// `CiInsights.GetFailureGroup`.
    pub async fn get_group(
        &self,
        ctx: &CallCtx,
        req: wire::GetFailureGroupRequest,
    ) -> Result<wire::GetFailureGroupResponse, Status> {
        let id = group_id(req.id)?;
        let cursor = parse_cursor(&req.page_token, 4)?;
        let limit = page_limit(req.limit, DEFAULT_OCCURRENCES, MAX_OCCURRENCES);
        let entry = self.open(ctx).await?;
        let ns = entry.ns;
        let group = {
            let mut conn = self.registry.pool().acquire().await.map_err(db)?;
            load_group(&mut conn, ns, id)
                .await
                .map_err(db)?
                .ok_or_else(|| Status::not_found("unknown failure group"))?
        };
        let mut binds = vec![Bind::Blob(ns_bytes(ns)), Bind::Int(id)];
        let mut cond = String::from("ns = $1 AND group_id = $2");
        if let Some(c) = cursor {
            let p: Vec<String> = c.iter().map(|v| push(&mut binds, Bind::Int(*v))).collect();
            cond.push_str(&format!(
                " AND (at < {0} OR (at = {0} AND (run_id < {1} OR (run_id = {1} AND (job_id < {2} OR (job_id = {2} AND seq < {3}))))))",
                p[0], p[1], p[2], p[3]
            ));
        }
        let lim = push(&mut binds, Bind::Int(i64::try_from(limit + 1).unwrap_or(i64::MAX)));
        let q = format!(
            "SELECT run_id, job_id, seq, change, version, head, attempt, at, message FROM test_cases \
             WHERE {cond} ORDER BY at DESC, run_id DESC, job_id DESC, seq DESC LIMIT {lim}"
        );
        let rows = run(self.registry.pool(), q, binds).await.map_err(db)?;
        let mut keys = Vec::new();
        let mut occurrences = Vec::new();
        for r in &rows {
            let key: Vec<i64> = ["at", "run_id", "job_id", "seq"]
                .iter()
                .map(|c| r.try_get::<i64, _>(*c))
                .collect::<Result<_, _>>()
                .map_err(db)?;
            keys.push(key);
            occurrences.push(occurrence_of(r).map_err(db)?);
        }
        let next_page_token = if occurrences.len() > limit {
            occurrences.truncate(limit);
            keys.truncate(limit);
            keys.last()
                .map(|k| k.iter().map(i64::to_string).collect::<Vec<_>>().join(":"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut conn = self.registry.pool().acquire().await.map_err(db)?;
        mark_occurrences_stale(&mut conn, ns, &mut occurrences).await?;
        let mut one = [group];
        fill_counts(&mut conn, ns, &mut one).await.map_err(db)?;
        let [group] = one;
        Ok(wire::GetFailureGroupResponse {
            group: Some(group),
            occurrences,
            next_page_token,
        })
    }

    /// `CiInsights.SetFailureGroupState`. `maintainer` is whether the caller is one.
    pub async fn set_state(
        &self,
        ctx: &CallCtx,
        maintainer: bool,
        req: wire::SetFailureGroupStateRequest,
    ) -> Result<wire::SetFailureGroupStateResponse, Status> {
        let id = group_id(req.id)?;
        let state = match wire::FailureGroupState::try_from(req.state) {
            Ok(wire::FailureGroupState::Unspecified) | Err(_) => return Err(bad("a state is required")),
            Ok(s) => s as i64,
        };
        if req.note.len() > MAX_NOTE || req.note.chars().any(|c| c.is_control() && c != '\n') {
            return Err(bad("note is at most 1024 bytes of text"));
        }
        let op_id = ctx
            .op_id
            .ok_or_else(|| bad("mutating calls need x-asc-op-id"))?;
        let entry = self.open(ctx).await?;
        if state == QUARANTINED {
            let policy = self.ci_policy(&entry).await?;
            ci::may_quarantine(policy.allow_quarantine, maintainer).map_err(|e| match e {
                ci::QuarantineDenied::PolicyOff => Status::failed_precondition(e.to_string()),
                ci::QuarantineDenied::NotMaintainer => Status::permission_denied(e.to_string()),
            })?;
        }
        retry_conflict(|| self.try_set_state(&entry, ctx, id, state, &req.note, op_id)).await
    }

    async fn try_set_state(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        id: i64,
        state: i64,
        note: &str,
        op_id: Uuid,
    ) -> Result<wire::SetFailureGroupStateResponse, WriteError> {
        let ns = entry.ns;
        let mut tx = self.registry.pool().begin().await?;
        if let Some(row) = sqlx::query("SELECT repo_id, result FROM op_results WHERE op_id = $1")
            .bind(op_id.as_bytes().to_vec())
            .fetch_optional(&mut *tx)
            .await?
        {
            let repo: i64 = row.try_get("repo_id")?;
            let result: Vec<u8> = row.try_get("result")?;
            return match result.strip_prefix(OP_TAG.as_slice()) {
                Some(body) if repo == entry.key.0 => wire::SetFailureGroupStateResponse::decode(body)
                    .map_err(|e| Status::internal(e.to_string()).into()),
                _ => Err(bad("op id was used for another operation").into()),
            };
        }
        self.registry.guard(&mut tx, entry).await?;
        let Some(group) = load_group(&mut tx, ns, id).await? else {
            return Err(Status::not_found("unknown failure group").into());
        };
        let at = now();
        sqlx::query(
            "UPDATE failure_groups SET state = $3, note = $4, state_by = $5, state_at = $6 \
             WHERE ns = $1 AND id = $2",
        )
        .bind(ns_bytes(ns))
        .bind(id)
        .bind(state)
        .bind(note.to_owned())
        .bind(ctx.principal.id.clone())
        .bind(at)
        .execute(&mut *tx)
        .await?;
        let mut updated = load_group(&mut tx, ns, id)
            .await?
            .ok_or_else(|| Status::internal("group vanished"))?;
        let mut one = [updated.clone()];
        fill_counts(&mut tx, ns, &mut one).await?;
        let [filled] = one;
        updated = filled;
        let response = wire::SetFailureGroupStateResponse {
            group: Some(updated),
        };
        let mut stored = OP_TAG.to_vec();
        stored.extend_from_slice(&response.encode_to_vec());
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(stored)
            .bind(at)
            .execute(&mut *tx)
            .await?;
        let event = NewEvent::new("ci.failure_group.state", audit::actor::actor_of(&ctx.principal.id))
            .repo(entry.name.as_str())
            .target(&id.to_string())
            .detail("group", id.to_string())
            .detail("state", state_name(state))
            .detail("test", group.test_name.clone())
            .detail("job", group.job_name.clone())
            .detail("note", note);
        audit::store::append_in_tx(&mut tx, &event)
            .await
            .map_err(|e| crate::review::internal("audit", &e))?;
        tx.commit().await?;
        Ok(response)
    }

    // -------------------------------------------------------------------------- flaky

    /// `CiInsights.FlakyHistory`.
    pub async fn flaky_history(
        &self,
        ctx: &CallCtx,
        req: wire::FlakyHistoryRequest,
    ) -> Result<wire::FlakyHistoryResponse, Status> {
        if req.test_name.is_empty() && req.job_name.is_empty() {
            return Err(bad("test_name or job_name is required"));
        }
        if req.test_name.len() > MAX_NAME || req.job_name.len() > MAX_NAME {
            return Err(bad("job_name and test_name are at most 512 bytes"));
        }
        let days = match req.window_days {
            0 => DEFAULT_WINDOW_DAYS,
            n => n.min(MAX_WINDOW_DAYS),
        };
        let entry = self.open(ctx).await?;
        let policy = self.ci_policy(&entry).await?;
        let since = ci::flaky::day_start(now()) - i64::from(days - 1) * DAY;
        let mut binds = vec![Bind::Blob(ns_bytes(entry.ns)), Bind::Int(since)];
        let mut cond = String::from("ns = $1 AND day >= $2");
        if !req.test_name.is_empty() {
            let p = push(&mut binds, Bind::Text(req.test_name.clone()));
            cond.push_str(&format!(" AND test = {p}"));
        }
        if !req.job_name.is_empty() {
            let p = push(&mut binds, Bind::Text(req.job_name.clone()));
            cond.push_str(&format!(" AND job_name = {p}"));
        }
        let q = format!(
            "SELECT day, CAST(SUM(runs) AS BIGINT) AS runs, CAST(SUM(failures) AS BIGINT) AS failures, \
             CAST(SUM(flips) AS BIGINT) AS flips FROM flaky_daily WHERE {cond} GROUP BY day ORDER BY day"
        );
        let rows = run(self.registry.pool(), q, binds).await.map_err(db)?;
        let mut buckets = Vec::with_capacity(rows.len());
        let (mut runs, mut flips, mut first) = (0i64, 0i64, 0i64);
        for r in &rows {
            let (day, n, failures, f): (i64, i64, i64, i64) = (
                r.try_get("day").map_err(db)?,
                r.try_get("runs").map_err(db)?,
                r.try_get("failures").map_err(db)?,
                r.try_get("flips").map_err(db)?,
            );
            runs += n;
            flips += f;
            if f > 0 && first == 0 {
                first = day;
            }
            buckets.push(wire::FlakyBucket {
                start: day,
                runs: u32::try_from(n).unwrap_or(u32::MAX),
                failures: u32::try_from(failures).unwrap_or(u32::MAX),
                flaky_flips: u32::try_from(f).unwrap_or(u32::MAX),
            });
        }
        let rate = if runs == 0 { 0.0 } else { flips as f64 / runs as f64 };
        Ok(wire::FlakyHistoryResponse {
            buckets,
            flake_rate: rate,
            flaky: flips > 0 && rate >= policy.flaky_threshold,
            first_flaky: first,
        })
    }

    /// `CiInsights.ListFlakyTests`.
    pub async fn list_flaky(
        &self,
        ctx: &CallCtx,
        req: wire::ListFlakyTestsRequest,
    ) -> Result<wire::ListFlakyTestsResponse, Status> {
        let days = match req.window_days {
            0 => DEFAULT_WINDOW_DAYS,
            n => n.min(MAX_WINDOW_DAYS),
        };
        let limit = page_limit(req.limit, DEFAULT_FLAKY, MAX_FLAKY);
        let offset = match req.page_token.as_str() {
            "" => 0usize,
            t if t.len() <= 9 && t.bytes().all(|b| b.is_ascii_digit()) => t.parse().unwrap_or(0),
            _ => return Err(bad("malformed page token")),
        };
        let entry = self.open(ctx).await?;
        let since = now() - i64::from(days) * DAY;
        let rows = sqlx::query(
            "SELECT job_name, test, runs, failures, flips, last_flip FROM flaky_tests \
             WHERE ns = $1 AND flaky = 1 AND last_flip >= $2 LIMIT 10000",
        )
        .bind(ns_bytes(entry.ns))
        .bind(since)
        .fetch_all(self.registry.pool())
        .await
        .map_err(db)?;
        let mut tests = Vec::with_capacity(rows.len());
        for r in &rows {
            let (runs, flips): (i64, i64) = (r.try_get("runs").map_err(db)?, r.try_get("flips").map_err(db)?);
            tests.push(wire::FlakyTest {
                test_name: r.try_get("test").map_err(db)?,
                job_name: r.try_get("job_name").map_err(db)?,
                flake_rate: if runs == 0 { 0.0 } else { flips as f64 / runs as f64 },
                runs: u32::try_from(runs).unwrap_or(u32::MAX),
                last_flip: r.try_get("last_flip").map_err(db)?,
                group_id: 0,
                flips: u32::try_from(flips).unwrap_or(u32::MAX),
            });
        }
        tests.sort_by(|a, b| {
            b.flake_rate
                .total_cmp(&a.flake_rate)
                .then(b.last_flip.cmp(&a.last_flip))
                .then(a.job_name.cmp(&b.job_name))
                .then(a.test_name.cmp(&b.test_name))
        });
        let more = tests.len() > offset + limit;
        let mut page: Vec<wire::FlakyTest> = tests.into_iter().skip(offset).take(limit).collect();
        let mut conn = self.registry.pool().acquire().await.map_err(db)?;
        for t in &mut page {
            let id: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM failure_groups WHERE ns = $1 AND job_name = $2 AND test_name = $3 \
                 ORDER BY last_seen DESC, id DESC LIMIT 1",
            )
            .bind(ns_bytes(entry.ns))
            .bind(t.job_name.clone())
            .bind(t.test_name.clone())
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?;
            t.group_id = u64::try_from(id.unwrap_or(0)).unwrap_or(0);
        }
        Ok(wire::ListFlakyTestsResponse {
            tests: page,
            next_page_token: if more { (offset + limit).to_string() } else { String::new() },
        })
    }
}

// ------------------------------------------------------------------------------ helpers

enum Bind {
    Text(String),
    Int(i64),
    Blob(Vec<u8>),
}

fn push(binds: &mut Vec<Bind>, b: Bind) -> String {
    binds.push(b);
    format!("${}", binds.len())
}

async fn run(pool: &AnyPool, sql: String, binds: Vec<Bind>) -> Result<Vec<AnyRow>, sqlx::Error> {
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in binds {
        q = match b {
            Bind::Text(s) => q.bind(s),
            Bind::Int(i) => q.bind(i),
            Bind::Blob(v) => q.bind(v),
        };
    }
    q.fetch_all(pool).await
}

/// A page token of `n` numbers separated by `:`.
fn parse_cursor(token: &str, n: usize) -> Result<Option<Vec<i64>>, Status> {
    if token.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = token.split(':').collect();
    if token.len() > 100
        || parts.len() != n
        || parts.iter().any(|p| p.is_empty() || p.len() > 19 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad("malformed page token"));
    }
    parts
        .iter()
        .map(|p| p.parse::<i64>().map_err(|_| bad("malformed page token")))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn group_id(n: u64) -> Result<i64, Status> {
    i64::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| bad("a group id is required"))
}

fn state_name(state: i64) -> &'static str {
    match wire::FailureGroupState::try_from(i32::try_from(state).unwrap_or(0)) {
        Ok(wire::FailureGroupState::Open) => "open",
        Ok(wire::FailureGroupState::Acknowledged) => "acknowledged",
        Ok(wire::FailureGroupState::Quarantined) => "quarantined",
        Ok(wire::FailureGroupState::Resolved) => "resolved",
        _ => "unknown",
    }
}

/// The name of a job and the change of its run.
async fn job_row(
    conn: &mut AnyConnection,
    ns: Ns,
    run: i64,
    job: i64,
) -> Result<(String, Vec<u8>), Status> {
    let row = sqlx::query(
        "SELECT j.name AS job_name, r.change AS change FROM check_jobs j \
         JOIN check_runs r ON r.ns = j.ns AND r.id = j.run \
         WHERE j.ns = $1 AND j.id = $2 AND j.run = $3",
    )
    .bind(ns_bytes(ns))
    .bind(job)
    .bind(run)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?
    .ok_or_else(|| Status::not_found("unknown run or job"))?;
    Ok((
        row.try_get("job_name").map_err(db)?,
        row.try_get("change").map_err(db)?,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn upsert_group(
    conn: &mut AnyConnection,
    ns: Ns,
    signature: &str,
    title: &str,
    location: &str,
    test: &str,
    job: &str,
    at: i64,
) -> Result<i64, sqlx::Error> {
    let nsb = ns_bytes(ns);
    let existing = sqlx::query("SELECT id, state FROM failure_groups WHERE ns = $1 AND signature = $2")
        .bind(nsb.clone())
        .bind(signature.to_owned())
        .fetch_optional(&mut *conn)
        .await?;
    if let Some(row) = existing {
        let id: i64 = row.try_get("id")?;
        // A resolved failure that comes back is open again.
        sqlx::query(
            "UPDATE failure_groups SET last_seen = CASE WHEN last_seen < $3 THEN $3 ELSE last_seen END, \
             state = CASE WHEN state = $4 THEN $5 ELSE state END, \
             state_by = CASE WHEN state = $4 THEN '' ELSE state_by END, \
             state_at = CASE WHEN state = $4 THEN $3 ELSE state_at END \
             WHERE ns = $1 AND id = $2",
        )
        .bind(nsb)
        .bind(id)
        .bind(at)
        .bind(RESOLVED)
        .bind(OPEN)
        .execute(&mut *conn)
        .await?;
        return Ok(id);
    }
    let id = next_id(conn, "failure_groups", &nsb).await?;
    sqlx::query(
        "INSERT INTO failure_groups (ns, id, signature, title, location, test_name, job_name, state, \
         first_seen, last_seen) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9)",
    )
    .bind(nsb)
    .bind(id)
    .bind(signature.to_owned())
    .bind(title.to_owned())
    .bind(location.to_owned())
    .bind(test.to_owned())
    .bind(job.to_owned())
    .bind(OPEN)
    .bind(at)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

fn group_of(r: &AnyRow) -> Result<wire::FailureGroup, sqlx::Error> {
    Ok(wire::FailureGroup {
        id: u64::try_from(r.try_get::<i64, _>("id")?).unwrap_or(0),
        signature: r.try_get("signature")?,
        title: r.try_get("title")?,
        test_name: r.try_get("test_name")?,
        job_name: r.try_get("job_name")?,
        state: i32::try_from(r.try_get::<i64, _>("state")?).unwrap_or(0),
        occurrences: 0,
        changes_affected: 0,
        first_seen: r.try_get("first_seen")?,
        last_seen: r.try_get("last_seen")?,
        flaky: r.try_get::<i64, _>("flaky").unwrap_or(0) != 0,
        note: r.try_get("note")?,
        new_vs_main: false,
    })
}

async fn load_group(
    conn: &mut AnyConnection,
    ns: Ns,
    id: i64,
) -> Result<Option<wire::FailureGroup>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT g.id, g.signature, g.title, g.test_name, g.job_name, g.state, g.note, g.first_seen, g.last_seen, \
         CAST(COALESCE((SELECT f.flaky FROM flaky_tests f WHERE f.ns = g.ns AND f.job_name = g.job_name \
         AND f.test = g.test_name), 0) AS BIGINT) AS flaky \
         FROM failure_groups g WHERE g.ns = $1 AND g.id = $2",
    )
    .bind(ns_bytes(ns))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(group_of).transpose()
}

/// Fills the counts of `groups`: occurrences, changes affected and whether any came from the trunk.
async fn fill_counts(
    conn: &mut AnyConnection,
    ns: Ns,
    groups: &mut [wire::FailureGroup],
) -> Result<(), sqlx::Error> {
    if groups.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = groups.iter().map(|g| i64::try_from(g.id).unwrap_or(0)).collect();
    let sql = format!(
        "SELECT group_id, CAST(COUNT(*) AS BIGINT) AS occ, CAST(COUNT(DISTINCT change) AS BIGINT) AS chg, \
         CAST(SUM(CASE WHEN change = $2 THEN 1 ELSE 0 END) AS BIGINT) AS trunk \
         FROM test_cases WHERE ns = $1 AND group_id IN ({}) GROUP BY group_id",
        placeholders(3, ids.len())
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(ns_bytes(ns))
        .bind(Vec::<u8>::new());
    for id in &ids {
        q = q.bind(*id);
    }
    let mut by: HashMap<i64, (i64, i64, i64)> = HashMap::new();
    for r in q.fetch_all(&mut *conn).await? {
        by.insert(
            r.try_get("group_id")?,
            (r.try_get("occ")?, r.try_get("chg")?, r.try_get("trunk")?),
        );
    }
    for g in groups {
        let (occ, chg, trunk) = by.get(&i64::try_from(g.id).unwrap_or(0)).copied().unwrap_or((0, 0, 0));
        g.occurrences = u32::try_from(occ).unwrap_or(u32::MAX);
        // The empty change of a trunk run is one of the distinct values.
        g.changes_affected = u32::try_from(chg - i64::from(trunk > 0)).unwrap_or(0);
        g.new_vs_main = ci::is_new_vs_main([trunk > 0]);
    }
    Ok(())
}

fn occurrence_of(r: &AnyRow) -> Result<wire::FailureOccurrence, sqlx::Error> {
    Ok(wire::FailureOccurrence {
        run_id: u64::try_from(r.try_get::<i64, _>("run_id")?).unwrap_or(0),
        job_id: u64::try_from(r.try_get::<i64, _>("job_id")?).unwrap_or(0),
        change: Bytes::from(r.try_get::<Vec<u8>, _>("change")?),
        version: u32::try_from(r.try_get::<i64, _>("version")?).unwrap_or(0),
        head: Bytes::from(r.try_get::<Vec<u8>, _>("head")?),
        attempt: u32::try_from(r.try_get::<i64, _>("attempt")?).unwrap_or(1),
        at: r.try_get("at")?,
        message: r.try_get("message")?,
        stale: false,
    })
}

/// The newest version of each of `changes`.
pub(crate) async fn latest_versions(
    conn: &mut AnyConnection,
    ns: Ns,
    changes: &[Vec<u8>],
) -> Result<HashMap<Vec<u8>, u32>, sqlx::Error> {
    let mut out = HashMap::new();
    let distinct: BTreeSet<&Vec<u8>> = changes.iter().filter(|c| !c.is_empty()).collect();
    let distinct: Vec<&Vec<u8>> = distinct.into_iter().collect();
    for chunk in distinct.chunks(100) {
        let sql = format!(
            "SELECT change, CAST(MAX(v) AS BIGINT) AS latest FROM change_versions \
             WHERE ns = $1 AND change IN ({}) GROUP BY change",
            placeholders(2, chunk.len())
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns_bytes(ns));
        for c in chunk {
            q = q.bind((*c).clone());
        }
        for r in q.fetch_all(&mut *conn).await? {
            out.insert(
                r.try_get::<Vec<u8>, _>("change")?,
                u32::try_from(r.try_get::<i64, _>("latest")?).unwrap_or(0),
            );
        }
    }
    Ok(out)
}

async fn mark_occurrences_stale(
    conn: &mut AnyConnection,
    ns: Ns,
    occurrences: &mut [wire::FailureOccurrence],
) -> Result<(), Status> {
    let changes: Vec<Vec<u8>> = occurrences.iter().map(|o| o.change.to_vec()).collect();
    let latest = latest_versions(conn, ns, &changes).await.map_err(db)?;
    for o in occurrences {
        o.stale = latest
            .get(o.change.as_ref())
            .is_some_and(|l| ci::is_stale(o.version, *l));
    }
    Ok(())
}

/// Marks the runs whose change has a newer version than the one they ran for (`CiRun.stale`).
pub(crate) async fn mark_stale(
    conn: &mut AnyConnection,
    ns: Ns,
    runs: &mut [wire::CiRun],
) -> Result<(), sqlx::Error> {
    let changes: Vec<Vec<u8>> = runs.iter().map(|r| r.change.to_vec()).collect();
    let latest = latest_versions(conn, ns, &changes).await?;
    for r in runs {
        r.stale = latest
            .get(r.change.as_ref())
            .is_some_and(|l| ci::is_stale(r.version, *l));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ for checks

/// The failure signatures quarantined in a namespace: what a landing check ignores.
pub async fn quarantine(conn: &mut AnyConnection, ns: Ns) -> Result<ci::Quarantine, sqlx::Error> {
    let rows = sqlx::query("SELECT signature FROM failure_groups WHERE ns = $1 AND state = $2")
        .bind(ns_bytes(ns))
        .bind(QUARANTINED)
        .fetch_all(&mut *conn)
        .await?;
    let mut q = ci::Quarantine::new();
    for r in rows {
        q.add(&r.try_get::<String, _>("signature")?);
    }
    Ok(q)
}

/// A failure that blocks a landing: a failed test of a run that no quarantined group covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockingFailure {
    pub job: String,
    pub test: String,
    pub group: i64,
}

/// The failed or errored cases of `run` that still block: those whose group is not quarantined.
/// A run for an older version of its change (stale) is the caller's to ignore; this reports what
/// the run says. A case of the last attempt that passed does not count.
pub async fn blocking_failures(
    conn: &mut AnyConnection,
    ns: Ns,
    run: i64,
) -> Result<Vec<BlockingFailure>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT c.job_name, c.name, c.group_id, g.signature, c.job_id, c.attempt FROM test_cases c \
         JOIN failure_groups g ON g.ns = c.ns AND g.id = c.group_id \
         WHERE c.ns = $1 AND c.run_id = $2 AND c.outcome IN (2, 4) ORDER BY c.job_id, c.seq",
    )
    .bind(ns_bytes(ns))
    .bind(run)
    .fetch_all(&mut *conn)
    .await?;
    let quarantined = quarantine(conn, ns).await?;
    // A test that failed and then passed on a retry of the same run is not a failure of the run.
    let passed = sqlx::query(
        "SELECT job_id, name, attempt FROM test_cases WHERE ns = $1 AND run_id = $2 AND outcome = 1",
    )
    .bind(ns_bytes(ns))
    .bind(run)
    .fetch_all(&mut *conn)
    .await?;
    let mut recovered: HashSet<(i64, String, i64)> = HashSet::new();
    for r in &passed {
        recovered.insert((r.try_get("job_id")?, r.try_get("name")?, r.try_get("attempt")?));
    }
    let mut out = BTreeMap::new();
    for r in rows {
        let (job_id, name, attempt): (i64, String, i64) =
            (r.try_get("job_id")?, r.try_get("name")?, r.try_get("attempt")?);
        let signature: String = r.try_get("signature")?;
        if quarantined.contains(&signature) {
            continue;
        }
        // Passed on a later attempt.
        if recovered.iter().any(|(j, n, a)| *j == job_id && *n == name && *a > attempt) {
            continue;
        }
        out.insert(
            (job_id, name.clone(), r.try_get::<i64, _>("group_id")?),
            BlockingFailure {
                job: r.try_get("job_name")?,
                test: name,
                group: r.try_get("group_id")?,
            },
        );
    }
    Ok(out.into_values().collect())
}
