//! The master key file: a ring of 32-byte generations, readable by the server user only.
//!
//! ```text
//! # asc vault key ring v1; keep this file private and backed up
//! active 2
//! key 1 1767225600 <64 hex>
//! key 2 1769904000 <64 hex>
//! ```
//!
//! The file is created on first start (`0600`, or a protected ACL on Windows, from the first
//! instant it exists) and refused on load when anyone else could read it. A rewrite goes through
//! a private temporary file and a rename, so a crash leaves the old ring or the new one.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use super::VaultError;
use crate::authz::key_file;

/// Longest ring file accepted.
const MAX_FILE: u64 = 64 * 1024;
/// Most generations one ring holds.
pub const MAX_GENERATIONS: usize = 64;
const HEADER: &str = "# asc vault key ring v1; keep this file private and backed up";

/// One master key.
pub struct KeyGen {
    pub generation: u32,
    /// Unix seconds.
    pub created: i64,
    key: Zeroizing<[u8; 32]>,
}

impl KeyGen {
    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }
}

impl std::fmt::Debug for KeyGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyGen")
            .field("generation", &self.generation)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// The generations and which one seals new values.
#[derive(Debug)]
pub struct KeyRing {
    gens: Vec<KeyGen>,
    active: u32,
}

fn io(e: &std::io::Error) -> VaultError {
    VaultError::Locked(format!("key file: {}", e.kind()))
}

fn bad(msg: &str) -> VaultError {
    VaultError::Locked(format!("key file: {msg}"))
}

fn fresh_key() -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let mut k = Zeroizing::new([0u8; 32]);
    getrandom::fill(k.as_mut()).map_err(|_| VaultError::Crypto)?;
    Ok(k)
}

fn hex(bytes: &[u8]) -> Zeroizing<String> {
    use std::fmt::Write as _;
    let mut s = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex(s: &str) -> Option<Zeroizing<[u8; 32]>> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = Zeroizing::new([0u8; 32]);
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

impl KeyRing {
    /// A ring with one fresh generation 1.
    pub fn generate(now: i64) -> Result<Self, VaultError> {
        Ok(Self {
            gens: vec![KeyGen {
                generation: 1,
                created: now,
                key: fresh_key()?,
            }],
            active: 1,
        })
    }

    /// A ring from explicit keys, for tests.
    #[cfg(test)]
    pub fn from_keys(keys: &[(u32, [u8; 32])], active: u32) -> Self {
        Self {
            gens: keys
                .iter()
                .map(|(g, k)| KeyGen {
                    generation: *g,
                    created: 0,
                    key: Zeroizing::new(*k),
                })
                .collect(),
            active,
        }
    }

    pub fn active(&self) -> u32 {
        self.active
    }

    pub fn generations(&self) -> &[KeyGen] {
        &self.gens
    }

    pub fn get(&self, generation: u32) -> Option<&KeyGen> {
        self.gens.iter().find(|g| g.generation == generation)
    }

    /// The key that seals new values.
    pub fn active_key(&self) -> &KeyGen {
        // `parse` and `add` keep `active` naming a held generation.
        self.get(self.active).unwrap_or(&self.gens[0])
    }

    /// A copy of this ring with a new active generation. The caller writes it before using it.
    pub fn with_new_generation(&self, now: i64) -> Result<Self, VaultError> {
        if self.gens.len() >= MAX_GENERATIONS {
            return Err(VaultError::Locked(
                "key file: too many generations; retire old ones".into(),
            ));
        }
        let next = self.gens.iter().map(|g| g.generation).max().unwrap_or(0) + 1;
        let mut gens: Vec<KeyGen> = self
            .gens
            .iter()
            .map(|g| KeyGen {
                generation: g.generation,
                created: g.created,
                key: g.key.clone(),
            })
            .collect();
        gens.push(KeyGen {
            generation: next,
            created: now,
            key: fresh_key()?,
        });
        Ok(Self { gens, active: next })
    }

    /// A copy without the generations in `drop`. The active generation is never dropped.
    pub fn without(&self, drop: &[u32]) -> Self {
        Self {
            gens: self
                .gens
                .iter()
                .filter(|g| g.generation == self.active || !drop.contains(&g.generation))
                .map(|g| KeyGen {
                    generation: g.generation,
                    created: g.created,
                    key: g.key.clone(),
                })
                .collect(),
            active: self.active,
        }
    }

    fn render(&self) -> Zeroizing<String> {
        let mut out = Zeroizing::new(String::new());
        out.push_str(HEADER);
        out.push('\n');
        out.push_str(&format!("active {}\n", self.active));
        for g in &self.gens {
            out.push_str(&format!(
                "key {} {} {}\n",
                g.generation,
                g.created,
                hex(g.key.as_ref()).as_str()
            ));
        }
        out
    }

    /// Reads a ring from text. Strict: unknown lines, duplicate or out-of-order generations, a
    /// short key and an active generation that is not held are all errors.
    pub fn parse(text: &str) -> Result<Self, VaultError> {
        let mut gens: Vec<KeyGen> = Vec::new();
        let mut active = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.as_slice() {
                ["active", n] => {
                    if active.is_some() {
                        return Err(bad("two active lines"));
                    }
                    active = Some(n.parse::<u32>().map_err(|_| bad("bad active line"))?);
                }
                ["key", g, created, k] => {
                    let generation: u32 = g.parse().map_err(|_| bad("bad generation"))?;
                    let created: i64 = created.parse().map_err(|_| bad("bad creation time"))?;
                    let key = unhex(k).ok_or_else(|| bad("a key is not 64 hex digits"))?;
                    if generation == 0 || gens.last().is_some_and(|l| l.generation >= generation) {
                        return Err(bad("generations must be positive and ascending"));
                    }
                    gens.push(KeyGen {
                        generation,
                        created,
                        key,
                    });
                }
                _ => return Err(bad("unrecognised line")),
            }
        }
        let active = active.ok_or_else(|| bad("no active line"))?;
        if gens.is_empty() || gens.len() > MAX_GENERATIONS {
            return Err(bad("no keys, or too many"));
        }
        if !gens.iter().any(|g| g.generation == active) {
            return Err(bad("the active generation has no key"));
        }
        Ok(Self { gens, active })
    }
}

fn read_all(mut f: File) -> Result<Zeroizing<String>, VaultError> {
    let len = f.metadata().map_err(|e| io(&e))?.len();
    if len > MAX_FILE {
        return Err(bad("file is too large"));
    }
    let mut buf = Zeroizing::new(Vec::with_capacity(len as usize));
    std::io::Read::take(&mut f, MAX_FILE + 1)
        .read_to_end(&mut buf)
        .map_err(|e| io(&e))?;
    let text = std::str::from_utf8(&buf).map_err(|_| bad("not UTF-8"))?;
    Ok(Zeroizing::new(text.to_owned()))
}

/// Loads the ring at `path`, refusing a file anyone else could read.
pub fn load(path: &Path) -> Result<KeyRing, VaultError> {
    let file = key_file::open_private(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            // The message names the rule, not the secret.
            VaultError::Locked(format!("key file: {e}"))
        } else {
            io(&e)
        }
    })?;
    KeyRing::parse(&read_all(file)?)
}

fn temp_sibling(path: &Path) -> Result<PathBuf, VaultError> {
    let mut rnd = [0u8; 6];
    getrandom::fill(&mut rnd).map_err(|_| VaultError::Crypto)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| bad("the path has no file name"))?;
    Ok(path.with_file_name(format!(".{name}.{}.tmp", hex(&rnd).as_str())))
}

/// Writes `ring` to a private temporary file next to `path`, flushed to disk.
fn write_temp(path: &Path, ring: &KeyRing) -> Result<PathBuf, VaultError> {
    let tmp = temp_sibling(path)?;
    let mut f = key_file::create_private(&tmp).map_err(|e| io(&e))?;
    let text = ring.render();
    let written = f
        .write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| io(&e));
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

/// Replaces the ring file with `ring`, atomically. The old file stays if anything fails.
pub fn store(path: &Path, ring: &KeyRing) -> Result<(), VaultError> {
    let tmp = write_temp(path, ring)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(&e)
    })
}

/// Loads the ring, or creates it with one fresh generation when the file does not exist. Two
/// servers starting together agree on one file: creation is exclusive, and the loser loads.
pub fn load_or_create(path: &Path, now: i64) -> Result<KeyRing, VaultError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return load(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(&e)),
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| io(&e))?;
    }
    let ring = KeyRing::generate(now)?;
    let tmp = write_temp(path, &ring)?;
    // A hard link fails when `path` exists, which makes the publish exclusive.
    let published = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match published {
        Ok(()) => load(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load(path),
        Err(_) => {
            // No hard links on this file system: publish by rename when still absent.
            if std::fs::symlink_metadata(path).is_ok() {
                return load(path);
            }
            let tmp = write_temp(path, &ring)?;
            std::fs::rename(&tmp, path).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                io(&e)
            })?;
            load(path)
        }
    }
}

/// What the file's access looks like, for `Vault.Status`: `0600` when only the server user can
/// read it (the Windows check of an equivalent protected ACL counts as `0600`).
pub fn mode_text(path: &Path) -> String {
    match key_file::open_private(path) {
        Ok(_) => "0600".into(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if let Ok(m) = std::fs::metadata(path) {
                    return format!("{:04o}", m.permissions().mode() & 0o7777);
                }
            }
            "open".into()
        }
        Err(_) => "unreadable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_and_reloads_the_same_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("vault.keys");
        let a = load_or_create(&path, 100).unwrap();
        assert_eq!(a.active(), 1);
        let b = load_or_create(&path, 200).unwrap();
        assert_eq!(a.active_key().key(), b.active_key().key());
        assert_eq!(b.active_key().created, 100);
        assert_eq!(mode_text(&path), "0600");
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["vault.keys".to_owned()]);
    }

    #[test]
    fn rotation_keeps_old_generations_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.keys");
        let one = load_or_create(&path, 1).unwrap();
        let two = one.with_new_generation(2).unwrap();
        store(&path, &two).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.active(), 2);
        assert_eq!(back.generations().len(), 2);
        assert_eq!(back.get(1).unwrap().key(), one.get(1).unwrap().key());
        assert_ne!(back.get(1).unwrap().key(), back.get(2).unwrap().key());
        let pruned = back.without(&[1, 2]);
        assert_eq!(pruned.generations().len(), 1, "the active generation stays");
        assert_eq!(pruned.active(), 2);
    }

    #[test]
    fn strict_parser() {
        let k = "00".repeat(32);
        let good = format!("active 1\nkey 1 5 {k}\n");
        assert!(KeyRing::parse(&good).is_ok());
        for text in [
            String::new(),
            format!("key 1 5 {k}\n"),
            format!("active 2\nkey 1 5 {k}\n"),
            format!("active 1\nactive 1\nkey 1 5 {k}\n"),
            format!("active 1\nkey 1 5 {}\n", "00".repeat(31)),
            format!("active 1\nkey 1 5 {}\n", "zz".repeat(32)),
            format!("active 2\nkey 2 5 {k}\nkey 1 5 {k}\n"),
            format!("active 1\nkey 1 5 {k}\nkey 1 6 {k}\n"),
            format!("active 1\nkey 0 5 {k}\n"),
            format!("active 1\nkey 1 5 {k}\nsurprise\n"),
        ] {
            assert!(KeyRing::parse(&text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn debug_output_hides_keys() {
        let ring = KeyRing::generate(0).unwrap();
        let shown = format!("{ring:?}");
        assert!(shown.contains("<redacted>"));
        assert!(!shown.contains(hex(ring.active_key().key()).as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn a_readable_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.keys");
        load_or_create(&path, 1).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(load(&path), Err(VaultError::Locked(_))));
        assert_eq!(mode_text(&path), "0644");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&path).is_ok());
    }
}
