//! `ls-tree` (SPEC §11.2): the entries of a tree in git's text forms, walked directory by
//! directory (a subtree is read only when the walk reaches it), with git's pathspec and
//! recursion rules.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_core::git::ObjectKind;
use asc_core::id::TreeId;
use asc_core::object::{Entry, Kind};

use super::objects::{Object, Resolver, git_order};
use super::quote::Names;
use super::usage;
use crate::cli::add::cwd_prefix;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::refs_plumbing::args::{Cursor, Word};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Default,
    Long,
    NameOnly,
    ObjectOnly,
}

struct Opts {
    tree_only: bool,
    recursive: bool,
    show_trees: bool,
    layout: Layout,
    nul: bool,
    abbrev: Option<usize>,
    full_name: bool,
    full_tree: bool,
    format: Option<String>,
    operands: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, usage::LS_TREE);
    let mut o = Opts {
        tree_only: false,
        recursive: false,
        show_trees: false,
        layout: Layout::Default,
        nul: false,
        abbrev: None,
        full_name: false,
        full_tree: false,
        format: None,
        operands: Vec::new(),
    };
    let mut layout_options = 0;
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.operands.extend(cur.rest()),
            Word::Operand(p) => o.operands.push(p),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'd' => o.tree_only = true,
                        'r' => o.recursive = true,
                        't' => o.show_trees = true,
                        'z' => o.nul = true,
                        'l' => {
                            o.layout = Layout::Long;
                            layout_options += 1;
                        }
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "long" => {
                    o.layout = Layout::Long;
                    layout_options += 1;
                }
                "name-only" | "name-status" => {
                    o.layout = Layout::NameOnly;
                    layout_options += 1;
                }
                "object-only" => {
                    o.layout = Layout::ObjectOnly;
                    layout_options += 1;
                }
                "full-name" => o.full_name = true,
                "full-tree" => o.full_tree = true,
                "format" => o.format = Some(cur.value("format", inline)?),
                "abbrev" => {
                    o.abbrev = Some(match inline {
                        Some(n) => n
                            .parse::<usize>()
                            .map(|n| n.clamp(4, 40))
                            .map_err(|_| CliError::Raw("error: option `abbrev' expects a numerical value".into(), 129))?,
                        None => 0,
                    })
                }
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    if o.format.is_some() && layout_options > 0 {
        return Err(CliError::Usage(format!(
            "fatal: --format can't be combined with other format-altering options\n\n{}",
            usage::LS_TREE
        )));
    }
    if o.tree_only && o.recursive {
        o.show_trees = true;
    }
    if o.operands.is_empty() {
        return Err(CliError::Usage(usage::LS_TREE.to_owned()));
    }
    Ok(o)
}

/// One element of a `--format` string.
enum Piece {
    Text(Vec<u8>),
    Mode,
    Type,
    Name,
    Size { padded: bool },
    Path,
}

fn parse_format(format: &str) -> Result<Vec<Piece>> {
    let bad = |what: String| CliError::Fatal(format!("bad ls-tree format: {what}"));
    let bytes = format.as_bytes();
    let mut out = Vec::new();
    let mut text = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            text.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest = &bytes[i + 1..];
        match rest.first() {
            Some(b'%') => {
                text.push(b'%');
                i += 2;
            }
            Some(b'n') => {
                text.push(b'\n');
                i += 2;
            }
            Some(b'x') if rest.len() >= 3 && rest[1].is_ascii_hexdigit() && rest[2].is_ascii_hexdigit() => {
                let hex = std::str::from_utf8(&rest[1..3]).unwrap_or("00");
                text.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 4;
            }
            Some(b'(') => {
                let Some(end) = rest.iter().position(|&b| b == b')') else {
                    return Err(bad(format!(
                        "element '{}' does not end in ')'",
                        String::from_utf8_lossy(rest)
                    )));
                };
                let atom = String::from_utf8_lossy(&rest[1..end]).into_owned();
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(match atom.as_str() {
                    "objectmode" => Piece::Mode,
                    "objecttype" => Piece::Type,
                    "objectname" => Piece::Name,
                    "objectsize" => Piece::Size { padded: false },
                    "objectsize:padded" => Piece::Size { padded: true },
                    "path" => Piece::Path,
                    _ => return Err(bad(format!("%({atom})"))),
                });
                i += end + 2;
            }
            _ => {
                text.push(b'%');
                i += 1;
            }
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}

struct Frame {
    base: Vec<u8>,
    entries: Vec<Entry>,
    at: usize,
}

/// How an entry relates to the pathspecs.
fn interesting(specs: &[Vec<u8>], path: &[u8], is_dir: bool) -> bool {
    if specs.is_empty() {
        return true;
    }
    specs.iter().any(|raw| {
        let slash = raw.ends_with(b"/");
        let s = raw.strip_suffix(b"/").unwrap_or(raw);
        s.is_empty()
            || (path == s && (is_dir || !slash))
            || (path.len() > s.len() && path.starts_with(s) && path[s.len()] == b'/')
            || (is_dir && s.len() > path.len() && s.starts_with(path) && s[path.len()] == b'/')
    })
}

/// `show_recursive`: whether a pathspec reaches below the directory `base`/`name`.
fn reaches_below(specs: &[Vec<u8>], full: &[u8]) -> bool {
    specs.iter().any(|s| s.len() > full.len() && s.starts_with(full) && s[full.len()] == b'/')
}

struct Printer<'a> {
    o: &'a Opts,
    names: Names,
    chomp: Vec<u8>,
    pieces: Option<Vec<Piece>>,
    abbrev: usize,
    ws: &'a WsCtx,
}

impl Printer<'_> {
    fn oid(&self, oid: &asc_core::id::GitOid) -> String {
        let hex = oid.to_string();
        if self.o.abbrev.is_some() { hex[..self.abbrev.min(40)].to_owned() } else { hex }
    }

    async fn size_text(&self, entry: &Entry) -> String {
        match entry.kind {
            Kind::Dir | Kind::Gitlink => "-".to_owned(),
            _ => entry.size.to_string(),
        }
    }

    async fn print(&self, out: &mut Output, entry: &Entry, full: &[u8]) -> Result<()> {
        let _ = self.ws;
        let mode = super::index::mode_of(entry.kind);
        let kind = match entry.kind {
            Kind::Dir => "tree",
            Kind::Gitlink => "commit",
            _ => "blob",
        };
        let shown = if self.chomp.is_empty() { full.to_vec() } else { relative_to(full, &self.chomp) };
        if let Some(pieces) = &self.pieces {
            for p in pieces {
                match p {
                    Piece::Text(t) => out.write_all(t)?,
                    Piece::Mode => write!(out, "{mode:06o}")?,
                    Piece::Type => write!(out, "{kind}")?,
                    Piece::Name => write!(out, "{}", self.oid(&entry.git))?,
                    Piece::Size { padded } => {
                        let size = self.size_text(entry).await;
                        if *padded { write!(out, "{size:>7}")? } else { write!(out, "{size}")? }
                    }
                    Piece::Path => self.names.write_bare(out, &shown)?,
                }
            }
            out.write_all(&[self.names.terminator])?;
            return Ok(());
        }
        match self.o.layout {
            Layout::NameOnly => {}
            Layout::ObjectOnly => {
                writeln_term(out, &self.oid(&entry.git), self.names.terminator)?;
                return Ok(());
            }
            Layout::Default => write!(out, "{mode:06o} {kind} {}\t", self.oid(&entry.git))?,
            Layout::Long => {
                let size = self.size_text(entry).await;
                write!(out, "{mode:06o} {kind} {} {size:>7}\t", self.oid(&entry.git))?;
            }
        }
        self.names.write(out, &shown)?;
        Ok(())
    }
}

/// git's `relative_path`: `full` as seen from the directory `base` (which ends in `/`), with
/// `../` for each directory of `base` that `full` does not share, and `./` for the directory
/// itself.
fn relative_to(full: &[u8], base: &[u8]) -> Vec<u8> {
    let base: Vec<&[u8]> = base.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let parts: Vec<&[u8]> = full.split(|&b| b == b'/').collect();
    let common = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
    let mut out = Vec::new();
    for _ in common..base.len() {
        out.extend_from_slice(b"../");
    }
    let rest = parts[common..].join(&b'/');
    if rest.is_empty() && out.is_empty() {
        out.extend_from_slice(b"./");
    }
    out.extend_from_slice(&rest);
    out
}

fn writeln_term(out: &mut Output, text: &str, term: u8) -> Result<()> {
    out.write_all(text.as_bytes())?;
    out.write_all(&[term])?;
    Ok(())
}

pub async fn run(ctx: &Ctx, args: Vec<OsString>) -> Result<ExitCode> {
    let o = parse(&args)?;
    let ws = ctx.ws()?;
    let pieces = o.format.as_deref().map(parse_format).transpose()?;
    let resolver = Resolver::new(ctx, ws);
    let word = &o.operands[0];
    let Some(found) = resolver.resolve(word).await? else {
        return Err(CliError::Fatal(format!("Not a valid object name {word}")));
    };
    let Some(Object::Tree { id: root, .. }) = resolver.peel(found, ObjectKind::Tree).await? else {
        return Err(CliError::Fatal("not a tree object".into()));
    };
    // Paths are relative to the directory the command runs in, unless told otherwise.
    let prefix = if o.full_tree { String::new() } else { cwd_prefix(ctx, ws) };
    let mut specs: Vec<Vec<u8>> = Vec::new();
    for arg in &o.operands[1..] {
        let abs = crate::cli::add::spec::repo_path(std::ffi::OsStr::new(arg), &prefix, ws.projection.toplevel())?;
        let mut spec = abs;
        let last = arg.rsplit('/').next().unwrap_or(arg);
        // A directory spelled as a directory (`dir/`, `.`, `..`) lists what is inside it.
        if !spec.is_empty() && (arg.ends_with('/') || last == "." || last == "..") {
            spec.push(b'/');
        }
        specs.push(spec);
    }
    if specs.is_empty() && !prefix.is_empty() {
        specs.push(format!("{prefix}/").into_bytes());
    }
    let chomp = if prefix.is_empty() || o.full_name || o.full_tree { Vec::new() } else { format!("{prefix}/").into_bytes() };
    let default_abbrev = ctx.config.get("core.abbrev").and_then(|v| v.parse::<usize>().ok()).map_or(7, |n| n.clamp(4, 40));
    let printer = Printer {
        o: &o,
        names: Names {
            terminator: if o.nul { 0 } else { b'\n' },
            quote_high: ctx.config.bool("core.quotePath").unwrap_or(true),
        },
        chomp: chomp.clone(),
        pieces,
        abbrev: match o.abbrev {
            Some(0) | None => default_abbrev,
            Some(n) => n,
        },
        ws,
    };
    let mut out = Output::open(ctx, "ls-tree", PagerWant::Never)?;
    let mut frames: Vec<Frame> = Vec::new();
    let tree = ws.engine.get_tree(root).await?;
    frames.push(Frame { base: Vec::new(), entries: git_order(&tree).into_iter().cloned().collect(), at: 0 });
    while let Some(frame) = frames.last_mut() {
        let Some(entry) = frame.entries.get(frame.at).cloned() else {
            frames.pop();
            continue;
        };
        frame.at += 1;
        let mut full = frame.base.clone();
        full.extend_from_slice(entry.name.as_bytes());
        let is_dir = entry.kind == Kind::Dir;
        if !interesting(&specs, &full, is_dir) {
            continue;
        }
        let mut recurse = false;
        if is_dir {
            if o.recursive || reaches_below(&specs, &full) {
                recurse = true;
            }
        } else if o.tree_only {
            continue;
        }
        let show = !recurse || o.show_trees;
        if show {
            resolver.resolve_entry(&entry);
            printer.print(&mut out, &entry, &full).await?;
        }
        if recurse {
            let mut base = full.clone();
            base.push(b'/');
            let tree = ws.engine.get_tree(TreeId::from_bytes(entry.id)).await?;
            frames.push(Frame { base, entries: git_order(&tree).into_iter().cloned().collect(), at: 0 });
        }
    }
    out.finish()?;
    resolver.flush().await?;
    Ok(ExitCode::SUCCESS)
}
