//! Git-remote mode (SPEC §6.3): [`GitRemote`] implements the engine's [`Remote`] over real git.
//!
//! A fetch is `git fetch` into the repository's private bare repo (`repos/<id>/git-remote.git`)
//! followed by an incremental import into the object stack, so by the time the engine asks for
//! objects they are already local and `get_many` answers from the local stack. A push exports
//! the new changesets into the bare repo and runs `git push --force-with-lease`. Every git child
//! comes from `RealGit::command` (`asc_git::gitremote`), so the shim never recurses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::git::ObjectKind;
use asc_core::id::{GitOid, RefTarget};
use asc_core::path::RefName;
use asc_engine::Engine;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_engine::sync::{Error, Remote, Result};
use asc_git::export::{ExportRoot, Exporter};
use asc_git::gitremote::{self, GitRemoteRepo, Lease, PushFailure, PushRef};
use asc_git::import::{ImportMap, ImportPolicy, Importer, Tip};
use asc_git::odb::Odb;
use asc_state::StateDb;
use asc_state::state::git_map;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Expect, RefConflict, RefEntry, RefError, RefUpdate, RepoKey};
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::OnceCell;

use crate::cli::common::{CliEngine, CliError, Ctx, ObjectStack, WsCtx};
use crate::refs_guard::ReadOnlyRefs;
use crate::remote::url::RemoteSpec;
use crate::remote::{Intent, Opened};

/// Concurrent point reads from the local stack.
const READS: usize = 16;

/// `state.db.git_map` as the importer's map.
struct StateMap {
    db: StateDb,
    repo: RepoKey,
}

#[async_trait]
impl GitMapSink for StateMap {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        git_map::insert_many(&self.db, self.repo, rows)
            .await
            .map_err(|e| derive::Error::Sink(e.to_string()))
    }
}

#[async_trait]
impl ImportMap for StateMap {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        git_map::git_to_asc(&self.db, self.repo, git, kind)
            .await
            .map_err(|e| derive::Error::Sink(e.to_string()))
    }
}

fn remote_err(e: impl std::fmt::Display) -> Error {
    Error::Remote(e.to_string())
}

/// A git remote of one repository.
pub struct GitRemote {
    name: String,
    url: String,
    ns: String,
    repo: GitRemoteRepo,
    engine: Arc<CliEngine>,
    objects: Arc<ObjectStack>,
    map: Arc<StateMap>,
    synced: OnceCell<Vec<RefEntry>>,
}

impl GitRemote {
    /// A remote named `name` at `url` for the workspace `ws`.
    pub fn new(ctx: &Ctx, ws: &WsCtx, name: &str, url: &str) -> crate::cli::common::Result<Self> {
        let git = ctx.real_git()?.clone();
        let dir = ws.home.repo_dir(ws.repo.repo.id).join("git-remote.git");
        let repo =
            GitRemoteRepo::open_or_init(git, dir).map_err(|e| CliError::Fatal(e.to_string()))?;
        let engine = Arc::new(Engine::new(
            ws.objects.clone(),
            Arc::new(ReadOnlyRefs(ws.db.refs())),
            ws.repo.key,
        ));
        Ok(Self {
            name: name.to_owned(),
            url: absolute_url(&ctx.cwd, url),
            ns: gitremote::namespace(name),
            repo,
            engine,
            objects: ws.objects.clone(),
            map: Arc::new(StateMap {
                db: ws.db.clone(),
                repo: ws.repo.key,
            }),
            synced: OnceCell::new(),
        })
    }

    async fn blocking<T: Send + 'static>(
        f: impl FnOnce() -> std::result::Result<T, gitremote::GitRemoteError> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| remote_err(format!("git task: {e}")))?
            .map_err(remote_err)
    }

    /// `git fetch`, then import every ref tip the map does not know yet.
    async fn sync(&self) -> Result<Vec<RefEntry>> {
        let (repo, url, ns) = (self.repo.clone(), self.url.clone(), self.ns.clone());
        let tracked = Self::blocking(move || {
            repo.fetch(&url, &ns)?;
            repo.tracked(&ns)
        })
        .await?;
        let tips: Vec<Tip> = tracked
            .iter()
            .map(|(name, oid)| Tip {
                name: name.clone().into_bytes(),
                oid: *oid,
            })
            .collect();
        let (engine, objects, map, dir) = (
            self.engine.clone(),
            self.objects.clone(),
            self.map.clone(),
            self.repo.dir().to_path_buf(),
        );
        let handle = tokio::runtime::Handle::current();
        let report = tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let odb = Odb::open(&dir).map_err(remote_err)?;
                let mut importer =
                    Importer::new(&engine, objects, &odb, &*map, ImportPolicy::default());
                importer.import_commits(&tips).await.map_err(remote_err)
            })
        })
        .await
        .map_err(|e| remote_err(format!("import task: {e}")))??;
        for skipped in &report.skipped_refs {
            eprintln!("{}", skipped.warning());
        }
        for (name, oid, why) in &report.rejected_tags {
            eprintln!(
                "warning: asc: skipped tag '{}' ({oid}): {why}",
                String::from_utf8_lossy(name)
            );
        }
        // Annotated tags name their `TagRecord`; everything else its changeset.
        let mut wanted: HashMap<Vec<u8>, RefTarget> = HashMap::new();
        for (name, cs) in &report.refs {
            wanted.insert(name.clone(), RefTarget::Changeset(*cs));
        }
        for (name, tag) in &report.tags {
            wanted.insert(name.clone(), RefTarget::Tag(*tag));
        }
        let mut out: Vec<RefEntry> = wanted
            .into_iter()
            .filter_map(|(name, target)| {
                Some(RefEntry {
                    name: RefName::new(name).ok()?,
                    target,
                    version: 0,
                    expires_at: None,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Ok(out)
    }
}

/// A relative local path made absolute against `cwd`, so the remote works from any directory.
pub fn absolute_url(cwd: &Path, url: &str) -> String {
    let looks_remote = url.contains("://") || url.starts_with("git+") || {
        // `host:path` (scp-like), but not a drive letter.
        url.find(':').is_some_and(|i| i > 1) && !url[..url.find(':').unwrap_or(0)].contains('/')
    };
    if looks_remote || Path::new(url).is_absolute() {
        return url.to_owned();
    }
    let p: PathBuf = cwd.join(url);
    if p.exists() {
        p.display().to_string()
    } else {
        url.to_owned()
    }
}

#[async_trait]
impl Remote for GitRemote {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_refs(&self) -> Result<Vec<RefEntry>> {
        Ok(self.synced.get_or_try_init(|| self.sync()).await?.clone())
    }

    /// Everything the engine could upload is exported by `update_refs`, and git decides what the
    /// remote lacks, so the remote is reported to have every object.
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        Ok(vec![true; keys.len()])
    }

    /// Fetched history is already in the local stack (the import wrote it).
    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, Result<(ObjKey, Bytes)>> {
        stream::iter(keys)
            .map(move |k| async move {
                let found = self.objects.get_stored(&k).await?;
                Ok::<_, Error>(found.map(|b| (k, b)))
            })
            .buffer_unordered(READS)
            .filter_map(|r| async move { r.transpose() })
            .boxed()
    }

    async fn put_many(&self, _objects: Vec<(ObjKey, Bytes)>) -> Result<()> {
        Ok(())
    }

    async fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        let tracked: HashMap<String, GitOid> = {
            let (repo, ns) = (self.repo.clone(), self.ns.clone());
            Self::blocking(move || repo.tracked(&ns))
                .await?
                .into_iter()
                .collect()
        };
        let mut roots: Vec<ExportRoot> = Vec::new();
        let mut plan: Vec<(String, Option<usize>, Lease)> = Vec::new();
        for u in updates {
            let dst = std::str::from_utf8(u.name.as_bytes())
                .map_err(|_| remote_err("a ref name is not UTF-8"))?
                .to_owned();
            let lease = match u.expect {
                Expect::Any => Lease::Force,
                Expect::Absent => Lease::Absent,
                Expect::Value(RefTarget::Changeset(c)) => {
                    Lease::Value(self.engine.git_commit_of(c).await.map_err(remote_err)?)
                }
                Expect::Value(RefTarget::Tag(_)) => match tracked.get(&dst) {
                    Some(oid) => Lease::Value(*oid),
                    None => Lease::Absent,
                },
            };
            let root = match u.new {
                Some(RefTarget::Changeset(c)) => Some(ExportRoot::Changeset(c)),
                Some(RefTarget::Tag(t)) => Some(ExportRoot::Tag(t)),
                None => None,
            };
            let idx = root.map(|r| {
                roots.push(r);
                roots.len() - 1
            });
            plan.push((dst, idx, lease));
        }

        // Export before push: the objects the roots need that the bare repo lacks.
        let (engine, objects, dir, git) = (
            self.engine.clone(),
            self.objects.clone(),
            self.repo.dir().to_path_buf(),
            self.repo.git().clone(),
        );
        let handle = tokio::runtime::Handle::current();
        let report = tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let odb = Odb::open(&dir).map_err(remote_err)?;
                Exporter::new(&engine, &*objects, &git, &dir)
                    .export(&roots, &odb)
                    .await
                    .map_err(remote_err)
            })
        })
        .await
        .map_err(|e| remote_err(format!("export task: {e}")))??;

        let pushes: Vec<PushRef> = plan
            .iter()
            .map(|(dst, idx, lease)| PushRef {
                dst: dst.clone(),
                new: idx.and_then(|i| report.tips.get(i).copied()),
                lease: *lease,
            })
            .collect();
        let (repo, url) = (self.repo.clone(), self.url.clone());
        let to_push = pushes.clone();
        let pushed = tokio::task::spawn_blocking(move || repo.push(&url, &to_push))
            .await
            .map_err(|e| remote_err(format!("push task: {e}")))?;
        match pushed {
            Ok(()) => {}
            Err(PushFailure::Stale { dst, .. }) => {
                let name = RefName::new(dst.into_bytes()).map_err(remote_err)?;
                return Err(Error::Refs(RefError::Conflict(RefConflict {
                    name,
                    actual: None,
                })));
            }
            Err(PushFailure::Failed(msg)) => return Err(Error::Remote(msg)),
        }
        let (repo, ns) = (self.repo.clone(), self.ns.clone());
        let moved: Vec<(String, Option<GitOid>)> =
            pushes.into_iter().map(|p| (p.dst, p.new)).collect();
        Self::blocking(move || repo.set_tracked(&ns, &moved)).await
    }
}

/// Opens the git remote `name` at `stored` for the workspace.
pub fn open(
    ctx: &Ctx,
    ws: &WsCtx,
    name: String,
    stored: String,
    configured: bool,
    spec: RemoteSpec,
    _intent: Intent,
) -> crate::cli::common::Result<Opened> {
    super::server::check_transport(&stored)?;
    let remote = GitRemote::new(ctx, ws, &name, &stored)?;
    Ok(Opened {
        name,
        configured,
        url: stored,
        spec,
        remote: Box::new(remote),
        server: None,
    })
}

/// `git ls-remote` of `url` outside any repository: what `clone` needs to pick a branch.
pub async fn ls_remote(ctx: &Ctx, url: &str) -> crate::cli::common::Result<gitremote::Advertised> {
    super::server::check_transport(url)?;
    let git = ctx.real_git()?.clone();
    let (url, cwd) = (url.to_owned(), ctx.cwd.clone());
    tokio::task::spawn_blocking(move || gitremote::ls_remote(&git, &url, &cwd))
        .await
        .map_err(|e| CliError::Fatal(format!("git task: {e}")))?
        .map_err(|e| CliError::Raw(e.to_string(), 128))
}

fn bare(ctx: &Ctx, ws: &WsCtx) -> crate::cli::common::Result<GitRemoteRepo> {
    let dir = ws.home.repo_dir(ws.repo.repo.id).join("git-remote.git");
    GitRemoteRepo::open_or_init(ctx.real_git()?.clone(), dir)
        .map_err(|e| CliError::Raw(e.to_string(), 128))
}

/// `clone --reference <git-dir>`: `git fetch <path>` into the private bare repo (reachable
/// objects only, no alternates link), so the old checkout can be deleted afterwards.
pub async fn seed_from_references(
    ctx: &Ctx,
    ws: &WsCtx,
    paths: &[String],
) -> crate::cli::common::Result<()> {
    let repo = bare(ctx, ws)?;
    let paths: Vec<PathBuf> = paths.iter().map(|p| ctx.cwd.join(p)).collect();
    tokio::task::spawn_blocking(move || {
        for p in &paths {
            repo.fetch_reference(p)?;
        }
        Ok::<_, gitremote::GitRemoteError>(())
    })
    .await
    .map_err(|e| CliError::Fatal(format!("git task: {e}")))?
    .map_err(|e| CliError::Raw(e.to_string(), 128))
}

/// Removes the scratch refs `seed_from_references` left.
pub async fn drop_reference_refs(ctx: &Ctx, ws: &WsCtx) -> crate::cli::common::Result<()> {
    let repo = bare(ctx, ws)?;
    tokio::task::spawn_blocking(move || repo.drop_reference_refs())
        .await
        .map_err(|e| CliError::Fatal(format!("git task: {e}")))?
        .map_err(|e| CliError::Raw(e.to_string(), 128))
}
