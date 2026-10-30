//! The `Backups` tonic service. Site administrators only (see [`crate::settings::access`]).

use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::backups_server::{Backups, BackupsServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::store::{self, Run, Schedule};
use super::{BackupsError, BackupsHub, CONFIRM};
use crate::ai::audit::Entry;
use crate::ai::caller::{Caller, once, op_id};
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.Backups` service.
pub struct BackupsService {
    hub: Arc<BackupsHub>,
    gate: Gate,
}

fn wire_run(r: &Run, restorable: bool) -> wire::Backup {
    let state = match r.state {
        store::RUNNING => wire::BackupState::Running,
        store::SUCCEEDED => wire::BackupState::Succeeded,
        store::FAILED => wire::BackupState::Failed,
        _ => wire::BackupState::Unspecified,
    };
    let verify = match r.verify {
        store::V_RUNNING => wire::BackupVerify::Running,
        store::V_OK => wire::BackupVerify::Verified,
        store::V_FAILED => wire::BackupVerify::Failed,
        _ => wire::BackupVerify::NotVerified,
    };
    let trigger = match r.trig {
        store::TRIG_MANUAL => wire::BackupTrigger::Manual,
        store::TRIG_SCHEDULE => wire::BackupTrigger::Schedule,
        _ => wire::BackupTrigger::Unspecified,
    };
    let u = |n: i64| u64::try_from(n).unwrap_or(0);
    wire::Backup {
        id: r.id.clone(),
        state: state as i32,
        trigger: trigger as i32,
        target: r.target.clone(),
        started: r.started,
        finished: r.finished,
        size_bytes: u(r.size_bytes),
        objects: u(r.objects),
        encrypted: r.encrypted,
        verify: verify as i32,
        verified_at: r.verified_at,
        note: r.note.clone(),
        done_bytes: u(r.done_bytes),
        total_bytes: u(r.total_bytes),
        actor: r.actor.clone(),
        restorable,
    }
}

fn restorable(r: &Run) -> bool {
    r.state == store::SUCCEEDED && !r.purged
}

fn wire_schedule(s: &Schedule, next_run: i64) -> wire::BackupSchedule {
    wire::BackupSchedule {
        enabled: s.enabled,
        interval_hours: s.interval_hours,
        minute_of_day: s.minute_of_day,
        target: s.target.clone(),
        keep_days: s.keep_days,
        keep_weeks: s.keep_weeks,
        next_run,
        include_vault_key: s.include_vault_key,
        include_objects: s.include_objects,
    }
}

impl BackupsService {
    pub fn new(hub: Arc<BackupsHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<BackupsServer<Self>, AuthInterceptor> {
        let server = BackupsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// The site administrator behind a call, with the caller.
    async fn admin(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
    ) -> Result<(Caller, Actor), Status> {
        let caller = self.gate.caller(ext).await?;
        let actor = self
            .gate
            .require(&caller, meta, &Subject::Instance, Role::Admin)?;
        Ok((caller, actor))
    }

    async fn audit(&self, action: &str, actor: &Actor, target: &str, details: &[(&str, String)]) {
        let details: Vec<(&str, String)> = details.to_vec();
        self.hub
            .record(Entry {
                target,
                on_behalf: actor.relay.as_deref(),
                details: &details,
                ..Entry::new(action, &actor.id)
            })
            .await;
    }
}

fn fail(e: BackupsError) -> Status {
    e.status()
}

#[tonic::async_trait]
impl Backups for BackupsService {
    async fn list_backups(
        &self,
        request: Request<wire::ListBackupsRequest>,
    ) -> Result<Response<wire::ListBackupsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        self.admin(&ext, &meta).await?;
        let runs = self.hub.list(msg.limit).await.map_err(fail)?;
        let sched = self.hub.schedule().await.map_err(fail)?;
        let next = self.hub.next_run(&sched).await.map_err(fail)?;
        let (ok, bad) = self
            .hub
            .store
            .last_results()
            .await
            .map_err(|e| fail(e.into()))?;
        let pending = self.hub.pending().await.map_err(fail)?;
        let now = crate::ai::clock::now();
        let status = wire::BackupStatus {
            last_success: ok,
            last_failure: bad,
            overdue: super::plan::overdue(
                sched.enabled,
                now,
                ok,
                sched.updated,
                sched.interval_hours,
            ),
            running: runs.iter().any(|r| r.state == store::RUNNING),
            restore_staged: pending.is_some(),
            restore_backup_id: pending.map(|p| p.backup_id).unwrap_or_default(),
            configured: self.hub.cfg.usable(),
        };
        Ok(Response::new(wire::ListBackupsResponse {
            backups: runs.iter().map(|r| wire_run(r, restorable(r))).collect(),
            schedule: Some(wire_schedule(&sched, next)),
            status: Some(status),
        }))
    }

    async fn list_targets(
        &self,
        request: Request<wire::ListBackupTargetsRequest>,
    ) -> Result<Response<wire::ListBackupTargetsResponse>, Status> {
        let (meta, ext, _) = request.into_parts();
        self.admin(&ext, &meta).await?;
        Ok(Response::new(wire::ListBackupTargetsResponse {
            targets: self
                .hub
                .targets()
                .into_iter()
                .map(|(name, kind, location, encrypted)| wire::BackupTarget {
                    name,
                    kind: kind.to_owned(),
                    location,
                    encrypted,
                })
                .collect(),
        }))
    }

    async fn run_backup(
        &self,
        request: Request<wire::RunBackupRequest>,
    ) -> Result<Response<wire::RunBackupResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (caller, actor) = self.admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        let _ = caller;
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Backups/RunBackup",
            true,
            || async {
                let run = hub
                    .start_run(&msg.target, store::TRIG_MANUAL, &actor.id)
                    .await
                    .map_err(fail)?;
                self.audit(
                    "backup.started",
                    &actor,
                    &run.id,
                    &[("target", run.target.clone()), ("trigger", "manual".into())],
                )
                .await;
                Ok(wire::RunBackupResponse {
                    backup: Some(wire_run(&run, false)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn set_schedule(
        &self,
        request: Request<wire::SetBackupScheduleRequest>,
    ) -> Result<Response<wire::SetBackupScheduleResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (_, actor) = self.admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let w = msg
            .schedule
            .ok_or_else(|| Status::invalid_argument("missing schedule"))?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Backups/SetSchedule",
            true,
            || async {
                let s = hub
                    .set_schedule(Schedule {
                        enabled: w.enabled,
                        interval_hours: w.interval_hours,
                        minute_of_day: w.minute_of_day,
                        target: w.target,
                        keep_days: w.keep_days,
                        keep_weeks: w.keep_weeks,
                        include_vault_key: w.include_vault_key,
                        include_objects: w.include_objects,
                        updated: 0,
                    })
                    .await
                    .map_err(fail)?;
                let next = hub.next_run(&s).await.map_err(fail)?;
                self.audit(
                    "backup.schedule.updated",
                    &actor,
                    "schedule",
                    &[
                        ("enabled", s.enabled.to_string()),
                        ("target", s.target.clone()),
                        ("interval_hours", s.interval_hours.to_string()),
                        ("keep_days", s.keep_days.to_string()),
                        ("keep_weeks", s.keep_weeks.to_string()),
                    ],
                )
                .await;
                Ok(wire::SetBackupScheduleResponse {
                    schedule: Some(wire_schedule(&s, next)),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn verify_backup(
        &self,
        request: Request<wire::VerifyBackupRequest>,
    ) -> Result<Response<wire::VerifyBackupResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (_, actor) = self.admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Backups/VerifyBackup",
            true,
            || async {
                let run = hub.start_verify(&msg.id).await.map_err(fail)?;
                self.audit("backup.verify.started", &actor, &run.id, &[])
                    .await;
                Ok(wire::VerifyBackupResponse {
                    backup: Some(wire_run(&run, restorable(&run))),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn preview_restore(
        &self,
        request: Request<wire::PreviewRestoreRequest>,
    ) -> Result<Response<wire::PreviewRestoreResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        self.admin(&ext, &meta).await?;
        let (run, m) = self.hub.preview(&msg.id).await.map_err(fail)?;
        let mut warnings = Vec::new();
        if run.verify != store::V_OK {
            warnings
                .push("This backup has not been verified; verify it before restoring.".to_owned());
        }
        if m.vault_key != "sealed" {
            warnings.push(
                "The backup holds no vault key: stored secrets open only with the current key file.".to_owned(),
            );
        }
        if self.hub.pending().await.map_err(fail)?.is_some() {
            warnings.push("A restore is already staged.".to_owned());
        }
        Ok(Response::new(wire::PreviewRestoreResponse {
            backup: Some(wire_run(&run, true)),
            confirmation: CONFIRM.to_owned(),
            loses_changes_since: if m.created > 0 {
                m.created
            } else {
                run.finished
            },
            read_only_during: true,
            verified: run.verify == store::V_OK,
            database: m.db_kind,
            objects: m.objects,
            object_bytes: m.object_bytes,
            has_vault_key: m.vault_key == "sealed",
            warnings,
        }))
    }

    async fn restore(
        &self,
        request: Request<wire::RestoreBackupRequest>,
    ) -> Result<Response<wire::RestoreBackupResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let (_, actor) = self.admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Backups/Restore",
            true,
            || async {
                let run = hub
                    .restore(&msg.id, &msg.confirm, &actor.id)
                    .await
                    .map_err(fail)?;
                self.audit(
                    "backup.restore.staged",
                    &actor,
                    &run.id,
                    &[("target", run.target.clone())],
                )
                .await;
                Ok(wire::RestoreBackupResponse {
                    backup_id: run.id.clone(),
                    restart_required: true,
                    command:
                        "restart asc serve; the staged restore is applied before the database opens"
                            .to_owned(),
                })
            },
        )
        .await
        .map(Response::new)
    }
}
