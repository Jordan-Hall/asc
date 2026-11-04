//! Feature routing of AI keys (`aiproviders.proto` ListFeatures and SetFeature): per owner and
//! feature, the key the feature uses and its monthly cap.
//!
//! The most specific owner of the chain with a setting decides, as for keys: the repository, its
//! organisation or user, the instance. A setting without a key leaves the choice to
//! [`AiHub::resolve`]. The feature's spending is counted under a scope of its own in `ai_month`,
//! so its cap holds by reservation like a key's, and a feature at its cap pauses until the month
//! ends or the cap is raised.

use asc_rpc::server::wire;
use sqlx::Row;

use super::owner::Owner;
use super::store::KeyRow;
use super::{AiError, AiHub, clock, wire_key};

/// A feature that calls a model with a key of its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    AgentRuns,
}

impl Feature {
    pub const ALL: [Self; 1] = [Self::AgentRuns];

    /// The name stored in `ai_features` and used in the spending scope.
    pub fn name(self) -> &'static str {
        match self {
            Self::AgentRuns => "agent_runs",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == s)
    }

    pub fn from_wire(n: i32) -> Result<Self, AiError> {
        match wire::AiFeature::try_from(n) {
            Ok(wire::AiFeature::AgentRuns) => Ok(Self::AgentRuns),
            _ => Err(AiError::Invalid("unknown feature".into())),
        }
    }

    pub fn to_wire(self) -> wire::AiFeature {
        match self {
            Self::AgentRuns => wire::AiFeature::AgentRuns,
        }
    }
}

/// The stored setting of one feature for one owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub owner: Owner,
    pub feature: Feature,
    /// Empty: the first usable key of the chain.
    pub key_id: String,
    /// 0: no cap.
    pub monthly_cap: u64,
    pub updated: i64,
    pub updated_by: String,
}

impl Setting {
    /// The `ai_month` scope of the feature's spending under this setting.
    pub fn scope(&self) -> String {
        scope(&self.owner, self.feature)
    }
}

/// The `ai_month` scope of `f` under the setting of `owner`.
pub fn scope(owner: &Owner, f: Feature) -> String {
    format!("feature:{}:{}", owner.label(), f.name())
}

/// The owners whose settings and keys apply to `owner`, most specific first.
pub fn chain_of(owner: &Owner) -> Vec<Owner> {
    match owner {
        Owner::Repo(r) => Owner::chain(r, None),
        Owner::Instance => vec![Owner::Instance],
        o => vec![o.clone(), Owner::Instance],
    }
}

/// A feature as an owner sees it: the setting that applies, the month's spending under it and the
/// key the feature uses now.
#[derive(Clone, Debug)]
pub struct View {
    pub feature: Feature,
    pub applying: Option<Setting>,
    pub spent: u64,
    pub key: Option<KeyRow>,
}

impl View {
    /// Whether the month's spending reached the cap.
    pub fn paused(&self) -> bool {
        self.applying
            .as_ref()
            .is_some_and(|s| s.monthly_cap > 0 && self.spent >= s.monthly_cap)
    }
}

const COLS: &str = "owner_kind, owner_name, feature, key_id, monthly_cap, updated, updated_by";

fn setting_of(r: &sqlx::any::AnyRow) -> Result<Option<Setting>, sqlx::Error> {
    let owner = Owner::from_columns(
        r.try_get("owner_kind")?,
        &r.try_get::<String, _>("owner_name")?,
    );
    let feature = Feature::from_name(&r.try_get::<String, _>("feature")?);
    let (Some(owner), Some(feature)) = (owner, feature) else {
        return Ok(None);
    };
    Ok(Some(Setting {
        owner,
        feature,
        key_id: r.try_get("key_id")?,
        monthly_cap: u64::try_from(r.try_get::<i64, _>("monthly_cap")?).unwrap_or(0),
        updated: r.try_get("updated")?,
        updated_by: r.try_get("updated_by")?,
    }))
}

impl AiHub {
    /// The setting `owner` itself made for `f`.
    pub async fn own_setting(&self, owner: &Owner, f: Feature) -> Result<Option<Setting>, AiError> {
        let (kind, name) = owner.columns();
        let q = format!(
            "SELECT {COLS} FROM ai_features WHERE owner_kind = $1 AND owner_name = $2 AND feature = $3"
        );
        let r = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(kind)
            .bind(name)
            .bind(f.name())
            .fetch_optional(self.store.pool())
            .await?;
        Ok(match r {
            Some(r) => setting_of(&r)?,
            None => None,
        })
    }

    /// The setting of `f` that applies to `owner`: its own, else the nearest one up its chain.
    pub async fn applying(&self, owner: &Owner, f: Feature) -> Result<Option<Setting>, AiError> {
        for o in chain_of(owner) {
            if let Some(s) = self.own_setting(&o, f).await? {
                return Ok(Some(s));
            }
        }
        Ok(None)
    }

    /// The setting of `f` for a call made for `repo` (`owner/name`); none for a call that names no
    /// repository.
    pub async fn setting_for_repo(
        &self,
        repo: &str,
        f: Feature,
    ) -> Result<Option<Setting>, AiError> {
        if repo.is_empty() {
            return Ok(None);
        }
        self.applying(&Owner::Repo(repo.to_ascii_lowercase()), f)
            .await
    }

    /// Whether `f` is at its cap for calls made for `repo`.
    pub async fn feature_paused(&self, repo: &str, f: Feature) -> Result<bool, AiError> {
        let Some(s) = self.setting_for_repo(repo, f).await? else {
            return Ok(false);
        };
        if s.monthly_cap == 0 {
            return Ok(false);
        }
        let spent = self
            .store
            .month_spent(&s.scope(), &clock::month(clock::now()))
            .await?;
        Ok(spent >= s.monthly_cap)
    }

    /// Every feature as `owner` sees it.
    pub async fn features(&self, owner: &Owner) -> Result<Vec<View>, AiError> {
        let chain = chain_of(owner);
        let month = clock::month(clock::now());
        let mut out = Vec::with_capacity(Feature::ALL.len());
        for f in Feature::ALL {
            let applying = self.applying(owner, f).await?;
            let spent = match &applying {
                Some(s) => self.store.month_spent(&s.scope(), &month).await?,
                None => 0,
            };
            let named = applying
                .as_ref()
                .map(|s| s.key_id.as_str())
                .filter(|k| !k.is_empty());
            let key = match self.resolve(&chain, named).await {
                Ok(k) => Some(k),
                Err(AiError::NotFound) => None,
                Err(e) => return Err(e),
            };
            out.push(View {
                feature: f,
                applying,
                spent,
                key,
            });
        }
        Ok(out)
    }

    /// Sets `owner`'s setting of `f`. A key named must be one the owner can use: its own, one of
    /// an owner up its chain, or one shared with it.
    pub async fn set_feature(
        &self,
        owner: &Owner,
        f: Feature,
        key_id: &str,
        monthly_cap: u64,
        by: &str,
    ) -> Result<Setting, AiError> {
        if !key_id.is_empty() {
            self.resolve(&chain_of(owner), Some(key_id)).await?;
        }
        let (kind, name) = owner.columns();
        let now = clock::now();
        sqlx::query(
            "INSERT INTO ai_features (owner_kind, owner_name, feature, key_id, monthly_cap, updated, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (owner_kind, owner_name, feature) DO UPDATE SET \
             key_id = EXCLUDED.key_id, monthly_cap = EXCLUDED.monthly_cap, updated = EXCLUDED.updated, \
             updated_by = EXCLUDED.updated_by",
        )
        .bind(kind)
        .bind(name)
        .bind(f.name())
        .bind(key_id.to_owned())
        .bind(i64::try_from(monthly_cap).unwrap_or(i64::MAX))
        .bind(now)
        .bind(by.to_owned())
        .execute(self.store.pool())
        .await?;
        Ok(Setting {
            owner: owner.clone(),
            feature: f,
            key_id: key_id.to_owned(),
            monthly_cap,
            updated: now,
            updated_by: by.to_owned(),
        })
    }

    /// Removes `owner`'s setting of `f`, so the nearest one up its chain applies.
    pub async fn clear_feature(&self, owner: &Owner, f: Feature) -> Result<(), AiError> {
        let (kind, name) = owner.columns();
        sqlx::query(
            "DELETE FROM ai_features WHERE owner_kind = $1 AND owner_name = $2 AND feature = $3",
        )
        .bind(kind)
        .bind(name)
        .bind(f.name())
        .execute(self.store.pool())
        .await?;
        Ok(())
    }

    /// Settings that chose `key_id` go back to the first usable key; called when the key is
    /// deleted.
    pub(crate) async fn forget_key_in_features(&self, key_id: &str) -> Result<(), AiError> {
        sqlx::query("UPDATE ai_features SET key_id = '' WHERE key_id = $1")
            .bind(key_id.to_owned())
            .execute(self.store.pool())
            .await?;
        Ok(())
    }
}

/// The wire form of `v`.
pub async fn wire_feature(hub: &AiHub, v: &View) -> wire::AiFeatureSetting {
    let paused = v.paused();
    let resumes_at = if paused {
        clock::next_month_start(clock::month_start(clock::now()))
    } else {
        0
    };
    let key = match &v.key {
        Some(k) => Some(wire_key(hub, k).await),
        None => None,
    };
    let s = v.applying.as_ref();
    wire::AiFeatureSetting {
        feature: v.feature.to_wire() as i32,
        set_by: s.map(|s| s.owner.to_wire()),
        key_id: s.map(|s| s.key_id.clone()).unwrap_or_default(),
        monthly_cap_micro_usd: s.map_or(0, |s| s.monthly_cap),
        month_spent_micro_usd: v.spent,
        paused,
        resumes_at,
        key,
        updated: s.map_or(0, |s| s.updated),
        updated_by: s.map(|s| s.updated_by.clone()).unwrap_or_default(),
    }
}
