//! `format-patch` (PLAN U27b): one mbox message per commit, in git's layout. A commit becomes
//! `From <id> Mon Sep 17 00:00:00 2001`, `From:`, `Date:` and `Subject: [PATCH n/m] ...`
//! headers, the message body, `---`, the diffstat with its summary, the patch and the
//! `-- ` signature (git's version string).
//!
//! Served: a revision range or `<since>`, `-<n>`, `--root`, `--stdout`, `-o <dir>`, `-n`, `-N`,
//! `-k`, `-q`, `--start-number`, `--subject-prefix`, `--signature`, `--no-signature`. Not served:
//! `--cover-letter`, threading, `--to`/`--cc`, attachments, `--base`, and binary diffs (they are
//! written as `Binary files differ`).

use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_core::id::TreeId;
use asc_core::object::Changeset;
use asc_core::sig::Signature;
use asc_engine::diff::format::write_summary;
use asc_engine::diff::{
    DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_patch, write_stat,
};

use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::format::date::Mode;
use super::log::opts::Opts;
use super::log::revs;
use super::log::walk::{Entry, Hist, Walker};
use super::switch::abbrev_len;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "format-patch",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "format-patch",
    flags: &[
        "--stdout",
        "-o",
        "--output-directory",
        "-n",
        "--numbered",
        "-N",
        "--no-numbered",
        "-k",
        "--keep-subject",
        "-q",
        "--quiet",
        "--start-number",
        "--subject-prefix",
        "--signature",
        "--no-signature",
        "--root",
        "-<n>",
    ],
    placeholders: &[],
}];

/// Width of the diffstat and the subject (git's `MAIL_DEFAULT_WRAP` for the stat, 78 columns for
/// the subject line).
const STAT_WIDTH: usize = 72;
const SUBJECT_WIDTH: usize = 78;
/// Longest file name part (git's `FORMAT_PATCH_NAME_MAX_DEFAULT`).
const NAME_MAX: usize = 64;
/// Most patches one run writes.
const MAX_PATCHES: usize = 1_000_000;

#[derive(Default)]
struct Flags {
    stdout: bool,
    out_dir: Option<String>,
    numbered: Option<bool>,
    keep: bool,
    quiet: bool,
    start: usize,
    prefix: Option<String>,
    signature: Option<Option<String>>,
    root: bool,
    count: Option<usize>,
    words: Vec<OsString>,
}

fn usage() -> CliError {
    CliError::Usage("usage: git format-patch [<options>] [<since> | <revision-range>]".to_owned())
}

fn parse(args: &[OsString]) -> Result<Flags> {
    let mut f = Flags {
        start: 1,
        ..Flags::default()
    };
    let mut i = 0;
    let mut ended = false;
    let text = |i: &mut usize, inline: Option<&str>| -> Result<String> {
        if let Some(v) = inline {
            return Ok(v.to_owned());
        }
        *i += 1;
        args.get(*i)
            .map(|a| a.to_string_lossy().into_owned())
            .ok_or_else(usage)
    };
    while i < args.len() {
        let arg = args[i].to_string_lossy().into_owned();
        if ended || !arg.starts_with('-') || arg == "-" {
            f.words.push(args[i].clone());
            i += 1;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        match name.as_str() {
            "--" => ended = true,
            "--stdout" => f.stdout = true,
            "--root" => f.root = true,
            "-n" | "--numbered" => f.numbered = Some(true),
            "-N" | "--no-numbered" => f.numbered = Some(false),
            "-k" | "--keep-subject" => f.keep = true,
            "-q" | "--quiet" => f.quiet = true,
            "--no-signature" => f.signature = Some(None),
            "--signature" => f.signature = Some(Some(text(&mut i, inline.as_deref())?)),
            "--subject-prefix" => f.prefix = Some(text(&mut i, inline.as_deref())?),
            "--start-number" => {
                let v = text(&mut i, inline.as_deref())?;
                f.start = v
                    .parse()
                    .map_err(|_| CliError::Fatal(format!("'{v}' is not an integer")))?;
            }
            "-o" | "--output-directory" => f.out_dir = Some(text(&mut i, inline.as_deref())?),
            s if s.starts_with("-o") && !s.starts_with("--") => f.out_dir = Some(s[2..].to_owned()),
            s if s.len() > 1 && s[1..].bytes().all(|b| b.is_ascii_digit()) => {
                f.count = Some(s[1..].parse().map_err(|_| usage())?);
            }
            _ => return Err(usage()),
        }
        i += 1;
    }
    Ok(f)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let f = parse(&a.rest)?;
    let entries = select(ctx, ws, &f).await?;
    if entries.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let total = entries.len();
    let numbered = match f.numbered {
        Some(n) => n,
        None => match ctx.config.get("format.numbered").as_deref() {
            Some(v) if v.eq_ignore_ascii_case("auto") => total > 1,
            Some(_) => ctx.config.bool("format.numbered").unwrap_or(total > 1),
            None => total > 1,
        },
    } && !f.keep;
    let prefix = f
        .prefix
        .clone()
        .or_else(|| ctx.config.get("format.subjectPrefix"))
        .unwrap_or_else(|| "PATCH".into());
    let signature = match &f.signature {
        Some(s) => s.clone(),
        None => match ctx.config.get("format.signature") {
            Some(s) if !s.is_empty() => Some(s),
            Some(_) => None,
            None => Some(git_version(ctx)?),
        },
    };
    let dir = f
        .out_dir
        .clone()
        .or_else(|| ctx.config.get("format.outputDirectory"));
    if !f.stdout
        && let Some(d) = &dir
    {
        std::fs::create_dir_all(ctx.cwd.join(d))
            .map_err(|e| CliError::Fatal(format!("could not create directory '{d}': {e}")))?;
    }

    let abbrev = abbrev_len(ctx);
    let mut opts = DiffOptions {
        renames: Some(RenameOptions::default()),
        abbrev,
        ..DiffOptions::default()
    };
    for key in [
        "diff.algorithm",
        "diff.indentHeuristic",
        "diff.context",
        "diff.interHunkContext",
        "diff.renameLimit",
        "diff.renames",
        "core.abbrev",
    ] {
        if let Some(v) = ctx.config.get(key) {
            opts.apply_config(key, &v).map_err(|e| {
                CliError::Fatal(format!(
                    "bad config variable '{}' value '{}'",
                    e.key, e.value
                ))
            })?;
        }
    }
    opts.abbrev = abbrev;
    opts.stat_width = Some(STAT_WIDTH);

    let width = total.to_string().len();
    let mut stdout = std::io::stdout().lock();
    for (n, e) in entries.iter().enumerate() {
        let number = f.start + n;
        let (subject, body) = split_message(&e.cs.message);
        let mut head = String::new();
        if !f.keep {
            head.push('[');
            head.push_str(&prefix);
            if numbered {
                head.push_str(&format!(
                    " {:0width$}/{}",
                    number,
                    f.start + total - 1,
                    width = width
                ));
            }
            head.push_str("] ");
        }
        let text = mail(ws, e, &head, &subject, &body, &opts, signature.as_deref()).await?;
        if f.stdout {
            if n > 0 {
                stdout.write_all(b"\n")?;
            }
            stdout.write_all(&text)?;
            continue;
        }
        let name = file_name(number, &subject);
        let rel = match &dir {
            Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
            None => name,
        };
        let path: PathBuf = ctx.cwd.join(&rel);
        std::fs::write(&path, &text).map_err(|e| {
            CliError::Fatal(format!(
                "failed to create output files: {}: {e}",
                path.display()
            ))
        })?;
        if !f.quiet {
            writeln!(stdout, "{rel}")?;
        }
    }
    stdout.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// The commits to write, oldest first.
async fn select(ctx: &Ctx, ws: &WsCtx, f: &Flags) -> Result<Vec<Entry>> {
    let mut args: Vec<OsString> = vec!["--no-merges".into()];
    let words = &f.words;
    let range = words.iter().any(|w| {
        let w = w.to_string_lossy();
        w.contains("..") || w.starts_with('^')
    });
    match (words.len(), range) {
        (0, _) => match f.count {
            Some(n) => {
                args.push(format!("-{n}").into());
                args.push("HEAD".into());
            }
            // git finds no upstream to start from and writes nothing.
            None => return Ok(Vec::new()),
        },
        (1, false) if f.count.is_none() && !f.root => {
            args.push(format!("^{}", words[0].to_string_lossy()).into());
            args.push("HEAD".into());
        }
        _ => {
            if let Some(n) = f.count {
                args.push(format!("-{n}").into());
            }
            args.extend(words.iter().cloned());
        }
    }
    let o = Opts::parse(&args)?;
    let hist = Hist::select(ws);
    let sel = revs::select(ctx, hist.api(ws), &o).await?;
    let mut walker = Walker::new(ws, &hist, &sel, &o).await?;
    let mut all: Vec<Entry> = Vec::new();
    while let Some(e) = walker.next().await? {
        if e.cs.parents.is_empty() && !f.root {
            continue;
        }
        all.push(e);
        if all.len() > MAX_PATCHES {
            return Err(CliError::Fatal("too many patches".into()));
        }
    }
    all.reverse();
    Ok(all)
}

/// The real git's version string, which git prints under the `-- ` line.
fn git_version(ctx: &Ctx) -> Result<String> {
    let git = ctx.real_git()?;
    let out = git
        .command()
        .map_err(|e| CliError::Fatal(e.to_string()))?
        .arg("--version")
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .trim()
        .strip_prefix("git version ")
        .unwrap_or("")
        .to_owned())
}

/// `(subject, body)`: the first paragraph joined by spaces, and what follows it.
fn split_message(message: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lines: Vec<&[u8]> = message.split(|&b| b == b'\n').collect();
    let mut i = 0;
    while i < lines.len() && is_blank(lines[i]) {
        i += 1;
    }
    let mut subject: Vec<u8> = Vec::new();
    while i < lines.len() && !is_blank(lines[i]) {
        if !subject.is_empty() {
            subject.push(b' ');
        }
        subject.extend_from_slice(lines[i].trim_ascii_end());
        i += 1;
    }
    while i < lines.len() && is_blank(lines[i]) {
        i += 1;
    }
    let mut body: Vec<u8> = Vec::new();
    let mut end = lines.len();
    while end > i && is_blank(lines[end - 1]) {
        end -= 1;
    }
    for l in &lines[i..end] {
        body.extend_from_slice(l);
        body.push(b'\n');
    }
    (subject, body)
}

fn is_blank(l: &[u8]) -> bool {
    l.iter().all(u8::is_ascii_whitespace)
}

/// git's `format_sanitized_subject`, cut to the longest name part.
fn file_name(number: usize, subject: &[u8]) -> String {
    let mut s = String::new();
    let mut space = 2;
    let mut keep = 0;
    let mut i = 0;
    while i < subject.len() {
        let c = subject[i];
        if c.is_ascii_alphanumeric() || c == b'.' || c == b'_' {
            if space == 1 {
                s.push('-');
            }
            space = 0;
            s.push(c as char);
            if c == b'.' {
                while subject.get(i + 1) == Some(&b'.') {
                    i += 1;
                }
            }
            keep = s.len();
        } else {
            space |= 1;
        }
        i += 1;
    }
    s.truncate(keep);
    while s.ends_with(['.', '-']) {
        s.pop();
    }
    let mut name = format!("{number:04}-{s}");
    let suffix = ".patch";
    let max = NAME_MAX - suffix.len();
    if name.len() > max {
        name.truncate(max);
    }
    name.push_str(suffix);
    name
}

fn has_non_ascii(b: &[u8]) -> bool {
    b.iter().any(|&c| c >= 0x80)
}

/// git's `add_rfc2047` for a subject (`phrase` false) or a name (`phrase` true).
fn rfc2047(text: &[u8], phrase: bool, col: usize) -> Vec<u8> {
    let needs = text
        .iter()
        .enumerate()
        .any(|(i, &c)| c >= 0x80 || (c == b'=' && text.get(i + 1) == Some(&b'?')));
    if !needs {
        return text.to_vec();
    }
    let special = |c: u8| {
        if c >= 0x80 || matches!(c, b'=' | b'?' | b'_' | b' ') || c < 0x20 {
            return true;
        }
        phrase && !(c.is_ascii_alphanumeric() || matches!(c, b'!' | b'*' | b'+' | b'-' | b'/'))
    };
    let mut out = Vec::new();
    let mut line = col;
    out.extend_from_slice(b"=?UTF-8?q?");
    line += 10;
    for &c in text.iter() {
        let enc = special(c);
        let w = if enc { 3 } else { 1 };
        // Close the word before it would pass 75 columns; the next one follows on a new line.
        if line + w + 2 > 76 {
            out.extend_from_slice(b"?=\n =?UTF-8?q?");
            line = 1 + 10;
        }
        if enc {
            out.extend_from_slice(format!("={c:02X}").as_bytes());
        } else {
            out.push(c);
        }
        line += w;
    }
    out.extend_from_slice(b"?=");
    out
}

/// `Subject:` text after the `[PATCH]` head: ASCII is folded at [`SUBJECT_WIDTH`] columns.
fn fold_subject(head: &str, subject: &[u8]) -> Vec<u8> {
    let col0 = "Subject: ".len() + head.len();
    if has_non_ascii(subject) || subject.windows(2).any(|w| w == b"=?") {
        return rfc2047(subject, false, col0);
    }
    let mut out = Vec::new();
    let mut col = col0;
    let mut first = true;
    for word in subject.split(|&b| b == b' ').filter(|w| !w.is_empty()) {
        if first {
            first = false;
        } else if col + 1 + word.len() > SUBJECT_WIDTH {
            out.extend_from_slice(b"\n ");
            col = 1;
        } else {
            out.push(b' ');
            col += 1;
        }
        out.extend_from_slice(word);
        col += word.len();
    }
    out
}

/// The `From:` header value for an author.
fn address(sig: &Signature) -> Vec<u8> {
    let name = sig.name();
    let mut out = Vec::new();
    if name.is_empty() {
        // git prints just the address
    } else if has_non_ascii(name) {
        out.extend(rfc2047(name, true, "From: ".len()));
        out.push(b' ');
    } else if name.iter().any(|c| b"\"(),.:;<>@[\\]".contains(c)) {
        out.push(b'"');
        for &c in name {
            if c == b'"' || c == b'\\' {
                out.push(b'\\');
            }
            out.push(c);
        }
        out.extend_from_slice(b"\" ");
    } else {
        out.extend_from_slice(name);
        out.push(b' ');
    }
    out.push(b'<');
    out.extend_from_slice(sig.email());
    out.push(b'>');
    out
}

async fn parent_root(ws: &WsCtx, cs: &Changeset) -> Result<TreeId> {
    match cs.parents.first() {
        Some(&p) => Ok(ws.engine.get_changeset(p).await.map_err(fatal)?.root),
        None => Ok(CliEngine::empty_tree_id()),
    }
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// One message.
async fn mail(
    ws: &WsCtx,
    e: &Entry,
    head: &str,
    subject: &[u8],
    body: &[u8],
    opts: &DiffOptions,
    signature: Option<&str>,
) -> Result<Vec<u8>> {
    let cs = &e.cs;
    let mut out: Vec<u8> = Vec::new();
    writeln!(out, "From {} Mon Sep 17 00:00:00 2001", e.git)?;
    out.extend_from_slice(b"From: ");
    out.extend(address(&cs.author));
    out.push(b'\n');
    let date = Mode {
        kind: super::format::date::Kind::Rfc2822,
        local: false,
    }
    .render(cs.author.time(), cs.author.tz_min(), cs.author.time());
    writeln!(out, "Date: {date}")?;
    out.extend_from_slice(b"Subject: ");
    out.extend_from_slice(head.as_bytes());
    out.extend(fold_subject(head, subject));
    out.push(b'\n');
    if has_non_ascii(&cs.message) {
        out.extend_from_slice(
            b"MIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\n",
        );
    }
    out.push(b'\n');
    out.extend_from_slice(body);

    let old = parent_root(ws, cs).await?;
    let changes = ws
        .engine
        .diff_trees(old, cs.root, None, opts)
        .await
        .map_err(fatal)?;
    if !changes.is_empty() {
        let dctx = DiffCtx {
            opts,
            content: &ws.engine,
            attrs: &NoAttributes,
        };
        out.extend_from_slice(b"---\n");
        write_stat(&mut out, &dctx, &changes).await.map_err(fatal)?;
        write_summary(&mut out, &changes, opts).map_err(fatal)?;
        out.push(b'\n');
        write_patch(&mut out, &dctx, &changes)
            .await
            .map_err(fatal)?;
    }
    if let Some(sig) = signature {
        out.extend_from_slice(b"-- \n");
        out.extend_from_slice(sig.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_follow_the_sanitised_subject() {
        assert_eq!(file_name(1, b"Change b to B"), "0001-Change-b-to-B.patch");
        assert_eq!(file_name(12, b"fix: a/b ... c.."), "0012-fix-a-b-.-c.patch");
        let long = "x".repeat(100);
        assert_eq!(file_name(1, long.as_bytes()).len(), NAME_MAX);
    }

    #[test]
    fn messages_split_into_subject_and_body() {
        let (s, b) = split_message(b"one\ntwo\n\nbody1\n\nbody2\n\n\n");
        assert_eq!(s, b"one two");
        assert_eq!(b, b"body1\n\nbody2\n");
        let (s, b) = split_message(b"only\n");
        assert_eq!((s.as_slice(), b.as_slice()), (&b"only"[..], &b""[..]));
    }

    #[test]
    fn subjects_fold_at_78_columns_and_encode_non_ascii() {
        let words = vec!["ab"; 40].join(" ");
        let folded = fold_subject("[PATCH] ", words.as_bytes());
        let first = folded.split(|&b| b == b'\n').next().unwrap();
        assert!("Subject: [PATCH] ".len() + first.len() <= 78);
        assert!(folded.contains(&b'\n'));
        assert_eq!(
            fold_subject("[PATCH] ", "na\u{ef}ve".as_bytes()),
            b"=?UTF-8?q?na=C3=AFve?="
        );
    }

    #[test]
    fn numbers_and_flags_parse() {
        let args: Vec<OsString> = [
            "-3",
            "--stdout",
            "-o",
            "d",
            "--subject-prefix=RFC",
            "HEAD~5",
        ]
        .iter()
        .map(Into::into)
        .collect();
        let f = parse(&args).unwrap();
        assert_eq!(
            (f.count, f.stdout, f.out_dir.as_deref(), f.prefix.as_deref()),
            (Some(3), true, Some("d"), Some("RFC"))
        );
        assert_eq!(f.words, vec![OsString::from("HEAD~5")]);
    }
}
