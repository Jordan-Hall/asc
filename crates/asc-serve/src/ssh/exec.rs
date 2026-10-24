//! Git over SSH: `git-upload-pack` and `git-receive-pack` on the channel (SPEC §25.11.5; PLAN U87).
//!
//! [`GitSshExec`] is the [`PackExec`] the listener hands a validated pack command to. It runs the
//! same engine as smart HTTP: the same derived view, the same `ServerGit` children (in session
//! mode: no `--stateless-rpc`, full-duplex pipes), the same `pre-receive` hook, `import_git_push`
//! and `UpdateRefs`, and the same limits. SSH adds no way to change asc state.
//!
//! The order of a command:
//!
//! 1. admission (class `git` for a fetch), for the principal the key logged in as;
//! 2. `authorize_ssh_key` for `read`, once more for `write` on a push. An unknown, unreadable or
//!    missing repository all give `asc: repository not found`; a key that may read but not write
//!    gives `asc: this key has read-only access`. The listener has no key list and no ACL of its
//!    own: the one [`Authorizer`](asc_rpc::auth::Authorizer) decides;
//! 3. the registry lookup, the mirror gate, a push slot and the disk gate;
//! 4. the view, brought in step by the step HTTP uses ([`sync_view`]);
//! 5. the child. Client data goes to its stdin, its stdout to the channel, its stderr to the
//!    channel's extended data once it has ended (paths and control bytes removed), and its exit
//!    code is the exit status. The child's process tree is killed by its own watchdog at the
//!    deadline and at the idle limit.
//!
//! A refusal is a line on stderr and exit status 1, since SSH has no status line. The bytes of a
//! command, in and out, are charged to the principal's `objects` bucket as they move, and a
//! client that sends more than the limits allow is cut.

use std::io::{Read, Write};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use asc_git::compat::{GitView, KillReason, PackService, ServerChild, Service};
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, Decision, Principal, SshFingerprint};
use asc_rpc::server::{CallCtx, RepoName};
use tokio::sync::{mpsc, oneshot};
use tonic::Code;
use uuid::Uuid;

use super::policy::Verb;
use super::server::{PackExec, PackIo, PackOut, PackRequest};
use crate::git_http::{GitServices, ViewWait, served_protocol, sync_view, version_label};
use crate::git_push::{Busy, PushOutcome};
use crate::limits::{Admission, PackMeter};
use crate::registry::{RegistryError, RepoMode};

/// The transport name in audit records and metrics.
pub const TRANSPORT: &str = "ssh";

/// Bytes read from a child's stdout at once: one SSH packet.
const CHUNK: usize = 32 * 1024;
/// Chunks waiting between a child's stdout and the channel, and between the channel and its stdin.
const QUEUE: usize = 4;
/// Room a push has over `max_push_bytes` for the command list, push options and pkt-line
/// framing (the same as smart HTTP).
const PUSH_STREAM_SLACK: u64 = 8 * 1024 * 1024;
/// Longest stderr of a child that is passed to the client, after cleaning.
const MAX_CHILD_STDERR: usize = 4 * 1024;

/// `asc: repository not found`: a missing repository and one the key cannot read look the same.
pub const NOT_FOUND: &str = "asc: repository not found\n";
/// A key that may read but not write.
pub const READ_ONLY_KEY: &str = "asc: this key has read-only access\n";
/// A mirror-mode repository (SPEC §25.6).
pub const READ_ONLY_MIRROR: &str =
    "asc: this repository is a read-only mirror; push to its upstream repository\n";
/// Over any limit of §25.11.7.
pub const BUSY: &str = "asc: too many concurrent operations, retry later\n";
const PUSH_UNAVAILABLE: &str =
    "asc: pushing over the git protocol is not available on this server\n";
const DISK_LOW: &str = "asc: server disk is low\n";
const PREPARING: &str = "asc: the repository is being prepared for git; retry shortly\n";
const MIRROR_MISSING: &str = "asc: the mirror has not been fetched yet; retry shortly\n";
const AUTH_UNAVAILABLE: &str = "asc: access could not be checked; retry later\n";
const INTERNAL: &str = "asc: the git service failed\n";
const TOO_LARGE: &str = "asc: the push is larger than this server accepts\n";
const TOO_SLOW: &str = "asc: the git request took too long\n";

// ------------------------------------------------------------------------------ audit

/// The audit record of one command (SPEC §25.8 item 9 plus `transport` and the fingerprint).
/// It holds a fingerprint, never a key blob.
#[derive(Clone, Debug)]
pub struct GitRecord {
    pub transport: &'static str,
    pub peer: Option<IpAddr>,
    pub principal: String,
    pub fingerprint: SshFingerprint,
    pub repo: String,
    pub service: &'static str,
    pub version: &'static str,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Objects of a push (zero for a fetch).
    pub objects: u64,
    /// The refs a push updated, `name old new` for each.
    pub refs: String,
    pub op_id: Uuid,
    /// `ok`, `noop`, `rejected:<why>`, `limit:<which>` or `killed`.
    pub outcome: String,
    pub exit_status: u32,
    pub millis: u64,
}

/// Receives the records.
pub trait GitAuditSink: Send + Sync {
    fn record(&self, record: &GitRecord);
}

/// The production sink: `tracing` events with the target `asc_git_audit`, the target smart HTTP
/// writes to.
pub struct TracingGitAudit;

impl GitAuditSink for TracingGitAudit {
    fn record(&self, r: &GitRecord) {
        tracing::info!(
            target: "asc_git_audit",
            transport = r.transport,
            principal = %r.principal,
            fingerprint = %r.fingerprint,
            repo = %r.repo,
            service = r.service,
            version = r.version,
            peer = r.peer.map(|p| p.to_string()).unwrap_or_default(),
            bytes_in = r.bytes_in,
            bytes_out = r.bytes_out,
            objects = r.objects,
            refs = %r.refs,
            exit_status = r.exit_status,
            op_id = %r.op_id,
            outcome = %r.outcome,
            millis = r.millis,
            "git request"
        );
    }
}

// ------------------------------------------------------------------------------ refusals

/// Why a command did not start. The client sees `text`; the audit record and the metrics see
/// `reason`.
struct Refuse {
    text: &'static str,
    reason: &'static str,
}

impl Refuse {
    const fn new(reason: &'static str, text: &'static str) -> Self {
        Self { text, reason }
    }

    fn not_found(reason: &'static str) -> Self {
        Self::new(reason, NOT_FOUND)
    }

    fn internal(reason: &'static str) -> Self {
        Self::new(reason, INTERNAL)
    }

    /// An admission or slot refusal: always the one retry text.
    fn busy(reason: &'static str) -> Self {
        Self::new(reason, BUSY)
    }
}

/// What the admission controller said, as a refusal.
fn admission_refusal(status: &tonic::Status) -> Refuse {
    match status.code() {
        Code::ResourceExhausted | Code::Unavailable => Refuse::busy("admission"),
        _ => Refuse::internal("admission"),
    }
}

// ------------------------------------------------------------------------------ the exec

/// The [`PackExec`] that serves git over SSH.
pub struct GitSshExec {
    svc: GitServices,
    admission: Arc<Admission>,
    sink: Arc<dyn GitAuditSink>,
}

impl GitSshExec {
    /// `svc` is what `git_http::register` returned; `admission` is the server's, shared with the
    /// HTTP surfaces.
    pub fn new(svc: GitServices, admission: Arc<Admission>) -> Self {
        Self {
            svc,
            admission,
            sink: Arc::new(TracingGitAudit),
        }
    }

    /// Replaces the audit sink (tests).
    #[must_use]
    pub fn with_audit(mut self, sink: Arc<dyn GitAuditSink>) -> Self {
        self.sink = sink;
        self
    }
}

/// What a command that got as far as a child leaves for the audit record.
struct Ran {
    exit_status: u32,
    outcome: String,
    bytes_in: u64,
    bytes_out: u64,
    objects: u64,
    refs: String,
    version: &'static str,
}

#[async_trait]
impl PackExec for GitSshExec {
    async fn run(&self, request: PackRequest, io: PackIo) -> u32 {
        let started = Instant::now();
        let service = match request.verb {
            Verb::UploadPack => Service::UploadPack,
            Verb::ReceivePack => Service::ReceivePack,
        };
        let mut record = GitRecord {
            transport: TRANSPORT,
            peer: request.peer,
            principal: request.principal.id.clone(),
            fingerprint: request.key.fingerprint(),
            repo: request.repo.clone(),
            service: service.name(),
            version: version_label(request.git_protocol.as_ref()),
            bytes_in: 0,
            bytes_out: 0,
            objects: 0,
            refs: String::new(),
            op_id: Uuid::new_v4(),
            outcome: String::new(),
            exit_status: 1,
            millis: 0,
        };
        let output = io.output.clone();
        let result = self.serve(request, io, service, &mut record).await;
        match result {
            Ok(ran) => {
                record.exit_status = ran.exit_status;
                record.outcome = ran.outcome;
                record.bytes_in = ran.bytes_in;
                record.bytes_out = ran.bytes_out;
                record.objects = ran.objects;
                record.refs = ran.refs;
                record.version = ran.version;
            }
            Err(refuse) => {
                let _ = output
                    .stderr(Bytes::from_static(refuse.text.as_bytes()))
                    .await;
                record.outcome = format!("rejected:{}", refuse.reason);
                record.exit_status = 1;
                self.svc.metrics.reject_via(TRANSPORT, refuse.reason);
            }
        }
        record.millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let metrics = &self.svc.metrics;
        metrics.request_via(
            TRANSPORT,
            record.service,
            record.version,
            if record.exit_status == 0 { 200 } else { 500 },
        );
        metrics.bytes_via(TRANSPORT, "in", record.bytes_in);
        metrics.bytes_via(TRANSPORT, "out", record.bytes_out);
        self.sink.record(&record);
        record.exit_status
    }
}

impl GitSshExec {
    /// One command, from admission to the child's end.
    async fn serve(
        &self,
        req: PackRequest,
        io: PackIo,
        service: Service,
        record: &mut GitRecord,
    ) -> Result<Ran, Refuse> {
        let push = service == Service::ReceivePack;

        // 1. Admission, before anything else is looked up.
        let (_admitted, meter) = self
            .admission
            .admit_pack(&req.principal.id, !push)
            .map_err(|s| admission_refusal(&s))?;

        // 2. The one authorizer decides read; a push asks for write as well.
        let principal = self
            .authorize(&req, Access::Read)
            .await
            .map_err(|r| match r {
                Denied::No => Refuse::not_found("no_read"),
                Denied::Lookup => Refuse::new("authz_unavailable", AUTH_UNAVAILABLE),
            })?;
        record.principal.clone_from(&principal.id);

        // 3. The repository, the mirror gate and the push gates.
        let name = RepoName::parse(&req.repo).map_err(|_| Refuse::not_found("bad_name"))?;
        let entry = match self.svc.registry.open(&name).await {
            Ok(e) => e,
            Err(RegistryError::NotFound(_)) => return Err(Refuse::not_found("no_repo")),
            Err(e) => {
                tracing::warn!(error = %e, "opening a repository for git over ssh");
                return Err(Refuse::internal("registry"));
            }
        };
        let mut principal = principal;
        let mut push_slot = None;
        if push {
            if entry.mode == RepoMode::Mirror {
                return Err(Refuse::new("read_only_mirror", READ_ONLY_MIRROR));
            }
            principal = self
                .authorize(&req, Access::Write)
                .await
                .map_err(|r| match r {
                    Denied::No => Refuse::new("no_write", READ_ONLY_KEY),
                    Denied::Lookup => Refuse::new("authz_unavailable", AUTH_UNAVAILABLE),
                })?;
            record.principal.clone_from(&principal.id);
            let Some(git_push) = self.svc.push.as_ref() else {
                return Err(Refuse::new("push_unavailable", PUSH_UNAVAILABLE));
            };
            let permit = git_push.acquire(&principal.id).map_err(|busy| match busy {
                Busy::Principal => Refuse::busy("push_slots_principal"),
                Busy::Replica => Refuse::busy("push_slots_replica"),
            })?;
            if !git_push.disk_ok() {
                return Err(Refuse::new("disk_low", DISK_LOW));
            }
            push_slot = Some(permit);
        }
        let protocol = served_protocol(service, entry.mode, req.git_protocol.clone());
        let version = version_label(protocol.as_ref());

        // 4. The view.
        let view = sync_view(&self.svc.views, &self.svc.registry, &entry)
            .await
            .map_err(|wait| match wait {
                ViewWait::Building(_) => Refuse::new("view_building", PREPARING),
                ViewWait::MirrorMissing => Refuse::new("mirror_missing", MIRROR_MISSING),
                ViewWait::TooManyViews => Refuse::busy("too_many_views"),
                ViewWait::Failed(reason) => Refuse::internal(reason),
            })?;

        // 5. The child.
        let session = match (&push_slot, self.svc.push.as_ref()) {
            (Some(_), Some(git_push)) => {
                let ctx = CallCtx {
                    repo: entry.name.clone(),
                    id_mode: IdMode::Asc,
                    principal: principal.clone(),
                    op_id: Some(record.op_id),
                    client: None,
                };
                let begun = git_push
                    .begin(ctx, entry.clone(), view.clone())
                    .map_err(|e| {
                        tracing::warn!(error = %e, "starting a push session");
                        Refuse::busy("push_sessions")
                    })?;
                Some(begun)
            }
            _ => None,
        };
        let env: Vec<(&'static str, String)> = session
            .as_ref()
            .map(|s| s.env().map(|(k, v)| (k, v.to_owned())).to_vec())
            .unwrap_or_default();
        let child = {
            let view = view.clone();
            let protocol = protocol.clone();
            tokio::task::spawn_blocking(move || {
                let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let pack = match service {
                    Service::UploadPack => PackService::UploadPack,
                    Service::ReceivePack => PackService::ReceivePack,
                };
                view.spawn_session_with_env(pack, protocol.as_ref(), &env)
            })
            .await
        };
        let child = match child {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::warn!(repo = %req.repo, error = %e, "starting a git pack child");
                return Err(Refuse::internal("spawn"));
            }
            Err(_) => return Err(Refuse::internal("spawn")),
        };
        if let Some(s) = &session {
            // The hook keeps the idle watchdog fed while it imports: git itself is silent.
            s.set_activity(child.activity());
        }

        let limits = &self.svc.limits;
        let cap = if push {
            limits.max_push_bytes.saturating_add(PUSH_STREAM_SLACK)
        } else {
            limits.fetch_body_bytes
        };
        let done = pump(child, io, &meter, cap, &view).await;

        // 6. What the hook decided is more precise than how the child ended.
        let mut ran = Ran {
            exit_status: done.exit_status,
            outcome: done.outcome,
            bytes_in: done.bytes_in,
            bytes_out: done.bytes_out,
            objects: 0,
            refs: String::new(),
            version,
        };
        self.svc.metrics.child_seconds(done.elapsed.as_secs_f64());
        if let Some(s) = &session {
            let outcome = s.outcome();
            ran.objects = outcome.objects;
            let hook_ran = outcome.result != "rejected:no_hook";
            if hook_ran && done.cut.is_none() && !done.killed {
                ran.outcome.clone_from(&outcome.result);
            } else if !hook_ran && done.exit_status == 0 && done.cut.is_none() {
                ran.outcome = "noop".to_owned();
            }
            if outcome.result == "ok" {
                ran.refs = refs_text(&outcome);
                // asc's refs are committed; bring the view in step (a user ref or a ref that git
                // set differently is removed or reset) so the next fetch finds it ready.
                if let Err(e) = self
                    .svc
                    .views
                    .ensure(&entry, self.svc.registry.refs())
                    .await
                {
                    tracing::warn!(repo = %entry.name, error = %e, "syncing the git view after a push");
                }
            }
        }
        if ran.outcome.starts_with("limit:") {
            self.svc.metrics.reject_via(TRANSPORT, "limit");
        }
        if done.killed {
            self.svc.metrics.reject_via(TRANSPORT, "killed");
        }
        Ok(ran)
    }

    /// The one authorizer's answer for this key and repository.
    async fn authorize(&self, req: &PackRequest, access: Access) -> Result<Principal, Denied> {
        match req
            .authorizer
            .authorize_ssh_key(&req.key, &req.repo, access)
            .await
        {
            Ok(Decision::Allowed(p)) => Ok(p),
            // A failed lookup is not a decision: the command is refused (fail closed).
            Err(_) => Err(Denied::Lookup),
            Ok(_) => Err(Denied::No),
        }
    }
}

/// Why `authorize_ssh_key` did not allow.
enum Denied {
    /// The key may not do this.
    No,
    /// The lookup failed.
    Lookup,
}

fn refs_text(outcome: &PushOutcome) -> String {
    outcome
        .refs
        .iter()
        .map(|r| {
            format!(
                "{} {} {}",
                r.name,
                r.old.map_or_else(|| "-".to_owned(), |o| o.to_string()),
                r.new.map_or_else(|| "-".to_owned(), |o| o.to_string()),
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

// ------------------------------------------------------------------------------ the child

/// Why the stream between the channel and the child was cut by asc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// The client sent more than the command may.
    TooLarge,
    /// The principal's `objects` budget is spent.
    Budget,
    /// The channel closed.
    Gone,
}

struct Pumped {
    exit_status: u32,
    outcome: String,
    bytes_in: u64,
    bytes_out: u64,
    cut: Option<Cut>,
    /// The child was killed by asc (deadline or idle limit).
    killed: bool,
    elapsed: Duration,
}

/// The child, kept so that a command dropped before its end (the client left, the deadline
/// passed) still ends well: the child's input ends, it fails on the short stream and removes its
/// own quarantine, and a waiter thread reaps it. Its watchdog still kills a child that does not
/// end, at the idle limit or the deadline.
struct Guard(Option<ServerChild>);

impl Guard {
    fn take(&mut self) -> Option<ServerChild> {
        self.0.take()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            let _ = std::thread::Builder::new()
                .name("asc-git-ssh-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

/// Copies the channel to the child's stdin and the child's stdout to the channel until the child's
/// output ends, then waits for it.
async fn pump(
    mut child: ServerChild,
    mut io: PackIo,
    meter: &PackMeter,
    cap: u64,
    view: &GitView,
) -> Pumped {
    let activity = child.activity();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let mut guard = Guard(Some(child));

    // The child's stdout, read on a thread of its own, to the channel.
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(QUEUE);
    let reader_activity = activity.clone();
    let _ = std::thread::Builder::new()
        .name("asc-git-ssh-out".into())
        .spawn(move || {
            let Some(mut stdout) = stdout else { return };
            let mut buf = vec![0u8; CHUNK];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        reader_activity.touch();
                        if out_tx
                            .blocking_send(Bytes::copy_from_slice(&buf[..n]))
                            .is_err()
                        {
                            // The command ended: dropping stdout makes the child's writes fail.
                            return;
                        }
                    }
                }
            }
        });

    // The child's stdin, written on a thread of its own, from the channel.
    let (in_tx, mut in_rx) = mpsc::channel::<Bytes>(QUEUE);
    let writer_activity = activity;
    let _ = std::thread::Builder::new()
        .name("asc-git-ssh-in".into())
        .spawn(move || {
            let Some(mut stdin) = stdin else { return };
            while let Some(part) = in_rx.blocking_recv() {
                if stdin.write_all(&part).is_err() {
                    // The child stopped reading: its exit status says why.
                    return;
                }
                writer_activity.touch();
            }
            // Every sender is gone: dropping stdin is the end of the client's input.
        });

    // The channel's output side runs by itself, so that a child writing and a client sending
    // never wait for each other.
    let (end_tx, mut end_rx) = oneshot::channel::<Option<Cut>>();
    let output: PackOut = io.output.clone();
    let out_meter = meter.clone();
    let sent = Arc::new(AtomicU64::new(0));
    let sent_count = sent.clone();
    let forward = tokio::spawn(async move {
        let mut cut = None;
        while let Some(chunk) = out_rx.recv().await {
            let n = chunk.len() as u64;
            if out_meter.charge(n).is_err() {
                cut = Some(Cut::Budget);
                break;
            }
            if !output.stdout(chunk).await {
                cut = Some(Cut::Gone);
                break;
            }
            sent_count.fetch_add(n, Ordering::Relaxed);
        }
        let _ = end_tx.send(cut);
    });
    let forward_guard = AbortOnDrop(forward);

    let mut bytes_in = 0u64;
    let cut: Option<Cut>;
    let mut input_open = true;
    let mut in_tx = Some(in_tx);
    loop {
        tokio::select! {
            biased;
            ended = &mut end_rx => {
                cut = ended.ok().flatten();
                break;
            }
            part = io.input.recv(), if input_open => match part {
                Some(chunk) => {
                    bytes_in = bytes_in.saturating_add(chunk.len() as u64);
                    if bytes_in > cap {
                        cut = Some(Cut::TooLarge);
                        break;
                    }
                    if meter.charge(chunk.len() as u64).is_err() {
                        cut = Some(Cut::Budget);
                        break;
                    }
                    let Some(tx) = in_tx.as_ref() else { continue };
                    // The child may stop reading (it refused the pack, say). Waiting for room is
                    // bounded by the child's end and by its idle limit.
                    tokio::select! {
                        biased;
                        ended = &mut end_rx => {
                            cut = ended.ok().flatten();
                            break;
                        }
                        sent = tx.send(chunk) => {
                            if sent.is_err() {
                                // The writer ended: the child stopped reading.
                                in_tx = None;
                            }
                        }
                    }
                }
                None => {
                    // The client's EOF: the child sees the end of its input.
                    input_open = false;
                    in_tx = None;
                }
            },
        }
    }
    drop(in_tx);
    drop(forward_guard);
    if cut.is_some() {
        // The stream was cut by asc: nothing more is wanted from the child.
        if let Some(child) = guard.0.as_ref() {
            child.kill();
        }
    }
    let bytes_out = sent.load(Ordering::Relaxed);

    let Some(child) = guard.take() else {
        return Pumped {
            exit_status: 1,
            outcome: "rejected:child_lost".to_owned(),
            bytes_in,
            bytes_out,
            cut,
            killed: false,
            elapsed: Duration::ZERO,
        };
    };
    let view_path = view.path().to_path_buf();
    let waited = tokio::task::spawn_blocking(move || child.wait()).await;
    let exit = match waited {
        Ok(Ok(exit)) => exit,
        _ => {
            return Pumped {
                exit_status: 1,
                outcome: "rejected:child_failed".to_owned(),
                bytes_in,
                bytes_out,
                cut,
                killed: false,
                elapsed: Duration::ZERO,
            };
        }
    };

    let ok = exit.status.success();
    let killed = matches!(exit.killed, Some(KillReason::Deadline | KillReason::Idle));
    let text = match (cut, exit.killed) {
        (Some(Cut::TooLarge), _) => Some(TOO_LARGE),
        (Some(Cut::Budget), _) => Some(BUSY),
        (Some(Cut::Gone), _) => None,
        (None, Some(KillReason::Deadline | KillReason::Idle)) => Some(TOO_SLOW),
        _ => None,
    };
    // The child's own stderr, cleaned, then asc's reason for ending it.
    let child_text = clean_stderr(&exit.stderr, &view_path);
    if !child_text.is_empty() {
        let _ = io.output.stderr(Bytes::from(child_text)).await;
    }
    if let Some(text) = text {
        let _ = io.output.stderr(Bytes::from_static(text.as_bytes())).await;
    }
    let outcome = match (cut, exit.killed, ok) {
        (Some(Cut::TooLarge), _, _) => "limit:push_bytes".to_owned(),
        (Some(Cut::Budget), _, _) => "limit:objects_budget".to_owned(),
        (Some(Cut::Gone), _, _) => "limit:client_gone".to_owned(),
        (None, Some(KillReason::Deadline | KillReason::Idle), _) => "killed".to_owned(),
        (None, Some(KillReason::Requested), _) => "limit:client_gone".to_owned(),
        (None, None, true) => "ok".to_owned(),
        (None, None, false) => "rejected:child_failed".to_owned(),
    };
    let exit_status = if cut.is_some() || killed {
        1
    } else {
        exit.status
            .code()
            .and_then(|c| u32::try_from(c).ok())
            .unwrap_or(1)
    };
    Pumped {
        exit_status,
        outcome,
        bytes_in,
        bytes_out,
        cut,
        killed,
        elapsed: exit.elapsed,
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A child's stderr for the client: valid text only, the view's path removed, control bytes other
/// than newline and tab replaced, cut to [`MAX_CHILD_STDERR`] bytes. Git's own messages (`fatal:
/// ...`) are kept; nothing that names a path on this server, and nothing that can drive the
/// client's terminal, is.
fn clean_stderr(raw: &[u8], view: &std::path::Path) -> String {
    let mut text = String::from_utf8_lossy(raw).into_owned();
    let path = view.to_string_lossy();
    for variant in [
        path.to_string(),
        path.replace('\\', "/"),
        path.replace('/', "\\"),
    ] {
        if !variant.is_empty() {
            text = text.replace(&variant, "<repository>");
        }
    }
    let mut out = String::with_capacity(text.len().min(MAX_CHILD_STDERR));
    for c in text.chars() {
        let c = if c == '\n' || c == '\t' || !c.is_control() {
            c
        } else {
            '?'
        };
        if out.len() + c.len_utf8() > MAX_CHILD_STDERR {
            break;
        }
        out.push(c);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_stderr_loses_paths_and_control_bytes() {
        let view = std::path::Path::new("/srv/asc/views/12.git");
        let raw = b"fatal: '/srv/asc/views/12.git' does not appear\x1b[2J\r\nsecond";
        let out = clean_stderr(raw, view);
        assert!(!out.contains("/srv/asc"), "{out}");
        assert!(!out.contains('\u{1b}') && !out.contains('\r'), "{out:?}");
        assert!(out.contains("<repository>") && out.ends_with("second\n"));
    }

    #[test]
    fn child_stderr_is_bounded_and_empty_stays_empty() {
        let view = std::path::Path::new("/v");
        assert_eq!(clean_stderr(b"", view), "");
        let out = clean_stderr(&vec![b'x'; 100_000], view);
        assert!(out.len() <= MAX_CHILD_STDERR + 1);
    }
}
