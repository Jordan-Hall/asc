//! Embedding runtime for code intelligence (SPEC §23.6, §23.7).
//!
//! Server and opt-in helper only: `asc.exe` never links this crate or ONNX Runtime.
//! Models load from bytes verified against the registry sha256; nothing is downloaded here.

pub mod calibrate;
pub mod error;
pub mod protocol;
pub mod registry;
pub mod runtime;

pub use calibrate::{Calibration, CalibrationDigest, Calibrator};
pub use error::Error;
pub use registry::{ModelFiles, ModelRecord, Registry};
pub use runtime::{Embedder, FastEmbedder};
