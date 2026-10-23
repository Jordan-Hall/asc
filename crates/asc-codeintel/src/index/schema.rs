//! The `files` and `chunks` tantivy schemas (SPEC §23.2.1, §23.2.4).
//!
//! `files` has one document per `FileId` ord: folded trigrams (`tri`, multi-valued `u64`),
//! symbol definitions and references (`sym_def`, `sym_ref`, raw terms), identifiers, doc
//! comments, path tokens and languages. `chunks` has one document per chunk, BM25 over
//! `ident`, `doc` and `symbol`. Nothing is stored: results are ords, resolved through
//! `ci_docs`.

use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, SchemaBuilder, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, TokenizerManager,
};

/// Bump to force a re-index; segment packs of another version are refused.
pub const SCHEMA_VER: u8 = 1;
/// Longest token kept by the `ident` tokenizer, in bytes.
pub const MAX_TOKEN_LEN: usize = 64;

pub const IDENT_TOK: &str = "ident";
pub const RAW_TOK: &str = "raw";

/// Which index a segment belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IndexKind {
    Files,
    Chunks,
}

impl IndexKind {
    /// Name used in `ci_manifest.index`.
    pub const fn name(self) -> &'static str {
        match self {
            IndexKind::Files => "files",
            IndexKind::Chunks => "chunks",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            IndexKind::Files => 0,
            IndexKind::Chunks => 1,
        }
    }

    pub(crate) fn from_code(c: u8) -> Option<Self> {
        match c {
            0 => Some(IndexKind::Files),
            1 => Some(IndexKind::Chunks),
            _ => None,
        }
    }

    pub fn schema(self) -> Schema {
        match self {
            IndexKind::Files => FilesSchema::build().schema,
            IndexKind::Chunks => ChunksSchema::build().schema,
        }
    }
}

/// Registers the project tokenizers. Call on every `Index` that is created or opened.
pub fn register_tokenizers(m: &TokenizerManager) {
    let ident = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .build();
    m.register(IDENT_TOK, ident);
}

fn text(tokenizer: &str, opt: IndexRecordOption) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(opt),
    )
}

/// Fields of the `files` index.
#[derive(Clone, Debug)]
pub struct FilesSchema {
    pub schema: Schema,
    /// Dense doc ordinal, indexed and fast.
    pub ord: Field,
    /// Distinct folded byte trigrams, 24-bit values.
    pub tri: Field,
    pub sym_def: Field,
    pub sym_ref: Field,
    pub ident: Field,
    pub doc: Field,
    pub path_tok: Field,
    pub lang: Field,
}

impl FilesSchema {
    pub fn build() -> Self {
        let mut b = SchemaBuilder::new();
        let ord = b.add_u64_field("ord", INDEXED | FAST);
        let tri = b.add_u64_field("tri", INDEXED);
        let sym_def = b.add_text_field("sym_def", text(RAW_TOK, IndexRecordOption::Basic));
        let sym_ref = b.add_text_field("sym_ref", text(RAW_TOK, IndexRecordOption::Basic));
        let ident = b.add_text_field("ident", text(IDENT_TOK, IndexRecordOption::WithFreqs));
        let doc = b.add_text_field("doc", text(IDENT_TOK, IndexRecordOption::WithFreqs));
        let path_tok = b.add_text_field("path_tok", text(IDENT_TOK, IndexRecordOption::Basic));
        let lang = b.add_text_field("lang", text(RAW_TOK, IndexRecordOption::Basic));
        FilesSchema {
            schema: b.build(),
            ord,
            tri,
            sym_def,
            sym_ref,
            ident,
            doc,
            path_tok,
            lang,
        }
    }
}

/// Fields of the `chunks` index.
#[derive(Clone, Debug)]
pub struct ChunksSchema {
    pub schema: Schema,
    pub ord: Field,
    /// Chunk number within the file.
    pub chunk: Field,
    pub ident: Field,
    pub doc: Field,
    pub symbol: Field,
}

impl ChunksSchema {
    pub fn build() -> Self {
        let mut b = SchemaBuilder::new();
        let ord = b.add_u64_field("ord", INDEXED | FAST);
        let chunk = b.add_u64_field("chunk", FAST);
        let ident = b.add_text_field("ident", text(IDENT_TOK, IndexRecordOption::WithFreqs));
        let doc = b.add_text_field("doc", text(IDENT_TOK, IndexRecordOption::WithFreqs));
        let symbol = b.add_text_field("symbol", text(IDENT_TOK, IndexRecordOption::WithFreqs));
        ChunksSchema {
            schema: b.build(),
            ord,
            chunk,
            ident,
            doc,
            symbol,
        }
    }
}
