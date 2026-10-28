//! TSRX (<https://tsrx.dev/>) source analysis: the layer between a `.tsrx` file and the
//! TypeScript grammar.
//!
//! TSRX is TypeScript plus a template syntax. A function whose body opens with `@{` is a
//! component; inside the container, ordinary TypeScript setup comes first and one output node
//! follows (an element, a fragment or a control-flow form: `@if`/`@else`, `@for`/`@empty`,
//! `@switch`/`@case`/`@default`, `@try`/`@pending`/`@catch`). The TypeScript grammar cannot
//! parse that, and parsing it as TSX would be wrong (`@{` is not a decorator, `@for` is not
//! JSX). So this module does what a grammar layer does:
//!
//! 1. it scans the file with a small, bounded, error-tolerant scanner that knows strings,
//!    template literals, comments, regular expressions, brackets, elements, attributes, the
//!    control-flow forms, raw `<style>`/`<script>` bodies and nested `@{` containers;
//! 2. it produces [`Analysis::masked`], the same bytes with every template region blanked to
//!    spaces (newlines kept) and each `@{` turned into ` {`, so the TSX grammar parses what is
//!    left (imports, exports, types, functions, setup statements) and every byte offset and row
//!    of the original file stays valid;
//! 3. it reports what only it can see: the component containers, the control-flow scopes, the
//!    names defined in them (loop variables, block locals, style classes) and the names the
//!    template uses (component tags, identifiers in expressions, attributes and headers).
//!
//! The scanner never panics, never recurses deeper than [`MAX_DEPTH`] and stops after
//! [`MAX_STEPS`] bytes of work, so hostile input costs bounded time. A construct it cannot
//! close ends at the end of the file and is reported in [`Analysis::diagnostics`].

/// Deepest nesting the scanner follows.
pub const MAX_DEPTH: usize = 48;
/// Most items (scopes, definitions, references) one file reports.
pub const MAX_ITEMS: usize = 20_000;

/// A byte range of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// The control-flow forms with a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    If,
    Else,
    For,
    Empty,
    Switch,
    Case,
    Default,
    Try,
    Pending,
    Catch,
}

impl ScopeKind {
    /// The keyword, without the `@`.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::If => "if",
            Self::Else => "else",
            Self::For => "for",
            Self::Empty => "empty",
            Self::Switch => "switch",
            Self::Case => "case",
            Self::Default => "default",
            Self::Try => "try",
            Self::Pending => "pending",
            Self::Catch => "catch",
        }
    }
}

/// A control-flow body: `@for (...) { ... }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    /// From the `@` to the closing `}`.
    pub span: Span,
    /// The header text, trimmed and cut: `@for (const user of users)`.
    pub label: String,
}

/// What an [`Item`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// Defined by `@for (const x of ...)`, its `index` option or a `catch (e)` parameter.
    LoopVar,
    /// A `const`/`let`/`var` of a template block's setup.
    Local,
    /// A class selector of a `<style>` block.
    StyleClass,
    /// A component tag: `<UserList ...>`, `<Ui.Button>`.
    ComponentUse,
    /// An identifier read in an expression, an attribute or a header.
    ValueUse,
}

/// A name and where it sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub kind: ItemKind,
    pub span: Span,
}

/// Something the scanner could not close or accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub at: usize,
    pub message: &'static str,
}

/// The result of [`analyze`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    /// The source with template regions blanked, as long as the source.
    pub masked: Vec<u8>,
    /// Every `@{ ... }` container, from the `@` to the closing `}`, outermost first.
    pub containers: Vec<Span>,
    pub scopes: Vec<Scope>,
    pub defs: Vec<Item>,
    pub uses: Vec<Item>,
    pub diagnostics: Vec<Diagnostic>,
    /// The work budget, the deadline or an item limit stopped the scan: the lists are partial and
    /// the masked text is blanked only as far as the scan got.
    pub truncated: bool,
}

/// Most `@{` containers one file reports.
pub const MAX_CONTAINERS: usize = 10_000;
/// Bytes of work between two looks at the clock.
const CLOCK_EVERY: usize = 8_192;

/// Analyses `src`. Total: any bytes give an answer.
pub fn analyze(src: &[u8]) -> Analysis {
    analyze_within(src, None)
}

/// Like [`analyze`], and stops at `deadline` (the same one the parse runs under) with a partial,
/// `truncated` answer. The work is also bounded by the size of the input, whatever the clock says.
pub fn analyze_within(src: &[u8], deadline: Option<std::time::Instant>) -> Analysis {
    let mut s = Scan {
        src,
        a: Analysis {
            masked: src.to_vec(),
            ..Analysis::default()
        },
        steps: 0,
        exhausted: false,
        deadline,
        next_clock: CLOCK_EVERY,
    };
    s.ts(0, None, false, 0);
    if s.exhausted {
        s.a.truncated = true;
    }
    s.a.containers.sort_by_key(|c| (c.start, std::cmp::Reverse(c.end)));
    s.a
}

/// Words that are never a use of a name.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
    "delete", "do", "else", "enum", "export", "extends", "false", "finally", "for", "from",
    "function", "if", "import", "in", "index", "instanceof", "interface", "key", "let", "new",
    "null", "of", "return", "satisfies", "static", "super", "switch", "this", "throw", "true",
    "try", "type", "typeof", "undefined", "var", "void", "while", "yield",
];

struct Scan<'a> {
    src: &'a [u8],
    a: Analysis,
    steps: usize,
    /// The budget or the deadline ran out; every loop that charges work stops from then on.
    exhausted: bool,
    deadline: Option<std::time::Instant>,
    next_clock: usize,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

impl Scan<'_> {
    fn at(&self, i: usize) -> u8 {
        self.src.get(i).copied().unwrap_or(0)
    }

    fn starts(&self, i: usize, pat: &[u8]) -> bool {
        self.src.get(i..).is_some_and(|r| r.starts_with(pat))
    }

    /// Charges `n` bytes of work; true once the budget or the deadline is gone (and from then on).
    fn spent(&mut self, n: usize) -> bool {
        if self.exhausted {
            return true;
        }
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.src.len().saturating_mul(16).saturating_add(4096) {
            self.exhausted = true;
        } else if self.steps >= self.next_clock {
            self.next_clock = self.steps.saturating_add(CLOCK_EVERY);
            if self.deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                self.exhausted = true;
            }
        }
        self.exhausted
    }

    fn diag(&mut self, at: usize, message: &'static str) {
        if message == "nesting is too deep" {
            self.a.truncated = true;
        }
        if self.a.diagnostics.len() < 64 {
            self.a.diagnostics.push(Diagnostic { at, message });
        }
    }

    fn push_container(&mut self, sp: Span) {
        if self.a.containers.len() < MAX_CONTAINERS {
            self.a.containers.push(sp);
        } else {
            self.a.truncated = true;
        }
    }

    fn room(&self) -> bool {
        self.a.scopes.len() + self.a.defs.len() + self.a.uses.len() < MAX_ITEMS
    }

    /// Blanks `a..b` (newlines stay so rows do not move).
    fn blank(&mut self, a: usize, b: usize) {
        let b = b.min(self.a.masked.len());
        for x in &mut self.a.masked[a.min(b)..b] {
            if *x != b'\n' && *x != b'\r' {
                *x = b' ';
            }
        }
    }

    /// Blanks `a..b` where an expression stood: `null` keeps the TypeScript valid.
    fn blank_expr(&mut self, a: usize, b: usize) {
        self.blank(a, b);
        if b.saturating_sub(a) >= 4 && self.a.masked[a..a + 4].iter().all(|c| *c == b' ') {
            self.a.masked[a..a + 4].copy_from_slice(b"null");
        }
    }

    fn skip_ws(&self, mut i: usize) -> usize {
        while i < self.src.len() && self.src[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }

    /// Skips whitespace and comments.
    fn skip_trivia(&mut self, mut i: usize) -> usize {
        loop {
            i = self.skip_ws(i);
            if self.starts(i, b"//") {
                i = self.line_end(i);
            } else if self.starts(i, b"/*") {
                i = self.block_comment_end(i);
            } else {
                return i;
            }
        }
    }

    fn line_end(&self, mut i: usize) -> usize {
        while i < self.src.len() && self.src[i] != b'\n' {
            i += 1;
        }
        i
    }

    fn block_comment_end(&self, i: usize) -> usize {
        let mut j = i + 2;
        while j < self.src.len() {
            if self.src[j] == b'*' && self.at(j + 1) == b'/' {
                return j + 2;
            }
            j += 1;
        }
        self.src.len()
    }

    /// The end of the string literal at `i` (a quote).
    fn string_end(&self, i: usize) -> usize {
        let q = self.src[i];
        let mut j = i + 1;
        while j < self.src.len() {
            match self.src[j] {
                b'\\' => j += 2,
                c if c == q => return j + 1,
                b'\n' => return j,
                _ => j += 1,
            }
        }
        self.src.len()
    }

    /// The end of the template literal at `i` (a backtick); `${ }` parts are scanned as code.
    fn template_end(&mut self, i: usize, depth: usize) -> usize {
        let mut j = i + 1;
        while j < self.src.len() {
            match self.src[j] {
                b'\\' => j += 2,
                b'`' => return j + 1,
                b'$' if self.at(j + 1) == b'{' => {
                    let from = j + 2;
                    j = self.ts(from, Some(b'}'), false, depth + 1);
                    self.uses_in(from, j.saturating_sub(1));
                }
                _ => j += 1,
            }
        }
        self.src.len()
    }

    /// The end of the regular expression literal at `i` (a slash).
    fn regex_end(&self, i: usize) -> Option<usize> {
        let mut j = i + 1;
        let mut class = false;
        while j < self.src.len() {
            match self.src[j] {
                b'\\' => j += 1,
                b'\n' => return None,
                b'[' => class = true,
                b']' => class = false,
                b'/' if !class => {
                    j += 1;
                    while j < self.src.len() && self.src[j].is_ascii_alphabetic() {
                        j += 1;
                    }
                    return Some(j);
                }
                _ => {}
            }
            j += 1;
        }
        None
    }

    // ---------------------------------------------------------------------- TypeScript level

    /// Scans TypeScript from `i` up to and including the `closer` that ends it (the end of the
    /// file when `closer` is `None`) and returns the index after it. With `in_container`, a
    /// statement may be a template item (an element or a control-flow form). A `@{` anywhere
    /// opens a component container.
    fn ts(&mut self, mut i: usize, closer: Option<u8>, in_container: bool, depth: usize) -> usize {
        let mut prev = 0u8;
        let mut newline = false;
        while i < self.src.len() {
            if self.spent(1) || depth > MAX_DEPTH {
                if depth > MAX_DEPTH {
                    self.diag(i, "nesting is too deep");
                }
                return self.src.len();
            }
            let c = self.src[i];
            if c == b'\n' {
                newline = true;
                i += 1;
                continue;
            }
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if self.starts(i, b"//") {
                i = self.line_end(i);
                continue;
            }
            if self.starts(i, b"/*") {
                i = self.block_comment_end(i);
                continue;
            }
            if Some(c) == closer {
                return i + 1;
            }
            let stmt_start = matches!(prev, 0 | b';' | b'{' | b'}')
                || (newline && (is_ident(prev) || matches!(prev, b')' | b']' | b'\'' | b'"' | b'`')));
            match c {
                b'\'' | b'"' => {
                    i = self.string_end(i);
                    prev = c;
                }
                b'`' => {
                    i = self.template_end(i, depth);
                    prev = b'`';
                }
                b'/' => {
                    if matches!(prev, 0 | b'(' | b',' | b'=' | b':' | b'[' | b'!' | b'&' | b'|' | b'?' | b'{' | b'}' | b';')
                        && let Some(e) = self.regex_end(i)
                    {
                        i = e;
                        prev = b'/';
                    } else {
                        i += 1;
                        prev = b'/';
                    }
                }
                b'@' if self.at(i + 1) == b'{' => {
                    self.a.masked[i] = b' ';
                    let end = self.ts(i + 2, Some(b'}'), true, depth + 1);
                    self.push_container(Span { start: i, end });
                    i = end;
                    prev = b'}';
                }
                b'@' if in_container
                    && stmt_start
                    && matches!(self.keyword(i + 1), Some("if" | "for" | "switch" | "try")) =>
                {
                    let end = self.control(i, depth + 1);
                    self.blank(i, end);
                    i = end;
                    prev = b';';
                }
                b'<' if (in_container && stmt_start || self.style_expr(i, prev)) && self.opens_element(i) => {
                    let end = self.element(i, depth + 1);
                    if in_container && stmt_start {
                        self.blank(i, end);
                    } else {
                        self.blank_expr(i, end);
                    }
                    i = end;
                    prev = b';';
                }
                b'{' => {
                    i = self.ts(i + 1, Some(b'}'), false, depth + 1);
                    prev = b'}';
                }
                b'(' => {
                    i = self.ts(i + 1, Some(b')'), false, depth + 1);
                    prev = b')';
                }
                b'[' => {
                    i = self.ts(i + 1, Some(b']'), false, depth + 1);
                    prev = b']';
                }
                _ => {
                    prev = c;
                    i += 1;
                }
            }
            newline = false;
        }
        if closer.is_some() {
            self.diag(self.src.len(), "unterminated block");
        }
        self.src.len()
    }

    /// `<style` in an expression position (`const theme = <style>...</style>;`).
    fn style_expr(&self, i: usize, prev: u8) -> bool {
        self.starts(i, b"<style")
            && matches!(self.at(i + 6), b' ' | b'>' | b'/' | b'\n' | b'\t' | b'\r')
            && matches!(prev, b'=' | b'(' | b',' | b':' | b'?' | b'>')
    }

    /// Whether the `<` at `i` opens an element, a fragment or a dynamic tag.
    fn opens_element(&self, i: usize) -> bool {
        let n = self.skip_ws(i + 1);
        let c = self.at(n);
        is_ident_start(c) || c == b'>' || c == b'{'
    }

    /// The keyword at `i` (after an `@`), when it is one of the TSRX words.
    fn keyword(&self, i: usize) -> Option<&'static str> {
        [
            "if", "else", "for", "empty", "switch", "case", "default", "try", "pending", "catch",
        ]
        .into_iter()
        .find(|k| self.starts(i, k.as_bytes()) && !is_ident(self.at(i + k.len())))
    }

    // ---------------------------------------------------------------------- templates

    /// An element, fragment or dynamic tag starting at the `<` at `i`; returns the index after it.
    fn element(&mut self, i: usize, depth: usize) -> usize {
        if depth > MAX_DEPTH {
            self.diag(i, "nesting is too deep");
            return self.src.len();
        }
        let mut j = self.skip_ws(i + 1);
        let mut name = String::new();
        if self.at(j) == b'>' {
            return self.children(j + 1, Until::CloseTag, depth + 1);
        }
        if self.at(j) == b'{' {
            let from = j + 1;
            j = self.ts(from, Some(b'}'), false, depth + 1);
            self.uses_in(from, j.saturating_sub(1));
        } else {
            let from = j;
            while j < self.src.len() && (is_ident(self.src[j]) || matches!(self.src[j], b'.' | b'-' | b':')) {
                j += 1;
            }
            name = String::from_utf8_lossy(&self.src[from..j]).into_owned();
            let head = name.split(['.', ':']).next().unwrap_or("");
            if head.bytes().next().is_some_and(|b| b.is_ascii_uppercase() || b == b'_' || b == b'$') && self.room() {
                self.a.uses.push(Item {
                    name: head.to_owned(),
                    kind: ItemKind::ComponentUse,
                    span: Span { start: from, end: from + head.len() },
                });
            }
        }
        // Attributes.
        loop {
            j = self.skip_trivia(j);
            if j >= self.src.len() || self.spent(1) {
                self.diag(j, "unterminated tag");
                return self.src.len();
            }
            match self.src[j] {
                b'/' if self.at(j + 1) == b'>' => return j + 2,
                b'>' => break,
                b'{' => {
                    let from = j + 1;
                    j = self.ts(from, Some(b'}'), false, depth + 1);
                    self.uses_in(from, j.saturating_sub(1));
                }
                _ => {
                    let from = j;
                    while j < self.src.len()
                        && !self.src[j].is_ascii_whitespace()
                        && !matches!(self.src[j], b'=' | b'/' | b'>' | b'{')
                    {
                        j += 1;
                    }
                    if j == from {
                        j += 1;
                    }
                    let k = self.skip_trivia(j);
                    if self.at(k) == b'=' {
                        j = self.skip_trivia(k + 1);
                        match self.at(j) {
                            b'"' | b'\'' => j = self.string_end(j),
                            b'`' => j = self.template_end(j, depth),
                            b'{' => {
                                let from = j + 1;
                                j = self.ts(from, Some(b'}'), false, depth + 1);
                                self.uses_in(from, j.saturating_sub(1));
                            }
                            b'<' => j = self.element(j, depth + 1),
                            _ => {
                                while j < self.src.len()
                                    && !self.src[j].is_ascii_whitespace()
                                    && !matches!(self.src[j], b'/' | b'>')
                                {
                                    j += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        // `j` is at the `>` that ends the opening tag.
        let body = j + 1;
        if name == "style" {
            return self.raw_body(body, b"</style", true);
        }
        if name == "script" {
            return self.raw_body(body, b"</script", false);
        }
        self.children(body, Until::CloseTag, depth + 1)
    }

    /// A raw-text body (`<style>`, `<script>`) from `i` to its closing tag; returns the index after it.
    fn raw_body(&mut self, i: usize, close: &[u8], css: bool) -> usize {
        let mut j = i;
        while j < self.src.len() && !self.starts(j, close) {
            j += 1;
        }
        self.spent(j - i);
        if css {
            self.style_classes(i, j);
        }
        if j >= self.src.len() {
            self.diag(i, "unterminated raw element");
            return self.src.len();
        }
        while j < self.src.len() && self.src[j] != b'>' {
            j += 1;
        }
        (j + 1).min(self.src.len())
    }

    /// The class selectors in the CSS `a..b`: outside declaration blocks only.
    fn style_classes(&mut self, a: usize, b: usize) {
        let mut stack: Vec<bool> = Vec::new(); // true = a declaration block
        let mut stmt_at_rule = false;
        let mut stmt_start = true;
        let mut j = a;
        while j < b {
            if self.spent(1) {
                return;
            }
            let c = self.src[j];
            let in_decl = stack.last().copied().unwrap_or(false);
            if self.starts(j, b"/*") {
                j = self.block_comment_end(j).min(b);
                continue;
            }
            match c {
                b'{' => {
                    if stack.len() >= MAX_DEPTH {
                        self.a.truncated = true;
                        return;
                    }
                    stack.push(in_decl || !stmt_at_rule);
                    stmt_start = true;
                    stmt_at_rule = false;
                }
                b'}' => {
                    stack.pop();
                    stmt_start = true;
                    stmt_at_rule = false;
                }
                b';' => {
                    stmt_start = true;
                    stmt_at_rule = false;
                }
                b'\'' | b'"' => {
                    j = self.string_end(j).min(b);
                    continue;
                }
                b'.' if !in_decl && j + 1 < b && is_ident_start(self.src[j + 1]) => {
                    let from = j + 1;
                    let mut e = from;
                    while e < b && (is_ident(self.src[e]) || self.src[e] == b'-') {
                        e += 1;
                    }
                    if self.room() {
                        self.a.defs.push(Item {
                            name: String::from_utf8_lossy(&self.src[from..e]).into_owned(),
                            kind: ItemKind::StyleClass,
                            span: Span { start: from, end: e },
                        });
                    }
                    j = e;
                    stmt_start = false;
                    continue;
                }
                c if c.is_ascii_whitespace() => {}
                b'@' if stmt_start => {
                    stmt_at_rule = true;
                    stmt_start = false;
                }
                _ => stmt_start = false,
            }
            j += 1;
        }
    }

    /// Template children from `i` until the closing tag (index after its `>`) or the `}` that ends
    /// a block (index after it).
    fn children(&mut self, mut i: usize, until: Until, depth: usize) -> usize {
        if depth > MAX_DEPTH {
            self.diag(i, "nesting is too deep");
            return self.src.len();
        }
        let mut last_sig = b'>';
        while i < self.src.len() {
            if self.spent(1) {
                return self.src.len();
            }
            let c = self.src[i];
            match c {
                b'<' if self.at(i + 1) == b'/' => {
                    if until == Until::CloseTag {
                        let mut j = i + 2;
                        if self.at(self.skip_ws(j)) == b'{' {
                            j = self.skip_ws(j) + 1;
                            j = self.ts(j, Some(b'}'), false, depth + 1);
                        }
                        while j < self.src.len() && self.src[j] != b'>' {
                            j += 1;
                        }
                        return (j + 1).min(self.src.len());
                    }
                    i += 1;
                }
                b'<' if self.opens_element(i) => {
                    i = self.element(i, depth + 1);
                    last_sig = b'>';
                }
                b'{' => {
                    let from = i + 1;
                    i = self.ts(from, Some(b'}'), false, depth + 1);
                    self.uses_in(from, i.saturating_sub(1));
                    last_sig = b'}';
                }
                b'}' if until == Until::Brace => return i + 1,
                b'@' if self.at(i + 1) == b'{' => {
                    self.a.masked[i] = b' ';
                    let end = self.ts(i + 2, Some(b'}'), true, depth + 1);
                    self.push_container(Span { start: i, end });
                    i = end;
                    last_sig = b'}';
                }
                b'@' if matches!(self.keyword(i + 1), Some("if" | "for" | "switch" | "try")) => {
                    i = self.control(i, depth + 1);
                    last_sig = b'}';
                }
                b'/' if self.at(i + 1) == b'*' => i = self.block_comment_end(i),
                b'/' if self.at(i + 1) == b'/'
                    && (i == 0 || matches!(self.src[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'}'))
                    && matches!(last_sig, b'>' | b'}') =>
                {
                    i = self.line_end(i);
                }
                _ if until == Until::Brace && self.setup_at(i) => {
                    i = self.setup_statement(i, depth + 1);
                    last_sig = b';';
                }
                c => {
                    if !c.is_ascii_whitespace() {
                        last_sig = c;
                    }
                    i += 1;
                }
            }
        }
        self.diag(self.src.len(), "unterminated element or block");
        self.src.len()
    }

    /// Whether a setup statement (`const x = ...`) starts at `i` inside a template block.
    fn setup_at(&self, i: usize) -> bool {
        if i > 0 && is_ident(self.src[i - 1]) {
            return false;
        }
        for kw in ["const", "let", "var"] {
            if self.starts(i, kw.as_bytes()) && self.at(i + kw.len()).is_ascii_whitespace() {
                let n = self.skip_ws(i + kw.len());
                return is_ident_start(self.at(n)) || matches!(self.at(n), b'{' | b'[');
            }
        }
        false
    }

    /// A setup statement of a template block; defines its names and uses the ones it reads.
    fn setup_statement(&mut self, i: usize, depth: usize) -> usize {
        let mut j = self.skip_ws(i);
        while is_ident(self.at(j)) {
            j += 1;
        }
        j = self.skip_ws(j);
        // The binding pattern up to `=` or the end.
        let pat_from = j;
        let mut d = 0usize;
        while j < self.src.len() {
            match self.src[j] {
                b'{' | b'[' | b'(' => d += 1,
                b'}' | b']' | b')' => d = d.saturating_sub(1),
                b'=' if d == 0 && self.at(j + 1) != b'>' => break,
                b';' | b'\n' if d == 0 => break,
                _ => {}
            }
            j += 1;
        }
        self.idents_of_pattern(pat_from, j, ItemKind::Local);
        let init_from = j;
        // The initializer: TypeScript up to `;` or a line end outside brackets.
        let end = self.statement_end(j, depth);
        self.uses_in(init_from, end);
        end
    }

    /// The end of the statement from `i`: after a `;`, or at a line end outside brackets when the
    /// next line does not continue the expression.
    fn statement_end(&mut self, mut i: usize, depth: usize) -> usize {
        let mut prev = 0u8;
        while i < self.src.len() {
            if self.spent(1) {
                return self.src.len();
            }
            let c = self.src[i];
            match c {
                b';' => return i + 1,
                b'\n' => {
                    let n = self.skip_trivia(i + 1);
                    let next = self.at(n);
                    let open = matches!(prev, b'=' | b',' | b'+' | b'-' | b'*' | b'/' | b'&' | b'|' | b'?' | b':' | b'(' | b'[' | b'{' | b'.' | b'<')
                        || matches!(next, b'.' | b'?' | b':' | b'+' | b'*' | b'&' | b'|' | b')' | b']' | b',')
                        || (next == b'-' || next == b'/') && self.at(n + 1) != b'/';
                    if !open || prev == 0 {
                        return i;
                    }
                    i = n.max(i + 1);
                }
                b'\'' | b'"' => {
                    i = self.string_end(i);
                    prev = c;
                }
                b'`' => {
                    i = self.template_end(i, depth);
                    prev = b'`';
                }
                b'{' => {
                    i = self.ts(i + 1, Some(b'}'), false, depth + 1);
                    prev = b'}';
                }
                b'(' => {
                    i = self.ts(i + 1, Some(b')'), false, depth + 1);
                    prev = b')';
                }
                b'[' => {
                    i = self.ts(i + 1, Some(b']'), false, depth + 1);
                    prev = b']';
                }
                b'}' => return i,
                c => {
                    if !c.is_ascii_whitespace() {
                        prev = c;
                    }
                    i += 1;
                }
            }
        }
        self.src.len()
    }

    /// Defines every identifier of the binding pattern `a..b` (`user`, `{ a, b: c }`, `[x, y]`).
    /// One pass: the brace depth is tracked as the range is walked.
    fn idents_of_pattern(&mut self, a: usize, b: usize, kind: ItemKind) {
        let b = b.min(self.src.len());
        let mut j = a;
        let mut braces = 0usize;
        while j < b {
            if self.spent(1) {
                return;
            }
            let c = self.src[j];
            if c == b'{' {
                braces += 1;
            } else if c == b'}' {
                braces = braces.saturating_sub(1);
            }
            if is_ident_start(c) {
                let from = j;
                while j < b && is_ident(self.src[j]) {
                    j += 1;
                }
                let word = String::from_utf8_lossy(&self.src[from..j]).into_owned();
                // `{ key: name }` defines `name`; the key is not a binding.
                let k = self.skip_ws(j);
                let is_key = self.at(k) == b':' && braces > 0;
                if !KEYWORDS.contains(&word.as_str()) && !is_key && self.room() {
                    self.a.defs.push(Item { name: word, kind, span: Span { start: from, end: j } });
                }
                continue;
            }
            j += 1;
        }
    }

    /// Records the identifiers read in `a..b` as value uses. One pass: what precedes an
    /// identifier (the last significant byte, a spread, a `?`) is tracked as the range is walked.
    fn uses_in(&mut self, a: usize, b: usize) {
        let b = b.min(self.src.len());
        let mut j = a;
        let mut last = 0u8; // the last significant byte before `j`
        let mut dots = 0u32; // dots ending at `last`
        let mut question = false;
        while j < b {
            if self.spent(1) {
                return;
            }
            let c = self.src[j];
            if c == b'\'' || c == b'"' {
                j = self.string_end(j);
                last = c;
                dots = 0;
                continue;
            }
            if c == b'`' {
                // Static parts of a template literal are text; its `${ }` parts were scanned by
                // `template_end` on the way in.
                j += 1;
                while j < b && self.src[j] != b'`' {
                    if self.src[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                j += 1;
                last = b'`';
                dots = 0;
                continue;
            }
            if self.starts(j, b"//") {
                j = self.line_end(j);
                continue;
            }
            if self.starts(j, b"/*") {
                j = self.block_comment_end(j);
                continue;
            }
            if c.is_ascii_digit() {
                while j < b && (is_ident(self.src[j]) || self.src[j] == b'.') {
                    j += 1;
                }
                last = b'0';
                dots = 0;
                continue;
            }
            if is_ident_start(c) {
                let from = j;
                while j < b && is_ident(self.src[j]) {
                    j += 1;
                }
                let dotted = last == b'.' && dots != 3;
                let key = {
                    let k = self.skip_ws(j);
                    self.at(k) == b':' && self.at(k + 1) != b':' && !question && matches!(last, b'{' | b',')
                };
                let text = String::from_utf8_lossy(&self.src[from..j]).into_owned();
                if !dotted && !key && !KEYWORDS.contains(&text.as_str()) && self.room() {
                    self.a.uses.push(Item { name: text, kind: ItemKind::ValueUse, span: Span { start: from, end: j } });
                }
                last = b'a';
                dots = 0;
                continue;
            }
            if !c.is_ascii_whitespace() {
                dots = if c == b'.' { dots + 1 } else { 0 };
                if c == b'?' {
                    question = true;
                }
                last = c;
            }
            j += 1;
        }
    }

    // ---------------------------------------------------------------------- control flow

    /// A control-flow form starting at the `@` at `i`, with its `@else`/`@empty`/`@catch`/
    /// `@pending`/`@case` continuations; returns the index after its last body.
    fn control(&mut self, i: usize, depth: usize) -> usize {
        if depth > MAX_DEPTH {
            self.diag(i, "nesting is too deep");
            return self.src.len();
        }
        let Some(word) = self.keyword(i + 1) else {
            return i + 1;
        };
        let kind = kind_of(word);
        let mut end = self.body_form(i, kind, depth);
        loop {
            let n = self.skip_trivia(end);
            if self.at(n) != b'@' {
                return end;
            }
            let follows = match (kind, self.keyword(n + 1)) {
                (ScopeKind::If, Some("else")) => Some(ScopeKind::Else),
                (ScopeKind::For, Some("empty")) => Some(ScopeKind::Empty),
                (ScopeKind::Try, Some("pending")) => Some(ScopeKind::Pending),
                (ScopeKind::Try, Some("catch")) => Some(ScopeKind::Catch),
                _ => None,
            };
            let Some(next) = follows else {
                return end;
            };
            if next == ScopeKind::Else {
                // `@else @if (...) { }` continues the chain; both bodies are scopes.
                let m = self.skip_trivia(n + 5);
                if self.at(m) == b'@' && self.keyword(m + 1) == Some("if") {
                    end = self.control(m, depth + 1);
                    // The chain was consumed by the nested call; record the `else` as part of it.
                    return end;
                }
            }
            end = self.body_form(n, next, depth);
        }
    }

    /// One `@word (header)? { body }` form (a `@switch` body holds the cases).
    fn body_form(&mut self, at: usize, kind: ScopeKind, depth: usize) -> usize {
        let kw_end = at + 1 + kind.keyword().len();
        let mut j = self.skip_trivia(kw_end);
        let mut label = format!("@{}", kind.keyword());
        if matches!(kind, ScopeKind::Default) {
            // `@default :` has a colon before the body.
            if self.at(j) == b':' {
                j = self.skip_trivia(j + 1);
            }
        }
        if matches!(kind, ScopeKind::Case) {
            // `@case expr : { body }`: the expression runs to the colon at depth 0.
            let from = j;
            let mut d = 0usize;
            while j < self.src.len() {
                match self.src[j] {
                    b'(' | b'[' | b'{' => d += 1,
                    b')' | b']' => d = d.saturating_sub(1),
                    b'}' => d = d.saturating_sub(1),
                    b'\'' | b'"' => {
                        j = self.string_end(j);
                        continue;
                    }
                    b':' if d == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            self.uses_in(from, j);
            label = format!("@case {}", self.text(from, j));
            j = self.skip_trivia(j + 1);
        } else if self.at(j) == b'(' {
            let from = j + 1;
            let close = self.ts(from, Some(b')'), false, depth + 1);
            let inner_end = close.saturating_sub(1).max(from);
            self.header(kind, from, inner_end);
            label = format!("@{} ({})", kind.keyword(), self.text(from, inner_end));
            j = self.skip_trivia(close);
        }
        let end = if matches!(kind, ScopeKind::Switch) && self.at(j) == b'{' {
            self.switch_cases(j, depth)
        } else if self.at(j) == b'{' {
            self.children(j + 1, Until::Brace, depth + 1)
        } else {
            self.diag(j, "a control-flow form needs a `{` body");
            j
        };
        if self.room() {
            self.a.scopes.push(Scope {
                kind,
                span: Span { start: at, end },
                label,
            });
        }
        end
    }

    /// The cases of a `@switch` body starting at its `{`; returns the index after the `}`.
    fn switch_cases(&mut self, open: usize, depth: usize) -> usize {
        let mut j = open + 1;
        loop {
            j = self.skip_trivia(j);
            if j >= self.src.len() {
                self.diag(j, "unterminated @switch");
                return self.src.len();
            }
            if self.src[j] == b'}' {
                return j + 1;
            }
            match (self.at(j), self.keyword(j + 1)) {
                (b'@', Some("case")) => j = self.body_form(j, ScopeKind::Case, depth + 1),
                (b'@', Some("default")) => j = self.body_form(j, ScopeKind::Default, depth + 1),
                _ => {
                    self.diag(j, "only @case and @default belong in a @switch");
                    j += 1;
                }
            }
        }
    }

    /// Definitions and uses of a header: `@for (const x of xs; key x.id index i)`, `@if (cond)`,
    /// `@catch (e)`.
    fn header(&mut self, kind: ScopeKind, a: usize, b: usize) {
        match kind {
            ScopeKind::For => {
                // `<decl> <pattern> of <iterable> [; key <expr>] [index <ident>]`
                self.spent(3 * (b - a));
                let text = &self.src[a..b];
                let of = find_word(text, b"of").map(|p| a + p);
                let semi = self.src[a..b].iter().position(|&c| c == b';').map(|p| a + p);
                if let Some(of) = of {
                    self.idents_of_pattern(a, of, ItemKind::LoopVar);
                    let iter_end = semi.unwrap_or(b);
                    self.uses_in(of + 2, iter_end);
                    if let Some(semi) = semi {
                        let opts = &self.src[semi + 1..b];
                        let key = find_word(opts, b"key").map(|p| semi + 1 + p);
                        let index = find_word(opts, b"index").map(|p| semi + 1 + p);
                        if let Some(k) = key {
                            let stop = index.filter(|&x| x > k).unwrap_or(b);
                            self.uses_in(k + 3, stop);
                        }
                        if let Some(x) = index {
                            let stop = key.filter(|&k| k > x).unwrap_or(b);
                            self.idents_of_pattern(x + 5, stop, ItemKind::LoopVar);
                        }
                    }
                } else {
                    self.uses_in(a, b);
                }
            }
            ScopeKind::Catch => self.idents_of_pattern(a, b, ItemKind::LoopVar),
            _ => self.uses_in(a, b),
        }
    }

    fn text(&self, a: usize, b: usize) -> String {
        let s = String::from_utf8_lossy(&self.src[a.min(b)..b.min(self.src.len())]);
        let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out: String = s.chars().take(120).collect();
        if s.chars().count() > 120 {
            out.push_str("...");
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Until {
    CloseTag,
    Brace,
}

fn kind_of(word: &str) -> ScopeKind {
    match word {
        "if" => ScopeKind::If,
        "else" => ScopeKind::Else,
        "for" => ScopeKind::For,
        "empty" => ScopeKind::Empty,
        "switch" => ScopeKind::Switch,
        "case" => ScopeKind::Case,
        "default" => ScopeKind::Default,
        "try" => ScopeKind::Try,
        "pending" => ScopeKind::Pending,
        _ => ScopeKind::Catch,
    }
}

/// The offset of the whole word `w` in `text`.
fn find_word(text: &[u8], w: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + w.len() <= text.len() {
        if text[i..].starts_with(w)
            && (i == 0 || !is_ident(text[i - 1]))
            && text.get(i + w.len()).is_none_or(|&c| !is_ident(c))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The module specifiers a TSRX file imports or re-exports, in source order, the way TypeScript
/// reads them: `import x from 'm'`, `import 'm'`, `export * from 'm'`, `import('m')` and
/// `require('m')`. Template regions are blank in `a.masked`, so only real statements count.
pub fn import_specifiers(a: &Analysis) -> Vec<String> {
    let t = &a.masked;
    let mut out = Vec::new();
    let mut i = 0;
    let quoted = |mut j: usize| -> Option<(String, usize)> {
        while j < t.len() && t[j].is_ascii_whitespace() {
            j += 1;
        }
        let q = *t.get(j).filter(|&&c| c == b'\'' || c == b'"')?;
        let from = j + 1;
        let mut e = from;
        while e < t.len() && t[e] != q && t[e] != b'\n' {
            e += 1;
        }
        (t.get(e) == Some(&q) && e - from <= 1024)
            .then(|| (String::from_utf8_lossy(&t[from..e]).into_owned(), e + 1))
    };
    while i < t.len() && out.len() < MAX_ITEMS {
        let word_start = i == 0 || !is_ident(t[i - 1]);
        if word_start && t[i..].starts_with(b"import") && !t.get(i + 6).is_some_and(|&c| is_ident(c)) {
            let mut j = i + 6;
            while j < t.len() && t[j].is_ascii_whitespace() {
                j += 1;
            }
            if t.get(j) == Some(&b'(') {
                if let Some((m, e)) = quoted(j + 1) {
                    out.push(m);
                    i = e;
                    continue;
                }
            } else if let Some((m, e)) = quoted(j) {
                out.push(m);
                i = e;
                continue;
            } else if let Some(p) = find_from(t, j, b"from")
                && let Some((m, e)) = quoted(p + 4)
            {
                out.push(m);
                i = e;
                continue;
            }
        } else if word_start && (t[i..].starts_with(b"export") || t[i..].starts_with(b"require")) {
            let n = if t[i..].starts_with(b"export") { 6 } else { 7 };
            if n == 7 {
                let mut j = i + n;
                while j < t.len() && t[j].is_ascii_whitespace() {
                    j += 1;
                }
                if t.get(j) == Some(&b'(')
                    && let Some((m, e)) = quoted(j + 1)
                {
                    out.push(m);
                    i = e;
                    continue;
                }
            } else if let Some(p) = find_from(t, i + n, b"from")
                && let Some((m, e)) = quoted(p + 4)
                && !t[i + n..p].contains(&b';')
            {
                out.push(m);
                i = e;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The offset of the whole word `w` in `t` at or after `from`, not past the next `;` or newline
/// gap of 400 bytes (an import clause is short).
fn find_from(t: &[u8], from: usize, w: &[u8]) -> Option<usize> {
    let end = (from + 400).min(t.len());
    let mut i = from;
    while i + w.len() <= end {
        if t[i] == b';' {
            return None;
        }
        if t[i..].starts_with(w) && !is_ident(t[i - 1]) && !t.get(i + w.len()).is_some_and(|&c| is_ident(c)) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The chain of components and control-flow scopes around `byte`, outermost first, for a review
/// hunk header: `["UserList", "@for (const user of users)", "@if (showBio)"]`. `components` are
/// the `(name, span)` of the file's component functions (from the tags); scopes come from `a`.
pub fn scope_chain(a: &Analysis, components: &[(String, Span)], byte: usize) -> Vec<String> {
    let mut chain: Vec<(usize, String)> = Vec::new();
    for (name, sp) in components {
        if sp.start <= byte && byte < sp.end {
            chain.push((sp.start, name.clone()));
        }
    }
    for s in &a.scopes {
        if s.span.start <= byte && byte < s.span.end {
            chain.push((s.span.start, s.label.clone()));
        }
    }
    chain.sort_by_key(|(start, _)| *start);
    chain.into_iter().map(|(_, n)| n).collect()
}

#[cfg(test)]
mod tests;
