//! The listener's host keys (SPEC §25.11.6).
//!
//! `[ssh] host_key_files` lists OpenSSH-format private keys, unencrypted as `sshd` reads them.
//! Creation, loading and the checks are the M30 key-file helpers (`authz::key_file`): a file is
//! created with `CREATE_NEW` and mode 0600 (Windows: the service account and SYSTEM only, a
//! protected DACL, in the same call), opened without following a link, and checked on the
//! opened handle. A file that is a link, is group- or world-accessible, or is not owned by the
//! service user is refused. A missing *first* file is generated once as an ed25519 key; a lost
//! creation race re-opens the winner's file.
//!
//! A private key is never logged and has no `Debug` or `Display` output: [`HostKeys`] prints
//! fingerprints only, and [`HostKeyError`] carries a path and a reason, never key text.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use asc_rpc::auth::SshFingerprint;
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{Algorithm, PrivateKey};

use crate::authz::key_file;
use crate::config::SshConfig;

/// Largest host key file read, in bytes. A 16,384-bit RSA key is under 14 KiB.
pub const MAX_HOST_KEY_FILE: u64 = 64 * 1024;
/// How long a lost creation race waits for the winner to finish writing.
const RACE_WAIT: Duration = Duration::from_secs(5);
const RACE_POLL: Duration = Duration::from_millis(25);
/// The comment of a generated key.
const COMMENT: &str = "asc-serve host key";

/// Why a host key could not be loaded or made. Never holds key text.
#[derive(Debug, thiserror::Error)]
pub enum HostKeyError {
    #[error("ssh host key {path}: {reason}")]
    Unusable { path: PathBuf, reason: String },
    #[error("ssh host key {path} does not exist (it is generated when the server starts)")]
    Missing { path: PathBuf },
}

fn unusable(path: &Path, reason: impl std::fmt::Display) -> HostKeyError {
    HostKeyError::Unusable {
        path: path.to_owned(),
        reason: reason.to_string(),
    }
}

/// One loaded key with its public identity.
pub struct HostKey {
    path: PathBuf,
    key: PrivateKey,
    fingerprint: SshFingerprint,
}

impl HostKey {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `SHA256:…`, the form OpenSSH shows and `known_hosts` checks.
    pub fn fingerprint(&self) -> SshFingerprint {
        self.fingerprint
    }

    /// The algorithm name (`ssh-ed25519`, `ssh-rsa`).
    pub fn algorithm(&self) -> String {
        self.key.algorithm().as_str().to_owned()
    }

    /// The public half as an `authorized_keys`-style line without a comment:
    /// `ssh-ed25519 AAAA…`.
    pub fn public_line(&self) -> String {
        self.key
            .public_key()
            .to_openssh()
            .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    }
}

/// The host keys the listener presents.
pub struct HostKeys {
    keys: Vec<HostKey>,
}

impl std::fmt::Debug for HostKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_list();
        for k in &self.keys {
            list.entry(&format_args!(
                "{} {} {}",
                k.path.display(),
                k.algorithm(),
                k.fingerprint
            ));
        }
        list.finish()
    }
}

impl HostKeys {
    /// Loads every file in `files`; a missing first file is generated (ed25519). Any other
    /// missing file, and any file that fails the checks, is an error: the listener does not
    /// start with a host key it cannot trust.
    pub fn load_or_create(files: &[PathBuf]) -> Result<Self, HostKeyError> {
        let mut keys: Vec<HostKey> = Vec::with_capacity(files.len());
        for (i, path) in files.iter().enumerate() {
            let key = match open_and_read(path) {
                Ok(key) => key,
                Err(HostKeyError::Missing { .. }) if i == 0 => create(path)?,
                // A concurrent creator made the file but has not written it yet: wait for it.
                Err(HostKeyError::Unusable { .. })
                    if i == 0 && std::fs::metadata(path).is_ok_and(|m| m.len() == 0) =>
                {
                    reopen_winner(path)?
                }
                Err(e) => return Err(e),
            };
            let fingerprint = fingerprint_of(&key);
            if keys.iter().any(|k| k.fingerprint == fingerprint) {
                return Err(unusable(path, "the same key is listed twice"));
            }
            keys.push(HostKey {
                path: path.clone(),
                key,
                fingerprint,
            });
        }
        if keys.is_empty() {
            return Err(unusable(Path::new(""), "no host key file is configured"));
        }
        Ok(Self { keys })
    }

    /// Loads the files without creating anything (for `show`).
    pub fn load_existing(files: &[PathBuf]) -> Result<Self, HostKeyError> {
        let mut keys = Vec::with_capacity(files.len());
        for path in files {
            let key = open_and_read(path)?;
            let fingerprint = fingerprint_of(&key);
            keys.push(HostKey {
                path: path.clone(),
                key,
                fingerprint,
            });
        }
        Ok(Self { keys })
    }

    pub fn keys(&self) -> &[HostKey] {
        &self.keys
    }

    /// Copies for the SSH library's `Config`, which prints them as `***`.
    pub fn private_keys(&self) -> Vec<PrivateKey> {
        self.keys.iter().map(|k| k.key.clone()).collect()
    }

    /// What `asc serve ssh host-key show` prints: each key's `SHA256:` fingerprint and a
    /// `known_hosts` line. `host` is the name clients use; `port` is the listener's port.
    pub fn show_text(&self, host: &str, port: u16) -> String {
        let pattern = if port == 22 {
            host.to_owned()
        } else {
            format!("[{host}]:{port}")
        };
        let mut out = String::new();
        for k in &self.keys {
            let _ = writeln!(
                out,
                "{}  {}  {}",
                k.fingerprint,
                k.algorithm(),
                k.path.display()
            );
            let _ = writeln!(out, "  known_hosts: {pattern} {}", k.public_line());
        }
        out
    }
}

fn fingerprint_of(key: &PrivateKey) -> SshFingerprint {
    SshFingerprint::of_blob(&key.public_key().to_bytes().unwrap_or_default())
}

/// Opens `path` through the key-file checks and parses it.
fn open_and_read(path: &Path) -> Result<PrivateKey, HostKeyError> {
    let file = match key_file::open_private(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HostKeyError::Missing {
                path: path.to_owned(),
            });
        }
        Err(e) => return Err(unusable(path, e)),
    };
    check_owner(&file).map_err(|e| unusable(path, e))?;
    read_key(file, path)
}

/// Reads and checks the key in the opened `file`.
fn read_key(file: File, path: &Path) -> Result<PrivateKey, HostKeyError> {
    let mut buf = Vec::new();
    file.take(MAX_HOST_KEY_FILE + 1)
        .read_to_end(&mut buf)
        .map_err(|e| unusable(path, e))?;
    let parsed = if buf.len() as u64 > MAX_HOST_KEY_FILE {
        Err(unusable(path, "the file is too large for a host key"))
    } else {
        parse(&buf, path)
    };
    // Best effort: the text held a private key.
    buf.fill(0);
    parsed
}

fn parse(bytes: &[u8], path: &Path) -> Result<PrivateKey, HostKeyError> {
    let key = PrivateKey::from_openssh(bytes)
        .map_err(|_| unusable(path, "not an OpenSSH private key (or still being written)"))?;
    if key.is_encrypted() {
        return Err(unusable(
            path,
            "the key is passphrase-protected; the server needs an unencrypted key, as sshd does",
        ));
    }
    match key.algorithm() {
        Algorithm::Ed25519 => Ok(key),
        Algorithm::Rsa { .. } => {
            let bits = key
                .public_key()
                .key_data()
                .rsa()
                .map(|r| r.key_size())
                .unwrap_or(0);
            if (2048..=SshConfig::MAX_RSA_BITS).contains(&bits) {
                Ok(key)
            } else {
                Err(unusable(
                    path,
                    "an RSA host key must have 2048 to 16384 bits",
                ))
            }
        }
        other => Err(unusable(
            path,
            format!(
                "host key type `{}` is not supported (use ed25519 or RSA)",
                other.as_str()
            ),
        )),
    }
}

/// A fresh ed25519 key from the operating system's random source.
fn generate() -> Result<PrivateKey, String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("no random source: {e}"))?;
    let pair = Ed25519Keypair::from_seed(&seed);
    seed.fill(0);
    PrivateKey::new(KeypairData::Ed25519(pair), COMMENT).map_err(|e| e.to_string())
}

/// Creates the first host key file. A lost race re-opens the winner's file.
fn create(path: &Path) -> Result<PrivateKey, HostKeyError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| unusable(path, e))?;
    }
    let key = generate().map_err(|e| unusable(path, e))?;
    let text = key
        .to_openssh(LineEnding::LF)
        .map_err(|_| unusable(path, "cannot encode the new key"))?;
    match key_file::create_private(path) {
        Ok(mut file) => {
            let written = key_file::check_private(&file)
                .and_then(|()| file.write_all(text.as_bytes()))
                .and_then(|()| file.sync_all());
            if let Err(e) = written {
                drop(file);
                // A half-written key must not block the next start.
                let _ = std::fs::remove_file(path);
                return Err(unusable(path, e));
            }
            Ok(key)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => reopen_winner(path),
        Err(e) => Err(unusable(path, e)),
    }
}

/// Another process created the file between our look and our create: wait for it to finish
/// writing, then load what it wrote, through the same checks.
fn reopen_winner(path: &Path) -> Result<PrivateKey, HostKeyError> {
    let deadline = Instant::now() + RACE_WAIT;
    loop {
        match open_and_read(path) {
            Ok(key) => return Ok(key),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(RACE_POLL);
            }
        }
    }
}

/// `rotate`: replaces the first host key with a new ed25519 key. The old key is gone; clients
/// that trusted it will see a changed-key warning.
pub fn rotate(files: &[PathBuf]) -> Result<String, HostKeyError> {
    let Some(path) = files.first() else {
        return Err(unusable(Path::new(""), "no host key file is configured"));
    };
    match open_and_read(path) {
        Ok(old) if !matches!(old.algorithm(), Algorithm::Ed25519) => {
            return Err(unusable(
                path,
                "rotate replaces ed25519 host keys only; replace this RSA key by hand",
            ));
        }
        // A missing, unreadable or unchecked file is replaced: that is what an operator who runs
        // `rotate` on a broken key wants.
        Ok(_) | Err(_) => {}
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| unusable(path, e))?;
    let key = generate().map_err(|e| unusable(path, e))?;
    let text = key
        .to_openssh(LineEnding::LF)
        .map_err(|_| unusable(path, "cannot encode the new key"))?;
    // Written to a protected sibling first, then moved over the old file in one step, so a
    // crash leaves either the old key or the new one and never a partial file.
    let mut suffix = [0u8; 6];
    getrandom::fill(&mut suffix).map_err(|e| unusable(path, e))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(
        ".{name}.new-{}",
        suffix.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    ));
    let mut file = key_file::create_private(&temp).map_err(|e| unusable(path, e))?;
    let written = key_file::check_private(&file)
        .and_then(|()| file.write_all(text.as_bytes()))
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(unusable(path, e));
    }
    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(unusable(path, e));
    }
    Ok(format!(
        "new host key {}\nclients that trusted the old key will see a changed-key warning; \
         restart the server (and copy the file to every replica) to use it\n",
        fingerprint_of(&key)
    ))
}

/// Refuses a key file that another user owns (SPEC §25.11.6). The Windows check is part of
/// `key_file::open_private` (owner and access list on the opened handle).
#[cfg(unix)]
fn check_owner(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = file.metadata()?.uid();
    owner_matches(uid, rustix::process::geteuid().as_raw())
}

#[cfg(not(unix))]
fn check_owner(_file: &File) -> std::io::Result<()> {
    Ok(())
}

/// `Ok` when the file's owner is the running user. Root may own what it reads.
#[cfg(unix)]
fn owner_matches(file_uid: u32, euid: u32) -> std::io::Result<()> {
    if file_uid == euid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the key file is owned by another user",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_missing_first_file_is_created_once_and_kept() {
        let d = dir();
        let path = d.path().join("keys").join("ssh_host_ed25519_key");
        let first = HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        let again = HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        assert_eq!(first.keys()[0].fingerprint(), again.keys()[0].fingerprint());
        assert_eq!(first.keys()[0].algorithm(), "ssh-ed25519");
        assert!(
            first.keys()[0]
                .public_line()
                .starts_with("ssh-ed25519 AAAA")
        );
    }

    #[test]
    fn only_the_first_missing_file_is_generated() {
        let d = dir();
        let a = d.path().join("a");
        let b = d.path().join("b");
        let err = HostKeys::load_or_create(&[a.clone(), b.clone()]).unwrap_err();
        assert!(matches!(err, HostKeyError::Missing { .. }), "{err}");
        assert!(a.exists());
        assert!(!b.exists());
    }

    #[test]
    fn a_lost_creation_race_reopens_the_winners_file() {
        let d = dir();
        let path = d.path().join("k");
        let runs: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || {
                    HostKeys::load_or_create(&[p]).map(|k| k.keys()[0].fingerprint())
                })
            })
            .collect();
        let prints: Vec<_> = runs
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        assert!(prints.windows(2).all(|w| w[0] == w[1]), "{prints:?}");
        let n = std::fs::read_dir(d.path()).unwrap().count();
        assert_eq!(n, 1, "no stray files");
    }

    #[test]
    fn rotate_replaces_the_key_and_says_so() {
        let d = dir();
        let path = d.path().join("k");
        let before = HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        let said = rotate(std::slice::from_ref(&path)).unwrap();
        let after = HostKeys::load_existing(std::slice::from_ref(&path)).unwrap();
        assert_ne!(
            before.keys()[0].fingerprint(),
            after.keys()[0].fingerprint()
        );
        assert!(said.contains(&after.keys()[0].fingerprint().to_string()));
        assert!(said.contains("changed-key warning"));
        // Rotating a missing file creates it.
        let fresh = d.path().join("fresh");
        rotate(std::slice::from_ref(&fresh)).unwrap();
        HostKeys::load_existing(&[fresh]).unwrap();
        // No temp file is left behind.
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
    }

    #[test]
    fn show_prints_the_fingerprint_and_a_known_hosts_line() {
        let d = dir();
        let path = d.path().join("k");
        let keys = HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        let text = keys.show_text("git.example", 2222);
        let k = &keys.keys()[0];
        assert!(text.contains(&k.fingerprint().to_string()));
        assert!(text.contains(&format!("[git.example]:2222 {}", k.public_line())));
        assert!(
            keys.show_text("git.example", 22)
                .contains(&format!("git.example {}", k.public_line()))
        );
        assert!(!text.contains("PRIVATE"));
    }

    #[test]
    fn the_private_key_is_in_no_debug_or_error_text() {
        let d = dir();
        let path = d.path().join("k");
        let keys = HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        let file_text = std::fs::read_to_string(&path).unwrap();
        // The base64 body of the private key file, line by line.
        let body: Vec<&str> = file_text
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        assert!(!body.is_empty());
        let shown = format!("{keys:?} {}", keys.show_text("h", 22));
        for line in &body {
            assert!(!shown.contains(line), "private key text in output");
        }
        let cfg = format!(
            "{:?}",
            russh::server::Config {
                keys: keys.private_keys(),
                ..Default::default()
            }
        );
        for line in &body {
            assert!(!cfg.contains(line), "private key text in Config debug");
        }
        // Errors name the path and a reason only.
        std::fs::write(&path, "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRETSECRET\n").unwrap();
        let err = HostKeys::load_existing(std::slice::from_ref(&path)).unwrap_err();
        assert!(!err.to_string().contains("SECRETSECRET"), "{err}");
    }

    #[test]
    fn junk_and_unsupported_keys_are_refused() {
        let d = dir();
        let path = d.path().join("k");
        drop(key_file::create_private(&path).unwrap());
        // Empty: still being written, or truncated.
        assert!(HostKeys::load_existing(std::slice::from_ref(&path)).is_err());
        std::fs::write(&path, "not a key").unwrap();
        assert!(HostKeys::load_existing(std::slice::from_ref(&path)).is_err());
        std::fs::write(&path, vec![b'a'; MAX_HOST_KEY_FILE as usize + 10]).unwrap();
        assert!(HostKeys::load_existing(std::slice::from_ref(&path)).is_err());
    }

    #[test]
    fn the_same_key_twice_is_refused() {
        let d = dir();
        let a = d.path().join("a");
        let b = d.path().join("b");
        HostKeys::load_or_create(std::slice::from_ref(&a)).unwrap();
        // The same key in a second, properly protected file.
        let text = std::fs::read(&a).unwrap();
        let mut copy = key_file::create_private(&b).unwrap();
        copy.write_all(&text).unwrap();
        drop(copy);
        let err = HostKeys::load_or_create(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_foreign_owner_is_refused() {
        assert!(owner_matches(1000, 1000).is_ok());
        assert!(owner_matches(0, 1000).is_err());
        assert!(owner_matches(1000, 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_file_and_a_link_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = dir();
        let path = d.path().join("k");
        HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(HostKeys::load_or_create(std::slice::from_ref(&path)).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(HostKeys::load_or_create(&[link]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_widened_access_list_and_a_link_are_refused() {
        let d = dir();
        let path = d.path().join("k");
        HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap();
        let widened = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:R"])
            .status()
            .unwrap()
            .success();
        assert!(widened);
        let err = HostKeys::load_or_create(std::slice::from_ref(&path)).unwrap_err();
        assert!(err.to_string().contains("ssh host key"), "{err}");
        // A link (needs a privilege to make; skipped without it).
        let real = d.path().join("real");
        HostKeys::load_or_create(std::slice::from_ref(&real)).unwrap();
        let link = d.path().join("link");
        if std::os::windows::fs::symlink_file(&real, &link).is_ok() {
            assert!(HostKeys::load_or_create(&[link]).is_err());
        }
    }
}
