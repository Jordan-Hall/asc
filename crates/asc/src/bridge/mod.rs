//! `asc api --stdio` (SPEC §24.2, P01a): the transport of ASC's IDE plugins. The host spawns one
//! long-lived bridge per IDE process; it reads `BridgeFrame` calls from stdin and writes replies
//! to stdout (`u32` little-endian length, at most 16 MiB). Standard output carries frames and
//! nothing else, and the bridge never prompts: it refuses to start when stdin is a terminal.
//!
//! A call names a method (`asc.v1.Workspace/Status`) and a workspace root. `route` decides where
//! it goes: the daemon over IPC, the asc server of the root's remote, or the bridge itself
//! (`Hello`, `Events`). Cancelling a call drops the upstream call. Restart and resubscription are
//! the host's job.
//!
//! `Bridge.Run` (verb children) is in `run`. `Bridge.Token` and `StoreToken` are in `auth`.
//! Known gap (M09): per-root routing through `LocalBackend`, see `route`.

mod argv_schema;
mod auth;
mod frame;
mod route;
mod run;
pub(crate) mod shape;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use asc_git::credential::{HelperConfig, RawEntry, section_entries};
use asc_rpc::ipc::connect;
use asc_rpc::server::MAX_MESSAGE;
use asc_rpc::server::wire::{
    BridgeEvent, BridgeFrame, Call, Connectivity, ConnectivityChange, End, HelloRequest,
    HelloResponse, RootInfo, StoreTokenRequest, StoreTokenResponse, TokenRequest, TokenResponse,
    bridge_event, bridge_frame,
};
use bytes::Bytes;
use prost_reflect::prost::Message as _;
use tokio::sync::{OnceCell, mpsc};
use tokio::task::{AbortHandle, LocalSet};
use tonic::Status;
use tonic::transport::Channel;

use crate::cli::common::{CliError, Ctx};
use crate::daemon::lifecycle;
use crate::remote::server::{ServerRemote, cfg_get, open_for};
use route::{BridgeMethod, RawCodec, Target};

/// Bridge flags (SPEC §24.2). Known gap (P45): add `json-verbs.v1`; Known gap (P03): `reorder-todo.v1`;
/// Known gap (P46): `split-hunks.v1`. A flag is advertised only when its unit ships, because the
/// plugins gate on it.
const FLAGS: &[&str] = &["plugin.v1"];
/// Longest wait for a server when answering `Hello`.
const HELLO_SERVER_WAIT: Duration = Duration::from_secs(3);
/// How often `Events` looks at the daemon.
const EVENTS_POLL: Duration = Duration::from_secs(2);
/// Most live calls; one more is refused.
const MAX_LIVE: usize = 128;
/// Longest accepted root path.
const MAX_ROOT: usize = 4096;

type Out = mpsc::Sender<BridgeFrame>;

fn fatal(text: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {text}"))
}

/// Entry point of `asc api --stdio`.
pub async fn serve_stdio(ctx: &Ctx) -> Result<ExitCode, CliError> {
    use std::io::IsTerminal;
    // `terminal_prompt` caches the same probe: it must be asked before the first read is pending.
    let _ = crate::credential::terminal_prompt();
    if std::io::stdin().is_terminal() {
        return Err(fatal(
            "'asc api --stdio' speaks framed protobuf on standard input and is started by an editor, not by hand",
        ));
    }
    let bridge = Rc::new(Bridge {
        prog: ctx.prog,
        env: ctx.env.clone(),
        own: ctx.cwd.clone(),
        roots: RefCell::new(HashMap::new()),
        daemon: OnceCell::new(),
        live: RefCell::new(HashMap::new()),
    });
    let local = LocalSet::new();
    local
        .run_until(async move {
            let (tx, mut rx) = mpsc::channel::<BridgeFrame>(64);
            let writer = tokio::task::spawn_local(async move {
                let mut stdout = tokio::io::stdout();
                while let Some(f) = rx.recv().await {
                    if frame::write_frame(&mut stdout, &f).await.is_err() {
                        return;
                    }
                }
            });
            let mut stdin = tokio::io::stdin();
            let result = loop {
                match frame::read_frame(&mut stdin).await {
                    Ok(Some(f)) => bridge.clone().on_frame(f, &tx),
                    Ok(None) => break Ok(ExitCode::SUCCESS),
                    Err(e) => break Err(fatal(e)),
                }
            };
            for (_, h) in bridge.live.borrow_mut().drain() {
                h.abort();
            }
            drop(tx);
            let _ = writer.await;
            result
        })
        .await
}

struct RootState {
    ctx: Ctx,
    server: OnceCell<Arc<ServerRemote>>,
}

struct DaemonLink {
    cfg: lifecycle::Config,
    proc: lifecycle::SystemProcess,
    channel: Channel,
}

struct Bridge {
    prog: crate::dispatch::Program,
    env: crate::env::GitEnv,
    own: PathBuf,
    roots: RefCell<HashMap<PathBuf, Rc<RootState>>>,
    daemon: OnceCell<DaemonLink>,
    live: RefCell<HashMap<u32, AbortHandle>>,
}

async fn send_end(out: &Out, id: u32, status: &Status) {
    let message: String = status
        .message()
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
    let end = End {
        code: status.code() as i32,
        message,
        error_detail: Bytes::copy_from_slice(status.details()),
    };
    let _ = out
        .send(BridgeFrame {
            id,
            f: Some(bridge_frame::F::End(end)),
        })
        .await;
}

async fn send_reply(out: &Out, id: u32, bytes: Bytes) -> bool {
    out.send(BridgeFrame {
        id,
        f: Some(bridge_frame::F::Reply(bytes)),
    })
    .await
    .is_ok()
}

impl Bridge {
    fn on_frame(self: Rc<Self>, f: BridgeFrame, out: &Out) {
        let id = f.id;
        match f.f {
            Some(bridge_frame::F::Call(call)) => {
                let refusal = if self.live.borrow().contains_key(&id) {
                    Some(Status::already_exists("call id is in use"))
                } else if self.live.borrow().len() >= MAX_LIVE {
                    Some(Status::resource_exhausted("too many calls in flight"))
                } else {
                    None
                };
                let out = out.clone();
                if let Some(st) = refusal {
                    tokio::task::spawn_local(async move { send_end(&out, id, &st).await });
                    return;
                }
                let me = self.clone();
                let work = tokio::task::spawn_local({
                    let out = out.clone();
                    async move { me.handle(id, call, &out).await }
                });
                self.live.borrow_mut().insert(id, work.abort_handle());
                let me = self.clone();
                tokio::task::spawn_local(async move {
                    let st = match work.await {
                        Ok(Ok(())) => Status::ok(""),
                        Ok(Err(s)) => s,
                        // Cancelled by the host: no End is owed.
                        Err(e) if e.is_cancelled() => return,
                        Err(_) => Status::internal("internal error"),
                    };
                    me.live.borrow_mut().remove(&id);
                    send_end(&out, id, &st).await;
                });
            }
            Some(bridge_frame::F::Cancel(_)) => {
                if let Some(h) = self.live.borrow_mut().remove(&id) {
                    h.abort();
                }
            }
            // Replies and ends flow from the bridge to the host only.
            _ => {}
        }
    }

    async fn handle(self: Rc<Self>, id: u32, call: Call, out: &Out) -> Result<(), Status> {
        match route::resolve(&call.method)? {
            Target::Bridge(m) => self.bridge_method(m, id, call, out).await,
            Target::Daemon(desc) => {
                let path = route::path_of(&desc);
                let streaming = desc.is_server_streaming();
                self.forward_daemon(id, &path, call.request, streaming, out)
                    .await
            }
            Target::Server(desc) => {
                let root = self.root(&call.root).await?;
                let path = route::path_of(&desc);
                let streaming = desc.is_server_streaming();
                self.forward_server(&root, id, &path, call.request, streaming, out)
                    .await
            }
        }
    }

    async fn bridge_method(
        self: Rc<Self>,
        m: BridgeMethod,
        id: u32,
        call: Call,
        out: &Out,
    ) -> Result<(), Status> {
        match m {
            BridgeMethod::Hello => {
                let req = HelloRequest::decode(call.request.as_ref())
                    .map_err(|_| Status::invalid_argument("HelloRequest does not decode"))?;
                let resp = self.hello(req).await;
                send_reply(out, id, Bytes::from(resp.encode_to_vec())).await;
                Ok(())
            }
            BridgeMethod::Events => self.events(id, &call.root, out).await,
            BridgeMethod::Run => run::serve(self.own.clone(), id, call, out).await,
            BridgeMethod::Token => {
                let req = TokenRequest::decode(call.request.as_ref())
                    .map_err(|_| Status::invalid_argument("TokenRequest does not decode"))?;
                let (real, helpers, _) = self.credentials(&call.root).await?;
                let token = auth::token(&req.host, real, helpers).await?;
                let resp = TokenResponse { token };
                send_reply(out, id, Bytes::from(resp.encode_to_vec())).await;
                Ok(())
            }
            BridgeMethod::StoreToken => {
                let req = StoreTokenRequest::decode(call.request.as_ref())
                    .map_err(|_| Status::invalid_argument("StoreTokenRequest does not decode"))?;
                let (real, helpers, entries) = self.credentials(&call.root).await?;
                auth::store_token(&req.host, &req.token, real, helpers, &entries).await?;
                send_reply(
                    out,
                    id,
                    Bytes::from(StoreTokenResponse::default().encode_to_vec()),
                )
                .await;
                Ok(())
            }
        }
    }

    /// Real git, the trusted `credential.*` config and its entries for the credential calls.
    async fn credentials(
        &self,
        root: &str,
    ) -> Result<
        (
            Option<asc_git::realgit::RealGit>,
            HelperConfig,
            Vec<RawEntry>,
        ),
        Status,
    > {
        let state = self.root(root).await?;
        let file = state.ctx.config.file();
        Ok((
            state.ctx.real_git().ok().cloned(),
            HelperConfig::from_file(file),
            section_entries(file, "credential"),
        ))
    }

    // ------------------------------------------------------------------------------ roots

    /// The state of workspace root `root` (empty: the bridge's own directory), opened once.
    async fn root(&self, root: &str) -> Result<Rc<RootState>, Status> {
        let path = if root.is_empty() {
            self.own.clone()
        } else {
            if root.len() > MAX_ROOT || root.contains('\0') || !Path::new(root).is_absolute() {
                return Err(Status::invalid_argument("root is not an absolute path"));
            }
            PathBuf::from(root)
        };
        if let Some(r) = self.roots.borrow().get(&path) {
            return Ok(r.clone());
        }
        let ctx = Ctx::open(self.prog, self.env.clone(), path.clone())
            .await
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let state = Rc::new(RootState {
            ctx,
            server: OnceCell::new(),
        });
        self.roots.borrow_mut().insert(path, state.clone());
        Ok(state)
    }

    async fn server_of(root: &RootState) -> Result<Arc<ServerRemote>, Status> {
        if cfg_get(root.ctx.config.file(), "remote.origin.url").is_none() {
            return Err(Status::failed_precondition("the workspace has no remote"));
        }
        root.server
            .get_or_try_init(|| async {
                open_for(&root.ctx, None)
                    .await
                    .map_err(|e| Status::unavailable(e.to_string()))
            })
            .await
            .cloned()
    }

    // ------------------------------------------------------------------------------ daemon

    async fn daemon(&self) -> Result<&DaemonLink, Status> {
        self.daemon
            .get_or_try_init(|| async {
                if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
                    return Err(Status::failed_precondition(
                        "the daemon is turned off (ASC_NO_DAEMON)",
                    ));
                }
                let (cfg, proc) =
                    crate::cli::daemon::system().map_err(|e| Status::unavailable(e.to_string()))?;
                lifecycle::ensure_running(&cfg, &proc)
                    .await
                    .map_err(|e| Status::unavailable(e.to_string()))?;
                let channel = connect(&cfg.endpoint);
                Ok(DaemonLink { cfg, proc, channel })
            })
            .await
    }

    async fn forward_daemon(
        &self,
        id: u32,
        path: &str,
        request: Bytes,
        streaming: bool,
        out: &Out,
    ) -> Result<(), Status> {
        let link = self.daemon().await?;
        let mut attempt = 0;
        loop {
            let req = tonic::Request::new(request.clone());
            match upstream(&link.channel, path, req, streaming).await {
                Ok(up) => return relay(up, id, out).await,
                Err(s) if s.code() == tonic::Code::Unavailable && attempt == 0 => {
                    // The daemon went away since the last call: start it again once.
                    attempt += 1;
                    lifecycle::ensure_running(&link.cfg, &link.proc)
                        .await
                        .map_err(|e| Status::unavailable(e.to_string()))?;
                }
                Err(s) => return Err(s),
            }
        }
    }

    // ------------------------------------------------------------------------------ server

    async fn forward_server(
        &self,
        root: &RootState,
        id: u32,
        path: &str,
        request: Bytes,
        streaming: bool,
        out: &Out,
    ) -> Result<(), Status> {
        let server = Self::server_of(root).await?;
        let op = uuid::Uuid::now_v7();
        let mut attempt = 0;
        loop {
            let session = server.session();
            let req = session.wire.request(request.clone(), Some(op));
            match upstream(&session.wire.channel, path, req, streaming).await {
                Ok(up) => {
                    let r = relay(up, id, out).await;
                    if r.is_ok() {
                        crate::credential::report_result(&session.resolved, true);
                    }
                    return r;
                }
                Err(s) if s.code() == tonic::Code::Unauthenticated && attempt == 0 => {
                    attempt += 1;
                    server
                        .reauth(&session)
                        .map_err(|e| Status::unauthenticated(e.to_string()))?;
                }
                Err(s) => {
                    if s.code() == tonic::Code::Unauthenticated {
                        crate::credential::report_result(&session.resolved, false);
                    }
                    return Err(s);
                }
            }
        }
    }

    // ------------------------------------------------------------------------------ Hello

    async fn hello(&self, req: HelloRequest) -> HelloResponse {
        let mut features: Vec<String> = FLAGS.iter().map(|f| (*f).to_owned()).collect();
        let roots = if req.roots.is_empty() {
            vec![String::new()]
        } else {
            req.roots
        };
        let mut infos = Vec::new();
        let daemon = daemon_probe().await.0;
        for r in roots.into_iter().take(64) {
            let display = if r.is_empty() {
                self.own.to_string_lossy().into_owned()
            } else {
                r.clone()
            };
            let mut info = RootInfo {
                root: display,
                daemon: daemon as i32,
                ..RootInfo::default()
            };
            match self.root(&r).await {
                Err(s) => {
                    info.error = s
                        .message()
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(512)
                        .collect();
                }
                Ok(state) => self.describe(&state, &mut info, &mut features).await,
            }
            infos.push(info);
        }
        HelloResponse {
            asc_version: env!("CARGO_PKG_VERSION").to_owned(),
            features,
            roots: infos,
        }
    }

    async fn describe(&self, state: &RootState, info: &mut RootInfo, features: &mut Vec<String>) {
        if let Some(ws) = state.ctx.ws_opt() {
            info.ws_id = ws.workspace.ws.id.to_string();
        }
        // Known gap (B07): the repository's own `branchless` flag; unknown, so the default applies.
        info.branchless =
            crate::branchless::mode::Settings::resolve(&state.ctx.config, &state.ctx.env, &|k| {
                std::env::var(k).ok()
            })
            .is_ok_and(|s| s.mode(None) == crate::branchless::mode::Mode::Branchless);
        let Some(url) = cfg_get(state.ctx.config.file(), "remote.origin.url") else {
            info.remote_kind = "none".to_owned();
            info.server = Connectivity::NotConfigured as i32;
            info.remote = Connectivity::NotConfigured as i32;
            return;
        };
        info.remote_url = crate::cli::url_display::anonymize(&url);
        match tokio::time::timeout(HELLO_SERVER_WAIT, Self::server_of(state)).await {
            Ok(Ok(server)) => {
                info.remote_kind = "asc".to_owned();
                info.repo = server.target().repo.clone();
                info.server = Connectivity::Up as i32;
                info.remote = Connectivity::Up as i32;
                if let Ok(Ok(v)) =
                    tokio::time::timeout(HELLO_SERVER_WAIT, server.client().server_version()).await
                {
                    for f in v.features {
                        if !features.contains(&f) {
                            features.push(f);
                        }
                    }
                }
            }
            _ => {
                info.remote_kind = "unknown".to_owned();
                info.server = Connectivity::Down as i32;
                info.remote = Connectivity::Down as i32;
            }
        }
    }

    // ------------------------------------------------------------------------------ Events

    async fn events(&self, id: u32, root: &str, out: &Out) -> Result<(), Status> {
        let root_name = if root.is_empty() {
            self.own.to_string_lossy().into_owned()
        } else {
            root.to_owned()
        };
        let mut last: Option<Connectivity> = None;
        let mut last_pid: Option<u32> = None;
        loop {
            let (state, pid) = daemon_probe().await;
            if last != Some(state) {
                last = Some(state);
                let ev = BridgeEvent {
                    event: Some(bridge_event::Event::Connectivity(ConnectivityChange {
                        root: root_name.clone(),
                        daemon: state as i32,
                        server: Connectivity::Unspecified as i32,
                        remote: Connectivity::Unspecified as i32,
                    })),
                };
                if !send_reply(out, id, Bytes::from(ev.encode_to_vec())).await {
                    return Ok(());
                }
            }
            if let Some(p) = pid {
                if last_pid.is_some_and(|old| old != p) {
                    let ev = BridgeEvent {
                        event: Some(bridge_event::Event::DaemonRestarted(p)),
                    };
                    if !send_reply(out, id, Bytes::from(ev.encode_to_vec())).await {
                        return Ok(());
                    }
                }
                last_pid = Some(p);
            }
            tokio::time::sleep(EVENTS_POLL).await;
        }
    }
}

/// The daemon as the bridge sees it now, without starting it.
async fn daemon_probe() -> (Connectivity, Option<u32>) {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return (Connectivity::NotConfigured, None);
    }
    let Ok((cfg, _)) = crate::cli::daemon::system() else {
        return (Connectivity::Unspecified, None);
    };
    match lifecycle::probe(&cfg).await {
        lifecycle::Probe::Up(info) => (Connectivity::Up, Some(info.pid)),
        _ => (Connectivity::Down, None),
    }
}

// ---------------------------------------------------------------------------------- relay

enum Upstream {
    One(Bytes),
    Many(tonic::Streaming<Bytes>),
}

async fn upstream(
    channel: &Channel,
    path: &str,
    req: tonic::Request<Bytes>,
    streaming: bool,
) -> Result<Upstream, Status> {
    let path: tonic::codegen::http::uri::PathAndQuery = path
        .parse()
        .map_err(|_| Status::internal("the method does not form a path"))?;
    let mut grpc = tonic::client::Grpc::new(channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    grpc.ready()
        .await
        .map_err(|e| Status::unavailable(format!("cannot reach the upstream: {e}")))?;
    if streaming {
        Ok(Upstream::Many(
            grpc.server_streaming(req, path, RawCodec)
                .await?
                .into_inner(),
        ))
    } else {
        Ok(Upstream::One(
            grpc.unary(req, path, RawCodec).await?.into_inner(),
        ))
    }
}

async fn relay(up: Upstream, id: u32, out: &Out) -> Result<(), Status> {
    match up {
        Upstream::One(b) => {
            send_reply(out, id, b).await;
            Ok(())
        }
        Upstream::Many(mut s) => {
            while let Some(m) = s.message().await? {
                if !send_reply(out, id, m).await {
                    return Ok(());
                }
            }
            Ok(())
        }
    }
}
