//! Bring your own AI keys (`aiproviders.proto`, docs/BUILDOUT.md section 10, docs/AGENTS.md).
//!
//! A user, an organisation, a repository or the instance stores API keys of AI providers.
//! Agent runs and workflows use the key of the repository, else of its organisation or owner,
//! else of the person who started the run, else the instance's; never another owner's.
//!
//! * The key is sealed by the vault the moment it arrives and is never returned: a key shows its
//!   last four characters and a `has_key` flag. It is opened for one provider call and dropped
//!   (`Zeroizing`). It is never in a log, an error, an audit event or a metric label.
//! * A key is used only after a test passed. A 401 or 403 from the provider later marks it failed.
//! * A custom `base_url` goes through the network guard on every use: https only, every resolved
//!   address public, the connection pinned to the vetted addresses, no redirects.
//! * The monthly cap is enforced by reservation (see [`store`]) so concurrent calls cannot pass
//!   it; the instance cap applies on top. Usage is recorded per key, day, repository, agent and
//!   model.

pub mod audit;
pub mod caller;
pub mod clock;
pub mod config;
pub mod model;
pub mod ops;
pub mod owner;
pub mod prices;
pub mod provider;
pub mod rpc;
pub mod share;
pub mod share_rpc;
pub mod store;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
pub(crate) mod share_tests;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{rpc_status, wire};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tonic::Status;
use zeroize::Zeroizing;

pub use config::AiKeysConfig;
use model::{Message, ModelError, ModelInfo, ModelRequest, ModelResponse, Usage};
use owner::{Owner, OwnerAdmin};
use prices::Prices;
use provider::{Anthropic, ModelClient, OpenAi};
use store::{AiStore, KeyRow, KeyState, Provider, ReserveError, UsageRecord};

use crate::netguard::{Guard, Shape};
use crate::vault::{Resealer, SecretContext, Vault, VaultError};

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "ai-keys.v1";
/// The `kind` of the vault contexts of AI keys.
pub const SECRET_KIND: &str = "ai-key";
/// Longest label, in bytes.
pub const MAX_LABEL: usize = 64;
/// Shortest and longest provider key, in bytes.
pub const MIN_KEY: usize = 8;
pub const MAX_KEY: usize = 512;
/// Longest model id.
pub const MAX_MODEL: usize = 200;

/// A random id: `prefix` and `n` characters of `[a-z2-7]`.
pub fn rand_id(prefix: &str, n: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut raw = vec![0u8; n];
    getrandom::fill(&mut raw).expect("random source");
    let mut s = String::with_capacity(prefix.len() + n);
    s.push_str(prefix);
    s.extend(raw.iter().map(|b| ALPHABET[usize::from(b & 31)] as char));
    s
}

// ------------------------------------------------------------------------------ errors

/// Why an AI call or key operation failed.
#[derive(Debug)]
pub enum AiError {
    NotFound,
    Invalid(String),
    Exists,
    TooMany(u32),
    /// The network guard refused the endpoint (the slug says why).
    Blocked(&'static str),
    /// The key cannot be used: `untested`, `failed` or `disabled`.
    NotUsable(&'static str),
    CapReached(&'static str),
    /// The model has no price and a cost limit applies.
    NoPrice(String),
    Vault(VaultError),
    Model(ModelError),
    InUse,
    Conflict,
    /// A field that decides where the stored credential is sent changed without a new credential
    /// in the same request: `changed` is the field and `needs` the one to supply with it.
    CredentialsRequired {
        changed: &'static str,
        needs: &'static str,
    },
    Sql(String),
}

impl From<sqlx::Error> for AiError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(store::class(&e))
    }
}

impl From<VaultError> for AiError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

fn info(path: impl Into<String>) -> ErrorInfo {
    ErrorInfo {
        paths: vec![path.into()],
        ..ErrorInfo::default()
    }
}

impl AiError {
    /// The status a client sees. Messages are fixed; none holds a key, a URL or a response body.
    pub fn status(&self) -> Status {
        match self {
            Self::NotFound => Status::not_found("no such key"),
            Self::Invalid(m) => Status::invalid_argument(m.clone()),
            Self::Exists => Status::already_exists("the owner already has a key with that label"),
            Self::TooMany(n) => {
                Status::resource_exhausted(format!("an owner may hold at most {n} keys"))
            }
            Self::Blocked(slug) => rpc_status(&RpcError::AiEndpointBlocked(info(*slug))),
            Self::NotUsable(why) => rpc_status(&RpcError::AiKeyUntested(info(*why))),
            Self::CapReached(which) => rpc_status(&RpcError::AiCapReached(info(*which))),
            Self::NoPrice(_) => Status::failed_precondition(
                "the model has no price in the price file, so a cost limit cannot be enforced; \
                 add the model to [aikeys] price_file or remove the cost limit",
            ),
            Self::Vault(VaultError::Locked(_)) => {
                rpc_status(&RpcError::VaultLocked(info("locked")))
            }
            Self::Vault(_) => Status::internal("internal error"),
            Self::Model(ModelError::Blocked(slug)) => {
                rpc_status(&RpcError::AiEndpointBlocked(info(*slug)))
            }
            Self::Model(ModelError::RateLimited { .. }) => {
                Status::resource_exhausted("the provider is rate limiting this key")
            }
            Self::Model(ModelError::Unauthorized) => {
                rpc_status(&RpcError::AiKeyUntested(info("failed")))
            }
            Self::Model(e) => Status::unavailable(e.to_string()),
            Self::InUse => Status::failed_precondition("a live agent run uses this key"),
            Self::Conflict => Status::aborted("the key changed while it was being updated; retry"),
            Self::CredentialsRequired { changed, needs } => Status::invalid_argument(format!(
                "{changed}: changing it sends the key somewhere new, so supply a new {needs} in the same request"
            )),
            Self::Sql(class) => {
                tracing::error!(class, "ai keys database error");
                Status::internal("internal error")
            }
        }
    }
}

// ------------------------------------------------------------------------------ the hub

/// New key fields, already validated.
#[derive(Debug)]
pub struct NewKey {
    pub label: String,
    pub provider: Provider,
    pub api_key: Zeroizing<String>,
    pub base_url: String,
    pub default_model: String,
    pub monthly_cap: u64,
}

/// A patch of a key; `None` leaves the field.
#[derive(Debug, Default)]
pub struct KeyPatch {
    pub label: Option<String>,
    pub api_key: Option<Zeroizing<String>>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub monthly_cap: Option<u64>,
    pub disabled: Option<bool>,
}

/// Who a call is made for, for the usage report.
#[derive(Clone, Copy, Debug)]
pub struct CallInfo<'a> {
    /// `owner/name`, or empty.
    pub repo: &'a str,
    pub agent: &'a str,
}

/// A finished call.
#[derive(Clone, Debug)]
pub struct Completion {
    pub response: ModelResponse,
    /// `None`: the model has no price.
    pub cost_micro_usd: Option<u64>,
}

/// The outcome of a key test.
#[derive(Clone, Debug)]
pub struct TestOutcome {
    pub ok: bool,
    pub latency_ms: u32,
    pub provider_status: &'static str,
    pub message: String,
    pub key: KeyRow,
}

/// The state the key services share.
pub struct AiHub {
    pub(crate) store: AiStore,
    vault: Arc<Vault>,
    guard: Guard,
    pub(crate) cfg: AiKeysConfig,
    prices: Prices,
    pub(crate) owners: Arc<dyn OwnerAdmin>,
    pub(crate) ops: ops::Ops,
    tests: Mutex<HashMap<String, VecDeque<i64>>>,
    audit: OnceLock<Arc<dyn audit::Audit>>,
}

/// The place the server's single [`AiHub`] is registered, once.
#[derive(Default)]
pub struct AiSlot(OnceLock<Arc<AiHub>>);

impl AiSlot {
    pub fn set(&self, hub: Arc<AiHub>) -> bool {
        self.0.set(hub).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<AiHub>> {
        self.0.get()
    }
}

fn printable_key(s: &str) -> bool {
    (MIN_KEY..=MAX_KEY).contains(&s.len()) && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn plain_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

impl AiHub {
    pub fn new(
        pool: sqlx::AnyPool,
        vault: Arc<Vault>,
        guard: Guard,
        cfg: AiKeysConfig,
        prices: Prices,
        owners: Arc<dyn OwnerAdmin>,
    ) -> Arc<Self> {
        let hub = Arc::new(Self {
            store: AiStore::new(pool.clone()),
            vault,
            guard,
            cfg,
            prices,
            owners,
            ops: ops::Ops::new(pool),
            tests: Mutex::new(HashMap::new()),
            audit: OnceLock::new(),
        });
        hub.vault.register(Arc::new(AiResealer {
            store: hub.store.clone(),
        }));
        hub
    }

    pub fn prices(&self) -> &Prices {
        &self.prices
    }

    /// Installs the audit sink, once. False when one is already installed.
    pub fn set_audit(&self, sink: Arc<dyn audit::Audit>) -> bool {
        self.audit.set(sink).is_ok()
    }

    /// Records an audit event of a key when a sink is installed.
    pub(crate) async fn audit(&self, entry: audit::Entry<'_>) {
        if let Some(a) = self.audit.get() {
            a.record(entry).await;
        }
    }

    fn ctx<'a>(&self, owner_label: &'a str, id: &'a str) -> SecretContext<'a> {
        SecretContext {
            kind: SECRET_KIND,
            owner: owner_label,
            id,
        }
    }

    /// Validates a base URL by resolving it through the network guard.
    async fn check_base(&self, provider: Provider, base_url: &str) -> Result<(), AiError> {
        // Only the compatible provider is pointed anywhere; the others always use their own API.
        if !base_url.is_empty() && provider != Provider::OpenAiCompatible {
            return Err(AiError::Invalid(
                "base_url: only an openai-compatible provider takes a base_url".into(),
            ));
        }
        if base_url.is_empty() {
            return if provider.default_base().is_some() {
                Ok(())
            } else {
                Err(AiError::Invalid(
                    "an OpenAI-compatible provider needs a base_url".into(),
                ))
            };
        }
        self.guard
            .pin(base_url, Shape::Url)
            .await
            .map(drop)
            .map_err(|b| AiError::Blocked(b.reason().slug()))
    }

    // -------------------------------------------------------------------------- key CRUD

    pub async fn create_key(&self, owner: &Owner, new: NewKey) -> Result<KeyRow, AiError> {
        if new.label.is_empty() || !plain_text(&new.label, MAX_LABEL) {
            return Err(AiError::Invalid("a label is 1 to 64 bytes of text".into()));
        }
        if !printable_key(&new.api_key) {
            return Err(AiError::Invalid(
                "an api_key is 8 to 512 printable ASCII characters".into(),
            ));
        }
        if !plain_text(&new.default_model, MAX_MODEL) {
            return Err(AiError::Invalid(
                "default_model is at most 200 bytes of text".into(),
            ));
        }
        if new.base_url.len() > crate::netguard::MAX_URL_LEN {
            return Err(AiError::Invalid("base_url is too long".into()));
        }
        self.check_base(new.provider, &new.base_url).await?;
        let max = i64::from(self.cfg.max_keys_per_owner);
        if self.store.count(owner).await? >= max {
            return Err(AiError::TooMany(self.cfg.max_keys_per_owner));
        }
        let id = rand_id("ak_", 20);
        let label = owner.label();
        let (key_gen, sealed) = self
            .vault
            .seal(&self.ctx(&label, &id), new.api_key.as_bytes())?;
        let now = clock::now();
        let row = KeyRow {
            id,
            owner: owner.clone(),
            label: new.label,
            provider: new.provider,
            base_url: new.base_url,
            default_model: new.default_model,
            sealed_key: sealed,
            key_gen,
            key_hint: hint(&new.api_key),
            key_version: 1,
            rev: 0,
            state: KeyState::Untested,
            disabled: false,
            monthly_cap: new.monthly_cap,
            created: now,
            updated: now,
            tested_at: 0,
            last_error: String::new(),
        };
        if !self.store.insert(&row).await? {
            return Err(AiError::Exists);
        }
        Ok(row)
    }

    pub async fn get_key(&self, id: &str) -> Result<KeyRow, AiError> {
        self.store.get(id).await?.ok_or(AiError::NotFound)
    }

    pub async fn list_keys(&self, owner: &Owner) -> Result<Vec<KeyRow>, AiError> {
        Ok(self.store.list(owner).await?)
    }

    pub async fn month_spent(&self, key_id: &str) -> u64 {
        self.store
            .month_spent(key_id, &clock::month(clock::now()))
            .await
            .unwrap_or(0)
    }

    pub async fn update_key(&self, id: &str, patch: KeyPatch) -> Result<KeyRow, AiError> {
        let mut k = self.get_key(id).await?;
        // The sealed key would otherwise be sent to a destination its owner never gave it to.
        if patch.base_url.as_ref().is_some_and(|b| *b != k.base_url) && patch.api_key.is_none() {
            return Err(AiError::CredentialsRequired {
                changed: "base_url",
                needs: "api_key",
            });
        }
        let mut reset = false;
        if let Some(label) = patch.label {
            if label.is_empty() || !plain_text(&label, MAX_LABEL) {
                return Err(AiError::Invalid("a label is 1 to 64 bytes of text".into()));
            }
            k.label = label;
        }
        if let Some(m) = patch.default_model {
            if !plain_text(&m, MAX_MODEL) {
                return Err(AiError::Invalid(
                    "default_model is at most 200 bytes of text".into(),
                ));
            }
            k.default_model = m;
        }
        if let Some(c) = patch.monthly_cap {
            k.monthly_cap = c;
        }
        if let Some(d) = patch.disabled {
            k.disabled = d;
        }
        if let Some(b) = patch.base_url
            && b != k.base_url
        {
            if b.len() > crate::netguard::MAX_URL_LEN {
                return Err(AiError::Invalid("base_url is too long".into()));
            }
            self.check_base(k.provider, &b).await?;
            k.base_url = b;
            reset = true;
        }
        if let Some(secret) = patch.api_key {
            if !printable_key(&secret) {
                return Err(AiError::Invalid(
                    "an api_key is 8 to 512 printable ASCII characters".into(),
                ));
            }
            let label = k.owner.label();
            let (g, sealed) = self
                .vault
                .seal(&self.ctx(&label, &k.id), secret.as_bytes())?;
            k.sealed_key = sealed;
            k.key_gen = g;
            k.key_hint = hint(&secret);
            reset = true;
        }
        if reset {
            k.key_version += 1;
            k.state = KeyState::Untested;
            k.last_error.clear();
            k.tested_at = 0;
        }
        k.updated = clock::now();
        let expect = k.rev;
        match self.store.replace(&k).await {
            Ok(true) => {
                k.rev = expect + 1;
                Ok(k)
            }
            Ok(false) => Err(AiError::Conflict),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => Err(AiError::Exists),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete_key(&self, id: &str) -> Result<(), AiError> {
        let live: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_runs WHERE ai_key_id = $1 AND state IN (1, 2, 3)",
        )
        .bind(id.to_owned())
        .fetch_one(self.store.pool())
        .await?;
        if live > 0 {
            return Err(AiError::InUse);
        }
        if self.store.delete(id).await? {
            Ok(())
        } else {
            Err(AiError::NotFound)
        }
    }

    // -------------------------------------------------------------------------- calling

    fn open_secret(&self, k: &KeyRow) -> Result<Zeroizing<String>, AiError> {
        let label = k.owner.label();
        let plain = self.vault.open(&self.ctx(&label, &k.id), &k.sealed_key)?;
        String::from_utf8(plain.to_vec())
            .map(Zeroizing::new)
            .map_err(|_| AiError::Vault(VaultError::Corrupt))
    }

    /// The provider key, for the one caller that must keep it out of text it stores: the secret
    /// masker of an agent run. The copy is zeroised when it drops.
    pub(crate) fn reveal(&self, k: &KeyRow) -> Result<Zeroizing<String>, AiError> {
        self.open_secret(k)
    }

    async fn client(
        &self,
        k: &KeyRow,
        secret: Zeroizing<String>,
    ) -> Result<Box<dyn ModelClient>, AiError> {
        let base = if k.base_url.is_empty() {
            k.provider
                .default_base()
                .ok_or_else(|| AiError::Invalid("the key has no base_url".into()))?
        } else {
            k.base_url.as_str()
        };
        let pinned = self
            .guard
            .pin(base, Shape::Url)
            .await
            .map_err(|b| AiError::Blocked(b.reason().slug()))?;
        let timeout = Duration::from_secs(self.cfg.completion_timeout_secs);
        let c: Box<dyn ModelClient> = match k.provider {
            Provider::Anthropic => {
                Box::new(Anthropic::new(&pinned, secret, timeout).map_err(AiError::Model)?)
            }
            Provider::OpenAi => {
                Box::new(OpenAi::new(&pinned, secret, timeout, true).map_err(AiError::Model)?)
            }
            Provider::OpenAiCompatible => {
                Box::new(OpenAi::new(&pinned, secret, timeout, false).map_err(AiError::Model)?)
            }
        };
        Ok(c)
    }

    /// The first usable key of `chain` (most specific owner first), or `explicit` when named. A
    /// named key must belong to an owner of the chain; any other id is `NotFound`.
    pub async fn resolve(
        &self,
        chain: &[Owner],
        explicit: Option<&str>,
    ) -> Result<KeyRow, AiError> {
        if let Some(id) = explicit.filter(|s| !s.is_empty()) {
            let k = self.get_key(id).await?;
            return if chain.contains(&k.owner) || self.shared_into(&k.id, chain).await? {
                Ok(k)
            } else {
                Err(AiError::NotFound)
            };
        }
        for owner in chain {
            let mut keys = self.store.list(owner).await?;
            // Keys other owners share with this one come after its own.
            keys.extend(
                self.shared_keys_for(owner)
                    .await?
                    .into_iter()
                    .filter(|(s, _)| s.allow_use)
                    .map(|(_, k)| k),
            );
            keys.retain(|k| !k.disabled);
            // A tested key first; an untested one is returned so the caller can say why it cannot run.
            keys.sort_by_key(|k| match k.state {
                KeyState::Active => 0,
                KeyState::Untested => 1,
                KeyState::Failed => 2,
            });
            if let Some(k) = keys.into_iter().next() {
                return Ok(k);
            }
        }
        Err(AiError::NotFound)
    }

    /// The model a run on `key` uses: the named one, else the key's default.
    pub fn model_for(key: &KeyRow, named: &str) -> Result<String, AiError> {
        let m = if named.is_empty() {
            key.default_model.as_str()
        } else {
            named
        };
        if m.is_empty() {
            return Err(AiError::Invalid(
                "no model: name one or set a default_model on the key".into(),
            ));
        }
        if !plain_text(m, MAX_MODEL) {
            return Err(AiError::Invalid(
                "the model name is at most 200 bytes of text".into(),
            ));
        }
        Ok(m.to_owned())
    }

    /// Whether `model` can be costed.
    pub fn priced(&self, model: &str) -> bool {
        self.prices.price(model).is_some()
    }

    pub(crate) fn estimate(req: &ModelRequest) -> Usage {
        let mut chars = req.system.len();
        for m in &req.messages {
            chars += match m {
                Message::User(t) => t.len(),
                Message::Assistant { text, tool_calls } => {
                    text.len()
                        + tool_calls
                            .iter()
                            .map(|c| c.input.to_string().len() + c.name.len())
                            .sum::<usize>()
                }
                Message::ToolResults(rs) => rs.iter().map(|r| r.content.len()).sum(),
            };
        }
        chars += req
            .tools
            .iter()
            .map(|t| t.schema.to_string().len() + t.description.len())
            .sum::<usize>();
        Usage {
            input_tokens: u64::try_from(chars / 3 + 64).unwrap_or(u64::MAX),
            output_tokens: u64::from(req.max_output_tokens),
        }
    }

    /// One model call on `key`, with the cap enforced and the usage recorded.
    ///
    /// `cost_limited` says the caller has a cost limit of its own (an agent budget), so a model
    /// without a price is refused.
    pub async fn complete(
        &self,
        key: &KeyRow,
        who: &CallInfo<'_>,
        req: &ModelRequest,
        cost_limited: bool,
        cancel: &CancellationToken,
    ) -> Result<Completion, AiError> {
        if key.disabled {
            return Err(AiError::NotUsable("disabled"));
        }
        match key.state {
            KeyState::Active => {}
            KeyState::Untested => return Err(AiError::NotUsable("untested")),
            KeyState::Failed => return Err(AiError::NotUsable("failed")),
        }
        // A key whose owner turned "stop at the cap" off keeps counting but is not stopped by its own
        // cap (the instance cap still holds); without a caps row the cap is a hard stop.
        let key_cap = match self.caps_row(&key.id).await {
            Ok(Some(c)) if !c.stop_at_cap => 0,
            _ => key.monthly_cap,
        };
        let capped = key_cap > 0 || self.cfg.instance_monthly_cap_micro_usd > 0;
        let priced = self.priced(&req.model);
        if (capped || cost_limited) && !priced {
            return Err(AiError::NoPrice(req.model.clone()));
        }
        let now = clock::now();
        let reservation = if capped {
            let worst = self
                .prices
                .cost(&req.model, Self::estimate(req))
                .unwrap_or(0);
            match self
                .store
                .reserve(
                    &key.id,
                    key_cap,
                    self.cfg.instance_monthly_cap_micro_usd,
                    worst,
                    now,
                )
                .await
            {
                Ok(r) => Some(r),
                Err(ReserveError::KeyCap) => return Err(AiError::CapReached("key")),
                Err(ReserveError::InstanceCap) => return Err(AiError::CapReached("instance")),
                Err(ReserveError::Sql(c)) => return Err(AiError::Sql(c)),
            }
        } else {
            None
        };
        let outcome = async {
            let secret = self.open_secret(key)?;
            let client = self.client(key, secret).await?;
            client.complete(req, cancel).await.map_err(AiError::Model)
        }
        .await;
        let at = clock::now();
        let (actual, record) = match &outcome {
            Ok(r) => {
                let cost = self.prices.cost(&req.model, r.usage);
                (cost.unwrap_or(0), Some((r.usage, cost)))
            }
            Err(_) => (0, None),
        };
        if let Some(res) = reservation
            && let Err(e) = self.store.settle(res, &key.id, actual, at).await
        {
            tracing::error!(class = %store::class(&e), "ai cap settle failed");
        }
        if let Some((usage, cost)) = record {
            let rec = UsageRecord {
                key_id: &key.id,
                owner: &key.owner,
                repo: who.repo,
                agent: who.agent,
                model: &req.model,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cost_micro_usd: cost,
            };
            if let Err(e) = self.store.record(&rec, at).await {
                tracing::error!(class = %store::class(&e), "ai usage record failed");
            }
            if !capped {
                // The key's month is shown in the report even when no cap is set.
                self.add_spent(&key.id, actual, at).await;
            }
            self.check_warn(key, at).await;
        }
        match outcome {
            Ok(response) => Ok(Completion {
                response,
                cost_micro_usd: record.and_then(|(_, c)| c),
            }),
            Err(AiError::Model(ModelError::Unauthorized)) => {
                let _ = self
                    .store
                    .mark(
                        &key.id,
                        key.key_version,
                        KeyState::Failed,
                        "unauthorized",
                        false,
                    )
                    .await;
                Err(AiError::Model(ModelError::Unauthorized))
            }
            Err(e) => Err(e),
        }
    }

    async fn add_spent(&self, key_id: &str, amount: u64, at: i64) {
        let month = clock::month(at);
        let r = async {
            sqlx::query("INSERT INTO ai_month (scope, month) VALUES ($1, $2) ON CONFLICT (scope, month) DO NOTHING")
                .bind(key_id.to_owned())
                .bind(month.clone())
                .execute(self.store.pool())
                .await?;
            sqlx::query("UPDATE ai_month SET spent = spent + $3 WHERE scope = $1 AND month = $2")
                .bind(key_id.to_owned())
                .bind(month)
                .bind(i64::try_from(amount).unwrap_or(i64::MAX))
                .execute(self.store.pool())
                .await
        }
        .await;
        if let Err(e) = r {
            tracing::error!(class = %store::class(&e), "ai month update failed");
        }
    }

    // -------------------------------------------------------------------------- testing

    fn test_allowed(&self, key_id: &str, now: i64) -> bool {
        let mut map = self.tests.lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() > 4096 {
            map.retain(|_, q| q.back().is_some_and(|t| now - t < 3600));
        }
        let q = map.entry(key_id.to_owned()).or_default();
        while q.front().is_some_and(|t| now - t >= 3600) {
            q.pop_front();
        }
        if q.len() >= self.cfg.test_per_key_per_hour as usize {
            return false;
        }
        q.push_back(now);
        true
    }

    /// One minimal request to the provider: a one-token completion when the key has a default
    /// model (which also checks the model), else a model listing. Rate limited per key.
    pub async fn test_key(&self, id: &str) -> Result<TestOutcome, AiError> {
        let key = self.get_key(id).await?;
        let started = std::time::Instant::now();
        let now = clock::now();
        if !self.test_allowed(id, now) {
            return Err(AiError::CapReached("test_rate"));
        }
        let cancel = CancellationToken::new();
        let result: Result<(), AiError> = async {
            let secret = self.open_secret(&key)?;
            let client = self.client(&key, secret).await?;
            if key.default_model.is_empty() {
                client
                    .list_models(&cancel)
                    .await
                    .map(drop)
                    .map_err(AiError::Model)
            } else {
                let req = ModelRequest {
                    model: key.default_model.clone(),
                    system: String::new(),
                    messages: vec![Message::User("ping".into())],
                    tools: Vec::new(),
                    max_output_tokens: 1,
                };
                let r = client
                    .complete(&req, &cancel)
                    .await
                    .map_err(AiError::Model)?;
                let cost = self.prices.cost(&req.model, r.usage);
                let rec = UsageRecord {
                    key_id: &key.id,
                    owner: &key.owner,
                    repo: "",
                    agent: "test",
                    model: &req.model,
                    input_tokens: r.usage.input_tokens,
                    output_tokens: r.usage.output_tokens,
                    cost_micro_usd: cost,
                };
                let at = clock::now();
                let _ = self.store.record(&rec, at).await;
                self.add_spent(&key.id, cost.unwrap_or(0), at).await;
                Ok(())
            }
        }
        .await;
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (ok, status, message, state, error): (
            bool,
            &'static str,
            String,
            Option<KeyState>,
            String,
        ) = match &result {
            Ok(()) => (
                true,
                "ok",
                "the provider accepted the key".into(),
                Some(KeyState::Active),
                String::new(),
            ),
            Err(AiError::Model(e)) => {
                let st = if matches!(e, ModelError::Unauthorized) {
                    Some(KeyState::Failed)
                } else {
                    None
                };
                (false, e.slug(), e.to_string(), st, e.slug().to_owned())
            }
            Err(AiError::Blocked(slug)) => (
                false,
                "blocked_address",
                format!("the endpoint is not allowed: {slug}"),
                None,
                "blocked_address".into(),
            ),
            Err(AiError::Vault(VaultError::Locked(_))) => {
                return Err(AiError::Vault(VaultError::Locked("vault locked".into())));
            }
            Err(AiError::Vault(_)) => (
                false,
                "bad_response",
                "the stored key could not be opened".into(),
                None,
                "vault".into(),
            ),
            Err(e) => {
                return Err(match e {
                    AiError::Sql(c) => AiError::Sql(c.clone()),
                    AiError::Invalid(m) => AiError::Invalid(m.clone()),
                    _ => AiError::NotFound,
                });
            }
        };
        match state {
            Some(s) => {
                self.store
                    .mark(&key.id, key.key_version, s, &error, true)
                    .await?
            }
            None => {
                // Keep the state; remember when and why the test failed.
                sqlx::query("UPDATE ai_keys SET last_error = $2, tested_at = $3 WHERE id = $1 AND key_version = $4")
                    .bind(key.id.clone())
                    .bind(error)
                    .bind(clock::now())
                    .bind(i64::from(key.key_version))
                    .execute(self.store.pool())
                    .await?;
            }
        }
        let key = self.get_key(id).await?;
        Ok(TestOutcome {
            ok,
            latency_ms,
            provider_status: status,
            message,
            key,
        })
    }

    /// The models a key can use.
    pub async fn list_models(&self, id: &str) -> Result<Vec<ModelInfo>, AiError> {
        let key = self.get_key(id).await?;
        if key.disabled || key.state != KeyState::Active {
            return Err(AiError::NotUsable(if key.disabled {
                "disabled"
            } else if key.state == KeyState::Failed {
                "failed"
            } else {
                "untested"
            }));
        }
        let secret = self.open_secret(&key)?;
        let client = self.client(&key, secret).await?;
        client
            .list_models(&CancellationToken::new())
            .await
            .map_err(AiError::Model)
    }

    /// Returns reservations a crashed process left behind; called by a periodic worker.
    pub async fn sweep(&self) -> u64 {
        self.store.sweep(clock::now()).await.unwrap_or(0)
    }

    /// Starts the periodic sweep of stale cap reservations.
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            loop {
                tick.tick().await;
                hub.sweep().await;
            }
        })
    }
}

/// The last four characters of a key.
fn hint(key: &str) -> String {
    let n = key.chars().count();
    key.chars().skip(n.saturating_sub(4)).collect()
}

/// Re-seals AI keys when the vault rotates its master key.
struct AiResealer {
    store: AiStore,
}

#[async_trait]
impl Resealer for AiResealer {
    fn kind(&self) -> &'static str {
        SECRET_KIND
    }

    async fn counts(&self) -> Result<Vec<(u32, u64)>, VaultError> {
        self.store
            .counts_by_generation()
            .await
            .map_err(|e| VaultError::Sql(store::class(&e)))
    }

    async fn reseal_batch(
        &self,
        vault: &Vault,
        target: u32,
        limit: usize,
    ) -> Result<usize, VaultError> {
        let rows = self
            .store
            .not_under(target, limit)
            .await
            .map_err(|e| VaultError::Sql(store::class(&e)))?;
        let n = rows.len();
        for (id, owner, sealed) in rows {
            let label = owner.label();
            let ctx = SecretContext {
                kind: SECRET_KIND,
                owner: &label,
                id: &id,
            };
            // A value already under the active generation only has its column corrected.
            let (generation, blob) = vault
                .reseal(&ctx, &sealed)?
                .unwrap_or((target, sealed.clone()));
            self.store
                .replace_sealed(&id, &sealed, &blob, generation)
                .await
                .map_err(|e| VaultError::Sql(store::class(&e)))?;
        }
        Ok(n)
    }
}

/// `AiKey` for a row, with the month's spending filled in.
pub async fn wire_key(hub: &AiHub, k: &KeyRow) -> wire::AiKey {
    k.to_wire(hub.month_spent(&k.id).await)
}
