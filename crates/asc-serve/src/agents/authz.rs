//! Agent tokens as credentials: the [`Authorizer`] that recognises `ascat_` secrets.
//!
//! [`AgentAuthorizer`] wraps the server's real authorizer. A secret that starts with `ascat_` is
//! decided here and never forwarded (so a forge never sees it); every other credential goes to the
//! inner authorizer untouched. A token authorises:
//!
//! * `Read` and `Write` on its own repository, nothing on any other;
//! * never `Admin`;
//! * `Write` only while it is neither revoked, expired nor exhausted. Expiry and revocation are
//!   read from the database on every call, so a revocation takes effect on the next call.
//!
//! The principal is `agent:<name>:<owner id>:<token id>` (see [`super::tokens::principal_id`]); the
//! policy, the ref log and the audit trail all see the agent's own name, never the person's.

use std::sync::Arc;

use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision, Principal, SshIdentity,
    SshKey,
};
use async_trait::async_trait;

use super::tokens::{PREFIX, TokenRow, TokenStore, looks_like_token};
use crate::ai::clock;

/// Wraps `inner`.
pub struct AgentAuthorizer {
    inner: Arc<dyn Authorizer>,
    tokens: TokenStore,
    enabled: bool,
}

impl AgentAuthorizer {
    pub fn new(inner: Arc<dyn Authorizer>, tokens: TokenStore, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            inner,
            tokens,
            enabled,
        })
    }

    /// The live token for `secret`, or the reason it does not authenticate.
    async fn live(&self, secret: &str) -> Result<TokenRow, AuthError> {
        if !self.enabled || !looks_like_token(secret) {
            return Err(AuthError::Unauthenticated);
        }
        let row = match self.tokens.by_secret(secret).await {
            Ok(Some(r)) => r,
            Ok(None) => return Err(AuthError::Unauthenticated),
            Err(e) => {
                // A database failure is not "unknown token", but it must not let anyone in.
                tracing::error!(class = %crate::ai::store::class(&e), "agent token lookup failed");
                return Err(AuthError::Unauthenticated);
            }
        };
        let now = clock::now();
        if row.revoked_at > 0 || row.expires_at <= now {
            return Err(AuthError::Unauthenticated);
        }
        self.tokens.touch(&row.id, now).await;
        Ok(row)
    }

    fn decide(row: &TokenRow, repo: &str, access: Access) -> Result<Principal, AuthError> {
        if !row.repo.eq_ignore_ascii_case(repo) {
            return Err(AuthError::Forbidden);
        }
        match access {
            Access::Read => {}
            Access::Write if !row.exhausted => {}
            Access::Write | Access::Admin => return Err(AuthError::Forbidden),
        }
        Ok(Principal {
            id: row.principal(),
        })
    }
}

fn is_agent_secret(s: &str) -> bool {
    s.starts_with(PREFIX)
}

#[async_trait]
impl Authorizer for AgentAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        match token {
            Some(t) if is_agent_secret(t.expose()) => {
                let row = self.live(t.expose()).await?;
                Self::decide(&row, repo, access)
            }
            _ => self.inner.authorize(token, repo, access).await,
        }
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        match credential {
            Some(c) if is_agent_secret(c.secret().expose()) => {
                let row = self.live(c.secret().expose()).await?;
                Self::decide(&row, repo, access)
            }
            _ => {
                self.inner
                    .authorize_credential(credential, repo, access)
                    .await
            }
        }
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        match credential {
            Some(c) if is_agent_secret(c.secret().expose()) => {
                let row = self.live(c.secret().expose()).await?;
                Ok(Principal {
                    id: row.principal(),
                })
            }
            _ => self.inner.authenticate(credential).await,
        }
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
        self.inner.authorize_ssh_key(key, repo, access).await
    }
}
