//! Remotes of the CLI (SPEC §6): URL parsing, the remote kinds, how a configured remote becomes
//! an engine [`Remote`], and the `state.db` boundary store behind the history window.

pub mod bucket;
pub mod discover;
pub mod git;
pub mod server;
pub mod url;

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::codec::decode_envelope;
use asc_core::content::CHUNK_THRESHOLD;
use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::{ChunkList, Kind};
use asc_engine::graph::Error as GraphError;
use asc_engine::history::BoundaryStore;
use asc_engine::sync::{BucketRemote, DEFAULT_ANCESTRY_BUDGET, Demote, Remote, SyncOptions};
use asc_state::StateDb;
use asc_state::state::{history_boundary, repos};
use asc_store::bucket::BucketObjects;
use asc_store::bucketrefs::BucketRefs;
use asc_store::cachedir::CacheDir;
use asc_store::key::ObjKey;
use asc_store::objects::{Objects, decode_verified};
use asc_store::refs::RepoKey;
use asc_store::storeset::StoreSet;

use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use url::{RefsMode, RemoteSpec};

/// `state.db.history_boundary` as the engine's [`BoundaryStore`] (SPEC §6.3).
pub struct StateBoundaries {
    db: StateDb,
    repo: RepoKey,
}

impl StateBoundaries {
    pub fn new(ws: &WsCtx) -> Self {
        Self {
            db: ws.db.clone(),
            repo: ws.repo.key,
        }
    }
}

fn store_err(e: asc_state::Error) -> GraphError {
    GraphError::BoundaryStore(e.to_string())
}

#[async_trait]
impl BoundaryStore for StateBoundaries {
    async fn list(&self) -> asc_engine::graph::Result<Vec<ChangesetId>> {
        history_boundary::list(&self.db, self.repo)
            .await
            .map_err(store_err)
    }

    async fn add(&self, ids: &[ChangesetId]) -> asc_engine::graph::Result<()> {
        for id in ids {
            history_boundary::insert(&self.db, self.repo, *id)
                .await
                .map_err(store_err)?;
        }
        Ok(())
    }

    async fn remove(&self, ids: &[ChangesetId]) -> asc_engine::graph::Result<()> {
        for id in ids {
            history_boundary::remove(&self.db, self.repo, *id)
                .await
                .map_err(store_err)?;
        }
        Ok(())
    }
}

/// Moves pushed objects from `store/` to `cache/` (SPEC §6.3, push step 5).
pub struct StoreDemote(pub StoreSet);

#[async_trait]
impl Demote for StoreDemote {
    async fn demote(&self, keys: &[ObjKey]) -> asc_engine::sync::Result<()> {
        self.0.demote(keys).await?;
        Ok(())
    }
}

/// The `store/` and `cache/` directories the way the object stack builds them.
pub fn store_set(ctx: &Ctx, ws: &WsCtx) -> Result<StoreSet> {
    let cache_root = ctx
        .config
        .get("asc.cache.dir")
        .map(|p| match (p.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => std::path::PathBuf::from(p),
        })
        .unwrap_or_else(|| ws.home.cache());
    Ok(StoreSet {
        store: CacheDir::open(ws.home.store(), ws.home.tmp())?,
        cache: CacheDir::open(cache_root, ws.home.tmp())?,
    })
}

/// A remote opened for an operation.
pub struct Opened {
    /// The remote's name, used for remote-tracking refs.
    pub name: String,
    /// Whether `name` is a configured remote (a URL typed on the command line is not).
    pub configured: bool,
    /// The URL as stored or typed.
    pub url: String,
    pub spec: RemoteSpec,
    pub remote: Box<dyn Remote>,
    /// The same remote as a [`server::ServerRemote`], when it is an asc server (push modes).
    pub server: Option<Arc<server::ServerRemote>>,
}

/// Whether the remote is used to read or to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Read,
    Write,
}

/// The `ascRefs` setting of a configured remote.
pub fn refs_mode(ctx: &Ctx, name: &str) -> RefsMode {
    match ctx.config.get(&format!("remote.{name}.ascRefs")).as_deref() {
        Some("shared") => RefsMode::Shared,
        _ => RefsMode::Local,
    }
}

/// The names of the configured remotes (those with a `url`), in file order.
pub fn remote_names(ctx: &Ctx) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for section in ctx
        .config
        .file()
        .sections_by_name("remote")
        .into_iter()
        .flatten()
    {
        let Some(sub) = section.header().subsection_name() else {
            continue;
        };
        let name = sub.to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.retain(|n| ctx.config.get(&format!("remote.{n}.url")).is_some());
    names
}

fn looks_like_url(arg: &str) -> bool {
    arg.contains("://")
        || arg.contains('/')
        || arg.contains('\\')
        || arg.starts_with('.')
        || arg.contains('@')
        || (arg.len() > 1 && arg.as_bytes()[1] == b':')
}

/// The git message for a remote that is neither configured nor a URL.
pub fn no_such_remote(arg: &str) -> CliError {
    CliError::Raw(
        format!(
            "fatal: '{arg}' does not appear to be a git repository\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists."
        ),
        128,
    )
}

fn not_yet(spec: &RemoteSpec) -> CliError {
    CliError::Fatal(format!(
        "asc: {} remotes are not supported yet (bucket remotes only)",
        spec.kind()
    ))
}

fn anon(url: &str) -> String {
    crate::cli::url_display::anonymize(url)
}

fn bucket_err(url: &str, why: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: cannot open the bucket {}: {why}", anon(url)))
}

/// The message of SPEC §3.4 for a bucket whose refs another machine holds.
pub fn other_machine(url: &str) -> CliError {
    CliError::Fatal(format!(
        "asc: refs for {} are local to another machine; use an asc server or --refs shared (U30)",
        anon(url)
    ))
}

/// Opens the remote called `arg` (a configured name, else a URL).
pub async fn open(ctx: &Ctx, ws: &WsCtx, arg: &str, intent: Intent) -> Result<Opened> {
    let configured = ctx.config.get(&format!("remote.{arg}.url"));
    let (name, stored, is_named) = match configured {
        Some(u) => (arg.to_owned(), u, true),
        None if looks_like_url(arg) => ("url".to_owned(), arg.to_owned(), false),
        None => return Err(no_such_remote(arg)),
    };
    let mode = if is_named {
        refs_mode(ctx, &name)
    } else {
        RefsMode::Local
    };
    match url::parse(&stored, mode) {
        Ok(spec @ (RemoteSpec::Server { .. } | RemoteSpec::Discover { .. })) => {
            let real = ctx.real_git().ok().cloned();
            let connected = server::connect(
                ctx.config.file(),
                &ws.db,
                real,
                is_named.then_some(name.as_str()),
                &stored,
                crate::credential::terminal_prompt(),
            )
            .await?;
            return match connected {
                Some(c) => Ok(server::opened(name, stored, is_named, spec, c)),
                None => git::open(ctx, ws, name, stored, is_named, spec, intent),
            };
        }
        Ok(spec @ RemoteSpec::Git { .. }) => {
            return git::open(ctx, ws, name, stored, is_named, spec, intent);
        }
        _ => {}
    }
    open_url(&ws.db, name, stored, is_named, mode, intent).await
}

/// Opens the remote at `stored` (a URL as the user typed it) over `db`.
pub async fn open_url(
    db: &StateDb,
    name: String,
    stored: String,
    configured: bool,
    mode: RefsMode,
    intent: Intent,
) -> Result<Opened> {
    let spec = url::parse(&stored, mode).map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    let RemoteSpec::Bucket {
        url: bucket_url,
        refs,
    } = &spec
    else {
        return Err(not_yet(&spec));
    };
    if intent == Intent::Write {
        bucket::ensure_local_dir(bucket_url)?;
    } else if !bucket::exists(bucket_url) {
        return Err(CliError::Fatal(format!(
            "asc: '{}' does not appear to be an asc bucket",
            anon(&stored)
        )));
    }
    let objects =
        Arc::new(BucketObjects::from_url(bucket_url).map_err(|e| bucket_err(bucket_url, e))?);
    let remote: Box<dyn Remote> = match refs {
        RefsMode::Local => {
            let machine = db.machine_id().await?;
            match bucket::holder(bucket_url, machine)
                .await
                .map_err(|e| bucket_err(bucket_url, e))?
            {
                bucket::Holder::Other => return Err(other_machine(bucket_url)),
                bucket::Holder::Nobody | bucket::Holder::Me => {}
            }
            let key = repos::bucket_refs_row(db, bucket_url).await?;
            Box::new(BucketRemote::new(
                name.clone(),
                objects,
                Arc::new(db.refs()),
                key,
            ))
        }
        RefsMode::Shared => {
            let (store, root) =
                bucket::open_store(bucket_url).map_err(|e| bucket_err(bucket_url, e))?;
            let refs = BucketRefs::connect(store, root)
                .await
                .map_err(|e| bucket_err(bucket_url, e))?;
            Box::new(BucketRemote::new(
                name.clone(),
                objects,
                Arc::new(refs),
                RepoKey(0),
            ))
        }
    };
    Ok(Opened {
        name,
        configured,
        url: stored,
        spec,
        remote,
        server: None,
    })
}

/// The first push to a `--refs local` bucket writes the `refs-local` key (SPEC §3.4).
pub async fn claim_local_refs(ws: &WsCtx, opened: &Opened) -> Result<()> {
    let RemoteSpec::Bucket {
        url,
        refs: RefsMode::Local,
    } = &opened.spec
    else {
        return Ok(());
    };
    let machine = ws.db.machine_id().await?;
    match bucket::claim(url, machine)
        .await
        .map_err(|e| bucket_err(url, e))?
    {
        bucket::Holder::Other => Err(other_machine(url)),
        _ => Ok(()),
    }
}

/// Sync options from the `asc.fetch.*` keys.
pub fn options<'a>(ctx: &Ctx, boundaries: &'a StateBoundaries) -> SyncOptions<'a> {
    let mut o = SyncOptions::new(boundaries);
    if let Some(w) = ctx
        .config
        .get("asc.fetch.historyWindow")
        .and_then(|v| v.parse::<u32>().ok())
    {
        o.window = Some(w);
    }
    o.ancestry_budget = ctx
        .config
        .get("asc.fetch.ancestryBudget")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_ANCESTRY_BUDGET);
    o
}

/// The `fetch` refspecs of a configured remote, else git's default for `name`.
pub fn fetch_refspecs(ctx: &Ctx, name: &str) -> Vec<String> {
    let configured = ctx.config.get_all(&format!("remote.{name}.fetch"));
    if configured.is_empty() {
        vec![format!("+refs/heads/*:refs/remotes/{name}/*")]
    } else {
        configured
    }
}

// ------------------------------------------------------------------------------ hydration

/// Most trees and files one hydration will walk (a hostile or broken server cannot make a
/// checkout run for ever).
const HYDRATE_MAX_TREES: usize = 4_000_000;
const HYDRATE_MAX_FILES: usize = 20_000_000;

fn hydrate_error(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// Downloads the keys of `keys` the local tiers lack into the object stack, verifying each.
async fn pull(ctx: &Ctx, ws: &WsCtx, remote: &dyn Remote, keys: Vec<ObjKey>) -> Result<usize> {
    if keys.is_empty() {
        return Ok(0);
    }
    let near = store_set(ctx, ws)?;
    let have = near.has_many(&keys).await.map_err(hydrate_error)?;
    let want: Vec<ObjKey> = keys
        .into_iter()
        .zip(have)
        .filter(|(_, h)| !*h)
        .map(|(k, _)| k)
        .collect();
    if want.is_empty() {
        return Ok(0);
    }
    let asked: HashSet<ObjKey> = want.iter().copied().collect();
    let mut got: HashSet<ObjKey> = HashSet::new();
    let mut stream = remote.get_many(want.clone());
    while let Some(item) = futures::StreamExt::next(&mut stream).await {
        let (key, stored) = item.map_err(hydrate_error)?;
        if !asked.contains(&key) || got.contains(&key) {
            continue;
        }
        decode_verified(&key, &stored).map_err(hydrate_error)?;
        ws.objects.put(&key, stored).await.map_err(hydrate_error)?;
        got.insert(key);
    }
    if let Some(k) = want.iter().find(|k| !got.contains(k)) {
        return Err(CliError::Fatal(format!("asc: the remote does not have {k}")));
    }
    Ok(want.len())
}

/// Brings the content of `files` (id and size) into the local store: a work tree is written from
/// the local tiers only, so a checkout of commits another machine made needs this first.
pub async fn hydrate_files(
    ctx: &Ctx,
    ws: &WsCtx,
    remote: &dyn Remote,
    files: &[(FileId, u64)],
) -> Result<usize> {
    if files.len() > HYDRATE_MAX_FILES {
        return Err(CliError::Fatal(
            "asc: too many files to download at once".into(),
        ));
    }
    let mut seen: HashSet<FileId> = HashSet::new();
    let (mut plain, mut lists) = (Vec::new(), Vec::new());
    for (file, size) in files {
        if !seen.insert(*file) {
            continue;
        }
        if *size > CHUNK_THRESHOLD {
            lists.push(ObjKey::ChunkList(*file));
        } else {
            plain.push(ObjKey::File(*file));
        }
    }
    let mut fetched = pull(ctx, ws, remote, plain).await?;
    fetched += pull(ctx, ws, remote, lists.clone()).await?;
    let mut chunks = Vec::new();
    for key in lists {
        let Some(bytes) = ws.objects.get(&key).await.map_err(hydrate_error)? else {
            continue;
        };
        let list: ChunkList = decode_envelope(&bytes).map_err(hydrate_error)?;
        chunks.extend(list.chunks.iter().map(|(id, _)| ObjKey::Chunk(*id)));
    }
    let mut unique: HashSet<ObjKey> = HashSet::new();
    chunks.retain(|k| unique.insert(*k));
    fetched += pull(ctx, ws, remote, chunks).await?;
    Ok(fetched)
}

/// Brings every tree below `root` and every file in them into the local store, level by level
/// with batched downloads (the `clone` checkout; SPEC §6.3 "tree then blob").
pub async fn hydrate(ctx: &Ctx, ws: &WsCtx, remote: &dyn Remote, root: TreeId) -> Result<usize> {
    let mut fetched = 0;
    let mut level = vec![root];
    let mut seen: HashSet<TreeId> = HashSet::from([root]);
    let mut files: Vec<(FileId, u64)> = Vec::new();
    while !level.is_empty() {
        fetched += pull(
            ctx,
            ws,
            remote,
            level.iter().map(|t| ObjKey::Tree(*t)).collect(),
        )
        .await?;
        let mut next = Vec::new();
        for batch in level.chunks(256) {
            let trees = ws.engine.get_trees(batch).await.map_err(hydrate_error)?;
            for tree in trees {
                for e in tree.entries() {
                    match e.kind {
                        Kind::Dir => {
                            let id = TreeId::from_bytes(e.id);
                            if seen.insert(id) {
                                next.push(id);
                            }
                        }
                        Kind::File | Kind::Exec | Kind::Symlink => {
                            files.push((FileId::from_bytes(e.id), e.size));
                        }
                        Kind::Gitlink => {}
                    }
                }
            }
        }
        if seen.len() > HYDRATE_MAX_TREES || files.len() > HYDRATE_MAX_FILES {
            return Err(CliError::Fatal(
                "asc: the tree is too large to download at once".into(),
            ));
        }
        level = next;
    }
    Ok(fetched + hydrate_files(ctx, ws, remote, &files).await?)
}
