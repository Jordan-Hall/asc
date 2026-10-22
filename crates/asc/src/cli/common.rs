//! The command context. `Ctx` is built once per invocation: argv[0] program, git environment,
//! the config snapshot, and, inside a workspace, `AscHome`, `StateDb`, the synthetic `.git`, the
//! object stack (SPEC §3.5) and an `Engine` over read-only refs. It owns the single local
//! ref-update path (SPEC §9.4.1).
#![allow(dead_code)] // The accessors are used by the commands of later units.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget};
use asc_core::object::{Change, Kind};
use asc_core::path::RefName;
use asc_engine::Engine;
use asc_git::projection::{Head, Projection, ProjectionError};
use asc_git::realgit::{RealGit, RealGitError};
use asc_state::state::repos::{self, RemoteKind, Repo};
use asc_state::state::stage::{self, Leaf, LeafDelta, StageRow, StageSet};
use asc_state::state::workspaces::{self, Workspace};
use asc_state::{AscHome, StateDb, WorkspaceId};
use asc_store::cachedir::CacheDir;
use asc_store::codec::encode_stored;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{Objects, Result as ObjResult};
use asc_store::refs::{Expect, RefError, RefTxn, Refs};
use asc_store::storeset::StoreSet;
use asc_store::tiered::Tiered;
use futures::stream::{BoxStream, StreamExt};

use super::config::{ConfigSnapshot, LocalConfig};
use super::output::OutputKind;
use crate::dispatch::Program;
use crate::env::GitEnv;
use crate::ext::{self, RefChange, RefTxnView, RemoteSpec};
use crate::msg::Msg;
use crate::refs_guard::{LocalRefs, ReadOnlyRefs};
use crate::child;

/// `Result` of a command.
pub type Result<T, E = CliError> = std::result::Result<T, E>;

/// A command failure and how it is reported.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A message with its own prefix and exit code.
    #[error("{0:?}")]
    Msg(Msg),
    /// `fatal: <text>`, exit 128.
    #[error("{0}")]
    Fatal(String),
    /// A usage error: the text as printed, exit 129.
    #[error("{0}")]
    Usage(String),
    /// Text printed as is with the given exit code.
    #[error("{0}")]
    Raw(String, u8),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    State(#[from] asc_state::Error),
    #[error(transparent)]
    Home(#[from] asc_state::HomeError),
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Tree(#[from] asc_engine::tree::Error),
    #[error(transparent)]
    Commit(#[from] asc_engine::commit::Error),
    #[error(transparent)]
    Objects(#[from] asc_store::objects::Error),
}

impl From<RealGitError> for CliError {
    fn from(e: RealGitError) -> Self {
        Self::Raw(e.to_string(), e.exit_code().clamp(0, 255) as u8)
    }
}

impl CliError {
    /// Prints the error as git would and returns its exit code.
    pub fn report(&self, prog: Program) -> ExitCode {
        let (text, code) = match self {
            Self::Msg(m) => (m.line(prog), m.exit_code()),
            Self::Usage(t) => (format!("{t}\n"), 129),
            Self::Raw(t, c) => (format!("{t}\n"), *c),
            other => (format!("fatal: {other}\n"), 128),
        };
        eprint!("{text}");
        ExitCode::from(code)
    }
}

/// What a ref update does to stage set 0 when HEAD's commit moves (SPEC §9.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StagingAfterMove {
    /// Rebase the rows across the move so the effective staged state is unchanged.
    #[default]
    Keep,
    /// Replace set 0 with these rows (commands that define a new index state).
    Set(Vec<StageRow>),
}

/// How display ids are chosen (SPEC §11.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayIds {
    Git,
    Asc,
    Both,
}

/// The object stack of SPEC §3.5: read-only cache tiers, `StoreSet{store, cache}`, then the
/// default remote's object source. New objects go to `store/`.
pub struct ObjectStack(Arc<dyn Objects>);

impl ObjectStack {
    /// Builds the stack from the `asc.cache.*` keys and the remote tier.
    pub fn build(
        home: &AscHome,
        config: &ConfigSnapshot,
        remote: Option<Arc<dyn Objects>>,
    ) -> ObjResult<Self> {
        let expand = |p: &str| match (p.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => PathBuf::from(p),
        };
        let cache_root = config
            .get("asc.cache.dir")
            .map(|p| expand(&p))
            .unwrap_or_else(|| home.cache());
        let set = StoreSet {
            store: CacheDir::open(home.store(), home.tmp())?,
            cache: CacheDir::open(cache_root, home.tmp())?,
        };
        let read_only = config
            .get_all("asc.cache.readOnly")
            .iter()
            .map(|p| CacheDir::open_read_only(expand(p)))
            .collect::<ObjResult<Vec<_>>>()?;
        let near = Near { read_only, set };
        let inner: Arc<dyn Objects> = match remote {
            Some(r) => Arc::new(Tiered::new(near, ObjectStack(r))),
            None => Arc::new(near),
        };
        Ok(Self(inner))
    }

    /// A stack over an explicit object store (tests).
    pub fn over(objects: Arc<dyn Objects>) -> Self {
        Self(objects)
    }
}

#[async_trait]
impl Objects for ObjectStack {
    async fn get(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        self.0.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        self.0.get_stored(key).await
    }
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> ObjResult<Option<Bytes>> {
        self.0.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> ObjResult<()> {
        self.0.put(key, stored).await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
        self.0.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> ObjResult<()> {
        self.0.delete(key).await
    }
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
        self.0.list(prefix)
    }
}

/// The local tiers: read-only caches (a corrupt file there is a miss), then `store/` and
/// `cache/`.
struct Near {
    read_only: Vec<CacheDir>,
    set: StoreSet,
}

#[async_trait]
impl Objects for Near {
    async fn get(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        for tier in &self.read_only {
            if let Ok(Some(data)) = tier.get(key).await {
                return Ok(Some(data));
            }
        }
        self.set.get(key).await
    }

    async fn get_stored(&self, key: &ObjKey) -> ObjResult<Option<Bytes>> {
        for tier in &self.read_only {
            if let Ok(Some(data)) = tier.get(key).await {
                return Ok(Some(encode_stored(&data)));
            }
        }
        self.set.get_stored(key).await
    }

    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> ObjResult<Option<Bytes>> {
        for tier in &self.read_only {
            if let Ok(Some(data)) = tier.get_range(key, r.clone()).await {
                return Ok(Some(data));
            }
        }
        self.set.get_range(key, r).await
    }

    async fn put(&self, key: &ObjKey, stored: Bytes) -> ObjResult<()> {
        self.set.put(key, stored).await
    }

    async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
        let mut found = self.set.has_many(keys).await?;
        for tier in &self.read_only {
            let missing: Vec<usize> = (0..keys.len()).filter(|&i| !found[i]).collect();
            if missing.is_empty() {
                break;
            }
            let ask: Vec<ObjKey> = missing.iter().map(|&i| keys[i]).collect();
            if let Ok(have) = tier.has_many(&ask).await {
                for (i, present) in missing.into_iter().zip(have) {
                    found[i] = present;
                }
            }
        }
        Ok(found)
    }

    async fn delete(&self, key: &ObjKey) -> ObjResult<()> {
        self.set.delete(key).await
    }

    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
        self.set.list(prefix).boxed()
    }
}

/// The CLI's engine: read-only refs, so an engine-side ref move fails in tests.
pub type CliEngine = Engine<ObjectStack, ReadOnlyRefs<asc_store::sqlrefs::SqlRefs>>;

/// Everything bound to the workspace of the current directory.
pub struct WsCtx {
    pub home: AscHome,
    pub db: StateDb,
    pub projection: Projection,
    pub workspace: Workspace,
    pub repo: Repo,
    pub engine: CliEngine,
    pub objects: Arc<ObjectStack>,
    refs: Arc<ReadOnlyRefs<asc_store::sqlrefs::SqlRefs>>,
    local: LocalRefs,
}

impl WsCtx {
    /// Opens the state of the workspace `projection` belongs to.
    pub async fn open(projection: Projection, config: &ConfigSnapshot) -> Result<Self> {
        let home = AscHome::for_workspace(projection.git_dir())?;
        let db = StateDb::open(&home).await?;
        let workspace = workspaces::get(&db, projection.workspace())
            .await?
            .ok_or_else(|| CliError::Fatal("asc: this workspace is not registered in state.db".into()))?;
        let repo = repos::get(&db, workspace.ws.repo)
            .await?
            .ok_or_else(|| CliError::Fatal("asc: the repository of this workspace is missing".into()))?;
        let spec = RemoteSpec {
            kind: repo.repo.remote_kind,
            url: repo.repo.remote_url.clone(),
        };
        let remote = match spec.kind {
            RemoteKind::LocalOnly | RemoteKind::GitRemote => None,
            _ => ext::remote_objects(&spec),
        };
        let objects = Arc::new(ObjectStack::build(&home, config, remote)?);
        let refs = Arc::new(ReadOnlyRefs(db.refs()));
        let engine = Engine::new(objects.clone(), refs.clone(), repo.key);
        Ok(Self {
            local: LocalRefs::new(db.refs()),
            home,
            db,
            projection,
            workspace,
            repo,
            engine,
            objects,
            refs,
        })
    }

    /// The commit HEAD resolves to; `None` on an unborn branch.
    pub async fn head_commit(&self) -> Result<Option<ChangesetId>> {
        match self.projection.read_head()? {
            Head::Branch(b) => {
                let name = RefName::new(format!("refs/heads/{b}").into_bytes())
                    .map_err(|e| CliError::Fatal(format!("HEAD names a bad branch: {e}")))?;
                let entry = self.refs.get(self.repo.key, &name).await?;
                Ok(entry.and_then(|e| match e.target {
                    RefTarget::Changeset(c) => Some(c),
                    RefTarget::Tag(_) => None,
                }))
            }
            Head::Detached(_) => Ok(workspaces::get(&self.db, self.workspace.ws.id)
                .await?
                .and_then(|w| w.ws.base_cs)),
        }
    }

    async fn root_of(&self, head: Option<ChangesetId>) -> Result<asc_core::id::TreeId> {
        Ok(match head {
            Some(c) => self.engine.get_changeset(c).await?.root,
            None => CliEngine::empty_tree_id(),
        })
    }

    /// `leafdiff(from, to)` in the shape `stage::rebase` takes.
    async fn leaf_deltas(&self, from: Option<ChangesetId>, to: Option<ChangesetId>) -> Result<Vec<LeafDelta>> {
        let (a, b) = (self.root_of(from).await?, self.root_of(to).await?);
        let mut out = Vec::new();
        for (path, change) in self.engine.leafdiff(a, b, None).await? {
            let to_leaf = match change {
                Change::Write { kind, file, size, git, .. } => Some(Leaf { kind, file, size, git }),
                Change::Gitlink { commit } => Some(Leaf {
                    kind: Kind::Gitlink,
                    file: FileId::from_bytes([0; 32]),
                    size: 0,
                    git: commit,
                }),
                Change::Delete => None,
            };
            let from_leaf = self.engine.lookup(a, &path).await?.map(|e| Leaf {
                kind: e.kind,
                file: FileId::from_bytes(e.id),
                size: e.size,
                git: e.git,
            });
            out.push(LeafDelta {
                path: path.as_bytes().to_vec(),
                from: from_leaf,
                to: to_leaf,
            });
        }
        Ok(out)
    }
}

/// The context of one invocation.
pub struct Ctx {
    pub prog: Program,
    pub env: GitEnv,
    pub cwd: PathBuf,
    pub config: ConfigSnapshot,
    ws: Option<WsCtx>,
    real_git: OnceLock<std::result::Result<RealGit, (String, u8)>>,
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_owned();
        if cfg!(windows) { s.to_ascii_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

/// Finds the workspace of `cwd` the way the shim does (SPEC §11.4): `GIT_DIR`, else the first
/// `.git` entry walking up, stopping below `GIT_CEILING_DIRECTORIES`. A directory `.git` or a
/// gitfile that is not an asc synthetic `.git` ends the walk: it is not an asc workspace.
pub async fn discover(env: &GitEnv, cwd: &Path) -> Result<Option<Projection>> {
    if let Some(git_dir) = &env.git_dir {
        let git_dir = if git_dir.is_absolute() { git_dir.clone() } else { cwd.join(git_dir) };
        let Ok(marker) = std::fs::read_to_string(git_dir.join("asc-workspace")) else {
            return Ok(None);
        };
        if let Some(tree) = &env.work_tree {
            let tree = if tree.is_absolute() { tree.clone() } else { cwd.join(tree) };
            return Ok(Projection::open(&tree).ok());
        }
        let home = AscHome::for_workspace(&git_dir)?;
        let db = StateDb::open(&home).await?;
        let id: WorkspaceId = marker
            .trim()
            .parse()
            .map_err(|_| CliError::Fatal("asc: malformed asc-workspace marker".into()))?;
        let row = workspaces::get(&db, id).await?;
        return Ok(match row {
            Some(w) => Projection::open(Path::new(&w.ws.path)).ok(),
            None => None,
        });
    }
    for dir in cwd.ancestors() {
        if dir != cwd && env.ceiling_dirs.iter().any(|c| same_path(c, dir)) {
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(dir.join(".git")) else {
            continue;
        };
        if !meta.is_file() {
            return Ok(None);
        }
        return Ok(Projection::open(dir).ok());
    }
    Ok(None)
}

impl Ctx {
    /// Builds the context for an invocation in `cwd`.
    pub async fn open(prog: Program, env: GitEnv, cwd: PathBuf) -> Result<Self> {
        let projection = discover(&env, &cwd).await?;
        let get = |k: &str| std::env::var_os(k);
        let local = projection.as_ref().map(|p| LocalConfig {
            common_dir: p.common_dir(),
            toplevel: p.toplevel(),
        });
        let config = ConfigSnapshot::load(&env, &get, local.as_ref())?;
        let ws = match projection {
            Some(p) => Some(WsCtx::open(p, &config).await?),
            None => None,
        };
        child::init(&env, None);
        Ok(Self {
            prog,
            env,
            cwd,
            config,
            ws,
            real_git: OnceLock::new(),
        })
    }

    /// A context with no workspace and an empty config (tests).
    pub fn bare(prog: Program, env: GitEnv, cwd: PathBuf) -> Self {
        Self {
            prog,
            env,
            cwd,
            config: ConfigSnapshot::empty(),
            ws: None,
            real_git: OnceLock::new(),
        }
    }

    /// The workspace, if the current directory is inside one.
    pub fn ws_opt(&self) -> Option<&WsCtx> {
        self.ws.as_ref()
    }

    /// The workspace, or git's "not a git repository" error.
    pub fn ws(&self) -> Result<&WsCtx> {
        self.ws.as_ref().ok_or_else(|| {
            CliError::Msg(Msg::NotARepository {
                cwd: self.cwd.display().to_string().replace('\\', "/"),
            })
        })
    }

    /// Real git, resolved once (SPEC §11.4). Resolving also gives children the Git for Windows
    /// `PATH` entries.
    pub fn real_git(&self) -> Result<&RealGit> {
        let slot = self.real_git.get_or_init(|| {
            let configured = self.config.path("asc.realGit");
            let home = self.ws.as_ref().map(|w| w.home.root());
            match RealGit::resolve(configured, home) {
                Ok(real) => {
                    child::init(&self.env, Some(&real));
                    Ok(real)
                }
                Err(e) => Err((e.to_string(), e.exit_code().clamp(0, 255) as u8)),
            }
        });
        slot.as_ref()
            .map_err(|(text, code)| CliError::Raw(text.clone(), *code))
    }

    /// A command for a hook, alias, editor, pager or external command (SPEC §9.4.1).
    pub fn child(&self, program: impl AsRef<std::ffi::OsStr>) -> Result<Command> {
        let _ = self.real_git(); // gives children the Git for Windows PATH when git is found
        child::command(program).map_err(CliError::Msg)
    }

    /// The mode of a new workspace: plain, until a factory offers virtual (U53a).
    pub fn workspace_mode(&self) -> asc_state::state::workspaces::Mode {
        ext::workspace_factory().plain()
    }

    /// The POSIX shell for `!` aliases and `sh -c` children.
    pub fn shell(&self) -> PathBuf {
        child::shell(self.real_git().ok())
    }

    /// Whether ids print as git SHA-1s, asc ids or both: git by default; `asc.displayIds` only
    /// for human output when invoked as `asc` (SPEC §11.1).
    pub fn display_ids(&self, kind: OutputKind) -> DisplayIds {
        if self.prog != Program::Asc || kind != OutputKind::Human {
            return DisplayIds::Git;
        }
        match self.config.get("asc.displayIds").as_deref() {
            Some("asc") => DisplayIds::Asc,
            Some("both") => DisplayIds::Both,
            _ => DisplayIds::Git,
        }
    }

    /// The single local ref-update path (SPEC §9.4.1): runs the `reference-transaction` hook,
    /// applies the CAS (which appends the ref log), and, when HEAD's commit changes, fixes
    /// stage set 0 per `staging` and tells the synthetic index. No command moves a local ref
    /// any other way.
    pub async fn update_local_refs(
        &self,
        mut txn: RefTxn,
        reflog_msg: &str,
        staging: StagingAfterMove,
    ) -> Result<()> {
        let ws = self.ws()?;
        let repo = ws.repo.key;
        let ws_id = ws.workspace.ws.id;
        txn.reason = reflog_msg.to_owned();
        let old_head = ws.head_commit().await?;
        let head_name = RefName::new_onelevel(b"HEAD".to_vec())
            .map_err(|e| CliError::Fatal(e.to_string()))?;
        let (head_updates, updates): (Vec<_>, Vec<_>) = std::mem::take(&mut txn.updates)
            .into_iter()
            .partition(|u| u.name == head_name);
        let mut changes = Vec::new();
        for u in &updates {
            let old = ws.refs.get(repo, &u.name).await?.map(|e| e.target);
            changes.push(RefChange {
                name: u.name.clone(),
                old,
                new: u.new,
            });
        }
        for u in &head_updates {
            changes.push(RefChange {
                name: u.name.clone(),
                old: old_head.map(RefTarget::Changeset),
                new: u.new,
            });
        }
        let view = RefTxnView {
            repo,
            workspace: ws_id,
            op: ext::OpId(txn.op_id),
            changes,
        };
        let hook = ext::ref_txn_hook();
        if let Err(rejected) = hook.prepared(&view).await {
            hook.aborted(&view).await;
            return Err(CliError::Fatal(format!("ref updates aborted by hook: {rejected}")));
        }
        let applied = async {
            if !updates.is_empty() {
                txn.updates = updates;
                ws.local.apply(repo, &txn).await?;
            }
            for u in &head_updates {
                self.move_detached_head(ws, u.expect, u.new, old_head).await?;
            }
            Ok::<(), CliError>(())
        }
        .await;
        if let Err(e) = applied {
            hook.aborted(&view).await;
            return Err(e);
        }
        hook.committed(&view).await;

        let new_head = ws.head_commit().await?;
        match staging {
            StagingAfterMove::Set(rows) => {
                stage::clear(&ws.db, ws_id, StageSet::Default).await?;
                stage::upsert(&ws.db, ws_id, StageSet::Default, &rows).await?;
            }
            StagingAfterMove::Keep if old_head != new_head => {
                let deltas = ws.leaf_deltas(old_head, new_head).await?;
                stage::rebase(&ws.db, ws_id, &deltas).await?;
            }
            StagingAfterMove::Keep => {}
        }
        if old_head != new_head {
            workspaces::update_head_base(&ws.db, ws_id, &ws.workspace.ws.head, new_head).await?;
            ext::index_sink()
                .on_head_moved(ws_id, old_head, new_head)
                .await
                .map_err(|e| CliError::Fatal(e.to_string()))?;
        }
        Ok(())
    }

    /// Moves a detached HEAD: the `HEAD` file holds the git SHA, the row the changeset.
    async fn move_detached_head(
        &self,
        ws: &WsCtx,
        expect: Expect,
        new: Option<RefTarget>,
        current: Option<ChangesetId>,
    ) -> Result<()> {
        let conflict = || CliError::Fatal("asc: HEAD changed concurrently".into());
        match (expect, current) {
            (Expect::Absent, Some(_)) => return Err(conflict()),
            (Expect::Value(RefTarget::Changeset(want)), cur) if cur != Some(want) => {
                return Err(conflict());
            }
            _ => {}
        }
        if !matches!(ws.projection.read_head()?, Head::Detached(_)) {
            return Err(CliError::Fatal(
                "asc: HEAD is a branch; update the branch instead".into(),
            ));
        }
        let Some(RefTarget::Changeset(cs)) = new else {
            return Err(CliError::Fatal("asc: HEAD cannot be deleted".into()));
        };
        let rows = asc_state::state::git_map::asc_to_git(&ws.db, ws.repo.key, cs.as_bytes()).await?;
        let oid: GitOid = rows
            .into_iter()
            .find(|(_, k)| *k == asc_core::git::ObjectKind::Commit)
            .map(|(o, _)| o)
            .ok_or_else(|| CliError::Fatal(format!("asc: no git commit for changeset {cs}")))?;
        ws.projection.write_head(&Head::Detached(oid))?;
        workspaces::update_head_base(&ws.db, ws.workspace.ws.id, "", Some(cs)).await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use asc_core::id::{ChangesetId, RefTarget};
    use asc_core::path::RefName;
    use asc_state::state::stage;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};

    use super::support::Fixture;
    use super::*;

    fn txn(update: RefUpdate) -> RefTxn {
        RefTxn {
            updates: vec![update],
            actor: Actor {
                id: "test".into(),
                via: "cli".into(),
            },
            reason: String::new(),
            op_id: uuid::Uuid::now_v7(),
        }
    }

    async fn ctx_of(f: &Fixture) -> Ctx {
        let env = GitEnv::from_vars(&|_| None);
        Ctx::open(Program::Asc, env, f.root.clone()).await.unwrap()
    }

    #[tokio::test]
    async fn keep_leaves_the_effective_staged_state_unchanged() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("f.txt", b"one"), ("g.txt", b"keep")]).await;
        let b = f.commit(&[a], &[("f.txt", b"two")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        let ctx = ctx_of(&f).await;

        // Move master from `a` to `b` (what `reset --soft HEAD~1` undoes): the effective index
        // must still equal `a`, so the commit's change shows as staged.
        let master = RefName::new(b"refs/heads/master".to_vec()).unwrap();
        ctx.update_local_refs(
            txn(RefUpdate {
                name: master.clone(),
                expect: Expect::Value(RefTarget::Changeset(a)),
                new: Some(RefTarget::Changeset(b)),
                expires_at: None,
            }),
            "test: move",
            StagingAfterMove::Keep,
        )
        .await
        .unwrap();

        let ws = ctx.ws().unwrap();
        assert_eq!(ws.head_commit().await.unwrap(), Some(b));
        let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default)
            .await
            .unwrap();
        let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, [b"f.txt".to_vec()], "only the moved path is staged");
        // The staged row holds `a`'s content, not `b`'s.
        let at_a = f.file_leaf(a, "f.txt").await;
        assert_eq!(rows[0].file, Some(at_a.file));
        assert_eq!(rows[0].git, Some(at_a.git));
    }

    #[tokio::test]
    async fn conflicting_expectation_fails_and_changes_nothing() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("f", b"1")]).await;
        let b = f.commit(&[a], &[("f", b"2")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        let ctx = ctx_of(&f).await;
        let master = RefName::new(b"refs/heads/master".to_vec()).unwrap();
        let err = ctx
            .update_local_refs(
                txn(RefUpdate {
                    name: master,
                    expect: Expect::Absent,
                    new: Some(RefTarget::Changeset(b)),
                    expires_at: None,
                }),
                "x",
                StagingAfterMove::Keep,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::Refs(RefError::Conflict(_))), "{err}");
        assert_eq!(ctx.ws().unwrap().head_commit().await.unwrap(), Some(a));
    }

    #[tokio::test]
    async fn set_replaces_the_rows() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("f", b"1")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        let ctx = ctx_of(&f).await;
        let row = StageRow::from_leaf(b"new.txt".to_vec(), Some(&f.file_leaf(a, "f").await));
        ctx.update_local_refs(
            txn(RefUpdate {
                name: RefName::new(b"refs/heads/other".to_vec()).unwrap(),
                expect: Expect::Absent,
                new: Some(RefTarget::Changeset(a)),
                expires_at: None,
            }),
            "branch",
            StagingAfterMove::Set(vec![row]),
        )
        .await
        .unwrap();
        let ws = ctx.ws().unwrap();
        let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, b"new.txt");
    }

    #[tokio::test]
    async fn engine_refs_are_read_only() {
        let f = Fixture::new().await;
        let ctx = ctx_of(&f).await;
        let ws = ctx.ws().unwrap();
        let t = txn(RefUpdate {
            name: RefName::new(b"refs/heads/x".to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(ChangesetId::from_bytes([9; 32]))),
            expires_at: None,
        });
        let err = Refs::update(ws.refs.as_ref(), ws.repo.key, &t).await.unwrap_err();
        assert!(crate::refs_guard::ReadOnly::is(&err));
    }
}
