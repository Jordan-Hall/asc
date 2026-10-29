//! The `AiKeyShares` tonic service.
//!
//! The owner of a key is found from the key row, and the caller must hold the owner's role (a
//! user's own account, an organisation's admin); anything else is `NOT_FOUND`, as for a key that
//! does not exist. The grantee side asks the grantee's administrators.

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::ai_key_shares_server::{AiKeyShares, AiKeySharesServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::audit::Entry;
use super::caller::{once, op_id};
use super::owner::Owner;
use super::share::{Caps, Share, ShareUse, matches};
use super::store::{KeyRow, KeyState};
use super::{AiError, AiHub, clock};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.AiKeyShares` service.
pub struct AiKeySharesService {
    hub: Arc<AiHub>,
    gate: Gate,
}

fn wire_share(s: &Share) -> wire::AiKeyShare {
    wire::AiKeyShare {
        id: s.id.clone(),
        key_id: s.key_id.clone(),
        grantee: Some(s.grantee.to_wire()),
        allow_use: s.allow_use,
        see_spend: s.see_spend,
        created: s.created,
        created_by: s.created_by.clone(),
    }
}

/// The subject and role that administer a key of `owner`; keys of other owners are not shared.
fn admin_of(owner: &Owner) -> Option<(Subject, Role)> {
    match owner {
        Owner::User(n) => Some((Subject::User(n.clone()), Role::Write)),
        Owner::Org(n) => Some((Subject::Org(n.clone()), Role::Admin)),
        Owner::Repo(_) | Owner::Instance => None,
    }
}

/// A grantee as the wire names it: a user, an organisation or a repository.
fn grantee_of(o: Option<&wire::OwnerRef>) -> Result<Owner, Status> {
    match Owner::from_wire(o)? {
        Owner::Instance => Err(Status::invalid_argument(
            "a key is shared with a user, an organisation or a repository",
        )),
        g => Ok(g),
    }
}

impl AiKeySharesService {
    pub fn new(hub: Arc<AiHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<AiKeySharesServer<Self>, AuthInterceptor> {
        let server = AiKeySharesServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The key and the actor, provided the caller administers its owner; else `NOT_FOUND`.
    async fn owner_of_key(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
        key_id: &str,
    ) -> Result<(KeyRow, Actor), Status> {
        let caller = self.gate.caller(ext).await?;
        let key = self.hub.get_key(key_id).await.map_err(|e| e.status())?;
        let (subject, role) = admin_of(&key.owner).ok_or_else(|| AiError::NotFound.status())?;
        let actor = self
            .gate
            .allow(&caller, meta, &subject, role)
            .ok_or_else(|| AiError::NotFound.status())?;
        Ok((key, actor))
    }

    async fn audit(&self, action: &str, actor: &Actor, key: &KeyRow, details: &[(&str, String)]) {
        let owner = match &key.owner {
            Owner::User(n) | Owner::Org(n) => n.clone(),
            _ => String::new(),
        };
        let details = details.to_vec();
        self.hub
            .audit(Entry {
                owner: &owner,
                target: &key.id,
                on_behalf: actor.relay.as_deref(),
                details: &details,
                ..Entry::new(action, &actor.id)
            })
            .await;
    }
}

fn range(since: i64, until: i64) -> Result<(String, String), Status> {
    let now = clock::now();
    let since = if since == 0 {
        clock::month_start(now)
    } else {
        since
    };
    let until = if until == 0 { now } else { until };
    if since < 0 || until < since || (until - since) / 86_400 > 400 {
        return Err(Status::invalid_argument(
            "since and until must be a range of at most 400 days",
        ));
    }
    Ok((clock::day(since), clock::day(until)))
}

fn use_row(share: Option<&Share>, u: &ShareUse) -> wire::AiKeyShareUsageRow {
    wire::AiKeyShareUsageRow {
        share_id: share.map(|s| s.id.clone()).unwrap_or_default(),
        grantee: share.map(|s| s.grantee.to_wire()),
        requests: u.requests,
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cost_micro_usd: u.cost,
    }
}

#[tonic::async_trait]
impl AiKeyShares for AiKeySharesService {
    async fn share_key(
        &self,
        request: Request<wire::ShareAiKeyRequest>,
    ) -> Result<Response<wire::ShareAiKeyResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (key, actor) = self.owner_of_key(&ext, &meta, &msg.key_id).await?;
        let op = op_id(&meta)?;
        let grantee = grantee_of(msg.grantee.as_ref())?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "AiKeyShares/ShareKey",
            true,
            || async {
                let s = hub
                    .share(&key, grantee, msg.allow_use, msg.see_spend, &actor.id)
                    .await
                    .map_err(|e| e.status())?;
                self.audit(
                    "ai.key.shared",
                    &actor,
                    &key,
                    &[
                        ("grantee", s.grantee.label()),
                        ("allow_use", s.allow_use.to_string()),
                        ("see_spend", s.see_spend.to_string()),
                    ],
                )
                .await;
                Ok(wire::ShareAiKeyResponse {
                    share: Some(wire_share(&s)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn list_shares(
        &self,
        request: Request<wire::ListAiKeySharesRequest>,
    ) -> Result<Response<wire::ListAiKeySharesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (key, _) = self.owner_of_key(&ext, &meta, &msg.key_id).await?;
        let shares = self.hub.shares(&key.id).await.map_err(|e| e.status())?;
        Ok(Response::new(wire::ListAiKeySharesResponse {
            shares: shares.iter().map(wire_share).collect(),
        }))
    }

    async fn revoke_share(
        &self,
        request: Request<wire::RevokeAiKeyShareRequest>,
    ) -> Result<Response<wire::RevokeAiKeyShareResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (key, actor) = self.owner_of_key(&ext, &meta, &msg.key_id).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "AiKeyShares/RevokeShare",
            true,
            || async {
                let s = hub
                    .revoke_share(&key.id, &msg.share_id)
                    .await
                    .map_err(|e| e.status())?;
                self.audit(
                    "ai.key.unshared",
                    &actor,
                    &key,
                    &[("grantee", s.grantee.label())],
                )
                .await;
                Ok(wire::RevokeAiKeyShareResponse {})
            },
        )
        .await
        .map(Response::new)
    }

    async fn share_usage(
        &self,
        request: Request<wire::AiKeyShareUsageRequest>,
    ) -> Result<Response<wire::AiKeyShareUsageResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (key, _) = self.owner_of_key(&ext, &meta, &msg.key_id).await?;
        let (first, last) = range(msg.since, msg.until)?;
        let (shares, own) = self
            .hub
            .share_usage(&key.id, &first, &last)
            .await
            .map_err(|e| e.status())?;
        let mut rows: Vec<_> = shares.iter().map(|(s, u)| use_row(Some(s), u)).collect();
        rows.push(use_row(None, &own));
        Ok(Response::new(wire::AiKeyShareUsageResponse { rows }))
    }

    async fn list_shared_with_me(
        &self,
        request: Request<wire::ListSharedAiKeysRequest>,
    ) -> Result<Response<wire::ListSharedAiKeysResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let grantee = grantee_of(msg.grantee.as_ref())?;
        let caller = self.gate.caller(&ext).await?;
        let (subject, role) = match &grantee {
            Owner::User(n) => (Subject::user(n)?, Role::Read),
            Owner::Org(n) => (Subject::org(n)?, Role::Admin),
            Owner::Repo(n) => (Subject::repo(n)?, Role::Admin),
            Owner::Instance => return Err(Gate::denied()),
        };
        self.gate.require(&caller, &meta, &subject, role)?;
        let now = clock::now();
        let (first, last) = (clock::day(clock::month_start(now)), clock::day(now));
        let mut keys = Vec::new();
        for (share, key) in self
            .hub
            .shared_keys_for(&grantee)
            .await
            .map_err(|e| e.status())?
        {
            let spent = if share.see_spend {
                let (shares, _) = self
                    .hub
                    .share_usage(&key.id, &first, &last)
                    .await
                    .map_err(|e| e.status())?;
                shares
                    .iter()
                    .find(|(s, _)| s.id == share.id)
                    .map_or(0, |(_, u)| u.cost)
            } else {
                0
            };
            let usable = share.allow_use
                && !key.disabled
                && matches!(key.state, KeyState::Active)
                && (key.monthly_cap == 0 || self.hub.month_spent(&key.id).await < key.monthly_cap);
            // Nothing of the key but a label, its provider and whether it works.
            keys.push(wire::SharedAiKey {
                share_id: share.id.clone(),
                key_id: key.id.clone(),
                label: key.label.clone(),
                provider: key.provider.to_wire(),
                owner: Some(key.owner.to_wire()),
                usable,
                allow_use: share.allow_use,
                see_spend: share.see_spend,
                month_spent_micro_usd: spent,
            });
        }
        let _ = matches;
        Ok(Response::new(wire::ListSharedAiKeysResponse { keys }))
    }

    async fn get_key_caps(
        &self,
        request: Request<wire::GetAiKeyCapsRequest>,
    ) -> Result<Response<wire::GetAiKeyCapsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (key, _) = self.owner_of_key(&ext, &meta, &msg.key_id).await?;
        Ok(Response::new(wire::GetAiKeyCapsResponse {
            caps: Some(self.caps_of(&key).await?),
        }))
    }

    async fn set_key_caps(
        &self,
        request: Request<wire::SetAiKeyCapsRequest>,
    ) -> Result<Response<wire::SetAiKeyCapsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (key, actor) = self.owner_of_key(&ext, &meta, &msg.key_id).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "AiKeyShares/SetKeyCaps",
            true,
            || async {
                hub.set_caps(
                    &key,
                    Caps {
                        warn_percent: msg.warn_percent,
                        stop_at_cap: msg.stop_at_cap,
                    },
                )
                .await
                .map_err(|e| e.status())?;
                self.audit(
                    "ai.key.caps.updated",
                    &actor,
                    &key,
                    &[
                        ("warn_percent", msg.warn_percent.to_string()),
                        ("stop_at_cap", msg.stop_at_cap.to_string()),
                    ],
                )
                .await;
                Ok(wire::SetAiKeyCapsResponse {
                    caps: Some(self.caps_of(&key).await?),
                })
            },
        )
        .await
        .map(Response::new)
    }
}

impl AiKeySharesService {
    async fn caps_of(&self, key: &KeyRow) -> Result<wire::AiKeyCaps, Status> {
        let (caps, warned) = self.hub.caps(&key.id).await.map_err(|e| e.status())?;
        Ok(wire::AiKeyCaps {
            warn_percent: caps.warn_percent,
            stop_at_cap: caps.stop_at_cap,
            monthly_cap_micro_usd: key.monthly_cap,
            month_spent_micro_usd: self.hub.month_spent(&key.id).await,
            warned: warned == clock::month(clock::now()),
        })
    }
}
