//! The authorizer of the services that do not apply path rules themselves (SPEC §15.5.1).
//!
//! The repository service filters every tree, file and history answer by the caller's path scope.
//! Review, code intelligence, agents, exports and the other services answer with content or
//! paths of the whole repository, so a caller whose scope restricts any path is refused there:
//! the guard answers `Forbidden` for it, as if it had no access. Everyone else, and the host
//! service, is decided by the inner authorizer alone.

use std::sync::Arc;

use asc_core::pathacl::Level;
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision, Principal, SshIdentity,
    SshKey, Standing,
};
use asc_rpc::forwarded::ForwardedVerifier;
use async_trait::async_trait;

use super::PathRulesSlot;
use super::scope::Scope;

/// Refuses callers with a restricted path; decides everything else as `inner` does.
pub struct PathGuard {
    inner: Arc<dyn Authorizer>,
    slot: Arc<PathRulesSlot>,
}

impl PathGuard {
    pub fn new(inner: Arc<dyn Authorizer>, slot: Arc<PathRulesSlot>) -> Self {
        Self { inner, slot }
    }

    /// `principal` when no path of `repo` is restricted for it.
    async fn admit(
        &self,
        principal: Principal,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let level = match access {
            Access::Read => Level::Read,
            Access::Write => Level::Write,
            Access::Admin => Level::Admin,
        };
        match Scope::of(&self.slot, repo, &principal.id, level).await {
            Ok(None) => Ok(principal),
            Ok(Some(_)) => Err(AuthError::Forbidden),
            Err(e) => {
                // Fail closed: an unreadable rule set admits nobody it might restrict.
                tracing::warn!(error = %e, repo, "reading the path rules failed");
                Err(AuthError::Forbidden)
            }
        }
    }
}

#[async_trait]
impl Authorizer for PathGuard {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let p = self.inner.authorize(token, repo, access).await?;
        self.admit(p, repo, access).await
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let p = self
            .inner
            .authorize_credential(credential, repo, access)
            .await?;
        self.admit(p, repo, access).await
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        self.inner.authenticate(credential).await
    }

    async fn user_standing(&self, user_id: i64, repo: &str, access: Access) -> Standing {
        self.inner.user_standing(user_id, repo, access).await
    }

    fn forwarded_verifier(&self) -> Option<&ForwardedVerifier> {
        self.inner.forwarded_verifier()
    }

    async fn identify_ssh_key(&self, key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        self.inner.identify_ssh_key(key).await
    }

    async fn authorize_ssh_key(
        &self,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<Decision, AuthError> {
        match self.inner.authorize_ssh_key(key, repo, access).await? {
            Decision::Allowed(p) => match self.admit(p, repo, access).await {
                Ok(p) => Ok(Decision::Allowed(p)),
                Err(_) => Ok(Decision::Denied),
            },
            other => Ok(other),
        }
    }
}
