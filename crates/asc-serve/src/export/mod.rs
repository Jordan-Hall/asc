//! Export of a person's own data (`dataexport.proto`, docs/BUILDOUT.md section 30).
//!
//! A job is a row in `export_jobs` and a file `<dir>/<id>.jsonl` (mode 0600). Every table is read
//! with the person as a bound parameter, and rows whose owner column does not name exactly that
//! person are dropped again in Rust, so a `LIKE` pattern can never export a neighbour's row.
//! Secrets are not selected: the column lists below are the whole of what can leave.

pub mod rpc;

#[cfg(test)]
mod tests;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sqlx::{AnyPool, Row};
use tokio::task::JoinHandle;
use tonic::Status;

use crate::ai::audit::Audit;
use crate::ai::clock;
use crate::ai::ops::Ops;
use crate::ai::rand_id;
use crate::review::handle_of;

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "dataexport.v1";
/// How long a ready archive is kept.
pub const TTL_SECS: i64 = 7 * 86_400;
/// One export per person in this long.
pub const MIN_GAP_SECS: i64 = 86_400;
/// Largest archive; a table is cut when it would pass this.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Most rows of one table.
pub const MAX_ROWS: i64 = 100_000;
/// Bytes per download chunk.
pub const CHUNK: usize = 1024 * 1024;

pub const PENDING: i64 = 1;
pub const RUNNING: i64 = 2;
pub const READY: i64 = 3;
pub const FAILED: i64 = 4;
pub const EXPIRED: i64 = 5;

#[derive(Clone, Copy)]
enum Col {
    T,
    I,
    N,
}

struct Spec {
    table: &'static str,
    sql: &'static str,
    cols: &'static [(&'static str, Col)],
    /// A column that must name the person exactly (checked in Rust).
    owner: Option<&'static str>,
}

use Col::{I, N, T};

/// Everything a person's export holds. `$1` is the login, `$2` is `%:login`, `$3` is `pat:login`.
const SPECS: &[Spec] = &[
    Spec {
        table: "ai_keys",
        sql: "SELECT id, label, provider, base_url, default_model, key_hint, monthly_cap, created, updated FROM ai_keys \
              WHERE owner_kind = 1 AND owner_name = $1 ORDER BY created, id",
        cols: &[
            ("id", T),
            ("label", T),
            ("provider", I),
            ("base_url", T),
            ("default_model", T),
            ("key_hint", T),
            ("monthly_cap", I),
            ("created", I),
            ("updated", I),
        ],
        owner: None,
    },
    Spec {
        table: "ai_usage",
        sql: "SELECT key_id, day, repo, agent, model, requests, input_tokens, output_tokens, cost_micro_usd FROM ai_usage \
              WHERE owner_kind = 1 AND owner_name = $1 ORDER BY day, key_id",
        cols: &[
            ("key_id", T),
            ("day", T),
            ("repo", T),
            ("agent", T),
            ("model", T),
            ("requests", I),
            ("input_tokens", I),
            ("output_tokens", I),
            ("cost_micro_usd", I),
        ],
        owner: None,
    },
    Spec {
        table: "agent_runs",
        sql: "SELECT id, repo, agent, title, task, state, created, started, finished FROM agent_runs \
              WHERE LOWER(starter) = $1 OR (starter = '' AND LOWER(created_by) = $1) ORDER BY id",
        cols: &[
            ("id", I),
            ("repo", T),
            ("agent", T),
            ("title", T),
            ("task", T),
            ("state", I),
            ("created", I),
            ("started", I),
            ("finished", I),
        ],
        owner: None,
    },
    Spec {
        table: "agent_tokens",
        sql: "SELECT id, repo, name, agent, created_by, created, expires_at, revoked_at FROM agent_tokens \
              WHERE created_by = $3 OR created_by = $1 OR created_by LIKE $2 ORDER BY created, id",
        cols: &[
            ("id", T),
            ("repo", T),
            ("name", T),
            ("agent", T),
            ("created_by", T),
            ("created", I),
            ("expires_at", I),
            ("revoked_at", I),
        ],
        owner: Some("created_by"),
    },
    Spec {
        table: "access_tokens",
        sql: "SELECT id, label, CAST(created AS BIGINT) AS created, CAST(expires AS BIGINT) AS expires, \
              CAST(revoked AS BIGINT) AS revoked FROM auth_tokens WHERE kind = 'pat' AND principal = $3 ORDER BY created, id",
        cols: &[
            ("id", T),
            ("label", T),
            ("created", I),
            ("expires", N),
            ("revoked", N),
        ],
        owner: None,
    },
    Spec {
        table: "ssh_keys",
        sql: "SELECT id, algo, title, CAST(created AS BIGINT) AS created, CAST(last_used AS BIGINT) AS last_used, \
              CAST(expires AS BIGINT) AS expires, CAST(revoked AS BIGINT) AS revoked FROM ssh_keys \
              WHERE kind = 'user' AND principal = $1 ORDER BY created, id",
        cols: &[
            ("id", T),
            ("algo", T),
            ("title", T),
            ("created", I),
            ("last_used", N),
            ("expires", N),
            ("revoked", N),
        ],
        owner: None,
    },
    Spec {
        table: "review_comments",
        sql: "SELECT id, thread, author, body, created FROM review_comments \
              WHERE author = $1 OR author LIKE $2 ORDER BY created, id",
        cols: &[
            ("id", I),
            ("thread", I),
            ("author", T),
            ("body", T),
            ("created", I),
        ],
        owner: Some("author"),
    },
    Spec {
        table: "audit_events",
        sql: "SELECT seq, time_ms, actor_name, action, repo, target, outcome FROM audit_events \
              WHERE actor_kind = 'user' AND (actor_name = $1 OR actor_name LIKE $2) ORDER BY time_ms, seq",
        cols: &[
            ("seq", I),
            ("time_ms", I),
            ("actor_name", T),
            ("action", T),
            ("repo", T),
            ("target", T),
            ("outcome", I),
        ],
        owner: Some("actor_name"),
    },
];

/// One job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub login: String,
    pub state: i64,
    pub requested: i64,
    pub finished: i64,
    pub expires: i64,
    pub size: i64,
    pub rows: i64,
    pub truncated: bool,
    pub error: String,
}

/// Why an export call failed.
#[derive(Debug)]
pub enum ExportError {
    NotFound,
    TooSoon(i64),
    Busy,
    NotReady,
    Sql(String),
}

impl From<sqlx::Error> for ExportError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(crate::ai::store::class(&e))
    }
}

impl ExportError {
    pub fn status(&self) -> Status {
        match self {
            Self::NotFound => Status::not_found("no such export"),
            Self::TooSoon(secs) => Status::resource_exhausted(format!(
                "one export per 24 hours; the next can be requested in {} hours",
                secs.div_euclid(3600) + 1
            )),
            Self::Busy => Status::failed_precondition("an export is already running"),
            Self::NotReady => Status::failed_precondition("the export is not ready or has expired"),
            Self::Sql(c) => {
                tracing::error!(class = c.as_str(), "export database error");
                Status::internal("internal error")
            }
        }
    }
}

/// The hub of the export service.
pub struct ExportHub {
    pool: AnyPool,
    dir: PathBuf,
    pub(crate) ops: Ops,
    pub(crate) audit: Arc<dyn Audit>,
}

/// The place the single [`ExportHub`] is registered.
#[derive(Default)]
pub struct ExportSlot(OnceLock<Arc<ExportHub>>);

impl ExportSlot {
    pub fn set(&self, h: Arc<ExportHub>) -> bool {
        self.0.set(h).is_ok()
    }
    pub fn get(&self) -> Option<&Arc<ExportHub>> {
        self.0.get()
    }
}

fn job_of(r: &sqlx::any::AnyRow) -> Result<Job, sqlx::Error> {
    Ok(Job {
        id: r.try_get("id")?,
        login: r.try_get("login")?,
        state: r.try_get("state")?,
        requested: r.try_get("requested")?,
        finished: r.try_get("finished")?,
        expires: r.try_get("expires")?,
        size: r.try_get("size")?,
        rows: r.try_get("rows")?,
        truncated: r.try_get::<i64, _>("truncated")? != 0,
        error: r.try_get("error")?,
    })
}

const JOB_COLS: &str =
    "id, login, state, requested, finished, expires, size, rows, truncated, error";

impl ExportHub {
    pub fn new(pool: AnyPool, dir: PathBuf, audit: Arc<dyn Audit>) -> Arc<Self> {
        Arc::new(Self {
            ops: Ops::new(pool.clone()),
            pool,
            dir,
            audit,
        })
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    pub async fn get(&self, login: &str, id: &str) -> Result<Job, ExportError> {
        if !id.starts_with("ex_") || id.len() > 40 {
            return Err(ExportError::NotFound);
        }
        let q = format!("SELECT {JOB_COLS} FROM export_jobs WHERE id = $1 AND login = $2");
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(id.to_owned())
            .bind(login.to_owned())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(job_of)
            .transpose()?
            .ok_or(ExportError::NotFound)
    }

    pub async fn list(&self, login: &str) -> Result<Vec<Job>, ExportError> {
        let q = format!(
            "SELECT {JOB_COLS} FROM export_jobs WHERE login = $1 ORDER BY requested DESC, id DESC LIMIT 20"
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(login.to_owned())
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(job_of)
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Starts an export for `login`: one at a time, one per 24 hours (a failed one does not count).
    pub async fn request(self: &Arc<Self>, login: &str) -> Result<Job, ExportError> {
        let now = clock::now();
        let jobs = self.list(login).await?;
        if jobs
            .iter()
            .any(|j| j.state == PENDING || j.state == RUNNING)
        {
            return Err(ExportError::Busy);
        }
        if let Some(j) = jobs
            .iter()
            .filter(|j| j.state != FAILED)
            .find(|j| now - j.requested < MIN_GAP_SECS)
        {
            return Err(ExportError::TooSoon(MIN_GAP_SECS - (now - j.requested)));
        }
        let job = Job {
            id: rand_id("ex_", 20),
            login: login.to_owned(),
            state: PENDING,
            requested: now,
            ..Job::default()
        };
        sqlx::query(
            "INSERT INTO export_jobs (id, login, state, requested) VALUES ($1, $2, $3, $4)",
        )
        .bind(job.id.clone())
        .bind(job.login.clone())
        .bind(PENDING)
        .bind(now)
        .execute(&self.pool)
        .await?;
        let (hub, j) = (self.clone(), job.clone());
        tokio::spawn(async move { hub.run(j).await });
        Ok(job)
    }

    async fn run(self: Arc<Self>, job: Job) {
        let _ = sqlx::query("UPDATE export_jobs SET state = $2 WHERE id = $1")
            .bind(job.id.clone())
            .bind(RUNNING)
            .execute(&self.pool)
            .await;
        let result = self.build(&job).await;
        let now = clock::now();
        let r = match result {
            Ok((rows, size, truncated)) => sqlx::query(
                "UPDATE export_jobs SET state = $2, finished = $3, expires = $4, size = $5, rows = $6, truncated = $7 WHERE id = $1",
            )
            .bind(job.id.clone())
            .bind(READY)
            .bind(now)
            .bind(now + TTL_SECS)
            .bind(i64::try_from(size).unwrap_or(i64::MAX))
            .bind(rows)
            .bind(i64::from(truncated))
            .execute(&self.pool)
            .await,
            Err(e) => {
                let _ = std::fs::remove_file(self.path(&job.id));
                let note: String = e.lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).take(200).collect();
                sqlx::query("UPDATE export_jobs SET state = $2, finished = $3, error = $4 WHERE id = $1")
                    .bind(job.id.clone())
                    .bind(FAILED)
                    .bind(now)
                    .bind(note)
                    .execute(&self.pool)
                    .await
            }
        };
        if let Err(e) = r {
            tracing::error!(class = %crate::ai::store::class(&e), "export record");
        }
        self.audit
            .record(crate::ai::audit::Entry {
                owner: &job.login,
                target: &job.id,
                details: &[("result", "finished".to_owned())],
                ..crate::ai::audit::Entry::new("export.finished", "system")
            })
            .await;
    }

    /// Writes the archive. `(rows, bytes, truncated)`.
    async fn build(&self, job: &Job) -> Result<(i64, u64, bool), String> {
        crate::workdir::create_private_all(&self.dir).map_err(|e| e.to_string())?;
        let mut file = crate::backups::crypto::create_private_file(&self.path(&job.id))
            .map_err(|e| e.to_string())?;
        let login = job.login.as_str();
        let (mut rows, mut bytes, mut truncated) = (0i64, 0u64, false);
        let meta = serde_json::to_vec(&json!({"table": "export", "row": {"login": login, "created": job.requested, "format": 1}}))
            .map_err(|e| e.to_string())?;
        file.write_all(&meta)
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|e| e.to_string())?;
        bytes += meta.len() as u64 + 1;
        for spec in SPECS {
            let q = format!("{} LIMIT {}", spec.sql, MAX_ROWS + 1);
            let got = sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(login.to_owned())
                .bind(format!("%:{login}"))
                .bind(format!("pat:{login}"))
                .fetch_all(&self.pool)
                .await
                .map_err(|e| crate::ai::store::class(&e))?;
            if i64::try_from(got.len()).unwrap_or(i64::MAX) > MAX_ROWS {
                truncated = true;
            }
            for r in got
                .iter()
                .take(usize::try_from(MAX_ROWS).unwrap_or(usize::MAX))
            {
                if let Some(c) = spec.owner {
                    let v: String = r.try_get(c).map_err(|e| e.to_string())?;
                    let mine = handle_of(&v).is_some_and(|h| h.eq_ignore_ascii_case(login))
                        || v.eq_ignore_ascii_case(login);
                    if !mine {
                        continue;
                    }
                }
                let mut obj = Map::new();
                for (name, kind) in spec.cols {
                    let v = match kind {
                        T => {
                            Value::String(r.try_get::<String, _>(*name).map_err(|e| e.to_string())?)
                        }
                        I => json!(r.try_get::<i64, _>(*name).map_err(|e| e.to_string())?),
                        N => r
                            .try_get::<Option<i64>, _>(*name)
                            .map_err(|e| e.to_string())?
                            .map_or(Value::Null, |n| json!(n)),
                    };
                    obj.insert((*name).to_owned(), v);
                }
                let line = serde_json::to_vec(&json!({"table": spec.table, "row": obj}))
                    .map_err(|e| e.to_string())?;
                if bytes + line.len() as u64 + 1 > MAX_BYTES {
                    truncated = true;
                    break;
                }
                file.write_all(&line)
                    .and_then(|()| file.write_all(b"\n"))
                    .map_err(|e| e.to_string())?;
                bytes += line.len() as u64 + 1;
                rows += 1;
            }
        }
        file.sync_all().map_err(|e| e.to_string())?;
        Ok((rows, bytes, truncated))
    }

    /// The path of a ready archive of `login`.
    pub async fn ready(&self, login: &str, id: &str) -> Result<(Job, PathBuf), ExportError> {
        let j = self.get(login, id).await?;
        if j.state != READY || j.expires <= clock::now() {
            return Err(ExportError::NotReady);
        }
        Ok((j, self.path(id)))
    }

    /// Deletes the archive and marks the job expired.
    pub async fn delete(&self, login: &str, id: &str) -> Result<(), ExportError> {
        let j = self.get(login, id).await?;
        if j.state == PENDING || j.state == RUNNING {
            return Err(ExportError::Busy);
        }
        let _ = std::fs::remove_file(self.path(id));
        sqlx::query("DELETE FROM export_jobs WHERE id = $1 AND login = $2")
            .bind(id.to_owned())
            .bind(login.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Deletes the files of expired archives and closes jobs a dead process left running.
    pub async fn sweep(&self) -> u64 {
        let now = clock::now();
        let Ok(rows) = sqlx::query("SELECT id FROM export_jobs WHERE state = $1 AND expires <= $2")
            .bind(READY)
            .bind(now)
            .fetch_all(&self.pool)
            .await
        else {
            return 0;
        };
        let mut n = 0;
        for r in rows {
            let Ok(id) = r.try_get::<String, _>("id") else {
                continue;
            };
            let _ = std::fs::remove_file(self.path(&id));
            let _ = sqlx::query("UPDATE export_jobs SET state = $2, size = 0 WHERE id = $1")
                .bind(id)
                .bind(EXPIRED)
                .execute(&self.pool)
                .await;
            n += 1;
        }
        let _ = sqlx::query("UPDATE export_jobs SET state = $1, finished = $2, error = 'interrupted by a restart' WHERE state IN ($3, $4)")
            .bind(FAILED)
            .bind(now)
            .bind(PENDING)
            .bind(RUNNING)
            .execute(&self.pool)
            .await;
        // Records of expired jobs go after a month; failed ones too.
        let _ = sqlx::query("DELETE FROM export_jobs WHERE state IN ($1, $2) AND requested < $3")
            .bind(EXPIRED)
            .bind(FAILED)
            .bind(now - 30 * 86_400)
            .execute(&self.pool)
            .await;
        n
    }

    pub fn spawn_sweeper(self: &Arc<Self>) -> JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                hub.sweep().await;
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        })
    }
}

/// Reads up to [`CHUNK`] bytes of `path` from `offset`.
pub fn read_chunk(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; CHUNK];
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    buf.truncate(n);
    Ok(buf)
}
