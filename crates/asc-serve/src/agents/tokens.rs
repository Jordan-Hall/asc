//! Agent tokens: the secret, the row, the budget counters.
//!
//! A token is `ascat_` and 43 base64url characters (32 random bytes). Only the SHA-256 of the whole
//! string is stored. The row says which repository the token is for, which agent name it acts as,
//! which paths it may write, when it expires and what it may spend. The counters change only by
//! conditional `UPDATE`s, so any number of concurrent callers can never pass a limit; a spend
//! that cannot be refused beforehand (tokens a provider already counted) is recorded in full and
//! exhausts the token.

use std::fmt;

use asc_rpc::server::wire;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};
use zeroize::Zeroizing;

use crate::ai::clock;
use crate::ai::store::class;

/// The prefix that marks a string as an agent token.
pub const PREFIX: &str = "ascat_";
/// The exact length of a token.
pub const TOKEN_LEN: usize = 6 + 43;
/// Live (unrevoked, unexpired) tokens one repository may hold.
pub const MAX_LIVE_PER_REPO: i64 = 500;
/// Longest token name.
pub const MAX_NAME: usize = 64;
/// Most path globs on a token.
pub const MAX_PATHS: usize = 32;
/// The first lifetime of a token that asks for none.
pub const DEFAULT_TTL_SECS: u64 = 3600;
/// How often `last_used` is written, at most.
const TOUCH_EVERY_SECS: i64 = 30;

/// Limits; 0 means none at this level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub changes: u64,
    pub tokens: u64,
    pub cost: u64,
    pub paths: u64,
    pub runtime: u64,
}

impl Budget {
    pub fn from_wire(l: Option<&wire::AgentBudgetLimits>) -> Self {
        l.map_or_else(Self::default, |l| Self {
            changes: u64::from(l.max_changes),
            tokens: l.max_tokens,
            cost: l.max_cost_micro_usd,
            paths: u64::from(l.max_paths),
            runtime: u64::from(l.max_runtime_seconds),
        })
    }

    pub fn to_wire(self) -> wire::AgentBudgetLimits {
        wire::AgentBudgetLimits {
            max_changes: u32::try_from(self.changes).unwrap_or(u32::MAX),
            max_tokens: self.tokens,
            max_cost_micro_usd: self.cost,
            max_paths: u32::try_from(self.paths).unwrap_or(u32::MAX),
            max_runtime_seconds: u32::try_from(self.runtime).unwrap_or(u32::MAX),
        }
    }

    /// The stricter of two limits: the smaller non-zero one; zero means "no limit here".
    pub fn stricter(a: u64, b: u64) -> u64 {
        match (a, b) {
            (0, x) | (x, 0) => x,
            (x, y) => x.min(y),
        }
    }

    /// Dimension by dimension, the stricter limit.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            changes: Self::stricter(self.changes, other.changes),
            tokens: Self::stricter(self.tokens, other.tokens),
            cost: Self::stricter(self.cost, other.cost),
            paths: Self::stricter(self.paths, other.paths),
            runtime: Self::stricter(self.runtime, other.runtime),
        }
    }
}

/// What a token has spent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    pub changes: u64,
    pub tokens: u64,
    pub cost: u64,
    pub paths: u64,
    pub requests: u64,
}

impl Spent {
    pub fn to_wire(self, runtime_seconds: u32) -> wire::AgentBudgetUsage {
        wire::AgentBudgetUsage {
            changes: u32::try_from(self.changes).unwrap_or(u32::MAX),
            tokens: self.tokens,
            cost_micro_usd: self.cost,
            paths: u32::try_from(self.paths).unwrap_or(u32::MAX),
            runtime_seconds,
            requests: self.requests,
        }
    }

    /// `limit - spent`, saturating at 0; a field with no limit stays 0 ("unset has no limit").
    pub fn remaining(limit: Budget, spent: Self, runtime_left: u32) -> wire::AgentBudgetUsage {
        let left = |lim: u64, used: u64| {
            if lim == 0 {
                0
            } else {
                lim.saturating_sub(used)
            }
        };
        wire::AgentBudgetUsage {
            changes: u32::try_from(left(limit.changes, spent.changes)).unwrap_or(u32::MAX),
            tokens: left(limit.tokens, spent.tokens),
            cost_micro_usd: left(limit.cost, spent.cost),
            paths: u32::try_from(left(limit.paths, spent.paths)).unwrap_or(u32::MAX),
            runtime_seconds: runtime_left,
            requests: 0,
        }
    }
}

/// A token row. The secret is not in it.
#[derive(Clone)]
pub struct TokenRow {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub agent: String,
    pub secret_hint: String,
    pub paths: Vec<String>,
    pub max_changes_per_run: u32,
    pub created_by: String,
    pub owner_id: i64,
    pub created: i64,
    pub expires_at: i64,
    pub revoked_at: i64,
    pub revoked_reason: String,
    pub last_used: i64,
    pub limits: Budget,
    pub spent: Spent,
    pub exhausted: bool,
    pub run_id: u64,
}

impl fmt::Debug for TokenRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenRow")
            .field("id", &self.id)
            .field("repo", &self.repo)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl TokenRow {
    /// Where the token stands at `now`.
    pub fn state(&self, now: i64) -> wire::AgentTokenState {
        if self.revoked_at > 0 {
            wire::AgentTokenState::Revoked
        } else if self.expires_at <= now {
            wire::AgentTokenState::Expired
        } else if self.exhausted {
            wire::AgentTokenState::Exhausted
        } else {
            wire::AgentTokenState::Active
        }
    }

    /// The principal the token acts as: `agent:<name>:<owner id>:<token id>`.
    pub fn principal(&self) -> String {
        principal_id(&self.agent, self.owner_id, &self.id)
    }

    pub fn to_wire(&self, now: i64) -> wire::AgentToken {
        wire::AgentToken {
            id: self.id.clone(),
            repo: self.repo.clone(),
            name: self.name.clone(),
            agent: self.agent.clone(),
            paths: self.paths.clone(),
            max_changes_per_run: self.max_changes_per_run,
            state: self.state(now) as i32,
            created_by: self.created_by.clone(),
            created: self.created,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            last_used: self.last_used,
            limits: Some(self.limits.to_wire()),
            usage: Some(self.spent.to_wire(0)),
            secret_hint: self.secret_hint.clone(),
        }
    }
}

// ------------------------------------------------------------------------------ principals

/// The principal id of an agent: `agent:<name>:<owner id>:<token id>`. The name is what the
/// repository policy's `agent:<glob>` subjects match; the owner id names the namespace of the
/// agent's draft refs (`refs/users/<owner id>/heads/agent/...`); the token id finds the budget.
pub fn principal_id(agent: &str, owner_id: i64, token_id: &str) -> String {
    format!("agent:{agent}:{owner_id}:{token_id}")
}

/// What a principal id of an agent holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPrincipal {
    pub name: String,
    pub owner_id: i64,
    pub token_id: String,
}

/// Parses [`principal_id`]; `None` for any other principal.
pub fn parse_principal(id: &str) -> Option<AgentPrincipal> {
    let rest = id.strip_prefix("agent:")?;
    let mut it = rest.splitn(3, ':');
    let name = it.next()?;
    let owner_id: i64 = it.next()?.parse().ok().filter(|n| *n > 0)?;
    let token_id = it.next()?;
    (valid_agent_name(name) && token_id.starts_with("at_") && !token_id.contains(':')).then(|| {
        AgentPrincipal {
            name: name.to_owned(),
            owner_id,
            token_id: token_id.to_owned(),
        }
    })
}

/// 1 to 40 bytes of `[a-z0-9-]`, not starting or ending with `-`.
pub fn valid_agent_name(s: &str) -> bool {
    asc_core::workflow::valid_agent_name(s)
}

/// Ids of users the forge numbers are small; agents of a deployment without numbered users get a
/// stable id from this base up, so their draft namespace cannot collide with a real user's.
pub const SYNTHETIC_OWNER_BASE: i64 = 1 << 40;

/// The owner id of the draft namespace of agents acting for `principal_id`: the id of a verified
/// forge user, else a stable id derived from the principal text.
pub fn owner_id_for(principal_id: &str) -> i64 {
    if let Some(id) =
        crate::authz::UserIdentity::parse(principal_id).filter(crate::authz::UserIdentity::is_user)
    {
        return id.user_id;
    }
    let digest = Sha256::digest(principal_id.as_bytes());
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    SYNTHETIC_OWNER_BASE + i64::from(n)
}

// ------------------------------------------------------------------------------ secrets

/// A fresh token secret. It is shown once and then only its hash exists.
pub fn new_secret() -> Zeroizing<String> {
    let mut raw = Zeroizing::new([0u8; 32]);
    getrandom::fill(raw.as_mut()).expect("random source");
    Zeroizing::new(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw.as_ref())))
}

/// Whether `s` has the shape of a token (not whether it is one that exists).
pub fn looks_like_token(s: &str) -> bool {
    s.len() == TOKEN_LEN
        && s.starts_with(PREFIX)
        && s[PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The lowercase hex SHA-256 of a token string: what the table stores.
pub fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Constant-time equality of two byte strings.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The last four characters of a secret.
pub fn hint(secret: &str) -> String {
    let n = secret.chars().count();
    secret.chars().skip(n.saturating_sub(4)).collect()
}

// ------------------------------------------------------------------------------ the store

/// Why a budget operation refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A limit would be passed (`changes`, `tokens`, `cost`, `paths`).
    Budget(&'static str),
    /// No such token, or it is revoked or expired.
    Gone,
    Sql(String),
}

impl From<sqlx::Error> for Refusal {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(class(&e))
    }
}

/// A new token.
#[derive(Debug)]
pub struct NewToken {
    pub repo: String,
    pub name: String,
    pub agent: String,
    pub paths: Vec<String>,
    pub max_changes_per_run: u32,
    pub created_by: String,
    pub owner_id: i64,
    /// The time the token is made at, which `expires_at` was counted from; 0 reads the clock. A
    /// caller that counts a lifetime passes the same reading for both, so the lifetime is exact.
    pub created: i64,
    pub expires_at: i64,
    pub limits: Budget,
    pub run_id: u64,
}

#[derive(Clone)]
pub struct TokenStore {
    pool: AnyPool,
}

const COLS: &str = "id, repo, name, agent, secret_hint, paths_json, max_changes_per_run, created_by, owner_id, created, \
     expires_at, revoked_at, revoked_reason, last_used, lim_changes, lim_tokens, lim_cost, lim_paths, lim_runtime, \
     used_changes, used_tokens, used_cost, used_paths, used_requests, exhausted, run_id";

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn row_of(r: &AnyRow) -> Result<TokenRow, sqlx::Error> {
    let paths: Vec<String> =
        serde_json::from_str(&r.try_get::<String, _>("paths_json")?).unwrap_or_default();
    Ok(TokenRow {
        id: r.try_get("id")?,
        repo: r.try_get("repo")?,
        name: r.try_get("name")?,
        agent: r.try_get("agent")?,
        secret_hint: r.try_get("secret_hint")?,
        paths,
        max_changes_per_run: u32::try_from(r.try_get::<i64, _>("max_changes_per_run")?)
            .unwrap_or(0),
        created_by: r.try_get("created_by")?,
        owner_id: r.try_get("owner_id")?,
        created: r.try_get("created")?,
        expires_at: r.try_get("expires_at")?,
        revoked_at: r.try_get("revoked_at")?,
        revoked_reason: r.try_get("revoked_reason")?,
        last_used: r.try_get("last_used")?,
        limits: Budget {
            changes: u(r.try_get("lim_changes")?),
            tokens: u(r.try_get("lim_tokens")?),
            cost: u(r.try_get("lim_cost")?),
            paths: u(r.try_get("lim_paths")?),
            runtime: u(r.try_get("lim_runtime")?),
        },
        spent: Spent {
            changes: u(r.try_get("used_changes")?),
            tokens: u(r.try_get("used_tokens")?),
            cost: u(r.try_get("used_cost")?),
            paths: u(r.try_get("used_paths")?),
            requests: u(r.try_get("used_requests")?),
        },
        exhausted: r.try_get::<i64, _>("exhausted")? != 0,
        run_id: u(r.try_get("run_id")?),
    })
}

impl TokenStore {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// Inserts a token whose secret hashes to `secret_hash`. `Ok(None)`: the repository holds as
    /// many live tokens as it may.
    pub async fn create(
        &self,
        n: &NewToken,
        secret: &str,
    ) -> Result<Option<TokenRow>, sqlx::Error> {
        let now = if n.created > 0 {
            n.created
        } else {
            clock::now()
        };
        let live: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_tokens WHERE repo = $1 AND revoked_at = 0 AND expires_at > $2",
        )
        .bind(n.repo.clone())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        if live >= MAX_LIVE_PER_REPO {
            return Ok(None);
        }
        let id = crate::ai::rand_id("at_", 20);
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO agent_tokens ({COLS}, secret_sha256) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 0, '', 0, $12, $13, $14, $15, $16, 0, 0, 0, 0, 0, 0, $17, $18)"
        )))
        .bind(id.clone())
        .bind(n.repo.clone())
        .bind(n.name.clone())
        .bind(n.agent.clone())
        .bind(hint(secret))
        .bind(serde_json::to_string(&n.paths).unwrap_or_else(|_| "[]".into()))
        .bind(i64::from(n.max_changes_per_run))
        .bind(n.created_by.clone())
        .bind(n.owner_id)
        .bind(now)
        .bind(n.expires_at)
        .bind(i(n.limits.changes))
        .bind(i(n.limits.tokens))
        .bind(i(n.limits.cost))
        .bind(i(n.limits.paths))
        .bind(i(n.limits.runtime))
        .bind(i(n.run_id))
        .bind(hash(secret))
        .execute(&self.pool)
        .await?;
        self.get(&id).await
    }

    /// The token whose secret is `secret`. Looked up by hash, then compared in constant time.
    pub async fn by_secret(&self, secret: &str) -> Result<Option<TokenRow>, sqlx::Error> {
        let h = hash(secret);
        let q = format!("SELECT {COLS}, secret_sha256 FROM agent_tokens WHERE secret_sha256 = $1");
        let Some(row) = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(h.clone())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let stored: String = row.try_get("secret_sha256")?;
        if !ct_eq(stored.as_bytes(), h.as_bytes()) {
            return Ok(None);
        }
        row_of(&row).map(Some)
    }

    pub async fn get(&self, id: &str) -> Result<Option<TokenRow>, sqlx::Error> {
        let q = format!("SELECT {COLS} FROM agent_tokens WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(id.to_owned())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(row_of)
            .transpose()
    }

    /// Newest first. `after` is `(created, id)` of the last row of the previous page.
    pub async fn list(
        &self,
        repo: &str,
        states: &[wire::AgentTokenState],
        after: Option<(i64, String)>,
        limit: usize,
        now: i64,
    ) -> Result<Vec<TokenRow>, sqlx::Error> {
        // `now` is a number from our own clock, so it is written into the statement; it is the
        // same value in every comparison of one listing.
        let mut conds: Vec<String> = Vec::new();
        for s in states {
            conds.push(match s {
                wire::AgentTokenState::Active => {
                    format!("(revoked_at = 0 AND expires_at > {now} AND exhausted = 0)")
                }
                wire::AgentTokenState::Expired => {
                    format!("(revoked_at = 0 AND expires_at <= {now})")
                }
                wire::AgentTokenState::Revoked => "(revoked_at > 0)".to_owned(),
                wire::AgentTokenState::Exhausted => {
                    format!("(revoked_at = 0 AND expires_at > {now} AND exhausted = 1)")
                }
                wire::AgentTokenState::Unspecified => "(1 = 1)".to_owned(),
            });
        }
        let state_sql = if conds.is_empty() {
            "1 = 1".to_owned()
        } else {
            conds.join(" OR ")
        };
        let page_sql = if after.is_some() {
            "AND (created < $2 OR (created = $2 AND id < $3))"
        } else {
            ""
        };
        let q = format!(
            "SELECT {COLS} FROM agent_tokens WHERE repo = $1 AND ({state_sql}) {page_sql}              ORDER BY created DESC, id DESC LIMIT {}",
            limit + 1
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(repo.to_owned());
        if let Some((c, id)) = after {
            query = query.bind(c).bind(id);
        }
        query
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_of)
            .collect()
    }

    /// Revokes a token. Idempotent: `Ok(false)` when it was revoked before.
    pub async fn revoke(&self, id: &str, reason: &str) -> Result<bool, sqlx::Error> {
        let r = sqlx::query("UPDATE agent_tokens SET revoked_at = $2, revoked_reason = $3 WHERE id = $1 AND revoked_at = 0")
            .bind(id.to_owned())
            .bind(clock::now())
            .bind(reason.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Replaces the limits. A token is exhausted again when it has already spent a limit it keeps.
    pub async fn set_limits(&self, id: &str, l: Budget) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_tokens SET lim_changes = $2, lim_tokens = $3, lim_cost = $4, lim_paths = $5, lim_runtime = $6, \
             exhausted = CASE WHEN ($3 > 0 AND used_tokens >= $3) OR ($4 > 0 AND used_cost >= $4) THEN 1 ELSE 0 END \
             WHERE id = $1",
        )
        .bind(id.to_owned())
        .bind(i(l.changes))
        .bind(i(l.tokens))
        .bind(i(l.cost))
        .bind(i(l.paths))
        .bind(i(l.runtime))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Puts the token on its run and gives it the limits the run row holds now, in one transaction.
    /// The run row is written first, which also takes its lock, so a concurrent edit of the limits
    /// either lands before the read below or finds the token on the run afterwards.
    pub async fn attach_to_run(&self, run_id: u64, token_id: &str) -> Result<(), sqlx::Error> {
        let run = i64::try_from(run_id).unwrap_or(0);
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_runs SET token_id = $2 WHERE id = $1")
            .bind(run)
            .bind(token_id.to_owned())
            .execute(&mut *tx)
            .await?;
        let json: Option<String> =
            sqlx::query_scalar("SELECT limits_json FROM agent_runs WHERE id = $1")
                .bind(run)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(l) = json.map(|j| super::runs::limits_of(&j)) {
            sqlx::query(
                "UPDATE agent_tokens SET lim_changes = $2, lim_tokens = $3, lim_cost = $4, lim_paths = $5, lim_runtime = $6,                  exhausted = CASE WHEN ($3 > 0 AND used_tokens >= $3) OR ($4 > 0 AND used_cost >= $4) THEN 1 ELSE 0 END                  WHERE id = $1",
            )
            .bind(token_id.to_owned())
            .bind(i(l.changes))
            .bind(i(l.tokens))
            .bind(i(l.cost))
            .bind(i(l.paths))
            .bind(i(l.runtime))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// Notes that the token was used (at most every 30 seconds).
    pub async fn touch(&self, id: &str, now: i64) {
        let _ =
            sqlx::query("UPDATE agent_tokens SET last_used = $2 WHERE id = $1 AND last_used < $3")
                .bind(id.to_owned())
                .bind(now)
                .bind(now - TOUCH_EVERY_SECS)
                .execute(&self.pool)
                .await;
    }

    /// Records what a provider call used, in full. The token is exhausted when a limit is
    /// reached; `Err(dimension)` says which limit was passed (not merely reached).
    pub async fn charge_usage(
        &self,
        id: &str,
        tokens: u64,
        cost: u64,
        requests: u64,
    ) -> Result<Result<TokenRow, (TokenRow, &'static str)>, Refusal> {
        let r = sqlx::query(
            "UPDATE agent_tokens SET used_tokens = used_tokens + $2, used_cost = used_cost + $3, \
             used_requests = used_requests + $4 WHERE id = $1 AND revoked_at = 0",
        )
        .bind(id.to_owned())
        .bind(i(tokens))
        .bind(i(cost))
        .bind(i(requests))
        .execute(&self.pool)
        .await?;
        if r.rows_affected() != 1 {
            return Err(Refusal::Gone);
        }
        let row = self.get(id).await?.ok_or(Refusal::Gone)?;
        let over = |lim: u64, used: u64| lim > 0 && used > lim;
        let reached = |lim: u64, used: u64| lim > 0 && used >= lim;
        if reached(row.limits.tokens, row.spent.tokens) || reached(row.limits.cost, row.spent.cost)
        {
            sqlx::query("UPDATE agent_tokens SET exhausted = 1 WHERE id = $1")
                .bind(id.to_owned())
                .execute(&self.pool)
                .await?;
        }
        let row = self.get(id).await?.ok_or(Refusal::Gone)?;
        if over(row.limits.tokens, row.spent.tokens) {
            return Ok(Err((row, "tokens")));
        }
        if over(row.limits.cost, row.spent.cost) {
            return Ok(Err((row, "cost")));
        }
        Ok(Ok(row))
    }

    /// Reserves `n` changes against the token's limit (`Refusal::Budget("changes")` when they do not fit).
    pub async fn reserve_changes(&self, id: &str, n: u64) -> Result<(), Refusal> {
        let r = sqlx::query(
            "UPDATE agent_tokens SET used_changes = used_changes + $2 WHERE id = $1 AND revoked_at = 0 \
             AND (lim_changes = 0 OR used_changes + $2 <= lim_changes)",
        )
        .bind(id.to_owned())
        .bind(i(n))
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 1 {
            Ok(())
        } else {
            Err(Refusal::Budget("changes"))
        }
    }

    /// Gives back changes reserved by an update that did not commit.
    pub async fn release_changes(&self, id: &str, n: u64) {
        let _ = sqlx::query(
            "UPDATE agent_tokens SET used_changes = CASE WHEN used_changes >= $2 THEN used_changes - $2 ELSE 0 END WHERE id = $1",
        )
        .bind(id.to_owned())
        .bind(i(n))
        .execute(&self.pool)
        .await;
    }

    /// Counts the distinct `paths` a token writes against its paths limit. Returns how many were
    /// new, so the caller can give them back.
    pub async fn reserve_paths(&self, id: &str, paths: &[String]) -> Result<Vec<String>, Refusal> {
        let mut tx = self.pool.begin().await?;
        let mut fresh = Vec::new();
        for p in paths {
            let r = sqlx::query("INSERT INTO agent_token_paths (token_id, path) VALUES ($1, $2) ON CONFLICT (token_id, path) DO NOTHING")
                .bind(id.to_owned())
                .bind(p.clone())
                .execute(&mut *tx)
                .await?;
            if r.rows_affected() == 1 {
                fresh.push(p.clone());
            }
        }
        let n = i(fresh.len() as u64);
        let r = sqlx::query(
            "UPDATE agent_tokens SET used_paths = used_paths + $2 WHERE id = $1 AND revoked_at = 0 \
             AND (lim_paths = 0 OR used_paths + $2 <= lim_paths)",
        )
        .bind(id.to_owned())
        .bind(n)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(Refusal::Budget("paths"));
        }
        tx.commit().await?;
        Ok(fresh)
    }

    /// Forgets paths reserved by an update that did not commit.
    pub async fn release_paths(&self, id: &str, fresh: &[String]) {
        for p in fresh {
            let _ = sqlx::query("DELETE FROM agent_token_paths WHERE token_id = $1 AND path = $2")
                .bind(id.to_owned())
                .bind(p.clone())
                .execute(&self.pool)
                .await;
        }
        let _ = sqlx::query(
            "UPDATE agent_tokens SET used_paths = CASE WHEN used_paths >= $2 THEN used_paths - $2 ELSE 0 END WHERE id = $1",
        )
        .bind(id.to_owned())
        .bind(i(fresh.len() as u64))
        .execute(&self.pool)
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_have_the_documented_shape_and_hash() {
        let s = new_secret();
        assert_eq!(s.len(), TOKEN_LEN);
        assert!(s.starts_with("ascat_"));
        assert!(looks_like_token(&s));
        assert_ne!(*s, *new_secret(), "random");
        assert_eq!(hash(&s).len(), 64);
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(!looks_like_token("ascat_short"));
        assert!(!looks_like_token(&format!("ascat_{}", "!".repeat(43))));
        assert!(!looks_like_token(&format!("other_{}", "a".repeat(43))));
        assert_eq!(hint("ascat_abcdefgh"), "efgh");
    }

    #[test]
    fn constant_time_comparison_is_exact() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn agent_principals_round_trip_and_reject_lookalikes() {
        let id = principal_id("fixer", 7, "at_abcdefghijklmnopqrst");
        assert_eq!(id, "agent:fixer:7:at_abcdefghijklmnopqrst");
        assert_eq!(
            parse_principal(&id),
            Some(AgentPrincipal {
                name: "fixer".into(),
                owner_id: 7,
                token_id: "at_abcdefghijklmnopqrst".into()
            })
        );
        for bad in [
            "agent:fixer",
            "agent:fixer:7",
            "agent:Fixer:7:at_x",
            "agent:fixer:0:at_x",
            "agent:fixer:-1:at_x",
            "agent:fixer:x:at_x",
            "agent:fixer:7:tok",
            "pat:alice",
            "agent::7:at_x",
            "agent:fixer:7:at_x:y",
        ] {
            assert_eq!(parse_principal(bad), None, "{bad}");
        }
        // A user cannot pose as an agent: handles cannot hold ':'.
        assert!(crate::authz::UserIdentity::parse(&id).is_none());
    }

    #[test]
    fn owner_ids_follow_the_forge_user_or_are_stable_and_far_from_real_ids() {
        assert_eq!(owner_id_for("7:alice"), 7);
        let a = owner_id_for("pat:alice");
        assert_eq!(a, owner_id_for("pat:alice"));
        assert_ne!(a, owner_id_for("pat:bob"));
        assert!(a >= SYNTHETIC_OWNER_BASE);
        // A deploy key, a CI task and the anonymous caller are not users: synthetic ids.
        assert!(owner_id_for("7:alice:3") >= SYNTHETIC_OWNER_BASE);
        assert!(owner_id_for("0:anonymous") >= SYNTHETIC_OWNER_BASE);
    }

    #[test]
    fn limits_merge_to_the_stricter_value() {
        assert_eq!(Budget::stricter(0, 0), 0);
        assert_eq!(Budget::stricter(0, 5), 5);
        assert_eq!(Budget::stricter(5, 0), 5);
        assert_eq!(Budget::stricter(5, 3), 3);
        let a = Budget {
            changes: 5,
            tokens: 0,
            cost: 10,
            paths: 0,
            runtime: 100,
        };
        let b = Budget {
            changes: 3,
            tokens: 7,
            cost: 0,
            paths: 0,
            runtime: 200,
        };
        assert_eq!(
            a.merge(b),
            Budget {
                changes: 3,
                tokens: 7,
                cost: 10,
                paths: 0,
                runtime: 100
            }
        );
    }

    #[test]
    fn remaining_saturates_and_leaves_unlimited_fields_at_zero() {
        let lim = Budget {
            changes: 5,
            tokens: 100,
            cost: 0,
            paths: 0,
            runtime: 0,
        };
        let spent = Spent {
            changes: 7,
            tokens: 40,
            cost: 999,
            paths: 1,
            requests: 3,
        };
        let r = Spent::remaining(lim, spent, 12);
        assert_eq!(
            (
                r.changes,
                r.tokens,
                r.cost_micro_usd,
                r.paths,
                r.runtime_seconds
            ),
            (0, 60, 0, 0, 12)
        );
    }

    #[test]
    fn the_state_follows_revocation_expiry_and_exhaustion() {
        let mut t = TokenRow {
            id: "at_x".into(),
            repo: "a/b".into(),
            name: "n".into(),
            agent: "bot".into(),
            secret_hint: "abcd".into(),
            paths: vec![],
            max_changes_per_run: 0,
            created_by: "pat:a".into(),
            owner_id: 7,
            created: 0,
            expires_at: 100,
            revoked_at: 0,
            revoked_reason: String::new(),
            last_used: 0,
            limits: Budget::default(),
            spent: Spent::default(),
            exhausted: false,
            run_id: 0,
        };
        assert_eq!(t.state(99), wire::AgentTokenState::Active);
        assert_eq!(
            t.state(100),
            wire::AgentTokenState::Expired,
            "expired at the second"
        );
        t.exhausted = true;
        assert_eq!(t.state(50), wire::AgentTokenState::Exhausted);
        t.revoked_at = 10;
        assert_eq!(t.state(50), wire::AgentTokenState::Revoked);
        assert!(
            !format!("{t:?}").contains("abcd"),
            "Debug hides the hint too"
        );
    }
}
