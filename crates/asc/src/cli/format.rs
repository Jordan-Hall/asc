//! git 2.55's complete pretty-format placeholder set (SPEC §11.1), shared by `log`, `show` and
//! every other command that takes `--format`/`--pretty`.
//!
//! A format string is parsed once into a [`Template`]; [`Template::render`] expands it for one
//! commit. Unknown `%` sequences stay literal, as in git. The placeholders that need more than
//! the commit itself (decorations, notes, reflog, `%(describe)`) read [`Extras`] that the caller
//! fills in, so this module does no repository access.

use std::collections::HashMap;

use asc_core::id::GitOid;
use asc_core::sig::Signature;

pub mod color;
pub mod date;
pub mod decor;
pub mod layout;
pub mod mailmap;
pub mod trailers;

use decor::{Decor, Style};
use mailmap::Mailmap;

/// The commit being formatted.
pub struct Commit<'a> {
    pub id: GitOid,
    pub tree: GitOid,
    pub parents: &'a [GitOid],
    pub author: &'a Signature,
    pub committer: &'a Signature,
    pub message: &'a [u8],
    /// The `encoding` header, when the commit has one.
    pub encoding: Option<&'a [u8]>,
}

/// A reflog entry for `log -g` (`%gd %gD %gn %gN %ge %gE %gs`).
#[derive(Clone, Debug, Default)]
pub struct Reflog {
    /// `refs/heads/master@{1}`.
    pub selector: String,
    /// `master@{1}`.
    pub short_selector: String,
    pub name: Vec<u8>,
    pub email: Vec<u8>,
    pub subject: Vec<u8>,
}

/// Invocation-wide settings.
pub struct Env<'a> {
    /// Hex digits of `%h %t %p`.
    pub abbrev: usize,
    /// `--date=` for `%ad %cd`.
    pub date: date::Mode,
    /// The time relative dates are measured from.
    pub now: i64,
    pub color: bool,
    /// `--decorate=full` for `%d`.
    pub decorate_full: bool,
    pub mailmap: Option<&'a Mailmap>,
    /// Terminal width for `%<|(-n)`.
    pub columns: usize,
}

impl Default for Env<'_> {
    fn default() -> Self {
        Self {
            abbrev: 7,
            date: date::Mode::default(),
            now: 0,
            color: false,
            decorate_full: false,
            mailmap: None,
            columns: 80,
        }
    }
}

/// What differs per commit beyond the commit itself.
pub struct Extras<'a> {
    pub decor: Option<&'a Decor>,
    /// `%S`, with `log --source`.
    pub source: Option<&'a str>,
    /// `%m`: `<`, `>` or `-`.
    pub mark: char,
    /// `%N`.
    pub notes: &'a [u8],
    pub reflog: Option<&'a Reflog>,
    /// `%(describe:<options>)` answers by the option text.
    pub describe: Option<&'a HashMap<String, String>>,
    /// Columns the `--graph` prefix takes, for `%<|(…)`.
    pub graph_width: usize,
}

impl Default for Extras<'_> {
    fn default() -> Self {
        Self {
            decor: None,
            source: None,
            mark: '>',
            notes: b"",
            reflog: None,
            describe: None,
            graph_width: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Magic {
    None,
    /// `%-x`: drop preceding line feeds when empty.
    DelLf,
    /// `%+x`: add a line feed before when not empty.
    AddLf,
    /// `% x`: add a space before when not empty.
    AddSp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    Author,
    Committer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Name,
    NameMap,
    Email,
    EmailMap,
    Local,
    LocalMap,
    Date,
    Rfc,
    Relative,
    Unix,
    Iso,
    Strict,
    Short,
    Human,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RField {
    Selector,
    ShortSelector,
    Name,
    NameMap,
    Email,
    EmailMap,
    Subject,
}

#[derive(Clone, Debug)]
enum Ph {
    Literal(Vec<u8>),
    Hash,
    AbbrevHash,
    Tree,
    AbbrevTree,
    Parents,
    AbbrevParents,
    Person(Who, Field),
    Decorate,
    DecorateBare,
    DecorateWith(Box<DecorOpts>),
    Describe(String),
    Source,
    Encoding,
    Subject,
    Sanitized,
    Body,
    Raw,
    Notes,
    SigStatus,
    SigEmpty,
    SigTrust,
    Reflog(RField),
    Trailers(trailers::Opts),
    Mark,
    Count,
}

#[derive(Clone, Debug, Default)]
struct DecorOpts {
    prefix: Option<Vec<u8>>,
    suffix: Option<Vec<u8>>,
    separator: Option<Vec<u8>>,
    pointer: Option<Vec<u8>>,
    tag: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flush {
    Left,
    Right,
    Both,
    /// `%>>`: right-aligned, borrowing spaces on its left when the text is too wide.
    RightEat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trunc {
    None,
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug)]
struct Pad {
    flush: Flush,
    column: bool,
    width: i64,
    trunc: Trunc,
}

#[derive(Clone, Debug)]
struct Tint {
    sgr: String,
    forced: bool,
    never: bool,
}

#[derive(Clone, Debug)]
enum Piece {
    Lit(Vec<u8>),
    Ph(Magic, Ph),
    Tint(Tint),
    Wrap(usize, usize, usize),
    Pad(Pad),
}

/// A parsed format string.
#[derive(Clone, Debug)]
pub struct Template {
    pieces: Vec<Piece>,
}

// ------------------------------------------------------------------------------- parsing

/// `%xNN`, `%n` and `%%` inside option values.
fn expand_literals(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            match raw.get(i + 1) {
                Some(b'n') => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                Some(b'%') => {
                    out.push(b'%');
                    i += 2;
                    continue;
                }
                Some(b'x') => {
                    if let Some(b) = hex_byte(raw.get(i + 2..i + 4)) {
                        out.push(b);
                        i += 4;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

fn hex_byte(digits: Option<&[u8]>) -> Option<u8> {
    let d = digits?;
    let text = std::str::from_utf8(d).ok()?;
    if !d.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u8::from_str_radix(text, 16).ok()
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "" | "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// The text between `(` and the matching `)` of `rest` (which starts after `%(`).
fn paren_body(rest: &[u8]) -> Option<(&[u8], usize)> {
    let end = rest.iter().position(|&b| b == b')')?;
    Some((&rest[..end], end + 1))
}

fn parse_trailers(options: &str) -> Option<trailers::Opts> {
    let mut o = trailers::Opts::default();
    if options.is_empty() {
        return Some(o);
    }
    for item in options.split(',') {
        let (name, value) = match item.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (item, None),
        };
        let flag = |v: Option<&str>| v.map_or(Some(true), parse_bool);
        match name {
            "key" => {
                o.keys.push(expand_literals(value?.as_bytes()));
                o.only = true;
            }
            "only" => o.only = flag(value)?,
            "unfold" => o.unfold = flag(value)?,
            "keyonly" => o.keyonly = flag(value)?,
            "valueonly" => o.valueonly = flag(value)?,
            "separator" => o.separator = Some(expand_literals(value?.as_bytes())),
            "key_value_separator" => o.kv_separator = Some(expand_literals(value?.as_bytes())),
            _ => return None,
        }
    }
    Some(o)
}

fn parse_decorate(options: &str) -> Option<DecorOpts> {
    let mut o = DecorOpts::default();
    if options.is_empty() {
        return Some(o);
    }
    for item in options.split(',') {
        let (name, value) = item.split_once('=')?;
        let v = Some(expand_literals(value.as_bytes()));
        match name {
            "prefix" => o.prefix = v,
            "suffix" => o.suffix = v,
            "separator" => o.separator = v,
            "pointer" => o.pointer = v,
            "tag" => o.tag = v,
            _ => return None,
        }
    }
    Some(o)
}

fn parse_describe(options: &str) -> bool {
    options.is_empty()
        || options.split(',').all(|item| {
            let name = item.split_once('=').map_or(item, |(n, _)| n);
            matches!(name, "tags" | "abbrev" | "match" | "exclude")
        })
}

/// `%w(…)`: the numbers inside the parentheses.
fn parse_wrap(body: &str) -> Option<(usize, usize, usize)> {
    let mut nums = [0usize; 3];
    for (slot, part) in nums.iter_mut().zip(body.split(',')) {
        let part = part.trim();
        if !part.is_empty() {
            *slot = part.parse().ok()?;
        }
    }
    (body.split(',').count() <= 3).then_some((nums[0], nums[1], nums[2]))
}

/// `%<(N[,trunc])` and its relatives. `rest` starts after the `%`; returns the pad and the bytes
/// consumed.
fn parse_pad(rest: &[u8]) -> Option<(Pad, usize)> {
    let mut i = 1;
    let flush = match rest[0] {
        b'<' => Flush::Left,
        b'>' if rest.get(1) == Some(&b'>') => {
            i = 2;
            Flush::RightEat
        }
        b'>' if rest.get(1) == Some(&b'<') => {
            i = 2;
            Flush::Both
        }
        _ => Flush::Right,
    };
    let column = rest.get(i) == Some(&b'|');
    if column {
        i += 1;
    }
    if rest.get(i) != Some(&b'(') {
        return None;
    }
    let (body, used) = paren_body(&rest[i + 1..])?;
    let body = std::str::from_utf8(body).ok()?;
    let (num, trunc) = match body.split_once(',') {
        Some((n, t)) => (
            n,
            match t {
                "trunc" => Trunc::Right,
                "ltrunc" => Trunc::Left,
                "mtrunc" => Trunc::Middle,
                _ => return None,
            },
        ),
        None => (body, Trunc::None),
    };
    let width: i64 = num.trim().parse().ok()?;
    if width == 0 || (width < 0 && !column) {
        return None;
    }
    Some((Pad { flush, column, width, trunc }, i + 1 + used))
}

/// One placeholder after the `%` and any modifier: the placeholder and the bytes consumed.
fn parse_placeholder(rest: &[u8]) -> Option<(Ph, usize)> {
    let at = |i: usize| rest.get(i).copied();
    Some(match at(0)? {
        b'n' => (Ph::Literal(b"\n".to_vec()), 1),
        b'%' => (Ph::Literal(b"%".to_vec()), 1),
        b'x' => (Ph::Literal(vec![hex_byte(rest.get(1..3))?]), 3),
        b'H' => (Ph::Hash, 1),
        b'h' => (Ph::AbbrevHash, 1),
        b'T' => (Ph::Tree, 1),
        b't' => (Ph::AbbrevTree, 1),
        b'P' => (Ph::Parents, 1),
        b'p' => (Ph::AbbrevParents, 1),
        b'd' => (Ph::Decorate, 1),
        b'D' => (Ph::DecorateBare, 1),
        b'S' => (Ph::Source, 1),
        b'e' => (Ph::Encoding, 1),
        b's' => (Ph::Subject, 1),
        b'f' => (Ph::Sanitized, 1),
        b'b' => (Ph::Body, 1),
        b'B' => (Ph::Raw, 1),
        b'N' => (Ph::Notes, 1),
        b'm' => (Ph::Mark, 1),
        c @ (b'a' | b'c') => {
            let who = if c == b'a' { Who::Author } else { Who::Committer };
            let field = match at(1)? {
                b'n' => Field::Name,
                b'N' => Field::NameMap,
                b'e' => Field::Email,
                b'E' => Field::EmailMap,
                b'l' => Field::Local,
                b'L' => Field::LocalMap,
                b'd' => Field::Date,
                b'D' => Field::Rfc,
                b'r' => Field::Relative,
                b't' => Field::Unix,
                b'i' => Field::Iso,
                b'I' => Field::Strict,
                b's' => Field::Short,
                b'h' => Field::Human,
                _ => return None,
            };
            (Ph::Person(who, field), 2)
        }
        b'G' => match at(1)? {
            b'?' => (Ph::SigStatus, 2),
            b'G' | b'S' | b'K' | b'F' | b'P' => (Ph::SigEmpty, 2),
            b'T' => (Ph::SigTrust, 2),
            _ => return None,
        },
        b'g' => {
            let f = match at(1)? {
                b'D' => RField::Selector,
                b'd' => RField::ShortSelector,
                b'n' => RField::Name,
                b'N' => RField::NameMap,
                b'e' => RField::Email,
                b'E' => RField::EmailMap,
                b's' => RField::Subject,
                _ => return None,
            };
            (Ph::Reflog(f), 2)
        }
        b'(' => {
            let (body, used) = paren_body(&rest[1..])?;
            let body = std::str::from_utf8(body).ok()?;
            let (name, options) = body.split_once(':').unwrap_or((body, ""));
            let ph = match name {
                "count" | "total" if !body.contains(':') => Ph::Count,
                "trailers" => Ph::Trailers(parse_trailers(options)?),
                "decorate" => Ph::DecorateWith(Box::new(parse_decorate(options)?)),
                "describe" if parse_describe(options) => Ph::Describe(options.to_owned()),
                _ => return None,
            };
            (ph, 1 + used)
        }
        _ => return None,
    })
}

impl Template {
    /// Parses `format`; every byte string is a valid template.
    pub fn parse(format: &[u8]) -> Self {
        let mut pieces = Vec::new();
        let mut lit = Vec::new();
        let mut i = 0;
        let flush = |lit: &mut Vec<u8>, pieces: &mut Vec<Piece>| {
            if !lit.is_empty() {
                pieces.push(Piece::Lit(std::mem::take(lit)));
            }
        };
        while i < format.len() {
            if format[i] != b'%' {
                lit.push(format[i]);
                i += 1;
                continue;
            }
            let rest = &format[i + 1..];
            if let Some((piece, used)) = Self::directive(rest) {
                flush(&mut lit, &mut pieces);
                pieces.push(piece);
                i += 1 + used;
                continue;
            }
            let (magic, skip) = match rest.first() {
                Some(b'-') => (Magic::DelLf, 1),
                Some(b'+') => (Magic::AddLf, 1),
                Some(b' ') => (Magic::AddSp, 1),
                _ => (Magic::None, 0),
            };
            if let Some((ph, used)) = parse_placeholder(&rest[skip..]) {
                flush(&mut lit, &mut pieces);
                pieces.push(Piece::Ph(magic, ph));
                i += 1 + skip + used;
            } else {
                lit.push(b'%');
                i += 1;
            }
        }
        flush(&mut lit, &mut pieces);
        Self { pieces }
    }

    /// `%C…`, `%w(…)`, `%<(…)`: the directives that change how later text is written.
    fn directive(rest: &[u8]) -> Option<(Piece, usize)> {
        match *rest.first()? {
            b'C' => {
                let tail = &rest[1..];
                for (name, sgr) in [("red", "red"), ("green", "green"), ("blue", "blue"), ("reset", "reset")] {
                    if tail.starts_with(name.as_bytes()) {
                        let t = Tint { sgr: color::sgr(sgr), forced: false, never: false };
                        return Some((Piece::Tint(t), 1 + name.len()));
                    }
                }
                let (body, used) = paren_body(tail.strip_prefix(b"(")?)?;
                let body = std::str::from_utf8(body).ok()?;
                let (mut forced, mut never, mut spec) = (false, false, body);
                if let Some(s) = body.strip_prefix("always,") {
                    (forced, spec) = (true, s);
                } else if let Some(s) = body.strip_prefix("never,") {
                    (never, spec) = (true, s);
                } else if let Some(s) = body.strip_prefix("auto,") {
                    spec = s;
                }
                let sgr = if spec == "auto" { String::new() } else { color::Spec::parse(spec)?.sgr() };
                Some((Piece::Tint(Tint { sgr, forced, never }), 1 + 1 + used))
            }
            b'w' => {
                let (body, used) = paren_body(rest[1..].strip_prefix(b"(")?)?;
                let (w, i1, i2) = parse_wrap(std::str::from_utf8(body).ok()?)?;
                Some((Piece::Wrap(w, i1, i2), 2 + used))
            }
            b'<' | b'>' => parse_pad(rest).map(|(p, used)| (Piece::Pad(p), used)),
            _ => None,
        }
    }

    /// Whether the format string is empty.
    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The `%(describe:<options>)` option texts the template uses, for the caller to answer.
    pub fn describe_options(&self) -> Vec<String> {
        self.pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Ph(_, Ph::Describe(o)) => Some(o.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether the template reads the reflog placeholders.
    pub fn uses_reflog(&self) -> bool {
        self.pieces.iter().any(|p| matches!(p, Piece::Ph(_, Ph::Reflog(_))))
    }

    /// Whether the template uses `%d`, `%D` or `%(decorate)`.
    pub fn uses_decorations(&self) -> bool {
        self.pieces
            .iter()
            .any(|p| matches!(p, Piece::Ph(_, Ph::Decorate | Ph::DecorateBare | Ph::DecorateWith(_))))
    }

    /// Whether the template uses `%S`.
    pub fn uses_source(&self) -> bool {
        self.pieces.iter().any(|p| matches!(p, Piece::Ph(_, Ph::Source)))
    }

    /// Whether the template uses `%N`.
    pub fn uses_notes(&self) -> bool {
        self.pieces.iter().any(|p| matches!(p, Piece::Ph(_, Ph::Notes)))
    }

    /// Expands the template for `commit`, appending to `out`.
    pub fn render(&self, commit: &Commit<'_>, env: &Env<'_>, extras: &Extras<'_>, out: &mut Vec<u8>) {
        let mut r = Renderer { commit, env, extras, base: out.len(), wrap: (0, 0, 0), wrap_start: out.len(), pending: None, out };
        let mut i = 0;
        while i < self.pieces.len() {
            i += r.piece(&self.pieces, i);
        }
        r.rewrap((0, 0, 0));
    }
}

// ------------------------------------------------------------------------------ rendering

struct Renderer<'a, 'b> {
    commit: &'a Commit<'a>,
    env: &'a Env<'a>,
    extras: &'a Extras<'a>,
    /// Where this commit's output starts in `out`.
    base: usize,
    wrap: (usize, usize, usize),
    wrap_start: usize,
    pending: Option<Pad>,
    out: &'b mut Vec<u8>,
}

fn hex(oid: &GitOid) -> String {
    oid.to_string()
}

/// The subject paragraph and the offset of the body (git's `parse_commit_message`).
fn split_message(msg: &[u8]) -> (Vec<u8>, usize) {
    let lines = |from: usize| {
        let end = msg[from..].iter().position(|&b| b == b'\n').map_or(msg.len(), |p| from + p + 1);
        (&msg[from..end], end)
    };
    let blank = |l: &[u8]| l.iter().all(u8::is_ascii_whitespace);
    let mut pos = 0;
    while pos < msg.len() {
        let (l, next) = lines(pos);
        if !blank(l) {
            break;
        }
        pos = next;
    }
    let mut subject: Vec<u8> = Vec::new();
    let mut first = true;
    while pos < msg.len() {
        let (l, next) = lines(pos);
        if blank(l) {
            break;
        }
        if !first {
            subject.push(b' ');
        }
        subject.extend_from_slice(l.trim_ascii_end());
        first = false;
        pos = next;
    }
    while pos < msg.len() {
        let (l, next) = lines(pos);
        if !blank(l) {
            break;
        }
        pos = next;
    }
    (subject, pos)
}

/// `%f`: the subject as a file name.
fn sanitized(subject: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut space = 2;
    let mut i = 0;
    while i < subject.len() {
        let c = subject[i];
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'_') {
            if space == 0 {
                space = 1;
            }
        } else {
            if space == 1 {
                out.push(b'-');
            }
            space = 0;
            out.push(c);
            if c == b'.' {
                while subject.get(i + 1) == Some(&b'.') {
                    i += 1;
                }
            }
        }
        i += 1;
    }
    if space == 2 {
        return out;
    }
    while out.last() == Some(&b'.') {
        out.pop();
    }
    out
}

fn local_part(email: &[u8]) -> &[u8] {
    email.iter().position(|&b| b == b'@').map_or(email, |p| &email[..p])
}

impl Renderer<'_, '_> {
    fn text(&self) -> &[u8] {
        self.out.as_slice()
    }

    /// Handles `pieces[i]` (and the pieces it pulls in); returns how many were consumed.
    fn piece(&mut self, pieces: &[Piece], i: usize) -> usize {
        match &pieces[i] {
            Piece::Lit(l) => {
                self.out.extend_from_slice(l);
                1
            }
            Piece::Wrap(w, i1, i2) => {
                self.rewrap((*w, *i1, *i2));
                if let Some(p) = self.pending.take() {
                    self.apply_pad(&p, Vec::new());
                }
                1
            }
            Piece::Pad(p) => {
                match self.pending.take() {
                    Some(old) => self.apply_pad(&old, Vec::new()),
                    None => self.pending = Some(*p),
                }
                1
            }
            Piece::Tint(_) => self.tinted(pieces, i),
            Piece::Ph(magic, ph) => {
                let before = self.out.len();
                let mut local = Vec::new();
                self.expand(ph, &mut local);
                match self.pending.take() {
                    Some(p) => self.apply_pad(&p, local),
                    None => self.out.extend_from_slice(&local),
                }
                self.apply_magic(*magic, before);
                1
            }
        }
    }

    /// A colour directive; with a pending pad it joins the next placeholder, like git.
    fn tinted(&mut self, pieces: &[Piece], i: usize) -> usize {
        let mut local = Vec::new();
        let mut used = 0;
        let mut j = i;
        while let Some(Piece::Tint(t)) = pieces.get(j) {
            self.tint(t, &mut local);
            used += 1;
            j += 1;
            if self.pending.is_none() {
                break;
            }
        }
        match self.pending.take() {
            Some(p) => {
                let mut magic = Magic::None;
                if let Some(Piece::Ph(m, ph)) = pieces.get(j) {
                    self.expand(ph, &mut local);
                    magic = *m;
                    used += 1;
                }
                let before = self.out.len();
                self.apply_pad(&p, local);
                self.apply_magic(magic, before);
            }
            None => self.out.extend_from_slice(&local),
        }
        used
    }

    fn tint(&self, t: &Tint, out: &mut Vec<u8>) {
        if !t.never && (t.forced || self.env.color) {
            out.extend_from_slice(t.sgr.as_bytes());
        }
    }

    fn apply_magic(&mut self, magic: Magic, before: usize) {
        let grew = self.out.len() != before;
        match magic {
            Magic::None => {}
            Magic::DelLf if !grew => {
                while self.out.len() > self.base && self.out.last() == Some(&b'\n') {
                    self.out.pop();
                }
            }
            Magic::AddLf if grew => self.out.insert(before, b'\n'),
            Magic::AddSp if grew => self.out.insert(before, b' '),
            _ => {}
        }
    }

    /// `rewrap_message_tail`: wraps what was written since the last `%w` with the old settings.
    fn rewrap(&mut self, new: (usize, usize, usize)) {
        if self.wrap == new {
            return;
        }
        if self.wrap_start < self.out.len() {
            let tail = self.out.split_off(self.wrap_start);
            let (w, i1, i2) = self.wrap;
            layout::add_wrapped(self.out, &tail, i1, i2, w);
        }
        self.wrap_start = self.out.len();
        self.wrap = new;
    }

    /// `format_and_pad_commit`.
    fn apply_pad(&mut self, pad: &Pad, local: Vec<u8>) {
        let mut padding = pad.width;
        if pad.column {
            let target = if pad.width > 0 { pad.width } else { self.env.columns as i64 + pad.width };
            let line = self.text().rsplit(|&b| b == b'\n').next().unwrap_or(&[]);
            let occupied = (layout::width(line) + self.extras.graph_width) as i64;
            padding = target - occupied;
        }
        let padding = padding.max(0) as usize;
        let mut local = local;
        let len = layout::width(&local);
        let mut padding = padding;
        if len > padding && pad.flush == Flush::RightEat {
            let mut need = len - padding;
            while need > 0 && self.out.len() > self.base && self.out.last() == Some(&b' ') {
                self.out.pop();
                need -= 1;
                padding += 1;
            }
        }
        if len > padding {
            let keep = padding.saturating_sub(2);
            local = match pad.trunc {
                Trunc::Left => layout::replace_columns(&local, 0, len - keep, b".."),
                Trunc::Middle => layout::replace_columns(&local, (padding / 2).saturating_sub(1), len - keep, b".."),
                Trunc::Right => layout::replace_columns(&local, keep, len - keep, b".."),
                Trunc::None => local,
            };
            self.out.extend_from_slice(&local);
            return;
        }
        let gap = padding - len;
        let (left, right) = match pad.flush {
            Flush::Left => (0, gap),
            Flush::Right | Flush::RightEat => (gap, 0),
            Flush::Both => (gap / 2, gap - gap / 2),
        };
        self.out.extend(std::iter::repeat_n(b' ', left));
        self.out.extend_from_slice(&local);
        self.out.extend(std::iter::repeat_n(b' ', right));
    }

    fn person(&self, who: Who) -> &Signature {
        match who {
            Who::Author => self.commit.author,
            Who::Committer => self.commit.committer,
        }
    }

    fn mapped(&self, name: &[u8], email: &[u8]) -> (Vec<u8>, Vec<u8>) {
        match self.env.mailmap {
            Some(m) => m.lookup(name, email),
            None => (name.to_vec(), email.to_vec()),
        }
    }

    fn date(&self, s: &Signature, field: Field) -> String {
        use date::{Kind, Mode};
        let mode = match field {
            Field::Date => self.env.date.clone(),
            Field::Rfc => Mode { kind: Kind::Rfc2822, local: false },
            Field::Relative => Mode { kind: Kind::Relative, local: false },
            Field::Unix => Mode { kind: Kind::Unix, local: false },
            Field::Iso => Mode { kind: Kind::Iso, local: false },
            Field::Strict => Mode { kind: Kind::IsoStrict, local: false },
            Field::Short => Mode { kind: Kind::Short, local: false },
            _ => Mode { kind: Kind::Human, local: false },
        };
        mode.render(s.time(), s.tz_min(), self.env.now)
    }

    fn expand(&self, ph: &Ph, out: &mut Vec<u8>) {
        let c = self.commit;
        let abbrev = self.env.abbrev.clamp(4, 40);
        match ph {
            Ph::Literal(b) => out.extend_from_slice(b),
            Ph::Hash => out.extend_from_slice(hex(&c.id).as_bytes()),
            Ph::AbbrevHash => out.extend_from_slice(&hex(&c.id).as_bytes()[..abbrev]),
            Ph::Tree => out.extend_from_slice(hex(&c.tree).as_bytes()),
            Ph::AbbrevTree => out.extend_from_slice(&hex(&c.tree).as_bytes()[..abbrev]),
            Ph::Parents => {
                let all: Vec<String> = c.parents.iter().map(hex).collect();
                out.extend_from_slice(all.join(" ").as_bytes());
            }
            Ph::AbbrevParents => {
                let all: Vec<String> = c.parents.iter().map(|p| hex(p)[..abbrev].to_owned()).collect();
                out.extend_from_slice(all.join(" ").as_bytes());
            }
            Ph::Person(who, field) => {
                let s = self.person(*who);
                match field {
                    Field::Name => out.extend_from_slice(s.name()),
                    Field::NameMap => out.extend_from_slice(&self.mapped(s.name(), s.email()).0),
                    Field::Email => out.extend_from_slice(s.email()),
                    Field::EmailMap => out.extend_from_slice(&self.mapped(s.name(), s.email()).1),
                    Field::Local => out.extend_from_slice(local_part(s.email())),
                    Field::LocalMap => out.extend_from_slice(local_part(&self.mapped(s.name(), s.email()).1)),
                    f => out.extend_from_slice(self.date(s, *f).as_bytes()),
                }
            }
            Ph::Decorate => self.decoration(&Style::standard(self.env.decorate_full, self.env.color), out),
            Ph::DecorateBare => {
                let mut style = Style::standard(self.env.decorate_full, self.env.color);
                style.prefix.clear();
                style.suffix.clear();
                self.decoration(&style, out);
            }
            Ph::DecorateWith(o) => {
                let mut style = Style::standard(self.env.decorate_full, self.env.color);
                let take = |slot: &mut Vec<u8>, v: &Option<Vec<u8>>| {
                    if let Some(v) = v {
                        slot.clone_from(v);
                    }
                };
                take(&mut style.prefix, &o.prefix);
                take(&mut style.suffix, &o.suffix);
                take(&mut style.separator, &o.separator);
                take(&mut style.pointer, &o.pointer);
                take(&mut style.tag, &o.tag);
                self.decoration(&style, out);
            }
            Ph::Describe(options) => {
                if let Some(text) = self.extras.describe.and_then(|m| m.get(options)) {
                    out.extend_from_slice(text.as_bytes());
                }
            }
            Ph::Source => out.extend_from_slice(self.extras.source.unwrap_or("").as_bytes()),
            Ph::Encoding => out.extend_from_slice(c.encoding.unwrap_or(b"")),
            Ph::Subject => out.extend_from_slice(&split_message(c.message).0),
            Ph::Sanitized => {
                let line = c.message.trim_ascii_start().split(|&b| b == b'\n').next().unwrap_or(&[]);
                out.extend_from_slice(&sanitized(line.trim_ascii_end()));
            }
            Ph::Body => {
                let (_, body) = split_message(c.message);
                out.extend_from_slice(&c.message[body.min(c.message.len())..]);
            }
            Ph::Raw => out.extend_from_slice(c.message),
            Ph::Notes => out.extend_from_slice(self.extras.notes),
            Ph::SigStatus => out.push(b'N'),
            Ph::SigEmpty => {}
            Ph::SigTrust => out.extend_from_slice(b"undefined"),
            Ph::Reflog(f) => {
                let Some(r) = self.extras.reflog else { return };
                match f {
                    RField::Selector => out.extend_from_slice(r.selector.as_bytes()),
                    RField::ShortSelector => out.extend_from_slice(r.short_selector.as_bytes()),
                    RField::Name => out.extend_from_slice(&r.name),
                    RField::NameMap => out.extend_from_slice(&self.mapped(&r.name, &r.email).0),
                    RField::Email => out.extend_from_slice(&r.email),
                    RField::EmailMap => out.extend_from_slice(&self.mapped(&r.name, &r.email).1),
                    RField::Subject => out.extend_from_slice(&r.subject),
                }
            }
            Ph::Trailers(o) => trailers::render(&trailers::parse(c.message), o, out),
            Ph::Mark => {
                let mut b = [0u8; 4];
                out.extend_from_slice(self.extras.mark.encode_utf8(&mut b).as_bytes());
            }
            Ph::Count => out.push(b'0'),
        }
    }

    fn decoration(&self, style: &Style, out: &mut Vec<u8>) {
        if let Some(d) = self.extras.decor {
            d.write(style, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, email: &str, time: i64, tz: i16) -> Signature {
        Signature::new(name.into(), email.into(), time, tz).unwrap()
    }

    fn expand(format: &str, message: &str) -> String {
        let (a, c) = (sig("Ann Author", "ann@example.com", 1_700_000_000, 0), sig("Cy", "cy@x.org", 1_700_000_100, 60));
        let id = GitOid::from_bytes([0xab; 20]);
        let commit = Commit {
            id,
            tree: GitOid::from_bytes([0xcd; 20]),
            parents: &[GitOid::from_bytes([0x12; 20])],
            author: &a,
            committer: &c,
            message: message.as_bytes(),
            encoding: None,
        };
        let mut out = Vec::new();
        Template::parse(format.as_bytes()).render(&commit, &Env { now: 1_700_000_000, ..Env::default() }, &Extras::default(), &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn identity_and_hash_placeholders() {
        assert_eq!(expand("%h %p %an <%ae> %cn %cI", "s\n"), "abababa 1212121 Ann Author <ann@example.com> Cy 2023-11-14T23:15:00+01:00");
        assert_eq!(expand("%al|%cL", "s\n"), "ann|cy");
    }

    #[test]
    fn subject_body_and_sanitized() {
        let m = "Fix: the thing.. now\nsecond line\n\nbody text\nmore\n";
        assert_eq!(expand("%s", m), "Fix: the thing.. now second line");
        assert_eq!(expand("%f", m), "Fix-the-thing.-now");
        assert_eq!(expand("%b", m), "body text\nmore\n");
        assert_eq!(expand("%B", m), m);
    }

    #[test]
    fn literals_and_unknown_sequences() {
        assert_eq!(expand("a%nb%%c%x41%xzz%q%", "s\n"), "a\nb%cA%xzz%q%");
    }

    #[test]
    fn magic_modifiers() {
        assert_eq!(expand("[%+s][%-b][% s]", "s\n"), "[\ns][][ s]");
        assert_eq!(expand("a%n%n%-b|", "s\n"), "a|");
    }

    #[test]
    fn padding_and_truncation() {
        assert_eq!(expand("[%<(6)%s]", "ab\n"), "[ab    ]");
        assert_eq!(expand("[%>(6)%s]", "ab\n"), "[    ab]");
        assert_eq!(expand("[%><(6)%s]", "ab\n"), "[  ab  ]");
        assert_eq!(expand("[%<(5,trunc)%s]", "abcdefghij\n"), "[abc..]");
        assert_eq!(expand("[%<(5,ltrunc)%s]", "abcdefghij\n"), "[..hij]");
        assert_eq!(expand("[%<(5,mtrunc)%s]", "abcdefghij\n"), "[a..ij]");
        assert_eq!(expand("[%<(0)%s]", "ab\n"), "[%<(0)ab]");
    }

    #[test]
    fn wrapping_covers_the_tail() {
        assert_eq!(expand("%w(8,1,3)%B", "aaa bbb ccc\n"), " aaa bbb\n   ccc\n");
    }

    #[test]
    fn colours_need_always_or_a_colour_terminal() {
        assert_eq!(expand("%Cred%s%Creset", "s\n"), "s");
        assert_eq!(expand("%C(always,bold blue)x%C(always,reset)", "s\n"), "\x1b[1;34mx\x1b[m");
    }

    #[test]
    fn trailers_placeholder() {
        let m = "s\n\nbody\n\nSigned-off-by: A\nTicket: 7\n";
        assert_eq!(expand("%(trailers:key=ticket,valueonly)", m), "7\n");
        assert_eq!(expand("%(trailers:only,separator=%x2C )", m), "Signed-off-by: A, Ticket: 7");
    }

    #[test]
    fn describe_options_are_collected() {
        let t = Template::parse(b"%(describe) %(describe:tags,abbrev=9) %(describe:bogus)");
        assert_eq!(t.describe_options(), vec!["".to_owned(), "tags,abbrev=9".to_owned()]);
    }
}
