//! Signed changes (docs/BUILDOUT.md section 5): signing keys, signatures and their verdicts.
//!
//! A signature is an SSHSIG over `asc-changeset-v1\n<changeset id>\n` ([`asc_core::sshsig`]). It is
//! kept beside the changeset in `change_signatures`, so signing never changes an id and a change can
//! be signed after it was pushed. A key belongs to one user; one fingerprint belongs to one key, so a
//! signature names its signer without a claim in the request.
//!
//! * [`store`]: keys and signature rows.
//! * [`verify`]: the verdict ([`VerificationStatus`](asc_rpc::server::wire::VerificationStatus)) of a
//!   changeset, derived from the stored signature, the key row and the author, every time it is asked.
//! * [`rpc`]: the `Signing` service.
//!
//! # Verdict
//!
//! `VALID` needs a good signature by a key of the changeset's author that was neither expired nor
//! revoked when the signature was submitted. The author is the user who first pushed the changeset
//! (the actor of its version in the change index); a changeset the index does not know has no author
//! the server can name, so it is never `VALID`. A key revoked after a signature was submitted does not
//! change that earlier verdict: the signature stays explainable ("signed before the key was revoked").
//!
//! # Server key
//!
//! `[signing] server_key_file` names an unencrypted OpenSSH private key (readable by the server user
//! only). The server signs what it makes itself with it (`Commit` and `Merge` with `sign`), and, when
//! a branch requires signed changes, the changesets a land rewrites from changesets that were `VALID`
//! (the rewrite keeps the content the author signed and the server attests to that). Such a
//! signature has `key_id = server` and verdict `VALID` with signer `asc-server`.
//!
//! # Web edits
//!
//! `[signing] web_edits = "server"` (the default) lets the host application ask for `sign` on
//! `Commit`, `Apply` and `Merge`: the server then signs the new changeset with its key
//! (`key_id = server`), stores that signature beside the changeset and names itself as the
//! committer. The author is given explicitly and is the person the host forwarded a verified claim
//! for; the signature says only that this server made the edit. Only the host service credential
//! may ask. The signature is stored only after the ref has moved; while the ref update runs the
//! gate of that update (and no other reader) sees it in memory, keyed by op id and changeset id
//! (see `verify::PendingSignature`), so a refused or lost update leaves nothing behind. A retry
//! of the same op id signs an edit that was published without a stored signature when the ref
//! log shows the host service moved the ref (SPEC §8.7). A pick or a merge made from other changesets is signed only when each of them
//! is validly signed. `"refuse"` answers such a request `UNSIGNED_CHANGE`. The
//! server advertises [`WEB_EDIT_FEATURE`] exactly when it would sign.

pub mod rpc;
pub mod store;
pub mod verify;

use std::path::{Path, PathBuf};

use asc_core::id::ChangesetId;
use asc_core::sshsig::{self, KeyFileSigner};
use serde::Deserialize;

/// What `Version.features` adds for the `Signing` service.
pub const FEATURE: &str = "signing.v1";
/// What `Version.features` adds when the server signs web edits (`web_edits = "server"` and a server
/// key is loaded).
pub const WEB_EDIT_FEATURE: &str = "sign.server.v1";
/// The `key_id` of a signature made with the server key.
pub const SERVER_KEY_ID: &str = "server";
/// The signer a server signature is attributed to.
pub const SERVER_SIGNER: &str = "asc-server";
/// Longest key title, in bytes.
pub const MAX_TITLE: usize = 128;
/// Longest revocation reason, in bytes.
pub const MAX_REASON: usize = 256;
/// Most changesets and changes one `Verify` call takes.
pub const MAX_VERIFY: usize = 100;
/// How far ahead a key may expire: ten years.
pub const MAX_KEY_LIFETIME_SECS: i64 = 10 * 365 * 24 * 3600;
/// Default of `[signing] max_keys_per_user`.
pub const DEFAULT_MAX_KEYS: usize = 32;
/// Largest server key file, in bytes.
const MAX_KEY_FILE: u64 = 64 * 1024;

/// `[signing] web_edits`: what the server does when the host asks it to sign a web edit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebEdits {
    /// Answers `UNSIGNED_CHANGE`: the server signs nothing it makes for a person.
    Refuse,
    /// Signs with the server key. Without a key the request is refused as with `Refuse`.
    #[default]
    Server,
}

/// `[signing]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// An unencrypted OpenSSH private key the server signs with; unset, the server signs nothing.
    pub server_key_file: Option<PathBuf>,
    /// Live signing keys one user may hold.
    pub max_keys_per_user: usize,
    /// `"refuse"` or `"server"` (default): whether the server signs web edits with its own key.
    pub web_edits: WebEdits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_key_file: None,
            max_keys_per_user: DEFAULT_MAX_KEYS,
            web_edits: WebEdits::default(),
        }
    }
}

impl Config {
    /// Checks the bounds. The error names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_keys_per_user == 0 || self.max_keys_per_user > 1000 {
            return Err("signing.max_keys_per_user must be 1 to 1000".into());
        }
        Ok(())
    }
}

/// The server's own signing key.
pub struct ServerSigner {
    signer: KeyFileSigner,
}

impl std::fmt::Debug for ServerSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ServerSigner({})",
            self.signer.public_key().fingerprint()
        )
    }
}

impl ServerSigner {
    /// Reads the key file. It must be readable by the server user only, small, unencrypted and of an
    /// accepted kind. The error never holds key material.
    pub fn load(path: &Path) -> Result<Self, String> {
        use std::io::Read as _;
        let file = crate::authz::key_file::open_private(path)
            .map_err(|e| format!("signing.server_key_file {}: {e}", path.display()))?;
        let mut text = String::new();
        file.take(MAX_KEY_FILE + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("signing.server_key_file {}: {e}", path.display()))?;
        if text.len() as u64 > MAX_KEY_FILE {
            return Err(format!(
                "signing.server_key_file {} is too large",
                path.display()
            ));
        }
        let signer = KeyFileSigner::from_openssh(&text)
            .map_err(|e| format!("signing.server_key_file {}: {e}", path.display()))?;
        Ok(Self { signer })
    }

    pub fn public_key(&self) -> &sshsig::PublicKey {
        self.signer.public_key()
    }

    /// The armored signature of `changeset`.
    pub fn sign(&self, changeset: &ChangesetId) -> Result<String, sshsig::Error> {
        let message = sshsig::message(&changeset.to_string());
        self.signer.sign(&message)?.armored()
    }

    /// The armored signature of `message` in the changeset namespace. Only for messages that are
    /// not changeset ids, such as the health probe's.
    pub fn sign_bytes(&self, message: &[u8]) -> Result<String, sshsig::Error> {
        self.signer.sign(message)?.armored()
    }

    /// The armored signature of an audit export whose SHA-512 is `sha512`, in the namespace
    /// [`sshsig::AUDIT_EXPORT_NAMESPACE`].
    pub fn sign_audit_export(&self, sha512: &[u8; 64]) -> Result<String, sshsig::Error> {
        self.signer.sign_audit_export(sha512)
    }
}

#[cfg(test)]
pub(crate) mod tests;
