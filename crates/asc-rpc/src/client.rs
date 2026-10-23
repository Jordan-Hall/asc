//! The `asc.v1.Repo` gRPC client (SPEC §7).
//!
//! [`RpcClient`] speaks for one repository on one server. It implements the engine's
//! [`HistoryApi`], [`BlameApi`] and [`Remote`], and [`RemoteObjects`] is the `Objects` view of the
//! same connection. Everything the server sends is untrusted: sizes and counts are bounded, ids
//! must be well formed, objects must be ones the client asked for, and error text is stripped of
//! control characters before it can reach a terminal.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::transport_policy::{Policy, Transport as Wire, TransportError, require_network};
use asc_engine::blame::{self, Blame, BlameApi};
use asc_engine::derive::{self, DerivedKeyed};
use asc_engine::graph::{self, LogQuery};
use asc_engine::history::{GitMapping, HistoryApi, LOG_PAGE, LogEntries, LogEntry, PageToken};
use asc_engine::sync::{self, GET_BATCH, GET_STREAMS, HAS_BATCH, PUT_BATCH_BYTES, Remote};
use asc_store::codec::check_codec;
use asc_store::key::KeyPrefix;
use asc_store::key::{KeyClass, ObjKey};
use asc_store::objects::{self, Objects, decode_verified, slice_range};
use asc_store::refs::{Expect, RefConflict, RefEntry, RefError, RefUpdate};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use tonic::codec::CompressionEncoding;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Uri};
use tonic::{Code, Request, Status, Streaming};
use uuid::Uuid;

use crate::auth::Credential;
use crate::convert::{self, ConvertError, IdMode, WireId};
use crate::errors::RpcError;
use crate::pb;
use crate::server::{MAX_MESSAGE, MAX_RESPONSE_BYTES, RepoName, status_rpc_error};
use crate::services::repo_client::RepoClient;

/// Objects per `PutObjects` request message.
const PUT_BATCH_OBJECTS: usize = 1024;
/// Tries for one upload; a retry first asks which objects the server already holds.
const PUT_ATTEMPTS: u32 = 3;
/// Most `ListRefs` pages one listing follows.
const MAX_LIST_PAGES: usize = 10_000;
/// Most blame runs accepted for one file.
const MAX_BLAME_RUNS: usize = 2_000_000;
/// Most changes accepted for one diff.
const MAX_DIFF_CHANGES: usize = 1_000_000;
/// Longest server-supplied error text shown.
const MAX_ERROR_TEXT: usize = 512;

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The server answered with an `asc.v1.ErrorDetail`.
    #[error("{0}")]
    Rpc(RpcError),
    /// A plain gRPC failure.
    #[error("{code:?}: {message}")]
    Status { code: Code, message: String },
    /// The server broke the protocol.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// The request cannot be sent.
    #[error("invalid request: {0}")]
    Invalid(String),
}

/// Keeps printable characters only, and bounds the length.
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_TEXT)
        .collect()
}

impl CallError {
    fn from_status(s: Status) -> Self {
        match status_rpc_error(&s) {
            Some(e) => Self::Rpc(e),
            None => Self::Status {
                code: s.code(),
                message: clean(s.message()),
            },
        }
    }

    /// True when the server could not be reached (or went away).
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Status {
                code: Code::Unavailable,
                ..
            }
        )
    }

    fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Status {
                code: Code::NotFound,
                ..
            }
        )
    }
}

impl From<ConvertError> for CallError {
    fn from(e: ConvertError) -> Self {
        Self::Protocol(e.to_string())
    }
}

impl From<Status> for CallError {
    fn from(s: Status) -> Self {
        Self::from_status(s)
    }
}

/// Why a client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid server url {0:?}")]
    InvalidUrl(String),
    /// The URL is not one asc may use (plain http, an unknown scheme).
    #[error(transparent)]
    Refused(#[from] TransportError),
    #[error("invalid repository name")]
    InvalidRepo,
    #[error("transport: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// How an `UpdateRefs` call may move a ref (the `UpdateMode` of the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushMode {
    /// The server refuses a move that is not a fast-forward.
    FastForward,
    /// The expectation alone decides.
    Force,
}

/// What `Version` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerVersion {
    pub server_version: String,
    pub min_client: String,
    pub features: Vec<String>,
}

/// The server's `Remote`: a repository reached over gRPC.
#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}

struct Inner {
    primary: Channel,
    /// Object tiers, tried in order for `HasObjects`/`GetObjects` (SPEC §7).
    tiers: Vec<Channel>,
    repo: RepoName,
    remote_name: String,
    credential: Option<Credential>,
    user_agent: String,
}

/// A lazily connected channel. `https` uses the system and bundled roots. Plain `http` is
/// refused unless the transport policy's loopback opt-in is set, and then only to the local
/// machine, because it would send the token in the clear (U89).
pub fn connect_channel(url: &str) -> Result<Channel, ClientError> {
    channel_with(url, Policy::current())
}

/// [`connect_channel`] under an explicit policy.
pub fn channel_with(url: &str, policy: Policy) -> Result<Channel, ClientError> {
    let uri: Uri = url
        .parse()
        .map_err(|_| ClientError::InvalidUrl(redacted(url)))?;
    if uri.host().is_none() {
        return Err(ClientError::InvalidUrl(redacted(url)));
    }
    let transport = require_network(url, policy)?;
    let mut endpoint = Endpoint::from_shared(url.to_owned())?;
    match (transport, uri.scheme_str()) {
        (Wire::Https, Some("https")) => {
            endpoint = endpoint.tls_config(ClientTlsConfig::new().with_enabled_roots())?
        }
        (Wire::LoopbackHttp, Some("http")) => {}
        _ => return Err(ClientError::InvalidUrl(redacted(url))),
    }
    Ok(endpoint
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_while_idle(true)
        .connect_lazy())
}

/// The URL without userinfo, path, query or fragment, for an error message.
fn redacted(url: &str) -> String {
    match url.parse::<Uri>() {
        Ok(u) => match (u.scheme_str(), u.host()) {
            (Some(scheme), Some(host)) => format!("{scheme}://{host}"),
            _ => "<unparsable>".to_owned(),
        },
        Err(_) => "<unparsable>".to_owned(),
    }
}

impl RpcClient {
    /// A client for `repo` (`owner/name`) on the server at `url`. `credential` is the resolved
    /// credential (SPEC §15.6.1), sent as the `Authorization` header of its own scheme; a
    /// `BearerToken` converts into a Bearer one.
    pub fn connect<C: Into<Credential>>(
        url: &str,
        repo: &str,
        credential: Option<C>,
    ) -> Result<Self, ClientError> {
        Self::from_channel(connect_channel(url)?, repo, credential)
    }

    /// A client over an existing channel.
    pub fn from_channel<C: Into<Credential>>(
        primary: Channel,
        repo: &str,
        credential: Option<C>,
    ) -> Result<Self, ClientError> {
        let repo = RepoName::parse(repo).map_err(|_| ClientError::InvalidRepo)?;
        Ok(Self {
            inner: Arc::new(Inner {
                primary,
                tiers: Vec::new(),
                remote_name: repo.to_string(),
                repo,
                credential: credential.map(Into::into),
                user_agent: format!(
                    "asc/{} {}-{}",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            }),
        })
    }

    fn rebuilt(&self, edit: impl FnOnce(&mut Inner)) -> Self {
        let i = &self.inner;
        let mut inner = Inner {
            primary: i.primary.clone(),
            tiers: i.tiers.clone(),
            repo: i.repo.clone(),
            remote_name: i.remote_name.clone(),
            credential: i.credential.clone(),
            user_agent: i.user_agent.clone(),
        };
        edit(&mut inner);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Sends `HasObjects` and `GetObjects` to the first reachable of `urls`, then to the primary
    /// server (`/.well-known/asc` `objects`, SPEC §7). Everything else still goes to the primary.
    pub fn with_object_tiers(&self, urls: &[&str]) -> Result<Self, ClientError> {
        let tiers = urls
            .iter()
            .map(|u| connect_channel(u))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.rebuilt(|i| i.tiers = tiers))
    }

    /// The name used in messages and remote-tracking refs (default: the repository name).
    #[must_use]
    pub fn with_remote_name(&self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.rebuilt(|i| i.remote_name = name)
    }

    /// The `Objects` view of this connection.
    pub fn objects(&self) -> RemoteObjects {
        RemoteObjects(self.clone())
    }

    pub(crate) fn repo_ref(&self) -> Option<pb::RepoRef> {
        Some(pb::RepoRef {
            name: self.inner.repo.to_string(),
            id_mode: IdMode::Asc.to_wire(),
        })
    }

    fn stub(channel: &Channel) -> RepoClient<Channel> {
        RepoClient::new(channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(CompressionEncoding::Zstd)
    }

    /// The channel of the primary server, for the other services of `asc.v1`.
    pub(crate) fn primary_channel(&self) -> &Channel {
        &self.inner.primary
    }

    pub(crate) fn primary(&self) -> RepoClient<Channel> {
        Self::stub(&self.inner.primary)
    }

    /// Wraps `msg` with the common metadata (SPEC §7): token, client version and, for mutating
    /// calls, the operation id.
    pub(crate) fn req<T>(&self, msg: T, op_id: Option<Uuid>) -> Request<T> {
        let mut r = Request::new(msg);
        let meta = r.metadata_mut();
        if let Some(c) = &self.inner.credential
            && let Ok(mut v) = c.header_value().parse::<MetadataValue<Ascii>>()
        {
            v.set_sensitive(true);
            meta.insert("authorization", v);
        }
        if let Ok(v) = self.inner.user_agent.parse::<MetadataValue<Ascii>>() {
            meta.insert("x-asc-client", v);
        }
        if let Some(id) = op_id
            && let Ok(v) = id.to_string().parse::<MetadataValue<Ascii>>()
        {
            meta.insert("x-asc-op-id", v);
        }
        r
    }

    /// Tiers first, then the primary.
    fn object_channels(&self) -> impl Iterator<Item = &Channel> {
        self.inner
            .tiers
            .iter()
            .chain(std::iter::once(&self.inner.primary))
    }

    fn offline(&self, e: &CallError) -> bool {
        e.is_unavailable()
    }

    pub(crate) fn graph_err(
        &self,
        e: CallError,
        operation: &'static str,
        rpc: &'static str,
    ) -> graph::Error {
        if self.offline(&e) {
            graph::Error::Offline {
                remote: self.inner.remote_name.clone(),
                operation,
                rpc,
            }
        } else {
            graph::Error::Remote(e.to_string())
        }
    }

    fn sync_err(&self, e: CallError, operation: &'static str, rpc: &'static str) -> sync::Error {
        sync::Error::Graph(self.graph_err(e, operation, rpc))
    }

    /// Asks the server for its version and requirements.
    pub async fn server_version(&self) -> Result<ServerVersion, CallError> {
        let r = self
            .primary()
            .version(self.req(pb::VersionRequest {}, None))
            .await?
            .into_inner();
        Ok(ServerVersion {
            server_version: clean(&r.server_version),
            min_client: clean(&r.min_client),
            features: r.features.iter().take(64).map(|f| clean(f)).collect(),
        })
    }

    // ---- objects ---------------------------------------------------------------------

    /// Presence of each key, asked of the object tiers in order.
    pub async fn has_objects(&self, keys: &[ObjKey]) -> Result<Vec<bool>, CallError> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(HAS_BATCH) {
            out.extend(self.has_page(chunk).await?);
        }
        Ok(out)
    }

    async fn has_page(&self, keys: &[ObjKey]) -> Result<Vec<bool>, CallError> {
        let msg = pb::HasObjectsRequest {
            repo: self.repo_ref(),
            keys: keys
                .iter()
                .map(pb::ObjKey::try_from)
                .collect::<Result<_, _>>()?,
        };
        let mut last = None;
        for ch in self.object_channels() {
            match Self::stub(ch)
                .has_objects(self.req(msg.clone(), None))
                .await
            {
                Ok(r) => {
                    let present = r.into_inner().present;
                    if present.len() != keys.len().div_ceil(8) {
                        return Err(CallError::Protocol(
                            "presence bitmap has the wrong size".into(),
                        ));
                    }
                    return Ok((0..keys.len())
                        .map(|i| present[i / 8] & (1 << (i % 8)) != 0)
                        .collect());
                }
                Err(s) if s.code() == Code::Unavailable => last = Some(s),
                Err(s) => return Err(s.into()),
            }
        }
        Err(last.map_or_else(|| CallError::Protocol("no object tier".into()), Into::into))
    }

    /// Opens one `GetObjects` stream on the first reachable tier.
    async fn open_get(
        &self,
        messages: Vec<pb::GetObjectsRequest>,
    ) -> Result<Streaming<pb::GetObjectsResponse>, CallError> {
        let mut last = None;
        for ch in self.object_channels() {
            let req = self.req(stream::iter(messages.clone()), None);
            match Self::stub(ch).get_objects(req).await {
                Ok(r) => return Ok(r.into_inner()),
                Err(s) if s.code() == Code::Unavailable => last = Some(s),
                Err(s) => return Err(s.into()),
            }
        }
        Err(last.map_or_else(|| CallError::Protocol("no object tier".into()), Into::into))
    }

    /// One `GetObjects` stream over `batches`, yielding the stored bytes of each response.
    fn lane(
        &self,
        batches: Vec<Vec<ObjKey>>,
    ) -> BoxStream<'static, Result<Vec<(ObjKey, Bytes)>, CallError>> {
        enum Lane {
            Pending(RpcClient, Vec<Vec<ObjKey>>),
            Open(Box<Streaming<pb::GetObjectsResponse>>, HashSet<ObjKey>),
        }
        stream::try_unfold(Lane::Pending(self.clone(), batches), |state| async move {
            let (mut responses, mut wanted) = match state {
                Lane::Open(r, w) => (*r, w),
                Lane::Pending(client, batches) => {
                    let wanted: HashSet<ObjKey> = batches.iter().flatten().copied().collect();
                    let mut messages = Vec::with_capacity(batches.len());
                    for b in &batches {
                        messages.push(pb::GetObjectsRequest {
                            repo: client.repo_ref(),
                            keys: b
                                .iter()
                                .map(pb::ObjKey::try_from)
                                .collect::<Result<_, _>>()?,
                        });
                    }
                    (client.open_get(messages).await?, wanted)
                }
            };
            let Some(resp) = responses.message().await? else {
                return Ok(None);
            };
            let mut out = Vec::with_capacity(resp.objects.len());
            for o in resp.objects {
                let key = o
                    .key
                    .as_ref()
                    .ok_or_else(|| CallError::Protocol("object without a key".into()))?;
                let key = ObjKey::try_from(key)?;
                if !wanted.remove(&key) {
                    return Err(CallError::Protocol(format!(
                        "unrequested or repeated object {key}"
                    )));
                }
                check_codec(&o.data).map_err(|e| CallError::Protocol(format!("{key}: {e}")))?;
                out.push((key, o.data));
            }
            for m in &resp.missing {
                if !wanted.remove(&ObjKey::try_from(m)?) {
                    return Err(CallError::Protocol(
                        "unrequested object reported missing".into(),
                    ));
                }
            }
            Ok(Some((out, Lane::Open(Box::new(responses), wanted))))
        })
        .boxed()
    }

    /// Stored bytes of the keys that exist, over up to eight pipelined streams.
    pub fn fetch(
        &self,
        keys: Vec<ObjKey>,
    ) -> BoxStream<'static, Result<(ObjKey, Bytes), CallError>> {
        let streams = GET_STREAMS.min(keys.len().div_ceil(GET_BATCH));
        let mut lanes: Vec<Vec<Vec<ObjKey>>> = vec![Vec::new(); streams];
        for (i, batch) in keys.chunks(GET_BATCH).enumerate() {
            lanes[i % streams].push(batch.to_vec());
        }
        stream::select_all(lanes.into_iter().map(|l| self.lane(l)))
            .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
            .try_flatten()
            .boxed()
    }

    /// Uploads stored-bytes objects. A failed upload is retried after asking which objects the
    /// server already has, so an interrupted push resumes where it stopped.
    pub async fn put_objects(&self, batch: Vec<(ObjKey, Bytes)>) -> Result<(), CallError> {
        let mut remaining = batch;
        for attempt in 1..=PUT_ATTEMPTS {
            let err = match self.put_once(&remaining).await {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            let retry = attempt < PUT_ATTEMPTS
                && matches!(&err, CallError::Status { code, .. }
                if matches!(code, Code::Unavailable | Code::DeadlineExceeded | Code::Aborted));
            if !retry {
                return Err(err);
            }
            tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
            let keys: Vec<ObjKey> = remaining.iter().map(|(k, _)| *k).collect();
            if let Ok(have) = self.has_primary(&keys).await {
                remaining = remaining
                    .into_iter()
                    .zip(have)
                    .filter_map(|(o, h)| (!h).then_some(o))
                    .collect();
            }
        }
        Ok(())
    }

    async fn has_primary(&self, keys: &[ObjKey]) -> Result<Vec<bool>, CallError> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(HAS_BATCH) {
            let msg = pb::HasObjectsRequest {
                repo: self.repo_ref(),
                keys: chunk
                    .iter()
                    .map(pb::ObjKey::try_from)
                    .collect::<Result<_, _>>()?,
            };
            let present = self
                .primary()
                .has_objects(self.req(msg, None))
                .await?
                .into_inner()
                .present;
            if present.len() != chunk.len().div_ceil(8) {
                return Err(CallError::Protocol(
                    "presence bitmap has the wrong size".into(),
                ));
            }
            out.extend((0..chunk.len()).map(|i| present[i / 8] & (1 << (i % 8)) != 0));
        }
        Ok(out)
    }

    async fn put_once(&self, batch: &[(ObjKey, Bytes)]) -> Result<(), CallError> {
        let mut messages = Vec::new();
        let mut current = Vec::new();
        let mut bytes = 0usize;
        let mut flush = |current: &mut Vec<pb::Object>, bytes: &mut usize| {
            if !current.is_empty() {
                messages.push(pb::PutObjectsRequest {
                    repo: self.repo_ref(),
                    objects: std::mem::take(current),
                });
                *bytes = 0;
            }
        };
        for (key, data) in batch {
            if data.len() > MAX_RESPONSE_BYTES {
                return Err(CallError::Invalid(format!(
                    "{key} is {} bytes, more than one 16 MiB message holds",
                    data.len()
                )));
            }
            if bytes + data.len() > PUT_BATCH_BYTES || current.len() >= PUT_BATCH_OBJECTS {
                flush(&mut current, &mut bytes);
            }
            current.push(pb::Object {
                key: Some(pb::ObjKey::try_from(key)?),
                data: data.clone(),
            });
            bytes += data.len();
        }
        flush(&mut current, &mut bytes);
        if messages.is_empty() {
            return Ok(());
        }
        self.primary()
            .put_objects(self.req(stream::iter(messages), None))
            .await?;
        Ok(())
    }

    // ---- refs ------------------------------------------------------------------------

    /// Every ref, following the server's pages.
    pub async fn list_refs_all(&self) -> Result<Vec<RefEntry>, CallError> {
        let mut out = Vec::new();
        let mut after = String::new();
        for _ in 0..MAX_LIST_PAGES {
            let req = pb::ListRefsRequest {
                repo: self.repo_ref(),
                after: after.clone(),
                ..pb::ListRefsRequest::default()
            };
            let page = self
                .primary()
                .list_refs(self.req(req, None))
                .await?
                .into_inner();
            for r in &page.refs {
                let target = r
                    .target
                    .as_ref()
                    .ok_or_else(|| CallError::Protocol("ref without a target".into()))?;
                out.push(RefEntry {
                    name: RefName::new_onelevel(r.name.as_bytes())
                        .map_err(|e| CallError::Protocol(e.to_string()))?,
                    target: RefTarget::try_from(target)?,
                    version: r.version,
                    expires_at: (r.expires_at > 0).then_some(r.expires_at),
                });
            }
            if page.next.is_empty() {
                return Ok(out);
            }
            if page.next == after {
                return Err(CallError::Protocol(
                    "ref page token does not advance".into(),
                ));
            }
            after = page.next;
        }
        Err(CallError::Protocol("too many ref pages".into()))
    }

    /// Applies `updates` atomically under `op_id` (reuse it to retry the same logical call).
    pub async fn update_refs_with(
        &self,
        updates: &[RefUpdate],
        mode: PushMode,
        op_id: Uuid,
    ) -> Result<(), CallError> {
        let mut ttl = None;
        let mut wire = Vec::with_capacity(updates.len());
        for u in updates {
            let name = String::from_utf8(u.name.as_bytes().to_vec())
                .map_err(|_| CallError::Invalid("ref name is not UTF-8".into()))?;
            if u.new.is_some() {
                match (ttl, u.expires_at) {
                    (None, e) => ttl = Some(e),
                    (Some(prev), e) if prev != e => {
                        return Err(CallError::Invalid(
                            "updates with different expiries need separate calls".into(),
                        ));
                    }
                    _ => {}
                }
            }
            wire.push(pb::RefUpdate {
                name,
                expect: Some(match &u.expect {
                    Expect::Any => pb::ref_update::Expect::Any(true),
                    Expect::Absent => pb::ref_update::Expect::Absent(true),
                    Expect::Value(t) => pb::ref_update::Expect::Value(pb::RefTarget::from(t)),
                }),
                new: u.new.as_ref().map(pb::RefTarget::from),
                delete: u.new.is_none(),
            });
        }
        let ttl_seconds = match ttl.flatten() {
            None => 0,
            Some(at) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
                (at - now).max(1)
            }
        };
        let req = pb::UpdateRefsRequest {
            repo: self.repo_ref(),
            updates: wire,
            mode: match mode {
                PushMode::FastForward => pb::UpdateMode::FastForward,
                PushMode::Force => pb::UpdateMode::Force,
            } as i32,
            ttl_seconds,
            ..pb::UpdateRefsRequest::default()
        };
        self.primary()
            .update_refs(self.req(req, Some(op_id)))
            .await?;
        Ok(())
    }
}

fn utf8(b: &[u8], what: &str) -> Result<String, CallError> {
    String::from_utf8(b.to_vec())
        .map_err(|_| CallError::Invalid(format!("{what} is not valid UTF-8")))
}

fn changeset_id(b: &[u8]) -> Result<ChangesetId, CallError> {
    Ok(ChangesetId::from_slice(b)?)
}

// ---------------------------------------------------------------------------------- Remote

#[async_trait]
impl Remote for RpcClient {
    fn name(&self) -> &str {
        &self.inner.remote_name
    }

    async fn list_refs(&self) -> sync::Result<Vec<RefEntry>> {
        self.list_refs_all()
            .await
            .map_err(|e| self.sync_err(e, "list refs", "ListRefs"))
    }

    async fn has_many(&self, keys: &[ObjKey]) -> sync::Result<Vec<bool>> {
        self.has_objects(keys)
            .await
            .map_err(|e| self.sync_err(e, "check objects", "HasObjects"))
    }

    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, sync::Result<(ObjKey, Bytes)>> {
        let client = self.clone();
        self.fetch(keys)
            .map_err(move |e| client.sync_err(e, "fetch", "GetObjects"))
            .boxed()
    }

    async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> sync::Result<()> {
        self.put_objects(objects)
            .await
            .map_err(|e| self.sync_err(e, "push", "PutObjects"))
    }

    /// Moves refs with `UPDATE_MODE_FORCE`: the caller already decided what may move, and each
    /// update's expectation guards against a concurrent change.
    async fn update_refs(&self, updates: &[RefUpdate]) -> sync::Result<()> {
        match self
            .update_refs_with(updates, PushMode::Force, Uuid::now_v7())
            .await
        {
            Ok(()) => Ok(()),
            Err(CallError::Rpc(RpcError::RefConflict(info))) => {
                let name = info
                    .paths
                    .first()
                    .and_then(|p| RefName::new_onelevel(p.as_bytes()).ok())
                    .ok_or_else(|| sync::Error::Remote("ref conflict without a name".into()))?;
                let actual = if info.actual.is_empty() {
                    None
                } else {
                    Some(
                        RefTarget::from_bytes(&info.actual)
                            .map_err(|e| sync::Error::Remote(e.to_string()))?,
                    )
                };
                Err(sync::Error::Refs(RefError::Conflict(RefConflict {
                    name,
                    actual,
                })))
            }
            Err(CallError::Rpc(RpcError::NotFastForward(info))) => {
                let name = info
                    .paths
                    .first()
                    .and_then(|p| RefName::new_onelevel(p.as_bytes()).ok())
                    .ok_or_else(|| sync::Error::Remote("rejected update without a name".into()))?;
                Err(sync::Error::Rejected {
                    name,
                    reason: "non-fast-forward",
                })
            }
            Err(e) => Err(self.sync_err(e, "update refs", "UpdateRefs")),
        }
    }

    async fn ingest(&self, tips: &[ChangesetId]) -> sync::Result<()> {
        let req = pb::IngestRequest {
            repo: self.repo_ref(),
            changesets: tips
                .iter()
                .map(|t| Bytes::copy_from_slice(t.as_bytes()))
                .collect(),
        };
        self.primary()
            .ingest(self.req(req, Some(Uuid::now_v7())))
            .await
            .map_err(|s| self.sync_err(s.into(), "ingest", "Ingest"))?;
        Ok(())
    }

    fn history(&self) -> Option<Arc<dyn HistoryApi>> {
        Some(Arc::new(self.clone()))
    }
}

// -------------------------------------------------------------------------------- HistoryApi

impl RpcClient {
    async fn log_page(
        &self,
        q: &LogQuery,
        page: Option<&PageToken>,
    ) -> Result<LogEntries, CallError> {
        if q.cursor.is_some() {
            return Err(CallError::Invalid(
                "log cursors do not cross the wire; pass a page token".into(),
            ));
        }
        if q.limit == Some(0) {
            return Ok(LogEntries::default());
        }
        let number =
            |n: usize| u32::try_from(n).map_err(|_| CallError::Invalid("number too large".into()));
        let req = pb::LogRequest {
            repo: self.repo_ref(),
            include: q.include.iter().map(ToString::to_string).collect(),
            exclude: q.exclude.iter().map(ToString::to_string).collect(),
            paths: q
                .paths
                .iter()
                .map(|p| utf8(p.as_bytes(), "path"))
                .collect::<Result<_, _>>()?,
            first_parent: q.first_parent,
            follow: q.follow,
            skip: number(q.skip)?,
            limit: q
                .limit
                .map_or(Ok(0), |l| number(l.min(u32::MAX as usize)))?,
            author: q
                .author
                .as_deref()
                .map(|a| utf8(a, "author filter"))
                .transpose()?
                .unwrap_or_default(),
            grep: q.grep.clone().unwrap_or_default(),
            since: q.since.unwrap_or(0),
            until: q.until.unwrap_or(0),
            page_token: page.map_or_else(String::new, |t| URL_SAFE_NO_PAD.encode(&t.0)),
            ..pb::LogRequest::default()
        };
        let mut responses = self.primary().log(self.req(req, None)).await?.into_inner();
        let first = responses
            .message()
            .await?
            .ok_or_else(|| CallError::Protocol("empty log stream".into()))?;
        // Later pages are fetched with the token; stop the server's walk here.
        drop(responses);
        if first.entries.len() > LOG_PAGE {
            return Err(CallError::Protocol("log page is too large".into()));
        }
        let mut entries = Vec::with_capacity(first.entries.len());
        for e in &first.entries {
            entries.push(LogEntry {
                id: changeset_id(&e.id)?,
                generation: e.r#gen,
                parents: e
                    .parents
                    .iter()
                    .map(|p| changeset_id(p))
                    .collect::<Result<_, _>>()?,
                git: convert::opt_id(&e.git)?,
            });
        }
        let next = if first.next_page_token.is_empty() {
            None
        } else if first.next_page_token.len() > 64 * 1024 {
            return Err(CallError::Protocol("page token is too long".into()));
        } else {
            Some(PageToken(
                URL_SAFE_NO_PAD
                    .decode(&first.next_page_token)
                    .map_err(|_| CallError::Protocol("malformed page token".into()))?,
            ))
        };
        Ok(LogEntries { entries, next })
    }

    async fn diff_changes(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        paths: &[String],
    ) -> Result<Vec<(RepoPath, Change)>, CallError> {
        let req = pb::DiffRequest {
            repo: self.repo_ref(),
            base: a.to_string(),
            head: b.to_string(),
            paths: paths.to_vec(),
            format: pb::DiffFormat::Raw as i32,
            ..pb::DiffRequest::default()
        };
        let mut responses = self.primary().diff(self.req(req, None)).await?.into_inner();
        let mut out = Vec::new();
        while let Some(resp) = responses.message().await? {
            if resp.truncated {
                return Err(CallError::Protocol("the server truncated the diff".into()));
            }
            for c in &resp.changes {
                if out.len() >= MAX_DIFF_CHANGES {
                    return Err(CallError::Protocol("diff has too many changes".into()));
                }
                out.push(leaf_change(&convert::PathChange::try_from(c)?)?);
            }
        }
        Ok(out)
    }
}

/// The leaf `Change` a wire path change describes.
fn leaf_change(c: &convert::PathChange) -> Result<(RepoPath, Change), CallError> {
    let bad = |what: &str| CallError::Protocol(format!("{}: {what}", c.path));
    let change = if c.status == "D" || c.new_mode == 0 {
        Change::Delete
    } else if c.new_mode == 0o160_000 {
        Change::Gitlink {
            commit: c.new_git.ok_or_else(|| bad("gitlink without a commit"))?,
        }
    } else {
        let kind = match c.new_mode {
            0o100_644 => Kind::File,
            0o100_755 => Kind::Exec,
            0o120_000 => Kind::Symlink,
            _ => return Err(bad("unsupported mode")),
        };
        let copy_from = if c.status == "C" {
            c.old_path.clone()
        } else {
            None
        };
        Change::write(
            kind,
            c.new_id.ok_or_else(|| bad("write without a file id"))?,
            c.new_size,
            c.new_git.ok_or_else(|| bad("write without a git oid"))?,
            copy_from,
        )
        .map_err(|e| bad(&e.to_string()))?
    };
    Ok((c.path.clone(), change))
}

#[async_trait]
impl HistoryApi for RpcClient {
    async fn log(&self, q: &LogQuery, page: Option<&PageToken>) -> graph::Result<LogEntries> {
        self.log_page(q, page)
            .await
            .map_err(|e| self.graph_err(e, "log", "Log"))
    }

    async fn merge_base(
        &self,
        a: ChangesetId,
        others: &[ChangesetId],
        all: bool,
    ) -> graph::Result<Vec<ChangesetId>> {
        let run = async {
            let req = pb::MergeBaseRequest {
                repo: self.repo_ref(),
                a: Bytes::copy_from_slice(a.as_bytes()),
                b: others
                    .iter()
                    .map(|o| Bytes::copy_from_slice(o.as_bytes()))
                    .collect(),
                all,
            };
            let r = self
                .primary()
                .merge_base(self.req(req, None))
                .await?
                .into_inner();
            if r.bases.len() > others.len().max(1) * 64 {
                return Err(CallError::Protocol("too many merge bases".into()));
            }
            r.bases.iter().map(|b| changeset_id(b)).collect()
        };
        run.await
            .map_err(|e| self.graph_err(e, "merge-base", "MergeBase"))
    }

    async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> graph::Result<bool> {
        let run = async {
            let req = pb::IsAncestorRequest {
                repo: self.repo_ref(),
                ancestor: Bytes::copy_from_slice(a.as_bytes()),
                descendant: Bytes::copy_from_slice(b.as_bytes()),
            };
            Ok(self
                .primary()
                .is_ancestor(self.req(req, None))
                .await?
                .into_inner()
                .result)
        };
        run.await
            .map_err(|e: CallError| self.graph_err(e, "is-ancestor", "IsAncestor"))
    }

    async fn diff(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        paths: &[String],
    ) -> graph::Result<Vec<(RepoPath, Change)>> {
        self.diff_changes(a, b, paths)
            .await
            .map_err(|e| self.graph_err(e, "diff", "Diff"))
    }

    async fn resolve(&self, specs: &[String]) -> graph::Result<Vec<Option<ChangesetId>>> {
        let run = async {
            let req = pb::ResolveRequest {
                repo: self.repo_ref(),
                revspecs: specs.to_vec(),
            };
            let r = self
                .primary()
                .resolve(self.req(req, None))
                .await?
                .into_inner();
            if r.ids.len() != specs.len() {
                return Err(CallError::Protocol(
                    "resolve answered the wrong number of ids".into(),
                ));
            }
            r.ids
                .iter()
                .map(|b| Ok(convert::opt_id::<ChangesetId>(b)?))
                .collect()
        };
        run.await
            .map_err(|e| self.graph_err(e, "resolve", "Resolve"))
    }

    async fn git_map(&self, asc: &[ChangesetId], git: &[GitOid]) -> graph::Result<GitMapping> {
        let run = async {
            let req = pb::GitMapRequest {
                repo: self.repo_ref(),
                asc: asc
                    .iter()
                    .map(|c| Bytes::copy_from_slice(c.as_bytes()))
                    .collect(),
                git: git
                    .iter()
                    .map(|g| Bytes::copy_from_slice(g.as_bytes()))
                    .collect(),
            };
            let r = self
                .primary()
                .git_map(self.req(req, None))
                .await?
                .into_inner();
            if r.asc_to_git.len() != asc.len() || r.git_to_asc.len() != git.len() {
                return Err(CallError::Protocol(
                    "git map answered the wrong number of ids".into(),
                ));
            }
            Ok(GitMapping {
                asc_to_git: r
                    .asc_to_git
                    .iter()
                    .map(|b| convert::opt_id::<GitOid>(b))
                    .collect::<Result<_, _>>()?,
                git_to_asc: r
                    .git_to_asc
                    .iter()
                    .map(|b| convert::opt_id::<ChangesetId>(b))
                    .collect::<Result<_, _>>()?,
            })
        };
        run.await
            .map_err(|e: CallError| self.graph_err(e, "git map", "GitMap"))
    }
}

// ---------------------------------------------------------------------------------- BlameApi

/// A store failure that came from the remote end.
fn remote_store_error(message: impl Into<String>) -> objects::Error {
    objects::Error::Store(object_store::Error::Generic {
        store: "asc-rpc",
        source: Box::new(std::io::Error::other(message.into())),
    })
}

impl RpcClient {
    async fn blame_runs(&self, rev: ChangesetId, path: &RepoPath) -> Result<Blame, CallError> {
        let req = pb::BlameRequest {
            repo: self.repo_ref(),
            rev: rev.to_string(),
            path: Bytes::copy_from_slice(path.as_bytes()),
            ..pb::BlameRequest::default()
        };
        let mut responses = self
            .primary()
            .blame(self.req(req, None))
            .await?
            .into_inner();
        let mut runs = 0u32;
        let mut body = Vec::new();
        while let Some(resp) = responses.message().await? {
            for h in &resp.hunks {
                runs += 1;
                if runs as usize > MAX_BLAME_RUNS {
                    return Err(CallError::Protocol("blame has too many runs".into()));
                }
                let origin = changeset_id(&h.origin)?;
                let plen = u32::try_from(h.orig_path.len())
                    .map_err(|_| CallError::Protocol("path too long".into()))?;
                body.extend(h.start.to_le_bytes());
                body.extend(h.len.to_le_bytes());
                body.extend(h.orig_start.to_le_bytes());
                body.extend(origin.as_bytes());
                body.extend(plen.to_le_bytes());
                body.extend(&h.orig_path);
            }
        }
        let mut encoded = runs.to_le_bytes().to_vec();
        encoded.extend(body);
        // The decoder rejects runs that do not tile the file from line 0 and bad paths.
        <Blame as DerivedKeyed>::decode(&encoded)
            .map_err(|_| CallError::Protocol("blame runs do not tile the file".into()))
    }
}

#[async_trait]
impl BlameApi for RpcClient {
    /// Fetches the whole file's blame and slices `lines` here, so a range never needs a
    /// different wire form.
    async fn blame(
        &self,
        rev: ChangesetId,
        path: &RepoPath,
        lines: Option<Range<u32>>,
    ) -> blame::Result<Blame> {
        let whole = self.blame_runs(rev, path).await.map_err(|e| {
            if e.is_not_found() {
                blame::Error::NotFound(path.clone())
            } else {
                blame::Error::Derive(derive::Error::Store(remote_store_error(e.to_string())))
            }
        })?;
        match lines {
            Some(range) => whole.slice(range),
            None => Ok(whole),
        }
    }
}

// ------------------------------------------------------------------------------ RemoteObjects

/// `Objects` over `GetObjects`/`PutObjects`/`HasObjects` (SPEC §3.3).
///
/// Reads verify the bytes against the key. `put` verifies before it uploads. There is no wire form
/// for `list` and `delete` (garbage collection runs on the server), and derived values never
/// travel: they read as absent and are not stored.
#[derive(Clone)]
pub struct RemoteObjects(RpcClient);

impl RemoteObjects {
    fn fail(e: CallError) -> objects::Error {
        remote_store_error(e.to_string())
    }
}

#[async_trait]
impl Objects for RemoteObjects {
    async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        match self.get_stored(key).await? {
            Some(stored) => decode_verified(key, &stored).map(Some),
            None => Ok(None),
        }
    }

    async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        if matches!(key, ObjKey::Derived { .. }) {
            return Ok(None);
        }
        let mut found = self.0.fetch(vec![*key]);
        match found.try_next().await.map_err(Self::fail)? {
            Some((k, stored)) if k == *key => Ok(Some(stored)),
            Some(_) => Err(remote_store_error("the server sent a different object")),
            None => Ok(None),
        }
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> objects::Result<Option<Bytes>> {
        if !matches!(key.class(), KeyClass::File | KeyClass::Chunk) {
            return Err(objects::Error::RangeUnsupported(*key));
        }
        Ok(self.get(key).await?.map(|d| slice_range(&d, r)))
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
        if matches!(key, ObjKey::Derived { .. }) {
            return Ok(());
        }
        decode_verified(key, &stored)?;
        self.0
            .put_objects(vec![(*key, stored)])
            .await
            .map_err(Self::fail)
    }

    async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
        let wire: Vec<usize> = (0..keys.len())
            .filter(|&i| !matches!(keys[i], ObjKey::Derived { .. }))
            .collect();
        let asked: Vec<ObjKey> = wire.iter().map(|&i| keys[i]).collect();
        let have = self.0.has_objects(&asked).await.map_err(Self::fail)?;
        let mut out = vec![false; keys.len()];
        for (i, h) in wire.into_iter().zip(have) {
            out[i] = h;
        }
        Ok(out)
    }

    async fn delete(&self, _key: &ObjKey) -> objects::Result<()> {
        Err(remote_store_error("delete is not available over RPC"))
    }

    fn list(
        &self,
        _prefix: KeyPrefix,
    ) -> BoxStream<'_, objects::Result<(ObjKey, std::time::SystemTime)>> {
        stream::once(async { Err(remote_store_error("list is not available over RPC")) }).boxed()
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    //! An in-process server over an `EngineBackend`, for the client and server tests.

    use crate::auth::BearerToken;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use asc_core::git::{ObjectKind, object_oid};
    use asc_core::id::FileId;
    use asc_core::sig::Signature;
    use asc_engine::Engine;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::refs::RepoKey;
    use asc_store::sqlrefs::SqlRefs;
    use sqlx::any::AnyPoolOptions;
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::TcpListenerStream;

    use super::*;
    use crate::auth::{Access, StaticTokens};
    use crate::server::{EngineBackend, RepoHandlers, RepoService, ServerInfo};

    pub const REPO: &str = "owner/repo";

    /// Objects that count decoded reads (`get`, `get_range`): serving must never decode.
    pub struct Counting {
        pub inner: BucketObjects,
        pub decodes: AtomicU64,
    }

    #[async_trait]
    impl Objects for Counting {
        async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
            self.decodes.fetch_add(1, Ordering::SeqCst);
            self.inner.get(key).await
        }
        async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
            self.inner.get_stored(key).await
        }
        async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> objects::Result<Option<Bytes>> {
            self.decodes.fetch_add(1, Ordering::SeqCst);
            self.inner.get_range(key, r).await
        }
        async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
            self.inner.put(key, stored).await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
            self.inner.has_many(keys).await
        }
        async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
            self.inner.delete(key).await
        }
        fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, objects::Result<(ObjKey, SystemTime)>> {
            self.inner.list(prefix)
        }
    }

    pub type Backend = EngineBackend<Counting, SqlRefs>;
    pub type Eng = Engine<Counting, SqlRefs>;

    pub struct Fixture {
        pub objects: Arc<Counting>,
        pub backend: Arc<Backend>,
        pub addr: SocketAddr,
        _dir: tempfile::TempDir,
        _shutdown: oneshot::Sender<()>,
    }

    async fn sqlite(dir: &Path) -> Arc<SqlRefs> {
        sqlx::any::install_default_drivers();
        let path = dir.join("refs.db").to_string_lossy().replace('\\', "/");
        let pool = AnyPoolOptions::new()
            .max_connections(4)
            .connect(&format!("sqlite:{path}?mode=rwc"))
            .await
            .unwrap();
        let refs = SqlRefs::new(pool);
        refs.migrate().await.unwrap();
        Arc::new(refs)
    }

    impl Fixture {
        pub async fn start() -> Self {
            Self::start_with(ServerInfo::default(), |b, h| b.register(h).unwrap()).await
        }

        /// `register` fills the handler groups; tokens: `rw` (write), `ro` (read), `adm`.
        pub async fn start_with(
            info: ServerInfo,
            register: impl FnOnce(&Arc<Backend>, &RepoHandlers),
        ) -> Self {
            // The fixture serves plain http on the loopback interface, which the transport
            // policy refuses unless asked (U89).
            asc_core::transport_policy::set_config_allow_loopback_http(true);
            let dir = tempfile::tempdir().unwrap();
            let refs = sqlite(dir.path()).await;
            let objects = Arc::new(Counting {
                inner: BucketObjects::memory(),
                decodes: AtomicU64::new(0),
            });
            let backend = Arc::new(EngineBackend::new(
                RepoName::parse(REPO).unwrap(),
                RepoKey(1),
                objects.clone(),
                refs,
            ));
            let handlers = Arc::new(RepoHandlers::new());
            register(&backend, &handlers);
            let auth = StaticTokens::new()
                .with_token("rw", "alice", Access::Write)
                .with_token("ro", "bob", Access::Read)
                .with_token("adm", "root", Access::Admin);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = oneshot::channel::<()>();
            let svc = RepoService::new(handlers, Arc::new(auth), info).into_service();
            tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(svc)
                    .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                        let _ = rx.await;
                    })
                    .await;
            });
            Self {
                objects,
                backend,
                addr,
                _dir: dir,
                _shutdown: tx,
            }
        }

        pub fn client(&self, token: Option<&str>) -> RpcClient {
            RpcClient::connect(
                &format!("http://{}", self.addr),
                REPO,
                token.map(|t| BearerToken::new(t).unwrap()),
            )
            .unwrap()
        }

        pub fn engine(&self) -> &Arc<Eng> {
            self.backend.engine()
        }
    }

    fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap()
    }

    /// A changeset on `parents` with `files` written over the first parent's tree.
    pub async fn commit(
        fx: &Fixture,
        parents: &[ChangesetId],
        files: &[(&str, &[u8])],
        message: &str,
    ) -> ChangesetId {
        let e = fx.engine();
        let base = match parents.first() {
            Some(&p) => e.get_changeset(p).await.unwrap().root,
            None => Eng::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in files {
            let file = FileId::of(content);
            fx.objects
                .put(&ObjKey::File(file), encode_stored(content))
                .await
                .unwrap();
            let git = object_oid(ObjectKind::Blob, content);
            changes.push((
                RepoPath::new(*path).unwrap(),
                Change::write(Kind::File, file, content.len() as u64, git, None).unwrap(),
            ));
        }
        let root = e.apply(base, &changes).await.unwrap();
        e.create_changeset(
            NewChangeset {
                parents: parents.to_vec(),
                author: sig(),
                committer: sig(),
                message: format!("{message}\n").into_bytes(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap()
    }

    pub struct History {
        pub c1: ChangesetId,
        pub c2: ChangesetId,
        pub c3: ChangesetId,
        pub b2: ChangesetId,
        pub merge: ChangesetId,
    }

    impl History {
        pub fn all(&self) -> [ChangesetId; 5] {
            [self.c1, self.c2, self.c3, self.b2, self.merge]
        }
    }

    /// `c1 <- c2 <- c3` and `c1 <- b2`, merged by `merge`.
    pub async fn history(fx: &Fixture) -> History {
        let c1 = commit(
            fx,
            &[],
            &[("f.txt", b"a\nb\nc\n"), ("d/g.txt", b"fixed\n")],
            "one",
        )
        .await;
        let c2 = commit(fx, &[c1], &[("f.txt", b"a\nB\nc\n")], "two").await;
        let c3 = commit(
            fx,
            &[c2],
            &[("f.txt", b"a\nB\nc\nd\n"), ("h.txt", b"h\n")],
            "three",
        )
        .await;
        let b2 = commit(fx, &[c1], &[("k.txt", b"k\n")], "branch").await;
        let merge = commit(fx, &[c3, b2], &[("k.txt", b"k\n")], "merge").await;
        History {
            c1,
            c2,
            c3,
            b2,
            merge,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use asc_core::id::FileId;
    use asc_core::object::Changeset;
    use asc_store::codec::encode_stored;
    use asc_store::refs::{Expect, RefUpdate};

    use super::testkit::{Fixture, History, REPO, history};
    use super::*;
    use crate::auth::BearerToken;
    use crate::server::{CallCtx, ObjectsHandler, RpcStream, ServerInfo};

    fn name(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }

    fn put_ref(n: &str, expect: Expect, to: Option<ChangesetId>) -> RefUpdate {
        RefUpdate {
            name: name(n),
            expect,
            new: to.map(RefTarget::Changeset),
            expires_at: None,
        }
    }

    async fn setup() -> (Fixture, History, RpcClient) {
        let fx = Fixture::start().await;
        let h = history(&fx).await;
        let client = fx.client(Some("rw"));
        (fx, h, client)
    }

    #[tokio::test]
    async fn history_matches_the_engine() {
        let (fx, h, rpc) = setup().await;
        let eng: &dyn HistoryApi = &**fx.engine();

        // Paged log, page by page, with every filter shape the wire carries.
        for limit in [None, Some(3)] {
            let q = LogQuery {
                include: vec![h.merge],
                limit,
                ..LogQuery::default()
            };
            let (mut a, mut b) = (None, None);
            loop {
                let want = eng.log(&q, a.as_ref()).await.unwrap();
                let got = HistoryApi::log(&rpc, &q, b.as_ref()).await.unwrap();
                assert_eq!(got.entries, want.entries);
                assert_eq!(got.next, want.next);
                (a, b) = (want.next, got.next);
                if a.is_none() {
                    break;
                }
            }
        }
        let q = LogQuery {
            include: vec![h.merge],
            exclude: vec![h.c1],
            first_parent: true,
            paths: vec![RepoPath::new("f.txt").unwrap()],
            ..LogQuery::default()
        };
        assert_eq!(
            HistoryApi::log(&rpc, &q, None).await.unwrap().entries,
            eng.log(&q, None).await.unwrap().entries
        );

        for all in [false, true] {
            assert_eq!(
                HistoryApi::merge_base(&rpc, h.c3, &[h.b2], all)
                    .await
                    .unwrap(),
                eng.merge_base(h.c3, &[h.b2], all).await.unwrap()
            );
        }
        for a in h.all() {
            for b in h.all() {
                assert_eq!(
                    HistoryApi::is_ancestor(&rpc, a, b).await.unwrap(),
                    eng.is_ancestor(a, b).await.unwrap(),
                    "{a} {b}"
                );
            }
        }
        let unknown = ChangesetId::from_bytes([9; 32]);
        assert!(!HistoryApi::is_ancestor(&rpc, unknown, h.c3).await.unwrap());

        let specs = vec![h.c3.to_string(), "nonsense".to_owned()];
        assert_eq!(
            HistoryApi::resolve(&rpc, &specs).await.unwrap(),
            eng.resolve(&specs).await.unwrap()
        );
        // A full id that names no changeset is an error on both sides, not a silent `None`.
        let ghost = [unknown.to_string()];
        assert!(eng.resolve(&ghost).await.is_err());
        assert!(HistoryApi::resolve(&rpc, &ghost).await.is_err());
        let ids = [h.c1, h.merge, unknown];
        let got = HistoryApi::git_map(&rpc, &ids, &[]).await.unwrap();
        let want = eng.git_map(&ids, &[]).await.unwrap();
        assert_eq!(got.asc_to_git, want.asc_to_git);
        assert!(got.asc_to_git[0].is_some() && got.asc_to_git[2].is_none());

        for (a, b) in [(h.c1, h.c3), (h.c3, h.c1), (h.c2, h.merge)] {
            assert_eq!(
                HistoryApi::diff(&rpc, a, b, &[]).await.unwrap(),
                eng.diff(a, b, &[]).await.unwrap()
            );
        }
        assert_eq!(
            HistoryApi::diff(&rpc, h.c1, h.c3, &["d".to_owned()])
                .await
                .unwrap(),
            eng.diff(h.c1, h.c3, &["d".to_owned()]).await.unwrap()
        );
    }

    #[tokio::test]
    async fn blame_matches_the_engine() {
        let (fx, h, rpc) = setup().await;
        let path = RepoPath::new("f.txt").unwrap();
        let e: &dyn BlameApi = &**fx.engine();
        for lines in [None, Some(1..3)] {
            let got = BlameApi::blame(&rpc, h.c3, &path, lines.clone())
                .await
                .unwrap();
            let want = e.blame(h.c3, &path, lines).await.unwrap();
            assert_eq!(got, want);
        }
        let missing = RepoPath::new("nope.txt").unwrap();
        assert!(matches!(
            BlameApi::blame(&rpc, h.c3, &missing, None).await,
            Err(blame::Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn objects_round_trip_and_reject_bad_bytes() {
        let (_fx, _h, rpc) = setup().await;
        let objs = rpc.objects();
        let text = b"remote payload 0123456789".repeat(40);
        let key = ObjKey::File(FileId::of(&text));
        let stored = encode_stored(&text);
        assert!(objs.get(&key).await.unwrap().is_none());
        assert_eq!(objs.has_many(&[key]).await.unwrap(), [false]);
        objs.put(&key, stored.clone()).await.unwrap();
        objs.put(&key, stored.clone()).await.unwrap();
        assert_eq!(objs.get(&key).await.unwrap().unwrap(), text.as_slice());
        assert_eq!(objs.get_stored(&key).await.unwrap().unwrap(), stored);
        assert_eq!(
            objs.get_range(&key, 3..10).await.unwrap().unwrap(),
            &text[3..10]
        );
        assert_eq!(objs.has_many(&[key, key]).await.unwrap(), [true, true]);
        assert!(matches!(
            objs.get_range(&ObjKey::Tag(asc_core::id::TagId::of(b"t")), 0..1)
                .await,
            Err(objects::Error::RangeUnsupported(_))
        ));

        // A put that does not hash to its key never leaves the client.
        let fresh = ObjKey::File(FileId::of(b"fresh"));
        assert!(matches!(
            objs.put(&fresh, encode_stored(b"not fresh")).await,
            Err(objects::Error::Corrupt(k)) if k == fresh
        ));
        // Nor does one the server would refuse for size.
        let big = vec![0u8; 4 * 1024 * 1024 + 1];
        let big_key = ObjKey::File(FileId::of(&big));
        assert!(matches!(
            objs.put(&big_key, encode_stored(&big)).await,
            Err(objects::Error::Codec(..))
        ));
        // The server verifies what a hostile client sends anyway.
        let msg = pb::PutObjectsRequest {
            repo: rpc.repo_ref(),
            objects: vec![pb::Object {
                key: Some(pb::ObjKey::try_from(&fresh).unwrap()),
                data: encode_stored(b"not fresh"),
            }],
        };
        let err = rpc
            .primary()
            .put_objects(rpc.req(stream::iter(vec![msg]), None))
            .await
            .unwrap_err();
        assert!(matches!(
            status_rpc_error(&err),
            Some(RpcError::InvalidObject(_))
        ));
        assert!(objs.get_stored(&fresh).await.unwrap().is_none());
        assert!(objs.delete(&key).await.is_err());
    }

    #[tokio::test]
    async fn serving_a_thousand_objects_decodes_nothing() {
        let (fx, _h, rpc) = setup().await;
        let mut keys = Vec::new();
        let mut want = std::collections::HashMap::new();
        for i in 0..1000u32 {
            let data = format!("object {i} {}", "compressible ".repeat(64)).into_bytes();
            let key = ObjKey::File(FileId::of(&data));
            let stored = encode_stored(&data);
            assert_eq!(stored[0], 1, "test data must be stored compressed");
            fx.objects.put(&key, stored.clone()).await.unwrap();
            want.insert(key, stored);
            keys.push(key);
        }
        let absent = ObjKey::File(FileId::of(b"absent"));
        keys.push(absent);
        fx.objects.decodes.store(0, Ordering::SeqCst);

        let got: Vec<(ObjKey, Bytes)> = rpc.fetch(keys.clone()).try_collect().await.unwrap();
        assert_eq!(got.len(), 1000);
        for (k, b) in &got {
            assert_eq!(want[k], *b, "stored bytes must arrive untouched");
        }
        assert_eq!(fx.objects.decodes.load(Ordering::SeqCst), 0);
        assert!(
            rpc.has_objects(&keys).await.unwrap()[..1000]
                .iter()
                .all(|&h| h)
        );
        assert_eq!(fx.objects.decodes.load(Ordering::SeqCst), 0);
        // The same keys through the sync `Remote` view.
        let via_remote: Vec<_> = Remote::get_many(&rpc, keys).try_collect().await.unwrap();
        assert_eq!(via_remote.len(), 1000);
    }

    #[tokio::test]
    async fn refs_swap_atomically_and_list_in_pages() {
        let (_fx, h, rpc) = setup().await;
        // The client's push uploads first, so every target exists on the server.
        let missing = Remote::update_refs(
            &rpc,
            &[put_ref("refs/heads/master", Expect::Absent, Some(h.c3))],
        )
        .await;
        // The fixture's objects are already on the server (they were created there).
        missing.unwrap();

        let refs = Remote::list_refs(&rpc).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, RefTarget::Changeset(h.c3));
        assert_eq!(refs[0].version, 1);

        // A stale expectation is a conflict that names the actual value; nothing else applies.
        let stale = Remote::update_refs(
            &rpc,
            &[
                put_ref("refs/heads/other", Expect::Absent, Some(h.c1)),
                put_ref(
                    "refs/heads/master",
                    Expect::Value(RefTarget::Changeset(h.c1)),
                    Some(h.c2),
                ),
            ],
        )
        .await;
        match stale {
            Err(sync::Error::Refs(RefError::Conflict(c))) => {
                assert_eq!(c.name, name("refs/heads/master"));
                assert_eq!(c.actual, Some(RefTarget::Changeset(h.c3)));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(Remote::list_refs(&rpc).await.unwrap().len(), 1);

        // Fast-forward mode refuses a sideways move and accepts a real fast-forward.
        let sideways = rpc
            .update_refs_with(
                &[put_ref(
                    "refs/heads/master",
                    Expect::Value(RefTarget::Changeset(h.c3)),
                    Some(h.b2),
                )],
                PushMode::FastForward,
                Uuid::now_v7(),
            )
            .await;
        assert!(matches!(
            sideways,
            Err(CallError::Rpc(RpcError::NotFastForward(_)))
        ));
        rpc.update_refs_with(
            &[put_ref(
                "refs/heads/master",
                Expect::Value(RefTarget::Changeset(h.c3)),
                Some(h.merge),
            )],
            PushMode::FastForward,
            Uuid::now_v7(),
        )
        .await
        .unwrap();

        // A target the server does not hold is refused and named.
        let ghost = ChangesetId::from_bytes([7; 32]);
        let err = rpc
            .update_refs_with(
                &[put_ref("refs/heads/ghost", Expect::Absent, Some(ghost))],
                PushMode::Force,
                Uuid::now_v7(),
            )
            .await;
        match err {
            Err(CallError::Rpc(RpcError::MissingObjects(info))) => {
                assert_eq!(info.paths, [ObjKey::Changeset(ghost).path()]);
            }
            other => panic!("expected MISSING_OBJECTS, got {other:?}"),
        }

        // Paging: five refs, two per page, in name order, no repeats.
        for i in 0..5 {
            Remote::update_refs(
                &rpc,
                &[put_ref(
                    &format!("refs/tags/t{i}"),
                    Expect::Absent,
                    Some(h.c1),
                )],
            )
            .await
            .unwrap();
        }
        let mut names = Vec::new();
        let mut after = String::new();
        loop {
            let req = pb::ListRefsRequest {
                repo: rpc.repo_ref(),
                prefix: "refs/tags/".into(),
                after: after.clone(),
                limit: 2,
                ..pb::ListRefsRequest::default()
            };
            let page = rpc
                .primary()
                .list_refs(rpc.req(req, None))
                .await
                .unwrap()
                .into_inner();
            assert!(page.refs.len() <= 2);
            names.extend(page.refs.iter().map(|r| r.name.clone()));
            if page.next.is_empty() {
                break;
            }
            after = page.next;
        }
        assert_eq!(
            names,
            [
                "refs/tags/t0",
                "refs/tags/t1",
                "refs/tags/t2",
                "refs/tags/t3",
                "refs/tags/t4"
            ]
        );

        // Deleting works through the same call.
        Remote::update_refs(&rpc, &[put_ref("refs/tags/t0", Expect::Any, None)])
            .await
            .unwrap();
        assert!(
            !Remote::list_refs(&rpc)
                .await
                .unwrap()
                .iter()
                .any(|r| r.name == name("refs/tags/t0"))
        );
    }

    #[tokio::test]
    async fn calls_are_authenticated_and_authorised() {
        let fx = Fixture::start().await;
        let h = history(&fx).await;
        let key = ObjKey::Changeset(h.c1);

        let anon = fx.client(None);
        let e = anon.has_objects(&[key]).await.unwrap_err();
        assert!(
            matches!(
                e,
                CallError::Status {
                    code: Code::Unauthenticated,
                    ..
                }
            ),
            "{e}"
        );
        let bad = fx.client(Some("wrong"));
        assert!(matches!(
            bad.has_objects(&[key]).await.unwrap_err(),
            CallError::Status {
                code: Code::Unauthenticated,
                ..
            }
        ));

        let ro = fx.client(Some("ro"));
        assert_eq!(ro.has_objects(&[key]).await.unwrap(), [true]);
        let denied = ro
            .update_refs_with(
                &[put_ref("refs/heads/x", Expect::Absent, Some(h.c1))],
                PushMode::Force,
                Uuid::now_v7(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                denied,
                CallError::Status {
                    code: Code::PermissionDenied,
                    ..
                }
            ),
            "{denied}"
        );
        let denied = ro
            .put_objects(vec![(key, Bytes::from_static(b"\0x"))])
            .await
            .unwrap_err();
        assert!(matches!(
            denied,
            CallError::Status {
                code: Code::PermissionDenied,
                ..
            }
        ));
        // Nothing was written.
        assert!(
            fx.client(Some("rw"))
                .list_refs_all()
                .await
                .unwrap()
                .is_empty()
        );

        // A mutating call without x-asc-op-id is refused before any handler runs.
        let rw = fx.client(Some("rw"));
        let req = pb::UpdateRefsRequest {
            repo: rw.repo_ref(),
            updates: vec![],
            ..pb::UpdateRefsRequest::default()
        };
        let e = rw
            .primary()
            .update_refs(rw.req(req, None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);

        // Repository names are validated, and another repository is not served.
        for bad in ["a", "a/b/c", "../x", "a/.b", "a b/c", ""] {
            assert!(
                RpcClient::connect("http://127.0.0.1:1", bad, None::<Credential>).is_err(),
                "{bad}"
            );
        }
        let other = RpcClient::connect(
            &format!("http://{}", fx.addr),
            "owner/other",
            Some(BearerToken::new("rw").unwrap()),
        )
        .unwrap();
        assert!(matches!(
            other.has_objects(&[key]).await.unwrap_err(),
            CallError::Status {
                code: Code::NotFound,
                ..
            }
        ));

        // Credentials are not sent over plain http to a remote host.
        assert!(matches!(
            RpcClient::connect("http://example.com", REPO, None::<Credential>),
            Err(ClientError::Refused(TransportError::PlainHttp { .. }))
        ));
        // Plain http to the local machine needs the opt-in, which this fixture set.
        assert!(matches!(
            channel_with("http://127.0.0.1:1", Policy::STRICT),
            Err(ClientError::Refused(
                TransportError::LoopbackNotEnabled { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn every_credential_scheme_authenticates_one_token() {
        let fx = Fixture::start().await;
        let h = history(&fx).await;
        let key = ObjKey::Changeset(h.c1);
        let rw = || BearerToken::new("rw").unwrap();
        for c in [
            Credential::bearer(rw()),
            Credential::token(rw()),
            Credential::basic("alice", "rw").unwrap(),
            Credential::basic("x-access-token", "rw").unwrap(),
            Credential::basic("", "rw").unwrap(),
        ] {
            let label = format!("{c:?} {:?}", c.user());
            let client = RpcClient::connect(&format!("http://{}", fx.addr), REPO, Some(c)).unwrap();
            assert_eq!(client.has_objects(&[key]).await.unwrap(), [true], "{label}");
        }
        let wrong = Credential::basic("alice", "nope").unwrap();
        let client = RpcClient::connect(&format!("http://{}", fx.addr), REPO, Some(wrong)).unwrap();
        assert!(matches!(
            client.has_objects(&[key]).await.unwrap_err(),
            CallError::Status {
                code: Code::Unauthenticated,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unregistered_groups_answer_unimplemented() {
        let (_fx, h, rpc) = setup().await;
        let e = rpc
            .primary()
            .classify(rpc.req(
                pb::ClassifyRequest {
                    repo: rpc.repo_ref(),
                    base: h.c1.to_string(),
                    head: h.c3.to_string(),
                    merge_base: false,
                },
                None,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::Unimplemented);
        let e = rpc
            .primary()
            .commit(rpc.req(
                pb::CommitRequest {
                    repo: rpc.repo_ref(),
                    ..pb::CommitRequest::default()
                },
                Some(Uuid::now_v7()),
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::Unimplemented);
    }

    #[tokio::test]
    async fn server_version_and_minimum_client() {
        let info = ServerInfo {
            server_version: "9.9.9".into(),
            min_client: crate::server::ClientVersion::parse("99.0.0"),
            features: vec!["x.v1".into()],
        };
        let fx = Fixture::start_with(info, |b, h| b.register(h).unwrap()).await;
        let rpc = fx.client(Some("rw"));
        let v = rpc.server_version().await.unwrap();
        assert_eq!(v.server_version, "9.9.9");
        assert_eq!(v.min_client, "99.0.0");
        assert_eq!(v.features, ["x.v1"]);
        let e = rpc
            .has_objects(&[ObjKey::File(FileId::of(b"x"))])
            .await
            .unwrap_err();
        assert!(
            matches!(e, CallError::Rpc(RpcError::ClientTooOld(_))),
            "{e}"
        );
    }

    #[tokio::test]
    async fn object_tiers_are_tried_in_order_then_the_primary() {
        let primary = Fixture::start().await;
        let tier = Fixture::start().await;
        let data = b"only on the tier".repeat(10);
        let key = ObjKey::File(FileId::of(&data));
        tier.objects.put(&key, encode_stored(&data)).await.unwrap();
        let other = ObjKey::File(FileId::of(b"only on the primary"));
        primary
            .objects
            .put(&other, encode_stored(b"only on the primary"))
            .await
            .unwrap();

        let dead = "http://127.0.0.1:1";
        let live = format!("http://{}", tier.addr);
        let client = primary
            .client(Some("rw"))
            .with_object_tiers(&[dead, &live])
            .unwrap();
        // HasObjects and GetObjects go to the first reachable tier.
        assert_eq!(
            client.has_objects(&[key, other]).await.unwrap(),
            [true, false]
        );
        let got: Vec<_> = client.fetch(vec![key]).try_collect().await.unwrap();
        assert_eq!(got[0].1, encode_stored(&data));
        // Everything else, and uploads, go to the primary.
        client
            .put_objects(vec![(other, encode_stored(b"only on the primary"))])
            .await
            .unwrap();
        assert!(!tier.client(Some("rw")).has_objects(&[other]).await.unwrap()[0]);

        // With no tier reachable the primary answers.
        let fallback = primary
            .client(Some("rw"))
            .with_object_tiers(&[dead])
            .unwrap();
        assert_eq!(
            fallback.has_objects(&[key, other]).await.unwrap(),
            [false, true]
        );
    }

    /// Stores the first upload, then loses the acknowledgement once.
    struct LostAck {
        inner: Arc<testkit::Backend>,
        puts: AtomicU32,
    }

    #[async_trait]
    impl ObjectsHandler for LostAck {
        async fn has_objects(
            &self,
            ctx: &CallCtx,
            req: pb::HasObjectsRequest,
        ) -> Result<pb::HasObjectsResponse, Status> {
            self.inner.has_objects(ctx, req).await
        }
        async fn put_objects(
            &self,
            ctx: &CallCtx,
            req: pb::PutObjectsRequest,
        ) -> Result<pb::PutObjectsResponse, Status> {
            let r = self.inner.put_objects(ctx, req).await?;
            if self.puts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(Status::unavailable("connection reset"));
            }
            Ok(r)
        }
        async fn get_objects(
            &self,
            ctx: &CallCtx,
            req: pb::GetObjectsRequest,
        ) -> Result<RpcStream<pb::GetObjectsResponse>, Status> {
            self.inner.get_objects(ctx, req).await
        }
    }

    #[tokio::test]
    async fn an_interrupted_upload_resumes_without_resending() {
        let flaky: Arc<std::sync::OnceLock<Arc<LostAck>>> = Arc::default();
        let slot = flaky.clone();
        let fx = Fixture::start_with(ServerInfo::default(), move |b, h| {
            let lost = Arc::new(LostAck {
                inner: b.clone(),
                puts: AtomicU32::new(0),
            });
            slot.set(lost.clone()).ok();
            h.set_objects(lost).unwrap();
            h.set_refs(b.clone()).unwrap();
            h.set_history(b.clone()).unwrap();
        })
        .await;
        let rpc = fx.client(Some("rw"));
        let mut batch = Vec::new();
        for i in 0..20u32 {
            let data = format!("resume {i}").into_bytes();
            batch.push((ObjKey::File(FileId::of(&data)), encode_stored(&data)));
        }
        rpc.put_objects(batch.clone()).await.unwrap();
        // The retry asked what the server holds and found everything: one upload only.
        assert_eq!(flaky.get().unwrap().puts.load(Ordering::SeqCst), 1);
        let keys: Vec<ObjKey> = batch.iter().map(|(k, _)| *k).collect();
        assert!(rpc.has_objects(&keys).await.unwrap().iter().all(|&h| h));
    }

    #[tokio::test]
    async fn the_server_refuses_git_import_extras_on_the_wire() {
        use asc_core::codec::Enveloped;
        use asc_core::object::SortedMap;
        let (fx, h, rpc) = setup().await;
        let base = fx.engine().get_changeset(h.c1).await.unwrap();
        let cs = Changeset {
            extra: SortedMap::new(vec![(b"git.commit".to_vec(), b"raw".to_vec())]).unwrap(),
            parents: vec![],
            ..(*base).clone()
        };
        let env = cs.to_envelope();
        let key = ObjKey::Changeset(ChangesetId::of(&env));
        let e = rpc
            .put_objects(vec![(key, encode_stored(&env))])
            .await
            .unwrap_err();
        assert!(
            matches!(e, CallError::Rpc(RpcError::InvalidObject(_))),
            "{e}"
        );
        assert_eq!(fx.objects.has_many(&[key]).await.unwrap(), [false]);
    }

    #[test]
    fn error_text_is_sanitised() {
        let e = CallError::from_status(Status::internal("bad\x1b[31m\nthing"));
        assert!(!e.to_string().contains('\x1b'));
        assert!(!e.to_string().contains('\n'));
        let long = CallError::from_status(Status::internal("x".repeat(10_000)));
        assert!(long.to_string().len() < 600);
    }
}
