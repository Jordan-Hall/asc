//! Idempotency of mutating calls that name no repository (`x-asc-op-id`).
//!
//! `UpdateRefs` keeps its op results per repository; the AI key, agent token and agent run calls
//! keep theirs here. A retry with the same id and the same caller and method gets the first answer.
//! A call whose answer holds a secret (a new agent token) stores no answer: a retry is told the
//! operation already happened and that the secret was shown once.

use sqlx::{AnyPool, Row};
use tonic::Status;
use uuid::Uuid;

use super::clock;

/// A pending operation older than this is considered lost (its process died).
const STALE_SECS: i64 = 10 * 60;
/// Rows older than this are deleted when a new one is written.
const KEEP_SECS: i64 = 3 * 86_400;

/// Where a call stands.
#[derive(Debug, PartialEq, Eq)]
pub enum OpState {
    /// First time: run the call, then [`Ops::finish`].
    New,
    /// Already done; this is the stored answer.
    Done(Vec<u8>),
    /// Already done, and the answer was not kept because it held a secret.
    Consumed,
}

/// The store.
#[derive(Clone)]
pub struct Ops {
    pool: AnyPool,
}

impl Ops {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    /// Claims `op` for `principal` and `method`.
    pub async fn begin(&self, op: Uuid, principal: &str, method: &str) -> Result<OpState, Status> {
        let internal = |e: &sqlx::Error| {
            tracing::error!(error = %super::store::class(e), "service_ops");
            Status::internal("internal error")
        };
        let now = clock::now();
        let id = op.to_string();
        let r = sqlx::query(
            "INSERT INTO service_ops (op_id, principal, method, state, result, at) \
             VALUES ($1, $2, $3, 0, $4, $5) ON CONFLICT (op_id) DO NOTHING",
        )
        .bind(id.clone())
        .bind(principal.to_owned())
        .bind(method.to_owned())
        .bind(Vec::<u8>::new())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| internal(&e))?;
        if r.rows_affected() == 1 {
            // Old rows go with the next write; a failure here does not matter.
            let _ = sqlx::query("DELETE FROM service_ops WHERE at < $1")
                .bind(now - KEEP_SECS)
                .execute(&self.pool)
                .await;
            return Ok(OpState::New);
        }
        let row = sqlx::query(
            "SELECT principal, method, state, result, at FROM service_ops WHERE op_id = $1",
        )
        .bind(id.clone())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| internal(&e))?;
        let Some(row) = row else {
            return Err(Status::aborted("retry the call"));
        };
        let same = row
            .try_get::<String, _>("principal")
            .is_ok_and(|p| p == principal)
            && row
                .try_get::<String, _>("method")
                .is_ok_and(|m| m == method);
        if !same {
            return Err(Status::invalid_argument(
                "the operation id was used for another call",
            ));
        }
        let state: i64 = row.try_get("state").map_err(|e| internal(&e))?;
        match state {
            1 => Ok(OpState::Done(
                row.try_get("result").map_err(|e| internal(&e))?,
            )),
            2 => Ok(OpState::Consumed),
            _ => {
                let at: i64 = row.try_get("at").map_err(|e| internal(&e))?;
                if now - at > STALE_SECS {
                    // The first attempt died. Take the id over; a client that sees an effect
                    // twice from a crashed call still gets one answer per id.
                    sqlx::query(
                        "UPDATE service_ops SET at = $2 WHERE op_id = $1 AND state = 0 AND at = $3",
                    )
                    .bind(id)
                    .bind(now)
                    .bind(at)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| internal(&e))?;
                    Ok(OpState::New)
                } else {
                    Err(Status::aborted(
                        "the operation is still running; retry shortly",
                    ))
                }
            }
        }
    }

    /// Records the answer (`None`: the answer holds a secret and is not kept).
    pub async fn finish(&self, op: Uuid, result: Option<Vec<u8>>) {
        let (state, bytes) = match result {
            Some(b) => (1_i64, b),
            None => (2_i64, Vec::new()),
        };
        if let Err(e) =
            sqlx::query("UPDATE service_ops SET state = $2, result = $3 WHERE op_id = $1")
                .bind(op.to_string())
                .bind(state)
                .bind(bytes)
                .execute(&self.pool)
                .await
        {
            tracing::error!(error = %super::store::class(&e), "service_ops finish");
        }
    }

    /// Forgets a claim whose call failed, so the client can retry with the same id.
    pub async fn abandon(&self, op: Uuid) {
        let _ = sqlx::query("DELETE FROM service_ops WHERE op_id = $1 AND state = 0")
            .bind(op.to_string())
            .execute(&self.pool)
            .await;
    }
}
