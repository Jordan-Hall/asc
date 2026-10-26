//! Forgejo user keys and deploy keys for the SSH listener (SPEC §25.11.3 item 5; PLAN U85).
//!
//! This is a child module of [`super`] (`forgejo.rs` registers it with one `#[path]` line), so it
//! reaches the client's private parts without widening any visibility.
//!
//! [`InternalAuthz`](super::InternalAuthz) asks the forge twice:
//!
//! * `identify_ssh_key` is `POST /api/internal/asc/ssh-key` `{type, key, fingerprint}` (K19). It
//!   answers `{known, key_id, kind, user_id, username}`. A key that is unknown, whose owner may not
//!   sign in, or that is a deploy key no repository uses, is `known: false`.
//! * `authorize_ssh_key` is the `ssh_key` variant of `POST /api/internal/asc/authz`
//!   `{ssh_key, owner, repo, mode}`. Forgejo finds the key in its own tables and decides with the
//!   owner's rights, or with the one repository of a deploy key.
//!
//! Both calls carry the `INTERNAL_TOKEN`, and the answers are cached 60 s when positive and 5 s
//! when negative. Everything the forge returns is untrusted. A 5xx, a timeout, an oversize or
//! malformed answer, an answer that contradicts itself and a lookup over the concurrency cap are
//! all refusals that are not cached, and the forge's body is never forwarded or logged. Only the
//! key's fingerprint reaches a log; the wire blob never does.

use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_rpc::auth::{
    Access, AuthError, Decision as SshDecision, Principal, SshFingerprint, SshIdentity, SshKey,
    SshKeyKind,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::{AuthzAnswer, AuthzCacheConfig, ForgejoClient, JsonBody, Perm};
use crate::authz::UserIdentity;

/// How long one lookup may take, connecting to reading the last byte. The SSH login itself has
/// 30 s (SPEC §25.11.3 item 6), so the forge gets a small part of it.
pub const SSH_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How many lookups may be in flight at once. A lookup over the cap is refused at once, never
/// queued, so a flood of key offers cannot pile up requests against the forge.
pub const MAX_SSH_LOOKUPS_IN_FLIGHT: usize = 64;
/// The most bytes read from one answer. Real answers are under 1 KiB.
const MAX_SSH_ANSWER: usize = 16 * 1024;
/// Prefix of the principal of a deploy key before the repository is known. It does not parse as a
/// [`UserIdentity`], so no hook can mistake it for a Forgejo user.
const DEPLOY_PRINCIPAL: &str = "forgejo-deploy-key:";

/// Whether an authorizer can resolve SSH keys. The U86 start check reads it to refuse
/// `[ssh] enabled = true` with `provider = "forge"` and no internal endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshSupport {
    Supported,
    Unsupported,
}

/// The limits of one lookup.
#[derive(Clone, Copy, Debug)]
pub struct SshLookupLimits {
    pub timeout: Duration,
    pub max_in_flight: usize,
}

impl Default for SshLookupLimits {
    fn default() -> Self {
        Self {
            timeout: SSH_LOOKUP_TIMEOUT,
            max_in_flight: MAX_SSH_LOOKUPS_IN_FLIGHT,
        }
    }
}

// ------------------------------------------------------------------------------ wire

/// `{type, key, fingerprint}`: the key as K19 takes it.
#[derive(Serialize)]
struct KeyBody<'a> {
    #[serde(rename = "type")]
    algo: &'a str,
    /// Standard base64 of the wire blob.
    key: String,
    fingerprint: String,
}

impl<'a> KeyBody<'a> {
    fn of(key: &'a SshKey) -> Self {
        Self {
            algo: key.algo(),
            key: STANDARD.encode(key.blob()),
            fingerprint: key.fingerprint().to_string(),
        }
    }
}

#[derive(Serialize)]
struct SshAuthzRequest<'a> {
    ssh_key: KeyBody<'a>,
    owner: &'a str,
    repo: &'a str,
    mode: &'static str,
}

/// The answer of `POST /api/internal/asc/ssh-key`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct KeyAnswer {
    known: bool,
    key_id: i64,
    kind: String,
    user_id: i64,
    username: String,
}

/// Why a call produced no usable answer.
enum Failure {
    /// Forgejo refused the request itself (400): a definite answer for this key.
    Rejected,
    /// No answer could be had; nothing is cached.
    Down,
}

// ------------------------------------------------------------------------------ cache

#[derive(Clone)]
struct Stamped<T> {
    at: Instant,
    value: T,
}

/// `(sha256(blob), repo, mode)` (SPEC §25.11.3 item 3).
type DecisionKey = ([u8; 32], String, Access);

/// The lookups and their caches. One per [`InternalAuthz`](super::InternalAuthz).
pub(super) struct SshLookup {
    identities: quick_cache::sync::Cache<[u8; 32], Stamped<Option<SshIdentity>>>,
    decisions: quick_cache::sync::Cache<DecisionKey, Stamped<Option<Principal>>>,
    cfg: AuthzCacheConfig,
    limits: SshLookupLimits,
    in_flight: Arc<Semaphore>,
}

impl SshLookup {
    pub(super) fn new(cfg: AuthzCacheConfig) -> Self {
        let limits = SshLookupLimits::default();
        Self {
            identities: quick_cache::sync::Cache::new(cfg.entries.max(1)),
            decisions: quick_cache::sync::Cache::new(cfg.entries.max(1)),
            cfg,
            in_flight: Arc::new(Semaphore::new(limits.max_in_flight)),
            limits,
        }
    }

    pub(super) fn with_limits(mut self, limits: SshLookupLimits) -> Self {
        self.in_flight = Arc::new(Semaphore::new(limits.max_in_flight));
        self.limits = limits;
        self
    }

    /// A cached answer is used while it is younger than the lifetime of its sign.
    fn fresh(&self, at: Instant, positive: bool) -> bool {
        let ttl = if positive {
            self.cfg.positive
        } else {
            self.cfg.negative
        };
        at.elapsed() < ttl
    }

    pub(super) async fn identify(
        &self,
        client: &ForgejoClient,
        key: &SshKey,
    ) -> Result<Option<SshIdentity>, AuthError> {
        let digest = *key.fingerprint().as_bytes();
        if let Some(hit) = self.identities.get(&digest)
            && self.fresh(hit.at, hit.value.is_some())
        {
            return Ok(hit.value);
        }
        let fingerprint = key.fingerprint();
        let found = match self
            .post(
                client,
                &["api", "internal", "asc", "ssh-key"],
                &KeyBody::of(key),
                &fingerprint,
            )
            .await
        {
            Ok(bytes) => match identity_of(&bytes) {
                Some(found) => found,
                None => {
                    tracing::warn!(%fingerprint, "forgejo sent an invalid ssh key answer");
                    return Err(AuthError::Forbidden);
                }
            },
            Err(Failure::Rejected) => None,
            Err(Failure::Down) => return Err(AuthError::Forbidden),
        };
        self.identities.insert(
            digest,
            Stamped {
                at: Instant::now(),
                value: found.clone(),
            },
        );
        Ok(found)
    }

    pub(super) async fn authorize(
        &self,
        client: &ForgejoClient,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<SshDecision, AuthError> {
        // SSH needs read for upload-pack and write for receive-pack and nothing else; K19 takes no
        // other mode. A bad name never reaches the forge.
        let mode = match access {
            Access::Read => "read",
            Access::Write => "write",
            Access::Admin => return Ok(SshDecision::Denied),
        };
        let Some((owner, name)) = super::parse_repo(repo)
            .ok()
            .and_then(|_| repo.split_once('/'))
        else {
            return Ok(SshDecision::Denied);
        };
        let fingerprint = key.fingerprint();
        let cache_key = (*fingerprint.as_bytes(), repo.to_owned(), access);
        if let Some(hit) = self.decisions.get(&cache_key)
            && self.fresh(hit.at, hit.value.is_some())
        {
            return Ok(decision_of(hit.value));
        }
        let body = SshAuthzRequest {
            ssh_key: KeyBody::of(key),
            owner,
            repo: name,
            mode,
        };
        let granted = match self
            .post(
                client,
                &["api", "internal", "asc", "authz"],
                &body,
                &fingerprint,
            )
            .await
        {
            Ok(bytes) => match principal_of(&bytes, access) {
                Some(granted) => granted,
                None => {
                    tracing::warn!(%fingerprint, "forgejo sent an invalid ssh authz answer");
                    return Err(AuthError::Forbidden);
                }
            },
            Err(Failure::Rejected) => None,
            Err(Failure::Down) => return Err(AuthError::Forbidden),
        };
        self.decisions.insert(
            cache_key,
            Stamped {
                at: Instant::now(),
                value: granted.clone(),
            },
        );
        Ok(decision_of(granted))
    }

    /// One internal POST: at most `max_in_flight` at a time, `timeout` long, and the answer capped
    /// at [`MAX_SSH_ANSWER`] bytes. Only a 200 is an answer, and a 400 is the forge refusing the
    /// request. Every other status, and every transport error, is `Down`.
    async fn post(
        &self,
        client: &ForgejoClient,
        segments: &[&str],
        body: &impl Serialize,
        fingerprint: &SshFingerprint,
    ) -> Result<Vec<u8>, Failure> {
        let Ok(_permit) = self.in_flight.try_acquire() else {
            tracing::warn!(%fingerprint, "too many forgejo ssh lookups in flight; refusing");
            return Err(Failure::Down);
        };
        let req = client
            .internal_request(segments)
            .and_then(|r| r.timeout(self.limits.timeout).json_body(body))
            .map_err(|e| {
                tracing::warn!(%fingerprint, error = %e, "forgejo ssh lookup was not sent");
                Failure::Down
            })?;
        let transport = |e: reqwest::Error| {
            tracing::warn!(%fingerprint, error = %e.without_url(), "forgejo ssh lookup failed");
            Failure::Down
        };
        let mut resp = req.send().await.map_err(transport)?;
        let status = resp.status();
        if status != StatusCode::OK && status != StatusCode::BAD_REQUEST {
            tracing::warn!(%fingerprint, status = status.as_u16(), "forgejo ssh lookup refused");
            return Err(Failure::Down);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(transport)? {
            if bytes.len() + chunk.len() > MAX_SSH_ANSWER {
                tracing::warn!(%fingerprint, "forgejo ssh answer is too large");
                return Err(Failure::Down);
            }
            bytes.extend_from_slice(&chunk);
        }
        if status == StatusCode::BAD_REQUEST {
            return Err(Failure::Rejected);
        }
        Ok(bytes)
    }
}

fn decision_of(granted: Option<Principal>) -> SshDecision {
    granted.map_or(SshDecision::Denied, SshDecision::Allowed)
}

/// Reads an identify answer. `None` is a malformed or self-contradicting answer; `Some(None)` is
/// "no such usable key".
fn identity_of(body: &[u8]) -> Option<Option<SshIdentity>> {
    let a: KeyAnswer = serde_json::from_slice(body).ok()?;
    if !a.known {
        return Some(None);
    }
    if a.key_id <= 0 {
        return None;
    }
    let (kind, principal) = match a.kind.as_str() {
        "user" => {
            if a.user_id <= 0 {
                return None;
            }
            let who = UserIdentity::new(a.user_id, &a.username, 0)?;
            (SshKeyKind::User, who.principal_id())
        }
        // A Forgejo deploy key is one row shared by every repository that uses it, so the
        // repository is only known per command (`authorize_ssh_key`).
        "deploy" => (
            SshKeyKind::Deploy,
            format!("{DEPLOY_PRINCIPAL}{}", a.key_id),
        ),
        _ => return None,
    };
    Some(Some(SshIdentity {
        key_id: a.key_id.to_string(),
        kind,
        principal: Principal { id: principal },
        repo: None,
    }))
}

/// Reads an authz answer for `access`. `None` is a malformed answer; `Some(None)` is a refusal.
/// The principal is the one the HTTP path builds, `<user_id>:<username>[:<deploy_key_id>]`, so the
/// ref log and the hooks agree.
fn principal_of(body: &[u8], access: Access) -> Option<Option<Principal>> {
    let a: AuthzAnswer = serde_json::from_slice(body).ok()?;
    // An SSH key is never an Actions task.
    if a.is_actions_task || a.task.is_some() {
        return None;
    }
    if !a.allowed {
        return Some(None);
    }
    // The decision is checked again here whatever the forge says: too little permission, and a
    // caller that is not a real account, are refusals.
    if !Perm::parse(&a.perm).allows(access) || a.user_id <= 0 {
        return Some(None);
    }
    let who = UserIdentity::new(a.user_id, &a.username, a.deploy_key_id)?;
    Some(Some(Principal {
        id: who.principal_id(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_answers_are_checked() {
        let ok = identity_of(
            br#"{"known":true,"key_id":4,"kind":"user","user_id":7,"username":"alice"}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.key_id, "4");
        assert_eq!(ok.kind, SshKeyKind::User);
        assert_eq!(ok.principal.id, "7:alice");
        let dep = identity_of(br#"{"known":true,"key_id":9,"kind":"deploy"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(dep.kind, SshKeyKind::Deploy);
        assert!(UserIdentity::parse(&dep.principal.id).is_none());
        // Unknown, and the empty answer, are "no key".
        assert_eq!(identity_of(b"{}"), Some(None));
        assert_eq!(identity_of(br#"{"known":false,"key_id":4}"#), Some(None));
        // Malformed or contradictory answers are not "no key": they fail the lookup.
        for bad in [
            &br#"not json"#[..],
            br#"{"known":true,"key_id":0,"kind":"user","user_id":7,"username":"a"}"#,
            br#"{"known":true,"key_id":1,"kind":"admin","user_id":7,"username":"a"}"#,
            br#"{"known":true,"key_id":1,"kind":"user","user_id":0,"username":"a"}"#,
            br#"{"known":true,"key_id":1,"kind":"user","user_id":7,"username":"a:b"}"#,
            br#"{"known":"yes","key_id":1,"kind":"user"}"#,
        ] {
            assert_eq!(identity_of(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn authz_answers_are_checked() {
        let user = br#"{"allowed":true,"user_id":7,"username":"alice","perm":"write"}"#;
        assert_eq!(
            principal_of(user, Access::Write),
            Some(Some(Principal {
                id: "7:alice".into()
            }))
        );
        // The forge said yes but the permission is too low for the mode: refused.
        let read = br#"{"allowed":true,"user_id":7,"username":"alice","perm":"read"}"#;
        assert_eq!(principal_of(read, Access::Write), Some(None));
        assert!(principal_of(read, Access::Read).unwrap().is_some());
        let none = br#"{"allowed":true,"user_id":7,"username":"alice","perm":"none"}"#;
        assert_eq!(principal_of(none, Access::Read), Some(None));
        assert_eq!(
            principal_of(br#"{"allowed":false}"#, Access::Read),
            Some(None)
        );
        // An anonymous caller is never a key's principal.
        let anon = br#"{"allowed":true,"user_id":0,"username":"anonymous","perm":"read"}"#;
        assert_eq!(principal_of(anon, Access::Read), Some(None));
        // A deploy key acts as the owner and carries its id.
        let dep =
            br#"{"allowed":true,"user_id":2,"username":"org","perm":"read","deploy_key_id":5}"#;
        assert_eq!(
            principal_of(dep, Access::Read),
            Some(Some(Principal {
                id: "2:org:5".into()
            }))
        );
        // Malformed: not JSON, an Actions task, a bad user name.
        for bad in [
            &b"<html>"[..],
            br#"{"allowed":true,"user_id":7,"username":"a","perm":"read","is_actions_task":true}"#,
            br#"{"allowed":true,"user_id":7,"username":"a b","perm":"read"}"#,
            br#"{"allowed":true,"user_id":7,"username":"a","perm":"read","deploy_key_id":-1}"#,
        ] {
            assert_eq!(principal_of(bad, Access::Read), None);
        }
    }
}
