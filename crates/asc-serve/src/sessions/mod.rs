//! The credentials a person holds on this server, and revoking them (`sessions.proto`,
//! docs/BUILDOUT.md section 27).

pub mod rpc;

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use sqlx::{AnyPool, Row};
use tonic::Status;

use crate::ai::audit::Audit;
use crate::ai::clock;
use crate::ai::ops::Ops;
use crate::review::handle_of;

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "sessions.v1";
/// Most sessions listed.
pub const MAX_LISTED: usize = 500;

/// A kind of credential and the prefix of its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Token,
    Ssh,
    Agent,
}

impl Kind {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Token => "tok",
            Self::Ssh => "ssh",
            Self::Agent => "agt",
        }
    }
}

/// One live credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    pub scope: String,
    pub created: i64,
    pub last_used: i64,
    pub expires: i64,
}

/// Splits `tok:<id>` into its kind and id; anything else is not a session id.
pub fn parse_id(s: &str) -> Option<(Kind, &str)> {
    let (p, id) = s.split_once(':')?;
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if !ok {
        return None;
    }
    let kind = match p {
        "tok" => Kind::Token,
        "ssh" => Kind::Ssh,
        "agt" => Kind::Agent,
        _ => return None,
    };
    Some((kind, id))
}

/// Why a sessions call failed.
#[derive(Debug)]
pub enum SessionError {
    NotFound,
    Invalid(&'static str),
    Sql(String),
}

impl From<sqlx::Error> for SessionError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(crate::ai::store::class(&e))
    }
}

impl SessionError {
    pub fn status(&self) -> Status {
        match self {
            Self::NotFound => Status::not_found("no such session"),
            Self::Invalid(m) => Status::invalid_argument(*m),
            Self::Sql(c) => {
                tracing::error!(class = c.as_str(), "sessions database error");
                Status::internal("internal error")
            }
        }
    }
}

/// The hub of the sessions service.
pub struct SessionsHub {
    pool: AnyPool,
    pub(crate) ops: Ops,
    pub(crate) audit: Arc<dyn Audit>,
}

/// The place the single [`SessionsHub`] is registered.
#[derive(Default)]
pub struct SessionsSlot(OnceLock<Arc<SessionsHub>>);

impl SessionsSlot {
    pub fn set(&self, h: Arc<SessionsHub>) -> bool {
        self.0.set(h).is_ok()
    }
    pub fn get(&self) -> Option<&Arc<SessionsHub>> {
        self.0.get()
    }
}

/// Whether the stored creator of an agent token is `login`.
fn made_by(created_by: &str, login: &str) -> bool {
    handle_of(created_by).is_some_and(|h| h.eq_ignore_ascii_case(login))
}

impl SessionsHub {
    pub fn new(pool: AnyPool, audit: Arc<dyn Audit>) -> Arc<Self> {
        Arc::new(Self {
            ops: Ops::new(pool.clone()),
            pool,
            audit,
        })
    }

    /// The live credentials of `login`, newest first.
    pub async fn list(&self, login: &str) -> Result<Vec<SessionRow>, SessionError> {
        let now = clock::now();
        let mut out = Vec::new();
        for r in sqlx::query(
            "SELECT t.id AS id, t.label AS label, CAST(t.created AS BIGINT) AS created, CAST(t.expires AS BIGINT) AS expires, \
             (SELECT MIN(g.repo) FROM auth_token_grants g WHERE g.token_id = t.id) AS repo, \
             (SELECT MIN(g.access) FROM auth_token_grants g WHERE g.token_id = t.id) AS access \
             FROM auth_tokens t WHERE t.kind = 'pat' AND t.principal = $1 AND t.revoked IS NULL \
             AND (t.expires IS NULL OR t.expires > $2) ORDER BY t.created DESC LIMIT 500",
        )
        .bind(format!("pat:{login}"))
        .bind(now)
        .fetch_all(&self.pool)
        .await?
        {
            let repo: Option<String> = r.try_get("repo")?;
            let access: Option<String> = r.try_get("access")?;
            out.push(SessionRow {
                id: format!("tok:{}", r.try_get::<String, _>("id")?),
                kind: Kind::Token,
                label: r.try_get("label")?,
                scope: match (repo, access) {
                    (Some(r), Some(a)) => format!("{r} {a}"),
                    _ => String::new(),
                },
                created: r.try_get::<Option<i64>, _>("created")?.unwrap_or(0),
                last_used: 0,
                expires: r.try_get::<Option<i64>, _>("expires")?.unwrap_or(0),
            });
        }
        for r in sqlx::query(
            "SELECT id, title, algo, CAST(created AS BIGINT) AS created, CAST(last_used AS BIGINT) AS last_used, \
             CAST(expires AS BIGINT) AS expires FROM ssh_keys WHERE kind = 'user' AND principal = $1 AND revoked IS NULL \
             AND (expires IS NULL OR expires > $2) ORDER BY created DESC LIMIT 500",
        )
        .bind(login.to_owned())
        .bind(now)
        .fetch_all(&self.pool)
        .await?
        {
            out.push(SessionRow {
                id: format!("ssh:{}", r.try_get::<String, _>("id")?),
                kind: Kind::Ssh,
                label: r.try_get("title")?,
                scope: r.try_get("algo")?,
                created: r.try_get::<Option<i64>, _>("created")?.unwrap_or(0),
                last_used: r.try_get::<Option<i64>, _>("last_used")?.unwrap_or(0),
                expires: r.try_get::<Option<i64>, _>("expires")?.unwrap_or(0),
            });
        }
        for r in sqlx::query(
            "SELECT id, name, repo, created_by, created, last_used, expires_at FROM agent_tokens \
             WHERE revoked_at = 0 AND expires_at > $1 AND (created_by = $2 OR created_by = $3 OR created_by LIKE $4) \
             ORDER BY created DESC LIMIT 2000",
        )
        .bind(now)
        .bind(format!("pat:{login}"))
        .bind(login.to_owned())
        .bind(format!("%:{login}"))
        .fetch_all(&self.pool)
        .await?
        {
            if !made_by(&r.try_get::<String, _>("created_by")?, login) {
                continue;
            }
            out.push(SessionRow {
                id: format!("agt:{}", r.try_get::<String, _>("id")?),
                kind: Kind::Agent,
                label: r.try_get("name")?,
                scope: r.try_get("repo")?,
                created: r.try_get("created")?,
                last_used: r.try_get("last_used")?,
                expires: r.try_get("expires_at")?,
            });
        }
        out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        out.truncate(MAX_LISTED);
        Ok(out)
    }

    /// Revokes one credential of `login`; the id of anyone else's is `NotFound`.
    pub async fn revoke(&self, login: &str, id: &str) -> Result<SessionRow, SessionError> {
        let (kind, raw) = parse_id(id).ok_or(SessionError::NotFound)?;
        let row = self
            .list(login)
            .await?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or(SessionError::NotFound)?;
        let now = clock::now();
        let done = match kind {
            Kind::Token => sqlx::query(
                "UPDATE auth_tokens SET revoked = $1 WHERE id = $2 AND kind = 'pat' AND principal = $3 AND revoked IS NULL",
            )
            .bind(now)
            .bind(raw.to_owned())
            .bind(format!("pat:{login}"))
            .execute(&self.pool)
            .await?
            .rows_affected(),
            Kind::Ssh => sqlx::query(
                "UPDATE ssh_keys SET revoked = $1 WHERE id = $2 AND kind = 'user' AND principal = $3 AND revoked IS NULL",
            )
            .bind(now)
            .bind(raw.to_owned())
            .bind(login.to_owned())
            .execute(&self.pool)
            .await?
            .rows_affected(),
            Kind::Agent => sqlx::query(
                "UPDATE agent_tokens SET revoked_at = $1, revoked_reason = 'revoked by its owner' WHERE id = $2 AND revoked_at = 0",
            )
            .bind(now)
            .bind(raw.to_owned())
            .execute(&self.pool)
            .await?
            .rows_affected(),
        };
        if done == 0 {
            return Err(SessionError::NotFound);
        }
        Ok(row)
    }

    /// Revokes every live credential of `login` but `keep`. Returns the revoked rows.
    pub async fn revoke_others(
        &self,
        login: &str,
        keep: &str,
    ) -> Result<Vec<SessionRow>, SessionError> {
        if !keep.is_empty() && parse_id(keep).is_none() {
            return Err(SessionError::Invalid("keep_id is not a session id"));
        }
        let mut gone = Vec::new();
        for s in self.list(login).await? {
            if s.id == keep {
                continue;
            }
            match self.revoke(login, &s.id).await {
                Ok(r) => gone.push(r),
                Err(SessionError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(gone)
    }
}
