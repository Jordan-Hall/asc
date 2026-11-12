//! [`NfsProvider`]: serves [`MergedFs`] over loopback NFSv3 and mounts it with the system's NFS
//! client (SPEC §10.4). macOS ships that client in the kernel, so no extension is installed.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

use asc_core::path::RepoPath;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::DiskState;

use super::server::{EXPORT, Evict, Files, RTMAX, Server, ServerConfig};
use crate::core::{CoreFs, DehydrateOutcome, Provider, Result, SwitchOutcome, VfsError};
use crate::unixfs::{self, Disk, MergedFs};

/// The most paths a switch refreshes in the client's cache one by one; past that the client
/// catches up through its attribute timeout.
const MAX_POKES: usize = 10_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The file beside the overlay's directories that holds the handle tag.
const TAG_FILE: &str = "nfs-handle-tag";

/// The secret that ties handles to a work tree: 8 random bytes in `dir/nfs-handle-tag`, readable
/// by the owner alone and kept across restarts, so handles a client holds stay valid and nobody
/// else can make one.
pub(crate) fn handle_tag(dir: &Path) -> io::Result<u64> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let p = dir.join(TAG_FILE);
    let nofollow = rustix::fs::OFlags::NOFOLLOW.bits() as i32;
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow)
        .open(&p)
    {
        Ok(mut f) => {
            let m = f.metadata()?;
            let mut b = [0u8; 8];
            if m.is_file() && m.mode() & 0o077 == 0 && f.read_exact(&mut b).is_ok() {
                let t = u64::from_be_bytes(b);
                if t != 0 {
                    return Ok(t);
                }
            }
            // Short, shared or not a file: replaced by a new secret.
            fs::remove_file(&p)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut b = [0u8; 8];
    while u64::from_be_bytes(b) == 0 {
        fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(&p)?;
    f.write_all(&b)?;
    f.sync_all()?;
    Ok(u64::from_be_bytes(b))
}

/// The system's NFS mount command for a server on `port`.
pub(crate) fn mount_command(macos: bool, port: u16, root: &Path) -> (&'static str, Vec<OsString>) {
    let source = format!("127.0.0.1:{EXPORT}");
    if macos {
        let opts = format!(
            "vers=3,tcp,port={port},mountport={port},soft,timeo=600,retrans=6,locallocks,\
             nonegnamecache,rsize={RTMAX},wsize={RTMAX},readahead=16,actimeo=1,nobrowse"
        );
        (
            "/sbin/mount_nfs",
            vec!["-o".into(), opts.into(), source.into(), root.into()],
        )
    } else {
        let opts = format!(
            "vers=3,proto=tcp,port={port},mountport={port},mountproto=tcp,soft,timeo=600,\
             retrans=6,nolock,lookupcache=positive,rsize={RTMAX},wsize={RTMAX},actimeo=1"
        );
        (
            "mount",
            vec![
                "-t".into(),
                "nfs".into(),
                "-o".into(),
                opts.into(),
                source.into(),
                root.into(),
            ],
        )
    }
}

/// The unmount commands to try, in order.
pub(crate) fn unmount_commands(macos: bool, root: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    let (umount, force) = if macos {
        ("/sbin/umount", "-f")
    } else {
        ("umount", "-l")
    };
    vec![
        (umount, vec![root.into()]),
        (umount, vec![force.into(), root.into()]),
    ]
}

/// Runs `prog args`, then `sudo -n prog args` (which never prompts). Answers the last error.
fn run_privileged(prog: &str, args: &[OsString]) -> std::result::Result<(), String> {
    let first = Command::new(prog).args(args).output();
    match &first {
        Ok(o) if o.status.success() => return Ok(()),
        _ => {}
    }
    let second = Command::new("sudo").arg("-n").arg(prog).args(args).output();
    match second {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let mut msg = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            if let Ok(f) = &first {
                let f = String::from_utf8_lossy(&f.stderr).trim().to_owned();
                if !f.is_empty() {
                    msg = f;
                }
            }
            Err(msg)
        }
        Err(e) => Err(e.to_string()),
    }
}

struct Mounted {
    server: Server,
    root: Option<PathBuf>,
}

/// The NFS [`Provider`].
pub struct NfsProvider {
    fs: Arc<MergedFs>,
    files: Arc<Mutex<Files>>,
    require_resvport: bool,
    /// MOUNT answers at any time (tests, whose client mounts by itself).
    mounts_open: bool,
    state: Mutex<Option<Mounted>>,
    disk: Disk,
}

impl NfsProvider {
    /// Opens the overlay and the inode table; nothing is served yet.
    pub fn new(cfg: unixfs::Config) -> io::Result<Self> {
        let fs = Arc::new(MergedFs::new(cfg)?);
        let files = Arc::new(Mutex::new(Files::default()));
        fs.set_notify(Arc::new(Evict(files.clone())));
        Ok(Self {
            disk: Disk::new(fs.clone()),
            fs,
            files,
            require_resvport: true,
            mounts_open: false,
            state: Mutex::new(None),
        })
    }

    /// Accepts connections from unprivileged ports and MOUNT at any time: for tests, whose
    /// client is not the kernel.
    #[cfg(test)]
    pub(crate) fn allow_any_port(mut self) -> Self {
        self.require_resvport = false;
        self.mounts_open = true;
        self
    }

    /// The file system core, for tests and statistics.
    pub fn core(&self) -> &Arc<CoreFs> {
        self.fs.core()
    }

    /// Bytes of regular files the overlay holds.
    pub fn overlay_bytes(&self) -> io::Result<u64> {
        self.fs.overlay_bytes()
    }

    /// The paths the user deleted.
    pub fn whiteouts(&self) -> io::Result<Vec<RepoPath>> {
        self.fs.whiteouts()
    }

    /// The merged file system, for conversion back to a plain work tree.
    pub fn merged(&self) -> &Arc<MergedFs> {
        &self.fs
    }

    /// Starts the server without mounting it (`mount` does both); answers its address.
    pub fn serve(&self) -> io::Result<SocketAddr> {
        let mut st = lock(&self.state);
        if let Some(m) = st.as_ref() {
            return Ok(m.server.addr());
        }
        let server = Server::start(
            self.fs.clone(),
            self.files.clone(),
            ServerConfig {
                owner: self.fs.uid,
                require_resvport: self.require_resvport,
                tag: handle_tag(self.fs.overlay_dir())?,
                mounts_open: self.mounts_open,
                workers: std::thread::available_parallelism()
                    .map_or(4, std::num::NonZero::get)
                    .clamp(4, 16),
            },
        )?;
        let addr = server.addr();
        *st = Some(Mounted { server, root: None });
        Ok(addr)
    }

    /// Opens or shuts MOUNT for good on the running server.
    #[cfg(test)]
    pub(crate) fn set_mounts_open(&self, open: bool) {
        if let Some(m) = lock(&self.state).as_ref() {
            m.server.set_mounts_open(open);
        }
    }

    /// Lets one MOUNT through, as `mount` does around the mount command.
    #[cfg(test)]
    pub(crate) fn allow_one_mount(&self) {
        if let Some(m) = lock(&self.state).as_ref() {
            m.server.allow_one_mount(true);
        }
    }

    /// Lookups held for handles handed to clients.
    pub fn held(&self) -> usize {
        lock(&self.state).as_ref().map_or(0, |m| m.server.held())
    }

    /// Refreshes the client's cached attributes of `paths` and their directories: a same-mode
    /// `chmod` through the mount makes the client fetch them now, not after its timeout.
    fn poke(root: &Path, paths: &[RepoPath]) {
        let mut seen: BTreeSet<RepoPath> = BTreeSet::new();
        for p in paths {
            let mut cur = Some(p.clone());
            while let Some(q) = cur {
                if seen.len() >= MAX_POKES || !seen.insert(q.clone()) {
                    break;
                }
                cur = q.parent();
            }
        }
        let mut targets: Vec<PathBuf> = seen
            .iter()
            .map(|p| root.join(OsStr::from_bytes(p.as_bytes())))
            .collect();
        targets.push(root.to_path_buf());
        for t in targets {
            if let Ok(m) = fs::symlink_metadata(&t)
                && !m.file_type().is_symlink()
            {
                let _ = fs::set_permissions(&t, fs::Permissions::from_mode(m.mode() & 0o7777));
            }
        }
    }
}

impl Provider for NfsProvider {
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
        let addr = self.serve()?;
        let (prog, args) = mount_command(cfg!(target_os = "macos"), addr.port(), root);
        // One MOUNT answers, while the system's client mounts.
        let window = |open: bool| {
            if let Some(m) = lock(&self.state).as_ref() {
                m.server.allow_one_mount(open);
            }
        };
        window(true);
        let ran = run_privileged(prog, &args);
        window(false);
        if let Err(msg) = ran {
            // Someone else may have taken the root handle: drop the server and the secret in
            // every handle it gave out, so the next mount starts with a new one.
            *lock(&self.state) = None;
            let _ = fs::remove_file(self.fs.overlay_dir().join(TAG_FILE));
            return Err(VfsError::Io(io::Error::other(format!(
                "could not mount the NFS work tree at {}: {msg} (see WORKTREES.md, \
                 \"macOS: the NFS backend\")",
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
            for (prog, args) in unmount_commands(cfg!(target_os = "macos"), root) {
                match run_privileged(prog, &args) {
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
        let root = {
            let st = lock(&self.state);
            st.as_ref().map(|m| {
                m.server.bump_generation();
                m.root.clone()
            })
        };
        if let Some(Some(root)) = root {
            Self::poke(&root, &changed);
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

impl Drop for NfsProvider {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_commands_pin_the_port_and_cache_options() {
        let (prog, args) = mount_command(true, 4321, Path::new("/w"));
        assert_eq!(prog, "/sbin/mount_nfs");
        let opts = args[1].to_str().unwrap();
        for o in [
            "vers=3",
            "tcp",
            "port=4321",
            "mountport=4321",
            "soft",
            "nonegnamecache",
            "locallocks",
            "actimeo=1",
        ] {
            assert!(opts.split(',').any(|x| x == o), "{o} in {opts}");
        }
        assert_eq!(args[2], "127.0.0.1:/asc");
        assert_eq!(args[3], "/w");
        let (prog, args) = mount_command(false, 9, Path::new("/w"));
        assert_eq!(prog, "mount");
        assert!(args[3].to_str().unwrap().contains("mountport=9"));
        let un = unmount_commands(true, Path::new("/w"));
        assert_eq!(un[1].1[0], "-f");
    }

    #[test]
    fn handle_tags_are_owner_only_secrets_kept_across_restarts() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let t = handle_tag(a.path()).unwrap();
        assert_eq!(handle_tag(a.path()).unwrap(), t, "stable across restarts");
        assert_ne!(
            handle_tag(b.path()).unwrap(),
            t,
            "each work tree has its own"
        );
        let p = a.path().join(TAG_FILE);
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        // A tag others can read is no secret: it is replaced.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let fresh = handle_tag(a.path()).unwrap();
        assert_ne!(fresh, t);
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        // A link in its place is not followed.
        fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(b.path().join(TAG_FILE), &p).unwrap();
        assert!(handle_tag(a.path()).is_err());
    }
}
