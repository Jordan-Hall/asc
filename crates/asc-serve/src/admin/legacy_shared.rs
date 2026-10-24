//! `legacy_shared` conversion (SPEC §21.6.2, PLAN B26): when a pull request closes, its head
//! branch (kept shared by `branchless enable` and listed in `repos.legacy_shared`) becomes a user
//! ref of the PR author.
//!
//! [`convert_closed`] is called by the forge close event for one `(repo, branch, author)`. It
//! fails closed and changes nothing unless all of these hold:
//!
//! * the branch is listed in `legacy_shared` (so a shared branch such as the default branch can
//!   never be converted by a forged or mistaken event);
//! * the branch does not match `shared_refs`;
//! * no other open PR still has the branch as its head ([`ForgeFacts::open_pr_heads`]);
//! * the author is a positive user id that is under [`MAX_USER_REFS`], and the author's user ref
//!   of that name is absent or already holds the branch value.
//!
//! The ref move (delete of `refs/heads/<branch>`, create of the user ref, `post_receive` outbox
//! row with the deletion) and the removal of the name from `legacy_shared` are one transaction,
//! guarded by the compare-and-swap on the branch tip, so a second call finds the name gone and
//! does nothing ([`Outcome::NotListed`]).

use std::collections::BTreeSet;
use std::sync::Arc;

use asc_core::path::RefName;
use asc_rpc::server::{RepoName, wire};
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs};
use prost::Message;
use sqlx::Row;
use tonic::Status;
use uuid::Uuid;

use super::branchless::{ACTOR, AGENT_TTL_SECS, ForgeFacts, VIA, is_agent_branch};
use crate::refpolicy::{self, MAX_USER_REFS, RepoPolicy};
use crate::registry::{Registry, RegistryError};
use crate::update::{EffectRef, OutboxEffects, internal, target_hex};

const HEADS: &str = "refs/heads/";

/// What [`convert_closed`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The branch became the author's user ref.
    Converted,
    /// The name is not in `legacy_shared` (never listed, or already converted): nothing changed.
    NotListed,
    /// Another open PR still has this head branch: it stays shared.
    StillOpen,
    /// The author's user ref of that name holds another value, or the author is at the user ref
    /// limit: the branch stays shared and listed.
    Kept,
}

fn invalid(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

/// Converts `branch` (a short name, no `refs/heads/`) to a user ref of `author`.
pub async fn convert_closed(
    registry: &Arc<Registry>,
    repo: &RepoName,
    branch: &str,
    author: i64,
    facts: &dyn ForgeFacts,
    now: i64,
) -> Result<Outcome, Status> {
    if author <= 0 {
        return Err(invalid("the PR author id must be positive"));
    }
    if branch.is_empty() || branch.starts_with("refs/") {
        return Err(invalid("expected a short branch name"));
    }
    let entry = registry.open(repo).await.map_err(|e| match e {
        RegistryError::NotFound(n) => Status::not_found(format!("repository {n} does not exist")),
        e => internal("registry", &e),
    })?;
    let pool = registry.pool();
    let full = format!("{HEADS}{branch}");
    let policy = RepoPolicy::load(pool, entry.key.0).await?;
    if policy.is_shared(&full) {
        return Err(invalid("a shared_refs branch is never converted"));
    }
    if facts.open_pr_heads().await?.contains(branch) {
        return Ok(Outcome::StillOpen);
    }
    let refs = registry.refs();
    let name = RefName::new(full.as_bytes()).map_err(|e| invalid(e.to_string()))?;
    let stored = format!("{}heads/{branch}", refpolicy::user_prefix(author));
    let user_name = RefName::new(stored.as_bytes()).map_err(|e| invalid(e.to_string()))?;

    let old_legacy: Option<String> = sqlx::query("SELECT legacy_shared FROM repos WHERE id = $1")
        .bind(entry.key.0)
        .fetch_one(pool)
        .await
        .and_then(|r| r.try_get("legacy_shared"))
        .map_err(|e| internal("repos", &e))?;
    let mut legacy: BTreeSet<String> = match old_legacy.as_deref() {
        Some(j) => serde_json::from_str(j).map_err(|e| internal("repos.legacy_shared", &e))?,
        None => BTreeSet::new(),
    };
    if !legacy.remove(branch) {
        return Ok(Outcome::NotListed);
    }
    let new_legacy = (!legacy.is_empty())
        .then(|| serde_json::to_string(&legacy).map_err(|e| internal("legacy_shared", &e)))
        .transpose()?;

    let tip = refs
        .get(entry.key, &name)
        .await
        .map_err(|e| internal("refs", &e))?;
    let mut updates = Vec::new();
    let mut results = Vec::new();
    let mut effects = Vec::new();
    if let Some(tip) = tip {
        let old = tip.target;
        let existing = refs
            .get(entry.key, &user_name)
            .await
            .map_err(|e| internal("refs", &e))?;
        let create = match existing {
            Some(e) if e.target == old => false,
            Some(_) => return Ok(Outcome::Kept),
            None => {
                let held = refs
                    .list(entry.key, &refpolicy::user_prefix(author))
                    .await
                    .map_err(|e| internal("refs", &e))?
                    .len();
                if i64::try_from(held).unwrap_or(i64::MAX) >= MAX_USER_REFS {
                    return Ok(Outcome::Kept);
                }
                true
            }
        };
        updates.push(RefUpdate {
            name: name.clone(),
            expect: Expect::Value(old),
            new: None,
            expires_at: None,
        });
        results.push(wire::RefResult {
            name: full.clone(),
            old: Some(wire::RefTarget::from(&old)),
            new: None,
            user: false,
        });
        effects.push(EffectRef {
            name: full.clone(),
            old: Some(target_hex(&old)),
            new: None,
        });
        if create {
            updates.push(RefUpdate {
                name: user_name,
                expect: Expect::Absent,
                new: Some(old),
                expires_at: is_agent_branch(branch).then_some(now.saturating_add(AGENT_TTL_SECS)),
            });
            results.push(wire::RefResult {
                name: stored,
                old: None,
                new: Some(wire::RefTarget::from(&old)),
                user: true,
            });
        }
    }
    let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
    registry.guard(&mut tx, &entry).await?;
    // A branch that is already gone only loses its listing.
    if !updates.is_empty() {
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: ACTOR.into(),
                via: VIA.into(),
            },
            reason: "legacy-shared-convert".into(),
            op_id: Uuid::new_v4(),
        };
        match refs.update_in_tx(&mut tx, entry.key, &txn).await {
            Ok(()) => {}
            Err(RefError::Conflict(c)) => {
                return Err(Status::aborted(format!("{} moved; retry", c.name)));
            }
            Err(e) => return Err(internal("refs", &e)),
        }
        let at = crate::outbox::now();
        let response = wire::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: String::new(),
        };
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: &["post_receive"],
            pusher: ACTOR,
            refs: effects,
        })
        .map_err(|e| internal("outbox", &e))?;
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(txn.op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(response.encode_to_vec())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("op_results", &e))?;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(entry.key.0)
        .bind(txn.op_id.as_bytes().to_vec())
        .bind(outbox)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("outbox", &e))?;
    }
    sqlx::query("UPDATE repos SET legacy_shared = $1 WHERE id = $2")
        .bind(new_legacy)
        .bind(entry.key.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("repos", &e))?;
    tx.commit().await.map_err(|e| internal("commit", &e))?;
    Ok(Outcome::Converted)
}
