//! The YAML the lockfiles use, and no more.
//!
//! `pnpm-lock.yaml`, `pubspec.lock` and `Podfile.lock` are machine-written YAML of a small kind: block
//! mappings and sequences, quoted and plain scalars, one-line flow sequences and mappings, comments.
//! This reader takes exactly that and refuses the rest, so a lockfile cannot make it do anything but
//! build a bounded tree of strings:
//!
//! * no anchors, aliases, tags, merge keys, complex keys, multiple documents or multi-line flow or quoted
//!   scalars (a block scalar `|` or `>` is skipped, its text is not kept);
//! * tabs in indentation, a line longer than [`MAX_LINE`] bytes, more than [`MAX_DEPTH`] levels or more
//!   than [`MAX_NODES`] nodes are errors, never a partial tree.
//!
//! Every scalar is a string; the callers know which ones are numbers.

/// Deepest nesting read.
pub const MAX_DEPTH: usize = 48;
/// Most nodes (scalars, sequences, mappings) in one document.
pub const MAX_NODES: usize = 1_000_000;
/// Longest line.
pub const MAX_LINE: usize = 64 * 1024;

/// A node of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Y {
    Null,
    Str(String),
    Seq(Vec<Y>),
    /// Entries in file order; a repeated key keeps both entries and [`Y::get`] answers the first.
    Map(Vec<(String, Y)>),
}

impl Y {
    pub fn get(&self, key: &str) -> Option<&Y> {
        match self {
            Self::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> &[Y] {
        match self {
            Self::Seq(s) => s,
            _ => &[],
        }
    }

    pub fn entries(&self) -> &[(String, Y)] {
        match self {
            Self::Map(m) => m,
            _ => &[],
        }
    }

    /// The string at `key`, when there is one.
    pub fn str_at(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Self::as_str)
    }
}

struct Line {
    indent: usize,
    text: String,
    no: usize,
}

struct Reader {
    lines: Vec<Line>,
    pos: usize,
    nodes: usize,
}

fn err<T>(no: usize, m: &str) -> Result<T, String> {
    Err(format!("line {no}: {m}"))
}

/// Parses `text`. An empty document is `Y::Null`.
pub fn parse(text: &str) -> Result<Y, String> {
    let mut lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let no = i + 1;
        if raw.len() > MAX_LINE {
            return err(no, "the line is too long");
        }
        let raw = raw.trim_end_matches('\r');
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let body = &raw[indent..];
        if body.starts_with('\t') {
            return err(no, "a tab indents the line");
        }
        let body = strip_comment(body);
        let body = body.trim_end();
        if body.is_empty() {
            continue;
        }
        if indent == 0 && body == "---" && lines.is_empty() {
            continue;
        }
        if indent == 0 && (body == "..." || body == "---" || body.starts_with("--- ")) {
            if body == "..." {
                break;
            }
            return err(no, "more than one document");
        }
        lines.push(Line {
            indent,
            text: body.to_owned(),
            no,
        });
    }
    if lines.is_empty() {
        return Ok(Y::Null);
    }
    let mut r = Reader {
        lines,
        pos: 0,
        nodes: 0,
    };
    let first = r.lines[0].indent;
    let v = r.node(first, 0)?;
    if r.pos < r.lines.len() {
        return err(r.lines[r.pos].no, "unexpected indentation");
    }
    Ok(v)
}

/// `s` without a trailing comment: a `#` at the start or after a space, outside quotes.
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    let mut quote = 0u8;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == b'\\' && quote == b'"' {
                i += 2;
                continue;
            }
            if c == quote {
                if quote == b'\'' && b.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
        } else if (c == b'\'' || c == b'"') && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            quote = c;
        } else if c == b'#' && (i == 0 || b[i - 1] == b' ') {
            return &s[..i];
        }
        i += 1;
    }
    s
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl Reader {
    fn bump(&mut self, no: usize) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return err(no, "the document has too many nodes");
        }
        Ok(())
    }

    fn node(&mut self, indent: usize, depth: usize) -> Result<Y, String> {
        let no = self.lines[self.pos].no;
        if depth > MAX_DEPTH {
            return err(no, "nested too deeply");
        }
        self.bump(no)?;
        let text = self.lines[self.pos].text.clone();
        if is_item(&text) {
            return self.seq(indent, depth);
        }
        if split_key(&text, no)?.is_some() {
            return self.map(indent, depth);
        }
        self.pos += 1;
        scalar(&text, no, depth, &mut self.nodes)
    }

    fn seq(&mut self, indent: usize, depth: usize) -> Result<Y, String> {
        let mut out = Vec::new();
        while self.pos < self.lines.len()
            && self.lines[self.pos].indent == indent
            && is_item(&self.lines[self.pos].text)
        {
            let no = self.lines[self.pos].no;
            let rest = self.lines[self.pos].text[1..].trim_start().to_owned();
            if rest.is_empty() {
                self.pos += 1;
                if self.pos < self.lines.len() && self.lines[self.pos].indent > indent {
                    let ind = self.lines[self.pos].indent;
                    out.push(self.node(ind, depth + 1)?);
                } else {
                    self.bump(no)?;
                    out.push(Y::Null);
                }
                continue;
            }
            // The rest of the line is content at the column it starts in: a mapping may go on below.
            let col = indent + (self.lines[self.pos].text.len() - rest.len());
            self.lines[self.pos].indent = col;
            self.lines[self.pos].text = rest;
            out.push(self.node(col, depth + 1)?);
        }
        Ok(Y::Seq(out))
    }

    fn map(&mut self, indent: usize, depth: usize) -> Result<Y, String> {
        let mut out: Vec<(String, Y)> = Vec::new();
        while self.pos < self.lines.len()
            && self.lines[self.pos].indent == indent
            && !is_item(&self.lines[self.pos].text)
        {
            let no = self.lines[self.pos].no;
            let text = self.lines[self.pos].text.clone();
            let Some((key, value)) = split_key(&text, no)? else {
                return err(no, "expected `key: value`");
            };
            self.bump(no)?;
            self.pos += 1;
            let v = if value.is_empty() {
                match self.lines.get(self.pos) {
                    Some(n) if n.indent > indent => {
                        let ind = n.indent;
                        self.node(ind, depth + 1)?
                    }
                    Some(n) if n.indent == indent && is_item(&n.text) => {
                        self.seq(indent, depth + 1)?
                    }
                    _ => Y::Null,
                }
            } else if value.starts_with('|') || value.starts_with('>') {
                while self.lines.get(self.pos).is_some_and(|n| n.indent > indent) {
                    self.pos += 1;
                }
                Y::Str(String::new())
            } else {
                scalar(&value, no, depth + 1, &mut self.nodes)?
            };
            out.push((key, v));
        }
        Ok(Y::Map(out))
    }
}

/// Splits `key: value`; `None` when the line is no mapping entry. The value is trimmed.
fn split_key(text: &str, no: usize) -> Result<Option<(String, String)>, String> {
    let b = text.as_bytes();
    if b.is_empty() {
        return Ok(None);
    }
    if matches!(b[0], b'&' | b'*' | b'!') || text.starts_with("? ") {
        return err(no, "anchors, aliases, tags and complex keys are not read");
    }
    if b[0] == b'"' || b[0] == b'\'' {
        let (key, end) = quoted(text, no)?;
        let rest = text[end..].trim_start();
        return Ok(rest
            .strip_prefix(':')
            .filter(|r| r.is_empty() || r.starts_with(' '))
            .map(|r| (key, r.trim().to_owned())));
    }
    if b[0] == b'[' || b[0] == b'{' {
        return Ok(None);
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ') {
            return Ok(Some((
                text[..i].trim_end().to_owned(),
                text[i + 1..].trim().to_owned(),
            )));
        }
        i += 1;
    }
    Ok(None)
}

/// Reads a quoted scalar that starts at the first byte of `s`; returns it and the byte after the
/// closing quote.
fn quoted(s: &str, no: usize) -> Result<(String, usize), String> {
    let single = s.as_bytes()[0] == b'\'';
    let mut out = String::new();
    let mut chars = s.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        let after = at + c.len_utf8();
        if single {
            if c == '\'' {
                if s[after..].starts_with('\'') {
                    out.push('\'');
                    chars.next();
                    continue;
                }
                return Ok((out, after));
            }
            out.push(c);
        } else {
            match c {
                '"' => return Ok((out, after)),
                '\\' => {
                    let Some((_, e)) = chars.next() else {
                        break;
                    };
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '0' => out.push('\0'),
                        '"' | '\\' | '/' | ' ' => out.push(e),
                        'u' => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let Some((_, h)) = chars.next() else {
                                    return err(no, "a short \\u escape");
                                };
                                let Some(d) = h.to_digit(16) else {
                                    return err(no, "a bad \\u escape");
                                };
                                code = code * 16 + d;
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        _ => return err(no, "an unknown escape"),
                    }
                }
                c => out.push(c),
            }
        }
    }
    err(no, "a quoted scalar does not end on its line")
}

/// A scalar or one-line flow collection.
fn scalar(text: &str, no: usize, depth: usize, nodes: &mut usize) -> Result<Y, String> {
    if depth > MAX_DEPTH {
        return err(no, "nested too deeply");
    }
    let t = text.trim();
    let mut i = 0;
    let v = flow_value(t, &mut i, no, depth, nodes, false)?;
    if t[i..].trim().is_empty() {
        Ok(v)
    } else {
        err(no, "unexpected text after the value")
    }
}

fn skip_ws(s: &str, i: &mut usize) {
    while s[*i..].starts_with(' ') {
        *i += 1;
    }
}

fn flow_value(
    s: &str,
    i: &mut usize,
    no: usize,
    depth: usize,
    nodes: &mut usize,
    in_flow: bool,
) -> Result<Y, String> {
    if depth > MAX_DEPTH {
        return err(no, "nested too deeply");
    }
    *nodes += 1;
    if *nodes > MAX_NODES {
        return err(no, "the document has too many nodes");
    }
    skip_ws(s, i);
    let rest = &s[*i..];
    let Some(first) = rest.chars().next() else {
        return Ok(Y::Null);
    };
    match first {
        '"' | '\'' => {
            let (v, n) = quoted(rest, no)?;
            *i += n;
            Ok(Y::Str(v))
        }
        '[' => {
            *i += 1;
            let mut out = Vec::new();
            loop {
                skip_ws(s, i);
                if s[*i..].starts_with(']') {
                    *i += 1;
                    return Ok(Y::Seq(out));
                }
                out.push(flow_value(s, i, no, depth + 1, nodes, true)?);
                skip_ws(s, i);
                if s[*i..].starts_with(',') {
                    *i += 1;
                } else if !s[*i..].starts_with(']') {
                    return err(no, "a flow sequence is not closed");
                }
            }
        }
        '{' => {
            *i += 1;
            let mut out = Vec::new();
            loop {
                skip_ws(s, i);
                if s[*i..].starts_with('}') {
                    *i += 1;
                    return Ok(Y::Map(out));
                }
                let key = match flow_value(s, i, no, depth + 1, nodes, true)? {
                    Y::Str(k) => k,
                    _ => return err(no, "a flow mapping key is not a string"),
                };
                skip_ws(s, i);
                let value = if s[*i..].starts_with(':') {
                    *i += 1;
                    flow_value(s, i, no, depth + 1, nodes, true)?
                } else {
                    Y::Null
                };
                out.push((key, value));
                skip_ws(s, i);
                if s[*i..].starts_with(',') {
                    *i += 1;
                } else if !s[*i..].starts_with('}') {
                    return err(no, "a flow mapping is not closed");
                }
            }
        }
        '&' | '*' | '!' => err(no, "anchors, aliases and tags are not read"),
        '|' | '>' if !in_flow => err(no, "a block scalar is not read here"),
        _ => {
            // A plain scalar: to the end of the line, or in a flow collection to `,` `]` `}` or `: `.
            let b = rest.as_bytes();
            let mut n = 0;
            while n < b.len() {
                let c = b[n];
                if in_flow {
                    if matches!(c, b',' | b']' | b'}') {
                        break;
                    }
                    if c == b':' && (n + 1 == b.len() || matches!(b[n + 1], b' ' | b',' | b'}')) {
                        break;
                    }
                }
                n += 1;
            }
            let v = rest[..n].trim_end();
            *i += n;
            Ok(if v.is_empty() || v == "~" {
                Y::Null
            } else {
                Y::Str(v.to_owned())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Y {
        Y::Str(v.to_owned())
    }

    #[test]
    fn mappings_sequences_and_scalars_nest_by_indent() {
        let y = parse(
            "# a lockfile\nlockfileVersion: '9.0'\n\nimporters:\n  .:\n    dependencies:\n      left-pad:\n        specifier: ^1.3.0\n        version: 1.3.0\n  packages/a: {}\nlist:\n- one\n- two: 2\n  three: 3\n-\n  - nested\n",
        )
        .unwrap();
        assert_eq!(y.str_at("lockfileVersion"), Some("9.0"));
        let dep = y
            .get("importers")
            .unwrap()
            .get(".")
            .unwrap()
            .get("dependencies")
            .unwrap()
            .get("left-pad")
            .unwrap();
        assert_eq!(dep.str_at("specifier"), Some("^1.3.0"));
        assert_eq!(dep.str_at("version"), Some("1.3.0"));
        assert_eq!(
            y.get("importers").unwrap().get("packages/a"),
            Some(&Y::Map(vec![]))
        );
        let list = y.get("list").unwrap().as_seq();
        assert_eq!(list[0], s("one"));
        assert_eq!(list[1].str_at("two"), Some("2"));
        assert_eq!(list[1].str_at("three"), Some("3"));
        assert_eq!(list[2], Y::Seq(vec![s("nested")]));
    }

    #[test]
    fn quoted_keys_and_values_keep_their_punctuation() {
        let y = parse(
            "packages:\n  '@babel/core@7.0.0':\n    resolution: {integrity: sha512-abc==, tarball: 'https://r.example/x.tgz'}\n    engines: {node: '>=6.9.0'}\n  \"a#b\": \"x # y\"  # comment\n  it: 'it''s'\n",
        )
        .unwrap();
        let p = y.get("packages").unwrap();
        let babel = p.get("@babel/core@7.0.0").unwrap();
        assert_eq!(
            babel.get("resolution").unwrap().str_at("integrity"),
            Some("sha512-abc==")
        );
        assert_eq!(
            babel.get("resolution").unwrap().str_at("tarball"),
            Some("https://r.example/x.tgz")
        );
        assert_eq!(
            babel.get("engines").unwrap().str_at("node"),
            Some(">=6.9.0")
        );
        assert_eq!(p.str_at("a#b"), Some("x # y"));
        assert_eq!(p.str_at("it"), Some("it's"));
    }

    #[test]
    fn a_sequence_item_may_open_a_mapping_with_a_nested_sequence() {
        let y = parse(
            "PODS:\n  - Alamofire (5.6.1)\n  - Firebase/Core (8.0.0):\n    - FirebaseAnalytics (= 8.0.0)\n    - GoogleUtilities (~> 7.0)\n  - Other (1.0)\nDEPENDENCIES:\n  - Alamofire (~> 5.0)\n",
        )
        .unwrap();
        let pods = y.get("PODS").unwrap().as_seq();
        assert_eq!(pods.len(), 3);
        assert_eq!(pods[0], s("Alamofire (5.6.1)"));
        let core = pods[1].get("Firebase/Core (8.0.0)").unwrap().as_seq();
        assert_eq!(
            core,
            &[
                s("FirebaseAnalytics (= 8.0.0)"),
                s("GoogleUtilities (~> 7.0)")
            ]
        );
        assert_eq!(pods[2], s("Other (1.0)"));
        assert_eq!(y.get("DEPENDENCIES").unwrap().as_seq().len(), 1);
    }

    #[test]
    fn flow_collections_nest_and_block_scalars_are_skipped() {
        let y = parse("a: [1, 'two', {k: v}, []]\nb: |\n  text: not a key\n  more\nc: after\n")
            .unwrap();
        let a = y.get("a").unwrap().as_seq();
        assert_eq!(a[0], s("1"));
        assert_eq!(a[1], s("two"));
        assert_eq!(a[2].str_at("k"), Some("v"));
        assert_eq!(a[3], Y::Seq(vec![]));
        assert_eq!(y.str_at("c"), Some("after"));
    }

    #[test]
    fn an_empty_value_is_null_and_an_empty_document_is_null() {
        assert_eq!(parse("").unwrap(), Y::Null);
        assert_eq!(parse("# nothing\n\n").unwrap(), Y::Null);
        let y = parse("a:\nb: ~\nc: x\n").unwrap();
        assert_eq!(y.get("a"), Some(&Y::Null));
        assert_eq!(y.get("b"), Some(&Y::Null));
    }

    #[test]
    fn what_the_lockfiles_never_use_is_refused() {
        for (text, why) in [
            ("a: &x 1\nb: *x\n", "anchor"),
            ("a: !!str 1\n", "tag"),
            ("a: 1\n---\nb: 2\n", "second document"),
            ("\ta: 1\n", "tab"),
            ("a: 'open\n", "unterminated quote"),
            ("a: [1, 2\n", "unclosed flow"),
            ("a: {b: 1\n", "unclosed flow map"),
            ("? a\n: b\n", "complex key"),
            ("a: 1\n  b: 2\n", "bad indent"),
            ("a: \"\\q\"\n", "bad escape"),
        ] {
            assert!(parse(text).is_err(), "{why}: {text:?}");
        }
    }

    #[test]
    fn the_bounds_hold() {
        let deep: String = (0..MAX_DEPTH + 10)
            .map(|i| format!("{}a:\n", " ".repeat(i)))
            .collect();
        assert!(parse(&deep).unwrap_err().contains("deeply"));
        let flow = format!(
            "a: {}{}\n",
            "[".repeat(MAX_DEPTH + 5),
            "]".repeat(MAX_DEPTH + 5)
        );
        assert!(parse(&flow).is_err());
        let long = format!("a: {}\n", "x".repeat(MAX_LINE + 1));
        assert!(parse(&long).unwrap_err().contains("too long"));
        // Lines within the length bound still add up to more nodes than a document may hold.
        let many: String = (0..40)
            .map(|i| format!("k{i}: [{}1]\n", "1,".repeat(30_000)))
            .collect();
        assert!(parse(&many).unwrap_err().contains("too many nodes"));
    }

    #[test]
    fn a_repeated_key_answers_the_first() {
        let y = parse("a: 1\na: 2\n").unwrap();
        assert_eq!(y.str_at("a"), Some("1"));
        assert_eq!(y.entries().len(), 2);
    }
}
