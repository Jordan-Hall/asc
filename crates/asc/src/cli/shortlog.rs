//! `shortlog` (SPEC §11.2): commit subjects grouped by author, committer or a trailer, over the
//! same revision walk as `log` (`-n`, `--since`, `--author`, pathspecs…). `-n`, `-s`, `-e`,
//! `-c`, `-w[<w>[,<i1>[,<i2>]]]` and `--group=author|committer|trailer:<key>` are served.
//! Without a revision and with standard input not a terminal, `git log` output is read from
//! standard input, as git does. `.mailmap` applies.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write as _};
use std::process::ExitCode;

use super::commit::message::subject;
use super::common::{CliError, Ctx, Result, WsCtx};
use super::format::mailmap::Mailmap;
use super::format::trailers::{self, Entry as TrailerEntry};
use super::log::opts::Opts;
use super::log::revs;
use super::log::walk::{Hist, Walker};
use super::output::Output;
use super::pager::PagerWant;
use crate::native::NativeSpec;

/// Largest `.mailmap` read.
const MAILMAP_MAX: u64 = 16 << 20;
/// Most lines read from standard input.
const MAX_STDIN_LINES: usize = 50_000_000;
const DEFAULT_WRAP: usize = 76;
const DEFAULT_INDENT1: usize = 6;
const DEFAULT_INDENT2: usize = 9;

const USAGE: &str = "usage: git shortlog [<options>] [<revision-range>] [[--] <path>...]
   or: git log --pretty=short | git shortlog [<options>]

    -c, --[no-]committer  group by committer rather than author
    -n, --[no-]numbered   sort output according to the number of commits per author
    -s, --[no-]summary    suppress commit descriptions, only provides commit count
    -e, --[no-]email      show the email address of each author
    -w[<w>[,<i1>[,<i2>]]] linewrap output
    --[no-]group <field>  group by field
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "shortlog",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "shortlog",
    flags: &[
        "-c",
        "--committer",
        "-n",
        "--numbered",
        "-s",
        "--summary",
        "-e",
        "--email",
        "-w",
        "--group",
        "-<n>",
        "--max-count",
        "--skip",
        "--since",
        "--after",
        "--until",
        "--before",
        "--author",
        "--grep",
        "-i",
        "--regexp-ignore-case",
        "-E",
        "--extended-regexp",
        "-F",
        "--fixed-strings",
        "--invert-grep",
        "--first-parent",
        "--all",
        "--branches",
        "--tags",
        "--remotes",
        "--not",
        "--merges",
        "--no-merges",
        "--reverse",
        "--end-of-options",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Group {
    Author,
    Committer,
    Trailer(String),
}

#[derive(Default)]
struct Mine {
    numbered: bool,
    summary: bool,
    email: bool,
    committer: bool,
    wrap: Option<(usize, usize, usize)>,
    groups: Vec<Group>,
}

/// Parses `-w[<w>[,<i1>[,<i2>]]]`.
fn parse_wrap(spec: &str) -> Result<(usize, usize, usize)> {
    let mut parts = spec.split(',');
    let num = |p: Option<&str>, default: usize| -> Result<usize> {
        match p {
            None | Some("") => Ok(default),
            Some(v) => v.parse::<usize>().map_err(|_| {
                CliError::Usage(format!("error: bad wrap argument '{spec}'\n{USAGE}"))
            }),
        }
    };
    let w = num(parts.next(), DEFAULT_WRAP)?;
    let i1 = num(parts.next(), DEFAULT_INDENT1)?;
    let i2 = num(parts.next(), DEFAULT_INDENT2)?;
    Ok((w, i1, i2))
}

/// Splits shortlog's own options from the rest, which `log`'s parser reads.
fn split(args: &[OsString]) -> Result<(Mine, Vec<OsString>)> {
    let mut mine = Mine::default();
    let mut rest: Vec<OsString> = Vec::new();
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    let mut done = false;
    while let Some(arg) = it.next() {
        if done {
            rest.push(arg.into());
            continue;
        }
        if arg == "--" {
            done = true;
            rest.push(arg.into());
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v.to_owned())));
            match name {
                "numbered" => mine.numbered = true,
                "no-numbered" => mine.numbered = false,
                "summary" => mine.summary = true,
                "no-summary" => mine.summary = false,
                "email" => mine.email = true,
                "no-email" => mine.email = false,
                "committer" => mine.committer = true,
                "no-committer" => mine.committer = false,
                "group" => {
                    let v = inline.or_else(|| it.next()).ok_or_else(|| {
                        CliError::Usage(format!("error: option `group' requires a value\n{USAGE}"))
                    })?;
                    mine.groups.push(match v.as_str() {
                        "author" => Group::Author,
                        "committer" => Group::Committer,
                        other => match other.strip_prefix("trailer:") {
                            Some(k) if !k.is_empty() => Group::Trailer(k.to_owned()),
                            _ => return Err(fatal(format!("unknown group type: {other}"))),
                        },
                    });
                }
                "no-group" => mine.groups.clear(),
                _ => rest.push(arg.into()),
            }
            continue;
        }
        if let Some(w) = arg.strip_prefix("-w") {
            mine.wrap = Some(parse_wrap(w)?);
            continue;
        }
        let is_cluster = arg.len() > 1
            && arg.starts_with('-')
            && arg[1..].chars().all(|c| matches!(c, 'c' | 'n' | 's' | 'e'));
        if is_cluster {
            for c in arg[1..].chars() {
                match c {
                    'c' => mine.committer = true,
                    'n' => mine.numbered = true,
                    's' => mine.summary = true,
                    _ => mine.email = true,
                }
            }
            continue;
        }
        rest.push(arg.into());
    }
    if mine.committer {
        mine.groups.push(Group::Committer);
    }
    if mine.groups.is_empty() {
        mine.groups.push(Group::Author);
    }
    Ok((mine, rest))
}

fn load_mailmap(ctx: &Ctx, ws: &WsCtx) -> Option<Mailmap> {
    let path = match ctx.config.get("mailmap.file") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let name = asc_core::path::RepoPath::new(b".mailmap".to_vec()).ok()?;
            asc_wc::SafeRoot::new(ws.projection.toplevel()).resolve(&name)?
        }
    };
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAILMAP_MAX {
        return None;
    }
    std::fs::read(&path).ok().map(|b| Mailmap::parse(&b))
}

/// What the report accumulates: per display name, the subjects in walk order.
#[derive(Default)]
struct Tally {
    by_name: BTreeMap<Vec<u8>, Vec<String>>,
}

impl Tally {
    fn add(&mut self, key: Vec<u8>, subject: &str) {
        self.by_name
            .entry(key)
            .or_default()
            .push(subject.to_owned());
    }
}

/// The text of a subject as `shortlog` stores it: `[PATCH…]` prefixes are dropped.
fn clean_subject(subject: &str) -> String {
    let s = subject.trim_start();
    if s.starts_with("[PATCH")
        && let Some(end) = s.find(']')
    {
        return s[end + 1..].trim_start().to_owned();
    }
    s.to_owned()
}

fn display(name: &[u8], email: &[u8], with_email: bool) -> Vec<u8> {
    let mut out = name.to_vec();
    if with_email {
        out.extend_from_slice(b" <");
        out.extend_from_slice(email);
        out.push(b'>');
    }
    out
}

/// `Name <email>` split; a value without an address is all name.
fn split_ident(value: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if let (Some(open), Some(close)) = (
        value.iter().position(|&b| b == b'<'),
        value.iter().rposition(|&b| b == b'>'),
    ) && open < close
    {
        return (
            value[..open].trim_ascii().to_vec(),
            value[open + 1..close].trim_ascii().to_vec(),
        );
    }
    (value.trim_ascii().to_vec(), Vec::new())
}

/// git's word wrap of one line: words are kept on a line while it stays within `width`.
fn wrap_text(text: &str, width: usize, indent1: usize, indent2: usize) -> String {
    let mut out = String::new();
    let mut col = indent1;
    out.push_str(&" ".repeat(indent1));
    let mut first_on_line = true;
    for word in text.split_whitespace() {
        let len = word.chars().count();
        if first_on_line {
            out.push_str(word);
            col += len;
            first_on_line = false;
        } else if col + 1 + len <= width {
            out.push(' ');
            out.push_str(word);
            col += 1 + len;
        } else {
            out.push('\n');
            out.push_str(&" ".repeat(indent2));
            out.push_str(word);
            col = indent2 + len;
        }
    }
    out
}

fn render(tally: &Tally, mine: &Mine) -> Vec<u8> {
    let mut rows: Vec<(&Vec<u8>, &Vec<String>)> = tally.by_name.iter().collect();
    if mine.numbered {
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    }
    let mut out: Vec<u8> = Vec::new();
    for (name, subjects) in rows {
        if mine.summary {
            out.extend_from_slice(format!("{:6}\t", subjects.len()).as_bytes());
            out.extend_from_slice(name);
            out.push(b'\n');
            continue;
        }
        out.extend_from_slice(name);
        out.extend_from_slice(format!(" ({}):\n", subjects.len()).as_bytes());
        // Oldest first: the walk is newest first.
        for s in subjects.iter().rev() {
            match mine.wrap {
                Some((w, i1, i2)) if w > 0 => {
                    out.extend_from_slice(wrap_text(s, w, i1, i2).as_bytes())
                }
                Some((_, i1, _)) => {
                    out.extend_from_slice(" ".repeat(i1).as_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
                None => {
                    out.extend_from_slice(" ".repeat(DEFAULT_INDENT1).as_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
            }
            out.push(b'\n');
        }
        out.push(b'\n');
    }
    out
}

/// The tally of the commits `git log` printed on standard input.
fn read_stdin(mine: &Mine, mailmap: Option<&Mailmap>) -> Result<Tally> {
    let want = if mine.groups.contains(&Group::Committer) && !mine.groups.contains(&Group::Author) {
        "Commit: "
    } else {
        "Author: "
    };
    let mut tally = Tally::default();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().split(b'\n');
    let mut count = 0usize;
    while let Some(line) = lines.next() {
        let line = line?;
        count += 1;
        if count > MAX_STDIN_LINES {
            return Err(fatal("too many input lines"));
        }
        let Some(value) = line.strip_prefix(want.as_bytes()) else {
            continue;
        };
        let (name, email) = split_ident(value);
        // Headers up to the blank line, then blank lines, then the subject.
        for l in lines.by_ref() {
            if l?.is_empty() {
                break;
            }
        }
        let mut subject_line = None;
        for l in lines.by_ref() {
            let l = l?;
            if !l.is_empty() {
                subject_line = Some(l);
                break;
            }
        }
        let Some(subject_line) = subject_line else {
            break;
        };
        let (name, email) = match mailmap {
            Some(m) => m.lookup(&name, &email),
            None => (name, email),
        };
        let text = clean_subject(&String::from_utf8_lossy(&subject_line));
        tally.add(display(&name, &email, mine.email), &text);
    }
    Ok(tally)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let (mine, rest) = split(&a.rest)?;
    let o = Opts::parse(&rest)?;
    let ws = ctx.ws()?;
    let mailmap = load_mailmap(ctx, ws);
    let from_stdin = o.revs.is_empty() && o.paths.is_empty() && !std::io::stdin().is_terminal();
    let tally = if from_stdin {
        read_stdin(&mine, mailmap.as_ref())?
    } else {
        let hist = Hist::select(ws);
        let sel = revs::select(ctx, hist.api(ws), &o).await?;
        let mut walker = Walker::new(ws, &hist, &sel, &o).await?;
        let mut tally = Tally::default();
        while let Some(e) = walker.next().await? {
            let text = clean_subject(&subject(&e.cs.message));
            let mut seen: Vec<Vec<u8>> = Vec::new();
            for g in &mine.groups {
                let idents: Vec<(Vec<u8>, Vec<u8>)> = match g {
                    Group::Author => {
                        vec![(e.cs.author.name().to_vec(), e.cs.author.email().to_vec())]
                    }
                    Group::Committer => {
                        vec![(
                            e.cs.committer.name().to_vec(),
                            e.cs.committer.email().to_vec(),
                        )]
                    }
                    Group::Trailer(key) => trailers::parse(&e.cs.message)
                        .into_iter()
                        .filter_map(|t| match t {
                            TrailerEntry::Trailer { key: k, value, .. }
                                if k.eq_ignore_ascii_case(key.as_bytes()) =>
                            {
                                Some(split_ident(&value))
                            }
                            _ => None,
                        })
                        .collect(),
                };
                for (name, email) in idents {
                    let (name, email) = match &mailmap {
                        Some(m) => m.lookup(&name, &email),
                        None => (name, email),
                    };
                    let key = display(&name, &email, mine.email);
                    // One commit counts once for a name, however many groups yield it.
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.push(key.clone());
                    tally.add(key, &text);
                }
            }
        }
        tally
    };
    let text = render(&tally, &mine);
    let mut out = Output::open(ctx, "shortlog", PagerWant::Auto)?;
    let result = out.write_all(&text).map_err(CliError::from);
    let flushed = out.finish();
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => {
            other?;
            flushed?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_like_git() {
        assert_eq!(
            wrap_text(
                "third: a long subject line to see wrapping behaviour of shortlog when asked -w",
                20,
                2,
                4
            ),
            "  third: a long\n    subject line to\n    see wrapping\n    behaviour of\n    shortlog when\n    asked -w"
        );
    }

    #[test]
    fn clusters_and_groups_split_out() {
        let args: Vec<OsString> = ["-sne", "--group=trailer:co-authored-by", "-3", "HEAD"]
            .iter()
            .map(OsString::from)
            .collect();
        let (m, rest) = split(&args).unwrap();
        assert!(m.summary && m.numbered && m.email);
        assert_eq!(m.groups, [Group::Trailer("co-authored-by".into())]);
        assert_eq!(rest, [OsString::from("-3"), OsString::from("HEAD")]);
    }

    #[test]
    fn patch_prefixes_are_dropped() {
        assert_eq!(clean_subject("[PATCH 1/2] fix"), "fix");
        assert_eq!(clean_subject("plain"), "plain");
    }

    #[test]
    fn renders_summary_and_groups() {
        let mut t = Tally::default();
        t.add(b"A".to_vec(), "one");
        t.add(b"A".to_vec(), "two");
        t.add(b"B".to_vec(), "x");
        let mut m = Mine::default();
        assert_eq!(
            String::from_utf8(render(&t, &m)).unwrap(),
            "A (2):\n      two\n      one\n\nB (1):\n      x\n\n"
        );
        m.summary = true;
        assert_eq!(
            String::from_utf8(render(&t, &m)).unwrap(),
            "     2\tA\n     1\tB\n"
        );
    }
}
