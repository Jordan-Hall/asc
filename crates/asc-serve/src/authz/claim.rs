//! Reading the hub's forwarded claim in a service (see [`asc_rpc::forwarded`]).
//!
//! A claim counts only on a call authenticated with the host service's credential, and only for the
//! subject it was signed for. Where a claim is present it is the whole of the caller's authority:
//! the host service's own breadth does not widen it, and a `read` claim does not write.

use asc_rpc::auth::{AdminProof, Authorizer};
use asc_rpc::forwarded::{Claim, Role};
use tonic::metadata::MetadataMap;

use super::is_host_service;

/// The subjects of the hub's claims, defined once for every service. Each takes the canonical
/// name the service stores and does not fold case: the claim must carry exactly that spelling, so
/// a claim for `acme/app` is never a claim for a distinct `acme/APP`. Users and organisations are
/// stored lower case everywhere; a repository is the name as the registry holds it.
pub mod subject {
    /// The whole server.
    pub const INSTANCE: &str = "instance";

    /// The account of a person.
    #[must_use]
    pub fn user(login: &str) -> String {
        format!("user:{login}")
    }

    /// An organisation.
    #[must_use]
    pub fn org(name: &str) -> String {
        format!("org:{name}")
    }

    /// A repository, `owner/name`.
    #[must_use]
    pub fn repo(name: &str) -> String {
        name.to_owned()
    }
}

/// The claim in `meta` for `subject`, when `principal_id` is the host service and the claim
/// verifies. Any other caller has no claim, whatever its metadata says.
pub fn host_claim(
    authorizer: &dyn Authorizer,
    principal_id: &str,
    meta: &MetadataMap,
    subject: &str,
) -> Option<Claim> {
    if !is_host_service(principal_id) {
        return None;
    }
    AdminProof::forwarded_in(authorizer, meta, subject)
}

/// The claim for the account of the person it names (`user:<login>`). The login is read from the
/// claim itself to find the subject; the MAC then proves the hub signed exactly that pair.
pub fn account_claim(
    authorizer: &dyn Authorizer,
    principal_id: &str,
    meta: &MetadataMap,
) -> Option<Claim> {
    let user = meta.get(asc_rpc::forwarded::USER)?.to_str().ok()?;
    let login = user.split_once(':')?.1;
    host_claim(authorizer, principal_id, meta, &format!("user:{login}"))
}

/// What a call from the host service says about who it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCall {
    /// Not the host service: its own rights decide, and metadata is ignored.
    NotHost,
    /// The host service with no claim: system work, decided as the service always was.
    System,
    /// The host service for a person, with a claim that verified for the subject. The claim is the
    /// whole of the authority: the service's breadth does not widen it.
    Person(Claim),
    /// The host service with a claim that does not verify for the subject (wrong key, subject,
    /// time or spelling). It is refused, never treated as system work.
    Refused,
}

/// Sorts a call by what it claims for `subject`.
pub fn classify(
    authorizer: &dyn Authorizer,
    principal_id: &str,
    meta: &MetadataMap,
    subject: &str,
) -> HostCall {
    if !is_host_service(principal_id) {
        return HostCall::NotHost;
    }
    let claimed = [
        asc_rpc::forwarded::USER,
        asc_rpc::forwarded::ROLE,
        asc_rpc::forwarded::TS,
        asc_rpc::forwarded::MAC,
    ]
    .iter()
    .any(|k| meta.contains_key(*k));
    if !claimed {
        return HostCall::System;
    }
    match AdminProof::forwarded_in(authorizer, meta, subject) {
        Some(c) => HostCall::Person(c),
        None => HostCall::Refused,
    }
}

/// The principal id a call acts as for something personal: the person of an account claim of at
/// least `needed`, else the caller as authenticated.
pub fn acting_principal(
    authorizer: &dyn Authorizer,
    principal_id: &str,
    meta: &MetadataMap,
    needed: Role,
) -> String {
    match account_claim(authorizer, principal_id, meta) {
        Some(c) if c.allows(needed) => c.user,
        _ => principal_id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use asc_rpc::forwarded::ForwardedVerifier;

    use super::super::forwarded::ForwardedAuthorizer;
    use super::*;

    fn now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
    }

    fn meta(v: &ForwardedVerifier, user: &str, role: Role, subject: &str) -> MetadataMap {
        let mut m = MetadataMap::new();
        for (k, val) in v.sign(user, role, subject, now()) {
            m.append(k, val.parse().unwrap());
        }
        m
    }

    fn authz() -> (ForwardedAuthorizer, ForwardedVerifier) {
        let v = ForwardedVerifier::new(vec![3u8; 32]).unwrap();
        let inner: Arc<dyn Authorizer> = Arc::new(asc_rpc::auth::StaticTokens::new());
        (ForwardedAuthorizer::new(inner, v.clone()), v)
    }

    #[test]
    fn only_the_host_service_acts_on_a_claim() {
        let (a, v) = authz();
        let m = meta(&v, "7:alice", Role::Write, "acme/app");
        let host = crate::authz::HOST_SERVICE_PRINCIPAL;
        assert!(host_claim(&a, host, &m, "acme/app").is_some());
        assert!(host_claim(&a, "pat:mallory", &m, "acme/app").is_none());
        assert!(host_claim(&a, "7:alice", &m, "acme/app").is_none());
        assert!(host_claim(&a, host, &m, "acme/other").is_none());
    }

    #[test]
    fn an_account_claim_makes_the_host_act_as_the_person_within_its_role() {
        let (a, v) = authz();
        let host = crate::authz::HOST_SERVICE_PRINCIPAL;
        let write = meta(&v, "7:alice", Role::Write, "user:alice");
        assert_eq!(acting_principal(&a, host, &write, Role::Write), "7:alice");
        assert_eq!(acting_principal(&a, host, &write, Role::Read), "7:alice");
        // A read claim does not write, and a claim for another account is nobody's.
        let read = meta(&v, "7:alice", Role::Read, "user:alice");
        assert_eq!(acting_principal(&a, host, &read, Role::Write), host);
        assert_eq!(acting_principal(&a, host, &read, Role::Read), "7:alice");
        let other = meta(&v, "7:alice", Role::Write, "user:bob");
        assert_eq!(acting_principal(&a, host, &other, Role::Read), host);
        // Another caller keeps its own identity, claim or no claim.
        assert_eq!(acting_principal(&a, "8:bob", &write, Role::Read), "8:bob");
        assert_eq!(
            acting_principal(&a, host, &MetadataMap::new(), Role::Read),
            host
        );
    }
}
