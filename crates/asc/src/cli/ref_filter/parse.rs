//! The format-string and sort-key grammar of git 2.55's ref-filter: `%(atom:args)` with its
//! `*` deref prefix, literal `%%` and `%xx`. Messages are git's, so a script sees the same
//! diagnostics from either tool.

use super::Error;
use crate::cli::format::{color, date};

/// How a ref name is shortened (`refname:short`, `:lstrip=`, `:rstrip=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strip {
    Whole,
    Short,
    Lstrip(i64),
    Rstrip(i64),
}

/// How an object id is abbreviated (`objectname:short[=n]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abbrev {
    Full,
    /// `None` is the `core.abbrev` default.
    Short(Option<usize>),
}

/// Whose `Name <email> time zone` line a person atom reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Author,
    Committer,
    Tagger,
    /// The committer of a commit, the tagger of a tag.
    Creator,
}

/// Which part of the person line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Line,
    Name { mailmap: bool },
    Email { trim: bool, localpart: bool, mailmap: bool },
    /// `formatted` is set when the atom names a date format: sorting is then by the text.
    Date { mode: date::Mode, formatted: bool },
}

/// `contents:<part>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contents {
    Bare,
    Size,
    Subject,
    Body,
    Signature,
    Lines(usize),
    Trailers(String),
}

/// `signature:<part>` (the GPG state of a commit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    Bare,
    Grade,
    Signer,
    Key,
    Fingerprint,
    PrimaryKeyFingerprint,
    TrustLevel,
}

/// What `upstream` and `push` print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteWhat {
    Ref(Strip),
    Track,
    TrackShort,
    RemoteName,
    RemoteRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remote {
    pub what: RemoteWhat,
    pub nobracket: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Left,
    Middle,
    Right,
}

/// `if` comparisons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IfCmp {
    Nonempty,
    Equals(Vec<u8>),
    NotEquals(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Refname(Strip),
    ObjectType,
    ObjectSize { disk: bool },
    ObjectName(Abbrev),
    DeltaBase,
    Tree(Abbrev),
    Parent(Abbrev),
    NumParent,
    Object,
    Type,
    Tag,
    /// `bogus` marks `author:x`-style spellings that git accepts and leaves empty.
    Person { who: Who, part: Part, bogus: bool },
    Subject { sanitize: bool },
    Body,
    Trailers(String),
    Contents(Contents),
    Signature(Sig),
    Raw { size: bool },
    Upstream(Remote),
    Push(Remote),
    Symref(Strip),
    Flag,
    Head,
    Color(String),
    Worktreepath,
    Describe(String),
    AheadBehind(String),
    IsBase(String),
    Align { width: usize, position: Position },
    If(IfCmp),
    Then,
    Else,
    End,
}

/// One parsed `%(…)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    pub deref: bool,
    pub kind: Kind,
    /// The atom as typed (without `%(`/`)`): the key of the per-ref values loaded for it.
    pub text: String,
}

/// One piece of a format string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    Lit(Vec<u8>),
    Atom(Atom),
}

fn fatal(text: impl Into<String>) -> Error {
    Error::Fatal(text.into())
}

fn unrecognized(atom: &str, arg: &str) -> Error {
    fatal(format!("unrecognized %({atom}) argument: {arg}"))
}

fn no_args(atom: &str) -> Error {
    fatal(format!("%({atom}) does not take arguments"))
}

fn strtol(text: &str) -> Option<i64> {
    let (sign, digits) = match text.as_bytes().first()? {
        b'-' => (-1, &text[1..]),
        b'+' => (1, &text[1..]),
        _ => (1, text),
    };
    digits.parse::<i64>().ok().map(|n| sign * n)
}

fn strip_of(arg: &str, atom: &str) -> Result<Strip, Error> {
    if arg.is_empty() {
        return Ok(Strip::Whole);
    }
    if arg == "short" {
        return Ok(Strip::Short);
    }
    for (prefix, left) in [("lstrip=", true), ("strip=", true), ("rstrip=", false)] {
        if let Some(n) = arg.strip_prefix(prefix) {
            let n = strtol(n).ok_or_else(|| fatal(format!("Integer value expected refname:{prefix}{n}")))?;
            return Ok(if left { Strip::Lstrip(n) } else { Strip::Rstrip(n) });
        }
    }
    Err(unrecognized(atom, arg))
}

fn oid_of(arg: &str, atom: &str) -> Result<Abbrev, Error> {
    if arg.is_empty() {
        return Ok(Abbrev::Full);
    }
    if arg == "short" {
        return Ok(Abbrev::Short(None));
    }
    if let Some(n) = arg.strip_prefix("short=") {
        let len = n.parse::<usize>().ok().filter(|&n| n > 0).ok_or_else(|| {
            fatal(format!("positive value expected '{n}' in %({atom}:{arg})"))
        })?;
        return Ok(Abbrev::Short(Some(len.max(4))));
    }
    Err(unrecognized(atom, arg))
}

fn remote_of(arg: &str, atom: &str) -> Result<Remote, Error> {
    let mut remote = Remote { what: RemoteWhat::Ref(Strip::Whole), nobracket: false };
    if arg.is_empty() {
        return Ok(remote);
    }
    for piece in arg.split(',') {
        match piece {
            "track" => remote.what = RemoteWhat::Track,
            "trackshort" => remote.what = RemoteWhat::TrackShort,
            "nobracket" => remote.nobracket = true,
            "remotename" => remote.what = RemoteWhat::RemoteName,
            "remoteref" => remote.what = RemoteWhat::RemoteRef,
            other => remote.what = RemoteWhat::Ref(strip_of(other, atom)?),
        }
    }
    Ok(remote)
}

fn email_of(arg: &str, atom: &str) -> Result<Part, Error> {
    let (mut trim, mut localpart, mut mailmap) = (false, false, false);
    if !arg.is_empty() {
        for piece in arg.split(',') {
            match piece {
                "trim" => trim = true,
                "localpart" => localpart = true,
                "mailmap" => mailmap = true,
                other => return Err(unrecognized(atom, other)),
            }
        }
    }
    Ok(Part::Email { trim, localpart, mailmap })
}

/// Splits a `%(trailers:…)` option text, naming the first piece git would not know.
fn check_trailers(options: &str) -> Result<(), Error> {
    for piece in options.split(',').filter(|p| !p.is_empty()) {
        let name = piece.split_once('=').map_or(piece, |(n, _)| n);
        let known = matches!(
            name,
            "only" | "unfold" | "keyonly" | "valueonly" | "key" | "separator" | "key_value_separator"
        );
        if !known {
            return Err(fatal(format!("unknown %(trailers) argument: {piece}")));
        }
    }
    Ok(())
}

fn contents_of(arg: &str) -> Result<Contents, Error> {
    Ok(match arg {
        "" => Contents::Bare,
        "size" => Contents::Size,
        "subject" => Contents::Subject,
        "body" => Contents::Body,
        "signature" => Contents::Signature,
        _ => {
            if let Some(n) = arg.strip_prefix("lines=") {
                let lines = n.parse::<usize>().ok().filter(|&n| n > 0);
                return lines
                    .map(Contents::Lines)
                    .ok_or_else(|| fatal(format!("positive value expected contents:lines={n}")));
            }
            if let Some(options) = arg.strip_prefix("trailers") {
                let options = options.strip_prefix(':').unwrap_or("");
                check_trailers(options)?;
                return Ok(Contents::Trailers(options.to_owned()));
            }
            return Err(unrecognized("contents", arg));
        }
    })
}

fn describe_of(arg: &str) -> Result<(), Error> {
    for piece in arg.split(',').filter(|p| !p.is_empty()) {
        if piece == "tags" {
            continue;
        }
        let Some((name, value)) = piece.split_once('=') else {
            return Err(unrecognized("describe", piece));
        };
        match name {
            "tags" if matches!(value, "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0" | "") => {}
            "abbrev" => {
                if value.parse::<u32>().is_err() {
                    return Err(fatal(format!("cannot fully parse describe:abbrev={value}")));
                }
            }
            "match" | "exclude" => {}
            _ => return Err(unrecognized("describe", piece)),
        }
    }
    Ok(())
}

fn align_of(arg: &str) -> Result<Kind, Error> {
    let (mut width, mut position) = (None, Position::Left);
    let position_of = |text: &str| match text {
        "left" => Some(Position::Left),
        "right" => Some(Position::Right),
        "middle" => Some(Position::Middle),
        _ => None,
    };
    if arg.is_empty() {
        return Err(fatal("expected format: %(align:<width>,<position>)"));
    }
    for piece in arg.split(',') {
        if let Some(w) = piece.strip_prefix("width=") {
            width = Some(w.parse::<usize>().map_err(|_| fatal(format!("unrecognized width:{w}")))?);
        } else if let Some(p) = piece.strip_prefix("position=") {
            position = position_of(p).ok_or_else(|| fatal(format!("unrecognized position:{p}")))?;
        } else if let Ok(w) = piece.parse::<usize>() {
            width = Some(w);
        } else if let Some(p) = position_of(piece) {
            position = p;
        } else {
            return Err(unrecognized("align", piece));
        }
    }
    match width {
        Some(w) if w > 0 => Ok(Kind::Align { width: w, position }),
        _ => Ok(Kind::Align { width: 0, position }),
    }
}

fn person_of(name: &str, arg: &str) -> Result<Option<Kind>, Error> {
    let (who, rest) = if let Some(rest) = name.strip_prefix("author") {
        (Who::Author, rest)
    } else if let Some(rest) = name.strip_prefix("committer") {
        (Who::Committer, rest)
    } else if let Some(rest) = name.strip_prefix("tagger") {
        (Who::Tagger, rest)
    } else if let Some(rest) = name.strip_prefix("creator") {
        (Who::Creator, rest)
    } else {
        return Ok(None);
    };
    let part = match rest {
        "" => {
            return Ok(Some(Kind::Person { who, part: Part::Line, bogus: !arg.is_empty() }));
        }
        "name" if who != Who::Creator => match arg {
            "" => Part::Name { mailmap: false },
            "mailmap" => Part::Name { mailmap: true },
            other => return Err(unrecognized(name, other)),
        },
        "email" if who != Who::Creator => email_of(arg, name)?,
        "date" => {
            let mode = if arg.is_empty() {
                date::Mode::default()
            } else {
                date::Mode::parse(arg).ok_or_else(|| fatal(format!("unknown date format {arg}")))?
            };
            Part::Date { mode, formatted: !arg.is_empty() }
        }
        _ => return Ok(None),
    };
    Ok(Some(Kind::Person { who, part, bogus: false }))
}

/// Parses the text between `%(` and `)`. `allow_rest` admits `%(rest)` (`cat-file` only).
pub fn parse_atom(text: &str) -> Result<Atom, Error> {
    let (deref, body) = match text.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (name, arg) = body.split_once(':').unwrap_or((body, ""));
    let has_arg = body.contains(':');
    let kind = if let Some(kind) = person_of(name, arg)? {
        kind
    } else {
        match name {
            "refname" => Kind::Refname(strip_of(arg, name)?),
            "objecttype" => {
                if has_arg {
                    return Err(no_args(name));
                }
                Kind::ObjectType
            }
            "objectsize" => match arg {
                "" => Kind::ObjectSize { disk: false },
                "disk" => Kind::ObjectSize { disk: true },
                other => return Err(unrecognized(name, other)),
            },
            "objectname" => Kind::ObjectName(oid_of(arg, name)?),
            "deltabase" => {
                if has_arg {
                    return Err(no_args(name));
                }
                Kind::DeltaBase
            }
            "tree" => Kind::Tree(oid_of(arg, name)?),
            "parent" => Kind::Parent(oid_of(arg, name)?),
            "numparent" => Kind::NumParent,
            "object" => Kind::Object,
            "type" => Kind::Type,
            "tag" => Kind::Tag,
            "subject" => match arg {
                "" => Kind::Subject { sanitize: false },
                "sanitize" => Kind::Subject { sanitize: true },
                other => return Err(unrecognized(name, other)),
            },
            "body" => {
                if has_arg {
                    return Err(no_args(name));
                }
                Kind::Body
            }
            "trailers" => {
                check_trailers(arg)?;
                Kind::Trailers(arg.to_owned())
            }
            "contents" => Kind::Contents(contents_of(arg)?),
            "signature" => Kind::Signature(match arg {
                "" => Sig::Bare,
                "grade" => Sig::Grade,
                "signer" => Sig::Signer,
                "key" => Sig::Key,
                "fingerprint" => Sig::Fingerprint,
                "primarykeyfingerprint" => Sig::PrimaryKeyFingerprint,
                "trustlevel" => Sig::TrustLevel,
                other => return Err(unrecognized(name, other)),
            }),
            "raw" => match arg {
                "" => Kind::Raw { size: false },
                "size" => Kind::Raw { size: true },
                other => return Err(unrecognized(name, other)),
            },
            "upstream" => Kind::Upstream(remote_of(arg, name)?),
            "push" => Kind::Push(remote_of(arg, name)?),
            "symref" => Kind::Symref(strip_of(arg, name)?),
            "flag" => Kind::Flag,
            "HEAD" => {
                if has_arg {
                    return Err(no_args(name));
                }
                Kind::Head
            }
            "color" => {
                if arg.is_empty() {
                    return Err(fatal("expected format: %(color:<color>)"));
                }
                if color::Spec::parse(arg).is_none() {
                    eprintln!("error: invalid color value: {arg}");
                    return Err(fatal(format!("unrecognized color: %(color:{arg})")));
                }
                Kind::Color(arg.to_owned())
            }
            "worktreepath" => {
                if has_arg {
                    return Err(no_args(name));
                }
                Kind::Worktreepath
            }
            "describe" => {
                describe_of(arg)?;
                Kind::Describe(arg.to_owned())
            }
            "ahead-behind" | "is-base" => {
                if arg.is_empty() {
                    return Err(fatal(format!("expected format: %({name}:<committish>)")));
                }
                if name == "is-base" { Kind::IsBase(arg.to_owned()) } else { Kind::AheadBehind(arg.to_owned()) }
            }
            "align" => align_of(arg)?,
            "if" => Kind::If(match arg {
                "" => IfCmp::Nonempty,
                _ => match (arg.strip_prefix("equals="), arg.strip_prefix("notequals=")) {
                    (Some(s), _) => IfCmp::Equals(s.as_bytes().to_vec()),
                    (_, Some(s)) => IfCmp::NotEquals(s.as_bytes().to_vec()),
                    _ => return Err(unrecognized("if", arg)),
                },
            }),
            "then" => Kind::Then,
            "else" => Kind::Else,
            "end" => Kind::End,
            "rest" => return Err(fatal("this command reject atom %(rest)")),
            _ => return Err(fatal(format!("unknown field name: {body}"))),
        }
    };
    if matches!(kind, Kind::Then | Kind::Else | Kind::End) && has_arg {
        // git ignores a stray argument here; the structure error still applies.
    }
    Ok(Atom { deref, kind, text: text.to_owned() })
}

fn hex_pair(a: u8, b: u8) -> Option<u8> {
    let h = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    Some(h(a)? * 16 + h(b)?)
}

/// Appends the literal text with git's `%%` and `%xx` handling.
fn push_literal(text: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'%' {
            if text.get(i + 1) == Some(&b'%') {
                out.push(b'%');
                i += 2;
                continue;
            }
            if let (Some(&a), Some(&b)) = (text.get(i + 1), text.get(i + 2))
                && let Some(byte) = hex_pair(a, b) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
        }
        out.push(text[i]);
        i += 1;
    }
}

/// The start of the next `%(` atom, skipping `%%`.
fn find_next(text: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < text.len() {
        if text[i] == b'%' {
            match text.get(i + 1) {
                Some(b'(') => return Some(i),
                Some(b'%') => i += 1,
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Parses a format string into literals and atoms; unknown atoms fail here, before any ref prints.
pub fn parse_format(format: &[u8]) -> Result<Vec<Token>, Error> {
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(start) = find_next(format, at) {
        let Some(len) = format[start..].iter().position(|&b| b == b')') else {
            return Err(Error::Malformed(String::from_utf8_lossy(&format[start..]).into_owned()));
        };
        if start > at {
            let mut lit = Vec::new();
            push_literal(&format[at..start], &mut lit);
            tokens.push(Token::Lit(lit));
        }
        let inner = String::from_utf8_lossy(&format[start + 2..start + len]).into_owned();
        tokens.push(Token::Atom(parse_atom(&inner)?));
        at = start + len + 1;
    }
    if at < format.len() {
        let mut lit = Vec::new();
        push_literal(&format[at..], &mut lit);
        tokens.push(Token::Lit(lit));
    }
    Ok(tokens)
}

/// One `--sort=<key>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortKey {
    pub atom: Atom,
    pub reverse: bool,
    pub version: bool,
}

/// Parses `[-][version:|v:]<atom>`.
pub fn parse_sort_key(text: &str) -> Result<SortKey, Error> {
    let (reverse, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (version, rest) = match rest.strip_prefix("version:").or_else(|| rest.strip_prefix("v:")) {
        Some(r) => (true, r),
        None => (false, rest),
    };
    if rest.is_empty() {
        return Err(fatal(format!("malformed field name: {rest}")));
    }
    Ok(SortKey { atom: parse_atom(rest)?, reverse, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lits(format: &str) -> Vec<Token> {
        parse_format(format.as_bytes()).unwrap()
    }

    #[test]
    fn literals_unescape_percent_and_hex() {
        assert_eq!(lits("a%%b%41%zz%"), [Token::Lit(b"a%bA%zz%".to_vec())]);
        assert_eq!(lits("%%(refname)"), [Token::Lit(b"%(refname)".to_vec())]);
        assert_eq!(lits("%00").len(), 1);
    }

    #[test]
    fn atoms_parse_with_their_arguments() {
        let tokens = lits("x%(refname:lstrip=2)%(*objectname:short=9)");
        assert_eq!(tokens.len(), 3);
        let Token::Atom(a) = &tokens[1] else { panic!() };
        assert_eq!(a.kind, Kind::Refname(Strip::Lstrip(2)));
        let Token::Atom(b) = &tokens[2] else { panic!() };
        assert!(b.deref);
        assert_eq!(b.kind, Kind::ObjectName(Abbrev::Short(Some(9))));
    }

    #[test]
    fn errors_use_gits_words() {
        let msg = |f: &str| match parse_format(f.as_bytes()).unwrap_err() {
            Error::Fatal(m) | Error::Malformed(m) => m,
        };
        assert_eq!(msg("%(foo)"), "unknown field name: foo");
        assert_eq!(msg("%(refname:bogus)"), "unrecognized %(refname) argument: bogus");
        assert_eq!(msg("%(refname"), "%(refname");
        assert_eq!(msg("%(HEAD:x)"), "%(HEAD) does not take arguments");
        assert_eq!(msg("%(objectname:short=x)"), "positive value expected 'x' in %(objectname:short=x)");
    }

    #[test]
    fn sort_keys() {
        let k = parse_sort_key("-v:refname").unwrap();
        assert!(k.reverse && k.version);
        assert_eq!(k.atom.kind, Kind::Refname(Strip::Whole));
    }
}
