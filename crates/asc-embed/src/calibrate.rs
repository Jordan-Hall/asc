//! i8 quantisation with per-dimension min/max calibration (SPEC §23.4).
//!
//! The table is calibrated once per model on 10k ASC chunks and stored with its blake3 digest.

use crate::error::Error;

/// Chunks the calibration set holds.
pub const CALIBRATION_CHUNKS: usize = 10_000;
/// Dimension bound for a table read from untrusted bytes.
pub const MAX_DIMS: usize = 4096;

/// blake3 digest of a calibration table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CalibrationDigest(pub [u8; 32]);

impl CalibrationDigest {
    /// Lowercase hex form, used in the model id string.
    pub fn hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.0)
    }
}

/// Per-dimension min and max.
#[derive(Clone, Debug, PartialEq)]
pub struct Calibration {
    min: Vec<f32>,
    max: Vec<f32>,
}

impl Calibration {
    /// Dimensions.
    pub fn dims(&self) -> usize {
        self.min.len()
    }

    /// Quantise `v` to i8: each dimension maps `[min, max]` onto `[-127, 127]`, clamped.
    pub fn quantise(&self, v: &[f32]) -> Result<Vec<i8>, Error> {
        if v.len() != self.dims() {
            return Err(Error::Calibration(format!(
                "vector has {} dims, table {}",
                v.len(),
                self.dims()
            )));
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

    /// Stable encoding: `u32 LE dims`, then all mins, then all maxes, as f32 LE.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.dims() * 8);
        out.extend_from_slice(&(self.dims() as u32).to_le_bytes());
        for x in self.min.iter().chain(&self.max) {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out
    }

    /// Decode a table from untrusted bytes, checking length, bounds and finiteness.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let bad = |m: &str| Error::Calibration(m.to_owned());
        let head: [u8; 4] = bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| bad("short table"))?;
        let dims = u32::from_le_bytes(head) as usize;
        if dims == 0 || dims > MAX_DIMS || bytes.len() != 4 + dims * 8 {
            return Err(bad("bad table size"));
        }
        let floats: Vec<f32> = bytes[4..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if floats.iter().any(|x| !x.is_finite()) {
            return Err(bad("non-finite value"));
        }
        let (min, max) = floats.split_at(dims);
        if min.iter().zip(max).any(|(lo, hi)| lo > hi) {
            return Err(bad("min above max"));
        }
        Ok(Self {
            min: min.to_vec(),
            max: max.to_vec(),
        })
    }

    /// blake3 digest of [`Self::to_bytes`].
    pub fn digest(&self) -> CalibrationDigest {
        CalibrationDigest(*blake3::hash(&self.to_bytes()).as_bytes())
    }

    /// Decode and require the bytes to hash to `expected`.
    pub fn from_verified_bytes(bytes: &[u8], expected: &CalibrationDigest) -> Result<Self, Error> {
        let table = Self::from_bytes(bytes)?;
        if table.digest() != *expected {
            return Err(Error::Calibration("digest mismatch".to_owned()));
        }
        Ok(table)
    }
}

/// Streaming min/max accumulator; memory is O(dims).
#[derive(Clone, Debug)]
pub struct Calibrator {
    min: Vec<f32>,
    max: Vec<f32>,
    seen: usize,
}

impl Calibrator {
    /// A calibrator for `dims`-dimensional vectors.
    pub fn new(dims: usize) -> Result<Self, Error> {
        if dims == 0 || dims > MAX_DIMS {
            return Err(Error::Calibration(format!("dims {dims} out of range")));
        }
        Ok(Self {
            min: vec![f32::INFINITY; dims],
            max: vec![f32::NEG_INFINITY; dims],
            seen: 0,
        })
    }

    /// Vectors observed so far.
    pub fn seen(&self) -> usize {
        self.seen
    }

    /// Fold one vector in. Non-finite values are rejected.
    pub fn observe(&mut self, v: &[f32]) -> Result<(), Error> {
        if v.len() != self.min.len() || v.iter().any(|x| !x.is_finite()) {
            return Err(Error::Calibration("bad vector".to_owned()));
        }
        for (i, &x) in v.iter().enumerate() {
            self.min[i] = self.min[i].min(x);
            self.max[i] = self.max[i].max(x);
        }
        self.seen += 1;
        Ok(())
    }

    /// Finish into a table. Fails when nothing was observed.
    pub fn finish(self) -> Result<Calibration, Error> {
        if self.seen == 0 {
            return Err(Error::Calibration("no vectors observed".to_owned()));
        }
        Ok(Calibration {
            min: self.min,
            max: self.max,
        })
    }
}

/// Model id string of SPEC §23.7: `name@<onnx sha256>/i8/calib@<digest>/<chunker_ver>`.
pub fn model_id(
    name: &str,
    onnx_sha256: &str,
    calib: &CalibrationDigest,
    chunker_ver: u32,
) -> String {
    format!(
        "{name}@{onnx_sha256}/i8/calib@{}/{chunker_ver}",
        calib.hex()
    )
}
