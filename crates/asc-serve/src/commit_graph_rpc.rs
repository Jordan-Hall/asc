//! The `CommitGraph` RPC and the stats enricher of `diffstat.v1` (PLAN C29, SPEC §20.6).
//!
//! Pages come from [`asc_engine::commit_graph`]: generation-ordered, with server-computed
//! `column` and `edges` (lanes capped at 256, the rest in one `overflow` column) and the lane
//! state in the page token. A page costs O(rows): earlier pages are never read again. With
//! `stats` each row carries its `diffstat.v1`: rows with a memo get it, and at most
//! [`MAX_STATS_DERIVE_PER_PAGE`] rows without one are derived by the call; the rest come back
//! without `stats` (pending) and are filled by a later call, so a page of `limit` rows never
//! forces `limit` diffs. The registered [`StatsEnricher`] fills `LogEntry.stats` of other read
//! RPCs from memos that already exist and never derives one itself.
//!
//! Access is checked by the service (`Access::Read`) before the handler runs. Request sizes,
//! the page token and the number of refs a preset expands to are bounded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::commit;
use asc_engine::commit_graph::{self, DEFAULT_PAGE, MAX_PAGE, Start, lanes};
use asc_engine::derive::{self, diffstat};
use asc_engine::graph;
use asc_rpc::server::{CallCtx, InsightsHandler, MAX_LIST, MAX_STRING, RpcStream, wire};
use asc_store::objects;
use asc_store::refs::Refs;
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
#[cfg(test)]
use futures::stream::TryStreamExt;
use futures::stream::{self, StreamExt};
use tonic::Status;

use crate::commit_graph_markers as markers;
use crate::history::compare_rpc::ServerCompare;
use crate::history::linehist_rpc::ServerLineHistory;
use crate::history::repo_config::ServerRepoConfig;
use crate::idmode::{Eng, IdCodec};
use crate::read::{LogEntryEnricher, ReadHooks};
use crate::registry::{Extensions, Registry, RegistryError, RepoEntry};

/// Rows per response message.
const ROWS_PER_MESSAGE: usize = 100;
/// Concurrent changeset reads while building rows.
const FANOUT: usize = 16;
/// Concurrent stat derivations.
const STAT_FANOUT: usize = 8;
/// `diffstat.v1` derivations one `CommitGraph` call may start. A row past the cap without a
/// memo is returned with no `stats` (pending), earliest rows first, and a repeat of the call
/// finds the memos made by this one.
const MAX_STATS_DERIVE_PER_PAGE: usize = 16;
/// Longest accepted page token, base64 (a token is at most 3,073 bytes).
const MAX_TOKEN_CHARS: usize = 8192;
/// Refs a preset expands to, and refs read for labels.
const MAX_REFS: usize = MAX_LIST;

pub(crate) fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

pub(crate) fn data_loss(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "stored data is damaged or missing");
    Status::data_loss("stored object is corrupt or missing")
}

pub(crate) fn store_status(e: objects::Error) -> Status {
    match e {
        objects::Error::Corrupt(_) | objects::Error::Codec(..) => data_loss("object", &e),
        e => internal("object store", &e),
    }
}

pub(crate) fn commit_status(e: commit::Error) -> Status {
    match e {
        commit::Error::MissingChangeset(id) => {
            Status::not_found(format!("changeset {id} is missing"))
        }
        commit::Error::Store(e) => store_status(e),
        e => internal("changeset", &e),
    }
}

fn derive_status(e: derive::Error) -> Status {
    match e {
        derive::Error::Commit(e) => commit_status(e),
        derive::Error::Store(e) => store_status(e),
        derive::Error::Diff(asc_engine::diff::Error::Store(e)) => store_status(e),
        derive::Error::Diff(
            e @ (asc_engine::diff::Error::MissingFile(_) | asc_engine::diff::Error::Codec(_)),
        ) => data_loss("file", &e),
        e => internal("derive", &e),
    }
}

fn graph_status(e: graph::Error) -> Status {
    match e {
        graph::Error::Missing(id) => Status::not_found(format!("changeset {id} is missing")),
        graph::Error::Query(m) => Status::invalid_argument(m),
        graph::Error::Remote(m) => Status::invalid_argument(m),
        graph::Error::Unsupported(m) => Status::unimplemented(m),
        graph::Error::Engine(e) => commit_status(e),
        e => internal("history", &e),
    }
}

fn page_status(e: commit_graph::Error) -> Status {
    use commit_graph::Error as E;
    match e {
        E::Query(m) => Status::invalid_argument(m),
        e @ E::Expired => Status::failed_precondition(e.to_string()),
        e @ E::TooWide(_) => Status::resource_exhausted(e.to_string()),
        E::Commit(e) => commit_status(e),
        E::Derive(e) => derive_status(e),
        E::Store(e) => store_status(e),
    }
}

pub(crate) fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn refs_status(e: asc_store::refs::RefError) -> Status {
    internal("refs", &e)
}

/// The wire form of a stat.
pub fn stat_wire(s: &diffstat::DiffStatV1) -> wire::DiffStat {
    wire::DiffStat {
        files: s
            .files
            .iter()
            .map(|f| wire::FileStat {
                path: Bytes::copy_from_slice(f.path.as_bytes()),
                added: f.added,
                removed: f.removed,
                binary: f.binary,
            })
            .collect(),
    }
}

// ------------------------------------------------------------------------------ enricher

/// Fills `LogEntry.stats` from `diffstat.v1` memos that exist. It reads one presence batch per
/// call and never derives, so a log page stays cheap; `CommitGraph` with `stats` derives.
pub struct StatsEnricher;

#[async_trait]
impl LogEntryEnricher for StatsEnricher {
    async fn enrich(
        &self,
        repo: &RepoEntry,
        ids: &[ChangesetId],
        entries: &mut [wire::LogEntry],
    ) -> Result<(), Status> {
        let want: Vec<usize> = (0..entries.len().min(ids.len()))
            .filter(|&i| entries[i].stats.is_none())
            .collect();
        if want.is_empty() {
            return Ok(());
        }
        let keys: Vec<ChangesetId> = want.iter().map(|&i| ids[i]).collect();
        let got = repo
            .backend
            .engine()
            .diffstat_memos(&keys)
            .await
            .map_err(derive_status)?;
        for (i, stat) in want.into_iter().zip(got) {
            if let Some(s) = stat {
                entries[i].stats = Some(stat_wire(&s));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ handler

/// The `commit_graph` method of the `insights` handler group.
pub struct ServerCommitGraph {
    registry: Arc<Registry>,
    /// The `LineHistory` method (PLAN C30a); `UNIMPLEMENTED` until set.
    line_history: Option<ServerLineHistory>,
    /// The `Compare` method (PLAN C30b); `UNIMPLEMENTED` until set.
    compare: Option<ServerCompare>,
    /// The `RepoConfig` method (PLAN C30d); `UNIMPLEMENTED` until set.
    repo_config: Option<ServerRepoConfig>,
}

impl ServerCommitGraph {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            line_history: None,
            compare: None,
            repo_config: None,
        }
    }

    /// Serves `LineHistory` through `handler`.
    pub fn with_line_history(mut self, handler: ServerLineHistory) -> Self {
        self.line_history = Some(handler);
        self
    }

    /// Serves `Compare` through `handler`.
    pub fn with_compare(mut self, handler: ServerCompare) -> Self {
        self.compare = Some(handler);
        self
    }

    /// Serves `RepoConfig` through `handler`.
    pub fn with_repo_config(mut self, handler: ServerRepoConfig) -> Self {
        self.repo_config = Some(handler);
        self
    }

    async fn rev(
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
    ) -> Result<Option<ChangesetId>, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        let spec = if spec.is_empty() { "HEAD" } else { spec };
        ids.resolve(spec, &entry.default_branch, graph_status).await
    }

    /// Changeset targets of the refs under `prefix`, by name.
    async fn refs_under(
        &self,
        entry: &RepoEntry,
        prefix: &str,
    ) -> Result<Vec<(String, ChangesetId)>, Status> {
        let all = self
            .registry
            .refs()
            .list(entry.key, prefix)
            .await
            .map_err(refs_status)?;
        Ok(all
            .into_iter()
            .filter_map(|r| match r.target {
                RefTarget::Changeset(id) => {
                    Some((String::from_utf8_lossy(r.name.as_bytes()).into_owned(), id))
                }
                // An annotated tag would need its object read and peeled; its row shows
                // under the commit it names once that is a start point.
                RefTarget::Tag(_) => None,
            })
            .collect())
    }

    /// The start and hidden points of a first page.
    async fn starts(
        &self,
        ids: &IdCodec,
        entry: &RepoEntry,
        req: &wire::CommitGraphRequest,
    ) -> Result<(Vec<ChangesetId>, Vec<ChangesetId>), Status> {
        use wire::GraphPreset as P;
        if req.include.len() > MAX_LIST || req.exclude.len() > MAX_LIST {
            return Err(Status::invalid_argument("too many revisions"));
        }
        let preset = P::try_from(req.preset)
            .map_err(|_| Status::invalid_argument("unknown graph preset"))?;
        let trunk_ref = format!("refs/heads/{}", entry.default_branch);
        let mut include = Vec::new();
        for s in &req.include {
            match Self::rev(ids, entry, s).await? {
                Some(id) => include.push(id),
                None => return Err(Status::not_found(format!("unknown revision {s:?}"))),
            }
        }
        let mut exclude = Vec::new();
        for s in &req.exclude {
            match Self::rev(ids, entry, s).await? {
                Some(id) => exclude.push(id),
                None => return Err(Status::not_found(format!("unknown revision {s:?}"))),
            }
        }
        let heads = || self.refs_under(entry, "refs/heads/");
        let too_many = |n: usize| {
            (n > MAX_REFS).then(|| {
                Status::resource_exhausted("too many refs for this preset; pass `include`")
            })
        };
        if matches!(preset, P::Stack) {
            let heads = heads().await?;
            if let Some(trunk) = heads.iter().find(|(n, _)| *n == trunk_ref) {
                exclude.push(trunk.1);
            }
            if include.is_empty() {
                if let Some(e) = too_many(heads.len()) {
                    return Err(e);
                }
                include.extend(
                    heads
                        .iter()
                        .filter(|(n, _)| *n != trunk_ref)
                        .map(|(_, id)| *id),
                );
            }
        } else if include.is_empty() {
            match preset {
                P::Current => include.extend(Self::rev(ids, entry, "HEAD").await?),
                P::Smart => {
                    include.extend(Self::rev(ids, entry, "HEAD").await?);
                    let heads = heads().await?;
                    if let Some(e) = too_many(heads.len()) {
                        return Err(e);
                    }
                    include.extend(heads.into_iter().map(|(_, id)| id));
                }
                _ => {
                    let mut all = heads().await?;
                    all.extend(self.refs_under(entry, "refs/tags/").await?);
                    if let Some(e) = too_many(all.len()) {
                        return Err(e);
                    }
                    include.extend(all.into_iter().map(|(_, id)| id));
                }
            }
        }
        Ok((include, exclude))
    }

    /// Branch and tag names of the changesets in `page`, by changeset.
    async fn labels(
        &self,
        entry: &RepoEntry,
        page: &[lanes::Row],
    ) -> Result<HashMap<ChangesetId, Vec<String>>, Status> {
        let shown: HashSet<ChangesetId> = page.iter().map(|r| r.id).collect();
        let mut out: HashMap<ChangesetId, Vec<String>> = HashMap::new();
        if shown.is_empty() {
            return Ok(out);
        }
        for prefix in ["refs/heads/", "refs/tags/"] {
            for (name, id) in self
                .refs_under(entry, prefix)
                .await?
                .into_iter()
                .take(MAX_REFS)
            {
                if shown.contains(&id) {
                    out.entry(id).or_default().push(name);
                }
            }
        }
        Ok(out)
    }
}

/// What building the messages of one page needs.
struct Build {
    engine: Arc<Eng>,
    ids: IdCodec,
    lanes: bool,
    stats: bool,
    /// Derivations the call may still start, shared by the messages of one response.
    derive_left: AtomicUsize,
    labels: HashMap<ChangesetId, Vec<String>>,
}

/// What a row's stats come from.
enum Stat {
    /// Not asked for.
    Off,
    /// A memo that already exists.
    Memo(diffstat::DiffStatV1),
    /// Derive it now; the call's budget covers it.
    Derive,
    /// No memo and no budget left: the row goes out without `stats`.
    Pending,
}

impl Build {
    async fn row(&self, row: &lanes::Row, stat: Stat) -> Result<wire::GraphRow, Status> {
        let cs = self
            .engine
            .get_changeset(row.id)
            .await
            .map_err(commit_status)?;
        let git = self.ids.git_of(row.id).await?;
        let stats = match stat {
            Stat::Off | Stat::Pending => None,
            Stat::Memo(s) => Some(stat_wire(&s)),
            Stat::Derive => {
                let s = self.engine.diffstat(row.id).await.map_err(derive_status)?;
                Some(stat_wire(&s))
            }
        };
        let entry = wire::LogEntry {
            id: self.ids.changeset_out(row.id).await?,
            git: Bytes::copy_from_slice(git.as_bytes()),
            parents: self.ids.changesets_out(&cs.parents).await?,
            author: Some(wire::Signature::from(&cs.author)),
            committer: Some(wire::Signature::from(&cs.committer)),
            message: Bytes::copy_from_slice(&cs.message),
            r#gen: row.generation,
            stats,
            ..wire::LogEntry::default()
        };
        let mut edges = Vec::new();
        if self.lanes {
            for e in &row.edges {
                edges.push(wire::GraphEdge {
                    to_column: e.to_column,
                    parent: self.ids.changeset_out(e.parent).await?,
                });
            }
        }
        Ok(wire::GraphRow {
            entry: Some(entry),
            refs: self.labels.get(&row.id).cloned().unwrap_or_default(),
            column: if self.lanes { row.column } else { 0 },
            edges,
            overflow: self.lanes && row.overflow,
            ..wire::GraphRow::default()
        })
    }

    /// Decides each row's stat in row order, so the budget goes to the earliest rows: a memo
    /// when one exists, else a derivation while the call's budget lasts, else pending.
    async fn plan(&self, rows: &[lanes::Row]) -> Result<Vec<Stat>, Status> {
        if !self.stats {
            return Ok(rows.iter().map(|_| Stat::Off).collect());
        }
        let ids: Vec<ChangesetId> = rows.iter().map(|r| r.id).collect();
        let memos = self
            .engine
            .diffstat_memos(&ids)
            .await
            .map_err(derive_status)?;
        let claim = || {
            self.derive_left
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
        };
        Ok(memos
            .into_iter()
            .map(|m| match m {
                Some(s) => Stat::Memo(s),
                None if claim() => Stat::Derive,
                None => Stat::Pending,
            })
            .collect())
    }

    async fn message(&self, rows: &[lanes::Row]) -> Result<wire::CommitGraphResponse, Status> {
        let fanout = if self.stats { STAT_FANOUT } else { FANOUT };
        let mut plan = self.plan(rows).await?.into_iter();
        let mut out = Vec::with_capacity(rows.len());
        for group in rows.chunks(fanout) {
            let jobs: Vec<_> = group.iter().zip(plan.by_ref()).collect();
            out.extend(
                futures::future::try_join_all(jobs.into_iter().map(|(r, s)| self.row(r, s)))
                    .await?,
            );
        }
        let rows = out;
        Ok(wire::CommitGraphResponse {
            rows,
            ..wire::CommitGraphResponse::default()
        })
    }
}

#[async_trait]
impl InsightsHandler for ServerCommitGraph {
    async fn compare(
        &self,
        ctx: &CallCtx,
        req: wire::CompareRequest,
    ) -> Result<wire::CompareResponse, Status> {
        match &self.compare {
            Some(h) => h.compare(ctx, req).await,
            None => Err(Status::unimplemented("Compare is not available")),
        }
    }

    async fn repo_config(
        &self,
        ctx: &CallCtx,
        req: wire::RepoConfigRequest,
    ) -> Result<wire::RepoConfigResponse, Status> {
        match &self.repo_config {
            Some(h) => h.repo_config(ctx, req).await,
            None => Err(Status::unimplemented("RepoConfig is not available")),
        }
    }

    async fn line_history(
        &self,
        ctx: &CallCtx,
        req: wire::LineHistoryRequest,
    ) -> Result<RpcStream<wire::LogResponse>, Status> {
        match &self.line_history {
            Some(h) => h.line_history(ctx, req).await,
            None => Err(Status::unimplemented("LineHistory is not available")),
        }
    }

    async fn commit_graph(
        &self,
        ctx: &CallCtx,
        req: wire::CommitGraphRequest,
    ) -> Result<RpcStream<wire::CommitGraphResponse>, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        let limit = match req.limit as usize {
            0 => DEFAULT_PAGE,
            n => n.min(MAX_PAGE),
        };
        let token = if req.page_token.is_empty() {
            None
        } else if req.page_token.len() > MAX_TOKEN_CHARS {
            return Err(Status::invalid_argument("page token is too long"));
        } else {
            Some(
                URL_SAFE_NO_PAD
                    .decode(&req.page_token)
                    .map_err(|_| Status::invalid_argument("malformed page token"))?,
            )
        };
        let token_is_none = token.is_none();
        let (include, exclude) = match token {
            None => self.starts(&ids, &entry, &req).await?,
            Some(_) => (Vec::new(), Vec::new()),
        };
        let start = match &token {
            Some(t) => Start::Token(t),
            None => Start::New {
                include: &include,
                exclude: &exclude,
            },
        };
        let page = engine.graph_page(start, limit).await.map_err(page_status)?;
        let labels = self.labels(&entry, &page.rows).await?;
        // The first page also carries the minimap header (A2); later pages never do.
        let header = token_is_none.then(|| {
            let ids: Vec<ChangesetId> = page.rows.iter().map(|r| r.id).collect();
            markers::first_page(&ids, &labels, page.next.is_some())
        });
        let build = Arc::new(Build {
            labels,
            engine,
            ids,
            lanes: req.lanes,
            stats: req.stats,
            derive_left: AtomicUsize::new(MAX_STATS_DERIVE_PER_PAGE),
        });
        let next = page.next.map(|t| URL_SAFE_NO_PAD.encode(t));
        let rows = Arc::new(page.rows);
        let chunks = rows.len().div_ceil(ROWS_PER_MESSAGE).max(1);
        Ok(stream::try_unfold(0usize, move |i| {
            let (build, rows, next) = (build.clone(), rows.clone(), next.clone());
            let header = if i == 0 { header.clone() } else { None };
            async move {
                if i >= chunks {
                    return Ok(None);
                }
                let lo = (i * ROWS_PER_MESSAGE).min(rows.len());
                let hi = (lo + ROWS_PER_MESSAGE).min(rows.len());
                let mut msg = build.message(&rows[lo..hi]).await?;
                if let Some(h) = header {
                    markers::apply(&mut msg, h);
                }
                if i + 1 == chunks {
                    msg.next_page_token = next.unwrap_or_default();
                }
                Ok(Some((msg, i + 1)))
            }
        })
        .boxed())
    }
}

/// Registers the `insights` group's `CommitGraph` and the stats enricher (one line in
/// `register_all`).
pub fn register(ext: &Extensions, registry: &Arc<Registry>, hooks: &Arc<ReadHooks>) {
    hooks.add_enricher(Arc::new(StatsEnricher));
    ext.handlers
        .set_insights(Arc::new(
            ServerCommitGraph::new(registry.clone())
                .with_line_history(ServerLineHistory::new(registry.clone(), hooks.clone()))
                .with_compare(ServerCompare::new(registry.clone()))
                .with_repo_config(ServerRepoConfig::new(registry.clone(), hooks.clone())),
        ))
        .expect("the insights handler group is registered once");
}

#[cfg(test)]
mod tests {
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    struct Fx {
        handler: ServerCommitGraph,
        reg: Arc<Registry>,
        entry: Arc<RepoEntry>,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let entry = repo(&reg).await;
        Fx {
            handler: ServerCommitGraph::new(reg.clone()),
            reg,
            entry,
        }
    }

    fn ctx(mode: IdMode) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: mode,
            principal: Principal { id: "alice".into() },
            op_id: None,
            client: None,
        }
    }

    fn req(mode: IdMode) -> wire::CommitGraphRequest {
        wire::CommitGraphRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: mode.to_wire(),
            }),
            lanes: true,
            ..wire::CommitGraphRequest::default()
        }
    }

    async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: asc_core::path::RefName::new(name.as_bytes()).unwrap(),
                expect: Expect::Any,
                new: Some(RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::new_v4(),
        };
        f.reg.refs().update(f.entry.key, &txn).await.unwrap();
    }

    async fn call(
        f: &Fx,
        mode: IdMode,
        r: wire::CommitGraphRequest,
    ) -> Result<Vec<wire::CommitGraphResponse>, Status> {
        f.handler
            .commit_graph(&ctx(mode), r)
            .await?
            .try_collect()
            .await
    }

    fn rows(rs: &[wire::CommitGraphResponse]) -> Vec<&wire::GraphRow> {
        rs.iter().flat_map(|r| &r.rows).collect()
    }

    fn id_of(r: &wire::GraphRow) -> Vec<u8> {
        r.entry.as_ref().unwrap().id.to_vec()
    }

    /// master: a <- b <- d (merge of b and c); side: a <- c.
    async fn history(f: &Fx) -> [ChangesetId; 4] {
        let a = commit(&f.entry, &[], &[("f", b"1\n")]).await;
        let b = commit(&f.entry, &[a], &[("f", b"1\n2\n")]).await;
        let c = commit(&f.entry, &[a], &[("g", b"g\n")]).await;
        let d = commit(&f.entry, &[b, c], &[("h", b"h\n")]).await;
        set_ref(f, "refs/heads/master", d).await;
        set_ref(f, "refs/heads/side", c).await;
        [a, b, c, d]
    }

    #[tokio::test]
    async fn pages_carry_lanes_refs_and_stats() {
        let f = fx().await;
        let [a, b, c, d] = history(&f).await;
        let mut r = req(IdMode::Asc);
        r.stats = true;
        let got = call(&f, IdMode::Asc, r).await.unwrap();
        let rs = rows(&got);
        let order: Vec<_> = rs.iter().map(|r| id_of(r)).collect();
        assert_eq!(order.len(), 4);
        assert_eq!(order[0], d.as_bytes());
        assert_eq!(order[3], a.as_bytes());
        // The merge draws two lines; the first parent continues in lane 0.
        assert_eq!(rs[0].column, 0);
        assert_eq!(rs[0].edges.len(), 2);
        assert_eq!(rs[0].edges[0].parent.as_ref(), b.as_bytes());
        assert_eq!(rs[0].edges[0].to_column, 0);
        assert_eq!(rs[0].edges[1].parent.as_ref(), c.as_bytes());
        assert_eq!(rs[0].edges[1].to_column, 1);
        assert_eq!(rs[0].refs, ["refs/heads/master"]);
        let side = rs.iter().find(|r| id_of(r) == c.as_bytes()).unwrap();
        assert_eq!(side.refs, ["refs/heads/side"]);
        assert!(rs.iter().all(|r| !r.overflow));
        assert!(got.last().unwrap().next_page_token.is_empty());
        // Stats are against the first parent: the merge's own tree adds only `h` to b's.
        let merge = rs[0].entry.as_ref().unwrap().stats.as_ref().unwrap();
        let paths: Vec<_> = merge.files.iter().map(|x| x.path.to_vec()).collect();
        assert_eq!(paths, [b"h".to_vec()]);
        let root = rs[3].entry.as_ref().unwrap().stats.as_ref().unwrap();
        assert_eq!((root.files[0].added, root.files[0].removed), (1, 0));
    }

    #[tokio::test]
    async fn a_page_derives_at_most_the_cap_and_later_calls_finish_the_rest() {
        let f = fx().await;
        let n = MAX_STATS_DERIVE_PER_PAGE + 8;
        let mut tip = commit(
            &f.entry,
            &[],
            &[(
                "f", b"0
",
            )],
        )
        .await;
        let mut chain = vec![tip];
        for i in 1..n {
            tip = commit(
                &f.entry,
                &[tip],
                &[(
                    "f",
                    format!(
                        "{i}
"
                    )
                    .as_bytes(),
                )],
            )
            .await;
            chain.push(tip);
        }
        set_ref(&f, "refs/heads/master", tip).await;
        let engine = f.entry.backend.engine().clone();
        let memos = || async {
            let got = engine.diffstat_memos(&chain).await.unwrap();
            got.iter().filter(|m| m.is_some()).count()
        };
        let with_stats = |got: &[wire::CommitGraphResponse]| {
            rows(got)
                .iter()
                .filter(|r| r.entry.as_ref().unwrap().stats.is_some())
                .count()
        };
        assert_eq!(memos().await, 0);
        let mut r = req(IdMode::Asc);
        r.stats = true;
        let got = call(&f, IdMode::Asc, r.clone()).await.unwrap();
        assert_eq!(rows(&got).len(), n, "every row is still returned");
        assert_eq!(
            memos().await,
            MAX_STATS_DERIVE_PER_PAGE,
            "derivations are capped"
        );
        // The earliest rows get the stats; the rest are pending.
        assert_eq!(with_stats(&got), MAX_STATS_DERIVE_PER_PAGE);
        let shown = rows(&got);
        assert!(
            shown[..MAX_STATS_DERIVE_PER_PAGE].iter().all(|r| r
                .entry
                .as_ref()
                .unwrap()
                .stats
                .is_some())
        );
        // A repeat reuses those memos and derives the next batch.
        let again = call(&f, IdMode::Asc, r).await.unwrap();
        assert_eq!(memos().await, n);
        assert_eq!(with_stats(&again), n);
    }

    #[tokio::test]
    async fn stats_are_absent_unless_asked_and_lanes_can_be_dropped() {
        let f = fx().await;
        history(&f).await;
        let mut r = req(IdMode::Asc);
        r.lanes = false;
        let got = call(&f, IdMode::Asc, r).await.unwrap();
        for row in rows(&got) {
            assert!(row.entry.as_ref().unwrap().stats.is_none());
            assert_eq!(row.column, 0);
            assert!(row.edges.is_empty());
        }
    }

    #[tokio::test]
    async fn pages_join_up_through_tokens() {
        let f = fx().await;
        history(&f).await;
        let whole = call(&f, IdMode::Asc, req(IdMode::Asc)).await.unwrap();
        let mut seen = Vec::new();
        let mut token = String::new();
        let mut pages = 0;
        loop {
            let mut r = req(IdMode::Asc);
            r.limit = 1;
            r.page_token = token.clone();
            let got = call(&f, IdMode::Asc, r).await.unwrap();
            pages += 1;
            seen.extend(got.iter().flat_map(|m| m.rows.clone()));
            token = got.last().unwrap().next_page_token.clone();
            if token.is_empty() {
                break;
            }
            assert!(token.len() < 4096);
        }
        assert_eq!(pages, 4);
        let whole: Vec<_> = rows(&whole).into_iter().cloned().collect();
        assert_eq!(seen, whole);
    }

    #[tokio::test]
    async fn include_exclude_and_presets_choose_the_start_points() {
        let f = fx().await;
        let [a, b, c, d] = history(&f).await;
        // Everything but what `b` reaches.
        let mut r = req(IdMode::Asc);
        r.include = vec![d.to_string()];
        r.exclude = vec![b.to_string()];
        let got = call(&f, IdMode::Asc, r).await.unwrap();
        let ids: Vec<_> = rows(&got).iter().map(|r| id_of(r)).collect();
        assert_eq!(ids, [d.as_bytes().to_vec(), c.as_bytes().to_vec()]);
        // The stack preset shows the work that has not landed on the default branch.
        set_ref(&f, "refs/heads/feature", c).await;
        let mut r = req(IdMode::Asc);
        r.preset = wire::GraphPreset::Stack as i32;
        let got = call(&f, IdMode::Asc, r).await.unwrap();
        assert!(rows(&got).is_empty(), "master reaches every branch");
        let e = commit(&f.entry, &[d], &[("x", b"x\n")]).await;
        set_ref(&f, "refs/heads/feature", e).await;
        let mut r = req(IdMode::Asc);
        r.preset = wire::GraphPreset::Stack as i32;
        let got = call(&f, IdMode::Asc, r).await.unwrap();
        let ids: Vec<_> = rows(&got).iter().map(|r| id_of(r)).collect();
        assert_eq!(ids, [e.as_bytes().to_vec()]);
        // `current` follows the default branch only.
        let mut r = req(IdMode::Asc);
        r.preset = wire::GraphPreset::Current as i32;
        assert_eq!(rows(&call(&f, IdMode::Asc, r).await.unwrap()).len(), 4);
        let _ = a;
    }

    #[tokio::test]
    async fn git_mode_speaks_git_ids() {
        let f = fx().await;
        let [_, _, _, d] = history(&f).await;
        let got = call(&f, IdMode::Git, req(IdMode::Git)).await.unwrap();
        let rs = rows(&got);
        let top = rs[0].entry.as_ref().unwrap();
        let git = f.entry.backend.engine().git_commit_of(d).await.unwrap();
        assert_eq!(top.id.as_ref(), git.as_bytes());
        assert_eq!(rs[0].edges[0].parent.len(), 20);
    }

    #[tokio::test]
    async fn hostile_requests_are_refused() {
        let f = fx().await;
        history(&f).await;
        let mut r = req(IdMode::Asc);
        r.page_token = "!!not base64!!".into();
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::InvalidArgument
        );
        let mut r = req(IdMode::Asc);
        r.page_token = "A".repeat(MAX_TOKEN_CHARS + 1);
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::InvalidArgument
        );
        let mut r = req(IdMode::Asc);
        r.page_token = URL_SAFE_NO_PAD.encode([0u8, 1, 2]);
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::InvalidArgument
        );
        let mut r = req(IdMode::Asc);
        r.include = vec!["nope".into()];
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::NotFound
        );
        let mut r = req(IdMode::Asc);
        r.include = vec!["x".repeat(MAX_STRING + 1)];
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::InvalidArgument
        );
        let mut r = req(IdMode::Asc);
        r.include = vec!["master".into(); MAX_LIST + 1];
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::InvalidArgument
        );
        let mut r = req(IdMode::Asc);
        r.preset = 99;
        assert_eq!(
            call(&f, IdMode::Asc, r).await.err().unwrap().code(),
            tonic::Code::InvalidArgument
        );
        // An empty repository has an empty graph.
        let empty = fx().await;
        let got = call(&empty, IdMode::Asc, req(IdMode::Asc)).await.unwrap();
        assert!(rows(&got).is_empty());
    }

    #[tokio::test]
    async fn the_enricher_fills_stats_only_from_existing_memos() {
        let f = fx().await;
        let [a, b, ..] = history(&f).await;
        let mut entries = vec![wire::LogEntry::default(), wire::LogEntry::default()];
        StatsEnricher
            .enrich(&f.entry, &[a, b], &mut entries)
            .await
            .unwrap();
        assert!(
            entries.iter().all(|e| e.stats.is_none()),
            "nothing is derived"
        );
        f.entry.backend.engine().diffstat(b).await.unwrap();
        StatsEnricher
            .enrich(&f.entry, &[a, b], &mut entries)
            .await
            .unwrap();
        assert!(entries[0].stats.is_none());
        let s = entries[1].stats.as_ref().unwrap();
        assert_eq!(s.files[0].path.as_ref(), b"f");
        assert_eq!((s.files[0].added, s.files[0].removed), (1, 0));
        // An entry that already has stats is left alone.
        let before = entries[1].clone();
        StatsEnricher
            .enrich(&f.entry, &[a, b], &mut entries)
            .await
            .unwrap();
        assert_eq!(entries[1], before);
    }
}
