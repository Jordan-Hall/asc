//! `ImportGitPush` (SPEC §8.3, §14.1, §14.4; PLAN U48d).
//!
//! A git push reaches the server as a quarantine: a directory of loose objects and packs that
//! git has not yet admitted to the repository, plus the ref commands. [`ImportPush::import_git_push`]
//! turns it into asc history and moves the refs:
//!
//! 1. validate the commands, and refuse a stale `old` value (`REF_CONFLICT`) before any work;
//! 2. under the per-repo importer lock (SPEC §14.4 I6) and the GC fence (SPEC §3.6), read the
//!    quarantine with `gix`, import the commits git's map does not know yet (every commit is
//!    checked to round-trip to its git bytes, `gitcommit == oid`), check every added or changed
//!    path (SPEC §2.8, §25.4), and record the graph rows and the stack metadata in one
//!    transaction;
//! 3. call `UpdateRefs` with `UPDATE_MODE_FAST_FORWARD`, even on land branches (git cannot be
//!    told about rewrites, SPEC §8.5), so a non-fast-forward is git's `(fetch first)` rejection.
//!
//! Step 3 is the normal pipeline (authorise, idempotency, mode check, hooks, one SQL transaction
//! with the outbox). The Forgejo RPC sets `SKIP_PRE` because the caller is Forgejo's own
//! pre-receive; the in-process entry used by the git protocol layer (U82) never does.
//!
//! The RPC accepts only Forgejo's internal token, and only directories inside the configured
//! `forgejo.repo_root`, under the repository the call names. The paths are untrusted input: they
//! are canonicalised, checked for containment, and refused if they could add a line to the
//! alternates file the importer builds from them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid, RefTarget, TagId, TreeId};
use asc_core::object::{Change, Changeset, Kind, Tree};
use asc_core::path::{Name, RefName, RepoPath, casefold, checkout_safe, windows_valid};
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_git::import::{ImportError, ImportMap, ImportPolicy, ImportReport, Importer, Tip};
use asc_git::odb::{Odb, OdbError};
use asc_rpc::IdMode;
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{
    CallCtx, ImportPushHandler, MAX_REF_UPDATES, RefsHandler, RepoName, status_rpc_error, wire,
};
use asc_store::key::ObjKey;
use asc_store::refs::Refs;
use sqlx::{AnyPool, Row};
use tokio::sync::Mutex;
use tonic::Status;
use uuid::Uuid;

use crate::expire::ttl_eligible;
use crate::graph_sql::{self, NewNode};
use crate::mirror::import_lock;
use crate::mutations::ServerMutations;
use crate::registry::{Extensions, Registry, RepoEntry, RepoMode};
use crate::update::{PipelineHooks, ServerRefs, info, internal, rpc};

/// Most alternate object directories one push may name (git itself uses one or two).
pub const MAX_ALTERNATES: usize = 16;
/// Longest accepted directory path, in bytes.
pub const MAX_DIR_PATH_BYTES: usize = 4096;
/// Most new commits one push may add; a bigger push is refused rather than half-imported.
pub const MAX_NEW_COMMITS: usize = 2_000_000;
/// Longest ref name, in bytes.
pub const MAX_REF_NAME_BYTES: usize = 1024;
/// Push options per request, and the longest one (as `UpdateRefs`).
const MAX_PUSH_OPTIONS: usize = 64;
const MAX_PUSH_OPTION_LEN: usize = 1024;
/// Changeset ids per `cs_graph` lookup while walking new history.
const GRAPH_BATCH: usize = 256;
/// Bytes of an import error shown to the caller.
const MAX_ERROR_BYTES: usize = 512;
/// Most files and directories of a quarantine looked at when sizing it for the quota.
const MAX_QUARANTINE_ENTRIES: usize = 200_000;

// ------------------------------------------------------------------------------ inputs

/// The directories git holds a push in: the quarantine itself and the object directories it
/// reads through (the repository's own `objects`).
#[derive(Clone, Debug)]
pub struct Quarantine {
    pub object_dir: PathBuf,
    pub alternate_dirs: Vec<PathBuf>,
}

/// One ref command of a push; `None` is git's all-zero id (a create or a delete).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushCommand {
    pub name: String,
    pub old: Option<GitOid>,
    pub new: Option<GitOid>,
}

/// How a push is run.
#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    /// Send `UpdateRefs` with `SKIP_PRE`: only for Forgejo's internal token (SPEC §8.3 step 1).
    pub skip_pre: bool,
    /// `GIT_PUSH_OPTION_*`, forwarded to `UpdateRefs`.
    pub push_options: Vec<String>,
    /// `asc.ttl`, in seconds; accepted only when every ref may carry a TTL (SPEC §8.8).
    pub ttl_seconds: i64,
    /// `repos.git_import_grade`: apply the import path rule (SPEC §2.8) instead of the native
    /// one. The first push to an empty repository gets the import rule as well.
    pub import_grade: bool,
    /// The `via` of the ref log row (`git-push` for the git protocol layer); `None` is the RPC's
    /// `asc-push`.
    pub via: Option<&'static str>,
}

impl Quarantine {
    /// Refuses directories that are not absolute, or whose text could add a line to, or change
    /// the meaning of a line in, the alternates file built from them.
    fn check_shape(&self) -> Result<(), Status> {
        if self.alternate_dirs.len() > MAX_ALTERNATES {
            return Err(Status::invalid_argument("too many alternate directories"));
        }
        for dir in std::iter::once(&self.object_dir).chain(&self.alternate_dirs) {
            let text = dir
                .to_str()
                .ok_or_else(|| Status::invalid_argument("a directory is not UTF-8"))?;
            let bad = text.is_empty()
                || text.len() > MAX_DIR_PATH_BYTES
                || !dir.is_absolute()
                || text.contains(['\n', '\r', '\0'])
                || text.starts_with(['#', '"']);
            if bad {
                return Err(Status::invalid_argument("a directory path is not usable"));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ the importer

/// The `ImportGitPush` engine.
pub struct ImportPush {
    registry: Arc<Registry>,
    pipeline: Arc<PipelineHooks>,
    refs: ServerRefs,
    locks: StdMutex<HashMap<i64, Arc<Mutex<()>>>>,
}

impl ImportPush {
    pub fn new(registry: Arc<Registry>, pipeline: Arc<PipelineHooks>) -> Self {
        Self {
            refs: ServerRefs::new(registry.clone(), pipeline.clone()),
            registry,
            pipeline,
            locks: StdMutex::default(),
        }
    }

    fn repo_lock(&self, repo: i64) -> Arc<Mutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, v| Arc::strong_count(v) > 1);
        m.entry(repo).or_default().clone()
    }

    /// Imports `quarantine` and applies `commands` through `UpdateRefs`. `ctx` is the caller:
    /// its principal is authorised by the pipeline and recorded in the ref log, and its `op_id`
    /// makes a retry return the stored result.
    pub async fn import_git_push(
        &self,
        ctx: &CallCtx,
        quarantine: &Quarantine,
        commands: &[PushCommand],
        opts: &ImportOptions,
    ) -> Result<wire::ImportGitPushResponse, Status> {
        quarantine.check_shape()?;
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        check_commands(commands, opts)?;
        if opts.skip_pre && !self.pipeline.push_hooks().may_skip_pre(ctx) {
            return Err(Status::permission_denied(
                "hooks cannot be skipped by this caller",
            ));
        }
        let entry = self.registry.open(&ctx.repo).await.map_err(|e| match e {
            crate::registry::RegistryError::NotFound(_) => Status::not_found("no such repository"),
            e => internal("registry", &e),
        })?;
        // A mirror takes no pushes (SPEC §8.3 step 3): refuse before reading a byte.
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }
        // A retry of a push that committed returns what it returned.
        if let Some(done) = self.refs.stored(&entry, op_id).await? {
            return Ok(wire::ImportGitPushResponse {
                results: done.results,
            });
        }

        // The refs as git sees them must still be what the pusher saw (the `old` of each
        // command). Checked before the import, and pinned into the CAS below.
        let map = PushMap::new(self.registry.pool().clone(), &entry);
        // A non-shared branch of a branchless repository is the pusher's user ref (SPEC §21.5.1):
        // git saw it as `refs/heads/<n>`, the `refs` table holds it under `refs/users/`. `UpdateRefs`
        // classifies the same names again; this pass finds the stored value for the CAS.
        let policy = crate::refpolicy::RepoPolicy::load(self.registry.pool(), entry.key.0).await?;
        let owner = crate::refpolicy::owner_of(&ctx.principal);
        // A push whose answer was lost (the git child was killed or the client went away after
        // asc committed) is sent again under a new op id: it is the same push when every ref
        // already holds what the commands ask for because this principal's push stored it.
        if let Some(via) = opts.via
            && let Some(results) = self
                .already_applied(&entry, ctx, via, &map, &policy, owner, commands)
                .await?
        {
            return Ok(wire::ImportGitPushResponse { results });
        }
        let mut expects = Vec::with_capacity(commands.len());
        let mut quota_owner = None;
        // Whether every ref the push writes is a user ref, so that what it imports is the
        // pusher's alone to pay for.
        let mut only_user_refs = true;
        for c in commands {
            let class = policy.classify(owner, &c.name)?;
            if let (crate::refpolicy::Class::User { owner, .. }, Some(_)) = (&class, c.new) {
                quota_owner = Some(*owner);
            } else if c.new.is_some() {
                only_user_refs = false;
            }
            expects.push(
                self.check_old(&entry, &map, c, class.stored(&c.name))
                    .await?,
            );
        }

        // Quota (SPEC §21.3.7), part 1: an owner with no bytes left is refused before the import
        // writes an object, and so is a user-ref push whose pack alone is larger than what is
        // left (a pack is never larger than what it holds). The exact charge is taken by
        // `UpdateRefs` below, in the transaction that moves the refs. A delete-only push is never
        // refused here.
        let mut budget = None;
        if let Some(owner) = quota_owner {
            let left = self
                .pipeline
                .user_quota()
                .remaining(self.registry.pool(), entry.ns, owner)
                .await?;
            if only_user_refs {
                if quarantine_bytes(&quarantine.object_dir).await > left {
                    return Err(crate::refauthz::quota_status(
                        &crate::refauthz::QuotaDenied("user byte quota".into()),
                    ));
                }
                budget = Some(left);
            }
        }

        let windows_safe = self.refs.windows_safe(&entry).await?;
        let native = !opts.import_grade && self.has_refs(&entry).await?;
        // Part 2: the import records the changesets in the graph itself, each with a ledger row
        // naming the pusher, so `UpdateRefs` below finds them known and adds none; it charges the
        // owner for what the refs newly reach, in the transaction that moves them. If it refuses,
        // the changesets stay in the graph, unreferenced and unclaimed: whoever later names them
        // from a user ref pays for them, so they are not free to adopt.
        let imported = self
            .import(
                &entry,
                &map,
                quarantine,
                commands,
                (windows_safe, native),
                (owner.unwrap_or(0), budget),
            )
            .await?;

        // Step 3. The import's fence is released: `UpdateRefs` takes its own, and a queued
        // exclusive GC lock between the two would otherwise deadlock them.
        let mut updates = Vec::with_capacity(commands.len());
        for (c, expect) in commands.iter().zip(expects) {
            let new = c.new.map(|_| imported.target(&c.name)).transpose()?;
            updates.push(wire::RefUpdate {
                name: c.name.clone(),
                expect: Some(match expect {
                    Some(t) => wire::ref_update::Expect::Value(wire::RefTarget::from(&t)),
                    None => wire::ref_update::Expect::Absent(true),
                }),
                new: new.as_ref().map(wire::RefTarget::from),
                delete: new.is_none(),
            });
        }
        let req = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: 0,
            }),
            updates,
            mode: wire::UpdateMode::FastForward as i32,
            push_options: opts.push_options.clone(),
            hooks: if opts.skip_pre {
                wire::HookMode::SkipPre as i32
            } else {
                wire::HookMode::Run as i32
            },
            ttl_seconds: opts.ttl_seconds,
            ..wire::UpdateRefsRequest::default()
        };
        let mut asc_ctx = ctx.clone();
        asc_ctx.id_mode = IdMode::Asc;
        let result = match opts.via {
            Some(via) => self.refs.update_via(&asc_ctx, req, via).await,
            None => self.refs.update_refs(&asc_ctx, req).await,
        };
        match result {
            Ok(done) => Ok(wire::ImportGitPushResponse {
                results: done.results,
            }),
            Err(s) => Err(fetch_first(s)),
        }
    }

    /// The results of `commands` when each of them was already applied by an earlier push of the
    /// same principal through `via`: its ref holds the requested value (or is absent for a
    /// deletion) and the newest ref log row of the ref is that principal's `via` update to it.
    /// `None` when any command was not, so that the push runs as a new one. Nothing is written,
    /// and no hook runs again.
    #[allow(clippy::too_many_arguments)]
    async fn already_applied(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        via: &str,
        map: &PushMap,
        policy: &crate::refpolicy::RepoPolicy,
        owner: Option<i64>,
        commands: &[PushCommand],
    ) -> Result<Option<Vec<wire::RefResult>>, Status> {
        let mut results = Vec::with_capacity(commands.len());
        for c in commands {
            let class = policy.classify(owner, &c.name)?;
            let name = RefName::new(class.stored(&c.name).as_bytes())
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let refs = self.registry.refs();
            let current = refs
                .get(entry.key, &name)
                .await
                .map_err(|e| internal("refs", &e))?
                .map(|e| e.target);
            let wanted = match c.new {
                Some(oid) => match map.target_of(oid).await? {
                    Some(t) => Some(t),
                    None => return Ok(None),
                },
                None => None,
            };
            if wanted != current {
                return Ok(None);
            }
            let log = refs
                .log(entry.key, &name, 1)
                .await
                .map_err(|e| internal("ref log", &e))?;
            let by_this_push = log.first().is_some_and(|row| {
                row.new == current && row.actor.id == ctx.principal.id && row.actor.via == via
            });
            if !by_this_push {
                return Ok(None);
            }
            let old = match c.old {
                Some(oid) => map.target_of(oid).await?,
                None => None,
            };
            results.push(wire::RefResult {
                name: c.name.clone(),
                old: old.as_ref().map(wire::RefTarget::from),
                new: current.as_ref().map(wire::RefTarget::from),
                user: class.is_user(),
            });
        }
        Ok(Some(results))
    }

    /// The target the ref held when the pusher read it must equal `command.old`; returns the
    /// target to pin in the CAS (`None` for an absent ref).
    async fn check_old(
        &self,
        entry: &RepoEntry,
        map: &PushMap,
        command: &PushCommand,
        stored: &str,
    ) -> Result<Option<RefTarget>, Status> {
        let name =
            RefName::new(stored.as_bytes()).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let current = self
            .registry
            .refs()
            .get(entry.key, &name)
            .await
            .map_err(|e| internal("refs", &e))?
            .map(|e| e.target);
        let wanted = match command.old {
            None => None,
            Some(oid) => match map.target_of(oid).await? {
                Some(t) => Some(t),
                // A value asc has never seen cannot be the ref's value.
                None => return Err(stale(&command.name, current)),
            },
        };
        if wanted != current {
            return Err(stale(&command.name, current));
        }
        Ok(current)
    }

    async fn has_refs(&self, entry: &RepoEntry) -> Result<bool, Status> {
        sqlx::query("SELECT 1 AS one FROM refs WHERE repo_id = $1 LIMIT 1")
            .bind(entry.key.0)
            .fetch_optional(self.registry.pool())
            .await
            .map(|r| r.is_some())
            .map_err(|e| internal("refs", &e))
    }

    /// Steps 2 of the module doc: import, check paths, record the graph.
    async fn import(
        &self,
        entry: &Arc<RepoEntry>,
        map: &PushMap,
        quarantine: &Quarantine,
        commands: &[PushCommand],
        (windows_safe, native): (bool, bool),
        (ingester, budget): (i64, Option<u64>),
    ) -> Result<Imported, Status> {
        let tips: Vec<Tip> = commands
            .iter()
            .filter_map(|c| {
                c.new.map(|oid| Tip {
                    name: c.name.as_bytes().to_vec(),
                    oid,
                })
            })
            .collect();
        if tips.is_empty() {
            return Ok(Imported::default());
        }
        let pool = self.registry.pool();
        let lock = self.repo_lock(entry.key.0);
        let _one = lock.lock().await;
        let _gc = self.pipeline.gc().shared().await.map_err(|e| {
            tracing::warn!(error = %e, "gc fence unavailable");
            Status::unavailable("garbage-collection fence unavailable; retry")
        })?;
        let lock_tx = import_lock(pool, entry.key.0)
            .await
            .map_err(|e| internal("import lock", &e))?;

        let report = import_tips(entry.clone(), map.clone(), quarantine.clone(), tips).await?;
        let imported = Imported::of(&report, commands)?;

        let nodes = new_graph_nodes(entry, pool, &imported.changesets).await?;
        let ids: Vec<ChangesetId> = nodes.iter().map(|n| n.id).collect();
        let costs = self.check_paths(entry, &ids, windows_safe, native).await?;
        let cost = costs.iter().fold(0u64, |a, (_, b)| a.saturating_add(*b));
        if budget.is_some_and(|left| cost > left) {
            return Err(crate::refauthz::quota_status(
                &crate::refauthz::QuotaDenied("user byte quota".into()),
            ));
        }

        let mut tx = match lock_tx {
            Some(t) => t,
            None => self
                .registry
                .refs()
                .begin()
                .await
                .map_err(|e| internal("refs", &e))?,
        };
        self.registry.guard(&mut tx, entry).await?;
        let rows: Vec<NewNode<'_>> = nodes
            .iter()
            .map(|n| NewNode {
                id: n.id,
                generation: n.generation,
                parents: &n.parents,
            })
            .collect();
        graph_sql::insert_nodes(&mut tx, entry.ns, &rows)
            .await
            .map_err(|e| internal("cs_graph", &e))?;
        crate::cloud::quota::record(&mut tx, entry.ns, ingester, &costs)
            .await
            .map_err(|e| internal("cs_ledger", &e))?;
        if !ids.is_empty() {
            let mut closure: Vec<ObjKey> = ids.iter().map(|&c| ObjKey::Changeset(c)).collect();
            closure.extend(report.tags.iter().map(|(_, t)| ObjKey::Tag(*t)));
            self.pipeline
                .observer()
                .on_ingested(&mut tx, entry.key, &ids, &closure)
                .await
                .map_err(|e| crate::update::ingest_status(e.into()))?;
        }
        tx.commit().await.map_err(|e| internal("commit", &e))?;
        Ok(imported)
    }

    /// SPEC §2.8 and §25.4 step 6 for every added or changed path of the new changesets:
    /// `checkout_safe` in every mode; with the native rule in a `windows_safe` repository, also
    /// Windows validity and case collisions.
    async fn check_paths(
        &self,
        entry: &RepoEntry,
        ids: &[ChangesetId],
        windows_safe: bool,
        native: bool,
    ) -> Result<Vec<(ChangesetId, u64)>, Status> {
        let engine = entry.backend.engine();
        let mut costs = Vec::with_capacity(ids.len());
        for &id in ids {
            let cs = engine
                .get_changeset(id)
                .await
                .map_err(|e| internal("changeset", &e))?;
            for (path, change) in cs.changes.as_slice() {
                if !matches!(change, Change::Delete) && !checkout_safe(path) {
                    return Err(path_policy(path, "not safe to check out"));
                }
            }
            if native && windows_safe {
                check_windows_paths(entry, &cs).await?;
            }
            costs.push((id, crate::cloud::quota::changeset_cost(&cs)));
        }
        Ok(costs)
    }
}

/// Bytes of the files under a push's quarantine directory (loose objects and packs), at most
/// [`MAX_QUARANTINE_ENTRIES`] entries looked at; more counts as unbounded.
async fn quarantine_bytes(dir: &Path) -> u64 {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut total = 0u64;
        let mut seen = 0usize;
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in read.flatten() {
                seen += 1;
                if seen > MAX_QUARANTINE_ENTRIES {
                    return u64::MAX;
                }
                let Ok(meta) = e.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(e.path());
                } else {
                    total = total.saturating_add(meta.len());
                }
            }
        }
        total
    })
    .await
    .unwrap_or(u64::MAX)
}

/// What an import produced: the asc target of each pushed ref, and the changesets to record.
#[derive(Default)]
struct Imported {
    targets: HashMap<String, RefTarget>,
    changesets: Vec<ChangesetId>,
}

impl Imported {
    /// Reads the report. A pushed ref that asc cannot represent (a tag of a tree, a tag that does
    /// not round-trip) fails the whole push: the pusher must not believe it was stored.
    fn of(report: &ImportReport, commands: &[PushCommand]) -> Result<Self, Status> {
        let refuse = |name: &[u8], why: &str| {
            let name = String::from_utf8_lossy(name);
            rpc(RpcError::InvalidObject(info(vec![format!(
                "{name}: {why}"
            )])))
        };
        if let Some(s) = report.skipped_refs.first() {
            return Err(refuse(
                &s.name,
                &format!("asc cannot represent a ref to a {}", s.target),
            ));
        }
        if let Some((name, _, why)) = report.rejected_tags.first() {
            return Err(refuse(name, &truncate(why)));
        }
        let mut targets = HashMap::new();
        for (name, cs) in &report.refs {
            targets.insert(
                String::from_utf8_lossy(name).into_owned(),
                RefTarget::Changeset(*cs),
            );
        }
        // An annotated tag ref names its `TagRecord`; the commit it peels to is only its target.
        for (name, tag) in &report.tags {
            targets.insert(
                String::from_utf8_lossy(name).into_owned(),
                RefTarget::Tag(*tag),
            );
        }
        for c in commands.iter().filter(|c| c.new.is_some()) {
            if !targets.contains_key(&c.name) {
                return Err(refuse(c.name.as_bytes(), "the push did not import"));
            }
        }
        let changesets = report
            .refs
            .iter()
            .map(|(_, cs)| *cs)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        Ok(Self {
            targets,
            changesets,
        })
    }

    fn target(&self, name: &str) -> Result<RefTarget, Status> {
        self.targets
            .get(name)
            .copied()
            .ok_or_else(|| internal("import", &"a pushed ref has no target"))
    }
}

// ------------------------------------------------------------------------------ validation

fn check_commands(commands: &[PushCommand], opts: &ImportOptions) -> Result<(), Status> {
    let bad = |m: &str| Status::invalid_argument(m.to_owned());
    if commands.is_empty() || commands.len() > MAX_REF_UPDATES {
        return Err(bad("bad number of ref updates"));
    }
    if opts.ttl_seconds < 0 {
        return Err(bad("negative ttl"));
    }
    if opts.push_options.len() > MAX_PUSH_OPTIONS
        || opts
            .push_options
            .iter()
            .any(|o| o.len() > MAX_PUSH_OPTION_LEN)
    {
        return Err(bad("too many or too long push options"));
    }
    let mut seen = HashSet::new();
    for c in commands {
        if c.name.len() > MAX_REF_NAME_BYTES {
            return Err(bad("a ref name is too long"));
        }
        RefName::new(c.name.as_bytes()).map_err(|e| Status::invalid_argument(e.to_string()))?;
        if !seen.insert(c.name.as_str()) {
            return Err(bad("a ref is updated twice"));
        }
        if c.old.is_none() && c.new.is_none() {
            return Err(bad("an update needs an old or a new value"));
        }
        if opts.ttl_seconds > 0 && c.new.is_some() && !ttl_eligible(&c.name) {
            return Err(bad(
                "a ttl is accepted only for agent branches and snapshots",
            ));
        }
    }
    Ok(())
}

fn truncate(s: &str) -> String {
    let mut end = s.len().min(MAX_ERROR_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn stale(name: &str, actual: Option<RefTarget>) -> Status {
    rpc(RpcError::RefConflict(ErrorInfo {
        paths: vec![name.to_owned()],
        actual: actual.map_or_else(bytes::Bytes::new, |t| {
            bytes::Bytes::copy_from_slice(&t.to_bytes())
        }),
        conflicts: Vec::new(),
    }))
}

fn path_policy(path: &RepoPath, why: &'static str) -> Status {
    tracing::debug!(%path, why, "push rejected");
    rpc(RpcError::PathPolicy(info(vec![path.to_string()])))
}

/// Git's `! [rejected] <ref> (fetch first)` for a non-fast-forward; every other status as is.
fn fetch_first(status: Status) -> Status {
    let Some(RpcError::NotFastForward(i)) = status_rpc_error(&status) else {
        return status;
    };
    let name = i.paths.first().map_or("", String::as_str);
    Status::with_details(
        status.code(),
        format!("! [rejected] {name} (fetch first)"),
        bytes::Bytes::copy_from_slice(status.details()),
    )
}

fn import_status(e: ImportError) -> Status {
    let invalid = |e: &dyn std::fmt::Display| {
        tracing::debug!(error = %e, "push rejected");
        rpc(RpcError::InvalidObject(info(vec![truncate(
            &e.to_string(),
        )])))
    };
    match e {
        ImportError::Odb(OdbError::NotFound(oid)) => {
            rpc(RpcError::MissingObjects(info(vec![oid.to_string()])))
        }
        e @ (ImportError::Odb(OdbError::Open { .. })
        | ImportError::Store(_)
        | ImportError::Derive(_)
        | ImportError::Meta(_)
        | ImportError::Codec(_)
        | ImportError::Inconsistent(_)) => internal("import", &e),
        e => invalid(&e),
    }
}

// ------------------------------------------------------------------------------ git map

/// The `git_map` table as the importer's lookup and sink.
#[derive(Clone)]
struct PushMap {
    pool: AnyPool,
    ns: crate::registry::Ns,
    sink: Arc<crate::registry::SqlGitMap>,
}

impl PushMap {
    fn new(pool: AnyPool, entry: &RepoEntry) -> Self {
        Self {
            pool,
            ns: entry.ns,
            sink: entry.git_map.clone(),
        }
    }

    /// The ref target a git id names: a commit's changeset, or an annotated tag's record.
    async fn target_of(&self, oid: GitOid) -> Result<Option<RefTarget>, Status> {
        let fail = |e: derive::Error| internal("git_map", &e);
        if let Some(asc) = self.get(oid, ObjectKind::Commit).await.map_err(fail)? {
            return Ok(Some(RefTarget::Changeset(ChangesetId::from_bytes(asc))));
        }
        let tag = self.get(oid, ObjectKind::Tag).await.map_err(fail)?;
        Ok(tag.map(|asc| RefTarget::Tag(TagId::from_bytes(asc))))
    }
}

#[async_trait]
impl GitMapSink for PushMap {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        self.sink.put(rows).await
    }
}

#[async_trait]
impl ImportMap for PushMap {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        let code: i32 = match kind {
            ObjectKind::Blob => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Commit => 3,
            ObjectKind::Tag => 4,
        };
        let sink = |e: sqlx::Error| derive::Error::Sink(e.to_string());
        let row = sqlx::query("SELECT asc FROM git_map WHERE ns = $1 AND git = $2 AND kind = $3")
            .bind(self.ns.as_bytes().to_vec())
            .bind(git.as_bytes().to_vec())
            .bind(code)
            .fetch_optional(&self.pool)
            .await
            .map_err(sink)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let asc: Vec<u8> = row.try_get("asc").map_err(sink)?;
        <[u8; 32]>::try_from(asc.as_slice())
            .map(Some)
            .map_err(|_| derive::Error::Sink("git_map holds an asc id that is not 32 bytes".into()))
    }
}

// ------------------------------------------------------------------------------ import glue

/// A scratch repository whose only content is an `objects/info/alternates` file naming the
/// quarantine and its alternates, so `gix` reads them as one object database. The quarantine
/// itself is never written to. Removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(q: &Quarantine) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("asc-import-{}", Uuid::new_v4().simple()));
        create_private_dir(&dir)?;
        let scratch = Self(dir);
        let info = scratch.0.join("objects").join("info");
        std::fs::create_dir_all(&info)?;
        let mut text = String::new();
        for d in std::iter::once(&q.object_dir).chain(&q.alternate_dirs) {
            // `check_shape` has refused anything that is not one clean line.
            text.push_str(&d.to_string_lossy());
            text.push('\n');
        }
        std::fs::write(info.join("alternates"), text)?;
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

/// Imports `tips` from `quarantine`. The importer holds git's single-threaded object database
/// handles, so it runs on a blocking thread rather than on the shared runtime workers.
async fn import_tips(
    entry: Arc<RepoEntry>,
    map: PushMap,
    quarantine: Quarantine,
    tips: Vec<Tip>,
) -> Result<ImportReport, Status> {
    let handle = tokio::runtime::Handle::current();
    let joined = tokio::task::spawn_blocking(move || {
        let scratch =
            Scratch::new(&quarantine).map_err(|e| internal("import scratch directory", &e))?;
        handle.block_on(async {
            let odb = Odb::open(&scratch.0).map_err(|e| import_status(e.into()))?;
            let mut importer = Importer::new(
                entry.backend.engine(),
                entry.objects.clone(),
                &odb,
                &map,
                ImportPolicy::default(),
            );
            importer.import_commits(&tips).await.map_err(import_status)
        })
    })
    .await;
    joined.map_err(|e| internal("import task", &e))?
}

/// One `cs_graph` row to add.
struct GraphNode {
    id: ChangesetId,
    generation: u32,
    parents: Vec<ChangesetId>,
}

/// The imported changesets (and their unknown ancestors) that `cs_graph` does not hold yet,
/// parents first, with generations.
async fn new_graph_nodes(
    entry: &RepoEntry,
    pool: &AnyPool,
    tips: &[ChangesetId],
) -> Result<Vec<GraphNode>, Status> {
    let graph = |e: &dyn std::fmt::Display| internal("graph", e);
    let engine = entry.backend.engine();
    let mut seen: HashSet<ChangesetId> = tips.iter().copied().collect();
    let mut frontier: Vec<ChangesetId> = seen.iter().copied().collect();
    let mut parents_of: HashMap<ChangesetId, Vec<ChangesetId>> = HashMap::new();
    let mut gens: HashMap<ChangesetId, u32> = HashMap::new();
    while !frontier.is_empty() {
        let at = frontier.len().saturating_sub(GRAPH_BATCH);
        let batch = frontier.split_off(at);
        let known = graph_sql::known(pool, entry.ns, &batch)
            .await
            .map_err(|e| graph(&e))?;
        for id in batch {
            if let Some(&g) = known.get(&id) {
                gens.insert(id, g);
                continue;
            }
            if parents_of.len() >= MAX_NEW_COMMITS {
                return Err(Status::resource_exhausted(format!(
                    "a push may add at most {MAX_NEW_COMMITS} commits"
                )));
            }
            let cs = engine.get_changeset(id).await.map_err(|e| graph(&e))?;
            for p in &cs.parents {
                if seen.insert(*p) {
                    frontier.push(*p);
                }
            }
            parents_of.insert(id, cs.parents.clone());
        }
    }
    // Iterative post-order: a node is numbered once all its parents are.
    let mut out = Vec::with_capacity(parents_of.len());
    let mut entered: HashSet<ChangesetId> = HashSet::new();
    let roots: Vec<ChangesetId> = parents_of.keys().copied().collect();
    for root in roots {
        let mut stack = vec![(root, false)];
        while let Some((id, done)) = stack.pop() {
            if gens.contains_key(&id) {
                continue;
            }
            let parents = parents_of
                .get(&id)
                .ok_or_else(|| graph(&"a parent is missing"))?;
            if !done {
                if entered.insert(id) {
                    stack.push((id, true));
                    stack.extend(parents.iter().map(|p| (*p, false)));
                }
                continue;
            }
            let mut top = 0u32;
            for p in parents {
                let g = gens
                    .get(p)
                    .ok_or_else(|| graph(&"the history has a cycle"))?;
                top = top.max(*g);
            }
            let generation = top.saturating_add(1);
            gens.insert(id, generation);
            out.push(GraphNode {
                id,
                generation,
                parents: parents.clone(),
            });
        }
    }
    Ok(out)
}

/// A directory read by the case-collision check, with its folded-name counts.
type DirInfo = (Arc<Tree>, HashMap<Vec<u8>, usize>);

/// SPEC §2.8 native rule for a `windows_safe` repository: each added or changed path is
/// Windows-valid in every component and has no case-fold sibling in the resulting tree.
async fn check_windows_paths(entry: &RepoEntry, cs: &Changeset) -> Result<(), Status> {
    let engine = entry.backend.engine();
    let mut dirs: HashMap<Vec<u8>, DirInfo> = HashMap::new();
    for (path, change) in cs.changes.as_slice() {
        if matches!(change, Change::Delete) {
            continue;
        }
        let mut dir_key: Vec<u8> = Vec::new();
        let mut dir_id = cs.root;
        let mut comps = path.components().peekable();
        while let Some(comp) = comps.next() {
            let name = Name::new(comp).map_err(|e| Status::invalid_argument(e.to_string()))?;
            if !windows_valid(&name) {
                return Err(path_policy(path, "invalid on Windows"));
            }
            if !dirs.contains_key(&dir_key) {
                let tree = engine
                    .get_tree(dir_id)
                    .await
                    .map_err(|e| internal("tree", &e))?;
                let mut folds = HashMap::new();
                for e in tree.entries() {
                    *folds.entry(casefold(e.name.as_bytes())).or_insert(0usize) += 1;
                }
                dirs.insert(dir_key.clone(), (tree, folds));
            }
            let (tree, folds) = &dirs[&dir_key];
            if folds.get(&casefold(comp)).copied().unwrap_or(0) > 1 {
                return Err(path_policy(path, "collides in case with a sibling"));
            }
            if comps.peek().is_none() {
                break;
            }
            match tree.get(&name) {
                Some(e) if e.kind == Kind::Dir => dir_id = TreeId::from_bytes(e.id),
                _ => break,
            }
            if !dir_key.is_empty() {
                dir_key.push(b'/');
            }
            dir_key.extend_from_slice(comp);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ the RPC

/// The `import_push` handler group: `Ingest` (U48a) and `ImportGitPush`.
pub struct ImportPushService {
    mutations: Arc<ServerMutations>,
    push: ImportPush,
    pipeline: Arc<PipelineHooks>,
    repo_root: Option<PathBuf>,
}

impl ImportPushService {
    pub fn new(
        registry: Arc<Registry>,
        pipeline: Arc<PipelineHooks>,
        mutations: Arc<ServerMutations>,
        repo_root: Option<PathBuf>,
    ) -> Self {
        Self {
            mutations,
            push: ImportPush::new(registry, pipeline.clone()),
            pipeline,
            repo_root,
        }
    }
}

#[async_trait]
impl ImportPushHandler for ImportPushService {
    async fn ingest(
        &self,
        ctx: &CallCtx,
        req: wire::IngestRequest,
    ) -> Result<wire::IngestResponse, Status> {
        self.mutations.ingest(ctx, req).await
    }

    async fn import_git_push(
        &self,
        ctx: &CallCtx,
        req: wire::ImportGitPushRequest,
    ) -> Result<wire::ImportGitPushResponse, Status> {
        // Forgejo's internal token only (SPEC §15.1). `Access::Admin` alone would let any admin
        // token point the server at a directory.
        if !self.pipeline.push_hooks().may_skip_pre(ctx) {
            return Err(Status::permission_denied(
                "ImportGitPush needs the Forgejo internal token",
            ));
        }
        let root = self
            .repo_root
            .as_deref()
            .ok_or_else(|| Status::failed_precondition("forgejo.repo_root is not configured"))?;
        if req.pusher_id < 0 {
            return Err(Status::invalid_argument("bad pusher id"));
        }
        let quarantine = resolve_dirs(root, &ctx.repo, &req).await?;
        let commands = req
            .updates
            .iter()
            .map(command_of)
            .collect::<Result<Vec<_>, _>>()?;
        let opts = ImportOptions {
            skip_pre: true,
            ..ImportOptions::default()
        };
        self.push
            .import_git_push(ctx, &quarantine, &commands, &opts)
            .await
    }
}

fn command_of(u: &wire::GitRefUpdate) -> Result<PushCommand, Status> {
    let oid = |b: &[u8]| -> Result<Option<GitOid>, Status> {
        let bad = || Status::invalid_argument("a git id is neither 20 bytes nor 40 hex digits");
        let oid = match b.len() {
            0 => return Ok(None),
            20 => GitOid::from_bytes(<[u8; 20]>::try_from(b).map_err(|_| bad())?),
            40 => std::str::from_utf8(b)
                .ok()
                .and_then(|s| s.parse::<GitOid>().ok())
                .ok_or_else(bad)?,
            _ => return Err(bad()),
        };
        // git's all-zero id means "no ref".
        Ok((oid.as_bytes() != &[0u8; 20]).then_some(oid))
    };
    Ok(PushCommand {
        name: u.name.clone(),
        old: oid(&u.old_git)?,
        new: oid(&u.new_git)?,
    })
}

/// Canonicalises the directories of an `ImportGitPush` request and checks that all of them lie
/// in the repository the call names: `git_dir` is `<root>/<owner>/<name>.git` (Forgejo lowercases
/// both), and `object_dir` and every alternate are inside its `objects`.
async fn resolve_dirs(
    root: &Path,
    repo: &RepoName,
    req: &wire::ImportGitPushRequest,
) -> Result<Quarantine, Status> {
    let refuse = || Status::permission_denied("that directory is outside the repository");
    if req.alternate_dirs.len() > MAX_ALTERNATES {
        return Err(Status::invalid_argument("too many alternate directories"));
    }
    let (owner, name) = repo
        .as_str()
        .split_once('/')
        .ok_or_else(|| Status::invalid_argument("invalid repository name"))?;
    let expected = root
        .join(owner.to_lowercase())
        .join(format!("{}.git", name.to_lowercase()));
    let canon = |p: &str| {
        let p = p.to_owned();
        async move {
            if p.is_empty() || p.len() > MAX_DIR_PATH_BYTES || p.contains('\0') {
                return Err(Status::invalid_argument("a directory path is not usable"));
            }
            tokio::fs::canonicalize(&p)
                .await
                .map(simplify)
                .map_err(|_| Status::invalid_argument("a directory does not exist"))
        }
    };
    let git_dir = canon(&req.git_dir).await?;
    let expected = tokio::fs::canonicalize(expected)
        .await
        .map(simplify)
        .map_err(|_| Status::not_found("no such repository on disk"))?;
    if git_dir != expected {
        return Err(refuse());
    }
    let objects = git_dir.join("objects");
    let object_dir = canon(&req.object_dir).await?;
    if !object_dir.starts_with(&objects) {
        return Err(refuse());
    }
    let mut alternate_dirs = Vec::with_capacity(req.alternate_dirs.len());
    for a in &req.alternate_dirs {
        let a = canon(a).await?;
        if !a.starts_with(&objects) {
            return Err(refuse());
        }
        alternate_dirs.push(a);
    }
    Ok(Quarantine {
        object_dir,
        alternate_dirs,
    })
}

/// Strips the `\\?\` prefix `canonicalize` adds on Windows, which `gix` does not expect in an
/// alternates file.
#[cfg(windows)]
fn simplify(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(not(windows))]
fn simplify(p: PathBuf) -> PathBuf {
    p
}

/// The one registration line of this unit (called from `lib.rs`): the `import_push` handler
/// group (`Ingest` and `ImportGitPush`). `repo_root` is `forgejo.repo_root`; without it the RPC
/// is refused.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    mutations: Arc<ServerMutations>,
    repo_root: Option<PathBuf>,
) {
    let handler = Arc::new(ImportPushService::new(
        registry.clone(),
        ext.pipeline.clone(),
        mutations,
        repo_root,
    ));
    ext.handlers
        .set_import_push(handler)
        .expect("the import_push handler group is registered once");
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use asc_git::realgit::RealGit;
    use asc_rpc::auth::Principal;

    use super::*;
    use crate::forgejo::INTERNAL_PRINCIPAL;
    use crate::ingest::testutil::registry;
    use crate::registry::NewRepo;
    use crate::update::{HookRejected, HookRequest, PushHooks};

    // -------------------------------------------------------------------------- git fixtures

    /// Runs real git on `dir` with fixed identities and dates; `env` adds variables.
    fn git_env(dir: &Path, env: &[(&str, &Path)], args: &[&str], stdin: &[u8]) -> String {
        let real = RealGit::resolve(None, None).expect("git is installed");
        let mut cmd = real.command().unwrap();
        cmd.arg("--git-dir")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A tree for `files` (path to content), nested directories included.
    fn tree_of(git: &dyn Fn(&[&str], &[u8]) -> String, files: &BTreeMap<String, String>) -> String {
        let mut here: BTreeMap<String, String> = BTreeMap::new();
        let mut dirs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for (path, content) in files {
            match path.split_once('/') {
                Some((d, rest)) => {
                    dirs.entry(d.to_owned())
                        .or_default()
                        .insert(rest.to_owned(), content.clone());
                }
                None => {
                    here.insert(path.clone(), content.clone());
                }
            }
        }
        let mut lines = String::new();
        for (name, content) in &here {
            let blob = git(&["hash-object", "-w", "--stdin"], content.as_bytes());
            lines.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        for (name, sub) in &dirs {
            let t = tree_of(git, sub);
            lines.push_str(&format!("040000 tree {t}\t{name}\n"));
        }
        git(&["mktree"], lines.as_bytes())
    }

    /// The forge's bare repository `<root>/o/r.git` and the paths around it.
    struct Forge {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        git_dir: PathBuf,
    }

    impl Forge {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("repos");
            let git_dir = root.join("o").join("r.git");
            std::fs::create_dir_all(&git_dir).unwrap();
            let real = RealGit::resolve(None, None).unwrap();
            assert!(
                real.command()
                    .unwrap()
                    .args(["init", "--bare", "--quiet", "--initial-branch=master"])
                    .arg(&git_dir)
                    .status()
                    .unwrap()
                    .success()
            );
            Self {
                _tmp: tmp,
                root,
                git_dir,
            }
        }

        fn git(&self, args: &[&str], stdin: &[u8]) -> String {
            git_env(&self.git_dir, &[], args, stdin)
        }

        /// A commit of `files` on `parent`, written into the repository's own object store.
        fn commit(&self, parent: Option<&str>, files: &[(&str, &str)]) -> String {
            self.commit_in(&[], parent, files)
        }

        /// As [`Forge::commit`], writing through `env` (a quarantine).
        fn commit_in(
            &self,
            env: &[(&str, &Path)],
            parent: Option<&str>,
            files: &[(&str, &str)],
        ) -> String {
            let map = files
                .iter()
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
                .collect();
            let run = |args: &[&str], stdin: &[u8]| git_env(&self.git_dir, env, args, stdin);
            let tree = tree_of(&run, &map);
            let msg = format!("commit {tree}");
            let mut args = vec!["commit-tree", tree.as_str(), "-m", msg.as_str()];
            if let Some(p) = parent {
                args.extend(["-p", p]);
            }
            run(&args, b"")
        }

        fn objects(&self) -> PathBuf {
            self.git_dir.join("objects")
        }

        fn whole(&self) -> Quarantine {
            Quarantine {
                object_dir: self.objects(),
                alternate_dirs: Vec::new(),
            }
        }
    }

    fn oid(s: &str) -> GitOid {
        s.parse().unwrap()
    }

    fn cmd(name: &str, old: Option<&str>, new: Option<&str>) -> PushCommand {
        PushCommand {
            name: name.into(),
            old: old.map(oid),
            new: new.map(oid),
        }
    }

    // -------------------------------------------------------------------------- server fixtures

    /// Hooks that let the Forgejo internal principal skip pre-receive and count the calls.
    #[derive(Default)]
    struct FakeForgejo {
        pre_receive: AtomicUsize,
    }

    #[async_trait]
    impl PushHooks for FakeForgejo {
        fn may_skip_pre(&self, ctx: &CallCtx) -> bool {
            ctx.principal.id == INTERNAL_PRINCIPAL
        }

        async fn pre_receive(&self, _req: &HookRequest<'_>) -> Result<(), HookRejected> {
            self.pre_receive.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct CountingObserver {
        calls: StdMutex<Vec<Vec<ChangesetId>>>,
    }

    #[async_trait]
    impl crate::ingest::IngestObserver for CountingObserver {
        async fn on_ingested(
            &self,
            _tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
            _repo: asc_store::refs::RepoKey,
            new: &[ChangesetId],
            _closure: &[ObjKey],
        ) -> Result<(), crate::ingest::ObserverError> {
            self.calls.lock().unwrap().push(new.to_vec());
            Ok(())
        }
    }

    struct Fx {
        forge: Forge,
        registry: Arc<Registry>,
        entry: Arc<RepoEntry>,
        pipeline: Arc<PipelineHooks>,
        push: ImportPush,
        hooks: Arc<FakeForgejo>,
        observer: Arc<CountingObserver>,
    }

    async fn fx() -> Fx {
        let forge = Forge::new();
        let registry = registry().await;
        let entry = registry
            .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let pipeline = Arc::new(PipelineHooks::default());
        let hooks = Arc::new(FakeForgejo::default());
        let observer = Arc::new(CountingObserver::default());
        assert!(pipeline.set_push_hooks(hooks.clone()).is_ok());
        assert!(pipeline.set_observer(observer.clone()).is_ok());
        let push = ImportPush::new(registry.clone(), pipeline.clone());
        Fx {
            forge,
            registry,
            entry,
            pipeline,
            push,
            hooks,
            observer,
        }
    }

    fn ctx_of(principal: &str) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Git,
            principal: Principal {
                id: principal.into(),
            },
            op_id: Some(Uuid::new_v4()),
            client: None,
        }
    }

    fn code(s: &Status) -> String {
        status_rpc_error(s).map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
    }

    impl Fx {
        async fn push(
            &self,
            commands: &[PushCommand],
        ) -> Result<wire::ImportGitPushResponse, Status> {
            self.push_as(&ctx_of("alice"), commands, &ImportOptions::default())
                .await
        }

        async fn push_as(
            &self,
            ctx: &CallCtx,
            commands: &[PushCommand],
            opts: &ImportOptions,
        ) -> Result<wire::ImportGitPushResponse, Status> {
            self.push
                .import_git_push(ctx, &self.forge.whole(), commands, opts)
                .await
        }

        async fn target(&self, name: &str) -> Option<RefTarget> {
            self.registry
                .refs()
                .get(self.entry.key, &RefName::new(name.as_bytes()).unwrap())
                .await
                .unwrap()
                .map(|e| e.target)
        }

        async fn git_of(&self, name: &str) -> String {
            match self.target(name).await.expect("ref exists") {
                RefTarget::Changeset(c) => self
                    .entry
                    .backend
                    .engine()
                    .git_commit_of(c)
                    .await
                    .unwrap()
                    .to_string(),
                RefTarget::Tag(_) => panic!("{name} is a tag"),
            }
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(self.registry.pool())
                .await
                .unwrap()
                .get::<i64, _>(0)
        }

        async fn outbox(&self) -> Vec<serde_json::Value> {
            sqlx::query("SELECT effects FROM outbox ORDER BY id")
                .fetch_all(self.registry.pool())
                .await
                .unwrap()
                .iter()
                .map(|r| serde_json::from_slice(&r.get::<Vec<u8>, _>("effects")).unwrap())
                .collect()
        }
    }

    // -------------------------------------------------------------------------- import and CAS

    #[tokio::test]
    async fn a_push_is_imported_round_trips_and_moves_the_ref_through_the_pipeline() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a.txt", "one")]);
        let c2 = f
            .forge
            .commit(Some(&c1), &[("a.txt", "two"), ("d/b.txt", "b")]);
        let done = f
            .push(&[cmd("refs/heads/master", None, Some(&c2))])
            .await
            .unwrap();
        assert_eq!(done.results.len(), 1);
        assert_eq!(done.results[0].name, "refs/heads/master");
        // The git commit of the stored changeset is the pushed commit, byte for byte (SPEC §14.1).
        assert_eq!(f.git_of("refs/heads/master").await, c2);
        assert_eq!(f.count("SELECT COUNT(*) FROM cs_graph").await, 2);
        // The observer saw the new changesets once, parents first (the `changes` derivation).
        let calls = f.observer.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 2);
        // One transaction wrote the ref log and the outbox row with the post-receive effect.
        let log = f
            .registry
            .refs()
            .log(
                f.entry.key,
                &RefName::new(b"refs/heads/master".as_slice()).unwrap(),
                5,
            )
            .await
            .unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(f.outbox().await.len(), 1);
        // A fast-forward with the right old value follows.
        let c3 = f.forge.commit(Some(&c2), &[("a.txt", "three")]);
        f.push(&[cmd("refs/heads/master", Some(&c2), Some(&c3))])
            .await
            .unwrap();
        assert_eq!(f.git_of("refs/heads/master").await, c3);
        assert_eq!(f.count("SELECT COUNT(*) FROM cs_graph").await, 3);
        // Not the internal token, so the full pipeline ran: pre-receive was asked both times.
        assert_eq!(f.hooks.pre_receive.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_stale_old_value_is_a_ref_conflict_and_nothing_is_imported() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let c2 = f.forge.commit(Some(&c1), &[("a", "2")]);
        f.push(&[cmd("refs/heads/master", None, Some(&c2))])
            .await
            .unwrap();
        let graph = f.count("SELECT COUNT(*) FROM cs_graph").await;
        let c3 = f.forge.commit(Some(&c2), &[("a", "3")]);
        // Pushed against the value before c2.
        let s = f
            .push(&[cmd("refs/heads/master", Some(&c1), Some(&c3))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
        // Pushed as a create although the ref exists.
        let s = f
            .push(&[cmd("refs/heads/master", None, Some(&c3))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
        // Pushed against a value asc has never seen.
        let unknown = "1111111111111111111111111111111111111111";
        let s = f
            .push(&[cmd("refs/heads/master", Some(unknown), Some(&c3))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
        // And an old value for a ref that does not exist.
        let s = f
            .push(&[cmd("refs/heads/other", Some(&c1), Some(&c3))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
        assert_eq!(f.git_of("refs/heads/master").await, c2);
        assert!(f.target("refs/heads/other").await.is_none());
        // The check came first: c3 was never imported.
        assert_eq!(f.count("SELECT COUNT(*) FROM cs_graph").await, graph);
    }

    #[tokio::test]
    async fn a_non_fast_forward_is_gits_fetch_first_even_on_a_land_branch() {
        let f = fx().await;
        sqlx::query("UPDATE repos SET land_branches = '[\"master\"]'")
            .execute(f.registry.pool())
            .await
            .unwrap();
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let c2 = f.forge.commit(Some(&c1), &[("a", "2")]);
        f.push(&[cmd("refs/heads/master", None, Some(&c2))])
            .await
            .unwrap();
        // A sibling of c2: the pusher did not fetch c2 first.
        let side = f.forge.commit(Some(&c1), &[("a", "side")]);
        let s = f
            .push(&[cmd("refs/heads/master", Some(&c2), Some(&side))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "NOT_FAST_FORWARD");
        assert_eq!(s.message(), "! [rejected] refs/heads/master (fetch first)");
        assert_eq!(f.git_of("refs/heads/master").await, c2);
        // A fast-forward on the same land branch is a plain CAS, not a land: no rewrite.
        let c3 = f.forge.commit(Some(&c2), &[("a", "3")]);
        f.push(&[cmd("refs/heads/master", Some(&c2), Some(&c3))])
            .await
            .unwrap();
        assert_eq!(f.git_of("refs/heads/master").await, c3);
    }

    #[tokio::test]
    async fn a_deletion_needs_the_current_value_and_queues_a_post_receive_deletion() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        f.push(&[cmd("refs/heads/topic", None, Some(&c1))])
            .await
            .unwrap();
        let wrong = "2222222222222222222222222222222222222222";
        let s = f
            .push(&[cmd("refs/heads/topic", Some(wrong), None)])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
        assert!(f.target("refs/heads/topic").await.is_some());
        f.push(&[cmd("refs/heads/topic", Some(&c1), None)])
            .await
            .unwrap();
        assert!(f.target("refs/heads/topic").await.is_none());
        let rows = f.outbox().await;
        assert_eq!(rows.len(), 2);
        assert!(rows[1]["refs"][0]["new"].is_null());
    }

    #[tokio::test]
    async fn a_retry_of_a_committed_push_returns_the_stored_result() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let ctx = ctx_of("alice");
        let commands = [cmd("refs/heads/master", None, Some(&c1))];
        let first = f
            .push_as(&ctx, &commands, &ImportOptions::default())
            .await
            .unwrap();
        // The same op id again: the ref exists now, so a fresh push would conflict.
        let again = f
            .push_as(&ctx, &commands, &ImportOptions::default())
            .await
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(f.outbox().await.len(), 1);
        // Another op id is a new push and does conflict.
        let s = f.push(&commands).await.unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
    }

    #[tokio::test]
    async fn a_push_sent_again_after_its_answer_was_lost_is_answered_and_changes_nothing() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let c2 = f.forge.commit(Some(&c1), &[("a", "2")]);
        let alice = ctx_of("alice");
        let opts = ImportOptions {
            via: Some("git-push"),
            ..ImportOptions::default()
        };
        let create = [cmd("refs/heads/topic", None, Some(&c1))];
        let first = f.push_as(&alice, &create, &opts).await.unwrap();
        let (log, outbox) = (
            f.count("SELECT COUNT(*) FROM ref_log").await,
            f.outbox().await,
        );

        // The client never saw the answer and pushes again: a new session, so a new op id.
        let again = f
            .push_as(&ctx_of("alice"), &create, &opts)
            .await
            .expect("the same push is answered, not refused as stale");
        assert_eq!(first, again);
        assert_eq!(f.count("SELECT COUNT(*) FROM ref_log").await, log);
        assert_eq!(f.outbox().await, outbox, "no second post-receive event");

        // It is the same push only for the principal whose push stored it, and only through the
        // git push route.
        let s = f.push_as(&ctx_of("bob"), &create, &opts).await.unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");
        let s = f
            .push_as(&ctx_of("alice"), &create, &ImportOptions::default())
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");

        // Once the ref has moved on, the old push is stale again.
        f.push_as(
            &ctx_of("alice"),
            &[cmd("refs/heads/topic", Some(&c1), Some(&c2))],
            &opts,
        )
        .await
        .unwrap();
        let s = f
            .push_as(&ctx_of("alice"), &create, &opts)
            .await
            .unwrap_err();
        assert_eq!(code(&s), "REF_CONFLICT");

        // A deletion sent again is answered too.
        let delete = [cmd("refs/heads/topic", Some(&c2), None)];
        let first = f.push_as(&ctx_of("alice"), &delete, &opts).await.unwrap();
        let again = f.push_as(&ctx_of("alice"), &delete, &opts).await.unwrap();
        assert_eq!(first, again);
        assert!(f.target("refs/heads/topic").await.is_none());
        assert_eq!(f.count("SELECT COUNT(*) FROM ref_log").await, 3);
    }

    #[tokio::test]
    async fn a_user_ref_push_sent_again_is_answered_as_a_user_ref() {
        // A user ref is not advertised, so the client never learns that its push landed and
        // sends the same create again: the case the retry rule exists for.
        let f = fx().await;
        sqlx::query("UPDATE repos SET branchless = TRUE")
            .execute(f.registry.pool())
            .await
            .unwrap();
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let opts = ImportOptions {
            via: Some("git-push"),
            ..ImportOptions::default()
        };
        let create = [cmd("refs/heads/feat", None, Some(&c1))];
        let first = f
            .push_as(&ctx_of("42:alice"), &create, &opts)
            .await
            .unwrap();
        assert!(first.results[0].user, "{first:?}");
        let again = f
            .push_as(&ctx_of("42:alice"), &create, &opts)
            .await
            .expect("the same create is answered, not refused as stale");
        assert_eq!(first, again);
        assert_eq!(f.count("SELECT COUNT(*) FROM ref_log").await, 1);
        // Another user's identical push is a push of their own: their branch, their ref.
        let other = f.push_as(&ctx_of("43:bob"), &create, &opts).await.unwrap();
        assert!(other.results[0].user);
        assert_eq!(f.count("SELECT COUNT(*) FROM ref_log").await, 2);
    }

    #[tokio::test]
    async fn a_quarantine_is_read_with_its_alternates_and_never_written() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        f.push(&[cmd("refs/heads/master", None, Some(&c1))])
            .await
            .unwrap();
        // git's layout: new objects land in `objects/incoming-x`, reads fall through to `objects`.
        let q = f.forge.objects().join("incoming-x");
        std::fs::create_dir_all(&q).unwrap();
        let main = f.forge.objects();
        let c2 = f.forge.commit_in(
            &[
                ("GIT_OBJECT_DIRECTORY", q.as_path()),
                ("GIT_ALTERNATE_OBJECT_DIRECTORIES", main.as_path()),
            ],
            Some(&c1),
            &[("a", "1"), ("new.txt", "fresh")],
        );
        let listing = |dir: &Path| -> Vec<PathBuf> {
            let mut v = Vec::new();
            let mut stack = vec![dir.to_owned()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(d).unwrap().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        v.push(p);
                    }
                }
            }
            v.sort();
            v
        };
        let before = listing(&q);
        assert!(!before.is_empty());
        let quarantine = Quarantine {
            object_dir: q.clone(),
            alternate_dirs: vec![main],
        };
        f.push
            .import_git_push(
                &ctx_of("alice"),
                &quarantine,
                &[cmd("refs/heads/master", Some(&c1), Some(&c2))],
                &ImportOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(f.git_of("refs/heads/master").await, c2);
        assert_eq!(
            before,
            listing(&q),
            "the quarantine is read-only to the importer"
        );
    }

    #[tokio::test]
    async fn a_tip_missing_from_the_quarantine_is_missing_objects() {
        let f = fx().await;
        let absent = "3333333333333333333333333333333333333333";
        let s = f
            .push(&[cmd("refs/heads/master", None, Some(absent))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "MISSING_OBJECTS");
        assert!(f.target("refs/heads/master").await.is_none());
    }

    #[tokio::test]
    async fn a_corrupt_object_is_refused_by_its_hash() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        // Overwrite the commit's tree with the loose file of another tree: the name no longer
        // matches the content.
        let tree = f.forge.git(&["rev-parse", &format!("{c1}^{{tree}}")], b"");
        let other = tree_of(
            &|a: &[&str], i: &[u8]| f.forge.git(a, i),
            &BTreeMap::from([("zzz".to_owned(), "other".to_owned())]),
        );
        let loose = |o: &str| f.forge.objects().join(&o[..2]).join(&o[2..]);
        let (victim, donor) = (loose(&tree), loose(&other));
        let mut perms = std::fs::metadata(&victim).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&victim, perms).unwrap();
        std::fs::copy(&donor, &victim).unwrap();
        let s = f
            .push(&[cmd("refs/heads/master", None, Some(&c1))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "INVALID_OBJECT", "{s:?}");
        assert!(f.target("refs/heads/master").await.is_none());
        assert_eq!(f.count("SELECT COUNT(*) FROM cs_graph").await, 0);
    }

    // -------------------------------------------------------------------------- tags

    #[tokio::test]
    async fn an_annotated_tag_is_stored_as_a_tag_record_and_deleted_by_its_tag_id() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        f.forge.git(&["update-ref", "refs/heads/master", &c1], b"");
        f.forge.git(&["tag", "-a", "-m", "release", "v1", &c1], b"");
        let tag = f.forge.git(&["rev-parse", "refs/tags/v1"], b"");
        assert_ne!(tag, c1);
        f.push(&[cmd("refs/tags/v1", None, Some(&tag))])
            .await
            .unwrap();
        assert!(matches!(
            f.target("refs/tags/v1").await,
            Some(RefTarget::Tag(_))
        ));
        f.push(&[cmd("refs/tags/v1", Some(&tag), None)])
            .await
            .unwrap();
        assert!(f.target("refs/tags/v1").await.is_none());
    }

    #[tokio::test]
    async fn a_tag_asc_cannot_represent_fails_the_whole_push() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let tree = f.forge.git(&["rev-parse", &format!("{c1}^{{tree}}")], b"");
        f.forge
            .git(&["tag", "-a", "-m", "tree tag", "treetag", &tree], b"");
        let tag = f.forge.git(&["rev-parse", "refs/tags/treetag"], b"");
        let s = f
            .push(&[
                cmd("refs/heads/master", None, Some(&c1)),
                cmd("refs/tags/treetag", None, Some(&tag)),
            ])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "INVALID_OBJECT");
        // Nothing moved: the pusher must not believe `master` was stored.
        assert!(f.target("refs/heads/master").await.is_none());
        assert!(f.target("refs/tags/treetag").await.is_none());
    }

    // -------------------------------------------------------------------------- authority

    #[tokio::test]
    async fn skip_pre_is_refused_to_every_caller_but_the_internal_token_and_before_any_work() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let skip = ImportOptions {
            skip_pre: true,
            ..ImportOptions::default()
        };
        let s = f
            .push_as(
                &ctx_of("alice"),
                &[cmd("refs/heads/master", None, Some(&c1))],
                &skip,
            )
            .await
            .unwrap_err();
        assert_eq!(s.code(), tonic::Code::PermissionDenied);
        assert_eq!(f.count("SELECT COUNT(*) FROM cs_graph").await, 0);
        assert_eq!(f.count("SELECT COUNT(*) FROM git_map").await, 0);
        // The internal token skips pre-receive; the hook is not called.
        f.push_as(
            &ctx_of(INTERNAL_PRINCIPAL),
            &[cmd("refs/heads/master", None, Some(&c1))],
            &skip,
        )
        .await
        .unwrap();
        assert_eq!(f.hooks.pre_receive.load(Ordering::SeqCst), 0);
        assert!(f.target("refs/heads/master").await.is_some());
    }

    #[tokio::test]
    async fn a_mirror_repository_takes_no_pushes_and_nothing_is_read() {
        let f = fx().await;
        let name = RepoName::parse("o/m").unwrap();
        f.registry
            .create(
                &name,
                &NewRepo {
                    mode: RepoMode::Mirror,
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let mut ctx = ctx_of("alice");
        ctx.repo = name;
        let s = f
            .push_as(
                &ctx,
                &[cmd("refs/heads/master", None, Some(&c1))],
                &ImportOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&s), "MIRROR_MODE");
        assert_eq!(f.count("SELECT COUNT(*) FROM git_map").await, 0);
    }

    #[tokio::test]
    async fn commands_are_validated_before_any_work() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let opts = |ttl| ImportOptions {
            ttl_seconds: ttl,
            ..ImportOptions::default()
        };
        let invalid = |r: Result<wire::ImportGitPushResponse, Status>| {
            assert_eq!(r.unwrap_err().code(), tonic::Code::InvalidArgument);
        };
        invalid(f.push(&[]).await);
        invalid(
            f.push(&[
                cmd("refs/heads/a", None, Some(&c1)),
                cmd("refs/heads/a", None, Some(&c1)),
            ])
            .await,
        );
        invalid(f.push(&[cmd("refs/heads/a", None, None)]).await);
        invalid(f.push(&[cmd("not a ref", None, Some(&c1))]).await);
        invalid(f.push(&[cmd("refs/heads/..", None, Some(&c1))]).await);
        let long = format!("refs/heads/{}", "x".repeat(MAX_REF_NAME_BYTES));
        invalid(f.push(&[cmd(&long, None, Some(&c1))]).await);
        let many: Vec<PushCommand> = (0..=MAX_REF_UPDATES)
            .map(|i| cmd(&format!("refs/heads/b{i}"), None, Some(&c1)))
            .collect();
        invalid(f.push(&many).await);
        // A TTL is for agent branches and snapshots only; `master` could otherwise be set to
        // expire by any writer.
        invalid(
            f.push_as(
                &ctx_of("alice"),
                &[cmd("refs/heads/master", None, Some(&c1))],
                &opts(60),
            )
            .await,
        );
        invalid(
            f.push_as(
                &ctx_of("alice"),
                &[cmd("refs/heads/agent/a", None, Some(&c1))],
                &opts(-1),
            )
            .await,
        );
        let too_many_options = ImportOptions {
            push_options: vec!["o".into(); 65],
            ..ImportOptions::default()
        };
        invalid(
            f.push_as(
                &ctx_of("alice"),
                &[cmd("refs/heads/a", None, Some(&c1))],
                &too_many_options,
            )
            .await,
        );
        assert_eq!(f.count("SELECT COUNT(*) FROM git_map").await, 0);
        // The ttl on an agent branch is stored.
        f.push_as(
            &ctx_of("alice"),
            &[cmd("refs/heads/agent/a", None, Some(&c1))],
            &opts(3600),
        )
        .await
        .unwrap();
        let expires = f
            .registry
            .refs()
            .get(
                f.entry.key,
                &RefName::new(b"refs/heads/agent/a".as_slice()).unwrap(),
            )
            .await
            .unwrap()
            .unwrap()
            .expires_at
            .unwrap();
        assert!((crate::update::now() + 3500..=crate::update::now() + 3700).contains(&expires));
    }

    #[tokio::test]
    async fn quarantine_directories_that_could_inject_an_alternate_are_refused() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let base = f.forge.objects();
        let bad = [
            PathBuf::from(format!("{}\n/etc", base.display())),
            PathBuf::from(format!("{}\r/etc", base.display())),
            PathBuf::from("relative/objects"),
            PathBuf::from("#/comment"),
            PathBuf::new(),
        ];
        for dir in bad {
            for as_alternate in [false, true] {
                let q = if as_alternate {
                    Quarantine {
                        object_dir: base.clone(),
                        alternate_dirs: vec![dir.clone()],
                    }
                } else {
                    Quarantine {
                        object_dir: dir.clone(),
                        alternate_dirs: Vec::new(),
                    }
                };
                let s = f
                    .push
                    .import_git_push(
                        &ctx_of("alice"),
                        &q,
                        &[cmd("refs/heads/master", None, Some(&c1))],
                        &ImportOptions::default(),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(s.code(), tonic::Code::InvalidArgument, "{dir:?}");
            }
        }
        let many = Quarantine {
            object_dir: base.clone(),
            alternate_dirs: vec![base; MAX_ALTERNATES + 1],
        };
        let s = f
            .push
            .import_git_push(
                &ctx_of("alice"),
                &many,
                &[cmd("refs/heads/master", None, Some(&c1))],
                &ImportOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(s.code(), tonic::Code::InvalidArgument);
        assert_eq!(f.count("SELECT COUNT(*) FROM git_map").await, 0);
    }

    // -------------------------------------------------------------------------- path policy

    #[tokio::test]
    async fn the_native_path_rule_applies_after_the_first_push_and_import_grade_waives_it() {
        let f = fx().await;
        // First push to an empty repository: the import rule, so a Windows-invalid name is fine.
        let c1 = f.forge.commit(None, &[("aux.txt", "legacy")]);
        f.push(&[cmd("refs/heads/master", None, Some(&c1))])
            .await
            .unwrap();
        // Later pushes follow the native rule of a `windows_safe` repository.
        let c2 = f
            .forge
            .commit(Some(&c1), &[("aux.txt", "legacy"), ("nul.txt", "x")]);
        let s = f
            .push(&[cmd("refs/heads/master", Some(&c1), Some(&c2))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "PATH_POLICY");
        assert_eq!(f.git_of("refs/heads/master").await, c1);
        // A case collision is refused too.
        let c3 = f.forge.commit(
            Some(&c1),
            &[("aux.txt", "l"), ("Readme", "1"), ("README", "2")],
        );
        let s = f
            .push(&[cmd("refs/heads/master", Some(&c1), Some(&c3))])
            .await
            .unwrap_err();
        assert_eq!(code(&s), "PATH_POLICY");
        // `repos.git_import_grade` (the migration window) waives the native rule.
        let import = ImportOptions {
            import_grade: true,
            ..ImportOptions::default()
        };
        f.push_as(
            &ctx_of("alice"),
            &[cmd("refs/heads/master", Some(&c1), Some(&c2))],
            &import,
        )
        .await
        .unwrap();
        assert_eq!(f.git_of("refs/heads/master").await, c2);
    }

    #[tokio::test]
    async fn git_aliases_are_refused_in_every_mode_and_nothing_is_recorded() {
        let f = fx().await;
        let import = ImportOptions {
            import_grade: true,
            ..ImportOptions::default()
        };
        // The importer itself refuses a literal `.git` component (INVALID_OBJECT); the aliases it
        // lets through are the path rule's to catch.
        for (path, want) in [
            ("GIT~1/config", "PATH_POLICY"),
            ("git~1/hooks/post-checkout", "PATH_POLICY"),
            ("sub/.git/hooks/post-checkout", "INVALID_OBJECT"),
            (".GIT/config", "INVALID_OBJECT"),
        ] {
            let c = f.forge.commit(None, &[("ok", "1"), (path, "x")]);
            // Both rules, and the empty-repository import rule, refuse it.
            for opts in [ImportOptions::default(), import.clone()] {
                let s = f
                    .push_as(
                        &ctx_of("alice"),
                        &[cmd("refs/heads/master", None, Some(&c))],
                        &opts,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(code(&s), want, "{path}");
            }
            assert!(f.target("refs/heads/master").await.is_none());
        }
    }

    // -------------------------------------------------------------------------- concurrency

    #[tokio::test]
    async fn two_pushes_to_one_ref_race_and_exactly_one_wins() {
        let f = fx().await;
        let a = f.forge.commit(None, &[("a", "a")]);
        let b = f.forge.commit(None, &[("b", "b")]);
        let (ca, cb) = (
            [cmd("refs/heads/master", None, Some(&a))],
            [cmd("refs/heads/master", None, Some(&b))],
        );
        let (ra, rb) = tokio::join!(f.push(&ca), f.push(&cb));
        assert_eq!(
            u8::from(ra.is_ok()) + u8::from(rb.is_ok()),
            1,
            "{ra:?} {rb:?}"
        );
        let loser = ra.err().or(rb.err()).unwrap();
        assert!(["REF_CONFLICT", "NOT_FAST_FORWARD"].contains(&code(&loser).as_str()));
        let won = f.git_of("refs/heads/master").await;
        assert!(won == a || won == b);
    }

    // -------------------------------------------------------------------------- the RPC

    struct Rpc {
        f: Fx,
        service: ImportPushService,
    }

    async fn rpc_fx(repo_root: bool, forgejo: bool) -> Rpc {
        let f = fx().await;
        let pipeline = if forgejo {
            f.pipeline.clone()
        } else {
            Arc::new(PipelineHooks::default())
        };
        let mutations = Arc::new(ServerMutations::new(
            f.registry.clone(),
            pipeline.clone(),
            Arc::default(),
        ));
        let service = ImportPushService::new(
            f.registry.clone(),
            pipeline,
            mutations,
            repo_root.then(|| f.forge.root.clone()),
        );
        Rpc { f, service }
    }

    fn request(f: &Fx, old: &str, new: &str) -> wire::ImportGitPushRequest {
        let git_dir = f.forge.git_dir.clone();
        let raw = |s: &str| {
            if s.is_empty() {
                Vec::new()
            } else {
                oid(s).as_bytes().to_vec()
            }
        };
        wire::ImportGitPushRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            git_dir: git_dir.to_string_lossy().into_owned(),
            object_dir: git_dir.join("objects").to_string_lossy().into_owned(),
            alternate_dirs: Vec::new(),
            updates: vec![wire::GitRefUpdate {
                name: "refs/heads/master".into(),
                old_git: raw(old).into(),
                new_git: raw(new).into(),
            }],
            pusher_id: 7,
        }
    }

    #[tokio::test]
    async fn the_rpc_imports_from_the_forge_repository_and_skips_pre_receive() {
        let r = rpc_fx(true, true).await;
        let c1 = r.f.forge.commit(None, &[("a", "1")]);
        // A real quarantine: new objects in `incoming-x`, reads through `objects`.
        let q = r.f.forge.objects().join("incoming-x");
        std::fs::create_dir_all(&q).unwrap();
        let main = r.f.forge.objects();
        let c2 = r.f.forge.commit_in(
            &[
                ("GIT_OBJECT_DIRECTORY", q.as_path()),
                ("GIT_ALTERNATE_OBJECT_DIRECTORIES", main.as_path()),
            ],
            Some(&c1),
            &[("a", "2")],
        );
        let done = r
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), request(&r.f, "", &c1))
            .await
            .unwrap();
        assert_eq!(done.results.len(), 1);
        let mut req = request(&r.f, &c1, &c2);
        req.object_dir = q.to_string_lossy().into_owned();
        req.alternate_dirs = vec![main.to_string_lossy().into_owned()];
        // The 40-digit hex form of an id is accepted as well.
        req.updates[0].old_git = c1.clone().into_bytes().into();
        r.service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), req)
            .await
            .unwrap();
        assert_eq!(r.f.git_of("refs/heads/master").await, c2);
        assert_eq!(r.f.hooks.pre_receive.load(Ordering::SeqCst), 0);
        // A non-fast-forward comes back as git's rejection.
        let side = r.f.forge.commit(Some(&c1), &[("a", "side")]);
        let s = r
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), request(&r.f, &c2, &side))
            .await
            .unwrap_err();
        assert_eq!(s.message(), "! [rejected] refs/heads/master (fetch first)");
    }

    #[tokio::test]
    async fn the_rpc_is_for_the_internal_token_only() {
        let r = rpc_fx(true, true).await;
        let c1 = r.f.forge.commit(None, &[("a", "1")]);
        for who in ["alice", "7:alice", "admin"] {
            let s = r
                .service
                .import_git_push(&ctx_of(who), request(&r.f, "", &c1))
                .await
                .unwrap_err();
            assert_eq!(s.code(), tonic::Code::PermissionDenied, "{who}");
        }
        // Without Forgejo no caller can skip hooks, so the RPC is closed to all of them.
        let none = rpc_fx(true, false).await;
        let c1 = none.f.forge.commit(None, &[("a", "1")]);
        let s = none
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), request(&none.f, "", &c1))
            .await
            .unwrap_err();
        assert_eq!(s.code(), tonic::Code::PermissionDenied);
        assert_eq!(none.f.count("SELECT COUNT(*) FROM git_map").await, 0);
    }

    #[tokio::test]
    async fn the_rpc_needs_a_configured_repo_root() {
        let r = rpc_fx(false, true).await;
        let c1 = r.f.forge.commit(None, &[("a", "1")]);
        let s = r
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), request(&r.f, "", &c1))
            .await
            .unwrap_err();
        assert_eq!(s.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn the_rpc_refuses_directories_outside_the_named_repository() {
        let r = rpc_fx(true, true).await;
        let c1 = r.f.forge.commit(None, &[("a", "1")]);
        // A second repository on the same forge, and a directory beside the root.
        let other = r.f.forge.root.join("o").join("secret.git");
        std::fs::create_dir_all(other.join("objects")).unwrap();
        let outside = r.f.forge.root.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let own = r.f.forge.objects();
        let cases: Vec<(&str, wire::ImportGitPushRequest)> = vec![
            ("git_dir of another repo", {
                let mut q = request(&r.f, "", &c1);
                q.git_dir = s(&other);
                q.object_dir = s(&other.join("objects"));
                q
            }),
            ("object_dir of another repo", {
                let mut q = request(&r.f, "", &c1);
                q.object_dir = s(&other.join("objects"));
                q
            }),
            ("object_dir outside the root", {
                let mut q = request(&r.f, "", &c1);
                q.object_dir = s(&outside);
                q
            }),
            ("alternate outside the repo", {
                let mut q = request(&r.f, "", &c1);
                q.alternate_dirs = vec![s(&outside)];
                q
            }),
            ("dot-dot out of objects", {
                let mut q = request(&r.f, "", &c1);
                q.object_dir = s(&own.join("..").join("..").join("secret.git").join("objects"));
                q
            }),
            ("the repo itself, not its objects", {
                let mut q = request(&r.f, "", &c1);
                q.object_dir = s(&r.f.forge.git_dir);
                q
            }),
        ];
        for (what, req) in cases {
            let e = r
                .service
                .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), req)
                .await
                .unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied, "{what}: {e:?}");
        }
        // Text that is not a path at all.
        for bad in ["", "no/such/dir", "a\0b"] {
            let mut q = request(&r.f, "", &c1);
            q.object_dir = bad.into();
            let e = r
                .service
                .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), q)
                .await
                .unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{bad:?}");
        }
        assert_eq!(r.f.count("SELECT COUNT(*) FROM git_map").await, 0);
    }

    #[tokio::test]
    async fn the_rpc_rejects_malformed_ids_and_pusher() {
        let r = rpc_fx(true, true).await;
        let c1 = r.f.forge.commit(None, &[("a", "1")]);
        let mut q = request(&r.f, "", &c1);
        q.updates[0].new_git = vec![1, 2, 3].into();
        let e = r
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), q)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let mut q = request(&r.f, "", &c1);
        q.pusher_id = -1;
        let e = r
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), q)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // git's all-zero id means "no ref": all-zero old and new is no update at all.
        let mut q = request(&r.f, "", &c1);
        q.updates[0].new_git = vec![0; 20].into();
        let e = r
            .service
            .import_git_push(&ctx_of(INTERNAL_PRINCIPAL), q)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    /// U48c: a stale entry of a renamed or deleted repository cannot record an import.
    #[tokio::test]
    async fn a_stale_entry_cannot_record_an_import() {
        let f = fx().await;
        let c1 = f.forge.commit(None, &[("a", "1")]);
        let commands = [cmd("refs/heads/master", None, Some(&c1))];
        let map = PushMap::new(f.registry.pool().clone(), &f.entry);
        let q = f.forge.whole();
        sqlx::query("UPDATE repos SET name = 'o/moved' WHERE id = $1")
            .bind(f.entry.key.0)
            .execute(f.registry.pool())
            .await
            .unwrap();
        let e = f
            .push
            .import(&f.entry, &map, &q, &commands, (true, false), (1, None))
            .await
            .err()
            .expect("a stale entry is refused");
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        sqlx::query("DELETE FROM repos WHERE id = $1")
            .bind(f.entry.key.0)
            .execute(f.registry.pool())
            .await
            .unwrap();
        let e = f
            .push
            .import(&f.entry, &map, &q, &commands, (true, false), (1, None))
            .await
            .err()
            .expect("a stale entry is refused");
        assert_eq!(e.code(), tonic::Code::NotFound);
        assert_eq!(f.count("SELECT COUNT(*) FROM cs_graph").await, 0);
    }
}
