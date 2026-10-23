//! `grep` (SPEC §11.2): searches the tracked files of the work tree (the staged tree) as they are
//! on disk, so dirty files are searched too. Every file is opened through `asc_wc::SafeRoot`: a
//! path under a link, a link itself and anything but a regular file is not searched. Patterns are
//! POSIX basic (default), extended (`-E`) or fixed (`-F`) expressions, run by the `regex` crate
//! (linear time, size limited); back-references are refused.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::ExitCode;

use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use regex::bytes::{Regex, RegexBuilder};

use super::add::cwd_prefix;
use super::add::spec::Spec;
use super::add::target::{Staged, Target};
use super::check_ignore::relative;
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "grep", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "grep",
    flags: &[
        "-n", "--line-number", "-i", "--ignore-case", "-w", "--word-regexp", "-l",
        "--files-with-matches", "--name-only", "-c", "--count", "-v", "--invert-match", "-q",
        "--quiet", "-e", "--regexp", "-F", "--fixed-strings", "-E", "--extended-regexp", "-G",
        "--basic-regexp",
    ],
    placeholders: &[],
}];

/// Longest line searched; a file with a longer line is reported and skipped.
const MAX_LINE: usize = 16 << 20;
/// Largest compiled expression (bytes of the program).
const MAX_REGEX_BYTES: usize = 16 << 20;
const NEWLINE: u8 = 10;
const LF: &[u8] = &[NEWLINE];
/// How much of a file git looks at to decide it is binary.
const BINARY_PROBE: usize = 8000;

#[derive(Default)]
struct Opts {
    line_number: bool,
    ignore_case: bool,
    word: bool,
    files_only: bool,
    count: bool,
    invert: bool,
    quiet: bool,
    fixed: bool,
    extended: bool,
    patterns: Vec<OsString>,
    paths: Vec<OsString>,
    /// A `--` was typed: the arguments after it are paths, never revisions.
    dashes: bool,
}

fn bad_option(name: &str, short: bool) -> CliError {
    let kind = if short { "switch" } else { "option" };
    CliError::Usage(format!("error: unknown {kind} `{name}'"))
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut positional: Vec<OsString> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        i += 1;
        let text = arg.to_string_lossy().into_owned();
        if o.dashes || !text.starts_with('-') || text == "-" {
            positional.push(arg.clone());
            continue;
        }
        if text == "--" {
            o.dashes = true;
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            match name {
                "line-number" => o.line_number = true,
                "ignore-case" => o.ignore_case = true,
                "word-regexp" => o.word = true,
                "files-with-matches" | "name-only" => o.files_only = true,
                "count" => o.count = true,
                "invert-match" => o.invert = true,
                "quiet" => o.quiet = true,
                "fixed-strings" => o.fixed = true,
                "extended-regexp" => o.extended = true,
                "basic-regexp" => o.extended = false,
                "regexp" => {
                    let value = match value {
                        Some(v) => OsString::from(v),
                        None => {
                            let v = rest.get(i).cloned().ok_or_else(|| {
                                CliError::Usage("error: switch `e' requires a value".to_owned())
                            })?;
                            i += 1;
                            v
                        }
                    };
                    o.patterns.push(value);
                }
                other => return Err(bad_option(other, false)),
            }
            continue;
        }
        for (at, c) in text[1..].char_indices() {
            match c {
                'n' => o.line_number = true,
                'i' => o.ignore_case = true,
                'w' => o.word = true,
                'l' => o.files_only = true,
                'c' => o.count = true,
                'v' => o.invert = true,
                'q' => o.quiet = true,
                'F' => o.fixed = true,
                'E' => o.extended = true,
                'G' => o.extended = false,
                'e' => {
                    let attached = &text[1 + at + 1..];
                    let value = if attached.is_empty() {
                        let v = rest.get(i).cloned().ok_or_else(|| {
                            CliError::Usage("error: switch `e' requires a value".to_owned())
                        })?;
                        i += 1;
                        v
                    } else {
                        OsString::from(attached)
                    };
                    o.patterns.push(value);
                    break;
                }
                other => return Err(bad_option(&other.to_string(), true)),
            }
        }
    }
    let mut positional = positional.into_iter();
    if o.patterns.is_empty() {
        match positional.next() {
            Some(p) => o.patterns.push(p),
            None => return Err(CliError::Fatal("no pattern given".into())),
        }
    }
    o.paths = positional.collect();
    Ok(o)
}

// ---------------------------------------------------------------- expressions

/// Pushes the POSIX bracket expression that starts at `chars[at]` (`[`) as a `regex` class and
/// returns the index after its closing `]`.
fn bracket(chars: &[char], at: usize, out: &mut String) -> std::result::Result<usize, String> {
    let mut i = at + 1;
    out.push('[');
    if chars.get(i) == Some(&'^') {
        out.push('^');
        i += 1;
    }
    let mut first = true;
    loop {
        let Some(&c) = chars.get(i) else {
            return Err("Unmatched [, [^, [:, [., or [=".to_owned());
        };
        match c {
            ']' if !first => {
                out.push(']');
                return Ok(i + 1);
            }
            ']' => out.push_str("\\]"),
            '[' if matches!(chars.get(i + 1), Some(':')) => {
                let close = (i + 2..chars.len().saturating_sub(1))
                    .find(|&j| chars[j] == ':' && chars[j + 1] == ']')
                    .ok_or_else(|| "Unmatched [, [^, [:, [., or [=".to_owned())?;
                out.extend(&chars[i..close + 2]);
                i = close + 2;
                first = false;
                continue;
            }
            '[' if matches!(chars.get(i + 1), Some('=' | '.')) => {
                return Err("equivalence classes and collating symbols are not supported".to_owned());
            }
            '[' | '\\' | '&' | '~' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
        first = false;
        i += 1;
    }
}

fn push_literal(out: &mut String, c: char) {
    let mut tmp = [0u8; 4];
    out.push_str(&regex::escape(c.encode_utf8(&mut tmp)));
}

/// Translates a POSIX basic (`extended` false) or extended expression to the `regex` syntax.
fn translate(pattern: &str, extended: bool) -> std::result::Result<String, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // Where `*` is a literal and `^` an anchor: the start, after `(` and after `|`.
    let mut at_start = true;
    while i < chars.len() {
        let c = chars[i];
        let mut next_start = false;
        match c {
            '\\' => {
                i += 1;
                let Some(&e) = chars.get(i) else {
                    return Err("Trailing backslash".to_owned());
                };
                match e {
                    '(' | ')' | '{' | '}' | '|' | '+' | '?' if !extended => {
                        out.push(e);
                        next_start = matches!(e, '(' | '|');
                    }
                    '<' => out.push_str("\\b{start}"),
                    '>' => out.push_str("\\b{end}"),
                    '1'..='9' => return Err("back-references are not supported".to_owned()),
                    'w' | 'W' | 's' | 'S' | 'b' | 'B' => {
                        out.push('\\');
                        out.push(e);
                    }
                    other => push_literal(&mut out, other),
                }
            }
            '[' => {
                i = bracket(&chars, i, &mut out)?;
                at_start = false;
                continue;
            }
            '*' if !extended && at_start => out.push_str("\\*"),
            '^' if !extended && !at_start => out.push_str("\\^"),
            '$' if !extended => {
                let end = i + 1 == chars.len()
                    || (chars.get(i + 1) == Some(&'\\') && matches!(chars.get(i + 2), Some(')' | '|')));
                out.push_str(if end { "$" } else { "\\$" });
            }
            '(' | ')' | '{' | '}' | '|' | '+' | '?' if !extended => push_literal(&mut out, c),
            '(' | '|' if extended => {
                out.push(c);
                next_start = true;
            }
            c => out.push(c),
        }
        at_start = next_start;
        i += 1;
    }
    Ok(out)
}

fn compile(o: &Opts) -> Result<Regex> {
    let mut parts = Vec::new();
    for p in &o.patterns {
        let text = p.to_string_lossy();
        let body = if o.fixed {
            regex::escape(&text)
        } else {
            translate(&text, o.extended)
                .map_err(|why| CliError::Fatal(format!("command line, '{text}': {why}")))?
        };
        parts.push(format!("(?:{body})"));
    }
    let build = |source: &str| {
        RegexBuilder::new(source)
            .case_insensitive(o.ignore_case)
            .size_limit(MAX_REGEX_BYTES)
            .dfa_size_limit(MAX_REGEX_BYTES)
            .build()
    };
    build(&parts.join("|")).map_err(|e| {
        let culprit = o
            .patterns
            .iter()
            .zip(&parts)
            .find(|(_, part)| build(part).is_err())
            .map_or_else(|| "?".to_owned(), |(p, _)| p.to_string_lossy().into_owned());
        CliError::Fatal(format!("command line, '{culprit}': Invalid regular expression ({})", first_line(&e.to_string())))
    })
}

fn first_line(s: &str) -> String {
    s.lines().last().unwrap_or(s).trim().to_owned()
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `re` matches `line`; with `word`, only at a match with a non-word byte (or the line
/// edge) on both sides.
fn hits(re: &Regex, word: bool, line: &[u8]) -> bool {
    if !word {
        return re.is_match(line);
    }
    let mut start = 0;
    while start <= line.len() {
        let Some(m) = re.find_at(line, start) else { return false };
        let left = m.start() == 0 || !is_word(line[m.start() - 1]);
        let right = m.end() == line.len() || !is_word(line[m.end()]);
        if left && right {
            return true;
        }
        start = m.start() + 1;
    }
    false
}

// ---------------------------------------------------------------- searching

struct Search<'a> {
    re: &'a Regex,
    o: &'a Opts,
}

impl Search<'_> {
    /// Streams `file`, calling `emit(number, line, binary)` for each selected line until `emit`
    /// says stop. Returns how many lines were selected, or why the file was given up on.
    fn file(
        &self,
        file: fs::File,
        emit: &mut dyn FnMut(usize, &[u8], bool) -> bool,
    ) -> io::Result<std::result::Result<usize, &'static str>> {
        let mut reader = BufReader::with_capacity(64 << 10, file);
        let head = reader.fill_buf()?;
        let binary = head[..head.len().min(BINARY_PROBE)].contains(&0);
        let mut count = 0usize;
        let mut number = 0usize;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut reader).take(MAX_LINE as u64 + 1).read_until(NEWLINE, &mut line)?;
            if n == 0 {
                break;
            }
            if line.len() > MAX_LINE && line.last() != Some(&NEWLINE) {
                return Ok(Err("a line is longer than 16 MiB"));
            }
            number += 1;
            if line.last() == Some(&NEWLINE) {
                line.pop();
            }
            if hits(self.re, self.o.word, &line) != self.o.invert {
                count += 1;
                if !emit(number, &line, binary) {
                    break;
                }
            }
        }
        Ok(Ok(count))
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut o = parse(&a.rest)?;
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let prefix = cwd_prefix(ctx, ws);
    let re = compile(&o)?;
    if o.paths.is_empty() && !prefix.is_empty() {
        o.paths.push(OsString::from("."));
    }
    let spec = Spec::parse(&o.paths, &prefix, root)?;
    let gate = SafeRoot::new(root);
    let target = Target::open(ctx).await?;
    let mut seen = vec![false; spec.includes().len()];
    let mut files: Vec<RepoPath> = Vec::new();
    target
        .tracked(ws, &spec, |path, t| {
            spec.mark(path.as_bytes(), &mut seen);
            if !matches!(&t.staged, Staged::Leaf { leaf, .. } if leaf.kind == Kind::Gitlink) {
                files.push(path.clone());
            }
            Ok(())
        })
        .await?;
    if !o.dashes {
        for (item, hit) in spec.includes().iter().zip(&seen) {
            let exists = item.literal().is_some_and(|p| {
                p.is_empty()
                    || RepoPath::new(p.to_vec())
                        .ok()
                        .and_then(|rp| gate.resolve(&rp))
                        .is_some_and(|abs| fs::symlink_metadata(abs).is_ok())
            });
            if !hit && !exists && !item.is_wild() {
                return Err(CliError::Fatal(format!(
                    "ambiguous argument '{}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'",
                    item.raw()
                )));
            }
        }
    }
    files.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

    let strip = if prefix.is_empty() { Vec::new() } else { format!("{prefix}/").into_bytes() };
    let search = Search { re: &re, o: &o };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut any = false;
    let mut failed: Option<io::Error> = None;
    for path in &files {
        let Some(abs) = gate.resolve(path) else { continue };
        let Ok(meta) = fs::symlink_metadata(&abs) else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(file) = fs::File::open(&abs) else { continue };
        let shown = relative(path.as_bytes(), &strip);
        let mut emit = |number: usize, line: &[u8], binary: bool| -> bool {
            any = true;
            if o.quiet {
                return false;
            }
            if o.files_only || o.count {
                return !o.files_only;
            }
            let written = if binary {
                [&b"Binary file "[..], &shown, &b" matches"[..], LF].iter().try_for_each(|p| out.write_all(p))
            } else {
                (|| {
                    out.write_all(&shown)?;
                    out.write_all(b":")?;
                    if o.line_number {
                        out.write_all(number.to_string().as_bytes())?;
                        out.write_all(b":")?;
                    }
                    out.write_all(line)?;
                    out.write_all(LF)
                })()
            };
            match written {
                Ok(()) => !binary,
                Err(e) => {
                    failed = Some(e);
                    false
                }
            }
        };
        match search.file(file, &mut emit) {
            Err(e) => {
                let name = String::from_utf8_lossy(&shown);
                eprintln!("warning: asc: grep: {name}: {e}; the file was not searched completely");
            }
            Ok(Err(why)) => {
                let name = String::from_utf8_lossy(&shown);
                eprintln!("warning: asc: grep: {name}: {why}; the file was not searched");
            }
            Ok(Ok(count)) if count > 0 && !o.quiet && (o.files_only || o.count) => {
                let count_text = format!(":{count}");
                let (name, tail): (&[u8], &[u8]) =
                    if o.files_only { (&shown, LF) } else { (&shown, count_text.as_bytes()) };
                if let Err(e) = out.write_all(name).and_then(|()| out.write_all(tail)).and_then(|()| if o.files_only { Ok(()) } else { out.write_all(LF) }) {
                    failed = Some(e);
                }
            }
            Ok(Ok(_)) => {}
        }
        if failed.is_some() || (o.quiet && any) {
            break;
        }
    }
    let flushed = match failed {
        None => out.flush(),
        Some(e) => Err(e),
    };
    match flushed {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(ExitCode::from(u8::from(!any))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, extended: bool, line: &str) -> bool {
        let re = Regex::new(&translate(pattern, extended).unwrap()).unwrap();
        re.is_match(line.as_bytes())
    }

    #[test]
    fn basic_expressions_follow_posix() {
        assert!(matches("a\\|b", false, "xb"));
        assert!(matches("a+b", false, "a+b"));
        assert!(!matches("a+b", false, "aab"));
        assert!(matches("a\\+b", false, "aab"));
        assert!(matches("(x)", false, "(x)"));
        assert!(matches("\\(x\\)y", false, "xy"));
        assert!(matches("*a", false, "*a"));
        assert!(matches("a^b$c", false, "a^b$c"));
        assert!(matches("^a.c$", false, "abc"));
        assert!(matches("[[:digit:]]\\{2\\}", false, "x42"));
        assert!(matches("[]x]", false, "]"));
        assert!(matches("[\\]", false, "\\"));
        assert!(matches("\\<foo\\>", false, "a foo b"));
        assert!(!matches("\\<foo\\>", false, "afoo b"));
    }

    #[test]
    fn extended_expressions_and_refusals() {
        assert!(matches("a|b", true, "b"));
        assert!(matches("(ab)+c", true, "ababc"));
        assert!(translate("(a)\\1", false).is_err());
        assert!(translate("(a)\\1", true).is_err());
        assert!(translate("a\\", false).is_err());
        assert!(translate("[a", false).is_err());
        assert!(translate("[[=a=]]", false).is_err());
    }

    #[test]
    fn word_matches_need_boundaries() {
        let re = Regex::new("foo").unwrap();
        assert!(hits(&re, true, b"a foo b"));
        assert!(hits(&re, true, b"foo_x foo"));
        assert!(!hits(&re, true, b"foobar"));
        assert!(!hits(&re, true, b"xfoo"));
        assert!(hits(&re, false, b"foobar"));
    }

    #[test]
    fn options_and_patterns_parse_like_git() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let o = parse(&args(&["-ni", "foo", "--", "a", "b"])).unwrap();
        assert!(o.line_number && o.ignore_case && o.dashes);
        assert_eq!((o.patterns.len(), o.paths.len()), (1, 2));
        let o = parse(&args(&["-e", "x", "-ey", "p"])).unwrap();
        assert_eq!(o.patterns, [OsString::from("x"), OsString::from("y")]);
        assert_eq!(o.paths, [OsString::from("p")]);
        assert!(matches!(parse(&args(&[])), Err(CliError::Fatal(_))));
        assert!(matches!(parse(&args(&["-Z", "x"])), Err(CliError::Usage(_))));
    }
}
