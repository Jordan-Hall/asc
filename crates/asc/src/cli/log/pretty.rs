//! The built-in `--pretty` formats (`pretty.c`): what `pretty_print_commit` writes for
//! `oneline`, `short`, `medium`, `full`, `fuller`, `raw`, `email` and `mboxrd`.

use asc_core::sig::Signature;

use crate::cli::format::date::Mode;

/// A built-in format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Oneline,
    Short,
    Medium,
    Full,
    Fuller,
    Raw,
    Email,
    Mboxrd,
}

impl Builtin {
    /// The format called `name`, or `None` (also for `reference`, which is a user format).
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "oneline" => Self::Oneline,
            "short" => Self::Short,
            "medium" => Self::Medium,
            "full" => Self::Full,
            "fuller" => Self::Fuller,
            "raw" => Self::Raw,
            "email" => Self::Email,
            "mboxrd" => Self::Mboxrd,
            _ => return None,
        })
    }

    pub fn is_mail(self) -> bool {
        matches!(self, Self::Email | Self::Mboxrd)
    }

    /// Spaces the message is indented by.
    fn indent(self) -> usize {
        match self {
            Self::Oneline | Self::Email | Self::Mboxrd => 0,
            _ => 4,
        }
    }
}

/// The commit as the built-in formats need it.
pub struct View<'a> {
    pub tree: String,
    pub parents: Vec<String>,
    /// Abbreviated parents for `Merge:`.
    pub parents_short: Vec<String>,
    pub author: &'a Signature,
    pub committer: &'a Signature,
    pub message: &'a [u8],
    pub date: &'a Mode,
    pub now: i64,
}

fn ident(s: &Signature) -> Vec<u8> {
    [s.name(), b" <", s.email(), b">"].concat()
}

/// A line without trailing whitespace, and whether nothing is left.
fn rtrim(line: &[u8]) -> (&[u8], bool) {
    let t = line.trim_ascii_end();
    (t, t.is_empty())
}

fn lines(msg: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = msg;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest.iter().position(|&b| b == b'\n').map_or(rest.len(), |p| p + 1);
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}

/// git's `strbuf_add_tabexpand` with 8-column tab stops.
fn expand_tabs(line: &[u8], out: &mut Vec<u8>) {
    let mut col = 0;
    for &b in line {
        if b == b'\t' {
            let n = 8 - col % 8;
            out.extend(std::iter::repeat_n(b' ', n));
            col += n;
        } else {
            out.push(b);
            if b & 0xc0 != 0x80 {
                col += 1;
            }
        }
    }
}

/// The subject paragraph joined with `sep`, and the offset after it and the blank lines that
/// follow.
pub fn subject(msg: &[u8], sep: &[u8]) -> (Vec<u8>, usize) {
    let mut pos = 0;
    let mut out = Vec::new();
    let mut first = true;
    let mut started = false;
    for line in lines(msg) {
        let (t, blank) = rtrim(line);
        if blank {
            pos += line.len();
            if started {
                break;
            }
            continue;
        }
        started = true;
        if !first {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(t);
        first = false;
        pos += line.len();
    }
    (out, pos)
}

/// `pp_remainder`: the message lines, indented, from `from` on.
fn remainder(msg: &[u8], from: usize, builtin: Builtin, out: &mut Vec<u8>) {
    let indent = builtin.indent();
    let expand = indent > 0 && builtin != Builtin::Raw;
    let mut first = true;
    for line in lines(&msg[from..]) {
        let (t, blank) = rtrim(line);
        if blank {
            if first {
                continue;
            }
            if builtin == Builtin::Short {
                break;
            }
        }
        first = false;
        out.extend(std::iter::repeat_n(b' ', indent));
        let mut text = Vec::new();
        if expand {
            expand_tabs(t, &mut text);
        } else {
            text.extend_from_slice(t);
        }
        if builtin == Builtin::Mboxrd {
            let from_quoted = text.iter().skip_while(|&&b| b == b'>').copied().take(5).collect::<Vec<_>>();
            if from_quoted == b"From " {
                out.push(b'>');
            }
        }
        out.extend_from_slice(&text);
        out.push(b'\n');
    }
}

fn user(label: &str, who: &Signature, fuller: bool, out: &mut Vec<u8>) {
    out.extend_from_slice(label.as_bytes());
    out.extend_from_slice(if fuller { b":     " } else { b": " });
    out.extend_from_slice(&ident(who));
    out.push(b'\n');
}

/// What `pretty_print_commit` produces for `builtin`: the header lines, the blank line and the
/// message, ending in a newline (a bare subject for `oneline`).
pub fn print(builtin: Builtin, v: &View<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    if builtin == Builtin::Oneline {
        out.extend_from_slice(&subject(v.message, b" ").0);
        return out;
    }
    let date = |s: &Signature| v.date.render(s.time(), s.tz_min(), v.now);
    match builtin {
        Builtin::Raw => {
            out.extend_from_slice(format!("tree {}\n", v.tree).as_bytes());
            for p in &v.parents {
                out.extend_from_slice(format!("parent {p}\n").as_bytes());
            }
            for (k, s) in [("author", v.author), ("committer", v.committer)] {
                out.extend_from_slice(k.as_bytes());
                out.push(b' ');
                out.extend_from_slice(&ident(s));
                out.extend_from_slice(format!(" {} {}\n", s.time(), tz_text(s.tz_min())).as_bytes());
            }
        }
        Builtin::Email | Builtin::Mboxrd => {
            out.extend_from_slice(b"From: ");
            out.extend_from_slice(&ident(v.author));
            out.push(b'\n');
            let rfc = Mode::parse("rfc").unwrap_or_default();
            out.extend_from_slice(
                format!("Date: {}\n", rfc.render(v.author.time(), v.author.tz_min(), v.now)).as_bytes(),
            );
            out.extend_from_slice(b"Subject: [PATCH] ");
            out.extend_from_slice(&subject(v.message, b" ").0);
            out.push(b'\n');
        }
        other => {
            if v.parents.len() > 1 {
                out.extend_from_slice(format!("Merge: {}\n", v.parents_short.join(" ")).as_bytes());
            }
            let fuller = other == Builtin::Fuller;
            user("Author", v.author, fuller, &mut out);
            match other {
                Builtin::Medium => out.extend_from_slice(format!("Date:   {}\n", date(v.author)).as_bytes()),
                Builtin::Fuller => out.extend_from_slice(format!("AuthorDate: {}\n", date(v.author)).as_bytes()),
                _ => {}
            }
            if matches!(other, Builtin::Full | Builtin::Fuller) {
                user("Commit", v.committer, fuller, &mut out);
            }
            if fuller {
                out.extend_from_slice(format!("CommitDate: {}\n", date(v.committer)).as_bytes());
            }
        }
    }
    out.push(b'\n');
    let (_, body) = if builtin.is_mail() { subject(v.message, b" ") } else { (Vec::new(), 0) };
    remainder(v.message, body, builtin, &mut out);
    while out.last().is_some_and(u8::is_ascii_whitespace) {
        out.pop();
    }
    out.push(b'\n');
    if builtin.is_mail() && body_empty(v.message) {
        out.push(b'\n');
    }
    out
}

/// Whether nothing follows the subject paragraph.
fn body_empty(msg: &[u8]) -> bool {
    let (_, at) = subject(msg, b" ");
    msg[at.min(msg.len())..].iter().all(u8::is_ascii_whitespace)
}

fn tz_text(min: i16) -> String {
    let a = min.abs();
    format!("{}{:02}{:02}", if min < 0 { '-' } else { '+' }, a / 60, a % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> Signature {
        Signature::new(b"A".to_vec(), b"a@b".to_vec(), 1_700_000_000, 0).unwrap()
    }

    fn view<'a>(s: &'a Signature, m: &'a [u8], d: &'a Mode) -> View<'a> {
        View {
            tree: "t".into(),
            parents: vec![],
            parents_short: vec![],
            author: s,
            committer: s,
            message: m,
            date: d,
            now: 0,
        }
    }

    #[test]
    fn medium_indents_and_expands_tabs() {
        let (s, d) = (sig(), Mode::default());
        let out = print(Builtin::Medium, &view(&s, b"Subject\n\nBody\twith tab  \n\n\n", &d));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Author: A <a@b>\nDate:   Tue Nov 14 22:13:20 2023 +0000\n\n    Subject\n    \n    Body    with tab\n"
        );
    }

    #[test]
    fn short_stops_after_the_title() {
        let (s, d) = (sig(), Mode::default());
        let out = print(Builtin::Short, &view(&s, b"One\ntwo\n\nbody\n", &d));
        assert_eq!(String::from_utf8(out).unwrap(), "Author: A <a@b>\n\n    One\n    two\n");
    }

    #[test]
    fn oneline_joins_the_subject() {
        let (s, d) = (sig(), Mode::default());
        assert_eq!(print(Builtin::Oneline, &view(&s, b"One\n two\n\nbody\n", &d)), b"One  two");
    }

    #[test]
    fn email_has_no_indent() {
        let (s, d) = (sig(), Mode::default());
        let out = String::from_utf8(print(Builtin::Email, &view(&s, b"Sub\n\nFrom me\n", &d))).unwrap();
        assert!(out.ends_with("Subject: [PATCH] Sub\n\nFrom me\n"), "{out}");
        let out = String::from_utf8(print(Builtin::Mboxrd, &view(&s, b"Sub\n\nFrom me\n>From you\n", &d))).unwrap();
        assert!(out.ends_with("\n>From me\n>>From you\n"), "{out}");
    }
}
