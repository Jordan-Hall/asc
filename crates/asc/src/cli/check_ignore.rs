//! `check-ignore` (SPEC §2.4.2, §11.5): which ignore rule decides a path, in git's output format.
//! `-v` names the `.ascignore`, `.gitignore`, exclude or global file and the line of the pattern.
//! The helpers at the top (quoting, relative names, the ignore sources) are shared with
//! `check-attr`, `grep` and `clean`.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use asc_wc::ignore::{Hit, IgnoreSources, IgnoreStack, shadow_warning};

use super::add::cwd_prefix;
use super::add::spec::{Spec, repo_path};
use super::add::target::Target;
use super::common::{CliError, Ctx, Result, WsCtx};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "check-ignore",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "check-ignore",
    flags: &[
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "-n",
        "--non-matching",
        "-z",
        "--stdin",
        "--no-index",
        "--index",
    ],
    placeholders: &[],
}];

/// Most bytes `--stdin` reads: a longer list is refused rather than held in memory.
pub const MAX_STDIN: u64 = 64 << 20;

/// Largest ignore file searched for the line of a pattern (the parser's own limit).
const MAX_IGNORE_FILE: u64 = 1 << 20;

const USAGE: &str = "usage: git check-ignore [<options>] <pathname>...\n   or: git check-ignore [<options>] --stdin\n\n    -q, --[no-]quiet      suppress progress reporting\n    -v, --[no-]verbose    be verbose\n\n    --[no-]stdin          read file names from stdin\n    -z                    terminate input and output records by a NUL character\n    -n, --[no-]non-matching\n                          show non-matching input paths\n    --no-index            ignore index when checking\n    --index               opposite of --no-index";

// ---------------------------------------------------------------- shared helpers

/// The bytes of an argument as typed.
pub fn os_bytes(s: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

/// git's `quote_c_style`: the quoted form of `name`, or `None` when it prints as is.
pub fn c_quote(name: &[u8], quote_high: bool) -> Option<Vec<u8>> {
    let special =
        |b: u8| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\' || (quote_high && b >= 0x80);
    if !name.iter().any(|&b| special(b)) {
        return None;
    }
    let mut out = vec![b'"'];
    for &b in name {
        match b {
            7 => out.extend_from_slice(b"\\a"),
            8 => out.extend_from_slice(b"\\b"),
            9 => out.extend_from_slice(b"\\t"),
            10 => out.extend_from_slice(b"\\n"),
            11 => out.extend_from_slice(b"\\v"),
            12 => out.extend_from_slice(b"\\f"),
            13 => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b if special(b) => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            b => out.push(b),
        }
    }
    out.push(b'"');
    Some(out)
}

/// Writes `name` quoted as git does (`quote_c_style`), without a terminator.
pub fn write_quoted(out: &mut dyn Write, name: &[u8], quote_high: bool) -> io::Result<()> {
    match c_quote(name, quote_high) {
        Some(q) => out.write_all(&q),
        None => out.write_all(name),
    }
}

/// git's `unquote_c_style` for a line that starts with `"`: the bytes between the quotes.
pub fn unquote_c(line: &[u8]) -> Option<Vec<u8>> {
    let body = line.strip_prefix(b"\"")?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b'"' => return (i + 1 == body.len()).then_some(out),
            b'\\' => {
                i += 1;
                let c = *body.get(i)?;
                match c {
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(10),
                    b'r' => out.push(13),
                    b't' => out.push(9),
                    b'v' => out.push(11),
                    b'\\' | b'"' => out.push(c),
                    b'0'..=b'3' => {
                        let digits = body.get(i..i + 3)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return None;
                        }
                        let v = digits
                            .iter()
                            .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                        out.push(u8::try_from(v).ok()?);
                        i += 2;
                    }
                    _ => return None,
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    None
}

/// git's `relative_path`: `name` below `prefix` (which ends in `/` or is empty) without it, else
/// with `../` steps.
pub fn relative(name: &[u8], prefix: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut common = 0;
    let mut i = 0;
    while i < name.len() && i < prefix.len() && name[i] == prefix[i] {
        if name[i] == b'/' {
            common = i + 1;
        }
        i += 1;
    }
    let ups = prefix[common..].iter().filter(|&&b| b == b'/').count();
    let mut out = b"../".repeat(ups);
    out.extend_from_slice(&name[common..]);
    out
}

/// The ignore sources of the workspace (SPEC §2.4.2): `asc.excludesFile` (default
/// `~/.config/asc/ignore`), git's `core.excludesFile` (default `$XDG_CONFIG_HOME/git/ignore`),
/// `.asc/info/exclude` and the common directory's `info/exclude`.
pub fn ignore_sources(ctx: &Ctx, ws: &WsCtx) -> IgnoreSources {
    let home = std::env::home_dir();
    let xdg = |sub: &str| {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        base.map(|b| b.join(sub))
    };
    let root = ws.projection.toplevel();
    IgnoreSources {
        asc_excludes: ctx
            .config
            .path("asc.excludesFile")
            .or_else(|| xdg("asc/ignore"))
            .unwrap_or_else(|| root.join(".asc/no-such-global-ignore")),
        core_excludes: ctx
            .config
            .path("core.excludesFile")
            .or_else(|| xdg("git/ignore")),
        local_exclude: root.join(".asc/info/exclude"),
        common_exclude: ws.projection.common_dir().join("info/exclude"),
    }
}

/// Reads all of stdin, at most [`MAX_STDIN`] bytes.
pub fn read_stdin() -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_STDIN + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_STDIN {
        return Err(CliError::Fatal(format!(
            "asc: --stdin input is larger than {} MiB",
            MAX_STDIN >> 20
        )));
    }
    Ok(buf)
}

/// The records of stdin: NUL-terminated with `-z`, else LF-terminated lines, a quoted line
/// unquoted. A final record needs no terminator.
pub fn stdin_records(buf: &[u8], nul: bool) -> Result<Vec<Vec<u8>>> {
    let sep = if nul { 0 } else { b'\n' };
    let mut out = Vec::new();
    let mut rest = buf;
    while !rest.is_empty() {
        let end = rest.iter().position(|&b| b == sep).unwrap_or(rest.len());
        let record = &rest[..end];
        rest = rest.get(end + 1..).unwrap_or(&[]);
        if !nul && record.first() == Some(&b'"') {
            let plain = unquote_c(record).ok_or_else(|| {
                CliError::Fatal(format!(
                    "line is badly quoted: {}",
                    String::from_utf8_lossy(record)
                ))
            })?;
            out.push(plain);
        } else {
            out.push(record.to_vec());
        }
    }
    Ok(out)
}

fn bad_option(name: &str, short: bool) -> CliError {
    let kind = if short { "switch" } else { "option" };
    CliError::Usage(format!("error: unknown {kind} `{name}'\n{USAGE}"))
}

// ---------------------------------------------------------------- the rule behind a hit

/// Where a pattern came from and which line it is, as `-v` prints them.
struct Source {
    file: String,
    line: usize,
    pattern: String,
}

/// The file name as git prints it: below the work tree root without it, else as is.
fn source_name(root: &Path, file: &Path) -> String {
    let shown = file.strip_prefix(root).unwrap_or(file);
    shown.to_string_lossy().replace('\\', "/")
}

/// The line of `pattern` in `text`: the last line that parses to it, as the last match wins.
fn line_of(text: &str, pattern: &str) -> usize {
    let mut found = 0;
    for (i, l) in text.lines().enumerate() {
        let t = if l.ends_with(r"\ ") { l } else { l.trim_end() };
        if t == pattern {
            found = i + 1;
        }
    }
    found
}

/// Reads an ignore file for its line numbers: a file in the work tree through the gate, any
/// other (a global or common exclude) directly. Large files read as empty.
fn read_ignore(gate: &SafeRoot, file: &Path) -> String {
    let path = match file.strip_prefix(gate.root()) {
        Ok(rel) => {
            let text = rel.to_string_lossy().replace('\\', "/");
            let Some(abs) = RepoPath::new(text).ok().and_then(|rp| gate.resolve(&rp)) else {
                return String::new();
            };
            abs
        }
        Err(_) => file.to_owned(),
    };
    let mut bytes = Vec::new();
    let read =
        std::fs::File::open(path).and_then(|f| f.take(MAX_IGNORE_FILE + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_IGNORE_FILE {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn describe(gate: &SafeRoot, hit: &Hit, path: &[u8]) -> Source {
    let Some(file) = &hit.file else {
        let pattern = if path.starts_with(b".git") {
            ".git/"
        } else {
            ".asc/info/"
        };
        return Source {
            file: String::new(),
            line: 0,
            pattern: pattern.to_owned(),
        };
    };
    let text = read_ignore(gate, file);
    Source {
        file: source_name(gate.root(), file),
        line: line_of(&text, &hit.pattern),
        pattern: hit.pattern.clone(),
    }
}

// ---------------------------------------------------------------- lookup

/// Looks paths up against the ignore rules of the work tree.
struct Lookup<'a> {
    gate: SafeRoot,
    sources: IgnoreSources,
    /// Directories whose `.gitignore` a differing `.ascignore` shadows (SPEC §2.4.2).
    shadowed: BTreeSet<String>,
    root: &'a Path,
}

impl Lookup<'_> {
    /// The pattern that decides `rel` (a repository path; `dir_hint` when typed with a trailing
    /// slash), or `None`. A path under an ignored directory is decided by that directory's rule.
    fn decide(&mut self, rel: &[u8], dir_hint: bool, arg: &[u8]) -> Result<Option<Hit>> {
        if rel.is_empty() {
            return Ok(None);
        }
        let mut stack = IgnoreStack::new(self.root, &self.sources);
        if stack.push_dir(self.root).map_err(CliError::Io)? {
            self.shadowed.insert(String::new());
        }
        let parts: Vec<&[u8]> = rel.split(|&b| b == b'/').collect();
        let mut at = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if !at.is_empty() {
                at.push(b'/');
            }
            at.extend_from_slice(part);
            let last = i + 1 == parts.len();
            let beyond = || {
                CliError::Fatal(format!(
                    "pathspec '{}' is beyond a symbolic link",
                    String::from_utf8_lossy(arg)
                ))
            };
            let repo = RepoPath::new(at.clone())
                .map_err(|e| CliError::Fatal(format!("{}: {e}", String::from_utf8_lossy(arg))))?;
            let abs = self.gate.resolve(&repo).ok_or_else(beyond)?;
            let is_dir = !last
                || dir_hint
                || std::fs::symlink_metadata(&abs)
                    .is_ok_and(|m| m.is_dir() && !asc_wc::safe_path::is_link(&m));
            let hit = stack.explain(&abs, is_dir);
            if last {
                return Ok(hit);
            }
            if hit.as_ref().is_some_and(|h| !h.negated) {
                return Ok(hit);
            }
            if stack.push_dir(&abs).map_err(CliError::Io)? {
                self.shadowed
                    .insert(String::from_utf8_lossy(&at).into_owned());
            }
        }
        Ok(None)
    }
}

/// Whether `rel` is tracked: a staged path, or a directory with staged paths below it.
async fn is_tracked(ws: &WsCtx, target: &Target, rel: &[u8], root: &Path) -> Result<bool> {
    let literal = format!(":(literal){}", String::from_utf8_lossy(rel));
    let spec = Spec::parse(&[OsString::from(literal)], "", root)?;
    let mut found = false;
    target
        .tracked(ws, &spec, |_, _| {
            found = true;
            Ok(())
        })
        .await?;
    Ok(found)
}

#[derive(Default)]
struct Opts {
    quiet: bool,
    verbose: bool,
    stdin: bool,
    nul: bool,
    non_matching: bool,
    no_index: bool,
    paths: Vec<OsString>,
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut ended = false;
    for arg in rest {
        let text = arg.to_string_lossy();
        if ended || !text.starts_with('-') || text == "-" {
            o.paths.push(arg.clone());
            continue;
        }
        if text == "--" {
            ended = true;
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            match long {
                "quiet" => o.quiet = true,
                "no-quiet" => o.quiet = false,
                "verbose" => o.verbose = true,
                "no-verbose" => o.verbose = false,
                "stdin" => o.stdin = true,
                "no-stdin" => o.stdin = false,
                "non-matching" => o.non_matching = true,
                "no-non-matching" => o.non_matching = false,
                "no-index" => o.no_index = true,
                "index" => o.no_index = false,
                other => return Err(bad_option(other, false)),
            }
            continue;
        }
        for c in text[1..].chars() {
            match c {
                'q' => o.quiet = true,
                'v' => o.verbose = true,
                'z' => o.nul = true,
                'n' => o.non_matching = true,
                other => return Err(bad_option(&other.to_string(), true)),
            }
        }
    }
    Ok(o)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    let ws = ctx.ws()?;
    if o.stdin {
        if !o.paths.is_empty() {
            return Err(CliError::Fatal(
                "cannot specify pathnames with --stdin".into(),
            ));
        }
    } else {
        if o.nul {
            return Err(CliError::Fatal("-z only makes sense with --stdin".into()));
        }
        if o.paths.is_empty() {
            return Err(CliError::Fatal("no path specified".into()));
        }
    }
    if o.quiet {
        if o.paths.len() > 1 {
            return Err(CliError::Fatal(
                "--quiet is only valid with a single pathname".into(),
            ));
        }
        if o.verbose {
            return Err(CliError::Fatal(
                "cannot have both --quiet and --verbose".into(),
            ));
        }
    }
    if o.non_matching && !o.verbose {
        return Err(CliError::Fatal(
            "--non-matching is only valid with --verbose".into(),
        ));
    }

    let root = ws.projection.toplevel();
    let prefix = cwd_prefix(ctx, ws);
    let quote_high = ctx.config.bool("core.quotePath").unwrap_or(true);
    let inputs: Vec<Vec<u8>> = if o.stdin {
        stdin_records(&read_stdin()?, o.nul)?
    } else {
        o.paths.iter().map(|p| os_bytes(p)).collect()
    };
    let target = if o.no_index {
        None
    } else {
        Some(Target::open(ctx).await?)
    };
    let mut lookup = Lookup {
        gate: SafeRoot::new(root),
        sources: ignore_sources(ctx, ws),
        shadowed: BTreeSet::new(),
        root,
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut matched = 0usize;
    for arg in &inputs {
        if arg.is_empty() {
            return Err(CliError::Fatal(
                "empty string is not a valid pathspec. please use . instead if you meant to match all paths".into(),
            ));
        }
        let text = String::from_utf8_lossy(arg).into_owned();
        let rel = repo_path(OsStr::new(&text), &prefix, root)?;
        let mut hit = lookup.decide(&rel, arg.ends_with(b"/"), arg)?;
        if hit.is_some()
            && let Some(t) = &target
            && is_tracked(ws, t, &rel, root).await?
        {
            hit = None;
        }
        if !o.verbose && hit.as_ref().is_some_and(|h| h.negated) {
            hit = None;
        }
        if hit.is_some() {
            matched += 1;
        }
        if o.quiet {
            continue;
        }
        let source = hit.as_ref().map(|h| describe(&lookup.gate, h, &rel));
        let written = write_record(&mut out, &o, arg, source.as_ref(), quote_high);
        match written {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                return Ok(ExitCode::from(u8::from(matched == 0)));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let flushed = out.flush();
    for dir in &lookup.shadowed {
        eprintln!("{}", shadow_warning(dir));
    }
    if let Err(e) = flushed
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(e.into());
    }
    Ok(ExitCode::from(u8::from(matched == 0)))
}

/// One output record: the path alone, or with `-v` the source, line and pattern.
fn write_record(
    out: &mut dyn Write,
    o: &Opts,
    path: &[u8],
    hit: Option<&Source>,
    quote_high: bool,
) -> io::Result<()> {
    if hit.is_none() && !o.non_matching {
        return Ok(());
    }
    if o.nul {
        if o.verbose {
            let empty = Source {
                file: String::new(),
                line: 0,
                pattern: String::new(),
            };
            let s = hit.unwrap_or(&empty);
            out.write_all(s.file.as_bytes())?;
            out.write_all(&[0])?;
            if hit.is_some() {
                out.write_all(s.line.to_string().as_bytes())?;
            }
            out.write_all(&[0])?;
            out.write_all(s.pattern.as_bytes())?;
            out.write_all(&[0])?;
        }
        out.write_all(path)?;
        return out.write_all(&[0]);
    }
    if o.verbose {
        let (file, line, pattern) = hit.map_or(("", String::new(), ""), |s| {
            (s.file.as_str(), s.line.to_string(), s.pattern.as_str())
        });
        write_quoted(out, file.as_bytes(), quote_high)?;
        out.write_all(b":")?;
        out.write_all(line.as_bytes())?;
        out.write_all(b":")?;
        write_quoted(out, pattern.as_bytes(), quote_high)?;
        out.write_all(b"\t")?;
    }
    write_quoted(out, path, quote_high)?;
    out.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_round_trips() {
        let q = c_quote(b"a\tb\"c\xc3\xa9", true).unwrap();
        assert_eq!(q, b"\"a\\tb\\\"c\\303\\251\"");
        assert_eq!(unquote_c(&q).unwrap(), b"a\tb\"c\xc3\xa9");
        assert_eq!(c_quote("é".as_bytes(), false), None);
        assert_eq!(unquote_c(b"\"bad"), None);
        assert_eq!(unquote_c(b"\"a\\q\""), None);
    }

    #[test]
    fn relative_names() {
        assert_eq!(relative(b"sub/a.txt", b"sub/"), b"a.txt");
        assert_eq!(relative(b"a.txt", b"sub/"), b"../a.txt");
        assert_eq!(relative(b"b/c", b"a/d/"), b"../../b/c");
    }

    #[test]
    fn records_split_and_unquote() {
        assert_eq!(
            stdin_records(b"a\n\"b c\"\nlast", false).unwrap(),
            vec![b"a".to_vec(), b"b c".to_vec(), b"last".to_vec()]
        );
        assert_eq!(
            stdin_records(b"a\0b\n\0", true).unwrap(),
            vec![b"a".to_vec(), b"b\n".to_vec()]
        );
        assert!(stdin_records(b"\"bad\n", false).is_err());
    }

    #[test]
    fn the_last_matching_line_is_reported() {
        assert_eq!(line_of("*.log\n!keep.log\n*.log\n", "*.log"), 3);
        assert_eq!(line_of("a\n", "b"), 0);
    }
}
