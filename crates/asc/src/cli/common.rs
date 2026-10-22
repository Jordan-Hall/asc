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
use asc_store::refs::{Expect, RefError, RefTxn, RefUpdate, Refs};
use asc_store::storeset::StoreSet;
use asc_store::tiered::Tiered;
use futures::stream::{BoxStream, StreamExt};

use super::config::{self, ConfigSnapshot, LocalConfig};
use super::safe_dir::{self, OwnerProbe, SafeDirectory, owned_by_current_user};
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
    /// Opens the state of the admitted workspace.
    pub async fn open(admitted: Admitted, config: &ConfigSnapshot) -> Result<Self> {
        // The home `admit` checked against, never one derived from the gitdir's path.
        let Admitted { projection, home } = admitted;
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

    /// Everything a ref move needs that can fail on untrusted input, computed before any state
    /// changes: the HEAD commit the move produces, the stage deltas from the commit the stage is
    /// relative to (`base_cs`) to it, and a detached target's git commit. A refusal here leaves
    /// the ref, `head_base` and the stage untouched.
    async fn plan_move(
        &self,
        updates: &[RefUpdate],
        head_updates: &[RefUpdate],
        old_head: Option<ChangesetId>,
        keep: bool,
    ) -> Result<MovePlan> {
        let row = workspaces::get(&self.db, self.workspace.ws.id)
            .await?
            .ok_or_else(|| CliError::Fatal("asc: this workspace is not registered in state.db".into()))?;
        let commit_of = |t: Option<RefTarget>| match t {
            Some(RefTarget::Changeset(c)) => Some(c),
            _ => None,
        };
        let (new_head, detached) = match self.projection.read_head()? {
            Head::Branch(b) => {
                if !head_updates.is_empty() {
                    return Err(CliError::Fatal("asc: HEAD is a branch; update the branch instead".into()));
                }
                let name = RefName::new(format!("refs/heads/{b}").into_bytes())
                    .map_err(|e| CliError::Fatal(format!("HEAD names a bad branch: {e}")))?;
                let moved = updates.iter().rev().find(|u| u.name == name);
                (moved.map_or(old_head, |u| commit_of(u.new)), None)
            }
            Head::Detached(_) => match head_updates {
                [] => (old_head, None),
                [u] => {
                    let conflict = || CliError::Fatal("asc: HEAD changed concurrently".into());
                    match (u.expect, old_head) {
                        (Expect::Absent, Some(_)) => return Err(conflict()),
                        (Expect::Value(RefTarget::Changeset(want)), cur) if cur != Some(want) => {
                            return Err(conflict());
                        }
                        _ => {}
                    }
                    let Some(RefTarget::Changeset(cs)) = u.new else {
                        return Err(CliError::Fatal("asc: HEAD cannot be deleted".into()));
                    };
                    let rows =
                        asc_state::state::git_map::asc_to_git(&self.db, self.repo.key, cs.as_bytes()).await?;
                    let oid: GitOid = rows
                        .into_iter()
                        .find(|(_, k)| *k == asc_core::git::ObjectKind::Commit)
                        .map(|(o, _)| o)
                        .ok_or_else(|| CliError::Fatal(format!("asc: no git commit for changeset {cs}")))?;
                    (Some(cs), Some(oid))
                }
                _ => return Err(CliError::Fatal("asc: one transaction may update HEAD only once".into())),
            },
        };
        let base = row.ws.base_cs;
        let deltas = if keep && base != new_head {
            self.leaf_deltas(base, new_head).await?
        } else {
            Vec::new()
        };
        // A detached move records no branch name; every other move keeps the recorded one.
        let head = if detached.is_some() { String::new() } else { row.ws.head };
        Ok(MovePlan { base, new_head, head, deltas, detached })
    }
}

/// What [`WsCtx::plan_move`] decided; see there.
struct MovePlan {
    /// `workspaces.base_cs` when planned: the commit the stage rows are relative to.
    base: Option<ChangesetId>,
    /// The commit HEAD resolves to once the move is applied.
    new_head: Option<ChangesetId>,
    /// The `workspaces.head` to record with `new_head`.
    head: String,
    /// `leafdiff(base, new_head)` for `stage::rebase` (empty unless staging is kept).
    deltas: Vec<LeafDelta>,
    /// The git commit to write into a detached `HEAD` file after the state commits.
    detached: Option<GitOid>,
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

/// What opening a workspace must pass (SPEC §11.4, "Trusted workspaces").
pub struct Gate<'a> {
    /// `safe.directory` from system, global and command-line config.
    pub safe: &'a SafeDirectory,
    /// The user's asc home, from their environment: never derived from a gitdir on disk.
    pub home: &'a dyn Fn() -> Result<AscHome>,
    /// Whether the current user owns a path.
    pub owned: OwnerProbe<'a>,
}

/// The canonical path, without the verbatim prefix Windows adds to a drive path (hook scripts
/// and config readers get these paths, and not every one of them takes the verbatim form).
fn canonical(path: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    if cfg!(windows) {
        let text = real.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            let drive = rest.as_bytes();
            if drive.len() >= 2 && drive[0].is_ascii_alphabetic() && drive[1] == b':' {
                return Some(PathBuf::from(rest));
            }
        }
    }
    Some(real)
}

/// A workspace that passed [`admit`]: its projection holds the canonical gitdir and common dir
/// that were checked, and `home` is the user's home they were checked against.
#[derive(Debug)]
pub struct Admitted {
    pub projection: Projection,
    pub home: AscHome,
}

/// Admits a discovered workspace or treats it as "not an asc repository" (`Ok(None)`), in the
/// order that keeps non-asc directories out of the ownership error: (1) the gitdir is exactly
/// `<home>/wc/<id>/git` and the common dir is inside `<home>/repos`, after canonicalisation;
/// (2) the root, gitfile, gitdir and common dir are owned by the current user or listed in
/// `safe.directory`, else git's "dubious ownership" error (exit 128); (3) the workspace id
/// is registered in the home's `state.db` and the common dir is its repository's.
async fn admit(p: Projection, gate: &Gate<'_>) -> Result<Option<Admitted>> {
    let home = (gate.home)()?;
    let (Some(git_dir), Some(common), Some(home_root)) = (
        canonical(p.git_dir()),
        canonical(p.common_dir()),
        canonical(home.root()),
    ) else {
        return Ok(None);
    };
    if canonical(&home.gitdir(p.workspace())).as_deref() != Some(git_dir.as_path())
        || !common.starts_with(home_root.join("repos"))
    {
        return Ok(None);
    }
    let gitfile = p.toplevel().join(".git");
    safe_dir::ensure_owned(
        p.toplevel(),
        &[p.toplevel(), &gitfile, p.git_dir(), p.common_dir()],
        gate.safe,
        gate.owned,
    )
    .map_err(|text| CliError::Raw(text, 128))?;
    let db = StateDb::open(&home).await?;
    let Some(row) = workspaces::get(&db, p.workspace()).await? else {
        return Ok(None);
    };
    let Some(repo) = repos::get(&db, row.ws.repo).await? else {
        return Ok(None);
    };
    if canonical(&home.common_dir(repo.repo.id)).as_deref() != Some(common.as_path()) {
        return Ok(None);
    }
    Ok(Some(Admitted {
        projection: p.with_dirs(git_dir, common),
        home,
    }))
}

/// Finds the workspace of `cwd` the way the shim does (SPEC §11.4): `GIT_DIR`, else the first
/// `.git` entry walking up, stopping below `GIT_CEILING_DIRECTORIES`. A directory `.git` or a
/// gitfile that is not an asc synthetic `.git` ends the walk: it is not an asc workspace. A
/// candidate must then pass [`admit`]; one that does not is not an asc workspace either.
pub async fn discover(env: &GitEnv, cwd: &Path, gate: &Gate<'_>) -> Result<Option<Admitted>> {
    let candidate = if let Some(git_dir) = &env.git_dir {
        let git_dir = if git_dir.is_absolute() { git_dir.clone() } else { cwd.join(git_dir) };
        let Ok(marker) = std::fs::read_to_string(git_dir.join("asc-workspace")) else {
            return Ok(None);
        };
        if let Some(tree) = &env.work_tree {
            let tree = if tree.is_absolute() { tree.clone() } else { cwd.join(tree) };
            Projection::open(&tree).ok()
        } else {
            let id: WorkspaceId = marker
                .trim()
                .parse()
                .map_err(|_| CliError::Fatal("asc: malformed asc-workspace marker".into()))?;
            // The user's home answers; the gitdir that named the workspace does not.
            let db = StateDb::open(&(gate.home)()?).await?;
            match workspaces::get(&db, id).await? {
                Some(w) => Projection::open(Path::new(&w.ws.path)).ok(),
                None => None,
            }
        }
    } else {
        let mut found = None;
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
            found = Projection::open(dir).ok();
            break;
        }
        found
    };
    match candidate {
        Some(p) => admit(p, gate).await,
        None => Ok(None),
    }
}

impl Ctx {
    /// Builds the context for an invocation in `cwd`.
    pub async fn open(prog: Program, env: GitEnv, cwd: PathBuf) -> Result<Self> {
        let get = |k: &str| std::env::var_os(k);
        let safe = config::safe_directories(&env, &get)?;
        let home = || AscHome::locate().map_err(CliError::from);
        let gate = Gate {
            safe: &safe,
            home: &home,
            owned: &owned_by_current_user,
        };
        Self::open_with(prog, env, cwd, &gate).await
    }

    /// [`Ctx::open`] with the user's home, `safe.directory` list and ownership probe supplied
    /// (tests; every other caller goes through [`Ctx::open`]).
    pub async fn open_with(prog: Program, env: GitEnv, cwd: PathBuf, gate: &Gate<'_>) -> Result<Self> {
        let admitted = discover(&env, &cwd, gate).await?;
        let get = |k: &str| std::env::var_os(k);
        let local = admitted.as_ref().map(|a| LocalConfig {
            common_dir: a.projection.common_dir(),
            toplevel: a.projection.toplevel(),
        });
        let config = ConfigSnapshot::load_with(&env, &get, local.as_ref(), gate.owned)?;
        let ws = match admitted {
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
    ///
    /// Crash and error safety: everything that can fail on untrusted input (the leaf diff of an
    /// over-long path or an over-large delta, a missing object, a bad detached target) runs
    /// before the `prepared` hook and before any state changes. The ref move, the stage change
    /// and the new head base then commit in ONE `state.db` transaction, so a failure or a crash
    /// leaves the ref, `head_base` and the stage all on the old head or all on the new one. The
    /// hook's `committed` event and the synthetic index follow the commit; the detached `HEAD`
    /// file (a projection of `base_cs`) is written last.
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
        let keep = matches!(staging, StagingAfterMove::Keep);
        let plan = ws.plan_move(&updates, &head_updates, old_head, keep).await?;
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
            let mut tx = ws.db.begin().await?;
            // The stage rows are relative to `base_cs`; a concurrent writer moved it since the
            // plan, so the planned deltas no longer fit. Roll back (dropping `tx`) and refuse.
            if workspaces::base_cs_tx(&mut tx, ws_id).await? != plan.base {
                return Err(CliError::Fatal("asc: HEAD changed concurrently".into()));
            }
            if !updates.is_empty() {
                txn.updates = updates;
                ws.local.apply_in_tx(&mut tx, repo, &txn).await?;
            }
            match staging {
                StagingAfterMove::Set(rows) => {
                    stage::replace_tx(&mut tx, ws_id, StageSet::Default, &rows).await?;
                }
                StagingAfterMove::Keep => stage::rebase_tx(&mut tx, ws_id, &plan.deltas).await?,
            }
            if plan.base != plan.new_head {
                workspaces::update_head_base_tx(&mut tx, ws_id, &plan.head, plan.new_head).await?;
            }
            tx.commit().await.map_err(asc_state::Error::from)?;
            Ok::<(), CliError>(())
        }
        .await;
        if let Err(e) = applied {
            hook.aborted(&view).await;
            return Err(e);
        }
        hook.committed(&view).await;

        // The state is committed; the projection and the index follow it. A failure here is
        // reported but leaves the ref, `head_base` and the stage consistent.
        if let Some(oid) = plan.detached {
            ws.projection.write_head(&Head::Detached(oid))?;
        }
        if old_head != plan.new_head {
            ext::index_sink()
                .on_head_moved(ws_id, old_head, plan.new_head)
                .await
                .map_err(|e| CliError::Fatal(e.to_string()))?;
        }
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
        let safe = SafeDirectory::default();
        let home = || Ok(f.home.clone());
        let gate = Gate { safe: &safe, home: &home, owned: &owned_by_current_user };
        Ctx::open_with(Program::Asc, env, f.root.clone(), &gate).await.unwrap()
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

    /// A changeset a hostile remote could push: its root tree holds an entry name of
    /// `MAX_WALK_PATH_BYTES + 1` bytes. `put_tree` refuses it, so it is stored raw, as a fetch
    /// would. Its `changes` are empty, so nothing validated them.
    async fn hostile_long_name_changeset(f: &Fixture, parent: ChangesetId) -> ChangesetId {
        use asc_core::codec::Enveloped;
        use asc_core::object::{Changeset, Entry, SortedMap, Tree};
        use asc_core::path::Name;
        use asc_store::key::ObjKey;

        let long = "n".repeat(asc_engine::tree::MAX_WALK_PATH_BYTES + 1);
        let entry = Entry {
            name: Name::new(long).unwrap(),
            kind: Kind::File,
            id: [1; 32],
            size: 1,
            git: asc_core::git::blob_oid(b""),
        };
        let tree = Tree::new(vec![entry], true).unwrap();
        let tree_env = tree.to_envelope();
        let root = asc_core::id::TreeId::of(&tree_env);
        f.objects.put(&ObjKey::Tree(root), encode_stored(&tree_env)).await.unwrap();
        let sig = asc_core::sig::Signature::new(b"T".to_vec(), b"t@example.com".to_vec(), 1_700_000_000, 0).unwrap();
        let cs = Changeset {
            parents: vec![parent],
            root,
            author: sig.clone(),
            committer: sig,
            message: b"hostile\n".to_vec(),
            extra: SortedMap::new(Vec::new()).unwrap(),
            changes: SortedMap::new(Vec::new()).unwrap(),
        };
        let env = cs.to_envelope();
        let id = ChangesetId::of(&env);
        f.objects.put(&ObjKey::Changeset(id), encode_stored(&env)).await.unwrap();
        id
    }

    /// The state a ref move must keep consistent: the branch, `base_cs`, HEAD and the stage rows.
    async fn snapshot(ctx: &Ctx) -> (Option<RefTarget>, Option<ChangesetId>, Option<ChangesetId>, Vec<StageRow>) {
        let ws = ctx.ws().unwrap();
        let master = RefName::new(b"refs/heads/master".to_vec()).unwrap();
        let target = ws.refs.get(ws.repo.key, &master).await.unwrap().map(|e| e.target);
        let base = workspaces::get(&ws.db, ws.workspace.ws.id).await.unwrap().unwrap().ws.base_cs;
        let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await.unwrap();
        (target, base, ws.head_commit().await.unwrap(), rows)
    }

    fn move_master(from: ChangesetId, to: ChangesetId) -> RefTxn {
        txn(RefUpdate {
            name: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
            expect: Expect::Value(RefTarget::Changeset(from)),
            new: Some(RefTarget::Changeset(to)),
            expires_at: None,
        })
    }

    #[tokio::test]
    async fn a_head_whose_delta_cannot_be_computed_is_refused_before_the_ref_moves() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("f.txt", b"one"), ("g.txt", b"keep")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        let ctx = ctx_of(&f).await;
        // A staged difference that a successful rebase would have to carry across the move.
        let ws = ctx.ws().unwrap();
        let edited = StageRow::from_leaf(b"f.txt".to_vec(), Some(&f.file_leaf(a, "f.txt").await));
        stage::replace(&ws.db, ws.workspace.ws.id, StageSet::Default, &[StageRow { size: 99, ..edited }])
            .await
            .unwrap();
        let before = snapshot(&ctx).await;

        let hostile = hostile_long_name_changeset(&f, a).await;
        // The same failing sink, reached through a target whose objects are absent.
        let missing = ChangesetId::from_bytes([9; 32]);
        for target in [hostile, missing] {
            let err = ctx
                .update_local_refs(move_master(a, target), "pull", StagingAfterMove::Keep)
                .await
                .unwrap_err();
            assert!(!matches!(err, CliError::Refs(RefError::Conflict(_))), "{err}");
            assert_eq!(snapshot(&ctx).await, before, "ref, head_base and stage stay on the old head: {err}");
        }
    }

    #[tokio::test]
    async fn a_stage_that_cannot_be_written_rolls_the_ref_move_back() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("f", b"1")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        let ctx = ctx_of(&f).await;
        let ws = ctx.ws().unwrap();
        let old = StageRow::from_leaf(b"old.txt".to_vec(), Some(&f.file_leaf(a, "f").await));
        stage::replace(&ws.db, ws.workspace.ws.id, StageSet::Default, &[old]).await.unwrap();
        let before = snapshot(&ctx).await;

        // Stage 4 does not exist: storing the row fails after the old rows were deleted.
        let bad = StageRow { stage: 4, ..StageRow::from_leaf(b"new.txt".to_vec(), Some(&f.file_leaf(a, "f").await)) };
        let b = f.commit(&[a], &[("f", b"2")]).await;
        let err = ctx
            .update_local_refs(move_master(a, b), "commit", StagingAfterMove::Set(vec![bad]))
            .await
            .unwrap_err();
        assert!(matches!(err, CliError::State(_)), "{err}");
        assert_eq!(snapshot(&ctx).await, before, "the old rows, ref and base all survive");
    }

    #[tokio::test]
    async fn a_successful_move_commits_ref_base_and_stage_together() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("f", b"1")]).await;
        let b = f.commit(&[a], &[("f", b"2")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        let ctx = ctx_of(&f).await;
        ctx.update_local_refs(move_master(a, b), "ff", StagingAfterMove::Keep).await.unwrap();
        let (target, base, head, rows) = snapshot(&ctx).await;
        assert_eq!(target, Some(RefTarget::Changeset(b)));
        assert_eq!((base, head), (Some(b), Some(b)));
        assert_eq!(rows.len(), 1, "the move shows as a staged difference");
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

/// The workspace gate (SPEC §11.4, "Trusted workspaces"): ownership, `safe.directory`, and
/// registration in the user's asc home.
#[cfg(test)]
mod gate_tests {
    use std::ffi::OsString;

    use asc_state::{RepoId, WorkspaceId};

    use super::support::Fixture;
    use super::*;
    use crate::env::ConfigParam;

    fn is_dubious(r: &Result<Option<Admitted>>) -> bool {
        matches!(r, Err(CliError::Raw(t, 128))
            if t.starts_with("fatal: detected dubious ownership in repository at '")
                && t.contains("git config --global --add safe.directory "))
    }

    /// Environment reading only `global` (no system config).
    fn hermetic(global: &Path) -> impl Fn(&str) -> Option<OsString> {
        let global = global.to_owned();
        move |k| match k {
            "GIT_CONFIG_NOSYSTEM" => Some("1".into()),
            "GIT_CONFIG_GLOBAL" => Some(global.clone().into()),
            _ => None,
        }
    }

    async fn discover_in(
        f: &Fixture,
        cwd: &Path,
        safe: &SafeDirectory,
        owned: OwnerProbe<'_>,
    ) -> Result<Option<Admitted>> {
        let home = || Ok(f.home.clone());
        let gate = Gate { safe, home: &home, owned };
        discover(&GitEnv::from_vars(&|_| None), cwd, &gate).await
    }

    const YES: &dyn Fn(&Path) -> std::io::Result<bool> = &|_| Ok(true);
    const NO: &dyn Fn(&Path) -> std::io::Result<bool> = &|_| Ok(false);

    #[tokio::test]
    async fn a_normal_workspace_is_admitted_from_its_root_and_a_subdirectory() {
        let f = Fixture::new().await;
        let none = SafeDirectory::default();
        let sub = f.root.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        for dir in [&f.root, &sub] {
            let p = discover_in(&f, dir, &none, &owned_by_current_user).await.unwrap();
            assert_eq!(p.expect("workspace").projection.workspace(), f.ws);
        }
    }

    #[tokio::test]
    async fn a_planted_gitfile_pointing_outside_asc_home_is_refused() {
        let f = Fixture::new().await;
        let other = AscHome::open(f.sandbox.dir("attacker-home")).unwrap();
        let planted = f.sandbox.dir("planted");
        let (repo, ws) = (RepoId::new(), WorkspaceId::new());
        let proj = Projection::create(&other, repo, ws, &planted).unwrap();
        std::fs::write(
            proj.common_dir().join("config"),
            b"[asc]\n\trealGit = /tmp/evil\n[alias]\n\tx = !evil\n[safe]\n\tdirectory = *\n",
        )
        .unwrap();
        let none = SafeDirectory::default();
        // Even when every path is "ours" and the list says `*`.
        for safe in [none.clone(), SafeDirectory::new(["*".to_owned()])] {
            let got = discover_in(&f, &planted, &safe, YES).await.unwrap();
            assert!(got.is_none(), "a workspace outside ASC_HOME is not an asc repository");
        }
        // The context never reads the planted config.
        let home = || Ok(f.home.clone());
        let gate = Gate { safe: &none, home: &home, owned: YES };
        let env = GitEnv::from_vars(&|k| (k == "GIT_CONFIG_NOSYSTEM").then(|| "1".into()));
        let ctx = Ctx::open_with(Program::Asc, env, planted, &gate).await.unwrap();
        assert!(ctx.ws_opt().is_none());
        assert_eq!(ctx.config.get("asc.realGit"), None);
        assert_eq!(ctx.config.get("alias.x"), None);
    }

    /// Makes `link` a directory link to `target`; `false` when the platform refuses.
    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[tokio::test]
    async fn a_linked_gitdir_never_moves_the_home_to_the_links_lexical_grandparent() {
        let f = Fixture::new().await;
        let real = Projection::open(&f.root).unwrap();
        let (real_git, real_common) = (real.git_dir().to_owned(), real.common_dir().to_owned());
        // `<evil>/wc/<id>/git` and `<evil>/repos` both lead into the user's home.
        let evil = f.sandbox.dir("evil");
        let wc = evil.join("wc").join(f.ws.to_string());
        std::fs::create_dir_all(&wc).unwrap();
        let linked = link_dir(&wc.join("git"), &real_git) && link_dir(&evil.join("repos"), &f.home.root().join("repos"));
        if !linked {
            eprintln!("skipped: cannot create directory links here");
            return;
        }
        std::fs::write(f.root.join(".git"), format!("gitdir: {}\n", wc.join("git").display())).unwrap();
        let lexical = Projection::open(&f.root).unwrap();
        assert!(lexical.git_dir().starts_with(&evil), "the projection is lexical before admission");

        let got = discover_in(&f, &f.root, &SafeDirectory::default(), YES).await.unwrap().expect("admitted");
        assert_eq!(got.home, f.home, "the gate's home, not the link's grandparent");
        assert_eq!(got.projection.git_dir(), canonical(&real_git).unwrap());
        assert_eq!(got.projection.common_dir(), canonical(&real_common).unwrap());

        // The whole context opens the user's state, and `evil` gets no state.db.
        let home = || Ok(f.home.clone());
        let none = SafeDirectory::default();
        let gate = Gate { safe: &none, home: &home, owned: YES };
        let env = GitEnv::from_vars(&|k| (k == "GIT_CONFIG_NOSYSTEM").then(|| "1".into()));
        let ctx = Ctx::open_with(Program::Asc, env, f.root.clone(), &gate).await.unwrap();
        assert_eq!(ctx.ws().unwrap().home, f.home);
        assert_eq!(ctx.ws().unwrap().projection.common_dir(), canonical(&real_common).unwrap());
        assert!(!evil.join("state.db").exists(), "no state was created under the link's parent");
    }

    #[tokio::test]
    async fn a_gitdir_not_registered_in_state_db_is_refused() {
        let f = Fixture::new().await;
        let dir = f.sandbox.dir("unregistered");
        // Inside the user's home, but no `workspaces` row.
        Projection::create(&f.home, RepoId::new(), WorkspaceId::new(), &dir).unwrap();
        let got = discover_in(&f, &dir, &SafeDirectory::default(), YES).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn a_workspace_row_with_another_repositorys_common_dir_is_refused() {
        let f = Fixture::new().await;
        let proj = Projection::open(&f.root).unwrap();
        // Repoint the workspace at a common dir of a repository the row does not name.
        let stray = f.home.common_dir(RepoId::new());
        std::fs::create_dir_all(&stray).unwrap();
        let rel = format!("../../../repos/{}/.git\n", stray.parent().unwrap().file_name().unwrap().to_string_lossy());
        std::fs::write(proj.git_dir().join("commondir"), rel).unwrap();
        let got = discover_in(&f, &f.root, &SafeDirectory::default(), YES).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn another_owner_is_refused_with_gits_message_unless_safe_directory_allows() {
        let f = Fixture::new().await;
        let none = SafeDirectory::default();
        let got = discover_in(&f, &f.root, &none, NO).await;
        assert!(is_dubious(&got), "{got:?}");
        // The gitdir alone being foreign is enough.
        let git_dir = Projection::open(&f.root).unwrap().git_dir().to_owned();
        let only_gitdir = |p: &Path| Ok(!p.starts_with(std::fs::canonicalize(&git_dir).unwrap()));
        assert!(is_dubious(&discover_in(&f, &f.root, &none, &only_gitdir).await));

        let star = SafeDirectory::new(["*".to_owned()]);
        assert!(discover_in(&f, &f.root, &star, NO).await.unwrap().is_some());
        let exact = SafeDirectory::new([safe_dir::display_path(&std::fs::canonicalize(&f.root).unwrap())]);
        assert!(discover_in(&f, &f.root, &exact, NO).await.unwrap().is_some());
        let parent = f.root.parent().unwrap();
        let prefix = SafeDirectory::new([format!("{}/*", safe_dir::display_path(&std::fs::canonicalize(parent).unwrap()))]);
        assert!(discover_in(&f, &f.root, &prefix, NO).await.unwrap().is_some());
        let elsewhere = SafeDirectory::new(["/somewhere/else".to_owned()]);
        assert!(is_dubious(&discover_in(&f, &f.root, &elsewhere, NO).await));
    }

    #[tokio::test]
    async fn safe_directory_comes_from_global_and_command_line_never_the_repo() {
        let f = Fixture::new().await;
        let proj = Projection::open(&f.root).unwrap();
        // The repository vouches for itself, and so does a worktree-level file: ignored.
        std::fs::write(proj.common_dir().join("config"), b"[safe]\n\tdirectory = *\n").unwrap();
        std::fs::write(proj.git_dir().join("config.worktree"), b"[safe]\n\tdirectory = *\n").unwrap();
        let global = f.sandbox.dir("g").join("gitconfig");
        std::fs::write(&global, b"").unwrap();
        let env = GitEnv::from_vars(&|_| None);
        let safe = config::safe_directories(&env, &hermetic(&global)).unwrap();
        assert!(is_dubious(&discover_in(&f, &f.root, &safe, NO).await), "repo-local safe.directory is ignored");

        std::fs::write(&global, b"[safe]\n\tdirectory = *\n").unwrap();
        let safe = config::safe_directories(&env, &hermetic(&global)).unwrap();
        assert!(discover_in(&f, &f.root, &safe, NO).await.unwrap().is_some(), "global `*` works");

        std::fs::write(&global, b"").unwrap();
        let mut env = GitEnv::from_vars(&|_| None);
        env.cli_config.push(ConfigParam::parse("safe.directory=*"));
        let safe = config::safe_directories(&env, &hermetic(&global)).unwrap();
        assert!(discover_in(&f, &f.root, &safe, NO).await.unwrap().is_some(), "-c works");
    }
}
