//! The synthetic index sink (SPEC §9.4): keeps `<gitdir>/index` equal to HEAD's tree overlaid
//! with stage set 0, patching only the paths a commit, head move or staging change touched.
//! Registered in `main`; every command that moves HEAD or set 0 reaches it through
//! [`ext::index_sink`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use asc_core::git::blob_oid;
use asc_core::id::ChangesetId;
use asc_core::object::{Entry, Kind};
use asc_core::path::{Name, RepoPath, checkout_safe, windows_valid};
use asc_git::index::{FileStat, IndexFile, PlainStat, StatSource, StatTime};
use asc_git::projection::Projection;
use asc_git::projection::index_export::{self, Basis, ExportOptions, TurboIndex};
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces::{self, Mode};
use asc_state::{AscHome, WorkspaceId};
use asc_wc::SafeRoot;
use asc_wc::plain::Sparse;
use async_trait::async_trait;

use crate::cli::common::{Admitted, CliEngine, WsCtx};
use crate::cli::config::{ConfigSnapshot, LocalConfig};
use crate::cli::safe_dir::owned_by_current_user;
use crate::env::GitEnv;
use crate::ext::{SinkResult, SyntheticIndexSink};

/// Implements [`SyntheticIndexSink`] over the user's asc home.
#[derive(Clone, Debug, Default)]
pub struct IndexSink {
    /// Overrides `AscHome::locate` (tests).
    home: Option<AscHome>,
}

impl IndexSink {
    /// A sink over `home` instead of the located one.
    pub fn with_home(home: AscHome) -> Self {
        Self { home: Some(home) }
    }

    async fn open(&self, ws: WorkspaceId) -> SinkResult<Option<(WsCtx, TurboIndex)>> {
        let home = match &self.home {
            Some(h) => h.clone(),
            None => AscHome::locate()?,
        };
        let db = asc_state::StateDb::open(&home).await?;
        let Some(row) = workspaces::get(&db, ws).await? else {
            return Ok(None);
        };
        let projection = Projection::open(Path::new(&row.ws.path))?;
        let env = GitEnv::from_process();
        let get = |k: &str| std::env::var_os(k);
        let local = LocalConfig {
            common_dir: projection.common_dir(),
            toplevel: projection.toplevel(),
        };
        let config = ConfigSnapshot::load_with(&env, &get, Some(&local), &owned_by_current_user)?;
        let admitted = Admitted { projection, home };
        let turbo = TurboIndex::parse(config.get("asc.turboIndex").as_deref());
        Ok(Some((WsCtx::open(admitted, &config).await?, turbo)))
    }
}

/// Times for a virtual workspace: the workspace's clone-time epoch (SPEC §10.1) for every path,
/// sizes from tree metadata.
struct VirtualStat {
    time: StatTime,
}

impl StatSource for VirtualStat {
    fn stat(&self, _: &RepoPath, entry: &Entry) -> FileStat {
        FileStat {
            ctime: self.time,
            mtime: self.time,
            size: entry.size as u32,
            ..FileStat::default()
        }
    }
}

/// The largest file a head move hashes to prove that it equals the entry it is exported as.
const VERIFY_LIMIT: u64 = 32 << 20;

/// Real stat for the paths a command just staged, whose files equal the staged content, and for
/// every other changed entry whose file hashes to the entry's oid (a checkout just wrote it);
/// stat-dirty (zero times, tree size) for any other, since the file may differ from the tree's
/// content and a recorded stat would make a reader trust the wrong oid. Entries that did not
/// change keep the stat they had (SPEC §9.4; `export::Inherit`).
struct Trusted {
    inner: PlainStat,
    safe: SafeRoot,
    paths: HashSet<Vec<u8>>,
}

impl Trusted {
    /// The stat of the regular file at `path` when its bytes hash to `entry.git`, taken before
    /// and after the read so a concurrent write is not recorded; stat-dirty otherwise.
    fn verified(&self, path: &RepoPath, entry: &Entry) -> FileStat {
        let dirty = FileStat {
            size: entry.size as u32,
            ..FileStat::default()
        };
        if !matches!(entry.kind, Kind::File | Kind::Exec) || entry.size > VERIFY_LIMIT {
            return dirty;
        }
        let Some(file) = self.safe.resolve(path) else {
            return dirty;
        };
        let before = self.inner.stat(path, entry);
        let regular =
            std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file() && m.len() == entry.size);
        let matches =
            regular && std::fs::read(&file).is_ok_and(|bytes| blob_oid(&bytes) == entry.git);
        if matches && self.inner.stat(path, entry) == before {
            before
        } else {
            dirty
        }
    }
}

impl StatSource for Trusted {
    fn stat(&self, path: &RepoPath, entry: &Entry) -> FileStat {
        if self.paths.contains(path.as_bytes()) {
            self.inner.stat(path, entry)
        } else {
            self.verified(path, entry)
        }
    }
}

/// Whether a path cannot exist in the work tree here (SPEC §2.8): git marks it skip-worktree.
fn unrepresentable(path: &RepoPath) -> bool {
    !checkout_safe(path)
        || (cfg!(windows)
            && path
                .components()
                .any(|c| Name::new(c).is_ok_and(|n| !windows_valid(&n))))
}

/// Re-renders or patches the export for `ws`. `head` is the commit HEAD now names when the
/// caller knows it; otherwise it is resolved from HEAD.
async fn sync(
    ws: &WsCtx,
    turbo: TurboIndex,
    head: Option<Option<ChangesetId>>,
    paths: &[RepoPath],
) -> SinkResult {
    let gitdir: PathBuf = ws.home.gitdir(ws.workspace.ws.id);
    let head = match head {
        Some(h) => h,
        None => ws.head_commit().await?,
    };
    let root = match head {
        Some(c) => ws.engine.get_changeset(c).await?.root,
        None => CliEngine::empty_tree_id(),
    };
    let plain = ws.workspace.ws.mode == Mode::Plain;
    let has_turbo = ws
        .engine
        .lookup(root, &RepoPath::new("turbo.json")?)
        .await?
        .is_some();
    let index_path = gitdir.join("index");
    if !turbo.enabled(has_turbo, plain) {
        for name in ["index", index_export::BASE_FILE] {
            let _ = std::fs::remove_file(gitdir.join(name));
        }
        return Ok(());
    }

    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let (base_root, staged) = match index_export::read_base(&gitdir) {
        Some((root, staged)) => (Some(root), staged),
        None => (None, Vec::new()),
    };
    let basis = IndexFile::read(&index_path).ok().map(|index| Basis {
        index,
        root: base_root,
        staged,
    });
    let sparse = ws.workspace.ws.sparse.as_deref().map(Sparse::parse);
    let skip = |p: &RepoPath| sparse.as_ref().is_some_and(|s| !s.contains(p)) || unrepresentable(p);
    let opts = ExportOptions {
        skip_worktree: Some(&skip),
        ..ExportOptions::default()
    };
    let index = if plain {
        let stat = Trusted {
            inner: PlainStat {
                root: ws.projection.toplevel().to_owned(),
            },
            safe: SafeRoot::new(ws.projection.toplevel().to_owned()),
            paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
        };
        index_export::update(&ws.engine, basis, root, paths, &rows, &stat, &opts).await?
    } else {
        let secs = u32::try_from(ws.workspace.created).unwrap_or(0);
        let stat = VirtualStat {
            time: StatTime { secs, nsecs: 0 },
        };
        index_export::update(&ws.engine, basis, root, paths, &rows, &stat, &opts).await?
    };
    index_export::store(&gitdir, &index, root, &rows)?;
    Ok(())
}

#[async_trait]
impl SyntheticIndexSink for IndexSink {
    async fn on_commit(&self, ws: WorkspaceId, head: ChangesetId) -> SinkResult {
        let Some((w, turbo)) = self.open(ws).await? else {
            return Ok(());
        };
        sync(&w, turbo, Some(Some(head)), &[]).await
    }

    async fn on_head_moved(
        &self,
        ws: WorkspaceId,
        _old: Option<ChangesetId>,
        new: Option<ChangesetId>,
    ) -> SinkResult {
        let Some((w, turbo)) = self.open(ws).await? else {
            return Ok(());
        };
        sync(&w, turbo, Some(new), &[]).await
    }

    async fn on_stage_changed(&self, ws: WorkspaceId, paths: &[RepoPath]) -> SinkResult {
        let Some((w, turbo)) = self.open(ws).await? else {
            return Ok(());
        };
        sync(&w, turbo, None, paths).await
    }
}

#[cfg(test)]
mod tests;
