//! The `Ci` tonic service and its handlers on the shared [`Reviews`] hub.
//!
//! Choices where the SPEC is silent:
//!
//! * A run is named by `(source, external_id)`; reporting it again updates it. Its change cannot
//!   change once set. A report that would move a finished run (or job) back to a running state is
//!   ignored for that status, so out-of-order reports from a runner cannot reopen a result.
//! * `AppendJobLog` appends one chunk at the current end of the log; two concurrent appends to one
//!   job cannot both take the same offset (primary key), the loser retries.
//! * `JobLog` without `since_offset` answers the tail, cut forward to a line start when it cuts.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use asc_core::change::ChangeId;
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::ci_server::{Ci, CiServer};
use sqlx::any::AnyRow;
use sqlx::{AnyConnection, Row};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::{
    DEFAULT_LOG, DEFAULT_PAGE, MAX_APPEND, MAX_JOBS, MAX_LOG, MAX_LOG_READ, MAX_PAGE, MAX_RUNS,
    is_terminal,
};
use crate::registry::RepoEntry;
use crate::review::{
    Env, Reviews, WriteError, change_of, next_id, now, page_limit, parse_after, placeholders,
    retry_conflict, sql, store,
};

/// Marks an `op_results` row as the result of `AppendJobLog`: the tag, then the log size.
const OP_TAG: &[u8; 3] = b"cl1";

fn id_of(n: u64) -> Result<i64, Status> {
    i64::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| Status::invalid_argument("invalid id"))
}

fn text_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

fn source_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A link for display: empty or `https://...` without whitespace or control characters.
fn url_ok(s: &str) -> bool {
    s.is_empty()
        || (s.len() <= 2048
            && s.starts_with("https://")
            && !s.chars().any(|c| c.is_control() || c.is_whitespace()))
}

fn status_of(n: i32) -> Result<i64, Status> {
    let s = wire::CiStatus::try_from(n).map_err(|_| Status::invalid_argument("invalid status"))?;
    Ok(match s {
        wire::CiStatus::Unspecified => wire::CiStatus::Queued as i64,
        s => s as i64,
    })
}

const RUN_COLS: &str = "id, source, external_id, name, change, version, status, created, started, \
     finished, url";

fn run_of(r: &AnyRow) -> Result<wire::CiRun, sqlx::Error> {
    Ok(wire::CiRun {
        id: u64::try_from(r.try_get::<i64, _>("id")?).unwrap_or(0),
        source: r.try_get("source")?,
        external_id: r.try_get("external_id")?,
        name: r.try_get("name")?,
        change: Bytes::from(r.try_get::<Vec<u8>, _>("change")?),
        version: u32::try_from(r.try_get::<i64, _>("version")?).unwrap_or(0),
        status: i32::try_from(r.try_get::<i64, _>("status")?).unwrap_or(0),
        created: r.try_get("created")?,
        started: r.try_get("started")?,
        finished: r.try_get("finished")?,
        url: r.try_get("url")?,
        jobs: Vec::new(),
    })
}

fn job_of(r: &AnyRow) -> Result<(i64, wire::CiJob), sqlx::Error> {
    Ok((
        r.try_get("run")?,
        wire::CiJob {
            id: u64::try_from(r.try_get::<i64, _>("id")?).unwrap_or(0),
            name: r.try_get("name")?,
            status: i32::try_from(r.try_get::<i64, _>("status")?).unwrap_or(0),
            started: r.try_get("started")?,
            finished: r.try_get("finished")?,
            url: r.try_get("url")?,
            log_size: u64::try_from(r.try_get::<i64, _>("log_size")?).unwrap_or(0),
        },
    ))
}

/// Fills `jobs` of `runs`.
async fn attach_jobs(
    conn: &mut AnyConnection,
    ns: &[u8],
    runs: &mut [wire::CiRun],
) -> Result<(), sqlx::Error> {
    let ids: Vec<i64> = runs
        .iter()
        .map(|r| i64::try_from(r.id).unwrap_or(0))
        .collect();
    let mut by: HashMap<i64, Vec<wire::CiJob>> = HashMap::new();
    for chunk in ids.chunks(100) {
        let q = format!(
            "SELECT id, run, name, status, started, finished, url, log_size FROM check_jobs \
             WHERE ns = $1 AND run IN ({}) ORDER BY id",
            placeholders(2, chunk.len())
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.to_vec());
        for id in chunk {
            query = query.bind(*id);
        }
        for r in query.fetch_all(&mut *conn).await? {
            let (run, job) = job_of(&r)?;
            by.entry(run).or_default().push(job);
        }
    }
    for r in runs {
        r.jobs = by
            .remove(&i64::try_from(r.id).unwrap_or(0))
            .unwrap_or_default();
    }
    Ok(())
}

async fn load_run(
    conn: &mut AnyConnection,
    ns: &[u8],
    id: i64,
) -> Result<Option<wire::CiRun>, sqlx::Error> {
    let q = format!("SELECT {RUN_COLS} FROM check_runs WHERE ns = $1 AND id = $2");
    let Some(row) = sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(ns.to_vec())
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let mut runs = [run_of(&row)?];
    attach_jobs(conn, ns, &mut runs).await?;
    let [run] = runs;
    Ok(Some(run))
}

/// A validated `ReportRun`.
struct Report {
    source: String,
    external_id: String,
    name: String,
    change: Option<ChangeId>,
    version: u32,
    status: i64,
    started: i64,
    finished: i64,
    url: String,
    jobs: Vec<(wire::ReportJob, i64)>,
}

fn report_of(req: wire::ReportRunRequest) -> Result<Report, Status> {
    let bad = |m: &str| Status::invalid_argument(m.to_owned());
    if !source_ok(&req.source) {
        return Err(bad("source is 1 to 64 bytes of [A-Za-z0-9._-]"));
    }
    if !text_ok(&req.external_id, 255) || !text_ok(&req.name, 255) {
        return Err(bad("external_id and name are 1 to 255 bytes of text"));
    }
    if !url_ok(&req.url) {
        return Err(bad("url must be empty or an https link"));
    }
    if req.started < 0 || req.finished < 0 {
        return Err(bad("negative time"));
    }
    if req.jobs.len() > MAX_JOBS {
        return Err(Status::invalid_argument(format!(
            "at most {MAX_JOBS} jobs per run"
        )));
    }
    let mut names = std::collections::HashSet::new();
    let mut jobs = Vec::with_capacity(req.jobs.len());
    for j in req.jobs {
        if !text_ok(&j.name, 255) || !url_ok(&j.url) || j.started < 0 || j.finished < 0 {
            return Err(bad("invalid job"));
        }
        if !names.insert(j.name.clone()) {
            return Err(bad("duplicate job name"));
        }
        let status = status_of(j.status)?;
        jobs.push((j, status));
    }
    let change = if req.change.is_empty() {
        if req.version != 0 {
            return Err(bad("version needs a change"));
        }
        None
    } else {
        Some(change_of(&req.change)?)
    };
    Ok(Report {
        source: req.source,
        external_id: req.external_id,
        name: req.name,
        change,
        version: req.version,
        status: status_of(req.status)?,
        started: req.started,
        finished: req.finished,
        url: req.url,
        jobs,
    })
}

impl Reviews {
    /// `Ci.ListRuns`.
    pub async fn ci_list_runs(
        &self,
        ctx: &CallCtx,
        req: wire::ListRunsRequest,
    ) -> Result<wire::ListRunsResponse, Status> {
        let change = if req.change.is_empty() {
            None
        } else {
            Some(change_of(&req.change)?)
        };
        let status = wire::CiStatus::try_from(req.status)
            .map_err(|_| Status::invalid_argument("invalid status"))? as i64;
        let before = match parse_after(&req.page_token)? {
            0 => i64::MAX,
            n => n,
        };
        let limit = page_limit(req.limit, DEFAULT_PAGE, MAX_PAGE);
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let q = format!(
            "SELECT {RUN_COLS} FROM check_runs WHERE ns = $1 AND id < $2 \
             AND (CAST($3 AS BIGINT) = 0 OR change = $4) \
             AND (CAST($5 AS BIGINT) = 0 OR status = $5) ORDER BY id DESC LIMIT $6"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(ns.clone())
            .bind(before)
            .bind(i64::from(change.is_some()))
            .bind(change.map(|c| c.as_bytes().to_vec()).unwrap_or_default())
            .bind(status)
            .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *conn)
            .await
            .map_err(sql)?;
        let mut runs = rows
            .iter()
            .map(run_of)
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        let next_page_token = if runs.len() > limit {
            runs.truncate(limit);
            runs.last().map(|r| r.id.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        attach_jobs(&mut conn, &ns, &mut runs).await.map_err(sql)?;
        Ok(wire::ListRunsResponse {
            runs,
            next_page_token,
        })
    }

    /// `Ci.JobLog`.
    pub async fn ci_job_log(
        &self,
        ctx: &CallCtx,
        req: wire::JobLogRequest,
    ) -> Result<wire::JobLogResponse, Status> {
        let (run, job) = (id_of(req.run_id)?, id_of(req.job_id)?);
        let max = match req.max_bytes {
            0 => DEFAULT_LOG,
            n => usize::try_from(n).unwrap_or(MAX_LOG_READ).min(MAX_LOG_READ),
        };
        let max = max as u64;
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let size: i64 = sqlx::query_scalar(
            "SELECT log_size FROM check_jobs WHERE ns = $1 AND id = $2 AND run = $3",
        )
        .bind(ns.clone())
        .bind(job)
        .bind(run)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sql)?
        .ok_or_else(|| Status::not_found("unknown job"))?;
        let size = u64::try_from(size).unwrap_or(0);
        // The window to read; a tail starts one byte early to see whether it begins a line.
        let (start, end, tail) = if req.since_offset > 0 {
            let s = req.since_offset.min(size);
            (s, size.min(s.saturating_add(max)), false)
        } else {
            (size.saturating_sub(max), size, true)
        };
        let from = if tail && start > 0 { start - 1 } else { start };
        let mut buf = read_log(&mut conn, &ns, job, from, end)
            .await
            .map_err(sql)?;
        let mut offset = start;
        if tail && start > 0 && !buf.is_empty() {
            // `buf[0]` is the byte before the window. Cut forward to the next line start unless the
            // window already starts one.
            let begins_line = buf.first() == Some(&b'\n');
            buf.remove(0);
            if !begins_line && let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                buf.drain(..=nl);
                offset = start + nl as u64 + 1;
            }
        }
        Ok(wire::JobLogResponse {
            next_offset: offset + buf.len() as u64,
            data: Bytes::from(buf),
            offset,
            size,
            truncated: offset > 0,
        })
    }

    /// `Ci.ReportRun`.
    pub async fn ci_report_run(
        &self,
        ctx: &CallCtx,
        req: wire::ReportRunRequest,
    ) -> Result<wire::ReportRunResponse, Status> {
        let report = report_of(req)?;
        let entry = self.open(ctx).await?;
        let id = retry_conflict(|| self.try_report(&entry, &report)).await?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let run = load_run(&mut conn, entry.ns.as_bytes(), id)
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::internal("internal error"))?;
        Ok(wire::ReportRunResponse { run: Some(run) })
    }

    async fn try_report(&self, entry: &RepoEntry, r: &Report) -> Result<i64, WriteError> {
        let ns = entry.ns;
        let nsb = ns.as_bytes().to_vec();
        let at = now();
        let mut tx = self.registry.pool().begin().await?;
        self.registry.guard(&mut tx, entry).await?;
        let (change_bytes, version) = match r.change {
            None => (Vec::new(), 0),
            Some(c) => {
                if !store::change_known(&mut tx, ns, c).await? {
                    return Err(Status::not_found("unknown change").into());
                }
                let latest = store::latest_version(&mut tx, ns, c).await?;
                let v = match r.version {
                    0 => latest,
                    v if v <= latest => v,
                    _ => return Err(Status::invalid_argument("unknown version").into()),
                };
                (c.as_bytes().to_vec(), i64::from(v))
            }
        };
        let existing = sqlx::query(
            "SELECT id, change, status, started FROM check_runs \
             WHERE ns = $1 AND source = $2 AND external_id = $3",
        )
        .bind(nsb.clone())
        .bind(r.source.clone())
        .bind(r.external_id.clone())
        .fetch_optional(&mut *tx)
        .await?;
        let run_id = if let Some(row) = existing {
            let id: i64 = row.try_get("id")?;
            let old_change: Vec<u8> = row.try_get("change")?;
            let old_status: i64 = row.try_get("status")?;
            let old_started: i64 = row.try_get("started")?;
            if old_change != change_bytes {
                return Err(Status::invalid_argument("a run cannot move to another change").into());
            }
            // A late report never reopens a finished run; its other fields still apply.
            let status = if is_terminal(old_status) && !is_terminal(r.status) {
                old_status
            } else {
                r.status
            };
            let started = if r.started != 0 {
                r.started
            } else {
                old_started
            };
            sqlx::query(
                "UPDATE check_runs SET name = $3, version = $4, status = $5, started = $6, \
                 finished = $7, url = $8 WHERE ns = $1 AND id = $2",
            )
            .bind(nsb.clone())
            .bind(id)
            .bind(r.name.clone())
            .bind(version)
            .bind(status)
            .bind(started)
            .bind(r.finished)
            .bind(r.url.clone())
            .execute(&mut *tx)
            .await?;
            id
        } else {
            let id = next_id(&mut tx, "check_runs", &nsb).await?;
            sqlx::query(
                "INSERT INTO check_runs (ns, id, source, external_id, name, change, version, status, \
                 created, started, finished, url) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(nsb.clone())
            .bind(id)
            .bind(r.source.clone())
            .bind(r.external_id.clone())
            .bind(r.name.clone())
            .bind(change_bytes)
            .bind(version)
            .bind(r.status)
            .bind(at)
            .bind(r.started)
            .bind(r.finished)
            .bind(r.url.clone())
            .execute(&mut *tx)
            .await?;
            prune(&mut tx, &nsb, id - MAX_RUNS).await?;
            id
        };
        for (job, status) in &r.jobs {
            upsert_job(&mut tx, &nsb, run_id, job, *status).await?;
        }
        let jobs: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM check_jobs WHERE ns = $1 AND run = $2",
        )
        .bind(nsb)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
        if usize::try_from(jobs).unwrap_or(usize::MAX) > MAX_JOBS {
            return Err(Status::resource_exhausted("the run has too many jobs").into());
        }
        tx.commit().await?;
        Ok(run_id)
    }

    /// `Ci.AppendJobLog`.
    pub async fn ci_append_log(
        &self,
        ctx: &CallCtx,
        req: wire::AppendJobLogRequest,
    ) -> Result<wire::AppendJobLogResponse, Status> {
        let (run, job) = (id_of(req.run_id)?, id_of(req.job_id)?);
        if req.data.is_empty() || req.data.len() > MAX_APPEND {
            return Err(Status::invalid_argument(format!(
                "an append is 1 to {MAX_APPEND} bytes"
            )));
        }
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        let entry = self.open(ctx).await?;
        let (size, dropped) =
            retry_conflict(|| self.try_append(&entry, run, job, &req.data, op_id)).await?;
        Ok(wire::AppendJobLogResponse {
            size: u64::try_from(size).unwrap_or(0),
            dropped,
        })
    }

    async fn try_append(
        &self,
        entry: &RepoEntry,
        run: i64,
        job: i64,
        data: &[u8],
        op_id: Uuid,
    ) -> Result<(i64, bool), WriteError> {
        let nsb = entry.ns.as_bytes().to_vec();
        // A retry of a call that already committed answers with the first result.
        let stored = sqlx::query("SELECT repo_id, result FROM op_results WHERE op_id = $1")
            .bind(op_id.as_bytes().to_vec())
            .fetch_optional(self.registry.pool())
            .await?;
        if let Some(row) = stored {
            let repo: i64 = row.try_get("repo_id")?;
            let result: Vec<u8> = row.try_get("result")?;
            return match result.strip_prefix(OP_TAG.as_slice()) {
                Some(v) if repo == entry.key.0 && v.len() == 9 => {
                    Ok((i64::from_be_bytes(v[..8].try_into().expect("8")), v[8] == 1))
                }
                _ => Err(Status::invalid_argument("op id was used for another operation").into()),
            };
        }
        let at = now();
        let mut tx = self.registry.pool().begin().await?;
        self.registry.guard(&mut tx, entry).await?;
        let size: i64 = sqlx::query_scalar(
            "SELECT log_size FROM check_jobs WHERE ns = $1 AND id = $2 AND run = $3",
        )
        .bind(nsb.clone())
        .bind(job)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Status::not_found("unknown job"))?;
        let len = i64::try_from(data.len()).unwrap_or(i64::MAX);
        let dropped = size + len > MAX_LOG;
        let size = if dropped {
            size
        } else {
            sqlx::query(
                "INSERT INTO check_job_logs (ns, job, at_offset, data) VALUES ($1, $2, $3, $4)",
            )
            .bind(nsb.clone())
            .bind(job)
            .bind(size)
            .bind(data.to_vec())
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE check_jobs SET log_size = $3 WHERE ns = $1 AND id = $2")
                .bind(nsb)
                .bind(job)
                .bind(size + len)
                .execute(&mut *tx)
                .await?;
            size + len
        };
        let mut result = OP_TAG.to_vec();
        result.extend_from_slice(&size.to_be_bytes());
        result.push(u8::from(dropped));
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(result)
            .bind(at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((size, dropped))
    }
}

/// Deletes runs numbered `upto` or less with their jobs and logs.
async fn prune(conn: &mut AnyConnection, ns: &[u8], upto: i64) -> Result<(), sqlx::Error> {
    if upto <= 0 {
        return Ok(());
    }
    for q in [
        "DELETE FROM check_job_logs WHERE ns = $1 AND job IN \
         (SELECT id FROM check_jobs WHERE ns = $1 AND run <= $2)",
        "DELETE FROM check_jobs WHERE ns = $1 AND run <= $2",
        "DELETE FROM check_runs WHERE ns = $1 AND id <= $2",
    ] {
        sqlx::query(q)
            .bind(ns.to_vec())
            .bind(upto)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

async fn upsert_job(
    conn: &mut AnyConnection,
    ns: &[u8],
    run: i64,
    job: &wire::ReportJob,
    status: i64,
) -> Result<(), sqlx::Error> {
    let existing = sqlx::query(
        "SELECT id, status, started FROM check_jobs WHERE ns = $1 AND run = $2 AND name = $3",
    )
    .bind(ns.to_vec())
    .bind(run)
    .bind(job.name.clone())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = existing {
        let id: i64 = row.try_get("id")?;
        let old: i64 = row.try_get("status")?;
        let old_started: i64 = row.try_get("started")?;
        let status = if is_terminal(old) && !is_terminal(status) {
            old
        } else {
            status
        };
        let started = if job.started != 0 {
            job.started
        } else {
            old_started
        };
        sqlx::query(
            "UPDATE check_jobs SET status = $3, started = $4, finished = $5, url = $6 \
             WHERE ns = $1 AND id = $2",
        )
        .bind(ns.to_vec())
        .bind(id)
        .bind(status)
        .bind(started)
        .bind(job.finished)
        .bind(job.url.clone())
        .execute(&mut *conn)
        .await?;
    } else {
        let id = next_id(conn, "check_jobs", ns).await?;
        sqlx::query(
            "INSERT INTO check_jobs (ns, id, run, name, status, started, finished, url) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(ns.to_vec())
        .bind(id)
        .bind(run)
        .bind(job.name.clone())
        .bind(status)
        .bind(job.started)
        .bind(job.finished)
        .bind(job.url.clone())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The log bytes in `[from, end)`.
async fn read_log(
    conn: &mut AnyConnection,
    ns: &[u8],
    job: i64,
    from: u64,
    end: u64,
) -> Result<Vec<u8>, sqlx::Error> {
    if end <= from {
        return Ok(Vec::new());
    }
    // A chunk starts at most MAX_APPEND bytes before the first byte it contributes.
    let lo = i64::try_from(from.saturating_sub(MAX_APPEND as u64)).unwrap_or(0);
    let hi = i64::try_from(end).unwrap_or(i64::MAX);
    let rows = sqlx::query(
        "SELECT at_offset, data FROM check_job_logs \
         WHERE ns = $1 AND job = $2 AND at_offset >= $3 AND at_offset < $4 ORDER BY at_offset",
    )
    .bind(ns.to_vec())
    .bind(job)
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(usize::try_from(end - from).unwrap_or(0));
    for r in rows {
        let at = u64::try_from(r.try_get::<i64, _>("at_offset")?).unwrap_or(0);
        let data: Vec<u8> = r.try_get("data")?;
        let chunk_end = at + data.len() as u64;
        if chunk_end <= from || at >= end {
            continue;
        }
        let a = from.saturating_sub(at) as usize;
        let b = (end.min(chunk_end) - at) as usize;
        out.extend_from_slice(&data[a..b]);
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Ci` tonic service.
pub struct CiService {
    hub: Arc<Reviews>,
    env: Env,
}

impl CiService {
    pub fn new(hub: Arc<Reviews>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<CiServer<Self>, AuthInterceptor> {
        let server = CiServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }
}

#[tonic::async_trait]
impl Ci for CiService {
    async fn list_runs(
        &self,
        request: Request<wire::ListRunsRequest>,
    ) -> Result<Response<wire::ListRunsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.ci_list_runs(&ctx, msg).await.map(Response::new)
    }

    async fn job_log(
        &self,
        request: Request<wire::JobLogRequest>,
    ) -> Result<Response<wire::JobLogResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.ci_job_log(&ctx, msg).await.map(Response::new)
    }

    async fn report_run(
        &self,
        request: Request<wire::ReportRunRequest>,
    ) -> Result<Response<wire::ReportRunResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        self.hub.ci_report_run(&ctx, msg).await.map(Response::new)
    }

    async fn append_job_log(
        &self,
        request: Request<wire::AppendJobLogRequest>,
    ) -> Result<Response<wire::AppendJobLogResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, true)
            .await?;
        self.hub.ci_append_log(&ctx, msg).await.map(Response::new)
    }
}
