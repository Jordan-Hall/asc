//! The hub's `policy_changed`: an outbox effect that runs after trunk moved.
//!
//! The policy in force is the file `.asc/policy.toml` on the trunk tip, so it changes when an
//! update of trunk changes that file. The effect compares the policy of the old tip with the new
//! one and, when they differ, tells the recent contributors of the repository (the people the
//! rules apply to; the hub drops those who cannot read it). The person who pushed the change is
//! not told. A change the file made is told once: the key is the new tip.

use std::sync::Arc;

use asc_core::id::RefTarget;
use async_trait::async_trait;
use sqlx::AnyPool;

use super::{Event, Kind, MAX_USERS};
use crate::agents::repo::{PolicyState, RepoView};
use crate::codeintel::source::RegistryChanges;
use crate::outbox::{Effect, EffectError, Job};
use crate::registry::Registry;

/// The name rows list in `effects`.
pub const EFFECT: &str = "notify.policy";

/// Refs one delivery looks at.
const MAX_REFS: usize = 256;

/// How the policy changed from `old` to `new`, or `None` when it did not.
pub fn change(old: &PolicyState, new: &PolicyState) -> Option<&'static str> {
    match (old, new) {
        (PolicyState::Absent, PolicyState::Absent) => None,
        (PolicyState::Absent, PolicyState::Valid(_)) => Some("A policy was added."),
        (PolicyState::Valid(_), PolicyState::Absent) => Some("The policy was removed."),
        (PolicyState::Valid(a), PolicyState::Valid(b)) => {
            (a != b).then_some("The rules of the repository changed.")
        }
        (PolicyState::Invalid(a), PolicyState::Invalid(b)) => {
            (a != b).then_some("The policy file cannot be used.")
        }
        (_, PolicyState::Invalid(_)) => Some("The policy file cannot be used."),
        (PolicyState::Invalid(_), _) => Some("The policy file can be used again."),
    }
}

/// The logins of the people who most recently made versions of changes in the repository, newest
/// first, at most [`MAX_USERS`], without `except`.
pub async fn contributors(pool: &AnyPool, ns: &[u8], except: &str) -> Vec<String> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT actor FROM change_versions WHERE ns = $1 GROUP BY actor ORDER BY MAX(at) DESC LIMIT 200",
    )
    .bind(ns.to_vec())
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for a in rows {
        if let Some(h) = crate::review::handle_of(&a)
            && h != except
            && !out.contains(&h)
        {
            out.push(h);
        }
        if out.len() >= MAX_USERS {
            break;
        }
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() || s.len() > 128 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// The `notify.policy` effect.
pub struct PolicyChanged {
    registry: Arc<Registry>,
    source: RegistryChanges,
    view: RepoView,
}

impl PolicyChanged {
    pub fn effect(registry: &Arc<Registry>) -> Arc<dyn Effect> {
        Arc::new(Self {
            registry: registry.clone(),
            source: RegistryChanges::new(registry.clone()),
            view: RepoView::new(registry.clone()),
        })
    }
}

#[async_trait]
impl Effect for PolicyChanged {
    fn kind(&self) -> &'static str {
        EFFECT
    }

    async fn run(&self, job: &Job) -> Result<(), EffectError> {
        let refs = job
            .doc
            .get("refs")
            .and_then(|v| v.as_array())
            .map_or(&[][..], Vec::as_slice);
        if refs.len() > MAX_REFS {
            return Ok(());
        }
        let Some(entry) = self.source.entry(job.repo_id).await? else {
            return Ok(());
        };
        let trunk = format!("refs/heads/{}", entry.default_branch);
        let pusher = job
            .doc
            .get("pusher")
            .and_then(|v| v.as_str())
            .and_then(crate::review::handle_of)
            .unwrap_or_default();
        for r in refs {
            if r.get("name").and_then(|v| v.as_str()) != Some(trunk.as_str()) {
                continue;
            }
            let target = |k: &str| {
                let bytes = unhex(r.get(k)?.as_str()?)?;
                match RefTarget::from_bytes(&bytes) {
                    Ok(RefTarget::Changeset(c)) => Some(c),
                    _ => None,
                }
            };
            let (Some(old), Some(new)) = (target("old"), target("new")) else {
                continue; // created or deleted: there is nothing to compare
            };
            let err = |e: tonic::Status| EffectError(format!("policy: {}", e.message()));
            let before = self.view.policy_at(&entry, old).await.map_err(err)?;
            let after = self.view.policy_at(&entry, new).await.map_err(err)?;
            let Some(how) = change(&before, &after) else {
                continue;
            };
            let people = contributors(self.registry.pool(), entry.ns.as_bytes(), &pusher).await;
            let repo = entry.name.as_str();
            let key: String = new
                .as_bytes()
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect();
            super::record(
                self.registry.pool(),
                repo,
                Event::new(Kind::PolicyChanged, format!("The policy of {repo} changed"))
                    .users(people)
                    .detail(how)
                    .path(super::paths::policy(repo))
                    .dedup(format!("policy:{key}")),
            )
            .await
            .map_err(|e| EffectError(format!("notify: {e}")))?;
        }
        Ok(())
    }
}
