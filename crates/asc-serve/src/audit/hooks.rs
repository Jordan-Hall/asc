//! The audit events of the `UpdateRefs` pipeline: one `repo.ref.update` per ref in the step-8
//! transaction, and one `push.refused` when a push is refused for a reason of authorisation,
//! policy or a hook. The pipeline calls these; they only build and write events.

use asc_rpc::server::{CallCtx, status_rpc_error};
use asc_store::refs::RefTxn;
use sqlx::{AnyConnection, AnyPool, Row};
use tonic::{Code, Status};

use super::actor::actor_of;
use super::{Actor, AuditError, NewEvent, Outcome, store};

/// Refs one push audits individually; a push of more audits this many and counts the rest in
/// `refs_total`.
pub const MAX_REFS_PER_EVENT_SET: usize = 64;

/// The typed errors that mean "this push was refused" rather than "this push failed".
const REFUSALS: &[&str] = &[
    "HOOK_REJECTED",
    "PATH_POLICY",
    "REF_NOT_SHARED",
    "NOT_FAST_FORWARD",
    "CHANGE_NOT_ALLOWED",
    "POLICY_DENIED",
    "SECRET_DETECTED",
    "UNSIGNED_CHANGE",
    "SIGNATURE_INVALID",
    "AGENT_SCOPE_DENIED",
    "AGENT_TOKEN_EXPIRED",
    "AGENT_TOKEN_REVOKED",
    "BUDGET_EXCEEDED",
    "USER_REFS_LIMIT",
    "QUOTA_EXCEEDED",
    "MIRROR_MODE",
    "REF_UNDO_PROTECTED",
];

/// The short code of a refusal: the typed error code, or `PERMISSION_DENIED` for a plain
/// permission failure. `None` for anything that is not a refusal (a conflict, an internal error,
/// a malformed request).
pub fn refusal_code(status: &Status) -> Option<String> {
    if let Some(e) = status_rpc_error(status) {
        let code = e.code();
        return REFUSALS.contains(&code).then(|| code.to_owned());
    }
    match status.code() {
        Code::PermissionDenied => Some("PERMISSION_DENIED".to_owned()),
        Code::Unauthenticated => Some("UNAUTHENTICATED".to_owned()),
        _ => None,
    }
}

/// The `push.refused` event of a refused push.
pub fn refused_event(ctx: &CallCtx, refs: &[String], via: &str, code: &str) -> NewEvent {
    let first = refs.first().cloned().unwrap_or_default();
    NewEvent::new("push.refused", actor_of(&ctx.principal.id))
        .repo(ctx.repo.as_str())
        .target(&first)
        .outcome(Outcome::Denied)
        .detail("ref", first)
        .detail("reason", code)
        .detail("code", code)
        .detail("via", via)
        .detail("refs_total", refs.len().to_string())
}

/// Writes the `push.refused` event; the refusal is returned to the pusher whether or not the
/// event could be written (a failure is logged).
pub async fn record_refused(pool: &AnyPool, ctx: &CallCtx, refs: &[String], via: &str, code: &str) {
    store::record_best_effort(pool, &refused_event(ctx, refs, via, code)).await;
}

/// Appends the `repo.ref.update` events of the ref transaction `txn` to the log, inside the
/// transaction that wrote it: one per ref the transaction changed, read back from the ref log rows
/// of its op id (so the old and new values are exactly the ones stored), at most
/// [`MAX_REFS_PER_EVENT_SET`] of them. The total is in each event.
pub async fn append_ref_updates(
    conn: &mut AnyConnection,
    repo_id: i64,
    repo: &str,
    txn: &RefTxn,
) -> Result<usize, AuditError> {
    let op = txn.op_id.as_bytes().to_vec();
    let total: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM ref_log WHERE repo_id = $1 AND op_id = $2",
    )
    .bind(repo_id)
    .bind(op.clone())
    .fetch_one(&mut *conn)
    .await?;
    let rows = sqlx::query(
        "SELECT name, old, new FROM ref_log WHERE repo_id = $1 AND op_id = $2 ORDER BY seq LIMIT $3",
    )
    .bind(repo_id)
    .bind(op)
    .bind(i64::try_from(MAX_REFS_PER_EVENT_SET).unwrap_or(64))
    .fetch_all(&mut *conn)
    .await?;
    let hex = |b: Option<Vec<u8>>| -> String {
        b.map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
            .unwrap_or_default()
    };
    let mut written = 0;
    for r in &rows {
        let name: String = r.try_get("name")?;
        let event = NewEvent::new("repo.ref.update", actor_of(&txn.actor.id))
            .repo(repo)
            .target(&name)
            .detail("ref", name.clone())
            .detail("old", hex(r.try_get("old")?))
            .detail("new", hex(r.try_get("new")?))
            .detail("via", txn.actor.via.clone())
            .detail("op_id", txn.op_id.to_string())
            .detail("refs_total", total.to_string());
        store::append_in_tx(&mut *conn, &event).await?;
        written += 1;
    }
    Ok(written)
}

/// The actor of the operator tools (`asc serve token`, `asc serve ssh-key`): they run against the
/// database with no request and no signed-in person, so the log names the tool.
pub fn operator() -> Actor {
    Actor::system("operator")
}

/// The scopes an access credential belongs to: its user's when it has one, else its repository's
/// when it is bound to one repository by name (a wildcard grant belongs to the instance only).
fn credential_event(action: &str, user: Option<&str>, repo: Option<&str>) -> NewEvent {
    let e = NewEvent::new(action, operator());
    match user.filter(|u| !u.is_empty()) {
        Some(user) => e.owner(user),
        None => match repo.filter(|r| r.contains('/') && !r.contains('*')) {
            Some(repo) => e.repo(repo),
            None => e,
        },
    }
}

/// `auth.token.created`. Never holds the token itself, only its public id.
pub fn token_created(
    id: &str,
    kind: &str,
    label: &str,
    expires: Option<i64>,
    user: Option<&str>,
    repo: Option<&str>,
) -> NewEvent {
    credential_event("auth.token.created", user, repo)
        .target(id)
        .detail("token", id)
        .detail("name", label)
        .detail("kind", kind)
        .detail(
            "expires_at",
            expires.map(|e| e.to_string()).unwrap_or_default(),
        )
}

/// `auth.token.revoked`.
pub fn token_revoked(id: &str, label: &str, user: Option<&str>, repo: Option<&str>) -> NewEvent {
    credential_event("auth.token.revoked", user, repo)
        .target(id)
        .detail("token", id)
        .detail("name", label)
}

/// `auth.ssh_key.added`. The fingerprint is public; the key is not stored here at all.
pub fn ssh_key_added(
    fingerprint: &str,
    title: &str,
    user: Option<&str>,
    repo: Option<&str>,
) -> NewEvent {
    credential_event("auth.ssh_key.added", user, repo)
        .target(fingerprint)
        .detail("fingerprint", fingerprint)
        .detail("title", title)
}

/// `auth.ssh_key.removed`.
pub fn ssh_key_removed(
    fingerprint: &str,
    title: &str,
    user: Option<&str>,
    repo: Option<&str>,
) -> NewEvent {
    credential_event("auth.ssh_key.removed", user, repo)
        .target(fingerprint)
        .detail("fingerprint", fingerprint)
        .detail("title", title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_rpc::errors::RpcError;
    use asc_rpc::server::rpc_status;

    #[test]
    fn typed_refusals_and_permission_failures_are_refusals() {
        let hook = rpc_status(&RpcError::HookRejected(Default::default()));
        assert_eq!(refusal_code(&hook).as_deref(), Some("HOOK_REJECTED"));
        let policy = rpc_status(&RpcError::PolicyDenied(Default::default()));
        assert_eq!(refusal_code(&policy).as_deref(), Some("POLICY_DENIED"));
        assert_eq!(
            refusal_code(&Status::permission_denied("no")).as_deref(),
            Some("PERMISSION_DENIED")
        );
        assert_eq!(
            refusal_code(&Status::unauthenticated("no")).as_deref(),
            Some("UNAUTHENTICATED")
        );
    }

    #[test]
    fn conflicts_and_failures_are_not_refusals() {
        let conflict = rpc_status(&RpcError::RefConflict(Default::default()));
        assert_eq!(refusal_code(&conflict), None);
        assert_eq!(refusal_code(&Status::internal("x")), None);
        assert_eq!(refusal_code(&Status::invalid_argument("x")), None);
        assert_eq!(refusal_code(&Status::unavailable("x")), None);
    }
}
