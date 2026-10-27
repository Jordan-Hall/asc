//! The `Storage` and `Vault` tonic services: parse, authenticate, call [`Service`], convert.
//!
//! Every call authenticates first (an anonymous caller is `UNAUTHENTICATED`), and the decision to
//! allow it is `Service`'s. A mutating call carries `x-asc-op-id`; a retried id returns the first
//! answer from `op_results`. Nothing here formats a credential, an endpoint with a query, or a
//! provider message.

use std::sync::Arc;

use asc_rpc::RpcError;
use asc_rpc::auth::{AuthError, AuthInterceptor, Authorizer, Credential};
use asc_rpc::errors::ErrorInfo;
use asc_rpc::server::{rpc_status, wire};
use asc_rpc::services::storage_server::{Storage, StorageServer};
use asc_rpc::services::vault_server::{Vault as VaultSvc, VaultServer};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use prost::Message;
use sha2::{Digest, Sha256};
use sqlx::AnyPool;
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::audit::{Audit, AuditEvent};
use super::authority::{Authority, Caller};
use super::db::MigrationRow;
use super::live::{Check, Report, Step};
use super::model::{self, Addressing, Credentials, Owner, OwnerKind, State};
use super::service::{NewFields, Patch, Service, StorageError, View};
use crate::authz::UserIdentity;
use crate::vault::{Vault, VaultError};

/// The `Version.features` entries.
pub const STORAGE_FEATURE: &str = "storage.v1";
pub const VAULT_FEATURE: &str = "vault.v1";
/// Largest request either service accepts. The biggest field is a session token.
const MAX_REQUEST: usize = 64 * 1024;
/// Longest page token.
const MAX_PAGE_TOKEN: usize = 256;

fn with_message(status: Status, message: &str) -> Status {
    Status::with_details(status.code(), message, status.details().to_vec().into())
}

fn blocked(reason: crate::netguard::BlockReason) -> Status {
    let info = ErrorInfo {
        paths: vec![reason.slug().to_owned()],
        ..ErrorInfo::default()
    };
    with_message(
        rpc_status(&RpcError::StorageEndpointBlocked(info)),
        reason.message(),
    )
}

/// The status of a [`StorageError`].
pub fn status_of(e: StorageError) -> Status {
    let plain = |f: fn(ErrorInfo) -> RpcError, msg: &str| {
        with_message(rpc_status(&f(ErrorInfo::default())), msg)
    };
    match e {
        StorageError::InvalidArgument(m) => Status::invalid_argument(m),
        StorageError::Blocked(r) => blocked(r),
        StorageError::NotFound(m) => Status::not_found(m),
        StorageError::PermissionDenied(m) => Status::permission_denied(m),
        StorageError::AlreadyExists(m) => Status::already_exists(m),
        StorageError::VerifyFailed(m) => plain(RpcError::StorageVerifyFailed, &m),
        StorageError::InUse(m) => plain(RpcError::StorageInUse, &m),
        StorageError::MigrationActive => plain(
            RpcError::MigrationActive,
            "a migration of this repository is already running",
        ),
        StorageError::Quota(m) => plain(RpcError::QuotaExceeded, &m),
        StorageError::RateLimited => plain(
            RpcError::RateLimited,
            "this target was verified too often; try again in an hour",
        ),
        StorageError::VaultLocked(why) => plain(
            RpcError::VaultLocked,
            &format!("the vault is locked: {why}"),
        ),
        StorageError::FailedPrecondition(m) => Status::failed_precondition(m),
        StorageError::Unavailable(m) => Status::unavailable(m),
        StorageError::CredentialsRequired { field } => Status::invalid_argument(format!(
            "{field}: changing it points the stored credentials at a new destination, so supply new credentials in the same request"
        )),
        StorageError::Internal => Status::internal("internal error"),
    }
}

fn vault_status(e: &VaultError) -> Status {
    match e {
        VaultError::Locked(why) => with_message(
            rpc_status(&RpcError::VaultLocked(ErrorInfo::default())),
            &format!("the vault is locked: {why}"),
        ),
        VaultError::RotationActive => with_message(
            rpc_status(&RpcError::RotationActive(ErrorInfo::default())),
            "a rotation is already running",
        ),
        other => {
            tracing::error!(error = %other, "vault error");
            Status::internal("internal error")
        }
    }
}

// ------------------------------------------------------------------------------ plumbing

/// Authenticates the request's credential.
async fn caller_of(
    authorizer: &Arc<dyn Authorizer>,
    ext: &tonic::Extensions,
) -> Result<Caller, Status> {
    let credential = ext.get::<Credential>().cloned();
    let principal = authorizer
        .authenticate(credential.as_ref())
        .await
        .map_err(Status::from)?;
    if UserIdentity::of(&principal).is_some_and(|u| u.is_anonymous()) {
        return Err(Status::from(AuthError::Unauthenticated));
    }
    Ok(Caller {
        principal,
        credential,
    })
}

fn op_id(meta: &MetadataMap) -> Result<Uuid, Status> {
    let v = meta
        .get("x-asc-op-id")
        .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
    v.to_str()
        .ok()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Status::invalid_argument("malformed x-asc-op-id"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Marks a stored answer with the caller it was made for: the SHA-256 of the principal id.
fn answer_owner(caller: &Caller) -> [u8; 32] {
    Sha256::digest(caller.principal.id.as_bytes()).into()
}

/// The first answer of a retried mutating call. The answer holds the target or migration the
/// first caller was allowed to see, so only that caller gets it back; any other principal who
/// presents the id is refused, and learns nothing of the answer.
async fn replay<M: Message + Default>(
    pool: &AnyPool,
    op: Uuid,
    caller: &Caller,
) -> Result<Option<M>, Status> {
    let row: Option<Vec<u8>> = sqlx::query_scalar("SELECT result FROM op_results WHERE op_id = $1")
        .bind(op.as_bytes().to_vec())
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "op_results read");
            Status::internal("internal error")
        })?;
    let Some(raw) = row else {
        return Ok(None);
    };
    let Some((owner, body)) = raw.split_first_chunk::<32>() else {
        return Err(Status::invalid_argument(
            "the operation id was used for another call",
        ));
    };
    if *owner != answer_owner(caller) {
        return Err(Status::invalid_argument(
            "the operation id was used for another call",
        ));
    }
    M::decode(body)
        .map(Some)
        .map_err(|_| Status::internal("internal error"))
}

async fn remember<M: Message>(pool: &AnyPool, op: Uuid, caller: &Caller, m: &M) {
    let mut stored = answer_owner(caller).to_vec();
    stored.extend_from_slice(&m.encode_to_vec());
    let r = sqlx::query(
        "INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, 0, $2, $3) ON CONFLICT (op_id) DO NOTHING",
    )
    .bind(op.as_bytes().to_vec())
    .bind(stored)
    .bind(now())
    .execute(pool)
    .await;
    if let Err(e) = r {
        tracing::error!(error = %e, "op_results write");
    }
}

fn owner_of(r: Option<&wire::OwnerRef>) -> Result<Owner, Status> {
    let r = r.ok_or_else(|| Status::invalid_argument("missing owner"))?;
    let kind = match wire::OwnerKind::try_from(r.kind) {
        Ok(wire::OwnerKind::User) => OwnerKind::User,
        Ok(wire::OwnerKind::Org) => OwnerKind::Org,
        Ok(wire::OwnerKind::Repo) => OwnerKind::Repo,
        _ => {
            return Err(Status::invalid_argument(
                "owner kind must be user, org or repo",
            ));
        }
    };
    Owner::new(kind, &r.name).map_err(Status::invalid_argument)
}

fn owner_pb(o: &Owner) -> wire::OwnerRef {
    let kind = match o.kind {
        OwnerKind::User => wire::OwnerKind::User,
        OwnerKind::Org => wire::OwnerKind::Org,
        OwnerKind::Repo => wire::OwnerKind::Repo,
    };
    wire::OwnerRef {
        kind: kind as i32,
        name: o.name.clone(),
    }
}

fn addressing_in(v: i32) -> Result<Addressing, Status> {
    match wire::StorageAddressing::try_from(v) {
        Ok(wire::StorageAddressing::Auto) => Ok(Addressing::Auto),
        Ok(wire::StorageAddressing::Path) => Ok(Addressing::Path),
        Ok(wire::StorageAddressing::VirtualHost) => Ok(Addressing::VirtualHost),
        Err(_) => Err(Status::invalid_argument("unknown addressing")),
    }
}

fn addressing_out(a: Addressing) -> i32 {
    let v = match a {
        Addressing::Auto => wire::StorageAddressing::Auto,
        Addressing::Path => wire::StorageAddressing::Path,
        Addressing::VirtualHost => wire::StorageAddressing::VirtualHost,
    };
    v as i32
}

fn credentials_in(c: &wire::StorageCredentials) -> Result<Credentials, Status> {
    Credentials::new(&c.access_key_id, &c.secret_access_key, &c.session_token)
        .map_err(Status::invalid_argument)
}

/// The wire form of a target. Credentials are not in [`View`], so they cannot be in the answer.
pub fn target_pb(v: &View) -> wire::StorageTarget {
    let t = &v.target;
    let state = match t.state {
        State::Pending => wire::StorageTargetState::Pending,
        State::Verified => wire::StorageTargetState::Verified,
        State::Enabled => wire::StorageTargetState::Enabled,
        State::Disabled => wire::StorageTargetState::Disabled,
        State::Failed => wire::StorageTargetState::Failed,
    };
    wire::StorageTarget {
        id: t.id.clone(),
        owner: Some(owner_pb(&t.owner)),
        name: t.name.clone(),
        endpoint: t.endpoint.clone(),
        region: t.region.clone(),
        bucket: t.bucket.clone(),
        prefix: model::prefix_out(&t.prefix),
        addressing: addressing_out(t.addressing),
        access_key_hint: t.access_key_hint.clone(),
        has_secret: true,
        state: state as i32,
        created: t.created,
        updated: t.updated,
        verified_at: t.verified_at,
        last_error: t.last_error.clone(),
        key_version: t.key_gen,
        repos: v.repos,
    }
}

fn migration_pb(m: &MigrationRow) -> wire::StorageMigration {
    use super::migrate::state as s;
    let state = match m.state.as_str() {
        s::QUEUED => wire::StorageMigrationState::Queued,
        s::COPYING => wire::StorageMigrationState::Copying,
        s::VERIFYING => wire::StorageMigrationState::Verifying,
        s::SWITCHING => wire::StorageMigrationState::Switching,
        s::DONE => wire::StorageMigrationState::Done,
        s::FAILED => wire::StorageMigrationState::Failed,
        s::CANCELLED => wire::StorageMigrationState::Cancelled,
        _ => wire::StorageMigrationState::Unspecified,
    };
    wire::StorageMigration {
        id: m.id.clone(),
        repo: Some(wire::RepoRef {
            name: m.repo_name.clone(),
            ..Default::default()
        }),
        from_target_id: m.from_target.clone(),
        to_target_id: m.to_target.clone(),
        state: state as i32,
        objects_total: u64::try_from(m.objects_total).unwrap_or(0),
        objects_copied: u64::try_from(m.objects_copied).unwrap_or(0),
        bytes_copied: u64::try_from(m.bytes_copied).unwrap_or(0),
        delete_source: m.delete_source,
        created: m.created_ms / 1000,
        updated: m.updated,
        error: m.error.clone(),
    }
}

fn check_pb(c: &Check) -> wire::StorageVerifyCheck {
    let step = match c.step {
        Step::Resolve => wire::StorageVerifyStep::Resolve,
        Step::Connect => wire::StorageVerifyStep::Connect,
        Step::Put => wire::StorageVerifyStep::Put,
        Step::Get => wire::StorageVerifyStep::Get,
        Step::Delete => wire::StorageVerifyStep::Delete,
    };
    wire::StorageVerifyCheck {
        step: step as i32,
        ok: c.ok,
        latency_ms: c.latency_ms,
        message: c.message.clone(),
    }
}

fn report_pb(r: &Report) -> Vec<wire::StorageVerifyCheck> {
    r.checks.iter().map(check_pb).collect()
}

fn repo_name(r: Option<&wire::RepoRef>) -> Result<String, Status> {
    let r = r.ok_or_else(|| Status::invalid_argument("missing repo"))?;
    asc_rpc::server::RepoName::parse(&r.name).map(|n| n.as_str().to_owned())
}

fn encode_token(created: i64, id: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{created}:{id}"))
}

fn decode_token(t: &str) -> Result<Option<(i64, String)>, Status> {
    if t.is_empty() {
        return Ok(None);
    }
    let bad = || Status::invalid_argument("bad page token");
    if t.len() > MAX_PAGE_TOKEN {
        return Err(bad());
    }
    let raw = URL_SAFE_NO_PAD.decode(t).map_err(|_| bad())?;
    let text = String::from_utf8(raw).map_err(|_| bad())?;
    let (c, id) = text.split_once(':').ok_or_else(bad)?;
    if !model::is_id("sm_", id) {
        return Err(bad());
    }
    Ok(Some((c.parse().map_err(|_| bad())?, id.to_owned())))
}

// ------------------------------------------------------------------------------ Storage

/// The `asc.v1.Storage` service.
pub struct StorageService {
    svc: Arc<Service>,
    authorizer: Arc<dyn Authorizer>,
}

impl StorageService {
    pub fn new(svc: Arc<Service>, authorizer: Arc<dyn Authorizer>) -> Self {
        Self { svc, authorizer }
    }

    /// The tonic service behind the credential parser.
    pub fn into_service(self) -> InterceptedService<StorageServer<Self>, AuthInterceptor> {
        let server = StorageServer::new(self)
            .max_decoding_message_size(MAX_REQUEST)
            .max_encoding_message_size(MAX_REQUEST * 16);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn caller(&self, ext: &tonic::Extensions) -> Result<Caller, Status> {
        caller_of(&self.authorizer, ext).await
    }

    fn pool(&self) -> &AnyPool {
        &self.svc.db.pool
    }
}

#[tonic::async_trait]
impl Storage for StorageService {
    async fn create_target(
        &self,
        request: Request<wire::CreateStorageTargetRequest>,
    ) -> Result<Response<wire::CreateStorageTargetResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let op = op_id(&meta)?;
        let owner = owner_of(msg.owner.as_ref())?;
        self.svc
            .authorize_owner(&caller, &owner)
            .await
            .map_err(status_of)?;
        if let Some(prev) = replay(self.pool(), op, &caller).await? {
            return Ok(Response::new(prev));
        }
        let fields = NewFields {
            owner,
            name: msg.name,
            endpoint: msg.endpoint,
            region: msg.region,
            bucket: msg.bucket,
            prefix: msg.prefix,
            addressing: addressing_in(msg.addressing)?,
            credentials: credentials_in(
                msg.credentials
                    .as_ref()
                    .ok_or_else(|| Status::invalid_argument("credentials are required"))?,
            )?,
        };
        let out = match self.svc.create_target(&caller, fields).await {
            Ok(v) => wire::CreateStorageTargetResponse {
                target: Some(target_pb(&v)),
            },
            Err(e) => {
                // A retry that raced the first call finds its answer.
                if let Some(prev) = replay(self.pool(), op, &caller).await? {
                    return Ok(Response::new(prev));
                }
                return Err(status_of(e));
            }
        };
        remember(self.pool(), op, &caller, &out).await;
        Ok(Response::new(out))
    }

    async fn list_targets(
        &self,
        request: Request<wire::ListStorageTargetsRequest>,
    ) -> Result<Response<wire::ListStorageTargetsResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let owner = owner_of(msg.owner.as_ref())?;
        let views = self
            .svc
            .list_targets(&caller, &owner)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::ListStorageTargetsResponse {
            targets: views.iter().map(target_pb).collect(),
        }))
    }

    async fn get_target(
        &self,
        request: Request<wire::GetStorageTargetRequest>,
    ) -> Result<Response<wire::GetStorageTargetResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let v = self
            .svc
            .get_target(&caller, &msg.id)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::GetStorageTargetResponse {
            target: Some(target_pb(&v)),
        }))
    }

    async fn update_target(
        &self,
        request: Request<wire::UpdateStorageTargetRequest>,
    ) -> Result<Response<wire::UpdateStorageTargetResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let op = op_id(&meta)?;
        self.svc
            .authorize_target(&caller, &msg.id)
            .await
            .map_err(status_of)?;
        if let Some(prev) = replay(self.pool(), op, &caller).await? {
            return Ok(Response::new(prev));
        }
        let patch = Patch {
            name: msg.name,
            endpoint: msg.endpoint,
            region: msg.region,
            bucket: msg.bucket,
            prefix: msg.prefix,
            addressing: msg.addressing.map(addressing_in).transpose()?,
            credentials: msg.credentials.as_ref().map(credentials_in).transpose()?,
        };
        let v = self
            .svc
            .update_target(&caller, &msg.id, patch)
            .await
            .map_err(status_of)?;
        let out = wire::UpdateStorageTargetResponse {
            target: Some(target_pb(&v)),
        };
        remember(self.pool(), op, &caller, &out).await;
        Ok(Response::new(out))
    }

    async fn delete_target(
        &self,
        request: Request<wire::DeleteStorageTargetRequest>,
    ) -> Result<Response<wire::DeleteStorageTargetResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        self.svc
            .delete_target(&caller, &msg.id)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::DeleteStorageTargetResponse {}))
    }

    async fn verify_target(
        &self,
        request: Request<wire::VerifyStorageTargetRequest>,
    ) -> Result<Response<wire::VerifyStorageTargetResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let (report, view) = self
            .svc
            .verify_target(&caller, &msg.id)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::VerifyStorageTargetResponse {
            ok: report.ok(),
            checks: report_pb(&report),
            target: Some(target_pb(&view)),
        }))
    }

    async fn set_enabled(
        &self,
        request: Request<wire::SetStorageTargetEnabledRequest>,
    ) -> Result<Response<wire::SetStorageTargetEnabledResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let v = self
            .svc
            .set_enabled(&caller, &msg.id, msg.enabled)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::SetStorageTargetEnabledResponse {
            target: Some(target_pb(&v)),
        }))
    }

    async fn assign_repo(
        &self,
        request: Request<wire::AssignRepoStorageRequest>,
    ) -> Result<Response<wire::AssignRepoStorageResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let repo = repo_name(msg.repo.as_ref())?;
        let id = self
            .svc
            .assign_repo(&caller, &repo, &msg.target_id)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::AssignRepoStorageResponse {
            target_id: id,
        }))
    }

    async fn start_migration(
        &self,
        request: Request<wire::StartStorageMigrationRequest>,
    ) -> Result<Response<wire::StartStorageMigrationResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let op = op_id(&meta)?;
        let repo = repo_name(msg.repo.as_ref())?;
        self.svc
            .authorize_repo(&caller, &repo)
            .await
            .map_err(status_of)?;
        if let Some(prev) = replay(self.pool(), op, &caller).await? {
            return Ok(Response::new(prev));
        }
        let m = self
            .svc
            .start_migration(&caller, &repo, &msg.to_target_id, msg.delete_source)
            .await
            .map_err(status_of)?;
        let out = wire::StartStorageMigrationResponse {
            migration: Some(migration_pb(&m)),
        };
        remember(self.pool(), op, &caller, &out).await;
        Ok(Response::new(out))
    }

    async fn get_migration(
        &self,
        request: Request<wire::GetStorageMigrationRequest>,
    ) -> Result<Response<wire::GetStorageMigrationResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let m = self
            .svc
            .get_migration(&caller, &msg.id)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::GetStorageMigrationResponse {
            migration: Some(migration_pb(&m)),
        }))
    }

    async fn list_migrations(
        &self,
        request: Request<wire::ListStorageMigrationsRequest>,
    ) -> Result<Response<wire::ListStorageMigrationsResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let repo = repo_name(msg.repo.as_ref())?;
        let after = decode_token(&msg.page_token)?;
        let (rows, next) = self
            .svc
            .list_migrations(&caller, &repo, after, msg.limit)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::ListStorageMigrationsResponse {
            migrations: rows.iter().map(migration_pb).collect(),
            next_page_token: next.map(|(c, id)| encode_token(c, &id)).unwrap_or_default(),
        }))
    }

    async fn cancel_migration(
        &self,
        request: Request<wire::CancelStorageMigrationRequest>,
    ) -> Result<Response<wire::CancelStorageMigrationResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let caller = self.caller(&ext).await?;
        let m = self
            .svc
            .cancel_migration(&caller, &msg.id)
            .await
            .map_err(status_of)?;
        Ok(Response::new(wire::CancelStorageMigrationResponse {
            migration: Some(migration_pb(&m)),
        }))
    }
}

// ------------------------------------------------------------------------------ Vault

/// The `asc.v1.Vault` service. Site administrators only.
pub struct VaultService {
    vault: Arc<Vault>,
    authority: Arc<dyn Authority>,
    authorizer: Arc<dyn Authorizer>,
    audit: Arc<dyn Audit>,
    pool: AnyPool,
}

impl VaultService {
    pub fn new(
        vault: Arc<Vault>,
        authority: Arc<dyn Authority>,
        authorizer: Arc<dyn Authorizer>,
        audit: Arc<dyn Audit>,
        pool: AnyPool,
    ) -> Self {
        Self {
            vault,
            authority,
            authorizer,
            audit,
            pool,
        }
    }

    pub fn into_service(self) -> InterceptedService<VaultServer<Self>, AuthInterceptor> {
        let server = VaultServer::new(self)
            .max_decoding_message_size(MAX_REQUEST)
            .max_encoding_message_size(MAX_REQUEST);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn admin(&self, ext: &tonic::Extensions) -> Result<Caller, Status> {
        let caller = caller_of(&self.authorizer, ext).await?;
        if self.authority.is_site_admin(&caller).await {
            Ok(caller)
        } else {
            Err(Status::permission_denied(
                "only a site administrator may use the vault",
            ))
        }
    }
}

fn rotation_pb(r: &crate::vault::Rotation) -> wire::VaultRotation {
    wire::VaultRotation {
        id: r.id.clone(),
        to_generation: r.to_generation,
        total: r.total,
        done: r.done,
        finished: r.finished,
        error: r.error.clone(),
    }
}

#[tonic::async_trait]
impl VaultSvc for VaultService {
    async fn status(
        &self,
        request: Request<wire::VaultStatusRequest>,
    ) -> Result<Response<wire::VaultStatusResponse>, Status> {
        let (_, ext, _) = request.into_parts();
        self.admin(&ext).await?;
        let s = self.vault.status().await;
        Ok(Response::new(wire::VaultStatusResponse {
            ready: s.ready,
            generations: s
                .generations
                .iter()
                .map(|g| wire::VaultGeneration {
                    generation: g.generation,
                    created: g.created,
                    active: g.active,
                    secrets: g.secrets,
                })
                .collect(),
            key_file_mode: s.key_file_mode,
            rotation_active: s.rotation_active,
        }))
    }

    async fn rotate(
        &self,
        request: Request<wire::RotateVaultRequest>,
    ) -> Result<Response<wire::RotateVaultResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let caller = self.admin(&ext).await?;
        let op = op_id(&meta)?;
        if let Some(prev) = replay(&self.pool, op, &caller).await? {
            return Ok(Response::new(prev));
        }
        let rotation = self
            .vault
            .rotate(msg.retire_old)
            .await
            .map_err(|e| vault_status(&e))?;
        self.audit.record(
            AuditEvent::new("vault.rotated")
                .actor(caller.actor())
                .target(&rotation.id)
                .detail("generation", &rotation.to_generation.to_string())
                .detail("values", &rotation.total.to_string()),
        );
        let out = wire::RotateVaultResponse {
            generation: rotation.to_generation,
            rotation_id: rotation.id,
        };
        remember(&self.pool, op, &caller, &out).await;
        Ok(Response::new(out))
    }

    async fn get_rotation(
        &self,
        request: Request<wire::GetVaultRotationRequest>,
    ) -> Result<Response<wire::GetVaultRotationResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        self.admin(&ext).await?;
        if msg.rotation_id.len() > 64 {
            return Err(Status::invalid_argument("bad rotation id"));
        }
        let found = if msg.rotation_id.is_empty() {
            self.vault.latest_rotation().await
        } else {
            self.vault.rotation(&msg.rotation_id).await
        }
        .map_err(|e| vault_status(&e))?;
        let r = found.ok_or_else(|| Status::not_found("no such rotation"))?;
        Ok(Response::new(wire::GetVaultRotationResponse {
            rotation: Some(rotation_pb(&r)),
        }))
    }
}
