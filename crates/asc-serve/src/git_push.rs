//! The server side of `git push` (SPEC §25.2 "Writes", §25.5, §25.6, §25.8; PLAN U82). Only
//! built with the Cargo feature `git-compat`.
//!
//! `git receive-pack --stateless-rpc` runs on the repo's git view with one asc-owned
//! `pre-receive` hook. The hook (`asc serve git-hook pre-receive`, [`run_hook`]) is the only place
//! git waits for asc:
//!
//! 1. the HTTP handler ([`crate::git_http`]) mints a [`PushSession`]: a single-use 256-bit token
//!    bound to the authenticated principal, the repo, the view and a fresh `op_id`, and starts
//!    `receive-pack` with `ASC_GIT_HOOK_TOKEN` and `ASC_GIT_HOOK_ENDPOINT` added to its
//!    environment (nothing else of asc's is, and never a credential);
//! 2. git unpacks the push into its quarantine and starts the hook, which connects to this
//!    process's local socket (a Unix socket in a 0700 directory, a named pipe with the owner-only
//!    DACL of the daemon on Windows) and presents the token;
//! 3. [`GitPush`] consumes the token (a replayed or expired one is refused), proves the
//!    directories the hook names are the quarantine of this session's view
//!    ([`asc_git::compat::receive::validate_quarantine`]), checks `max_push_objects` and
//!    `max_object_bytes` from the quarantine, applies the push options, and runs the shared
//!    `import_git_push` (U48d): import under the per-repo lock and the GC fence, then
//!    `UpdateRefs` through the normal pipeline, without `SKIP_PRE`, with `via = git-push`;
//! 4. the answer goes back to the hook, which prints a refusal on stderr (git relays it as
//!    `remote:` lines and reports `pre-receive hook declined`) and exits 0 or 1.
//!
//! A hook outcome is all-or-nothing for the whole push. Every limit here is a named constant or a
//! `[limits.git]` value; every refusal text is built from fixed words (a ref name or path from the
//! client is made printable first), never from an internal error.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use asc_core::id::GitOid;
use asc_core::path::checkout_safe;
use asc_git::compat::hook::{
    ENV_ENDPOINT, ENV_TOKEN, HEADER_LEN, HookInput, HookReply, HookRequest, MAX_REPLY_BYTES,
    MAX_REQUEST_BYTES, UNREACHABLE_MESSAGE, frame_len, printable,
};
use asc_git::compat::receive::{
    PushOptions, QUARANTINE_PREFIXES, ReceiveError, check_stats, validate_quarantine, view_objects,
};
use asc_git::compat::{Activity, GitLimits, GitView, ViewError};
use asc_rpc::errors::RpcError;
use asc_rpc::ipc::{IpcEndpoint, IpcListener, IpcStream, connect_stream};
use asc_rpc::server::{CallCtx, status_rpc_error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::Status;
use uuid::Uuid;

use crate::git_view::GitViewService;
use crate::import_push::{ImportOptions, ImportPush, PushCommand, Quarantine};
use crate::registry::{Registry, RepoEntry};
use crate::update::PipelineHooks;

/// The `via` of the ref log row of a push through the git protocol (SPEC §25.2 step 5).
pub const VIA: &str = "git-push";
/// How long a request token lives at most (SPEC §25.2: "at most 15 minutes"); the request
/// deadline caps it further.
pub const TOKEN_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// Pushes with a live token at once on one replica; a ceiling over `push_per_replica`.
pub const MAX_LIVE_SESSIONS: usize = 4096;
/// Hook connections served at once.
pub const MAX_HOOK_CONNECTIONS: usize = 256;
/// Time a connected hook has to send its request.
pub const HOOK_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Time the hook process waits for the socket when it connects.
pub const HOOK_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time to write a reply to the hook.
pub const HOOK_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// The idle watchdog of the child is told about progress this many times per idle period.
const KEEPALIVE_DIVISOR: u32 = 4;
/// A quarantine older than this was left by a child that was killed (SPEC §25.8 item 4: "the
/// janitor removes any left after a crash after 24 hours"); a request lives 15 minutes at most.
pub const QUARANTINE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the janitor looks.
pub const SWEEP_PERIOD: Duration = Duration::from_secs(60 * 60);
/// The one hook git runs.
const HOOK_NAME: &str = "pre-receive";
/// What a hook is told when its token is not (or no longer) valid. It names nothing.
const TOKEN_REFUSED: &str = "asc: this push is no longer accepted; push again";
/// What the hook is told when the server fails for a reason of its own.
const SERVER_FAILED: &str = "asc: the server could not process the push; try again later";
/// Characters allowed in the fixed arguments of the hook program (never client text).
const ARG_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.:=/-";

/// Why the push layer could not start.
#[derive(Debug, thiserror::Error)]
pub enum PushSetupError {
    #[error("the hook program path is not usable: {0}")]
    HookProgram(&'static str),
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error("the hook socket: {0}")]
    Socket(String),
}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> PushSetupError {
    move |source| PushSetupError::Io { what, source }
}

// ------------------------------------------------------------------------------ the program

/// The program the `pre-receive` script executes: an absolute path and fixed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookProgram {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

impl HookProgram {
    /// This executable running `serve git-hook pre-receive` (the `asc` binary).
    pub fn current_asc() -> io::Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
            args: ["serve", "git-hook", "pre-receive"]
                .map(str::to_owned)
                .to_vec(),
        })
    }

    /// The text of the script: two lines, no variable and no client text. The path and the
    /// arguments are checked, so nothing in them can end the quoting.
    pub fn script(&self) -> Result<String, PushSetupError> {
        let bad = PushSetupError::HookProgram;
        if !self.exe.is_absolute() {
            return Err(bad("it is not absolute"));
        }
        let exe = self.exe.to_string_lossy().replace('\\', "/");
        if exe.contains(['\'', '\n', '\r', '\0']) {
            return Err(bad("it has a quote or a line break"));
        }
        let mut line = format!("exec '{exe}'");
        for arg in &self.args {
            if arg.is_empty() || !arg.chars().all(|c| ARG_CHARS.contains(c)) {
                return Err(bad("an argument has an unexpected character"));
            }
            line.push(' ');
            line.push_str(arg);
        }
        Ok(format!("#!/bin/sh\n{line}\n"))
    }
}

/// Makes `dir` hold exactly the `pre-receive` script `script`, read-only. Anything else found in
/// the asc-owned directory is removed.
fn install_hook(dir: &Path, script: &str) -> Result<(), PushSetupError> {
    fs::create_dir_all(dir).map_err(io_err("creating the hooks directory"))?;
    let target = dir.join(HOOK_NAME);
    for entry in fs::read_dir(dir).map_err(io_err("reading the hooks directory"))? {
        let entry = entry.map_err(io_err("reading the hooks directory"))?;
        if entry.file_name() == HOOK_NAME {
            continue;
        }
        let path = entry.path();
        let _ = make_writable(&path);
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
    if fs::read_to_string(&target).ok().as_deref() == Some(script) && is_read_only(&target) {
        return Ok(());
    }
    let tmp = dir.join(format!(".{HOOK_NAME}.{}", Uuid::new_v4().simple()));
    fs::write(&tmp, script).map_err(io_err("writing the hook"))?;
    set_executable_read_only(&tmp).map_err(io_err("protecting the hook"))?;
    let _ = make_writable(&target);
    fs::rename(&tmp, &target).map_err(io_err("installing the hook"))
}

fn is_read_only(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.permissions().readonly())
}

fn make_writable(path: &Path) -> io::Result<()> {
    let mut perms = fs::symlink_metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o700);
    }
    #[cfg(not(unix))]
    {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
    }
    fs::set_permissions(path, perms)
}

fn set_executable_read_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o555))
    }
    #[cfg(not(unix))]
    {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)
    }
}

// ------------------------------------------------------------------------------ slots

/// Why a push could not start now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// This principal has `push_per_principal` pushes running.
    Principal,
    /// This replica has `push_per_replica` pushes running.
    Replica,
}

/// Concurrent pushes per principal and per replica (SPEC §25.5), held for a push's whole run.
pub struct PushSlots {
    replica: Arc<Semaphore>,
    principals: Arc<Mutex<HashMap<String, u32>>>,
    per_principal: u32,
}

/// A held push slot; released on drop.
pub struct PushPermit {
    _replica: OwnedSemaphorePermit,
    principals: Arc<Mutex<HashMap<String, u32>>>,
    principal: String,
}

impl Drop for PushPermit {
    fn drop(&mut self) {
        let mut map = self
            .principals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = map.get_mut(&self.principal) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.principal);
            }
        }
    }
}

impl PushSlots {
    pub fn new(limits: &GitLimits) -> Self {
        Self {
            replica: Arc::new(Semaphore::new(limits.push_per_replica as usize)),
            principals: Arc::default(),
            per_principal: limits.push_per_principal,
        }
    }

    /// Takes a slot for `principal`, or says which cap is full.
    pub fn acquire(&self, principal: &str) -> Result<PushPermit, Busy> {
        {
            let mut map = self
                .principals
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let n = map.entry(principal.to_owned()).or_insert(0);
            if *n >= self.per_principal {
                return Err(Busy::Principal);
            }
            *n += 1;
        }
        // The principal count is already taken: build the permit first so every exit releases it.
        let permit = |replica| PushPermit {
            _replica: replica,
            principals: self.principals.clone(),
            principal: principal.to_owned(),
        };
        match self.replica.clone().try_acquire_owned() {
            Ok(replica) => Ok(permit(replica)),
            Err(_) => {
                // Give back the principal's count through a permit-less path.
                let mut map = self
                    .principals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(n) = map.get_mut(principal) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        map.remove(principal);
                    }
                }
                Err(Busy::Replica)
            }
        }
    }
}

// ------------------------------------------------------------------------------ sessions

/// One ref change of a push, for the audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    pub name: String,
    pub old: Option<GitOid>,
    pub new: Option<GitOid>,
}

/// What the hook did with a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    /// `ok`, `rejected:<reason>`, `limit:<which>`; `rejected:no_hook` while the hook never ran.
    pub result: String,
    pub objects: u64,
    /// The refs moved, with old and new git ids; empty unless the push committed.
    pub refs: Vec<RefChange>,
}

impl Default for PushOutcome {
    fn default() -> Self {
        Self {
            result: "rejected:no_hook".into(),
            objects: 0,
            refs: Vec::new(),
        }
    }
}

struct Session {
    ctx: CallCtx,
    entry: Arc<RepoEntry>,
    view: GitView,
    expires: Instant,
    activity: OnceLock<Activity>,
    outcome: Mutex<PushOutcome>,
}

impl Session {
    fn record(&self, outcome: PushOutcome) {
        *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = outcome;
    }
}

/// The live tokens of one server process.
#[derive(Default)]
struct Broker {
    sessions: Mutex<HashMap<[u8; 32], Arc<Session>>>,
}

impl Broker {
    fn mint(&self, session: Arc<Session>) -> Result<String, Status> {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| {
            tracing::error!(error = %e, "no random source for a push token");
            Status::internal("internal error")
        })?;
        let mut map = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        map.retain(|_, s| s.expires > now);
        if map.len() >= MAX_LIVE_SESSIONS {
            return Err(Status::unavailable("too many pushes are running"));
        }
        map.insert(raw, session);
        Ok(hex(&raw))
    }

    /// Consumes a token: it works once. `None` for an unknown, used or expired one.
    fn take(&self, token: &str) -> Option<Arc<Session>> {
        let raw = unhex(token)?;
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&raw)?;
        (session.expires > Instant::now()).then_some(session)
    }

    fn forget(&self, token: &str) {
        if let Some(raw) = unhex(token) {
            self.sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&raw);
        }
    }

    fn live(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

fn hex(raw: &[u8; 32]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if !asc_git::compat::hook::valid_token(text) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// A started push: the environment for `receive-pack` and, after it ends, what the hook did.
/// Dropping it voids the token.
pub struct PushSession {
    token: String,
    inner: Arc<Inner>,
    session: Arc<Session>,
}

impl PushSession {
    /// The two `ASC_*` variables the hook needs.
    pub fn env(&self) -> [(&'static str, &str); 2] {
        [
            (ENV_TOKEN, self.token.as_str()),
            (ENV_ENDPOINT, self.inner.endpoint.as_str()),
        ]
    }

    /// Lets the hook keep the child's idle watchdog fed while a long import runs.
    pub fn set_activity(&self, activity: Activity) {
        let _ = self.session.activity.set(activity);
    }

    /// What the hook recorded; `rejected:no_hook` if it never ran.
    pub fn outcome(&self) -> PushOutcome {
        self.session
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for PushSession {
    fn drop(&mut self) {
        self.inner.broker.forget(&self.token);
    }
}

// ------------------------------------------------------------------------------ the service

struct Inner {
    registry: Arc<Registry>,
    views: Arc<GitViewService>,
    import: ImportPush,
    limits: GitLimits,
    endpoint: String,
    broker: Broker,
    slots: PushSlots,
    hook_connections: Arc<Semaphore>,
}

/// Receive-pack for the git protocol layer: the hook socket, the live tokens and the push slots.
#[derive(Clone)]
pub struct GitPush {
    inner: Arc<Inner>,
}

impl GitPush {
    /// Installs the hook script, binds the hook socket and starts serving it. Must run inside the
    /// tokio runtime (the accept loop is a task).
    pub fn start(
        views: Arc<GitViewService>,
        registry: Arc<Registry>,
        pipeline: Arc<PipelineHooks>,
        hook: &HookProgram,
    ) -> Result<Self, PushSetupError> {
        let hooks_dir = views.server().hooks_dir().to_path_buf();
        install_hook(&hooks_dir, &hook.script()?)?;
        let compat = hooks_dir.parent().ok_or(PushSetupError::HookProgram(
            "the hooks directory has no parent",
        ))?;
        let run_dir = compat.join("run");
        fs::create_dir_all(&run_dir).map_err(io_err("creating the hook socket directory"))?;
        let id = Uuid::new_v4().simple().to_string();
        let endpoint = if cfg!(windows) {
            IpcEndpoint::from_raw(format!("asc-git-hook-{id}"))
        } else {
            IpcEndpoint::from_raw(
                run_dir
                    .join(format!("hook-{}.sock", &id[..16]))
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        let listener =
            IpcListener::bind(&endpoint).map_err(|e| PushSetupError::Socket(e.to_string()))?;
        let limits = views.server().limits().clone();
        let inner = Arc::new(Inner {
            import: ImportPush::new(registry.clone(), pipeline),
            registry,
            views,
            slots: PushSlots::new(&limits),
            limits,
            endpoint: endpoint.as_str().to_owned(),
            broker: Broker::default(),
            hook_connections: Arc::new(Semaphore::new(MAX_HOOK_CONNECTIONS)),
        });
        tokio::spawn(accept_loop(inner.clone(), listener));
        tokio::spawn(janitor(inner.views.view_dir().to_path_buf()));
        Ok(Self { inner })
    }

    pub fn limits(&self) -> &GitLimits {
        &self.inner.limits
    }

    /// Takes a push slot for `principal` (SPEC §25.5).
    pub fn acquire(&self, principal: &str) -> Result<PushPermit, Busy> {
        self.inner.slots.acquire(principal)
    }

    /// Whether the volume of the views has at least `min_free_disk` free (SPEC §25.5). A volume
    /// that cannot be measured is treated as full: a push fails closed.
    pub fn disk_ok(&self) -> bool {
        let need = self.inner.limits.min_free_disk;
        need == 0
            || fs4::available_space(self.inner.views.view_dir()).is_ok_and(|free| free >= need)
    }

    /// Starts a session for a push by `ctx.principal` to `entry`'s `view`.
    pub fn begin(
        &self,
        ctx: CallCtx,
        entry: Arc<RepoEntry>,
        view: GitView,
    ) -> Result<PushSession, Status> {
        let lifetime = TOKEN_LIFETIME.min(self.inner.limits.request_timeout);
        let session = Arc::new(Session {
            ctx,
            entry,
            view,
            expires: Instant::now() + lifetime,
            activity: OnceLock::new(),
            outcome: Mutex::new(PushOutcome::default()),
        });
        let token = self.inner.broker.mint(session.clone())?;
        Ok(PushSession {
            token,
            inner: self.inner.clone(),
            session,
        })
    }

    /// Live tokens (for tests and metrics).
    pub fn live_sessions(&self) -> usize {
        self.inner.broker.live()
    }

    /// The registry the service was started with.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }

    /// The hook socket's name, for tests that act as the hook.
    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }
}

async fn accept_loop(inner: Arc<Inner>, listener: IpcListener) {
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let Ok(permit) = inner.hook_connections.clone().try_acquire_owned() else {
                    // Over the cap: the hook sees a closed socket and declines the push.
                    drop(stream);
                    continue;
                };
                let inner = inner.clone();
                tokio::spawn(async move {
                    serve_connection(&inner, stream).await;
                    drop(permit);
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "accepting on the git hook socket");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Removes the quarantines that killed children left behind, now and then every [`SWEEP_PERIOD`].
async fn janitor(view_dir: PathBuf) {
    let mut tick = tokio::time::interval(SWEEP_PERIOD);
    loop {
        tick.tick().await;
        let dir = view_dir.clone();
        match tokio::task::spawn_blocking(move || sweep_quarantines(&dir, QUARANTINE_MAX_AGE)).await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!(removed = n, "removed stale git push quarantines"),
            Err(e) => tracing::warn!(error = %e, "the git quarantine janitor failed"),
        }
    }
}

/// Deletes the quarantine directories (`objects/tmp_objdir-incoming-*`) of every view under
/// `view_dir` that were last written at least `max_age` ago. Returns how many were removed. A
/// link is never followed: it is removed as a link.
pub fn sweep_quarantines(view_dir: &Path, max_age: Duration) -> usize {
    let mut removed = 0;
    let Ok(views) = fs::read_dir(view_dir) else {
        return 0;
    };
    for view in views.flatten() {
        let objects = view.path().join("objects");
        let Ok(entries) = fs::read_dir(&objects) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let named = name
                .to_str()
                .is_some_and(|n| QUARANTINE_PREFIXES.iter().any(|p| n.starts_with(p)));
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let old = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age >= max_age);
            if named && old {
                let path = entry.path();
                let gone = if meta.file_type().is_symlink() {
                    fs::remove_file(&path)
                } else {
                    fs::remove_dir_all(&path)
                };
                if gone.is_ok() {
                    removed += 1;
                }
            }
        }
    }
    removed
}

async fn read_request(stream: &mut IpcStream) -> io::Result<HookRequest> {
    let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let n = frame_len(&header, MAX_REQUEST_BYTES).map_err(|e| invalid(e.to_string()))?;
    let mut payload = vec![0u8; n];
    stream.read_exact(&mut payload).await?;
    HookRequest::decode(&payload).map_err(|e| invalid(e.to_string()))
}

async fn serve_connection(inner: &Arc<Inner>, mut stream: IpcStream) {
    let request = tokio::time::timeout(HOOK_READ_TIMEOUT, read_request(&mut stream)).await;
    let reply = match request {
        Ok(Ok(request)) => inner.serve_hook(request).await,
        // A malformed or stalled request never reaches a session.
        _ => HookReply::reject(TOKEN_REFUSED),
    };
    let write = async {
        stream.write_all(&reply.encode()).await?;
        stream.flush().await
    };
    let _ = tokio::time::timeout(HOOK_WRITE_TIMEOUT, write).await;
}

/// A refusal of a push: a label for metrics and the audit record, and the text for `remote:`.
struct Refused {
    reason: String,
    text: String,
}

impl Refused {
    fn new(reason: &str, text: impl Into<String>) -> Self {
        Self {
            reason: reason.to_owned(),
            text: text.into(),
        }
    }

    fn from_receive(e: &ReceiveError) -> Self {
        let reason = match e {
            ReceiveError::NotTheQuarantine => "quarantine",
            ReceiveError::TooManyObjects { .. } => "limit:objects",
            ReceiveError::ObjectTooLarge { .. } => "limit:object_bytes",
            ReceiveError::LandNeedsAsc => "land",
            ReceiveError::BadTtl => "ttl",
            ReceiveError::Malformed => "malformed",
        };
        Self::new(reason, e.to_string())
    }
}

/// Ends the keepalive task when the hook request ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Inner {
    async fn serve_hook(self: &Arc<Self>, request: HookRequest) -> HookReply {
        let Some(session) = self.broker.take(&request.token) else {
            return HookReply::reject(TOKEN_REFUSED);
        };
        // Keeps the child's idle watchdog fed while the import runs: git itself is silent.
        let _keepalive = session.activity.get().cloned().map(|activity| {
            let period =
                (self.limits.idle_timeout / KEEPALIVE_DIVISOR).max(Duration::from_millis(100));
            AbortOnDrop(tokio::spawn(async move {
                loop {
                    activity.touch();
                    tokio::time::sleep(period).await;
                }
            }))
        });
        let deadline = tokio::time::Instant::from_std(session.expires);
        let done =
            tokio::time::timeout_at(deadline, self.check_and_import(&session, &request)).await;
        match done {
            Ok(Ok(reply)) => reply,
            Ok(Err(refused)) => {
                let kind = if refused.reason.starts_with("limit:") {
                    refused.reason.clone()
                } else {
                    format!("rejected:{}", refused.reason)
                };
                let mut outcome = PushOutcome {
                    result: kind,
                    ..PushOutcome::default()
                };
                outcome.objects = session
                    .outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .objects;
                session.record(outcome);
                HookReply::reject(refused.text)
            }
            Err(_) => {
                session.record(PushOutcome {
                    result: "limit:deadline".into(),
                    ..PushOutcome::default()
                });
                HookReply::reject("asc: the push took too long")
            }
        }
    }

    async fn check_and_import(
        &self,
        session: &Arc<Session>,
        request: &HookRequest,
    ) -> Result<HookReply, Refused> {
        let paths = validate_quarantine(session.view.path(), request)
            .map_err(|e| Refused::from_receive(&e))?;
        let options = PushOptions::from_request(request).map_err(|e| Refused::from_receive(&e))?;
        let commands: Vec<PushCommand> = request
            .commands
            .iter()
            .map(|c| PushCommand {
                name: c.name.clone(),
                old: c.old,
                new: c.new,
            })
            .collect();

        // Object count and size, from the quarantine, before any import (SPEC §25.5). A push of
        // deletions only has no pack and no quarantine: nothing to count, and the import reads
        // nothing.
        let mut objects = 0;
        let quarantine = match paths {
            Some(paths) => {
                let (view, dir) = (session.view.clone(), paths.object_dir.clone());
                let stats = tokio::task::spawn_blocking(move || view.quarantine_stats(&dir))
                    .await
                    .map_err(|e| server_failed("stats task", &e))?
                    .map_err(|e: ViewError| server_failed("quarantine stats", &e))?;
                session
                    .outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .objects = stats.objects;
                check_stats(&stats, &self.limits).map_err(|e| Refused::from_receive(&e))?;
                objects = stats.objects;
                Quarantine {
                    object_dir: paths.object_dir,
                    alternate_dirs: paths.alternate_dirs,
                }
            }
            None => Quarantine {
                object_dir: view_objects(session.view.path()),
                alternate_dirs: Vec::new(),
            },
        };
        let opts = ImportOptions {
            skip_pre: false,
            push_options: options.all,
            ttl_seconds: options.ttl_seconds,
            import_grade: false,
            via: Some(VIA),
        };
        let done = self
            .import
            .import_git_push(&session.ctx, &quarantine, &commands, &opts)
            .await
            .map_err(|status| refusal_of(&status))?;

        let mut lines = Vec::new();
        for (command, result) in commands.iter().zip(&done.results) {
            if let Some(line) =
                user_ref_line(&command.name, &result.name, &session.entry.default_branch)
            {
                lines.push(line);
            }
        }
        session.record(PushOutcome {
            result: "ok".into(),
            objects,
            refs: commands
                .iter()
                .map(|c| RefChange {
                    name: c.name.clone(),
                    old: c.old,
                    new: c.new,
                })
                .collect(),
        });
        Ok(HookReply::accept(lines.join("\n")))
    }
}

fn server_failed(context: &str, e: &dyn std::fmt::Display) -> Refused {
    tracing::error!(%context, error = %e, "git push failed inside the server");
    Refused::new("internal", SERVER_FAILED)
}

/// The `remote:` line for a plain push that `UpdateRefs` stored as the pusher's own branch
/// (SPEC §25.6): the pipeline answered with a `refs/users/…` name for a `refs/heads/…` push.
pub fn user_ref_line(pushed: &str, stored: &str, trunk: &str) -> Option<String> {
    let branch = pushed.strip_prefix("refs/heads/")?;
    (stored != pushed && stored.starts_with("refs/users/")).then(|| {
        format!(
            "asc: stored as your branch '{}'; run 'asc submit' or push HEAD:refs/for/{} to open a draft change",
            printable(branch, 200),
            printable(trunk, 200)
        )
    })
}

/// A refusal from the pipeline's status: a reason label and the lines the user sees. Internal
/// errors become one fixed sentence; nothing of their text is shown.
fn refusal_of(status: &Status) -> Refused {
    let name = |paths: &[String]| paths.first().map(|p| printable(p, 300)).unwrap_or_default();
    let Some(error) = status_rpc_error(status) else {
        return match status.code() {
            tonic::Code::InvalidArgument
            | tonic::Code::NotFound
            | tonic::Code::PermissionDenied => Refused::new(
                "invalid",
                format!("asc: {}", printable(status.message(), 200)),
            ),
            tonic::Code::ResourceExhausted => Refused::new(
                "limit:server",
                format!("asc: {}", printable(status.message(), 200)),
            ),
            tonic::Code::Unavailable | tonic::Code::FailedPrecondition => Refused::new(
                "unavailable",
                "asc: the repository is busy or being changed; try again",
            ),
            _ => Refused::new("internal", SERVER_FAILED),
        };
    };
    let paths = &error.info().paths;
    let reason = error.code().to_ascii_lowercase();
    let text = match &error {
        RpcError::PathPolicy(_) => {
            let p = name(paths);
            match asc_core::path::RepoPath::new(p.as_bytes()) {
                Ok(path) if !checkout_safe(&path) => {
                    format!("asc: refusing path '{p}': a .git alias")
                }
                _ => format!("asc: refusing path '{p}': not allowed by the path policy"),
            }
        }
        RpcError::RefConflict(_) => format!(
            "asc: {} was changed by someone else; fetch and try again",
            name(paths)
        ),
        RpcError::NotFastForward(_) => format!(
            "asc: refusing a non-fast-forward update of {}; fetch and try again",
            name(paths)
        ),
        RpcError::HookRejected(_) => format!("asc: {}", name(paths).trim_start_matches("asc: ")),
        RpcError::MirrorMode(_) => "asc: this repository is a read-only mirror".to_owned(),
        RpcError::InvalidObject(_) => format!("asc: the push is not acceptable: {}", name(paths)),
        RpcError::MissingObjects(_) => "asc: the push is missing objects".to_owned(),
        RpcError::RefNotShared(_) => format!("asc: {} is not a shared ref", name(paths)),
        RpcError::UserRefsLimit(_) | RpcError::QuotaExceeded(_) => {
            "asc: the quota for your branches is used up".to_owned()
        }
        _ => format!("asc: the push was refused ({reason})"),
    };
    Refused::new(&reason, text)
}

// ------------------------------------------------------------------------------ the hook process

/// The whole of `asc serve git-hook pre-receive`: reads git's stdin and environment, asks the
/// server and prints its answer on stderr. Returns the exit status (0 accepts the push). Any
/// failure to reach or understand the server declines the push.
pub async fn run_hook() -> i32 {
    let input = match HookInput::collect(io::stdin().lock(), std::env::vars_os()) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("{UNREACHABLE_MESSAGE}");
            return 1;
        }
    };
    match request_hook(&input).await {
        Ok(reply) => {
            eprint!("{}", reply.stderr_text());
            reply.exit_code()
        }
        Err(_) => {
            eprintln!("{UNREACHABLE_MESSAGE}");
            1
        }
    }
}

/// Sends `input` to the hook socket it names and reads the answer: the client half of the hook,
/// public for the tests of this crate.
pub async fn request_hook(input: &HookInput) -> io::Result<HookReply> {
    let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
    let endpoint = IpcEndpoint::from_raw(input.endpoint.clone());
    let mut stream = tokio::time::timeout(HOOK_CONNECT_TIMEOUT, connect_stream(&endpoint))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connecting"))??;
    let frame = input.request.encode().map_err(|e| invalid(e.to_string()))?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let n = frame_len(&header, MAX_REPLY_BYTES).map_err(|e| invalid(e.to_string()))?;
    let mut payload = vec![0u8; n];
    stream.read_exact(&mut payload).await?;
    Ok(HookReply::decode(&payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(principal: u32, replica: u32) -> GitLimits {
        GitLimits {
            push_per_principal: principal,
            push_per_replica: replica,
            ..GitLimits::default()
        }
    }

    #[test]
    fn the_script_is_two_fixed_lines() {
        let exe = if cfg!(windows) {
            PathBuf::from(r"C:\tools\asc.exe")
        } else {
            PathBuf::from("/usr/bin/asc")
        };
        let p = HookProgram {
            exe,
            args: vec!["serve".into(), "git-hook".into(), "pre-receive".into()],
        };
        let s = p.script().unwrap();
        assert_eq!(s.lines().count(), 2);
        assert!(s.starts_with("#!/bin/sh\nexec '"));
        assert!(s.ends_with("' serve git-hook pre-receive\n"));
        assert!(!s.contains('\\'));
    }

    #[test]
    fn a_hostile_path_or_argument_cannot_end_the_quoting() {
        let abs = if cfg!(windows) { "C:/x" } else { "/x" };
        for exe in [
            format!("{abs}/a'b"),
            format!("{abs}/a\nb"),
            format!("{abs}/a\0b"),
            "rel/asc".into(),
        ] {
            let p = HookProgram {
                exe: PathBuf::from(exe),
                args: vec![],
            };
            assert!(p.script().is_err());
        }
        for arg in ["a b", "a;b", "$(x)", "`x`", "a'b", "", "a\nb", "a&b"] {
            let p = HookProgram {
                exe: PathBuf::from(format!("{abs}/asc")),
                args: vec![arg.into()],
            };
            assert!(p.script().is_err(), "{arg:?}");
        }
    }

    #[test]
    fn installing_replaces_everything_else_and_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir_all(hooks.join("sub")).unwrap();
        fs::write(hooks.join("post-receive"), "evil").unwrap();
        fs::write(hooks.join("pre-receive"), "old").unwrap();
        install_hook(&hooks, "#!/bin/sh\nexec x\n").unwrap();
        let names: Vec<_> = fs::read_dir(&hooks)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["pre-receive"]);
        assert_eq!(
            fs::read_to_string(hooks.join("pre-receive")).unwrap(),
            "#!/bin/sh\nexec x\n"
        );
        assert!(is_read_only(&hooks.join("pre-receive")));
        // Again, and with other content over the read-only file.
        install_hook(&hooks, "#!/bin/sh\nexec x\n").unwrap();
        install_hook(&hooks, "#!/bin/sh\nexec y\n").unwrap();
        assert_eq!(
            fs::read_to_string(hooks.join("pre-receive")).unwrap(),
            "#!/bin/sh\nexec y\n"
        );
    }

    #[test]
    fn push_slots_cap_per_principal_and_per_replica_and_release() {
        let slots = PushSlots::new(&limits(2, 3));
        let a1 = slots.acquire("a").unwrap();
        let _a2 = slots.acquire("a").unwrap();
        assert_eq!(slots.acquire("a").err(), Some(Busy::Principal));
        let _b1 = slots.acquire("b").unwrap();
        assert_eq!(slots.acquire("c").err(), Some(Busy::Replica));
        // The refused attempts left no count behind.
        drop(a1);
        let _a3 = slots.acquire("a").unwrap();
        assert_eq!(slots.acquire("a").err(), Some(Busy::Principal));
    }

    #[test]
    fn user_ref_line_only_for_a_stored_user_branch() {
        let line = user_ref_line("refs/heads/feat", "refs/users/7/heads/feat", "master").unwrap();
        assert_eq!(
            line,
            "asc: stored as your branch 'feat'; run 'asc submit' or push HEAD:refs/for/master to open a draft change"
        );
        assert_eq!(
            user_ref_line("refs/heads/feat", "refs/heads/feat", "master"),
            None
        );
        assert_eq!(
            user_ref_line("refs/tags/v1", "refs/users/7/tags/v1", "master"),
            None
        );
    }

    #[test]
    fn tokens_are_hex_round_trip_and_single_use() {
        let raw = [0xabu8; 32];
        assert_eq!(unhex(&hex(&raw)), Some(raw));
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(&"A".repeat(64)), None);
    }

    #[test]
    fn refusals_name_paths_but_never_internals() {
        use crate::update::{info, rpc};
        let r = refusal_of(&rpc(RpcError::PathPolicy(info(vec![
            "git~1/config".into(),
        ]))));
        assert_eq!(r.text, "asc: refusing path 'git~1/config': a .git alias");
        let r = refusal_of(&rpc(RpcError::PathPolicy(info(vec!["con.txt".into()]))));
        assert!(r.text.contains("path policy"), "{}", r.text);
        let r = refusal_of(&Status::internal("sqlx: password=hunter2 at /srv/db"));
        assert_eq!(r.text, SERVER_FAILED);
        assert!(!r.text.contains("hunter2"));
        let r = refusal_of(&rpc(RpcError::HookRejected(info(vec![
            "protected branch".into(),
        ]))));
        assert_eq!(r.text, "asc: protected branch");
        // Terminal escapes in a client-chosen name are dropped.
        let r = refusal_of(&rpc(RpcError::RefConflict(info(vec![
            "refs/heads/a\u{1b}[2Jb".into(),
        ]))));
        assert!(!r.text.contains('\u{1b}'));
    }
}
