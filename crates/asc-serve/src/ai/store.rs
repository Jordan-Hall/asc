//! The SQL side of AI keys: the key rows, usage per day, and the monthly cap.
//!
//! A key row holds the sealed provider key and never the plaintext. The cap is enforced by
//! reservation: a call first adds its worst-case cost to `ai_month.reserved` with one conditional
//! `UPDATE ... WHERE spent + reserved + amount <= cap`, which the database serialises per row, so
//! any number of concurrent calls together cannot pass the cap. When the call ends its
//! reservation is replaced by the real cost.

use std::fmt;

use asc_rpc::server::wire;
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};
use tonic::Status;

use super::clock;
use super::owner::Owner;

/// The scope of the instance-wide cap in `ai_month`.
pub const INSTANCE_SCOPE: &str = "*";
/// A reservation older than this is returned by the sweep (a call never runs this long).
pub const RESERVATION_TTL_SECS: i64 = 30 * 60;

/// A provider this server speaks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
    OpenAiCompatible,
}

impl Provider {
    pub fn from_wire(n: i32) -> Result<Self, Status> {
        match wire::AiProvider::try_from(n) {
            Ok(wire::AiProvider::Anthropic) => Ok(Self::Anthropic),
            Ok(wire::AiProvider::Openai) => Ok(Self::OpenAi),
            Ok(wire::AiProvider::OpenaiCompatible) => Ok(Self::OpenAiCompatible),
            Ok(wire::AiProvider::Google) => Err(Status::invalid_argument(
                "the Google provider is not supported by this server; use an OpenAI-compatible endpoint",
            )),
            _ => Err(Status::invalid_argument("unknown provider")),
        }
    }

    pub fn to_wire(self) -> i32 {
        (match self {
            Self::Anthropic => wire::AiProvider::Anthropic,
            Self::OpenAi => wire::AiProvider::Openai,
            Self::OpenAiCompatible => wire::AiProvider::OpenaiCompatible,
        }) as i32
    }

    /// The name audit events and docs use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }

    /// The API root used when the key names none. `None`: the provider needs a `base_url`.
    pub fn default_base(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some(super::provider::ANTHROPIC_BASE),
            Self::OpenAi => Some(super::provider::OPENAI_BASE),
            Self::OpenAiCompatible => None,
        }
    }
}

/// What the server last learned about a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Untested = 1,
    Active = 2,
    Failed = 3,
}

impl KeyState {
    fn from_i64(n: i64) -> Self {
        match n {
            2 => Self::Active,
            3 => Self::Failed,
            _ => Self::Untested,
        }
    }
}

/// A key row.
#[derive(Clone)]
pub struct KeyRow {
    pub id: String,
    pub owner: Owner,
    pub label: String,
    pub provider: Provider,
    pub base_url: String,
    pub default_model: String,
    pub sealed_key: Vec<u8>,
    pub key_gen: u32,
    pub key_hint: String,
    pub key_version: u32,
    /// Counts every change of the row; an update succeeds only against the revision it read.
    pub rev: u64,
    pub state: KeyState,
    pub disabled: bool,
    pub monthly_cap: u64,
    pub created: i64,
    pub updated: i64,
    pub tested_at: i64,
    pub last_error: String,
}

impl fmt::Debug for KeyRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRow")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("label", &self.label)
            .field("provider", &self.provider)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl KeyRow {
    /// The state a client sees: the stored one, `Disabled` by the owner's switch, `Capped` when
    /// the month's spending has reached the owner's cap.
    pub fn wire_state(&self, month_spent: u64) -> wire::AiKeyState {
        if self.disabled {
            wire::AiKeyState::Disabled
        } else if self.monthly_cap > 0 && month_spent >= self.monthly_cap {
            wire::AiKeyState::Capped
        } else {
            match self.state {
                KeyState::Untested => wire::AiKeyState::Untested,
                KeyState::Active => wire::AiKeyState::Active,
                KeyState::Failed => wire::AiKeyState::Failed,
            }
        }
    }

    /// The wire form. The key itself never appears: only its hint.
    pub fn to_wire(&self, month_spent: u64) -> wire::AiKey {
        wire::AiKey {
            id: self.id.clone(),
            owner: Some(self.owner.to_wire()),
            label: self.label.clone(),
            provider: self.provider.to_wire(),
            base_url: self.base_url.clone(),
            default_model: self.default_model.clone(),
            key_hint: self.key_hint.clone(),
            has_key: !self.sealed_key.is_empty(),
            state: self.wire_state(month_spent) as i32,
            monthly_cap_micro_usd: self.monthly_cap,
            month_spent_micro_usd: month_spent,
            created: self.created,
            updated: self.updated,
            tested_at: self.tested_at,
            last_error: self.last_error.clone(),
            key_version: self.key_version,
        }
    }
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

const COLS: &str = "id, owner_kind, owner_name, label, provider, base_url, default_model, sealed_key, \
     key_gen, key_hint, key_version, rev, state, disabled, monthly_cap, created, updated, tested_at, last_error";

fn row_of(r: &AnyRow) -> Result<KeyRow, sqlx::Error> {
    let provider = match r.try_get::<i64, _>("provider")? {
        2 => Provider::OpenAi,
        4 => Provider::OpenAiCompatible,
        _ => Provider::Anthropic,
    };
    let owner = Owner::from_columns(
        r.try_get("owner_kind")?,
        &r.try_get::<String, _>("owner_name")?,
    )
    .ok_or_else(|| sqlx::Error::Decode("owner".into()))?;
    Ok(KeyRow {
        id: r.try_get("id")?,
        owner,
        label: r.try_get("label")?,
        provider,
        base_url: r.try_get("base_url")?,
        default_model: r.try_get("default_model")?,
        sealed_key: r.try_get("sealed_key")?,
        key_gen: u32::try_from(r.try_get::<i64, _>("key_gen")?).unwrap_or(0),
        key_hint: r.try_get("key_hint")?,
        key_version: u32::try_from(r.try_get::<i64, _>("key_version")?).unwrap_or(1),
        rev: u64_of(r.try_get("rev")?),
        state: KeyState::from_i64(r.try_get("state")?),
        disabled: r.try_get::<i64, _>("disabled")? != 0,
        monthly_cap: u64_of(r.try_get("monthly_cap")?),
        created: r.try_get("created")?,
        updated: r.try_get("updated")?,
        tested_at: r.try_get("tested_at")?,
        last_error: r.try_get("last_error")?,
    })
}

/// A recorded call.
#[derive(Clone, Debug)]
pub struct UsageRecord<'a> {
    pub key_id: &'a str,
    pub owner: &'a Owner,
    pub repo: &'a str,
    pub agent: &'a str,
    pub model: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `None`: the model has no price (recorded as cost 0 and counted as unpriced).
    pub cost_micro_usd: Option<u64>,
}

/// One row of a usage report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageSum {
    pub group: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    pub unpriced: u64,
}

/// What a usage report groups by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    Day,
    Model,
    Repo,
    Agent,
}

impl GroupBy {
    fn column(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Model => "model",
            Self::Repo => "repo",
            Self::Agent => "agent",
        }
    }
}

/// A cap reservation taken by [`AiStore::reserve`].
#[derive(Debug)]
pub struct Reservation {
    ids: Vec<String>,
}

/// Why a reservation was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// The key's monthly cap would be passed.
    KeyCap,
    /// The instance cap would be passed.
    InstanceCap,
    Sql(String),
}

impl From<sqlx::Error> for ReserveError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(class(&e))
    }
}

/// A driver error reduced to its class: statements and values are never quoted.
pub fn class(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(_) => "statement failed",
        sqlx::Error::PoolTimedOut => "pool timed out",
        sqlx::Error::Io(_) => "io",
        _ => "error",
    }
    .to_owned()
}

/// The store.
#[derive(Clone)]
pub struct AiStore {
    pool: AnyPool,
}

impl AiStore {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    // -------------------------------------------------------------------------- keys

    /// Inserts a key; `Ok(false)` when the owner already has a key with that label.
    pub async fn insert(&self, k: &KeyRow) -> Result<bool, sqlx::Error> {
        let (kind, name) = k.owner.columns();
        let q = format!(
            "INSERT INTO ai_keys ({COLS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)"
        );
        let r = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(k.id.clone())
            .bind(kind)
            .bind(name)
            .bind(k.label.clone())
            .bind(i64::from(k.provider.to_wire()))
            .bind(k.base_url.clone())
            .bind(k.default_model.clone())
            .bind(k.sealed_key.clone())
            .bind(i64::from(k.key_gen))
            .bind(k.key_hint.clone())
            .bind(i64::from(k.key_version))
            .bind(i64_of(k.rev))
            .bind(k.state as i64)
            .bind(i64::from(k.disabled))
            .bind(i64_of(k.monthly_cap))
            .bind(k.created)
            .bind(k.updated)
            .bind(k.tested_at)
            .bind(k.last_error.clone())
            .execute(&self.pool)
            .await;
        match r {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn get(&self, id: &str) -> Result<Option<KeyRow>, sqlx::Error> {
        let q = format!("SELECT {COLS} FROM ai_keys WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(id.to_owned())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(row_of)
            .transpose()
    }

    pub async fn list(&self, owner: &Owner) -> Result<Vec<KeyRow>, sqlx::Error> {
        let (kind, name) = owner.columns();
        let q = format!(
            "SELECT {COLS} FROM ai_keys WHERE owner_kind = $1 AND owner_name = $2 ORDER BY label"
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(kind)
            .bind(name)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_of)
            .collect()
    }

    pub async fn count(&self, owner: &Owner) -> Result<i64, sqlx::Error> {
        let (kind, name) = owner.columns();
        sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM ai_keys WHERE owner_kind = $1 AND owner_name = $2",
        )
        .bind(kind)
        .bind(name)
        .fetch_one(&self.pool)
        .await
    }

    /// Writes every mutable field of `k` back and bumps its revision, provided the row still has
    /// the revision `k` was read at (`Ok(false)` when another writer changed the key first).
    pub async fn replace(&self, k: &KeyRow) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE ai_keys SET label = $2, base_url = $3, default_model = $4, sealed_key = $5, key_gen = $6,              key_hint = $7, key_version = $8, state = $9, disabled = $10, monthly_cap = $11, updated = $12,              tested_at = $13, last_error = $14, rev = rev + 1 WHERE id = $1 AND rev = $15",
        )
        .bind(k.id.clone())
        .bind(k.label.clone())
        .bind(k.base_url.clone())
        .bind(k.default_model.clone())
        .bind(k.sealed_key.clone())
        .bind(i64::from(k.key_gen))
        .bind(k.key_hint.clone())
        .bind(i64::from(k.key_version))
        .bind(k.state as i64)
        .bind(i64::from(k.disabled))
        .bind(i64_of(k.monthly_cap))
        .bind(k.updated)
        .bind(k.tested_at)
        .bind(k.last_error.clone())
        .bind(i64_of(k.rev))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Records the outcome of a test or of a provider rejection. Only moves a key whose secret
    /// is still the version that was tried.
    pub async fn mark(
        &self,
        id: &str,
        key_version: u32,
        state: KeyState,
        error: &str,
        tested: bool,
    ) -> Result<(), sqlx::Error> {
        let now = clock::now();
        sqlx::query(
            "UPDATE ai_keys SET state = $2, last_error = $3, tested_at = CASE WHEN $4 = 1 THEN $5 ELSE tested_at END \
             WHERE id = $1 AND key_version = $6",
        )
        .bind(id.to_owned())
        .bind(state as i64)
        .bind(error.to_owned())
        .bind(i64::from(tested))
        .bind(now)
        .bind(i64::from(key_version))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<bool, sqlx::Error> {
        let r = sqlx::query("DELETE FROM ai_keys WHERE id = $1")
            .bind(id.to_owned())
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM ai_month WHERE scope = $1")
            .bind(id.to_owned())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    // -------------------------------------------------------------------------- usage

    /// Adds one call to the usage of its key and day.
    pub async fn record(&self, u: &UsageRecord<'_>, at: i64) -> Result<(), sqlx::Error> {
        let (kind, name) = u.owner.columns();
        sqlx::query(
            "INSERT INTO ai_usage (key_id, owner_kind, owner_name, day, repo, agent, model, requests, \
             input_tokens, output_tokens, cost_micro_usd, unpriced) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9, $10, $11) \
             ON CONFLICT (key_id, day, repo, agent, model) DO UPDATE SET \
             requests = ai_usage.requests + 1, \
             input_tokens = ai_usage.input_tokens + EXCLUDED.input_tokens, \
             output_tokens = ai_usage.output_tokens + EXCLUDED.output_tokens, \
             cost_micro_usd = ai_usage.cost_micro_usd + EXCLUDED.cost_micro_usd, \
             unpriced = ai_usage.unpriced + EXCLUDED.unpriced",
        )
        .bind(u.key_id.to_owned())
        .bind(kind)
        .bind(name)
        .bind(clock::day(at))
        .bind(u.repo.to_owned())
        .bind(u.agent.to_owned())
        .bind(u.model.to_owned())
        .bind(i64_of(u.input_tokens))
        .bind(i64_of(u.output_tokens))
        .bind(i64_of(u.cost_micro_usd.unwrap_or(0)))
        .bind(i64::from(u.cost_micro_usd.is_none()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Usage of `owner` (or of one of its keys) between two UTC days, inclusive.
    pub async fn usage(
        &self,
        owner: &Owner,
        key_id: Option<&str>,
        first_day: &str,
        last_day: &str,
        by: GroupBy,
    ) -> Result<Vec<UsageSum>, sqlx::Error> {
        let (kind, name) = owner.columns();
        let col = by.column();
        let key_filter = if key_id.is_some() {
            "AND key_id = $5"
        } else {
            ""
        };
        let q = format!(
            "SELECT {col} AS g, CAST(SUM(requests) AS BIGINT) AS r, CAST(SUM(input_tokens) AS BIGINT) AS i, \
             CAST(SUM(output_tokens) AS BIGINT) AS o, CAST(SUM(cost_micro_usd) AS BIGINT) AS c, \
             CAST(SUM(unpriced) AS BIGINT) AS u FROM ai_usage \
             WHERE owner_kind = $1 AND owner_name = $2 AND day >= $3 AND day <= $4 {key_filter} \
             GROUP BY {col} ORDER BY {col}"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(kind)
            .bind(name)
            .bind(first_day.to_owned())
            .bind(last_day.to_owned());
        if let Some(k) = key_id {
            query = query.bind(k.to_owned());
        }
        query
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                Ok(UsageSum {
                    group: r.try_get("g")?,
                    requests: u64_of(r.try_get("r")?),
                    input_tokens: u64_of(r.try_get("i")?),
                    output_tokens: u64_of(r.try_get("o")?),
                    cost_micro_usd: u64_of(r.try_get("c")?),
                    unpriced: u64_of(r.try_get("u")?),
                })
            })
            .collect()
    }

    // -------------------------------------------------------------------------- the monthly cap

    /// What `scope` (a key id, or [`INSTANCE_SCOPE`]) spent in `month`.
    pub async fn month_spent(&self, scope: &str, month: &str) -> Result<u64, sqlx::Error> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT spent FROM ai_month WHERE scope = $1 AND month = $2")
                .bind(scope.to_owned())
                .bind(month.to_owned())
                .fetch_optional(&self.pool)
                .await?;
        Ok(u64_of(v.unwrap_or(0)))
    }

    /// Reserves `amount` against the key's cap and the instance cap (0 means no cap). All or none.
    pub async fn reserve(
        &self,
        key_id: &str,
        key_cap: u64,
        instance_cap: u64,
        amount: u64,
        at: i64,
    ) -> Result<Reservation, ReserveError> {
        let month = clock::month(at);
        let mut tx = self.pool.begin().await?;
        let mut ids = Vec::new();
        for (scope, cap, refusal) in [
            (key_id, key_cap, ReserveError::KeyCap),
            (INSTANCE_SCOPE, instance_cap, ReserveError::InstanceCap),
        ] {
            if cap == 0 {
                continue;
            }
            sqlx::query("INSERT INTO ai_month (scope, month) VALUES ($1, $2) ON CONFLICT (scope, month) DO NOTHING")
                .bind(scope.to_owned())
                .bind(month.clone())
                .execute(&mut *tx)
                .await?;
            let r = sqlx::query(
                "UPDATE ai_month SET reserved = reserved + $3 \
                 WHERE scope = $1 AND month = $2 AND spent + reserved + $3 <= $4",
            )
            .bind(scope.to_owned())
            .bind(month.clone())
            .bind(i64_of(amount))
            .bind(i64_of(cap))
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() != 1 {
                tx.rollback().await?;
                return Err(refusal);
            }
            let id = crate::ai::rand_id("rv_", 20);
            sqlx::query("INSERT INTO ai_reservations (id, scope, month, amount, at) VALUES ($1, $2, $3, $4, $5)")
                .bind(id.clone())
                .bind(scope.to_owned())
                .bind(month.clone())
                .bind(i64_of(amount))
                .bind(at)
                .execute(&mut *tx)
                .await?;
            ids.push(id);
        }
        tx.commit().await?;
        Ok(Reservation { ids })
    }

    /// Replaces a reservation by what the call really cost. The real cost counts even when the
    /// reservation was swept in the meantime.
    pub async fn settle(
        &self,
        r: Reservation,
        key_id: &str,
        actual: u64,
        at: i64,
    ) -> Result<(), sqlx::Error> {
        let month = clock::month(at);
        let mut tx = self.pool.begin().await?;
        let mut scopes_done: Vec<String> = Vec::new();
        for id in &r.ids {
            // The first statement of the transaction is a write, so SQLite takes its write lock
            // here (and waits for it) instead of failing a later upgrade of a read lock.
            let row = sqlx::query(
                "DELETE FROM ai_reservations WHERE id = $1 RETURNING scope, month, amount",
            )
            .bind(id.clone())
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else { continue };
            let scope: String = row.try_get("scope")?;
            let res_month: String = row.try_get("month")?;
            let amount: i64 = row.try_get("amount")?;
            sqlx::query(
                "UPDATE ai_month SET reserved = CASE WHEN reserved >= $3 THEN reserved - $3 ELSE 0 END,                  spent = spent + $4 WHERE scope = $1 AND month = $2",
            )
            .bind(scope.clone())
            .bind(res_month)
            .bind(amount)
            .bind(i64_of(actual))
            .execute(&mut *tx)
            .await?;
            scopes_done.push(scope);
        }
        // Spending counts against the key's month even when it has no cap (the report shows it),
        // and against the instance's when that has one.
        if !scopes_done.iter().any(|s| s == key_id) {
            sqlx::query("INSERT INTO ai_month (scope, month) VALUES ($1, $2) ON CONFLICT (scope, month) DO NOTHING")
                .bind(key_id.to_owned())
                .bind(month.clone())
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE ai_month SET spent = spent + $3 WHERE scope = $1 AND month = $2")
                .bind(key_id.to_owned())
                .bind(month)
                .bind(i64_of(actual))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Returns the reservations older than [`RESERVATION_TTL_SECS`] (calls that never settled).
    pub async fn sweep(&self, at: i64) -> Result<u64, sqlx::Error> {
        let cutoff = at - RESERVATION_TTL_SECS;
        let rows = sqlx::query("SELECT id FROM ai_reservations WHERE at < $1 LIMIT 500")
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await?;
        let mut n = 0;
        for row in rows {
            let id: String = row.try_get("id")?;
            let mut tx = self.pool.begin().await?;
            let Some(r) = sqlx::query(
                "DELETE FROM ai_reservations WHERE id = $1 RETURNING scope, month, amount",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            else {
                continue;
            };
            sqlx::query(
                "UPDATE ai_month SET reserved = CASE WHEN reserved >= $3 THEN reserved - $3 ELSE 0 END                  WHERE scope = $1 AND month = $2",
            )
            .bind(r.try_get::<String, _>("scope")?)
            .bind(r.try_get::<String, _>("month")?)
            .bind(r.try_get::<i64, _>("amount")?)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            n += 1;
        }
        Ok(n)
    }

    // -------------------------------------------------------------------------- re-sealing

    /// Sealed values per vault generation.
    pub async fn counts_by_generation(&self) -> Result<Vec<(u32, u64)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT key_gen, CAST(COUNT(*) AS BIGINT) AS n FROM ai_keys GROUP BY key_gen ORDER BY key_gen",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    u32::try_from(r.try_get::<i64, _>("key_gen")?).unwrap_or(0),
                    u64_of(r.try_get("n")?),
                ))
            })
            .collect()
    }

    /// Up to `limit` keys not sealed under `target`: `(id, owner, sealed)`.
    pub async fn not_under(
        &self,
        target: u32,
        limit: usize,
    ) -> Result<Vec<(String, Owner, Vec<u8>)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_kind, owner_name, sealed_key FROM ai_keys WHERE key_gen <> $1 ORDER BY id LIMIT $2",
        )
        .bind(i64::from(target))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let owner = Owner::from_columns(
                    r.try_get("owner_kind")?,
                    &r.try_get::<String, _>("owner_name")?,
                )
                .ok_or_else(|| sqlx::Error::Decode("owner".into()))?;
                Ok((r.try_get("id")?, owner, r.try_get("sealed_key")?))
            })
            .collect()
    }

    /// Replaces a sealed key only if it still holds `old`.
    pub async fn replace_sealed(
        &self,
        id: &str,
        old: &[u8],
        new: &[u8],
        generation: u32,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE ai_keys SET sealed_key = $3, key_gen = $4 WHERE id = $1 AND sealed_key = $2",
        )
        .bind(id.to_owned())
        .bind(old.to_vec())
        .bind(new.to_vec())
        .bind(i64::from(generation))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }
}
