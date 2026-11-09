//! The `Maintenance` tonic service. Site administrators only (see [`crate::settings::access`]).

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::maintenance_server::{Maintenance, MaintenanceServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::{MaintenanceHub, Setting, Switch};
use crate::ai::caller::{once, op_id};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.Maintenance` service.
pub struct MaintenanceService {
    hub: Arc<MaintenanceHub>,
    gate: Gate,
}

fn wire_state(s: &Setting, restore_staged: bool) -> wire::MaintenanceState {
    wire::MaintenanceState {
        on: s.on,
        message: s.message.clone(),
        until: s.until,
        version: s.version.clone(),
        updated: s.updated,
        updated_by: s.updated_by.clone(),
        restore_staged,
    }
}

impl MaintenanceService {
    pub fn new(
        hub: Arc<MaintenanceHub>,
        authorizer: Arc<dyn Authorizer>,
        _info: ServerInfo,
    ) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<MaintenanceServer<Self>, AuthInterceptor> {
        let server = MaintenanceServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn admin(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
    ) -> Result<Actor, Status> {
        let caller = self.gate.caller(ext).await?;
        self.gate
            .require(&caller, meta, &Subject::Instance, Role::Admin)
    }

    async fn state(&self) -> Result<wire::MaintenanceState, Status> {
        let s = self.hub.setting().await.map_err(|e| e.status())?;
        Ok(wire_state(&s, self.hub.flag().restore_staged()))
    }
}

fn runner_wire(r: &crate::agents::runners::RunnerView) -> wire::AgentRunner {
    wire::AgentRunner {
        name: r.name.clone(),
        slots: r.slots,
        vcpu: r.vcpu,
        version: r.version.clone(),
        started: r.started,
        last_seen: r.last_seen,
        online: r.online,
        runs: r
            .runs
            .iter()
            .map(|a| wire::AgentRunnerRun {
                id: a.id,
                repo: a.repo.clone(),
                title: a.title.clone(),
                agent: a.agent.clone(),
                started: a.started,
            })
            .collect(),
    }
}

/// The clock in unix milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[tonic::async_trait]
impl Maintenance for MaintenanceService {
    async fn get_maintenance(
        &self,
        request: Request<wire::GetMaintenanceRequest>,
    ) -> Result<Response<wire::GetMaintenanceResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        self.admin(&ext, &meta).await?;
        Ok(Response::new(wire::GetMaintenanceResponse {
            state: Some(self.state().await?),
        }))
    }

    async fn check_health(
        &self,
        request: Request<wire::CheckHealthRequest>,
    ) -> Result<Response<wire::CheckHealthResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        self.admin(&ext, &meta).await?;
        let probes = self
            .hub
            .health()
            .ok_or_else(|| Status::unimplemented("the health probes are not installed"))?;
        let h = probes.check(now_ms()).await;
        Ok(Response::new(h.to_wire(now_ms())))
    }

    async fn list_agent_runners(
        &self,
        request: Request<wire::ListAgentRunnersRequest>,
    ) -> Result<Response<wire::ListAgentRunnersResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        self.admin(&ext, &meta).await?;
        let runners = self
            .hub
            .runners()
            .ok_or_else(|| Status::unimplemented("agents are not switched on"))?;
        let now = crate::ai::clock::now();
        let list = runners.list(now).await.map_err(|e| {
            tracing::error!(class = %crate::ai::store::class(&e), "list agent runners");
            Status::unavailable("the runner table cannot be read")
        })?;
        Ok(Response::new(wire::ListAgentRunnersResponse {
            runners: list.iter().map(runner_wire).collect(),
            now,
        }))
    }

    async fn set_maintenance(
        &self,
        request: Request<wire::SetMaintenanceRequest>,
    ) -> Result<Response<wire::SetMaintenanceResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let actor = self.admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        let switch = Switch {
            on: msg.on,
            message: msg.message.trim().to_owned(),
            until: msg.until,
            version: msg.version.trim().to_owned(),
        };
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Maintenance/SetMaintenance",
            true,
            || async {
                let s = hub
                    .switch(
                        &switch,
                        &actor.id,
                        actor.relay.as_deref(),
                        crate::ai::clock::now(),
                    )
                    .await
                    .map_err(|e| e.status())?;
                Ok(wire::SetMaintenanceResponse {
                    state: Some(wire_state(&s, hub.flag().restore_staged())),
                })
            },
        )
        .await
        .map(Response::new)
    }
}
