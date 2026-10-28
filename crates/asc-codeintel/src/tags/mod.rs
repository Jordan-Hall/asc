//! Tree-sitter tags (SPEC 23.2.1, 23.6): definitions and references with a container, a
//! signature and a doc comment, as a pure function of `(lang, bytes)`. Input is untrusted: the
//! file size, the parse and query time, every string and the tag count are bounded.

mod grammar;
mod text;
mod tsrx;

use std::cell::Cell;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, ParseOptions, Parser, QueryCursor, QueryCursorOptions, StreamingIterator};

use crate::lang::Lang;
use crate::skip::MAX_BYTES;

pub use grammar::query_source;

/// Per-file time budget for parse plus query (SPEC 23.6).
pub const BUDGET: Duration = Duration::from_millis(250);
/// At most this many tags per file; further matches are dropped.
pub const MAX_TAGS: usize = 50_000;
/// Longest tag name in bytes; longer names are not tags.
pub const MAX_NAME: usize = 256;
/// Longest signature in bytes.
pub const MAX_SIG: usize = 512;
/// Longest doc text in bytes.
pub const MAX_DOC: usize = 2048;
/// `Tag::container` of a top-level tag.
pub const NO_CONTAINER: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagsError {
    #[error("file exceeds {MAX_BYTES} bytes")]
    TooLarge,
    #[error("tags cancelled after the time budget")]
    Cancelled,
    #[error("the grammar could not parse the file")]
    Parse,
    #[error("grammar or query failed to load: {0}")]
    Query(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Role {
    Def,
    Ref,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Kind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Variant,
    Interface,
    Trait,
    Impl,
    Module,
    Type,
    Constant,
    Variable,
    Field,
    Property,
    Macro,
    Call,
    /// A TSRX component: a function whose body is a `@{` container.
    Component,
    /// A TSRX control-flow body: `@for (...) { }`, `@if (...) { }`, ...
    Scope,
}

impl Kind {
    const ALL: [Kind; 19] = [
        Kind::Function,
        Kind::Method,
        Kind::Class,
        Kind::Struct,
        Kind::Enum,
        Kind::Variant,
        Kind::Interface,
        Kind::Trait,
        Kind::Impl,
        Kind::Module,
        Kind::Type,
        Kind::Constant,
        Kind::Variable,
        Kind::Field,
        Kind::Property,
        Kind::Macro,
        Kind::Call,
        Kind::Component,
        Kind::Scope,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Function => "function",
            Kind::Method => "method",
            Kind::Class => "class",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Variant => "variant",
            Kind::Interface => "interface",
            Kind::Trait => "trait",
            Kind::Impl => "impl",
            Kind::Module => "module",
            Kind::Type => "type",
            Kind::Constant => "constant",
            Kind::Variable => "variable",
            Kind::Field => "field",
            Kind::Property => "property",
            Kind::Macro => "macro",
            Kind::Call => "call",
            Kind::Component => "component",
            Kind::Scope => "scope",
        }
    }

    pub fn from_name(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    fn holds_methods(self) -> bool {
        matches!(
            self,
            Kind::Class | Kind::Struct | Kind::Enum | Kind::Interface | Kind::Trait | Kind::Impl
        )
    }

    /// Lower wins when two patterns tag the same name.
    fn rank(self) -> u8 {
        match self {
            Kind::Variable => 2,
            Kind::Type => 1,
            _ => 0,
        }
    }
}

/// A byte and row range. Rows are zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_row: u32,
    pub end_row: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub kind: Kind,
    pub role: Role,
    pub range: Range,
    pub name_range: Range,
    /// Index into the same `Vec<Tag>` of the innermost enclosing definition, or [`NO_CONTAINER`].
    pub container: u32,
    pub sig: Vec<u8>,
    pub doc: Vec<u8>,
}

/// Tags of one file in source order (start ascending, larger first, definitions before
/// references), within the default time budget.
pub fn extract(lang: Lang, src: &[u8]) -> Result<Vec<Tag>, TagsError> {
    extract_within(lang, src, BUDGET)
}

pub fn extract_within(lang: Lang, src: &[u8], budget: Duration) -> Result<Vec<Tag>, TagsError> {
    if src.len() > MAX_BYTES {
        return Err(TagsError::TooLarge);
    }
    // A TSRX file is parsed with its template regions blanked (same offsets, same rows); the
    // template's own names come from the analysis and are added at the end.
    let deadline = Instant::now() + budget;
    let analysis = (lang == Lang::Tsrx).then(|| crate::tsrx::analyze_within(src, Some(deadline)));
    let original = src;
    let src: &[u8] = analysis.as_ref().map_or(src, |a| a.masked.as_slice());
    let language = grammar::language(lang)?;
    let query = grammar::query(lang)?;
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|e| TagsError::Query(e.to_string()))?;
    let mut cancel_parse = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |i, _| if i < src.len() { &src[i..] } else { &[][..] },
            None,
            Some(ParseOptions::new().progress_callback(&mut cancel_parse)),
        )
        .ok_or(if Instant::now() >= deadline {
            TagsError::Cancelled
        } else {
            TagsError::Parse
        })?;

    let cancelled = Cell::new(false);
    let mut cancel_query = |_: &tree_sitter::QueryCursorState| {
        if Instant::now() >= deadline {
            cancelled.set(true);
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let names = query.capture_names();
    let mut raw: Vec<Tag> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches_with_options(
        query,
        tree.root_node(),
        src,
        QueryCursorOptions::new().progress_callback(&mut cancel_query),
    );
    while let Some(m) = matches.next() {
        if raw.len() >= MAX_TAGS {
            break;
        }
        let mut main: Option<(Node, Role, Kind)> = None;
        let mut name_node: Option<Node> = None;
        for c in m.captures() {
            let cname = names[c.index as usize];
            if cname == "name" {
                name_node = Some(c.node);
            } else if let Some(k) = cname.strip_prefix("def.") {
                main = Kind::from_name(k).map(|k| (c.node, Role::Def, k));
            } else if let Some(k) = cname.strip_prefix("ref.") {
                main = Kind::from_name(k).map(|k| (c.node, Role::Ref, k));
            }
        }
        let Some((node, role, kind)) = main else {
            continue;
        };
        let nn = name_node.unwrap_or(node);
        if nn.end_byte() - nn.start_byte() > MAX_NAME * 4 {
            continue;
        }
        let name = text::name(&src[nn.start_byte()..nn.end_byte().min(src.len())]);
        if name.is_empty() {
            continue;
        }
        let (sig, doc) = match role {
            Role::Def => (text::sig(node, src), text::doc(lang, node, src)),
            Role::Ref => (Vec::new(), Vec::new()),
        };
        raw.push(Tag {
            name,
            kind,
            role,
            range: range_of(node),
            name_range: range_of(nn),
            container: NO_CONTAINER,
            sig,
            doc,
        });
    }
    drop(matches);
    if cancelled.get() {
        return Err(TagsError::Cancelled);
    }
    let mut tags = finish(raw);
    if let Some(a) = &analysis {
        tsrx::add(&mut tags, a, original);
    }
    Ok(tags)
}

fn range_of(n: Node) -> Range {
    let (s, e) = (n.start_position(), n.end_position());
    Range {
        start_byte: n.start_byte() as u32,
        end_byte: n.end_byte() as u32,
        start_row: s.row as u32,
        end_row: e.row as u32,
    }
}

fn key(r: &Range) -> (u32, u32) {
    (r.start_byte, r.end_byte)
}

fn finish(mut raw: Vec<Tag>) -> Vec<Tag> {
    // One tag per (name range, role): the most specific kind wins.
    raw.sort_by_key(|t| {
        (
            key(&t.name_range),
            t.role,
            t.kind.rank(),
            std::cmp::Reverse(key(&t.range)),
        )
    });
    raw.dedup_by(|b, a| a.name_range == b.name_range && a.role == b.role);
    // A reference at the name of a definition is the definition itself.
    let def_names: HashSet<(u32, u32)> = raw
        .iter()
        .filter(|t| t.role == Role::Def)
        .map(|t| key(&t.name_range))
        .collect();
    raw.retain(|t| t.role == Role::Def || !def_names.contains(&key(&t.name_range)));

    sort_source_order(&mut raw);
    assign_containers(&mut raw);
    // Locals are not part of a file's symbols.
    let local: Vec<bool> = raw
        .iter()
        .map(|t| {
            t.role == Role::Def
                && matches!(
                    t.kind,
                    Kind::Variable | Kind::Constant | Kind::Property | Kind::Field
                )
                && t.container != NO_CONTAINER
                && matches!(
                    raw[t.container as usize].kind,
                    Kind::Function | Kind::Method
                )
        })
        .collect();
    let mut i = 0;
    raw.retain(|_| {
        i += 1;
        !local[i - 1]
    });
    assign_containers(&mut raw);
    for i in 0..raw.len() {
        let c = raw[i].container;
        if c != NO_CONTAINER
            && raw[i].role == Role::Def
            && raw[i].kind == Kind::Function
            && raw[c as usize].kind.holds_methods()
        {
            raw[i].kind = Kind::Method;
        }
    }
    raw
}

fn sort_source_order(tags: &mut [Tag]) {
    tags.sort_by_key(|t| {
        (
            t.range.start_byte,
            std::cmp::Reverse(t.range.end_byte),
            t.role,
            t.name_range.start_byte,
        )
    });
}

fn assign_containers(tags: &mut [Tag]) {
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..tags.len() {
        while let Some(&t) = stack.last() {
            if tags[t].range.start_byte <= tags[i].range.start_byte
                && tags[i].range.end_byte <= tags[t].range.end_byte
            {
                break;
            }
            stack.pop();
        }
        tags[i].container = stack.last().map_or(NO_CONTAINER, |&t| t as u32);
        if tags[i].role == Role::Def {
            stack.push(i);
        }
    }
}

/// One line of a file's outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutlineEntry {
    /// Number of enclosing definitions.
    pub depth: u16,
    /// Index of the definition in the tags.
    pub index: u32,
}

/// The definitions of one file in source order, with nesting depth. No index is needed.
/// Tolerates tags that [`extract`] did not produce: a container must come earlier.
pub fn outline(tags: &[Tag]) -> Vec<OutlineEntry> {
    let mut depth_of: Vec<u16> = vec![0; tags.len()];
    let mut out = Vec::new();
    for (i, t) in tags.iter().enumerate() {
        let c = t.container as usize;
        let d = if t.container != NO_CONTAINER && c < i {
            depth_of[c].saturating_add(1)
        } else {
            0
        };
        depth_of[i] = d;
        if t.role == Role::Def {
            out.push(OutlineEntry {
                depth: d,
                index: i as u32,
            });
        }
    }
    out
}
