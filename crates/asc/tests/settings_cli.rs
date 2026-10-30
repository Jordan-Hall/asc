//! `asc backup`, `asc usage`, `asc sessions`, `asc data-export` and `asc ai` against a stand-in
//! server on loopback that serves `Backups`, `Usage`, `Sessions`, `DataExport` and `AiKeyShares`.
//! They show the request each command builds, the text it prints, the exit code it ends with, that
//! a mutating call carries an operation id, that a bad argument is refused before any call, that
//! the restore confirmation word is never typed for the user, that an archive is written to a new
//! file only, and that text from the server cannot reach the terminal as control sequences.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asc_rpc::server::wire;
use asc_rpc::services::ai_key_shares_server::{AiKeyShares, AiKeySharesServer};
use asc_rpc::services::backups_server::{Backups, BackupsServer};
use asc_rpc::services::data_export_server::{DataExport, DataExportServer};
use asc_rpc::services::sessions_server::{Sessions, SessionsServer};
use asc_rpc::services::usage_server::{Usage, UsageServer};
use support::{Sandbox, text};
use tonic::{Request, Response, Status};

const TOKEN: &str = "secret-token-value";

#[derive(Default)]
struct Seen {
    list_backups: Vec<wire::ListBackupsRequest>,
    run_backup: Vec<wire::RunBackupRequest>,
    verify_backup: Vec<wire::VerifyBackupRequest>,
    set_schedule: Vec<wire::SetBackupScheduleRequest>,
    preview: Vec<wire::PreviewRestoreRequest>,
    restore: Vec<wire::RestoreBackupRequest>,
    get_usage: Vec<wire::GetUsageRequest>,
    list_spend: Vec<wire::ListSpendRequest>,
    list_sessions: Vec<wire::ListSessionsRequest>,
    revoke_session: Vec<wire::RevokeSessionRequest>,
    revoke_others: Vec<wire::RevokeOtherSessionsRequest>,
    request_export: usize,
    list_exports: usize,
    download: Vec<wire::DownloadExportRequest>,
    delete_export: Vec<wire::DeleteExportRequest>,
    share: Vec<wire::ShareAiKeyRequest>,
    list_shares: Vec<wire::ListAiKeySharesRequest>,
    revoke_share: Vec<wire::RevokeAiKeyShareRequest>,
    get_caps: Vec<wire::GetAiKeyCapsRequest>,
    set_caps: Vec<wire::SetAiKeyCapsRequest>,
    op_ids: Vec<Option<String>>,
    auth: Vec<Option<String>>,
}

#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Seen>>,
    /// The restore confirmation word the server asks for.
    word: Option<String>,
    /// `Download` sends its chunks out of order.
    scrambled: bool,
    /// `Download` fails after its first chunk.
    breaks: bool,
}

impl Fake {
    fn note<T>(&self, r: &Request<T>) {
        let md = r.metadata();
        let mut s = self.seen.lock().unwrap();
        s.auth.push(
            md.get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
        s.op_ids.push(
            md.get("x-asc-op-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
    }
}

fn backup(id: &str, note: &str) -> wire::Backup {
    wire::Backup {
        id: id.into(),
        state: wire::BackupState::Succeeded as i32,
        trigger: wire::BackupTrigger::Manual as i32,
        target: "offsite".into(),
        started: 1_700_000_000,
        finished: 1_700_000_100,
        size_bytes: 3 * 1024 * 1024,
        verify: wire::BackupVerify::Verified as i32,
        note: note.into(),
        restorable: true,
        ..Default::default()
    }
}

fn schedule() -> wire::BackupSchedule {
    wire::BackupSchedule {
        enabled: true,
        interval_hours: 24,
        minute_of_day: 120,
        target: "offsite".into(),
        keep_days: 7,
        keep_weeks: 8,
        next_run: 1_700_086_400,
        ..Default::default()
    }
}

#[tonic::async_trait]
impl Backups for Fake {
    async fn list_backups(
        &self,
        r: Request<wire::ListBackupsRequest>,
    ) -> Result<Response<wire::ListBackupsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_backups.push(r.into_inner());
        Ok(Response::new(wire::ListBackupsResponse {
            backups: vec![
                backup("bk_aaaaaaaaaaaaaaaaaaaa", "Restore test passed"),
                backup("bk_bbbbbbbbbbbbbbbbbbbb", "Target \u{1b}[2Jgone"),
            ],
            schedule: Some(schedule()),
            status: Some(wire::BackupStatus {
                last_success: 1_700_000_100,
                overdue: true,
                restore_staged: true,
                ..Default::default()
            }),
        }))
    }

    async fn list_targets(
        &self,
        r: Request<wire::ListBackupTargetsRequest>,
    ) -> Result<Response<wire::ListBackupTargetsResponse>, Status> {
        self.note(&r);
        Ok(Response::new(wire::ListBackupTargetsResponse {
            targets: vec![
                wire::BackupTarget {
                    name: "offsite".into(),
                    kind: "s3".into(),
                    location: "bucket/prefix".into(),
                    encrypted: true,
                },
                wire::BackupTarget {
                    name: "local".into(),
                    kind: "local".into(),
                    location: "/var/backups".into(),
                    encrypted: false,
                },
            ],
        }))
    }

    async fn run_backup(
        &self,
        r: Request<wire::RunBackupRequest>,
    ) -> Result<Response<wire::RunBackupResponse>, Status> {
        self.note(&r);
        let target = r.get_ref().target.clone();
        self.seen.lock().unwrap().run_backup.push(r.into_inner());
        let mut b = backup("bk_cccccccccccccccccccc", "");
        b.state = wire::BackupState::Running as i32;
        b.target = target;
        Ok(Response::new(wire::RunBackupResponse { backup: Some(b) }))
    }

    async fn set_schedule(
        &self,
        r: Request<wire::SetBackupScheduleRequest>,
    ) -> Result<Response<wire::SetBackupScheduleResponse>, Status> {
        self.note(&r);
        let want = r.get_ref().schedule.clone().unwrap_or_default();
        self.seen.lock().unwrap().set_schedule.push(r.into_inner());
        Ok(Response::new(wire::SetBackupScheduleResponse {
            schedule: Some(want),
        }))
    }

    async fn verify_backup(
        &self,
        r: Request<wire::VerifyBackupRequest>,
    ) -> Result<Response<wire::VerifyBackupResponse>, Status> {
        self.note(&r);
        let id = r.get_ref().id.clone();
        self.seen.lock().unwrap().verify_backup.push(r.into_inner());
        let mut b = backup(&id, "");
        b.verify = wire::BackupVerify::Running as i32;
        Ok(Response::new(wire::VerifyBackupResponse {
            backup: Some(b),
        }))
    }

    async fn preview_restore(
        &self,
        r: Request<wire::PreviewRestoreRequest>,
    ) -> Result<Response<wire::PreviewRestoreResponse>, Status> {
        self.note(&r);
        let id = r.get_ref().id.clone();
        self.seen.lock().unwrap().preview.push(r.into_inner());
        Ok(Response::new(wire::PreviewRestoreResponse {
            backup: Some(backup(&id, "")),
            confirmation: self.word.clone().unwrap_or_else(|| "RESTORE".into()),
            loses_changes_since: 1_700_000_000,
            verified: true,
            warnings: vec!["the vault key is not in this backup \u{1b}]0;owned\u{7}".into()],
            ..Default::default()
        }))
    }

    async fn restore(
        &self,
        r: Request<wire::RestoreBackupRequest>,
    ) -> Result<Response<wire::RestoreBackupResponse>, Status> {
        self.note(&r);
        let id = r.get_ref().id.clone();
        self.seen.lock().unwrap().restore.push(r.into_inner());
        Ok(Response::new(wire::RestoreBackupResponse {
            backup_id: id,
            restart_required: true,
            command: "asc serve restore --from X".into(),
        }))
    }
}

fn meter(used: u64, limit: u64) -> Option<wire::UsageMeter> {
    Some(wire::UsageMeter {
        used,
        limit,
        over: limit > 0 && used > limit,
    })
}

#[tonic::async_trait]
impl Usage for Fake {
    async fn get_usage(
        &self,
        r: Request<wire::GetUsageRequest>,
    ) -> Result<Response<wire::GetUsageResponse>, Status> {
        self.note(&r);
        let period = r.get_ref().period.clone();
        self.seen.lock().unwrap().get_usage.push(r.into_inner());
        Ok(Response::new(wire::GetUsageResponse {
            period: if period.is_empty() {
                "2026-10".into()
            } else {
                period
            },
            resets_at: 1_793_000_000,
            storage: meter(2 * 1024 * 1024 * 1024, 5 * 1024 * 1024 * 1024),
            agent_minutes: meter(412, 600),
            ai_spend: meter(22_400_000, 20_000_000),
            bandwidth: meter(1024, 0),
            byo_storage: true,
            over_quota: true,
            repos: vec![wire::UsageRepo {
                repo: "acme/app\u{1b}[31m".into(),
                storage_bytes: 1024,
                agent_minutes: 7,
                ai_spend_micro_usd: 5_000_000,
                bandwidth_bytes: 0,
            }],
            ..Default::default()
        }))
    }

    async fn list_spend(
        &self,
        r: Request<wire::ListSpendRequest>,
    ) -> Result<Response<wire::ListSpendResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_spend.push(r.into_inner());
        Ok(Response::new(wire::ListSpendResponse {
            rows: vec![wire::SpendRow {
                group: "acme/app".into(),
                label: "ci key".into(),
                requests: 12,
                cost_micro_usd: 3_250_000,
                ..Default::default()
            }],
            total: None,
        }))
    }

    async fn get_quota(
        &self,
        _r: Request<wire::GetQuotaRequest>,
    ) -> Result<Response<wire::GetQuotaResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn set_quota(
        &self,
        _r: Request<wire::SetQuotaRequest>,
    ) -> Result<Response<wire::SetQuotaResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn report(
        &self,
        _r: Request<wire::ReportUsageRequest>,
    ) -> Result<Response<wire::ReportUsageResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }
}

#[tonic::async_trait]
impl Sessions for Fake {
    async fn list_sessions(
        &self,
        r: Request<wire::ListSessionsRequest>,
    ) -> Result<Response<wire::ListSessionsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_sessions.push(r.into_inner());
        Ok(Response::new(wire::ListSessionsResponse {
            sessions: vec![
                wire::Session {
                    id: "tok:abc123".into(),
                    kind: wire::SessionKind::Token as i32,
                    label: "laptop".into(),
                    scope: "acme/* write".into(),
                    created: 1_700_000_000,
                    last_used: 1_700_000_500,
                    expires: 0,
                },
                wire::Session {
                    id: "ssh:k_1".into(),
                    kind: wire::SessionKind::SshKey as i32,
                    label: "work \u{1b}[2Jkey".into(),
                    scope: String::new(),
                    created: 1_600_000_000,
                    last_used: 0,
                    expires: 0,
                },
            ],
        }))
    }

    async fn revoke_session(
        &self,
        r: Request<wire::RevokeSessionRequest>,
    ) -> Result<Response<wire::RevokeSessionResponse>, Status> {
        self.note(&r);
        self.seen
            .lock()
            .unwrap()
            .revoke_session
            .push(r.into_inner());
        Ok(Response::new(wire::RevokeSessionResponse {}))
    }

    async fn revoke_other_sessions(
        &self,
        r: Request<wire::RevokeOtherSessionsRequest>,
    ) -> Result<Response<wire::RevokeOtherSessionsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().revoke_others.push(r.into_inner());
        Ok(Response::new(wire::RevokeOtherSessionsResponse {
            revoked: 2,
        }))
    }
}

fn job(id: &str, state: wire::ExportState) -> wire::ExportJob {
    wire::ExportJob {
        id: id.into(),
        state: state as i32,
        requested: 1_700_000_000,
        expires: 1_700_086_400,
        size_bytes: 2048,
        error: if state == wire::ExportState::Failed {
            "disk \u{1b}[2Jfull".into()
        } else {
            String::new()
        },
        ..Default::default()
    }
}

type ChunkStream = futures::stream::Iter<std::vec::IntoIter<Result<wire::ExportChunk, Status>>>;

#[tonic::async_trait]
impl DataExport for Fake {
    type DownloadStream = ChunkStream;

    async fn request_export(
        &self,
        r: Request<wire::RequestExportRequest>,
    ) -> Result<Response<wire::RequestExportResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().request_export += 1;
        Ok(Response::new(wire::RequestExportResponse {
            job: Some(job("ex_new1", wire::ExportState::Pending)),
        }))
    }

    async fn list_exports(
        &self,
        r: Request<wire::ListExportsRequest>,
    ) -> Result<Response<wire::ListExportsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_exports += 1;
        Ok(Response::new(wire::ListExportsResponse {
            jobs: vec![
                job("ex_ready1", wire::ExportState::Ready),
                job("ex_failed1", wire::ExportState::Failed),
            ],
        }))
    }

    async fn get_export(
        &self,
        _r: Request<wire::GetExportRequest>,
    ) -> Result<Response<wire::GetExportResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn download(
        &self,
        r: Request<wire::DownloadExportRequest>,
    ) -> Result<Response<Self::DownloadStream>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().download.push(r.into_inner());
        let chunk = |offset: u64, data: &'static [u8], last: bool| {
            Ok(wire::ExportChunk {
                offset,
                data: bytes::Bytes::from_static(data),
                last,
            })
        };
        let items = if self.scrambled {
            vec![
                chunk(0, b"{\"a\":1}\n", false),
                chunk(99, b"{\"b\":2}\n", true),
            ]
        } else if self.breaks {
            vec![
                chunk(0, b"{\"a\":1}\n", false),
                Err(Status::data_loss("the archive is damaged")),
            ]
        } else {
            vec![
                chunk(0, b"{\"a\":1}\n", false),
                chunk(8, b"{\"b\":2}\n", true),
            ]
        };
        Ok(Response::new(futures::stream::iter(items)))
    }

    async fn delete_export(
        &self,
        r: Request<wire::DeleteExportRequest>,
    ) -> Result<Response<wire::DeleteExportResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().delete_export.push(r.into_inner());
        Ok(Response::new(wire::DeleteExportResponse {}))
    }
}

fn share(id: &str, name: &str) -> wire::AiKeyShare {
    wire::AiKeyShare {
        id: id.into(),
        key_id: "ak_abc".into(),
        grantee: Some(wire::OwnerRef {
            kind: wire::OwnerKind::User as i32,
            name: name.into(),
        }),
        allow_use: true,
        see_spend: false,
        created: 1_700_000_000,
        created_by: "alice".into(),
    }
}

#[tonic::async_trait]
impl AiKeyShares for Fake {
    async fn share_key(
        &self,
        r: Request<wire::ShareAiKeyRequest>,
    ) -> Result<Response<wire::ShareAiKeyResponse>, Status> {
        self.note(&r);
        let g = r.get_ref().grantee.clone().unwrap_or_default();
        self.seen.lock().unwrap().share.push(r.into_inner());
        Ok(Response::new(wire::ShareAiKeyResponse {
            share: Some(share("as_new1", &g.name)),
        }))
    }

    async fn list_shares(
        &self,
        r: Request<wire::ListAiKeySharesRequest>,
    ) -> Result<Response<wire::ListAiKeySharesResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().list_shares.push(r.into_inner());
        Ok(Response::new(wire::ListAiKeySharesResponse {
            shares: vec![share("as_one", "bob"), share("as_two", "carol\u{1b}[31m")],
        }))
    }

    async fn revoke_share(
        &self,
        r: Request<wire::RevokeAiKeyShareRequest>,
    ) -> Result<Response<wire::RevokeAiKeyShareResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().revoke_share.push(r.into_inner());
        Ok(Response::new(wire::RevokeAiKeyShareResponse {}))
    }

    async fn share_usage(
        &self,
        _r: Request<wire::AiKeyShareUsageRequest>,
    ) -> Result<Response<wire::AiKeyShareUsageResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn list_shared_with_me(
        &self,
        _r: Request<wire::ListSharedAiKeysRequest>,
    ) -> Result<Response<wire::ListSharedAiKeysResponse>, Status> {
        Err(Status::unimplemented("not served by this stand-in"))
    }

    async fn get_key_caps(
        &self,
        r: Request<wire::GetAiKeyCapsRequest>,
    ) -> Result<Response<wire::GetAiKeyCapsResponse>, Status> {
        self.note(&r);
        self.seen.lock().unwrap().get_caps.push(r.into_inner());
        Ok(Response::new(wire::GetAiKeyCapsResponse {
            caps: Some(wire::AiKeyCaps {
                warn_percent: 50,
                stop_at_cap: true,
                monthly_cap_micro_usd: 10_000_000,
                month_spent_micro_usd: 2_000_000,
                warned: false,
            }),
        }))
    }

    async fn set_key_caps(
        &self,
        r: Request<wire::SetAiKeyCapsRequest>,
    ) -> Result<Response<wire::SetAiKeyCapsResponse>, Status> {
        self.note(&r);
        let m = r.get_ref().clone();
        self.seen.lock().unwrap().set_caps.push(r.into_inner());
        Ok(Response::new(wire::SetAiKeyCapsResponse {
            caps: Some(wire::AiKeyCaps {
                warn_percent: m.warn_percent,
                stop_at_cap: m.stop_at_cap,
                monthly_cap_micro_usd: 10_000_000,
                ..Default::default()
            }),
        }))
    }
}

struct Server {
    addr: SocketAddr,
    fake: Fake,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(fake: Fake) -> Self {
        let svc = fake.clone();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let incoming = futures::stream::unfold(listener, |l| async move {
                    let r = l.accept().await.map(|(s, _)| s);
                    Some((r, l))
                });
                let _ = tonic::transport::Server::builder()
                    .add_service(BackupsServer::new(svc.clone()))
                    .add_service(UsageServer::new(svc.clone()))
                    .add_service(SessionsServer::new(svc.clone()))
                    .add_service(DataExportServer::new(svc.clone()))
                    .add_service(AiKeySharesServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self {
            addr: addr_rx.recv().unwrap(),
            fake,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("asc+http://{}/acme/app", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn setup(fake: Fake) -> (Server, Sandbox, std::path::PathBuf) {
    let server = Server::start(fake);
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    (server, sb, cwd)
}

fn run(sb: &Sandbox, cwd: &Path, server: &Server, args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.env("ASC_TOKEN", TOKEN).env("ASC_NO_DAEMON", "1");
    cmd.args(args).args(["--remote", &server.url()]);
    cmd.output().expect("spawn asc")
}

fn no_controls(s: &str) {
    assert!(
        !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
        "control characters reached the output: {s:?}"
    );
}

/// The token is not in what a command printed, and no control sequence is.
fn clean(out: &Output) {
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(!all.contains(TOKEN), "the token was printed: {all}");
    no_controls(&all);
}

fn ok(out: &Output) -> (String, String) {
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {se}");
    clean(out);
    (so, se)
}

fn refused(out: &Output) -> String {
    assert_ne!(out.status.code(), Some(0), "{}", text(&out.stdout));
    clean(out);
    text(&out.stderr)
}

fn untouched(server: &Server) {
    let seen = server.fake.seen.lock().unwrap();
    assert!(seen.auth.is_empty(), "no call reached the server");
}

// ------------------------------------------------------------------------------- backup

#[test]
fn backup_list_shows_the_state_the_runs_and_json() {
    let (server, sb, cwd) = setup(Fake::default());
    let out = run(&sb, &cwd, &server, &["backup", "list"]);
    let (so, _) = ok(&out);
    assert!(
        so.contains("(overdue)") && so.contains("a restore is staged"),
        "{so}"
    );
    assert!(
        so.contains("bk_aaaaaaaaaaaaaaaaaaaa") && so.contains("succeeded"),
        "{so}"
    );
    assert!(so.contains("verified") && so.contains("offsite"), "{so}");
    assert!(
        so.contains("Restore test passed") && so.contains("Target"),
        "{so}"
    );
    let out = run(&sb, &cwd, &server, &["backup", "list", "--json"]);
    let (so, _) = ok(&out);
    assert!(so.contains("bk_bbbbbbbbbbbbbbbbbbbb"), "{so}");
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.list_backups.len(), 2);
    assert!(seen.auth[0].as_deref().is_some_and(|a| a.contains(TOKEN)));
    assert!(seen.op_ids[0].is_none(), "a read carries no operation id");
}

#[test]
fn backup_targets_run_and_verify_build_their_requests() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(&sb, &cwd, &server, &["backup", "targets"]));
    assert!(so.contains("offsite  s3  bucket/prefix  encrypted"), "{so}");
    assert!(so.contains("local  local  /var/backups"), "{so}");

    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["backup", "run", "--target", "offsite"],
    ));
    assert!(
        so.contains("bk_cccccccccccccccccccc") && so.contains("running"),
        "{so}"
    );
    ok(&run(
        &sb,
        &cwd,
        &server,
        &["backup", "verify", "bk_aaaaaaaaaaaaaaaaaaaa"],
    ));
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.run_backup[0].target, "offsite");
    assert_eq!(seen.verify_backup[0].id, "bk_aaaaaaaaaaaaaaaaaaaa");
    // Both start work, so both carry an operation id (a retry cannot start two).
    let with_ids = seen.op_ids.iter().filter(|o| o.is_some()).count();
    assert_eq!(with_ids, 2);
}

#[test]
fn backup_schedule_shows_by_default_and_changes_only_what_was_asked() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(&sb, &cwd, &server, &["backup", "schedule"]));
    assert!(
        so.contains("on every 24 h at 02:00 UTC to offsite; keep 7 days then 8 weeks"),
        "{so}"
    );
    assert!(server.fake.seen.lock().unwrap().set_schedule.is_empty());

    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &[
            "backup",
            "schedule",
            "--every-hours",
            "12",
            "--at",
            "03:30",
            "--keep-days",
            "14",
        ],
    ));
    assert!(
        so.contains("every 12 h at 03:30 UTC") && so.contains("keep 14 days then 8 weeks"),
        "{so}"
    );
    {
        let seen = server.fake.seen.lock().unwrap();
        let s = seen.set_schedule[0].schedule.clone().unwrap();
        assert_eq!(
            (
                s.enabled,
                s.interval_hours,
                s.minute_of_day,
                s.keep_days,
                s.keep_weeks
            ),
            (true, 12, 210, 14, 8),
            "what was not named is what the server had"
        );
        assert_eq!(s.target, "offsite");
    }
    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["backup", "schedule", "--disable"],
    ));
    assert!(so.starts_with("off every"), "{so}");

    // Out-of-range and malformed values are refused here, not by the server.
    for bad in [
        &["backup", "schedule", "--every-hours", "200"][..],
        &["backup", "schedule", "--every-hours", "0"][..],
        &["backup", "schedule", "--at", "25:00"][..],
        &["backup", "schedule", "--at", "2:00"][..],
    ] {
        let calls = server.fake.seen.lock().unwrap().set_schedule.len();
        refused(&run(&sb, &cwd, &server, bad));
        assert_eq!(
            server.fake.seen.lock().unwrap().set_schedule.len(),
            calls,
            "{bad:?}"
        );
    }
}

#[test]
fn backup_restore_needs_the_word_typed_by_the_person_and_never_types_it() {
    let (server, sb, cwd) = setup(Fake::default());
    let id = "bk_aaaaaaaaaaaaaaaaaaaa";
    let se = refused(&run(&sb, &cwd, &server, &["backup", "restore", id]));
    assert!(se.contains("--confirm RESTORE"), "{se}");
    assert!(se.contains("everything since is lost"), "{se}");
    assert!(se.contains("the vault key is not in this backup"), "{se}");
    let se = refused(&run(
        &sb,
        &cwd,
        &server,
        &["backup", "restore", id, "--confirm", "yes"],
    ));
    assert!(se.contains("--confirm RESTORE"), "{se}");
    assert!(
        server.fake.seen.lock().unwrap().restore.is_empty(),
        "nothing was staged"
    );

    let out = run(
        &sb,
        &cwd,
        &server,
        &["backup", "restore", id, "--confirm", "RESTORE"],
    );
    let (_, se) = ok(&out);
    assert!(
        se.contains("staged") && se.contains("restart the server"),
        "{se}"
    );
    assert!(se.contains("asc serve restore --from X"), "{se}");
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(
        (
            seen.restore[0].id.as_str(),
            seen.restore[0].confirm.as_str()
        ),
        (id, "RESTORE")
    );
    assert!(seen.op_ids.last().unwrap().is_some());
}

#[test]
fn backup_restore_refuses_a_confirmation_word_the_server_could_use_to_inject() {
    let fake = Fake {
        word: Some("RESTORE\u{1b}[2J; rm".into()),
        ..Fake::default()
    };
    let (server, sb, cwd) = setup(fake);
    let se = refused(&run(
        &sb,
        &cwd,
        &server,
        &[
            "backup",
            "restore",
            "bk_aaaaaaaaaaaaaaaaaaaa",
            "--confirm",
            "RESTORE",
        ],
    ));
    assert!(se.contains("not a plain word"), "{se}");
    assert!(server.fake.seen.lock().unwrap().restore.is_empty());
}

// --------------------------------------------------------------------------------- usage

#[test]
fn usage_needs_an_owner_and_prints_the_meters() {
    let (server, sb, cwd) = setup(Fake::default());
    refused(&run(&sb, &cwd, &server, &["usage"]));
    refused(&run(&sb, &cwd, &server, &["usage", "--owner", "somebody"]));
    refused(&run(&sb, &cwd, &server, &["usage", "--owner", "user:"]));
    refused(&run(
        &sb,
        &cwd,
        &server,
        &["usage", "--owner", "user:alice", "--spend", "team"],
    ));
    untouched(&server);

    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["usage", "--owner", "user:alice", "--period", "2026-09"],
    ));
    assert!(so.starts_with("period 2026-09 (resets "), "{so}");
    assert!(so.contains("storage") && so.contains("of 5"), "{so}");
    assert!(
        so.contains("agent minutes 412 of 600") || so.contains("agent minutes   412 of 600"),
        "{so}"
    );
    assert!(so.contains("$22.40 of $20.00  OVER"), "{so}");
    assert!(so.contains("do not count against the quota"), "{so}");
    assert!(
        so.contains("acme/app") && so.contains("7 min") && so.contains("$5.00"),
        "{so}"
    );
    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["usage", "--owner", "org:acme", "--json"],
    ));
    assert!(so.contains("\"period\""), "{so}");
    let seen = server.fake.seen.lock().unwrap();
    let r = &seen.get_usage[0];
    assert_eq!(r.period, "2026-09");
    let o = r.owner.as_ref().unwrap();
    assert_eq!(
        (o.kind, o.name.as_str()),
        (wire::OwnerKind::User as i32, "alice")
    );
    assert_eq!(
        seen.get_usage[1].owner.as_ref().unwrap().kind,
        wire::OwnerKind::Org as i32
    );
    assert!(
        seen.op_ids.iter().all(Option::is_none),
        "reading is not an operation"
    );
}

#[test]
fn usage_spend_groups_the_ai_spend() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["usage", "--owner", "instance", "--spend", "repo"],
    ));
    assert!(so.contains("acme/app  ci key  $3.25  12 requests"), "{so}");
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.list_spend[0].group_by, wire::SpendGroup::Repo as i32);
    assert_eq!(
        seen.list_spend[0].owner.as_ref().unwrap().kind,
        wire::OwnerKind::Instance as i32
    );
}

// ------------------------------------------------------------------------------ sessions

#[test]
fn sessions_list_and_sign_out() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["sessions", "list", "--login", "bob"],
    ));
    assert!(
        so.contains("tok:abc123  laptop  acme/* write  created"),
        "{so}"
    );
    assert!(so.contains("ssh:k_1") && so.contains("work"), "{so}");

    let (_, se) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["sessions", "revoke", "tok:abc123"],
    ));
    assert!(se.contains("tok:abc123 revoked"), "{se}");
    let (_, se) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["sessions", "others", "--keep", "ssh:k_1"],
    ));
    assert!(se.contains("2 signed out"), "{se}");
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.list_sessions[0].login, "bob");
    assert_eq!(seen.revoke_session[0].id, "tok:abc123");
    assert_eq!(seen.revoke_others[0].keep_id, "ssh:k_1");
    // list is a read; revoke and others are operations.
    assert_eq!(seen.op_ids.iter().filter(|o| o.is_some()).count(), 2);
}

#[test]
fn sessions_ids_that_cannot_be_ids_never_leave_the_machine() {
    let (server, sb, cwd) = setup(Fake::default());
    for bad in ["abc", "tok:", "tok:../x", "usr:1", "tok:a b"] {
        refused(&run(&sb, &cwd, &server, &["sessions", "revoke", bad]));
    }
    refused(&run(
        &sb,
        &cwd,
        &server,
        &["sessions", "others", "--keep", "nonsense"],
    ));
    untouched(&server);
}

// ---------------------------------------------------------------------------- data export

#[test]
fn data_export_requests_lists_and_deletes() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(&sb, &cwd, &server, &["data-export", "request"]));
    assert!(so.contains("ex_new1") && so.contains("pending"), "{so}");
    let (so, _) = ok(&run(&sb, &cwd, &server, &["data-export", "list"]));
    assert!(
        so.contains("ex_ready1  ready") && so.contains("2.0 KB") || so.contains("ex_ready1  ready"),
        "{so}"
    );
    assert!(so.contains("ex_failed1") && so.contains("disk"), "{so}");
    let (_, se) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["data-export", "delete", "ex_ready1"],
    ));
    assert!(se.contains("ex_ready1 deleted"), "{se}");
    for bad in ["ready1", "ex_", "ex_a b", "ex_../x"] {
        refused(&run(&sb, &cwd, &server, &["data-export", "delete", bad]));
    }
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!((seen.request_export, seen.list_exports), (1, 1));
    assert_eq!(seen.delete_export.len(), 1, "bad ids stayed local");
    assert_eq!(seen.op_ids.iter().filter(|o| o.is_some()).count(), 2);
}

#[test]
fn data_export_download_writes_a_new_file_and_never_an_old_one() {
    let (server, sb, cwd) = setup(Fake::default());
    let file = cwd.join("mine.jsonl");
    let path = file.to_string_lossy().into_owned();
    let (_, se) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["data-export", "download", "ex_ready1", "-o", &path],
    ));
    assert!(se.contains("16 bytes written"), "{se}");
    assert_eq!(std::fs::read(&file).unwrap(), b"{\"a\":1}\n{\"b\":2}\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // The file exists now: a second download does not touch it, and asks the server for nothing.
    let calls = server.fake.seen.lock().unwrap().download.len();
    refused(&run(
        &sb,
        &cwd,
        &server,
        &["data-export", "download", "ex_ready1", "-o", &path],
    ));
    assert_eq!(std::fs::read(&file).unwrap(), b"{\"a\":1}\n{\"b\":2}\n");
    assert_eq!(server.fake.seen.lock().unwrap().download.len(), calls);
}

#[test]
fn a_damaged_or_reordered_download_leaves_no_file() {
    for fake in [
        Fake {
            scrambled: true,
            ..Fake::default()
        },
        Fake {
            breaks: true,
            ..Fake::default()
        },
    ] {
        let (server, sb, cwd) = setup(fake);
        let file = cwd.join("broken.jsonl");
        let path = file.to_string_lossy().into_owned();
        refused(&run(
            &sb,
            &cwd,
            &server,
            &["data-export", "download", "ex_ready1", "-o", &path],
        ));
        assert!(!file.exists(), "a partial archive was left behind");
    }
}

// --------------------------------------------------------------------------------------- ai

#[test]
fn ai_share_builds_the_grant_and_lists_and_withdraws_it() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(
        &sb,
        &cwd,
        &server,
        &[
            "ai",
            "share",
            "ak_abc",
            "--with",
            "user:bob",
            "--no-use",
            "--see-spend",
        ],
    ));
    assert!(so.contains("as_new1 shared with user:bob"), "{so}");
    ok(&run(
        &sb,
        &cwd,
        &server,
        &["ai", "share", "ak_abc", "--with", "repo:acme/app"],
    ));
    let (so, _) = ok(&run(&sb, &cwd, &server, &["ai", "shares", "ak_abc"]));
    assert!(
        so.contains("as_one  bob  use=true  see-spend=false  by alice"),
        "{so}"
    );
    assert!(so.contains("as_two") && so.contains("carol"), "{so}");
    let (_, se) = ok(&run(
        &sb,
        &cwd,
        &server,
        &["ai", "unshare", "ak_abc", "as_one"],
    ));
    assert!(se.contains("share as_one withdrawn"), "{se}");
    let seen = server.fake.seen.lock().unwrap();
    let a = &seen.share[0];
    assert_eq!(a.key_id, "ak_abc");
    assert_eq!((a.allow_use, a.see_spend), (false, true));
    assert_eq!(a.grantee.as_ref().unwrap().name, "bob");
    let b = &seen.share[1];
    assert_eq!((b.allow_use, b.see_spend), (true, false));
    assert_eq!(
        b.grantee.as_ref().unwrap().kind,
        wire::OwnerKind::Repo as i32
    );
    assert_eq!(seen.revoke_share[0].share_id, "as_one");
    assert_eq!(seen.list_shares[0].key_id, "ak_abc");
}

#[test]
fn ai_share_refuses_what_cannot_be_a_key_or_a_grantee_before_asking() {
    let (server, sb, cwd) = setup(Fake::default());
    for args in [
        &["ai", "share", "abc", "--with", "user:bob"][..],
        &["ai", "share", "ak_abc", "--with", "instance"][..],
        &["ai", "share", "ak_abc", "--with", "everyone"][..],
        &["ai", "share", "ak_a b", "--with", "user:bob"][..],
        &["ai", "share", "ak_", "--with", "user:bob"][..],
        &["ai", "shares", "../x"][..],
        &["ai", "caps", "ak_abc", "--warn", "101"][..],
    ] {
        refused(&run(&sb, &cwd, &server, args));
    }
    let seen = server.fake.seen.lock().unwrap();
    assert!(seen.share.is_empty() && seen.list_shares.is_empty() && seen.set_caps.is_empty());
}

#[test]
fn ai_caps_show_and_change_only_what_was_named() {
    let (server, sb, cwd) = setup(Fake::default());
    let (so, _) = ok(&run(&sb, &cwd, &server, &["ai", "caps", "ak_abc"]));
    assert!(so.contains("50") && so.contains("stop"), "{so}");
    assert!(server.fake.seen.lock().unwrap().set_caps.is_empty());

    ok(&run(
        &sb,
        &cwd,
        &server,
        &["ai", "caps", "ak_abc", "--warn", "80"],
    ));
    ok(&run(
        &sb,
        &cwd,
        &server,
        &["ai", "caps", "ak_abc", "--no-stop"],
    ));
    let seen = server.fake.seen.lock().unwrap();
    let (a, b) = (&seen.set_caps[0], &seen.set_caps[1]);
    assert_eq!(
        (a.warn_percent, a.stop_at_cap),
        (80, true),
        "stop stays as it was"
    );
    assert_eq!(
        (b.warn_percent, b.stop_at_cap),
        (50, false),
        "warn stays as it was"
    );
}
