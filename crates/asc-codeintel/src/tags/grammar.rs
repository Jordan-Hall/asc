//! Grammar and project-owned query per language. Queries compile once per process.

use std::sync::OnceLock;

use tree_sitter::{Language, Query};

use super::TagsError;
use crate::lang::Lang;

struct Compiled {
    language: Language,
    query: Query,
}

fn language_of(lang: Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// The `tags.scm` text. TypeScript and TSX share one file: TSX only adds JSX nodes.
pub fn query_source(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => include_str!("../../queries/rust/tags.scm"),
        Lang::TypeScript | Lang::Tsx => include_str!("../../queries/typescript/tags.scm"),
        Lang::JavaScript => include_str!("../../queries/javascript/tags.scm"),
        Lang::Python => include_str!("../../queries/python/tags.scm"),
        Lang::CSharp => include_str!("../../queries/csharp/tags.scm"),
        Lang::Go => include_str!("../../queries/go/tags.scm"),
    }
}

fn slot(lang: Lang) -> &'static OnceLock<Result<Compiled, String>> {
    static SLOTS: [OnceLock<Result<Compiled, String>>; 7] = [const { OnceLock::new() }; 7];
    let i = match lang {
        Lang::Rust => 0,
        Lang::TypeScript => 1,
        Lang::Tsx => 2,
        Lang::JavaScript => 3,
        Lang::Python => 4,
        Lang::CSharp => 5,
        Lang::Go => 6,
    };
    &SLOTS[i]
}

fn compiled(lang: Lang) -> Result<&'static Compiled, TagsError> {
    slot(lang)
        .get_or_init(|| {
            let language = language_of(lang);
            Query::new(&language, query_source(lang))
                .map(|query| Compiled { language, query })
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| TagsError::Query(e.clone()))
}

pub fn language(lang: Lang) -> Result<&'static Language, TagsError> {
    compiled(lang).map(|c| &c.language)
}

pub fn query(lang: Lang) -> Result<&'static Query, TagsError> {
    compiled(lang).map(|c| &c.query)
}
