//! Calls made for a person by the host service (the hub), on its forwarded claim.
//!
//! The hub calls with its own service credential and, when a signed-in person is behind the
//! call, a signed claim of what its permission model lets that person do on the repository
//! (`read`, `write` or `admin`; see [`asc_rpc::forwarded`]). Every repository-scoped service that
//! builds its [`CallCtx`] through `Env::context` gets the same rule:
//!
//! * a caller that is not the host service is decided by its own rights; metadata is ignored;
//! * the host service with no claim is system work, decided as it always was;
//! * the host service with a claim that verifies for this repository and covers the access the
//!   RPC needs acts as the person of the claim, with the claim as the whole of its authority (a
//!   `read` claim does not write, whatever the service credential could do);
//! * the host service with a claim that does not verify (wrong key, subject, age or spelling) or
//!   whose role is too small is `PERMISSION_DENIED`, never system work. Fail closed.
//!
//! The claim stays on `CallCtx::admin`, so a handler tells a relayed call from a direct one with
//! [`relayed`], and the audit events of the call name the person as the actor and the host service
//! as the relay ([`event`]).

use asc_rpc::auth::{Access, AdminProof, Authorizer, Principal};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{CallCtx, RepoName};
use tonic::Status;
use tonic::metadata::MetadataMap;

use crate::audit::actor::actor_of;
use crate::audit::{Actor, NewEvent, store};
use crate::authz::claim::{HostCall, classify};

/// The role a claim needs to cover `access`.
pub(crate) fn role_of(access: Access) -> Role {
    match access {
        Access::Read => Role::Read,
        Access::Write => Role::Write,
        Access::Admin => Role::Admin,
    }
}

/// The principal and admin proof a call acts with, given the credential's own principal.
pub(crate) fn act_as(
    authorizer: &dyn Authorizer,
    principal: Principal,
    meta: &MetadataMap,
    repo: &RepoName,
    access: Access,
) -> Result<(Principal, AdminProof), Status> {
    match classify(authorizer, &principal.id, meta, repo.as_str()) {
        HostCall::Person(c) if c.allows(role_of(access)) => {
            let id = c.user.clone();
            Ok((
                Principal { id },
                AdminProof::default().with_forwarded(Some(c)),
            ))
        }
        HostCall::Person(_) | HostCall::Refused => Err(Status::permission_denied(
            "the claim the host service forwarded does not cover this call",
        )),
        HostCall::NotHost | HostCall::System => Ok((principal, AdminProof::default())),
    }
}

/// Whether the call was made by the host service for the person in `ctx.principal`.
pub(crate) fn relayed(ctx: &CallCtx) -> bool {
    ctx.admin.forwarded_claim().is_some()
}

/// The audit event of `action` by the caller of `ctx`: the person as actor and, on a relayed call,
/// the host service as the relay.
pub(crate) fn event(ctx: &CallCtx, action: &str, target: &str) -> NewEvent {
    let mut e = NewEvent::new(action, actor_of(&ctx.principal.id))
        .repo(ctx.repo.as_str())
        .target(target);
    if relayed(ctx) {
        e = e.on_behalf_of(Actor::new("host", "host"));
    }
    e
}

/// Writes `event`; a failure is logged and never fails the call (the change is already made).
pub(crate) async fn record(pool: &sqlx::AnyPool, event: NewEvent) {
    store::record_best_effort(pool, &event).await;
}
