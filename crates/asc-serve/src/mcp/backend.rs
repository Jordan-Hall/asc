//! `ServeBackend`: the MCP read tools over the server's own gRPC handlers (SPEC 22.7.2, 16.3;
//! PLAN M12b).
//!
//! A tool call becomes an in-process call of the same tonic services that serve the wire, with no
//! socket in between. Every call:
//!
//! 1. needs an authenticated principal (the auth layer of M13 and M29 sets it);
//! 2. holds one of at most `[mcp] max_inflight_per_principal` slots of that principal on this
//!    instance, else `RATE_LIMITED`;
//! 3. is checked against [`RepoGate`]. A repository the principal cannot read answers the very
//!    same `NOT_FOUND` as one that does not exist, so a caller learns nothing about private names;
//! 4. is admitted by the SPEC 16.3 [`Admission`] under the rate class of its RPC
//!    ([`class_of`]), keyed by the principal like every other surface;
//! 5. runs with `id_mode = GIT`, so ids in and out are git oids.
//!
//! Authorisation inside the handlers cannot use the caller's token (the backend never sees it).
//! The services are built with a [`ServeAuthorizer`], which accepts one kind of credential in
//! addition to the real ones: a per-call secret minted here, bound to one principal and one
//! repository and to read access. It dies with the call. The 256-bit instance secret never leaves
//! the process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::catalogue::{self, Needs, ToolDef};
use asc_mcp::cursor::Position;
use asc_mcp::errors::{ToolError, code};
use asc_mcp::rpc_backend::DynCodec;
use asc_mcp::schema::Surface;
use asc_mcp::search;
use asc_rpc::IdMode;
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision, Principal, SshIdentity,
    SshKey,
};
use asc_rpc::json;
use asc_rpc::retry::retry_after;
use asc_rpc::server::{RepoName, status_rpc_error};
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use prost_reflect::{
    DescriptorPool, DynamicMessage, Kind, MethodDescriptor, ReflectMessage as _, Value as PbValue,
};
use serde_json::{Map, Value};
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Routes;
use tonic::{Code, Request, Status};

use super::write;
use crate::limits::{Admission, Caller, Class, class_of};

/// Largest message accepted from a handler.
pub(super) const MAX_MESSAGE: usize = 17 * 1024 * 1024;
/// Longest a call may run.
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest handler-supplied error text kept.
const MAX_ERROR_TEXT: usize = 512;
/// Arguments the MCP layer owns; they are never part of a wire request.
const PLUMBING: [&str; 5] = ["cursor", "max_tokens", "worktree", "repo", "ws_id"];
/// The one message for a repository that is missing or not readable.
pub(super) const REPO_NOT_FOUND: &str = "repository not found";
/// Retry hint for a principal over its in-flight cap.
pub(super) const INFLIGHT_RETRY_MS: u64 = 500;
/// Prefix of the per-call credential. A caller-supplied token with this prefix still has to carry
/// the instance secret.
const CALL_PREFIX: &str = "ascmcp";

/// Whether a principal may read a repository. The answer for a missing repository must equal the
/// answer for an unreadable one (`false`).
///
/// In production this is [`super::gate::LedgerGate`]: the authenticator authorises the call with
/// one `Authorizer::authorize_credential(credential, repo, Read)` (through the shared cache
/// entry) and the gate answers from that decision.
#[async_trait]
pub trait RepoGate: Send + Sync + 'static {
    /// `true` only when `principal` may read `repo` (`owner/name`).
    async fn can_read(&self, principal: &str, repo: &str) -> bool;
}

/// Whether a principal may write a repository (`owner/name`). Installed with
/// [`ServeBackend::with_writes`]; without it no write tool runs. The `[mcp] write` switch is the
/// decision to install it, and the answer is the authorizer's `Access::Write`, never the token's
/// scope text alone. A principal that may not even read answers the same `NOT_FOUND` as a missing
/// repository.
#[async_trait]
pub trait WriteGate: Send + Sync + 'static {
    /// `true` only when `principal` may write `repo`.
    async fn can_write(&self, principal: &str, repo: &str) -> bool;
}

// ------------------------------------------------------------------------------ authorizer

#[derive(Clone)]
struct Grant {
    access: Access,
    principal: String,
    repo: String,
}

/// Mints and checks per-call credentials, and forwards everything else to the real authorizer.
pub struct ServeAuthorizer {
    inner: Arc<dyn Authorizer>,
    secret: String,
    next: AtomicU64,
    grants: Mutex<HashMap<u64, Grant>>,
}

impl ServeAuthorizer {
    /// Wraps `inner`, the authorizer the public services use.
    ///
    /// # Panics
    /// When the operating system has no random source.
    #[must_use]
    pub fn new(inner: Arc<dyn Authorizer>) -> Arc<Self> {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("random source");
        let secret = raw.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
        Arc::new(Self {
            inner,
            secret,
            next: AtomicU64::new(1),
            grants: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn mint(self: &Arc<Self>, principal: &str, repo: &str, access: Access) -> CallGrant {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Grant {
                    access,
                    principal: principal.to_owned(),
                    repo: repo.to_owned(),
                },
            );
        CallGrant {
            owner: Arc::clone(self),
            id,
            token: format!("{CALL_PREFIX}.{}.{id}", self.secret),
        }
    }

    /// The grant a per-call credential names. `None` when `token` is not one of ours; `Some(None)`
    /// when it has our prefix but is forged or spent.
    fn lookup(&self, token: &str) -> Option<Option<Grant>> {
        let rest = token.strip_prefix(CALL_PREFIX)?.strip_prefix('.')?;
        let (secret, id) = rest.split_once('.')?;
        if !constant_time_eq(secret.as_bytes(), self.secret.as_bytes()) {
            // Not ours: the real authorizer decides (and will refuse it).
            return None;
        }
        let grant = id.parse::<u64>().ok().and_then(|id| {
            self.grants
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&id)
                .cloned()
        });
        Some(grant)
    }

    fn decide(grant: Option<Grant>, repo: &str, access: Access) -> Result<Principal, AuthError> {
        let grant = grant.ok_or(AuthError::Unauthenticated)?;
        // The access the call was gated for, on the one repository it was gated for, nothing more.
        if access > grant.access || grant.repo != repo {
            return Err(AuthError::Forbidden);
        }
        Ok(Principal {
            id: grant.principal,
        })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A minted credential; the grant is revoked when it drops.
pub(super) struct CallGrant {
    owner: Arc<ServeAuthorizer>,
    id: u64,
    pub(super) token: String,
}

impl Drop for CallGrant {
    fn drop(&mut self) {
        self.owner
            .grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

#[async_trait]
impl Authorizer for ServeAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        if let Some(grant) = token.and_then(|t| self.lookup(t.expose())) {
            return Self::decide(grant, repo, access);
        }
        self.inner.authorize(token, repo, access).await
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        if let Some(grant) = credential.and_then(|c| self.lookup(c.secret().expose())) {
            return Self::decide(grant, repo, access);
        }
        self.inner
            .authorize_credential(credential, repo, access)
            .await
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        self.inner.authenticate(credential).await
    }

    async fn identify_ssh_key(&self, key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        self.inner.identify_ssh_key(key).await
    }

    async fn authorize_ssh_key(
        &self,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<Decision, AuthError> {
        self.inner.authorize_ssh_key(key, repo, access).await
    }
}

// ------------------------------------------------------------------------------ in flight

/// In-flight calls per principal on this instance.
pub(super) struct Inflight {
    max: usize,
    count: Mutex<HashMap<String, usize>>,
}

pub(super) struct InflightGuard {
    owner: Arc<Inflight>,
    key: String,
}

impl Inflight {
    pub(super) fn acquire(self: &Arc<Self>, principal: &str) -> Option<InflightGuard> {
        let mut map = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        let n = map.entry(principal.to_owned()).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(InflightGuard {
            owner: Arc::clone(self),
            key: principal.to_owned(),
        })
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut map = self
            .owner
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

// ------------------------------------------------------------------------------ backend

/// The `McpBackend` of `asc serve`.
pub struct ServeBackend {
    pub(super) routes: Routes,
    pub(super) pool: Arc<DescriptorPool>,
    pub(super) scope: Arc<ServeAuthorizer>,
    pub(super) gate: Arc<dyn RepoGate>,
    pub(super) writes: Option<Arc<dyn WriteGate>>,
    pub(super) admission: Arc<Admission>,
    pub(super) inflight: Arc<Inflight>,
}

impl std::fmt::Debug for ServeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServeBackend")
    }
}

impl ServeBackend {
    /// A backend over `routes`, which must have been built with `scope` as their authorizer and
    /// must not sit behind the admission layer (this backend charges admission itself, by
    /// principal). `max_inflight` is `[mcp] max_inflight_per_principal`.
    ///
    /// # Panics
    /// Never in practice: the descriptor set is built into the binary.
    #[must_use]
    pub fn new(
        routes: Routes,
        scope: Arc<ServeAuthorizer>,
        gate: Arc<dyn RepoGate>,
        admission: Arc<Admission>,
        max_inflight: u32,
    ) -> Self {
        let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).expect("built-in descriptor set");
        Self {
            routes,
            pool: Arc::new(pool),
            scope,
            gate,
            writes: None,
            admission,
            inflight: Arc::new(Inflight {
                max: max_inflight.max(1) as usize,
                count: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Serves the write tools of SPEC 22.3.3 (`commit`, `submit`, `rebase`, `restack`), each
    /// gated by `gate`. Call it only when `[mcp] write` is set.
    #[must_use]
    pub fn with_writes(mut self, gate: Arc<dyn WriteGate>) -> Self {
        self.writes = Some(gate);
        self
    }

    /// The method a catalogue row's `rpc` names: the first alternative that is not a `Workspace`
    /// (daemon) call.
    pub(super) fn method(&self, rpc: &str) -> Option<MethodDescriptor> {
        let alt = rpc
            .split(" / ")
            .map(str::trim)
            .find(|a| !a.starts_with("Workspace."))?;
        let (svc, method) = alt.split_once('.').unwrap_or(("Repo", alt));
        let service = self.pool.get_service_by_name(&format!("asc.v1.{svc}"))?;
        service.methods().find(|m| m.name() == method)
    }

    /// The rate class a tool's RPC is charged to.
    #[must_use]
    pub fn class_of_tool(&self, tool: &str) -> Option<Class> {
        let def = catalogue::find(tool, Surface::Http)?;
        let m = self.method(def.rpc)?;
        Some(
            class_of(&format!("/{}/{}", m.parent_service().full_name(), m.name()))
                .unwrap_or(Class::Expensive),
        )
    }

    pub(super) fn build(
        def: &ToolDef,
        method: &MethodDescriptor,
        repo: &str,
        args: &Value,
        ctx: &CallContext,
    ) -> Result<DynamicMessage, ToolError> {
        let input = method.input();
        let mut obj = args.as_object().cloned().unwrap_or_default();
        for k in PLUMBING {
            obj.remove(k);
        }
        if method.name() == "ReadFile"
            && input
                .get_field_by_name("rev_path")
                .is_some_and(|f| f.kind().as_message().is_none())
        {
            let rev = obj
                .remove("rev")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let path = obj
                .remove("path")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            if rev.is_empty() {
                return Err(ToolError::new(
                    "INVALID_ARGUMENT",
                    "read_file needs rev on a server",
                ));
            }
            obj.insert("rev_path".into(), Value::String(format!("{rev}:{path}")));
        }
        // A `query` wrapper takes the flat arguments its inner message knows.
        if let Some(q) = input
            .get_field_by_name("query")
            .and_then(|f| f.kind().as_message().cloned())
            && !obj.contains_key("query")
        {
            let mut inner = Map::new();
            for k in obj.keys().cloned().collect::<Vec<_>>() {
                if input.get_field_by_name(&k).is_none()
                    && q.get_field_by_name(&k).is_some()
                    && let Some(v) = obj.remove(&k)
                {
                    inner.insert(k, v);
                }
            }
            obj.insert("query".into(), Value::Object(inner));
        }
        // MCP-only fields are not wire fields.
        obj.retain(|k, _| input.get_field_by_name(k).is_some());
        if let Some(Position::Page { token }) = &ctx.position {
            for name in ["page_token", "after"] {
                if input.get_field_by_name(name).is_some() {
                    obj.insert(name.into(), Value::String(token.clone()));
                    break;
                }
            }
        }
        if let Some(Position::File { offset, .. }) = &ctx.position
            && input.get_field_by_name("offset").is_some()
        {
            obj.insert("offset".into(), Value::from(*offset));
        }
        let mut msg = json::decode(&input, &Value::Object(obj))
            .map_err(|e| ToolError::new("INVALID_ARGUMENT", format!("{}: {e}", def.name)))?;
        set_repo(&mut msg, repo);
        if let Some(PbValue::Message(q)) = msg
            .get_field_by_name("query")
            .map(std::borrow::Cow::into_owned)
        {
            let mut q = q;
            set_repo(&mut q, repo);
            msg.set_field_by_name("query", PbValue::Message(q));
        }
        Ok(msg)
    }
}

/// Sets `repo` from the gated name, always with `id_mode = GIT`. The arguments never decide it.
pub(super) fn set_repo(msg: &mut DynamicMessage, name: &str) {
    let Some(f) = msg.descriptor().get_field_by_name("repo") else {
        return;
    };
    let Kind::Message(d) = f.kind() else { return };
    let mut repo = DynamicMessage::new(d);
    repo.set_field_by_name("name", PbValue::String(name.to_owned()));
    repo.set_field_by_name("id_mode", PbValue::EnumNumber(IdMode::Git.to_wire()));
    msg.set_field_by_name("repo", PbValue::Message(repo));
}

pub(super) fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_TEXT)
        .collect()
}

fn retry_ms(s: &Status) -> u64 {
    retry_after(s)
        .map_or(1000, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .max(1)
}

/// Maps a handler failure to the tool error the model sees.
pub(super) fn map_status(s: &Status) -> ToolError {
    if let Some(e) = status_rpc_error(s) {
        let mut t = ToolError::new(e.code(), clean(s.message()));
        t.paths = e.info().paths.iter().map(|p| clean(p)).collect();
        if t.code == code::RATE_LIMITED {
            t.retry_after_ms = Some(retry_ms(s));
        }
        return t;
    }
    let message = clean(s.message());
    match s.code() {
        Code::NotFound | Code::PermissionDenied | Code::Unauthenticated => {
            ToolError::new(code::NOT_FOUND, REPO_NOT_FOUND)
        }
        Code::ResourceExhausted => ToolError::rate_limited(message, retry_ms(s)),
        Code::Unavailable => ToolError::not_available(if message.is_empty() {
            "the server is busy".into()
        } else {
            message
        }),
        Code::InvalidArgument | Code::OutOfRange => ToolError::new("INVALID_ARGUMENT", message),
        Code::Unimplemented | Code::FailedPrecondition => ToolError::not_available(message),
        Code::Cancelled => ToolError::internal("the call was cancelled"),
        Code::DeadlineExceeded => ToolError::internal("the call timed out"),
        _ => ToolError::internal(message),
    }
}

pub(super) fn encode_msg(m: &DynamicMessage) -> Result<Value, ToolError> {
    json::encode(m).map_err(|e| ToolError::internal(format!("response cannot be shown: {e}")))
}

#[async_trait]
impl McpBackend for ServeBackend {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let def = catalogue::find(tool, Surface::Http)
            .ok_or_else(|| ToolError::not_available(format!("{tool} is not served here")))?;
        // The server write tools (M14) have their own path. `land` is M32.
        if write::is_write_tool(def) {
            return write::call(self, def, args, ctx).await;
        }
        if !def.ann.read_only || def.needs != Needs::None {
            return Err(ToolError::not_available(format!(
                "{tool} is not served by this endpoint"
            )));
        }
        // `search` is one of three `CodeIntel` RPCs, chosen by `mode` (unit I21).
        let search = if def.name == "search" {
            let page = match &ctx.position {
                Some(Position::Page { token }) => Some(token.as_str()),
                _ => None,
            };
            Some(search::plan(args, ctx.budget_bytes, page)?)
        } else {
            None
        };
        let method = match &search {
            Some(plan) => self.method(&format!("CodeIntel.{}", plan.rpc.method())),
            None => self.method(def.rpc),
        }
        .ok_or_else(|| {
            ToolError::not_available(format!("{tool} is not served by this endpoint"))
        })?;
        let Some(principal) = ctx.principal.as_deref() else {
            return Err(ToolError::new(
                "UNAUTHENTICATED",
                "authentication is required",
            ));
        };
        let repo_arg = args
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("INVALID_ARGUMENT", "repo (owner/name) is required"))?;
        let repo = RepoName::parse(repo_arg)
            .map_err(|s| ToolError::new("INVALID_ARGUMENT", clean(s.message())))?;

        let slot = self.inflight.acquire(principal).ok_or_else(|| {
            ToolError::rate_limited(
                "too many calls in flight for this principal",
                INFLIGHT_RETRY_MS,
            )
        })?;
        if !self.gate.can_read(principal, repo.as_str()).await {
            return Err(ToolError::new(code::NOT_FOUND, REPO_NOT_FOUND));
        }

        let path = format!("/{}/{}", method.parent_service().full_name(), method.name());
        let class = class_of(&path).unwrap_or(Class::Expensive);
        let permit = self
            .admission
            .admit(Caller::principal(principal), class, 1)
            .map_err(|s| map_status(&s))?;

        let request = match &search {
            Some(plan) => search::wire_request(plan, &method, repo.as_str(), IdMode::Git)?,
            None => Self::build(def, &method, repo.as_str(), args, ctx)?,
        };
        let grant = self.scope.mint(principal, repo.as_str(), Access::Read);
        let mut req = Request::new(request);
        req.set_timeout(CALL_TIMEOUT);
        let meta = req.metadata_mut();
        if let Ok(mut v) = format!("Bearer {}", grant.token).parse::<MetadataValue<Ascii>>() {
            v.set_sensitive(true);
            meta.insert("authorization", v);
        }
        if let Ok(v) =
            format!("asc/{} mcp", env!("CARGO_PKG_VERSION")).parse::<MetadataValue<Ascii>>()
        {
            meta.insert("x-asc-client", v);
        }

        let uri: PathAndQuery = path
            .parse()
            .map_err(|_| ToolError::internal("bad method path"))?;
        let mut grpc = Grpc::new(self.routes.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let codec = DynCodec::new(method.output());
        let started = async {
            grpc.ready()
                .await
                .map_err(|e| ToolError::not_available(clean(&e.to_string())))?;
            if method.is_server_streaming() {
                let r = grpc
                    .server_streaming(req, uri, codec)
                    .await
                    .map_err(|s| map_status(&s))?;
                Ok::<_, ToolError>(r.into_inner().boxed())
            } else {
                let r = grpc
                    .unary(req, uri, codec)
                    .await
                    .map_err(|s| map_status(&s))?;
                Ok(stream::once(async move { Ok(r.into_inner()) }).boxed())
            }
        };
        let raw: BoxStream<'static, Result<DynamicMessage, Status>> = tokio::select! {
            r = started => r?,
            () = ctx.cancel.cancelled() => return Err(ToolError::internal("the call was cancelled")),
        };

        let skip = match &ctx.position {
            Some(Position::Index { index }) => usize::try_from(*index).unwrap_or(usize::MAX),
            _ => 0,
        };
        let budget = ctx.budget_bytes;
        // The slot, the admission permit and the credential live as long as the stream.
        let holds = (slot, permit, grant);
        let shape = search.is_some();
        let messages = raw
            .skip(skip)
            .map(|r| r.map_err(|s| map_status(&s)).and_then(|m| encode_msg(&m)))
            // `search` replies carry `scope`, `coverage` and `est_tokens` whatever the service sent.
            .map(move |r| if shape { r.map(search::finish) } else { r })
            .scan((0u64, false, holds), move |(used, done, _holds), item| {
                if *done {
                    return futures::future::ready(None);
                }
                if let Ok(v) = &item {
                    *used = used.saturating_add(v.to_string().len() as u64);
                    *done = *used > budget;
                }
                futures::future::ready(Some(item))
            })
            .take_until(ctx.cancel.clone().cancelled_owned())
            .boxed();
        Ok(Reply::Messages(messages))
    }
}

#[cfg(test)]
mod tests {
    use asc_rpc::auth::StaticTokens;

    use super::*;

    fn scope() -> Arc<ServeAuthorizer> {
        ServeAuthorizer::new(Arc::new(StaticTokens::new().with_token(
            "real",
            "carol",
            Access::Write,
        )))
    }

    fn tok(s: &str) -> BearerToken {
        BearerToken::new(s).unwrap()
    }

    #[tokio::test]
    async fn a_call_credential_is_bound_to_one_principal_repo_and_read() {
        let s = scope();
        let g = s.mint("alice", "acme/app", Access::Read);
        let t = tok(&g.token);
        assert_eq!(
            s.authorize(Some(&t), "acme/app", Access::Read)
                .await
                .unwrap()
                .id,
            "alice"
        );
        // Another repository, or more than read, is refused.
        assert!(matches!(
            s.authorize(Some(&t), "acme/other", Access::Read).await,
            Err(AuthError::Forbidden)
        ));
        assert!(matches!(
            s.authorize(Some(&t), "acme/app", Access::Write).await,
            Err(AuthError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn a_forged_or_spent_credential_is_refused() {
        let s = scope();
        let g = s.mint("alice", "acme/app", Access::Read);
        let id = g.id;
        // Right shape, wrong secret: handed to the real authorizer, which refuses it.
        let forged = tok(&format!("{CALL_PREFIX}.{}.{id}", "0".repeat(64)));
        assert!(
            s.authorize(Some(&forged), "acme/app", Access::Read)
                .await
                .is_err()
        );
        // Right secret, an id nobody holds.
        let unknown = tok(&format!("{CALL_PREFIX}.{}.{}", s.secret, id + 1000));
        assert!(matches!(
            s.authorize(Some(&unknown), "acme/app", Access::Read).await,
            Err(AuthError::Unauthenticated)
        ));
        // Revoked when the call ends.
        let t = tok(&g.token);
        drop(g);
        assert!(matches!(
            s.authorize(Some(&t), "acme/app", Access::Read).await,
            Err(AuthError::Unauthenticated)
        ));
    }

    #[tokio::test]
    async fn real_tokens_still_go_to_the_inner_authorizer() {
        let s = scope();
        assert_eq!(
            s.authorize(Some(&tok("real")), "acme/app", Access::Write)
                .await
                .unwrap()
                .id,
            "carol"
        );
        assert!(s.authorize(None, "acme/app", Access::Read).await.is_err());
    }

    #[test]
    fn the_inflight_cap_counts_per_principal() {
        let f = Arc::new(Inflight {
            max: 2,
            count: Mutex::new(HashMap::new()),
        });
        let a = f.acquire("p").unwrap();
        let _b = f.acquire("p").unwrap();
        assert!(f.acquire("p").is_none());
        assert!(f.acquire("q").is_some());
        drop(a);
        assert!(f.acquire("p").is_some());
    }
}
