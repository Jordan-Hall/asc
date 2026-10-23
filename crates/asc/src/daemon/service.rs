//! The `Workspace` service of the daemon (SPEC §9.1, §10; U53b).
//!
//! The daemon hosts workspaces: [`Mounts`] holds one [`Mounted`] per mounted workspace, each
//! with its change journal and a [`Hosted`] backend. A plain workspace is hosted by
//! [`PlainHost`] (journal only; there is no placeholder to switch or dehydrate). A virtual one
//! is hosted by whatever the OS backend (ProjFS, FUSE) registered as the [`VirtualBackend`];
//! without one, mounting a virtual workspace fails with `FAILED_PRECONDITION`.
//!
//! **Authorisation.** The transport admits only the current user. On top of that no RPC acts on
//! a workspace the daemon was not asked to host, and `Mount` hosts only a workspace that is
//! registered in the user's `state.db`, whose mount root is a directory owned by the user that
//! is neither a link nor below one (never follow symlinked ancestors; CARRY, "Work tree
//! access"). Everything a client sends is bounded: ids, token text, path counts and lengths,
//! and the size of every answer.
//!
//! The plugin RPCs (`Status` … `Stage`) are served by [`super::plugin_api`] (U73).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use asc_core::path::RepoPath;
use asc_rpc::ipc::Workspace as WorkspaceRpc;
use asc_rpc::server::wire;
use asc_state::state::workspaces::{self, Mode, Workspace};
use asc_state::{AscHome, StateDb, WorkspaceId};
use asc_wc::journal::{Journal, MAX_SINCE, Token};
use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use super::codeintel_overlay::{CodeIntel, ServerAnswer};
use super::metrics::{Collected, EventLog, Metrics, Source};
use super::plugin_api::{self, PluginStream};
use super::plugin_api_read;
use super::plugin_api_stack;

/// Longest id or token text accepted from a client.
const MAX_TEXT: usize = 64;
/// Most paths in one pathspec.
const MAX_PATHSPECS: usize = 10_000;
/// Most workspaces mounted at once.
pub const MAX_MOUNTS: usize = 256;
/// Most hydration rows in a `Stats` answer, per list.
const MAX_STATS_ROWS: usize = 1_000;
/// Longest accepted tree id (`from_root`, `to_root`).
const MAX_ROOT: usize = 64;

// ---------------------------------------------------------------------------------- hosts

/// Why a hosted operation failed.
#[derive(Debug, PartialEq, Eq)]
pub enum HostError {
    /// The request does not apply to this workspace.
    Precondition(String),
    /// The request is malformed.
    Invalid(String),
    /// The provider failed.
    Failed(String),
}

impl From<HostError> for Status {
    fn from(e: HostError) -> Self {
        match e {
            HostError::Precondition(m) => Status::failed_precondition(m),
            HostError::Invalid(m) => Status::invalid_argument(m),
            HostError::Failed(m) => Status::internal(m),
        }
    }
}

/// Hydration counters of one hosted workspace.
#[derive(Clone, Debug, Default)]
pub struct HostStats {
    /// `(path, hydrations, bytes)`.
    pub by_path: Vec<(String, u64, u64)>,
    /// `(process image, hydrations, bytes)`.
    pub by_process: Vec<(String, u64, u64)>,
    pub bytes_hydrated: u64,
}

fn plain(what: &str) -> HostError {
    HostError::Precondition(format!("a plain workspace has no placeholders to {what}"))
}

/// One hosted workspace, as the OS backend sees it. Every method has the plain-workspace
/// behaviour, so a plain host is an empty impl.
#[async_trait]
pub trait Hosted: Send + Sync {
    fn stats(&self) -> HostStats {
        HostStats::default()
    }
    /// Paths a switch left pending (held open).
    fn pending(&self) -> u64 {
        0
    }
    /// Returns clean hydrated placeholders under `paths` (all when empty) to empty ones;
    /// answers `(files, bytes freed)`.
    async fn dehydrate(&self, _paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
        Err(plain("dehydrate"))
    }
    /// Moves the work tree from `from` to `to` (tree ids); answers the paths in conflict.
    async fn switch(&self, _from: &[u8], _to: &[u8], _mode: wire::SwitchMode) -> Result<Vec<Vec<u8>>, HostError> {
        Err(plain("switch"))
    }
    /// Fetches the content of `paths` into the machine cache; answers `(files, bytes)`.
    async fn prefetch(&self, _paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
        Err(plain("prefetch"))
    }
    /// Retries the paths a switch left pending.
    async fn retry_pending(&self) -> Result<(), HostError> {
        Ok(())
    }
    /// Compares disk with the journal and the base tree; answers the paths examined.
    async fn reconcile(&self) -> Result<u64, HostError> {
        Ok(0)
    }
    /// The idle dehydration pass (SPEC §10.2 item 9); answers `(files, bytes freed)`.
    async fn dehydrate_idle(&self) -> Result<(u64, u64), HostError> {
        Ok((0, 0))
    }
    /// Stops serving. Called once.
    async fn unmount(&self) -> Result<(), HostError> {
        Ok(())
    }
}

/// A plain workspace: the daemon keeps its journal and nothing else.
pub struct PlainHost;

#[async_trait]
impl Hosted for PlainHost {}

/// What a backend needs to host a virtual workspace.
pub struct HostContext {
    pub ws: Workspace,
    pub home: AscHome,
    pub db: StateDb,
    /// The workspace's journal; provider callbacks record into it.
    pub journal: Arc<Journal>,
}

/// The OS backend for virtual workspaces (ProjFS or FUSE), registered at daemon start.
#[async_trait]
pub trait VirtualBackend: Send + Sync {
    async fn host(&self, cx: HostContext) -> Result<Arc<dyn Hosted>, HostError>;
}

// ---------------------------------------------------------------------------------- mounts

/// A mounted workspace.
pub struct Mounted {
    pub id: WorkspaceId,
    pub row: Workspace,
    pub journal: Arc<Journal>,
    pub hosted: Arc<dyn Hosted>,
    /// Serialises the operations that change the work tree.
    pub gate: tokio::sync::Mutex<()>,
}

/// The mounted workspaces.
pub struct Mounts {
    map: Mutex<HashMap<WorkspaceId, Arc<Mounted>>>,
    /// Newly mounted workspaces whose post-clone prefetch has not run (SPEC §10.2 item 5a).
    fresh: Mutex<Vec<WorkspaceId>>,
    activity: Mutex<Instant>,
    stopping: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Default for Mounts {
    fn default() -> Self {
        Self {
            map: Mutex::default(),
            fresh: Mutex::default(),
            activity: Mutex::new(Instant::now()),
            stopping: AtomicBool::new(false),
        }
    }
}

impl Mounts {
    pub fn get(&self, id: WorkspaceId) -> Option<Arc<Mounted>> {
        lock(&self.map).get(&id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Mounted>> {
        let mut v: Vec<_> = lock(&self.map).values().cloned().collect();
        v.sort_by_key(|m| m.id.to_string());
        v
    }

    pub fn len(&self) -> usize {
        lock(&self.map).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Notes client activity (any RPC but `Ping`).
    pub fn touch(&self) {
        *lock(&self.activity) = Instant::now();
    }

    /// How long no client has asked for anything.
    pub fn idle_for(&self) -> Duration {
        lock(&self.activity).elapsed()
    }

    /// Takes the workspaces waiting for their post-clone prefetch.
    pub fn take_fresh(&self) -> Vec<WorkspaceId> {
        std::mem::take(&mut *lock(&self.fresh))
    }
}

/// Everything the service and the scheduler share.
pub struct State {
    pub db: StateDb,
    pub home: AscHome,
    pub mounts: Mounts,
    pub backend: Option<Arc<dyn VirtualBackend>>,
    pub metrics: Arc<Metrics>,
    pub events: Arc<EventLog>,
    mount_gate: tokio::sync::Mutex<()>,
}

pub(super) fn parse_id(text: &str) -> Result<WorkspaceId, Status> {
    if text.len() > MAX_TEXT {
        return Err(Status::invalid_argument("workspace id too long"));
    }
    text.parse().map_err(|_| Status::invalid_argument("malformed workspace id"))
}

pub(super) fn parse_paths(raw: &[bytes::Bytes]) -> Result<Vec<RepoPath>, Status> {
    if raw.len() > MAX_PATHSPECS {
        return Err(Status::invalid_argument(format!("more than {MAX_PATHSPECS} paths")));
    }
    raw.iter()
        .map(|p| RepoPath::new(p.to_vec()).map_err(|_| Status::invalid_argument("invalid path")))
        .collect()
}

fn store(e: impl std::fmt::Display) -> Status {
    Status::internal(format!("state: {e}"))
}

fn wall_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The mount root must be a real directory of this user, reached through no link.
pub(super) fn check_mount_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute() {
        return Err("the mount root is not an absolute path".into());
    }
    for dir in root.ancestors() {
        // A drive root or share has no metadata of its own that matters; links inside do.
        let meta = match std::fs::symlink_metadata(dir) {
            Ok(m) => m,
            Err(e) if dir.parent().is_none() => {
                let _ = e;
                continue;
            }
            Err(e) => return Err(format!("cannot read '{}': {e}", dir.display())),
        };
        if asc_wc::safe_path::is_link(&meta) {
            return Err(format!("'{}' is a link; a workspace is never reached through one", dir.display()));
        }
        if dir == root && !meta.is_dir() {
            return Err("the mount root is not a directory".into());
        }
    }
    match crate::cli::safe_dir::owned_by_current_user(root) {
        Ok(true) => Ok(()),
        Ok(false) => Err("the mount root is not owned by the current user".into()),
        Err(e) => Err(format!("cannot read the owner of the mount root: {e}")),
    }
}

impl State {
    pub fn new(
        db: StateDb,
        home: AscHome,
        backend: Option<Arc<dyn VirtualBackend>>,
        metrics: Arc<Metrics>,
        events: Arc<EventLog>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            home,
            mounts: Mounts::default(),
            backend,
            metrics,
            events,
            mount_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// The mounted workspace `id`; an unmounted one is never touched.
    pub fn mounted(&self, id: WorkspaceId) -> Result<Arc<Mounted>, Status> {
        self.mounts
            .get(id)
            .ok_or_else(|| Status::failed_precondition("the workspace is not mounted; mount it first"))
    }

    /// Hosts a registered workspace. Mounting a mounted workspace is a no-op.
    pub async fn mount(&self, id: WorkspaceId) -> Result<Arc<Mounted>, Status> {
        let _serial = self.mount_gate.lock().await;
        if self.mounts.stopping.load(Ordering::SeqCst) {
            return Err(Status::unavailable("the daemon is shutting down"));
        }
        if let Some(m) = self.mounts.get(id) {
            return Ok(m);
        }
        if self.mounts.len() >= MAX_MOUNTS {
            return Err(Status::resource_exhausted(format!("{MAX_MOUNTS} workspaces are mounted already")));
        }
        let row = workspaces::get(&self.db, id)
            .await
            .map_err(store)?
            .ok_or_else(|| Status::not_found("no such workspace is registered"))?;
        let root = std::path::PathBuf::from(&row.ws.mount);
        tokio::task::spawn_blocking(move || check_mount_root(&root))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(Status::permission_denied)?;

        let journal = Arc::new(
            Journal::open(self.db.clone(), id, u64::try_from(row.epoch).unwrap_or(0))
                .await
                .map_err(store)?,
        );
        let hosted: Arc<dyn Hosted> = match row.ws.mode {
            Mode::Plain => Arc::new(PlainHost),
            Mode::Virtual => {
                let backend = self.backend.as_ref().ok_or_else(|| {
                    Status::failed_precondition("no virtual file system backend is available on this machine")
                })?;
                backend
                    .host(HostContext {
                        ws: row.clone(),
                        home: self.home.clone(),
                        db: self.db.clone(),
                        journal: journal.clone(),
                    })
                    .await
                    .inspect_err(|_| self.metrics.provider_error("mount"))?
            }
        };
        Journal::spawn_flusher(&journal);
        let mounted = Arc::new(Mounted { id, row, journal, hosted, gate: tokio::sync::Mutex::new(()) });
        lock(&self.mounts.map).insert(id, mounted.clone());
        {
            let mut fresh = lock(&self.mounts.fresh);
            if fresh.len() < MAX_MOUNTS && !fresh.contains(&id) {
                fresh.push(id);
            }
        }
        self.events.emit("info", "workspace.mount", &[("ws", &id.to_string())]);
        Ok(mounted)
    }

    /// Stops hosting `id`. A workspace that is not mounted is not an error.
    pub async fn unmount(&self, id: WorkspaceId) -> Result<(), Status> {
        let _serial = self.mount_gate.lock().await;
        let Some(m) = lock(&self.mounts.map).remove(&id) else { return Ok(()) };
        lock(&self.mounts.fresh).retain(|f| *f != id);
        let _held = m.gate.lock().await;
        let stopped = m.hosted.unmount().await;
        let _ = m.journal.flush().await;
        self.events.emit("info", "workspace.unmount", &[("ws", &id.to_string())]);
        stopped.inspect_err(|_| self.metrics.provider_error("unmount")).map_err(Status::from)
    }

    /// Mounts every virtual workspace that `state.db` registers (a daemon restart re-attaches
    /// them; SPEC §9.1). A workspace that cannot be mounted is logged and skipped.
    pub async fn restore(&self) {
        let rows = match workspaces::list(&self.db, None).await {
            Ok(rows) => rows,
            Err(e) => {
                self.events.emit("error", "daemon.restore", &[("error", &e.to_string())]);
                return;
            }
        };
        for row in rows.into_iter().filter(|r| r.ws.mode == Mode::Virtual) {
            if let Err(s) = self.mount(row.ws.id).await {
                self.events.emit("warn", "workspace.restore", &[("ws", &row.ws.id.to_string()), ("error", s.message())]);
            }
        }
    }

    /// Unmounts the workspaces that are no longer registered (removed while mounted).
    pub async fn forget_unregistered(&self) -> Result<usize, String> {
        let mut gone = 0;
        for m in self.mounts.all() {
            if workspaces::get(&self.db, m.id).await.map_err(|e| e.to_string())?.is_none() {
                self.unmount(m.id).await.map_err(|s| s.message().to_owned())?;
                gone += 1;
            }
        }
        Ok(gone)
    }

    /// Unmounts everything and refuses new mounts (daemon shutdown).
    pub async fn shutdown(&self) {
        self.mounts.stopping.store(true, Ordering::SeqCst);
        for m in self.mounts.all() {
            let _ = self.unmount(m.id).await;
        }
    }

    /// The number of journal entries of `m`, at most [`MAX_SINCE`].
    pub async fn journal_size(&self, m: &Mounted) -> u64 {
        let Ok(now) = m.journal.token().await else { return 0 };
        match m.journal.since(Some(&Token { epoch: now.epoch, seq: 0 })).await {
            Ok(s) if s.fresh => MAX_SINCE as u64,
            Ok(s) => s.paths.len() as u64,
            Err(_) => 0,
        }
    }
}

#[async_trait]
impl Source for State {
    async fn collect(&self) -> Collected {
        let mut c = Collected::default();
        let mut by: HashMap<String, u64> = HashMap::new();
        for m in self.mounts.all() {
            let s = m.hosted.stats();
            c.fetch_bytes += s.bytes_hydrated;
            for (image, n, _) in s.by_process {
                *by.entry(image).or_default() += n;
            }
            c.pending_paths += m.hosted.pending();
            c.journal_entries += self.journal_size(&m).await;
        }
        c.hydrations = by.into_iter().collect();
        c
    }
}

// ---------------------------------------------------------------------------------- service

/// The `Workspace` service of the daemon. `Ping` and `Shutdown` need no state; the workspace
/// RPCs need the daemon's [`State`].
pub struct Service {
    version: String,
    started: Instant,
    stop: Arc<Notify>,
    state: Option<Arc<State>>,
    codeintel: CodeIntel,
    /// No asc server is configured: the local index is the only index, so the daemon starts the
    /// helper that builds it (SPEC §23.2.5).
    local_only: bool,
}

impl Service {
    pub(super) fn new(version: String, stop: Arc<Notify>, state: Option<Arc<State>>) -> Self {
        // Without an asc server the local index answers (SPEC §23.2.5).
        let mut codeintel = CodeIntel::new();
        codeintel.server =
            crate::codeintel_local::query::system_answer().map(|l| l as Arc<dyn ServerAnswer>);
        Self {
            version,
            started: Instant::now(),
            stop,
            state,
            codeintel,
            local_only: true,
        }
    }

    /// Lets the workspace-scope code intelligence RPCs ask an asc server for its part (SPEC §23.2.5).
    #[must_use]
    pub fn with_codeintel_server(mut self, server: Arc<dyn ServerAnswer>) -> Self {
        // The server answers; when it cannot be reached, the local index does (SPEC §23.2.5).
        self.codeintel.server = Some(match crate::codeintel_local::query::system_answer() {
            Some(local) => Arc::new(crate::codeintel_local::routed::Routed::new(server, local)),
            None => server,
        });
        self.local_only = false;
        self
    }

    /// Starts the local index helper for `ws_id` when it is due (SPEC §23.2.5): only without a
    /// asc server, only when the workspace is idle, and never inside the daemon.
    fn nudge_local_index(&self, state: &Arc<State>, ws_id: &str) {
        if !self.local_only {
            return;
        }
        if let Ok(id) = parse_id(ws_id)
            && let Ok(m) = state.mounted(id)
        {
            crate::codeintel_local::start::nudge(
                std::path::Path::new(&m.row.ws.mount),
                &state.home.gitdir(id),
                false,
            );
        }
    }

    fn state(&self) -> Result<&Arc<State>, Status> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("this daemon hosts no workspaces (no state database)"))?;
        state.mounts.touch();
        Ok(state)
    }

    /// Runs a provider operation on a mounted workspace, serialised with the other changes to
    /// its work tree, and counts a provider failure.
    async fn hosted<T>(
        &self,
        ws_id: &str,
        op: &'static str,
        run: impl AsyncFnOnce(&Mounted) -> Result<T, HostError>,
    ) -> Result<T, Status> {
        let state = self.state()?;
        let m = state.mounted(parse_id(ws_id)?)?;
        let _held = m.gate.lock().await;
        run(&m).await.map_err(|e| {
            if matches!(e, HostError::Failed(_)) {
                state.metrics.provider_error(op);
            }
            e.into()
        })
    }
}

fn token_text(t: &Token) -> String {
    t.to_string()
}

#[tonic::async_trait]
impl WorkspaceRpc for Service {
    async fn status(&self, req: Request<wire::WsStatusRequest>) -> Result<Response<wire::WsStatusResponse>, Status> {
        let state = self.state()?;
        plugin_api::status(state, req.into_inner()).await.map(Response::new)
    }

    type DiffStream = PluginStream<wire::DiffResponse>;
    async fn diff(&self, req: Request<wire::WsDiffRequest>) -> Result<Response<Self::DiffStream>, Status> {
        let state = self.state()?;
        plugin_api::diff(state, req.into_inner()).await.map(Response::new)
    }

    type LogStream = PluginStream<wire::LogResponse>;
    async fn log(&self, req: Request<wire::WsLogRequest>) -> Result<Response<Self::LogStream>, Status> {
        let state = self.state()?;
        plugin_api::log(state, req.into_inner()).await.map(Response::new)
    }

    type BlameStream = PluginStream<wire::BlameResponse>;
    async fn blame(&self, req: Request<wire::WsBlameRequest>) -> Result<Response<Self::BlameStream>, Status> {
        let state = self.state()?;
        plugin_api::blame(state, req.into_inner()).await.map(Response::new)
    }

    async fn affected(&self, req: Request<wire::WsAffectedRequest>) -> Result<Response<wire::WsAffectedResponse>, Status> {
        let state = self.state()?;
        plugin_api::affected(state, req.into_inner()).await.map(Response::new)
    }

    type WatchStream = PluginStream<wire::WatchEvent>;
    async fn watch(&self, req: Request<wire::WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        let state = self.state()?;
        plugin_api::watch(state, req.into_inner()).await.map(Response::new)
    }

    async fn stage(&self, req: Request<wire::WsStageRequest>) -> Result<Response<wire::WsStageResponse>, Status> {
        let state = self.state()?;
        plugin_api::stage(state, req.into_inner()).await.map(Response::new)
    }

    async fn stacks(&self, req: Request<wire::WsStacksRequest>) -> Result<Response<wire::WsStacksResponse>, Status> {
        let state = self.state()?;
        plugin_api_stack::stacks(state, req.into_inner()).await.map(Response::new)
    }

    async fn worktrees(
        &self,
        req: Request<wire::WsWorktreesRequest>,
    ) -> Result<Response<wire::WsWorktreesResponse>, Status> {
        let state = self.state()?;
        plugin_api_stack::worktrees(state, req.into_inner()).await.map(Response::new)
    }

    async fn conflicts(
        &self,
        req: Request<wire::WsConflictsRequest>,
    ) -> Result<Response<wire::WsConflictsResponse>, Status> {
        let state = self.state()?;
        plugin_api_stack::conflicts(state, req.into_inner()).await.map(Response::new)
    }

    type CommitGraphStream = PluginStream<wire::CommitGraphResponse>;
    async fn commit_graph(
        &self,
        req: Request<wire::WsCommitGraphRequest>,
    ) -> Result<Response<Self::CommitGraphStream>, Status> {
        let state = self.state()?;
        plugin_api_stack::commit_graph(state, req.into_inner()).await.map(Response::new)
    }

    type SearchStream = PluginStream<wire::SearchResponse>;
    async fn search(
        &self,
        req: Request<wire::WsSearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        let state = self.state()?;
        let req = req.into_inner();
        self.nudge_local_index(state, &req.ws_id);
        let s = self.codeintel.search(state, req).await?;
        Ok(Response::new(s))
    }

    async fn symbols(
        &self,
        req: Request<wire::WsSymbolsRequest>,
    ) -> Result<Response<wire::SymbolsResponse>, Status> {
        let state = self.state()?;
        let req = req.into_inner();
        self.nudge_local_index(state, &req.ws_id);
        let r = self.codeintel.symbols(state, req).await?;
        Ok(Response::new(r))
    }

    async fn semantic(
        &self,
        req: Request<wire::WsSemanticRequest>,
    ) -> Result<Response<wire::SemanticResponse>, Status> {
        let state = self.state()?;
        let r = self.codeintel.semantic(state, req.into_inner()).await?;
        Ok(Response::new(r))
    }

    async fn index_status(
        &self,
        req: Request<wire::WsIndexStatusRequest>,
    ) -> Result<Response<wire::IndexStatusResponse>, Status> {
        let state = self.state()?;
        let req = req.into_inner();
        self.nudge_local_index(state, &req.ws_id);
        let r = self.codeintel.index_status(state, req).await?;
        Ok(Response::new(r))
    }

    type ReadFileStream = PluginStream<wire::ReadFileResponse>;
    async fn read_file(
        &self,
        req: Request<wire::WsReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        let state = self.state()?;
        plugin_api_read::read_file(state, req.into_inner())
            .await
            .map(Response::new)
    }

    async fn ping(&self, _: Request<wire::PingRequest>) -> Result<Response<wire::PingResponse>, Status> {
        Ok(Response::new(wire::PingResponse {
            version: self.version.clone(),
            pid: std::process::id(),
            uptime_ms: wall_ms(self.started.elapsed()),
        }))
    }

    async fn mount(&self, req: Request<wire::MountRequest>) -> Result<Response<wire::MountResponse>, Status> {
        let state = self.state()?;
        state.mount(parse_id(&req.get_ref().ws_id)?).await?;
        Ok(Response::new(wire::MountResponse {}))
    }

    async fn unmount(&self, req: Request<wire::UnmountRequest>) -> Result<Response<wire::UnmountResponse>, Status> {
        // A daemon without state hosts nothing, so there is nothing to stop.
        let Some(state) = self.state.as_ref() else {
            parse_id(&req.get_ref().ws_id)?;
            return Ok(Response::new(wire::UnmountResponse {}));
        };
        state.mounts.touch();
        state.unmount(parse_id(&req.get_ref().ws_id)?).await?;
        Ok(Response::new(wire::UnmountResponse {}))
    }

    async fn switch(&self, req: Request<wire::SwitchRequest>) -> Result<Response<wire::SwitchResponse>, Status> {
        let r = req.into_inner();
        if r.from_root.len() > MAX_ROOT || r.to_root.len() > MAX_ROOT {
            return Err(Status::invalid_argument("tree id too long"));
        }
        let mode = wire::SwitchMode::try_from(r.mode).map_err(|_| Status::invalid_argument("unknown switch mode"))?;
        let conflicts = self.hosted(&r.ws_id, "switch", async |m| m.hosted.switch(&r.from_root, &r.to_root, mode).await).await?;
        Ok(Response::new(wire::SwitchResponse { conflicts: conflicts.into_iter().map(bytes::Bytes::from).collect() }))
    }

    async fn journal(&self, req: Request<wire::JournalRequest>) -> Result<Response<wire::JournalResponse>, Status> {
        let r = req.into_inner();
        let state = self.state()?;
        let m = state.mounted(parse_id(&r.ws_id)?)?;
        let since = match r.since_token.as_str() {
            "" => None,
            t => Some(t.parse::<Token>().map_err(|e| Status::invalid_argument(e.to_string()))?),
        };
        let answer = m.journal.since(since.as_ref()).await.map_err(store)?;
        let mut seen = HashSet::new();
        let paths = answer
            .paths
            .into_iter()
            .filter(|e| seen.insert(e.path.clone()))
            .map(|e| bytes::Bytes::from(e.path))
            .collect();
        Ok(Response::new(wire::JournalResponse { paths, token: token_text(&answer.token), fresh: answer.fresh }))
    }

    async fn clear_journal(
        &self,
        req: Request<wire::ClearJournalRequest>,
    ) -> Result<Response<wire::ClearJournalResponse>, Status> {
        let r = req.into_inner();
        let state = self.state()?;
        let m = state.mounted(parse_id(&r.ws_id)?)?;
        let upto: Token = r.token.parse().map_err(|e: asc_wc::journal::BadToken| Status::invalid_argument(e.to_string()))?;
        // Forget the hints up to the token the caller reconciled at; later ones stay.
        let all = m.journal.since(Some(&Token { epoch: upto.epoch, seq: 0 })).await.map_err(store)?;
        if all.fresh {
            return Err(Status::failed_precondition("the journal is stale or too large to clear; reconcile first"));
        }
        let mut seen = HashSet::new();
        let paths: Vec<Vec<u8>> = all.paths.into_iter().filter(|e| seen.insert(e.path.clone())).map(|e| e.path).collect();
        m.journal.clear(&paths, &upto).await.map_err(store)?;
        Ok(Response::new(wire::ClearJournalResponse {}))
    }

    async fn dehydrate(&self, req: Request<wire::DehydrateRequest>) -> Result<Response<wire::DehydrateResponse>, Status> {
        let r = req.into_inner();
        let paths = parse_paths(&r.pathspec)?;
        let (files, bytes_freed) = self.hosted(&r.ws_id, "dehydrate", async |m| m.hosted.dehydrate(&paths).await).await?;
        Ok(Response::new(wire::DehydrateResponse { files, bytes_freed }))
    }

    async fn prefetch(&self, req: Request<wire::PrefetchRequest>) -> Result<Response<wire::PrefetchResponse>, Status> {
        let r = req.into_inner();
        let paths = parse_paths(&r.pathspec)?;
        let (files, bytes_fetched) = self.hosted(&r.ws_id, "prefetch", async |m| m.hosted.prefetch(&paths).await).await?;
        Ok(Response::new(wire::PrefetchResponse { files, bytes_fetched }))
    }

    async fn stats(&self, req: Request<wire::StatsRequest>) -> Result<Response<wire::StatsResponse>, Status> {
        let r = req.into_inner();
        let state = self.state()?;
        // An empty id asks for the whole daemon.
        let wanted = if r.ws_id.is_empty() { state.mounts.all() } else { vec![state.mounted(parse_id(&r.ws_id)?)?] };
        let mut paths: HashMap<String, (u64, u64)> = HashMap::new();
        let mut procs: HashMap<String, (u64, u64)> = HashMap::new();
        let (mut bytes_hydrated, mut journal_size) = (0u64, 0u64);
        for m in &wanted {
            let s = m.hosted.stats();
            bytes_hydrated = bytes_hydrated.saturating_add(s.bytes_hydrated);
            for (k, n, b) in s.by_path {
                let e = paths.entry(k).or_default();
                *e = (e.0.saturating_add(n), e.1.saturating_add(b));
            }
            for (k, n, b) in s.by_process {
                let e = procs.entry(k).or_default();
                *e = (e.0.saturating_add(n), e.1.saturating_add(b));
            }
            journal_size = journal_size.saturating_add(state.journal_size(m).await);
        }
        let rows = |map: HashMap<String, (u64, u64)>| {
            let mut v: Vec<_> = map.into_iter().collect();
            v.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
            v.truncate(MAX_STATS_ROWS);
            v.into_iter().map(|(key, (count, bytes))| wire::HydrationCount { key, count, bytes }).collect()
        };
        Ok(Response::new(wire::StatsResponse {
            by_path: rows(paths),
            by_process: rows(procs),
            bytes_hydrated,
            journal_size,
        }))
    }

    async fn shutdown(&self, _: Request<wire::ShutdownRequest>) -> Result<Response<wire::ShutdownResponse>, Status> {
        // `notify_one` keeps a permit, so a stop requested before the server awaits is not lost.
        self.stop.notify_one();
        Ok(Response::new(wire::ShutdownResponse::default()))
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use asc_state::state::repos::{NewRepo, RemoteKind};
    use asc_state::state::workspaces::NewWorkspace;
    use asc_state::RepoId;

    use super::*;

    /// A home with a `state.db` and registered workspaces.
    pub struct Fixture {
        pub dir: tempfile::TempDir,
        pub state: Arc<State>,
    }

    pub async fn fixture(backend: Option<Arc<dyn VirtualBackend>>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let state = State::new(db, home, backend, Arc::new(Metrics::new()), Arc::new(EventLog::none()));
        Fixture { dir, state }
    }

    impl Fixture {
        pub async fn repo(&self) -> asc_store::refs::RepoKey {
            let id = RepoId::new();
            asc_state::state::repos::insert(
                &self.state.db,
                &NewRepo {
                    id,
                    name: "r".into(),
                    ns: vec![1, 2, 3],
                    remote_url: None,
                    remote_kind: RemoteKind::Server,
                    default_branch: "master".into(),
                    windows_safe: true,
                },
            )
            .await
            .unwrap()
        }

        /// Registers a workspace whose mount root is a new directory under the fixture.
        pub async fn workspace(&self, mode: Mode, ephemeral_until: Option<i64>) -> Workspace {
            let repo = self.repo().await;
            let id = WorkspaceId::new();
            let root = self.dir.path().join(format!("wc-{id}"));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.to_string_lossy().into_owned();
            let ws = NewWorkspace {
                id,
                repo,
                path: path.clone(),
                mount: path,
                mode,
                head: "refs/heads/master".into(),
                base_cs: None,
                sparse: None,
                ephemeral_until,
            };
            workspaces::insert(&self.state.db, &ws).await.unwrap();
            workspaces::get(&self.state.db, id).await.unwrap().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use asc_state::state::workspaces::Mode;
    use asc_wc::journal::Op;

    use super::fixtures::fixture;
    use super::*;

    fn service(state: Option<Arc<State>>) -> Service {
        Service::new("9.9.9".into(), Arc::new(Notify::new()), state)
    }

    fn id_of(ws: &Workspace) -> String {
        ws.ws.id.to_string()
    }

    async fn mount(svc: &Service, id: &str) -> Result<(), Status> {
        svc.mount(Request::new(wire::MountRequest { ws_id: id.into() })).await.map(|_| ())
    }

    fn code<T: std::fmt::Debug>(r: Result<T, Status>) -> tonic::Code {
        r.expect_err("expected an error").code()
    }

    /// Hosts nothing but counts and answers.
    #[derive(Default)]
    struct Host {
        dehydrated: AtomicU32,
        switched: AtomicU32,
    }

    #[async_trait]
    impl Hosted for Arc<Host> {
        fn stats(&self) -> HostStats {
            HostStats {
                by_path: vec![("src/a.rs".into(), 2, 20), ("src/b.rs".into(), 5, 50)],
                by_process: vec![("tsserver.exe".into(), 7, 70)],
                bytes_hydrated: 70,
            }
        }
        async fn dehydrate(&self, paths: &[RepoPath]) -> Result<(u64, u64), HostError> {
            self.dehydrated.fetch_add(1, Ordering::SeqCst);
            Ok((paths.len() as u64, 9))
        }
        async fn switch(&self, from: &[u8], to: &[u8], mode: wire::SwitchMode) -> Result<Vec<Vec<u8>>, HostError> {
            self.switched.fetch_add(1, Ordering::SeqCst);
            if mode == wire::SwitchMode::Force {
                return Ok(vec![]);
            }
            Ok(vec![[from, b":", to].concat()])
        }
        async fn prefetch(&self, _: &[RepoPath]) -> Result<(u64, u64), HostError> {
            Err(HostError::Failed("the cache is full".into()))
        }
    }

    struct HostBackend(Arc<Host>);

    #[async_trait]
    impl VirtualBackend for HostBackend {
        async fn host(&self, _: HostContext) -> Result<Arc<dyn Hosted>, HostError> {
            Ok(Arc::new(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn ping_and_shutdown_need_no_state() {
        let stop = Arc::new(Notify::new());
        let svc = Service::new("9.9.9".into(), stop.clone(), None);
        let pong = svc.ping(Request::new(wire::PingRequest {})).await.unwrap().into_inner();
        assert_eq!((pong.version.as_str(), pong.pid), ("9.9.9", std::process::id()));
        assert_eq!(code(mount(&svc, &WorkspaceId::new().to_string()).await), tonic::Code::FailedPrecondition);
        svc.unmount(Request::new(wire::UnmountRequest { ws_id: WorkspaceId::new().to_string() })).await.unwrap();
        svc.shutdown(Request::new(wire::ShutdownRequest {})).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), stop.notified()).await.expect("shutdown notifies");
        let status = svc.status(Request::new(wire::WsStatusRequest::default())).await;
        assert_eq!(code(status), tonic::Code::FailedPrecondition, "no state database, so no workspaces");
    }

    #[tokio::test]
    async fn mount_authorises_the_workspace_and_its_root() {
        let f = fixture(None).await;
        let svc = service(Some(f.state.clone()));

        assert_eq!(code(mount(&svc, "not-an-id").await), tonic::Code::InvalidArgument);
        assert_eq!(code(mount(&svc, &"a".repeat(500)).await), tonic::Code::InvalidArgument);
        assert_eq!(code(mount(&svc, &WorkspaceId::new().to_string()).await), tonic::Code::NotFound, "unregistered");

        let plain = f.workspace(Mode::Plain, None).await;
        mount(&svc, &id_of(&plain)).await.unwrap();
        mount(&svc, &id_of(&plain)).await.unwrap();
        assert_eq!(f.state.mounts.len(), 1, "mounting twice mounts once");

        let virt = f.workspace(Mode::Virtual, None).await;
        assert_eq!(code(mount(&svc, &id_of(&virt)).await), tonic::Code::FailedPrecondition, "no backend");

        // A root that is gone, or a file, is refused.
        let gone = f.workspace(Mode::Plain, None).await;
        std::fs::remove_dir_all(&gone.ws.mount).unwrap();
        assert_eq!(code(mount(&svc, &id_of(&gone)).await), tonic::Code::PermissionDenied);
        let file = f.workspace(Mode::Plain, None).await;
        std::fs::remove_dir_all(&file.ws.mount).unwrap();
        std::fs::write(&file.ws.mount, b"x").unwrap();
        assert_eq!(code(mount(&svc, &id_of(&file)).await), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn a_root_behind_a_link_is_refused() {
        let f = fixture(None).await;
        let svc = service(Some(f.state.clone()));
        let real = f.workspace(Mode::Plain, None).await;
        let link = f.dir.path().join("link");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real.ws.mount, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&real.ws.mount, &link).is_ok();
        if !made {
            return; // no privilege to make a link here
        }
        // The same workspace registered through the link, and one below it.
        let through = f.workspace(Mode::Plain, None).await;
        let below = link.join("inner");
        std::fs::create_dir_all(&below).unwrap();
        for target in [&link, &below] {
            let ws = through.ws.clone();
            workspaces::set_location(&f.state.db, ws.id, &target.to_string_lossy(), &target.to_string_lossy())
                .await
                .unwrap();
            assert_eq!(code(mount(&svc, &id_of(&through)).await), tonic::Code::PermissionDenied, "{}", target.display());
        }
    }

    #[tokio::test]
    async fn rpcs_act_only_on_mounted_workspaces() {
        let f = fixture(None).await;
        let svc = service(Some(f.state.clone()));
        let plain = f.workspace(Mode::Plain, None).await;
        let id = id_of(&plain);

        // Registered but not mounted: nothing is touched.
        let r = svc.journal(Request::new(wire::JournalRequest { ws_id: id.clone(), since_token: String::new() })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);
        let r = svc.dehydrate(Request::new(wire::DehydrateRequest { ws_id: id.clone(), pathspec: vec![] })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);
        let r = svc.switch(Request::new(wire::SwitchRequest { ws_id: id.clone(), ..Default::default() })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);
        let r = svc.stats(Request::new(wire::StatsRequest { ws_id: id.clone() })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);

        mount(&svc, &id).await.unwrap();
        // A plain workspace has no placeholders.
        let r = svc.dehydrate(Request::new(wire::DehydrateRequest { ws_id: id.clone(), pathspec: vec![] })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);
        let r = svc.prefetch(Request::new(wire::PrefetchRequest { ws_id: id.clone(), pathspec: vec![] })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);

        svc.unmount(Request::new(wire::UnmountRequest { ws_id: id.clone() })).await.unwrap();
        svc.unmount(Request::new(wire::UnmountRequest { ws_id: id.clone() })).await.unwrap();
        assert!(f.state.mounts.is_empty());
        let r = svc.stats(Request::new(wire::StatsRequest { ws_id: id })).await;
        assert_eq!(code(r), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn the_journal_round_trips_and_clears_up_to_a_token() {
        let f = fixture(None).await;
        let svc = service(Some(f.state.clone()));
        let ws = f.workspace(Mode::Plain, None).await;
        let id = id_of(&ws);
        mount(&svc, &id).await.unwrap();
        let m = f.state.mounted(ws.ws.id).unwrap();

        let ask = |token: String| {
            let id = id.clone();
            let svc = &svc;
            async move {
                svc.journal(Request::new(wire::JournalRequest { ws_id: id, since_token: token })).await.map(|r| r.into_inner())
            }
        };
        let first = ask(String::new()).await.unwrap();
        assert!(first.fresh, "no token: the caller must reconcile");

        m.journal.record(Op::Modified, b"a.txt");
        m.journal.record(Op::Modified, b"a.txt");
        m.journal.record(Op::Created, b"dir/b.txt");
        let next = ask(first.token.clone()).await.unwrap();
        assert!(!next.fresh);
        let paths: Vec<&[u8]> = next.paths.iter().map(|p| p.as_ref()).collect();
        assert_eq!(paths, [b"a.txt".as_slice(), b"dir/b.txt".as_slice()], "deduplicated, in order");

        m.journal.record(Op::Modified, b"later.txt");
        svc.clear_journal(Request::new(wire::ClearJournalRequest { ws_id: id.clone(), token: next.token.clone() }))
            .await
            .unwrap();
        let after = ask(first.token.clone()).await.unwrap();
        let left: Vec<&[u8]> = after.paths.iter().map(|p| p.as_ref()).collect();
        assert_eq!(left, [b"later.txt".as_slice()], "a hint recorded after the token survives");

        for bad in ["x", "1:", ":2", "1:2:3", "-1:0", &"9".repeat(200)] {
            assert_eq!(code(ask(bad.into()).await), tonic::Code::InvalidArgument, "{bad}");
            let r = svc.clear_journal(Request::new(wire::ClearJournalRequest { ws_id: id.clone(), token: bad.into() })).await;
            assert_eq!(code(r), tonic::Code::InvalidArgument, "{bad}");
        }
    }

    #[tokio::test]
    async fn virtual_workspaces_dispatch_to_their_host() {
        let host = Arc::new(Host::default());
        let f = fixture(Some(Arc::new(HostBackend(host.clone())))).await;
        let svc = service(Some(f.state.clone()));
        let ws = f.workspace(Mode::Virtual, None).await;
        let id = id_of(&ws);
        mount(&svc, &id).await.unwrap();

        let d = svc
            .dehydrate(Request::new(wire::DehydrateRequest {
                ws_id: id.clone(),
                pathspec: vec![bytes::Bytes::from_static(b"src"), bytes::Bytes::from_static(b"docs/a.md")],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((d.files, d.bytes_freed), (2, 9));

        let sw = |mode: wire::SwitchMode| wire::SwitchRequest {
            ws_id: id.clone(),
            from_root: bytes::Bytes::from_static(b"old"),
            to_root: bytes::Bytes::from_static(b"new"),
            mode: mode as i32,
        };
        let safe = svc.switch(Request::new(sw(wire::SwitchMode::Safe))).await.unwrap().into_inner();
        assert_eq!(safe.conflicts, [bytes::Bytes::from_static(b"old:new")]);
        let forced = svc.switch(Request::new(sw(wire::SwitchMode::Force))).await.unwrap().into_inner();
        assert!(forced.conflicts.is_empty());
        assert_eq!(host.switched.load(Ordering::SeqCst), 2);

        // A host failure is an internal error and is counted; a precondition is not.
        let r = svc.prefetch(Request::new(wire::PrefetchRequest { ws_id: id.clone(), pathspec: vec![] })).await;
        assert_eq!(code(r), tonic::Code::Internal);
        assert_eq!(f.state.metrics.provider_errors("prefetch"), 1);

        let stats = svc.stats(Request::new(wire::StatsRequest { ws_id: id.clone() })).await.unwrap().into_inner();
        assert_eq!(stats.bytes_hydrated, 70);
        assert_eq!(stats.by_path[0].key, "src/b.rs", "busiest first");
        assert_eq!(stats.by_process[0].count, 7);
        let all = svc.stats(Request::new(wire::StatsRequest { ws_id: String::new() })).await.unwrap().into_inner();
        assert_eq!(all.bytes_hydrated, 70, "an empty id asks about the whole daemon");

        // What a scrape reports.
        let c = f.state.collect().await;
        assert_eq!(c.fetch_bytes, 70);
        assert_eq!(c.hydrations, vec![("tsserver.exe".to_owned(), 7)]);
    }

    #[tokio::test]
    async fn requests_are_bounded() {
        let host = Arc::new(Host::default());
        let f = fixture(Some(Arc::new(HostBackend(host.clone())))).await;
        let svc = service(Some(f.state.clone()));
        let ws = f.workspace(Mode::Virtual, None).await;
        let id = id_of(&ws);
        mount(&svc, &id).await.unwrap();

        let many = vec![bytes::Bytes::from_static(b"a"); MAX_PATHSPECS + 1];
        let r = svc.dehydrate(Request::new(wire::DehydrateRequest { ws_id: id.clone(), pathspec: many })).await;
        assert_eq!(code(r), tonic::Code::InvalidArgument);
        for bad in [&b""[..], b"/abs", b"a//b", b"a/../b", b"a\0b", b"."] {
            let r = svc
                .dehydrate(Request::new(wire::DehydrateRequest { ws_id: id.clone(), pathspec: vec![bytes::Bytes::copy_from_slice(bad)] }))
                .await;
            assert_eq!(code(r), tonic::Code::InvalidArgument, "{bad:?}");
        }
        let r = svc
            .switch(Request::new(wire::SwitchRequest {
                ws_id: id.clone(),
                from_root: bytes::Bytes::from(vec![0u8; MAX_ROOT + 1]),
                ..Default::default()
            }))
            .await;
        assert_eq!(code(r), tonic::Code::InvalidArgument);
        let r = svc.switch(Request::new(wire::SwitchRequest { ws_id: id.clone(), mode: 99, ..Default::default() })).await;
        assert_eq!(code(r), tonic::Code::InvalidArgument);
        assert_eq!(host.dehydrated.load(Ordering::SeqCst) + host.switched.load(Ordering::SeqCst), 0, "nothing reached the host");
    }

    #[tokio::test]
    async fn shutdown_unmounts_everything_and_refuses_new_mounts() {
        let f = fixture(None).await;
        let svc = service(Some(f.state.clone()));
        let ws = f.workspace(Mode::Plain, None).await;
        mount(&svc, &id_of(&ws)).await.unwrap();
        f.state.shutdown().await;
        assert!(f.state.mounts.is_empty());
        assert_eq!(code(mount(&svc, &id_of(&ws)).await), tonic::Code::Unavailable, "no new mounts while stopping");
    }

    #[tokio::test]
    async fn restore_remounts_virtual_workspaces_only() {
        let host = Arc::new(Host::default());
        let f = fixture(Some(Arc::new(HostBackend(host)))).await;
        let virt = f.workspace(Mode::Virtual, None).await;
        let plain = f.workspace(Mode::Plain, None).await;
        f.state.restore().await;
        assert!(f.state.mounts.get(virt.ws.id).is_some());
        assert!(f.state.mounts.get(plain.ws.id).is_none(), "plain workspaces are mounted on request");
    }

    /// SPEC §16.4: the daemon with two mounted plain workspaces, idle, holds at most 64 MiB.
    /// The measurement runs in a fresh process (this test binary re-runs itself), because the
    /// resident set of a test binary that has run hundreds of tests says nothing about the
    /// daemon.
    #[test]
    fn idle_daemon_rss_stays_under_64_mib() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "daemon::service::tests::idle_daemon_child", "--ignored", "--nocapture", "--test-threads=1"])
            .env("ASC_IDLE_DAEMON_CHILD", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "child failed:\n{text}\n{}", String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("idle rss "), "{text}");
    }

    #[tokio::test]
    #[ignore = "run by idle_daemon_rss_stays_under_64_mib in a fresh process"]
    async fn idle_daemon_child() {
        use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};

        if std::env::var_os("ASC_IDLE_DAEMON_CHILD").is_none() {
            return;
        }
        let f = fixture(None).await;
        let (a, b) = (f.workspace(Mode::Plain, None).await, f.workspace(Mode::Plain, None).await);
        let endpoint = if cfg!(windows) {
            IpcEndpoint::from_raw(format!("asc-u53b-idle-{}", std::process::id()))
        } else {
            IpcEndpoint::from_raw(f.dir.path().join("run").join("asc.sock").to_string_lossy().into_owned())
        };
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let mut settings = super::super::scheduler::Settings::new(std::path::PathBuf::from("asc"));
        settings.timing = super::super::scheduler::Timing {
            cache_gc: Duration::from_millis(200),
            idle_dehydrate: Duration::from_millis(100),
            ephemeral_expiry: Duration::from_millis(200),
            pending_retry: Duration::from_millis(100),
            prefetch: Duration::from_millis(100),
            update_check: Duration::from_millis(500),
        };
        settings.idle_after = Duration::ZERO;
        let home = f.state.home.clone();
        let cfg = super::super::RunConfig {
            endpoint: endpoint.clone(),
            run_dir: home.run(),
            version: "1.0.0".into(),
        };
        let hosting = super::super::Hosting {
            home,
            metrics_listen: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            backend: None,
            settings,
            hooks: super::super::scheduler::Hooks::default(),
            extra_jobs: Vec::new(),
        };
        let daemon = tokio::spawn(super::super::run_hosting(cfg, hosting, std::future::pending()));

        let mut client = WorkspaceClient::new(connect(&endpoint));
        let mut up = false;
        for _ in 0..200 {
            if client.ping(wire::PingRequest {}).await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(up, "the daemon did not come up");
        for ws in [&a, &b] {
            client.mount(wire::MountRequest { ws_id: ws.ws.id.to_string() }).await.unwrap();
        }
        // Idle: the scheduler ticks several times and nothing else happens.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let rss = super::super::metrics::rss_bytes().expect("rss is readable on this platform");
        println!("idle rss {rss}");

        // The same figure is on the metrics endpoint.
        let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut tcp, format!("GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut body = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut tcp, &mut body).await.unwrap();
        assert!(body.contains("asc_rss_bytes ") && body.contains("asc_daemon_task_runs_total{task=\"pending_retry\"}"), "{body}");

        client.shutdown(wire::ShutdownRequest {}).await.unwrap();
        daemon.await.unwrap().unwrap();
        assert!(rss <= 64 * 1024 * 1024, "idle daemon RSS is {} MiB, the budget is 64", rss >> 20);
    }
}
