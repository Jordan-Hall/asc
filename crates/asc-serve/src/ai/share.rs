//! Sharing of AI keys and the warning and stop rules of their caps (`aikeyshares.proto`,
//! docs/BUILDOUT.md section 31).
//!
//! A share lets the agents and workflows of a grantee use a key it does not own. The key stays
//! sealed and unreadable to the grantee; [`AiHub::resolve`] consults the shares after each owner's
//! own keys. Usage is attributed to a grantee by the repository the call was made for.

use sqlx::Row;

use super::owner::Owner;
use super::store::{KeyRow, KeyState};
use super::{AiError, AiHub, audit, clock, rand_id};

/// Most shares of one key.
pub const MAX_SHARES: i64 = 50;

/// One share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub id: String,
    pub key_id: String,
    pub grantee: Owner,
    pub allow_use: bool,
    pub see_spend: bool,
    pub created: i64,
    pub created_by: String,
}

/// The caps side of a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub warn_percent: u32,
    pub stop_at_cap: bool,
}

/// The use of one grantee.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareUse {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: u64,
}

fn u(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn share_of(r: &sqlx::any::AnyRow) -> Result<Option<Share>, sqlx::Error> {
    let grantee = Owner::from_columns(
        r.try_get("grantee_kind")?,
        &r.try_get::<String, _>("grantee_name")?,
    );
    Ok(grantee.map(|grantee| Share {
        id: r.try_get("id").unwrap_or_default(),
        key_id: r.try_get("key_id").unwrap_or_default(),
        grantee,
        allow_use: r.try_get::<i64, _>("allow_use").unwrap_or(0) != 0,
        see_spend: r.try_get::<i64, _>("see_spend").unwrap_or(0) != 0,
        created: r.try_get("created").unwrap_or(0),
        created_by: r.try_get("created_by").unwrap_or_default(),
    }))
}

const SHARE_COLS: &str =
    "id, key_id, grantee_kind, grantee_name, allow_use, see_spend, created, created_by";

/// Whether a call for `repo` belongs to `grantee`: the repository itself, or one its organisation
/// or user owns.
pub fn matches(grantee: &Owner, repo: &str) -> bool {
    let repo = repo.to_ascii_lowercase();
    match grantee {
        Owner::Repo(r) => *r == repo,
        Owner::Org(o) | Owner::User(o) => repo.strip_prefix(&format!("{o}/")).is_some(),
        Owner::Instance => false,
    }
}

impl AiHub {
    /// The shares of `key_id`, oldest first.
    pub async fn shares(&self, key_id: &str) -> Result<Vec<Share>, AiError> {
        let q = format!(
            "SELECT {SHARE_COLS} FROM ai_key_shares WHERE key_id = $1 ORDER BY created, id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(key_id.to_owned())
            .fetch_all(self.store.pool())
            .await?;
        let mut out = Vec::new();
        for r in &rows {
            out.extend(share_of(r)?);
        }
        Ok(out)
    }

    /// Creates a share, or changes the permissions of the one for the same grantee.
    pub async fn share(
        &self,
        key: &KeyRow,
        grantee: Owner,
        allow_use: bool,
        see_spend: bool,
        by: &str,
    ) -> Result<Share, AiError> {
        if matches!(key.owner, Owner::Repo(_) | Owner::Instance) {
            return Err(AiError::Invalid(
                "only a key of a user or an organisation can be shared".into(),
            ));
        }
        if matches!(grantee, Owner::Instance) || grantee == key.owner {
            return Err(AiError::Invalid(
                "a key is shared with another user, organisation or repository".into(),
            ));
        }
        let (kind, name) = grantee.columns();
        let existing = self.shares(&key.id).await?;
        if let Some(s) = existing.iter().find(|s| s.grantee == grantee) {
            sqlx::query("UPDATE ai_key_shares SET allow_use = $2, see_spend = $3 WHERE id = $1")
                .bind(s.id.clone())
                .bind(i64::from(allow_use))
                .bind(i64::from(see_spend))
                .execute(self.store.pool())
                .await?;
        } else {
            if i64::try_from(existing.len()).unwrap_or(i64::MAX) >= MAX_SHARES {
                return Err(AiError::TooMany(50));
            }
            sqlx::query(
                "INSERT INTO ai_key_shares (id, key_id, grantee_kind, grantee_name, allow_use, see_spend, created, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(rand_id("as_", 20))
            .bind(key.id.clone())
            .bind(kind)
            .bind(name)
            .bind(i64::from(allow_use))
            .bind(i64::from(see_spend))
            .bind(clock::now())
            .bind(by.to_owned())
            .execute(self.store.pool())
            .await?;
        }
        self.shares(&key.id)
            .await?
            .into_iter()
            .find(|s| s.grantee == grantee)
            .ok_or(AiError::NotFound)
    }

    pub async fn revoke_share(&self, key_id: &str, share_id: &str) -> Result<Share, AiError> {
        let share = self
            .shares(key_id)
            .await?
            .into_iter()
            .find(|s| s.id == share_id)
            .ok_or(AiError::NotFound)?;
        sqlx::query("DELETE FROM ai_key_shares WHERE id = $1 AND key_id = $2")
            .bind(share_id.to_owned())
            .bind(key_id.to_owned())
            .execute(self.store.pool())
            .await?;
        Ok(share)
    }

    /// The shares granted to `grantee` that allow use, with their keys (disabled keys left out).
    pub async fn shared_keys_for(&self, grantee: &Owner) -> Result<Vec<(Share, KeyRow)>, AiError> {
        if matches!(grantee, Owner::Instance) {
            return Ok(Vec::new());
        }
        let (kind, name) = grantee.columns();
        let q = format!(
            "SELECT {SHARE_COLS} FROM ai_key_shares WHERE grantee_kind = $1 AND grantee_name = $2 ORDER BY created, id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(kind)
            .bind(name)
            .fetch_all(self.store.pool())
            .await?;
        let mut out = Vec::new();
        for r in &rows {
            let Some(s) = share_of(r)? else { continue };
            if let Ok(k) = self.get_key(&s.key_id).await {
                out.push((s, k));
            }
        }
        Ok(out)
    }

    /// Whether `key_id` is shared for use with an owner of `chain`.
    pub async fn shared_into(&self, key_id: &str, chain: &[Owner]) -> Result<bool, AiError> {
        Ok(self
            .shares(key_id)
            .await?
            .iter()
            .any(|s| s.allow_use && chain.contains(&s.grantee)))
    }

    /// Use of `key_id` per grantee, then the owner's own use. A call counts for the most specific
    /// grantee that matches its repository (a repository before an organisation).
    pub async fn share_usage(
        &self,
        key_id: &str,
        first: &str,
        last: &str,
    ) -> Result<(Vec<(Share, ShareUse)>, ShareUse), AiError> {
        let rows = sqlx::query(
            "SELECT repo, CAST(SUM(requests) AS BIGINT) AS req, CAST(SUM(input_tokens) AS BIGINT) AS inp, \
             CAST(SUM(output_tokens) AS BIGINT) AS outp, CAST(SUM(cost_micro_usd) AS BIGINT) AS cost \
             FROM ai_usage WHERE key_id = $1 AND day >= $2 AND day <= $3 GROUP BY repo",
        )
        .bind(key_id.to_owned())
        .bind(first.to_owned())
        .bind(last.to_owned())
        .fetch_all(self.store.pool())
        .await?;
        let mut shares: Vec<(Share, ShareUse)> = self
            .shares(key_id)
            .await?
            .into_iter()
            .map(|s| (s, ShareUse::default()))
            .collect();
        shares.sort_by_key(|(s, _)| u8::from(!matches!(s.grantee, Owner::Repo(_))));
        let mut own = ShareUse::default();
        for r in &rows {
            let repo: String = r.try_get("repo")?;
            let add = ShareUse {
                requests: u(r.try_get("req")?),
                input_tokens: u(r.try_get("inp")?),
                output_tokens: u(r.try_get("outp")?),
                cost: u(r.try_get("cost")?),
            };
            let slot = match shares.iter_mut().find(|(s, _)| matches(&s.grantee, &repo)) {
                Some((_, u)) => u,
                None => &mut own,
            };
            slot.requests += add.requests;
            slot.input_tokens += add.input_tokens;
            slot.output_tokens += add.output_tokens;
            slot.cost += add.cost;
        }
        shares.sort_by(|a, b| {
            a.0.created
                .cmp(&b.0.created)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        Ok((shares, own))
    }

    // -------------------------------------------------------------------------- caps

    pub async fn caps(&self, key_id: &str) -> Result<(Caps, String), AiError> {
        let r = sqlx::query(
            "SELECT warn_percent, stop_at_cap, warned_month FROM ai_key_caps WHERE key_id = $1",
        )
        .bind(key_id.to_owned())
        .fetch_optional(self.store.pool())
        .await?;
        let Some(r) = r else {
            return Ok((Caps::default(), String::new()));
        };
        Ok((
            Caps {
                warn_percent: u32::try_from(r.try_get::<i64, _>("warn_percent")?).unwrap_or(0),
                stop_at_cap: r.try_get::<i64, _>("stop_at_cap")? != 0,
            },
            r.try_get("warned_month")?,
        ))
    }

    pub async fn set_caps(&self, key: &KeyRow, c: Caps) -> Result<(), AiError> {
        if c.warn_percent > 100 {
            return Err(AiError::Invalid("warn_percent is 0 to 100".into()));
        }
        if c.warn_percent > 0 && key.monthly_cap == 0 {
            return Err(AiError::Invalid(
                "set a monthly cap on the key before asking for a warning".into(),
            ));
        }
        sqlx::query("INSERT INTO ai_key_caps (key_id) VALUES ($1) ON CONFLICT (key_id) DO NOTHING")
            .bind(key.id.clone())
            .execute(self.store.pool())
            .await?;
        // A new percentage warns again in the same month.
        sqlx::query("UPDATE ai_key_caps SET warn_percent = $2, stop_at_cap = $3, warned_month = '' WHERE key_id = $1")
            .bind(key.id.clone())
            .bind(i64::from(c.warn_percent))
            .bind(i64::from(c.stop_at_cap))
            .execute(self.store.pool())
            .await?;
        Ok(())
    }

    /// Writes the once-a-month warning when the key's spend has crossed its warning percentage.
    /// Called after every settled call; cheap when there is nothing to do.
    pub(crate) async fn check_warn(&self, key: &KeyRow, at: i64) {
        if key.monthly_cap == 0 || !matches!(key.state, KeyState::Active) {
            return;
        }
        let Ok((caps, warned)) = self.caps(&key.id).await else {
            return;
        };
        let month = clock::month(at);
        if caps.warn_percent == 0 || warned == month {
            return;
        }
        let spent = self.month_spent(&key.id).await;
        if spent.saturating_mul(100) < key.monthly_cap.saturating_mul(u64::from(caps.warn_percent))
        {
            return;
        }
        let claimed = sqlx::query(
            "UPDATE ai_key_caps SET warned_month = $2 WHERE key_id = $1 AND warned_month <> $2",
        )
        .bind(key.id.clone())
        .bind(month)
        .execute(self.store.pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .unwrap_or(false);
        if claimed {
            let owner = match &key.owner {
                Owner::User(n) | Owner::Org(n) => n.clone(),
                _ => String::new(),
            };
            self.audit(audit::Entry {
                action: "ai.key.cap.warning",
                target: &key.id,
                owner: &owner,
                details: &[
                    ("percent", caps.warn_percent.to_string()),
                    ("scope", key.owner.label()),
                ],
                ..audit::Entry::new("ai.key.cap.warning", "system")
            })
            .await;
        }
    }
}
