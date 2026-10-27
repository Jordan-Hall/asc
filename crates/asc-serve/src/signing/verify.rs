//! The verdict on a changeset's signature.
//!
//! The verdict is derived, not stored: the stored signature, the key row, the time the signature was
//! submitted and the author of the changeset decide it every time it is asked. The stored `status`
//! column is the verdict at submission, kept for people reading the table.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::sshsig::{self, Signature};
use asc_rpc::server::wire::VerificationStatus as Status;
use sqlx::AnyPool;

use super::store::{self, KeyRow, SigRow};
use super::{SERVER_KEY_ID, SERVER_SIGNER, ServerSigner};

/// A verdict with the facts it rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub status: Status,
    /// The signing key id; `server` for a server signature; empty when unsigned or unknown.
    pub key_id: String,
    pub fingerprint: String,
    /// The handle of the key's owner; empty for `UnknownKey` and `Unsigned`.
    pub signer: String,
    /// When the signature was submitted.
    pub signed_at: i64,
    pub reason: String,
}

impl Verdict {
    pub fn unsigned() -> Self {
        Self {
            status: Status::Unsigned,
            key_id: String::new(),
            fingerprint: String::new(),
            signer: String::new(),
            signed_at: 0,
            reason: "no signature has been submitted for this changeset".into(),
        }
    }

    fn with(status: Status, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            ..Self::unsigned()
        }
    }

    pub fn is_valid(&self) -> bool {
        self.status == Status::Valid
    }
}

/// What a verdict needs about the changeset: the user who first pushed it.
#[derive(Clone, Debug, Default)]
pub struct Author {
    /// The handle, when the change index knows who pushed the changeset and they are a user.
    pub handle: Option<String>,
}

impl Author {
    /// The author of `changeset` in namespace `ns`.
    pub async fn of(
        pool: &AnyPool,
        ns: &[u8],
        changeset: &ChangesetId,
    ) -> Result<Self, sqlx::Error> {
        let actor = store::author_of(pool, ns, changeset).await?;
        Ok(Self {
            handle: actor.as_deref().and_then(crate::review::handle_of),
        })
    }
}

/// Judges a signature of `changeset`, submitted at `signed_at`, given the key registered for the
/// fingerprint it carries (if any) and the author. Pure.
pub fn judge(
    sig: &Signature,
    changeset: &ChangesetId,
    signed_at: i64,
    key: Option<&KeyRow>,
    author: &Author,
) -> Verdict {
    let message = sshsig::message(&changeset.to_string());
    let fingerprint = sig.key().fingerprint().to_owned();
    let mut v = Verdict {
        fingerprint,
        signed_at,
        ..Verdict::unsigned()
    };
    if sig.verify(&message).is_err() {
        v.status = Status::BadSignature;
        v.reason = "the signature does not match this changeset".into();
        return v;
    }
    let Some(key) = key else {
        v.status = Status::UnknownKey;
        v.reason = "the key is not registered by any user".into();
        return v;
    };
    v.key_id = key.id.clone();
    let Some(author) = author.handle.as_deref() else {
        v.status = Status::KeyNotAuthor;
        v.signer = key.owner.clone();
        v.reason = "the server does not know which user authored this changeset".into();
        return v;
    };
    v.signer = key.owner.clone();
    if key.owner != author {
        v.status = Status::KeyNotAuthor;
        v.reason = format!(
            "signed with a key of {}, not of the author {author}",
            key.owner
        );
        return v;
    }
    if key.expires_at != 0 && key.expires_at <= signed_at {
        v.status = Status::KeyExpired;
        v.reason = "the key had expired when the signature was submitted".into();
        return v;
    }
    if key.revoked_at != 0 && key.revoked_at <= signed_at {
        v.status = Status::KeyRevoked;
        v.reason = "the key had been revoked when the signature was submitted".into();
        return v;
    }
    v.status = Status::Valid;
    v.reason = if key.revoked_at != 0 {
        "signed before the key was revoked".into()
    } else {
        "good signature by a key of the author".into()
    };
    v
}

/// Judges a server signature: good, and made with the key the server holds now.
pub fn judge_server(
    sig: &Signature,
    changeset: &ChangesetId,
    signed_at: i64,
    server: Option<&ServerSigner>,
) -> Verdict {
    let message = sshsig::message(&changeset.to_string());
    let mut v = Verdict {
        key_id: SERVER_KEY_ID.into(),
        fingerprint: sig.key().fingerprint().to_owned(),
        signed_at,
        ..Verdict::unsigned()
    };
    if sig.verify(&message).is_err() {
        v.status = Status::BadSignature;
        v.reason = "the signature does not match this changeset".into();
        return v;
    }
    match server {
        Some(s) if s.public_key().fingerprint() == sig.key().fingerprint() => {
            v.status = Status::Valid;
            v.signer = SERVER_SIGNER.into();
            v.reason = "signed by the server".into();
        }
        _ => {
            v.status = Status::UnknownKey;
            v.reason = "signed with a server key that is not the one in use now".into();
        }
    }
    v
}

/// Reads and judges signatures from the database.
pub struct Verifier {
    pool: AnyPool,
    server: Option<Arc<ServerSigner>>,
}

impl Verifier {
    pub fn new(pool: AnyPool, server: Option<Arc<ServerSigner>>) -> Self {
        Self { pool, server }
    }

    pub fn server(&self) -> Option<&Arc<ServerSigner>> {
        self.server.as_ref()
    }

    /// The verdict on `changeset` of repository `repo_id` (namespace `ns`).
    pub async fn verdict(
        &self,
        repo_id: i64,
        ns: &[u8],
        changeset: &ChangesetId,
    ) -> Result<Verdict, sqlx::Error> {
        let Some(row) = store::get_signature(&self.pool, repo_id, changeset).await? else {
            return Ok(Verdict::unsigned());
        };
        self.judge_row(ns, changeset, &row).await
    }

    async fn judge_row(
        &self,
        ns: &[u8],
        changeset: &ChangesetId,
        row: &SigRow,
    ) -> Result<Verdict, sqlx::Error> {
        let Ok(sig) = Signature::parse(&row.signature) else {
            return Ok(Verdict::with(
                Status::Unsupported,
                "the stored signature cannot be read",
            ));
        };
        if row.key_id == SERVER_KEY_ID {
            return Ok(judge_server(
                &sig,
                changeset,
                row.signed_at,
                self.server.as_deref(),
            ));
        }
        let key = store::by_fingerprint(&self.pool, sig.key().fingerprint()).await?;
        let author = Author::of(&self.pool, ns, changeset).await?;
        Ok(judge(&sig, changeset, row.signed_at, key.as_ref(), &author))
    }

    /// Judges `armored` as if it were submitted at `at`, without storing it. The error is the
    /// refusal of a signature that cannot be read at all.
    pub async fn evaluate(
        &self,
        ns: &[u8],
        changeset: &ChangesetId,
        armored: &str,
        at: i64,
    ) -> Result<(Verdict, Option<KeyRow>), sshsig::Error> {
        let sig = Signature::parse(armored)?;
        // A server key is never accepted from a request: only the server makes those.
        let key = store::by_fingerprint(&self.pool, sig.key().fingerprint())
            .await
            .map_err(|_| sshsig::Error::Unsupported("the key registry is not available".into()))?;
        let author = Author::of(&self.pool, ns, changeset)
            .await
            .map_err(|_| sshsig::Error::Unsupported("the change index is not available".into()))?;
        Ok((judge(&sig, changeset, at, key.as_ref(), &author), key))
    }

    /// Signs `changeset` with the server key and stores the signature. `None` when the server has
    /// no key.
    pub async fn attest(
        &self,
        repo_id: i64,
        changeset: &ChangesetId,
        at: i64,
    ) -> Result<Option<()>, sqlx::Error> {
        let Some(server) = &self.server else {
            return Ok(None);
        };
        let Ok(armored) = server.sign(changeset) else {
            return Ok(None);
        };
        let row = SigRow {
            signature: armored,
            key_id: SERVER_KEY_ID.into(),
            status: i64::from(Status::Valid as i32),
            signed_at: at,
        };
        store::put_signature(&self.pool, repo_id, changeset, &row, None).await?;
        Ok(Some(()))
    }
}
