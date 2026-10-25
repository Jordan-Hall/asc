//! The server's embedding model: the `asc-embed` helper process (SPEC §23.6, §23.7).
//!
//! `asc-serve` never links ONNX Runtime (the `asc` binary embeds this crate, and `asc.exe` must not
//! load a model, SPEC §15.2): the model runs in the separate `asc-embed` helper, which this module
//! spawns on first use and talks to with the helper's length-prefixed JSON frames. The helper
//! checks every model file against the sha256 its registry pins; this side checks the
//! calibration table against its own digest and reads the ONNX file's sha256 for the model id
//! (`name@<onnx sha256>/i8/calib@<calibration blake3>/<chunker_ver>`, SPEC §23.7), so a changed
//! model or table is a new model generation.
//!
//! One directory holds everything, in the layout `asc codeintel enable-embed` installs:
//!
//! ```text
//! <dir>/asc-embed[.exe]        the helper
//! <dir>/model/                model.onnx, tokenizer.json, config.json,
//!                             special_tokens_map.json, tokenizer_config.json
//! <dir>/calibration.bin       the i8 quantisation table (u32 LE dims, mins, maxes, f32 LE)
//! ```
//!
//! `[codeintel.embed] dir` names it. Without it the server has no embedder, which is a normal,
//! documented state: `Semantic` answers `FAILED_PRECONDITION` with [`NOT_CONFIGURED`], and the
//! other methods are unaffected. A configured directory that does not load is logged and leaves
//! the server in the same state.
//!
//! Failure handling: the helper is one process; calls are serialised. A call that errors or does
//! not answer within [`REQUEST_TIMEOUT`] kills the helper, and the next call starts a new one.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asc_codeintel::chunk::CHUNKER_VERSION;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::modelgen::ModelSpec;
use super::worker::EmbedModel;
use crate::config::EmbedConfig;

/// The `FAILED_PRECONDITION` message of `Semantic` on a server without an embedder.
pub const NOT_CONFIGURED: &str = "semantic search needs an embedding model and this server has \
none configured: install the asc-embed helper, the model files and calibration.bin into one \
directory (the layout `asc codeintel enable-embed` installs: asc-embed, model/, calibration.bin), \
set `[codeintel.embed] dir = \"<that directory>\"` in asc-serve.toml and restart the server. \
Search, symbols and status work without it";

/// The helper's frame protocol version (`asc_embed::protocol::VERSION`).
const PROTOCOL: u32 = 1;
/// Largest frame either side sends or accepts.
const MAX_FRAME: usize = 16 << 20;
/// Texts in one `embed` request (the helper accepts 256).
const BATCH: usize = 32;
/// How long the helper may take to answer one request, its start included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest calibration table: `u32` dims, then mins and maxes of up to 4096 dims.
const MAX_CALIB: u64 = 4 + 4096 * 8;
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

/// Where the embedder lives, from `[codeintel.embed]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub dir: PathBuf,
    pub model: String,
}

impl Setup {
    /// `None` when no `dir` is set or the provider is not the local helper.
    pub fn from_config(cfg: &EmbedConfig) -> Option<Self> {
        let dir = cfg.dir.clone()?;
        if cfg.provider != "local" {
            tracing::warn!(
                provider = %cfg.provider,
                "[codeintel.embed] provider is not `local`; only the local asc-embed helper is supported, so Semantic stays unavailable"
            );
            return None;
        }
        if cfg.device != "cpu" {
            tracing::warn!(
                device = %cfg.device,
                "[codeintel.embed] device is not `cpu`; the asc-embed helper runs on the CPU"
            );
        }
        Some(Self {
            dir,
            model: cfg.model.clone(),
        })
    }
}

/// Opens the embedder of `setup`, logging why it is unavailable. `None` is the documented
/// state without a model.
pub fn open(setup: Option<&Setup>) -> Option<Arc<dyn EmbedModel>> {
    let setup = setup?;
    match HelperEmbedder::open(setup) {
        Ok(e) => {
            tracing::info!(model = %e.spec.model_id, "semantic search is available");
            Some(Arc::new(e))
        }
        Err(e) => {
            tracing::error!(dir = %setup.dir.display(), error = %e, "the embedding model does not load; Semantic is unavailable");
            None
        }
    }
}

// ------------------------------------------------------------------------- calibration

/// The i8 table: per-dimension min and max (`asc_embed::Calibration`'s encoding).
#[derive(Clone, Debug, PartialEq)]
struct Table {
    min: Vec<f32>,
    max: Vec<f32>,
}

impl Table {
    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let bad = || "bad calibration table".to_owned();
        let head: [u8; 4] = bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(bad)?;
        let dims = u32::from_le_bytes(head) as usize;
        if dims == 0 || dims > 4096 || bytes.len() != 4 + dims * 8 {
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

    /// Maps each dimension's `[min, max]` onto `[-127, 127]`, clamped.
    fn quantise(&self, v: &[f32]) -> Result<Vec<i8>, String> {
        if v.len() != self.min.len() {
            return Err("vector and calibration table differ in dimensions".to_owned());
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

// ------------------------------------------------------------------------- frames

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

/// One frame-speaking connection to a helper.
struct Conn<R, W> {
    r: R,
    w: W,
}

impl<R: Read, W: Write> Conn<R, W> {
    fn call(&mut self, body: &[u8]) -> Result<Reply, String> {
        if body.len() > MAX_FRAME {
            return Err("request frame too large".to_owned());
        }
        let io = |e: std::io::Error| format!("the embed helper: {e}");
        self.w
            .write_all(&(body.len() as u32).to_le_bytes())
            .map_err(io)?;
        self.w.write_all(body).map_err(io)?;
        self.w.flush().map_err(io)?;
        let mut head = [0u8; 4];
        self.r
            .read_exact(&mut head)
            .map_err(|_| "the embed helper closed its output".to_owned())?;
        let len = u32::from_le_bytes(head) as usize;
        if len > MAX_FRAME {
            return Err(format!("helper frame of {len} bytes exceeds the bound"));
        }
        let mut buf = Vec::new();
        (&mut self.r)
            .take(len as u64)
            .read_to_end(&mut buf)
            .map_err(io)?;
        if buf.len() != len {
            return Err("truncated helper frame".to_owned());
        }
        serde_json::from_slice(&buf).map_err(|e| format!("bad helper frame: {e}"))
    }

    /// Greets the helper and checks it serves `model` with `dims` outputs.
    fn hello(&mut self, model: &str, dims: usize) -> Result<(), String> {
        match self.call(br#"{"op":"hello"}"#)? {
            Reply::Hello {
                version,
                model: got,
                dims: d,
            } if version == PROTOCOL && got == model && d == dims => Ok(()),
            Reply::Hello { .. } => Err("the embed helper serves another model or protocol".into()),
            Reply::Error { message } => Err(format!("embed helper: {message}")),
            Reply::Vectors { .. } => Err("unexpected helper reply".to_owned()),
        }
    }

    fn embed(&mut self, texts: &[String], dims: usize) -> Result<Vec<Vec<f32>>, String> {
        let body = serde_json::to_vec(&serde_json::json!({"op": "embed", "texts": texts}))
            .map_err(|e| e.to_string())?;
        match self.call(&body)? {
            Reply::Vectors { vectors }
                if vectors.len() == texts.len() && vectors.iter().all(|v| v.len() == dims) =>
            {
                Ok(vectors)
            }
            Reply::Vectors { .. } => Err("the embed helper answered the wrong shape".to_owned()),
            Reply::Error { message } => Err(format!("embed helper: {message}")),
            Reply::Hello { .. } => Err("unexpected helper reply".to_owned()),
        }
    }
}

// ------------------------------------------------------------------------- the process

struct Proc {
    child: Arc<Mutex<Child>>,
    conn: Conn<ChildStdout, ChildStdin>,
}

impl Proc {
    fn kill(self) {
        let mut c = self.child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// The helper behind [`EmbedModel`].
pub struct HelperEmbedder {
    spec: ModelSpec,
    name: String,
    helper: PathBuf,
    model_dir: PathBuf,
    table: Table,
    proc: Mutex<Option<Proc>>,
}

fn plain_file(p: &Path, max: u64) -> Result<u64, String> {
    let m = std::fs::symlink_metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if !m.is_file() {
        return Err(format!("{} is not a regular file", p.display()));
    }
    if m.len() > max {
        return Err(format!("{} is over {max} bytes", p.display()));
    }
    Ok(m.len())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(p: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

impl HelperEmbedder {
    /// Reads the directory's calibration table and model identity. The helper is not started.
    pub fn open(setup: &Setup) -> Result<Self, String> {
        let dir = &setup.dir;
        let helper = dir.join(HELPER_FILE);
        plain_file(&helper, u64::MAX)?;
        let model_dir = dir.join("model");
        for f in MODEL_FILES {
            plain_file(&model_dir.join(f), u64::MAX)?;
        }
        let calib_path = dir.join("calibration.bin");
        plain_file(&calib_path, MAX_CALIB)?;
        let calib = std::fs::read(&calib_path).map_err(|e| format!("calibration.bin: {e}"))?;
        let table = Table::from_bytes(&calib)?;
        let digest = blake3::hash(&calib);
        let onnx = sha256_file(&model_dir.join("model.onnx"))?;
        let spec = ModelSpec {
            model_id: format!(
                "{}@{onnx}/i8/calib@{}/{CHUNKER_VERSION}",
                setup.model,
                digest.to_hex()
            ),
            calib: *digest.as_bytes(),
            dims: table.min.len(),
        };
        Ok(Self {
            spec,
            name: setup.model.clone(),
            helper,
            model_dir,
            table,
            proc: Mutex::new(None),
        })
    }

    fn start(&self) -> Result<Proc, String> {
        let mut child = Command::new(&self.helper)
            .arg(&self.name)
            .arg(&self.model_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.helper.display()))?;
        let (Some(w), Some(r)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("the embed helper has no pipes".to_owned());
        };
        Ok(Proc {
            child: Arc::new(Mutex::new(child)),
            conn: Conn { r, w },
        })
    }

    /// Runs `f` on the helper connection with the request timeout; the helper is killed when it
    /// fails or times out.
    fn with_helper<T>(
        &self,
        f: impl FnOnce(&mut Conn<ChildStdout, ChildStdin>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut slot = self.proc.lock().unwrap_or_else(|p| p.into_inner());
        let mut p = match slot.take() {
            Some(p) => p,
            None => {
                let mut p = self.start()?;
                let (done, wait) = mpsc::channel::<()>();
                let child = p.child.clone();
                std::thread::spawn(move || watchdog(&child, &wait));
                let hello = p.conn.hello(&self.name, self.spec.dims);
                drop(done);
                if let Err(e) = hello {
                    p.kill();
                    return Err(e);
                }
                p
            }
        };
        let (done, wait) = mpsc::channel::<()>();
        let child = p.child.clone();
        std::thread::spawn(move || watchdog(&child, &wait));
        let out = f(&mut p.conn);
        drop(done);
        match out {
            Ok(v) => {
                *slot = Some(p);
                Ok(v)
            }
            Err(e) => {
                p.kill();
                Err(e)
            }
        }
    }
}

/// Kills the helper when `done` is not dropped within [`REQUEST_TIMEOUT`].
fn watchdog(child: &Mutex<Child>, done: &mpsc::Receiver<()>) {
    if matches!(
        done.recv_timeout(REQUEST_TIMEOUT),
        Err(mpsc::RecvTimeoutError::Timeout)
    ) {
        let mut c = child.lock().unwrap_or_else(|p| p.into_inner());
        let _ = c.kill();
    }
}

impl EmbedModel for HelperEmbedder {
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<i8>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            let vectors = self.with_helper(|c| c.embed(batch, self.spec.dims))?;
            for v in vectors {
                out.push(self.table.quantise(&v)?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(json: &str) -> Vec<u8> {
        let mut v = (json.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(json.as_bytes());
        v
    }

    fn table_bytes(dims: u32, lo: f32, hi: f32) -> Vec<u8> {
        let mut v = dims.to_le_bytes().to_vec();
        for _ in 0..dims {
            v.extend_from_slice(&lo.to_le_bytes());
        }
        for _ in 0..dims {
            v.extend_from_slice(&hi.to_le_bytes());
        }
        v
    }

    fn conn(reply: Vec<u8>) -> Conn<Cursor<Vec<u8>>, Vec<u8>> {
        Conn {
            r: Cursor::new(reply),
            w: Vec::new(),
        }
    }

    #[test]
    fn hello_accepts_only_the_configured_model() {
        let ok = frame(r#"{"op":"hello","version":1,"model":"m","dims":2}"#);
        assert!(conn(ok.clone()).hello("m", 2).is_ok());
        assert!(conn(ok.clone()).hello("other", 2).is_err());
        assert!(conn(ok).hello("m", 3).is_err());
        let old = frame(r#"{"op":"hello","version":9,"model":"m","dims":2}"#);
        assert!(conn(old).hello("m", 2).is_err());
        let err = frame(r#"{"op":"error","message":"no model"}"#);
        assert!(conn(err).hello("m", 2).unwrap_err().contains("no model"));
    }

    #[test]
    fn embed_checks_the_shape_and_frames_the_request() {
        let mut c = conn(frame(r#"{"op":"vectors","vectors":[[1.0,2.0],[3.0,4.0]]}"#));
        let got = c.embed(&["a".into(), "b".into()], 2).unwrap();
        assert_eq!(got, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        let sent = &c.w;
        let len = u32::from_le_bytes(sent[..4].try_into().unwrap()) as usize;
        assert_eq!(len, sent.len() - 4);
        assert!(
            std::str::from_utf8(&sent[4..])
                .unwrap()
                .contains(r#""texts":["a","b"]"#)
        );
        // one vector for two texts, and vectors of the wrong size
        let short = frame(r#"{"op":"vectors","vectors":[[1.0,2.0]]}"#);
        assert!(conn(short).embed(&["a".into(), "b".into()], 2).is_err());
        let wide = frame(r#"{"op":"vectors","vectors":[[1.0,2.0,3.0]]}"#);
        assert!(conn(wide).embed(&["a".into()], 2).is_err());
    }

    #[test]
    fn frames_are_bounded_and_truncation_is_an_error() {
        let huge = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        assert!(conn(huge).hello("m", 2).unwrap_err().contains("exceeds"));
        let mut cut = frame(r#"{"op":"hello","version":1,"model":"m","dims":2}"#);
        cut.truncate(10);
        assert!(conn(cut).hello("m", 2).is_err());
        assert!(conn(Vec::new()).hello("m", 2).is_err());
        assert!(conn(frame("not json")).hello("m", 2).is_err());
    }

    #[test]
    fn calibration_is_validated_and_quantises_to_the_i8_range() {
        let t = Table::from_bytes(&table_bytes(2, -1.0, 1.0)).unwrap();
        assert_eq!(t.quantise(&[-1.0, 1.0]).unwrap(), vec![-127, 127]);
        assert_eq!(t.quantise(&[0.0, 5.0]).unwrap(), vec![0, 127]);
        assert!(t.quantise(&[0.0]).is_err());
        assert!(Table::from_bytes(&[]).is_err());
        assert!(Table::from_bytes(&table_bytes(0, 0.0, 0.0)).is_err());
        assert!(Table::from_bytes(&table_bytes(2, 1.0, -1.0)).is_err());
        assert!(Table::from_bytes(&table_bytes(2, f32::NAN, 1.0)).is_err());
        let mut short = table_bytes(2, 0.0, 1.0);
        short.pop();
        assert!(Table::from_bytes(&short).is_err());
    }

    #[test]
    fn no_dir_means_no_embedder_and_the_message_says_how_to_configure_one() {
        let cfg = EmbedConfig::default();
        assert!(Setup::from_config(&cfg).is_none());
        assert!(open(None).is_none());
        assert!(NOT_CONFIGURED.contains("[codeintel.embed] dir"));
        assert!(NOT_CONFIGURED.contains("asc-embed"));
        assert!(NOT_CONFIGURED.contains("calibration.bin"));
    }

    #[test]
    fn a_directory_without_the_helper_does_not_load() {
        let d = std::env::temp_dir().join(format!("asc-embedder-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let setup = Setup {
            dir: d.clone(),
            model: "m".into(),
        };
        assert!(HelperEmbedder::open(&setup).is_err());
        assert!(open(Some(&setup)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_complete_directory_gives_a_model_id_from_the_files() {
        let d = std::env::temp_dir().join(format!("asc-embedder-ok-{}", std::process::id()));
        let model = d.join("model");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(d.join(HELPER_FILE), b"x").unwrap();
        for f in MODEL_FILES {
            std::fs::write(model.join(f), f.as_bytes()).unwrap();
        }
        let calib = table_bytes(4, -1.0, 1.0);
        std::fs::write(d.join("calibration.bin"), &calib).unwrap();
        let setup = Setup {
            dir: d.clone(),
            model: "m".into(),
        };
        let e = HelperEmbedder::open(&setup).unwrap();
        let spec = e.spec();
        assert_eq!(spec.dims, 4);
        assert_eq!(spec.calib, *blake3::hash(&calib).as_bytes());
        assert!(spec.model_id.starts_with("m@"));
        assert!(spec.model_id.contains(&sha256_hex(b"model.onnx")));
        assert!(spec.model_id.contains(&hex(&spec.calib)));
        assert!(spec.model_id.ends_with(&format!("/{CHUNKER_VERSION}")));
        // The "helper" is not an executable: the call fails with a message and nothing is left
        // running, so the next call tries again.
        for _ in 0..2 {
            let err = e.embed(&["a".to_owned()]).unwrap_err();
            assert!(err.contains("cannot start"), "{err}");
        }
        assert!(e.proc.lock().unwrap().is_none());
        assert!(e.embed(&[]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn sha256_hex(b: &[u8]) -> String {
        hex(&Sha256::digest(b))
    }
}
