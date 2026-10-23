//! `ext::WorkspaceFactory` for the daemon era (U53a; SPEC §9.1).
//!
//! * Plain workspaces are U18's (`asc_wc::plain`); nothing here changes them.
//! * Virtual workspaces are offered only once an OS backend (ProjFS or FUSE, behind the
//!   `vfs-projfs` and `vfs-fuse` features) has registered itself with
//!   [`register_virtual_backend`] and the daemon is not disabled by `ASC_NO_DAEMON`.
//! * [`convert`] is `worktree convert --plain`: unmount, materialise in place through U18, set
//!   `workspaces.mode = plain` and leave an index with real stat, so `status` reads the same
//!   before and after. It never needs the daemon: a daemon that is down has nothing mounted.
//!
//! Conversion keeps every file that holds local edits (a hydrated file whose content differs
//! from HEAD) and rewrites everything else: absent files and unhydrated placeholders (which
//! cannot be read once the provider is gone) are written from the object store. A file that
//! cannot be reached without crossing a symlink is outside the work tree and is not touched.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use asc_core::git::BlobOidHasher;
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_state::state::workspaces::{self, Mode, Workspace};
use asc_state::{AscHome, WorkspaceId};
use asc_wc::SafeRoot;

use super::lifecycle::{self, Probe};
use crate::cli::common::{CliEngine, CliError, Ctx, Gate};
use crate::cli::restore::{root_of, sync_worktree};
use crate::cli::config;
use crate::cli::safe_dir::owned_by_current_user;
use crate::dispatch::Program;
use crate::env::GitEnv;
use crate::ext::{SinkError, SinkResult, WorkspaceFactory};

/// Paths rewritten per `sync_worktree` call; bounds the trees built at once.
const BATCH: usize = 8192;

static VIRTUAL_BACKEND: AtomicBool = AtomicBool::new(false);

/// An OS backend (U56 ProjFS, U57 FUSE) calls this once it can host a virtual workspace.
pub fn register_virtual_backend() {
    VIRTUAL_BACKEND.store(true, Ordering::SeqCst);
}

/// What conversion needs from the daemon.
#[async_trait]
pub trait Mounts: Send + Sync {
    /// Whether new workspaces may be virtual.
    fn virtual_available(&self) -> bool;
    /// Stops serving `ws`. A workspace the daemon does not host is not an error.
    async fn unmount(&self, ws: WorkspaceId) -> SinkResult;
}

/// The mounts of the running daemon, reached over IPC.
pub struct DaemonMounts;

#[async_trait]
impl Mounts for DaemonMounts {
    fn virtual_available(&self) -> bool {
        VIRTUAL_BACKEND.load(Ordering::SeqCst) && !lifecycle::disabled(&|k| std::env::var_os(k))
    }

    async fn unmount(&self, ws: WorkspaceId) -> SinkResult {
        if lifecycle::disabled(&|k| std::env::var_os(k)) {
            return Ok(());
        }
        let cfg = lifecycle::Config::for_home(&AscHome::locate()?)?;
        match lifecycle::probe(&cfg).await {
            // No daemon, nothing mounted.
            Probe::Down => Ok(()),
            Probe::Hung => Err("the daemon does not answer; run 'asc daemon stop' and retry".into()),
            Probe::Up(_) => {
                let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
                let ask = wire::UnmountRequest { ws_id: ws.to_string() };
                match tokio::time::timeout(cfg.stop_wait, client.unmount(ask)).await {
                    Err(_) => Err("the daemon did not unmount the workspace in time".into()),
                    Ok(Ok(_)) => Ok(()),
                    // The daemon serves no mounts (yet): there is nothing to stop.
                    Ok(Err(s)) if s.code() == tonic::Code::Unimplemented => Ok(()),
                    Ok(Err(s)) => Err(format!("unmount: {}", s.message()).into()),
                }
            }
        }
    }
}

/// The factory registered in `main`.
pub struct DaemonWorkspaceFactory {
    mounts: Arc<dyn Mounts>,
}

impl DaemonWorkspaceFactory {
    pub fn new(mounts: Arc<dyn Mounts>) -> Self {
        Self { mounts }
    }

    /// The factory over the user's daemon.
    pub fn system() -> Self {
        Self::new(Arc::new(DaemonMounts))
    }
}

#[async_trait]
impl WorkspaceFactory for DaemonWorkspaceFactory {
    fn virtual_mode(&self) -> Option<Mode> {
        self.mounts.virtual_available().then_some(Mode::Virtual)
    }

    async fn convert_plain(&self, ws: &Workspace) -> SinkResult {
        if ws.ws.mode == Mode::Plain {
            return Ok(());
        }
        // The command layer's futures are not `Send`, so the conversion runs to completion on a
        // blocking thread of this runtime.
        let (mounts, ws, runtime) = (self.mounts.clone(), ws.clone(), tokio::runtime::Handle::current());
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let get = |k: &str| std::env::var_os(k);
                let env = GitEnv::from_process();
                let safe = config::safe_directories(&env, &get).map_err(text)?;
                let home = AscHome::locate()?;
                let locate = || Ok(home.clone());
                let gate = Gate {
                    safe: &safe,
                    home: &locate,
                    owned: &owned_by_current_user,
                };
                convert(mounts.as_ref(), &ws, &home, env, &gate).await
            })
        })
        .await?
    }
}

/// `convert_plain` with the home and the trust gate supplied.
pub(crate) async fn convert(
    mounts: &dyn Mounts,
    ws: &Workspace,
    home: &AscHome,
    env: GitEnv,
    gate: &Gate<'_>,
) -> SinkResult {
    mounts.unmount(ws.ws.id).await?;
    let root = PathBuf::from(&ws.ws.mount);
    restore_gitfile(&root, &home.gitdir(ws.ws.id))?;
    // The gate checks ownership, `safe.directory` and the registration before anything is read.
    let ctx = Ctx::open_with(Program::Asc, env, root, gate).await.map_err(text)?;
    let opened = ctx.ws().map_err(text)?;
    if opened.workspace.ws.id != ws.ws.id {
        return Err("the directory belongs to another workspace".into());
    }
    materialise(&ctx).await.map_err(text)?;
    workspaces::set_mode(&opened.db, ws.ws.id, Mode::Plain).await.map_err(|e| e.to_string())?;
    Ok(())
}

fn text(e: CliError) -> SinkError {
    e.to_string().into()
}

/// Writes the real `.git` gitfile over whatever the provider served there.
fn restore_gitfile(root: &Path, gitdir: &Path) -> SinkResult {
    if !gitdir.join("asc-workspace").is_file() {
        return Err(format!("{} is not a workspace git directory", gitdir.display()).into());
    }
    let gitfile = root.join(".git");
    match fs::symlink_metadata(&gitfile) {
        Ok(m) if m.is_dir() => return Err(format!("{} is a directory", gitfile.display()).into()),
        Ok(_) => fs::remove_file(&gitfile)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut out = fs::OpenOptions::new().write(true).create_new(true).open(&gitfile)?;
    std::io::Write::write_all(&mut out, format!("gitdir: {}\n", gitdir.display()).as_bytes())?;
    Ok(())
}

/// Whether the file at `path` holds content other than the tracked blob.
fn modified(gate: &SafeRoot, path: &RepoPath, size: u64, git: &asc_core::id::GitOid) -> bool {
    let Some(abs) = gate.resolve(path) else { return false };
    let Ok(meta) = fs::symlink_metadata(&abs) else { return false };
    if !meta.is_file() {
        return false;
    }
    if meta.len() != size {
        // A readable file of another size is an edit; an unreadable one is an unhydrated
        // placeholder.
        return fs::File::open(&abs).is_ok();
    }
    let Ok(mut file) = fs::File::open(&abs) else { return false };
    let mut hasher = BlobOidHasher::new(size);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(_) => return false,
        }
    }
    !hasher.finalize().is_ok_and(|oid| oid == *git)
}

/// Writes HEAD's tree into the work tree where the work tree does not already hold edits, with
/// real stat in the index.
async fn materialise(ctx: &Ctx) -> Result<(), CliError> {
    let ws = ctx.ws()?;
    let head = ws.head_commit().await?;
    let tree = root_of(ws, head).await?;
    let all = ws.engine.leafdiff(CliEngine::empty_tree_id(), tree, None).await?;
    let gate = SafeRoot::new(ws.projection.toplevel());
    let mut write: Vec<RepoPath> = Vec::new();
    for (path, change) in &all {
        if gate.resolve(path).is_none() {
            continue; // Behind a link: outside the work tree.
        }
        if let Change::Write { kind: Kind::File | Kind::Exec, size, git, .. } = change
            && modified(&gate, path, *size, git)
        {
            continue; // A local edit stays; its index entry stays stat-dirty.
        }
        write.push(path.clone());
    }
    for batch in write.chunks(BATCH) {
        sync_worktree(ctx, tree, batch, true).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use asc_git::index::IndexFile;

    use super::*;
    use crate::cli::safe_dir::SafeDirectory;

    use super::support::Fixture;

    #[derive(Default)]
    struct Recording(Mutex<Vec<WorkspaceId>>);

    #[async_trait]
    impl Mounts for Recording {
        fn virtual_available(&self) -> bool {
            true
        }
        async fn unmount(&self, ws: WorkspaceId) -> SinkResult {
            self.0.lock().unwrap().push(ws);
            Ok(())
        }
    }

    #[test]
    fn virtual_mode_follows_the_backend() {
        let f = DaemonWorkspaceFactory::new(Arc::new(Recording::default()));
        assert_eq!(f.virtual_mode(), Some(Mode::Virtual));
        assert_eq!(f.plain(), Mode::Plain);
    }

    #[tokio::test]
    async fn converting_a_virtual_workspace_materialises_it_and_keeps_edits() {
        let f = Fixture::new().await;
        let a = f.commit(&[], &[("a.txt", b"alpha"), ("keep.txt", b"original"), ("d/c.txt", b"deep")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        // A virtual workspace as the provider leaves it after unmount: no placeholders readable,
        // one hydrated file with a local edit, no gitfile.
        workspaces::set_mode(&f.db, f.ws, Mode::Virtual).await.unwrap();
        fs::write(f.root.join("keep.txt"), b"edited by the user").unwrap();
        fs::remove_file(f.root.join(".git")).unwrap();

        let ws = workspaces::get(&f.db, f.ws).await.unwrap().unwrap();
        let mounts = Recording::default();
        let safe = SafeDirectory::default();
        let home = || Ok(f.home.clone());
        let gate = Gate { safe: &safe, home: &home, owned: &owned_by_current_user };
        convert(&mounts, &ws, &f.home, GitEnv::from_vars(&|_| None), &gate).await.unwrap();

        assert_eq!(*mounts.0.lock().unwrap(), [f.ws], "the workspace is unmounted first");
        let ws = workspaces::get(&f.db, f.ws).await.unwrap().unwrap();
        assert_eq!(ws.ws.mode, Mode::Plain);
        assert_eq!(fs::read(f.root.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(f.root.join("d").join("c.txt")).unwrap(), b"deep");
        assert_eq!(fs::read(f.root.join("keep.txt")).unwrap(), b"edited by the user");

        // The index carries real stat for what was written, so `status` has nothing to hash.
        let index = IndexFile::read_or_empty(&f.home.gitdir(f.ws).join("index")).unwrap();
        let entry = |p: &str| index.entries.iter().find(|e| e.path.as_bytes() == p.as_bytes()).unwrap();
        assert_eq!(entry("a.txt").size, 5);
        assert_ne!(entry("a.txt").mtime, Default::default());
        assert_eq!(entry("d/c.txt").size, 4);

        // Converting again is a no-op for a plain workspace.
        DaemonWorkspaceFactory::new(Arc::new(Recording::default())).convert_plain(&ws).await.unwrap();
    }
}
