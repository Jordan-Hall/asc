//! What an agent may do to refs, enforced inside the `UpdateRefs` pipeline.
//!
//! The pipeline calls the gate for every ref update, whatever route brought it there (a push,
//! `Commit` with an `update_ref`, a land, a restack). For a principal that is not an agent the
//! gate does nothing. For an agent:
//!
//! * **Never trunk.** The only refs an agent writes are in its own token's namespace,
//!   `refs/heads/agent/<its name>/<its token id>/<anything>`, or the ref of a run its token
//!   started, `refs/heads/agent/<its name>/run-<n>`: its own drafts, under the namespace of the
//!   person it acts for. Two tokens of one agent name never write each other's branches. A land, a tag, any shared
//!   branch and another agent's branch are refused with `AGENT_SCOPE_DENIED` before a byte is
//!   ingested ([`AgentGate::check_refs`]).
//! * **Scope.** The paths of the changesets the update adds (everything reachable from the new
//!   tip that is not already on trunk or on the ref) must lie inside the token's path globs *and*
//!   inside what the repository policy admits an agent of this name (`default_scope`,
//!   `denied_paths`, the path rules). A path outside either is refused, all of them listed.
//! * **Budget.** The number of changes and distinct paths is checked against the policy's
//!   per-run caps, the token's per-run cap and the token's lifetime limits, and charged with
//!   conditional updates so concurrent updates cannot pass a limit together. A charge for an
//!   update that does not commit is given back ([`AgentGate::settle`]).
//! * **Switches.** The instance switch `[agents] enabled` and the policy's `[agents] enabled`
//!   must both be on.
//! * **Fail closed.** An unusable policy file, a token that cannot be read, a history too long
//!   to walk: the update is refused.

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::policy::{Kind, PathGlob, Who};
use asc_rpc::server::CallCtx;
use async_trait::async_trait;
use tonic::Status;

use super::Core;
use super::errors;
use super::repo::PolicyState;
use super::tokens::{AgentPrincipal, Refusal, parse_principal};
use crate::ai::clock;
use crate::registry::RepoEntry;

/// Most changesets one update may add before the gate refuses to check it.
pub const MAX_ADDED: usize = 10_000;
/// Most refused paths listed in an error.
pub const MAX_REPORTED: usize = 20;

/// One ref change as the gate is told.
#[derive(Clone, Debug)]
pub struct RefChange {
    /// The name as pushed (`refs/heads/agent/fixer/run-1`).
    pub name: String,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
}

/// What was charged to a token for an update, to give back if the update does not commit.
#[derive(Debug)]
pub struct Charge {
    token_id: String,
    changes: u64,
    fresh_paths: Vec<String>,
}

/// The hook the `UpdateRefs` pipeline calls (`PipelineHooks::set_agent_gate`).
#[async_trait]
pub trait AgentGate: Send + Sync {
    /// Before anything is ingested: may this principal write these refs at all? `land` is true
    /// for a LAND update. Cheap; a non-agent passes.
    async fn check_refs(
        &self,
        ctx: &CallCtx,
        land: bool,
        refs: &[(String, bool)],
    ) -> Result<(), Status>;

    /// After the closure is ingested, before the refs move: scope and budget. `None`: not an
    /// agent, nothing charged.
    async fn charge(
        &self,
        ctx: &CallCtx,
        entry: &RepoEntry,
        updates: &[RefChange],
    ) -> Result<Option<Charge>, Status>;

    /// The update committed (`true`) or did not; an update that did not gets its charge back.
    async fn settle(&self, charge: Charge, committed: bool);
}

/// The gate of a server.
pub struct ServerAgentGate {
    core: Arc<Core>,
}

impl ServerAgentGate {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }
}

/// Whether `name` is `refs/heads/agent/<agent>/<token id>/<something>`, the namespace of one
/// token, or `refs/heads/agent/<agent>/run-<n>`, a run ref of the agent (whose run the caller
/// must also own: see [`ServerAgentGate::check_refs`]).
pub fn is_agent_ref(agent: &AgentPrincipal, name: &str) -> bool {
    name.starts_with("refs/heads/") && crate::refpolicy::agent_ref_shape(agent, name)
}

#[async_trait]
impl AgentGate for ServerAgentGate {
    async fn check_refs(
        &self,
        ctx: &CallCtx,
        land: bool,
        refs: &[(String, bool)],
    ) -> Result<(), Status> {
        let Some(p) = parse_principal(&ctx.principal.id) else {
            return Ok(());
        };
        if land {
            return Err(errors::scope_denied(vec![
                "an agent never lands a change".into(),
            ]));
        }
        let mut bad: Vec<String> = Vec::new();
        for (name, _) in refs {
            let own = match crate::refpolicy::agent_ref_kind(&p, name) {
                // Inside this token's own namespace.
                Some(crate::refpolicy::AgentRefKind::Token) if name.starts_with("refs/heads/") => {
                    true
                }
                // A run ref belongs to the run's own token and to nobody else's; a run that does
                // not exist, or cannot be read, owns nothing.
                Some(crate::refpolicy::AgentRefKind::Run(run))
                    if name.starts_with("refs/heads/") =>
                {
                    matches!(
                        self.core.runs.get(run).await,
                        Ok(Some(r)) if r.token_id == p.token_id
                    )
                }
                _ => false,
            };
            if !own && bad.len() < MAX_REPORTED {
                bad.push(name.clone());
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            Err(errors::scope_denied(bad))
        }
    }

    async fn charge(
        &self,
        ctx: &CallCtx,
        entry: &RepoEntry,
        updates: &[RefChange],
    ) -> Result<Option<Charge>, Status> {
        let Some(p) = parse_principal(&ctx.principal.id) else {
            return Ok(None);
        };
        let core = &self.core;
        if !core.cfg.enabled {
            return Err(errors::disabled("instance"));
        }
        let now = clock::now();
        let token = core
            .tokens
            .get(&p.token_id)
            .await
            .map_err(|e| {
                crate::update::internal("agent gate: token", &crate::ai::store::class(&e))
            })?
            .ok_or_else(errors::token_revoked)?;
        if token.revoked_at > 0 {
            return Err(errors::token_revoked());
        }
        if token.expires_at <= now {
            return Err(errors::token_expired());
        }
        if !token.repo.eq_ignore_ascii_case(ctx.repo.as_str()) || token.agent != p.name {
            return Err(errors::scope_denied(vec![
                "the token is for another repository".into(),
            ]));
        }
        if token.exhausted {
            let dim = if token.limits.cost > 0 && token.spent.cost >= token.limits.cost {
                "cost"
            } else {
                "tokens"
            };
            return Err(errors::budget_exceeded(dim));
        }
        let state = core.view.policy(entry).await?;
        let policy = match &*state {
            PolicyState::Absent => None,
            PolicyState::Valid(p) => Some(p.clone()),
            PolicyState::Invalid(why) => {
                return Err(errors::policy_denied(
                    format!(
                        "the repository policy cannot be used: {why}; fix .asc/policy.toml on trunk"
                    ),
                    vec![],
                ));
            }
        };
        if let Some(pol) = &policy
            && !pol.agents.enabled
        {
            return Err(errors::disabled("repository policy"));
        }
        let who = Who::new(Kind::Agent, p.name.clone());

        // The changesets this update adds: reachable from a new tip, not from the old value of
        // the ref and not from trunk.
        let trunk = core.view.trunk_tip(entry).await?;
        let mut added: Vec<ChangesetId> = Vec::new();
        for u in updates {
            let Some(RefTarget::Changeset(new)) = u.new else {
                continue;
            };
            let mut exclude = Vec::new();
            if let Some(RefTarget::Changeset(old)) = u.old {
                exclude.push(old);
            }
            exclude.extend(trunk);
            let Some(ids) = core.view.range(entry, new, exclude, MAX_ADDED).await? else {
                return Err(Status::failed_precondition(format!(
                    "this update adds more than {MAX_ADDED} changesets and cannot be checked; push it in parts"
                )));
            };
            for id in ids {
                if !added.contains(&id) {
                    added.push(id);
                }
            }
        }
        if added.is_empty() {
            return Ok(None);
        }

        let globs: Vec<PathGlob> = token
            .paths
            .iter()
            .map(|g| PathGlob::parse(g).map_err(|_| Status::internal("internal error")))
            .collect::<Result<_, _>>()?;
        let mut paths: Vec<String> = Vec::new();
        for id in &added {
            for path in core.view.changed_paths(entry, *id).await? {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        let mut refused: Vec<String> = Vec::new();
        for path in &paths {
            let in_token = globs.is_empty() || globs.iter().any(|g| g.matches(path.as_bytes()));
            let in_policy = policy
                .as_ref()
                .is_none_or(|pol| pol.check_write(&who, path.as_bytes()).is_ok());
            if !(in_token && in_policy) && refused.len() < MAX_REPORTED {
                refused.push(path.clone());
            }
        }
        if !refused.is_empty() {
            return Err(errors::scope_denied(refused));
        }

        // Per-run caps first (cheap, no state), then the lifetime budget.
        let n = added.len() as u64;
        if token.max_changes_per_run > 0 && n > u64::from(token.max_changes_per_run) {
            return Err(errors::budget_exceeded("changes"));
        }
        if let Some(pol) = &policy
            && let Err(d) = pol.check_run(&who, added.len(), paths.len())
        {
            let dim = if d.reason.contains("paths") {
                "paths"
            } else {
                "changes"
            };
            return Err(errors::budget_exceeded(dim));
        }
        core.tokens
            .reserve_changes(&token.id, n)
            .await
            .map_err(refusal)?;
        let fresh = match core.tokens.reserve_paths(&token.id, &paths).await {
            Ok(f) => f,
            Err(e) => {
                core.tokens.release_changes(&token.id, n).await;
                return Err(refusal(e));
            }
        };
        Ok(Some(Charge {
            token_id: token.id,
            changes: n,
            fresh_paths: fresh,
        }))
    }

    async fn settle(&self, charge: Charge, committed: bool) {
        if committed {
            return;
        }
        self.core
            .tokens
            .release_changes(&charge.token_id, charge.changes)
            .await;
        self.core
            .tokens
            .release_paths(&charge.token_id, &charge.fresh_paths)
            .await;
    }
}

fn refusal(r: Refusal) -> Status {
    match r {
        Refusal::Budget(d) => errors::budget_exceeded(d),
        Refusal::Gone => errors::token_revoked(),
        Refusal::Sql(class) => crate::update::internal("agent gate: budget", &class),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixer() -> AgentPrincipal {
        AgentPrincipal {
            name: "fixer".into(),
            owner_id: 7,
            token_id: "at_one".into(),
        }
    }

    #[test]
    fn only_the_tokens_own_branches_qualify() {
        let a = fixer();
        assert!(is_agent_ref(&a, "refs/heads/agent/fixer/run-1"));
        assert!(is_agent_ref(&a, "refs/heads/agent/fixer/at_one/a/b/c"));
        for bad in [
            // another token of the same agent name
            "refs/heads/agent/fixer/at_two/x",
            "refs/heads/agent/fixer/at_one2/x",
            // no token namespace
            "refs/heads/agent/fixer/a/b/c",
            "refs/heads/agent/fixer/run-x",
            "refs/heads/agent/fixer/run-",
            "refs/heads/agent/fixer/at_one",
            "refs/heads/agent/fixer/at_one/",
            "refs/heads/agent/fixer",
            "refs/heads/agent/fixer/",
            "refs/heads/agent/fixer2/run-1",
            "refs/heads/agent/other/run-1",
            "refs/heads/master",
            "refs/heads/agent/fixer/at_one/../../master",
            "refs/heads/agent/fixer/at_one//x",
            "refs/heads/agent/fixer/at_one/./x",
            "refs/heads/agent/fixer/at_one/x\\y",
            "refs/tags/agent/fixer/at_one/v1",
            "refs/users/7/heads/agent/fixer/at_one/x",
            "agent/fixer/at_one/x",
            "refs/heads/agent/fixer/at_one/x
",
        ] {
            assert!(!is_agent_ref(&a, bad), "{bad:?}");
        }
    }
}
