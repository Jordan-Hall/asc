//! `interpret-trailers` (SPEC §11.2): adds, replaces and prints the trailers of a commit message
//! read from files or standard input, in git's grammar: `--trailer`, `--where`, `--if-exists`,
//! `--if-missing`, `--trim-empty`, `--only-trailers`, `--only-input`, `--unfold`, `--parse`,
//! `--in-place`, `--divider`. `trailer.where`, `trailer.ifExists`, `trailer.ifMissing` and
//! `trailer.<alias>.{key,where,ifExists,ifMissing}` apply unless `--only-input`; a
//! `trailer.<alias>.command` is refused rather than run.
//!
//! Like git, an option such as `--where` applies to the `--trailer` arguments after it.

use std::ffi::OsString;
use std::io::{Read, Write as _};
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use super::refs_plumbing::args::{Cursor, Word};
use crate::native::NativeSpec;

/// Largest message read.
const MAX_INPUT: u64 = 64 << 20;
/// Most `--trailer` arguments accepted.
const MAX_TRAILERS: usize = 100_000;

const USAGE: &str = "usage: git interpret-trailers [--in-place] [--trim-empty]
                              [(--trailer (<key>|<key-alias>)[(=|:)<value>])...]
                              [--parse] [<file>...]

    --[no-]in-place       edit files in place
    --[no-]trim-empty     trim empty trailers
    --[no-]where <placement>
                          where to place the new trailer
    --[no-]if-exists <action>
                          action if trailer already exists
    --[no-]if-missing <action>
                          action if trailer is missing
    --[no-]only-trailers  output only the trailers
    --[no-]only-input     do not apply trailer.<key-alias> configuration variables
    --[no-]unfold         reformat multiline trailer values as single-line values
    --parse               alias for --only-trailers --only-input --unfold
    --no-divider          do not treat \"---\" as the end of input
    --divider             opposite of --no-divider
    --[no-]trailer <trailer>
                          trailer(s) to add
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "interpret-trailers",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "interpret-trailers",
    flags: &[
        "--in-place",
        "--no-in-place",
        "--trim-empty",
        "--no-trim-empty",
        "--where",
        "--no-where",
        "--if-exists",
        "--no-if-exists",
        "--if-missing",
        "--no-if-missing",
        "--only-trailers",
        "--no-only-trailers",
        "--only-input",
        "--no-only-input",
        "--unfold",
        "--no-unfold",
        "--parse",
        "--no-divider",
        "--divider",
        "--trailer",
        "--no-trailer",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Where {
    After,
    Before,
    End,
    Start,
}

impl Where {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "after" => Self::After,
            "before" => Self::Before,
            "end" => Self::End,
            "start" => Self::Start,
            _ => return None,
        })
    }

    /// Whether new trailers go after what they are placed against.
    fn after(self) -> bool {
        matches!(self, Self::After | Self::End)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exists {
    AddIfDifferentNeighbor,
    AddIfDifferent,
    Add,
    Replace,
    DoNothing,
}

impl Exists {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "addifdifferentneighbor" => Self::AddIfDifferentNeighbor,
            "addifdifferent" => Self::AddIfDifferent,
            "add" => Self::Add,
            "replace" => Self::Replace,
            "donothing" => Self::DoNothing,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Missing {
    Add,
    DoNothing,
}

impl Missing {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "add" => Self::Add,
            "donothing" => Self::DoNothing,
            _ => return None,
        })
    }
}

/// The placement settings that apply to one new trailer.
#[derive(Clone, Copy, Debug)]
struct Conf {
    wh: Where,
    exists: Exists,
    missing: Missing,
}

/// A `--trailer` argument with the settings in force when it was typed.
struct Arg {
    text: String,
    conf: Conf,
}

#[derive(Default)]
struct Opts {
    in_place: bool,
    trim_empty: bool,
    only_trailers: bool,
    only_input: bool,
    unfold: bool,
    no_divider: bool,
    args: Vec<Arg>,
    files: Vec<String>,
    /// Command-line placement overrides, applied to the `--trailer`s after them.
    wh: Option<Where>,
    exists: Option<Exists>,
    missing: Option<Missing>,
    /// `--where` etc. typed before any `--trailer`, per argument; resolved against config later.
    pending: Vec<(Option<Where>, Option<Exists>, Option<Missing>)>,
}

fn bad_value(cur: &Cursor, what: &str, value: &str) -> CliError {
    cur.usage_error(&format!("unknown value '{value}' for {what}"))
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.files.extend(cur.rest()),
            Word::Operand(p) => o.files.push(p),
            Word::Short(f) => {
                return Err(cur.usage_error(&format!("unknown switch `{}'", &f[..1])));
            }
            Word::Long(name, inline) => match name.as_str() {
                "in-place" => o.in_place = true,
                "no-in-place" => o.in_place = false,
                "trim-empty" => o.trim_empty = true,
                "no-trim-empty" => o.trim_empty = false,
                "only-trailers" => o.only_trailers = true,
                "no-only-trailers" => o.only_trailers = false,
                "only-input" => o.only_input = true,
                "no-only-input" => o.only_input = false,
                "unfold" => o.unfold = true,
                "no-unfold" => o.unfold = false,
                "parse" => {
                    o.only_trailers = true;
                    o.only_input = true;
                    o.unfold = true;
                }
                "no-divider" => o.no_divider = true,
                "divider" => o.no_divider = false,
                "where" => {
                    let v = cur.value("where", inline)?;
                    o.wh = Some(Where::parse(&v).ok_or_else(|| bad_value(&cur, "--where", &v))?);
                }
                "no-where" => o.wh = None,
                "if-exists" => {
                    let v = cur.value("if-exists", inline)?;
                    o.exists =
                        Some(Exists::parse(&v).ok_or_else(|| bad_value(&cur, "--if-exists", &v))?);
                }
                "no-if-exists" => o.exists = None,
                "if-missing" => {
                    let v = cur.value("if-missing", inline)?;
                    o.missing = Some(
                        Missing::parse(&v).ok_or_else(|| bad_value(&cur, "--if-missing", &v))?,
                    );
                }
                "no-if-missing" => o.missing = None,
                "trailer" => {
                    if o.args.len() >= MAX_TRAILERS {
                        return Err(fatal("too many --trailer arguments"));
                    }
                    let text = cur.value("trailer", inline)?;
                    o.pending.push((o.wh, o.exists, o.missing));
                    o.args.push(Arg {
                        text,
                        conf: Conf {
                            wh: Where::End,
                            exists: Exists::AddIfDifferentNeighbor,
                            missing: Missing::Add,
                        },
                    });
                }
                "no-trailer" => {
                    o.args.clear();
                    o.pending.clear();
                }
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

// ---------------------------------------------------------------- configuration

/// Settings from `trailer.*`.
#[derive(Default)]
struct TrailerConfig {
    wh: Option<Where>,
    exists: Option<Exists>,
    missing: Option<Missing>,
    separators: Option<String>,
    /// `(alias, key, where, ifexists, ifmissing)` of each `trailer.<alias>` section.
    aliases: Vec<Alias>,
}

struct Alias {
    name: String,
    key: Option<String>,
    wh: Option<Where>,
    exists: Option<Exists>,
    missing: Option<Missing>,
}

fn load_config(ctx: &Ctx) -> Result<TrailerConfig> {
    let mut c = TrailerConfig::default();
    let get = |key: &str| ctx.config.get(key);
    if let Some(v) = get("trailer.where") {
        c.wh = Some(
            Where::parse(&v)
                .ok_or_else(|| fatal(format!("unknown value '{v}' for key 'trailer.where'")))?,
        );
    }
    if let Some(v) = get("trailer.ifexists") {
        c.exists = Some(
            Exists::parse(&v)
                .ok_or_else(|| fatal(format!("unknown value '{v}' for key 'trailer.ifexists'")))?,
        );
    }
    if let Some(v) = get("trailer.ifmissing") {
        c.missing =
            Some(Missing::parse(&v).ok_or_else(|| {
                fatal(format!("unknown value '{v}' for key 'trailer.ifmissing'"))
            })?);
    }
    c.separators = get("trailer.separators");
    let mut names: Vec<String> = Vec::new();
    for section in ctx
        .config
        .file()
        .sections_by_name("trailer")
        .into_iter()
        .flatten()
    {
        if let Some(sub) = section.header().subsection_name() {
            let n = sub.to_string();
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    for name in names {
        for cmd in ["command", "cmd"] {
            if get(&format!("trailer.{name}.{cmd}")).is_some() {
                return Err(fatal(format!("asc: trailer.{name}.{cmd} is not supported")));
            }
        }
        let wh = match get(&format!("trailer.{name}.where")) {
            Some(v) => Some(Where::parse(&v).ok_or_else(|| {
                fatal(format!(
                    "unknown value '{v}' for key 'trailer.{name}.where'"
                ))
            })?),
            None => None,
        };
        let exists = match get(&format!("trailer.{name}.ifexists")) {
            Some(v) => Some(Exists::parse(&v).ok_or_else(|| {
                fatal(format!(
                    "unknown value '{v}' for key 'trailer.{name}.ifexists'"
                ))
            })?),
            None => None,
        };
        let missing = match get(&format!("trailer.{name}.ifmissing")) {
            Some(v) => Some(Missing::parse(&v).ok_or_else(|| {
                fatal(format!(
                    "unknown value '{v}' for key 'trailer.{name}.ifmissing'"
                ))
            })?),
            None => None,
        };
        c.aliases.push(Alias {
            key: get(&format!("trailer.{name}.key")),
            name,
            wh,
            exists,
            missing,
        });
    }
    Ok(c)
}

// ---------------------------------------------------------------- the message

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Position of the separator in `line` when it starts with a trailer token.
fn separator_pos(line: &[u8], seps: &[u8]) -> Option<usize> {
    let mut blank_seen = false;
    for (i, &c) in line.iter().enumerate() {
        if seps.contains(&c) {
            return Some(i);
        }
        if !blank_seen && (c.is_ascii_alphanumeric() || c == b'-') {
            continue;
        }
        if i != 0 && (c == b' ' || c == b'\t') {
            blank_seen = true;
            continue;
        }
        break;
    }
    None
}

/// The lines of `message`, each without its newline.
fn lines_of(message: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = message.split(|&b| b == b'\n').collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

/// Index of the first line of the trailer block, or `len` when there is none.
fn block_start(lines: &[&[u8]], seps: &[u8]) -> usize {
    let Some(title_end) = lines.iter().position(|l| is_blank(l)) else {
        return lines.len();
    };
    let (mut trailer, mut other, mut cont) = (0usize, 0usize, 0usize);
    let (mut recognised, mut only_spaces) = (false, true);
    for i in (title_end..lines.len()).rev() {
        let line = lines[i];
        if is_blank(line) {
            if only_spaces {
                continue;
            }
            other += cont;
            if (recognised && trailer * 3 >= other) || (trailer > 0 && other == 0) {
                return i + 1;
            }
            return lines.len();
        }
        only_spaces = false;
        if line.starts_with(b"Signed-off-by: ") || line.starts_with(b"(cherry picked from commit ")
        {
            trailer += 1;
            cont = 0;
            recognised = true;
            continue;
        }
        if separator_pos(line, seps).is_some_and(|p| p >= 1) && !line[0].is_ascii_whitespace() {
            trailer += 1;
            cont = 0;
        } else if line[0].is_ascii_whitespace() {
            cont += 1;
        } else {
            other += 1 + cont;
            cont = 0;
        }
    }
    lines.len()
}

/// One entry of the trailer list.
#[derive(Clone, Debug)]
struct Item {
    /// `None` for a block line that is not a trailer.
    token: Option<Vec<u8>>,
    value: Vec<u8>,
    /// The text to print, without the final newline.
    raw: Vec<u8>,
    /// Whether the item came from the input (and may be printed as it was).
    original: bool,
}

fn unfold(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'\n' {
            while out.last().is_some_and(|c: &u8| c.is_ascii_whitespace()) {
                out.pop();
            }
            i += 1;
            while i < value.len() && value[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        out.push(value[i]);
        i += 1;
    }
    out
}

/// The items of the block lines.
fn items_of(block: &[&[u8]], seps: &[u8]) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for line in block {
        if line.first().is_some_and(u8::is_ascii_whitespace) {
            if let Some(last) = out.last_mut() {
                last.raw.push(b'\n');
                last.raw.extend_from_slice(line);
                if last.token.is_some() {
                    last.value.push(b'\n');
                    last.value.extend_from_slice(line);
                }
            } else {
                out.push(Item {
                    token: None,
                    value: Vec::new(),
                    raw: line.to_vec(),
                    original: true,
                });
            }
            continue;
        }
        match separator_pos(line, seps) {
            Some(p) if p >= 1 => out.push(Item {
                token: Some(line[..p].trim_ascii_end().to_vec()),
                value: line[p + 1..].trim_ascii().to_vec(),
                raw: line.to_vec(),
                original: true,
            }),
            _ => out.push(Item {
                token: None,
                value: Vec::new(),
                raw: line.to_vec(),
                original: true,
            }),
        }
    }
    out
}

/// git compares only as much of the two tokens as the shorter one has, ignoring case.
fn same_token(a: &Item, b: &Item) -> bool {
    match (&a.token, &b.token) {
        (Some(x), Some(y)) => {
            let n = x.len().min(y.len());
            x[..n].eq_ignore_ascii_case(&y[..n])
        }
        _ => false,
    }
}

fn same_trailer(a: &Item, b: &Item) -> bool {
    same_token(a, b) && a.value == b.value
}

/// Whether `new` differs from the neighbours of `at` that its placement checks.
fn differs(list: &[Item], at: usize, new: &Item, wh: Where, check_all: bool) -> bool {
    let mut i = at as isize;
    loop {
        if same_trailer(&list[i as usize], new) {
            return false;
        }
        i += if wh.after() { -1 } else { 1 };
        if i < 0 || i as usize >= list.len() || !check_all {
            return true;
        }
    }
}

/// Applies one new trailer to the list, as git's `process_trailers_lists` does.
fn apply(list: &mut Vec<Item>, new: Item, conf: Conf) {
    let found = if conf.wh.after() {
        (0..list.len()).rev().find(|&i| same_token(&list[i], &new))
    } else {
        (0..list.len()).find(|&i| same_token(&list[i], &new))
    };
    let Some(at) = found else {
        if conf.missing == Missing::Add {
            if conf.wh.after() {
                list.push(new);
            } else {
                list.insert(0, new);
            }
        }
        return;
    };
    // Start and end place the new trailer at the ends of the list, after and before next to
    // the match.
    let place = |list: &mut Vec<Item>, new: Item, at: usize| match conf.wh {
        Where::End => list.push(new),
        Where::Start => list.insert(0, new),
        Where::After => list.insert(at + 1, new),
        Where::Before => list.insert(at, new),
    };
    match conf.exists {
        Exists::DoNothing => {}
        Exists::Replace => {
            // The new trailer goes in first; the match then sits one later when it went before.
            let removed = match conf.wh {
                Where::Start | Where::Before => at + 1,
                Where::End | Where::After => at,
            };
            place(list, new, at);
            list.remove(removed);
        }
        Exists::Add => place(list, new, at),
        Exists::AddIfDifferent => {
            if differs(list, at, &new, conf.wh, true) {
                place(list, new, at);
            }
        }
        Exists::AddIfDifferentNeighbor => {
            if differs(list, at, &new, conf.wh, false) {
                place(list, new, at);
            }
        }
    }
}

/// A `--trailer` argument as an item, with its alias resolved.
fn new_item(
    text: &str,
    seps: &str,
    cfg: Option<&TrailerConfig>,
) -> Result<Option<(Item, Option<usize>)>> {
    let bytes = text.as_bytes();
    let sep_chars: Vec<u8> = {
        let mut s = seps.as_bytes().to_vec();
        if !s.contains(&b'=') {
            s.push(b'=');
        }
        s
    };
    let at = bytes.iter().position(|b| sep_chars.contains(b));
    let (token, value) = match at {
        Some(p) => (bytes[..p].trim_ascii(), bytes[p + 1..].trim_ascii()),
        None => (bytes.trim_ascii(), &b""[..]),
    };
    if token.is_empty() {
        eprintln!("error: empty trailer token in trailer '{text}'");
        return Ok(None);
    }
    let mut key = token.to_vec();
    let mut alias_ix = None;
    if let Some(cfg) = cfg {
        for (i, a) in cfg.aliases.iter().enumerate() {
            if a.name.as_bytes().eq_ignore_ascii_case(token)
                || a.key.as_deref().is_some_and(|k| {
                    k.trim()
                        .trim_end_matches(|c| seps.contains(c))
                        .trim()
                        .as_bytes()
                        .eq_ignore_ascii_case(token)
                })
            {
                alias_ix = Some(i);
                if let Some(k) = &a.key {
                    key = k
                        .trim()
                        .trim_end_matches(|c| seps.contains(c))
                        .trim()
                        .as_bytes()
                        .to_vec();
                }
                break;
            }
        }
    }
    let sep = seps.as_bytes().first().copied().unwrap_or(b':');
    let mut raw = key.clone();
    raw.push(sep);
    raw.push(b' ');
    raw.extend_from_slice(value);
    Ok(Some((
        Item {
            token: Some(key),
            value: value.to_vec(),
            raw,
            original: false,
        },
        alias_ix,
    )))
}

/// Where the trailing run of comment lines (with the blank lines among them) starts; that run
/// stays after the trailers.
fn ignored_tail(lines: &[&[u8]]) -> usize {
    let mut keep = lines.len();
    let mut start = lines.len();
    let mut has_comment = false;
    let mut i = lines.len();
    while i > 0 {
        let l = lines[i - 1];
        if l.first() == Some(&b'#') {
            has_comment = true;
        } else if !is_blank(l) {
            break;
        }
        start = i - 1;
        i -= 1;
    }
    if has_comment {
        keep = start;
    }
    keep
}

/// Splits the divider (`---` at the start of a line) from the text.
fn split_divider(input: &[u8]) -> (&[u8], &[u8]) {
    let mut at = 0;
    for line in input.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b"---") && line[3..].iter().all(u8::is_ascii_whitespace)
            || line.starts_with(b"--- ")
        {
            return (&input[..at], &input[at..]);
        }
        at += line.len();
    }
    (input, &[])
}

struct Plan<'a> {
    o: &'a Opts,
    seps: String,
    cfg: Option<TrailerConfig>,
}

fn process(plan: &Plan, input: &[u8]) -> Result<Vec<u8>> {
    let o = plan.o;
    let (text, patch) = if o.no_divider {
        (input, &b""[..])
    } else {
        split_divider(input)
    };
    let all = lines_of(text);
    let kept = ignored_tail(&all);
    let body = &all[..kept];
    let tail = &all[kept..];
    let seps = plan.seps.as_bytes();
    let start = block_start(body, seps);
    let had_block = start < body.len();
    let head = &body[..start];
    let mut end = body.len();
    while end > start && is_blank(body[end - 1]) {
        end -= 1;
    }
    let mut list = items_of(&body[start.min(end)..end], seps);
    let mut pending = o.pending.iter();
    for arg in &o.args {
        let (wh, exists, missing) = pending.next().copied().unwrap_or((None, None, None));
        let item = new_item(&arg.text, &plan.seps, plan.cfg.as_ref())?;
        let Some((item, alias)) = item else { continue };
        let cfg = plan.cfg.as_ref();
        let alias = alias.and_then(|i| cfg.map(|c| &c.aliases[i]));
        let conf = Conf {
            wh: wh
                .or_else(|| alias.and_then(|a| a.wh))
                .or_else(|| cfg.and_then(|c| c.wh))
                .unwrap_or(Where::End),
            exists: exists
                .or_else(|| alias.and_then(|a| a.exists))
                .or_else(|| cfg.and_then(|c| c.exists))
                .unwrap_or(Exists::AddIfDifferentNeighbor),
            missing: missing
                .or_else(|| alias.and_then(|a| a.missing))
                .or_else(|| cfg.and_then(|c| c.missing))
                .unwrap_or(Missing::Add),
        };
        apply(&mut list, item, conf);
    }
    if o.trim_empty {
        list.retain(|i| i.token.is_none() || !i.value.is_empty());
    }
    let mut out: Vec<u8> = Vec::new();
    let printed = |i: &Item| -> Vec<u8> {
        if o.unfold {
            unfold(&i.raw)
        } else {
            i.raw.clone()
        }
    };
    if o.only_trailers {
        for i in &list {
            out.extend_from_slice(&printed(i));
            out.push(b'\n');
        }
        return Ok(out);
    }
    for l in head {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    if !had_block && (!list.is_empty() || !o.args.is_empty()) {
        // Separate the new block from the text before it.
        if !head.last().is_some_and(|l| is_blank(l)) {
            out.push(b'\n');
        }
    }
    for i in &list {
        out.extend_from_slice(&printed(i));
        out.push(b'\n');
    }
    for l in tail {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    out.extend_from_slice(patch);
    Ok(out)
}

fn read_bounded(reader: impl Read, what: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader.take(MAX_INPUT + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_INPUT {
        return Err(fatal(format!("{what} is too large")));
    }
    Ok(buf)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    if o.in_place && o.files.is_empty() {
        return Err(fatal("no input file given for in-place editing"));
    }
    if o.only_input && !o.args.is_empty() {
        return Err(CliError::Raw(
            format!(
                "fatal: --trailer with --only-input does not make sense

{USAGE}"
            ),
            129,
        ));
    }
    let cfg = if o.only_input {
        None
    } else {
        Some(load_config(ctx)?)
    };
    let seps = cfg
        .as_ref()
        .and_then(|c| c.separators.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ":".to_owned());
    let plan = Plan { o: &o, seps, cfg };
    if o.files.is_empty() {
        let input = read_bounded(std::io::stdin().lock(), "standard input")?;
        let out = process(&plan, &input)?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&out)?;
        stdout.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    for file in &o.files {
        let path = std::path::Path::new(file);
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            fatal(format!(
                "could not read input file '{file}': {}",
                os_reason(&e)
            ))
        })?;
        if !meta.is_file() {
            return Err(fatal(format!("file {file} is not a regular file")));
        }
        let handle = std::fs::File::open(path).map_err(|e| {
            fatal(format!(
                "could not read input file '{file}': {}",
                os_reason(&e)
            ))
        })?;
        let input = read_bounded(handle, file)?;
        let out = process(&plan, &input)?;
        if o.in_place {
            let tmp = path.with_extension(format!("asc-trailers-{}", std::process::id()));
            std::fs::write(&tmp, &out)
                .and_then(|()| std::fs::rename(&tmp, path))
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    fatal(format!("could not rename temporary file to {file}: {e}"))
                })?;
        } else {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&out)?;
            stdout.flush()?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn os_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_on(input: &str, args: &[&str]) -> String {
        let argv: Vec<OsString> = args.iter().map(OsString::from).collect();
        let o = parse(&argv).unwrap();
        let plan = Plan {
            o: &o,
            seps: ":".into(),
            cfg: None,
        };
        String::from_utf8(process(&plan, input.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn adds_after_the_existing_block() {
        let got = run_on("subject\n\nbody\n\nA: 1\n", &["--trailer", "B: 2"]);
        assert_eq!(got, "subject\n\nbody\n\nA: 1\nB: 2\n");
    }

    #[test]
    fn starts_a_block_when_there_is_none() {
        let got = run_on("subject\n", &["--trailer", "A: 1"]);
        assert_eq!(got, "subject\n\nA: 1\n");
    }

    #[test]
    fn identical_neighbour_is_not_repeated() {
        let got = run_on("s\n\nb\n\nA: 1\n", &["--trailer", "a: 1"]);
        assert_eq!(got, "s\n\nb\n\nA: 1\n");
    }

    #[test]
    fn replace_puts_the_new_trailer_at_the_end() {
        let got = run_on(
            "s\n\nb\n\nA: 1\nB: 2\n",
            &["--if-exists", "replace", "--trailer", "A: 3"],
        );
        assert_eq!(got, "s\n\nb\n\nB: 2\nA: 3\n");
    }

    #[test]
    fn parse_unfolds_and_prints_only_trailers() {
        let got = run_on("s\n\nb\n\nA: 1\n  more\nB: 2\n", &["--parse"]);
        assert_eq!(got, "A: 1 more\nB: 2\n");
    }
}
