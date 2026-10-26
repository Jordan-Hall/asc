//! Unified patch output (`diff --git` headers, mode and rename lines, hunks).

use std::io::Write;

use asc_core::object::Kind;
use bytes::Bytes;

use super::format::{names, quote_c_style};
use super::line::{self, LineDiff, is_binary};
use super::{
    Attributes, ContentSource, DiffAttr, DiffOptions, Result, Status, TreeChange, abbrev_oid,
};

/// What a diff run reads from: options, file bytes and attributes.
pub struct DiffCtx<'a> {
    pub opts: &'a DiffOptions,
    pub content: &'a dyn ContentSource,
    pub attrs: &'a dyn Attributes,
}

/// The comparable form of one change.
pub(crate) enum Loaded {
    /// Both sides are the same object; only the header (modes, rename) differs.
    Same,
    Binary,
    /// Text on each side after any textconv; an absent side is empty.
    Text {
        old: Bytes,
        new: Bytes,
    },
}

/// Reads and prepares the content of `c`, deciding text or binary.
pub(crate) async fn load(ctx: &DiffCtx<'_>, c: &TreeChange) -> Result<Loaded> {
    if let (Some(o), Some(n)) = (&c.old, &c.new)
        && o.git == n.git
        && (o.kind == Kind::Gitlink) == (n.kind == Kind::Gitlink)
    {
        return Ok(Loaded::Same);
    }
    let attr = ctx.attrs.diff_attr(&c.path);
    if attr == DiffAttr::Binary {
        return Ok(Loaded::Binary);
    }
    let big = ctx.opts.big_file_threshold;
    if [&c.old, &c.new]
        .into_iter()
        .flatten()
        .any(|e| e.kind != Kind::Gitlink && e.size > big)
    {
        return Ok(Loaded::Binary);
    }
    let mut sides = [Bytes::new(), Bytes::new()];
    let mut converted = false;
    for (slot, (entry, path)) in sides
        .iter_mut()
        .zip([(&c.old, &c.old_path), (&c.new, &c.path)])
    {
        let Some(e) = entry else { continue };
        let raw = if e.kind == Kind::Gitlink {
            Bytes::from(format!("Subproject commit {}\n", e.git))
        } else {
            ctx.content.read(e).await?
        };
        *slot = match &attr {
            DiffAttr::Driver(d) if e.kind != Kind::Gitlink => {
                match ctx.attrs.textconv(d, path, &raw).await? {
                    Some(t) => {
                        converted = true;
                        t
                    }
                    None => raw,
                }
            }
            _ => raw,
        };
    }
    let force_text = converted || attr == DiffAttr::Text;
    if !force_text && sides.iter().any(|s| is_binary(s)) {
        return Ok(Loaded::Binary);
    }
    let [old, new] = sides;
    Ok(Loaded::Text { old, new })
}

/// Writes the unified patch for `changes`.
pub async fn write_patch(
    w: &mut impl Write,
    ctx: &DiffCtx<'_>,
    changes: &[TreeChange],
) -> Result<()> {
    for c in changes {
        write_change(w, ctx, c, false).await?;
    }
    Ok(())
}

/// One change as a patch, or as a word diff when `words` is set. A type change is written as a
/// deletion followed by a creation, like git.
pub(crate) async fn write_change(
    w: &mut impl Write,
    ctx: &DiffCtx<'_>,
    c: &TreeChange,
    words: bool,
) -> Result<()> {
    if c.status == Status::TypeChanged {
        let del = TreeChange {
            new: None,
            status: Status::Deleted,
            ..c.clone()
        };
        let add = TreeChange {
            old: None,
            status: Status::Added,
            ..c.clone()
        };
        Box::pin(write_change(w, ctx, &del, words)).await?;
        return Box::pin(write_change(w, ctx, &add, words)).await;
    }
    let Some(n) = names(ctx.opts, c) else {
        return Ok(());
    };
    let (pa, pb) = ctx.opts.prefixes();
    let join = |p: &str, name: &[u8]| [p.as_bytes(), name].concat();
    let a_name = join(&pa, &n.old);
    let b_name = join(&pb, &n.new);
    let label = |present: bool, name: &[u8]| -> Vec<u8> {
        if present {
            quote_c_style(name)
        } else {
            b"/dev/null".to_vec()
        }
    };
    let a_label = label(c.old.is_some(), &a_name);
    let b_label = label(c.new.is_some(), &b_name);

    w.write_all(b"diff --git ")?;
    w.write_all(&quote_c_style(&a_name))?;
    w.write_all(b" ")?;
    w.write_all(&quote_c_style(&b_name))?;
    w.write_all(b"\n")?;
    let (om, nm) = (c.old_mode(), c.new_mode());
    match c.status {
        Status::Added => writeln!(w, "new file mode {nm:06o}")?,
        Status::Deleted => writeln!(w, "deleted file mode {om:06o}")?,
        _ if om != nm => write!(w, "old mode {om:06o}\nnew mode {nm:06o}\n")?,
        _ => {}
    }
    if let Some(score) = c.status.score() {
        let verb = if matches!(c.status, Status::Renamed(_)) {
            "rename"
        } else {
            "copy"
        };
        writeln!(w, "similarity index {score}%")?;
        w.write_all(format!("{verb} from ").as_bytes())?;
        w.write_all(&quote_c_style(&n.old))?;
        w.write_all(format!("\n{verb} to ").as_bytes())?;
        w.write_all(&quote_c_style(&n.new))?;
        w.write_all(b"\n")?;
    }
    let (og, ng) = (
        c.old.as_ref().map(|e| &e.git),
        c.new.as_ref().map(|e| &e.git),
    );
    if og != ng {
        write!(
            w,
            "index {}..{}",
            abbrev_oid(og, ctx.opts.abbrev),
            abbrev_oid(ng, ctx.opts.abbrev)
        )?;
        if om == nm {
            write!(w, " {nm:06o}")?;
        }
        w.write_all(b"\n")?;
    }

    match load(ctx, c).await? {
        Loaded::Same => {}
        Loaded::Binary => {
            w.write_all(b"Binary files ")?;
            w.write_all(&a_label)?;
            w.write_all(b" and ")?;
            w.write_all(&b_label)?;
            w.write_all(b" differ\n")?;
        }
        Loaded::Text { old, new } => {
            let ld = line::diff(&old, &new, ctx.opts.algorithm, ctx.opts.indent_heuristic);
            if ld.hunks.is_empty() {
                return Ok(());
            }
            // Git appends a tab to real names that contain a space, never to /dev/null.
            let tab = |present: bool, name: &[u8]| -> &'static [u8] {
                if present && name.contains(&b' ') {
                    b"\t"
                } else {
                    b""
                }
            };
            w.write_all(b"--- ")?;
            w.write_all(&a_label)?;
            w.write_all(tab(c.old.is_some(), &a_name))?;
            w.write_all(b"\n+++ ")?;
            w.write_all(&b_label)?;
            w.write_all(tab(c.new.is_some(), &b_name))?;
            w.write_all(b"\n")?;
            if words {
                super::words::write_hunks(w, &ld, ctx.opts)?;
            } else {
                write_hunks(w, &ld, ctx.opts)?;
            }
        }
    }
    Ok(())
}

/// Hunk header text: `-start[,count] +start[,count]`.
pub(crate) fn range_text(start: usize, count: usize) -> String {
    let first = if count == 0 { start } else { start + 1 };
    if count == 1 {
        format!("{first}")
    } else {
        format!("{first},{count}")
    }
}

/// Writes `@@ -a,b +c,d @@ func` for one group.
pub(crate) fn write_hunk_header(
    w: &mut impl Write,
    g: &line::Group,
    func: Option<&[u8]>,
) -> Result<()> {
    write!(
        w,
        "@@ -{} +{} @@",
        range_text(g.old.start, g.old.len()),
        range_text(g.new.start, g.new.len())
    )?;
    if let Some(f) = func {
        w.write_all(b" ")?;
        w.write_all(f)?;
    }
    w.write_all(b"\n")?;
    Ok(())
}

/// One diff line with its marker; a missing final newline gets git's notice.
pub(crate) fn emit_line(w: &mut impl Write, marker: u8, text: &[u8]) -> Result<()> {
    w.write_all(&[marker])?;
    w.write_all(text)?;
    if text.last() != Some(&b'\n') {
        w.write_all(b"\n\\ No newline at end of file\n")?;
    }
    Ok(())
}

fn write_hunks(w: &mut impl Write, ld: &LineDiff<'_>, opts: &DiffOptions) -> Result<()> {
    let mut funcs = line::FuncTracker::new();
    for g in ld.groups(opts.context, opts.inter_hunk) {
        let func = funcs.next(&ld.old, g.old.start);
        write_hunk_header(w, &g, func)?;
        let mut pos = g.old.start;
        for h in &ld.hunks[g.hunks.clone()] {
            let (b0, b1) = (h.before.start as usize, h.before.end as usize);
            for l in &ld.old[pos..b0] {
                emit_line(w, b' ', l)?;
            }
            for l in &ld.old[b0..b1] {
                emit_line(w, b'-', l)?;
            }
            for l in &ld.new[h.after.start as usize..h.after.end as usize] {
                emit_line(w, b'+', l)?;
            }
            pos = b1;
        }
        for l in &ld.old[pos..g.old.end] {
            emit_line(w, b' ', l)?;
        }
    }
    Ok(())
}

/// Writes the word diff (`--word-diff=plain`) for `changes`.
pub async fn write_word_diff(
    w: &mut impl Write,
    ctx: &DiffCtx<'_>,
    changes: &[TreeChange],
) -> Result<()> {
    for c in changes {
        write_change(w, ctx, c, true).await?;
    }
    Ok(())
}
