//! `RpcBackend`: the MCP read tools over gRPC (SPEC 22.3.2, 22.7.2, unit M08).
//!
//! Repository calls (`Repo`, `Changes`, `Projects`) go to an asc server; `Workspace` calls go to the
//! daemon over the IPC channel. The catalogue row names the RPC (`Service.Method`); the request is
//! built from the validated tool arguments by descriptor, so the backend holds no per-tool code
//! beyond the few places where the MCP projection differs from the wire message:
//!
//! - `repo` and `ws_id` are injected from the connection and never taken from the arguments;
//! - `read_file` joins `rev` and `path` into `rev_path` (`rev:path`);
//! - a `query` wrapper (`Workspace.Log`, `Workspace.Affected`) is filled from the flat arguments.
//!
//! Only read tools are routed (`ann.read_only`, no `needs`). A write tool, a child-verb tool and
//! `search` (M15 wires `CodeIntel`) answer `NOT_AVAILABLE`. Responses are encoded in the asc JSON
//! profile. The stream stops (and is dropped, which cancels the RPC) when the byte budget is spent
//! or the call is cancelled.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use asc_rpc::IdMode;
use asc_rpc::auth::Credential;
use asc_rpc::client::{ClientError, connect_channel};
use asc_rpc::json;
use asc_rpc::server::status_rpc_error;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use prost_reflect::prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, Kind, MessageDescriptor, MethodDescriptor, ReflectMessage as _,
    Value as PbValue,
};
use serde_json::{Map, Value};
use tonic::client::Grpc;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

use crate::backend::{CallContext, McpBackend, Reply};
use crate::catalogue::{self, Needs, ToolDef};
use crate::cursor::Position;
use crate::errors::{ToolError, code};
use crate::schema::Surface;

/// Largest message accepted from a server (matches the asc client).
const MAX_MESSAGE: usize = 17 * 1024 * 1024;
/// Longest a call may run before the server is told to stop.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest server-supplied error text kept.
const MAX_ERROR_TEXT: usize = 512;
/// Arguments the MCP layer owns; they are never part of a wire request.
const PLUMBING: [&str; 5] = ["cursor", "max_tokens", "worktree", "repo", "ws_id"];

/// An asc server and the repository on it.
#[derive(Clone)]
struct Remote {
    channel: Channel,
    repo: String,
    credential: Option<Credential>,
}

/// The daemon's IPC channel and the workspace the server was started for.
#[derive(Clone)]
struct Local {
    channel: Channel,
    ws_id: String,
}

/// The `McpBackend` over gRPC.
#[derive(Clone)]
pub struct RpcBackend {
    surface: Surface,
    pool: Arc<DescriptorPool>,
    server: Option<Remote>,
    local: Option<Local>,
}

impl std::fmt::Debug for RpcBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcBackend")
            .field("surface", &self.surface)
            .field("server", &self.server.as_ref().map(|s| &s.repo))
            .field("local", &self.local.is_some())
            .finish()
    }
}

impl RpcBackend {
    /// A backend for `surface` with no connection yet.
    ///
    /// # Panics
    /// Never in practice: the descriptor set is built into the binary.
    #[must_use]
    pub fn new(surface: Surface) -> Self {
        let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).expect("built-in descriptor set");
        Self { surface, pool: Arc::new(pool), server: None, local: None }
    }

    /// Routes `Repo`, `Changes` and `Projects` calls to `channel`, for `repo` (`owner/name`).
    /// `credential` is sent as the `Authorization` header and never logged.
    #[must_use]
    pub fn with_server(mut self, channel: Channel, repo: &str, credential: Option<Credential>) -> Self {
        self.server = Some(Remote { channel, repo: repo.to_owned(), credential });
        self
    }

    /// [`Self::with_server`] over a lazily connecting channel to `url`. Plain `http` is refused
    /// by the transport policy except on the loopback interface when opted in.
    ///
    /// # Errors
    /// The URL is not one asc may connect to.
    pub fn connect_server(self, url: &str, repo: &str, credential: Option<Credential>) -> Result<Self, ClientError> {
        Ok(self.with_server(connect_channel(url)?, repo, credential))
    }

    /// Routes `Workspace` calls to the daemon on `channel` (`asc_rpc::ipc::connect`), for `ws_id`.
    #[must_use]
    pub fn with_workspace(mut self, channel: Channel, ws_id: &str) -> Self {
        self.local = Some(Local { channel, ws_id: ws_id.to_owned() });
        self
    }

    fn method(&self, rpc: &str) -> Option<MethodDescriptor> {
        let (svc, method) = rpc.split_once('.').unwrap_or(("Repo", rpc));
        let service = self.pool.get_service_by_name(&format!("asc.v1.{svc}"))?;
        service.methods().find(|m| m.name() == method)
    }

    /// The method a catalogue row's `rpc` names. A row such as `Workspace.Log / Repo.Log` lists
    /// alternatives: the first one this backend can reach wins (the daemon on stdio, else the
    /// server).
    fn resolve(&self, rpc: &str) -> Option<MethodDescriptor> {
        rpc.split(" / ")
            .map(str::trim)
            .filter(|alt| {
                let local = alt.starts_with("Workspace.");
                if local { self.surface == Surface::Stdio && self.local.is_some() } else { self.server.is_some() }
            })
            .find_map(|alt| self.method(alt))
            .or_else(|| self.method(rpc.split(" / ").next().unwrap_or(rpc)))
    }

    /// Builds the wire request for `def` from the validated `args`.
    fn build(&self, def: &ToolDef, method: &MethodDescriptor, args: &Value, ctx: &CallContext) -> Result<DynamicMessage, ToolError> {
        let input = method.input();
        let mut obj = args.as_object().cloned().unwrap_or_default();
        for k in PLUMBING {
            // `repo` (HTTP surface) is checked below; the connection decides it.
            if k != "repo" {
                obj.remove(k);
            }
        }
        if let (Some(want), Some(remote)) = (obj.remove("repo"), self.server.as_ref())
            && want.as_str().is_some_and(|w| w != remote.repo)
        {
            return Err(ToolError::new(code::PATH_POLICY, "the repository is fixed by the connection"));
        }
        if method.name() == "ReadFile" && input.get_field_by_name("rev_path").is_some_and(|f| f.kind().as_message().is_none()) {
            let rev = obj.remove("rev").and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
            let path = obj.remove("path").and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
            if rev.is_empty() {
                return Err(ToolError::not_available(
                    "a working-copy read is served by the local backend; give rev to read a revision",
                ));
            }
            obj.insert("rev_path".into(), Value::String(format!("{rev}:{path}")));
        }
        // A `query` wrapper takes the flat arguments its inner message knows.
        if let Some(q) = input.get_field_by_name("query").and_then(|f| f.kind().as_message().cloned())
            && !obj.contains_key("query")
        {
            let mut inner = Map::new();
            for k in obj.keys().cloned().collect::<Vec<_>>() {
                if input.get_field_by_name(&k).is_none() && q.get_field_by_name(&k).is_some()
                    && let Some(v) = obj.remove(&k)
                {
                    inner.insert(k, v);
                }
            }
            obj.insert("query".into(), Value::Object(inner));
        }
        // MCP-only fields (`start_line`, `limit` on a stream, ...) are not wire fields.
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
        self.inject(&mut msg);
        Ok(msg)
    }

    /// Sets `repo` and `ws_id` from the connection, on the message and on a `query` inside it.
    fn inject(&self, msg: &mut DynamicMessage) {
        if let Some(r) = &self.server {
            set_repo(msg, &r.repo);
            if let Some(PbValue::Message(q)) = msg.get_field_by_name("query").map(std::borrow::Cow::into_owned) {
                let mut q = q;
                set_repo(&mut q, &r.repo);
                msg.set_field_by_name("query", PbValue::Message(q));
            }
        }
        if let Some(l) = &self.local
            && msg.descriptor().get_field_by_name("ws_id").is_some()
        {
            msg.set_field_by_name("ws_id", PbValue::String(l.ws_id.clone()));
        }
    }

    /// The channel and credential for a service, or why there is none.
    fn route(&self, service: &str) -> Result<(Channel, Option<&Credential>), ToolError> {
        if service == "Workspace" {
            let l = self.local.as_ref().ok_or_else(|| ToolError::not_available("the workspace daemon is not connected"))?;
            return Ok((l.channel.clone(), None));
        }
        let r = self.server.as_ref().ok_or_else(|| ToolError::not_available("no asc server is configured for this workspace"))?;
        Ok((r.channel.clone(), r.credential.as_ref()))
    }
}

fn set_repo(msg: &mut DynamicMessage, name: &str) {
    let Some(f) = msg.descriptor().get_field_by_name("repo") else { return };
    let Kind::Message(d) = f.kind() else { return };
    let mut repo = DynamicMessage::new(d);
    repo.set_field_by_name("name", PbValue::String(name.to_owned()));
    repo.set_field_by_name("id_mode", PbValue::EnumNumber(IdMode::Asc.to_wire()));
    msg.set_field_by_name("repo", PbValue::Message(repo));
}

/// A tonic codec for [`DynamicMessage`]: it encodes any message and decodes into `decode`.
#[derive(Clone)]
pub struct DynCodec {
    decode: MessageDescriptor,
}

impl DynCodec {
    /// A codec that decodes into messages of type `decode`.
    #[must_use]
    pub const fn new(decode: MessageDescriptor) -> Self {
        Self { decode }
    }
}

/// Encodes a [`DynamicMessage`].
pub struct DynEncoder;
/// Decodes into a [`DynamicMessage`] of a fixed type.
pub struct DynDecoder(MessageDescriptor);

impl Encoder for DynEncoder {
    type Item = DynamicMessage;
    type Error = Status;
    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst).map_err(|e| Status::internal(e.to_string()))
    }
}

impl Decoder for DynDecoder {
    type Item = DynamicMessage;
    type Error = Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|e| Status::internal(format!("protocol violation: {e}")))
    }
}

impl Codec for DynCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynEncoder;
    type Decoder = DynDecoder;
    fn encoder(&mut self) -> Self::Encoder {
        DynEncoder
    }
    fn decoder(&mut self) -> Self::Decoder {
        DynDecoder(self.decode.clone())
    }
}

/// Maps a gRPC failure to the tool error the model sees. Server text is stripped of control
/// characters and bounded; the credential never appears in it.
fn map_status(s: &Status) -> ToolError {
    if let Some(e) = status_rpc_error(s) {
        let mut t = ToolError::new(e.code(), clean(s.message()));
        t.paths = e.info().paths.iter().map(|p| clean(p)).collect();
        return t;
    }
    let message = clean(s.message());
    match s.code() {
        Code::NotFound | Code::PermissionDenied => ToolError::new(code::NOT_FOUND, message),
        Code::Unauthenticated => ToolError::new("UNAUTHENTICATED", "authentication is required or has expired"),
        Code::ResourceExhausted => ToolError::rate_limited(message, 1000),
        Code::Unavailable => ToolError::not_available(if message.is_empty() { "the server is unreachable".into() } else { message }),
        Code::InvalidArgument | Code::OutOfRange => ToolError::new("INVALID_ARGUMENT", message),
        Code::Cancelled => ToolError::internal("the call was cancelled"),
        Code::DeadlineExceeded => ToolError::internal("the call timed out"),
        _ => ToolError::internal(message),
    }
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_ERROR_TEXT).collect()
}

fn encode_msg(m: &DynamicMessage) -> Result<Value, ToolError> {
    json::encode(m).map_err(|e| ToolError::internal(format!("response cannot be shown: {e}")))
}

#[async_trait::async_trait]
impl McpBackend for RpcBackend {
    async fn call(&self, tool: &str, args: &Value, ctx: &CallContext) -> Result<Reply, ToolError> {
        let def = catalogue::find(tool, self.surface).ok_or_else(|| ToolError::not_available(format!("{tool} is not served here")))?;
        // Only read tools reach the wire: writes are M32/B22; `search` (feature-gated) is served by
        // `search::SearchTool` over a `CodeIntelClient`, wired by I21.
        if !def.ann.read_only || def.needs != Needs::None || def.requires_feature.is_some() {
            return Err(ToolError::not_available(format!("{tool} is not served over RPC")));
        }
        let method = self
            .resolve(def.rpc)
            .ok_or_else(|| ToolError::not_available(format!("{tool} is not served over RPC")))?;
        let (channel, credential) = self.route(method.parent_service().name())?;
        let request = self.build(def, &method, args, ctx)?;

        let mut req = Request::new(request);
        req.set_timeout(CALL_TIMEOUT);
        let meta = req.metadata_mut();
        if let Some(c) = credential
            && let Ok(mut v) = c.header_value().parse::<MetadataValue<Ascii>>()
        {
            v.set_sensitive(true);
            meta.insert("authorization", v);
        }
        if let Ok(v) = format!("asc/{} mcp", env!("CARGO_PKG_VERSION")).parse::<MetadataValue<Ascii>>() {
            meta.insert("x-asc-client", v);
        }

        let path: PathAndQuery = format!("/{}/{}", method.parent_service().full_name(), method.name())
            .parse()
            .map_err(|_| ToolError::internal("bad method path"))?;
        let mut grpc = Grpc::new(channel).max_decoding_message_size(MAX_MESSAGE).max_encoding_message_size(MAX_MESSAGE);
        let codec = DynCodec::new(method.output());
        let started = async {
            grpc.ready().await.map_err(|e| ToolError::not_available(clean(&e.to_string())))?;
            if method.is_server_streaming() {
                let r = grpc.server_streaming(req, path, codec).await.map_err(|s| map_status(&s))?;
                Ok::<_, ToolError>(r.into_inner().boxed())
            } else {
                let r = grpc.unary(req, path, codec).await.map_err(|s| map_status(&s))?;
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
        let messages = raw
            .skip(skip)
            .map(|r| r.map_err(|s| map_status(&s)).and_then(|m| encode_msg(&m)))
            // Hand over the message that spends the budget, then end (and drop) the RPC.
            .scan((0u64, false), move |(used, done), item| {
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

    async fn worktrees(&self) -> Result<Vec<PathBuf>, ToolError> {
        let Some(local) = &self.local else { return Ok(Vec::new()) };
        let method = self
            .method("Workspace.Worktrees")
            .ok_or_else(|| ToolError::internal("Workspace.Worktrees is missing"))?;
        let mut grpc = Grpc::new(local.channel.clone());
        grpc.ready().await.map_err(|e| ToolError::not_available(clean(&e.to_string())))?;
        let path: PathAndQuery = "/asc.v1.Workspace/Worktrees".parse().map_err(|_| ToolError::internal("bad method path"))?;
        let mut req = Request::new(DynamicMessage::new(method.input()));
        req.set_timeout(Duration::from_secs(10));
        let resp = grpc
            .unary(req, path, DynCodec::new(method.output()))
            .await
            .map_err(|s| map_status(&s))?
            .into_inner();
        let v = encode_msg(&resp)?;
        Ok(v.get("worktrees")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|w| w.get("path").and_then(Value::as_str)).map(PathBuf::from).collect())
            .unwrap_or_default())
    }
}
