//! User-ref authorisation and limits (SPEC §21.5.1, §21.3.2, §21.3.7; PLAN B08b).
//!
//! A user ref `refs/users/<owner-id>/heads/<n>` belongs to one user. [`authorize`] admits a write
//! only to the owner, [`check_cap`] bounds how many refs one user holds in one repository
//! (`USER_REFS_LIMIT`), and [`QuotaDenied`] is what a [`crate::update::PushHooks`] quota check
//! returns (`QUOTA_EXCEEDED`). User refs run no pre-receive, so the quota is the only size policy
//! they meet.

use asc_core::path::RefName;
use asc_rpc::errors::RpcError;
use asc_store::refs::{Refs, RepoKey};
use tonic::Status;

use crate::refpolicy::{Class, USER_NS};
use crate::update::{info, internal, rpc};

/// `[cloud] max_user_refs`: user refs per user per repository.
pub const DEFAULT_MAX_USER_REFS: usize = 1_000;
/// The most user refs this module counts, whatever the configured cap.
const COUNT_CEILING: usize = 1_000_000;

/// A quota check refused the write; the text is shown to the user (`QUOTA_EXCEEDED`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct QuotaDenied(pub String);

/// Only the owner writes a user ref. `caller` is the numeric id of the authenticated principal.
/// [`crate::refpolicy::RepoPolicy::classify`] already maps a push into the caller's own namespace;
/// this is the check that stays true if a caller ever hands in a stored name.
pub fn authorize(caller: Option<i64>, class: &Class) -> Result<(), Status> {
    match class {
        Class::User { owner, stored } => {
            if caller != Some(*owner) {
                return Err(Status::permission_denied(
                    "a user ref can only be changed by its owner",
                ));
            }
            match crate::refpolicy::parse_user_ref(stored) {
                Some((o, _)) if o == *owner => Ok(()),
                _ => Err(Status::permission_denied("not a user ref of this caller")),
            }
        }
        _ => Ok(()),
    }
}

/// Whether the stored ref name `name` belongs to `caller`.
pub fn owns(caller: Option<i64>, name: &str) -> bool {
    name.starts_with(USER_NS)
        && matches!(
            (caller, crate::refpolicy::parse_user_ref(name)),
            (Some(c), Some((o, _))) if c == o
        )
}

/// Refuses with `USER_REFS_LIMIT` when creating `creates` (stored names, all absent today) would
/// leave `owner` with more than `max` user refs in the repository. Reads only the owner's prefix.
pub async fn check_cap<R: Refs + ?Sized>(
    refs: &R,
    repo: RepoKey,
    owner: i64,
    max: usize,
    creates: &[&RefName],
) -> Result<(), Status> {
    if creates.is_empty() {
        return Ok(());
    }
    let prefix = format!("{USER_NS}{owner}/heads/");
    let held = refs
        .list(repo, &prefix)
        .await
        .map_err(|e| internal("refs", &e))?
        .len()
        .min(COUNT_CEILING);
    if held.saturating_add(creates.len()) > max.min(COUNT_CEILING) {
        return Err(rpc(RpcError::UserRefsLimit(info(vec![format!(
            "you already hold {held} branches in this repository; delete some (limit {max})"
        )]))));
    }
    Ok(())
}

/// The error a refused quota check becomes.
pub fn quota_status(d: &QuotaDenied) -> Status {
    rpc(RpcError::QuotaExceeded(info(vec![d.0.clone()])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(owner: i64, stored: &str) -> Class {
        Class::User {
            owner,
            stored: stored.into(),
        }
    }

    #[test]
    fn only_the_owner_writes_a_user_ref() {
        let c = user(5, "refs/users/5/heads/x");
        assert!(authorize(Some(5), &c).is_ok());
        assert!(authorize(Some(6), &c).is_err());
        assert!(authorize(None, &c).is_err());
        assert!(authorize(Some(5), &user(5, "refs/users/6/heads/x")).is_err());
        assert!(authorize(None, &Class::Shared).is_ok());
        assert!(owns(Some(5), "refs/users/5/heads/x"));
        assert!(!owns(Some(5), "refs/users/50/heads/x"));
        assert!(!owns(Some(5), "refs/heads/x"));
    }
}
