//! `blame` output: git's default layout, `--porcelain`, `--line-porcelain`, `--incremental`
//! and the asc-only `--json` (`BlameResponse` lines).
//!
//! Anything that comes from the repository (paths, names, summaries) is written either as one
//! line it cannot leave (names and summaries cannot hold a line feed) or C-quoted (paths), so a
//! hostile history cannot forge porcelain records.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_core::sig::Signature;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};

use super::compute::{Blamed, Origin, Piece, entry_at, fatal, previous_of};
use crate::cli::check_ignore::c_quote;
use crate::cli::common::{CliError, Result, WsCtx};
use crate::cli::format::date::Mode;
use crate::cli::format::layout::width;
use crate::cli::json::JsonLines;

/// Hunks per `BlameResponse` line.
const PAGE: usize = 500;
const NOT_COMMITTED: &[u8] = b"Not Committed Yet";
const NOT_COMMITTED_MAIL: &[u8] = b"not.committed.yet";

/// What one origin prints.
pub struct Meta {
    pub hex: String,
    pub author: Signature,
    pub committer: Signature,
    pub summary: Vec<u8>,
    pub boundary: bool,
    /// Rank for `--incremental`: newer first.
    time: i64,
    first_line: u32,
}

/// The facts for every origin of the lines shown.
pub struct Facts {
    metas: HashMap<Origin, Meta>,
    /// `previous <commit> <path>` per (origin, its path).
    previous: HashMap<(Origin, RepoPath), (String, RepoPath)>,
    /// Origins that appear under more than one path.
    multi_path: HashSet<Origin>,
}

/// The first line of the message, as `summary` prints it.
fn summary_of(message: &[u8]) -> Vec<u8> {
    let start = message.iter().position(|b| *b != b'\n').unwrap_or(message.len());
    let line = message[start..].split(|b| *b == b'\n').next().unwrap_or(&[]);
    let end = line.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(0, |i| i + 1);
    line[..end].to_vec()
}

/// git's `Version of <path> from <path>` summary for uncommitted lines. A path with control bytes
/// is C-quoted, as the `filename` line is, so it cannot break out of the header line.
fn dirty_summary(path: &[u8]) -> Vec<u8> {
    let shown = c_quote(path, false).unwrap_or_else(|| path.to_vec());
    let mut out = b"Version of ".to_vec();
    out.extend_from_slice(&shown);
    out.extend_from_slice(b" from ");
    out.extend_from_slice(&shown);
    out
}

/// Appends one single-line header (`tag value`, then LF). A line feed or carriage return in
/// `value` would start a forged record, so each becomes a space.
fn header(out: &mut Vec<u8>, tag: &str, value: &[u8]) {
    out.extend_from_slice(tag.as_bytes());
    out.push(b' ');
    out.extend(value.iter().map(|&b| if b == b'\n' || b == b'\r' { b' ' } else { b }));
    out.push(b'\n');
}

/// The local zone in minutes east of UTC, from `TZ` (`UTC`/`GMT` with an optional offset); UTC
/// otherwise.
fn local_tz_min() -> i16 {
    let Ok(tz) = std::env::var("TZ") else { return 0 };
    let rest = tz.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let (sign, digits) = match rest.as_bytes().first() {
        Some(b'-') => (1, &rest[1..]),
        Some(b'+') => (-1, &rest[1..]),
        _ => (-1, rest),
    };
    let mut parts = digits.split(':');
    let h: i32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let m: i32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    i16::try_from(sign * (h * 60 + m)).unwrap_or(0)
}

/// `+0100` for 60 minutes east.
pub fn tz_text(tz_min: i16) -> String {
    let t = i32::from(tz_min);
    format!("{}{:02}{:02}", if t < 0 { '-' } else { '+' }, t.abs() / 60, t.abs() % 60)
}

fn sig(name: &[u8], email: &[u8], time: i64, tz: i16) -> Result<Signature> {
    Signature::new(name.to_vec(), email.to_vec(), time, tz).map_err(fatal)
}

impl Facts {
    pub async fn gather(ws: &WsCtx, b: &Blamed, shown: &[Piece], root: bool) -> Result<Facts> {
        let mut metas: HashMap<Origin, Meta> = HashMap::new();
        let mut previous = HashMap::new();
        let mut paths: HashMap<Origin, HashSet<&RepoPath>> = HashMap::new();
        for p in shown {
            paths.entry(p.origin).or_default().insert(&p.orig_path);
            if let Some(m) = metas.get_mut(&p.origin) {
                m.first_line = m.first_line.min(p.start);
            } else {
                let meta = match p.origin {
                    Origin::Commit(cs) => {
                        let c = ws.engine.get_changeset(cs).await?;
                        let git = ws.engine.git_commit_of(cs).await.map_err(fatal)?;
                        Meta {
                            hex: git.to_string(),
                            author: c.author.clone(),
                            committer: c.committer.clone(),
                            summary: summary_of(&c.message),
                            boundary: b.reverse_start.map_or(c.parents.is_empty() && !root, |s| s == cs),
                            time: c.committer.time(),
                            first_line: p.start,
                        }
                    }
                    Origin::Dirty => {
                        let (mtime, tz) = (b.dirty.as_ref().map_or(0, |d| d.mtime), local_tz_min());
                        let who = sig(NOT_COMMITTED, NOT_COMMITTED_MAIL, mtime, tz)?;
                        Meta {
                            hex: "0".repeat(40),
                            author: who.clone(),
                            committer: who,
                            summary: dirty_summary(p.orig_path.as_bytes()),
                            boundary: false,
                            time: i64::MAX,
                            first_line: p.start,
                        }
                    }
                };
                metas.insert(p.origin, meta);
            }
            let key = (p.origin, p.orig_path.clone());
            if previous.contains_key(&key) {
                continue;
            }
            let prev = match p.origin {
                Origin::Commit(cs) => match previous_of(ws, cs, &p.orig_path).await? {
                    Some((parent, path)) => {
                        Some((ws.engine.git_commit_of(parent).await.map_err(fatal)?.to_string(), path))
                    }
                    None => None,
                },
                Origin::Dirty => match &b.dirty {
                    Some(d) if entry_at(ws, d.head, &p.orig_path).await?.is_some() => Some((
                        ws.engine.git_commit_of(d.head).await.map_err(fatal)?.to_string(),
                        p.orig_path.clone(),
                    )),
                    _ => None,
                },
            };
            if let Some(v) = prev {
                previous.insert(key, v);
            }
        }
        let multi_path = paths.into_iter().filter(|(_, s)| s.len() > 1).map(|(o, _)| o).collect();
        Ok(Facts { metas, previous, multi_path })
    }
}

/// Output settings.
pub struct Style {
    /// Hex digits of an abbreviated id (`core.abbrev`).
    pub abbrev: usize,
    pub quote_high: bool,
}

fn quoted(path: &RepoPath, style: &Style) -> Vec<u8> {
    c_quote(path.as_bytes(), style.quote_high).unwrap_or_else(|| path.as_bytes().to_vec())
}

fn lines_of(text: &[u8]) -> Vec<&[u8]> {
    asc_engine::diff::line::split_lines(text)
}

fn without_newline(l: &[u8]) -> &[u8] {
    l.strip_suffix(b"\n").unwrap_or(l)
}

fn io(e: std::io::Error) -> CliError {
    CliError::Io(e)
}

fn digits(n: u32) -> usize {
    n.max(1).to_string().len()
}

/// The default layout: `<id> [<file> ](<author> <date> <line>) <text>`.
pub fn default_layout(
    w: &mut dyn Write,
    b: &Blamed,
    shown: &[Piece],
    facts: &Facts,
    path: &RepoPath,
    style: &Style,
) -> Result<()> {
    let text = lines_of(&b.text);
    let iso = Mode::parse("iso").expect("iso is a date mode");
    let show_name = shown.iter().any(|p| p.orig_path != *path);
    let mut author_w = 0;
    let mut name_w = 0;
    let mut last_line = 1;
    for p in shown {
        author_w = author_w.max(width(facts.metas[&p.origin].author.name()));
        name_w = name_w.max(width(&quoted(&p.orig_path, style)));
        last_line = last_line.max(p.end());
    }
    let line_w = digits(last_line);
    for p in shown {
        let m = &facts.metas[&p.origin];
        let id = match (p.origin, m.boundary) {
            (_, true) => format!("^{}", &m.hex[..style.abbrev.min(40)]),
            _ => m.hex[..(style.abbrev + 1).min(40)].to_owned(),
        };
        let date = iso.render(m.author.time(), m.author.tz_min(), 0);
        let name = quoted(&p.orig_path, style);
        for j in 0..p.len {
            let n = p.start + j;
            let mut out = Vec::with_capacity(96 + text[n as usize].len());
            out.extend_from_slice(id.as_bytes());
            out.push(b' ');
            if show_name {
                out.extend_from_slice(&name);
                out.resize(out.len() + (name_w - width(&name)), b' ');
                out.push(b' ');
            }
            out.push(b'(');
            out.extend_from_slice(m.author.name());
            out.resize(out.len() + (author_w - width(m.author.name())), b' ');
            out.extend_from_slice(format!(" {date} {:>line_w$}) ", n + 1).as_bytes());
            out.extend_from_slice(without_newline(text[n as usize]));
            out.push(b'\n');
            w.write_all(&out).map_err(io)?;
        }
    }
    Ok(())
}

fn details(w: &mut dyn Write, m: &Meta) -> Result<()> {
    let mut out = Vec::new();
    for (tag, s) in [("author", &m.author), ("committer", &m.committer)] {
        header(&mut out, tag, s.name());
        let mut mail = b"<".to_vec();
        mail.extend_from_slice(s.email());
        mail.push(b'>');
        header(&mut out, &format!("{tag}-mail"), &mail);
        out.extend_from_slice(format!("{tag}-time {}\n{tag}-tz {}\n", s.time(), tz_text(s.tz_min())).as_bytes());
    }
    header(&mut out, "summary", &m.summary);
    if m.boundary {
        out.extend_from_slice(b"boundary\n");
    }
    w.write_all(&out).map_err(io)
}

fn file_info(w: &mut dyn Write, facts: &Facts, p: &Piece, style: &Style) -> Result<()> {
    let mut out = Vec::new();
    if let Some((hex, path)) = facts.previous.get(&(p.origin, p.orig_path.clone())) {
        out.extend_from_slice(format!("previous {hex} ").as_bytes());
        out.extend_from_slice(&quoted(path, style));
        out.push(b'\n');
    }
    out.extend_from_slice(b"filename ");
    out.extend_from_slice(&quoted(&p.orig_path, style));
    out.push(b'\n');
    w.write_all(&out).map_err(io)
}

/// `--porcelain` (details once per commit) and `--line-porcelain` (details for every line).
pub fn porcelain(
    w: &mut dyn Write,
    b: &Blamed,
    shown: &[Piece],
    facts: &Facts,
    repeat: bool,
    style: &Style,
) -> Result<()> {
    let text = lines_of(&b.text);
    let mut seen: HashSet<Origin> = HashSet::new();
    for p in shown {
        let m = &facts.metas[&p.origin];
        for j in 0..p.len {
            let n = p.start + j;
            let mut head = format!("{} {} {}", m.hex, p.orig_line + j + 1, n + 1);
            if j == 0 {
                head.push_str(&format!(" {}", p.len));
            }
            head.push('\n');
            w.write_all(head.as_bytes()).map_err(io)?;
            if j == 0 {
                if seen.insert(p.origin) || repeat {
                    details(w, m)?;
                    file_info(w, facts, p, style)?;
                } else if facts.multi_path.contains(&p.origin) {
                    file_info(w, facts, p, style)?;
                }
            } else if repeat {
                details(w, m)?;
                file_info(w, facts, p, style)?;
            }
            let mut out = vec![b'\t'];
            out.extend_from_slice(without_newline(text[n as usize]));
            out.push(b'\n');
            w.write_all(&out).map_err(io)?;
        }
    }
    Ok(())
}

/// `--incremental`: one record per group, newest origin first, without the text.
pub fn incremental(w: &mut dyn Write, shown: &[Piece], facts: &Facts, style: &Style) -> Result<()> {
    let mut order: Vec<&Piece> = shown.iter().collect();
    order.sort_by_key(|p| {
        let m = &facts.metas[&p.origin];
        (std::cmp::Reverse(m.time), m.first_line, p.start)
    });
    let mut seen: HashSet<Origin> = HashSet::new();
    for p in order {
        let m = &facts.metas[&p.origin];
        w.write_all(format!("{} {} {} {}\n", m.hex, p.orig_line + 1, p.start + 1, p.len).as_bytes())
            .map_err(io)?;
        if seen.insert(p.origin) {
            details(w, m)?;
        }
        file_info(w, facts, p, style)?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------- json

fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec().into())
}

fn signature(desc: &MessageDescriptor, s: &Signature) -> Value {
    let mut m = DynamicMessage::new(desc.clone());
    m.set_field_by_name("name", bytes(s.name()));
    m.set_field_by_name("email", bytes(s.email()));
    m.set_field_by_name("time", Value::I64(s.time()));
    m.set_field_by_name("tz_min", Value::I32(i32::from(s.tz_min())));
    Value::Message(m)
}

/// `BlameResponse` lines: up to [`PAGE`] hunks each, with the commits they name. Lines are
/// one-based, as git prints them; an uncommitted hunk has an empty `origin`.
pub async fn json(ws: &WsCtx, shown: &[Piece], facts: &Facts, out: &mut dyn Write) -> Result<()> {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(fatal)?;
    let get = |n: &str| pool.get_message_by_name(n).ok_or_else(|| fatal(format!("asc: missing message {n}")));
    let (resp, hunk_d, entry_d, sig_d) =
        (get("asc.v1.BlameResponse")?, get("asc.v1.BlameHunk")?, get("asc.v1.LogEntry")?, get("asc.v1.Signature")?);
    let mut lines = JsonLines::new(out);
    for page in shown.chunks(PAGE) {
        let mut hunks = Vec::with_capacity(page.len());
        let mut commits: Vec<Value> = Vec::new();
        let mut named: HashSet<ChangesetId> = HashSet::new();
        for p in page {
            let mut h = DynamicMessage::new(hunk_d.clone());
            match p.origin {
                Origin::Commit(cs) => {
                    h.set_field_by_name("origin", bytes(cs.as_bytes()));
                    if named.insert(cs) {
                        let c = ws.engine.get_changeset(cs).await?;
                        let git = ws.engine.git_commit_of(cs).await.map_err(fatal)?;
                        let mut e = DynamicMessage::new(entry_d.clone());
                        e.set_field_by_name("id", bytes(cs.as_bytes()));
                        e.set_field_by_name("git", bytes(git.as_bytes()));
                        e.set_field_by_name(
                            "parents",
                            Value::List(c.parents.iter().map(|x| bytes(x.as_bytes())).collect()),
                        );
                        e.set_field_by_name("author", signature(&sig_d, &c.author));
                        e.set_field_by_name("committer", signature(&sig_d, &c.committer));
                        e.set_field_by_name("message", bytes(&c.message));
                        commits.push(Value::Message(e));
                    }
                }
                Origin::Dirty => h.set_field_by_name("origin", bytes(&[])),
            }
            h.set_field_by_name("orig_path", bytes(p.orig_path.as_bytes()));
            h.set_field_by_name("orig_start", Value::U32(p.orig_line + 1));
            h.set_field_by_name("start", Value::U32(p.start + 1));
            h.set_field_by_name("len", Value::U32(p.len));
            hunks.push(Value::Message(h));
        }
        let mut m = DynamicMessage::new(resp.clone());
        m.set_field_by_name("hunks", Value::List(hunks));
        m.set_field_by_name("commits", Value::List(commits));
        lines.emit(&m).map_err(fatal)?;
    }
    let _ = facts;
    lines.finish().map_err(io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_print_as_git() {
        assert_eq!(tz_text(60), "+0100");
        assert_eq!(tz_text(-330), "-0530");
        assert_eq!(tz_text(0), "+0000");
    }

    #[test]
    fn summary_is_the_first_line() {
        assert_eq!(summary_of(b"first  \nbody\n"), b"first");
        assert_eq!(summary_of(b"\n\nfirst\nbody"), b"first");
        assert_eq!(summary_of(b""), b"");
    }

    fn meta(author: &[u8], summary: Vec<u8>) -> Meta {
        let who = sig(author, b"a@b", 5, 0).unwrap();
        Meta {
            hex: "0".repeat(40),
            author: who.clone(),
            committer: who,
            summary,
            boundary: false,
            time: 0,
            first_line: 0,
        }
    }

    fn details_text(m: &Meta) -> String {
        let mut out = Vec::new();
        details(&mut out, m).unwrap();
        String::from_utf8(out).unwrap()
    }

    const TAGS: [&str; 9] = [
        "author",
        "author-mail",
        "author-time",
        "author-tz",
        "committer",
        "committer-mail",
        "committer-time",
        "committer-tz",
        "summary",
    ];

    #[test]
    fn normal_dirty_summary_is_git_exact() {
        assert_eq!(dirty_summary(b"dir/a b.txt"), b"Version of dir/a b.txt from dir/a b.txt");
    }

    #[test]
    fn hostile_path_cannot_forge_records() {
        let m = meta(b"Me", dirty_summary(b"a\nauthor evil\r\nfilename x"));
        let text = details_text(&m);
        let tags: Vec<&str> = text.lines().map(|l| l.split(' ').next().unwrap()).collect();
        assert_eq!(tags, TAGS);
        assert!(!text.contains('\r'));
        assert!(text.contains("summary Version of \"a\\nauthor evil\\r\\nfilename x\" from "));
    }

    #[test]
    fn hostile_author_cannot_forge_records() {
        // `Signature::new` already refuses LF; CR is the byte that can still reach the writer.
        let m = meta(b"Eve\rauthor-mail x\rsummary forged", b"s\r".to_vec());
        let text = details_text(&m);
        let tags: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).map(|l| l.split(' ').next().unwrap()).collect();
        assert_eq!(tags, TAGS);
        assert!(!text.contains('\r'));
        assert!(text.starts_with("author Eve author-mail x summary forged\nauthor-mail <a@b>\n"));
    }

    #[test]
    fn numbers_have_widths() {
        assert_eq!((digits(0), digits(9), digits(10)), (1, 1, 2));
    }
}
