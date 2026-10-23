//! git 2.55's ref-filter (SPEC §11.1): the complete `%(atom)` set, `--sort` keys, patterns and
//! reachability filters that `for-each-ref`, `branch --format` and `tag --format` share.
//!
//! A [`Format`] is parsed once; [`list`] selects the refs, loads only what the format and sort
//! keys read, sorts, and returns [`Item`]s that [`Format::render`] expands. Atoms follow git's
//! `ref-filter.c`; the places asc differs (no GPG verification, no pack sizes, a fixed-length
//! abbreviation) are noted where they arise.

#![allow(dead_code)] // The `branch` and `tag` units reuse the matching and sorting pieces.

mod extras;
mod load;
mod object;
mod render;
mod track;

pub mod item;
pub mod parse;
pub mod quote;
pub mod select;
pub mod sort;
pub mod value;

pub use item::Item;
pub use load::{Filters, Query};
pub use parse::{Atom, Kind, SortKey, parse_sort_key};
pub use quote::Quote;
pub use load::commit_raw as raw_commit;
pub use value::Env;

use super::common::{CliError, Result, WsCtx};
use super::log::walk::Hist;
use parse::Token;

/// Why a format or sort key is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// `fatal: <text>`.
    #[error("{0}")]
    Fatal(String),
    /// `%(` without its `)`: git prints the usage after the error.
    #[error("malformed format string {0}")]
    Malformed(String),
}

/// Every placeholder `for-each-ref` serves, for the native table (SPEC §11.5): each atom and,
/// for the object-sourced ones, its `*` form.
pub const ATOMS: &[&str] = &[
    "%(refname)", "%(objecttype)", "%(objectsize)", "%(objectname)", "%(deltabase)", "%(tree)",
    "%(parent)", "%(numparent)", "%(object)", "%(type)", "%(tag)", "%(author)", "%(authorname)",
    "%(authoremail)", "%(authordate)", "%(committer)", "%(committername)", "%(committeremail)",
    "%(committerdate)", "%(tagger)", "%(taggername)", "%(taggeremail)", "%(taggerdate)",
    "%(creator)", "%(creatordate)", "%(subject)", "%(body)", "%(trailers)", "%(contents)",
    "%(signature)", "%(raw)", "%(upstream)", "%(push)", "%(symref)", "%(flag)", "%(HEAD)",
    "%(color)", "%(worktreepath)", "%(align)", "%(end)", "%(if)", "%(then)", "%(else)",
    "%(ahead-behind)", "%(is-base)", "%(describe)", "%(*refname)", "%(*objecttype)",
    "%(*objectsize)", "%(*objectname)", "%(*deltabase)", "%(*tree)", "%(*parent)",
    "%(*numparent)", "%(*object)", "%(*type)", "%(*tag)", "%(*author)", "%(*authorname)",
    "%(*authoremail)", "%(*authordate)", "%(*committer)", "%(*committername)",
    "%(*committeremail)", "%(*committerdate)", "%(*tagger)", "%(*taggername)",
    "%(*taggeremail)", "%(*taggerdate)", "%(*creator)", "%(*creatordate)", "%(*subject)",
    "%(*body)", "%(*trailers)", "%(*contents)", "%(*signature)", "%(*raw)", "%(*upstream)",
    "%(*push)", "%(*symref)", "%(*flag)", "%(*HEAD)", "%(*color)", "%(*worktreepath)",
    "%(*ahead-behind)", "%(*describe)",
    // `%xx`: the byte with that hex code.
    "%0", "%1", "%2", "%3", "%4", "%5", "%6", "%7", "%8", "%9", "%a", "%b", "%c", "%d", "%e", "%f",
    "%A", "%B", "%C", "%D", "%E", "%F",
];

/// A parsed `--format`.
#[derive(Clone, Debug)]
pub struct Format {
    tokens: Vec<Token>,
    quote: Quote,
}

impl Format {
    /// Parses `text`; an unknown atom fails here, before any ref prints.
    pub fn parse(text: &[u8], quote: Quote) -> std::result::Result<Self, Error> {
        let tokens = parse::parse_format(text)?;
        if !matches!(quote, Quote::None | Quote::Perl)
            && tokens.iter().any(|t| matches!(t, Token::Atom(a) if matches!(a.kind, Kind::Raw { size: false })))
        {
            return Err(Error::Fatal("--format=raw cannot be used with --python, --shell, --tcl".to_owned()));
        }
        Ok(Self { tokens, quote })
    }

    /// The atoms the format reads.
    pub fn atoms(&self) -> impl Iterator<Item = &Atom> {
        self.tokens.iter().filter_map(|t| match t {
            Token::Atom(a) => Some(a),
            Token::Lit(_) => None,
        })
    }

    /// The text for one ref (no trailing newline).
    pub fn render(&self, item: &Item, env: &Env) -> std::result::Result<Vec<u8>, Error> {
        render::render(&self.tokens, item, env, self.quote)
    }
}

/// How to list.
pub struct Request<'a> {
    pub query: Query,
    pub format: &'a Format,
    /// In command-line order; the last is the primary key. Empty means `refname`.
    pub sorts: Vec<SortKey>,
    pub color: bool,
}

/// The refs `req` selects, sorted and with every value the format reads loaded; `Env` evaluates
/// them.
pub async fn list(ctx: &super::common::Ctx, ws: &WsCtx, req: &Request<'_>) -> Result<(Vec<Item>, Env)> {
    let default_sort;
    let sorts: &[SortKey] = if req.sorts.is_empty() {
        default_sort = [parse_sort_key("refname").map_err(to_cli)?];
        &default_sort
    } else {
        &req.sorts
    };
    let atoms: Vec<&Atom> = req.format.atoms().chain(sorts.iter().map(|k| &k.atom)).collect();
    let hist = Hist::select(ws);
    let mut items = load::collect(ws, &hist, &req.query, &atoms).await?;
    let env = load::env(ctx, ws, &atoms, req.color);
    extras::fill(ctx, ws, &hist, &mut items, &atoms).await?;
    extras::assign_is_base(ctx, ws, &mut items, &atoms).await?;
    sort::sort(&mut items, sorts, &env, req.query.ignore_case);
    Ok((items, env))
}

/// A parse or render error as the command reports it.
pub fn to_cli(e: Error) -> CliError {
    match e {
        Error::Fatal(text) => CliError::Fatal(text),
        Error::Malformed(text) => CliError::Usage(format!("error: malformed format string {text}")),
    }
}
