//! Local semantic search, the opt-in half of the local index (SPEC §23.2.5, §23.5, unit I20).
//!
//! `asc codeintel enable-embed` downloads the `asc-embed` helper, the pinned model files and the
//! calibration table into `$ASC_HOME/embed/`. What it downloads is described by a `manifest.json`
//! whose minisign signature is checked against the release key baked into the build; every file
//! the manifest lists is then fetched with a size cap and checked against the sha256 the signed
//! manifest holds. The model files are checked a second time by the helper itself, against the
//! digests pinned in its registry. Nothing is installed when any check fails.
//!
//! The helper is a separate process (the daemon and `asc.exe` never load a model): the worker
//! spawns it on demand, talks to it with the length-prefixed JSON frames of `asc-embed`, and
//! closes it after [`EXIT_AFTER`] without work. Only when `codeintel.local.embed = true` does the
//! worker embed the chunks of an indexed revision. The vectors go to
//! `ci/<repo-id>/vec/<model_gen>/` as immutable b1 + i8 files (`asc-codeintel` `LocalVectors`,
//! each with a header holding the model id and the calibration digest); a query prefilters
//! with b1 Hamming distance and rescores the top 200 with the i8 vectors. A directory of
//! another generation is deleted and rebuilt, never migrated.
//!
//! Everything read back is untrusted: sizes are capped before allocation, frames and vectors are
//! validated, vector files are hash-checked, and no path is followed through a link.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use asc_codeintel::chunk::{self, CHUNKER_VERSION};
use asc_codeintel::lang::Lang;
use asc_codeintel::skip::{self, Flags};
use asc_codeintel::vectors::{
    self, Hit, LocalVectors, MAX_DIMS, MAX_K, ModelRef, SegmentBuilder, chunk_key,
    drop_stale_generations, generation_dir, load_local,
};
use asc_core::id::FileId;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::store::{
    CiRoot, Lock, MAX_TRACKED, RepoStore, RevRec, SegRec, ensure_dir, read_capped, write_atomic,
};
use super::{EXIT_AFTER, LocalError, Result, hex, unhex32};

/// The only model the registry of `asc-embed` knows (SPEC §23.7).
pub const MODEL: &str = "jina-embeddings-v2-base-code";
/// The `asc-embed` frame protocol version.
const PROTOCOL: u32 = 1;
/// Largest frame the helper may send or receive.
const MAX_FRAME: usize = 16 << 20;
/// Texts per embed request (the SPEC batch).
pub const BATCH: usize = 32;
/// A vector file is written once its builder holds this many chunks.
const FLUSH_CHUNKS: usize = 4_096;
/// Chunks one run embeds before it yields; the rest is picked up by the next run.
pub const MAX_RUN_CHUNKS: usize = 200_000;
/// Longest wait for one helper answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Most vector files one generation may hold.
const MAX_VEC_FILES: usize = 4_096;
/// Longest query text.
const MAX_QUERY: usize = 16 << 10;
/// Vectors remembered by text within one run.
const CACHE_ENTRIES: usize = 8_192;

const MAX_MANIFEST: u64 = 1 << 20;
const MAX_SIG: u64 = 16 << 10;
const MAX_HELPER: u64 = 256 << 20;
const MAX_ONNX: u64 = 1 << 30;
const MAX_MODEL_SMALL: u64 = 64 << 20;
const MAX_CALIB: u64 = 4 + (MAX_DIMS as u64) * 8;
const MAX_STATE: u64 = 16 << 20;
const MODEL_FILES: [&str; 5] = [
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];
const HELPER_FILE: &str = if cfg!(windows) {
    "asc-embed.exe"
} else {
    "asc-embed"
};
const MANIFEST_NAME: &str = "manifest.json";
const PUBKEY: Option<&str> = option_env!("ASC_RELEASE_PUBKEY");

const TARGET: &str = if cfg!(all(windows, target_arch = "x86_64")) {
    "x86_64-pc-windows-msvc"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "x86_64-unknown-linux-gnu"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "aarch64-unknown-linux-gnu"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "aarch64-apple-darwin"
} else {
    "unknown"
};

fn other(m: impl std::fmt::Display) -> LocalError {
    LocalError::Other(m.to_string())
}

fn vec_err(e: vectors::VectorError) -> LocalError {
    LocalError::Other(format!("vectors: {e}"))
}

/// `codeintel.local.embed`.
pub fn enabled(config: &crate::cli::config::ConfigSnapshot) -> bool {
    config.bool("codeintel.local.embed") == Some(true)
}

// ------------------------------------------------------------------------- calibration

/// The i8 table (per-dimension min and max), read from untrusted bytes. The layout is that of
/// `asc_embed::Calibration::to_bytes`: `u32 LE dims`, all mins, all maxes, as f32 LE.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    min: Vec<f32>,
    max: Vec<f32>,
}

impl Table {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let bad = || other("bad calibration table");
        let head: [u8; 4] = bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(bad)?;
        let dims = u32::from_le_bytes(head) as usize;
        if dims == 0 || dims > MAX_DIMS || bytes.len() != 4 + dims * 8 {
            return Err(bad());
        }
        let floats: Vec<f32> = bytes[4..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let (min, max) = floats.split_at(dims);
        if floats.iter().any(|x| !x.is_finite()) || min.iter().zip(max).any(|(lo, hi)| lo > hi) {
            return Err(bad());
        }
        Ok(Self {
            min: min.to_vec(),
            max: max.to_vec(),
        })
    }

    pub fn dims(&self) -> usize {
        self.min.len()
    }

    /// Maps each dimension's `[min, max]` onto `[-127, 127]`, clamped.
    pub fn quantise(&self, v: &[f32]) -> Result<Vec<i8>> {
        if v.len() != self.dims() {
            return Err(other("vector and calibration table differ in dimensions"));
        }
        Ok(v.iter()
            .zip(self.min.iter().zip(&self.max))
            .map(|(&x, (&lo, &hi))| {
                let span = hi - lo;
                if span <= 0.0 || !x.is_finite() {
                    return 0;
                }
                let t = ((x - lo) / span).clamp(0.0, 1.0);
                (t * 254.0 - 127.0).round() as i8
            })
            .collect())
    }
}

// ------------------------------------------------------------------------- the install

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    file: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    model: String,
    helpers: BTreeMap<String, Artifact>,
    model_files: BTreeMap<String, Artifact>,
    calibration: Artifact,
}

/// `install.json`: what was installed and the digests the files must still have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRec {
    pub model: String,
    pub onnx_sha256: String,
    pub calibration_blake3: String,
    pub helper_sha256: String,
    pub manifest_sha256: String,
}

fn hex_n(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn plain_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 96
        && !n.starts_with('.')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

impl Artifact {
    fn check(&self, max: u64) -> Result<()> {
        if !plain_name(&self.file) || !hex_n(&self.sha256, 64) || self.size == 0 || self.size > max
        {
            return Err(other("the manifest names an invalid file"));
        }
        Ok(())
    }
}

impl Manifest {
    fn check(&self) -> Result<()> {
        if self.version != 1 || self.model != MODEL {
            return Err(other("the manifest is for another version or model"));
        }
        if self.helpers.len() > 16 || self.helpers.keys().any(|k| !plain_name(k)) {
            return Err(other("the manifest lists too many helpers"));
        }
        for a in self.helpers.values() {
            a.check(MAX_HELPER)?;
        }
        if self
            .model_files
            .keys()
            .map(String::as_str)
            .ne(sorted_model_files())
        {
            return Err(other("the manifest does not list exactly the model files"));
        }
        for (name, a) in &self.model_files {
            a.check(if name == "model.onnx" {
                MAX_ONNX
            } else {
                MAX_MODEL_SMALL
            })?;
        }
        self.calibration.check(MAX_CALIB)
    }
}

fn sorted_model_files() -> impl Iterator<Item = &'static str> {
    let mut v = MODEL_FILES;
    v.sort_unstable();
    v.into_iter()
}

/// An installed, intact helper with its model and calibration.
#[derive(Debug, Clone)]
pub struct Installed {
    dir: PathBuf,
    pub rec: InstallRec,
    pub table: Table,
}

fn embed_dir(home: &Path) -> PathBuf {
    home.join("embed")
}

fn is_plain_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && !asc_wc::safe_path::is_link(&m))
}

fn sha256_file(p: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(p)?;
    if !meta.is_file() || asc_wc::safe_path::is_link(&meta) {
        return Err(other("not a regular file"));
    }
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

impl Installed {
    /// The installation under `home`, or `None` when there is none. A damaged one is an error.
    pub fn load(home: &Path) -> Result<Option<Self>> {
        let dir = embed_dir(home);
        if std::fs::symlink_metadata(&dir).is_err() {
            return Ok(None);
        }
        if !is_plain_dir(&dir) {
            return Err(other("the embed directory is not a real directory"));
        }
        let rec: InstallRec =
            serde_json::from_slice(&read_capped(&dir.join("install.json"), 64 << 10)?)
                .map_err(|_| other("damaged install.json"))?;
        if rec.model != MODEL
            || !hex_n(&rec.onnx_sha256, 64)
            || !hex_n(&rec.calibration_blake3, 64)
            || !hex_n(&rec.helper_sha256, 64)
            || !hex_n(&rec.manifest_sha256, 64)
        {
            return Err(other("damaged install.json"));
        }
        let calib = read_capped(&dir.join("calibration.bin"), MAX_CALIB)?;
        if blake3::hash(&calib).to_hex().as_str() != rec.calibration_blake3 {
            return Err(other("the calibration table does not match its digest"));
        }
        Ok(Some(Self {
            table: Table::from_bytes(&calib)?,
            dir,
            rec,
        }))
    }

    fn helper_path(&self) -> PathBuf {
        self.dir.join(HELPER_FILE)
    }

    fn model_dir(&self) -> PathBuf {
        self.dir.join("model")
    }

    /// `name@<onnx sha256>/i8/calib@<digest>/<chunker_ver>` (SPEC §23.7), and its generation.
    /// The local generation is derived from the id, so any change to a part of it is a new
    /// generation and the old vectors are dropped.
    pub fn model_ref(&self) -> ModelRef {
        let model_id = format!(
            "{}@{}/i8/calib@{}/{CHUNKER_VERSION}",
            self.rec.model, self.rec.onnx_sha256, self.rec.calibration_blake3
        );
        let h = blake3::hash(model_id.as_bytes());
        let b = h.as_bytes();
        ModelRef {
            model_gen: u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0x7fff_ffff,
            calib: unhex32(&self.rec.calibration_blake3).unwrap_or([0; 32]),
            model_id,
        }
    }
}

/// Where the helper files come from.
pub enum Source {
    Dir(PathBuf),
    Url(String),
}

impl Source {
    pub fn url(base: &str) -> Result<Self> {
        if !base.starts_with("https://") {
            return Err(other("asc.releasesUrl must be an https URL"));
        }
        Ok(Self::Url(base.trim_end_matches('/').to_owned()))
    }

    /// Streams `embed/<name>` to `sink`, at most `max` bytes.
    async fn pull(
        &self,
        name: &str,
        max: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if !plain_name(name) {
            return Err(other("unsafe file name"));
        }
        let rel = format!("embed/{name}");
        let too_big = || other(format!("{rel} is larger than {max} bytes"));
        match self {
            Self::Dir(d) => {
                let p = d.join(&rel);
                let meta = std::fs::symlink_metadata(&p)?;
                if !meta.is_file() || asc_wc::safe_path::is_link(&meta) || meta.len() > max {
                    return Err(too_big());
                }
                let mut f = std::fs::File::open(&p)?;
                let mut buf = vec![0u8; 1 << 20];
                let mut got = 0u64;
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    got += n as u64;
                    if got > max {
                        return Err(too_big());
                    }
                    sink(&buf[..n])?;
                }
            }
            Self::Url(base) => {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(1800))
                    .https_only(true)
                    .redirect(reqwest::redirect::Policy::limited(5))
                    .build()
                    .map_err(|e| other(format!("http client: {e}")))?;
                let mut resp = client
                    .get(format!("{base}/{rel}"))
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| other(format!("fetching {rel}: {e}")))?;
                if resp.content_length().is_some_and(|n| n > max) {
                    return Err(too_big());
                }
                let mut got = 0u64;
                while let Some(c) = resp
                    .chunk()
                    .await
                    .map_err(|e| other(format!("fetching {rel}: {e}")))?
                {
                    got += c.len() as u64;
                    if got > max {
                        return Err(too_big());
                    }
                    sink(&c)?;
                }
                Ok(())
            }
        }
    }

    async fn small(&self, name: &str, max: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.pull(name, max, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Downloads `a` to `dest` (a new file), checking its size and sha256.
    async fn file(&self, a: &Artifact, dest: &Path, max: u64) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)?;
        let mut h = Sha256::new();
        let mut size = 0u64;
        self.pull(&a.file, max.min(a.size), &mut |b| {
            h.update(b);
            size += b.len() as u64;
            f.write_all(b)?;
            Ok(())
        })
        .await?;
        f.sync_all()?;
        if size != a.size || hex(&h.finalize()) != a.sha256 {
            return Err(other(format!(
                "{} does not match the signed manifest",
                a.file
            )));
        }
        Ok(())
    }
}

/// Checks the minisign signature of `bytes`. The trusted comment must name `file:<expected>`,
/// so a validly signed file cannot stand in for another.
pub fn verify_signed(bytes: &[u8], sig: &str, key: &str, expected: &str) -> Result<()> {
    let key = minisign_verify::PublicKey::from_base64(key)
        .map_err(|e| other(format!("bad release key: {e}")))?;
    let sig = minisign_verify::Signature::decode(sig)
        .map_err(|e| other(format!("bad signature file: {e}")))?;
    key.verify(bytes, &sig, false).map_err(|e| {
        other(format!(
            "signature check failed, nothing was installed: {e}"
        ))
    })?;
    let named = sig
        .trusted_comment()
        .split(['\t', ' '])
        .any(|t| t.strip_prefix("file:") == Some(expected));
    if !named {
        return Err(other(format!("the signature is not for {expected}")));
    }
    Ok(())
}

/// Downloads and verifies the helper, then installs it under `home`.
pub async fn enable(home: &Path, source: &Source) -> Result<Installed> {
    let Some(key) = PUBKEY else {
        return Err(other(
            "this build has no release key (ASC_RELEASE_PUBKEY); it cannot verify the helper",
        ));
    };
    install_with(home, source, &|bytes, sig| {
        verify_signed(bytes, sig, key, MANIFEST_NAME)
    })
    .await
}

/// The install, with the signature check supplied.
pub async fn install_with(
    home: &Path,
    source: &Source,
    verify: &dyn Fn(&[u8], &str) -> Result<()>,
) -> Result<Installed> {
    let manifest_bytes = source.small(MANIFEST_NAME, MAX_MANIFEST).await?;
    let sig = source
        .small(&format!("{MANIFEST_NAME}.minisig"), MAX_SIG)
        .await?;
    let sig = String::from_utf8(sig).map_err(|_| other("the signature file is not text"))?;
    verify(&manifest_bytes, &sig)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| other(format!("bad manifest: {e}")))?;
    manifest.check()?;
    let helper = manifest
        .helpers
        .get(TARGET)
        .ok_or_else(|| other(format!("the manifest has no helper for {TARGET}")))?;

    let final_dir = embed_dir(home);
    let stage = home.join(format!("embed.new-{}", std::process::id()));
    let old = home.join("embed.old");
    for p in [&stage, &old] {
        if std::fs::symlink_metadata(p).is_ok() {
            if !is_plain_dir(p) {
                return Err(other("a stale embed directory is not a real directory"));
            }
            std::fs::remove_dir_all(p)?;
        }
    }
    ensure_dir(home)?;
    std::fs::create_dir(&stage)?;
    let done = async {
        std::fs::create_dir(stage.join("model"))?;
        source
            .file(helper, &stage.join(HELPER_FILE), MAX_HELPER)
            .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                stage.join(HELPER_FILE),
                std::fs::Permissions::from_mode(0o755),
            )?;
        }
        for (name, a) in &manifest.model_files {
            let max = if name == "model.onnx" {
                MAX_ONNX
            } else {
                MAX_MODEL_SMALL
            };
            source.file(a, &stage.join("model").join(name), max).await?;
        }
        source
            .file(
                &manifest.calibration,
                &stage.join("calibration.bin"),
                MAX_CALIB,
            )
            .await?;
        let calib = read_capped(&stage.join("calibration.bin"), MAX_CALIB)?;
        Table::from_bytes(&calib)?;
        let rec = InstallRec {
            model: MODEL.to_owned(),
            onnx_sha256: manifest.model_files["model.onnx"].sha256.clone(),
            calibration_blake3: blake3::hash(&calib).to_hex().to_string(),
            helper_sha256: helper.sha256.clone(),
            manifest_sha256: hex(&Sha256::digest(&manifest_bytes)),
        };
        write_atomic(
            &stage.join("install.json"),
            &serde_json::to_vec(&rec).map_err(other)?,
        )?;
        Ok::<(), LocalError>(())
    }
    .await;
    if let Err(e) = done {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    let swap = || -> Result<()> {
        if std::fs::symlink_metadata(&final_dir).is_ok() {
            if !is_plain_dir(&final_dir) {
                return Err(other("the embed directory is not a real directory"));
            }
            std::fs::rename(&final_dir, &old)?;
        }
        std::fs::rename(&stage, &final_dir)?;
        Ok(())
    };
    if let Err(e) = swap() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&old);
    Installed::load(home)?.ok_or_else(|| other("the installation vanished"))
}

// ------------------------------------------------------------------------- the helper process

/// Something that turns texts into float vectors.
pub trait Embedder {
    #[allow(async_fn_in_trait)]
    async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Reply {
    Hello {
        version: u32,
        model: String,
        dims: usize,
    },
    Vectors {
        vectors: Vec<Vec<f32>>,
    },
    Error {
        message: String,
    },
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<()> {
    if body.len() > MAX_FRAME {
        return Err(other("request frame too large"));
    }
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await?;
    Ok(())
}

async fn read_reply<R: AsyncRead + Unpin>(r: &mut R) -> Result<Reply> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head)
        .await
        .map_err(|_| other("the embed helper closed its output"))?;
    let len = u32::from_le_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(other(format!(
            "helper frame of {len} bytes exceeds the bound"
        )));
    }
    let mut body = Vec::new();
    (&mut *r).take(len as u64).read_to_end(&mut body).await?;
    if body.len() != len {
        return Err(other("truncated helper frame"));
    }
    serde_json::from_slice(&body).map_err(|e| other(format!("bad helper frame: {e}")))
}

/// A connection to a running helper.
pub struct Helper<R = ChildStdout, W = ChildStdin> {
    r: R,
    w: W,
    dims: usize,
    last_used: Instant,
    child: Option<Child>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Helper<R, W> {
    /// Greets the helper and checks that it serves the expected model and `dims`.
    pub async fn connect(r: R, w: W, dims: usize, child: Option<Child>) -> Result<Self> {
        let mut h = Self {
            r,
            w,
            dims,
            last_used: Instant::now(),
            child,
        };
        let hello = serde_json::to_vec(&serde_json::json!({"op": "hello"})).map_err(other)?;
        match h.call(&hello).await? {
            Reply::Hello {
                version,
                model,
                dims: got,
            } if version == PROTOCOL && model == MODEL && got == dims => Ok(h),
            Reply::Hello { .. } => Err(other("the embed helper serves another model or protocol")),
            Reply::Error { message } => Err(other(format!("embed helper: {message}"))),
            Reply::Vectors { .. } => Err(other("unexpected helper reply")),
        }
    }

    async fn call(&mut self, body: &[u8]) -> Result<Reply> {
        let r = &mut self.r;
        let w = &mut self.w;
        let out = tokio::time::timeout(REQUEST_TIMEOUT, async {
            write_frame(w, body).await?;
            read_reply(r).await
        })
        .await
        .map_err(|_| other("the embed helper did not answer in time"))?;
        self.last_used = Instant::now();
        out
    }

    /// Time since the helper last answered.
    pub fn idle(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Asks the helper to stop and reaps it.
    pub async fn close(mut self) {
        if let Ok(b) = serde_json::to_vec(&serde_json::json!({"op": "shutdown"})) {
            let _ = write_frame(&mut self.w, &b).await;
        }
        if let Some(mut c) = self.child.take()
            && tokio::time::timeout(Duration::from_secs(5), c.wait())
                .await
                .is_err()
        {
            let _ = c.kill().await;
        }
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Embedder for Helper<R, W> {
    async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if texts.len() > BATCH {
            return Err(other("too many texts in one request"));
        }
        let body = serde_json::to_vec(&serde_json::json!({"op": "embed", "texts": texts}))
            .map_err(other)?;
        match self.call(&body).await? {
            Reply::Vectors { vectors } => {
                if vectors.len() != texts.len()
                    || vectors
                        .iter()
                        .any(|v| v.len() != self.dims || v.iter().any(|x| !x.is_finite()))
                {
                    return Err(other("the embed helper returned malformed vectors"));
                }
                Ok(vectors)
            }
            Reply::Error { message } => Err(other(format!("embed helper: {message}"))),
            Reply::Hello { .. } => Err(other("unexpected helper reply")),
        }
    }
}

impl Helper {
    /// Starts the installed helper (low priority, no console). The helper binary is hashed
    /// first, so a file that changed since the install is refused.
    pub async fn spawn(inst: &Installed) -> Result<Self> {
        if sha256_file(&inst.helper_path())? != inst.rec.helper_sha256 {
            return Err(other("the embed helper changed since it was verified"));
        }
        let mut cmd = Command::new(inst.helper_path());
        cmd.arg(MODEL)
            .arg(inst.model_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0000_0040 | 0x0800_0000); // IDLE_PRIORITY_CLASS | CREATE_NO_WINDOW
        let mut child = cmd.spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(other("the embed helper has no stdio"));
        };
        Helper::connect(stdout, stdin, inst.table.dims(), Some(child)).await
    }
}

// ------------------------------------------------------------------------- the vector files

#[derive(Serialize, Deserialize)]
struct Done {
    model_id: String,
    ords: Vec<(u32, u32)>,
}

fn done_path(dir: &Path) -> PathBuf {
    dir.join("done.json")
}

fn load_done(dir: &Path, model: &ModelRef) -> RoaringBitmap {
    let Ok(bytes) = read_capped(&done_path(dir), MAX_STATE) else {
        return RoaringBitmap::new();
    };
    match serde_json::from_slice::<Done>(&bytes) {
        Ok(d)
            if d.model_id == model.model_id
                && d.ords.len() <= MAX_TRACKED
                && d.ords.iter().all(|&(lo, hi)| lo <= hi) =>
        {
            SegRec {
                bytes: 0,
                ords: d.ords,
            }
            .bitmap()
        }
        _ => RoaringBitmap::new(),
    }
}

fn save_done(dir: &Path, model: &ModelRef, done: &RoaringBitmap) -> Result<()> {
    let rec = SegRec::from_bitmap(done);
    let body = serde_json::to_vec(&Done {
        model_id: model.model_id.clone(),
        ords: rec.ords,
    })
    .map_err(other)?;
    write_atomic(&done_path(dir), &body)
}

/// The generation directory, created, after the other generations were dropped.
fn gen_dir(store: &RepoStore, model: &ModelRef) -> Result<PathBuf> {
    drop_stale_generations(store.path(), model.model_gen).map_err(vec_err)?;
    let vec_dir = store.path().join("vec");
    ensure_dir(&vec_dir)?;
    let dir = generation_dir(store.path(), model.model_gen);
    ensure_dir(&dir)?;
    Ok(dir)
}

/// The `*.ascvl` files of a generation directory, within the file count bound.
fn vector_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir)?; // lint:allow-worktree-fs: index dir under $ASC_HOME
    for e in rd {
        let e = e?;
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("ascvl") {
            continue;
        }
        if out.len() >= MAX_VEC_FILES {
            return Err(other("too many local vector files"));
        }
        out.push(p);
    }
    out.sort();
    Ok(out)
}

/// What one embedding run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Embedded {
    /// Files newly covered by vectors.
    pub files: u64,
    /// Chunks embedded.
    pub chunks: u64,
    /// Files of the revision still without vectors (the run hit its bound).
    pub remaining: u64,
}

/// Embeds the chunks of the files of `rev` that have no vectors yet.
///
/// Secret-flagged and skipped files are never sent to a model. A vector file is written and
/// only then are its files recorded as done, so a crash repeats work and never loses it.
#[allow(clippy::too_many_arguments)]
pub async fn embed_rev<E: Embedder>(
    ci: &CiRoot,
    store: &RepoStore,
    lock: Option<&Lock>,
    rev: &RevRec,
    objects: &dyn Objects,
    inst: &Installed,
    embedder: &mut E,
    cap: u64,
) -> Result<Embedded> {
    let model = inst.model_ref();
    let dir = gen_dir(store, &model)?;
    let mut done = load_done(&dir, &model);
    let st = store.state()?;
    let by_ord = st.by_ord();
    let dims = inst.table.dims();

    let mut todo: Vec<u32> = rev
        .files
        .iter()
        .map(|&(_, o)| o)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|o| !done.contains(*o))
        .collect();
    todo.retain(|o| {
        by_ord
            .get(o)
            .and_then(|id| st.files.get(*id))
            .is_some_and(|r| {
                Flags::from_bits(r.flags)
                    .is_some_and(|f| f.indexable() && !f.contains(Flags::SECRET))
            })
    });
    // Ords the revision lists but that are skipped or secret need no vectors; they count as done
    // so that they are not looked at again.
    let skippable: Vec<u32> = rev
        .files
        .iter()
        .map(|&(_, o)| o)
        .filter(|o| !done.contains(*o) && !todo.contains(o))
        .collect();
    for o in skippable {
        done.insert(o);
    }

    let mut out = Embedded::default();
    let mut builder = SegmentBuilder::new(model.clone(), dims).map_err(vec_err)?;
    let mut unflushed: Vec<u32> = Vec::new();
    let mut cache: HashMap<[u8; 32], Vec<i8>> = HashMap::new();
    let flush = |builder: &mut SegmentBuilder,
                 unflushed: &mut Vec<u32>,
                 done: &mut RoaringBitmap|
     -> Result<()> {
        let b = std::mem::replace(
            builder,
            SegmentBuilder::new(model.clone(), dims).map_err(vec_err)?,
        );
        if !b.is_empty() {
            let seg = b.finish().map_err(vec_err)?;
            let bytes = LocalVectors::from_segment(&seg).encode().map_err(vec_err)?;
            if ci.size().saturating_add(bytes.len() as u64) > cap {
                return Err(other(
                    "the local vectors would exceed codeintel.local.maxSize",
                ));
            }
            let name = format!("{}.ascvl", blake3::hash(&bytes).to_hex());
            write_atomic(&dir.join(name), &bytes)?;
        }
        done.extend(unflushed.drain(..));
        save_done(&dir, &model, done)
    };

    let mut at = 0usize;
    while at < todo.len() {
        if out.chunks as usize >= MAX_RUN_CHUNKS {
            break;
        }
        let ord = todo[at];
        at += 1;
        if let Some(l) = lock {
            l.refresh();
        }
        let Some(file) = by_ord
            .get(&ord)
            .and_then(|id| Some((unhex32(id)?, st.files.get(*id)?)))
        else {
            continue;
        };
        let id = FileId::from_bytes(file.0);
        let bytes = match objects.get(&ObjKey::File(id)).await {
            Ok(Some(b))
                if FileId::of(&b) == id
                    && b.len() as u64 == file.1.size
                    && b.len() <= skip::MAX_BYTES =>
            {
                b
            }
            // Not on this machine or not the content the table names: nothing to embed, and
            // nothing is fetched.
            _ => continue,
        };
        let lang = file.1.lang.as_deref().and_then(Lang::from_name);
        let chunks = chunk::chunk(lang, &bytes).unwrap_or_default();
        let mut keyed: Vec<(u64, Vec<i8>)> = Vec::with_capacity(chunks.len());
        let mut batch: Vec<usize> = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            if let Some(v) = cache.get(&c.id.0) {
                keyed.push((chunk_key(ord, i as u32), v.clone()));
            } else {
                batch.push(i);
            }
        }
        for part in batch.chunks(BATCH) {
            let texts: Vec<String> = part.iter().map(|&i| chunks[i].embed_text.clone()).collect();
            let floats = embedder.embed(&texts).await?;
            if floats.len() != texts.len() {
                return Err(other(
                    "the embed helper returned the wrong number of vectors",
                ));
            }
            for (&i, f) in part.iter().zip(&floats) {
                let q = inst.table.quantise(f)?;
                if cache.len() >= CACHE_ENTRIES {
                    cache.clear();
                }
                cache.insert(chunks[i].id.0, q.clone());
                keyed.push((chunk_key(ord, i as u32), q));
            }
            out.chunks += part.len() as u64;
        }
        for (k, v) in keyed {
            builder.add(k, &v).map_err(vec_err)?;
        }
        unflushed.push(ord);
        out.files += 1;
        if builder.len() >= FLUSH_CHUNKS {
            flush(&mut builder, &mut unflushed, &mut done)?;
        }
    }
    flush(&mut builder, &mut unflushed, &mut done)?;
    out.remaining = todo
        .iter()
        .filter(|o| !done.contains(**o))
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    Ok(out)
}

/// Semantic search over the local vectors: embeds `query`, prefilters with b1 Hamming distance
/// and rescores the best 200 with i8 cosine, over the chunks of the docs in `kept`.
pub async fn search<E: Embedder>(
    store: &RepoStore,
    inst: &Installed,
    embedder: &mut E,
    query: &str,
    k: usize,
    kept: &RoaringBitmap,
) -> Result<Vec<Hit>> {
    if query.is_empty() || query.len() > MAX_QUERY || k == 0 || k > MAX_K {
        return Err(other("bad semantic query"));
    }
    let model = inst.model_ref();
    let dir = generation_dir(store.path(), model.model_gen);
    if !is_plain_dir(&dir) {
        return Ok(Vec::new());
    }
    let files = vector_files(&dir)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mut v = embedder.embed(&[query.to_owned()]).await?;
    let q = inst
        .table
        .quantise(&v.pop().ok_or_else(|| other("no query vector"))?)?;
    let mut best: HashMap<u64, f32> = HashMap::new();
    for f in files {
        // A file of another generation or model is skipped; a damaged file is skipped too, so
        // one bad file does not take the whole search down.
        let Ok(Some(lv)) = load_local(store.path(), &f, &model) else {
            continue;
        };
        for h in lv.search(&model, &q, k, kept).map_err(vec_err)? {
            let e = best.entry(h.key).or_insert(h.score);
            if h.score > *e {
                *e = h.score;
            }
        }
    }
    let mut hits: Vec<Hit> = best
        .into_iter()
        .map(|(key, score)| Hit { key, score })
        .collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.key.cmp(&b.key)));
    hits.truncate(k);
    Ok(hits)
}

// ------------------------------------------------------------------------- the worker's use

/// The worker's embedding state: the helper, started on first use and closed when idle.
pub struct Session {
    on: bool,
    inst: Option<Installed>,
    helper: Option<Helper>,
    warned: bool,
}

impl Session {
    pub fn new(on: bool) -> Self {
        Self {
            on,
            inst: None,
            helper: None,
            warned: false,
        }
    }

    /// Embeds the new chunks of `rev`. Does nothing unless `codeintel.local.embed = true`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &mut self,
        home: &Path,
        ci: &CiRoot,
        store: &RepoStore,
        lock: Option<&Lock>,
        rev: &RevRec,
        objects: &dyn Objects,
        cap: u64,
    ) -> Result<Option<Embedded>> {
        if !self.on {
            return Ok(None);
        }
        if self.inst.is_none() {
            match Installed::load(home)? {
                Some(i) => self.inst = Some(i),
                None => {
                    if !self.warned {
                        self.warned = true;
                        return Err(other(
                            "codeintel.local.embed is set but the helper is not installed; run `asc codeintel enable-embed`",
                        ));
                    }
                    return Ok(None);
                }
            }
        }
        let Some(inst) = self.inst.clone() else {
            return Ok(None);
        };
        if self.helper.is_none() {
            self.helper = Some(Helper::spawn(&inst).await?);
        }
        let Some(h) = self.helper.as_mut() else {
            return Ok(None);
        };
        let r = embed_rev(ci, store, lock, rev, objects, &inst, h, cap).await;
        if r.is_err() {
            // The helper may be wedged; the next run starts a fresh one.
            if let Some(h) = self.helper.take() {
                h.close().await;
            }
        }
        r.map(Some)
    }

    /// Closes the helper after [`EXIT_AFTER`] without work.
    pub async fn reap(&mut self) {
        if self.helper.as_ref().is_some_and(|h| h.idle() >= EXIT_AFTER)
            && let Some(h) = self.helper.take()
        {
            h.close().await;
        }
    }
}

/// `asc codeintel enable-embed`.
pub async fn enable_command(home: &Path, source: &Source) -> Result<String> {
    let inst = enable(home, source).await?;
    let m = inst.model_ref();
    Ok(format!(
        "embed helper installed: model {} generation {}\nset `codeintel.local.embed = true` to build local vectors",
        inst.rec.model, m.model_gen
    ))
}

#[cfg(test)]
mod tests {
    use super::super::build::{Entry, Job, index_rev};
    use super::*;
    use asc_store::cachedir::CacheDir;
    use asc_store::codec::encode_stored;
    use asc_store::storeset::StoreSet;
    use std::sync::Arc;

    const DIMS: usize = 64;
    const REPO: &str = "00000000-0000-4000-8000-0000000000bb";
    // minisign-verify's own vector: key, a legacy signature over b"test".
    const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const SIG: &str = "untrusted comment: signature from minisign secret key\nRWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\ntrusted comment: timestamp:1555779966\tfile:test\nQtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";

    fn table_bytes(lo: f32, hi: f32) -> Vec<u8> {
        let mut b = (DIMS as u32).to_le_bytes().to_vec();
        for _ in 0..DIMS {
            b.extend_from_slice(&lo.to_le_bytes());
        }
        for _ in 0..DIMS {
            b.extend_from_slice(&hi.to_le_bytes());
        }
        b
    }

    fn sha(b: &[u8]) -> String {
        hex(&Sha256::digest(b))
    }

    /// Writes a release directory and returns its source.
    fn release(dir: &Path, onnx: &[u8], calib: &[u8]) -> Source {
        let e = dir.join("embed");
        std::fs::create_dir_all(&e).unwrap();
        let put = |name: &str, b: &[u8]| {
            std::fs::write(e.join(name), b).unwrap();
            serde_json::json!({"file": name, "sha256": sha(b), "size": b.len()})
        };
        let helper = put("helper.bin", b"#!fake helper");
        let mut files = serde_json::Map::new();
        for n in MODEL_FILES {
            let body: &[u8] = if n == "model.onnx" {
                onnx
            } else {
                n.as_bytes()
            };
            files.insert(n.to_owned(), put(&format!("m-{n}"), body));
        }
        let calibration = put("calibration.bin", calib);
        let manifest = serde_json::json!({
            "version": 1,
            "model": MODEL,
            "helpers": { TARGET: helper },
            "model_files": files,
            "calibration": calibration,
        });
        std::fs::write(e.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(e.join("manifest.json.minisig"), b"sig").unwrap();
        Source::Dir(dir.to_path_buf())
    }

    fn accept(_: &[u8], _: &str) -> Result<()> {
        Ok(())
    }

    async fn installed(home: &Path, rel: &Path, onnx: &[u8]) -> Installed {
        let src = release(rel, onnx, &table_bytes(0.0, 1.0));
        install_with(home, &src, &accept).await.unwrap()
    }

    #[test]
    fn the_table_quantises_and_refuses_bad_bytes() {
        let t = Table::from_bytes(&table_bytes(0.0, 1.0)).unwrap();
        let mut v = vec![0.5f32; DIMS];
        v[0] = 0.0;
        v[1] = 1.0;
        v[2] = 9.0;
        v[3] = f32::NAN;
        let q = t.quantise(&v).unwrap();
        assert_eq!(&q[..4], &[-127, 127, 127, 0]);
        assert!(t.quantise(&v[1..]).is_err());
        assert!(Table::from_bytes(&table_bytes(1.0, 0.0)).is_err());
        assert!(Table::from_bytes(&table_bytes(f32::NAN, 1.0)).is_err());
        assert!(Table::from_bytes(&table_bytes(0.0, 1.0)[1..]).is_err());
        assert!(Table::from_bytes(&0u32.to_le_bytes()).is_err());
        let mut huge = (u32::MAX).to_le_bytes().to_vec();
        huge.extend_from_slice(&[0; 16]);
        assert!(Table::from_bytes(&huge).is_err());
    }

    #[test]
    fn signatures_must_match_bytes_key_and_file_name() {
        // The vector is a legacy signature, which the production check refuses.
        assert!(verify_signed(b"test", SIG, KEY, "test").is_err());
        assert!(verify_signed(b"test", "junk", KEY, "test").is_err());
        assert!(verify_signed(b"test", SIG, "junk", "test").is_err());
    }

    #[tokio::test]
    async fn install_verifies_every_file_and_swaps_atomically() {
        let t = tempfile::tempdir().unwrap();
        let (home, rel) = (t.path().join("home"), t.path().join("rel"));
        let inst = installed(&home, &rel, b"onnx-1").await;
        assert_eq!(inst.rec.onnx_sha256, sha(b"onnx-1"));
        assert_eq!(inst.table.dims(), DIMS);
        assert!(Installed::load(&home).unwrap().is_some());
        let a = inst.model_ref();
        assert!(
            a.model_id
                .starts_with(&format!("{MODEL}@{}/i8/calib@", sha(b"onnx-1")))
        );
        assert!(a.model_id.ends_with(&format!("/{CHUNKER_VERSION}")));

        // A new model is a new generation and replaces the install.
        let inst2 = installed(&home, &t.path().join("rel2"), b"onnx-2").await;
        assert_ne!(inst2.model_ref().model_gen, a.model_gen);
        assert_eq!(
            Installed::load(&home).unwrap().unwrap().rec.onnx_sha256,
            sha(b"onnx-2")
        );
        assert!(!home.join("embed.old").exists());
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_tampered_or_unsigned_release_installs_nothing() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let rel = t.path().join("rel");
        let src = release(&rel, b"onnx", &table_bytes(0.0, 1.0));
        // The signature check refuses.
        let no = |_: &[u8], _: &str| Err(other("bad signature"));
        assert!(install_with(&home, &src, &no).await.is_err());
        assert!(!home.join("embed").exists());
        // A file that no longer matches the signed manifest.
        std::fs::write(rel.join("embed/m-model.onnx"), b"evil").unwrap();
        let e = install_with(&home, &src, &accept).await.unwrap_err();
        assert!(e.to_string().contains("signed manifest"), "{e}");
        assert!(!home.join("embed").exists());
        assert!(
            std::fs::read_dir(&home).unwrap().next().is_none(),
            "staging is removed"
        );
    }

    #[tokio::test]
    async fn a_manifest_cannot_name_paths_or_extra_files() {
        let t = tempfile::tempdir().unwrap();
        let rel = t.path().join("rel");
        let src = release(&rel, b"onnx", &table_bytes(0.0, 1.0));
        let mp = rel.join("embed/manifest.json");
        let good = std::fs::read_to_string(&mp).unwrap();
        for (from, to) in [
            ("m-model.onnx\"", "../x\""),
            ("\"version\":1", "\"version\":2"),
            ("\"config.json\":", "\"extra.json\":"),
        ] {
            assert!(good.contains(from), "{from}");
            std::fs::write(&mp, good.replacen(from, to, 1)).unwrap();
            let home = t.path().join("home");
            assert!(install_with(&home, &src, &accept).await.is_err(), "{to}");
            assert!(!home.join("embed").exists());
        }
        // The size is bounded.
        let big = good.replacen("\"size\":", "\"size\":99999999999,\"x\":", 1);
        std::fs::write(&mp, big).unwrap();
        assert!(
            install_with(&t.path().join("h2"), &src, &accept)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_changed_calibration_is_noticed() {
        let t = tempfile::tempdir().unwrap();
        let (home, rel) = (t.path().join("home"), t.path().join("rel"));
        installed(&home, &rel, b"onnx").await;
        let mut bytes = std::fs::read(home.join("embed/calibration.bin")).unwrap();
        bytes[8] ^= 1;
        std::fs::write(home.join("embed/calibration.bin"), bytes).unwrap();
        assert!(Installed::load(&home).is_err());
    }

    fn embed_words(text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; DIMS];
        for w in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let h = blake3::hash(w.as_bytes());
            v[h.as_bytes()[0] as usize % DIMS] = 1.0;
        }
        v
    }

    #[derive(Default)]
    struct Fake {
        calls: usize,
        texts: Vec<String>,
    }

    impl Embedder for Fake {
        async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            assert!(texts.len() <= BATCH);
            self.calls += 1;
            self.texts.extend_from_slice(texts);
            Ok(texts.iter().map(|t| embed_words(t)).collect())
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        ci: CiRoot,
        store: RepoStore,
        objects: Arc<StoreSet>,
        home: PathBuf,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let objects = Arc::new(StoreSet {
            store: CacheDir::open(dir.path().join("store"), dir.path().join("tmp")).unwrap(),
            cache: CacheDir::open(dir.path().join("cache"), dir.path().join("tmp")).unwrap(),
        });
        let ci = CiRoot::new(dir.path());
        let store = ci.repo(REPO).unwrap();
        Rig {
            home: dir.path().to_path_buf(),
            _dir: dir,
            ci,
            store,
            objects,
        }
    }

    async fn index(r: &Rig, files: &[(&str, &str)]) -> RevRec {
        let mut entries = Vec::new();
        for (p, c) in files {
            let id = FileId::of(c.as_bytes());
            r.objects
                .put(&ObjKey::File(id), encode_stored(c.as_bytes()))
                .await
                .unwrap();
            entries.push(Entry {
                path: (*p).to_owned(),
                id,
                size: c.len() as u64,
            });
        }
        index_rev(Job {
            ci: &r.ci,
            store: &r.store,
            lock: None,
            cap: 64 << 20,
            changeset: [5; 32],
            git: None,
            entries,
            extra_missing: 0,
            objects: &*r.objects,
            threads: 2,
        })
        .await
        .unwrap()
        .rev
    }

    fn ord_of(rev: &RevRec, path: &str) -> u32 {
        rev.files.iter().find(|(p, _)| p == path).unwrap().1
    }

    async fn run(
        r: &Rig,
        rev: &RevRec,
        inst: &Installed,
        f: &mut Fake,
        cap: u64,
    ) -> Result<Embedded> {
        embed_rev(&r.ci, &r.store, None, rev, &*r.objects, inst, f, cap).await
    }

    #[tokio::test]
    async fn vectors_are_built_once_searched_and_skip_secrets() {
        let r = rig();
        let inst = installed(&r.home.join("h"), &r.home.join("rel"), b"onnx").await;
        let rev = index(
            &r,
            &[
                ("fruit.txt", "apple banana cherry\n"),
                ("animals.txt", "zebra yak walrus\n"),
                (".env", "TOKEN=apple\n"),
            ],
        )
        .await;
        let mut fake = Fake::default();
        let a = run(&r, &rev, &inst, &mut fake, 64 << 20).await.unwrap();
        assert_eq!((a.files, a.remaining), (2, 0));
        assert!(a.chunks >= 2);
        assert!(
            fake.texts.iter().all(|t| !t.contains("TOKEN")),
            "a secret file is never sent to the model"
        );

        let model = inst.model_ref();
        let dir = generation_dir(r.store.path(), model.model_gen);
        let files = vector_files(&dir).unwrap();
        assert_eq!(files.len(), 1);
        let bytes = std::fs::read(&files[0]).unwrap();
        assert_eq!(&bytes[..4], b"ASVL");
        // The header holds the model id; another model reads nothing from it.
        assert!(
            bytes
                .windows(model.model_id.len())
                .any(|w| w == model.model_id.as_bytes())
        );
        let other_model = ModelRef {
            model_id: format!("{}x", model.model_id),
            ..model.clone()
        };
        assert!(
            load_local(r.store.path(), &files[0], &other_model)
                .unwrap()
                .is_none()
        );

        // Nothing new: no helper call, no new file.
        let calls = fake.calls;
        let b = run(&r, &rev, &inst, &mut fake, 64 << 20).await.unwrap();
        assert_eq!((b.files, b.chunks, fake.calls), (0, 0, calls));
        assert_eq!(vector_files(&dir).unwrap().len(), 1);

        let kept: RoaringBitmap = rev.files.iter().map(|&(_, o)| o).collect();
        let hits = search(&r.store, &inst, &mut fake, "apple", 3, &kept)
            .await
            .unwrap();
        assert_eq!(
            vectors::doc_ord_of(hits[0].key),
            ord_of(&rev, "fruit.txt"),
            "{hits:?}"
        );
        // The scope filter holds.
        let only: RoaringBitmap = [ord_of(&rev, "animals.txt")].into_iter().collect();
        let hits = search(&r.store, &inst, &mut fake, "apple", 3, &only)
            .await
            .unwrap();
        assert!(
            hits.iter()
                .all(|h| vectors::doc_ord_of(h.key) == ord_of(&rev, "animals.txt"))
        );
        assert!(
            search(&r.store, &inst, &mut fake, "", 3, &kept)
                .await
                .is_err()
        );
        assert!(
            search(&r.store, &inst, &mut fake, "x", 0, &kept)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_new_generation_drops_the_old_vectors_and_rebuilds() {
        let r = rig();
        let rev = index(&r, &[("a.txt", "apple banana\n")]).await;
        let home = r.home.join("h");
        let one = installed(&home, &r.home.join("rel1"), b"onnx-1").await;
        let mut fake = Fake::default();
        run(&r, &rev, &one, &mut fake, 64 << 20).await.unwrap();
        let old = generation_dir(r.store.path(), one.model_ref().model_gen);
        assert!(old.is_dir());

        let two = installed(&home, &r.home.join("rel2"), b"onnx-2").await;
        let b = run(&r, &rev, &two, &mut fake, 64 << 20).await.unwrap();
        assert_eq!(b.files, 1, "rebuilt, not migrated");
        assert!(!old.exists(), "the old generation is gone");
        assert!(generation_dir(r.store.path(), two.model_ref().model_gen).is_dir());
    }

    #[tokio::test]
    async fn the_size_cap_stops_the_vectors() {
        let r = rig();
        let inst = installed(&r.home.join("h"), &r.home.join("rel"), b"onnx").await;
        let rev = index(&r, &[("a.txt", "apple banana\n")]).await;
        let mut fake = Fake::default();
        let e = run(&r, &rev, &inst, &mut fake, 1).await.unwrap_err();
        assert!(e.to_string().contains("maxSize"), "{e}");
    }

    /// Serves the helper protocol on one end of a duplex pipe.
    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        mut s: S,
        model: &str,
        dims: usize,
        embed_dims: usize,
    ) {
        loop {
            let mut head = [0u8; 4];
            if s.read_exact(&mut head).await.is_err() {
                return;
            }
            let mut body = vec![0u8; u32::from_le_bytes(head) as usize];
            s.read_exact(&mut body).await.unwrap();
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let reply = match req["op"].as_str().unwrap() {
                "hello" => serde_json::json!({"op":"hello","version":1,"model":model,"dims":dims}),
                "embed" => {
                    let n = req["texts"].as_array().unwrap().len();
                    serde_json::json!({"op":"vectors","vectors":vec![vec![0.5; embed_dims]; n]})
                }
                _ => return,
            };
            let b = serde_json::to_vec(&reply).unwrap();
            s.write_all(&(b.len() as u32).to_le_bytes()).await.unwrap();
            s.write_all(&b).await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_helper_protocol_is_validated() {
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(b, MODEL, 4, 4));
        let (r, w) = tokio::io::split(a);
        let mut h = Helper::connect(r, w, 4, None).await.unwrap();
        let v = h.embed(&["x".into(), "y".into()]).await.unwrap();
        assert_eq!(v.len(), 2);
        assert!(h.embed(&vec!["x".to_owned(); BATCH + 1]).await.is_err());

        // Wrong model, wrong dimensions in hello, wrong dimensions in vectors.
        for (model, dims, ed, ok_connect) in [
            ("other", 4, 4, false),
            (MODEL, 5, 4, false),
            (MODEL, 4, 3, true),
        ] {
            let (a, b) = tokio::io::duplex(1 << 16);
            tokio::spawn(serve(b, model, dims, ed));
            let (r, w) = tokio::io::split(a);
            match Helper::connect(r, w, 4, None).await {
                Ok(mut h) => {
                    assert!(ok_connect);
                    assert!(h.embed(&["x".into()]).await.is_err());
                }
                Err(_) => assert!(!ok_connect),
            }
        }
    }

    #[tokio::test]
    async fn an_oversized_or_truncated_frame_is_refused() {
        let (a, mut b) = tokio::io::duplex(1 << 16);
        let (r, w) = tokio::io::split(a);
        let t = tokio::spawn(async move {
            let mut head = [0u8; 4];
            b.read_exact(&mut head).await.unwrap();
            let mut body = vec![0u8; u32::from_le_bytes(head) as usize];
            b.read_exact(&mut body).await.unwrap();
            b.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes())
                .await
                .unwrap();
        });
        let e = Helper::connect(r, w, 4, None).await.err().unwrap();
        assert!(e.to_string().contains("exceeds"), "{e}");
        t.await.unwrap();

        let (a, b) = tokio::io::duplex(1 << 16);
        let (r, w) = tokio::io::split(a);
        drop(b);
        assert!(Helper::connect(r, w, 4, None).await.is_err());
    }

    #[test]
    fn the_source_is_https_and_names_are_plain() {
        assert!(Source::url("http://example.com").is_err());
        assert!(Source::url("https://example.com/r/").is_ok());
        assert!(plain_name("a-b_c.1"));
        for bad in ["", ".x", "a/b", "a\\b", "..", "a b"] {
            assert!(!plain_name(bad), "{bad}");
        }
    }
}
