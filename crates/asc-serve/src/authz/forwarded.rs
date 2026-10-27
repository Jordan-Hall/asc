//! The key host-service claims are checked with (`[auth] forwarded_key_file`).
//!
//! The hub forwards, with a repair it makes through its service credential, the signed-in person
//! and that they administer the repository, signed under a key it shares with this server (see
//! [`asc_rpc::forwarded`]). [`ForwardedAuthorizer`] wraps the server's authorizer so every service
//! can check a claim; the claim counts only on a call made with the host service's credential.

use std::path::Path;
use std::sync::Arc;

use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision, Principal, SshIdentity,
    SshKey,
};
use asc_rpc::forwarded::ForwardedVerifier;
use async_trait::async_trait;

/// Longest key file read: 32 raw bytes, or 64 hex digits and a newline.
const MAX_KEY_FILE: u64 = 129;

/// Reads the verifier key: 32 raw bytes or 64 hex digits, in a file only this user can read.
pub fn load_verifier(path: &Path) -> Result<ForwardedVerifier, String> {
    let bad = |m: &str| format!("auth.forwarded_key_file {}: {m}", path.display());
    let file = super::key_file::open_private(path).map_err(|e| bad(&e.to_string()))?;
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_KEY_FILE + 1), &mut raw)
        .map_err(|e| bad(&e.to_string()))?;
    if raw.len() as u64 > MAX_KEY_FILE {
        return Err(bad("too long for a 32-byte key"));
    }
    let key = if raw.len() == 32 {
        raw
    } else {
        let text = std::str::from_utf8(&raw)
            .map_err(|_| bad("not a 32-byte key"))?
            .trim();
        if text.len() != 64 || !text.is_ascii() {
            return Err(bad("not a 32-byte key"));
        }
        (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| bad("not a 32-byte key"))?
    };
    ForwardedVerifier::new(key).ok_or_else(|| bad("not a 32-byte key"))
}

/// An authorizer that also holds the claim key, and decides everything else as `inner` does.
pub struct ForwardedAuthorizer {
    inner: Arc<dyn Authorizer>,
    verifier: ForwardedVerifier,
}

impl ForwardedAuthorizer {
    pub fn new(inner: Arc<dyn Authorizer>, verifier: ForwardedVerifier) -> Self {
        Self { inner, verifier }
    }
}

#[async_trait]
impl Authorizer for ForwardedAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.inner.authorize(token, repo, access).await
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.inner
            .authorize_credential(credential, repo, access)
            .await
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        self.inner.authenticate(credential).await
    }

    fn forwarded_verifier(&self) -> Option<&ForwardedVerifier> {
        Some(&self.verifier)
    }

    async fn user_standing(&self, user_id: i64, repo: &str, access: Access) -> Option<bool> {
        self.inner.user_standing(user_id, repo, access).await
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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use asc_rpc::forwarded::Role;

    use super::*;

    fn key_file(dir: &Path, bytes: &[u8], mode: u32) -> std::path::PathBuf {
        let p = dir.join("fwd.key");
        std::fs::write(&p, bytes).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn the_key_is_32_raw_bytes_or_64_hex_digits_in_a_private_file() {
        let d = tempfile::tempdir().unwrap();
        assert!(load_verifier(&key_file(d.path(), &[5u8; 32], 0o600)).is_ok());
        assert!(load_verifier(&key_file(d.path(), "ab".repeat(32).as_bytes(), 0o600)).is_ok());
        assert!(load_verifier(&key_file(d.path(), "ab\n".as_bytes(), 0o600)).is_err());
        assert!(load_verifier(&key_file(d.path(), &[5u8; 31], 0o600)).is_err());
        assert!(load_verifier(&key_file(d.path(), &[5u8; 32], 0o644)).is_err());
        assert!(load_verifier(&d.path().join("missing")).is_err());
    }

    #[test]
    fn a_call_carries_the_claim_only_when_the_server_holds_the_key_and_it_verifies() {
        use asc_rpc::auth::AdminProof;
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let v = ForwardedVerifier::new(vec![1u8; 32]).unwrap();
        let mut meta = tonic::metadata::MetadataMap::new();
        for (k, val) in v.sign("7:alice", Role::Admin, "acme/app", now) {
            meta.append(k, val.parse().unwrap());
        }
        let plain: Arc<dyn Authorizer> = Arc::new(asc_rpc::auth::StaticTokens::new());
        // A server with no key takes no claim, whatever the call says.
        assert!(AdminProof::forwarded_in(&*plain, &meta, "acme/app").is_none());
        let keyed = ForwardedAuthorizer::new(plain, v);
        let claim = AdminProof::forwarded_in(&keyed, &meta, "acme/app").unwrap();
        assert_eq!((claim.user.as_str(), claim.role), ("7:alice", Role::Admin));
        assert!(AdminProof::forwarded_in(&keyed, &meta, "acme/other").is_none());
        // A client that guesses: right shape, wrong key.
        let theirs = ForwardedVerifier::new(vec![2u8; 32]).unwrap();
        let mut forged = tonic::metadata::MetadataMap::new();
        for (k, val) in theirs.sign("7:alice", Role::Admin, "acme/app", now) {
            forged.append(k, val.parse().unwrap());
        }
        assert!(AdminProof::forwarded_in(&keyed, &forged, "acme/app").is_none());
    }

    #[test]
    fn the_wrapper_holds_the_key_and_nothing_else_changes() {
        let v = ForwardedVerifier::new(vec![1u8; 32]).unwrap();
        let inner: Arc<dyn Authorizer> = Arc::new(asc_rpc::auth::StaticTokens::new());
        assert!(inner.forwarded_verifier().is_none());
        let w = ForwardedAuthorizer::new(inner, v);
        assert!(w.forwarded_verifier().is_some());
    }
}
