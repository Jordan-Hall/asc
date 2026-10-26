//! The storage operations, independent of the wire: what each `Storage` RPC does after the
//! request is parsed. `rpc.rs` is the thin layer that parses, calls and converts.
//!
//! Rules that hold for every method here:
//!
//! * authorisation is the first thing a method decides, and a "no" is `PermissionDenied` when
//!   the call names an owner or a repository and `NotFound` when it names an id;
//! * a secret goes in once ([`Credentials`]) and never comes out;
//! * every mutation records an [`AuditEvent`];
//! * an error carries a fixed phrase, never provider text or the endpoint.

use std::sync::Arc;

use serde::Deserialize;

use super::audit::{Audit, AuditEvent};
use super::authority::{Authority, Caller};
use super::db::{Db, MigrationRow, NewMigration, NewTarget};
use super::live::{self, Builder, Report, RouteError, Router};
use super::migrate::{Engine, state};
use super::model::{
    self, Addressing, Credentials, Owner, OwnerKind, State, Target, check_bucket, check_name,
    check_prefix, check_region, is_id, secret_context,
};
use crate::netguard::{BlockReason, Guard, Shape};
use crate::vault::{Vault, VaultError};

/// `[storage]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    /// Targets one owner may hold.
    pub max_targets_per_owner: u32,
    /// Repositories one target may serve.
    pub max_repos_per_target: u32,
    /// Whether a user (not an organisation or a repository) may add targets.
    pub allow_user_targets: bool,
    /// Seconds between health checks of enabled targets; 0 turns the checks off.
    pub health_interval_secs: u64,
    /// Seconds before a target's DNS answer is judged again.
    pub repin_secs: u64,
    /// Seconds replicas are given to notice a route change during a migration.
    pub migration_settle_secs: u64,
    /// Migrations that run at once on one server.
    pub migration_workers: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_targets_per_owner: model::DEFAULT_MAX_TARGETS,
            max_repos_per_target: 1000,
            allow_user_targets: true,
            health_interval_secs: 300,
            repin_secs: 300,
            migration_settle_secs: 5,
            migration_workers: 2,
        }
    }
}

impl StorageConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_targets_per_owner == 0 || self.max_targets_per_owner > 10_000 {
            return Err("storage.max_targets_per_owner must be 1 to 10000".into());
        }
        if self.max_repos_per_target == 0 {
            return Err("storage.max_repos_per_target must be positive".into());
        }
        if self.repin_secs == 0 || self.migration_workers == 0 || self.migration_workers > 64 {
            return Err("storage.repin_secs and storage.migration_workers must be positive (workers at most 64)".into());
        }
        if self.health_interval_secs != 0 && self.health_interval_secs < 10 {
            return Err("storage.health_interval_secs must be 0 or at least 10".into());
        }
        Ok(())
    }
}

/// Why an operation failed. Each maps to one status (`into_status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    InvalidArgument(String),
    Blocked(BlockReason),
    NotFound(&'static str),
    PermissionDenied(&'static str),
    AlreadyExists(&'static str),
    VerifyFailed(String),
    InUse(String),
    MigrationActive,
    Quota(String),
    RateLimited,
    VaultLocked(String),
    FailedPrecondition(String),
    Unavailable(String),
    /// The update moves the target (`field` changed) but brings no new credentials, so the stored
    /// ones would be sent to a destination their owner never chose.
    CredentialsRequired { field: &'static str },
    Internal,
}

impl From<sqlx::Error> for StorageError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "storage database error");
        Self::Internal
    }
}

impl From<VaultError> for StorageError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::Locked(why) => Self::VaultLocked(why),
            VaultError::Corrupt => Self::VaultLocked("a stored secret does not open with this key file".into()),
            other => {
                tracing::error!(error = %other, "vault error");
                Self::Internal
            }
        }
    }
}

impl From<RouteError> for StorageError {
    fn from(e: RouteError) -> Self {
        match e {
            RouteError::Blocked(r) => Self::Blocked(r),
            RouteError::Vault(v) => v.into(),
            RouteError::NotEnabled(_) | RouteError::Gone => Self::FailedPrecondition(e.to_string()),
            RouteError::Other(m) => Self::Unavailable(m),
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A target with the facts the API shows beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub target: Target,
    pub repos: u32,
}

/// What `UpdateTarget` may change. `None` leaves the field.
#[derive(Default)]
pub struct Patch {
    pub name: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub addressing: Option<Addressing>,
    pub credentials: Option<Credentials>,
}

/// The new fields of a target.
pub struct NewFields {
    pub owner: Owner,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub addressing: Addressing,
    pub credentials: Credentials,
}

/// The storage operations.
pub struct Service {
    pub db: Db,
    pub vault: Arc<Vault>,
    pub guard: Guard,
    pub router: Arc<Router>,
    pub engine: Arc<Engine>,
    pub audit: Arc<dyn Audit>,
    pub authority: Arc<dyn Authority>,
    pub cfg: StorageConfig,
}

impl Service {
    fn builder(&self) -> &Builder {
        &self.router.builder
    }

    fn event(&self, action: &str, caller: &Caller) -> AuditEvent {
        AuditEvent::new(action).actor(caller.actor())
    }

    async fn view(&self, target: Target) -> Result<View, StorageError> {
        let repos = self.db.repos_assigned(&target.id).await?;
        Ok(View {
            target,
            repos: u32::try_from(repos).unwrap_or(u32::MAX),
        })
    }

    /// A target the caller administers, or `NotFound` (never `PermissionDenied`).
    async fn owned(&self, caller: &Caller, id: &str) -> Result<Target, StorageError> {
        if !is_id("st_", id) {
            return Err(StorageError::NotFound("no such storage target"));
        }
        let t = self
            .db
            .target(id)
            .await?
            .ok_or(StorageError::NotFound("no such storage target"))?;
        if !self.authority.can_admin_owner(caller, &t.owner).await {
            return Err(StorageError::NotFound("no such storage target"));
        }
        Ok(t)
    }

    /// Refuses a caller who does not administer `owner`. The RPC layer runs this before it
    /// looks up a stored answer, so an answer never reaches a caller the call would refuse.
    pub async fn authorize_owner(&self, caller: &Caller, owner: &Owner) -> Result<(), StorageError> {
        if self.authority.can_admin_owner(caller, owner).await {
            Ok(())
        } else {
            Err(StorageError::PermissionDenied("you do not administer this owner"))
        }
    }

    /// As [`Self::authorize_owner`], for the owner of target `id` (`NotFound` otherwise).
    pub async fn authorize_target(&self, caller: &Caller, id: &str) -> Result<(), StorageError> {
        self.owned(caller, id).await.map(drop)
    }

    /// As [`Self::authorize_owner`], for the administrator of repository `repo`.
    pub async fn authorize_repo(&self, caller: &Caller, repo: &str) -> Result<(), StorageError> {
        self.repo_row(caller, repo).await.map(drop)
    }

    /// Resolves `endpoint` for `bucket` with the guard; returns the normalised endpoint.
    async fn check_endpoint(
        &self,
        endpoint: &str,
        bucket: &str,
        region: &str,
        addressing: Addressing,
    ) -> Result<String, StorageError> {
        let parsed = self
            .guard
            .parse_endpoint(endpoint, Shape::Endpoint)
            .map_err(|b| StorageError::Blocked(b.reason()))?;
        let normalised = parsed.as_string();
        let cfg = asc_store::s3::TargetConfig {
            endpoint: normalised.clone(),
            region: region.to_owned(),
            bucket: bucket.to_owned(),
            prefix: String::new(),
            addressing: addressing.to_store(),
            credentials: asc_store::s3::TargetCredentials {
                access_key_id: String::new(),
                secret_access_key: String::new(),
                session_token: None,
            },
            allow_http: false,
        };
        let effective = asc_store::s3::effective_endpoint(&cfg)
            .map_err(|_| StorageError::InvalidArgument("the endpoint and bucket do not form a host name".into()))?;
        self.guard
            .pin(&effective, Shape::Endpoint)
            .await
            .map_err(|b| StorageError::Blocked(b.reason()))?;
        Ok(normalised)
    }

    // ------------------------------------------------------------------ targets

    pub async fn create_target(&self, caller: &Caller, f: NewFields) -> Result<View, StorageError> {
        if !self.authority.can_admin_owner(caller, &f.owner).await {
            self.audit.record(
                self.event("storage.target.created", caller)
                    .owner(&f.owner.scope())
                    .outcome("denied"),
            );
            return Err(StorageError::PermissionDenied("you do not administer this owner"));
        }
        if f.owner.kind == OwnerKind::User && !self.cfg.allow_user_targets {
            return Err(StorageError::PermissionDenied(
                "this server does not allow storage targets owned by a user",
            ));
        }
        check_name(&f.name).map_err(StorageError::InvalidArgument)?;
        check_bucket(&f.bucket).map_err(StorageError::InvalidArgument)?;
        let region = check_region(&f.region).map_err(StorageError::InvalidArgument)?;
        let prefix = check_prefix(&f.prefix).map_err(StorageError::InvalidArgument)?;
        if f.owner.kind == OwnerKind::Repo && self.db.repo(&f.owner.name).await?.is_none() {
            return Err(StorageError::NotFound("no such repository"));
        }
        let endpoint = self
            .check_endpoint(&f.endpoint, &f.bucket, &region, f.addressing)
            .await?;
        if self.db.count_targets(&f.owner).await? >= i64::from(self.cfg.max_targets_per_owner) {
            return Err(StorageError::Quota(format!(
                "an owner may have at most {} storage targets",
                self.cfg.max_targets_per_owner
            )));
        }
        let id = model::new_id("st_");
        let packed = f.credentials.pack().map_err(StorageError::InvalidArgument)?;
        let scope = f.owner.scope();
        let (key_gen, sealed) = self.vault.seal(&secret_context(&scope, &id), &packed)?;
        let hint = f.credentials.hint();
        let t = now();
        let inserted = self
            .db
            .insert_target(&NewTarget {
                id: &id,
                owner: &f.owner,
                name: &f.name,
                endpoint: &endpoint,
                region: &region,
                bucket: &f.bucket,
                prefix: &prefix,
                addressing: f.addressing,
                hint: &hint,
                sealed: &sealed,
                key_gen,
                now: t,
            })
            .await?;
        if !inserted {
            return Err(StorageError::AlreadyExists("this owner already has a target with that name"));
        }
        self.audit.record(
            self.event("storage.target.created", caller)
                .owner(&scope)
                .target(&id)
                .detail("name", &f.name)
                .detail("bucket", &f.bucket),
        );
        let target = self.db.target(&id).await?.ok_or(StorageError::Internal)?;
        self.view(target).await
    }

    pub async fn list_targets(&self, caller: &Caller, owner: &Owner) -> Result<Vec<View>, StorageError> {
        if !self.authority.can_admin_owner(caller, owner).await {
            return Err(StorageError::PermissionDenied("you do not administer this owner"));
        }
        let mut out = Vec::new();
        for t in self.db.targets_of(owner).await? {
            out.push(self.view(t).await?);
        }
        Ok(out)
    }

    pub async fn get_target(&self, caller: &Caller, id: &str) -> Result<View, StorageError> {
        let t = self.owned(caller, id).await?;
        self.view(t).await
    }

    pub async fn update_target(&self, caller: &Caller, id: &str, p: Patch) -> Result<View, StorageError> {
        let old = self.owned(caller, id).await?;
        let mut new = old.clone();
        if let Some(n) = p.name {
            check_name(&n).map_err(StorageError::InvalidArgument)?;
            new.name = n;
        }
        if let Some(b) = p.bucket {
            check_bucket(&b).map_err(StorageError::InvalidArgument)?;
            new.bucket = b;
        }
        if let Some(r) = p.region {
            new.region = check_region(&r).map_err(StorageError::InvalidArgument)?;
        }
        if let Some(x) = p.prefix {
            new.prefix = check_prefix(&x).map_err(StorageError::InvalidArgument)?;
        }
        if let Some(a) = p.addressing {
            new.addressing = a;
        }
        if let Some(e) = p.endpoint {
            new.endpoint = e;
        }
        // The stored credentials were given for the old destination. Pointing them at a new one
        // needs credentials in the same request, or an administrator could lift the secret by
        // redirecting the target to a server they run.
        let moved = [
            ("endpoint", new.endpoint != old.endpoint),
            ("bucket", new.bucket != old.bucket),
            ("region", new.region != old.region),
            ("addressing", new.addressing != old.addressing),
        ]
        .into_iter()
        .find_map(|(field, changed)| changed.then_some(field));
        if let (Some(field), None) = (moved, &p.credentials) {
            return Err(StorageError::CredentialsRequired { field });
        }
        let connection_changed = new.endpoint != old.endpoint
            || new.region != old.region
            || new.bucket != old.bucket
            || new.prefix != old.prefix
            || new.addressing != old.addressing
            || p.credentials.is_some();
        if connection_changed {
            new.endpoint = self
                .check_endpoint(&new.endpoint, &new.bucket, &new.region, new.addressing)
                .await?;
        }
        let sealed = match &p.credentials {
            Some(c) => {
                let packed = c.pack().map_err(StorageError::InvalidArgument)?;
                let scope = old.owner.scope();
                let (g, blob) = self.vault.seal(&secret_context(&scope, &old.id), &packed)?;
                Some((blob, g, c.hint()))
            }
            None => None,
        };
        if connection_changed {
            new.was_enabled = false;
        }
        let applied = self
            .db
            .update_target(
                &old,
                &new,
                sealed.as_ref().map(|(b, g, h)| (b.as_slice(), *g, h.as_str())),
                connection_changed,
                now(),
            )
            .await?;
        if !applied {
            return Err(StorageError::AlreadyExists(
                "the target changed meanwhile, or the name is taken; read it and retry",
            ));
        }
        self.router.invalidate(&old.id);
        self.audit.record(
            self.event("storage.target.updated", caller)
                .owner(&old.owner.scope())
                .target(&old.id)
                .detail("connection_changed", if connection_changed { "true" } else { "false" })
                .detail("credentials_replaced", if p.credentials.is_some() { "true" } else { "false" }),
        );
        let t = self.db.target(&old.id).await?.ok_or(StorageError::Internal)?;
        self.view(t).await
    }

    pub async fn delete_target(&self, caller: &Caller, id: &str) -> Result<(), StorageError> {
        let t = self.owned(caller, id).await?;
        if self.db.repos_using(id).await? > 0 {
            return Err(StorageError::InUse("repositories still keep objects in this target".into()));
        }
        if self.db.active_migrations_on(id).await? > 0 {
            return Err(StorageError::InUse("a migration involves this target".into()));
        }
        self.db.delete_target(id).await?;
        self.router.invalidate(id);
        self.audit.record(
            self.event("storage.target.deleted", caller)
                .owner(&t.owner.scope())
                .target(id),
        );
        Ok(())
    }

    /// The handshake, rate limited to six a target an hour.
    pub async fn verify_target(&self, caller: &Caller, id: &str) -> Result<(Report, View), StorageError> {
        let t = self.owned(caller, id).await?;
        if !self.db.take_verify_slot(id, now()).await? {
            return Err(StorageError::RateLimited);
        }
        let sealed = self.db.sealed(id).await?.ok_or(StorageError::NotFound("no such storage target"))?;
        let report = live::probe(self.builder(), &t, &sealed).await?;
        let ok = report.ok();
        let (state, was_enabled) = if ok {
            match t.state {
                State::Enabled | State::Failed if t.was_enabled || t.state == State::Enabled => (State::Enabled, true),
                State::Disabled => (State::Disabled, false),
                _ => (State::Verified, false),
            }
        } else {
            match t.state {
                State::Disabled => (State::Disabled, false),
                State::Enabled => (State::Failed, true),
                State::Failed => (State::Failed, t.was_enabled),
                _ => (State::Failed, false),
            }
        };
        let last_error = report.failure().unwrap_or_default();
        self.db
            .set_state(
                id,
                state,
                Some(was_enabled),
                &last_error,
                ok.then(now),
                now(),
            )
            .await?;
        self.router.invalidate(id);
        self.audit.record(
            self.event("storage.target.verified", caller)
                .owner(&t.owner.scope())
                .target(id)
                .outcome(if ok { "ok" } else { "error" })
                .detail("result", state.as_str()),
        );
        let fresh = self.db.target(id).await?.ok_or(StorageError::Internal)?;
        Ok((report, self.view(fresh).await?))
    }

    pub async fn set_enabled(&self, caller: &Caller, id: &str, enabled: bool) -> Result<View, StorageError> {
        let t = self.owned(caller, id).await?;
        if enabled {
            match t.state {
                State::Enabled => {}
                State::Verified => {
                    self.db.set_state(id, State::Enabled, Some(true), "", None, now()).await?;
                }
                State::Disabled if t.verified_at > 0 => {
                    self.db.set_state(id, State::Enabled, Some(true), "", None, now()).await?;
                }
                _ => {
                    return Err(StorageError::VerifyFailed(
                        "only a verified target can be enabled; run the verification first".into(),
                    ));
                }
            }
        } else if t.state != State::Disabled {
            if self.db.repos_using(id).await? > 0 {
                return Err(StorageError::InUse(
                    "repositories keep objects in this target; move them before disabling it".into(),
                ));
            }
            self.db.set_state(id, State::Disabled, Some(false), "", None, now()).await?;
        }
        self.router.invalidate(id);
        self.audit.record(
            self.event(if enabled { "storage.target.enabled" } else { "storage.target.updated" }, caller)
                .owner(&t.owner.scope())
                .target(id)
                .detail("enabled", if enabled { "true" } else { "false" }),
        );
        let t = self.db.target(id).await?.ok_or(StorageError::Internal)?;
        self.view(t).await
    }

    // ------------------------------------------------------------------ repositories

    async fn repo_row(&self, caller: &Caller, repo: &str) -> Result<(i64, crate::registry::Ns), StorageError> {
        let name = asc_rpc::server::RepoName::parse(repo)
            .map_err(|_| StorageError::InvalidArgument("invalid repository name".into()))?;
        let Some((id, ns)) = self.db.repo(name.as_str()).await? else {
            return Err(StorageError::NotFound("no such repository"));
        };
        if !self.authority.can_admin_repo(caller, name.as_str()).await {
            return Err(StorageError::PermissionDenied("you do not administer this repository"));
        }
        let ns = crate::registry::Ns::from_bytes(&ns).ok_or(StorageError::Internal)?;
        Ok((id, ns))
    }

    /// A target the caller may put a repository into: enabled, caller-administered, and for a
    /// repository-owned target, that repository.
    async fn usable_target(&self, caller: &Caller, id: &str, repo: &str) -> Result<Target, StorageError> {
        let t = self.owned(caller, id).await?;
        if t.state != State::Enabled {
            return Err(StorageError::FailedPrecondition(format!(
                "the storage target is {}, not enabled",
                t.state.as_str()
            )));
        }
        if t.owner.kind == OwnerKind::Repo && t.owner.name != repo {
            return Err(StorageError::PermissionDenied("this target belongs to another repository"));
        }
        let assigned = self.db.repos_assigned(id).await?;
        if assigned >= i64::from(self.cfg.max_repos_per_target) {
            return Err(StorageError::Quota(format!(
                "a target serves at most {} repositories",
                self.cfg.max_repos_per_target
            )));
        }
        Ok(t)
    }

    /// Whether the namespace holds at least one object at `member`.
    async fn holds_objects(&self, member: &super::routed::Member) -> Result<bool, StorageError> {
        use futures::TryStreamExt;
        let mut list = member.store.list(Some(&member.root));
        match list.try_next().await {
            Ok(first) => Ok(first.is_some()),
            Err(e) => Err(StorageError::Unavailable(live::sanitize(&e).into())),
        }
    }

    /// New objects of `repo` go to `target_id` (`""`: the instance default). Objects already
    /// stored stay where they are, readable through the fallback, until a migration moves them.
    pub async fn assign_repo(&self, caller: &Caller, repo: &str, target_id: &str) -> Result<String, StorageError> {
        let (repo_id, ns) = self.repo_row(caller, repo).await?;
        if !target_id.is_empty() {
            self.usable_target(caller, target_id, repo).await?;
        }
        let active = self.db.count_active_for_repo(repo_id).await?;
        if active > 0 {
            return Err(StorageError::MigrationActive);
        }
        for _ in 0..6 {
            let cur = self.db.route(repo_id).await?;
            let (cur_primary, cur_fallback, version) = match &cur {
                Some(r) => (r.target_id.clone(), r.fallback.clone(), Some(r.version)),
                None => (String::new(), None, None),
            };
            if cur_primary == target_id {
                return Ok(target_id.to_owned());
            }
            // Where the old objects are: the old primary, if it holds any; a fallback that still
            // holds objects makes a second hop impossible without a migration.
            let old_primary = self.router.member(&cur_primary, ns).await?;
            let mut fallback: Option<String> = None;
            if let Some(f) = &cur_fallback {
                let place = self.router.member(f, ns).await?;
                if self.holds_objects(&place).await? {
                    return Err(StorageError::FailedPrecondition(
                        "objects are still in an earlier place; run a migration before assigning again".into(),
                    ));
                }
            }
            if self.holds_objects(&old_primary).await? {
                fallback = Some(cur_primary.clone());
            }
            let ok = if target_id.is_empty() && fallback.is_none() {
                // Default, with nothing elsewhere: no row says the same.
                self.db.clear_route(repo_id).await?;
                true
            } else {
                self.db
                    .put_route(repo_id, target_id, fallback.as_deref(), None, version, now())
                    .await?
            };
            if ok {
                self.router.forget_route(ns);
                self.audit.record(
                    self.event("storage.repo.assigned", caller)
                        .repo(repo)
                        .target(target_id)
                        .detail("fallback", if fallback.is_some() { "true" } else { "false" }),
                );
                return Ok(target_id.to_owned());
            }
        }
        Err(StorageError::Unavailable("the repository's route kept changing; retry".into()))
    }

    // ------------------------------------------------------------------ migrations

    pub async fn start_migration(
        &self,
        caller: &Caller,
        repo: &str,
        to_target_id: &str,
        delete_source: bool,
    ) -> Result<MigrationRow, StorageError> {
        let (repo_id, _ns) = self.repo_row(caller, repo).await?;
        let to = if to_target_id.is_empty() {
            None
        } else {
            Some(self.usable_target(caller, to_target_id, repo).await?)
        };
        let route = self.db.route(repo_id).await?;
        let (primary, fallback) = match &route {
            Some(r) => (r.target_id.clone(), r.fallback.clone()),
            None => (String::new(), None),
        };
        if primary == to_target_id && fallback.is_none() {
            return Err(StorageError::InvalidArgument("the repository already keeps its objects there".into()));
        }
        // Moving between two targets that point at the same bucket and prefix would delete the
        // objects it just "copied" when the source is removed.
        if let Some(to) = &to {
            for other in std::iter::once(&primary).chain(fallback.iter()).filter(|p| !p.is_empty()) {
                if let Some(o) = self.db.target(other).await?
                    && o.endpoint == to.endpoint
                    && o.bucket == to.bucket
                    && o.prefix == to.prefix
                    && o.id != to.id
                {
                    return Err(StorageError::InvalidArgument(
                        "the source and the destination are the same bucket and prefix".into(),
                    ));
                }
            }
        }
        let id = model::new_id("sm_");
        let from = primary;
        let created = self
            .db
            .insert_migration(
                &NewMigration { id: &id, repo_id, repo_name: repo, from: &from, to: to_target_id, delete_source },
                now_ms(),
            )
            .await?;
        if !created {
            return Err(StorageError::MigrationActive);
        }
        self.audit.record(
            self.event("storage.migration.started", caller)
                .repo(repo)
                .target(to_target_id)
                .detail("migration", &id)
                .detail("delete_source", if delete_source { "true" } else { "false" }),
        );
        self.engine.kick();
        self.db.migration(&id).await?.ok_or(StorageError::Internal)
    }

    async fn migration_for(&self, caller: &Caller, id: &str) -> Result<MigrationRow, StorageError> {
        if !is_id("sm_", id) {
            return Err(StorageError::NotFound("no such migration"));
        }
        let m = self
            .db
            .migration(id)
            .await?
            .ok_or(StorageError::NotFound("no such migration"))?;
        if !self.authority.can_admin_repo(caller, &m.repo_name).await {
            return Err(StorageError::NotFound("no such migration"));
        }
        Ok(m)
    }

    pub async fn get_migration(&self, caller: &Caller, id: &str) -> Result<MigrationRow, StorageError> {
        self.migration_for(caller, id).await
    }

    pub async fn list_migrations(
        &self,
        caller: &Caller,
        repo: &str,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> Result<(Vec<MigrationRow>, Option<(i64, String)>), StorageError> {
        let (repo_id, _) = self.repo_row(caller, repo).await?;
        let limit = if limit == 0 { 20 } else { limit.min(100) };
        let mut rows = self
            .db
            .migrations_of(
                repo_id,
                after.as_ref().map(|(c, i)| (*c, i.as_str())),
                i64::from(limit) + 1,
            )
            .await?;
        let next = if rows.len() > limit as usize {
            rows.truncate(limit as usize);
            rows.last().map(|r| (r.created_ms, r.id.clone()))
        } else {
            None
        };
        Ok((rows, next))
    }

    /// Idempotent: a migration that ended, or already switched, comes back unchanged.
    pub async fn cancel_migration(&self, caller: &Caller, id: &str) -> Result<MigrationRow, StorageError> {
        let m = self.migration_for(caller, id).await?;
        let over = matches!(m.state.as_str(), state::DONE | state::FAILED | state::CANCELLED);
        if !over && !m.switched {
            self.db.migration_mark(id, super::db::Mark::Cancel, now()).await?;
            self.audit.record(
                self.event("storage.migration.finished", caller)
                    .repo(&m.repo_name)
                    .detail("migration", id)
                    .detail("outcome", "cancel requested"),
            );
            self.engine.kick();
        }
        self.db.migration(id).await?.ok_or(StorageError::Internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_bounds() {
        let c = StorageConfig::default();
        assert!(c.validate().is_ok());
        assert_eq!((c.max_targets_per_owner, c.allow_user_targets), (64, true));
        for bad in [
            StorageConfig { max_targets_per_owner: 0, ..c.clone() },
            StorageConfig { max_repos_per_target: 0, ..c.clone() },
            StorageConfig { migration_workers: 0, ..c.clone() },
            StorageConfig { health_interval_secs: 5, ..c.clone() },
        ] {
            assert!(bad.validate().is_err());
        }
        assert!(StorageConfig { health_interval_secs: 0, ..c }.validate().is_ok());
        let parsed: Result<StorageConfig, _> = toml::from_str("max_targets_per_owner = 3\nunknown = 1");
        assert!(parsed.is_err());
    }
}
