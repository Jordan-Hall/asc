//! [`FskitProvider`]: serves [`MergedFs`] to the asc FSKit module over the work tree's socket and
//! mounts it with `mount -F -t ascfs` (SPEC §10.3b).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

use asc_core::path::RepoPath;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::DiskState;

use super::server::{Evict, Files, Server};
use super::{FS_TYPE, MAX_SOCKET_PATH, socket_path, volume_url};
use crate::core::{CoreFs, DehydrateOutcome, Provider, Result, SwitchOutcome, VfsError};
use crate::nfs::NfsProvider;
use crate::nfs::provider::{secret, watched};
use crate::unixfs::{self, Disk, MergedFs};

/// The file beside the overlay's directories that holds the work tree's FSKit id.
const ID_FILE: &str = "fskit-id";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The mount command for the volume `url` at `root`.
pub(crate) fn mount_command(url: &str, root: &Path) -> (&'static str, Vec<OsString>) {
    (
        "/sbin/mount",
        vec![
            "-F".into(),
            "-t".into(),
            FS_TYPE.into(),
            url.into(),
            root.into(),
        ],
    )
}

/// The unmount commands to try, in order.
pub(crate) fn unmount_commands(root: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    vec![
        ("/sbin/umount", vec![root.into()]),
        ("/sbin/umount", vec!["-f".into(), root.into()]),
    ]
}

/// Runs `prog args` as the calling user: an FSKit volume belongs to the user who mounts it.
fn run(prog: &str, args: &[OsString]) -> std::result::Result<(), String> {
    match Command::new(prog).args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            Err(if msg.is_empty() {
                format!("{prog} exited with {}", o.status)
            } else {
                msg
            })
        }
        Err(e) => Err(format!("{prog}: {e}")),
    }
}

struct Mounted {
    server: Server,
    root: Option<PathBuf>,
}

/// The FSKit [`Provider`].
pub struct FskitProvider {
    fs: Arc<MergedFs>,
    files: Arc<Mutex<Files>>,
    group: PathBuf,
    id: u64,
    state: Mutex<Option<Mounted>>,
    disk: Disk,
}

impl FskitProvider {
    /// Opens the overlay and the inode table; nothing is served yet. `group` is the app group
    /// directory the module reads sockets from ([`super::group_dir`]).
    pub fn new(cfg: unixfs::Config, group: PathBuf) -> io::Result<Self> {
        let fs = Arc::new(MergedFs::new(cfg)?);
        let id = secret(fs.overlay_dir(), ID_FILE)?;
        let sock = socket_path(&group, id);
        if sock.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the FSKit socket path {} is longer than {MAX_SOCKET_PATH} bytes; set \
                     ASC_FSKIT_GROUP_DIR to a shorter app group directory",
                    sock.display()
                ),
            ));
        }
        let files = Arc::new(Mutex::new(Files::default()));
        fs.set_notify(Arc::new(Evict(files.clone())));
        Ok(Self {
            disk: Disk::new(fs.clone()),
            fs,
            files,
            group,
            id,
            state: Mutex::new(None),
        })
    }

    /// The file system core, for tests and statistics.
    pub fn core(&self) -> &Arc<CoreFs> {
        self.fs.core()
    }

    /// The merged file system, for conversion back to a plain work tree.
    pub fn merged(&self) -> &Arc<MergedFs> {
        &self.fs
    }

    /// The socket this work tree is served at.
    pub fn socket(&self) -> PathBuf {
        socket_path(&self.group, self.id)
    }

    /// The URL `mount` passes to the module.
    pub fn url(&self) -> String {
        volume_url(self.id)
    }

    /// Starts the server without mounting it (`mount` does both); answers the socket.
    pub fn serve(&self) -> io::Result<PathBuf> {
        let mut st = lock(&self.state);
        if let Some(m) = st.as_ref() {
            return Ok(m.server.path().to_path_buf());
        }
        let server = Server::start(
            self.fs.clone(),
            self.files.clone(),
            &self.socket(),
            std::thread::available_parallelism()
                .map_or(4, std::num::NonZero::get)
                .clamp(4, 16),
        )?;
        let path = server.path().to_path_buf();
        *st = Some(Mounted { server, root: None });
        Ok(path)
    }

    /// Lookups the module holds.
    pub fn held(&self) -> u64 {
        lock(&self.state).as_ref().map_or(0, |m| m.server.held())
    }

    /// Files the server keeps open.
    pub fn open_files(&self) -> usize {
        lock(&self.files).open_count()
    }
}

impl unixfs::UnixProvider for FskitProvider {
    fn merged(&self) -> &Arc<MergedFs> {
        &self.fs
    }
    fn backend(&self) -> &'static str {
        "fskit"
    }
    fn switch_note(&self) -> Option<String> {
        let root = lock(&self.state).as_ref()?.root.clone()?;
        watched(&root).then(|| unixfs::WATCH_NOTE.to_owned())
    }
}

impl Provider for FskitProvider {
    fn mount(&self, root: &Path) -> Result<()> {
        if lock(&self.state).as_ref().is_some_and(|m| m.root.is_some()) {
            return Err(VfsError::AlreadyMounted);
        }
        fs::create_dir_all(root)?;
        let meta = fs::symlink_metadata(root)?;
        if !meta.is_dir() || asc_wc::safe_path::is_link(&meta) {
            return Err(VfsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the mount point must be a real directory",
            )));
        }
        self.serve()?;
        let (prog, args) = mount_command(&self.url(), root);
        if let Err(msg) = run(prog, &args) {
            *lock(&self.state) = None;
            return Err(VfsError::Io(io::Error::other(format!(
                "could not mount the FSKit work tree at {}: {msg} (see WORKTREES.md, \
                 \"macOS: the FSKit backend\")",
                root.display()
            ))));
        }
        if let Some(m) = lock(&self.state).as_mut() {
            m.root = Some(root.to_path_buf());
        }
        Ok(())
    }

    fn unmount(&self) -> Result<()> {
        let Some(m) = lock(&self.state).take() else {
            return Err(VfsError::NotMounted);
        };
        let mut result = Ok(());
        if let Some(root) = &m.root {
            let mut last = String::new();
            let mut done = false;
            for (prog, args) in unmount_commands(root) {
                match run(prog, &args) {
                    Ok(()) => {
                        done = true;
                        break;
                    }
                    Err(e) => last = e,
                }
            }
            if !done {
                result = Err(VfsError::Io(io::Error::other(format!(
                    "could not unmount {}: {last}",
                    root.display()
                ))));
            }
        }
        drop(m);
        self.fs.drop_handles();
        lock(&self.files).clear();
        result
    }

    fn apply_switch(&self, plan: &Plan, _times: Switched) -> Result<SwitchOutcome> {
        let (out, changed) = self.fs.apply_switch(plan)?;
        let root = lock(&self.state).as_ref().and_then(|m| m.root.clone());
        if let Some(root) = root {
            // The kernel caches item attributes; touching each changed path through the mount
            // makes it ask again.
            NfsProvider::poke(&root, &changed);
        }
        Ok(out)
    }

    fn dehydrate(&self, paths: &[RepoPath]) -> Result<DehydrateOutcome> {
        self.fs.dehydrate(paths)
    }

    fn disk_state(&self) -> &dyn DiskState {
        &self.disk
    }
}

impl Drop for FskitProvider {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_and_unmount_commands_name_the_module_and_the_volume() {
        let (prog, args) = mount_command("ascfs://00000000000000ff", Path::new("/w"));
        assert_eq!(prog, "/sbin/mount");
        assert_eq!(
            args,
            ["-F", "-t", "ascfs", "ascfs://00000000000000ff", "/w"]
        );
        let un = unmount_commands(Path::new("/w"));
        assert_eq!(un[0].1, ["/w"]);
        assert_eq!(un[1].1, ["-f", "/w"]);
    }
}
