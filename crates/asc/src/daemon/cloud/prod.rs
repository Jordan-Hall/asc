//! The real [`Uplinks`]: opens a repository of `state.db` the way a command does, connects to its
//! asc server without ever prompting, and drives B13's upload pipeline over it.
//!
//! The engine's upload future holds a `&dyn Fn` (the ignore rules), so it is not `Send`. It runs
//! on a blocking thread with `Handle::block_on`, one queue entry at a time; the daemon's other
//! tasks are not held up by it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_engine::branchless::guard::{DenyList, Guard};
use asc_engine::branchless::upload::{DEFAULT_MAX_SIZE, Outcome, UploadOptions, UploadQueue};
use asc_engine::sync::changes::ChangesExt;
use asc_rpc::server::{MAX_MESSAGE, wire};
use asc_rpc::services::repo_client::RepoClient;
use asc_state::AscHome;
use asc_state::StateDb;
use asc_state::changes::StateChanges;
use asc_state::drafts::{Drafts, QueueState};
use asc_state::state::repos::{self, RemoteKind};
use asc_state::state::workspaces;
use asc_store::refs::RepoKey;
use asc_wc::ignore::IgnoreStack;
use tonic::codec::CompressionEncoding;

use super::{
    Clock, CloudSync, Due, FEATURE_CLOUD, Facts, Phase, RpcCloudStore, StateRows, Step,
    SystemClock, Uplink, Uplinks, note, repos_of, workspace_name,
};
use crate::branchless::mode::{CloudSetting, RepoPolicy, Settings as ModeSettings};
use crate::cli::common::{Ctx, Gate, WsCtx};
use crate::cli::config;
use crate::cli::safe_dir::owned_by_current_user;
use crate::credential::Prompt;
use crate::daemon::scheduler::parse_size;
use crate::dispatch::Program;
use crate::env::GitEnv;
use crate::remote::server::{self, ServerRemote};
use crate::remote::{StateBoundaries, StoreDemote, options, store_set};

/// How long an opened repository is kept before it is opened again (credentials, config and the
/// advertised features are read afresh).
const KEEP: Duration = Duration::from_secs(300);
/// Ancestors of a queue head whose `ChangeMeta` ids are loaded for the closure.
const WARM_MAX: usize = 4096;

/// [`Uplinks`] over the user's `state.db`.
pub struct ProdUplinks {
    home: AscHome,
    db: tokio::sync::OnceCell<StateDb>,
    open: Mutex<HashMap<RepoKey, (Instant, Arc<dyn Uplink>)>>,
}

impl ProdUplinks {
    pub fn new(home: AscHome) -> Self {
        Self {
            home,
            db: tokio::sync::OnceCell::new(),
            open: Mutex::default(),
        }
    }

    async fn db(&self) -> Result<StateDb, String> {
        self.db
            .get_or_try_init(|| async { StateDb::open(&self.home).await })
            .await
            .cloned()
            .map_err(|e| note(&e.to_string()))
    }
}

#[async_trait]
impl Uplinks for ProdUplinks {
    async fn repos(&self, _phase: Phase, _now: i64) -> Result<Vec<RepoKey>, String> {
        let db = self.db().await?;
        let mut out = Vec::new();
        for key in repos_of(&db).await? {
            let server = repos::get(&db, key)
                .await
                .map_err(|e| note(&e.to_string()))?
                .is_some_and(|r| r.repo.remote_kind == RemoteKind::Server);
            if server {
                out.push(key);
            }
        }
        Ok(out)
    }

    async fn open(&self, repo: RepoKey) -> Result<Option<Arc<dyn Uplink>>, String> {
        if let Some((at, link)) = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&repo)
            && at.elapsed() < KEEP
        {
            return Ok(Some(link.clone()));
        }
        let db = self.db().await?;
        let Some(row) = repos::get(&db, repo)
            .await
            .map_err(|e| note(&e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(url) = row
            .repo
            .remote_url
            .clone()
            .filter(|_| row.repo.remote_kind == RemoteKind::Server)
        else {
            return Ok(None);
        };
        let list = workspaces::list(&db, Some(repo))
            .await
            .map_err(|e| note(&e.to_string()))?;
        let Some(path) = list
            .iter()
            .map(|w| PathBuf::from(&w.ws.path))
            .find(|p| p.is_dir())
        else {
            return Ok(None);
        };
        // The context's futures are not `Send` (the trust gate holds plain `dyn Fn`s), so the
        // whole open runs on a blocking thread.
        let home = self.home.clone();
        let handle = tokio::runtime::Handle::current();
        let Some(inner) =
            tokio::task::spawn_blocking(move || handle.block_on(open_inner(home, path, url)))
                .await
                .map_err(|e| note(&e.to_string()))??
        else {
            return Ok(None);
        };
        let link: Arc<dyn Uplink> = Arc::new(RepoUplink {
            inner: Arc::new(inner),
        });
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(repo, (Instant::now(), link.clone()));
        Ok(Some(link))
    }
}

/// Opens the repository at `path` and connects to its server. `None` for a git remote or a
/// mirror-mode repository: cloud is off there.
async fn open_inner(home: AscHome, path: PathBuf, url: String) -> Result<Option<Inner>, String> {
    let ctx = open_ctx(&home, path).await?;
    let settings = ModeSettings::resolve(&ctx.config, &ctx.env, &|k| std::env::var(k).ok())?;
    let real = ctx.real_git().ok().cloned();
    let db = ctx.ws().map_err(|e| note(&format!("{e:?}")))?.db.clone();
    // No prompt: a daemon never asks anything (SPEC §9.1).
    let Some(connected) = server::connect(ctx.config.file(), &db, real, None, &url, Prompt::Never)
        .await
        .map_err(|e| note(&format!("{e:?}")))?
    else {
        return Ok(None);
    };
    Ok(Some(Inner {
        ctx,
        remote: Arc::new(ServerRemote::new("cloud", connected)),
        settings,
        features: Mutex::default(),
        clock: Arc::new(SystemClock),
    }))
}

/// A context for the workspace at `path`, built the way `Ctx::open` builds one but without the
/// version-skew check (that stops this very daemon).
async fn open_ctx(home: &AscHome, path: PathBuf) -> Result<Ctx, String> {
    let env = GitEnv::from_process();
    let get = |k: &str| std::env::var_os(k);
    let safe = config::safe_directories(&env, &get).map_err(|e| note(&format!("{e:?}")))?;
    let home_fn = || Ok(home.clone());
    let gate = Gate {
        safe: &safe,
        home: &home_fn,
        owned: &owned_by_current_user,
    };
    let ctx = Ctx::open_with(Program::Asc, env, path, &gate)
        .await
        .map_err(|e| note(&format!("{e:?}")))?;
    if ctx.ws_opt().is_none() {
        return Err("the workspace is not admitted".into());
    }
    Ok(ctx)
}

struct Inner {
    ctx: Ctx,
    remote: Arc<ServerRemote>,
    settings: ModeSettings,
    features: Mutex<Vec<String>>,
    clock: Arc<dyn Clock>,
}

struct RepoUplink {
    inner: Arc<Inner>,
}

impl Inner {
    fn ws(&self) -> Result<&WsCtx, String> {
        self.ctx.ws().map_err(|e| note(&format!("{e:?}")))
    }

    fn rows(&self) -> Result<StateRows, String> {
        let ws = self.ws()?;
        Ok(StateRows::new(ws.db.clone(), ws.repo.key))
    }

    fn syncer(&self, rows: StateRows) -> CloudSync<RpcCloudStore> {
        let name = workspace_name(&self.settings.workspace);
        let wire = self.remote.session().wire.clone();
        CloudSync::new(
            rows,
            RpcCloudStore::new(wire, name.clone()),
            name,
            self.clock.clone(),
        )
    }

    fn guard(&self) -> Result<Guard, String> {
        let patterns = self.ctx.config.get_all("asc.cloud.denyPaths");
        let deny = if patterns.is_empty() {
            DenyList::default_list()
        } else {
            DenyList::new(patterns.iter().map(String::as_str))
        }
        .map_err(|e| note(&e.to_string()))?;
        Guard::new(deny).map_err(|e| note(&e.to_string()))
    }

    /// The ignore rules of the workspace as a path test. A path is ignored when the rules in
    /// effect along its directories say so.
    fn ignored(&self, ws: &WsCtx) -> impl Fn(&RepoPath) -> bool + use<> {
        let sources = crate::cli::check_ignore::ignore_sources(&self.ctx, ws);
        let root = ws.projection.toplevel().to_path_buf();
        move |p: &RepoPath| {
            let rel = String::from_utf8_lossy(p.as_bytes()).into_owned();
            let abs = root.join(&rel);
            let mut stack = IgnoreStack::new(&root, &sources);
            let mut dir = root.clone();
            let _ = stack.push_dir(&dir);
            if let Some(parent) = Path::new(&rel).parent() {
                for part in parent.components() {
                    dir.push(part);
                    let _ = stack.push_dir(&dir);
                }
            }
            stack.is_ignored(&abs, false)
        }
    }

    /// Loads the `ChangeMeta` ids of `head` and its nearest ancestors so the push closure can
    /// add their `m/` objects (the hook is synchronous).
    async fn warm(&self, ext: &ChangesExt<StateChanges>, head: ChangesetId) {
        let Ok(ws) = self.ws() else { return };
        let mut seen: HashSet<ChangesetId> = HashSet::from([head]);
        let mut queue = VecDeque::from([head]);
        let mut all = Vec::new();
        while let Some(cs) = queue.pop_front() {
            all.push(cs);
            if all.len() >= WARM_MAX {
                break;
            }
            if let Ok(body) = ws.engine.get_changeset(cs).await {
                for &p in body.parents.iter() {
                    if seen.insert(p) {
                        queue.push_back(p);
                    }
                }
            }
        }
        let _ = ext.warm(&all).await;
    }

    async fn upload_once(&self, now: i64) -> Result<Option<Step>, String> {
        let ws = self.ws()?;
        let rows = self.rows()?;
        let Some(entry) = rows
            .next_pending(now)
            .await
            .map_err(|e| note(&e.to_string()))?
        else {
            return Ok(None);
        };
        let syncer = self.syncer(rows.clone());
        let guard = self.guard()?;
        let ignored = self.ignored(ws);
        let boundaries = StateBoundaries::new(ws);
        let mut sync = options(&self.ctx, &boundaries);
        let demote = StoreDemote(store_set(&self.ctx, ws).map_err(|e| note(&format!("{e:?}")))?);
        sync.demote = Some(&demote);
        let features = self
            .features
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ext = ChangesExt::new(StateChanges::new(ws.db.clone(), ws.repo.key), &features);
        self.warm(&ext, entry.cs).await;
        sync.ext = &ext;
        let max_size = self
            .ctx
            .config
            .get("asc.cloud.maxChangesetSize")
            .and_then(|v| parse_size(&v))
            .unwrap_or(DEFAULT_MAX_SIZE);
        let opts = UploadOptions {
            guard: &guard,
            ignored: &ignored,
            max_size,
            skip_guard: false,
            ignore_size: false,
            sync,
            heads: &syncer,
        };
        let done = ws
            .engine
            .drain_one(&*self.remote, &rows, now, &opts)
            .await
            .map_err(|e| note(&e.to_string()))?;
        Ok(done.map(|(_, outcome)| match outcome {
            Outcome::Uploaded(_) => Step::Uploaded,
            Outcome::Held(_) => Step::Held,
            Outcome::TooLarge { .. } => Step::TooLarge,
        }))
    }
}

#[async_trait]
impl Uplink for RepoUplink {
    async fn facts(&self) -> Result<Facts, String> {
        let i = &self.inner;
        let settings = i.settings.clone();
        let off = RepoPolicy {
            branchless: false,
            cloud: false,
            public: false,
        };
        // Nothing is asked of the server for a repo the user switched off.
        if settings.cloud == CloudSetting::Off || !settings.branchless {
            return Ok(Facts {
                settings,
                policy: off,
                cloud_v1: false,
            });
        }
        let version = i
            .remote
            .client()
            .server_version()
            .await
            .map_err(|e| note(&e.to_string()))?;
        *i.features.lock().unwrap_or_else(|e| e.into_inner()) = version.features.clone();
        let cloud_v1 = version.features.iter().any(|f| f == FEATURE_CLOUD);
        if !cloud_v1 {
            return Ok(Facts {
                settings,
                policy: off,
                cloud_v1,
            });
        }
        let wire_conn = i.remote.session().wire.clone();
        let req = wire::RepoConfigRequest {
            repo: Some(wire::RepoRef {
                name: wire_conn.repo.clone(),
                id_mode: asc_rpc::IdMode::Asc.to_wire(),
            }),
            rev: String::new(),
        };
        let conf = RepoClient::new(wire_conn.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(CompressionEncoding::Zstd)
            .repo_config(wire_conn.request(req, None))
            .await
            .map_err(|s| note(&asc_rpc::client::CallError::from(s).to_string()))?
            .into_inner();
        Ok(Facts {
            settings,
            policy: RepoPolicy {
                branchless: conf.branchless,
                cloud: conf.cloud,
                public: conf.visibility == "public",
            },
            cloud_v1,
        })
    }

    async fn ping(&self) -> Result<(), String> {
        self.inner
            .remote
            .client()
            .server_version()
            .await
            .map(|_| ())
            .map_err(|e| note(&e.to_string()))
    }

    async fn next_due(&self, now: i64) -> Result<Option<Due>, String> {
        let rows = self.inner.rows()?;
        Ok(rows
            .next_pending(now)
            .await
            .map_err(|e| note(&e.to_string()))?
            .map(|e| Due {
                cs: e.cs,
                attempts: e.attempts,
            }))
    }

    async fn upload_next(&self, now: i64) -> Result<Option<Step>, String> {
        let inner = self.inner.clone();
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || handle.block_on(inner.upload_once(now)))
            .await
            .map_err(|e| note(&e.to_string()))?
    }

    async fn record_failure(
        &self,
        cs: ChangesetId,
        permanent: bool,
        next_at: i64,
        error: &str,
    ) -> Result<(), String> {
        let ws = self.inner.ws()?;
        let state = if permanent {
            QueueState::Failed
        } else {
            QueueState::Pending
        };
        Drafts::new(ws.repo.key)
            .queue_failed(&ws.db, cs, state, next_at, error)
            .await
            .map_err(|e| note(&e.to_string()))
    }

    async fn sync(&self) -> Result<(), String> {
        let rows = self.inner.rows()?;
        self.inner.syncer(rows).run().await.map(|_| ())
    }
}
