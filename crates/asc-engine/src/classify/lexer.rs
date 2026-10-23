//! Per-extension lexers for change classification (SPEC §8.10).
//!
//! Soundness rule: a false `LOGIC` is acceptable, a false lower class is a bug. So every lexer
//! refuses (returns [`LexError`]) whenever it cannot be sure where a comment, string or token
//! ends, and its callers then answer `LOGIC`.
//!
//! Two families exist. The *token* family (rs, java, ts, tsx, js, jsx, cs, go, c, cpp) turns the
//! source into [`Item`]s; whitespace between tokens only survives where it separates two tokens
//! that would otherwise merge (and as a `Newline` where a line break can matter). The *line*
//! family (py, yaml, yml, sh, toml, md, html, Makefile) cannot drop whitespace between tokens, so
//! it reports comment spans and which line ends are inside a string or continuation
//! ([`Lines`]); the caller compares lines with trailing whitespace trimmed.

/// A language the classifier can lex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Java,
    Ts,
    Tsx,
    Js,
    Jsx,
    Cs,
    Go,
    C,
    Cpp,
    Py,
    Yaml,
    Sh,
    Toml,
    Md,
    Html,
    Make,
}

/// The lexer refused the input; the file is `LOGIC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexError;

type Res<T> = Result<T, LexError>;

/// Which comparison family a language belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Token,
    Line,
}

impl Lang {
    /// The language of `path` by extension (or file name for makefiles).
    pub fn of_path(path: &[u8]) -> Option<Lang> {
        let name = path.rsplit(|&c| c == b'/').next()?;
        if matches!(name, b"Makefile" | b"makefile" | b"GNUmakefile") {
            return Some(Lang::Make);
        }
        let dot = name.iter().rposition(|&c| c == b'.')?;
        if dot == 0 {
            return None;
        }
        let ext = std::str::from_utf8(&name[dot + 1..])
            .ok()?
            .to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "java" => Lang::Java,
            "ts" => Lang::Ts,
            "tsx" => Lang::Tsx,
            "js" => Lang::Js,
            "jsx" => Lang::Jsx,
            "cs" => Lang::Cs,
            "go" => Lang::Go,
            "c" => Lang::C,
            "cpp" => Lang::Cpp,
            "py" => Lang::Py,
            "yaml" | "yml" => Lang::Yaml,
            "sh" => Lang::Sh,
            "toml" => Lang::Toml,
            "md" => Lang::Md,
            "html" => Lang::Html,
            _ => return None,
        })
    }

    pub fn family(self) -> Family {
        match self {
            Lang::Rust
            | Lang::Java
            | Lang::Ts
            | Lang::Tsx
            | Lang::Js
            | Lang::Jsx
            | Lang::Cs
            | Lang::Go
            | Lang::C
            | Lang::Cpp => Family::Token,
            _ => Family::Line,
        }
    }

    fn js_like(self) -> bool {
        matches!(self, Lang::Ts | Lang::Tsx | Lang::Js | Lang::Jsx)
    }

    fn c_like(self) -> bool {
        matches!(self, Lang::C | Lang::Cpp)
    }

    /// A line break is a token (automatic semicolons, preprocessor lines).
    fn newline_significant(self) -> bool {
        !matches!(self, Lang::Rust | Lang::Java)
    }
}

/// Comments that tools or compilers read are not droppable: a change to one is `LOGIC`.
///
/// Matched case-insensitively against the start of the comment's first line, after the comment
/// markers (`/`, `*`, `#`) are stripped; so `//# sourceMappingURL=` is `sourcemappingurl` and
/// `/*#__PURE__*/` is `__pure__`.
const DIRECTIVES: &[&str] = &[
    "go:",
    "line ",
    "export ",
    "+build",
    "@ts-",
    "<reference",
    "<amd",
    "-*-",
    "type:",
    "pragma",
    "noqa",
    "nolint",
    "nosec",
    "nosemgrep",
    "lgtm",
    "gitleaks",
    "shellcheck",
    "hadolint",
    "fmt:",
    "fmt ",
    "eslint",
    "oxlint",
    "tslint",
    "jshint",
    "jscs",
    "stylelint",
    "prettier",
    "biome",
    "rome-",
    "deno-",
    "dprint",
    "markdownlint",
    "mdformat",
    "vale ",
    "doctoc",
    "global ",
    "globals ",
    "exported ",
    "istanbul",
    "c8 ",
    "v8 ",
    "rustfmt",
    "clippy",
    "cspell",
    "isort",
    "pylint",
    "flake8",
    "ruff",
    "yapf",
    "pytype",
    "pyre-",
    "mypy",
    "pyright",
    "yaml-language-server",
    "coding",
    "vim:",
    // Source maps: `//# sourceMappingURL=`, `//# sourceURL=` and the old `//@` spelling.
    "sourcemappingurl",
    "sourceurl",
    "@ source",
    "lint:",
    "asc:",
    "webpack",
    // Bundler and minifier annotations: `/*#__PURE__*/`, `/*@__NO_SIDE_EFFECTS__*/`.
    "__",
    "@__",
    "@vite",
    "@vitest",
    "@jest",
    "@refresh",
    "@jsx",
    "@flow",
    "@license",
    "@preserve",
    "@cc_on",
    "[if",
    "<![endif",
];

/// Prefixes that count on any line of a multi-line comment (a JSDoc block).
fn line_directive(p: &str) -> bool {
    p.starts_with('@') || p.starts_with('<')
}

/// Words that make a comment a legal or minifier notice wherever they appear (terser's rule).
const ANYWHERE: &[&str] = &["@preserve", "@license", "@lic", "@copyright", "@cc_on"];

fn is_directive(body: &str, hash_style: bool, at_file_start: bool) -> bool {
    if hash_style && at_file_start && body.starts_with('!') {
        return true;
    }
    let lower = body.to_ascii_lowercase();
    if ANYWHERE.iter().any(|w| lower.contains(w)) {
        return true;
    }
    lower.lines().enumerate().any(|(n, line)| {
        let line =
            line.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '/' | '*' | '#'));
        DIRECTIVES
            .iter()
            .any(|p| (n == 0 || line_directive(p)) && line.starts_with(p))
    })
}

// ------------------------------------------------------------------------------ token family

/// The kind of a lexed [`Item`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Word,
    Punct,
    Str,
    Comment,
    Newline,
    /// Whitespace that separates two tokens which would merge without it.
    Space,
}

/// One token of the normalised stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item<'a> {
    pub class: Class,
    pub text: &'a str,
}

const REGEX_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
    "default",
];

/// Characters that can combine with a neighbour into a longer operator (`&&`, `->`, `//`).
const OP_CHARS: &[u8] = b"+-*/%&|^!<>=:.?~@#";

/// Whether whitespace between `prev` and the next token (`cur`, `text`) can change the tokens.
fn glue(prev: &Item<'_>, cur: Class, text: &str) -> bool {
    use Class::{Punct, Str, Word};
    match (prev.class, cur) {
        (Word | Str, Word | Str) => true,
        (Punct, Punct) => {
            let op = |s: &str| s.bytes().next().is_some_and(|c| OP_CHARS.contains(&c));
            op(prev.text) && op(text)
        }
        _ => false,
    }
}

struct Scan<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    lang: Lang,
    keep: bool,
    out: Vec<Item<'a>>,
    ws: bool,
    nl: bool,
}

/// Lexes `src` into a normalised token stream. With `keep_comments` false, comments (other than
/// directives) become whitespace.
pub fn tokens(lang: Lang, src: &str, keep_comments: bool) -> Res<Vec<Item<'_>>> {
    if lang.family() != Family::Token || src.as_bytes().contains(&0) || odd_line_break(src) {
        return Err(LexError);
    }
    // Java reads `\u000a` before it lexes; C may read `??/` as a backslash.
    if (lang == Lang::Java && java_ascii_unicode_escape(src.as_bytes()))
        || (lang.c_like() && has_trigraph(src.as_bytes()))
    {
        return Err(LexError);
    }
    let mut sc = Scan {
        s: src,
        b: src.as_bytes(),
        i: 0,
        lang,
        keep: keep_comments,
        out: Vec::new(),
        ws: false,
        nl: false,
    };
    sc.run()?;
    // The comment before `import "C"` is C source (cgo), not a comment.
    if lang == Lang::Go
        && sc
            .out
            .iter()
            .any(|t| t.class == Class::Str && matches!(t.text, "\"C\"" | "`C`"))
    {
        return Err(LexError);
    }
    Ok(sc.out)
}

/// A line ends at a bare CR, NEL, LS or PS in some languages and not in others; refuse them all.
fn odd_line_break(src: &str) -> bool {
    let b = src.as_bytes();
    b.iter()
        .enumerate()
        .any(|(i, &c)| c == b'\r' && b.get(i + 1) != Some(&b'\n'))
        || src.contains(['\u{85}', '\u{2028}', '\u{2029}'])
}

/// Whether Java source holds a `\uXXXX` escape (outside a `\\` pair) that is not a plain
/// non-ASCII character. Java decodes it before lexing, so it can be a quote, a newline or `/`.
fn java_ascii_unicode_escape(b: &[u8]) -> bool {
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let mut k = i;
        while b.get(k) == Some(&b'\\') {
            k += 1;
        }
        if (k - i) % 2 == 1 && b.get(k) == Some(&b'u') {
            let mut h = k;
            while b.get(h) == Some(&b'u') {
                h += 1;
            }
            let digits = b
                .get(h..h + 4)
                .filter(|d| d.iter().all(u8::is_ascii_hexdigit));
            let value = digits
                .and_then(|d| std::str::from_utf8(d).ok())
                .and_then(|d| u32::from_str_radix(d, 16).ok());
            if value.is_none_or(|v| v < 0x80) {
                return true;
            }
        }
        i = k;
    }
    false
}

/// Whether C source holds a trigraph (`??/` is a backslash).
fn has_trigraph(b: &[u8]) -> bool {
    b.windows(3)
        .any(|w| w[0] == b'?' && w[1] == b'?' && b"=/'()!<>-".contains(&w[2]))
}

/// Whether `t` holds a backslash followed by blanks and a line break (a line splice).
fn has_splice(t: &[u8]) -> bool {
    t.iter().enumerate().any(|(i, &c)| {
        c == b'\\'
            && t[i + 1..]
                .iter()
                .find(|&&x| !matches!(x, b' ' | b'\t' | b'\r' | 0x0b | 0x0c))
                == Some(&b'\n')
    })
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

impl<'a> Scan<'a> {
    fn emit(&mut self, class: Class, a: usize, z: usize) -> Res<()> {
        let text = self.s.get(a..z).ok_or(LexError)?;
        if let Some(prev) = self.out.last().copied() {
            if self.nl && self.lang.newline_significant() {
                if prev.class != Class::Newline {
                    self.out.push(Item {
                        class: Class::Newline,
                        text: "\n",
                    });
                }
            } else if (self.ws || self.nl) && glue(&prev, class, text) {
                self.out.push(Item {
                    class: Class::Space,
                    text: " ",
                });
            }
        }
        self.ws = false;
        self.nl = false;
        self.out.push(Item { class, text });
        Ok(())
    }

    fn at(&self, j: usize) -> u8 {
        self.b.get(j).copied().unwrap_or(0)
    }

    fn starts(&self, j: usize, pat: &str) -> bool {
        self.b
            .get(j..)
            .is_some_and(|r| r.starts_with(pat.as_bytes()))
    }

    /// The index after the closing `q` of a string whose body starts at `j`.
    fn quoted(&self, mut j: usize, q: u8, multiline: bool) -> Res<usize> {
        loop {
            match self.b.get(j) {
                None => return Err(LexError),
                Some(b'\\') => j += 2,
                Some(&c) if c == q => return Ok(j + 1),
                Some(b'\n') if !multiline => return Err(LexError),
                _ => j += 1,
            }
        }
    }

    /// The index after a short character literal whose body starts at `j`.
    fn char_lit(&self, mut j: usize, limit: usize) -> Res<usize> {
        let start = j;
        loop {
            match self.b.get(j) {
                None | Some(b'\n') => return Err(LexError),
                Some(b'\\') => j += 2,
                Some(b'\'') => return Ok(j + 1),
                _ => j += 1,
            }
            if j - start > limit {
                return Err(LexError);
            }
        }
    }

    fn comment(&mut self, a: usize, z: usize, body_from: usize, first_line: bool) -> Res<()> {
        let text = self.s.get(a..z).ok_or(LexError)?;
        let body = text.get(body_from - a..).unwrap_or("");
        // `/*! legal notice */` is kept by minifiers.
        let directive = is_directive(body, false, first_line)
            || (text.starts_with("/*") && body.trim_start_matches('*').starts_with('!'));
        if self.keep || directive {
            self.emit(Class::Comment, a, z)?;
        } else {
            self.ws = true;
            if text.contains('\n') {
                self.nl = true;
            }
        }
        self.i = z;
        Ok(())
    }

    fn line_comment_end(&self, from: usize) -> usize {
        let mut j = from;
        loop {
            match self.b.get(j) {
                None => return j,
                Some(b'\n') => {
                    // C and C++ continue a line comment over a backslash-newline.
                    let cont = self.lang.c_like()
                        && (self.b.get(j.wrapping_sub(1)) == Some(&b'\\')
                            || (self.b.get(j.wrapping_sub(1)) == Some(&b'\r')
                                && self.b.get(j.wrapping_sub(2)) == Some(&b'\\')));
                    if !cont {
                        return j;
                    }
                    j += 1;
                }
                _ => j += 1,
            }
        }
    }

    fn block_end(&self, from: usize) -> Res<usize> {
        let nested = self.lang == Lang::Rust;
        let mut depth = 1usize;
        let mut j = from;
        while j < self.b.len() {
            if self.starts(j, "*/") {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return Ok(j);
                }
            } else if nested && self.starts(j, "/*") {
                depth += 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        Err(LexError)
    }

    /// Whether the `)` that ends `out` closes the head of `if`, `while`, `for` or `with`, so a
    /// `/` after it starts a regex. An unbalanced `)` counts as one.
    fn stmt_head_closed(&self) -> bool {
        let mut depth = 0usize;
        for (k, it) in self.out.iter().enumerate().rev() {
            if it.class != Class::Punct {
                continue;
            }
            match it.text {
                ")" => depth += 1,
                "(" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let mut before = self.out[..k].iter().rev().filter(|t| {
                            !matches!(t.class, Class::Space | Class::Newline | Class::Comment)
                        });
                        return match before.next() {
                            Some(w) if w.class == Class::Word => {
                                matches!(w.text, "if" | "while" | "for" | "with")
                                    || (w.text == "await"
                                        && before.next().is_some_and(|f| f.text == "for"))
                            }
                            _ => false,
                        };
                    }
                }
                _ => {}
            }
        }
        true
    }

    fn regex_possible(&self) -> bool {
        match self.out.last() {
            None => true,
            Some(it) => match it.class {
                Class::Punct => match it.text {
                    "]" => false,
                    ")" => self.stmt_head_closed(),
                    _ => true,
                },
                Class::Word => REGEX_KEYWORDS.contains(&it.text),
                Class::Str => false,
                Class::Comment | Class::Newline | Class::Space => true,
            },
        }
    }

    fn run(&mut self) -> Res<()> {
        let lang = self.lang;
        while self.i < self.b.len() {
            let i = self.i;
            let c = self.b[i];
            match c {
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                    self.ws = true;
                    self.i += 1;
                }
                b'\n' => {
                    self.nl = true;
                    self.i += 1;
                }
                b'/' if self.at(i + 1) == b'/' => {
                    let z = self.line_comment_end(i + 2);
                    self.comment(i, z, i + 2, i == 0)?;
                }
                b'/' if self.at(i + 1) == b'*' => {
                    let z = self.block_end(i + 2)?;
                    // A backslash-newline inside `*\<nl>/` closes the comment early in C.
                    if lang.c_like() && has_splice(&self.b[i..z]) {
                        return Err(LexError);
                    }
                    self.comment(i, z, i + 2, false)?;
                }
                b'/' => {
                    if lang.js_like() && (self.regex_possible() || self.at(i + 1) == b'>') {
                        return Err(LexError);
                    }
                    self.emit(Class::Punct, i, i + 1)?;
                    self.i += 1;
                }
                // JSX: a tag can only start where an expression can, and its text is not code.
                b'<' if lang.js_like()
                    && (matches!(self.at(i + 1), b'/' | b'>')
                        || (self.at(i + 1).is_ascii_alphabetic() && self.regex_possible())) =>
                {
                    return Err(LexError);
                }
                b'@' if lang == Lang::Cs && self.at(i + 1) == b'"' => {
                    self.cs_string(i, i + 1, true, false)?;
                }
                b'@' if lang == Lang::Cs && self.at(i + 1) == b'$' && self.at(i + 2) == b'"' => {
                    self.cs_string(i, i + 2, true, true)?;
                }
                b'"' => self.string_dq(i, i)?,
                b'\'' => self.single_quote(i)?,
                b'`' => match lang {
                    Lang::Go => {
                        // A raw string has no escapes: only `raw_go` is right.
                        let z = self.raw_go(i + 1)?;
                        self.emit(Class::Str, i, z)?;
                        self.i = z;
                    }
                    l if l.js_like() => self.template(i)?,
                    _ => {
                        self.emit(Class::Punct, i, i + 1)?;
                        self.i += 1;
                    }
                },
                b'\\' => {
                    let nl = self.at(i + 1) == b'\n'
                        || (self.at(i + 1) == b'\r' && self.at(i + 2) == b'\n');
                    if lang.c_like() && nl {
                        // `/\<nl>/` and `/\<nl>*` start a comment the scan would not see.
                        if i > 0 && self.b[i - 1] == b'/' {
                            return Err(LexError);
                        }
                        self.ws = true;
                        self.i += if self.at(i + 1) == b'\r' { 3 } else { 2 };
                    } else {
                        return Err(LexError);
                    }
                }
                c if is_word_byte(c) => self.word(i)?,
                0 => return Err(LexError),
                _ => {
                    self.emit(Class::Punct, i, i + 1)?;
                    self.i += 1;
                }
            }
        }
        Ok(())
    }

    fn raw_go(&self, from: usize) -> Res<usize> {
        // Go raw strings have no escapes: the next backquote closes.
        self.b
            .get(from..)
            .and_then(|r| r.iter().position(|&c| c == b'`'))
            .map(|p| from + p + 1)
            .ok_or(LexError)
    }

    /// A word, number or prefixed string starting at `i`.
    fn word(&mut self, i: usize) -> Res<()> {
        let lang = self.lang;
        let digit = self.b[i].is_ascii_digit();
        let mut j = i;
        while j < self.b.len() && (is_word_byte(self.b[j]) || (digit && self.b[j] == b'.')) {
            // A C++ digit separator (1'000) continues the number.
            j += 1;
        }
        if lang.c_like() && digit && self.at(j) == b'\'' && self.at(j + 1).is_ascii_hexdigit() {
            while j < self.b.len()
                && (is_word_byte(self.b[j]) || self.b[j] == b'.' || self.b[j] == b'\'')
            {
                j += 1;
            }
        }
        let word = &self.s[i..j];
        // String prefixes.
        match lang {
            Lang::Rust => {
                if matches!(word, "b" | "c" | "r" | "br" | "cr") {
                    let raw = word.ends_with('r');
                    if raw {
                        let mut k = j;
                        while self.at(k) == b'#' {
                            k += 1;
                        }
                        if self.at(k) == b'"' {
                            let hashes = k - j;
                            let mut close = vec![b'"'];
                            close.extend(std::iter::repeat_n(b'#', hashes));
                            let z = self
                                .b
                                .get(k + 1..)
                                .and_then(|r| {
                                    r.windows(close.len()).position(|w| w == close.as_slice())
                                })
                                .map(|p| k + 1 + p + close.len())
                                .ok_or(LexError)?;
                            self.emit(Class::Str, i, z)?;
                            self.i = z;
                            return Ok(());
                        }
                    } else if self.at(j) == b'"' {
                        return self.string_dq(i, j);
                    } else if word == "b" && self.at(j) == b'\'' {
                        let z = self.char_lit(j + 1, 16)?;
                        self.emit(Class::Str, i, z)?;
                        self.i = z;
                        return Ok(());
                    }
                }
            }
            Lang::Cs => {
                if word == "$" && self.at(j) == b'"' {
                    return self.cs_string(i, j, false, true);
                }
                if word == "$" && self.at(j) == b'@' && self.at(j + 1) == b'"' {
                    return self.cs_string(i, j + 1, true, true);
                }
            }
            Lang::C | Lang::Cpp => {
                if word.ends_with('R') && self.at(j) == b'"' {
                    return Err(LexError);
                }
                if matches!(word, "L" | "u" | "U" | "u8") && self.at(j) == b'"' {
                    return self.string_dq(i, j);
                }
                if matches!(word, "L" | "u" | "U" | "u8") && self.at(j) == b'\'' {
                    let z = self.char_lit(j + 1, 12)?;
                    self.emit(Class::Str, i, z)?;
                    self.i = z;
                    return Ok(());
                }
                if matches!(word, "include" | "include_next" | "import")
                    && self
                        .out
                        .last()
                        .is_some_and(|p| p.class == Class::Punct && p.text == "#")
                {
                    self.emit(Class::Word, i, j)?;
                    self.i = j;
                    let mut k = j;
                    while matches!(self.at(k), b' ' | b'\t') {
                        k += 1;
                    }
                    if self.at(k) == b'<' {
                        let z = self
                            .b
                            .get(k..)
                            .and_then(|r| r.iter().position(|&c| c == b'>' || c == b'\n'))
                            .filter(|&p| self.b[k + p] == b'>')
                            .map(|p| k + p + 1)
                            .ok_or(LexError)?;
                        self.ws = true;
                        self.emit(Class::Str, k, z)?;
                        self.i = z;
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
        self.emit(Class::Word, i, j)?;
        self.i = j;
        Ok(())
    }

    /// A double-quoted string whose quote is at `q`; the token starts at `from`.
    fn string_dq(&mut self, from: usize, q: usize) -> Res<()> {
        let lang = self.lang;
        if matches!(lang, Lang::Java | Lang::Cs) && self.starts(q, "\"\"\"") {
            return Err(LexError);
        }
        if lang == Lang::Cs {
            return self.cs_string(from, q, false, false);
        }
        let multiline = matches!(lang, Lang::Rust | Lang::C | Lang::Cpp);
        let z = self.quoted(q + 1, b'"', multiline)?;
        self.emit(Class::Str, from, z)?;
        self.i = z;
        Ok(())
    }

    /// A C# string: verbatim (`@`) and interpolated (`$`) forms.
    fn cs_string(&mut self, from: usize, q: usize, verbatim: bool, interpolated: bool) -> Res<()> {
        if self.starts(q, "\"\"\"") {
            return Err(LexError);
        }
        let mut j = q + 1;
        let mut depth = 0usize;
        loop {
            let c = *self.b.get(j).ok_or(LexError)?;
            match c {
                b'"' if depth > 0 => return Err(LexError),
                b'"' if verbatim && self.at(j + 1) == b'"' => j += 2,
                b'"' => {
                    j += 1;
                    break;
                }
                b'\\' if !verbatim => j += 2,
                b'\n' if !verbatim && depth == 0 => return Err(LexError),
                b'{' if interpolated => {
                    if depth == 0 && self.at(j + 1) == b'{' {
                        j += 2;
                    } else {
                        depth += 1;
                        j += 1;
                    }
                }
                b'}' if interpolated && depth > 0 => {
                    depth -= 1;
                    j += 1;
                }
                b'\'' if depth > 0 => return Err(LexError),
                _ => j += 1,
            }
        }
        self.emit(Class::Str, from, j)?;
        self.i = j;
        Ok(())
    }

    /// A `'` starts a character literal, a lifetime or nothing, by language.
    fn single_quote(&mut self, i: usize) -> Res<()> {
        match self.lang {
            Lang::Rust => {
                if self.at(i + 1) == b'\\' {
                    let z = self.char_lit(i + 1, 16)?;
                    self.emit(Class::Str, i, z)?;
                    self.i = z;
                    return Ok(());
                }
                let next = self.s.get(i + 1..).and_then(|r| r.chars().next());
                if let Some(ch) = next
                    && ch != '\''
                    && self.at(i + 1 + ch.len_utf8()) == b'\''
                {
                    let z = i + 2 + ch.len_utf8();
                    self.emit(Class::Str, i, z)?;
                    self.i = z;
                    return Ok(());
                }
                // A lifetime or label.
                self.emit(Class::Punct, i, i + 1)?;
                self.i += 1;
                Ok(())
            }
            Lang::Java | Lang::Cs | Lang::Go | Lang::C | Lang::Cpp => {
                let z = self.char_lit(i + 1, 12)?;
                self.emit(Class::Str, i, z)?;
                self.i = z;
                Ok(())
            }
            _ => {
                // JavaScript and TypeScript: no raw newline.
                let z = self.quoted(i + 1, b'\'', false)?;
                self.emit(Class::Str, i, z)?;
                self.i = z;
                Ok(())
            }
        }
    }

    /// A JavaScript template literal. Expressions with quotes, slashes or braces are refused.
    fn template(&mut self, i: usize) -> Res<()> {
        let mut j = i + 1;
        loop {
            match *self.b.get(j).ok_or(LexError)? {
                b'`' => {
                    j += 1;
                    break;
                }
                b'\\' => j += 2,
                b'$' if self.at(j + 1) == b'{' => {
                    j += 2;
                    loop {
                        match *self.b.get(j).ok_or(LexError)? {
                            b'}' => {
                                j += 1;
                                break;
                            }
                            b'`' | b'"' | b'\'' | b'/' | b'{' => return Err(LexError),
                            _ => j += 1,
                        }
                    }
                }
                _ => j += 1,
            }
        }
        self.emit(Class::Str, i, j)?;
        self.i = j;
        Ok(())
    }
}

// ------------------------------------------------------------------------------ line family

/// What the line-family scan found.
#[derive(Debug, Default)]
pub struct Lines {
    /// Byte spans of droppable comments, in order and disjoint.
    pub comments: Vec<(usize, usize)>,
    /// Per line: its line break is inside a string or a continuation, so trailing whitespace
    /// on it is content.
    pub protected: Vec<bool>,
    /// Whether this language supports dropping comments at all.
    pub comments_ok: bool,
}

struct LineScan<'a> {
    s: &'a str,
    b: &'a [u8],
    starts: Vec<usize>,
    out: Lines,
}

/// Scans a line-family file.
pub fn scan_lines(lang: Lang, src: &str) -> Res<Lines> {
    if lang.family() != Family::Line
        || src.as_bytes().iter().any(|&c| c == 0 || c == 1)
        || odd_line_break(src)
    {
        return Err(LexError);
    }
    let b = src.as_bytes();
    let mut starts = vec![0usize];
    starts.extend(
        b.iter()
            .enumerate()
            .filter(|(_, c)| **c == b'\n')
            .map(|(i, _)| i + 1),
    );
    let n = starts.len();
    let mut sc = LineScan {
        s: src,
        b,
        starts,
        out: Lines {
            comments: Vec::new(),
            protected: vec![false; n],
            comments_ok: true,
        },
    };
    match lang {
        Lang::Py => sc.python()?,
        Lang::Yaml => sc.yaml()?,
        Lang::Sh => sc.shell()?,
        Lang::Toml => sc.toml()?,
        Lang::Md | Lang::Html => sc.markup(lang)?,
        Lang::Make => sc.out.comments_ok = false,
        _ => return Err(LexError),
    }
    sc.protect_continuations();
    Ok(sc.out)
}

impl LineScan<'_> {
    fn at(&self, j: usize) -> u8 {
        self.b.get(j).copied().unwrap_or(0)
    }

    fn starts_with(&self, j: usize, pat: &str) -> bool {
        self.b
            .get(j..)
            .is_some_and(|r| r.starts_with(pat.as_bytes()))
    }

    fn line_of(&self, pos: usize) -> usize {
        self.starts.partition_point(|&s| s <= pos) - 1
    }

    /// Marks the line breaks inside `a..z` as protected.
    fn protect(&mut self, a: usize, z: usize) {
        let (la, lz) = (self.line_of(a), self.line_of(z.saturating_sub(1).max(a)));
        for l in la..lz {
            self.out.protected[l] = true;
        }
    }

    fn protect_continuations(&mut self) {
        for l in 0..self.starts.len() {
            let end = self.starts.get(l + 1).map_or(self.b.len(), |e| e - 1);
            let line = &self.b[self.starts[l]..end];
            let t = line.trim_ascii_end();
            if t.last() == Some(&b'\\') {
                self.out.protected[l] = true;
            }
        }
    }

    fn comment(&mut self, a: usize, z: usize, hash: bool) {
        let body = self.s.get(a + 1..z).unwrap_or("");
        if !is_directive(body, hash, a == 0) {
            self.out.comments.push((a, z));
        }
    }

    fn eol(&self, from: usize) -> usize {
        self.b
            .get(from..)
            .and_then(|r| r.iter().position(|&c| c == b'\n'))
            .map_or(self.b.len(), |p| from + p)
    }

    /// Index after the closing `q` (escape `\`), or an error if there is none.
    fn quoted(&self, mut j: usize, q: u8, esc: bool, multi: bool) -> Res<usize> {
        loop {
            match self.b.get(j) {
                None => return Err(LexError),
                Some(b'\\') if esc => j += 2,
                Some(&c) if c == q => return Ok(j + 1),
                Some(b'\n') if !multi => return Err(LexError),
                _ => j += 1,
            }
        }
    }

    fn python(&mut self) -> Res<()> {
        let mut i = 0;
        while i < self.b.len() {
            let c = self.b[i];
            if c == b'#' {
                let z = self.eol(i);
                self.comment(i, z, true);
                i = z;
            } else if c == b'\\' {
                if matches!(self.at(i + 1), b'\n' | b'\r') {
                    i += 2;
                } else {
                    return Err(LexError);
                }
            } else if c.is_ascii_alphabetic() || c == b'_' {
                let mut j = i;
                while j < self.b.len() && (self.b[j].is_ascii_alphanumeric() || self.b[j] == b'_') {
                    j += 1;
                }
                let word = self.s[i..j].to_ascii_lowercase();
                let prefix = matches!(
                    word.as_str(),
                    "r" | "u" | "b" | "f" | "br" | "rb" | "fr" | "rf" | "t" | "tr" | "rt"
                );
                if prefix && matches!(self.at(j), b'"' | b'\'') {
                    // f-strings and t-strings both have `{...}` holes.
                    i = self.py_string(j, word.contains(['f', 't']))?;
                } else {
                    i = j;
                }
            } else if c == b'"' || c == b'\'' {
                i = self.py_string(i, false)?;
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// A Python string whose opening quote is at `q`; returns the index after it.
    fn py_string(&mut self, q: usize, fstr: bool) -> Res<usize> {
        let d = self.b[q];
        let triple = self.at(q + 1) == d && self.at(q + 2) == d;
        let mut j = q + if triple { 3 } else { 1 };
        let mut depth = 0usize;
        loop {
            let c = *self.b.get(j).ok_or(LexError)?;
            if fstr {
                // `\{` is a backslash then a hole; reading it as an escape loses the hole.
                if c == b'\\' && depth == 0 && self.at(j + 1) == b'{' {
                    return Err(LexError);
                }
                if c == b'{' {
                    if depth == 0 && self.at(j + 1) == b'{' {
                        j += 2;
                        continue;
                    }
                    depth += 1;
                    j += 1;
                    continue;
                }
                if c == b'}' && depth > 0 {
                    depth -= 1;
                    j += 1;
                    continue;
                }
                if depth > 0 && (c == d || c == b'#') {
                    return Err(LexError);
                }
            }
            if c == b'\\' {
                j += 2;
            } else if c == d && (!triple || (self.at(j + 1) == d && self.at(j + 2) == d)) {
                j += if triple { 3 } else { 1 };
                break;
            } else if c == b'\n' && !triple {
                return Err(LexError);
            } else {
                j += 1;
            }
        }
        self.protect(q, j);
        Ok(j)
    }

    fn yaml(&mut self) -> Res<()> {
        let mut i = 0;
        while i < self.b.len() {
            let c = self.b[i];
            let prev = if i == 0 { b'\n' } else { self.b[i - 1] };
            match c {
                b'#' if matches!(prev, b' ' | b'\t' | b'\n' | b'\r') => {
                    let z = self.eol(i);
                    self.comment(i, z, true);
                    i = z;
                }
                b'"' | b'\'' if !is_word_byte(prev) => {
                    let z = if c == b'"' {
                        self.quoted(i + 1, b'"', true, true)?
                    } else {
                        let mut j = i + 1;
                        loop {
                            match self.b.get(j) {
                                None => return Err(LexError),
                                Some(b'\'') if self.at(j + 1) == b'\'' => j += 2,
                                Some(b'\'') => break j + 1,
                                _ => j += 1,
                            }
                        }
                    };
                    self.protect(i, z);
                    i = z;
                }
                b'|' | b'>' => {
                    let mut j = i + 1;
                    while matches!(self.at(j), b'-' | b'+' | b'0'..=b'9') {
                        j += 1;
                    }
                    while matches!(self.at(j), b' ' | b'\t' | b'\r') {
                        j += 1;
                    }
                    if matches!(self.at(j), b'\n' | b'#' | 0) {
                        return Err(LexError);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        Ok(())
    }

    fn shell(&mut self) -> Res<()> {
        let mut i = 0;
        // The index just after the last backslash escape: a `#` there is mid-word.
        let mut esc_end = usize::MAX;
        while i < self.b.len() {
            let c = self.b[i];
            let prev = if i == 0 { b'\n' } else { self.b[i - 1] };
            match c {
                b'#' if i == esc_end => i += 1,
                b'#' if i == 0
                    || matches!(
                        prev,
                        b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'('
                    ) =>
                {
                    let z = self.eol(i);
                    self.comment(i, z, true);
                    i = z;
                }
                b'\\' => {
                    // A joined line continues the word, so `#` after it is not a comment start.
                    let k = i + 1 + usize::from(self.at(i + 1) == b'\r');
                    if self.at(k) == b'\n' && self.at(k + 1) == b'#' {
                        return Err(LexError);
                    }
                    i += 2;
                    esc_end = i;
                }
                // The body of `${...}` is not scanned for comments or quotes.
                b'$' if self.at(i + 1) == b'{' => {
                    let mut j = i + 2;
                    loop {
                        match *self.b.get(j).ok_or(LexError)? {
                            b'}' => break,
                            b'\'' | b'"' | b'`' | b'\\' | b'\n' => return Err(LexError),
                            b'$' if matches!(self.at(j + 1), b'(' | b'{' | b'\'' | b'"') => {
                                return Err(LexError);
                            }
                            _ => j += 1,
                        }
                    }
                    i = j + 1;
                }
                b'<' if self.at(i + 1) == b'<' => return Err(LexError),
                b'\'' => {
                    let z = self.quoted(i + 1, b'\'', false, true)?;
                    self.protect(i, z);
                    i = z;
                }
                b'"' => {
                    let mut j = i + 1;
                    let mut brace = false;
                    loop {
                        match *self.b.get(j).ok_or(LexError)? {
                            b'\\' => j += 2,
                            b'"' if brace => return Err(LexError),
                            b'"' => {
                                j += 1;
                                break;
                            }
                            b'`' => return Err(LexError),
                            b'$' if self.at(j + 1) == b'(' => return Err(LexError),
                            b'$' if self.at(j + 1) == b'{' => {
                                brace = true;
                                j += 2;
                            }
                            b'}' => {
                                brace = false;
                                j += 1;
                            }
                            _ => j += 1,
                        }
                    }
                    self.protect(i, j);
                    i = j;
                }
                b'`' => {
                    let z = self.quoted(i + 1, b'`', true, true)?;
                    self.protect(i, z);
                    i = z;
                }
                b'$' if self.at(i + 1) == b'\'' => {
                    let z = self.quoted(i + 2, b'\'', true, true)?;
                    self.protect(i, z);
                    i = z;
                }
                _ => i += 1,
            }
        }
        Ok(())
    }

    fn toml(&mut self) -> Res<()> {
        let mut i = 0;
        while i < self.b.len() {
            let c = self.b[i];
            match c {
                b'#' => {
                    let z = self.eol(i);
                    self.comment(i, z, true);
                    i = z;
                }
                b'"' | b'\'' => {
                    let triple = self.at(i + 1) == c && self.at(i + 2) == c;
                    let z = if triple {
                        let mut j = i + 3;
                        loop {
                            match self.b.get(j) {
                                None => return Err(LexError),
                                Some(b'\\') if c == b'"' => j += 2,
                                Some(&x)
                                    if x == c && self.at(j + 1) == c && self.at(j + 2) == c =>
                                {
                                    break j + 3;
                                }
                                _ => j += 1,
                            }
                        }
                    } else {
                        self.quoted(i + 1, c, c == b'"', false)?
                    };
                    self.protect(i, z);
                    i = z;
                }
                _ => i += 1,
            }
        }
        Ok(())
    }

    fn markup(&mut self, lang: Lang) -> Res<()> {
        let lower = self.s.to_ascii_lowercase();
        let has_comment = lower.contains("<!--");
        if lang == Lang::Html {
            // Raw-text elements hold no comments; `<title>` is skipped to its end tag below.
            for bad in [
                "<script",
                "<style",
                "<pre",
                "<textarea",
                "<![cdata[",
                "<?",
                "<xmp",
                "<plaintext",
                "<iframe",
                "<noembed",
                "<noframes",
                "<noscript",
            ] {
                if lower.contains(bad) {
                    return Err(LexError);
                }
            }
        } else if has_comment
            && (self.s.starts_with("---")
                || self.s.starts_with("+++")
                || self.s.contains('`')
                || self.s.contains("~~~")
                || self
                    .s
                    .lines()
                    .any(|l| l.starts_with("    ") || l.starts_with('\t')))
        {
            return Err(LexError);
        }
        let mut i = 0;
        while i < self.b.len() {
            if self.starts_with(i, "<!--") {
                // `\<!--` is text; `<!-->`, `<!--->` and a `--!>` end the comment early.
                if (i > 0 && self.b[i - 1] == b'\\')
                    || self.starts_with(i, "<!-->")
                    || self.starts_with(i, "<!--->")
                {
                    return Err(LexError);
                }
                let z = self
                    .s
                    .get(i + 4..)
                    .and_then(|r| r.find("-->"))
                    .map(|p| i + 4 + p + 3)
                    .ok_or(LexError)?;
                let inner = self.s.get(i + 4..z - 3).unwrap_or("");
                if inner.contains("--!>") {
                    return Err(LexError);
                }
                if !is_directive(inner, false, false) {
                    self.out.comments.push((i, z));
                }
                i = z;
            } else if lang == Lang::Html
                && self.b[i] == b'<'
                && (self.at(i + 1).is_ascii_alphabetic() || self.at(i + 1) == b'/')
            {
                // Skip a whole tag: attribute values may hold `<!--`.
                let title = lower.get(i..).is_some_and(|r| {
                    r.starts_with("<title")
                        && !r.as_bytes().get(6).is_some_and(|&c| is_word_byte(c))
                });
                let mut j = i + 1;
                loop {
                    match *self.b.get(j).ok_or(LexError)? {
                        b'>' => break,
                        q @ (b'"' | b'\'') => {
                            j = self.quoted(j + 1, q, false, true)?;
                        }
                        _ => j += 1,
                    }
                }
                i = j + 1;
                if title {
                    // RCDATA: the text up to `</title` holds no comments.
                    i = lower
                        .get(i..)
                        .and_then(|r| r.find("</title"))
                        .map(|p| i + p)
                        .ok_or(LexError)?;
                }
            } else {
                i += 1;
            }
        }
        Ok(())
    }
}
