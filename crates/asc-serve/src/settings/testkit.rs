//! Shared fixtures of the settings services' tests.

use std::sync::{Arc, Mutex, PoisonError};

use asc_rpc::auth::{Authorizer, BearerToken, Credential};
use asc_rpc::forwarded::{ForwardedVerifier, Role};
use async_trait::async_trait;
use sqlx::AnyPool;
use tonic::Request;

use crate::ai::audit::{Audit, Entry};

/// A database, an authorizer that knows alice, bob, carol and the host, and a claim key.
pub(crate) struct Kit {
    pub pool: AnyPool,
    pub authz: Arc<dyn Authorizer>,
    pub verifier: ForwardedVerifier,
    pub dir: tempfile::TempDir,
}

pub(crate) async fn kit() -> Kit {
    let dir = tempfile::tempdir().unwrap();
    let pool = crate::ai::tests::pool_in(&dir).await;
    let verifier = ForwardedVerifier::new(vec![8u8; 32]).unwrap();
    let authz: Arc<dyn Authorizer> = Arc::new(crate::authz::forwarded::ForwardedAuthorizer::new(
        crate::ai::tests::authorizer(),
        verifier.clone(),
    ));
    Kit {
        pool,
        authz,
        verifier,
        dir,
    }
}

/// A request carrying the bearer `token` and, when given, an operation id.
pub(crate) fn req<T>(msg: T, token: &str, op: Option<&str>) -> Request<T> {
    let mut r = Request::new(msg);
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new(token).unwrap()));
    if let Some(op) = op {
        r.metadata_mut().insert("x-asc-op-id", op.parse().unwrap());
    }
    r
}

/// Adds the hub's claim for `user` (`<id>:<login>`) on `subject`.
pub(crate) fn claim<T>(
    mut r: Request<T>,
    k: &Kit,
    user: &str,
    role: Role,
    subject: &str,
) -> Request<T> {
    let now = crate::ai::clock::now();
    for (key, val) in k.verifier.sign(user, role, subject, now) {
        r.metadata_mut().append(key, val.parse().unwrap());
    }
    r
}

/// Adds the hub's claim signed by `v`, for tests that build their own verifier.
pub(crate) fn claim_with<T>(
    mut r: Request<T>,
    v: &ForwardedVerifier,
    user: &str,
    role: Role,
    subject: &str,
) -> Request<T> {
    let now = crate::ai::clock::now();
    for (key, val) in v.sign(user, role, subject, now) {
        r.metadata_mut().append(key, val.parse().unwrap());
    }
    r
}

/// The `n`th operation id.
pub(crate) fn op(n: u32) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

/// What an audit sink recorded: action, actor, relay, target, details.
pub(crate) type Recorded = (
    String,
    String,
    Option<String>,
    String,
    Vec<(String, String)>,
);

#[derive(Default)]
pub(crate) struct Recorder(pub Mutex<Vec<Recorded>>);

impl Recorder {
    pub fn all(&self) -> Vec<Recorded> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn of(&self, action: &str) -> Vec<Recorded> {
        self.all().into_iter().filter(|r| r.0 == action).collect()
    }
}

#[async_trait]
impl Audit for Recorder {
    async fn record(&self, e: Entry<'_>) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push((
            e.action.to_owned(),
            e.actor.to_owned(),
            e.on_behalf.map(str::to_owned),
            e.target.to_owned(),
            e.details
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        ));
    }
}
