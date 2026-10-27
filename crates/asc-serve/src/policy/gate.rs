//! The gate every ref update passes before a ref moves.
//!
//! `UpdateRefs` (and so land, merge, rebase, restack and `git push`) calls [`RefGate::check`] after
//! the closure of the new targets is ingested and before the hooks and the commit. The gate
//! decides whether the update breaks the repository policy, adds a secret or lacks a signature; a
//! refusal is a status with a typed error code and moves nothing.
//!
//! # What binds what
//!
//! * A **protected ref** is trunk (the default branch), `release/*` and the repository's land
//!   branches. Path rules, secret scanning and the agent rules apply to every update of one;
//!   `[trunk]` (force, approvals, checks) and `[signing]` apply to trunk.
//! * An **agent** never writes a shared ref. Its drafts (user refs) are checked against the agent
//!   scope, its denied paths and its run budget, so a stack of agent changes is exactly what the
//!   policy would let it write, and landing it is a person's decision.
//! * The policy is the file on the trunk tip *before* the update. When that file cannot be used,
//!   only an update of trunk that leaves a usable file passes (the repair).
//!
//! Nothing here writes a ref or a file. The two writes it makes are bookkeeping: a server
//! attestation of a rewritten signed changeset, and findings when `block_on_push` is off.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::object::Changeset;
use asc_core::policy::{Denial, Diagnostic, POLICY_PATH, Policy, SecretsPolicy, Who};
use asc_engine::graph::LogQuery;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{CallCtx, wire};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use prost::Message;
use tonic::Status;

use super::{Loaded, PolicyStore, State, graph_of, who_of};
use crate::audit::actor::actor_of;
use crate::audit::{NewEvent, Outcome};
use crate::registry::{Registry, RepoEntry};
use crate::review::{internal, now, sql};
use crate::secretscan::scan::Report;
use crate::secretscan::{MAX_CHANGESETS, Scanner};
use crate::signing::rpc::Signing;
use crate::update::{info, rpc};

/// Approvals an update of an unusable policy needs when its sender is no administrator.
const REPAIR_APPROVALS: u32 = 1;
/// Most changesets one update may add before the gate refuses to check it.
pub const MAX_ADDED: usize = 100_000;
/// Most paths or locations a refusal lists.
pub const MAX_REPORTED: usize = 20;
/// `CiStatus::Success`.
const CI_SUCCESS: i64 = 3;
const CI_FAILURE: i64 = 4;

/// One ref change as the gate sees it.
#[derive(Clone, Debug)]
pub struct GateUpdate {
    /// The name as pushed (`refs/heads/x`).
    pub name: String,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
    /// Stored as the pusher's own user ref (a draft).
    pub user: bool,
}

/// How the update arrived.
#[derive(Clone, Debug)]
pub enum GateMode {
    /// `UpdateRefs` in fast-forward or force mode.
    Push { force: bool },
    /// A land. `rewrites` are the `(pushed, landed)` pairs of the server's rebase; the landed
    /// changesets are already ingested.
    Land {
        rewrites: Vec<(ChangesetId, ChangesetId)>,
    },
    /// `refs/for/<branch>/asc-<change>`: a changeset published for review. Nothing moves a ref, so
    /// protection, approvals and signatures are for the landing; what a push to the branch would
    /// meet before them still applies: the policy must be usable, the pusher's path rules, an
    /// agent's scope, and the secret scan with its fail-closed truncation.
    Submit,
}

/// What the gate is asked.
pub struct GateRequest<'a> {
    pub ctx: &'a CallCtx,
    pub entry: &'a RepoEntry,
    pub updates: &'a [GateUpdate],
    pub mode: GateMode,
}

/// The check a ref update passes.
#[async_trait]
pub trait RefGate: Send + Sync {
    /// `Ok` lets the update go on; an error refuses it with the status to answer.
    async fn check(&self, req: &GateRequest<'_>) -> Result<(), Status>;
}

/// The gate of a server: policy, secret scanning and signatures.
pub struct ServerGate {
    registry: Arc<Registry>,
    policies: Arc<PolicyStore>,
    scanner: Arc<Scanner>,
    signing: Arc<Signing>,
}

/// The first 12 hex characters of a changeset id.
fn short(cs: &ChangesetId) -> String {
    cs.to_string().chars().take(12).collect()
}

fn denied(reason: impl Into<String>, paths: impl IntoIterator<Item = String>) -> Status {
    let mut v = vec![reason.into()];
    v.extend(paths.into_iter().take(MAX_REPORTED));
    rpc(RpcError::PolicyDenied(info(v)))
}

/// The refusal for a blocking secret policy whose scan stopped before it read every file.
fn truncated_refusal(sp: &SecretsPolicy, report: &Report) -> Option<Status> {
    (sp.block_on_push && report.truncated).then(|| {
        denied(
            "the update is too large to scan for secrets; push it in parts",
            [],
        )
    })
}

fn from_denial(d: &Denial, more: &[String]) -> Status {
    let mut paths: Vec<String> = Vec::new();
    if !d.path.is_empty() {
        paths.push(d.path.clone());
    }
    paths.extend(more.iter().cloned());
    denied(d.reason.clone(), paths)
}

fn unusable(d: &Diagnostic) -> Status {
    let at = if d.line > 0 {
        format!(" (line {})", d.line)
    } else {
        String::new()
    };
    denied(
        format!(
            "the repository policy cannot be used{at}: {}; fix .asc/policy.toml on trunk",
            d.message
        ),
        [],
    )
}

fn invalid_policy(d: &Diagnostic) -> Status {
    let at = if d.line > 0 {
        format!(" (line {}, column {})", d.line, d.column)
    } else {
        String::new()
    };
    rpc(RpcError::PolicyInvalid(info(vec![format!(
        "the update would leave a policy file that cannot be used{at}: {}",
        d.message
    )])))
}

/// What the gate needs of the changesets an update adds.
struct Added {
    ids: Vec<ChangesetId>,
    bodies: Vec<Arc<Changeset>>,
}

impl Added {
    /// Every path the changesets write or delete, in path order.
    fn paths(&self) -> BTreeSet<Vec<u8>> {
        self.bodies
            .iter()
            .flat_map(|b| {
                b.changes
                    .as_slice()
                    .iter()
                    .map(|(p, _)| p.as_bytes().to_vec())
            })
            .collect()
    }
}

/// What a landing needs, borrowed.
struct LandNeed<'a> {
    approvals: u32,
    checks: &'a [String],
    /// `[ci] allow_quarantine`: a failed check whose failures are all quarantined passes.
    quarantine: bool,
}

/// What the review side knows about the change a changeset belongs to.
struct Facts {
    change: Vec<u8>,
    version: i64,
    approvals: u32,
    reviewed_version: u32,
}

impl ServerGate {
    pub fn new(
        registry: Arc<Registry>,
        policies: Arc<PolicyStore>,
        scanner: Arc<Scanner>,
        signing: Arc<Signing>,
    ) -> Self {
        Self {
            registry,
            policies,
            scanner,
            signing,
        }
    }

    /// Whether the caller may repair a repository whose policy is unusable: the credential of
    /// this very call holds admin on the repository. The host service's own credential never
    /// does: it relays calls of users the gate cannot see. A relayed call is allowed only when
    /// the host service also forwards the person it acts for, signed under the key it shares with
    /// this server (`asc_rpc::forwarded`), as an administrator of this very repository; the
    /// claim is read from the call's metadata and checked before it gets here.
    async fn may_repair(&self, ctx: &CallCtx) -> bool {
        if crate::authz::is_host_service(&ctx.principal.id) {
            return match ctx.admin.forwarded_admin() {
                Some(person) => {
                    tracing::info!(
                        event = "policy.repair.forwarded_admin",
                        repo = %ctx.repo,
                        person,
                        "unusable policy repaired on behalf of a forwarded administrator"
                    );
                    true
                }
                None => false,
            };
        }
        ctx.admin.holds().await
    }

    /// Whether `name` is a protected ref of the repository: trunk, `release/*` or a land branch.
    async fn protected(&self, entry: &RepoEntry, name: &str) -> Result<bool, Status> {
        let trunk = format!("refs/heads/{}", entry.default_branch);
        if name == trunk || name.starts_with("refs/heads/release/") {
            return Ok(true);
        }
        let raw: Option<String> =
            sqlx::query_scalar("SELECT land_branches FROM repos WHERE id = $1")
                .bind(entry.key.0)
                .fetch_optional(self.registry.pool())
                .await
                .map_err(sql)?;
        // A malformed list only drops the extras, as the land path does.
        let land: Vec<String> = raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default();
        Ok(land.iter().any(|b| {
            if b.starts_with("refs/") {
                b == name
            } else {
                name.strip_prefix("refs/heads/") == Some(b.as_str())
            }
        }))
    }

    async fn added(
        &self,
        entry: &RepoEntry,
        old: Option<ChangesetId>,
        new: ChangesetId,
        limit: usize,
    ) -> Result<Added, Status> {
        let graph = graph_of(&self.registry, entry);
        let ids: Vec<ChangesetId> = graph
            .log(LogQuery {
                include: vec![new],
                exclude: old.into_iter().collect(),
                limit: Some(limit + 1),
                ..LogQuery::default()
            })
            .try_collect()
            .await
            .map_err(|e| internal("gate: history", &e))?;
        if ids.len() > limit {
            return Err(Status::failed_precondition(format!(
                "this update adds more than {limit} changesets and cannot be checked; push it in parts"
            )));
        }
        let engine = entry.backend.engine();
        let bodies: Vec<Arc<Changeset>> = futures::stream::iter(ids.iter().copied())
            .map(|id| async move { engine.get_changeset(id).await })
            .buffered(16)
            .try_collect()
            .await
            .map_err(|e| internal("gate: changeset", &e))?;
        Ok(Added { ids, bodies })
    }

    async fn audit_denied(&self, ctx: &CallCtx, name: &str, reason: &str, path: &str) {
        let event = NewEvent::new("policy.denied", actor_of(&ctx.principal.id))
            .repo(ctx.repo.as_str())
            .outcome(Outcome::Denied)
            .target(name)
            .detail("ref", name)
            .detail("reason", reason)
            .detail("path", path);
        crate::audit::store::record_best_effort(self.registry.pool(), &event).await;
    }

    /// Audits a refusal and passes it on.
    async fn refuse(&self, ctx: &CallCtx, name: &str, status: Status) -> Status {
        let (reason, path) = match asc_rpc::server::status_rpc_error(&status) {
            Some(e) => {
                let p = &e.info().paths;
                (
                    p.first().cloned().unwrap_or_default(),
                    p.get(1).cloned().unwrap_or_default(),
                )
            }
            None => (status.message().to_owned(), String::new()),
        };
        self.audit_denied(ctx, name, &reason, &path).await;
        status
    }

    // -------------------------------------------------------------------------- agents

    /// An agent's draft: its scope, its denied paths and its run budget.
    async fn check_draft(
        &self,
        req: &GateRequest<'_>,
        who: &Who,
        u: &GateUpdate,
        in_force: &Loaded,
    ) -> Result<(), Status> {
        let policy = match &in_force.state {
            State::Absent => return Ok(()),
            State::Valid(p) => p.clone(),
            State::Invalid(d) => return Err(unusable(d)),
        };
        let Some(RefTarget::Changeset(new)) = u.new else {
            return Ok(());
        };
        let old = match u.old {
            Some(RefTarget::Changeset(c)) => Some(c),
            _ => None,
        };
        // A new draft adds what it has beyond trunk, not trunk's whole history.
        let added = self
            .added(req.entry, old.or(in_force.source), new, MAX_ADDED)
            .await?;
        let paths = added.paths();
        let mut refused = Vec::new();
        let mut first: Option<Denial> = None;
        for p in &paths {
            if let Err(d) = policy.check_write(who, p) {
                refused.push(d.path.clone());
                first.get_or_insert(d);
            }
        }
        if let Some(d) = first {
            let mut i = info(vec![d.reason.clone()]);
            i.paths.extend(refused.into_iter().take(MAX_REPORTED));
            return Err(rpc(RpcError::AgentScopeDenied(i)));
        }
        if let Err(d) = policy.check_run(who, added.ids.len(), paths.len()) {
            return Err(rpc(RpcError::BudgetExceeded(info(vec![d.reason]))));
        }
        Ok(())
    }

    // -------------------------------------------------------------------------- submitted changes

    async fn check_submitted(&self, req: &GateRequest<'_>, who: &Who) -> Result<(), Status> {
        let in_force = self.policies.in_force(req.entry).await?;
        for u in req.updates {
            let checked = if who.is_agent() {
                // An agent's output is a draft under review: its scope and its run budget.
                self.check_draft(req, who, u, &in_force).await
            } else {
                self.check_submitted_one(req, who, u, &in_force).await
            };
            if let Err(status) = checked {
                return Err(self.refuse(req.ctx, &u.name, status).await);
            }
        }
        Ok(())
    }

    async fn check_submitted_one(
        &self,
        req: &GateRequest<'_>,
        who: &Who,
        u: &GateUpdate,
        in_force: &Loaded,
    ) -> Result<(), Status> {
        let entry = req.entry;
        let Some(RefTarget::Changeset(new)) = u.new else {
            return Ok(());
        };
        // While the policy in force cannot be read nothing is judged by what the update brings.
        if let Some(d) = in_force.error() {
            return Err(unusable(d));
        }
        let sp = in_force
            .policy()
            .map(|p| p.secrets.clone())
            .unwrap_or_default();
        let mut limit = if in_force.policy().is_some() {
            MAX_ADDED
        } else {
            self.scanner.config().max_files.clamp(1, MAX_ADDED)
        };
        if sp.enabled {
            limit = limit.min(MAX_CHANGESETS);
        }
        let added = self.added(entry, in_force.source, new, limit).await?;
        if let Some(p) = in_force.policy() {
            let mut refused = Vec::new();
            let mut first: Option<Denial> = None;
            for path in added.paths() {
                if let Err(d) = p.check_write(who, &path) {
                    refused.push(d.path.clone());
                    first.get_or_insert(d);
                }
            }
            if let Some(d) = first {
                return Err(from_denial(&d, &refused));
            }
        }
        if sp.enabled {
            let report = self
                .scanner
                .scan_changesets(entry.backend.engine(), entry.key.0, &sp, &added.ids)
                .await?;
            if !report.found.is_empty() {
                if sp.block_on_push {
                    return Err(self.secret_refusal(req.ctx, &u.name, &report).await);
                }
                self.record_findings(entry, &report).await?;
            }
            if let Some(status) = truncated_refusal(&sp, &report) {
                return Err(status);
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------- protected refs

    async fn check_protected(
        &self,
        req: &GateRequest<'_>,
        who: &Who,
        u: &GateUpdate,
        is_trunk: bool,
        in_force: &Loaded,
    ) -> Result<(), Status> {
        let entry = req.entry;
        let engine = entry.backend.engine();
        let old = match u.old {
            Some(RefTarget::Changeset(c)) => Some(c),
            _ => None,
        };
        let Some(new) = u.new else {
            // Deleting trunk needs allow_force; deleting any other protected ref is a maintainer
            // matter the forge decides.
            if is_trunk && let Some(p) = in_force.policy() {
                p.check_force().map_err(|d| from_denial(&d, &[]))?;
            }
            if is_trunk && let Some(d) = in_force.error() {
                return Err(unusable(d));
            }
            return Ok(());
        };
        let RefTarget::Changeset(new) = new else {
            return Ok(());
        };

        // The policy that decides, and what trunk will say after the update.
        let new_loaded = if is_trunk {
            Some(self.policies.at(entry, new).await?)
        } else {
            None
        };
        // An update of trunk never leaves a policy file that cannot be used, whoever sends it.
        if let Some(n) = &new_loaded
            && let Some(d) = n.error()
        {
            // An update that did not touch the broken file is refused for the file that was
            // already broken, not blamed for it.
            return Err(match in_force.error() {
                Some(was) if in_force.text == n.text && was.code == d.code => unusable(d),
                _ => invalid_policy(d),
            });
        }
        // Only the policy in force judges an update. While it is unusable nothing is judged by the
        // policy the update brings (it could switch `[trunk]` and `[signing]` off); the repair
        // below decides who may move the ref.
        let mut broken: Option<&Diagnostic> = None;
        let effective: Option<Arc<Policy>> = match &in_force.state {
            State::Valid(p) => Some(p.clone()),
            State::Absent => None,
            State::Invalid(d) => {
                broken = Some(d);
                None
            }
        };

        // Rewriting history needs allow_force on trunk.
        if let (GateMode::Push { force: true }, Some(o), true) = (&req.mode, old, is_trunk)
            && o != new
        {
            let graph = graph_of(&self.registry, entry);
            let fast = graph
                .is_ancestor(o, new)
                .await
                .map_err(|e| internal("gate: ancestry", &e))?;
            if !fast {
                if let Some(p) = &effective {
                    p.check_force().map_err(|d| from_denial(&d, &[]))?;
                } else if let Some(d) = broken {
                    return Err(unusable(d));
                }
            }
        }

        // Secrets: under the policy in force (or the safe defaults), never the incoming one.
        let sp = in_force
            .policy()
            .map(|p| p.secrets.clone())
            .unwrap_or_default();

        // What the update adds. Without a policy only the secret scan reads it, and that stops
        // at its own limit. The scan reads no more than MAX_CHANGESETS changesets, so an update
        // with more is refused here rather than half scanned.
        let mut limit = if effective.is_some() {
            MAX_ADDED
        } else {
            self.scanner.config().max_files.clamp(1, MAX_ADDED)
        };
        if sp.enabled {
            limit = limit.min(MAX_CHANGESETS);
        }
        let added = self
            .added(entry, old.or(in_force.source), new, limit)
            .await?;

        let rewrites: HashMap<ChangesetId, ChangesetId> = match &req.mode {
            GateMode::Land { rewrites } => rewrites.iter().map(|(o, n)| (*n, *o)).collect(),
            GateMode::Push { .. } | GateMode::Submit => HashMap::new(),
        };

        // The repair of an unusable policy: an administrator, or an update of trunk that touches
        // nothing but the policy file and still has the approval a reviewed change needs.
        if let Some(d) = broken
            && !self.may_repair(req.ctx).await
        {
            let paths = added.paths();
            let only_policy =
                !paths.is_empty() && paths.iter().all(|p| p.as_slice() == POLICY_PATH.as_bytes());
            if !is_trunk || !only_policy {
                return Err(unusable(d));
            }
            self.check_approvals(entry, REPAIR_APPROVALS, &[], false, &added, &rewrites)
                .await?;
        }

        if let Some(p) = &effective {
            // Path rules and the agent scope, for the pusher.
            let mut refused = Vec::new();
            let mut first: Option<Denial> = None;
            for path in added.paths() {
                if let Err(d) = p.check_write(who, &path) {
                    refused.push(d.path.clone());
                    first.get_or_insert(d);
                }
            }
            if let Some(d) = first {
                return Err(from_denial(&d, &refused));
            }
            if is_trunk {
                self.check_review(entry, p, &added, &rewrites).await?;
                if p.signing.require_signed {
                    self.check_signed(entry, &added, &rewrites).await?;
                }
            }
        }

        if sp.enabled {
            let report = self
                .scanner
                .scan_changesets(engine, entry.key.0, &sp, &added.ids)
                .await?;
            if !report.found.is_empty() {
                if sp.block_on_push {
                    return Err(self.secret_refusal(req.ctx, &u.name, &report).await);
                }
                self.record_findings(entry, &report).await?;
            }
            // A scan that stopped at a limit proves nothing about the files it did not read, so a
            // blocking policy cannot let the update through on it.
            if let Some(status) = truncated_refusal(&sp, &report) {
                return Err(status);
            }
        }
        Ok(())
    }

    async fn secret_refusal(&self, ctx: &CallCtx, name: &str, report: &Report) -> Status {
        let mut seen = BTreeSet::new();
        let locations: Vec<String> = report
            .found
            .iter()
            .map(|f| f.location())
            .filter(|l| seen.insert(l.clone()))
            .take(MAX_REPORTED * 2)
            .collect();
        let mut rules: Vec<&str> = report.found.iter().map(|f| f.rule_id.as_str()).collect();
        rules.sort_unstable();
        rules.dedup();
        let event = NewEvent::new("secret.push.blocked", actor_of(&ctx.principal.id))
            .repo(ctx.repo.as_str())
            .outcome(Outcome::Denied)
            .target(name)
            .detail("ref", name)
            .detail("rule", rules.join(","))
            .detail("count", report.found.len().to_string());
        crate::audit::store::record_best_effort(self.registry.pool(), &event).await;
        rpc(RpcError::SecretDetected(info(locations)))
    }

    async fn record_findings(&self, entry: &RepoEntry, report: &Report) -> Result<(), Status> {
        let new: Vec<crate::secretscan::store::NewFinding> = report
            .found
            .iter()
            .map(|f| crate::secretscan::store::NewFinding {
                rule_id: f.rule_id.clone(),
                severity: f.severity,
                path: f.path.clone(),
                line: f.line,
                column: f.column,
                changeset: f.changeset,
                fingerprint: f.fingerprint.clone(),
                preview: f.preview.clone(),
            })
            .collect();
        crate::secretscan::store::upsert(self.registry.pool(), entry.key.0, &new, now())
            .await
            .map_err(sql)?;
        Ok(())
    }

    // -------------------------------------------------------------------------- review

    async fn facts(&self, entry: &RepoEntry, cs: &ChangesetId) -> Result<Option<Facts>, Status> {
        let pool = self.registry.pool();
        let ns = entry.ns.as_bytes().to_vec();
        let change: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
                .bind(ns.clone())
                .bind(cs.as_bytes().to_vec())
                .fetch_optional(pool)
                .await
                .map_err(sql)?;
        let Some(change) = change else {
            return Ok(None);
        };
        let version: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(v AS BIGINT) FROM change_versions WHERE ns = $1 AND change = $2 AND cs = $3",
        )
        .bind(ns.clone())
        .bind(change.clone())
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(pool)
        .await
        .map_err(sql)?;
        let state: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT state FROM review_state WHERE ns = $1 AND change = $2")
                .bind(ns)
                .bind(change.clone())
                .fetch_optional(pool)
                .await
                .map_err(sql)?;
        let review = match state {
            // A damaged summary proves no approval.
            Some(raw) => wire::ReviewState::decode(raw.as_slice()).ok(),
            None => None,
        };
        let (approvals, reviewed_version) =
            review.map_or((0, 0), |r| (r.approvals, r.reviewed_version));
        Ok(Some(Facts {
            change,
            version: version.unwrap_or(0),
            approvals,
            reviewed_version,
        }))
    }

    /// Whether the latest run named `name` for `version` of `change` succeeded. A job of that
    /// name counts as well, so `require_checks` can name either.
    async fn check_passed(
        &self,
        entry: &RepoEntry,
        change: &[u8],
        version: i64,
        name: &str,
        quarantine: bool,
    ) -> Result<bool, Status> {
        let pool = self.registry.pool();
        let run: Option<(i64, i64)> = sqlx::query_as(
            "SELECT CAST(id AS BIGINT), status FROM check_runs WHERE ns = $1 AND change = $2 \
             AND version = $3 AND name = $4 ORDER BY id DESC LIMIT 1",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.to_vec())
        .bind(version)
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(sql)?;
        if run.is_some_and(|(_, status)| status == CI_SUCCESS) {
            return Ok(true);
        }
        let job: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT CAST(r.id AS BIGINT), j.status, CAST(j.id AS BIGINT) FROM check_jobs j \
             JOIN check_runs r ON r.ns = j.ns AND r.id = j.run \
             WHERE r.ns = $1 AND r.change = $2 AND r.version = $3 AND j.name = $4 \
             ORDER BY r.id DESC, j.id DESC LIMIT 1",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.to_vec())
        .bind(version)
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(sql)?;
        if job.is_some_and(|(_, status, _)| status == CI_SUCCESS) {
            return Ok(true);
        }
        // A check that failed only on failures a maintainer quarantined does not block, when the
        // policy still allows quarantine. A run still going, cancelled or skipped is not a failure.
        // Named by a job, the check is that job alone; named by a run, every failed job of it.
        let failed = match (run, job) {
            (Some((id, CI_FAILURE)), _) => Some((id, None)),
            (_, Some((id, CI_FAILURE, job_id))) => Some((id, Some(job_id))),
            _ => None,
        };
        if let (true, Some((id, job_id))) = (quarantine, failed) {
            let mut conn = pool.acquire().await.map_err(sql)?;
            return crate::ci::insights::store::failed_only_on_quarantined(
                &mut conn, entry.ns, id, job_id,
            )
            .await
            .map_err(sql);
        }
        Ok(false)
    }

    /// `[trunk] require_approvals` and `require_checks`, for each changeset that lands. Approvals
    /// and check runs belong to a change and the version that was reviewed, so a changeset that is
    /// no version of any change cannot show either and is refused when either is required.
    async fn check_review(
        &self,
        entry: &RepoEntry,
        policy: &Policy,
        added: &Added,
        rewrites: &HashMap<ChangesetId, ChangesetId>,
    ) -> Result<(), Status> {
        let need = policy.land_requirements();
        self.check_approvals(
            entry,
            need.approvals,
            &need.checks,
            policy.ci.allow_quarantine,
            added,
            rewrites,
        )
        .await
    }

    /// `approvals` and the named `checks` for each changeset that lands.
    async fn check_approvals(
        &self,
        entry: &RepoEntry,
        approvals: u32,
        checks: &[String],
        quarantine: bool,
        added: &Added,
        rewrites: &HashMap<ChangesetId, ChangesetId>,
    ) -> Result<(), Status> {
        let need = LandNeed {
            approvals,
            checks,
            quarantine,
        };
        if need.approvals == 0 && need.checks.is_empty() {
            return Ok(());
        }
        let mut problems: Vec<String> = Vec::new();
        for cs in &added.ids {
            // The reviewed version is the one that was pushed, before any server rebase.
            let orig = rewrites.get(cs).copied().unwrap_or(*cs);
            let Some(f) = self.facts(entry, &orig).await? else {
                problems.push(format!("{}: not a version of a reviewed change", short(cs)));
                continue;
            };
            if need.approvals > 0 {
                // A host that does not track versions reports 0; otherwise the approvals must be
                // for the version that lands.
                let current = f.reviewed_version == 0 || i64::from(f.reviewed_version) == f.version;
                if !current {
                    problems.push(format!(
                        "{}: approved at version {}, landing version {}",
                        short(cs),
                        f.reviewed_version,
                        f.version
                    ));
                } else if f.approvals < need.approvals {
                    problems.push(format!(
                        "{}: {} of {} approvals",
                        short(cs),
                        f.approvals,
                        need.approvals
                    ));
                }
            }
            for check in need.checks {
                if !self
                    .check_passed(entry, &f.change, f.version, check, need.quarantine)
                    .await?
                {
                    problems.push(format!("{}: check {check:?} has not passed", short(cs)));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(denied(
                "landing on trunk needs the approvals and passing checks the policy requires",
                problems,
            ))
        }
    }

    // -------------------------------------------------------------------------- signatures

    /// `[signing] require_signed`: each changeset that lands has a valid signature. A changeset
    /// the server rebased has none of its own; when the server holds a signing key and the
    /// changeset it was made from was valid, the server attests to it.
    async fn check_signed(
        &self,
        entry: &RepoEntry,
        added: &Added,
        rewrites: &HashMap<ChangesetId, ChangesetId>,
    ) -> Result<(), Status> {
        let verifier = self.signing.verifier();
        let ns = entry.ns.as_bytes();
        let mut unsigned: Vec<String> = Vec::new();
        let mut lost_by_rebase = false;
        for cs in &added.ids {
            let v = verifier.verdict(entry.key.0, ns, cs).await.map_err(sql)?;
            if v.is_valid() {
                continue;
            }
            if let Some(orig) = rewrites.get(cs) {
                let source = verifier.verdict(entry.key.0, ns, orig).await.map_err(sql)?;
                if source.is_valid() {
                    if verifier
                        .attest(entry.key.0, cs, now())
                        .await
                        .map_err(sql)?
                        .is_some()
                    {
                        continue;
                    }
                    lost_by_rebase = true;
                }
            }
            unsigned.push(format!("{} {}", cs, status_word(v.status)));
        }
        if unsigned.is_empty() {
            return Ok(());
        }
        if lost_by_rebase {
            return Err(rpc(RpcError::LandNeedsClientRebase(info(vec![
                "the branch requires signed changes and a server rebase would drop the signatures; \
                 rebase locally, sign and push again"
                    .into(),
            ]))));
        }
        Err(rpc(RpcError::UnsignedChange(info(unsigned))))
    }
}

fn status_word(s: wire::VerificationStatus) -> &'static str {
    use wire::VerificationStatus as V;
    match s {
        V::Unspecified | V::Unsigned => "unsigned",
        V::Valid => "valid",
        V::UnknownKey => "unknown_key",
        V::KeyNotAuthor => "key_not_author",
        V::KeyExpired => "key_expired",
        V::KeyRevoked => "key_revoked",
        V::BadSignature => "bad_signature",
        V::Unsupported => "unsupported",
    }
}

#[async_trait]
impl RefGate for ServerGate {
    async fn check(&self, req: &GateRequest<'_>) -> Result<(), Status> {
        let who = who_of(&req.ctx.principal.id);
        if matches!(req.mode, GateMode::Submit) {
            return self.check_submitted(req, &who).await;
        }
        let trunk = format!("refs/heads/{}", req.entry.default_branch);
        let mut protected: Vec<&GateUpdate> = Vec::new();
        let mut drafts: Vec<&GateUpdate> = Vec::new();
        for u in req.updates {
            if u.user {
                if who.is_agent() {
                    drafts.push(u);
                }
                continue;
            }
            if who.is_agent() {
                // An agent's output is a stack of drafts under review; it never moves a shared ref.
                let status = rpc(RpcError::AgentScopeDenied(info(vec![
                    "agents write their own drafts only, never a shared ref".into(),
                    u.name.clone(),
                ])));
                return Err(self.refuse(req.ctx, &u.name, status).await);
            }
            if u.name.starts_with("refs/heads/") && self.protected(req.entry, &u.name).await? {
                protected.push(u);
            }
        }
        if protected.is_empty() && drafts.is_empty() {
            return Ok(());
        }
        let in_force = self.policies.in_force(req.entry).await?;
        for u in drafts {
            if let Err(status) = self.check_draft(req, &who, u, &in_force).await {
                return Err(self.refuse(req.ctx, &u.name, status).await);
            }
        }
        for u in protected {
            let is_trunk = u.name == trunk;
            if let Err(status) = self
                .check_protected(req, &who, u, is_trunk, &in_force)
                .await
            {
                // A secret refusal audits itself; the rest are policy refusals.
                let code = asc_rpc::server::status_rpc_error(&status).map(|e| e.code().to_owned());
                if code.as_deref() == Some("SECRET_DETECTED") {
                    return Err(status);
                }
                return Err(self.refuse(req.ctx, &u.name, status).await);
            }
        }
        Ok(())
    }
}
