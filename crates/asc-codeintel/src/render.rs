//! The one output renderer (SPEC §23.7, I22) for every surface except the local human CLI:
//! MCP, `--json`, the Forgejo search page and API, and the symbol panels.
//!
//! * **Token budget:** `est_tokens = ceil(bytes / 3)`; the default is 6,000 tokens, the hard
//!   cap 25,000. Output is cut structure-first: whole files, then matching lines of the last
//!   file. A cut sets `truncated`.
//! * **`also_at`:** one `FileId` is one file; its other paths (at most 5) go in `also_at`.
//! * **Redaction:** a flagged line loses `text` and `cols` (`redacted = true`); context lines
//!   are scanned too; a flagged file (by path or because the caller says so) shows its path
//!   only. Path and line number stay (git-grep parity). A PEM-style block (`-----BEGIN ...
//!   PRIVATE KEY-----`) redacts every line up to and including its `-----END` line, across
//!   match, context and chunk boundaries of one file; an unterminated block redacts to the end.
//!   A document whose secret flag is unknown counts as flagged.
//! * **Injection hygiene:** control characters, ANSI/OSC escapes and bidi/zero-width
//!   characters are removed; lines are cut at 300 bytes around the first match.
//! * **Provenance:** every hit carries `repo`, `changeset`, `path`, lines and `file_id`.
//!
//! All input is untrusted: every line is capped at 4 KiB before it is scanned or shown, so
//! text beyond what was scanned is never shown.

use std::collections::HashMap;

use asc_core::id::FileId;

use crate::search;
use crate::secrets;

/// Longest line in the output, bytes.
pub const MAX_LINE_OUT: usize = 300;
/// Input bytes of one line that are looked at; the rest is dropped unscanned and unshown.
pub const MAX_LINE_IN: usize = 4096;
/// Paths in `also_at`.
pub const MAX_ALSO_AT: usize = 5;
/// Default token budget.
pub const DEFAULT_TOKENS: u32 = 6_000;
/// Hard cap of the token budget.
pub const MAX_TOKENS: u32 = 25_000;
/// Default result (file) count.
pub const DEFAULT_RESULTS: u32 = 100;
/// Hard cap of the result count.
pub const MAX_RESULTS: u32 = 2_000;
/// Longest path in the output, bytes.
const MAX_PATH_OUT: usize = 1024;
/// Context lines kept on each side of a match.
const MAX_CONTEXT: usize = 10;
/// Lines of one chunk that are rendered.
const MAX_CHUNK_LINES: usize = 400;
/// Replacement text of a flagged line that must keep a body (chunk and context lines).
pub const REDACTED_TEXT: &[u8] = b"[redacted: possible secret]";

/// Bytes to tokens: `ceil(bytes / 3)`.
pub fn est_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX)
}

/// The caller's budget (the `Budget` message); zero means the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub max_results: u32,
    pub max_bytes: u32,
    pub max_tokens: u32,
}

/// Resolved limits.
#[derive(Clone, Copy, Debug)]
struct Limits {
    results: usize,
    bytes: usize,
}

impl Budget {
    fn limits(self) -> Limits {
        let tokens = match self.max_tokens {
            0 => DEFAULT_TOKENS,
            n => n.min(MAX_TOKENS),
        };
        let mut bytes = tokens as usize * 3;
        if self.max_bytes != 0 {
            bytes = bytes.min(self.max_bytes as usize);
        }
        let results = match self.max_results {
            0 => DEFAULT_RESULTS,
            n => n.min(MAX_RESULTS),
        };
        Limits { results: results as usize, bytes }
    }
}

/// What a response is about; copied into the provenance of every hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderCtx {
    pub repo: String,
    /// The resolved changeset id (raw bytes).
    pub changeset: Vec<u8>,
}

/// Where a hit came from, so an agent can cite it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Provenance {
    pub repo: String,
    pub changeset: Vec<u8>,
    pub path: Vec<u8>,
    pub start_line: u32,
    pub end_line: u32,
    pub file_id: Option<[u8; 32]>,
}

// ---------------------------------------------------------------- sanitising

/// Input line plus the byte columns (0-based) of its matches.
struct Cleaned {
    text: String,
    /// Output byte offset of each input match start, ascending as given.
    cols: Vec<usize>,
    secret: bool,
}

fn is_hidden(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Decodes lossily, drops control characters, ANSI escapes and hidden characters, and maps
/// the given input offsets to output offsets. `raw` must already be capped.
fn clean(raw: &[u8], starts: &[u32]) -> (String, Vec<usize>) {
    let mut chars: Vec<(usize, char)> = Vec::with_capacity(raw.len());
    let mut off = 0usize;
    for chunk in raw.utf8_chunks() {
        for (i, c) in chunk.valid().char_indices() {
            chars.push((off + i, c));
        }
        off += chunk.valid().len();
        if !chunk.invalid().is_empty() {
            chars.push((off, '\u{FFFD}'));
            off += chunk.invalid().len();
        }
    }
    let mut out = String::with_capacity(raw.len());
    let mut pos: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut i = 0;
    while i < chars.len() {
        let here = out.len();
        let c = chars[i].1;
        i += 1;
        if c == '\u{1b}' {
            // CSI ends at a byte 0x40..=0x7E; string escapes end at BEL or ST; others take
            // one character.
            match chars.get(i).map(|x| x.1) {
                Some('[') => {
                    i += 1;
                    while i < chars.len() {
                        let f = chars[i].1;
                        i += 1;
                        if ('\u{40}'..='\u{7e}').contains(&f) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    i += 1;
                    while i < chars.len() {
                        let f = chars[i].1;
                        i += 1;
                        if f == '\u{7}' {
                            break;
                        }
                        if f == '\u{1b}' {
                            if chars.get(i).map(|x| x.1) == Some('\\') {
                                i += 1;
                            }
                            break;
                        }
                    }
                }
                Some(_) => i += 1,
                None => {}
            }
            continue;
        }
        if c == '\t' {
            out.push(' ');
        } else if !c.is_control() && !is_hidden(c) {
            out.push(c);
        }
        pos.resize(i, here);
    }
    // Escape sequences `continue` past the fill above, so `pos` can be short.
    pos.resize(chars.len(), out.len());
    pos.push(out.len());
    let cols = starts
        .iter()
        .map(|&s| {
            let at = chars.partition_point(|x| x.0 < s as usize);
            pos.get(at).or_else(|| pos.last()).copied().unwrap_or(out.len())
        })
        .collect();
    (out, cols)
}

/// Multi-line secret state: inside a `-----BEGIN ... PRIVATE KEY-----` style block.
#[derive(Clone, Copy, Debug, Default)]
struct Block {
    open: bool,
}

/// Labels of PEM-style blocks whose body must not be shown.
fn block_label(label: &str) -> bool {
    ["PRIVATE KEY", "SECRET", "CREDENTIAL", "TOKEN"].iter().any(|k| label.contains(k))
}

/// Byte offset of the last secret `-----BEGIN <label>-----` marker in `s`, if any.
fn last_begin(s: &str) -> Option<usize> {
    let mut found = None;
    let mut from = 0;
    while let Some(i) = s[from..].find("-----BEGIN ") {
        let at = from + i;
        let rest = &s[at + 11..];
        // An unclosed marker still counts: the label is the rest of the line.
        let label = rest.split("-----").next().unwrap_or(rest);
        if block_label(label) {
            found = Some(at);
        }
        from = at + 11;
    }
    found
}

impl Block {
    /// Feeds one line (both views of it); true when the line is part of a block.
    fn feed(&mut self, views: [&str; 2]) -> bool {
        let (mut hit, mut open) = (self.open, false);
        for v in views {
            let up = v.to_ascii_uppercase();
            let (begin, end) = (last_begin(&up), up.rfind("-----END "));
            let mut o = self.open;
            if self.open {
                if let Some(e) = end {
                    o = begin.is_some_and(|b| b > e);
                }
            } else if let Some(b) = begin {
                hit = true;
                o = end.is_none_or(|e| e < b);
            }
            open |= o;
        }
        self.open = open;
        hit
    }
}

fn clean_line(raw: &[u8], starts: &[u32], block: &mut Block) -> Cleaned {
    let raw = &raw[..raw.len().min(MAX_LINE_IN)];
    let (text, cols) = clean(raw, starts);
    // Scan the raw bytes and the cleaned text: stripping can join a split token.
    let mut secret = secrets::line_is_secret(raw) || secrets::line_is_secret(text.as_bytes());
    // Every line is fed, so the state follows a block even through already-flagged lines.
    secret |= block.feed([&String::from_utf8_lossy(raw), &text]);
    Cleaned { text, cols, secret }
}

/// Cuts `text` to [`MAX_LINE_OUT`] bytes around `anchor`; returns the cut and its start.
fn window(text: &str, anchor: usize) -> (&str, usize) {
    if text.len() <= MAX_LINE_OUT {
        return (text, 0);
    }
    let mut start = anchor.min(text.len()).saturating_sub(MAX_LINE_OUT / 3);
    start = start.min(text.len() - MAX_LINE_OUT);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + MAX_LINE_OUT).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[start..end], start)
}

/// A path for display: cleaned and capped.
fn clean_path(raw: &[u8]) -> Vec<u8> {
    let raw = &raw[..raw.len().min(MAX_PATH_OUT)];
    let (mut s, _) = clean(raw, &[]);
    while s.len() > MAX_PATH_OUT {
        s.pop();
    }
    s.into_bytes()
}

fn clean_text(s: &str, cap: usize) -> String {
    let (mut t, _) = clean(&s.as_bytes()[..s.len().min(cap)], &[]);
    while t.len() > cap {
        t.pop();
    }
    t
}

fn path_flagged(path: &[u8]) -> bool {
    secrets::path_is_secret(&String::from_utf8_lossy(path))
}

/// A context or chunk line for display: flagged lines become [`REDACTED_TEXT`].
fn body_line(raw: &[u8], block: &mut Block) -> (Vec<u8>, bool) {
    let c = clean_line(raw, &[], block);
    if c.secret {
        return (REDACTED_TEXT.to_vec(), true);
    }
    let (w, _) = window(&c.text, 0);
    (w.as_bytes().to_vec(), false)
}

// ---------------------------------------------------------------- search

/// One matching line as the executor produced it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawLine {
    pub line: u32,
    pub text: Vec<u8>,
    /// Byte ranges of matches inside `text`; only the starts are used.
    pub spans: Vec<(u32, u32)>,
    pub before: Vec<Vec<u8>>,
    pub after: Vec<Vec<u8>>,
}

/// One file as the executor produced it. `paths[0]` is the primary path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFileHit {
    pub file: FileId,
    pub lang: String,
    pub paths: Vec<Vec<u8>>,
    pub lines: Vec<RawLine>,
    /// The file is flagged (the doc's secret skip flag).
    pub flagged: bool,
}

impl RawFileHit {
    /// Wraps an executor hit; `lang` is the file's language name, if known. `secret` is the
    /// doc's secret skip flag; `None` (not known) counts as flagged, so redaction fails closed.
    pub fn from_search(hit: &search::FileHit, lang: Option<&str>, secret: Option<bool>) -> Self {
        RawFileHit {
            file: hit.file,
            lang: lang.unwrap_or_default().to_owned(),
            paths: hit.paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
            lines: hit
                .lines
                .iter()
                .map(|l| RawLine {
                    line: l.line,
                    text: l.text.clone(),
                    spans: l.spans.clone(),
                    ..RawLine::default()
                })
                .collect(),
            flagged: secret != Some(false),
        }
    }
}

/// The `LineMatch` message; `redacted` marks a line whose `text` and `cols` were removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineOut {
    pub line: u32,
    /// 1-based byte columns inside `text`.
    pub cols: Vec<u32>,
    pub text: Vec<u8>,
    pub before: Vec<Vec<u8>>,
    pub after: Vec<Vec<u8>>,
    pub redacted: bool,
    pub provenance: Provenance,
}

/// The `FileHit` message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOut {
    pub path: Vec<u8>,
    pub file_id: [u8; 32],
    pub lang: String,
    pub also_at: Vec<Vec<u8>>,
    pub matches: Vec<LineOut>,
    pub redacted: bool,
    pub provenance: Provenance,
}

/// The renderer's answer for a search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedSearch {
    pub files: Vec<FileOut>,
    pub truncated: bool,
    pub est_tokens: u32,
}

fn line_cost(l: &LineOut) -> usize {
    8 + l.cols.len() * 4
        + l.text.len()
        + l.before.iter().chain(&l.after).map(|b| b.len() + 1).sum::<usize>()
}

fn file_head_cost(f: &FileOut) -> usize {
    f.path.len() + 32 + f.lang.len() + f.also_at.iter().map(|p| p.len() + 1).sum::<usize>() + 8
}

/// Context lines for display; all are scanned (for the block state), the first few are kept.
fn ctx_lines(v: &[Vec<u8>], block: &mut Block) -> Vec<Vec<u8>> {
    v.iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let (b, _) = body_line(l, block);
            (i < MAX_CONTEXT).then_some(b)
        })
        .collect()
}

fn render_line(
    ctx: &RenderCtx,
    file: &FileId,
    path: &[u8],
    raw: &RawLine,
    block: &mut Block,
) -> LineOut {
    let starts: Vec<u32> = raw.spans.iter().map(|s| s.0).collect();
    let before = ctx_lines(&raw.before, block);
    let c = clean_line(&raw.text, &starts, block);
    let after = ctx_lines(&raw.after, block);
    let prov = Provenance {
        repo: ctx.repo.clone(),
        changeset: ctx.changeset.clone(),
        path: path.to_vec(),
        start_line: raw.line,
        end_line: raw.line,
        file_id: Some(*file.as_bytes()),
    };
    if c.secret {
        return LineOut {
            line: raw.line,
            before,
            after,
            redacted: true,
            provenance: prov,
            ..LineOut::default()
        };
    }
    let (w, start) = window(&c.text, c.cols.first().copied().unwrap_or(0));
    let cols = c
        .cols
        .iter()
        .filter(|&&p| p >= start && p < start + w.len())
        .filter_map(|&p| u32::try_from(p - start + 1).ok())
        .take(search::MAX_SPANS_PER_LINE)
        .collect();
    LineOut {
        line: raw.line,
        cols,
        text: w.as_bytes().to_vec(),
        before,
        after,
        redacted: false,
        provenance: prov,
    }
}

/// Renders search hits under `budget`. `input_truncated` is the executor's own `truncated`
/// (a window cut upstream); it is carried into the output.
pub fn render_search(
    ctx: &RenderCtx,
    budget: Budget,
    hits: &[RawFileHit],
    input_truncated: bool,
) -> RenderedSearch {
    let lim = budget.limits();
    let mut out = RenderedSearch { truncated: input_truncated, ..RenderedSearch::default() };
    // One FileId is one file, however many hits name it.
    let mut by_id: HashMap<FileId, usize> = HashMap::new();
    let mut merged: Vec<(&RawFileHit, Vec<&Vec<u8>>)> = Vec::new();
    for h in hits {
        match by_id.get(&h.file) {
            Some(&i) => merged[i].1.extend(&h.paths),
            None => {
                by_id.insert(h.file, merged.len());
                merged.push((h, h.paths.iter().collect()));
            }
        }
    }
    // The repo and changeset are counted once per response, not once per hit.
    let mut used = ctx.repo.len() + ctx.changeset.len();
    for (h, mut paths) in merged {
        if out.files.len() >= lim.results {
            out.truncated = true;
            break;
        }
        paths.sort();
        paths.dedup();
        let mut shown: Vec<Vec<u8>> = paths.iter().map(|p| clean_path(p)).collect();
        shown.dedup();
        if shown.is_empty() {
            continue;
        }
        let path = shown.remove(0);
        if shown.len() > MAX_ALSO_AT {
            out.truncated = true;
            shown.truncate(MAX_ALSO_AT);
        }
        let flagged = h.flagged || paths.iter().any(|p| path_flagged(p));
        let mut f = FileOut {
            file_id: *h.file.as_bytes(),
            lang: clean_text(&h.lang, 32),
            provenance: Provenance {
                repo: ctx.repo.clone(),
                changeset: ctx.changeset.clone(),
                path: path.clone(),
                start_line: 0,
                end_line: 0,
                file_id: Some(*h.file.as_bytes()),
            },
            path,
            also_at: shown,
            matches: Vec::new(),
            redacted: flagged,
        };
        if used + file_head_cost(&f) > lim.bytes {
            out.truncated = true;
            break;
        }
        used += file_head_cost(&f);
        if !flagged {
            let primary = f.path.clone();
            // In line order, so a block opened above a later match is still open there.
            let mut order: Vec<&RawLine> = h.lines.iter().collect();
            order.sort_by_key(|l| l.line);
            let mut block = Block::default();
            for raw in order {
                let l = render_line(ctx, &h.file, &primary, raw, &mut block);
                let cost = line_cost(&l);
                if used + cost > lim.bytes {
                    out.truncated = true;
                    break;
                }
                used += cost;
                f.redacted |= l.redacted;
                f.matches.push(l);
            }
        }
        let (lo, hi) = (
            f.matches.iter().map(|m| m.line).min().unwrap_or(0),
            f.matches.iter().map(|m| m.line).max().unwrap_or(0),
        );
        f.provenance.start_line = lo;
        f.provenance.end_line = hi;
        out.files.push(f);
        if out.truncated {
            break;
        }
    }
    out.est_tokens = est_tokens(used);
    out
}

// ---------------------------------------------------------------- symbols

/// A rendered symbol (the `Symbol` message plus `redacted` and provenance).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolOut {
    pub name: String,
    pub kind: String,
    pub is_def: bool,
    pub path: Vec<u8>,
    pub line: u32,
    pub end_line: u32,
    pub signature: Vec<u8>,
    pub container: String,
    pub doc: Vec<u8>,
    pub confidence: String,
    pub redacted: bool,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedSymbols {
    pub symbols: Vec<SymbolOut>,
    pub truncated: bool,
    pub est_tokens: u32,
}

fn text_field(raw: &[u8], block: &mut Block) -> (Vec<u8>, bool) {
    // Signatures and docs can span lines; every line is scanned.
    let mut out = Vec::new();
    let mut flagged = false;
    for (i, l) in raw[..raw.len().min(MAX_LINE_IN * 4)].split(|&b| b == b'\n').enumerate() {
        if i >= 20 {
            break;
        }
        let (b, f) = body_line(l, block);
        flagged |= f;
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(&b);
    }
    (out, flagged)
}

/// Renders symbols under `budget`. `flagged_paths` lists paths whose file the caller knows
/// to be flagged; paths matching the path rules are always flagged.
pub fn render_symbols(
    ctx: &RenderCtx,
    budget: Budget,
    symbols: &[crate::symbols::Symbol],
    flagged_paths: &[Vec<u8>],
    input_truncated: bool,
) -> RenderedSymbols {
    let lim = budget.limits();
    let mut out = RenderedSymbols { truncated: input_truncated, ..RenderedSymbols::default() };
    let mut used = ctx.repo.len() + ctx.changeset.len();
    for s in symbols {
        if out.symbols.len() >= lim.results {
            out.truncated = true;
            break;
        }
        let path = clean_path(&s.path);
        let file_flagged = path_flagged(&s.path) || flagged_paths.contains(&s.path);
        let (signature, doc, redacted) = if file_flagged {
            (Vec::new(), Vec::new(), true)
        } else {
            let mut block = Block::default();
            let (sig, a) = text_field(&s.signature, &mut block);
            let (doc, b) = text_field(&s.doc, &mut block);
            (sig, doc, a || b)
        };
        let o = SymbolOut {
            name: clean_text(&s.name, 256),
            kind: clean_text(&s.kind, 32),
            is_def: s.is_def,
            line: s.line,
            end_line: s.end_line,
            signature,
            container: clean_text(&s.container, 256),
            doc,
            confidence: s.confidence.to_owned(),
            redacted,
            provenance: Provenance {
                repo: ctx.repo.clone(),
                changeset: ctx.changeset.clone(),
                path: path.clone(),
                start_line: s.line,
                end_line: s.end_line.max(s.line),
                file_id: None,
            },
            path,
        };
        let cost = o.name.len()
            + o.kind.len()
            + o.path.len()
            + o.signature.len()
            + o.container.len()
            + o.doc.len()
            + o.confidence.len()
            + 16;
        if used + cost > lim.bytes {
            out.truncated = true;
            break;
        }
        used += cost;
        out.symbols.push(o);
    }
    out.est_tokens = est_tokens(used);
    out
}

// ---------------------------------------------------------------- chunks

/// A semantic chunk as the executor produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct RawChunk {
    pub path: Vec<u8>,
    pub file: FileId,
    pub start_line: u32,
    pub end_line: u32,
    pub symbol: String,
    pub score: f64,
    pub sources: Vec<String>,
    pub text: Vec<u8>,
    pub flagged: bool,
}

/// The `Chunk` message plus provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkOut {
    pub path: Vec<u8>,
    pub file_id: [u8; 32],
    pub start_line: u32,
    pub end_line: u32,
    pub symbol: String,
    pub score: f64,
    pub sources: Vec<String>,
    pub text: Vec<u8>,
    pub redacted: bool,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderedChunks {
    pub chunks: Vec<ChunkOut>,
    pub truncated: bool,
    pub est_tokens: u32,
}

/// Renders chunks under `budget`. A flagged line is replaced by [`REDACTED_TEXT`]; a flagged
/// file shows its path only. The last chunk that does not fit is cut at a line boundary.
pub fn render_chunks(
    ctx: &RenderCtx,
    budget: Budget,
    chunks: &[RawChunk],
    input_truncated: bool,
) -> RenderedChunks {
    let lim = budget.limits();
    let mut out = RenderedChunks { truncated: input_truncated, ..RenderedChunks::default() };
    let mut used = ctx.repo.len() + ctx.changeset.len();
    // Block state entering each chunk: the chunks of one file are walked in line order, so a
    // key split across two chunks is redacted in both, whatever order they are ranked in.
    let mut entry = vec![false; chunks.len()];
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by_key(|&i| (*chunks[i].file.as_bytes(), chunks[i].start_line));
    let mut walk: Option<(FileId, Block)> = None;
    for i in order {
        let c = &chunks[i];
        let block = match &mut walk {
            Some((f, b)) if *f == c.file => b,
            slot => &mut slot.insert((c.file, Block::default())).1,
        };
        entry[i] = block.open;
        for l in c.text.split(|&b| b == b'\n') {
            clean_line(l, &[], block);
        }
    }
    for (n, c) in chunks.iter().enumerate() {
        if out.chunks.len() >= lim.results {
            out.truncated = true;
            break;
        }
        let path = clean_path(&c.path);
        let mut redacted = c.flagged || path_flagged(&c.path);
        let sources: Vec<String> = c.sources.iter().take(8).map(|s| clean_text(s, 32)).collect();
        let head = path.len()
            + 32
            + 24
            + c.symbol.len().min(256)
            + sources.iter().map(|s| s.len() + 1).sum::<usize>();
        if used + head > lim.bytes {
            out.truncated = true;
            break;
        }
        let mut text: Vec<u8> = Vec::new();
        let mut cut = false;
        if !redacted {
            let mut block = Block { open: entry[n] };
            let mut spent = used + head;
            for (i, l) in c.text.split(|&b| b == b'\n').enumerate() {
                if i >= MAX_CHUNK_LINES {
                    cut = true;
                    break;
                }
                let (b, f) = body_line(l, &mut block);
                if spent + b.len() + 1 > lim.bytes {
                    cut = true;
                    break;
                }
                spent += b.len() + 1;
                redacted |= f;
                if i > 0 {
                    text.push(b'\n');
                }
                text.extend_from_slice(&b);
            }
        }
        used += head + text.len() + 1;
        out.truncated |= cut;
        out.chunks.push(ChunkOut {
            file_id: *c.file.as_bytes(),
            start_line: c.start_line,
            end_line: c.end_line,
            symbol: clean_text(&c.symbol, 256),
            score: c.score,
            sources,
            text,
            redacted,
            provenance: Provenance {
                repo: ctx.repo.clone(),
                changeset: ctx.changeset.clone(),
                path: path.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                file_id: Some(*c.file.as_bytes()),
            },
            path,
        });
        if cut {
            break;
        }
    }
    out.est_tokens = est_tokens(used);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWS: &str = "let k = \"AKIAIOSFODNN7EXAMPLE\";";

    fn ctx() -> RenderCtx {
        RenderCtx { repo: "acme/app".into(), changeset: vec![7; 32] }
    }

    fn fid(n: u8) -> FileId {
        FileId::from_bytes([n; 32])
    }

    fn raw_line(n: u32, text: &str) -> RawLine {
        let at = text.find("needle").unwrap_or(0) as u32;
        RawLine { line: n, text: text.as_bytes().to_vec(), spans: vec![(at, at + 6)], ..RawLine::default() }
    }

    fn hit(n: u8, paths: &[&str], lines: Vec<RawLine>) -> RawFileHit {
        RawFileHit {
            file: fid(n),
            lang: "rust".into(),
            paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
            lines,
            flagged: false,
        }
    }

    #[test]
    fn aws_key_line_is_absent() {
        let mut l = raw_line(3, AWS);
        l.before = vec![b"// ok".to_vec(), AWS.as_bytes().to_vec()];
        l.after = vec![AWS.as_bytes().to_vec()];
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["src/a.rs"], vec![l])], false);
        let f = &r.files[0];
        assert!(f.redacted);
        let m = &f.matches[0];
        assert!(m.redacted && m.text.is_empty() && m.cols.is_empty());
        assert_eq!((m.line, f.path.as_slice()), (3, b"src/a.rs".as_slice()));
        let dump = format!("{r:?}");
        assert!(!dump.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(m.before[1], REDACTED_TEXT);
        assert_eq!(m.after[0], REDACTED_TEXT);
        assert_eq!(m.before[0], b"// ok");
    }

    #[test]
    fn secret_path_shows_path_only() {
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["cfg/.env"], vec![raw_line(1, "x=1")])], false);
        assert!(r.files[0].redacted && r.files[0].matches.is_empty());
        assert_eq!(r.files[0].path, b"cfg/.env");
        // A secret alias flags the whole file.
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["a.txt", "b/.env"], vec![raw_line(1, "x=1")])], false);
        assert!(r.files[0].redacted && r.files[0].matches.is_empty());
    }

    #[test]
    fn controls_and_ansi_are_stripped() {
        let raw = "a\x1b[31mred\x1b[0m\x07b\x1b]0;title\x07c\td\r\u{202e}e\u{200b}f\x00g";
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["a"], vec![raw_line(1, raw)])], false);
        assert_eq!(r.files[0].matches[0].text, b"aredbc defg");
    }

    #[test]
    fn invalid_utf8_and_cols() {
        let mut l = RawLine { line: 2, text: b"\x1b[1mab\xffneedle".to_vec(), spans: vec![(7, 13)], ..RawLine::default() };
        l.spans = vec![(7, 13)];
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["a"], vec![l])], false);
        let m = &r.files[0].matches[0];
        assert_eq!(String::from_utf8_lossy(&m.text), "ab\u{fffd}needle");
        // "ab" (2) + U+FFFD (3 bytes) = 5 bytes before the match; 1-based column 6.
        assert_eq!(m.cols, vec![6]);
    }

    #[test]
    fn long_lines_cut_around_the_match() {
        let text = format!("{}needle{}", "x".repeat(900), "y".repeat(900));
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["a"], vec![raw_line(1, &text)])], false);
        let m = &r.files[0].matches[0];
        assert!(m.text.len() <= MAX_LINE_OUT);
        let at = m.cols[0] as usize - 1;
        assert_eq!(&m.text[at..at + 6], b"needle");
        // Multi-byte text is cut on a boundary.
        let text = format!("{}needle", "é".repeat(500));
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["a"], vec![raw_line(1, &text)])], false);
        assert!(std::str::from_utf8(&r.files[0].matches[0].text).is_ok());
    }

    #[test]
    fn also_at_dedups_and_caps() {
        let paths: Vec<String> = (0..9).map(|i| format!("d/f{i}.rs")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let r = render_search(
            &ctx(),
            Budget::default(),
            &[hit(1, &refs[..4], vec![raw_line(1, "needle")]), hit(1, &refs[4..], vec![])],
            false,
        );
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].path, b"d/f0.rs");
        assert_eq!(r.files[0].also_at.len(), MAX_ALSO_AT);
        assert!(r.truncated);
        assert!(!r.files[0].also_at.contains(&b"d/f0.rs".to_vec()));
    }

    #[test]
    fn provenance_on_every_hit() {
        let r = render_search(&ctx(), Budget::default(), &[hit(4, &["a/b.rs"], vec![raw_line(8, "needle"), raw_line(12, "needle")])], false);
        let f = &r.files[0];
        assert_eq!(f.provenance.repo, "acme/app");
        assert_eq!(f.provenance.changeset, vec![7; 32]);
        assert_eq!((f.provenance.start_line, f.provenance.end_line), (8, 12));
        assert_eq!(f.provenance.file_id, Some([4; 32]));
        for m in &f.matches {
            assert_eq!(m.provenance.path, b"a/b.rs");
            assert_eq!(m.provenance.start_line, m.line);
            assert_eq!(m.provenance.file_id, Some([4; 32]));
        }
    }

    #[test]
    fn budget_bounds_output() {
        let lines: Vec<RawLine> = (1..=200).map(|n| raw_line(n, &"z".repeat(200))).collect();
        let hits: Vec<RawFileHit> = (0..50).map(|i| hit(i, &["p/file.rs"], lines.clone())).collect();
        let small: Vec<RawFileHit> = (0..50).map(|i| hit(i, &["p/file.rs"], lines[..2].to_vec())).collect();
        let b = Budget { max_tokens: 500, ..Budget::default() };
        let r = render_search(&ctx(), b, &hits, false);
        assert!(r.truncated);
        assert!(r.est_tokens <= 500, "{}", r.est_tokens);
        assert!(!r.files.is_empty());
        // The hard cap wins over a bigger request.
        let r = render_search(&ctx(), Budget { max_tokens: 1_000_000, ..Budget::default() }, &hits, false);
        assert!(r.est_tokens <= MAX_TOKENS);
        // max_results and max_bytes.
        let r = render_search(&ctx(), Budget { max_results: 2, ..Budget::default() }, &small, false);
        assert_eq!(r.files.len(), 2);
        let r = render_search(&ctx(), Budget { max_bytes: 300, ..Budget::default() }, &hits, false);
        assert!(r.est_tokens <= 100);
        assert_eq!(est_tokens(0), 0);
        assert_eq!(est_tokens(1), 1);
        assert_eq!(est_tokens(3), 1);
        assert_eq!(est_tokens(4), 2);
    }

    #[test]
    fn symbols_redact_and_sanitise() {
        let s = |path: &str, sig: &str| crate::symbols::Symbol {
            name: "f\x1b[0m".into(),
            kind: "fn".into(),
            is_def: true,
            path: path.as_bytes().to_vec(),
            line: 4,
            end_line: 6,
            signature: sig.as_bytes().to_vec(),
            container: String::new(),
            doc: AWS.as_bytes().to_vec(),
            confidence: "high",
        };
        let r = render_symbols(&ctx(), Budget::default(), &[s("a.rs", "fn f()"), s("x/.env", "fn g()")], &[], false);
        assert_eq!(r.symbols[0].name, "f");
        assert_eq!(r.symbols[0].signature, b"fn f()");
        assert!(r.symbols[0].redacted);
        assert_eq!(r.symbols[0].doc, REDACTED_TEXT);
        assert!(r.symbols[1].redacted && r.symbols[1].signature.is_empty() && r.symbols[1].doc.is_empty());
        assert_eq!(r.symbols[1].provenance.start_line, 4);
        assert!(!format!("{r:?}").contains("AKIA"));
    }

    #[test]
    fn chunks_redact_flagged_lines_and_files() {
        let c = |text: &str, flagged: bool| RawChunk {
            path: b"src/a.rs".to_vec(),
            file: fid(2),
            start_line: 1,
            end_line: 3,
            symbol: "f".into(),
            score: 0.5,
            sources: vec!["vector".into()],
            text: text.as_bytes().to_vec(),
            flagged,
        };
        let text = format!("fn f() {{\n{AWS}\n}}");
        let r = render_chunks(&ctx(), Budget::default(), &[c(&text, false), c("x", true)], false);
        assert!(r.chunks[0].redacted);
        assert!(!format!("{r:?}").contains("AKIA"));
        assert_eq!(r.chunks[0].text, [b"fn f() {\n".as_slice(), REDACTED_TEXT, b"\n}"].concat());
        assert!(r.chunks[1].redacted && r.chunks[1].text.is_empty());
        let big = "w".repeat(200) + "\n";
        let r = render_chunks(&ctx(), Budget { max_tokens: 100, ..Budget::default() }, &[c(&big.repeat(50), false)], false);
        assert!(r.truncated && r.est_tokens <= 100);
    }

    #[test]
    fn from_search_maps_fields() {
        use asc_core::path::RepoPath;
        let h = search::FileHit {
            ord: 1,
            file: fid(9),
            paths: vec![RepoPath::new("a/b.rs").unwrap()],
            lines: vec![search::LineMatch { line: 5, text: b"needle".to_vec(), spans: vec![(0, 6)] }],
            more_lines: false,
        };
        let r = render_search(&ctx(), Budget::default(), &[RawFileHit::from_search(&h, Some("rust"), Some(false))], false);
        assert_eq!(r.files[0].lang, "rust");
        assert_eq!(r.files[0].matches[0].cols, vec![1]);
    }

    #[test]
    fn from_search_fails_closed_without_a_secret_flag() {
        use asc_core::path::RepoPath;
        let h = search::FileHit {
            ord: 1,
            file: fid(9),
            paths: vec![RepoPath::new("a/b.rs").unwrap()],
            lines: vec![search::LineMatch { line: 5, text: b"harmless needle".to_vec(), spans: vec![(9, 15)] }],
            more_lines: false,
        };
        for (flag, shown) in [(None, false), (Some(true), false), (Some(false), true)] {
            let raw = RawFileHit::from_search(&h, None, flag);
            assert_eq!(raw.flagged, !shown);
            let r = render_search(&ctx(), Budget::default(), &[raw], false);
            assert_eq!(r.files[0].redacted, !shown);
            assert_eq!(!r.files[0].matches.is_empty(), shown, "{flag:?}");
        }
    }

    const PEM_BEGIN: &str = "-----BEGIN RSA PRIVATE KEY-----";
    const PEM_END: &str = "-----END RSA PRIVATE KEY-----";
    const BODY1: &str = "MIIEowIBAAKCAQEAyj1ZsecretBODYlineONE";
    const BODY2: &str = "Zm9vYmFyYmF6secretBODYlineTWO";

    fn chunk(file: u8, start: u32, text: &str) -> RawChunk {
        RawChunk {
            path: b"src/k.rs".to_vec(),
            file: fid(file),
            start_line: start,
            end_line: start + text.lines().count() as u32,
            symbol: String::new(),
            score: 0.5,
            sources: vec![],
            text: text.as_bytes().to_vec(),
            flagged: false,
        }
    }

    #[test]
    fn pem_block_spanning_lines_is_fully_redacted() {
        let text = format!("fn f() {{\n{PEM_BEGIN}\n{BODY1}\n{BODY2}\n{PEM_END}\nlet after = 1;\n}}");
        let r = render_chunks(&ctx(), Budget::default(), &[chunk(1, 1, &text)], false);
        let t = String::from_utf8_lossy(&r.chunks[0].text).into_owned();
        assert!(r.chunks[0].redacted);
        assert!(!t.contains("secretBODY") && !t.contains("PRIVATE KEY"), "{t}");
        assert!(t.contains("fn f()") && t.contains("let after = 1;"), "{t}");
        // An unterminated block redacts to the end.
        let text = format!("a\n{PEM_BEGIN}\n{BODY1}\n{BODY2}");
        let r = render_chunks(&ctx(), Budget::default(), &[chunk(1, 1, &text)], false);
        let t = String::from_utf8_lossy(&r.chunks[0].text).into_owned();
        assert!(!t.contains("secretBODY"), "{t}");
        assert!(t.starts_with("a\n"));
    }

    #[test]
    fn pem_block_split_across_chunks_is_redacted_in_both() {
        let first = format!("// keys\n{PEM_BEGIN}\n{BODY1}");
        let second = format!("{BODY2}\n{PEM_END}\nlet ok = 2;");
        for chunks in [
            [chunk(1, 1, &first), chunk(1, 4, &second)],
            // Ranked the other way round, the later chunk comes first.
            [chunk(1, 4, &second), chunk(1, 1, &first)],
        ] {
            let r = render_chunks(&ctx(), Budget::default(), &chunks, false);
            let dump = format!("{r:?}");
            assert!(!dump.contains("secretBODY"), "{dump}");
            let later = r.chunks.iter().find(|c| c.start_line == 4).unwrap();
            assert!(String::from_utf8_lossy(&later.text).ends_with("let ok = 2;"));
            assert!(later.redacted);
        }
        // A different file's chunk is not affected.
        let r = render_chunks(&ctx(), Budget::default(), &[chunk(1, 1, &first), chunk(2, 4, &second)], false);
        assert!(String::from_utf8_lossy(&r.chunks[1].text).contains("secretBODY"));
    }

    #[test]
    fn pem_block_in_search_context_and_symbols_is_redacted() {
        // Begin in the match line's trailing context, body in the next match.
        let mut a = raw_line(2, "needle");
        a.after = vec![PEM_BEGIN.as_bytes().to_vec(), BODY1.as_bytes().to_vec()];
        let mut b = raw_line(9, BODY2);
        b.before = vec![b"still inside".to_vec()];
        b.after = vec![PEM_END.as_bytes().to_vec(), b"after".to_vec()];
        let r = render_search(&ctx(), Budget::default(), &[hit(1, &["a.rs"], vec![b, a])], false);
        let dump = format!("{r:?}");
        assert!(!dump.contains("secretBODY") && !dump.contains("still inside"), "{dump}");
        let m = &r.files[0].matches;
        assert_eq!(m[0].line, 2);
        assert!(m[1].redacted && m[1].text.is_empty());
        assert_eq!(m[1].after[1], b"after");
        let s = crate::symbols::Symbol {
            name: "k".into(),
            kind: "const".into(),
            is_def: true,
            path: b"k.rs".to_vec(),
            line: 1,
            end_line: 1,
            signature: format!("{PEM_BEGIN}\n{BODY1}").into_bytes(),
            container: String::new(),
            doc: format!("{BODY2}\n{PEM_END}\nplain").into_bytes(),
            confidence: "high",
        };
        let r = render_symbols(&ctx(), Budget::default(), &[s], &[], false);
        assert!(!format!("{r:?}").contains("secretBODY"));
        assert!(r.symbols[0].doc.ends_with(b"plain"));
    }

    #[test]
    fn crafted_escape_sequences_never_panic() {
        let mut inputs: Vec<Vec<u8>> = [
            "\x1b",
            "\x1b[",
            "a\x1b[",
            "\x1b[\x1b[",
            "\x1b[31",
            "ab\x1b[31;1;",
            "é\x1b[é",
            "\x1b]0;title",
            "\x1b]0;title\x1b",
            "x\x1bP",
            "\x1b^\x1b\\y",
            "\x1b_",
            "\u{202e}\x1b[",
            "\x1b\u{1f600}",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        inputs.push([b"\x1b[".as_slice(), "0123456789;".repeat(2000).as_bytes()].concat());
        inputs.push([b"needle\x1b[".as_slice(), "9;".repeat(5000).as_bytes()].concat());
        inputs.push(b"\xff\x1b[\xc3".to_vec());
        inputs.push(b"\xe2\x82\x1b[\xf0\x9f".to_vec());
        // Every string over a small alphabet up to length 5, then a pseudo-random tail.
        let alpha: [&[u8]; 11] = [
            b"\x1b", b"[", b"]", b"P", b";", b"1", b"m", b"\x07", b"\\", "é".as_bytes(), b"\xff",
        ];
        let mut idx = vec![0usize; 0];
        for len in 1..=5usize {
            idx.clear();
            idx.resize(len, 0);
            loop {
                inputs.push(idx.iter().flat_map(|&i| alpha[i].iter().copied()).collect());
                let Some(k) = (0..len).rev().find(|&k| idx[k] + 1 < alpha.len()) else { break };
                idx[k] += 1;
                idx[k + 1..].fill(0);
            }
        }
        let mut x = 0x2545_f491_u64;
        for _ in 0..2000 {
            let mut v = Vec::new();
            for _ in 0..(x % 40) {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                v.extend_from_slice(alpha[(x >> 33) as usize % alpha.len()]);
            }
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            inputs.push(v);
        }
        for raw in &inputs {
            // Match starts at every offset, inside and beyond the input.
            let spans: Vec<(u32, u32)> = (0..=raw.len() as u32 + 2).map(|s| (s, s + 1)).collect();
            let line = RawLine {
                line: 1,
                text: raw.clone(),
                spans,
                before: vec![raw.clone()],
                after: vec![raw.clone()],
            };
            let h = hit(1, &["a.rs"], vec![line]);
            let r = render_search(&ctx(), Budget::default(), &[h], false);
            for m in &r.files[0].matches {
                assert!(!m.text.contains(&0x1b));
                assert!(m.cols.iter().all(|&c| c as usize <= m.text.len() + 1));
            }
            let (t, cols) = clean(raw, &[0, 1, u32::MAX, raw.len() as u32]);
            assert!(cols.iter().all(|&c| c <= t.len()));
            assert!(!clean_path(raw).contains(&0x1b));
            let _ = body_line(raw, &mut Block::default());
            let _ = render_chunks(&ctx(), Budget::default(), &[chunk(1, 1, &String::from_utf8_lossy(raw))], false);
        }
    }
}
