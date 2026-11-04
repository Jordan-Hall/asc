//! Feature routing of AI keys (`aiproviders.proto` ListFeatures and SetFeature): per owner and
//! feature, the key the feature uses and its monthly cap.
//!
//! The most specific owner of the chain with a setting chooses the key, as for keys: the
//! repository, its organisation or user, the instance. A setting without a key leaves the choice
//! to [`AiHub::resolve`]. Every setting of the chain counts the feature's spending under a scope of
//! its own in `ai_month` and its cap holds by reservation like a key's, so a repository's setting
//! cannot lift its owner's cap. A feature at any of its caps pauses until the month ends or that
//! cap is raised.

use asc_rpc::server::wire;
use sqlx::Row;

use super::owner::Owner;
use super::store::KeyRow;
use super::{AiError, AiHub, clock, wire_key};

/// A feature that calls a model with a key of its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    AgentRuns,
    ReviewSummaries,
}

impl Feature {
    pub const ALL: [Self; 2] = [Self::AgentRuns, Self::ReviewSummaries];

    /// The name stored in `ai_features` and used in the spending scope.
    pub fn name(self) -> &'static str {
        match self {
            Self::AgentRuns => "agent_runs",
            Self::ReviewSummaries => "review_summaries",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == s)
    }

    pub fn from_wire(n: i32) -> Result<Self, AiError> {
        match wire::AiFeature::try_from(n) {
            Ok(wire::AiFeature::AgentRuns) => Ok(Self::AgentRuns),
            Ok(wire::AiFeature::ReviewSummaries) => Ok(Self::ReviewSummaries),
            _ => Err(AiError::Invalid("unknown feature".into())),
        }
    }

    pub fn to_wire(self) -> wire::AiFeature {
        match self {
            Self::AgentRuns => wire::AiFeature::AgentRuns,
            Self::ReviewSummaries => wire::AiFeature::ReviewSummaries,
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

/// A feature as an owner sees it: the setting that applies, the month's spending under it, the
/// owner whose cap stops it and the key the feature uses now.
#[derive(Clone, Debug)]
pub struct View {
    pub feature: Feature,
    pub applying: Option<Setting>,
    pub spent: u64,
    pub paused_by: Option<Owner>,
    pub key: Option<KeyRow>,
}

impl View {
    /// Whether the month's spending reached a cap of the chain.
    pub fn paused(&self) -> bool {
        self.paused_by.is_some()
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

    /// Every setting of `f` up `owner`'s chain, most specific first: the first chooses the key,
    /// and the cap of each holds.
    pub async fn settings_up(&self, owner: &Owner, f: Feature) -> Result<Vec<Setting>, AiError> {
        let mut out = Vec::new();
        for o in chain_of(owner) {
            if let Some(s) = self.own_setting(&o, f).await? {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// The setting of `f` that applies to `owner`: its own, else the nearest one up its chain.
    pub async fn applying(&self, owner: &Owner, f: Feature) -> Result<Option<Setting>, AiError> {
        Ok(self.settings_up(owner, f).await?.into_iter().next())
    }

    /// Every setting of `f` for a call made for `repo` (`owner/name`), most specific first; none
    /// for a call that names no repository.
    pub async fn settings_for_repo(&self, repo: &str, f: Feature) -> Result<Vec<Setting>, AiError> {
        if repo.is_empty() {
            return Ok(Vec::new());
        }
        self.settings_up(&Owner::Repo(repo.to_ascii_lowercase()), f)
            .await
    }

    /// The setting of `f` that chooses the key for a call made for `repo`.
    pub async fn setting_for_repo(
        &self,
        repo: &str,
        f: Feature,
    ) -> Result<Option<Setting>, AiError> {
        Ok(self.settings_for_repo(repo, f).await?.into_iter().next())
    }

    /// The owner of the first of `settings` whose cap the month's spending reached.
    async fn capped_by(&self, settings: &[Setting]) -> Result<Option<Owner>, AiError> {
        let month = clock::month(clock::now());
        for s in settings.iter().filter(|s| s.monthly_cap > 0) {
            if self.store.month_spent(&s.scope(), &month).await? >= s.monthly_cap {
                return Ok(Some(s.owner.clone()));
            }
        }
        Ok(None)
    }

    /// Whether `f` is at any of its caps for calls made for `repo`.
    pub async fn feature_paused(&self, repo: &str, f: Feature) -> Result<bool, AiError> {
        let settings = self.settings_for_repo(repo, f).await?;
        Ok(self.capped_by(&settings).await?.is_some())
    }

    /// Every feature as `owner` sees it.
    pub async fn features(&self, owner: &Owner) -> Result<Vec<View>, AiError> {
        let chain = chain_of(owner);
        let month = clock::month(clock::now());
        let mut out = Vec::with_capacity(Feature::ALL.len());
        for f in Feature::ALL {
            let settings = self.settings_up(owner, f).await?;
            let paused_by = self.capped_by(&settings).await?;
            let applying = settings.into_iter().next();
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
                paused_by,
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

/// The wire form of `v` for `viewer`, the owner whose features were asked for. A key of another
/// owner is only a reference: the viewer may use it but does not administer it, so its base URL,
/// last error, spending and creator stay with its owner.
pub async fn wire_feature(hub: &AiHub, v: &View, viewer: &Owner) -> wire::AiFeatureSetting {
    let paused = v.paused();
    let resumes_at = if paused {
        clock::next_month_start(clock::month_start(clock::now()))
    } else {
        0
    };
    let key = match &v.key {
        Some(k) if k.owner == *viewer => Some(wire_key(hub, k).await),
        Some(k) => Some(key_reference(hub, k).await),
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
        paused_by: v.paused_by.as_ref().map(Owner::to_wire),
    }
}

/// What an owner that may use `k` but does not own it sees of it.
async fn key_reference(hub: &AiHub, k: &KeyRow) -> wire::AiKey {
    let full = wire_key(hub, k).await;
    wire::AiKey {
        id: full.id,
        owner: full.owner,
        label: full.label,
        provider: full.provider,
        has_key: full.has_key,
        state: full.state,
        ..wire::AiKey::default()
    }
}
