//! `cat-file` (SPEC §11.2): the type, size and bytes of an object, one at a time or many from
//! standard input (`--batch`, `--batch-check`). Blobs stream; the object bytes are git's.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::process::ExitCode;

use asc_core::object::Kind;

use super::index::mode_of;
use super::objects::{Object, Resolver, git_order, kind_named, raw, size, stream_blob};
use super::quote::Names;
use super::usage;
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::refs_plumbing::args::{Cursor, Word};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Exists,
    Pretty,
    Type,
    Size,
    Batch,
    BatchCheck,
}

#[derive(Default)]
struct Opts {
    mode: Option<Mode>,
    batch_format: Option<String>,
    operands: Vec<String>,
}

fn usage_with(message: &str) -> CliError {
    CliError::Usage(format!("fatal: {message}\n\n{}", usage::CAT_FILE))
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, usage::CAT_FILE);
    let mut o = Opts::default();
    let mut chosen: Vec<&str> = Vec::new();
    let mut batch: Option<(Mode, Option<String>)> = None;
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.operands.extend(cur.rest()),
            Word::Operand(p) => o.operands.push(p),
            Word::Short(flags) => {
                for c in flags.chars() {
                    let (mode, name) = match c {
                        'e' => (Mode::Exists, "-e"),
                        'p' => (Mode::Pretty, "-p"),
                        't' => (Mode::Type, "-t"),
                        's' => (Mode::Size, "-s"),
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    };
                    if let Some(&before) = chosen.last()
                        && before != name
                    {
                        return Err(CliError::Raw(
                            format!(
                                "error: options '{name}' and '{before}' cannot be used together"
                            ),
                            129,
                        ));
                    }
                    chosen.push(name);
                    o.mode = Some(mode);
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "batch" | "batch-check" => {
                    if batch.is_some() {
                        return Err(CliError::Raw(
                            "error: only one batch option may be specified".into(),
                            129,
                        ));
                    }
                    let mode = if name == "batch" {
                        Mode::Batch
                    } else {
                        Mode::BatchCheck
                    };
                    batch = Some((mode, inline));
                }
                "buffer" => {}
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    if let Some((mode, format)) = batch {
        if let Some(short) = chosen.first() {
            return Err(usage_with(&format!(
                "'{short}' is incompatible with batch mode"
            )));
        }
        o.mode = Some(mode);
        o.batch_format = format;
    }
    Ok(o)
}

/// Prints the object a user named, or the error git gives for a name that names none.
async fn find(r: &Resolver<'_>, name: &str) -> Result<Option<Object>> {
    if let Some(obj) = r.resolve(name).await? {
        return Ok(Some(obj));
    }
    if let Some((rev, path)) = name.split_once(':').filter(|(rev, _)| !rev.is_empty())
        && r.resolve(rev).await?.is_some()
    {
        return Err(CliError::Fatal(format!(
            "path '{path}' does not exist in '{rev}'"
        )));
    }
    Ok(None)
}

fn not_valid(name: &str) -> CliError {
    CliError::Fatal(format!("Not a valid object name {name}"))
}

/// `cat-file -p` of a tree: `ls-tree`'s lines with full ids.
async fn pretty_tree(r: &Resolver<'_>, id: asc_core::id::TreeId, out: &mut dyn Write) -> Result<()> {
    let names = Names {
        terminator: b'\n',
        quote_high: r.ctx.config.bool("core.quotePath").unwrap_or(true),
    };
    let tree = r.ws.engine.get_tree(id).await?;
    for e in git_order(&tree) {
        let kind = match e.kind {
            Kind::Dir => "tree",
            Kind::Gitlink => "commit",
            _ => "blob",
        };
        write!(out, "{:06o} {kind} {}\t", mode_of(e.kind), e.git)?;
        names.write(out, e.name.as_bytes())?;
    }
    Ok(())
}

async fn write_content(r: &Resolver<'_>, obj: Object, out: &mut dyn Write) -> Result<()> {
    if let Object::Commit { id, .. } = obj {
        r.learn_tree_of(id).await?;
    }
    match obj {
        Object::Blob { file, .. } => stream_blob(r.ws, file, out).await,
        other => {
            out.write_all(&raw(r.ws, other).await?)?;
            Ok(())
        }
    }
}

async fn single(r: &Resolver<'_>, mode: Mode, name: &str, out: &mut Output) -> Result<ExitCode> {
    let found = find(r, name).await?;
    let hex_like = name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit());
    let Some(obj) = found else {
        return match mode {
            Mode::Exists if hex_like => Ok(ExitCode::from(1)),
            Mode::Type | Mode::Size if hex_like => Err(CliError::Fatal(
                "git cat-file: could not get object info".into(),
            )),
            _ => Err(not_valid(name)),
        };
    };
    match mode {
        Mode::Exists => {}
        Mode::Type => writeln!(out, "{}", obj.type_name())?,
        Mode::Size => writeln!(out, "{}", size(r.ws, obj).await?)?,
        Mode::Pretty => match obj {
            Object::Tree { id, .. } => pretty_tree(r, id, out).await?,
            other => write_content(r, other, out).await?,
        },
        Mode::Batch | Mode::BatchCheck => unreachable!("batch modes are handled by the caller"),
    }
    Ok(ExitCode::SUCCESS)
}

/// A `--batch` format: literal text and the atoms of git's `cat-file`.
enum Piece {
    Text(String),
    Name,
    Type,
    Size,
    Disk,
    DeltaBase,
    Rest,
}

fn parse_format(format: &str) -> Result<Vec<Piece>> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = format;
    while let Some(at) = rest.find('%') {
        text.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(after) = rest.strip_prefix('%') {
            text.push('%');
            rest = after;
        } else if let Some(inner) = rest.strip_prefix('(') {
            let Some(end) = inner.find(')') else {
                return Err(CliError::Fatal(format!("bad cat-file format: {format}")));
            };
            let piece = match &inner[..end] {
                "objectname" => Piece::Name,
                "objecttype" => Piece::Type,
                "objectsize" => Piece::Size,
                "objectsize:disk" => Piece::Disk,
                "deltabase" => Piece::DeltaBase,
                "rest" => Piece::Rest,
                _ => {
                    return Err(CliError::Fatal(format!(
                        "bad cat-file format: %({})",
                        &inner[..end]
                    )));
                }
            };
            if !text.is_empty() {
                out.push(Piece::Text(std::mem::take(&mut text)));
            }
            out.push(piece);
            rest = &inner[end + 1..];
        } else {
            text.push('%');
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}

async fn batch(
    r: &Resolver<'_>,
    mode: Mode,
    format: Option<&str>,
    out: &mut Output,
) -> Result<ExitCode> {
    let default = "%(objectname) %(objecttype) %(objectsize)";
    let pieces = parse_format(format.unwrap_or(default))?;
    let wants_rest = pieces.iter().any(|p| matches!(p, Piece::Rest));
    let stdin = std::io::stdin();
    let mut lines = stdin.lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if lines.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let full = String::from_utf8_lossy(&line).into_owned();
        let (name, rest) = if wants_rest {
            match full.split_once(char::is_whitespace) {
                Some((n, r)) => (n.to_owned(), r.trim_start().to_owned()),
                None => (full.clone(), String::new()),
            }
        } else {
            (full.clone(), String::new())
        };
        let Some(obj) = r.resolve(&name).await? else {
            writeln!(out, "{name} missing")?;
            out.flush()?;
            continue;
        };
        let bytes_len = size(r.ws, obj).await?;
        for piece in &pieces {
            match piece {
                Piece::Text(t) => out.write_all(t.as_bytes())?,
                Piece::Name => write!(out, "{}", obj.oid())?,
                Piece::Type => write!(out, "{}", obj.type_name())?,
                Piece::Size | Piece::Disk => write!(out, "{bytes_len}")?,
                Piece::DeltaBase => write!(out, "{}", "0".repeat(40))?,
                Piece::Rest => out.write_all(rest.as_bytes())?,
            }
        }
        out.write_all(b"\n")?;
        if mode == Mode::Batch {
            write_content(r, obj, out).await?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `cat-file <type> <object>`: the object's bytes, peeled to `want` when it is another type.
async fn typed(r: &Resolver<'_>, want: &str, name: &str, out: &mut Output) -> Result<ExitCode> {
    let Some(kind) = kind_named(want) else {
        return Err(CliError::Fatal(format!("invalid object type \"{want}\"")));
    };
    let Some(obj) = find(r, name).await? else {
        let hex_like = name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit());
        return Err(if hex_like {
            CliError::Fatal(format!("git cat-file {name}: bad file"))
        } else {
            not_valid(name)
        });
    };
    let Some(peeled) = r.peel(obj, kind).await? else {
        return Err(CliError::Fatal(format!("git cat-file {name}: bad file")));
    };
    write_content(r, peeled, out).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    let r = Resolver::new(ctx, ws);
    let mut out = Output::open(ctx, "cat-file", PagerWant::Never)?;
    let code = match o.mode {
        Some(mode @ (Mode::Batch | Mode::BatchCheck)) => {
            if !o.operands.is_empty() {
                return Err(usage_with("batch modes take no arguments"));
            }
            batch(&r, mode, o.batch_format.as_deref(), &mut out).await?
        }
        Some(mode) => {
            let flag = match mode {
                Mode::Exists => "-e",
                Mode::Pretty => "-p",
                Mode::Type => "-t",
                _ => "-s",
            };
            match o.operands.as_slice() {
                [name] => single(&r, mode, name, &mut out).await?,
                [] => return Err(usage_with(&format!("<object> required with '{flag}'"))),
                _ => return Err(usage_with("too many arguments")),
            }
        }
        None => match o.operands.as_slice() {
            [want, name] => typed(&r, want, name, &mut out).await?,
            [] => return Err(CliError::Usage(usage::CAT_FILE.to_owned())),
            other => {
                return Err(usage_with(&format!(
                    "only two arguments allowed in <type> <object> mode, not {}",
                    other.len()
                )));
            }
        },
    };
    out.finish()?;
    r.flush().await?;
    Ok(code)
}
