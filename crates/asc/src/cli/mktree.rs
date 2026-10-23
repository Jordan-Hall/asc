//! `mktree` (SPEC §11.2): a tree from `ls-tree`-style lines on standard input, printed by id.
//!
//! asc trees hold only canonical modes and unique names, and every entry names an object asc
//! has (SPEC §2.4), so `--missing`, a non-canonical mode and a repeated name are refused
//! where git would write the tree anyway.

use std::ffi::OsString;
use std::io::Read;
use std::process::ExitCode;

use asc_core::git::{ObjectKind, tree_oid};
use asc_core::id::GitOid;
use asc_core::object::{Entry, Kind, Tree};
use asc_core::path::Name;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::plumbing::objects::{self, Object, Resolver};
use super::refs_plumbing::args::{Cursor, Word, parse_oid};
use crate::native::NativeSpec;

/// Largest input read from standard input.
const MAX_INPUT: u64 = 256 << 20;
/// Most entries one tree takes.
const MAX_ENTRIES: usize = 4_000_000;

const USAGE: &str = "usage: git mktree [-z] [--missing] [--batch]

    -z                    input is NUL terminated
    --[no-]missing        allow missing objects
    --[no-]batch          allow creation of more than one tree
";

#[derive(clap::Parser, Debug)]
#[command(name = "mktree", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "mktree",
    flags: &["-z", "--batch"],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// Undoes git's C-style quoting of a path (`"a\tb"`); an unquoted name is returned as is.
fn unquote(name: &[u8]) -> Option<Vec<u8>> {
    let Some(inner) = name.strip_prefix(b"\"").and_then(|n| n.strip_suffix(b"\"")) else {
        return Some(name.to_vec());
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        i += 1;
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let e = *inner.get(i)?;
        i += 1;
        match e {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'\\' | b'"' => out.push(e),
            b'0'..=b'3' => {
                let d1 = *inner.get(i)?;
                let d2 = *inner.get(i + 1)?;
                if !(b'0'..=b'7').contains(&d1) || !(b'0'..=b'7').contains(&d2) {
                    return None;
                }
                i += 2;
                out.push(((e - b'0') << 6) | ((d1 - b'0') << 3) | (d2 - b'0'));
            }
            _ => return None,
        }
    }
    Some(out)
}

/// One parsed input line.
struct Line {
    mode: u32,
    kind: ObjectKind,
    oid: GitOid,
    name: Vec<u8>,
}

fn format_error(line: &[u8]) -> CliError {
    fatal(format!(
        "input format error: {}",
        String::from_utf8_lossy(line)
    ))
}

/// `<mode> SP <type> SP <oid> TAB <name>`.
fn parse_line(line: &[u8], nul: bool) -> Result<Line> {
    let bad = || format_error(line);
    let tab = line.iter().position(|&b| b == b'\t').ok_or_else(bad)?;
    let head = std::str::from_utf8(&line[..tab]).map_err(|_| bad())?;
    let mut parts = head.split(' ');
    let mode = parts
        .next()
        .filter(|m| !m.is_empty() && m.len() <= 7 && m.bytes().all(|b| (b'0'..=b'7').contains(&b)))
        .and_then(|m| u32::from_str_radix(m, 8).ok())
        .ok_or_else(bad)?;
    let kind = parts.next().and_then(objects::kind_named).ok_or_else(bad)?;
    let oid = parts.next().and_then(parse_oid).ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    let raw = &line[tab + 1..];
    let name = if nul {
        raw.to_vec()
    } else {
        unquote(raw).ok_or_else(bad)?
    };
    Ok(Line {
        mode,
        kind,
        oid,
        name,
    })
}

/// The entry kind a mode spells, and the object type it needs.
fn mode_kind(mode: u32) -> Option<(Kind, ObjectKind)> {
    Some(match mode {
        0o100644 => (Kind::File, ObjectKind::Blob),
        0o100755 => (Kind::Exec, ObjectKind::Blob),
        0o120000 => (Kind::Symlink, ObjectKind::Blob),
        0o040000 => (Kind::Dir, ObjectKind::Tree),
        0o160000 => (Kind::Gitlink, ObjectKind::Commit),
        _ => return None,
    })
}

async fn entry_of(res: &Resolver<'_>, ws: &WsCtx, l: &Line) -> Result<Entry> {
    let shown = String::from_utf8_lossy(&l.name).into_owned();
    if l.name.contains(&b'/') {
        return Err(fatal(format!("path {shown} contains slash")));
    }
    let Some((kind, want)) = mode_kind(l.mode) else {
        return Err(fatal(format!(
            "asc: mktree: unsupported mode {:o} for '{shown}'",
            l.mode
        )));
    };
    if l.kind != want {
        return Err(fatal(format!(
            "entry '{shown}' object type ({}) doesn't match mode type ({})",
            objects::type_name(l.kind),
            objects::type_name(want)
        )));
    }
    let name = Name::new(l.name.clone()).map_err(|e| fatal(format!("{shown}: {e}")))?;
    if kind == Kind::Gitlink {
        return Ok(Entry {
            name,
            kind,
            id: [0; 32],
            size: 0,
            git: l.oid,
        });
    }
    let unavailable = || fatal(format!("entry '{shown}' object {} is unavailable", l.oid));
    let obj = res
        .resolve(&l.oid.to_string())
        .await?
        .ok_or_else(unavailable)?;
    match (obj, kind) {
        (Object::Tree { id, .. }, Kind::Dir) => {
            let tree = ws.engine.get_tree(id).await?;
            let size = tree
                .entries()
                .iter()
                .fold(0u64, |s, e| s.saturating_add(e.size));
            Ok(Entry {
                name,
                kind,
                id: *id.as_bytes(),
                size,
                git: l.oid,
            })
        }
        (Object::Blob { file, .. }, Kind::File | Kind::Exec | Kind::Symlink) => Ok(Entry {
            name,
            kind,
            id: *file.as_bytes(),
            size: objects::size(ws, obj).await?,
            git: l.oid,
        }),
        (other, _) => Err(fatal(format!(
            "entry '{shown}' object type ({}) doesn't match mode type ({})",
            other.type_name(),
            objects::type_name(want)
        ))),
    }
}

/// Builds, stores and names one tree.
async fn make(ctx: &Ctx, lines: &[Line]) -> Result<GitOid> {
    let ws = ctx.ws()?;
    let res = Resolver::new(ctx, ws);
    let mut entries = Vec::with_capacity(lines.len());
    for l in lines {
        entries.push(entry_of(&res, ws, l).await?);
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(w) = entries.windows(2).find(|w| w[0].name == w[1].name) {
        return Err(fatal(format!(
            "asc: mktree: duplicate entry '{}'",
            String::from_utf8_lossy(w[0].name.as_bytes())
        )));
    }
    let tree = Tree::new(entries, true).map_err(|e| fatal(e.to_string()))?;
    let oid = tree_oid(&tree);
    let id = ws.engine.put_tree(tree).await?;
    res.learn(oid, ObjectKind::Tree, *id.as_bytes());
    res.flush().await?;
    Ok(oid)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut cur = Cursor::new(&a.rest, USAGE);
    let (mut nul, mut batch) = (false, false);
    while let Some(word) = cur.next() {
        match word {
            Word::End => {}
            Word::Operand(_) => {}
            Word::Short(f) if f == "z" => nul = true,
            Word::Short(f) => {
                return Err(cur.usage_error(&format!("unknown switch `{}'", &f[..1])));
            }
            Word::Long(n, _) if n == "batch" => batch = true,
            Word::Long(n, _) if n == "no-batch" => batch = false,
            Word::Long(n, _) => return Err(cur.usage_error(&format!("unknown option `{n}'"))),
        }
    }
    let mut input = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_INPUT {
        return Err(fatal("asc: mktree: input is too large"));
    }
    let sep = if nul { 0 } else { b'\n' };
    let mut records: Vec<&[u8]> = input.split(|&b| b == sep).collect();
    if records.last().is_some_and(|r| r.is_empty()) {
        records.pop();
    }
    let mut group: Vec<Line> = Vec::new();
    let mut out = String::new();
    for rec in records {
        if rec.is_empty() && batch {
            out.push_str(&format!("{}\n", make(ctx, &group).await?));
            group.clear();
            continue;
        }
        if group.len() >= MAX_ENTRIES {
            return Err(fatal("asc: mktree: too many entries"));
        }
        group.push(parse_line(rec, nul)?);
    }
    if !batch || !group.is_empty() {
        out.push_str(&format!("{}\n", make(ctx, &group).await?));
    }
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_c_style_names() {
        assert_eq!(unquote(b"plain").unwrap(), b"plain");
        assert_eq!(unquote(br#""q\tx""#).unwrap(), b"q\tx");
        assert_eq!(unquote(br#""\303\251""#).unwrap(), "é".as_bytes());
        assert!(unquote(br#""bad\q""#).is_none());
    }

    #[test]
    fn parses_a_line() {
        let l = parse_line(
            b"100644 blob 78981922613b2afb6025042ff6bd878ac1994e85\tf",
            false,
        )
        .unwrap();
        assert_eq!((l.mode, l.name.as_slice()), (0o100644, &b"f"[..]));
        assert!(parse_line(b"100644 blob zz\tf", false).is_err());
        assert!(
            parse_line(
                b"100644 blob 78981922613b2afb6025042ff6bd878ac1994e85 f",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn modes_map_to_kinds() {
        assert_eq!(mode_kind(0o100664), None);
        assert_eq!(mode_kind(0o040000), Some((Kind::Dir, ObjectKind::Tree)));
    }
}
