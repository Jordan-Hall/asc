//! The `Signing` service (`signing.proto`).
//!
//! Key calls (`ListKeys`, `AddKey`, `RevokeKey`, `GetServerKey`) name no repository: the caller is
//! the authenticated principal, and keys are scoped to the user handle taken from it, never from a
//! request field. Another user's keys are visible to the host service only (the site administrator
//! acting through the hub), and every other caller gets `NOT_FOUND` for them, so handles cannot be
//! probed. `SubmitSignature` needs repo write and `Verify` repo read.

use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::sshsig::{self, KeyKind, PublicKey};
use asc_rpc::auth::{Access, AuthInterceptor, Authorizer, Credential};
use asc_rpc::convert::WireId;
use asc_rpc::errors::RpcError;
use asc_rpc::forwarded::Role;
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::signing_server::{Signing as SigningApi, SigningServer};
use bytes::Bytes;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions as TonicExt, Request, Response, Status};
use uuid::Uuid;

use super::store::{self, AddError, KeyRow, NewKey, SigRow};
use super::verify::{Verdict, Verifier};
use super::{Config, MAX_KEY_LIFETIME_SECS, MAX_REASON, MAX_TITLE, MAX_VERIFY, ServerSigner};
use crate::audit::NewEvent;
use crate::audit::actor::actor_of;
use crate::authz::identity::is_host_service;
use crate::idmode::IdCodec;
use crate::registry::{Registry, RepoEntry};
use crate::review::{Env, handle_of, internal, now, registry_status, sql};
use crate::update::{info, rpc};

use wire::VerificationStatus as Status_;

/// The state the `Signing` service shares.
pub struct Signing {
    registry: Arc<Registry>,
    verifier: Verifier,
    cfg: Config,
}

fn kind_to_wire(k: KeyKind) -> wire::SigningKeyKind {
    match k {
        KeyKind::Ed25519 => wire::SigningKeyKind::SshEd25519,
        KeyKind::EcdsaP256 => wire::SigningKeyKind::SshEcdsaP256,
        KeyKind::Rsa => wire::SigningKeyKind::SshRsa,
    }
}

fn key_to_wire(k: &KeyRow) -> wire::SigningKey {
    wire::SigningKey {
        id: k.id.clone(),
        owner: k.owner.clone(),
        title: k.title.clone(),
        kind: kind_to_wire(k.kind) as i32,
        fingerprint: k.fingerprint.clone(),
        public_key: k.public_key.clone(),
        created: k.created,
        expires_at: k.expires_at,
        revoked_at: k.revoked_at,
        revoked_reason: k.revoked_reason.clone(),
        last_used: k.last_used,
    }
}

fn clean_text(s: &str, max: usize, what: &str) -> Result<String, Status> {
    let t = s.trim();
    if t.len() > max || t.chars().any(char::is_control) {
        return Err(Status::invalid_argument(format!(
            "{what} is at most {max} bytes of text"
        )));
    }
    Ok(t.to_owned())
}

impl Signing {
    pub fn new(registry: Arc<Registry>, cfg: Config, server: Option<Arc<ServerSigner>>) -> Self {
        let verifier = Verifier::new(registry.pool().clone(), server);
        Self {
            registry,
            verifier,
            cfg,
        }
    }

    pub fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The handle of the user a principal acts as, or `PERMISSION_DENIED`.
    fn handle(principal_id: &str) -> Result<String, Status> {
        handle_of(principal_id)
            .ok_or_else(|| Status::permission_denied("this call needs a user identity"))
    }

    /// `Signing.ListKeys`.
    pub async fn list_keys(
        &self,
        principal_id: &str,
        req: wire::ListSigningKeysRequest,
    ) -> Result<wire::ListSigningKeysResponse, Status> {
        let me = handle_of(principal_id);
        let owner = if req.owner.is_empty() {
            Self::handle(principal_id)?
        } else {
            if !crate::review::valid_handle(&req.owner) {
                return Err(Status::invalid_argument("invalid owner"));
            }
            if me.as_deref() != Some(req.owner.as_str()) && !is_host_service(principal_id) {
                // Not the caller's own keys and not the site administrator: nothing is there.
                return Err(Status::not_found("no such user"));
            }
            req.owner
        };
        let keys = store::list_keys(self.registry.pool(), &owner)
            .await
            .map_err(sql)?;
        Ok(wire::ListSigningKeysResponse {
            keys: keys.iter().map(key_to_wire).collect(),
        })
    }

    /// `Signing.AddKey`.
    pub async fn add_key(
        &self,
        principal_id: &str,
        relay: Option<&str>,
        req: wire::AddSigningKeyRequest,
    ) -> Result<wire::AddSigningKeyResponse, Status> {
        let owner = Self::handle(principal_id)?;
        let key = PublicKey::parse(&req.public_key)
            .map_err(|e| Status::invalid_argument(format!("not an accepted public key: {e}")))?;
        let title = clean_text(&req.title, MAX_TITLE, "the title")?;
        let at = now();
        if req.expires_at != 0
            && (req.expires_at <= at || req.expires_at > at + MAX_KEY_LIFETIME_SECS)
        {
            return Err(Status::invalid_argument(
                "expires_at must be in the future and at most ten years ahead",
            ));
        }
        let pool = self.registry.pool();
        if let Some(existing) = store::by_fingerprint(pool, key.fingerprint())
            .await
            .map_err(sql)?
        {
            // A retry by the owner returns the key; the key of anyone else is taken.
            if existing.owner == owner && existing.revoked_at == 0 {
                return Ok(wire::AddSigningKeyResponse {
                    key: Some(key_to_wire(&existing)),
                });
            }
            return Err(Status::already_exists("this key is already registered"));
        }
        let new = NewKey {
            id: format!("sk_{}", Uuid::new_v4().simple()),
            owner: owner.clone(),
            title: if title.is_empty() {
                format!("{} key", key.kind().algorithm())
            } else {
                title
            },
            kind: key.kind(),
            fingerprint: key.fingerprint().to_owned(),
            public_key: key.line().to_owned(),
            created: at,
            expires_at: req.expires_at,
        };
        let mut event = NewEvent::new("signing.key.added", actor_of(principal_id))
            .owner(&owner)
            .target(&new.id)
            .detail("fingerprint", key.fingerprint())
            .detail("kind", key.kind().algorithm());
        if let Some(r) = relay {
            event = event.on_behalf_of(actor_of(r));
        }
        match store::add_key(pool, &new, self.cfg.max_keys_per_user, Some(&event)).await {
            Ok(row) => Ok(wire::AddSigningKeyResponse {
                key: Some(key_to_wire(&row)),
            }),
            Err(AddError::Limit) => Err(rpc(RpcError::SigningKeyLimit(info(vec![format!(
                "at most {} live keys",
                self.cfg.max_keys_per_user
            )])))),
            Err(AddError::Duplicate) => {
                Err(Status::already_exists("this key is already registered"))
            }
            Err(AddError::Sql(e)) => Err(sql(e)),
        }
    }

    /// `Signing.RevokeKey`.
    pub async fn revoke_key(
        &self,
        principal_id: &str,
        relay: Option<&str>,
        req: wire::RevokeSigningKeyRequest,
    ) -> Result<wire::RevokeSigningKeyResponse, Status> {
        let me = handle_of(principal_id);
        let reason = clean_text(&req.reason, MAX_REASON, "the reason")?;
        if req.id.is_empty() || req.id.len() > 64 {
            return Err(Status::invalid_argument("invalid key id"));
        }
        let pool = self.registry.pool();
        let Some(key) = store::get_key(pool, &req.id).await.map_err(sql)? else {
            return Err(Status::not_found("no such key"));
        };
        let allowed = me.as_deref() == Some(key.owner.as_str()) || is_host_service(principal_id);
        if !allowed {
            return Err(Status::not_found("no such key"));
        }
        let mut event = NewEvent::new("signing.key.revoked", actor_of(principal_id))
            .owner(&key.owner)
            .target(&key.id)
            .detail("fingerprint", &key.fingerprint)
            .detail("kind", key.kind.algorithm());
        if let Some(r) = relay {
            event = event.on_behalf_of(actor_of(r));
        }
        let revoked = store::revoke_key(pool, &key.id, &reason, now(), Some(&event))
            .await
            .map_err(sql)?
            .ok_or_else(|| Status::not_found("no such key"))?;
        Ok(wire::RevokeSigningKeyResponse {
            key: Some(key_to_wire(&revoked)),
        })
    }

    /// `Signing.GetServerKey`.
    pub fn get_server_key(&self) -> wire::GetServerKeyResponse {
        match self.verifier.server() {
            None => wire::GetServerKeyResponse::default(),
            Some(s) => {
                let k = s.public_key();
                wire::GetServerKeyResponse {
                    configured: true,
                    kind: kind_to_wire(k.kind()) as i32,
                    fingerprint: k.fingerprint().to_owned(),
                    public_key: k.line().to_owned(),
                }
            }
        }
    }

    async fn open(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(registry_status)
    }

    fn codec(&self, ctx: &CallCtx, entry: &RepoEntry) -> IdCodec {
        IdCodec::new(ctx.id_mode, &self.registry, entry).for_viewer(&ctx.principal)
    }

    async fn exists(entry: &RepoEntry, cs: ChangesetId) -> Result<(), Status> {
        match entry.backend.engine().get_changeset(cs).await {
            Ok(_) => Ok(()),
            Err(asc_engine::commit::Error::MissingChangeset(_)) => {
                Err(Status::not_found(format!("unknown changeset {cs}")))
            }
            Err(e) => Err(internal("changeset", &e)),
        }
    }

    async fn change_of(&self, entry: &RepoEntry, cs: &ChangesetId) -> Result<Bytes, Status> {
        let row: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
                .bind(entry.ns.as_bytes().to_vec())
                .bind(cs.as_bytes().to_vec())
                .fetch_optional(self.registry.pool())
                .await
                .map_err(sql)?;
        Ok(row.map(Bytes::from).unwrap_or_default())
    }

    async fn verification(
        &self,
        ids: &IdCodec,
        entry: &RepoEntry,
        cs: ChangesetId,
        v: &Verdict,
    ) -> Result<wire::SignatureVerification, Status> {
        Ok(wire::SignatureVerification {
            changeset: ids.changeset_out(cs).await?,
            change: self.change_of(entry, &cs).await?,
            status: v.status as i32,
            key_id: v.key_id.clone(),
            fingerprint: v.fingerprint.clone(),
            signer: v.signer.clone(),
            signed_at: v.signed_at,
            reason: v.reason.clone(),
        })
    }

    /// `Signing.SubmitSignature`.
    pub async fn submit_signature(
        &self,
        ctx: &CallCtx,
        req: wire::SubmitSignatureRequest,
    ) -> Result<wire::SubmitSignatureResponse, Status> {
        if req.signature.len() > sshsig::MAX_SIGNATURE_BYTES {
            return Err(rpc(RpcError::SignatureInvalid(info(vec![
                "the signature is too long".into(),
            ]))));
        }
        let entry = self.open(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let cs = ids.changeset_in(&req.changeset).await?;
        Self::exists(&entry, cs).await?;
        let at = now();
        let ns = entry.ns.as_bytes();
        let (verdict, key) = match self.verifier.evaluate(ns, &cs, &req.signature, at).await {
            Ok(v) => v,
            Err(e) => {
                return Err(rpc(RpcError::SignatureInvalid(info(vec![e.to_string()]))));
            }
        };
        if matches!(verdict.status, Status_::BadSignature | Status_::Unsupported) {
            return Err(rpc(RpcError::SignatureInvalid(info(vec![verdict.reason]))));
        }
        let pool = self.registry.pool();
        // A signature never replaces a valid one with a worse one: anyone with write access could
        // otherwise unsign a change by submitting a signature that does not count.
        if !verdict.is_valid() {
            let current = self
                .verifier
                .verdict(entry.key.0, ns, &cs)
                .await
                .map_err(sql)?;
            if current.is_valid() {
                return Ok(wire::SubmitSignatureResponse {
                    verification: Some(self.verification(&ids, &entry, cs, &current).await?),
                });
            }
        }
        let armored = sshsig::Signature::parse(&req.signature)
            .and_then(|s| s.armored())
            .map_err(|e| rpc(RpcError::SignatureInvalid(info(vec![e.to_string()]))))?;
        let row = SigRow {
            signature: armored,
            key_id: key.as_ref().map(|k| k.id.clone()).unwrap_or_default(),
            status: i64::from(verdict.status as i32),
            signed_at: at,
        };
        let event = NewEvent::new("signing.signature.submitted", actor_of(&ctx.principal.id))
            .repo(ctx.repo.as_str())
            .target(&cs.to_string())
            .detail("change", hex_of(&self.change_of(&entry, &cs).await?))
            .detail("fingerprint", &verdict.fingerprint);
        store::put_signature(pool, entry.key.0, &cs, &row, Some(&event))
            .await
            .map_err(sql)?;
        if let (true, Some(k)) = (verdict.is_valid(), key.as_ref()) {
            store::touch_key(pool, &k.id, at).await.map_err(sql)?;
        }
        Ok(wire::SubmitSignatureResponse {
            verification: Some(self.verification(&ids, &entry, cs, &verdict).await?),
        })
    }

    /// `Signing.Verify`.
    pub async fn verify(
        &self,
        ctx: &CallCtx,
        req: wire::VerifySignaturesRequest,
    ) -> Result<wire::VerifySignaturesResponse, Status> {
        if req.changesets.len() + req.changes.len() > MAX_VERIFY {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_VERIFY} changesets and changes in one call"
            )));
        }
        let entry = self.open(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let ns = entry.ns.as_bytes();
        let mut targets = Vec::with_capacity(req.changesets.len() + req.changes.len());
        for raw in &req.changesets {
            targets.push(ids.changeset_in(raw).await?);
        }
        for raw in &req.changes {
            let change = <[u8; 16]>::try_from(raw.as_ref())
                .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))?;
            let cs: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT cs FROM change_versions WHERE ns = $1 AND change = $2 ORDER BY v DESC LIMIT 1",
            )
            .bind(ns.to_vec())
            .bind(change.to_vec())
            .fetch_optional(self.registry.pool())
            .await
            .map_err(sql)?;
            let cs = cs.ok_or_else(|| Status::not_found("unknown change"))?;
            targets.push(ChangesetId::from_slice(&cs).map_err(|e| internal("change index", &e))?);
        }
        let mut results = Vec::with_capacity(targets.len());
        for cs in targets {
            Self::exists(&entry, cs).await?;
            let v = self
                .verifier
                .verdict(entry.key.0, ns, &cs)
                .await
                .map_err(sql)?;
            results.push(self.verification(&ids, &entry, cs, &v).await?);
        }
        Ok(wire::VerifySignaturesResponse { results })
    }
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Signing` tonic service.
pub struct SigningService {
    hub: Arc<Signing>,
    env: Env,
}

impl SigningService {
    pub fn new(hub: Arc<Signing>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<SigningServer<Self>, AuthInterceptor> {
        let server = SigningServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Who is calling a method that names no repository. The hub's web pages call with the host
    /// service's credential and a claim for the account (`user:<login>`, at least `needed`): the
    /// call then acts as that person, with their rights and no more. Without a claim the host
    /// service is the host service.
    async fn caller(
        &self,
        ext: &TonicExt,
        meta: &tonic::metadata::MetadataMap,
        needed: asc_rpc::forwarded::Role,
    ) -> Result<(String, Option<String>), Status> {
        let id = self
            .env
            .authorizer
            .authenticate(ext.get::<Credential>())
            .await?
            .id;
        let acting =
            crate::authz::claim::acting_principal(&*self.env.authorizer, &id, meta, needed)?;
        // The host service stays on the record as the relay of a call made for a person.
        let relay = (acting != id).then_some(id);
        Ok((acting, relay))
    }
}

#[tonic::async_trait]
impl SigningApi for SigningService {
    async fn list_keys(
        &self,
        request: Request<wire::ListSigningKeysRequest>,
    ) -> Result<Response<wire::ListSigningKeysResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (who, _) = self.caller(&ext, &meta, Role::Read).await?;
        self.hub.list_keys(&who, msg).await.map(Response::new)
    }

    async fn add_key(
        &self,
        request: Request<wire::AddSigningKeyRequest>,
    ) -> Result<Response<wire::AddSigningKeyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (who, relay) = self.caller(&ext, &meta, Role::Write).await?;
        self.hub
            .add_key(&who, relay.as_deref(), msg)
            .await
            .map(Response::new)
    }

    async fn revoke_key(
        &self,
        request: Request<wire::RevokeSigningKeyRequest>,
    ) -> Result<Response<wire::RevokeSigningKeyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (who, relay) = self.caller(&ext, &meta, Role::Write).await?;
        self.hub
            .revoke_key(&who, relay.as_deref(), msg)
            .await
            .map(Response::new)
    }

    async fn submit_signature(
        &self,
        request: Request<wire::SubmitSignatureRequest>,
    ) -> Result<Response<wire::SubmitSignatureResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        self.hub
            .submit_signature(&ctx, msg)
            .await
            .map(Response::new)
    }

    async fn verify(
        &self,
        request: Request<wire::VerifySignaturesRequest>,
    ) -> Result<Response<wire::VerifySignaturesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.verify(&ctx, msg).await.map(Response::new)
    }

    async fn get_server_key(
        &self,
        request: Request<wire::GetServerKeyRequest>,
    ) -> Result<Response<wire::GetServerKeyResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        // Public information, but only for a caller the server can name.
        self.caller(&ext, &meta, Role::Read).await?;
        Ok(Response::new(self.hub.get_server_key()))
    }
}
