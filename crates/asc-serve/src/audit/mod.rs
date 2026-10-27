//! The audit log (docs/BUILDOUT.md section 3).
//!
//! Every security-relevant action appends one [`NewEvent`], in the transaction of the action
//! ([`store::append_in_tx`]): who did it (a principal, and the person behind an agent token),
//! what (a dotted action from the [`catalogue`]), when, from where (the network prefix and the
//! user agent of the request, [`meta`]) and with what result. An event is written to every scope
//! it belongs to: its repository, its owner and the instance. The events of one scope are
//! gap free and hash chained ([`chain`]), so [`store::verify_chain`] finds an edited or removed
//! event. Nothing updates or deletes an event except [`retention`], which writes a segment record
//! that keeps the hash of the last dropped event.
//!
//! * [`store`]: append, list, export rows, verification.
//! * [`export`]: JSON lines and CSV, chunked, with a digest.
//! * [`rpc`]: the `Audit` service and who may read which scope.
//! * [`undo`] and [`stacks_rpc`]: ref-level undo and the ref log (`Stacks.UndoRef`, `ListRefLog`).
//! * [`retention`]: the worker that drops events older than `[audit] retention_days`.
//!
//! Details of an event are built from an allow-list of keys per action; text is cut to
//! [`MAX_DETAIL_BYTES`] and a value that looks like a credential is dropped
//! ([`catalogue::sanitize_details`]), so the log never holds a secret.

pub mod actor;
pub mod catalogue;
pub mod chain;
pub mod export;
pub mod hooks;
pub mod meta;
pub mod retention;
pub mod rpc;
pub mod stacks_rpc;
pub mod store;
pub mod undo;

#[cfg(test)]
mod credential_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod undo_tests;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

pub use chain::Event;

use crate::registry::{Extensions, Registry};

/// What `Version.features` adds for the `Audit` service.
pub const FEATURE: &str = "audit.v1";
/// What `Version.features` adds for the ref log and ref undo of `Stacks`.
pub const STACKS_FEATURE: &str = "stacks.undo.v1";
/// Longest text of one detail value or target, in bytes.
pub const MAX_DETAIL_BYTES: usize = 256;
/// Longest target, in bytes.
pub const MAX_TARGET_BYTES: usize = 512;
/// Longest user agent kept, in bytes.
pub const MAX_USER_AGENT_BYTES: usize = 256;
/// Events in one `List` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 50;
/// Events in one `List` page, at most.
pub const MAX_PAGE: usize = 200;
/// Longest filter list (actors, actions).
pub const MAX_FILTER_ITEMS: usize = 32;
/// Default of `[audit] retention_days`.
pub const DEFAULT_RETENTION_DAYS: u32 = 365;
/// Default of `[audit] export_max_events`.
pub const DEFAULT_EXPORT_MAX_EVENTS: u64 = 1_000_000;
/// Default of `[audit] prune_interval_secs`.
pub const DEFAULT_PRUNE_INTERVAL_SECS: u64 = 3600;
/// Default of `[audit] undo_window_secs`.
pub const DEFAULT_UNDO_WINDOW_SECS: u64 = 600;
/// Most site administrators `[audit] admins` can name.
pub const MAX_ADMINS: usize = 256;

/// `[audit]`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuditConfig {
    /// Days an event is kept; 0 keeps everything.
    pub retention_days: u32,
    /// Most events one export holds.
    pub export_max_events: u64,
    /// Seconds between two runs of the retention worker.
    pub prune_interval_secs: u64,
    /// Seconds in which a maintainer can undo their own update of a protected ref (a land, say);
    /// 0 switches the undo of protected refs off. Refs of one's own are undone at any time.
    pub undo_window_secs: u64,
    /// User handles that may read the audit log of every scope, the instance included. The host
    /// application's service credential may read every scope as well (it has already checked its
    /// own permissions).
    pub admins: Vec<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            export_max_events: DEFAULT_EXPORT_MAX_EVENTS,
            prune_interval_secs: DEFAULT_PRUNE_INTERVAL_SECS,
            undo_window_secs: DEFAULT_UNDO_WINDOW_SECS,
            admins: Vec::new(),
        }
    }
}

impl AuditConfig {
    /// Checks the bounds. The error names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.retention_days > 36_500 {
            return Err(
                "retention_days is above 36500 (100 years); use 0 to keep everything".into(),
            );
        }
        if self.export_max_events == 0 || self.export_max_events > 100_000_000 {
            return Err("export_max_events must be 1 to 100000000".into());
        }
        if self.prune_interval_secs < 60 {
            return Err("prune_interval_secs must be at least 60".into());
        }
        if self.undo_window_secs > 86_400 {
            return Err("undo_window_secs must be at most 86400".into());
        }
        if self.admins.len() > MAX_ADMINS {
            return Err(format!("admins names more than {MAX_ADMINS} users"));
        }
        for a in &self.admins {
            if !crate::review::valid_handle(a) {
                return Err(format!("admins: {a:?} is not a user handle"));
            }
        }
        Ok(())
    }
}

/// Why an audit call failed.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The action is not in the catalogue. A programming error, never caused by a caller.
    #[error("unknown audit action {0:?}")]
    UnknownAction(String),
    /// An event field is out of bounds.
    #[error("invalid audit event: {0}")]
    Invalid(&'static str),
    #[error("audit database: {0}")]
    Sql(#[from] sqlx::Error),
    /// A stored row does not decode.
    #[error("corrupt audit row: {0}")]
    Corrupt(&'static str),
}

/// The kinds of scope an event is written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeKind {
    /// The whole server.
    Instance,
    /// A user or an organisation.
    Owner,
    /// One repository (`owner/name`).
    Repo,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Owner => "owner",
            Self::Repo => "repo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "instance" => Some(Self::Instance),
            "owner" => Some(Self::Owner),
            "repo" => Some(Self::Repo),
            _ => None,
        }
    }
}

/// A scope: a kind and a name (empty for the instance).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    pub kind: ScopeKind,
    pub name: String,
}

impl Scope {
    pub fn instance() -> Self {
        Self {
            kind: ScopeKind::Instance,
            name: String::new(),
        }
    }

    pub fn owner(name: impl Into<String>) -> Self {
        Self {
            kind: ScopeKind::Owner,
            name: name.into(),
        }
    }

    pub fn repo(name: impl Into<String>) -> Self {
        Self {
            kind: ScopeKind::Repo,
            name: name.into(),
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.name)
    }
}

/// Who acted: a kind (`user`, `agent`, `deploy`, `ci`, `host` or `system`) and a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    pub kind: String,
    pub name: String,
}

impl Actor {
    pub fn new(kind: &str, name: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            name: name.to_owned(),
        }
    }

    /// The server itself (workers, retention).
    pub fn system(name: &str) -> Self {
        Self::new("system", name)
    }

    /// `kind:name`, the form an `actors` filter matches.
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind, self.name)
    }
}

/// How an action ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// Refused by authorisation or policy.
    Denied,
    /// Allowed, then failed.
    Failed,
}

impl Outcome {
    /// The code stored and hashed; it is also the wire value of `AuditOutcome`.
    pub fn code(self) -> u8 {
        match self {
            Self::Success => 1,
            Self::Denied => 2,
            Self::Failed => 3,
        }
    }

    pub fn from_code(code: i64) -> Option<Self> {
        match code {
            1 => Some(Self::Success),
            2 => Some(Self::Denied),
            3 => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// An event to append. Build one with [`NewEvent::new`] and the `with_*` methods.
#[derive(Clone, Debug)]
pub struct NewEvent {
    pub action: String,
    pub actor: Actor,
    pub on_behalf_of: Option<Actor>,
    /// `owner/name` of the repository the action concerned.
    pub repo: Option<String>,
    /// The owner scope to write to when there is no repository (a user's own settings).
    pub owner: Option<String>,
    pub target: String,
    pub outcome: Outcome,
    pub details: Vec<(String, String)>,
    /// Request facts; the request in progress ([`meta::current`]) when `None`.
    pub meta: Option<meta::RequestMeta>,
    /// The time of the event; now when `None`.
    pub time_ms: Option<i64>,
}

impl NewEvent {
    pub fn new(action: &str, actor: Actor) -> Self {
        Self {
            action: action.to_owned(),
            actor,
            on_behalf_of: None,
            repo: None,
            owner: None,
            target: String::new(),
            outcome: Outcome::Success,
            details: Vec::new(),
            meta: None,
            time_ms: None,
        }
    }

    #[must_use]
    pub fn repo(mut self, repo: &str) -> Self {
        self.repo = Some(repo.to_owned());
        self
    }

    #[must_use]
    pub fn owner(mut self, owner: &str) -> Self {
        self.owner = Some(owner.to_owned());
        self
    }

    #[must_use]
    pub fn target(mut self, target: &str) -> Self {
        self.target = target.to_owned();
        self
    }

    #[must_use]
    pub fn outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    #[must_use]
    pub fn on_behalf_of(mut self, who: Actor) -> Self {
        self.on_behalf_of = Some(who);
        self
    }

    #[must_use]
    pub fn detail(mut self, key: &str, value: impl Into<String>) -> Self {
        self.details.push((key.to_owned(), value.into()));
        self
    }

    #[must_use]
    pub fn with_meta(mut self, meta: meta::RequestMeta) -> Self {
        self.meta = Some(meta);
        self
    }

    #[must_use]
    pub fn at(mut self, time_ms: i64) -> Self {
        self.time_ms = Some(time_ms);
        self
    }

    /// The scopes this event is written to: the repository, its owner, the instance.
    pub fn scopes(&self) -> Vec<Scope> {
        let mut out = vec![Scope::instance()];
        let owner = self
            .repo
            .as_deref()
            .and_then(|r| r.split_once('/').map(|(o, _)| o))
            .or(self.owner.as_deref());
        if let Some(o) = owner.filter(|o| !o.is_empty()) {
            out.push(Scope::owner(o));
        }
        if let Some(r) = self.repo.as_deref().filter(|r| !r.is_empty()) {
            out.push(Scope::repo(r));
        }
        out
    }
}

/// The audit state the services share.
pub struct Audit {
    pub(crate) registry: Arc<Registry>,
    pub(crate) cfg: AuditConfig,
    pub(crate) ref_log: Arc<undo::RefLog>,
    /// The whole-stack operations of `Stacks`, registered by the unit that provides them.
    pub stack_ops: stacks_rpc::StackOpsSlot,
}

impl Audit {
    pub fn new(
        registry: Arc<Registry>,
        cfg: AuditConfig,
        hooks: Arc<crate::update::PipelineHooks>,
    ) -> Self {
        let ref_log = Arc::new(undo::RefLog::new(
            registry.clone(),
            hooks,
            cfg.undo_window_secs,
        ));
        Self {
            registry,
            cfg,
            ref_log,
            stack_ops: stacks_rpc::StackOpsSlot::default(),
        }
    }

    pub fn config(&self) -> &AuditConfig {
        &self.cfg
    }

    /// Appends `event` in its own transaction (for the paths that have none, such as a refused
    /// push). A failure is logged and returned; the caller decides whether it matters.
    pub async fn record(&self, event: &NewEvent) -> Result<(), AuditError> {
        store::record(self.registry.pool(), event).await
    }
}

/// The place the server's single [`Audit`] state is registered, once.
#[derive(Default)]
pub struct AuditSlot(OnceLock<Arc<Audit>>);

impl AuditSlot {
    /// Registers `audit`; false when one is registered.
    pub fn set(&self, audit: Arc<Audit>) -> bool {
        self.0.set(audit).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<Audit>> {
        self.0.get()
    }
}

/// The unit's one line in `register_all`.
pub fn register(ext: &Extensions, registry: &Arc<Registry>, cfg: &AuditConfig) {
    let audit = Arc::new(Audit::new(
        registry.clone(),
        cfg.clone(),
        ext.pipeline.clone(),
    ));
    assert!(ext.audit.set(audit), "the audit state is registered once");
}

/// Now, in unix milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Details as the map the log keeps.
pub(crate) type Details = BTreeMap<String, String>;
