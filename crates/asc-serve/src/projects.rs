//! The `Projects` service (PLAN G09, SPEC §12A.8): `ProjectGraph`, `Affected`, `ProjectHashes`.
//!
//! The service shell (`asc_rpc::projects::ProjectsService`) authorises read access to the
//! repository on every call and bounds the request; [`ServerProjects`] resolves the revisions,
//! asks the repository's engine and builds the answer. Nothing is read but manifests, lockfiles and
//! `.asc/projects.toml`, through the object store, never through a checkout. A bad configuration is
//! `INVALID_ARGUMENT` with `ErrorDetail.code = PROJECTS_CONFIG` and the file's path. The graph
//! warmer ([`crate::projects_warm`]) derives new tips in the background so that a request right
//! after a push finds its graph already built.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::history::HistoryApi;
use asc_engine::projects::{self, AffectedQuery};
use asc_engine::{commit, graph, tree};
use asc_graph::model::{Ecosystem, ProjectKey};
use asc_graph::select::Selector;
use asc_rpc::projects::{
    MAX_RESPONSE_BYTES, ProjectsHandler, affected_project, config_status, detect_issue, id_index,
    project_info, project_infos,
};
use asc_rpc::server::{CallCtx, MAX_STRING, wire};
use asc_store::objects;
use prost::Message;
use tokio::sync::Semaphore;
use tonic::Status;

use crate::idmode::{Eng, IdCodec};
use crate::projects_warm::{Stats, WarmSet, Warmer};
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};

/// Requests deriving or reading graphs at once, server wide. Further callers wait their turn.
const CONCURRENT_REQUESTS: usize = 16;
/// The configuration file a `PROJECTS_CONFIG` error names when no narrower path is known.
const CONFIG_PATH: &str = ".asc/projects.toml";

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn data_loss(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "stored data is damaged or missing");
    Status::data_loss("stored object is corrupt or missing")
}

fn store_status(e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(_) | objects::Error::Codec(..) => data_loss("object", &e),
        e => internal("object store", &e),
    }
}

fn tree_status(e: tree::Error) -> Status {
    match e {
        tree::Error::MissingTree(_) | tree::Error::Codec(_) => data_loss("tree", &e),
        tree::Error::TooLarge(_) | tree::Error::OutputTooLarge(_) => {
            Status::resource_exhausted(e.to_string())
        }
        tree::Error::PathTooLong(_) | tree::Error::TooDeep => Status::out_of_range(e.to_string()),
        tree::Error::Store(e) => store_status(e),
        e => internal("tree", &e),
    }
}

fn commit_status(e: commit::Error) -> Status {
    match e {
        commit::Error::MissingChangeset(id) => {
            Status::not_found(format!("changeset {id} is missing"))
        }
        commit::Error::Store(e) => store_status(e),
        e => internal("changeset", &e),
    }
}

fn graph_status(e: graph::Error) -> Status {
    match e {
        graph::Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        graph::Error::Query(m) => Status::invalid_argument(m),
        graph::Error::Remote(m) => Status::invalid_argument(m),
        graph::Error::Engine(e) => commit_status(e),
        e => internal("history", &e),
    }
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn config_path() -> RepoPath {
    RepoPath::new(CONFIG_PATH).expect("the configuration path is a valid path")
}

/// The status of a failed project query.
pub fn projects_status(e: projects::Error) -> Status {
    use projects::Error as E;
    match e {
        E::Config { path, message } => config_status(&path, &message),
        // Named inputs and globs come from the configuration in force.
        E::Resolve(e) => config_status(&config_path(), &e.to_string()),
        E::Pattern(e) => config_status(&config_path(), &e.to_string()),
        E::Affected(asc_graph::affected::AffectedError::Resolve(e)) => {
            config_status(&config_path(), &e.to_string())
        }
        E::Affected(asc_graph::affected::AffectedError::Pattern(e)) => {
            config_status(&config_path(), &e.to_string())
        }
        E::Select(e) | E::Affected(asc_graph::affected::AffectedError::Select(e)) => {
            Status::invalid_argument(e.to_string())
        }
        E::Tree(e) => tree_status(e),
        E::Commit(e) => commit_status(e),
        E::Store(e) => store_status(e),
        E::History(e) => graph_status(e),
        E::MissingFile(_) => data_loss("manifest", &e),
        E::NoMergeBase { .. } => Status::failed_precondition(e.to_string()),
        e => internal("projects", &e),
    }
}

fn selectors(v: &[String]) -> Result<Vec<Selector>, Status> {
    v.iter()
        .map(|s| Selector::parse(s).map_err(|e| Status::invalid_argument(e.to_string())))
        .collect()
}

/// Refuses an answer that would not fit one message.
fn bounded<M: Message>(m: M) -> Result<M, Status> {
    if m.encoded_len() > MAX_RESPONSE_BYTES {
        Err(Status::resource_exhausted(
            "the answer is too large; narrow it with `select` or `types`",
        ))
    } else {
        Ok(m)
    }
}

/// The `Projects` handler of a server.
pub struct ServerProjects {
    registry: Arc<Registry>,
    warm: Arc<WarmSet>,
    stats: Arc<Stats>,
    permits: Semaphore,
}

impl ServerProjects {
    pub fn new(registry: Arc<Registry>, warm: Arc<WarmSet>, stats: Arc<Stats>) -> Self {
        Self {
            registry,
            warm,
            stats,
            permits: Semaphore::new(CONCURRENT_REQUESTS),
        }
    }

    async fn open(&self, ctx: &CallCtx) -> Result<(Arc<RepoEntry>, IdCodec, Arc<Eng>), Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry);
        let engine = entry.backend.engine().clone();
        Ok((entry, ids, engine))
    }

    /// Resolves `spec` (`fallback` when empty) to a changeset.
    async fn rev(
        engine: &Eng,
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
        fallback: &str,
    ) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let spec = if spec.is_empty() { fallback } else { spec };
        let spec = ids.rev_in(spec, &entry.default_branch).await?;
        engine
            .resolve(std::slice::from_ref(&spec))
            .await
            .map_err(graph_status)?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    /// Counts the request as warm or cold, by whether the warmer reached `cs` first.
    fn note(&self, entry: &RepoEntry, cs: ChangesetId) {
        let counter = if self.warm.contains(entry.key.0, cs) {
            &self.stats.warm_requests
        } else {
            &self.stats.cold_requests
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    async fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, Status> {
        self.permits
            .acquire()
            .await
            .map_err(|e| internal("permits", &e))
    }
}

#[async_trait]
impl ProjectsHandler for ServerProjects {
    async fn project_graph(
        &self,
        ctx: &CallCtx,
        req: wire::ProjectGraphRequest,
        types: Vec<Ecosystem>,
    ) -> Result<wire::ProjectGraphResponse, Status> {
        let selectors = selectors(&req.select)?;
        let (entry, ids, engine) = self.open(ctx).await?;
        let cs = Self::rev(&engine, &ids, &entry, &req.rev, "HEAD").await?;
        let _permit = self.permit().await?;
        self.note(&entry, cs);
        let view = engine.project_graph(cs).await.map_err(projects_status)?;
        let g = &*view.graph;
        let mut keep: Vec<usize> = if selectors.is_empty() {
            (0..g.projects.len()).collect()
        } else {
            asc_graph::select::select(&g.projects, &selectors)
                .map_err(|e| Status::invalid_argument(e.to_string()))?
        };
        if !types.is_empty() {
            keep.retain(|&i| types.contains(&g.projects[i].key.eco));
        }
        bounded(wire::ProjectGraphResponse {
            changeset: ids.changeset_out(cs).await?,
            graph_id: Bytes::copy_from_slice(view.id.as_bytes()),
            projects: project_infos(g, Some(&keep)),
            issues: g.issues.iter().map(detect_issue).collect(),
        })
    }

    async fn affected(
        &self,
        ctx: &CallCtx,
        req: wire::AffectedRequest,
        types: Vec<Ecosystem>,
    ) -> Result<wire::AffectedResponse, Status> {
        // Parse the selectors now so that a bad one fails before any work.
        selectors(&req.select)?;
        let (entry, ids, engine) = self.open(ctx).await?;
        let base = Self::rev(&engine, &ids, &entry, &req.base, &entry.default_branch).await?;
        let head = Self::rev(&engine, &ids, &entry, &req.head, "HEAD").await?;
        let _permit = self.permit().await?;
        self.note(&entry, head);
        let query = AffectedQuery {
            base,
            head,
            no_merge_base: req.no_merge_base,
            target: (!req.target.is_empty()).then(|| req.target.clone()),
            ecosystems: types,
            select: req.select.clone(),
            with_hashes: req.with_hashes,
        };
        let got = engine.affected(&query).await.map_err(projects_status)?;
        let view = engine.project_graph(head).await.map_err(projects_status)?;
        let g = &*view.graph;
        let display = g.display_ids();
        let by_id = id_index(g, &display);
        let index: HashMap<&ProjectKey, usize> = g
            .projects
            .iter()
            .enumerate()
            .map(|(i, p)| (&p.key, i))
            .collect();
        let projects = got
            .projects
            .iter()
            .map(|p| {
                let info = match index.get(&p.project.key) {
                    Some(&i) => project_info(&g.projects[i], &p.project.id, &by_id),
                    // Not in the head graph (it cannot be affected there): name it only.
                    None => wire::ProjectInfo {
                        id: p.project.id.clone(),
                        ecosystem: asc_rpc::projects::ecosystem_to_wire(p.project.key.eco),
                        manifest: Bytes::copy_from_slice(p.project.key.manifest.as_bytes()),
                        ..Default::default()
                    },
                };
                affected_project(p, info)
            })
            .collect();
        bounded(wire::AffectedResponse {
            base: ids.changeset_out(got.base).await?,
            head: ids.changeset_out(got.head).await?,
            merge_base: ids.changeset_out(got.merge_base).await?,
            all: got.all,
            projects,
            removed: got.removed.iter().map(|r| r.id.clone()).collect(),
            unowned: got
                .unowned
                .iter()
                .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                .collect(),
            issues: got.issues.iter().map(detect_issue).collect(),
            changed_paths: u32::try_from(got.changed_paths).unwrap_or(u32::MAX),
        })
    }

    async fn project_hashes(
        &self,
        ctx: &CallCtx,
        req: wire::ProjectHashesRequest,
    ) -> Result<wire::ProjectHashesResponse, Status> {
        let selectors = selectors(&req.select)?;
        let (entry, ids, engine) = self.open(ctx).await?;
        let cs = Self::rev(&engine, &ids, &entry, &req.rev, "HEAD").await?;
        let _permit = self.permit().await?;
        self.note(&entry, cs);
        let view = engine.project_graph(cs).await.map_err(projects_status)?;
        let hashes = engine
            .input_hashes(cs, &req.target)
            .await
            .map_err(projects_status)?;
        let g = &*view.graph;
        let wanted: Option<Vec<usize>> = if selectors.is_empty() {
            None
        } else {
            Some(
                asc_graph::select::select(&g.projects, &selectors)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?,
            )
        };
        let display = g.display_ids();
        let mut by_project = HashMap::new();
        for (i, p) in g.projects.iter().enumerate() {
            if wanted.as_ref().is_some_and(|w| !w.contains(&i)) {
                continue;
            }
            if let Ok(h) = hashes.0.binary_search_by(|(k, _)| k.cmp(&p.key)) {
                by_project.insert(display[i].clone(), Bytes::copy_from_slice(&hashes.0[h].1));
            }
        }
        bounded(wire::ProjectHashesResponse {
            graph_id: Bytes::copy_from_slice(view.id.as_bytes()),
            by_project,
        })
    }
}

/// Registers the `Projects` service and returns the warmer to run (one line in `serve`).
pub fn register(ext: &Extensions, registry: &Arc<Registry>) -> Arc<Warmer> {
    let warm = Arc::new(WarmSet::new());
    let stats = Arc::new(Stats::default());
    ext.projects
        .set(Arc::new(ServerProjects::new(
            registry.clone(),
            warm.clone(),
            stats.clone(),
        )))
        .expect("the projects handler is registered once");
    Arc::new(Warmer::new(registry.clone(), warm, stats))
}

/// What `Version.features` adds for this service.
pub fn feature() -> String {
    asc_rpc::projects::FEATURE.to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use asc_core::git::ObjectKind;
    use asc_core::id::{RefTarget, TreeId};
    use asc_core::object::Change;
    use asc_core::path::RefName;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    use asc_engine::derive::{GitMapRow, GitMapSink};
    use asc_engine::projects::{GraphView, ProjectsApi};
    use asc_rpc::IdMode;
    use asc_rpc::auth::{Access, BearerToken, StaticTokens};
    use asc_rpc::client::RpcClient;
    use asc_rpc::errors::RpcError;
    use asc_rpc::projects::ProjectsService;
    use asc_rpc::server::{RepoName, ServerInfo, status_rpc_error};
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
    use tonic::Request;
    use uuid::Uuid;

    use super::*;
    use crate::ingest::testutil::{commit, put_file, registry, repo, sig};

    const TOKEN: &str = "secret";

    struct Fx {
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
        handler: Arc<ServerProjects>,
        stats: Arc<Stats>,
        warm: Arc<WarmSet>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let warm = Arc::new(WarmSet::new());
        let stats = Arc::new(Stats::default());
        let handler = Arc::new(ServerProjects::new(
            reg.clone(),
            warm.clone(),
            stats.clone(),
        ));
        Fx {
            reg,
            entry,
            handler,
            stats,
            warm,
        }
    }

    fn service(f: &Fx) -> ProjectsService {
        let auth = StaticTokens::new().with_token(TOKEN, "tester", Access::Read);
        ProjectsService::new(f.handler.clone(), Arc::new(auth), ServerInfo::default())
    }

    /// Serves the `Projects` service on a loopback port; returns a client for `o/r`.
    async fn client(f: &Fx) -> RpcClient {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let incoming = futures::stream::unfold(listener, |l| async move {
            let r = l.accept().await.map(|(s, _)| s);
            Some((r, l))
        });
        let svc = service(f).into_service();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(svc)
                .serve_with_incoming(incoming)
                .await;
        });
        RpcClient::connect(
            &format!("http://127.0.0.1:{port}"),
            "o/r",
            Some(BearerToken::new(TOKEN).unwrap()),
        )
        .unwrap()
    }

    fn ctx(mode: IdMode) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: mode,
            principal: asc_rpc::auth::Principal { id: "t".into() },
            op_id: None,
            client: None,
        }
    }

    const A: &[u8] =
        b"[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nb = { path = \"../b\" }\n";
    const B: &[u8] = b"[package]\nname = \"b\"\nversion = \"0.1.0\"\n";

    async fn base(f: &Fx) -> ChangesetId {
        commit(
            &f.entry,
            &[],
            &[
                ("a/Cargo.toml", A),
                ("a/src/lib.rs", b"//a"),
                ("b/Cargo.toml", B),
                ("b/src/lib.rs", b"//b"),
                ("notes.md", b"n"),
            ],
        )
        .await
    }

    /// A changeset on `parent` that applies `changes` (`None` deletes) to its tree.
    async fn change(
        f: &Fx,
        parent: ChangesetId,
        changes: Vec<(&str, Option<&[u8]>)>,
    ) -> ChangesetId {
        let engine = f.entry.backend.engine();
        let root = engine.get_changeset(parent).await.unwrap().root;
        let mut list = Vec::new();
        for (p, bytes) in changes {
            let c = match bytes {
                Some(b) => put_file(&f.entry, b).await,
                None => Change::Delete,
            };
            list.push((RepoPath::new(p).unwrap(), c));
        }
        list.sort_by(|a, b| a.0.cmp(&b.0));
        let root: TreeId = engine.apply(root, &list).await.unwrap();
        engine
            .create_changeset(
                NewChangeset {
                    parents: vec![parent],
                    author: sig(),
                    committer: sig(),
                    message: b"m".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    async fn set_ref(f: &Fx, name: &str, to: ChangesetId) {
        let rn = RefName::new(name.as_bytes().to_vec()).unwrap();
        let cur = f.reg.refs().get(f.entry.key, &rn).await.unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: rn,
                expect: cur.map_or(Expect::Absent, |c| Expect::Value(c.target)),
                new: Some(RefTarget::Changeset(to)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        f.reg.refs().update(f.entry.key, &txn).await.unwrap();
    }

    /// What of a graph the wire carries.
    fn shape(v: &GraphView) -> Vec<String> {
        v.graph
            .projects
            .iter()
            .map(|p| {
                format!(
                    "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
                    p.key, p.name, p.kind, p.root, p.deps, p.tags, p.gitlink, p.opaque
                )
            })
            .collect()
    }

    fn query(base: ChangesetId, head: ChangesetId) -> AffectedQuery {
        AffectedQuery {
            base,
            head,
            no_merge_base: false,
            target: None,
            ecosystems: vec![],
            select: vec![],
            with_hashes: false,
        }
    }

    #[tokio::test]
    async fn rpc_results_equal_the_local_engine() {
        let f = fx().await;
        let a = base(&f).await;
        let b = change(&f, a, vec![("b/src/lib.rs", Some(b"//b2"))]).await;
        let c = change(
            &f,
            b,
            vec![("b/Cargo.toml", None), ("notes.md", Some(b"m"))],
        )
        .await;
        let engine = f.entry.backend.engine().clone();
        let remote = client(&f).await;

        for cs in [a, b, c] {
            let (l, r) = (
                engine.project_graph(cs).await.unwrap(),
                remote.project_graph(cs).await.unwrap(),
            );
            assert_eq!(l.id, r.id);
            assert_eq!(l.changeset, r.changeset);
            assert_eq!(shape(&l), shape(&r));
            assert_eq!(l.graph.issues, r.graph.issues);
            assert_eq!(l.graph.display_ids(), r.graph.display_ids());
            for target in ["", "build"] {
                assert_eq!(
                    ProjectsApi::project_hashes(&*engine, cs, target)
                        .await
                        .unwrap(),
                    remote.project_hashes(cs, target).await.unwrap(),
                    "hashes of {cs} for {target:?}"
                );
            }
        }
        assert_eq!(
            engine.project_graph(a).await.unwrap().graph.projects.len(),
            2
        );

        for (base, head, no_mb, hashes, eco, select) in [
            (a, b, false, true, vec![], vec![]),
            (a, b, false, false, vec![Ecosystem::Rust], vec![]),
            (a, c, false, true, vec![], vec![]),
            (c, a, false, false, vec![], vec![]),
            (b, a, true, true, vec![], vec![]),
            (a, c, false, false, vec![], vec!["rust:a".to_owned()]),
        ] {
            let q = AffectedQuery {
                base,
                head,
                no_merge_base: no_mb,
                target: hashes.then(|| "build".to_owned()),
                ecosystems: eco,
                select,
                with_hashes: hashes,
            };
            assert_eq!(
                engine.affected(&q).await.unwrap(),
                remote.affected(&q).await.unwrap(),
                "{q:?}"
            );
        }
        // Comparing with the commit that deleted a project reports it as removed.
        let got = remote.affected(&query(a, c)).await.unwrap();
        let removed: Vec<_> = got.removed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(removed, ["rust:b"]);
    }

    #[tokio::test]
    async fn git_id_mode_speaks_git_oids() {
        let f = fx().await;
        let a = base(&f).await;
        let b = change(&f, a, vec![("b/src/lib.rs", Some(b"//b2"))]).await;
        let engine = f.entry.backend.engine().clone();
        let mut git = Vec::new();
        for c in [a, b] {
            let g = engine.git_commit_of(c).await.unwrap();
            f.entry
                .git_map
                .put(&[GitMapRow {
                    git: g,
                    kind: ObjectKind::Commit,
                    asc: *c.as_bytes(),
                }])
                .await
                .unwrap();
            git.push(g);
        }
        let h = &f.handler;
        let graph_req = |rev: String| wire::ProjectGraphRequest {
            rev,
            ..Default::default()
        };
        let got = h
            .project_graph(&ctx(IdMode::Git), graph_req(git[1].to_string()), vec![])
            .await
            .unwrap();
        assert_eq!(&got.changeset[..], git[1].as_bytes());
        let asc = h
            .project_graph(&ctx(IdMode::Asc), graph_req(b.to_string()), vec![])
            .await
            .unwrap();
        assert_eq!(&asc.changeset[..], b.as_bytes());
        assert_eq!(got.graph_id, asc.graph_id);
        assert_eq!(got.projects, asc.projects);

        let aff = h
            .affected(
                &ctx(IdMode::Git),
                wire::AffectedRequest {
                    base: git[0].to_string(),
                    head: git[1].to_string(),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(&aff.base[..], git[0].as_bytes());
        assert_eq!(&aff.head[..], git[1].as_bytes());
        assert_eq!(&aff.merge_base[..], git[0].as_bytes());
        let ids: Vec<_> = aff
            .projects
            .iter()
            .map(|p| p.project.as_ref().unwrap().id.as_str())
            .collect();
        assert_eq!(ids, ["rust:b", "rust:a"]);
    }

    #[tokio::test]
    async fn an_invalid_config_is_a_projects_config_error_naming_the_file() {
        let f = fx().await;
        let a = base(&f).await;
        let bad = change(&f, a, vec![(".asc/projects.toml", Some(b"ignore = [[["))]).await;
        let st = f
            .handler
            .project_graph(
                &ctx(IdMode::Asc),
                wire::ProjectGraphRequest {
                    rev: bad.to_string(),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(st.code(), tonic::Code::InvalidArgument);
        let Some(RpcError::ProjectsConfig(info)) = status_rpc_error(&st) else {
            panic!("no PROJECTS_CONFIG detail: {st:?}");
        };
        assert_eq!(info.paths, [".asc/projects.toml"]);
        // The client sees the same thing as a project error naming the path.
        let remote = client(&f).await;
        match remote.project_graph(bad).await.unwrap_err() {
            projects::Error::Config { path, .. } => {
                assert_eq!(path.to_string(), ".asc/projects.toml");
            }
            e => panic!("{e:?}"),
        }
        assert!(matches!(
            remote.affected(&query(a, bad)).await.unwrap_err(),
            projects::Error::Config { .. }
        ));
    }

    #[tokio::test]
    async fn select_and_types_narrow_the_graph() {
        let f = fx().await;
        let a = base(&f).await;
        let h = &f.handler;
        let req = |select: &[&str]| wire::ProjectGraphRequest {
            rev: a.to_string(),
            select: select.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        };
        let ids = |r: &wire::ProjectGraphResponse| {
            r.projects.iter().map(|p| p.id.clone()).collect::<Vec<_>>()
        };
        let all = h
            .project_graph(&ctx(IdMode::Asc), req(&[]), vec![])
            .await
            .unwrap();
        assert_eq!(ids(&all), ["rust:a", "rust:b"]);
        let one = h
            .project_graph(&ctx(IdMode::Asc), req(&["rust:b"]), vec![])
            .await
            .unwrap();
        assert_eq!(ids(&one), ["rust:b"]);
        // Dependencies still name projects outside the selection by id.
        let a_only = h
            .project_graph(&ctx(IdMode::Asc), req(&["rust:a"]), vec![])
            .await
            .unwrap();
        assert_eq!(a_only.projects[0].deps[0].id, "rust:b");
        let none = h
            .project_graph(&ctx(IdMode::Asc), req(&[]), vec![Ecosystem::Python])
            .await
            .unwrap();
        assert!(none.projects.is_empty());
        // A selector that matches nothing is an empty answer; the caller decides what that means.
        let nothing = h
            .project_graph(&ctx(IdMode::Asc), req(&["rust:nope"]), vec![])
            .await
            .unwrap();
        assert!(nothing.projects.is_empty());
        let malformed = h
            .project_graph(&ctx(IdMode::Asc), req(&[""]), vec![])
            .await
            .unwrap_err();
        assert_eq!(malformed.code(), tonic::Code::InvalidArgument);
        let hashes = h
            .project_hashes(
                &ctx(IdMode::Asc),
                wire::ProjectHashesRequest {
                    rev: a.to_string(),
                    select: vec!["rust:b".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hashes.by_project.keys().collect::<Vec<_>>(), ["rust:b"]);
        let missing = h
            .project_graph(
                &ctx(IdMode::Asc),
                wire::ProjectGraphRequest {
                    rev: "no-such-branch".into(),
                    ..Default::default()
                },
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(missing.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn every_call_is_authorised_and_bounded() {
        use asc_rpc::services::projects_server::Projects;
        let f = fx().await;
        let a = base(&f).await;
        let svc = service(&f);
        let repo = Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: IdMode::Asc.to_wire(),
        });
        let msg = |select: Vec<String>| wire::ProjectGraphRequest {
            repo: repo.clone(),
            rev: a.to_string(),
            types: vec![],
            select,
        };
        let with_token = |m: wire::ProjectGraphRequest, token: Option<&str>| {
            let mut r = Request::new(m);
            if let Some(t) = token {
                r.extensions_mut().insert(BearerToken::new(t).unwrap());
            }
            r
        };
        let code =
            |r: Result<tonic::Response<wire::ProjectGraphResponse>, Status>| r.unwrap_err().code();
        assert_eq!(
            code(svc.project_graph(with_token(msg(vec![]), None)).await),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            code(
                svc.project_graph(with_token(msg(vec![]), Some("wrong")))
                    .await
            ),
            tonic::Code::Unauthenticated
        );
        svc.project_graph(with_token(msg(vec![]), Some(TOKEN)))
            .await
            .unwrap();
        let too_many = vec!["rust:a".to_owned(); asc_rpc::projects::MAX_LIST + 1];
        assert_eq!(
            code(
                svc.project_graph(with_token(msg(too_many), Some(TOKEN)))
                    .await
            ),
            tonic::Code::InvalidArgument
        );
        let long = vec!["x".repeat(asc_rpc::projects::MAX_TEXT + 1)];
        assert_eq!(
            code(svc.project_graph(with_token(msg(long), Some(TOKEN))).await),
            tonic::Code::InvalidArgument
        );
        let mut no_repo = msg(vec![]);
        no_repo.repo = None;
        assert_eq!(
            code(svc.project_graph(with_token(no_repo, Some(TOKEN))).await),
            tonic::Code::InvalidArgument
        );
        let mut bad_eco = msg(vec![]);
        bad_eco.types = vec![0];
        assert_eq!(
            code(svc.project_graph(with_token(bad_eco, Some(TOKEN))).await),
            tonic::Code::InvalidArgument
        );
        let mut other = msg(vec![]);
        other.repo.as_mut().unwrap().name = "o/missing".into();
        assert_eq!(
            code(svc.project_graph(with_token(other, Some(TOKEN))).await),
            tonic::Code::NotFound
        );
        // The authorizer decides: a service whose authorizer knows no token refuses this one.
        let denied = ProjectsService::new(
            f.handler.clone(),
            Arc::new(StaticTokens::new()),
            ServerInfo::default(),
        );
        assert_eq!(
            code(
                denied
                    .project_graph(with_token(msg(vec![]), Some(TOKEN)))
                    .await
            ),
            tonic::Code::Unauthenticated
        );
    }

    #[tokio::test]
    async fn the_warmer_derives_pushed_tips_before_the_request_arrives() {
        let f = fx().await;
        let warmer = Warmer::new(f.reg.clone(), f.warm.clone(), f.stats.clone());
        warmer.start_at_head().await.unwrap();
        let a = base(&f).await;
        let b = change(&f, a, vec![("b/src/lib.rs", Some(b"//b2"))]).await;
        let bad = change(&f, b, vec![(".asc/projects.toml", Some(b"ignore = [[["))]).await;
        let unrelated = change(&f, b, vec![("notes.md", Some(b"zz"))]).await;
        // Nothing derives on the push itself.
        set_ref(&f, "refs/heads/topic", b).await;
        set_ref(&f, "refs/heads/master", a).await;
        set_ref(&f, "refs/heads/broken", bad).await;
        set_ref(&f, "refs/tags/v1", unrelated).await;
        set_ref(&f, "refs/pull/7/head", unrelated).await;
        assert_eq!(f.stats.derived(), 0);
        assert!(!f.warm.contains(f.entry.key.0, a));
        assert_eq!(warmer.run_once().await.unwrap(), 5);
        assert_eq!(f.stats.derived(), 3, "master, topic and the pull head");
        assert_eq!(
            f.stats.failed.load(Ordering::Relaxed),
            1,
            "the invalid config"
        );
        assert!(f.warm.contains(f.entry.key.0, a));
        assert!(f.warm.contains(f.entry.key.0, b));
        assert!(f.warm.contains(f.entry.key.0, unrelated));
        assert!(!f.warm.contains(f.entry.key.0, bad));
        assert_eq!(warmer.run_once().await.unwrap(), 0);

        let req = |cs: ChangesetId| wire::ProjectGraphRequest {
            rev: cs.to_string(),
            ..Default::default()
        };
        let h = &f.handler;
        h.project_graph(&ctx(IdMode::Asc), req(b), vec![])
            .await
            .unwrap();
        h.project_graph(&ctx(IdMode::Asc), req(a), vec![])
            .await
            .unwrap();
        assert_eq!((f.stats.warm_requests(), f.stats.cold_requests()), (2, 0));
        // A changeset nobody pushed a ref for derives on demand.
        let late = change(&f, b, vec![("a/Cargo.toml", Some(B))]).await;
        h.project_graph(&ctx(IdMode::Asc), req(late), vec![])
            .await
            .unwrap();
        assert_eq!((f.stats.warm_requests(), f.stats.cold_requests()), (2, 1));
    }

    #[tokio::test]
    async fn the_warmer_skips_history_that_existed_before_it_started() {
        let f = fx().await;
        let a = base(&f).await;
        set_ref(&f, "refs/heads/master", a).await;
        let warmer = Warmer::new(f.reg.clone(), f.warm.clone(), f.stats.clone());
        warmer.start_at_head().await.unwrap();
        assert_eq!(warmer.run_once().await.unwrap(), 0);
        assert_eq!(f.stats.derived(), 0);
    }

    /// 140 crates and a 1.1 MiB lockfile, the size of ASC's manifest set.
    fn asc_files() -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        let mut members = Vec::new();
        for i in 0..140 {
            let dep = if i > 0 {
                format!("c{} = {{ path = \"../c{}\" }}\n", i - 1, i - 1)
            } else {
                String::new()
            };
            files.push((
                format!("crates/c{i}/Cargo.toml"),
                format!("[package]\nname = \"c{i}\"\nversion = \"0.1.0\"\n\n[dependencies]\n{dep}")
                    .into_bytes(),
            ));
            files.push((
                format!("crates/c{i}/src/lib.rs"),
                format!("//{i}").into_bytes(),
            ));
            members.push(format!("\"crates/c{i}\""));
        }
        files.push((
            "Cargo.toml".into(),
            format!("[workspace]\nmembers = [{}]\n", members.join(", ")).into_bytes(),
        ));
        let mut lock = String::from("version = 4\n");
        for i in 0..140 {
            lock.push_str(&format!(
                "\n[[package]]\nname = \"c{i}\"\nversion = \"0.1.0\"\n"
            ));
        }
        let mut n = 0u64;
        while lock.len() < 1_150_000 {
            lock.push_str(&format!(
                "\n[[package]]\nname = \"dep{n:05}\"\nversion = \"1.{n}.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{:064x}\"\ndependencies = [\n \"dep{:05}\",\n]\n",
                n * 7919,
                n / 2
            ));
            n += 1;
        }
        files.push(("Cargo.lock".into(), lock.into_bytes()));
        files
    }

    #[tokio::test]
    async fn asc_sized_latency_budgets() {
        let f = fx().await;
        let owned = asc_files();
        let files: Vec<(&str, &[u8])> = owned
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let a = commit(&f.entry, &[], &files).await;
        let b = change(&f, a, vec![("crates/c0/src/lib.rs", Some(b"//changed"))]).await;
        let remote = client(&f).await;
        let q = query(a, b);
        let t = Instant::now();
        let cold = remote.affected(&q).await.unwrap();
        let cold_took = t.elapsed();
        let t = Instant::now();
        let warm = remote.affected(&q).await.unwrap();
        let warm_took = t.elapsed();
        assert_eq!(cold, warm);
        assert_eq!(
            cold.projects.len(),
            141,
            "c0, its dependents and the workspace"
        );
        // SPEC §12A.8: under 2 s cold (a new manifest set, parsing the lockfile), 300 ms warm.
        assert!(cold_took < Duration::from_secs(2), "cold {cold_took:?}");
        assert!(warm_took < Duration::from_millis(300), "warm {warm_took:?}");
    }

    #[test]
    fn the_feature_flag_is_projects_v1() {
        assert_eq!(feature(), "projects.v1");
    }
}
