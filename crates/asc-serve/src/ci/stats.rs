//! Check statistics for the Checks board and the comparison with main for a change (`ci.proto`,
//! "statistics").
//!
//! * `Ci.Stats`: over a window of days, how many runs finished, how many passed, the pass rate against
//!   the window before it, the failure groups still open, the tests flaky now, the median duration of a
//!   finished run and the slowest job (by its median over the window).
//! * `Ci.CompareWithMain`: for the newest finished run of a change, which failing tests are *new*
//!   (not failing in the newest finished run on the default branch), what main looked like, and the
//!   slowest test now against the same test on main. Runs on the default branch are the runs whose
//!   change is empty (a run that belongs to no change, as the reporter sends for a branch run).

use asc_rpc::server::{CallCtx, wire};
use sqlx::Row;
use tonic::Status;

use crate::review::{Reviews, change_of, sql};

const DAY: i64 = 86_400;
/// Most runs one statistics call reads.
const MAX_RUNS: i64 = 20_000;
const SUCCESS: i64 = 3;
const FAILURE: i64 = 4;

fn median(v: &mut [i64]) -> i64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn permille(num: u32, den: u32) -> u32 {
    if den == 0 { 0 } else { u32::try_from(u64::from(num) * 1000 / u64::from(den)).unwrap_or(1000) }
}

impl Reviews {
    /// `Ci.Stats`.
    pub async fn ci_stats(&self, ctx: &CallCtx, req: wire::CiStatsRequest) -> Result<wire::CiStatsResponse, Status> {
        let days = match req.days {
            0 => 14,
            n => i64::from(n.min(90)),
        };
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let now = crate::review::now();
        let since = now - days * DAY;
        let before = since - days * DAY;
        let runs = sqlx::query(
            "SELECT id, status, created, started, finished FROM check_runs WHERE ns = $1 AND created >= $2 AND status IN (3, 4) \
             ORDER BY created DESC LIMIT $3",
        )
        .bind(ns.clone())
        .bind(before)
        .bind(MAX_RUNS)
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let (mut cur_total, mut cur_pass, mut prev_total, mut prev_pass) = (0u32, 0u32, 0u32, 0u32);
        let mut durations: Vec<i64> = Vec::new();
        let mut run_ids: Vec<i64> = Vec::new();
        for r in &runs {
            let (status, created, started, finished): (i64, i64, i64, i64) = (
                r.try_get("status").map_err(sql)?,
                r.try_get("created").map_err(sql)?,
                r.try_get("started").map_err(sql)?,
                r.try_get("finished").map_err(sql)?,
            );
            if created >= since {
                cur_total += 1;
                cur_pass += u32::from(status == SUCCESS);
                if started > 0 && finished >= started {
                    durations.push(finished - started);
                }
                run_ids.push(r.try_get("id").map_err(sql)?);
            } else {
                prev_total += 1;
                prev_pass += u32::from(status == SUCCESS);
            }
        }
        // The slowest job by its median duration over the window.
        let mut by_job: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
        for chunk in run_ids.chunks(200) {
            let q = format!(
                "SELECT name, started, finished FROM check_jobs WHERE ns = $1 AND finished >= started AND started > 0 AND run IN ({})",
                crate::review::placeholders(2, chunk.len())
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
            for id in chunk {
                query = query.bind(*id);
            }
            for r in query.fetch_all(self.registry.pool()).await.map_err(sql)? {
                let (s, f): (i64, i64) = (r.try_get("started").map_err(sql)?, r.try_get("finished").map_err(sql)?);
                by_job.entry(r.try_get("name").map_err(sql)?).or_default().push(f - s);
            }
        }
        let slowest = by_job
            .iter_mut()
            .map(|(n, v)| (n.clone(), median(v)))
            .max_by_key(|(_, m)| *m)
            .unwrap_or_default();
        let open: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM failure_groups WHERE ns = $1 AND state = 1")
            .bind(ns.clone())
            .fetch_one(self.registry.pool())
            .await
            .map_err(sql)?;
        let flaky: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM flaky_tests WHERE ns = $1 AND flaky = 1")
            .bind(ns)
            .fetch_one(self.registry.pool())
            .await
            .map_err(sql)?;
        Ok(wire::CiStatsResponse {
            days: u32::try_from(days).unwrap_or(0),
            runs: cur_total,
            passed: cur_pass,
            failed: cur_total - cur_pass,
            pass_permille: permille(cur_pass, cur_total),
            previous_runs: prev_total,
            previous_pass_permille: permille(prev_pass, prev_total),
            failure_groups_open: u32::try_from(open).unwrap_or(u32::MAX),
            flaky_tests: u32::try_from(flaky).unwrap_or(u32::MAX),
            median_duration_seconds: u32::try_from(median(&mut durations)).unwrap_or(u32::MAX),
            slowest_job: slowest.0,
            slowest_job_seconds: u32::try_from(slowest.1).unwrap_or(u32::MAX),
        })
    }

    /// `Ci.CompareWithMain`.
    pub async fn ci_compare_with_main(
        &self,
        ctx: &CallCtx,
        req: wire::CompareWithMainRequest,
    ) -> Result<wire::CompareWithMainResponse, Status> {
        let change = change_of(&req.change)?;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        {
            let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
            if !crate::review::store::change_known(&mut conn, entry.ns, change).await? {
                return Err(Status::not_found("unknown change"));
            }
        }
        let newest = |change: Vec<u8>| {
            let (pool, ns) = (self.registry.pool().clone(), ns.clone());
            async move {
                sqlx::query("SELECT id, status FROM check_runs WHERE ns = $1 AND change = $2 AND status IN (3, 4) ORDER BY id DESC LIMIT 1")
                    .bind(ns)
                    .bind(change)
                    .fetch_optional(&pool)
                    .await
            }
        };
        let mine = newest(change.as_bytes().to_vec()).await.map_err(sql)?;
        let main = newest(Vec::new()).await.map_err(sql)?;
        let mut out = wire::CompareWithMainResponse::default();
        let id_of = |r: &Option<sqlx::any::AnyRow>| -> Result<Option<i64>, Status> {
            r.as_ref().map(|r| r.try_get::<i64, _>("id").map_err(sql)).transpose()
        };
        out.main_status = match &main {
            Some(r) if r.try_get::<i64, _>("status").map_err(sql)? == SUCCESS => wire::CiStatus::Success as i32,
            Some(_) => wire::CiStatus::Failure as i32,
            None => wire::CiStatus::Unspecified as i32,
        };
        let (Some(my_id), main_id) = (id_of(&mine)?, id_of(&main)?) else {
            return Ok(out);
        };
        let load = |run: i64| {
            let (pool, ns) = (self.registry.pool().clone(), ns.clone());
            async move {
                sqlx::query("SELECT job_name, name, outcome, duration_ms FROM test_cases WHERE ns = $1 AND run_id = $2")
                    .bind(ns)
                    .bind(run)
                    .fetch_all(&pool)
                    .await
            }
        };
        let theirs = load(my_id).await.map_err(sql)?;
        let ours = match main_id {
            Some(m) => load(m).await.map_err(sql)?,
            None => Vec::new(),
        };
        let key = |r: &sqlx::any::AnyRow| -> Result<(String, String), Status> {
            Ok((r.try_get("job_name").map_err(sql)?, r.try_get("name").map_err(sql)?))
        };
        let failing = |r: &sqlx::any::AnyRow| -> bool { matches!(r.try_get::<i64, _>("outcome"), Ok(2 | 4)) };
        let main_failing: std::collections::HashSet<(String, String)> =
            ours.iter().filter(|r| failing(r)).map(|r| key(r)).collect::<Result<_, _>>()?;
        let main_ms: std::collections::HashMap<(String, String), i64> = ours
            .iter()
            .map(|r| Ok((key(r)?, r.try_get::<i64, _>("duration_ms").map_err(sql)?)))
            .collect::<Result<_, Status>>()?;
        let mut slowest: Option<(i64, (String, String))> = None;
        for r in &theirs {
            let k = key(r)?;
            let ms: i64 = r.try_get("duration_ms").map_err(sql)?;
            if failing(r) {
                out.failing += 1;
                if !main_failing.contains(&k) && out.new_failures.len() < 100 {
                    out.new_failures.push(wire::TestRef { job: k.0.clone(), name: k.1.clone() });
                }
            }
            if slowest.as_ref().is_none_or(|(m, _)| ms > *m) {
                slowest = Some((ms, k));
            }
        }
        out.new_failure_count = u32::try_from(
            theirs.iter().filter(|r| failing(r)).filter(|r| key(r).is_ok_and(|k| !main_failing.contains(&k))).count(),
        )
        .unwrap_or(u32::MAX);
        if let Some((ms, k)) = slowest {
            out.slowest_test = Some(wire::TestTiming {
                job: k.0.clone(),
                name: k.1.clone(),
                duration_ms: u64::try_from(ms).unwrap_or(0),
                main_duration_ms: main_ms.get(&k).map_or(0, |m| u64::try_from(*m).unwrap_or(0)),
            });
        }
        out.compared = true;
        let _ = FAILURE;
        Ok(out)
    }
}
