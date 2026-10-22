//! Output forms that need no file content: `--name-only`, `--name-status`, `--raw`, `--summary`.
//! Also the name helpers shared by every formatter (C quoting, rename display, `--relative`).

use std::io::Write;

use super::{DiffOptions, Result, Status, TreeChange, abbrev_oid};

/// The old and new display names of a change, after `--relative`.
pub(crate) struct Names {
    pub old: Vec<u8>,
    pub new: Vec<u8>,
}

/// Display names of `c`, or `None` when `--relative` hides it.
pub(crate) fn names(opts: &DiffOptions, c: &TreeChange) -> Option<Names> {
    let strip = |p: &[u8]| -> Option<Vec<u8>> {
        let dir = opts.relative.as_ref()?;
        let rest = p.strip_prefix(dir.as_bytes())?.strip_prefix(b"/")?;
        Some(rest.to_vec())
    };
    if opts.relative.is_none() {
        return Some(Names {
            old: c.old_path.as_bytes().to_vec(),
            new: c.path.as_bytes().to_vec(),
        });
    }
    let (new, old) = (strip(c.path.as_bytes()), strip(c.old_path.as_bytes()));
    if new.is_none() && old.is_none() {
        return None;
    }
    Some(Names {
        new: new.unwrap_or_else(|| c.path.as_bytes().to_vec()),
        old: old.unwrap_or_else(|| c.old_path.as_bytes().to_vec()),
    })
}

/// Git's `quote_c_style` with `core.quotepath=true`: bytes outside printable ASCII, `"` and `\`
/// force a double-quoted form with C escapes and octal bytes.
pub(crate) fn quote_c_style(name: &[u8]) -> Vec<u8> {
    let plain = |b: u8| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\';
    if name.iter().all(|&b| plain(b)) {
        return name.to_vec();
    }
    let mut out = vec![b'"'];
    for &b in name {
        match b {
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b if plain(b) => out.push(b),
            b => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
        }
    }
    out.push(b'"');
    out
}

/// A path as written to a listing: raw with `-z`, C-quoted otherwise.
fn listed(opts: &DiffOptions, name: &[u8]) -> Vec<u8> {
    if opts.nul {
        name.to_vec()
    } else {
        quote_c_style(name)
    }
}

/// The line terminator: NUL with `-z`, else LF.
fn eol(opts: &DiffOptions) -> &'static [u8] {
    if opts.nul { b"\0" } else { b"\n" }
}

/// `--name-only`.
pub fn write_name_only(
    w: &mut impl Write,
    changes: &[TreeChange],
    opts: &DiffOptions,
) -> Result<()> {
    for c in changes {
        let Some(n) = names(opts, c) else { continue };
        w.write_all(&listed(opts, &n.new))?;
        w.write_all(eol(opts))?;
    }
    Ok(())
}

/// The status column: letter, plus a three-digit score for renames and copies.
fn status_text(s: Status) -> String {
    match s.score() {
        Some(score) => format!("{}{score:03}", s.letter()),
        None => s.letter().to_string(),
    }
}

/// `--name-status`.
pub fn write_name_status(
    w: &mut impl Write,
    changes: &[TreeChange],
    opts: &DiffOptions,
) -> Result<()> {
    for c in changes {
        let Some(n) = names(opts, c) else { continue };
        w.write_all(status_text(c.status).as_bytes())?;
        let sep: &[u8] = if opts.nul { b"\0" } else { b"\t" };
        w.write_all(sep)?;
        if c.status.score().is_some() {
            w.write_all(&listed(opts, &n.old))?;
            w.write_all(sep)?;
        }
        w.write_all(&listed(opts, &n.new))?;
        w.write_all(eol(opts))?;
    }
    Ok(())
}

/// `--raw`.
pub fn write_raw(w: &mut impl Write, changes: &[TreeChange], opts: &DiffOptions) -> Result<()> {
    for c in changes {
        let Some(n) = names(opts, c) else { continue };
        write!(
            w,
            ":{:06o} {:06o} {} {} {}",
            c.old_mode(),
            c.new_mode(),
            abbrev_oid(c.old.as_ref().map(|e| &e.git), opts.abbrev),
            abbrev_oid(c.new.as_ref().map(|e| &e.git), opts.abbrev),
            status_text(c.status),
        )?;
        let sep: &[u8] = if opts.nul { b"\0" } else { b"\t" };
        w.write_all(sep)?;
        if c.status.score().is_some() {
            w.write_all(&listed(opts, &n.old))?;
            w.write_all(sep)?;
        }
        w.write_all(&listed(opts, &n.new))?;
        w.write_all(eol(opts))?;
    }
    Ok(())
}

/// Git's `pprint_rename`: `pre{old => new}post`, with the common directory prefix and suffix
/// factored out.
pub(crate) fn rename_display(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut pfx = 0;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x != y {
            break;
        }
        if *x == b'/' {
            pfx = i + 1;
        }
    }
    let mut sfx = 0;
    let (mut i, mut j) = (a.len() as isize - 1, b.len() as isize - 1);
    let floor = pfx as isize - isize::from(pfx > 0);
    while i >= floor && j >= floor && i >= 0 && j >= 0 && a[i as usize] == b[j as usize] {
        if a[i as usize] == b'/' {
            sfx = a.len() - i as usize;
        }
        i -= 1;
        j -= 1;
    }
    let a_mid = a.len().saturating_sub(pfx + sfx);
    let b_mid = b.len().saturating_sub(pfx + sfx);
    let mut out = Vec::new();
    let braces = pfx + sfx > 0;
    if braces {
        out.extend_from_slice(&a[..pfx]);
        out.push(b'{');
    }
    out.extend_from_slice(&a[pfx..pfx + a_mid]);
    out.extend_from_slice(b" => ");
    out.extend_from_slice(&b[pfx..pfx + b_mid]);
    if braces {
        out.push(b'}');
        out.extend_from_slice(&a[a.len() - sfx..]);
    }
    out
}

/// `--summary`: creations, deletions, renames and mode changes.
pub fn write_summary(w: &mut impl Write, changes: &[TreeChange], opts: &DiffOptions) -> Result<()> {
    for c in changes {
        let Some(n) = names(opts, c) else { continue };
        let name = quote_c_style(&n.new);
        match c.status {
            Status::Added => {
                write!(w, " create mode {:06o} ", c.new_mode())?;
                w.write_all(&name)?;
                w.write_all(b"\n")?;
            }
            Status::Deleted => {
                write!(w, " delete mode {:06o} ", c.old_mode())?;
                w.write_all(&quote_c_style(&n.old))?;
                w.write_all(b"\n")?;
            }
            Status::Renamed(score) | Status::Copied(score) => {
                let verb = if matches!(c.status, Status::Renamed(_)) {
                    "rename"
                } else {
                    "copy"
                };
                write!(w, " {verb} ")?;
                w.write_all(&quote_c_style(&rename_display(&n.old, &n.new)))?;
                writeln!(w, " ({score}%)")?;
                if c.old_mode() != c.new_mode() {
                    writeln!(
                        w,
                        " mode change {:06o} => {:06o}",
                        c.old_mode(),
                        c.new_mode()
                    )?;
                }
            }
            Status::Modified | Status::TypeChanged => {
                if c.old_mode() != c.new_mode() {
                    write!(
                        w,
                        " mode change {:06o} => {:06o} ",
                        c.old_mode(),
                        c.new_mode()
                    )?;
                    w.write_all(&name)?;
                    w.write_all(b"\n")?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_git() {
        assert_eq!(quote_c_style(b"plain/path.rs"), b"plain/path.rs");
        assert_eq!(quote_c_style(b"a\tb"), b"\"a\\tb\"");
        assert_eq!(quote_c_style(b"q\"x"), b"\"q\\\"x\"");
        assert_eq!(quote_c_style("é".as_bytes()), b"\"\\303\\251\"");
    }

    #[test]
    fn rename_display_factors_common_parts() {
        let d = |a: &str, b: &str| String::from_utf8(rename_display(a.as_bytes(), b.as_bytes()));
        assert_eq!(d("a", "b").unwrap(), "a => b");
        assert_eq!(d("src/a.rs", "src/b.rs").unwrap(), "src/{a.rs => b.rs}");
        assert_eq!(d("x/old/f", "x/new/f").unwrap(), "x/{old => new}/f");
        assert_eq!(d("old/f", "new/f").unwrap(), "{old => new}/f");
        assert_eq!(d("a/b/c", "a/b/c2").unwrap(), "a/b/{c => c2}");
    }
}
