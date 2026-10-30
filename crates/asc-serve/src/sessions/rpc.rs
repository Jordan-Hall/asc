//! The `Sessions` tonic service.

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::sessions_server::{Sessions, SessionsServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::{Kind, SessionRow, SessionsHub};
use crate::ai::audit::Entry;
use crate::ai::caller::{once, op_id};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.Sessions` service.
pub struct SessionsService {
    hub: Arc<SessionsHub>,
    gate: Gate,
}

fn fail(e: super::SessionError) -> Status {
    e.status()
}

fn wire_session(s: &SessionRow) -> wire::Session {
    wire::Session {
        id: s.id.clone(),
        kind: match s.kind {
            Kind::Token => wire::SessionKind::Token,
            Kind::Ssh => wire::SessionKind::SshKey,
            Kind::Agent => wire::SessionKind::AgentToken,
        } as i32,
        label: s.label.clone(),
        scope: s.scope.clone(),
        created: s.created,
        last_used: s.last_used,
        expires: s.expires,
    }
}

impl SessionsService {
    pub fn new(hub: Arc<SessionsHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<SessionsServer<Self>, AuthInterceptor> {
        let server = SessionsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The account the call is about and who acts: the person for their own account, or the
    /// site administrator for any account.
    async fn account(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
        login: &str,
        needed: Role,
    ) -> Result<(String, Actor), Status> {
        let caller = self.gate.caller(ext).await?;
        if login.is_empty() {
            let (subject, actor) = self.gate.person(&caller, meta, needed)?;
            let Subject::User(name) = subject else {
                return Err(Gate::denied());
            };
            return Ok((name, actor));
        }
        let subject = Subject::user(login)?;
        let Subject::User(name) = subject.clone() else {
            return Err(Gate::denied());
        };
        if let Some(actor) = self.gate.allow(&caller, meta, &subject, needed) {
            return Ok((name, actor));
        }
        // The site administrator, by the instance claim, for the Admin user page.
        let actor = self
            .gate
            .require(&caller, meta, &Subject::Instance, Role::Admin)?;
        Ok((name, actor))
    }

    async fn audit(&self, action: &str, actor: &Actor, login: &str, id: &str, kind: &str) {
        self.hub
            .audit
            .record(Entry {
                owner: login,
                target: id,
                on_behalf: actor.relay.as_deref(),
                details: &[("kind", kind.to_owned())],
                ..Entry::new(action, &actor.id)
            })
            .await;
    }
}

#[tonic::async_trait]
impl Sessions for SessionsService {
    async fn list_sessions(
        &self,
        request: Request<wire::ListSessionsRequest>,
    ) -> Result<Response<wire::ListSessionsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (login, _) = self.account(&ext, &meta, &msg.login, Role::Read).await?;
        let rows = self.hub.list(&login).await.map_err(fail)?;
        Ok(Response::new(wire::ListSessionsResponse {
            sessions: rows.iter().map(wire_session).collect(),
        }))
    }

    async fn revoke_session(
        &self,
        request: Request<wire::RevokeSessionRequest>,
    ) -> Result<Response<wire::RevokeSessionResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (login, actor) = self.account(&ext, &meta, &msg.login, Role::Write).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Sessions/RevokeSession",
            true,
            || async {
                let s = hub.revoke(&login, &msg.id).await.map_err(fail)?;
                self.audit("session.revoked", &actor, &login, &s.id, s.kind.prefix())
                    .await;
                Ok(wire::RevokeSessionResponse {})
            },
        )
        .await
        .map(Response::new)
    }

    async fn revoke_other_sessions(
        &self,
        request: Request<wire::RevokeOtherSessionsRequest>,
    ) -> Result<Response<wire::RevokeOtherSessionsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (login, actor) = self.account(&ext, &meta, &msg.login, Role::Write).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Sessions/RevokeOtherSessions",
            true,
            || async {
                let gone = hub
                    .revoke_others(&login, &msg.keep_id)
                    .await
                    .map_err(fail)?;
                for s in &gone {
                    self.audit("session.revoked", &actor, &login, &s.id, s.kind.prefix())
                        .await;
                }
                Ok(wire::RevokeOtherSessionsResponse {
                    revoked: u32::try_from(gone.len()).unwrap_or(u32::MAX),
                })
            },
        )
        .await
        .map(Response::new)
    }
}
