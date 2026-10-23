//! The `asc.v1.Repo` gRPC server (SPEC §7).
//!
//! [`RepoService`] authenticates and authorises every call, validates the repository name and the
//! request limits, and then hands the request to a handler group in [`RepoHandlers`]. A group
//! nobody registered answers `UNIMPLEMENTED`. Later units register their groups once each
//! (`RepoHandlers::set_*`); [`EngineBackend`] fills the object, ref and history groups from a
//! `asc-engine` [`Engine`] for tests and local use.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::object::{Change, Changeset, Kind, TagRecord};
use asc_core::path::{RefName, RepoPath};
use asc_engine::Engine;
use asc_engine::blame::{self, BlameApi};
use asc_engine::graph::{self, LogQuery};
use asc_engine::history::{HistoryApi, LOG_PAGE, PageToken};
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects, decode_verified};
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs, RepoKey};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use prost::Message;
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions, Request, Response, Status, Streaming};
use uuid::Uuid;

use crate::auth::{Access, AuthInterceptor, Authorizer, Credential, Principal};
use crate::convert::{self, ConvertError, IdMode};
use crate::errors::{ErrorInfo, RpcError};
use crate::pb;
use crate::services::repo_server::{Repo, RepoServer};

/// Largest gRPC message either side accepts or sends: a 16 MiB payload plus framing.
pub const MAX_MESSAGE: usize = 17 * 1024 * 1024;
/// Keys per `HasObjects` request.
pub const MAX_HAS_KEYS: usize = 4096;
/// Keys per `GetObjects` request message.
pub const MAX_GET_KEYS: usize = 256;
/// Stored bytes per `GetObjects` response message.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Objects per `PutObjects` request message.
pub const MAX_PUT_OBJECTS: usize = 4096;
/// Revspecs, paths or ids per request.
pub const MAX_LIST: usize = 4096;
/// Longest revspec, path or prefix string, in bytes.
pub const MAX_STRING: usize = 4096;
/// Ref updates per `UpdateRefs` request.
pub const MAX_REF_UPDATES: usize = 1024;
/// Refs per `ListRefs` page when the request names no limit.
pub const DEFAULT_REF_PAGE: usize = 1000;
/// Most refs one `ListRefs` page returns.
pub const MAX_REF_PAGE: usize = 10_000;
/// Longest repository name, in bytes.
pub const MAX_REPO_NAME: usize = 255;
/// Changes per `DiffResponse` message.
const DIFF_CHUNK: usize = 1000;
/// Changes a diff returns when the request sets no `max_files`.
const DEFAULT_MAX_FILES: usize = 1_000_000;
/// Hunks per `BlameResponse` message.
const BLAME_CHUNK: usize = 10_000;
/// Concurrent point reads for one `GetObjects` request.
const READ_CONCURRENCY: usize = 16;
/// Concurrent writes for one `PutObjects` message.
const WRITE_CONCURRENCY: usize = 16;

/// The raw generated messages, for the units that implement handler groups.
pub mod wire {
    pub use crate::pb::*;
}

/// A boxed stream of response messages.
pub type RpcStream<T> = BoxStream<'static, Result<T, Status>>;

// ------------------------------------------------------------------------------ names

/// A validated `owner/repo` name: two non-empty segments of `[A-Za-z0-9._-]`, none starting with
/// a dot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepoName(String);

impl RepoName {
    /// Validates `s`.
    pub fn parse(s: &str) -> Result<Self, Status> {
        let bad = || Status::invalid_argument("invalid repository name");
        if s.len() > MAX_REPO_NAME {
            return Err(bad());
        }
        let mut parts = s.split('/');
        let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad());
        };
        let segment_ok = |p: &str| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        if segment_ok(owner) && segment_ok(name) {
            Ok(Self(s.to_owned()))
        } else {
            Err(bad())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The version a client reports in `x-asc-client: asc/<semver> <os>-<arch>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ClientVersion {
    /// Parses `major.minor.patch`; a `-pre` or `+build` suffix is ignored.
    pub fn parse(s: &str) -> Option<Self> {
        let core = s.split(['-', '+']).next()?;
        let mut it = core.split('.');
        let v = Self {
            major: it.next()?.parse().ok()?,
            minor: it.next()?.parse().ok()?,
            patch: it.next()?.parse().ok()?,
        };
        it.next().is_none().then_some(v)
    }

    /// Reads the version from a header value; `Ok(None)` when it names another client.
    pub fn from_header(h: &str) -> Result<Option<Self>, Status> {
        let first = h.split_whitespace().next().unwrap_or("");
        let Some(v) = first.strip_prefix("asc/") else {
            return Ok(None);
        };
        Self::parse(v)
            .map(Some)
            .ok_or_else(|| Status::invalid_argument("malformed x-asc-client version"))
    }
}

impl fmt::Display for ClientVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What `Version` reports and the oldest client the server accepts.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub server_version: String,
    pub min_client: Option<ClientVersion>,
    pub features: Vec<String>,
}

impl Default for ServerInfo {
    fn default() -> Self {
        Self {
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            min_client: None,
            features: Vec::new(),
        }
    }
}

/// Per-call facts a handler may rely on: the repository is valid and the caller is authorised
/// for the access the RPC needs.
#[derive(Clone, Debug)]
pub struct CallCtx {
    pub repo: RepoName,
    pub id_mode: IdMode,
    pub principal: Principal,
    /// `x-asc-op-id`; always present on mutating calls.
    pub op_id: Option<Uuid>,
    pub client: Option<ClientVersion>,
}

// ------------------------------------------------------------------------------ errors

fn code_of(e: &RpcError) -> tonic::Code {
    use tonic::Code;
    match e {
        RpcError::RefConflict(_) | RpcError::VersionConflict(_) => Code::Aborted,
        RpcError::NotFastForward(_)
        | RpcError::LandConflict(_)
        | RpcError::LandNeedsClientRebase(_)
        | RpcError::MissingObjects(_)
        | RpcError::HookRejected(_)
        | RpcError::MirrorMode(_)
        | RpcError::ClientTooOld(_)
        | RpcError::ProjectsConfig(_)
        | RpcError::ChangeDiverged(_)
        | RpcError::ChangeFrozen(_)
        | RpcError::LandNeedsContentMerge(_) => Code::FailedPrecondition,
        RpcError::InvalidObject(_) | RpcError::PathPolicy(_) => Code::InvalidArgument,
        RpcError::ChangeNotAllowed(_) => Code::PermissionDenied,
        RpcError::RateLimited(_)
        | RpcError::CloudHeadsLimit(_)
        | RpcError::CloudWorkspacesLimit(_)
        | RpcError::UserRefsLimit(_)
        | RpcError::QuotaExceeded(_) => Code::ResourceExhausted,
        RpcError::Unknown { .. } => Code::Unknown,
        _ => Code::FailedPrecondition,
    }
}

/// The gRPC status for `e`, with the `ErrorDetail` as the status details.
pub fn rpc_status(e: &RpcError) -> Status {
    let detail = pb::ErrorDetail::from(e);
    Status::with_details(
        code_of(e),
        e.to_string(),
        Bytes::from(detail.encode_to_vec()),
    )
}

/// The `ErrorDetail` a status carries, if any.
pub fn status_rpc_error(s: &Status) -> Option<RpcError> {
    if s.details().is_empty() {
        return None;
    }
    let detail = pb::ErrorDetail::decode(s.details()).ok()?;
    RpcError::try_from(&detail).ok()
}

fn convert_status(e: ConvertError) -> Status {
    Status::invalid_argument(e.to_string())
}

fn internal(context: &str, e: &dyn fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn store_status(e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(k) => {
            tracing::error!(key = %k, "stored object does not match its key");
            Status::data_loss("stored object is corrupt")
        }
        objects::Error::Codec(k, c) => {
            tracing::error!(key = %k, error = %c, "stored object does not decode");
            Status::data_loss("stored object is corrupt")
        }
        objects::Error::RangeUnsupported(_) => Status::invalid_argument("range not supported"),
        e => internal("object store", &e),
    }
}

fn put_status(key: ObjKey, e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(_) | objects::Error::Codec(..) => {
            rpc_status(&RpcError::InvalidObject(ErrorInfo {
                paths: vec![key.path()],
                ..ErrorInfo::default()
            }))
        }
        e => store_status(e),
    }
}

fn graph_status(e: graph::Error) -> Status {
    match e {
        graph::Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        graph::Error::Query(m) => Status::invalid_argument(m),
        graph::Error::Grep(e) => Status::invalid_argument(e.to_string()),
        graph::Error::Unsupported(m) => Status::unimplemented(m),
        e if e.is_missing() => Status::not_found("changeset is missing"),
        e => internal("history", &e),
    }
}

fn ref_status(e: RefError) -> Status {
    match e {
        RefError::Conflict(c) => rpc_status(&RpcError::RefConflict(ErrorInfo {
            paths: vec![String::from_utf8_lossy(c.name.as_bytes()).into_owned()],
            actual: c
                .actual
                .map_or_else(Bytes::new, |t| Bytes::copy_from_slice(&t.to_bytes())),
            conflicts: Vec::new(),
        })),
        e @ RefError::NonUtf8Name(_) => Status::invalid_argument(e.to_string()),
        e => internal("refs", &e),
    }
}

// ------------------------------------------------------------------------------ handlers

/// Answers every method `UNIMPLEMENTED`; used for groups nobody registered.
struct Unset;

fn unimplemented_rpc(name: &str) -> Status {
    Status::unimplemented(format!("{name} is not available on this server"))
}

macro_rules! handler_group {
    (
        $(#[$doc:meta])*
        $trait:ident {
            $( fn $name:ident($req:ident) -> $resp:ident; )*
            $( stream fn $sname:ident($sreq:ident) -> $sresp:ident; )*
        }
    ) => {
        $(#[$doc])*
        #[async_trait]
        pub trait $trait: Send + Sync {
            $(
                async fn $name(&self, _ctx: &CallCtx, _req: pb::$req) -> Result<pb::$resp, Status> {
                    Err(unimplemented_rpc(stringify!($name)))
                }
            )*
            $(
                async fn $sname(
                    &self,
                    _ctx: &CallCtx,
                    _req: pb::$sreq,
                ) -> Result<RpcStream<pb::$sresp>, Status> {
                    Err(unimplemented_rpc(stringify!($sname)))
                }
            )*
        }

        impl $trait for Unset {}
    };
}

handler_group! {
    /// `HasObjects`, `GetObjects`, `PutObjects`.
    ///
    /// `get_objects` is called once per request message and streams its responses; the service
    /// serialises the messages of one call. `put_objects` is called once per request message.
    ObjectsHandler {
        fn has_objects(HasObjectsRequest) -> HasObjectsResponse;
        fn put_objects(PutObjectsRequest) -> PutObjectsResponse;
        stream fn get_objects(GetObjectsRequest) -> GetObjectsResponse;
    }
}
handler_group! {
    /// `ListRefs`, `UpdateRefs`.
    RefsHandler {
        fn list_refs(ListRefsRequest) -> ListRefsResponse;
        fn update_refs(UpdateRefsRequest) -> UpdateRefsResponse;
    }
}
handler_group! {
    /// `Resolve`, `GitMap`, `Log`, `MergeBase`, `IsAncestor`, `Diff`.
    HistoryHandler {
        fn resolve(ResolveRequest) -> ResolveResponse;
        fn git_map(GitMapRequest) -> GitMapResponse;
        fn merge_base(MergeBaseRequest) -> MergeBaseResponse;
        fn is_ancestor(IsAncestorRequest) -> IsAncestorResponse;
        stream fn log(LogRequest) -> LogResponse;
        stream fn diff(DiffRequest) -> DiffResponse;
    }
}
handler_group! {
    /// `Blame`, `LastCommits`, `ReadChangeset`, `ReadTree`, `ReadFile`, `Stat`, `CheckAttr`,
    /// `Grep`, `Archive`.
    ReadsHandler {
        fn last_commits(LastCommitsRequest) -> LastCommitsResponse;
        fn read_changeset(ReadChangesetRequest) -> ReadChangesetResponse;
        fn stat(StatRequest) -> StatResponse;
        fn check_attr(CheckAttrRequest) -> CheckAttrResponse;
        stream fn blame(BlameRequest) -> BlameResponse;
        stream fn read_tree(ReadTreeRequest) -> ReadTreeResponse;
        stream fn read_file(ReadFileRequest) -> ReadFileResponse;
        stream fn grep(GrepRequest) -> GrepResponse;
        stream fn archive(ArchiveRequest) -> ArchiveResponse;
    }
}
handler_group! {
    /// `Commit`, `Merge`, `Apply`, `CopyClosure`.
    MutationsHandler {
        fn commit(CommitRequest) -> CommitResponse;
        fn merge(MergeRequest) -> MergeResponse;
        fn apply(ApplyRequest) -> ApplyResponse;
        fn copy_closure(CopyClosureRequest) -> CopyClosureResponse;
    }
}
handler_group! {
    /// `RepoAdmin`.
    AdminHandler {
        fn repo_admin(RepoAdminRequest) -> RepoAdminResponse;
    }
}
handler_group! {
    /// `ImportGitPush`, `Ingest`.
    ImportPushHandler {
        fn import_git_push(ImportGitPushRequest) -> ImportGitPushResponse;
        fn ingest(IngestRequest) -> IngestResponse;
    }
}
handler_group! {
    /// `FindLanding`.
    LandingHandler {
        fn find_landing(FindLandingRequest) -> FindLandingResponse;
    }
}
handler_group! {
    /// `Classify`.
    ClassifyHandler {
        fn classify(ClassifyRequest) -> ClassifyResponse;
    }
}
handler_group! {
    /// `Rebase`.
    RebaseHandler {
        fn rebase(RebaseRequest) -> RebaseResponse;
    }
}
handler_group! {
    /// `CommitGraph`, `LineHistory`, `Compare`, `RepoConfig` (SPEC §20.6).
    InsightsHandler {
        fn compare(CompareRequest) -> CompareResponse;
        fn repo_config(RepoConfigRequest) -> RepoConfigResponse;
        stream fn commit_graph(CommitGraphRequest) -> CommitGraphResponse;
        stream fn line_history(LineHistoryRequest) -> LogResponse;
    }
}

/// A handler group was registered twice.
#[derive(Debug, thiserror::Error)]
#[error("the {0} handler group is already registered")]
pub struct AlreadyRegistered(pub &'static str);

macro_rules! registry {
    ($( $field:ident, $setter:ident: $trait:ident; )*) => {
        /// One `Arc<dyn …>` per RPC group, each set at most once (SPEC §7, PLAN U42).
        #[derive(Default)]
        pub struct RepoHandlers {
            $( $field: OnceLock<Arc<dyn $trait>>, )*
        }

        impl RepoHandlers {
            pub fn new() -> Self {
                Self::default()
            }

            $(
                #[doc = concat!("Registers the `", stringify!($field), "` group.")]
                pub fn $setter(&self, h: Arc<dyn $trait>) -> Result<(), AlreadyRegistered> {
                    self.$field.set(h).map_err(|_| AlreadyRegistered(stringify!($field)))
                }

                fn $field(&self) -> &dyn $trait {
                    self.$field.get().map_or(&Unset as &dyn $trait, |h| &**h)
                }
            )*
        }
    };
}

registry! {
    objects, set_objects: ObjectsHandler;
    refs, set_refs: RefsHandler;
    history, set_history: HistoryHandler;
    reads, set_reads: ReadsHandler;
    mutations, set_mutations: MutationsHandler;
    admin, set_admin: AdminHandler;
    import_push, set_import_push: ImportPushHandler;
    landing, set_landing: LandingHandler;
    classify, set_classify: ClassifyHandler;
    rebase, set_rebase: RebaseHandler;
    insights, set_insights: InsightsHandler;
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Repo` service.
pub struct RepoService {
    handlers: Arc<RepoHandlers>,
    authorizer: Arc<dyn Authorizer>,
    info: ServerInfo,
}

impl RepoService {
    pub fn new(
        handlers: Arc<RepoHandlers>,
        authorizer: Arc<dyn Authorizer>,
        info: ServerInfo,
    ) -> Self {
        Self {
            handlers,
            authorizer,
            info,
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<RepoServer<Self>, AuthInterceptor> {
        let server = RepoServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    async fn context(
        &self,
        meta: &MetadataMap,
        ext: &Extensions,
        repo: Option<&pb::RepoRef>,
        access: Access,
        mutating: bool,
    ) -> Result<CallCtx, Status> {
        let repo = repo.ok_or_else(|| Status::invalid_argument("missing repo"))?;
        let name = RepoName::parse(&repo.name)?;
        let id_mode = IdMode::from_wire(repo.id_mode).map_err(convert_status)?;
        let client = match meta.get("x-asc-client") {
            None => None,
            Some(v) => {
                let text = v
                    .to_str()
                    .map_err(|_| Status::invalid_argument("malformed x-asc-client"))?;
                ClientVersion::from_header(text)?
            }
        };
        if let (Some(min), Some(got)) = (self.info.min_client, client)
            && got < min
        {
            let mut info = ErrorInfo::default();
            info.paths.push(format!("minimum client version is {min}"));
            return Err(rpc_status(&RpcError::ClientTooOld(info)));
        }
        let principal = self
            .authorizer
            .authorize_credential(ext.get::<Credential>(), name.as_str(), access)
            .await?;
        let op_id = match meta.get("x-asc-op-id") {
            None => None,
            Some(v) => Some(
                v.to_str()
                    .ok()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(|| Status::invalid_argument("malformed x-asc-op-id"))?,
            ),
        };
        if mutating && op_id.is_none() {
            return Err(Status::invalid_argument("mutating calls need x-asc-op-id"));
        }
        Ok(CallCtx {
            repo: name,
            id_mode,
            principal,
            op_id,
            client,
        })
    }

    async fn open<T>(
        &self,
        request: Request<T>,
        access: Access,
        mutating: bool,
        repo_of: fn(&T) -> Option<&pb::RepoRef>,
    ) -> Result<(CallCtx, T), Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, repo_of(&msg), access, mutating)
            .await?;
        Ok((ctx, msg))
    }
}

/// Later messages of one stream must name the repository the first one did.
fn same_repo(ctx: &CallCtx, repo: Option<&pb::RepoRef>) -> Result<(), Status> {
    let ok = repo.is_some_and(|r| {
        r.name == ctx.repo.as_str() && IdMode::from_wire(r.id_mode).ok() == Some(ctx.id_mode)
    });
    if ok {
        Ok(())
    } else {
        Err(Status::invalid_argument(
            "every message of a stream must name the same repository",
        ))
    }
}

/// The messages of an inbound stream, starting with the already-read `first`.
fn messages<T: Send + 'static>(
    input: Streaming<T>,
    first: T,
) -> impl futures::Stream<Item = Result<T, Status>> + Send {
    stream::try_unfold((input, Some(first)), |(mut input, pending)| async move {
        let next = match pending {
            Some(m) => Some(m),
            None => input.message().await?,
        };
        Ok(next.map(|m| (m, (input, None))))
    })
}

/// Writes the whole `impl Repo for RepoService` in one expansion, so `async_trait` sees plain
/// `async fn`s: the `manual` items verbatim, then one forwarding method per listed RPC.
macro_rules! repo_impl {
    (
        manual { $($manual:tt)* }
        unary { $( $m:ident($req:ident) -> $resp:ident, $group:ident, $access:expr, $mutating:expr; )* }
        streaming { $( $sm:ident($sreq:ident) -> $ty:ident($sresp:ident), $sgroup:ident; )* }
    ) => {
        #[tonic::async_trait]
        impl Repo for RepoService {
            $($manual)*

            $(
                async fn $m(
                    &self,
                    request: Request<pb::$req>,
                ) -> Result<Response<pb::$resp>, Status> {
                    let (ctx, msg) = self
                        .open(request, $access, $mutating, |m: &pb::$req| m.repo.as_ref())
                        .await?;
                    self.handlers.$group().$m(&ctx, msg).await.map(Response::new)
                }
            )*

            $(
                type $ty = RpcStream<pb::$sresp>;

                async fn $sm(
                    &self,
                    request: Request<pb::$sreq>,
                ) -> Result<Response<RpcStream<pb::$sresp>>, Status> {
                    let (ctx, msg) = self
                        .open(request, Access::Read, false, |m: &pb::$sreq| m.repo.as_ref())
                        .await?;
                    self.handlers.$sgroup().$sm(&ctx, msg).await.map(Response::new)
                }
            )*
        }
    };
}

repo_impl! {
    manual {
    async fn version(
        &self,
        _request: Request<pb::VersionRequest>,
    ) -> Result<Response<pb::VersionResponse>, Status> {
        Ok(Response::new(pb::VersionResponse {
            server_version: self.info.server_version.clone(),
            min_client: self
                .info
                .min_client
                .map_or_else(String::new, |v| v.to_string()),
            features: self.info.features.clone(),
        }))
    }

    async fn has_objects(
        &self,
        request: Request<pb::HasObjectsRequest>,
    ) -> Result<Response<pb::HasObjectsResponse>, Status> {
        let (ctx, msg) = self
            .open(request, Access::Read, false, |m: &pb::HasObjectsRequest| {
                m.repo.as_ref()
            })
            .await?;
        if msg.keys.len() > MAX_HAS_KEYS {
            return Err(Status::invalid_argument("too many keys"));
        }
        self.handlers
            .objects()
            .has_objects(&ctx, msg)
            .await
            .map(Response::new)
    }

    type GetObjectsStream = RpcStream<pb::GetObjectsResponse>;

    async fn get_objects(
        &self,
        request: Request<Streaming<pb::GetObjectsRequest>>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        let (meta, ext, mut input) = request.into_parts();
        let Some(first) = input.message().await? else {
            return Ok(Response::new(stream::empty().boxed()));
        };
        let ctx = Arc::new(
            self.context(&meta, &ext, first.repo.as_ref(), Access::Read, false)
                .await?,
        );
        let handlers = self.handlers.clone();
        let out = messages(input, first)
            .and_then(move |req| {
                let (ctx, handlers) = (ctx.clone(), handlers.clone());
                async move {
                    same_repo(&ctx, req.repo.as_ref())?;
                    if req.keys.len() > MAX_GET_KEYS {
                        return Err(Status::invalid_argument("too many keys"));
                    }
                    handlers.objects().get_objects(&ctx, req).await
                }
            })
            .try_flatten()
            .boxed();
        Ok(Response::new(out))
    }

    async fn put_objects(
        &self,
        request: Request<Streaming<pb::PutObjectsRequest>>,
    ) -> Result<Response<pb::PutObjectsResponse>, Status> {
        let (meta, ext, mut input) = request.into_parts();
        let Some(first) = input.message().await? else {
            return Ok(Response::new(pb::PutObjectsResponse::default()));
        };
        let ctx = self
            .context(&meta, &ext, first.repo.as_ref(), Access::Write, false)
            .await?;
        let mut total = pb::PutObjectsResponse::default();
        let mut requests = std::pin::pin!(messages(input, first));
        while let Some(req) = requests.try_next().await? {
            same_repo(&ctx, req.repo.as_ref())?;
            if req.objects.len() > MAX_PUT_OBJECTS {
                return Err(Status::invalid_argument("too many objects"));
            }
            let r = self.handlers.objects().put_objects(&ctx, req).await?;
            total.stored = total.stored.saturating_add(r.stored);
            total.already_present = total.already_present.saturating_add(r.already_present);
        }
        Ok(Response::new(total))
    }

    async fn merge(
        &self,
        request: Request<pb::MergeRequest>,
    ) -> Result<Response<pb::MergeResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        // A head in another repository is read from it, so the caller needs read access there
        // too (SPEC §3.3), before the handler looks at that namespace.
        if let Some(head) = msg.head_repo.as_ref() {
            let head = RepoName::parse(&head.name)?;
            if head != ctx.repo {
                self.authorizer
                    .authorize_credential(ext.get::<Credential>(), head.as_str(), Access::Read)
                    .await?;
            }
        }
        self.handlers
            .mutations()
            .merge(&ctx, msg)
            .await
            .map(Response::new)
    }

    async fn copy_closure(
        &self,
        request: Request<pb::CopyClosureRequest>,
    ) -> Result<Response<pb::CopyClosureResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .context(&meta, &ext, msg.to.as_ref(), Access::Write, true)
            .await?;
        // The source namespace needs its own read check (SPEC §3.3).
        let from = msg
            .from
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing from"))?;
        let from = RepoName::parse(&from.name)?;
        self.authorizer
            .authorize_credential(ext.get::<Credential>(), from.as_str(), Access::Read)
            .await?;
        self.handlers
            .mutations()
            .copy_closure(&ctx, msg)
            .await
            .map(Response::new)
    }

    }
    unary {
        list_refs(ListRefsRequest) -> ListRefsResponse, refs, Access::Read, false;
        update_refs(UpdateRefsRequest) -> UpdateRefsResponse, refs, Access::Write, true;
        resolve(ResolveRequest) -> ResolveResponse, history, Access::Read, false;
        git_map(GitMapRequest) -> GitMapResponse, history, Access::Read, false;
        merge_base(MergeBaseRequest) -> MergeBaseResponse, history, Access::Read, false;
        is_ancestor(IsAncestorRequest) -> IsAncestorResponse, history, Access::Read, false;
        last_commits(LastCommitsRequest) -> LastCommitsResponse, reads, Access::Read, false;
        read_changeset(ReadChangesetRequest) -> ReadChangesetResponse, reads, Access::Read, false;
        stat(StatRequest) -> StatResponse, reads, Access::Read, false;
        check_attr(CheckAttrRequest) -> CheckAttrResponse, reads, Access::Read, false;
        commit(CommitRequest) -> CommitResponse, mutations, Access::Write, true;
        apply(ApplyRequest) -> ApplyResponse, mutations, Access::Write, true;
        repo_admin(RepoAdminRequest) -> RepoAdminResponse, admin, Access::Admin, true;
        import_git_push(ImportGitPushRequest) -> ImportGitPushResponse, import_push, Access::Admin, true;
        ingest(IngestRequest) -> IngestResponse, import_push, Access::Write, true;
        find_landing(FindLandingRequest) -> FindLandingResponse, landing, Access::Read, false;
        classify(ClassifyRequest) -> ClassifyResponse, classify, Access::Read, false;
        rebase(RebaseRequest) -> RebaseResponse, rebase, Access::Write, true;
        compare(CompareRequest) -> CompareResponse, insights, Access::Read, false;
        repo_config(RepoConfigRequest) -> RepoConfigResponse, insights, Access::Read, false;
    }
    streaming {
        log(LogRequest) -> LogStream(LogResponse), history;
        diff(DiffRequest) -> DiffStream(DiffResponse), history;
        blame(BlameRequest) -> BlameStream(BlameResponse), reads;
        read_tree(ReadTreeRequest) -> ReadTreeStream(ReadTreeResponse), reads;
        read_file(ReadFileRequest) -> ReadFileStream(ReadFileResponse), reads;
        grep(GrepRequest) -> GrepStream(GrepResponse), reads;
        archive(ArchiveRequest) -> ArchiveStream(ArchiveResponse), reads;
        commit_graph(CommitGraphRequest) -> CommitGraphStream(CommitGraphResponse), insights;
        line_history(LineHistoryRequest) -> LineHistoryStream(LogResponse), insights;
    }
}

// ------------------------------------------------------------------------------ engine backend

/// Serves one repository from an `asc-engine` [`Engine`]: objects, refs, history and blame.
///
/// This is the backend for tests and local use. The full server (U43b) wraps the same traits with
/// the ingest verification, hooks and land pipeline. Git id mode needs the git map index and is
/// answered `UNIMPLEMENTED`.
pub struct EngineBackend<O: Objects, R: Refs> {
    name: RepoName,
    repo: RepoKey,
    objects: Arc<O>,
    refs: Arc<R>,
    engine: Arc<Engine<O, R>>,
}

fn cs_ids(raw: &[Bytes], what: &'static str) -> Result<Vec<ChangesetId>, Status> {
    if raw.len() > MAX_LIST {
        return Err(Status::invalid_argument(format!("too many {what}")));
    }
    raw.iter()
        .map(|b| convert::WireId::from_slice(b).map_err(convert_status))
        .collect()
}

fn id_bytes(id: &ChangesetId) -> Bytes {
    Bytes::copy_from_slice(id.as_bytes())
}

fn log_entry(e: &asc_engine::history::LogEntry) -> pb::LogEntry {
    pb::LogEntry {
        id: id_bytes(&e.id),
        git: convert::opt_id_to_wire(e.git.as_ref()),
        parents: e.parents.iter().map(id_bytes).collect(),
        r#gen: e.generation,
        ..pb::LogEntry::default()
    }
}

fn kind_mode(kind: Kind) -> u32 {
    match kind {
        Kind::Exec => 0o100_755,
        Kind::Symlink => 0o120_000,
        Kind::Dir => 0o040_000,
        Kind::Gitlink => 0o160_000,
        Kind::File => 0o100_644,
    }
}

/// The wire form of one leaf change (`status`: `M` write, `C` write with a copy source, `D`
/// delete; the engine's leaf diff cannot tell an add from a modify).
fn path_change(path: &RepoPath, change: &Change) -> convert::PathChange {
    let base = convert::PathChange {
        path: path.clone(),
        status: "M".into(),
        old_path: None,
        old_mode: 0,
        new_mode: 0,
        old_git: None,
        new_git: None,
        old_id: None,
        new_id: None,
        old_size: 0,
        new_size: 0,
        similarity: 0,
    };
    match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            copy_from,
        } => convert::PathChange {
            status: if copy_from.is_some() { "C" } else { "M" }.into(),
            old_path: copy_from.clone(),
            new_mode: kind_mode(*kind),
            new_git: Some(*git),
            new_id: Some(*file),
            new_size: *size,
            ..base
        },
        Change::Gitlink { commit } => convert::PathChange {
            new_mode: kind_mode(Kind::Gitlink),
            new_git: Some(*commit),
            ..base
        },
        Change::Delete => convert::PathChange {
            status: "D".into(),
            ..base
        },
    }
}

/// `git.commit` and `git.tag` extras are only legal on git import (CARRY: ingest rule).
/// Derived values are recomputed server-side, never accepted from a client.
fn ingest_policy(key: &ObjKey, stored: &Bytes) -> Result<(), Status> {
    let invalid = || {
        rpc_status(&RpcError::InvalidObject(ErrorInfo {
            paths: vec![key.path()],
            ..ErrorInfo::default()
        }))
    };
    match key {
        ObjKey::Changeset(_) => {
            let data = decode_verified(key, stored).map_err(|e| put_status(*key, e))?;
            let cs: Changeset = decode_envelope(&data).map_err(|_| invalid())?;
            if cs.extra.get(&b"git.commit".to_vec()).is_some() {
                return Err(invalid());
            }
        }
        ObjKey::Tag(_) => {
            let data = decode_verified(key, stored).map_err(|e| put_status(*key, e))?;
            let tag: TagRecord = decode_envelope(&data).map_err(|_| invalid())?;
            if tag.extra.get(&b"git.tag".to_vec()).is_some() {
                return Err(invalid());
            }
        }
        ObjKey::Derived { .. } => return Err(invalid()),
        _ => {}
    }
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl<O: Objects + 'static, R: Refs + 'static> EngineBackend<O, R> {
    /// Serves `repo` (named `name` on the wire) over `objects` and `refs`.
    pub fn new(name: RepoName, repo: RepoKey, objects: Arc<O>, refs: Arc<R>) -> Self {
        let engine = Arc::new(Engine::new(objects.clone(), refs.clone(), repo));
        Self {
            name,
            repo,
            objects,
            refs,
            engine,
        }
    }

    /// The engine behind the backend.
    pub fn engine(&self) -> &Arc<Engine<O, R>> {
        &self.engine
    }

    /// Registers the objects, refs, history and reads (blame) groups.
    pub fn register(self: &Arc<Self>, handlers: &RepoHandlers) -> Result<(), AlreadyRegistered> {
        handlers.set_objects(self.clone())?;
        handlers.set_refs(self.clone())?;
        handlers.set_history(self.clone())?;
        handlers.set_reads(self.clone())
    }

    fn check(&self, ctx: &CallCtx) -> Result<(), Status> {
        if ctx.repo != self.name {
            return Err(Status::not_found("no such repository"));
        }
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode needs the git map index, which this backend does not keep",
            ));
        }
        Ok(())
    }

    async fn resolve_one(&self, spec: &str) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let got = self
            .engine
            .resolve(&[spec.to_owned()])
            .await
            .map_err(graph_status)?;
        got.into_iter()
            .next()
            .flatten()
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    async fn resolve_all(&self, specs: &[String]) -> Result<Vec<ChangesetId>, Status> {
        if specs.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many revisions"));
        }
        let mut out = Vec::with_capacity(specs.len());
        for s in specs {
            out.push(self.resolve_one(s).await?);
        }
        Ok(out)
    }

    fn log_query(&self, req: &pb::LogRequest) -> Result<LogQuery, Status> {
        if req.count_only
            || req.with_stats
            || !req.pickaxe.is_empty()
            || !req.resume_token.is_empty()
        {
            return Err(Status::unimplemented(
                "count_only, with_stats, pickaxe and resume_token need the full server",
            ));
        }
        if req.paths.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many paths"));
        }
        let paths = req
            .paths
            .iter()
            .map(|p| {
                RepoPath::new(p.as_bytes()).map_err(|e| Status::invalid_argument(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LogQuery {
            include: Vec::new(),
            exclude: Vec::new(),
            first_parent: req.first_parent,
            paths,
            follow: req.follow,
            author: (!req.author.is_empty()).then(|| req.author.clone().into_bytes()),
            grep: (!req.grep.is_empty()).then(|| req.grep.clone()),
            since: (req.since != 0).then_some(req.since),
            until: (req.until != 0).then_some(req.until),
            skip: req.skip as usize,
            limit: (req.limit != 0).then_some(req.limit as usize),
            cursor: None,
        })
    }
}

#[async_trait]
impl<O: Objects + 'static, R: Refs + 'static> ObjectsHandler for EngineBackend<O, R> {
    async fn has_objects(
        &self,
        ctx: &CallCtx,
        req: pb::HasObjectsRequest,
    ) -> Result<pb::HasObjectsResponse, Status> {
        self.check(ctx)?;
        let keys = req
            .keys
            .iter()
            .map(|k| ObjKey::try_from(k).map_err(convert_status))
            .collect::<Result<Vec<_>, _>>()?;
        let have = self.objects.has_many(&keys).await.map_err(store_status)?;
        let mut bitmap = vec![0u8; keys.len().div_ceil(8)];
        for (i, _) in have.iter().enumerate().filter(|(_, h)| **h) {
            bitmap[i / 8] |= 1 << (i % 8);
        }
        Ok(pb::HasObjectsResponse {
            present: Bytes::from(bitmap),
        })
    }

    async fn put_objects(
        &self,
        ctx: &CallCtx,
        req: pb::PutObjectsRequest,
    ) -> Result<pb::PutObjectsResponse, Status> {
        self.check(ctx)?;
        let mut batch = Vec::with_capacity(req.objects.len());
        for o in &req.objects {
            let key = o
                .key
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("object without a key"))?;
            batch.push((
                ObjKey::try_from(key).map_err(convert_status)?,
                o.data.clone(),
            ));
        }
        let keys: Vec<ObjKey> = batch.iter().map(|(k, _)| *k).collect();
        let had = self.objects.has_many(&keys).await.map_err(store_status)?;
        let objects = &self.objects;
        stream::iter(batch)
            .map(|(key, stored)| async move {
                ingest_policy(&key, &stored)?;
                objects
                    .put(&key, stored)
                    .await
                    .map_err(|e| put_status(key, e))
            })
            .buffer_unordered(WRITE_CONCURRENCY)
            .try_collect::<()>()
            .await?;
        let already = had.iter().filter(|h| **h).count() as u64;
        Ok(pb::PutObjectsResponse {
            stored: keys.len() as u64 - already,
            already_present: already,
        })
    }

    async fn get_objects(
        &self,
        ctx: &CallCtx,
        req: pb::GetObjectsRequest,
    ) -> Result<RpcStream<pb::GetObjectsResponse>, Status> {
        self.check(ctx)?;
        let keys = req
            .keys
            .iter()
            .map(|k| ObjKey::try_from(k).map_err(convert_status))
            .collect::<Result<Vec<_>, _>>()?;
        let objects = self.objects.clone();
        // Stored bytes go out as they were read: no decode, no re-encode, no hashing (SPEC §3.2).
        let reads: BoxStream<'static, Result<(ObjKey, Option<Bytes>), objects::Error>> =
            stream::iter(keys)
                .map(move |key| {
                    let objects = objects.clone();
                    async move { objects.get_stored(&key).await.map(|b| (key, b)) }
                })
                .buffered(READ_CONCURRENCY)
                .boxed();
        let out = stream::try_unfold(
            (reads, None::<(ObjKey, Bytes)>, false),
            |(mut reads, carry, done)| async move {
                if done {
                    return Ok(None);
                }
                let mut resp = pb::GetObjectsResponse::default();
                let mut size = 0usize;
                let wire =
                    |k: &ObjKey| pb::ObjKey::try_from(k).map_err(|e| internal("object key", &e));
                if let Some((k, b)) = carry {
                    size += b.len();
                    resp.objects.push(pb::Object {
                        key: Some(wire(&k)?),
                        data: b,
                    });
                }
                loop {
                    match reads.next().await {
                        None => {
                            let empty = resp.objects.is_empty() && resp.missing.is_empty();
                            return Ok((!empty).then_some((resp, (reads, None, true))));
                        }
                        Some(Err(e)) => return Err(store_status(e)),
                        Some(Ok((k, None))) => resp.missing.push(wire(&k)?),
                        Some(Ok((k, Some(b)))) => {
                            if b.len() > MAX_RESPONSE_BYTES {
                                return Err(Status::resource_exhausted(
                                    "object is larger than one response message",
                                ));
                            }
                            if size + b.len() > MAX_RESPONSE_BYTES {
                                return Ok(Some((resp, (reads, Some((k, b)), false))));
                            }
                            size += b.len();
                            resp.objects.push(pb::Object {
                                key: Some(wire(&k)?),
                                data: b,
                            });
                        }
                    }
                }
            },
        )
        .boxed();
        Ok(out)
    }
}

#[async_trait]
impl<O: Objects + 'static, R: Refs + 'static> RefsHandler for EngineBackend<O, R> {
    async fn list_refs(
        &self,
        ctx: &CallCtx,
        req: pb::ListRefsRequest,
    ) -> Result<pb::ListRefsResponse, Status> {
        self.check(ctx)?;
        if !req.contains.is_empty() {
            return Err(Status::unimplemented(
                "ListRefs contains needs the full server",
            ));
        }
        if req.prefix.len() > MAX_STRING || req.after.len() > MAX_STRING {
            return Err(Status::invalid_argument("prefix or page token is too long"));
        }
        let limit = match req.limit as usize {
            0 => DEFAULT_REF_PAGE,
            n => n.min(MAX_REF_PAGE),
        };
        let all = self
            .refs
            .list(self.repo, &req.prefix)
            .await
            .map_err(ref_status)?;
        let start = if req.after.is_empty() {
            0
        } else {
            all.partition_point(|e| e.name.as_bytes() <= req.after.as_bytes())
        };
        let page = &all[start..];
        let shown = &page[..page.len().min(limit)];
        let next = if page.len() > shown.len() {
            shown
                .last()
                .map(|e| String::from_utf8_lossy(e.name.as_bytes()).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let refs = shown
            .iter()
            .map(|e| pb::Ref {
                name: String::from_utf8_lossy(e.name.as_bytes()).into_owned(),
                target: Some(pb::RefTarget::from(&e.target)),
                version: e.version,
                expires_at: e.expires_at.unwrap_or(0),
            })
            .collect();
        Ok(pb::ListRefsResponse {
            refs,
            next,
            head: String::new(),
        })
    }

    async fn update_refs(
        &self,
        ctx: &CallCtx,
        req: pb::UpdateRefsRequest,
    ) -> Result<pb::UpdateRefsResponse, Status> {
        use pb::ref_update::Expect as Wire;
        self.check(ctx)?;
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        if req.updates.is_empty() || req.updates.len() > MAX_REF_UPDATES {
            return Err(Status::invalid_argument("bad number of updates"));
        }
        let mode = pb::UpdateMode::try_from(req.mode)
            .map_err(|_| Status::invalid_argument("unknown update mode"))?;
        if mode == pb::UpdateMode::Land {
            return Err(Status::unimplemented("LAND needs the full server"));
        }
        if req.hooks != pb::HookMode::Run as i32 {
            return Err(Status::permission_denied("hooks cannot be skipped here"));
        }
        if req.ttl_seconds < 0 {
            return Err(Status::invalid_argument("negative ttl"));
        }
        let expires_at = (req.ttl_seconds > 0).then(|| now_secs().saturating_add(req.ttl_seconds));
        let mut seen = HashSet::new();
        let mut updates = Vec::with_capacity(req.updates.len());
        let mut results = Vec::with_capacity(req.updates.len());
        let mut wanted = Vec::new();
        for u in &req.updates {
            let name = RefName::new(u.name.as_bytes())
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            if !seen.insert(name.clone()) {
                return Err(Status::invalid_argument("a ref is updated twice"));
            }
            let new = match (&u.new, u.delete) {
                (Some(t), false) => Some(RefTarget::try_from(t).map_err(convert_status)?),
                (None, true) => None,
                _ => {
                    return Err(Status::invalid_argument(
                        "an update either sets a target or deletes",
                    ));
                }
            };
            let current = self
                .refs
                .get(self.repo, &name)
                .await
                .map_err(ref_status)?
                .map(|e| e.target);
            let mut expect = match &u.expect {
                Some(Wire::Any(true)) => Expect::Any,
                Some(Wire::Absent(true)) => Expect::Absent,
                Some(Wire::Value(t)) => {
                    Expect::Value(RefTarget::try_from(t).map_err(convert_status)?)
                }
                _ => return Err(Status::invalid_argument("an update needs an expectation")),
            };
            if mode == pb::UpdateMode::FastForward {
                if let (Some(old), Some(new)) = (current, new) {
                    let ff = match (old, new) {
                        (RefTarget::Changeset(o), RefTarget::Changeset(n)) => {
                            o == n || self.engine.is_ancestor(o, n).await.map_err(graph_status)?
                        }
                        (o, n) => o == n,
                    };
                    if !ff {
                        return Err(rpc_status(&RpcError::NotFastForward(ErrorInfo {
                            paths: vec![u.name.clone()],
                            ..ErrorInfo::default()
                        })));
                    }
                }
                // Pin the value the check saw, so a concurrent move cannot slip past it.
                if expect == Expect::Any {
                    expect = current.map_or(Expect::Absent, Expect::Value);
                }
            }
            match new {
                Some(RefTarget::Changeset(c)) => wanted.push(ObjKey::Changeset(c)),
                Some(RefTarget::Tag(t)) => wanted.push(ObjKey::Tag(t)),
                None => {}
            }
            results.push(pb::RefResult {
                name: u.name.clone(),
                old: current.as_ref().map(pb::RefTarget::from),
                new: new.as_ref().map(pb::RefTarget::from),
            });
            updates.push(RefUpdate {
                name,
                expect,
                new,
                expires_at: if new.is_some() { expires_at } else { None },
            });
        }
        let have = self.objects.has_many(&wanted).await.map_err(store_status)?;
        let missing: Vec<String> = wanted
            .iter()
            .zip(&have)
            .filter(|(_, h)| !**h)
            .map(|(k, _)| k.path())
            .collect();
        if !missing.is_empty() {
            return Err(rpc_status(&RpcError::MissingObjects(ErrorInfo {
                paths: missing,
                ..ErrorInfo::default()
            })));
        }
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: ctx.principal.id.clone(),
                via: "serve".into(),
            },
            reason: "UpdateRefs".into(),
            op_id,
        };
        self.refs
            .update(self.repo, &txn)
            .await
            .map_err(ref_status)?;
        Ok(pb::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: String::new(),
        })
    }
}

#[async_trait]
impl<O: Objects + 'static, R: Refs + 'static> HistoryHandler for EngineBackend<O, R> {
    async fn resolve(
        &self,
        ctx: &CallCtx,
        req: pb::ResolveRequest,
    ) -> Result<pb::ResolveResponse, Status> {
        self.check(ctx)?;
        if req.revspecs.len() > MAX_LIST || req.revspecs.iter().any(|s| s.len() > MAX_STRING) {
            return Err(Status::invalid_argument("too many or too long revisions"));
        }
        let ids = self
            .engine
            .resolve(&req.revspecs)
            .await
            .map_err(graph_status)?;
        Ok(pb::ResolveResponse {
            ids: ids
                .iter()
                .map(|id| id.as_ref().map_or_else(Bytes::new, id_bytes))
                .collect(),
        })
    }

    async fn git_map(
        &self,
        ctx: &CallCtx,
        req: pb::GitMapRequest,
    ) -> Result<pb::GitMapResponse, Status> {
        self.check(ctx)?;
        let asc = cs_ids(&req.asc, "ids")?;
        if req.git.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many ids"));
        }
        let git: Vec<GitOid> = req
            .git
            .iter()
            .map(|b| convert::WireId::from_slice(b).map_err(convert_status))
            .collect::<Result<_, _>>()?;
        let m = self.engine.git_map(&asc, &git).await.map_err(graph_status)?;
        Ok(pb::GitMapResponse {
            asc_to_git: m
                .asc_to_git
                .iter()
                .map(|g| convert::opt_id_to_wire(g.as_ref()))
                .collect(),
            git_to_asc: m
                .git_to_asc
                .iter()
                .map(|c| c.as_ref().map_or_else(Bytes::new, id_bytes))
                .collect(),
        })
    }

    async fn merge_base(
        &self,
        ctx: &CallCtx,
        req: pb::MergeBaseRequest,
    ) -> Result<pb::MergeBaseResponse, Status> {
        self.check(ctx)?;
        let a = convert::WireId::from_slice(&req.a).map_err(convert_status)?;
        let others = cs_ids(&req.b, "ids")?;
        let bases = self
            .engine
            .merge_base(a, &others, req.all)
            .await
            .map_err(graph_status)?;
        Ok(pb::MergeBaseResponse {
            bases: bases.iter().map(id_bytes).collect(),
        })
    }

    async fn is_ancestor(
        &self,
        ctx: &CallCtx,
        req: pb::IsAncestorRequest,
    ) -> Result<pb::IsAncestorResponse, Status> {
        self.check(ctx)?;
        let a = convert::WireId::from_slice(&req.ancestor).map_err(convert_status)?;
        let b = convert::WireId::from_slice(&req.descendant).map_err(convert_status)?;
        Ok(pb::IsAncestorResponse {
            result: self.engine.is_ancestor(a, b).await.map_err(graph_status)?,
        })
    }

    async fn log(
        &self,
        ctx: &CallCtx,
        req: pb::LogRequest,
    ) -> Result<RpcStream<pb::LogResponse>, Status> {
        self.check(ctx)?;
        let mut query = self.log_query(&req)?;
        query.include = self.resolve_all(&req.include).await?;
        query.exclude = self.resolve_all(&req.exclude).await?;
        let token = if req.page_token.is_empty() {
            None
        } else if req.page_token.len() > 64 * 1024 {
            return Err(Status::invalid_argument("page token is too long"));
        } else {
            Some(PageToken(URL_SAFE_NO_PAD.decode(&req.page_token).map_err(
                |_| Status::invalid_argument("malformed page token"),
            )?))
        };
        let engine = self.engine.clone();
        // One response per engine page, produced as the client reads; dropping the stream stops
        // the walk.
        let out = stream::try_unfold((query, token, false), move |(query, token, finished)| {
            let engine = engine.clone();
            async move {
                if finished {
                    return Ok(None);
                }
                let page = engine
                    .log(&query, token.as_ref())
                    .await
                    .map_err(graph_status)?;
                debug_assert!(page.entries.len() <= LOG_PAGE);
                let resp = pb::LogResponse {
                    entries: page.entries.iter().map(log_entry).collect(),
                    count: page.entries.len() as u64,
                    next_page_token: page
                        .next
                        .as_ref()
                        .map_or_else(String::new, |t| URL_SAFE_NO_PAD.encode(&t.0)),
                    ..pb::LogResponse::default()
                };
                let finished = page.next.is_none();
                Ok(Some((resp, (query, page.next, finished))))
            }
        })
        .boxed();
        Ok(out)
    }

    async fn diff(
        &self,
        ctx: &CallCtx,
        req: pb::DiffRequest,
    ) -> Result<RpcStream<pb::DiffResponse>, Status> {
        self.check(ctx)?;
        let format = pb::DiffFormat::try_from(req.format)
            .map_err(|_| Status::invalid_argument("unknown diff format"))?;
        if !matches!(
            format,
            pb::DiffFormat::Names | pb::DiffFormat::Status | pb::DiffFormat::Raw
        ) || req.renames
        {
            return Err(Status::unimplemented(
                "patch, stat and rename output need the full server",
            ));
        }
        if req.paths.len() > MAX_LIST || req.paths.iter().any(|p| p.len() > MAX_STRING) {
            return Err(Status::invalid_argument("too many or too long paths"));
        }
        let head = self.resolve_one(&req.head).await?;
        let mut base = self.resolve_one(&req.base).await?;
        if req.merge_base {
            base = *self
                .engine
                .merge_base(base, &[head], false)
                .await
                .map_err(graph_status)?
                .first()
                .ok_or_else(|| Status::failed_precondition("no merge base"))?;
        }
        let changes = self
            .engine
            .diff(base, head, &req.paths)
            .await
            .map_err(graph_status)?;
        let cap = match req.max_files as usize {
            0 => DEFAULT_MAX_FILES,
            n => n.min(DEFAULT_MAX_FILES),
        };
        let truncated = changes.len() > cap;
        let wire: Vec<pb::PathChange> = changes
            .iter()
            .take(cap)
            .map(|(p, c)| pb::PathChange::from(&path_change(p, c)))
            .collect();
        let mut chunks: Vec<pb::DiffResponse> = wire
            .chunks(DIFF_CHUNK)
            .map(|c| pb::DiffResponse {
                changes: c.to_vec(),
                ..pb::DiffResponse::default()
            })
            .collect();
        if chunks.is_empty() {
            chunks.push(pb::DiffResponse::default());
        }
        if let Some(last) = chunks.last_mut() {
            last.truncated = truncated;
        }
        Ok(stream::iter(chunks.into_iter().map(Ok)).boxed())
    }
}

#[async_trait]
impl<O: Objects + 'static, R: Refs + 'static> ReadsHandler for EngineBackend<O, R> {
    /// Hunks use zero-based lines; `[start_line, end_line)` selects a range and `end_line == 0`
    /// means to the end of the file.
    async fn blame(
        &self,
        ctx: &CallCtx,
        req: pb::BlameRequest,
    ) -> Result<RpcStream<pb::BlameResponse>, Status> {
        self.check(ctx)?;
        if req.reverse {
            return Err(Status::unimplemented("reverse blame needs the full server"));
        }
        if req.path.len() > MAX_STRING {
            return Err(Status::invalid_argument("path is too long"));
        }
        let rev = self.resolve_one(&req.rev).await?;
        let path =
            RepoPath::new(&req.path[..]).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let blame_err = |e: blame::Error| match e {
            blame::Error::NotFound(p) => {
                Status::not_found(format!("{p} is not a file at that revision"))
            }
            blame::Error::BadRange { .. } => Status::out_of_range(e.to_string()),
            e => internal("blame", &e),
        };
        let whole = BlameApi::blame(&*self.engine, rev, &path, None)
            .await
            .map_err(blame_err)?;
        let view = if req.start_line == 0 && req.end_line == 0 {
            whole
        } else {
            let end = if req.end_line == 0 {
                whole.lines()
            } else {
                req.end_line
            };
            whole.slice(req.start_line..end).map_err(blame_err)?
        };
        let hunk = |run: &blame::Run| pb::BlameHunk {
            origin: id_bytes(&run.origin),
            orig_path: Bytes::copy_from_slice(run.orig_path.as_bytes()),
            orig_start: run.orig_line,
            start: run.start,
            len: run.len,
        };
        let chunks: Vec<pb::BlameResponse> = view
            .runs()
            .chunks(BLAME_CHUNK)
            .map(|runs| {
                let mut seen = HashSet::new();
                pb::BlameResponse {
                    hunks: runs.iter().map(hunk).collect(),
                    commits: runs
                        .iter()
                        .filter(|r| seen.insert(r.origin))
                        .map(|r| pb::LogEntry {
                            id: id_bytes(&r.origin),
                            ..pb::LogEntry::default()
                        })
                        .collect(),
                    reviews: Vec::new(),
                }
            })
            .collect();
        Ok(stream::iter(chunks.into_iter().map(Ok)).boxed())
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::FileId;
    use asc_store::codec::encode_stored;

    use super::*;
    use crate::client::testkit::{Fixture, REPO};

    #[test]
    fn repo_names_are_validated() {
        for ok in ["owner/repo", "a/b", "Ow-ner_1/re.po-2"] {
            assert!(RepoName::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "a", "a/", "/b", "a/b/c", "a//b", ".a/b", "a/.b", "../b", "a/..", "a b/c", "a/b\n",
            r"a\b/c", "ä/b",
        ] {
            assert!(RepoName::parse(bad).is_err(), "{bad:?}");
        }
        assert!(RepoName::parse(&format!("a/{}", "b".repeat(MAX_REPO_NAME))).is_err());
    }

    #[test]
    fn client_versions_parse_and_order() {
        let v = |s| ClientVersion::parse(s).unwrap();
        assert!(v("1.2.3") < v("1.10.0"));
        assert_eq!(v("1.2.3-rc.1"), v("1.2.3"));
        assert!(ClientVersion::parse("1.2").is_none());
        assert!(ClientVersion::parse("1.2.3.4").is_none());
        assert_eq!(
            ClientVersion::from_header("asc/0.4.1 linux-x86_64").unwrap(),
            Some(v("0.4.1"))
        );
        assert_eq!(ClientVersion::from_header("forgejo/9.0").unwrap(), None);
        assert!(ClientVersion::from_header("asc/banana x").is_err());
    }

    #[test]
    fn a_handler_group_registers_once() {
        let h = RepoHandlers::new();
        assert!(h.set_landing(Arc::new(Unset)).is_ok());
        let again = h.set_landing(Arc::new(Unset)).unwrap_err();
        assert_eq!(
            again.to_string(),
            "the landing handler group is already registered"
        );
    }

    #[test]
    fn error_details_cross_the_status() {
        let e = RpcError::RefConflict(ErrorInfo {
            paths: vec!["refs/heads/master".into()],
            actual: Bytes::from_static(&[1; 33]),
            conflicts: Vec::new(),
        });
        let s = rpc_status(&e);
        assert_eq!(s.code(), tonic::Code::Aborted);
        assert_eq!(status_rpc_error(&s), Some(e));
        assert_eq!(status_rpc_error(&Status::internal("x")), None);
        // Garbage details are ignored rather than trusted.
        let junk =
            Status::with_details(tonic::Code::Internal, "x", Bytes::from_static(b"\xff\xff"));
        assert_eq!(status_rpc_error(&junk), None);
    }

    #[test]
    fn clients_cannot_upload_derived_values() {
        let key = ObjKey::Derived {
            name: "gen",
            version: 1,
            id: [7; 32],
        };
        let err = ingest_policy(&key, &Bytes::from_static(&[0, 1])).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// Allows everything on `o/a` and nothing on any other repository.
    struct OnlyA;

    #[async_trait]
    impl Authorizer for OnlyA {
        async fn authorize(
            &self,
            _: Option<&crate::auth::BearerToken>,
            repo: &str,
            _: Access,
        ) -> Result<Principal, crate::auth::AuthError> {
            if repo == "o/a" {
                Ok(Principal { id: "alice".into() })
            } else {
                Err(crate::auth::AuthError::Forbidden)
            }
        }
    }

    /// Counts the merges that reach the handler.
    #[derive(Default)]
    struct SpyMutations(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl MutationsHandler for SpyMutations {
        async fn merge(
            &self,
            _: &CallCtx,
            _: pb::MergeRequest,
        ) -> Result<pb::MergeResponse, Status> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(pb::MergeResponse::default())
        }
    }

    #[tokio::test]
    async fn a_merge_from_another_repository_needs_read_access_there() {
        let spy = Arc::new(SpyMutations::default());
        let handlers = Arc::new(RepoHandlers::new());
        handlers.set_mutations(spy.clone()).unwrap();
        let svc = RepoService::new(handlers, Arc::new(OnlyA), ServerInfo::default());
        let call = |head: Option<&str>| {
            let mut r = Request::new(pb::MergeRequest {
                repo: Some(pb::RepoRef {
                    name: "o/a".into(),
                    id_mode: 0,
                }),
                head_repo: head.map(|n| pb::RepoRef {
                    name: n.into(),
                    id_mode: 0,
                }),
                ..pb::MergeRequest::default()
            });
            r.metadata_mut()
                .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
            r
        };
        let denied = Repo::merge(&svc, call(Some("o/secret"))).await.unwrap_err();
        assert_eq!(denied.code(), tonic::Code::PermissionDenied);
        assert_eq!(spy.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        // A malformed head name is refused before the handler too.
        let bad = Repo::merge(&svc, call(Some("no-slash"))).await.unwrap_err();
        assert_eq!(bad.code(), tonic::Code::InvalidArgument);
        assert_eq!(spy.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Same repository, and no head repository, reach the handler.
        Repo::merge(&svc, call(Some("o/a"))).await.unwrap();
        Repo::merge(&svc, call(None)).await.unwrap();
        assert_eq!(spy.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn request_limits_are_enforced() {
        let fx = Fixture::start().await;
        let rpc = fx.client(Some("rw"));
        let key = pb::ObjKey::try_from(&ObjKey::File(FileId::of(b"x"))).unwrap();

        let too_many = pb::HasObjectsRequest {
            repo: rpc.repo_ref(),
            keys: vec![key.clone(); MAX_HAS_KEYS + 1],
        };
        let e = rpc
            .primary()
            .has_objects(rpc.req(too_many, None))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);

        let too_many = pb::GetObjectsRequest {
            repo: rpc.repo_ref(),
            keys: vec![key; MAX_GET_KEYS + 1],
        };
        let e = rpc
            .primary()
            .get_objects(rpc.req(stream::iter(vec![too_many]), None))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);

        // A later message may not switch repositories under the first message's authorisation.
        let mk = |repo: &str| pb::GetObjectsRequest {
            repo: Some(pb::RepoRef {
                name: repo.into(),
                id_mode: 0,
            }),
            keys: Vec::new(),
        };
        let mut s = rpc
            .primary()
            .get_objects(rpc.req(stream::iter(vec![mk(REPO), mk("owner/other")]), None))
            .await
            .unwrap()
            .into_inner();
        let e = s.message().await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);

        // Git id mode needs the git map, which the engine backend does not have.
        let git = pb::ResolveRequest {
            repo: Some(pb::RepoRef {
                name: REPO.into(),
                id_mode: IdMode::Git.to_wire(),
            }),
            revspecs: vec!["x".into()],
        };
        let e = rpc.primary().resolve(rpc.req(git, None)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn large_reads_split_into_bounded_messages() {
        let fx = Fixture::start().await;
        let rpc = fx.client(Some("rw"));
        // Incompressible 3.5 MiB files: five of them cannot fit in one 16 MiB message.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut keys = Vec::new();
        for _ in 0..5 {
            let data: Vec<u8> = (0..3_500_000)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    (seed >> 24) as u8
                })
                .collect();
            let key = ObjKey::File(FileId::of(&data));
            fx.objects.put(&key, encode_stored(&data)).await.unwrap();
            keys.push(key);
        }
        let req = pb::GetObjectsRequest {
            repo: rpc.repo_ref(),
            keys: keys
                .iter()
                .map(|k| pb::ObjKey::try_from(k).unwrap())
                .collect(),
        };
        let mut s = rpc
            .primary()
            .get_objects(rpc.req(stream::iter(vec![req]), None))
            .await
            .unwrap()
            .into_inner();
        let (mut messages, mut objects) = (0, 0);
        while let Some(m) = s.message().await.unwrap() {
            let bytes: usize = m.objects.iter().map(|o| o.data.len()).sum();
            assert!(bytes <= MAX_RESPONSE_BYTES);
            messages += 1;
            objects += m.objects.len();
        }
        assert_eq!((messages, objects), (2, 5));
    }
}
