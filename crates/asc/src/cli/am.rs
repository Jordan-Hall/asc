//! `am` (PLAN U27b): applies the messages of an mbox as commits. Each message becomes a commit
//! whose author, date and message come from the mail, on top of HEAD; the patch is applied to
//! the work tree and the staged tree as `git apply --index` does. State lives in git's
//! `rebase-apply/` directory with the files git writes (`0001`.., `next`, `last`, `patch`,
//! `msg`, `final-commit`, `author-script`, `info`, `abort-safety`, ...), so `--continue`,
//! `--skip` and `--abort` work from it. Hooks: `applypatch-msg`, `pre-applypatch`,
//! `post-applypatch`.
//!
//! Not served: `-3`, `-s`, `-i`, `-k`, `-b`, `-u`, `--scissors`, `--message-id`, `--reject`,
//! Maildir and other mail formats, MIME bodies (quoted-printable or base64) and `--show-current-patch`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use asc_core::id::ChangesetId;
use asc_core::sig::Signature;
use asc_engine::diff::apply::{self, ParseOptions};
use asc_state::state::stage::{self, StageSet};
use asc_state::state::tables::ws_ops;

use super::add;
use super::apply::{Settings, apply_patches};
use super::commit::ident::{self, parse_date};
use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict::{self, NewCommit};
use super::restore::{index_tree, root_of, sync_worktree};
use super::seqstate;
use crate::hooks_runner::{Hook, HookEnv, Outcome, Runner};
use crate::native::NativeSpec;
use asc_engine::diff::apply::Work;

#[derive(clap::Parser, Debug)]
#[command(name = "am", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(long = "continue", short = 'r', alias = "resolved")]
    pub cont: bool,
    #[arg(long = "skip")]
    pub skip: bool,
    #[arg(long = "abort")]
    pub abort: bool,
    #[arg(long = "quit")]
    pub quit: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,
    #[arg(long = "ignore-date")]
    pub ignore_date: bool,
    #[arg(long = "committer-date-is-author-date")]
    pub committer_date: bool,
    #[arg(short = 'p')]
    pub strip: Option<usize>,
    pub mbox: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "am",
    flags: &[
        "--continue",
        "-r",
        "--resolved",
        "--skip",
        "--abort",
        "--quit",
        "-q",
        "--quiet",
        "-n",
        "--no-verify",
        "--ignore-date",
        "--committer-date-is-author-date",
        "-p",
    ],
    placeholders: &[],
}];

/// Most mails one run takes, and the largest mbox.
const MAX_MAILS: usize = 100_000;
const MAX_MBOX_BYTES: u64 = 1 << 30;
/// Most dirty paths named in the dirty-index error.
const MAX_LISTED: usize = 100;

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// The settings `am` keeps for the run (the state files `quiet`, `apply-opt`, ...).
#[derive(Clone, Debug, Default)]
struct Run {
    quiet: bool,
    no_verify: bool,
    ignore_date: bool,
    committer_date: bool,
    strip: Option<usize>,
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let gitdir = ws.projection.git_dir().to_owned();
    let dir = gitdir.join("rebase-apply");
    let modes = [a.cont, a.skip, a.abort, a.quit]
        .iter()
        .filter(|m| **m)
        .count();
    if modes > 1 {
        return Err(CliError::Usage(
            "error: options '--continue', '--skip', '--abort' and '--quit' cannot be used together"
                .into(),
        ));
    }
    let in_progress = dir.join("last").is_file() && dir.join("next").is_file();
    if modes == 1 {
        if !in_progress {
            return Err(fatal(
                "Resolve operation not in progress, we are not resuming.",
            ));
        }
        let run = load_run(&dir);
        return if a.cont {
            resume(ctx, ws, &gitdir, &dir, &run).await
        } else if a.skip {
            skip(ctx, ws, &gitdir, &dir, &run).await
        } else if a.abort {
            abort(ctx, ws, &gitdir, &dir).await
        } else {
            let _ = fs::remove_dir_all(&dir);
            Ok(ExitCode::SUCCESS)
        };
    }
    if in_progress || dir.exists() {
        return Err(fatal(format!(
            "previous rebase directory {} still exists but mbox given.",
            display_dir(ctx, &dir)
        )));
    }
    let run = Run {
        quiet: a.quiet,
        no_verify: a.no_verify,
        ignore_date: a.ignore_date,
        committer_date: a.committer_date,
        strip: a.strip,
    };
    let mails = read_mails(ctx, &a.mbox)?;
    if mails.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let head = ws.head_commit().await?;
    fs::create_dir_all(&dir)?;
    let setup = write_setup(&gitdir, &dir, &run, &mails, head, ws).await;
    if let Err(e) = setup {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    // git sets the session up before it looks at the index, and leaves it in place.
    if let Err(e) = check_clean_index(ctx, ws).await {
        seqstate::write(&dir, "dirtyindex", b"t\n")?;
        seqstate::write(&dir, "applying", b"")?;
        return Err(e);
    }
    drive(ctx, ws, &gitdir, &dir, &run, true).await
}

/// The directory as git shows it: `.git/rebase-apply`, whatever the gitdir of a workspace is.
fn display_dir(_ctx: &Ctx, _dir: &Path) -> String {
    ".git/rebase-apply".to_owned()
}

// ------------------------------------------------------------------ reading mails

/// The mails of the inputs, in order. Standard input when none is named.
fn read_mails(ctx: &Ctx, inputs: &[OsString]) -> Result<Vec<Vec<u8>>> {
    let mut all: Vec<Vec<u8>> = Vec::new();
    let mut inputs = inputs.to_vec();
    if inputs.is_empty() {
        inputs.push(OsString::from("-"));
    }
    for item in &inputs {
        let mut bytes = Vec::new();
        if item == "-" {
            std::io::stdin()
                .lock()
                .take(MAX_MBOX_BYTES + 1)
                .read_to_end(&mut bytes)?;
        } else {
            let path = ctx.cwd.join(item);
            let f = fs::File::open(&path).map_err(|e| {
                fatal(format!(
                    "could not open '{}' for reading: {}",
                    item.to_string_lossy(),
                    describe(&e)
                ))
            })?;
            f.take(MAX_MBOX_BYTES + 1).read_to_end(&mut bytes)?;
        }
        if bytes.len() as u64 > MAX_MBOX_BYTES {
            return Err(fatal("mailbox is too large"));
        }
        if bytes.is_empty() {
            continue;
        }
        let mut mails = split_mbox(&bytes)
            .ok_or_else(|| CliError::Raw("Patch format detection failed.".into(), 128))?;
        all.append(&mut mails);
        if all.len() > MAX_MAILS {
            return Err(fatal("too many patches"));
        }
    }
    Ok(all)
}

fn describe(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".into(),
        _ => e.to_string(),
    }
}

/// A line that starts a message of an mbox: `From <word> <rest with a time>`.
fn is_from_line(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"From ") else {
        return false;
    };
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    let Some(sp) = rest.iter().position(|&b| b == b' ') else {
        return false;
    };
    sp > 0 && rest[sp..].contains(&b':') && rest[sp..].iter().any(u8::is_ascii_digit)
}

/// Splits an mbox into messages; a single message without a `From ` line is one mail. `None`
/// when the input is neither.
fn split_mbox(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let first = data.split(|&b| b == b'\n').next()?;
    let starts_mbox = is_from_line(first);
    if !starts_mbox {
        let looks_like_mail = [
            &b"From:"[..],
            b"Subject:",
            b"Date:",
            b"Message-ID:",
            b"Message-Id:",
        ]
        .iter()
        .any(|h| first.starts_with(h));
        return looks_like_mail.then(|| vec![data.to_vec()]);
    }
    let mut mails: Vec<Vec<u8>> = Vec::new();
    let mut start = 0;
    let mut pos = 0;
    let mut prev_blank = true;
    while pos < data.len() {
        let end = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(data.len(), |i| pos + i + 1);
        let line = &data[pos..end];
        if pos > start && prev_blank && is_from_line(line) {
            mails.push(data[start..pos].to_vec());
            start = pos;
        }
        prev_blank = line == b"\n" || line == b"\r\n";
        pos = end;
    }
    if start < data.len() {
        mails.push(data[start..].to_vec());
    }
    Some(mails)
}

// ------------------------------------------------------------------ parsing a mail

/// What `mailinfo` takes from one message.
#[derive(Debug, Default, PartialEq, Eq)]
struct Mail {
    name: String,
    email: String,
    date: String,
    subject: String,
    /// The text between the headers and the patch, blank lines at both ends removed.
    body: Vec<u8>,
    /// From the line that starts the patch to the end.
    patch: Vec<u8>,
}

fn lines_of(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut s = 0;
    while s < data.len() {
        let e = data[s..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(data.len(), |i| s + i + 1);
        out.push(&data[s..e]);
        s = e;
    }
    out
}

fn trim_nl(l: &[u8]) -> &[u8] {
    let l = l.strip_suffix(b"\n").unwrap_or(l);
    l.strip_suffix(b"\r").unwrap_or(l)
}

/// Whether a body line starts the patch (`mailinfo`'s `patchbreak`).
fn patch_break(line: &[u8]) -> bool {
    if line.starts_with(b"diff -") || line.starts_with(b"Index: ") {
        return true;
    }
    if !line.starts_with(b"---") {
        return false;
    }
    // `---` followed by white space only is the separator; `--- <file>` starts a bare diff.
    let rest = &line[3..];
    rest.iter().all(u8::is_ascii_whitespace) || rest.first() == Some(&b' ')
}

fn parse_mail(raw: &[u8]) -> Mail {
    let lines = lines_of(raw);
    let mut i = 0;
    if lines.first().is_some_and(|l| is_from_line(l)) {
        i = 1;
    }
    // Headers with their continuation lines folded in.
    let mut headers: Vec<(String, String)> = Vec::new();
    while i < lines.len() {
        let l = trim_nl(lines[i]);
        if l.is_empty() {
            i += 1;
            break;
        }
        if (l[0] == b' ' || l[0] == b'\t')
            && let Some(last) = headers.last_mut()
        {
            last.1.push_str(&String::from_utf8_lossy(l));
        } else if let Some(c) = l.iter().position(|&b| b == b':') {
            headers.push((
                String::from_utf8_lossy(&l[..c]).to_ascii_lowercase(),
                String::from_utf8_lossy(&l[c + 1..]).trim_start().to_owned(),
            ));
        }
        i += 1;
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let (name, email) = split_from(&decode_words(&get("from").unwrap_or_default()));
    let date = get("date").unwrap_or_default().trim().to_owned();
    let subject = clean_subject(&decode_words(&get("subject").unwrap_or_default()));
    // Body, up to the patch.
    let mut body: Vec<&[u8]> = Vec::new();
    let mut patch = Vec::new();
    while i < lines.len() {
        if patch_break(lines[i]) {
            for l in &lines[i..] {
                patch.extend_from_slice(l);
            }
            break;
        }
        body.push(lines[i]);
        i += 1;
    }
    while body
        .last()
        .is_some_and(|l| trim_nl(l).iter().all(u8::is_ascii_whitespace))
    {
        body.pop();
    }
    let skip = body
        .iter()
        .take_while(|l| trim_nl(l).iter().all(u8::is_ascii_whitespace))
        .count();
    let body = body[skip..].concat();
    Mail {
        name,
        email,
        date,
        subject,
        body,
        patch,
    }
}

/// `Name <email>`, `<email>`, `"Quoted Name" <email>` or `email (Name)`.
fn split_from(value: &str) -> (String, String) {
    let value = value.trim();
    if let (Some(lt), Some(gt)) = (value.rfind('<'), value.rfind('>'))
        && lt < gt
    {
        let mut name = value[..lt].trim().to_owned();
        if name.len() >= 2 && name.starts_with('"') && name.ends_with('"') {
            let inner = &name[1..name.len() - 1];
            let mut out = String::new();
            let mut esc = false;
            for c in inner.chars() {
                if esc {
                    out.push(c);
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else {
                    out.push(c);
                }
            }
            name = out;
        }
        return (name, value[lt + 1..gt].trim().to_owned());
    }
    if let (Some(lp), Some(rp)) = (value.find('('), value.rfind(')'))
        && lp < rp
    {
        return (
            value[lp + 1..rp].trim().to_owned(),
            value[..lp].trim().to_owned(),
        );
    }
    (String::new(), value.to_owned())
}

/// `Re:` and leading `[...]` groups are not part of the commit subject.
fn clean_subject(s: &str) -> String {
    let mut s = s.trim();
    loop {
        let before = s;
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("re:") {
            s = s[3..].trim_start();
        } else if s.starts_with('[')
            && let Some(close) = s.find(']')
        {
            s = s[close + 1..].trim_start();
        }
        if s == before {
            break;
        }
    }
    s.trim().to_owned()
}

/// Decodes `=?UTF-8?q?..?=` and `=?UTF-8?b?..?=` words; text outside them is kept.
fn decode_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    let mut after_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, tail) = rest.split_at(start);
        let Some(end) = tail[2..].find("?=") else {
            break;
        };
        let word = &tail[2..2 + end];
        let pieces: Vec<&str> = word.splitn(3, '?').collect();
        let Some(bytes) = (pieces.len() == 3)
            .then(|| match pieces[1].to_ascii_lowercase().as_str() {
                "q" => Some(decode_q(pieces[2])),
                "b" => decode_b64(pieces[2]),
                _ => None,
            })
            .flatten()
        else {
            out.push_str(before);
            out.push_str("=?");
            rest = &tail[2..];
            after_word = false;
            continue;
        };
        // White space between two encoded words is dropped.
        if !(after_word && before.trim().is_empty()) {
            out.push_str(before);
        }
        out.push_str(&String::from_utf8_lossy(&bytes));
        rest = &tail[2 + end + 2..];
        after_word = true;
    }
    out.push_str(rest);
    out
}

fn decode_q(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 2;
                }
                Err(_) => out.push(b'='),
            },
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The commit message: the subject, then the body after a blank line.
fn commit_message(mail: &Mail) -> Vec<u8> {
    let mut m = mail.subject.clone().into_bytes();
    m.push(b'\n');
    if !mail.body.is_empty() {
        m.push(b'\n');
        m.extend_from_slice(&mail.body);
        if !m.ends_with(b"\n") {
            m.push(b'\n');
        }
    }
    m
}

// ------------------------------------------------------------------ state

fn flag(v: bool) -> &'static [u8] {
    if v { b"t\n" } else { b"f\n" }
}

async fn write_setup(
    gitdir: &Path,
    dir: &Path,
    run: &Run,
    mails: &[Vec<u8>],
    head: Option<ChangesetId>,
    ws: &WsCtx,
) -> Result<()> {
    for (n, m) in mails.iter().enumerate() {
        seqstate::write(dir, &format!("{:04}", n + 1), m)?;
    }
    seqstate::write(dir, "last", format!("{}\n", mails.len()).as_bytes())?;
    seqstate::write(dir, "next", b"1\n")?;
    let head_git = match head {
        Some(h) => conflict::git_id(ws, h).await?.to_string(),
        None => String::new(),
    };
    seqstate::write(dir, "abort-safety", format!("{head_git}\n").as_bytes())?;
    let opt = run.strip.map(|p| format!("-p{p}\n")).unwrap_or_default();
    seqstate::write(dir, "apply-opt", opt.as_bytes())?;
    seqstate::write(dir, "quiet", flag(run.quiet))?;
    seqstate::write(dir, "sign", flag(false))?;
    seqstate::write(dir, "utf8", flag(true))?;
    seqstate::write(dir, "keep", flag(false))?;
    seqstate::write(dir, "messageid", flag(false))?;
    seqstate::write(dir, "scissors", b"")?;
    seqstate::write(dir, "quoted-cr", b"")?;
    seqstate::write(dir, "threeway", flag(false))?;
    if run.no_verify {
        seqstate::write(dir, "no-verify", b"")?;
    }
    if run.ignore_date {
        seqstate::write(dir, "ignore-date", b"")?;
    }
    if run.committer_date {
        seqstate::write(dir, "committer-date-is-author-date", b"")?;
    }
    if let Some(h) = head {
        seqstate::write_orig_head(gitdir, conflict::git_id(ws, h).await?)?;
    }
    Ok(())
}

fn load_run(dir: &Path) -> Run {
    let flag_of = |name: &str| fs::read(dir.join(name)).is_ok_and(|b| b.starts_with(b"t"));
    let strip = fs::read_to_string(dir.join("apply-opt"))
        .ok()
        .and_then(|t| t.trim().strip_prefix("-p").and_then(|n| n.parse().ok()));
    Run {
        quiet: flag_of("quiet"),
        no_verify: dir.join("no-verify").is_file(),
        ignore_date: dir.join("ignore-date").is_file(),
        committer_date: dir.join("committer-date-is-author-date").is_file(),
        strip,
    }
}

fn read_number(dir: &Path, name: &str) -> Result<usize> {
    let bytes = seqstate::read(dir, name)?.unwrap_or_default();
    String::from_utf8_lossy(&bytes)
        .trim()
        .parse()
        .map_err(|_| fatal(format!("rebase-apply/{name} does not hold a number")))
}

/// Refuses to start on staged changes.
async fn check_clean_index(ctx: &Ctx, ws: &WsCtx) -> Result<()> {
    let _ = ctx;
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let (tree, _) = index_tree(ws, head_root).await?;
    if tree == head_root {
        return Ok(());
    }
    let mut names: Vec<String> = ws
        .engine
        .leafdiff(head_root, tree, None)
        .await?
        .into_iter()
        .take(MAX_LISTED)
        .map(|(p, _)| p.to_string())
        .collect();
    names.sort();
    Err(fatal(format!(
        "Dirty index: cannot apply patches (dirty: {})",
        names.join(" ")
    )))
}

// ------------------------------------------------------------------ the loop

/// The hint block git prints when a patch stops the run.
fn resolve_hints(ctx: &Ctx, with_patch_hint: bool) {
    let mut lines: Vec<&str> = Vec::new();
    if with_patch_hint {
        lines.push("Use 'git am --show-current-patch=diff' to see the failed patch");
    }
    lines.push("When you have resolved this problem, run \"git am --continue\".");
    lines.push("If you prefer to skip this patch, run \"git am --skip\" instead.");
    lines.push("To restore the original branch and stop patching, run \"git am --abort\".");
    add::print_hints(ctx, "mergeConflict", &lines);
}

fn first_line(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Applies the mails from `next` to `last`. `fresh` is true when the first mail has not been
/// parsed yet.
async fn drive(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &Path,
    dir: &Path,
    run: &Run,
    fresh: bool,
) -> Result<ExitCode> {
    let _ = fresh;
    // One budget for every patch of the mailbox.
    let work = Work::default();
    let last = read_number(dir, "last")?;
    loop {
        let next = read_number(dir, "next")?;
        if next > last {
            break;
        }
        let raw = seqstate::read(dir, &format!("{next:04}"))?
            .ok_or_else(|| fatal(format!("rebase-apply/{next:04} is missing")))?;
        let mail = parse_mail(&raw);
        // The per-patch files.
        let info = format!(
            "Author: {}\nEmail: {}\nSubject: {}\nDate: {}\n\n",
            mail.name, mail.email, mail.subject, mail.date
        );
        let author = format!(
            "GIT_AUTHOR_NAME={}\nGIT_AUTHOR_EMAIL={}\nGIT_AUTHOR_DATE={}\n",
            sq(&mail.name),
            sq(&mail.email),
            sq(&mail.date)
        );
        seqstate::write(dir, "info", info.as_bytes())?;
        seqstate::write(dir, "msg", &mail.body)?;
        seqstate::write(dir, "patch", &mail.patch)?;
        seqstate::write(dir, "author-script", author.as_bytes())?;
        seqstate::write(dir, "final-commit", &commit_message(&mail))?;
        let runner = Runner::for_ctx(ctx)?;
        let env = HookEnv::of(ctx, "am")
            .with_index(gitdir.join("index"))
            .without_editor();
        if !run.no_verify {
            let hook = Hook::ApplypatchMsg {
                file: dir.join("final-commit"),
            };
            if let Outcome::Exit(code) = runner.run(hook, &env).await?
                && code != 0
            {
                return Ok(ExitCode::from(1));
            }
        }
        let message = seqstate::read(dir, "final-commit")?.unwrap_or_default();
        let subject = first_line(&message);
        if !run.quiet {
            println!("Applying: {subject}");
        }
        seqstate::write(dir, "applying", b"")?;
        if mail.patch.is_empty() {
            println!("Patch is empty.");
            return stopped(ctx, &next, &subject, false);
        }
        let opts = ParseOptions {
            strip: run.strip.unwrap_or(1),
            recount: false,
        };
        let patches = match apply::parse(&mail.patch, &opts) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return stopped(ctx, &next, &subject, true);
            }
        };
        if patches.is_empty() {
            println!("Patch is empty.");
            return stopped(ctx, &next, &subject, false);
        }
        let settings = Settings {
            index: true,
            ..Settings::default()
        };
        if !apply_patches(ctx, ws, &patches, &settings, &work).await? {
            return stopped(ctx, &next, &subject, true);
        }
        match commit(ctx, ws, gitdir, dir, run).await? {
            Committed::Done => {}
            Committed::Rejected => return Ok(ExitCode::from(1)),
        }
        finish_one(dir, next)?;
    }
    let _ = fs::remove_dir_all(dir);
    Ok(ExitCode::SUCCESS)
}

/// A patch stopped the run: git's report, exit 128.
fn stopped(ctx: &Ctx, next: &usize, subject: &str, with_patch_hint: bool) -> Result<ExitCode> {
    println!("Patch failed at {next:04} {subject}");
    resolve_hints(ctx, with_patch_hint);
    Ok(ExitCode::from(128))
}

/// Forgets the files of the patch that was committed and moves on.
fn finish_one(dir: &Path, n: usize) -> Result<()> {
    for f in [
        "info",
        "msg",
        "patch",
        "author-script",
        "final-commit",
        "applying",
    ] {
        seqstate::remove(dir, f);
    }
    seqstate::remove(dir, &format!("{n:04}"));
    seqstate::write(dir, "next", format!("{}\n", n + 1).as_bytes())
}

enum Committed {
    Done,
    Rejected,
}

/// The author recorded in `author-script`.
fn read_author(ctx: &Ctx, dir: &Path, run: &Run) -> Result<Signature> {
    let text = String::from_utf8_lossy(&seqstate::read(dir, "author-script")?.unwrap_or_default())
        .into_owned();
    let get = |key: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(unsq)
            .unwrap_or_default()
    };
    let (name, email, date) = (
        get("GIT_AUTHOR_NAME"),
        get("GIT_AUTHOR_EMAIL"),
        get("GIT_AUTHOR_DATE"),
    );
    let me = ident::resolve(ctx, None, None, None)?;
    let name = if name.is_empty() {
        String::from_utf8_lossy(me.author.name()).into_owned()
    } else {
        name
    };
    let email = if email.is_empty() {
        String::from_utf8_lossy(me.author.email()).into_owned()
    } else {
        email
    };
    let (time, tz) = if run.ignore_date {
        let now = gix_date::Time::now_local_or_utc();
        (now.seconds, i16::try_from(now.offset / 60).unwrap_or(0))
    } else if date.is_empty() {
        (me.committer.time(), me.committer.tz_min())
    } else {
        let t = parse_date(&date)?;
        (t.seconds, i16::try_from(t.offset / 60).unwrap_or(0))
    };
    Signature::new(name.into_bytes(), email.into_bytes(), time, tz)
        .map_err(|e| fatal(e.to_string()))
}

fn unsq(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(r) = rest.strip_prefix('\'') {
        let end = r.find('\'').unwrap_or(r.len());
        out.push_str(&r[..end]);
        rest = &r[(end + 1).min(r.len())..];
        if let Some(r) = rest.strip_prefix("\\'") {
            out.push('\'');
            rest = r;
        }
    }
    out
}

/// Commits what is staged with the patch's author and message.
async fn commit(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, dir: &Path, run: &Run) -> Result<Committed> {
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let (root, unmerged) = index_tree(ws, head_root).await?;
    if !unmerged.is_empty() {
        return Err(fatal("asc: the index has unmerged paths"));
    }
    let runner = Runner::for_ctx(ctx)?;
    let env = HookEnv::of(ctx, "am")
        .with_index(gitdir.join("index"))
        .without_editor();
    if !run.no_verify
        && let Outcome::Exit(code) = runner.run(Hook::PreApplypatch, &env).await?
        && code != 0
    {
        return Ok(Committed::Rejected);
    }
    let message = seqstate::read(dir, "final-commit")?.unwrap_or_default();
    let author = read_author(ctx, dir, run)?;
    let mut idents = ident::resolve(ctx, None, None, None)?;
    if run.committer_date {
        idents.committer = Signature::new(
            idents.committer.name().to_vec(),
            idents.committer.email().to_vec(),
            author.time(),
            author.tz_min(),
        )
        .map_err(|e| fatal(e.to_string()))?;
    }
    let parents: Vec<ChangesetId> = head.into_iter().collect();
    let (id, _git) = conflict::write_commit(
        ws,
        NewCommit {
            parents,
            author,
            committer: idents.committer.clone(),
            message: message.clone(),
            root,
        },
    )
    .await?;
    let subject = first_line(&message);
    let op = conflict::move_head(
        ctx,
        ws,
        head,
        id,
        &format!("am: {subject}"),
        StagingAfterMove::Set(Vec::new()),
        Some(&idents.committer),
    )
    .await?;
    conflict::announce(ws, id, op).await?;
    ws_ops::clear(&ws.db, ws.workspace.ws.id).await?;
    let _ = runner.run(Hook::PostApplypatch, &env).await?;
    Ok(Committed::Done)
}

// ------------------------------------------------------------------ resume, skip, abort

async fn resume(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, dir: &Path, run: &Run) -> Result<ExitCode> {
    let next = read_number(dir, "next")?;
    let message = seqstate::read(dir, "final-commit")?;
    let Some(message) = message else {
        // Between patches: nothing to commit, go on.
        return drive(ctx, ws, gitdir, dir, run, false).await;
    };
    let subject = first_line(&message);
    println!("Applying: {subject}");
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let (tree, _) = index_tree(ws, head_root).await?;
    if tree == head_root {
        println!(
            "No changes - did you forget to use 'git add'?\nIf there is nothing left to stage, chances are that something else\nalready introduced the same changes; you might want to skip this patch."
        );
        resolve_hints(ctx, false);
        return Ok(ExitCode::from(128));
    }
    match commit(ctx, ws, gitdir, dir, run).await? {
        Committed::Done => {}
        Committed::Rejected => return Ok(ExitCode::from(1)),
    }
    finish_one(dir, next)?;
    drive(ctx, ws, gitdir, dir, run, false).await
}

/// Resets the index and the paths the stopped patch touched to `target`.
async fn reset_to(ctx: &Ctx, ws: &WsCtx, target: Option<ChangesetId>) -> Result<()> {
    let target_root = root_of(ws, target).await?;
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let head_root = root_of(ws, ws.head_commit().await?).await?;
    let mut paths: BTreeSet<asc_core::path::RepoPath> = rows
        .iter()
        .filter_map(|r| asc_core::path::RepoPath::new(r.path.clone()).ok())
        .collect();
    paths.extend(
        ws.engine
            .leafdiff(head_root, target_root, None)
            .await?
            .into_iter()
            .map(|(p, _)| p),
    );
    let paths: Vec<_> = paths.into_iter().collect();
    sync_worktree(ctx, target_root, &paths, true).await?;
    stage::clear(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    crate::ext::index_sink()
        .on_stage_changed(ws.workspace.ws.id, &[])
        .await
        .map_err(conflict::fatal)
}

async fn skip(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, dir: &Path, run: &Run) -> Result<ExitCode> {
    crate::ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    reset_to(ctx, ws, head).await?;
    let next = read_number(dir, "next")?;
    finish_one(dir, next)?;
    drive(ctx, ws, gitdir, dir, run, false).await
}

async fn abort(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, dir: &Path) -> Result<ExitCode> {
    crate::ext::op_sink().snapshot_wc();
    let head = ws.head_commit().await?;
    let safety = seqstate::read(dir, "abort-safety")?
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .unwrap_or_default();
    let current = match head {
        Some(h) => conflict::git_id(ws, h).await?.to_string(),
        None => String::new(),
    };
    if current != safety {
        eprintln!("warning: You seem to have moved HEAD. Not rewinding, check your HEAD!");
        let _ = fs::remove_dir_all(dir);
        return Ok(ExitCode::SUCCESS);
    }
    match seqstate::read_oid(gitdir, "ORIG_HEAD")? {
        Some(orig) => {
            let original = conflict::changeset_of(ws, orig).await?;
            reset_to(ctx, ws, Some(original)).await?;
            if head != Some(original) {
                conflict::move_head(
                    ctx,
                    ws,
                    head,
                    original,
                    "am --abort",
                    StagingAfterMove::Set(Vec::new()),
                    None,
                )
                .await?;
            }
        }
        None => reset_to(ctx, ws, head).await?,
    }
    let _ = fs::remove_dir_all(dir);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIL: &[u8] = b"From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001\nFrom: A B <a@b.c>\nDate: Fri, 2 Jan 2026 00:00:00 +0000\nSubject: [PATCH 1/2] Re: fix the thing\n and more\n\nBody text.\n\n---\n f | 1 +\n\ndiff --git a/f b/f\n";

    #[test]
    fn a_mail_splits_into_headers_message_and_patch() {
        let m = parse_mail(MAIL);
        assert_eq!((m.name.as_str(), m.email.as_str()), ("A B", "a@b.c"));
        assert_eq!(m.subject, "fix the thing and more");
        assert_eq!(m.date, "Fri, 2 Jan 2026 00:00:00 +0000");
        assert_eq!(m.body, b"Body text.\n");
        assert!(m.patch.starts_with(b"---\n f | 1 +"));
        assert_eq!(
            commit_message(&m),
            b"fix the thing and more\n\nBody text.\n"
        );
    }

    #[test]
    fn mboxes_split_at_from_lines() {
        let mut two = MAIL.to_vec();
        // As in git's mailsplit, a `From ` line starts a message only after a blank line.
        two.extend_from_slice(b"\n");
        two.extend_from_slice(MAIL);
        assert_eq!(split_mbox(&two).unwrap().len(), 2);
        // Body text that looks like a `From ` line, with no blank line before it, does not split.
        let mut glued = MAIL.to_vec();
        glued.extend_from_slice(MAIL);
        assert_eq!(split_mbox(&glued).unwrap().len(), 1);
        assert!(split_mbox(b"just text\n").is_none());
        assert_eq!(split_mbox(b"From: x\nSubject: y\n\nz\n").unwrap().len(), 1);
    }

    #[test]
    fn encoded_words_and_quoting_decode() {
        assert_eq!(
            decode_words("=?UTF-8?q?na=C3=AFve_caf=C3=A9?="),
            "na\u{ef}ve caf\u{e9}"
        );
        assert_eq!(decode_words("=?UTF-8?b?bmHDr3Zl?= x"), "na\u{ef}ve x");
        assert_eq!(
            split_from("\"Last, First\" <a@b.c>"),
            ("Last, First".to_owned(), "a@b.c".to_owned())
        );
        assert_eq!(unsq(&sq("it's")), "it's");
    }

    #[test]
    fn patch_breaks_follow_mailinfo() {
        assert!(patch_break(b"---\n"));
        assert!(patch_break(b"--- a/f\n"));
        assert!(patch_break(b"diff --git a/f b/f\n"));
        assert!(!patch_break(b"----\n"));
        assert!(!patch_break(b"text\n"));
    }
}
