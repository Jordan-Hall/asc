//! The value of one atom for one ref (git's `populate_value` and its helpers).

use asc_core::id::GitOid;
use asc_core::sig::Signature;

use super::item::{Item, Obj, ObjKind};
use super::object::{self, Layout};
use super::parse::{Abbrev, Atom, Contents, Kind, Part, Sig, Strip, Who};
use crate::cli::format::mailmap::Mailmap;
use crate::cli::format::{self, color};

/// What evaluation needs from the invocation.
pub struct Env {
    /// Hex digits of `objectname:short`.
    pub abbrev: usize,
    /// The time relative dates are measured from.
    pub now: i64,
    /// Whether `%(color:…)` prints escapes.
    pub color: bool,
    pub mailmap: Option<Mailmap>,
}

/// An atom's text and, for the numeric atoms, its number (what a sort compares).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Value {
    pub text: Vec<u8>,
    pub num: i64,
}

impl Value {
    fn text(text: impl Into<Vec<u8>>) -> Self {
        Self {
            text: text.into(),
            num: 0,
        }
    }

    fn number(n: i64) -> Self {
        Self {
            text: n.to_string().into_bytes(),
            num: n,
        }
    }
}

/// How much of an object an atom reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    None,
    /// Type and id only.
    Id,
    /// The raw bytes.
    Raw,
}

impl Atom {
    /// Whether sorting compares the number rather than the text.
    pub fn numeric(&self) -> bool {
        matches!(
            self.kind,
            Kind::ObjectSize { .. }
                | Kind::NumParent
                | Kind::Person {
                    part: Part::Date {
                        formatted: false,
                        ..
                    },
                    bogus: false,
                    ..
                }
        )
    }

    /// Whether the value needs the repository beyond the object (loaded into `Item::extra`).
    pub fn is_extra(&self) -> bool {
        matches!(
            self.kind,
            Kind::Refname(Strip::Short)
                | Kind::Symref(Strip::Short)
                | Kind::Upstream(_)
                | Kind::Push(_)
                | Kind::Worktreepath
                | Kind::Describe(_)
                | Kind::AheadBehind(_)
                | Kind::IsBase(_)
        )
    }

    /// How much of the object (or of the object it peels to, for `*atoms`) this atom reads.
    pub fn level(&self) -> Level {
        match self.kind {
            Kind::ObjectType | Kind::ObjectName(_) | Kind::DeltaBase => Level::Id,
            Kind::ObjectSize { .. }
            | Kind::Tree(_)
            | Kind::Parent(_)
            | Kind::NumParent
            | Kind::Object
            | Kind::Type
            | Kind::Tag
            | Kind::Person { .. }
            | Kind::Subject { .. }
            | Kind::Body
            | Kind::Trailers(_)
            | Kind::Contents(_)
            | Kind::Signature(_)
            | Kind::Raw { .. } => Level::Raw,
            _ => Level::None,
        }
    }
}

fn components(name: &str) -> i64 {
    name.matches('/').count() as i64 + 1
}

/// `lstrip_ref_components`.
pub fn lstrip(name: &str, n: i64) -> String {
    let mut remaining = if n < 0 { components(name) + n } else { n };
    let mut rest = name;
    while remaining > 0 {
        match rest.find('/') {
            Some(p) => rest = &rest[p + 1..],
            None => return String::new(),
        }
        remaining -= 1;
    }
    rest.to_owned()
}

/// `rstrip_ref_components`.
pub fn rstrip(name: &str, n: i64) -> String {
    let mut remaining = if n < 0 { components(name) + n } else { n };
    let mut rest = name;
    while remaining > 0 {
        match rest.rfind('/') {
            Some(p) => rest = &rest[..p],
            None => return String::new(),
        }
        remaining -= 1;
    }
    rest.to_owned()
}

/// A ref name through a strip option; `short` is the loaded unambiguous form.
fn stripped(name: &str, strip: Strip, short: Option<&[u8]>) -> Vec<u8> {
    match strip {
        Strip::Whole => name.as_bytes().to_vec(),
        Strip::Short => short.map_or_else(|| name.as_bytes().to_vec(), <[u8]>::to_vec),
        Strip::Lstrip(n) => lstrip(name, n).into_bytes(),
        Strip::Rstrip(n) => rstrip(name, n).into_bytes(),
    }
}

fn abbreviate(hex: &str, how: Abbrev, env: &Env) -> String {
    let len = match how {
        Abbrev::Full => 40,
        Abbrev::Short(None) => env.abbrev,
        Abbrev::Short(Some(n)) => n,
    };
    hex[..len.clamp(4, 40).min(hex.len())].to_owned()
}

/// `%(trailers:…)` of `message`, through the pretty-format implementation.
fn trailers(options: &str, message: &[u8]) -> Vec<u8> {
    let Ok(who) = Signature::new(b"-".to_vec(), b"-".to_vec(), 0, 0) else {
        return Vec::new();
    };
    let spec = if options.is_empty() {
        "%(trailers)".to_owned()
    } else {
        format!("%(trailers:{options})")
    };
    let zero = GitOid::from_bytes([0; 20]);
    let commit = format::Commit {
        id: zero,
        tree: zero,
        parents: &[],
        author: &who,
        committer: &who,
        message,
        encoding: None,
    };
    let mut out = Vec::new();
    format::Template::parse(spec.as_bytes()).render(
        &commit,
        &format::Env::default(),
        &format::Extras::default(),
        &mut out,
    );
    out
}

/// `format_sanitized_subject`: runs of characters other than letters, digits, `.` and `_` become
/// one `-`; no leading or trailing `-` or `.`; repeated dots collapse.
fn sanitized(subject: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut space = 2u8;
    let mut i = 0;
    while i < subject.len() {
        let c = subject[i];
        if c.is_ascii_alphanumeric() || c == b'.' || c == b'_' {
            if space == 1 {
                out.push(b'-');
            }
            space = 0;
            out.push(c);
            if c == b'.' {
                while i + 1 < subject.len() && subject[i + 1] == b'.' {
                    i += 1;
                }
            }
        } else {
            space |= 1;
        }
        i += 1;
    }
    while out.last().is_some_and(|&c| c == b'.' || c == b'-') {
        out.pop();
    }
    out
}

/// The `gpgsig` header of a commit with its folding undone.
fn commit_signature(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in raw.split_inclusive(|&b| b == b'\n') {
        if line == b"\n" {
            break;
        }
        if let Some(first) = line.strip_prefix(b"gpgsig ") {
            inside = true;
            out.extend_from_slice(first);
        } else if inside && line.first() == Some(&b' ') {
            out.extend_from_slice(&line[1..]);
        } else {
            inside = false;
        }
    }
    out
}

fn contents(part: &Contents, obj: &Obj) -> Vec<u8> {
    let raw = &obj.raw;
    let l: Layout = object::layout(raw);
    match part {
        Contents::Bare => raw[l.sub..].to_vec(),
        Contents::Size => (raw.len() - l.sub).to_string().into_bytes(),
        Contents::Subject => object::subject(raw, &l),
        Contents::Body => raw[l.body.min(l.sig)..l.sig].to_vec(),
        Contents::Signature => raw[l.sig..].to_vec(),
        Contents::Lines(n) => object::first_lines(&raw[l.sub..l.sig.max(l.sub)], *n),
        Contents::Trailers(options) => trailers(options, &raw[l.sub..l.sig.max(l.sub)]),
    }
}

fn person(who: Who, kind: ObjKind) -> &'static str {
    match who {
        Who::Author => "author",
        Who::Committer => "committer",
        Who::Tagger => "tagger",
        Who::Creator if kind == ObjKind::Commit => "committer",
        Who::Creator => "tagger",
    }
}

/// The value of an object-sourced atom; `None` obj gives the empty value git prints for a ref
/// without that object (a `*atom` of a lightweight ref).
fn of_object(atom: &Atom, obj: Option<&Obj>, env: &Env) -> Value {
    let Some(obj) = obj else {
        return Value::default();
    };
    let raw = &obj.raw;
    let is_commit = obj.kind == ObjKind::Commit;
    match &atom.kind {
        Kind::ObjectType => Value::text(obj.kind.name()),
        Kind::ObjectSize { .. } => Value::number(raw.len() as i64),
        Kind::ObjectName(how) => Value::text(abbreviate(&obj.oid.to_string(), *how, env)),
        Kind::DeltaBase => Value::text("0".repeat(40)),
        Kind::Tree(how) if is_commit => Value::text(abbreviate(
            &String::from_utf8_lossy(object::header(raw, "tree")),
            *how,
            env,
        )),
        Kind::Parent(how) if is_commit => {
            let parents = object::header_values(raw, "parent");
            let shown: Vec<String> = parents
                .iter()
                .map(|p| abbreviate(&String::from_utf8_lossy(p), *how, env))
                .collect();
            Value::text(shown.join(" "))
        }
        Kind::NumParent if is_commit => {
            Value::number(object::header_values(raw, "parent").len() as i64)
        }
        Kind::Object if !is_commit => Value::text(object::header(raw, "object")),
        Kind::Type if !is_commit => Value::text(object::header(raw, "type")),
        Kind::Tag if !is_commit => Value::text(object::header(raw, "tag")),
        Kind::Person { who, part, bogus } => {
            if *bogus {
                return Value::default();
            }
            let line = object::person_tail(raw, person(*who, obj.kind));
            match part {
                Part::Line => Value::text(object::line_of(line)),
                Part::Name { mailmap } => Value::text(object::mapped_name(
                    line,
                    env.mailmap.as_ref().filter(|_| *mailmap),
                )),
                Part::Email {
                    trim,
                    localpart,
                    mailmap,
                } => Value::text(object::email_of(
                    line,
                    *trim,
                    *localpart,
                    env.mailmap.as_ref().filter(|_| *mailmap),
                )),
                Part::Date { mode, .. } => {
                    let (text, num) = object::date_of(line, mode, env.now);
                    Value { text, num }
                }
            }
        }
        Kind::Subject { sanitize } => {
            let l = object::layout(raw);
            if *sanitize {
                Value::text(sanitized(&raw[l.sub..l.sub_end]))
            } else {
                Value::text(object::subject(raw, &l))
            }
        }
        Kind::Body => {
            let l = object::layout(raw);
            Value::text(&raw[l.body.min(raw.len())..])
        }
        Kind::Trailers(options) => {
            let l = object::layout(raw);
            Value::text(trailers(options, &raw[l.sub..l.sig.max(l.sub)]))
        }
        Kind::Contents(part) => Value::text(contents(part, obj)),
        Kind::Signature(part) => {
            if !is_commit {
                return Value::default();
            }
            let signed = commit_signature(raw);
            Value::text(match part {
                Sig::Bare => signed,
                Sig::Grade if signed.is_empty() => b"N".to_vec(),
                Sig::Grade => b"E".to_vec(),
                Sig::TrustLevel => b"undefined".to_vec(),
                _ => Vec::new(),
            })
        }
        Kind::Raw { size: false } => Value::text(raw.clone()),
        Kind::Raw { size: true } => Value::text(raw.len().to_string()),
        _ => Value::default(),
    }
}

/// The value of `atom` for `item`.
pub fn value(atom: &Atom, item: &Item, env: &Env) -> Value {
    let loaded = |item: &Item| item.extra.get(&atom.text).cloned().unwrap_or_default();
    match &atom.kind {
        Kind::Refname(strip) => {
            let short = item.extra.get(&atom.text).map(Vec::as_slice);
            let mut text = stripped(&item.name, *strip, short);
            if atom.deref {
                text.extend_from_slice(b"^{}");
            }
            Value::text(text)
        }
        Kind::Symref(strip) => {
            let short = item.extra.get(&atom.text).map(Vec::as_slice);
            let mut text = match &item.symref {
                Some(target) => stripped(target, *strip, short),
                None => Vec::new(),
            };
            if atom.deref {
                text.extend_from_slice(b"^{}");
            }
            Value::text(text)
        }
        Kind::Head => Value::text(if item.head { "*" } else { " " }),
        Kind::Flag => Value::default(),
        Kind::Color(spec) => Value::text(if env.color {
            color::sgr(spec)
        } else {
            String::new()
        }),
        Kind::Upstream(_)
        | Kind::Push(_)
        | Kind::Worktreepath
        | Kind::Describe(_)
        | Kind::AheadBehind(_)
        | Kind::IsBase(_) => {
            if atom.deref {
                Value::default()
            } else {
                Value::text(loaded(item))
            }
        }
        Kind::Align { .. } | Kind::If(_) | Kind::Then | Kind::Else | Kind::End => Value::default(),
        _ => {
            let obj = if atom.deref {
                item.deref.as_ref()
            } else {
                item.obj.as_ref()
            };
            of_object(atom, obj, env)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_follow_gits_rules() {
        assert_eq!(lstrip("refs/tags/foo", 2), "foo");
        assert_eq!(lstrip("refs/tags/foo", 3), "");
        assert_eq!(lstrip("refs/tags/foo", 0), "refs/tags/foo");
        assert_eq!(lstrip("refs/tags/foo", -2), "tags/foo");
        assert_eq!(lstrip("refs/tags/foo", -9), "refs/tags/foo");
        assert_eq!(rstrip("refs/tags/foo", 2), "refs");
        assert_eq!(rstrip("refs/tags/foo", 3), "");
        assert_eq!(rstrip("refs/tags/foo", -1), "refs");
        assert_eq!(rstrip("refs/tags/foo", -9), "refs/tags/foo");
    }
}
