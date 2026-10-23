//! Model registry: sha256-pinned files and the licence allow-list (SPEC §23.6, §23.7).

use crate::error::Error;
use sha2::{Digest, Sha256};

/// Licences a registry model may carry.
pub const LICENCE_ALLOW: [&str; 2] = ["MIT", "Apache-2.0"];

/// Pooling applied to token embeddings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    /// First-token pooling.
    Cls,
    /// Mean pooling.
    Mean,
}

/// One registry entry. Every file is pinned by sha256 (lowercase hex).
#[derive(Clone, Debug)]
pub struct ModelRecord {
    /// Short name, for example `jina-embeddings-v2-base-code`.
    pub name: &'static str,
    /// SPDX licence id.
    pub licence: &'static str,
    /// Output dimensions.
    pub dims: usize,
    /// Token limit.
    pub max_length: usize,
    /// Pooling method.
    pub pooling: Pooling,
    /// sha256 of the ONNX file.
    pub onnx_sha256: &'static str,
    /// sha256 of `tokenizer.json`.
    pub tokenizer_sha256: &'static str,
    /// sha256 of `config.json`.
    pub config_sha256: &'static str,
    /// sha256 of `special_tokens_map.json`.
    pub special_tokens_sha256: &'static str,
    /// sha256 of `tokenizer_config.json`.
    pub tokenizer_config_sha256: &'static str,
}

/// Model file bytes, not yet verified.
#[derive(Clone, Debug, Default)]
pub struct ModelFiles {
    /// ONNX graph.
    pub onnx: Vec<u8>,
    /// `tokenizer.json`.
    pub tokenizer: Vec<u8>,
    /// `config.json`.
    pub config: Vec<u8>,
    /// `special_tokens_map.json`.
    pub special_tokens: Vec<u8>,
    /// `tokenizer_config.json`.
    pub tokenizer_config: Vec<u8>,
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn check(name: &'static str, expected: &str, bytes: &[u8]) -> Result<(), Error> {
    let actual = sha256_hex(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(Error::Digest {
            name,
            expected: expected.to_owned(),
            actual,
        })
    }
}

impl ModelRecord {
    /// Whether the licence is on the allow-list.
    pub fn licence_allowed(&self) -> bool {
        LICENCE_ALLOW.contains(&self.licence)
    }

    /// Check the licence and every file digest. Call before handing bytes to the runtime.
    pub fn verify(&self, files: &ModelFiles) -> Result<(), Error> {
        if !self.licence_allowed() {
            return Err(Error::Licence(self.licence.to_owned()));
        }
        check("onnx", self.onnx_sha256, &files.onnx)?;
        check("tokenizer", self.tokenizer_sha256, &files.tokenizer)?;
        check("config", self.config_sha256, &files.config)?;
        check(
            "special_tokens",
            self.special_tokens_sha256,
            &files.special_tokens,
        )?;
        check(
            "tokenizer_config",
            self.tokenizer_config_sha256,
            &files.tokenizer_config,
        )
    }
}

/// Built-in default model: jinaai/jina-embeddings-v2-base-code at revision 516f4baf.
pub const JINA_V2_BASE_CODE: ModelRecord = ModelRecord {
    name: "jina-embeddings-v2-base-code",
    licence: "Apache-2.0",
    dims: 768,
    max_length: 8192,
    pooling: Pooling::Mean,
    onnx_sha256: "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b",
    tokenizer_sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
    config_sha256: "e426aa684c7f9a95c5f020aa855faf93a24f065f5fad0c9e17b124670cabdea6",
    special_tokens_sha256: "06e405a36dfe4b9604f484f6a1e619af1a7f7d09e34a8555eb0b77b66318067f",
    tokenizer_config_sha256: "f477aeb15ff9f78d3c1ddf2361d2b0b8b20cf55220f839f29a37f3a18efddd89",
};

/// The set of known models.
#[derive(Clone, Debug)]
pub struct Registry {
    records: Vec<ModelRecord>,
}

impl Registry {
    /// The built-in registry.
    pub fn builtin() -> Self {
        Self {
            records: vec![JINA_V2_BASE_CODE],
        }
    }

    /// A registry of the given records, dropping any with a disallowed licence.
    pub fn new(records: Vec<ModelRecord>) -> Self {
        Self {
            records: records
                .into_iter()
                .filter(ModelRecord::licence_allowed)
                .collect(),
        }
    }

    /// Look a model up by name.
    pub fn get(&self, name: &str) -> Result<&ModelRecord, Error> {
        self.records
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| Error::UnknownModel(name.to_owned()))
    }
}
