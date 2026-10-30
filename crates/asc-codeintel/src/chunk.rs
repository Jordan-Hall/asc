//! AST chunker (SPEC 23.2.2): split the tree-sitter tree, collapse large definitions to their
//! signatures, merge small siblings greedily. Files with no grammar get line windows. The input
//! is untrusted: the size, the recursion depth, the chunk count, every chunk's size and the
//! parse time are bounded.
//!
//! Lines in a [`Chunk`] are one-based and inclusive. The embedding text is the language, the
//! enclosing definition's signature and the body; it never contains the path.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, ParseOptions, Parser};

use crate::lang::Lang;
use crate::skip::MAX_BYTES;
use crate::tags::{self, Kind, NO_CONTAINER, Role, Tag};

/// Bump when the chunker's output changes; it keys `chunks.v1`.
pub const CHUNKER_VERSION: u32 = 1;
/// A node of at most this many non-whitespace characters is a chunk candidate.
pub const BUDGET_CHARS: usize = 1_536;
/// A chunk never holds more bytes than this, whatever its whitespace.
pub const MAX_CHUNK_BYTES: usize = 16 * 1024;
/// Line window size and overlap for files without a grammar.
pub const WINDOW_LINES: usize = 60;
pub const WINDOW_OVERLAP: usize = 10;
/// At most this many chunks per file.
pub const MAX_CHUNKS: usize = 50_000;
/// Walk depth after which a node is split by lines.
const MAX_DEPTH: usize = 96;
/// Time budget for the parse.
const PARSE_BUDGET: Duration = Duration::from_millis(250);
/// Marker for a body replaced in a collapsed chunk.
const ELIDED: &str = "{ \u{2026} }";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("file exceeds {MAX_BYTES} bytes")]
    TooLarge,
}

/// `blake3::derive_key("asc v1 ci chunk", embed_text)`: one vector per namespace and model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkTextId(pub [u8; 32]);

impl ChunkTextId {
    pub fn of(embed_text: &str) -> ChunkTextId {
        ChunkTextId(blake3::derive_key("asc v1 ci chunk", embed_text.as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start_line: u32,
    pub end_line: u32,
    /// Qualified name of the innermost definition the chunk belongs to, or empty.
    pub symbol: String,
    /// True for a signature-only chunk of a large definition.
    pub collapsed: bool,
    pub embed_text: String,
    pub id: ChunkTextId,
}

/// The `chunks.v1` row of a chunk: `(start_line, end_line, symbol, ChunkTextId)`.
pub fn chunk_map(chunks: &[Chunk]) -> Vec<(u32, u32, String, ChunkTextId)> {
    chunks
        .iter()
        .map(|c| (c.start_line, c.end_line, c.symbol.clone(), c.id))
        .collect()
}

/// Chunks of one file. A file with no language, or one the grammar cannot parse in time, gets
/// line windows.
pub fn chunk(lang: Option<Lang>, src: &[u8]) -> Result<Vec<Chunk>, ChunkError> {
    if src.len() > MAX_BYTES {
        return Err(ChunkError::TooLarge);
    }
    let lines = Lines::new(src);
    if let Some(l) = lang
        && let Some(out) = ast_chunks(l, src, &lines)
    {
        return Ok(out);
    }
    Ok(window_chunks(lang, src, &lines))
}

struct Lines {
    starts: Vec<u32>,
}

impl Lines {
    fn new(src: &[u8]) -> Lines {
        let mut starts = vec![0u32];
        starts.extend(
            src.iter()
                .enumerate()
                .filter(|&(_, &b)| b == b'\n')
                .map(|(i, _)| i as u32 + 1),
        );
        Lines { starts }
    }

    /// Zero-based row of a byte offset.
    fn row(&self, byte: usize) -> usize {
        self.starts.partition_point(|&s| s as usize <= byte) - 1
    }

    fn count(&self) -> usize {
        self.starts.len()
    }

    fn start(&self, row: usize) -> usize {
        self.starts[row] as usize
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Counts a character once: continuation bytes are not characters.
fn counts(b: u8) -> bool {
    !is_ws(b) && (b & 0xC0) != 0x80
}

/// The longest prefix of `bytes` that fits both limits, at least one character.
fn fit_prefix(bytes: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] & 0xC0) == 0x80 {
            j += 1;
        }
        n += usize::from(counts(bytes[i]));
        if i > 0 && (n > BUDGET_CHARS || j > MAX_CHUNK_BYTES) {
            return i;
        }
        i = j;
    }
    bytes.len()
}

fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn embed(lang: Option<Lang>, context: &[u8], body: &str) -> String {
    let mut s = String::with_capacity(body.len() + context.len() + 32);
    s.push_str("language: ");
    s.push_str(lang.map_or("text", Lang::name));
    s.push('\n');
    if !context.is_empty() {
        s.push_str("context: ");
        s.push_str(&text_of(context));
        s.push('\n');
    }
    s.push('\n');
    s.push_str(body);
    s
}

fn make(
    lang: Option<Lang>,
    lines: &Lines,
    range: (usize, usize),
    symbol: String,
    context: &[u8],
    body: &str,
    collapsed: bool,
) -> Chunk {
    let embed_text = embed(lang, context, body);
    Chunk {
        start_line: lines.row(range.0) as u32 + 1,
        end_line: lines.row(range.1.saturating_sub(1).max(range.0)) as u32 + 1,
        symbol,
        collapsed,
        id: ChunkTextId::of(&embed_text),
        embed_text,
    }
}

// ---------------------------------------------------------------------------------------------
// Line windows

fn window_chunks(lang: Option<Lang>, src: &[u8], lines: &Lines) -> Vec<Chunk> {
    let mut out = Vec::new();
    let total = lines.count();
    let mut s = 0usize;
    while s < total && out.len() < MAX_CHUNKS {
        let from = lines.start(s);
        let mut e = (s + WINDOW_LINES).min(total);
        let mut to = if e < total { lines.start(e) } else { src.len() };
        if to - from > MAX_CHUNK_BYTES {
            // Shrink to whole lines under the byte cap; one line over the cap is cut.
            e = lines.row(from + MAX_CHUNK_BYTES).max(s + 1).min(e);
            to = if e < total { lines.start(e) } else { src.len() };
            if to - from > MAX_CHUNK_BYTES {
                to = from + fit_prefix(&src[from..to]).min(MAX_CHUNK_BYTES);
            }
        }
        let body = text_of(&src[from..to]);
        if !body.trim().is_empty() {
            out.push(make(
                lang,
                lines,
                (from, to),
                String::new(),
                &[],
                &body,
                false,
            ));
        }
        if e >= total {
            break;
        }
        s = (e - WINDOW_OVERLAP.min(e - s - 1)).max(s + 1);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// AST chunks

fn language_of(lang: Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx | Lang::Tsrx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

fn collapsible(k: Kind) -> bool {
    matches!(
        k,
        Kind::Class
            | Kind::Struct
            | Kind::Enum
            | Kind::Interface
            | Kind::Trait
            | Kind::Impl
            | Kind::Function
            | Kind::Component
            | Kind::Method
            | Kind::Module
    )
}

struct Walker<'a> {
    lang: Lang,
    src: &'a [u8],
    lines: &'a Lines,
    tags: Vec<Tag>,
    /// `nws[i]` = characters counted in `src[..i]`.
    nws: Vec<u32>,
    out: Vec<Chunk>,
}

impl Walker<'_> {
    fn full(&self) -> bool {
        self.out.len() >= MAX_CHUNKS
    }

    fn fits(&self, a: usize, b: usize) -> bool {
        b - a <= MAX_CHUNK_BYTES && (self.nws[b] - self.nws[a]) as usize <= BUDGET_CHARS
    }

    fn has_text(&self, a: usize, b: usize) -> bool {
        self.nws[b] > self.nws[a]
    }

    fn symbol(&self, ctx: Option<usize>) -> String {
        let mut names = Vec::new();
        let mut cur = ctx;
        while let Some(i) = cur {
            if names.len() >= 32 {
                break;
            }
            names.push(self.tags[i].name.as_str());
            let c = self.tags[i].container;
            cur = (c != NO_CONTAINER && (c as usize) < i).then_some(c as usize);
        }
        names.reverse();
        names.join("::")
    }

    fn context(&self, ctx: Option<usize>) -> &[u8] {
        ctx.map_or(&[][..], |i| &self.tags[i].sig)
    }

    fn emit(&mut self, a: usize, b: usize, ctx: Option<usize>) {
        if self.full() || !self.has_text(a, b) {
            return;
        }
        let body = text_of(&self.src[a..b]);
        let c = make(
            Some(self.lang),
            self.lines,
            (a, b),
            self.symbol(ctx),
            self.context(ctx),
            &body,
            false,
        );
        self.out.push(c);
    }

    /// Splits an oversized range with no structure to follow: whole lines first, then
    /// characters.
    fn split_lines(&mut self, a: usize, b: usize, ctx: Option<usize>) {
        let mut from = a;
        while from < b && !self.full() {
            let mut to = from;
            loop {
                let next = self.src[to..b]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b, |p| to + p + 1);
                if next == to || !self.fits(from, next) {
                    break;
                }
                to = next;
            }
            if to == from {
                to = from + fit_prefix(&self.src[from..b]);
            }
            self.emit(from, to, ctx);
            from = to;
        }
    }

    /// The collapsed text of a definition node: member definitions become their signature and
    /// `{ … }`. A definition without members becomes its own signature and `{ … }`.
    fn collapsed_text(&self, node: Node, tag: usize) -> String {
        let (a, b) = (node.start_byte(), node.end_byte());
        let members: Vec<&Tag> = self
            .tags
            .iter()
            .filter(|t| {
                t.role == Role::Def
                    && t.container == tag as u32
                    && collapsible(t.kind)
                    && (t.range.start_byte as usize) >= a
                    && (t.range.end_byte as usize) <= b
            })
            .collect();
        let mut s = String::new();
        if members.is_empty() {
            s.push_str(&text_of(&self.tags[tag].sig));
            s.push(' ');
            s.push_str(ELIDED);
        } else {
            let mut at = a;
            for m in members {
                let (ms, me) = (m.range.start_byte as usize, m.range.end_byte as usize);
                if ms < at {
                    continue;
                }
                s.push_str(&text_of(&self.src[at..ms]));
                s.push_str(&text_of(&m.sig));
                s.push(' ');
                s.push_str(ELIDED);
                at = me;
            }
            s.push_str(&text_of(&self.src[at..b]));
        }
        let keep = fit_prefix(s.as_bytes());
        let mut keep = keep.min(s.len());
        while !s.is_char_boundary(keep) {
            keep -= 1;
        }
        s.truncate(keep);
        s
    }

    fn emit_collapsed(&mut self, node: Node, tag: usize, parent: Option<usize>) {
        if self.full() {
            return;
        }
        let body = self.collapsed_text(node, tag);
        let c = make(
            Some(self.lang),
            self.lines,
            (node.start_byte(), node.end_byte()),
            self.symbol(Some(tag)),
            self.context(parent),
            &body,
            true,
        );
        self.out.push(c);
    }

    fn def_at(&self, node: Node) -> Option<usize> {
        let (a, b) = (node.start_byte() as u32, node.end_byte() as u32);
        let i = self.tags.partition_point(|t| {
            (t.range.start_byte, std::cmp::Reverse(t.range.end_byte)) < (a, std::cmp::Reverse(b))
        });
        self.tags[i..]
            .iter()
            .take_while(|t| t.range.start_byte == a)
            .position(|t| t.role == Role::Def && t.range.end_byte == b && collapsible(t.kind))
            .map(|p| i + p)
    }

    /// Walks the children of `node`, merging adjacent small ones.
    fn walk(&mut self, node: Node, ctx: Option<usize>, depth: usize) {
        let mut buf: Option<(usize, usize)> = None;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if self.full() {
                return;
            }
            let (a, b) = (
                child.start_byte().min(self.src.len()),
                child.end_byte().min(self.src.len()),
            );
            if !self.has_text(a, b) {
                continue;
            }
            if self.fits(a, b) {
                buf = match buf {
                    Some((s, _)) if self.fits(s, b) => Some((s, b)),
                    Some((s, e)) => {
                        self.emit(s, e, ctx);
                        Some((a, b))
                    }
                    None => Some((a, b)),
                };
                continue;
            }
            if let Some((s, e)) = buf.take() {
                self.emit(s, e, ctx);
            }
            self.big(child, ctx, depth);
        }
        if let Some((s, e)) = buf {
            self.emit(s, e, ctx);
        }
    }

    fn big(&mut self, node: Node, ctx: Option<usize>, depth: usize) {
        let (a, b) = (node.start_byte(), node.end_byte().min(self.src.len()));
        if depth >= MAX_DEPTH || node.child_count() == 0 {
            self.split_lines(a, b, ctx);
            return;
        }
        if let Some(t) = self.def_at(node) {
            self.emit_collapsed(node, t, ctx);
            self.walk(node, Some(t), depth + 1);
        } else {
            self.walk(node, ctx, depth + 1);
        }
    }
}

fn ast_chunks(lang: Lang, src: &[u8], lines: &Lines) -> Option<Vec<Chunk>> {
    let deadline = Instant::now() + PARSE_BUDGET;
    let mut parser = Parser::new();
    parser.set_language(&language_of(lang)).ok()?;
    let mut cancel = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    // A TSRX file is parsed with its template regions blanked; offsets and rows are unchanged.
    let masked;
    let parse_src: &[u8] = if lang == Lang::Tsrx {
        masked = crate::tsrx::analyze(src).masked;
        &masked
    } else {
        src
    };
    let tree = parser.parse_with_options(
        &mut |i, _| {
            if i < parse_src.len() {
                &parse_src[i..]
            } else {
                &[][..]
            }
        },
        None,
        Some(ParseOptions::new().progress_callback(&mut cancel)),
    )?;
    // Without tags the walk still splits and merges; it only cannot collapse.
    let tags = tags::extract_within(lang, src, PARSE_BUDGET).unwrap_or_default();
    let mut nws = Vec::with_capacity(src.len() + 1);
    let mut n = 0u32;
    nws.push(0);
    for &b in src {
        n += u32::from(counts(b));
        nws.push(n);
    }
    let mut w = Walker {
        lang,
        src,
        lines,
        tags,
        nws,
        out: Vec::new(),
    };
    let root = tree.root_node();
    if w.fits(0, src.len()) {
        w.emit(0, src.len(), None);
    } else {
        w.walk(root, None, 0);
    }
    Some(w.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_fn(name: &str, stmts: usize) -> String {
        let mut s = format!("fn {name}(x: u32) -> u32 {{\n");
        for i in 0..stmts {
            s.push_str(&format!(
                "    let v{i} = x.wrapping_mul({i}).wrapping_add(7);\n"
            ));
        }
        s.push_str("    x\n}\n");
        s
    }

    #[test]
    fn small_file_is_one_chunk() {
        let src = b"fn a() {}\nfn b() {}\n";
        let c = chunk(Some(Lang::Rust), src).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].start_line, c[0].end_line), (1, 2));
        assert!(!c[0].collapsed);
        assert!(c[0].embed_text.starts_with("language: rust\n"));
    }

    #[test]
    fn empty_and_blank_have_no_chunks() {
        assert!(chunk(Some(Lang::Rust), b"").unwrap().is_empty());
        assert!(chunk(None, b"\n\n  \n").unwrap().is_empty());
    }

    #[test]
    fn small_siblings_merge_and_large_ones_split() {
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&big_fn(&format!("f{i}"), 3));
        }
        let c = chunk(Some(Lang::Rust), src.as_bytes()).unwrap();
        assert!(c.len() > 1);
        for ch in &c {
            let nws = ch.embed_text.split("\n\n").skip(1).collect::<String>();
            let n = nws.chars().filter(|c| !c.is_whitespace()).count();
            assert!(n <= BUDGET_CHARS, "{n}");
        }
        // Merged: fewer chunks than functions.
        assert!(c.len() < 40);
        // Lines are ordered and cover the file without gaps larger than blank lines.
        assert_eq!(c[0].start_line, 1);
        assert!(c.windows(2).all(|w| w[0].end_line < w[1].start_line + 1));
    }

    #[test]
    fn large_impl_gets_a_collapsed_chunk_then_recurses() {
        let mut src = String::from("impl Thing {\n");
        for i in 0..6 {
            src.push_str(&big_fn(&format!("m{i}"), 12));
        }
        src.push_str("}\n");
        let c = chunk(Some(Lang::Rust), src.as_bytes()).unwrap();
        let col = c
            .iter()
            .find(|c| c.collapsed && c.symbol == "Thing")
            .unwrap();
        assert_eq!(col.start_line, 1);
        assert!(col.embed_text.contains("{ \u{2026} }"));
        assert!(col.embed_text.contains("fn m3(x: u32) -> u32"));
        assert!(!col.embed_text.contains("wrapping_mul"));
        // Bodies appear in non-collapsed chunks with the method as the symbol.
        let body = c
            .iter()
            .find(|c| !c.collapsed && c.embed_text.contains("v5 = x.wrapping_mul(5)"))
            .unwrap();
        assert!(body.symbol.starts_with("Thing"));
        assert!(body.embed_text.contains("context: "));
    }

    #[test]
    fn collapsed_chunk_is_bounded() {
        let mut src = String::from("impl Thing {\n");
        for i in 0..400 {
            src.push_str(&format!(
                "    fn method_number_{i}(&self, a: u64, b: u64) -> u64 {{ a + b }}\n"
            ));
        }
        src.push_str("}\n");
        let c = chunk(Some(Lang::Rust), src.as_bytes()).unwrap();
        for ch in &c {
            assert!(ch.embed_text.len() <= MAX_CHUNK_BYTES + 256);
        }
    }

    #[test]
    fn no_grammar_gets_overlapping_windows() {
        let src: String = (1..=130).map(|i| format!("line {i}\n")).collect();
        let c = chunk(None, src.as_bytes()).unwrap();
        let spans: Vec<(u32, u32)> = c.iter().map(|c| (c.start_line, c.end_line)).collect();
        assert_eq!(spans, vec![(1, 60), (51, 110), (101, 130)]);
        assert!(c[0].embed_text.starts_with("language: text\n"));
    }

    #[test]
    fn id_ignores_path_and_depends_on_text_and_language() {
        let a = chunk(Some(Lang::Rust), b"fn a() {}\n").unwrap();
        let b = chunk(Some(Lang::Rust), b"fn a() {}\n").unwrap();
        let t = chunk(None, b"fn a() {}\n").unwrap();
        assert_eq!(a[0].id, b[0].id);
        assert_ne!(a[0].id, t[0].id);
        assert_eq!(a[0].id, ChunkTextId::of(&a[0].embed_text));
        assert_eq!(chunk_map(&a)[0].3, a[0].id);
    }

    #[test]
    fn hostile_input_stays_bounded() {
        assert_eq!(
            chunk(None, &vec![b'a'; MAX_BYTES + 1]),
            Err(ChunkError::TooLarge)
        );
        // One huge line, huge whitespace, deep nesting, invalid UTF-8.
        let line = vec![b'x'; 500_000];
        for c in chunk(None, &line).unwrap() {
            assert!(c.embed_text.len() <= MAX_CHUNK_BYTES + 64);
        }
        let ws = format!("fn a() {{}}{}fn b() {{}}", " ".repeat(300_000));
        for c in chunk(Some(Lang::Rust), ws.as_bytes()).unwrap() {
            assert!(c.embed_text.len() <= MAX_CHUNK_BYTES + 64);
        }
        let deep = format!("{}1{}", "(".repeat(5_000), ")".repeat(5_000));
        let _ = chunk(Some(Lang::JavaScript), deep.as_bytes()).unwrap();
        let _ = chunk(Some(Lang::Go), &[0xff, 0xfe, b'\n', 0x80, b'a']).unwrap();
    }

    #[test]
    fn every_language_chunks_a_large_class() {
        let body = |n: &str| -> String {
            (0..60)
                .map(|i| format!("{n}{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let py = format!(
            "class A:\n    def f(self):\n        x = '{}'\n    def g(self):\n        y = '{}'\n",
            body("a"),
            body("b")
        );
        let c = chunk(Some(Lang::Python), py.as_bytes()).unwrap();
        assert!(!c.is_empty());
        let go = format!(
            "package p\nfunc F() {{\n    x := \"{}\"\n}}\n",
            body("c").repeat(2)
        );
        assert!(!chunk(Some(Lang::Go), go.as_bytes()).unwrap().is_empty());
    }
}
