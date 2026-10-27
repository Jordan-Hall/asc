//! The `AiProviders` tonic service.
//!
//! Every call authenticates the credential, then asks the [`OwnerAdmin`] whether the caller
//! administers the owner of the key (named in the request, or found from the key id). A caller who
//! does not gets `NOT_FOUND`, the same answer as for a key that does not exist. The `api_key`
//! of a request is wrapped in `Zeroizing` at once and never reaches a log line or an error.

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::ai_providers_server::{AiProviders, AiProvidersServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use zeroize::Zeroizing;

use super::audit::Entry;
use super::caller::{Caller, authenticate, once, op_id};
use super::owner::Owner;
use super::store::{GroupBy, Provider};
use super::{AiError, AiHub, KeyPatch, NewKey, clock, wire_key};

/// Days a usage report may span.
const MAX_REPORT_DAYS: i64 = 400;

/// The `asc.v1.AiProviders` service.
pub struct AiService {
    hub: Arc<AiHub>,
    authorizer: Arc<dyn Authorizer>,
}

impl AiService {
    pub fn new(hub: Arc<AiHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self { hub, authorizer }
    }

    /// The tonic service with message limits and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<AiProvidersServer<Self>, AuthInterceptor> {
        let server = AiProvidersServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The caller, provided they administer `owner`; else `NOT_FOUND`.
    async fn admin_of(&self, ext: &tonic::Extensions, owner: &Owner) -> Result<Caller, Status> {
        let caller = authenticate(&self.authorizer, ext).await?;
        if self
            .hub
            .owners
            .can_admin(caller.credential.as_ref(), &caller.principal, owner)
            .await
        {
            Ok(caller)
        } else {
            Err(AiError::NotFound.status())
        }
    }

    /// The key `id` and the caller, provided they administer its owner; else `NOT_FOUND`.
    async fn key_admin(
        &self,
        ext: &tonic::Extensions,
        id: &str,
    ) -> Result<(Caller, super::store::KeyRow), Status> {
        let caller = authenticate(&self.authorizer, ext).await?;
        let key = self.hub.get_key(id).await.map_err(|e| e.status())?;
        if self
            .hub
            .owners
            .can_admin(caller.credential.as_ref(), &caller.principal, &key.owner)
            .await
        {
            Ok((caller, key))
        } else {
            Err(AiError::NotFound.status())
        }
    }
}

/// The owner scope of the audit log for a key owner: the user or organisation name.
fn audit_owner(o: &Owner) -> String {
    match o {
        Owner::User(n) | Owner::Org(n) => n.clone(),
        Owner::Repo(r) => r
            .split_once('/')
            .map(|(o, _)| o.to_owned())
            .unwrap_or_default(),
        Owner::Instance => String::new(),
    }
}

fn audit_repo(o: &Owner) -> &str {
    match o {
        Owner::Repo(r) => r,
        _ => "",
    }
}

/// Records a key event. Only the provider, the last four characters of the key and the owner
/// label are written; the key itself is never in memory here.
async fn audit_key(hub: &AiHub, action: &str, actor: &str, k: &super::store::KeyRow) {
    hub.audit(Entry {
        action,
        target: &k.id,
        owner: &audit_owner(&k.owner),
        repo: audit_repo(&k.owner),
        details: &[
            ("provider", k.provider.name().to_owned()),
            ("key_hint", k.key_hint.clone()),
            ("scope", k.owner.label()),
        ],
        ..Entry::new(action, actor)
    })
    .await;
}

fn secret(s: String) -> Zeroizing<String> {
    Zeroizing::new(s)
}

#[tonic::async_trait]
impl AiProviders for AiService {
    async fn create_key(
        &self,
        request: Request<wire::CreateAiKeyRequest>,
    ) -> Result<Response<wire::CreateAiKeyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let api_key = secret(msg.api_key);
        let owner = Owner::from_wire(msg.owner.as_ref())?;
        let caller = self.admin_of(&ext, &owner).await?;
        let op = op_id(&meta)?;
        let provider = Provider::from_wire(msg.provider)?;
        let new = NewKey {
            label: msg.label,
            provider,
            api_key,
            base_url: msg.base_url,
            default_model: msg.default_model,
            monthly_cap: msg.monthly_cap_micro_usd,
        };
        let hub = self.hub.clone();
        let actor = caller.principal.id.clone();
        once(
            &hub.ops,
            op,
            &caller.principal.id,
            "AiProviders/CreateKey",
            true,
            || async {
                let k = hub.create_key(&owner, new).await.map_err(|e| e.status())?;
                audit_key(&hub, "ai.key.created", &actor, &k).await;
                Ok(wire::CreateAiKeyResponse {
                    key: Some(wire_key(&hub, &k).await),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn list_keys(
        &self,
        request: Request<wire::ListAiKeysRequest>,
    ) -> Result<Response<wire::ListAiKeysResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let owner = Owner::from_wire(msg.owner.as_ref())?;
        self.admin_of(&ext, &owner).await?;
        let rows = self.hub.list_keys(&owner).await.map_err(|e| e.status())?;
        let mut keys = Vec::with_capacity(rows.len());
        for k in &rows {
            keys.push(wire_key(&self.hub, k).await);
        }
        Ok(Response::new(wire::ListAiKeysResponse { keys }))
    }

    async fn get_key(
        &self,
        request: Request<wire::GetAiKeyRequest>,
    ) -> Result<Response<wire::GetAiKeyResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let (_, key) = self.key_admin(&ext, &msg.id).await?;
        Ok(Response::new(wire::GetAiKeyResponse {
            key: Some(wire_key(&self.hub, &key).await),
        }))
    }

    async fn update_key(
        &self,
        request: Request<wire::UpdateAiKeyRequest>,
    ) -> Result<Response<wire::UpdateAiKeyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let api_key = msg.api_key.map(secret);
        let (caller, _) = self.key_admin(&ext, &msg.id).await?;
        let op = op_id(&meta)?;
        let patch = KeyPatch {
            label: msg.label,
            api_key,
            base_url: msg.base_url,
            default_model: msg.default_model,
            monthly_cap: msg.monthly_cap_micro_usd,
            disabled: msg.disabled,
        };
        let hub = self.hub.clone();
        let id = msg.id;
        let actor = caller.principal.id.clone();
        once(
            &hub.ops,
            op,
            &caller.principal.id,
            "AiProviders/UpdateKey",
            true,
            || async {
                let k = hub.update_key(&id, patch).await.map_err(|e| e.status())?;
                audit_key(&hub, "ai.key.updated", &actor, &k).await;
                Ok(wire::UpdateAiKeyResponse {
                    key: Some(wire_key(&hub, &k).await),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn delete_key(
        &self,
        request: Request<wire::DeleteAiKeyRequest>,
    ) -> Result<Response<wire::DeleteAiKeyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (caller, key) = self.key_admin(&ext, &msg.id).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        let actor = caller.principal.id.clone();
        once(
            &hub.ops,
            op,
            &caller.principal.id,
            "AiProviders/DeleteKey",
            true,
            || async {
                hub.delete_key(&msg.id).await.map_err(|e| e.status())?;
                audit_key(&hub, "ai.key.deleted", &actor, &key).await;
                Ok(wire::DeleteAiKeyResponse {})
            },
        )
        .await
        .map(Response::new)
    }

    async fn test_key(
        &self,
        request: Request<wire::TestAiKeyRequest>,
    ) -> Result<Response<wire::TestAiKeyResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let (caller, _) = self.key_admin(&ext, &msg.id).await?;
        let t = self.hub.test_key(&msg.id).await.map_err(|e| e.status())?;
        let result = if t.ok {
            "ok".to_owned()
        } else {
            t.provider_status.to_owned()
        };
        self.hub
            .audit(Entry {
                action: "ai.key.tested",
                target: &t.key.id,
                owner: &audit_owner(&t.key.owner),
                repo: audit_repo(&t.key.owner),
                details: &[
                    ("provider", t.key.provider.name().to_owned()),
                    ("result", result),
                ],
                ..Entry::new("ai.key.tested", &caller.principal.id)
            })
            .await;
        Ok(Response::new(wire::TestAiKeyResponse {
            ok: t.ok,
            latency_ms: t.latency_ms,
            provider_status: t.provider_status.to_owned(),
            message: t.message,
            key: Some(wire_key(&self.hub, &t.key).await),
        }))
    }

    async fn usage(
        &self,
        request: Request<wire::AiUsageRequest>,
    ) -> Result<Response<wire::AiUsageResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        let owner = Owner::from_wire(msg.owner.as_ref())?;
        self.admin_of(&ext, &owner).await?;
        if !msg.key_id.is_empty() {
            // The key must belong to the owner named; anything else is not found.
            let k = self
                .hub
                .get_key(&msg.key_id)
                .await
                .map_err(|e| e.status())?;
            if k.owner != owner {
                return Err(AiError::NotFound.status());
            }
        }
        let now = clock::now();
        let since = if msg.since == 0 {
            clock::month_start(now)
        } else {
            msg.since
        };
        let until = if msg.until == 0 { now } else { msg.until };
        if since < 0 || until < since || (until - since) / 86_400 > MAX_REPORT_DAYS {
            return Err(Status::invalid_argument(
                "since and until must be a range of at most 400 days",
            ));
        }
        let by = match wire::AiUsageGroup::try_from(msg.group_by) {
            Ok(wire::AiUsageGroup::Day) => GroupBy::Day,
            Ok(wire::AiUsageGroup::Model) => GroupBy::Model,
            Ok(wire::AiUsageGroup::Repo) => GroupBy::Repo,
            Ok(wire::AiUsageGroup::Agent) => GroupBy::Agent,
            Err(_) => return Err(Status::invalid_argument("unknown group_by")),
        };
        let key = (!msg.key_id.is_empty()).then_some(msg.key_id.as_str());
        let sums = self
            .hub
            .store
            .usage(&owner, key, &clock::day(since), &clock::day(until), by)
            .await
            .map_err(|e| AiError::from(e).status())?;
        let mut total = wire::AiUsageRow {
            group: "total".into(),
            ..Default::default()
        };
        let rows = sums
            .into_iter()
            .map(|s| {
                total.requests += s.requests;
                total.input_tokens += s.input_tokens;
                total.output_tokens += s.output_tokens;
                total.cost_micro_usd += s.cost_micro_usd;
                total.unpriced_requests += s.unpriced;
                wire::AiUsageRow {
                    group: s.group,
                    requests: s.requests,
                    input_tokens: s.input_tokens,
                    output_tokens: s.output_tokens,
                    cost_micro_usd: s.cost_micro_usd,
                    unpriced_requests: s.unpriced,
                }
            })
            .collect();
        Ok(Response::new(wire::AiUsageResponse {
            rows,
            total: Some(total),
        }))
    }

    async fn list_models(
        &self,
        request: Request<wire::ListAiModelsRequest>,
    ) -> Result<Response<wire::ListAiModelsResponse>, Status> {
        let (_, ext, msg) = request.into_parts();
        self.key_admin(&ext, &msg.id).await?;
        let models = self
            .hub
            .list_models(&msg.id)
            .await
            .map_err(|e| e.status())?;
        Ok(Response::new(wire::ListAiModelsResponse {
            models: models
                .into_iter()
                .map(|m| wire::AiModel {
                    id: m.id,
                    display_name: m.display_name,
                    context_tokens: m.context_tokens,
                })
                .collect(),
        }))
    }
}
