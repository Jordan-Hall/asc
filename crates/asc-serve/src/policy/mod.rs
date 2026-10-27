//! Repository policy as code, enforced (docs/BUILDOUT.md sections 1, 4 and 5).
//!
//! `.asc/policy.toml` on the trunk tip says who may write which paths, what landing on trunk needs
//! (approvals, checks, signatures), what an agent may do, what the secret scanner does and what the
//! merge queue re-checks. [`asc_core::policy`] parses the file and answers the pure questions; this
//! module reads it from the repository, decides which ref updates it binds and refuses the ones that
//! break it.
//!
//! * [`PolicyStore`]: the policy of a changeset, cached by changeset id (an id never changes meaning).
//! * [`gate`]: the check every `UpdateRefs`, land, merge and git push passes before a ref moves.
//! * [`rpc`]: the `Policy` service and `Repo.GetPolicy`.
//!
//! # The rules in force
//!
//! The policy that decides an update is the one on the trunk tip *before* the update, never the one
//! the update brings, so a change cannot lift the rules that bind it. An update to trunk that
//! changes the file is also checked to leave a policy that parses: a broken file would lock every
//! later update out. A file that cannot be used (present but invalid, for instance written before
//! the server checked) fails closed: every update of a protected ref is refused except one that
//! repairs the file.
//!
//! # Who is who
//!
//! [`who_of`] maps a principal id to a policy subject: `7:alice` and `pat:alice` are `user:alice`,
//! `agent:<name>` is an agent, a deploy key is `deploy:`, a CI task or token is `ci:` and the host
//! service is `host:`. An anonymous or unknown principal matches `*` only.

pub mod gate;
pub mod rpc;

#[cfg(test)]
mod scan_truncation_tests;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::object::Kind;
use asc_core::path::{RefName, RepoPath};
use asc_core::policy::Kind as WhoKind;
use asc_core::policy::{Diagnostic, MAX_POLICY_BYTES, POLICY_PATH, Policy, Who};
use asc_store::refs::Refs;
use bytes::Bytes;
use tonic::Status;

use crate::authz::UserIdentity;
use crate::authz::identity::is_host_service;
use crate::registry::{Extensions, Registry, RepoEntry};
use crate::review::internal;

/// What `Version.features` adds for the `Policy` service.
pub const FEATURE: &str = "policy-api.v1";
/// What `Version.features` adds for `Repo.GetPolicy`.
pub const REPO_FEATURE: &str = "policy.v1";
/// Most paths one `Evaluate` or `GetPolicy` call decides.
pub const MAX_EVAL_PATHS: usize = 64;

/// The policy subject a principal id stands for.
pub fn who_of(principal_id: &str) -> Who {
    if is_host_service(principal_id) {
        return Who::new(WhoKind::Host, "host-service");
    }
    if let Some(name) = principal_id.strip_prefix("agent:") {
        return if name.is_empty() {
            Who::unknown()
        } else {
            Who::new(WhoKind::Agent, name)
        };
    }
    if let Some(user) = principal_id.strip_prefix("pat:") {
        return if user.is_empty() {
            Who::unknown()
        } else {
            Who::new(WhoKind::User, user)
        };
    }
    if let Some(repo) = principal_id.strip_prefix("deploy:") {
        return Who::new(WhoKind::Deploy, repo);
    }
    if let Some(id) = principal_id.strip_prefix("ci:") {
        return Who::new(WhoKind::Ci, id);
    }
    if let Some(id) = UserIdentity::parse(principal_id) {
        if id.is_anonymous() {
            return Who::unknown();
        }
        if id.deploy_key_id != 0 {
            return Who::new(WhoKind::Deploy, id.handle);
        }
        if id.user_id < 0 {
            return Who::new(WhoKind::Ci, id.handle);
        }
        return Who::new(WhoKind::User, id.handle);
    }
    Who::unknown()
}

/// The policy a [`Who`] is judged under for a wire `PrincipalRef`: kinds `user`, `agent`,
/// `deploy`, `ci` and `host`. Anything else (`system`, unknown) matches `*` only.
pub fn who_from_ref(kind: &str, name: &str) -> Who {
    let kind = match kind {
        "user" => WhoKind::User,
        "agent" => WhoKind::Agent,
        "deploy" => WhoKind::Deploy,
        "ci" => WhoKind::Ci,
        "host" => WhoKind::Host,
        _ => return Who::unknown(),
    };
    Who::new(kind, name)
}

/// What a changeset's `.asc/policy.toml` is.
#[derive(Clone, Debug)]
pub enum State {
    /// No such file.
    Absent,
    /// A file that parses.
    Valid(Arc<Policy>),
    /// A file that is there and cannot be used.
    Invalid(Diagnostic),
}

/// The policy file of one changeset.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The changeset it was read from; `None` when the repository has no trunk yet.
    pub source: Option<ChangesetId>,
    /// The file as stored (empty when absent).
    pub text: Bytes,
    pub state: State,
}

impl Loaded {
    pub fn absent(source: Option<ChangesetId>) -> Self {
        Self {
            source,
            text: Bytes::new(),
            state: State::Absent,
        }
    }

    pub fn present(&self) -> bool {
        !matches!(self.state, State::Absent)
    }

    /// The valid policy, if there is one.
    pub fn policy(&self) -> Option<&Arc<Policy>> {
        match &self.state {
            State::Valid(p) => Some(p),
            _ => None,
        }
    }

    /// The error of an unusable file.
    pub fn error(&self) -> Option<&Diagnostic> {
        match &self.state {
            State::Invalid(d) => Some(d),
            _ => None,
        }
    }
}

fn path() -> RepoPath {
    RepoPath::new(POLICY_PATH).expect("the policy path is a valid path")
}

/// Reads policies from repositories.
pub struct PolicyStore {
    registry: Arc<Registry>,
    cache: quick_cache::sync::Cache<(i64, ChangesetId), Arc<Loaded>>,
}

impl PolicyStore {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            cache: quick_cache::sync::Cache::new(1024),
        }
    }

    /// The policy file at changeset `cs` of `entry`. A read failure is an error (the caller refuses),
    /// never an absent policy.
    pub async fn at(&self, entry: &RepoEntry, cs: ChangesetId) -> Result<Arc<Loaded>, Status> {
        if let Some(hit) = self.cache.get(&(entry.key.0, cs)) {
            return Ok(hit);
        }
        let loaded = Arc::new(self.read(entry, cs).await?);
        self.cache.insert((entry.key.0, cs), loaded.clone());
        Ok(loaded)
    }

    async fn read(&self, entry: &RepoEntry, cs: ChangesetId) -> Result<Loaded, Status> {
        let engine = entry.backend.engine();
        let body = engine
            .get_changeset(cs)
            .await
            .map_err(|e| internal("policy: changeset", &e))?;
        let found = engine
            .lookup(body.root, &path())
            .await
            .map_err(|e| internal("policy: lookup", &e))?;
        let Some(e) = found else {
            return Ok(Loaded::absent(Some(cs)));
        };
        let invalid = |message: &str| Loaded {
            source: Some(cs),
            text: Bytes::new(),
            state: State::Invalid(Diagnostic {
                code: "not_a_file",
                message: message.to_owned(),
                line: 0,
                column: 0,
            }),
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) {
            return Ok(invalid("the policy path is not a regular file"));
        }
        if e.size > MAX_POLICY_BYTES as u64 {
            return Ok(Loaded {
                source: Some(cs),
                text: Bytes::new(),
                state: State::Invalid(Diagnostic {
                    code: "too_large",
                    message: format!("the policy file is larger than {MAX_POLICY_BYTES} bytes"),
                    line: 0,
                    column: 0,
                }),
            });
        }
        let text = engine
            .read_file(asc_core::id::FileId::from_bytes(e.id), e.size)
            .await
            .map_err(|e| internal("policy: read", &e))?;
        let state = match Policy::diagnose(&text) {
            Ok(p) => State::Valid(Arc::new(p)),
            Err(d) => State::Invalid(d),
        };
        Ok(Loaded {
            source: Some(cs),
            text,
            state,
        })
    }

    /// The tip of trunk (the default branch), if it exists.
    pub async fn trunk_tip(&self, entry: &RepoEntry) -> Result<Option<ChangesetId>, Status> {
        let name = RefName::new(format!("refs/heads/{}", entry.default_branch).as_bytes())
            .map_err(|e| internal("policy: default branch", &e))?;
        let got = self
            .registry
            .refs()
            .get(entry.key, &name)
            .await
            .map_err(|e| internal("policy: refs", &e))?;
        Ok(match got.map(|r| r.target) {
            Some(asc_core::id::RefTarget::Changeset(c)) => Some(c),
            _ => None,
        })
    }

    /// The policy in force now: the file on the trunk tip.
    pub async fn in_force(&self, entry: &RepoEntry) -> Result<Arc<Loaded>, Status> {
        match self.trunk_tip(entry).await? {
            Some(tip) => self.at(entry, tip).await,
            None => Ok(Arc::new(Loaded::absent(None))),
        }
    }
}

/// The services this unit mounts, behind one registration.
pub struct Security {
    pub policy: Arc<rpc::PolicyHub>,
    pub secrets: Arc<crate::secretscan::rpc::SecretScanHub>,
    pub signing: Arc<crate::signing::rpc::Signing>,
}

/// The place the server's single [`Security`] is registered, once (read by `build_grpc`).
#[derive(Default)]
pub struct SecuritySlot(std::sync::OnceLock<Arc<Security>>);

impl SecuritySlot {
    pub fn set(&self, s: Arc<Security>) -> bool {
        self.0.set(s).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<Security>> {
        self.0.get()
    }
}

/// The commit graph of a repository, as the gate and the scanner walk it.
pub type RepoGraph = asc_engine::graph::Graph<
    crate::graph_sql::SqlGraph<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>,
>;

/// A graph over `entry`'s changesets.
pub fn graph_of(registry: &Registry, entry: &RepoEntry) -> RepoGraph {
    asc_engine::graph::Graph::new(crate::graph_sql::SqlGraph::new(
        registry.pool().clone(),
        entry.ns,
        entry.backend.engine().clone(),
    ))
}

/// Registers the gate, the three services and the `Repo.GetPolicy` handler: the unit's one line in
/// `register_all`. A bad `[secrets]` or `[signing]` section, or an unusable server key file, is
/// an error that stops the server rather than a feature that is silently off.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    mutations: &Arc<crate::mutations::ServerMutations>,
    secrets: crate::secretscan::Config,
    signing: crate::signing::Config,
) -> Result<(), String> {
    secrets.validate()?;
    signing.validate()?;
    let server = match &signing.server_key_file {
        Some(path) => Some(Arc::new(crate::signing::ServerSigner::load(path)?)),
        None => None,
    };
    let policies = Arc::new(PolicyStore::new(registry.clone()));
    let scanner = Arc::new(crate::secretscan::Scanner::new(registry.clone(), secrets));
    let signing_hub = Arc::new(crate::signing::rpc::Signing::new(
        registry.clone(),
        signing,
        server,
    ));
    let policy_hub = Arc::new(rpc::PolicyHub::new(
        registry.clone(),
        policies.clone(),
        mutations.clone(),
    ));
    let secret_hub = Arc::new(crate::secretscan::rpc::SecretScanHub::new(
        registry.clone(),
        scanner.clone(),
        policies.clone(),
    ));
    let gate = Arc::new(gate::ServerGate::new(
        registry.clone(),
        policies,
        scanner,
        signing_hub.clone(),
    ));
    if ext.pipeline.set_gate(gate).is_err() {
        return Err("the ref gate was already set".into());
    }
    ext.handlers
        .set_policy(policy_hub.clone())
        .map_err(|e| e.to_string())?;
    let ok = ext.security.set(Arc::new(Security {
        policy: policy_hub,
        secrets: secret_hub,
        signing: signing_hub,
    }));
    if ok {
        Ok(())
    } else {
        Err("the security services were already registered".into())
    }
}

/// What `Version.features` adds when the security services are mounted.
pub fn features() -> Vec<String> {
    vec![
        FEATURE.to_owned(),
        REPO_FEATURE.to_owned(),
        crate::secretscan::FEATURE.to_owned(),
        crate::signing::FEATURE.to_owned(),
    ]
}
