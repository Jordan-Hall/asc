//! Where a finished backup lives: a local directory or an S3 bucket.
//!
//! A local backup is the directory `asc serve backup` writes, kept under the target's path as
//! `<id>`. An S3 backup is the same files as objects under `<prefix><id>/`; every file but
//! `manifest.json` is encrypted when a backup key is configured, and a plain sidecar
//! (`asc-stored.json`) lists the stored size of each file so a check can compare sizes without
//! downloading the backup.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt as _;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt as _, WriteMultipart};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

use super::config::TargetConfig;
use super::crypto;

/// The manifest `asc serve backup` writes; it is never encrypted (it holds sizes and digests).
pub const MANIFEST: &str = "manifest.json";
/// The sidecar of an S3 backup.
pub const SIDECAR: &str = "asc-stored.json";
const PART: usize = 16 * 1024 * 1024;

/// What an S3 backup says about itself. It sits beside the backup and is NOT trusted: the server
/// compares it with its own record of the run (`encrypted`, and the seal) before it believes it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub encrypted: bool,
    pub files: Vec<(String, u64)>,
}

/// Largest backup a restore will download, in stored bytes.
pub const MAX_RESTORE_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
/// What the server recorded of a run, which a restore believes instead of anything stored
/// beside the backup.
#[derive(Clone, Debug)]
pub struct Expected {
    pub encrypted: bool,
    /// The stored bytes of all files.
    pub size: u64,
    pub seal: String,
}

/// Largest sidecar read.
const MAX_SIDECAR: u64 = 16 * 1024 * 1024;

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// The server's record of what a finished backup holds: its id, whether it was encrypted, the
/// stored size of each file and the SHA-256 of the manifest (which in turn lists the digest of every
/// other file). It is kept in the database, never beside the backup. With a backup key it is an
/// HMAC-SHA256 under a key derived from it, so it also proves the key; without one, a SHA-256.
pub fn seal(
    key: Option<&[u8; 32]>,
    id: &str,
    encrypted: bool,
    files: &[(String, u64)],
    manifest: &[u8],
) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest as _, Sha256};
    let mut files = files.to_vec();
    files.sort();
    let mut input = format!(
        "asc-backup-seal-v1\nid={id}\nenc={}\nmanifest={}\n",
        u8::from(encrypted),
        hex(&Sha256::digest(manifest))
    );
    for (rel, size) in &files {
        input.push_str(&format!("file={size} {rel}\n"));
    }
    match key {
        Some(k) => {
            let mut okm = [0u8; 32];
            hkdf::Hkdf::<Sha256>::new(None, k)
                .expand(b"asc-backup-seal-v1", &mut okm)
                .expect("32 bytes is a valid output length");
            let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&okm).expect("any key length");
            mac.update(input.as_bytes());
            format!("hmac:{}", hex(&mac.finalize().into_bytes()))
        }
        None => format!("sha256:{}", hex(&Sha256::digest(input.as_bytes()))),
    }
}

/// Copies a local backup directory into a new private one (mode 0700 directories, 0600 files).
/// A link inside the backup is an error; the copy never reads the original again.
pub fn copy_private(src: &Path, dst: &Path) -> Result<(), String> {
    let files = list_dir(src).map_err(err)?;
    let total = files
        .iter()
        .map(|(_, n)| *n)
        .fold(0u64, u64::saturating_add);
    if files.len() > 1_000_000 || total > MAX_RESTORE_BYTES {
        return Err("the backup is larger than a restore will take".into());
    }
    crate::workdir::create_private_all(dst).map_err(err)?;
    for (rel, size) in files {
        if !safe_rel(&rel) {
            return Err("unsafe file name in the backup".into());
        }
        let from = src.join(&rel);
        if std::fs::symlink_metadata(&from)
            .map_err(err)?
            .file_type()
            .is_symlink()
        {
            return Err("a link in the backup".into());
        }
        let to = dst.join(&rel);
        if let Some(p) = to.parent() {
            crate::workdir::create_private_all(p).map_err(err)?;
        }
        let mut input = std::io::Read::take(std::fs::File::open(&from).map_err(err)?, size);
        let mut out = crypto::create_private_file(&to).map_err(err)?;
        std::io::copy(&mut input, &mut out).map_err(err)?;
        out.sync_all().map_err(err)?;
    }
    Ok(())
}

/// The seal of a local backup directory as it is now.
pub fn local_seal(dir: &Path, id: &str) -> Result<String, String> {
    let files = list_dir(dir).map_err(err)?;
    let manifest =
        std::fs::read(dir.join(MANIFEST)).map_err(|_| "the backup has no manifest".to_owned())?;
    Ok(seal(None, id, false, &files, &manifest))
}

/// A target ready to use.
pub enum Place {
    Local(PathBuf),
    S3 {
        store: Arc<dyn ObjectStore>,
        prefix: String,
    },
}

fn err(m: impl std::fmt::Display) -> String {
    m.to_string()
}

/// Opens a target. An S3 credential is read from the environment variables the configuration names.
pub fn open(t: &TargetConfig) -> Result<Place, String> {
    match t {
        TargetConfig::Local { path } => Ok(Place::Local(path.clone())),
        TargetConfig::S3 {
            bucket,
            endpoint,
            region,
            prefix,
            access_key_id_env,
            secret_access_key_env,
        } => {
            let mut b = object_store::aws::AmazonS3Builder::new().with_bucket_name(bucket);
            if !endpoint.is_empty() {
                b = b.with_endpoint(endpoint);
            }
            b = b.with_region(if region.is_empty() { "auto" } else { region });
            for (var, set) in [(access_key_id_env, 0), (secret_access_key_env, 1)] {
                if var.is_empty() {
                    continue;
                }
                let v = std::env::var(var)
                    .map_err(|_| "the target's credential variable is not set".to_owned())?;
                b = if set == 0 {
                    b.with_access_key_id(v)
                } else {
                    b.with_secret_access_key(v)
                };
            }
            let store = b.build().map_err(err)?;
            let mut p = prefix.trim_matches('/').to_owned();
            if !p.is_empty() {
                p.push('/');
            }
            Ok(Place::S3 {
                store: Arc::new(store),
                prefix: p,
            })
        }
    }
}

/// A file name of a backup is a plain relative path.
pub fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        && !rel.contains(['\\', '\0'])
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, u64)>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        let path = e.path();
        if ft.is_dir() {
            walk(&path, base, out)?;
        } else if ft.is_symlink() {
            return Err(std::io::Error::other("a link in the backup"));
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(std::io::Error::other)?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, e.metadata()?.len()));
        }
    }
    Ok(())
}

/// Every file under `dir`, relative, sorted, with its size.
pub fn list_dir(dir: &Path) -> std::io::Result<Vec<(String, u64)>> {
    let mut v = Vec::new();
    walk(dir, dir, &mut v)?;
    v.sort();
    Ok(v)
}

impl Place {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// The directory of backup `id` on a local target.
    pub fn local_dir(&self, id: &str) -> Option<PathBuf> {
        match self {
            Self::Local(base)
                if id.starts_with("bk_")
                    && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
            {
                Some(base.join(id))
            }
            _ => None,
        }
    }

    fn key(&self, id: &str, rel: &str) -> Option<(Arc<dyn ObjectStore>, ObjPath)> {
        match self {
            Self::S3 { store, prefix } => {
                Some((store.clone(), ObjPath::from(format!("{prefix}{id}/{rel}"))))
            }
            Self::Local(_) => None,
        }
    }

    /// Copies the backup in `src` to an S3 target as `id`, encrypting under `key` when given.
    /// `progress` is told the bytes of `src` sent so far. Returns the stored bytes.
    pub async fn upload_dir(
        &self,
        id: &str,
        src: &Path,
        key: Option<[u8; 32]>,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<(u64, String), String> {
        let Self::S3 { store, prefix } = self else {
            return Err("only an S3 target is uploaded to".into());
        };
        let files = list_dir(src).map_err(err)?;
        let mut stored = Vec::new();
        let (mut sent, mut stored_total) = (0u64, 0u64);
        for (rel, size) in &files {
            if !safe_rel(rel) {
                return Err("unsafe file name in the backup".into());
            }
            let encrypt = key.is_some() && rel != MANIFEST;
            let (path, tmp) = if encrypt {
                let tmp = src.with_file_name(format!(".enc-{id}"));
                let (from, to, k) = (src.join(rel), tmp.clone(), key.expect("checked"));
                let _ = std::fs::remove_file(&tmp);
                tokio::task::spawn_blocking(move || crypto::encrypt_file(&k, &from, &to))
                    .await
                    .map_err(err)?
                    .map_err(err)?;
                (tmp.clone(), Some(tmp))
            } else {
                (src.join(rel), None)
            };
            let res = put_file(
                store.as_ref(),
                &ObjPath::from(format!("{prefix}{id}/{rel}")),
                &path,
            )
            .await;
            if let Some(t) = &tmp {
                let _ = std::fs::remove_file(t);
            }
            let n = res?;
            stored.push((rel.clone(), n));
            stored_total += n;
            sent += size;
            progress(sent);
        }
        let manifest = std::fs::read(src.join(MANIFEST))
            .map_err(|_| "the backup has no manifest".to_owned())?;
        let sealed = seal(key.as_ref(), id, key.is_some(), &stored, &manifest);
        let side = serde_json::to_vec(&Sidecar {
            encrypted: key.is_some(),
            files: stored,
        })
        .map_err(err)?;
        store
            .put(
                &ObjPath::from(format!("{prefix}{id}/{SIDECAR}")),
                side.into(),
            )
            .await
            .map_err(err)?;
        Ok((stored_total, sealed))
    }

    /// Copies backup `id` of an S3 target to `dst` and returns its seal as it now stands.
    ///
    /// `encrypted` is what the server recorded for the run, not what the sidecar claims: a
    /// sidecar that says otherwise is refused, and when the run was encrypted every file but the
    /// manifest must decrypt and authenticate under `key`. The caller compares the returned seal
    /// with the one in its database before it uses anything.
    pub async fn download_dir(
        &self,
        id: &str,
        dst: &Path,
        key: Option<[u8; 32]>,
        expected: &Expected,
    ) -> Result<(), String> {
        let encrypted = expected.encrypted;
        let side = self.sidecar(id).await?;
        if side.encrypted != encrypted {
            return Err("the backup's record on the target disagrees with the server's".into());
        }
        if encrypted && key.is_none() {
            return Err("the backup is encrypted and no backup key is configured".into());
        }
        // The sidecar is untrusted input. Before anything is downloaded it must add up to what the
        // server recorded, and (with the manifest, read under its own cap) match the server's seal.
        let total = side
            .files
            .iter()
            .map(|(_, n)| *n)
            .fold(0u64, u64::saturating_add);
        if side.files.len() > 1_000_000 || total > MAX_RESTORE_BYTES || total != expected.size {
            return Err("the backup's record on the target disagrees with the server's".into());
        }
        let manifest = self.read_manifest(id).await?;
        if seal(key.as_ref(), id, encrypted, &side.files, &manifest) != expected.seal
            || expected.seal.is_empty()
        {
            return Err("the backup does not match the record the server made of it".into());
        }
        let mut downloaded = 0u64;
        let mut seen = std::collections::HashSet::new();
        for (rel, declared) in &side.files {
            if !safe_rel(rel) || !seen.insert(rel.as_str()) || rel == SIDECAR {
                return Err("unsafe file name in the backup".into());
            }
            let (store, path) = self.key(id, rel).ok_or("not an S3 target")?;
            let target = dst.join(rel);
            let encrypted_file = side.encrypted && rel != MANIFEST;
            let raw = if encrypted_file {
                dst.join(format!(".dl-{id}"))
            } else {
                target.clone()
            };
            if let Some(p) = raw.parent() {
                std::fs::create_dir_all(p).map_err(err)?;
            }
            let mut file = crypto::create_private_file(&raw).map_err(err)?;
            let mut body = store.get(&path).await.map_err(err)?.into_stream();
            let mut got = 0u64;
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(err)?;
                got += chunk.len() as u64;
                downloaded += chunk.len() as u64;
                if got > *declared || downloaded > expected.size {
                    return Err(format!("{rel} is larger on the target than it was stored"));
                }
                std::io::Write::write_all(&mut file, &chunk).map_err(err)?;
            }
            file.sync_all().map_err(err)?;
            drop(file);
            if got != *declared {
                return Err(format!("{rel} has the wrong size on the target"));
            }
            if encrypted_file {
                let (from, to, k) = (raw.clone(), target, key.expect("checked"));
                let r = tokio::task::spawn_blocking(move || crypto::decrypt_file(&k, &from, &to))
                    .await
                    .map_err(err)?;
                let _ = std::fs::remove_file(&raw);
                r.map_err(err)?;
            }
        }
        Ok(())
    }

    pub async fn sidecar(&self, id: &str) -> Result<Sidecar, String> {
        let (store, path) = self.key(id, SIDECAR).ok_or("not an S3 target")?;
        let got = store
            .get(&path)
            .await
            .map_err(|_| "the backup's files are not on the target".to_owned())?;
        if got.meta.size > MAX_SIDECAR {
            return Err("the backup's record on the target is too large".into());
        }
        let bytes = got.bytes().await.map_err(err)?;
        serde_json::from_slice(&bytes)
            .map_err(|_| "the backup's record on the target is damaged".to_owned())
    }

    /// The manifest of backup `id`, at most 16 MiB.
    pub async fn read_manifest(&self, id: &str) -> Result<Vec<u8>, String> {
        const MAX: u64 = 16 * 1024 * 1024;
        match self {
            Self::Local(_) => {
                let dir = self.local_dir(id).ok_or("not a valid backup id")?;
                let f = std::fs::File::open(dir.join(MANIFEST))
                    .map_err(|_| "the backup's files are not on the target".to_owned())?;
                let mut raw = Vec::new();
                std::io::Read::read_to_end(&mut std::io::Read::take(f, MAX + 1), &mut raw)
                    .map_err(err)?;
                if raw.len() as u64 > MAX {
                    return Err("the manifest is too large".into());
                }
                Ok(raw)
            }
            Self::S3 { .. } => {
                let (store, path) = self.key(id, MANIFEST).ok_or("not an S3 target")?;
                let got = store
                    .get(&path)
                    .await
                    .map_err(|_| "the backup's files are not on the target".to_owned())?;
                if got.meta.size > MAX {
                    return Err("the manifest is too large".into());
                }
                Ok(got.bytes().await.map_err(err)?.to_vec())
            }
        }
    }

    /// Whether every file of S3 backup `id` is on the target at the size it was stored with, and
    /// its seal as it now stands (compare it with the server's record). `encrypted` is the
    /// server's record of the run.
    pub async fn check(
        &self,
        id: &str,
        key: Option<[u8; 32]>,
        encrypted: bool,
    ) -> Result<String, String> {
        let side = self.sidecar(id).await?;
        if side.encrypted != encrypted {
            return Err("the backup's record on the target disagrees with the server's".into());
        }
        let mut total = 0u64;
        for (rel, size) in &side.files {
            let (store, path) = self.key(id, rel).ok_or("not an S3 target")?;
            let meta = store
                .head(&path)
                .await
                .map_err(|_| format!("{rel} is missing from the target"))?;
            if meta.size != *size {
                return Err(format!("{rel} has the wrong size on the target"));
            }
            total += size;
        }
        let _ = total;
        let manifest = self.read_manifest(id).await?;
        Ok(seal(key.as_ref(), id, encrypted, &side.files, &manifest))
    }

    /// Removes backup `id` from the target. A missing backup is not an error.
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        match self {
            Self::Local(_) => {
                if let Some(dir) = self.local_dir(id)
                    && dir.exists()
                {
                    std::fs::remove_dir_all(dir).map_err(err)?;
                }
                Ok(())
            }
            Self::S3 { store, prefix } => {
                let root = ObjPath::from(format!("{prefix}{id}"));
                let mut list = store.list(Some(&root));
                let mut paths = Vec::new();
                while let Some(m) = list.next().await {
                    paths.push(m.map_err(err)?.location);
                }
                for p in paths {
                    store.delete(&p).await.map_err(err)?;
                }
                Ok(())
            }
        }
    }
}

async fn put_file(store: &dyn ObjectStore, path: &ObjPath, file: &Path) -> Result<u64, String> {
    let mut f = tokio::fs::File::open(file).await.map_err(err)?;
    let upload = store.put_multipart(path).await.map_err(err)?;
    let mut w = WriteMultipart::new_with_chunk_size(upload, PART);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf).await.map_err(err)?;
        if k == 0 {
            break;
        }
        w.wait_for_capacity(4).await.map_err(err)?;
        w.write(&buf[..k]);
        n += k as u64;
    }
    w.finish().await.map_err(err)?;
    Ok(n)
}
