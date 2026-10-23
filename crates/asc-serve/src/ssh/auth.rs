//! Public-key login (SPEC §25.11.3).
//!
//! `publickey` is the only method. A key is checked against the key-type policy of
//! [`ssh_keys::check_policy`](crate::authz::ssh_keys::check_policy) (the same list the token
//! store applies when a key is registered), then recognised by
//! [`Authorizer::identify_ssh_key`] at the offer, before the client signs anything. The SSH
//! library verifies the signature. Rights are not decided here: the exec step asks
//! [`Authorizer::authorize_ssh_key`] for every command.
//!
//! Every refusal is the same `Permission denied (publickey)` to the client, whatever the
//! cause; the cause only goes to the audit record.

use std::time::Duration;

use asc_rpc::auth::{Authorizer, SshFingerprint, SshIdentity, SshKey};
use russh::keys::PublicKey;

use super::audit::LoginOutcome;
use crate::authz::ssh_keys::check_policy;

/// Longest the authorizer may take to recognise a key. Past it the key is refused.
pub const AUTHORIZER_TIMEOUT: Duration = Duration::from_secs(10);

/// A key the authorizer recognised.
#[derive(Clone)]
pub struct Identified {
    pub key: SshKey,
    pub identity: SshIdentity,
    pub fingerprint: SshFingerprint,
}

/// Whether `user` is the configured SSH user name (`""` accepts any).
pub fn user_ok(configured: &str, user: &str) -> bool {
    configured.is_empty() || configured == user
}

/// The key as the authorizer sees it, or the reason the key type is refused.
///
/// # Errors
/// [`LoginOutcome::RsaBits`] for an RSA key outside `min_rsa_bits..=16384`;
/// [`LoginOutcome::KeyType`] for DSA, a security key, a curve outside the NIST three, any other
/// type, or a key that cannot be encoded.
pub fn key_for(public: &PublicKey, min_rsa_bits: u32) -> Result<SshKey, LoginOutcome> {
    if check_policy(public, min_rsa_bits).is_err() {
        return Err(if public.key_data().rsa().is_some() {
            LoginOutcome::RsaBits
        } else {
            LoginOutcome::KeyType
        });
    }
    let blob = public.to_bytes().map_err(|_| LoginOutcome::KeyType)?;
    SshKey::new(public.algorithm().as_str(), &blob).map_err(|_| LoginOutcome::KeyType)
}

/// Applies the key policy and asks the authorizer whether it knows the key.
///
/// # Errors
/// The [`LoginOutcome`] to audit: a policy refusal, or [`LoginOutcome::UnknownKey`] for an
/// unknown, revoked or expired key and for an authorizer that failed or timed out (fail
/// closed; the client cannot tell the cases apart).
pub async fn identify(
    authorizer: &dyn Authorizer,
    public: &PublicKey,
    min_rsa_bits: u32,
) -> Result<Identified, LoginOutcome> {
    let key = key_for(public, min_rsa_bits)?;
    let fingerprint = key.fingerprint();
    match tokio::time::timeout(AUTHORIZER_TIMEOUT, authorizer.identify_ssh_key(&key)).await {
        Ok(Ok(Some(identity))) => Ok(Identified {
            key,
            identity,
            fingerprint,
        }),
        Ok(Ok(None)) => Err(LoginOutcome::UnknownKey),
        Ok(Err(e)) => {
            tracing::warn!(%fingerprint, error = %e, "ssh key lookup failed; login refused");
            Err(LoginOutcome::UnknownKey)
        }
        Err(_) => {
            tracing::warn!(%fingerprint, "ssh key lookup timed out; login refused");
            Err(LoginOutcome::UnknownKey)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_name_must_match_unless_the_config_is_empty() {
        assert!(user_ok("git", "git"));
        assert!(!user_ok("git", "root"));
        assert!(!user_ok("git", "git "));
        assert!(!user_ok("git", ""));
        assert!(user_ok("", "anything"));
        assert!(user_ok("", ""));
    }
}
