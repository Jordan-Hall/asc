//! Who is calling a service that names no repository, and the idempotency wrapper of its
//! mutating calls. Shared by the AI key, agent token, agent run and workflow services.

use std::future::Future;
use std::sync::Arc;

use asc_rpc::auth::{Authorizer, Credential, Principal};
use prost::Message;
use tonic::metadata::MetadataMap;
use tonic::{Extensions, Status};
use uuid::Uuid;

use super::ops::{OpState, Ops};

/// An authenticated caller.
pub struct Caller {
    pub principal: Principal,
    pub credential: Option<Credential>,
}

/// Authenticates the credential of a request; an anonymous or unknown caller is `UNAUTHENTICATED`.
pub async fn authenticate(
    authorizer: &Arc<dyn Authorizer>,
    ext: &Extensions,
) -> Result<Caller, Status> {
    let credential = ext.get::<Credential>().cloned();
    let principal = authorizer.authenticate(credential.as_ref()).await?;
    Ok(Caller {
        principal,
        credential,
    })
}

/// The `x-asc-op-id` of a mutating call.
pub fn op_id(meta: &MetadataMap) -> Result<Uuid, Status> {
    let v = meta
        .get("x-asc-op-id")
        .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
    v.to_str()
        .ok()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Status::invalid_argument("malformed x-asc-op-id"))
}

/// Runs a mutating call once per operation id. A retry gets the first answer; if the answer was
/// not kept (`keep` false: it holds a secret) a retry is told the call already happened.
pub async fn once<T, F, Fut>(
    ops: &Ops,
    op: Uuid,
    principal: &str,
    method: &str,
    keep: bool,
    run: F,
) -> Result<T, Status>
where
    T: Message + Default,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, Status>>,
{
    match ops.begin(op, principal, method).await? {
        OpState::Done(bytes) => {
            T::decode(bytes.as_slice()).map_err(|_| Status::internal("internal error"))
        }
        OpState::Consumed => Err(Status::already_exists(
            "this operation already happened; its answer held a secret that is shown only once",
        )),
        OpState::New => match run().await {
            Ok(v) => {
                ops.finish(op, keep.then(|| v.encode_to_vec())).await;
                Ok(v)
            }
            Err(e) => {
                ops.abandon(op).await;
                Err(e)
            }
        },
    }
}
