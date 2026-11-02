//! Writable mounts (SPEC §10.6): `asc submodule edit <path>` turns the read-only composition of
//! a gitlink into a workspace of the child repository, detached at the pinned commit, at the same
//! path. Edits there are the child's; the parent's `status`, `commit` and `push` reach through it
//! ([`Mounts`]). `asc submodule release <path>` turns it back into the read-only composition once
//! nothing is left that the parent does not record. The server decides write access on the child
//! when the group is pushed; a mount the caller may not read never gets here (it is restricted).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_git::projection::{Head, Projection};
use asc_state::WorkspaceId;
use asc_state::state::git_map;
use asc_state::state::repos::Repo;
use asc_state::state::stage::Leaf;
use asc_state::state::workspaces::{self, Mode, NewWorkspace, Workspace};
use asc_wc::SafeRoot;
use asc_wc::compose_plain::{check_composed, clear};
use asc_wc::worktree as wt;

use super::super::add::cwd_prefix;
use super::super::add::spec::repo_path;
use super::super::add::target::{Edit, Target};
use super::super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::super::restore::{root_of, sync_worktree};
use super::super::worktree::{delete, is_dirty, leave, open_at, text_of};
use super::{fatal, nested_repo, pin_of, reproject, submodules};

const USAGE_EDIT: &str = "usage: asc submodule edit <path>";
const USAGE_RELEASE: &str = "usage: asc submodule release <path>";

/// One writable mount of the current workspace.
pub struct Mount {
    pub path: RepoPath,
    pub repo: Repo,
    pub row: Workspace,
}

impl Mount {
    /// The child workspace's root, where its commands run.
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.row.ws.mount)
    }

    /// The branch of the child a push moves: `asc.submodule.<path>.branch`, else the child's
    /// default branch.
    pub fn branch(&self, ctx: &Ctx) -> String {
        ctx.config
            .get(&format!("asc.submodule.{}.branch", self.path))
            .filter(|b| !b.trim().is_empty())
            .map_or_else(
                || self.repo.repo.default_branch.clone(),
                |b| b.trim().to_owned(),
            )
    }
}

fn child_abs(ws: &WsCtx, path: &RepoPath) -> PathBuf {
    asc_wc::plain::fs_path(ws.projection.toplevel(), path)
}

/// The child workspace at `abs`, when it is one of `repo`.
async fn row_at(ws: &WsCtx, abs: &Path, repo: &Repo) -> Result<Option<Workspace>> {
    let found = workspaces::by_path(&ws.db, text_of(abs)?).await?;
    Ok(found.filter(|r| r.ws.repo == repo.key))
}

/// Every writable mount among the gitlinks `paths` of the current workspace.
pub async fn mounts(ctx: &Ctx, ws: &WsCtx, paths: &[RepoPath]) -> Result<Vec<Mount>> {
    let subs = submodules(ctx, ws, paths);
    let mut out = Vec::new();
    for path in paths {
        let Some(name) = subs.repo_for(path) else {
            continue;
        };
        let Some(repo) = nested_repo(ws, name).await? else {
            continue;
        };
        let abs = child_abs(ws, path);
        if let Some(row) = row_at(ws, &abs, &repo).await? {
            out.push(Mount {
                path: path.clone(),
                repo,
                row,
            });
        }
    }
    Ok(out)
}

/// Every writable mount inside the current workspace: the workspaces of other repositories whose
/// path is a gitlink of this one.
pub async fn mounts_here(ctx: &Ctx, ws: &WsCtx) -> Result<Vec<Mount>> {
    let root = ws.projection.toplevel();
    let mut paths = Vec::new();
    for row in workspaces::list(&ws.db, None).await? {
        if row.ws.repo == ws.repo.key || !wt::is_inside(Path::new(&row.ws.path), root) {
            continue;
        }
        let Ok(rel) = Path::new(&row.ws.path).strip_prefix(root) else {
            continue;
        };
        let text = rel.to_string_lossy().replace('\\', "/");
        if let Ok(p) = RepoPath::new(text.into_bytes()) {
            paths.push(p);
        }
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let target = Target::open(ctx).await?;
    let mut links = Vec::new();
    for p in paths {
        if pin_of(ws, &target, &p).await.is_ok() {
            links.push(p);
        }
    }
    mounts(ctx, ws, &links).await
}

/// The commit at the child workspace's HEAD, as the git id a gitlink pins.
pub async fn head_pin(ctx: &Ctx, m: &Mount) -> Result<Option<GitOid>> {
    let child = open_at(ctx, m.root()).await?;
    let cws = child.ws()?;
    let Some(cs) = cws.head_commit().await? else {
        return Ok(None);
    };
    Ok(Some(super::super::switch::ensure_mapped(cws, cs).await?))
}

/// Whether the child at `dir` has something `asc commit` would record: a staged change, or with
/// `all` a modified or deleted tracked file. Untracked files alone are not a commit.
pub fn has_work(dir: &Path, all: bool) -> Result<bool> {
    let exe = std::env::current_exe()?;
    let out = std::process::Command::new(exe)
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(fatal(format!(
            "cannot read the status of the mount at '{}': {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout.split(|&b| b == b'\n').any(|line| {
        let (x, y) = match line {
            [x, y, ..] => (*x, *y),
            _ => return false,
        };
        if x == b'?' || x == b'!' {
            return false;
        }
        x != b' ' || (all && y != b' ')
    }))
}

/// Stages each mount's head as the pin of its gitlink, where they differ.
pub async fn stage_pins(ctx: &Ctx, ws: &WsCtx, mounts: &[Mount]) -> Result<()> {
    let mut target = Target::open(ctx).await?;
    let mut edits = Vec::new();
    for m in mounts {
        let Some(head) = head_pin(ctx, m).await? else {
            continue;
        };
        if pin_of(ws, &target, &m.path).await? == head {
            continue;
        }
        edits.push(Edit::Put {
            path: m.path.clone(),
            leaf: Leaf {
                kind: Kind::Gitlink,
                file: FileId::from_bytes([0; 32]),
                size: 0,
                git: head,
            },
            ita: false,
        });
    }
    if !edits.is_empty() {
        target.apply(ctx, edits).await?;
    }
    Ok(())
}

fn one_path(ctx: &Ctx, ws: &WsCtx, rest: &[OsString], usage: &str) -> Result<RepoPath> {
    let [arg] = rest else {
        return Err(CliError::Usage(usage.into()));
    };
    let bytes = repo_path(arg, &cwd_prefix(ctx, ws), ws.projection.toplevel())?;
    RepoPath::new(bytes).map_err(fatal)
}

pub(super) async fn edit(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if ws.workspace.ws.mode != Mode::Plain {
        return Err(fatal(
            "asc: writable mounts are made in plain workspaces; a virtual workspace composes submodules read-only",
        ));
    }
    let path = one_path(ctx, ws, rest, USAGE_EDIT)?;
    let target = Target::open(ctx).await?;
    let pin = pin_of(ws, &target, &path).await?;
    let subs = submodules(ctx, ws, std::slice::from_ref(&path));
    let Some(name) = subs.repo_for(&path) else {
        return Err(fatal(format!(
            "no repository is configured for '{path}'; set asc.submodule.{path}.repo"
        )));
    };
    let Some(repo) = nested_repo(ws, name).await? else {
        return Err(fatal(format!(
            "{name} is not on this machine; clone it first"
        )));
    };
    let Some(asc) = git_map::git_to_asc(&ws.db, repo.key, pin, ObjectKind::Commit)
        .await
        .map_err(fatal)?
    else {
        return Err(fatal(format!("{name} has no commit {pin}; fetch it first")));
    };
    let commit = ChangesetId::from_bytes(asc);
    let abs = child_abs(ws, &path);
    if row_at(ws, &abs, &repo).await?.is_some() {
        return Err(fatal(format!("'{path}' is already a writable mount")));
    }

    // Only our own read-only projection, or an empty placeholder, is replaced.
    let gate = SafeRoot::new(ws.projection.toplevel());
    if std::fs::symlink_metadata(&abs).is_ok() {
        if std::fs::read_dir(&abs)?.next().is_some() {
            check_composed(&gate, &path).map_err(fatal)?;
            clear(&gate, &path).map_err(fatal)?;
        }
        if abs.exists() {
            std::fs::remove_dir(&abs)?;
        }
    }

    let id = WorkspaceId::new();
    let mount = ws.home.worktree_mount(id);
    let made = materialise(ctx, ws, &repo, id, commit, pin, &abs, &mount).await;
    if let Err(e) = made {
        let _ = wt::remove_tree(&abs);
        let _ = wt::remove_tree(&mount);
        let _ = wt::remove_tree(&ws.home.workspace_dir(id));
        let _ = workspaces::remove(&ws.db, id).await;
        // Put the read-only composition back, so the parent looks as it did.
        reproject(ctx, ws, &subs, &path, &pin).await;
        return Err(e);
    }
    println!("{path}: writable mount of {name} at {pin}");
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
async fn materialise(
    ctx: &Ctx,
    ws: &WsCtx,
    repo: &Repo,
    id: WorkspaceId,
    commit: ChangesetId,
    pin: GitOid,
    abs: &Path,
    mount: &Path,
) -> Result<()> {
    std::fs::create_dir_all(mount)?;
    let projection = Projection::create(&ws.home, repo.repo.id, id, mount)?;
    projection.write_head(&Head::Detached(pin))?;
    workspaces::insert(
        &ws.db,
        &NewWorkspace {
            id,
            repo: repo.key,
            path: text_of(abs)?.to_owned(),
            mount: text_of(mount)?.to_owned(),
            mode: Mode::Plain,
            head: String::new(),
            base_cs: Some(commit),
            sparse: None,
            ephemeral_until: None,
        },
    )
    .await?;
    let fresh = open_at(ctx, mount.to_owned()).await?;
    let fws = fresh.ws()?;
    if fws.workspace.ws.id != id {
        return Err(fatal("asc: the mount did not open as itself"));
    }
    let root = root_of(fws, Some(commit)).await?;
    let paths: Vec<_> = fws
        .engine
        .leafdiff(CliEngine::empty_tree_id(), root, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    sync_worktree(&fresh, root, &paths, true).await?;
    wt::make_dir_link(abs, mount)
        .map_err(|e| fatal(format!("cannot link the mount into the work tree: {e}")))?;
    Ok(())
}

pub(super) async fn release(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let path = one_path(ctx, ws, rest, USAGE_RELEASE)?;
    let found = mounts(ctx, ws, std::slice::from_ref(&path)).await?;
    let Some(m) = found.into_iter().next() else {
        return Err(fatal(format!("'{path}' is not a writable mount")));
    };
    if is_dirty(&m.root())? {
        return Err(fatal(format!(
            "'{path}' has changes; commit them (asc commit) or discard them first"
        )));
    }
    let target = Target::open(ctx).await?;
    let pin = pin_of(ws, &target, &path).await?;
    if head_pin(ctx, &m).await? != Some(pin) {
        return Err(fatal(format!(
            "'{path}' has commits the parent does not pin; run asc commit first"
        )));
    }
    leave(&m.row, ws);
    delete(ws, &m.row).await?;
    std::fs::create_dir_all(child_abs(ws, &path))?;
    let subs = submodules(ctx, ws, std::slice::from_ref(&path));
    reproject(ctx, ws, &subs, &path, &pin).await;
    println!("{path}: read-only again at {pin}");
    Ok(ExitCode::SUCCESS)
}
