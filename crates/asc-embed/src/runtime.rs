//! The embedding runtime: fastembed over ONNX Runtime, loaded from verified bytes.

use crate::error::Error;
use crate::registry::{ModelFiles, ModelRecord, Pooling};
use fastembed::{
    InitOptionsUserDefined, Pooling as FePooling, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};

/// Texts embedded per ONNX batch.
pub const BATCH: usize = 32;

/// Something that turns texts into vectors.
pub trait Embedder {
    /// Model name.
    fn name(&self) -> &str;
    /// Output dimensions.
    fn dims(&self) -> usize;
    /// Embed `texts`, one vector each.
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error>;
}

/// fastembed-backed embedder. Constructed only from bytes that passed [`ModelRecord::verify`].
pub struct FastEmbedder {
    name: &'static str,
    dims: usize,
    inner: TextEmbedding,
}

impl FastEmbedder {
    /// Verify `files` against `record`, then load the model. Never touches the network.
    pub fn load(record: &ModelRecord, files: ModelFiles) -> Result<Self, Error> {
        record.verify(&files)?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: files.tokenizer,
            config_file: files.config,
            special_tokens_map_file: files.special_tokens,
            tokenizer_config_file: files.tokenizer_config,
        };
        let pooling = match record.pooling {
            Pooling::Cls => FePooling::Cls,
            Pooling::Mean => FePooling::Mean,
        };
        let model =
            UserDefinedEmbeddingModel::new(files.onnx, tokenizer_files).with_pooling(pooling);
        let opts = InitOptionsUserDefined::new().with_max_length(record.max_length);
        let inner = TextEmbedding::try_new_from_user_defined(model, opts)
            .map_err(|e| Error::Backend(e.to_string()))?;
        Ok(Self {
            name: record.name,
            dims: record.dims,
            inner,
        })
    }
}

impl Embedder for FastEmbedder {
    fn name(&self) -> &str {
        self.name
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        let out = self
            .inner
            .embed(texts, Some(BATCH))
            .map_err(|e| Error::Backend(e.to_string()))?;
        if out.len() != texts.len() || out.iter().any(|v| v.len() != self.dims) {
            return Err(Error::Backend("unexpected output shape".to_owned()));
        }
        Ok(out)
    }
}
