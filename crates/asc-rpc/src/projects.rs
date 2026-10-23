//! The `asc.v1.Projects` service: server shell, wire conversions and the client side (SPEC §12A.8).
//!
//! [`ProjectsService`] authenticates and authorises every call (read access to the repository),
//! bounds the request and hands it to a [`ProjectsHandler`]. The conversions turn the engine's
//! project graph and affected result into wire messages ([`project_infos`], [`affected_project`])
//! and back. `RpcClient` implements the engine's [`ProjectsApi`] over the wire here. Everything the
//! server sends is untrusted: counts, lengths and ids are checked before they become domain values.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::projects::{
    AffectedQuery, AffectedResult, Error, GraphId, GraphView, ProjectsApi, Result,
};
use asc_graph::affected::{AffectedProject, ProjectRef, Reason};
use asc_graph::model::{
    DepKind, Ecosystem, Fingerprint, InputHashes, Issue, MAX_PROJECTS, Project, ProjectGraph,
    ProjectKey, ProjectKind, SetId,
};
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Extensions, Request, Response, Status};

use crate::auth::{Access, AuthInterceptor, Authorizer, Credential};
use crate::client::{CallError, RpcClient};
use crate::convert::{ConvertError, IdMode, WireId};
use crate::errors::{ErrorInfo, RpcError};
use crate::pb;
use crate::server::{
    AlreadyRegistered, CallCtx, ClientVersion, MAX_MESSAGE, MAX_STRING, RepoName, ServerInfo,
};
use crate::services::projects_client::ProjectsClient;
use crate::services::projects_server::{Projects, ProjectsServer};

/// The `VersionResponse.features` entry of this service.
pub const FEATURE: &str = "projects.v1";
/// Longest accepted revision, target or selector.
pub const MAX_TEXT: usize = MAX_STRING;
/// Most selectors or ecosystems in one request.
pub const MAX_LIST: usize = 256;
/// Largest response the server builds or the client accepts, in encoded bytes.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Most dependency edges the client accepts in one graph.
const MAX_EDGES: usize = 4_000_000;
/// Longest name, root or id a response may carry.
const MAX_FIELD: usize = 64 * 1024;
/// Most issues the client accepts in one response.
const MAX_ISSUES: usize = 1_000_000;

// ------------------------------------------------------------------------------ handler

/// What the server shell asks of the implementation: one method per RPC, after authorisation and
/// request bounds. `Projects` answers `UNIMPLEMENTED` until a handler is registered.
#[async_trait]
pub trait ProjectsHandler: Send + Sync {
    async fn project_graph(
        &self,
        ctx: &CallCtx,
        req: pb::ProjectGraphRequest,
        types: Vec<Ecosystem>,
    ) -> std::result::Result<pb::ProjectGraphResponse, Status>;

    async fn affected(
        &self,
        ctx: &CallCtx,
        req: pb::AffectedRequest,
        types: Vec<Ecosystem>,
    ) -> std::result::Result<pb::AffectedResponse, Status>;

    async fn project_hashes(
        &self,
        ctx: &CallCtx,
        req: pb::ProjectHashesRequest,
    ) -> std::result::Result<pb::ProjectHashesResponse, Status>;
}

/// The place a server's single [`ProjectsHandler`] is registered, once.
#[derive(Default)]
pub struct ProjectsSlot(OnceLock<Arc<dyn ProjectsHandler>>);

impl ProjectsSlot {
    /// Registers `handler`; a second registration is refused.
    pub fn set(
        &self,
        handler: Arc<dyn ProjectsHandler>,
    ) -> std::result::Result<(), AlreadyRegistered> {
        self.0
            .set(handler)
            .map_err(|_| AlreadyRegistered("projects"))
    }

    /// The registered handler.
    pub fn get(&self) -> Option<&Arc<dyn ProjectsHandler>> {
        self.0.get()
    }
}

// ------------------------------------------------------------------------------ service

/// The `asc.v1.Projects` tonic service.
pub struct ProjectsService {
    handler: Arc<dyn ProjectsHandler>,
    authorizer: Arc<dyn Authorizer>,
    info: ServerInfo,
}

impl ProjectsService {
    pub fn new(
        handler: Arc<dyn ProjectsHandler>,
        authorizer: Arc<dyn Authorizer>,
        info: ServerInfo,
    ) -> Self {
        Self {
            handler,
            authorizer,
            info,
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<ProjectsServer<Self>, AuthInterceptor> {
        let server = ProjectsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Validates the repository, checks the client version and authorises read access.
    async fn context(
        &self,
        meta: &MetadataMap,
        ext: &Extensions,
        repo: Option<&pb::RepoRef>,
    ) -> std::result::Result<CallCtx, Status> {
        let repo = repo.ok_or_else(|| Status::invalid_argument("missing repo"))?;
        let name = RepoName::parse(&repo.name)?;
        let id_mode =
            IdMode::from_wire(repo.id_mode).map_err(|e| Status::invalid_argument(e.to_string()))?;
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
            return Err(crate::server::rpc_status(&RpcError::ClientTooOld(info)));
        }
        let principal = self
            .authorizer
            .authorize_credential(ext.get::<Credential>(), name.as_str(), Access::Read)
            .await?;
        Ok(CallCtx {
            repo: name,
            id_mode,
            principal,
            op_id: None,
            client,
        })
    }
}

fn text(s: &str, what: &str) -> std::result::Result<(), Status> {
    if s.len() > MAX_TEXT {
        Err(Status::invalid_argument(format!("{what} is too long")))
    } else {
        Ok(())
    }
}

fn selectors(v: &[String]) -> std::result::Result<(), Status> {
    if v.len() > MAX_LIST {
        return Err(Status::invalid_argument("too many selectors"));
    }
    v.iter().try_for_each(|s| text(s, "selector"))
}

/// Decodes `--type` values; an unspecified or unknown ecosystem is refused.
pub fn ecosystems_in(v: &[i32]) -> std::result::Result<Vec<Ecosystem>, Status> {
    if v.len() > MAX_LIST {
        return Err(Status::invalid_argument("too many ecosystems"));
    }
    let mut out = Vec::with_capacity(v.len());
    for &e in v {
        let e = ecosystem_from_wire(e).map_err(|e| Status::invalid_argument(e.to_string()))?;
        if !out.contains(&e) {
            out.push(e);
        }
    }
    Ok(out)
}

#[tonic::async_trait]
impl Projects for ProjectsService {
    async fn project_graph(
        &self,
        request: Request<pb::ProjectGraphRequest>,
    ) -> std::result::Result<Response<pb::ProjectGraphResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.context(&meta, &ext, msg.repo.as_ref()).await?;
        text(&msg.rev, "revision")?;
        selectors(&msg.select)?;
        let types = ecosystems_in(&msg.types)?;
        self.handler
            .project_graph(&ctx, msg, types)
            .await
            .map(Response::new)
    }

    async fn affected(
        &self,
        request: Request<pb::AffectedRequest>,
    ) -> std::result::Result<Response<pb::AffectedResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.context(&meta, &ext, msg.repo.as_ref()).await?;
        text(&msg.base, "base")?;
        text(&msg.head, "head")?;
        text(&msg.target, "target")?;
        selectors(&msg.select)?;
        let types = ecosystems_in(&msg.types)?;
        self.handler
            .affected(&ctx, msg, types)
            .await
            .map(Response::new)
    }

    async fn project_hashes(
        &self,
        request: Request<pb::ProjectHashesRequest>,
    ) -> std::result::Result<Response<pb::ProjectHashesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self.context(&meta, &ext, msg.repo.as_ref()).await?;
        text(&msg.rev, "revision")?;
        text(&msg.target, "target")?;
        selectors(&msg.select)?;
        self.handler
            .project_hashes(&ctx, msg)
            .await
            .map(Response::new)
    }
}

/// The `PROJECTS_CONFIG` status for a bad `.asc/projects.toml` (SPEC §12A.8).
pub fn config_status(path: &RepoPath, message: &str) -> Status {
    let mut info = ErrorInfo::default();
    info.paths.push(path.to_string());
    let mut status = crate::server::rpc_status(&RpcError::ProjectsConfig(info));
    // `rpc_status` maps the code to FAILED_PRECONDITION; SPEC §12A.8 says INVALID_ARGUMENT.
    let detail = Bytes::copy_from_slice(status.details());
    let clean: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
    status = Status::with_details(
        tonic::Code::InvalidArgument,
        format!("{path}: {clean}"),
        detail,
    );
    status
}

// ------------------------------------------------------------------------------ enums

/// Decodes a wire ecosystem; `ECOSYSTEM_UNSPECIFIED` is not a value.
pub fn ecosystem_from_wire(v: i32) -> std::result::Result<Ecosystem, ConvertError> {
    match pb::Ecosystem::try_from(v) {
        Ok(pb::Ecosystem::Rust) => Ok(Ecosystem::Rust),
        Ok(pb::Ecosystem::Python) => Ok(Ecosystem::Python),
        Ok(pb::Ecosystem::Dotnet) => Ok(Ecosystem::Dotnet),
        Ok(pb::Ecosystem::Node) => Ok(Ecosystem::Node),
        Ok(pb::Ecosystem::Declared) => Ok(Ecosystem::Declared),
        _ => Err(ConvertError::BadEnum {
            what: "Ecosystem",
            value: v,
        }),
    }
}

/// The wire form of an ecosystem.
pub fn ecosystem_to_wire(e: Ecosystem) -> i32 {
    (match e {
        Ecosystem::Rust => pb::Ecosystem::Rust,
        Ecosystem::Python => pb::Ecosystem::Python,
        Ecosystem::Dotnet => pb::Ecosystem::Dotnet,
        Ecosystem::Node => pb::Ecosystem::Node,
        Ecosystem::Declared => pb::Ecosystem::Declared,
    }) as i32
}

fn kind_from_wire(v: i32) -> std::result::Result<ProjectKind, ConvertError> {
    match pb::ProjectKind::try_from(v) {
        Ok(pb::ProjectKind::Package) => Ok(ProjectKind::Package),
        Ok(pb::ProjectKind::Workspace) => Ok(ProjectKind::Workspace),
        Ok(pb::ProjectKind::Solution) => Ok(ProjectKind::Solution),
        Err(_) => Err(ConvertError::BadEnum {
            what: "ProjectKind",
            value: v,
        }),
    }
}

fn kind_to_wire(k: ProjectKind) -> i32 {
    (match k {
        ProjectKind::Package => pb::ProjectKind::Package,
        ProjectKind::Workspace => pb::ProjectKind::Workspace,
        ProjectKind::Solution => pb::ProjectKind::Solution,
    }) as i32
}

fn dep_kind_str(k: DepKind) -> &'static str {
    match k {
        DepKind::Normal => "normal",
        DepKind::Dev => "dev",
        DepKind::Build => "build",
        DepKind::Peer => "peer",
        DepKind::Optional => "optional",
        DepKind::Implicit => "implicit",
        DepKind::Contains => "contains",
        DepKind::Member => "member",
    }
}

fn dep_kind_parse(s: &str) -> Option<DepKind> {
    Some(match s {
        "normal" => DepKind::Normal,
        "dev" => DepKind::Dev,
        "build" => DepKind::Build,
        "peer" => DepKind::Peer,
        "optional" => DepKind::Optional,
        "implicit" => DepKind::Implicit,
        "contains" => DepKind::Contains,
        "member" => DepKind::Member,
        _ => return None,
    })
}

// ------------------------------------------------------------------------------ domain to wire

fn path_bytes(p: &RepoPath) -> Bytes {
    Bytes::copy_from_slice(p.as_bytes())
}

/// One project as a wire message. `ids` maps every key of the graph to its display id.
pub fn project_info(p: &Project, id: &str, ids: &HashMap<&ProjectKey, &str>) -> pb::ProjectInfo {
    pb::ProjectInfo {
        id: id.to_owned(),
        ecosystem: ecosystem_to_wire(p.key.eco),
        name: Bytes::copy_from_slice(&p.name),
        root: p.root.as_ref().map(path_bytes).unwrap_or_default(),
        manifest: path_bytes(&p.key.manifest),
        kind: kind_to_wire(p.kind),
        deps: p
            .deps
            .iter()
            .filter_map(|(k, kind)| {
                ids.get(k).map(|id| pb::ProjectDep {
                    id: (*id).to_owned(),
                    kind: dep_kind_str(*kind).to_owned(),
                })
            })
            .collect(),
        tags: p
            .tags
            .iter()
            .map(|t| String::from_utf8_lossy(t).into_owned())
            .collect(),
        gitlink: p.gitlink.as_ref().map(path_bytes).unwrap_or_default(),
        opaque: p.opaque,
    }
}

/// The display id of every project, by key.
pub fn id_index<'a>(
    graph: &'a ProjectGraph,
    display: &'a [String],
) -> HashMap<&'a ProjectKey, &'a str> {
    graph
        .projects
        .iter()
        .zip(display)
        .map(|(p, id)| (&p.key, id.as_str()))
        .collect()
}

/// The projects at `keep` (indices into `graph.projects`; `None` = all) in graph order.
pub fn project_infos(graph: &ProjectGraph, keep: Option<&[usize]>) -> Vec<pb::ProjectInfo> {
    let display = graph.display_ids();
    let ids = id_index(graph, &display);
    let one = |i: usize| project_info(&graph.projects[i], &display[i], &ids);
    match keep {
        Some(k) => k.iter().map(|&i| one(i)).collect(),
        None => (0..graph.projects.len()).map(one).collect(),
    }
}

/// An affected project as a wire message; `info` is its entry in the head graph.
pub fn affected_project(p: &AffectedProject, info: pb::ProjectInfo) -> pb::AffectedProject {
    pb::AffectedProject {
        project: Some(info),
        direct: p.direct,
        reasons: p.reasons.iter().map(ToString::to_string).collect(),
        input_hash: p
            .input_hash
            .map(|h| Bytes::copy_from_slice(h.as_bytes()))
            .unwrap_or_default(),
    }
}

/// An issue as a wire message.
pub fn detect_issue(i: &Issue) -> pb::DetectIssue {
    pb::DetectIssue {
        path: path_bytes(&i.path),
        message: String::from_utf8_lossy(&i.message).into_owned(),
    }
}

// ------------------------------------------------------------------------------ wire to domain

fn bad(what: &'static str) -> CallError {
    CallError::Protocol(format!("{what} in a Projects response"))
}

fn field(b: &[u8], what: &'static str) -> std::result::Result<(), CallError> {
    if b.len() > MAX_FIELD {
        Err(bad(what))
    } else {
        Ok(())
    }
}

fn opt_path(b: &[u8], what: &'static str) -> std::result::Result<Option<RepoPath>, CallError> {
    field(b, what)?;
    if b.is_empty() {
        Ok(None)
    } else {
        RepoPath::new(b).map(Some).map_err(|_| bad(what))
    }
}

fn req_path(b: &[u8], what: &'static str) -> std::result::Result<RepoPath, CallError> {
    field(b, what)?;
    RepoPath::new(b).map_err(|_| bad(what))
}

fn cs_of(b: &[u8], what: &'static str) -> std::result::Result<ChangesetId, CallError> {
    ChangesetId::from_slice(b).map_err(|_| bad(what))
}

fn issues_from_wire(v: &[pb::DetectIssue]) -> std::result::Result<Vec<Issue>, CallError> {
    if v.len() > MAX_ISSUES {
        return Err(bad("too many issues"));
    }
    v.iter()
        .map(|i| {
            field(i.message.as_bytes(), "issue message")?;
            Ok(Issue {
                path: req_path(&i.path, "issue path")?,
                message: i.message.as_bytes().to_vec(),
            })
        })
        .collect()
}

/// Rebuilds a project from its wire form; dependencies resolve through `keys` (display id to key).
fn project_from_wire(
    p: &pb::ProjectInfo,
    keys: &HashMap<&str, ProjectKey>,
) -> std::result::Result<Project, CallError> {
    field(&p.name, "project name")?;
    let eco = ecosystem_from_wire(p.ecosystem).map_err(|_| bad("ecosystem"))?;
    let key = ProjectKey {
        eco,
        manifest: req_path(&p.manifest, "manifest path")?,
    };
    let kind = kind_from_wire(p.kind).map_err(|_| bad("project kind"))?;
    let mut out = Project::new(key, p.name.to_vec(), kind);
    out.root = opt_path(&p.root, "project root")?;
    out.gitlink = opt_path(&p.gitlink, "gitlink")?;
    out.opaque = p.opaque;
    out.tags = p.tags.iter().map(|t| t.as_bytes().to_vec()).collect();
    for d in &p.deps {
        let key = keys
            .get(d.id.as_str())
            .ok_or_else(|| bad("dangling dependency"))?;
        let kind = dep_kind_parse(&d.kind).ok_or_else(|| bad("dependency kind"))?;
        out.deps.push((key.clone(), kind));
    }
    out.deps.sort();
    out.deps.dedup();
    Ok(out)
}

/// The domain graph of a `ProjectGraph` response. Config, fingerprints and extra inputs are not on
/// the wire: they are left at their defaults.
pub fn graph_from_wire(
    cs: ChangesetId,
    r: &pb::ProjectGraphResponse,
) -> std::result::Result<GraphView, CallError> {
    if r.projects.len() > MAX_PROJECTS {
        return Err(bad("too many projects"));
    }
    let mut keys: HashMap<&str, ProjectKey> = HashMap::with_capacity(r.projects.len());
    for p in &r.projects {
        field(&p.manifest, "manifest path")?;
        let eco = ecosystem_from_wire(p.ecosystem).map_err(|_| bad("ecosystem"))?;
        let manifest = req_path(&p.manifest, "manifest path")?;
        if keys.insert(&p.id, ProjectKey { eco, manifest }).is_some() {
            return Err(bad("duplicate project id"));
        }
    }
    if r.projects.iter().map(|p| p.deps.len()).sum::<usize>() > MAX_EDGES {
        return Err(bad("too many dependencies"));
    }
    let mut projects = r
        .projects
        .iter()
        .map(|p| project_from_wire(p, &keys))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    projects.sort_by(|a, b| a.key.cmp(&b.key));
    if projects.windows(2).any(|w| w[0].key == w[1].key) {
        return Err(bad("duplicate project"));
    }
    let id: SetId = postcard::from_bytes(&r.graph_id)
        .ok()
        .filter(|_| r.graph_id.len() == 32)
        .ok_or_else(|| bad("graph id"))?;
    Ok(GraphView {
        changeset: cs,
        id: GraphId::from(id),
        graph: Arc::new(ProjectGraph {
            projects,
            config: asc_graph::config::ResolvedConfig::default(),
            issues: issues_from_wire(&r.issues)?,
        }),
    })
}

/// Parses a reason in its display form (`Reason`'s `Display`).
pub fn reason_from_str(s: &str) -> Option<Reason> {
    let path = |p: &str| RepoPath::new(p).ok();
    Some(match s {
        "new" => Reason::New,
        "config" => Reason::Config,
        "lockfile" => Reason::Lockfile,
        "manifest" => Reason::Manifest,
        _ => {
            let (tag, rest) = s.split_once(':')?;
            match tag {
                "global" => Reason::Global(path(rest)?),
                "eco-global" => Reason::EcoGlobal(path(rest)?),
                "file" => Reason::File(path(rest)?),
                "gitlink" => Reason::Gitlink(path(rest)?),
                "dep" => Reason::Dep(rest.to_owned()),
                _ => return None,
            }
        }
    })
}

fn affected_from_wire(
    r: &pb::AffectedResponse,
    removed: Vec<ProjectRef>,
) -> std::result::Result<AffectedResult, CallError> {
    let mut projects = Vec::with_capacity(r.projects.len());
    for p in &r.projects {
        let info = p.project.as_ref().ok_or_else(|| bad("missing project"))?;
        let eco = ecosystem_from_wire(info.ecosystem).map_err(|_| bad("ecosystem"))?;
        let key = ProjectKey {
            eco,
            manifest: req_path(&info.manifest, "manifest path")?,
        };
        let mut reasons = p
            .reasons
            .iter()
            .map(|s| reason_from_str(s).ok_or_else(|| bad("reason")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        reasons.sort();
        reasons.dedup();
        let input_hash = match p.input_hash.len() {
            0 => None,
            32 => <[u8; 32]>::try_from(&p.input_hash[..])
                .ok()
                .map(Fingerprint::from),
            _ => return Err(bad("input hash")),
        };
        projects.push(AffectedProject {
            project: ProjectRef {
                key,
                id: info.id.clone(),
            },
            direct: p.direct,
            reasons,
            input_hash,
        });
    }
    Ok(AffectedResult {
        base: cs_of(&r.base, "base")?,
        head: cs_of(&r.head, "head")?,
        merge_base: cs_of(&r.merge_base, "merge base")?,
        all: r.all,
        projects,
        removed,
        unowned: r
            .unowned
            .iter()
            .map(|p| req_path(p, "unowned path"))
            .collect::<std::result::Result<_, _>>()?,
        issues: issues_from_wire(&r.issues)?,
        changed_paths: usize::try_from(r.changed_paths).map_err(|_| bad("changed paths"))?,
    })
}

// ------------------------------------------------------------------------------ client

impl RpcClient {
    fn projects_stub(&self) -> ProjectsClient<tonic::transport::Channel> {
        ProjectsClient::new(self.primary_channel().clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd)
    }

    fn projects_err(&self, e: CallError, operation: &'static str, rpc: &'static str) -> Error {
        if let CallError::Rpc(RpcError::ProjectsConfig(info)) = &e {
            let path = info
                .paths
                .first()
                .and_then(|p| RepoPath::new(p.as_str()).ok());
            if let Some(path) = path {
                return Error::Config {
                    message: e.to_string(),
                    path,
                };
            }
        }
        Error::History(self.graph_err(e, operation, rpc))
    }

    /// Display id to key for the graph at `cs`.
    async fn id_keys(&self, cs: ChangesetId) -> Result<BTreeMap<String, ProjectKey>> {
        let g = self.project_graph(cs).await?;
        let ids = g.graph.display_ids();
        Ok(ids
            .into_iter()
            .zip(g.graph.projects.iter())
            .map(|(id, p)| (id, p.key.clone()))
            .collect())
    }
}

#[async_trait]
impl ProjectsApi for RpcClient {
    async fn project_graph(&self, cs: ChangesetId) -> Result<GraphView> {
        const RPC: &str = "ProjectGraph";
        let req = pb::ProjectGraphRequest {
            repo: self.repo_ref(),
            rev: cs.to_string(),
            types: Vec::new(),
            select: Vec::new(),
        };
        let r = self
            .projects_stub()
            .project_graph(self.req(req, None))
            .await
            .map_err(|s| self.projects_err(s.into(), "project_graph", RPC))?
            .into_inner();
        graph_from_wire(cs, &r).map_err(|e| self.projects_err(e, "project_graph", RPC))
    }

    async fn affected(&self, q: &AffectedQuery) -> Result<AffectedResult> {
        const RPC: &str = "Affected";
        let req = pb::AffectedRequest {
            repo: self.repo_ref(),
            base: q.base.to_string(),
            head: q.head.to_string(),
            no_merge_base: q.no_merge_base,
            types: q.ecosystems.iter().map(|e| ecosystem_to_wire(*e)).collect(),
            target: q.target.clone().unwrap_or_default(),
            select: q.select.clone(),
            with_hashes: q.with_hashes,
        };
        let r = self
            .projects_stub()
            .affected(self.req(req, None))
            .await
            .map_err(|s| self.projects_err(s.into(), "affected", RPC))?
            .into_inner();
        // `removed` names projects of the merge base's graph by display id only.
        let removed = if r.removed.is_empty() {
            Vec::new()
        } else {
            let base = cs_of(&r.merge_base, "merge base")
                .map_err(|e| self.projects_err(e, "affected", RPC))?;
            let keys = self.id_keys(base).await?;
            r.removed
                .iter()
                .map(|id| {
                    keys.get(id)
                        .map(|key| ProjectRef {
                            key: key.clone(),
                            id: id.clone(),
                        })
                        .ok_or_else(|| {
                            self.projects_err(bad("unknown removed project"), "affected", RPC)
                        })
                })
                .collect::<Result<Vec<_>>>()?
        };
        affected_from_wire(&r, removed).map_err(|e| self.projects_err(e, "affected", RPC))
    }

    async fn project_hashes(&self, cs: ChangesetId, target: &str) -> Result<InputHashes> {
        const RPC: &str = "ProjectHashes";
        let req = pb::ProjectHashesRequest {
            repo: self.repo_ref(),
            rev: cs.to_string(),
            target: target.to_owned(),
            select: Vec::new(),
        };
        let r = self
            .projects_stub()
            .project_hashes(self.req(req, None))
            .await
            .map_err(|s| self.projects_err(s.into(), "project_hashes", RPC))?
            .into_inner();
        if r.by_project.len() > MAX_PROJECTS {
            return Err(self.projects_err(bad("too many hashes"), "project_hashes", RPC));
        }
        let keys = self.id_keys(cs).await?;
        let mut out = Vec::with_capacity(r.by_project.len());
        for (id, hash) in &r.by_project {
            let key = keys
                .get(id)
                .ok_or_else(|| self.projects_err(bad("unknown project"), "project_hashes", RPC))?;
            let hash = <[u8; 32]>::try_from(&hash[..])
                .map_err(|_| self.projects_err(bad("input hash"), "project_hashes", RPC))?;
            out.push((key.clone(), hash));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(InputHashes(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_round_trip_through_their_display_form() {
        let p = |s: &str| RepoPath::new(s).unwrap();
        for r in [
            Reason::Global(p("a/b")),
            Reason::EcoGlobal(p("Cargo.lock")),
            Reason::New,
            Reason::Config,
            Reason::Lockfile,
            Reason::Manifest,
            Reason::File(p("src/x.rs")),
            Reason::Gitlink(p("vendor/g")),
            Reason::Dep("rust:a".into()),
        ] {
            assert_eq!(reason_from_str(&r.to_string()), Some(r));
        }
        assert_eq!(reason_from_str("bogus:x"), None);
        assert_eq!(reason_from_str("file:/abs"), None);
    }

    #[test]
    fn ecosystem_enum_round_trips_and_refuses_unspecified() {
        for e in Ecosystem::ALL {
            assert_eq!(ecosystem_from_wire(ecosystem_to_wire(e)).unwrap(), e);
        }
        assert!(ecosystem_from_wire(0).is_err());
        assert!(ecosystem_from_wire(99).is_err());
        assert_eq!(
            ecosystems_in(&[1, 1, 4]).unwrap(),
            vec![Ecosystem::Rust, Ecosystem::Node]
        );
        assert!(ecosystems_in(&[0]).is_err());
        assert!(ecosystems_in(&vec![1; MAX_LIST + 1]).is_err());
    }

    #[test]
    fn config_status_is_invalid_argument_with_the_code_and_path() {
        let s = config_status(&RepoPath::new(".asc/projects.toml").unwrap(), "bad\nkey");
        assert_eq!(s.code(), tonic::Code::InvalidArgument);
        assert!(!s.message().contains('\n'));
        match crate::server::status_rpc_error(&s) {
            Some(RpcError::ProjectsConfig(i)) => {
                assert_eq!(i.paths, vec![".asc/projects.toml".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
    }
}
