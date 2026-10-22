//! `--stat`, `--numstat` and `--shortstat`, with git's width rules.

use std::io::Write;

use super::format::{names, quote_c_style, rename_display};
use super::line;
use super::patch::{DiffCtx, Loaded, load};
use super::{Result, TreeChange};

/// Line counts of one file, or byte sizes for binary content.
struct FileStat {
    /// Name as `--stat` and `--numstat` print it (rename form for renames, C-quoted).
    shown: Vec<u8>,
    old: Vec<u8>,
    new: Vec<u8>,
    renamed: bool,
    added: u64,
    deleted: u64,
    binary: bool,
}

async fn collect(ctx: &DiffCtx<'_>, changes: &[TreeChange]) -> Result<Vec<FileStat>> {
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        let Some(n) = names(ctx.opts, c) else {
            continue;
        };
        let renamed = c.status.score().is_some();
        let shown = if renamed {
            rename_display(&n.old, &n.new)
        } else {
            n.new.clone()
        };
        let (added, deleted, binary) = match load(ctx, c).await? {
            Loaded::Same => (0, 0, false),
            Loaded::Binary => (
                c.new.as_ref().map_or(0, |e| e.size),
                c.old.as_ref().map_or(0, |e| e.size),
                true,
            ),
            Loaded::Text { old, new } => {
                let ld = line::diff(&old, &new, ctx.opts.algorithm, ctx.opts.indent_heuristic);
                let (a, d) = ld.counts();
                (a as u64, d as u64, false)
            }
        };
        out.push(FileStat {
            shown: quote_c_style(&shown),
            old: n.old,
            new: n.new,
            renamed,
            added,
            deleted,
            binary,
        });
    }
    Ok(out)
}

fn digits(n: u64) -> usize {
    n.to_string().len()
}

fn scale(it: u64, width: usize, max_change: u64) -> usize {
    if it == 0 {
        0
    } else {
        1 + (it as usize * (width - 1)) / max_change as usize
    }
}

fn summary_line(w: &mut impl Write, files: &[FileStat]) -> Result<()> {
    // Binary files count as changed files but contribute no lines.
    let lines =
        |pick: fn(&FileStat) -> u64| files.iter().filter(|f| !f.binary).map(pick).sum::<u64>();
    let adds = lines(|f| f.added);
    let dels = lines(|f| f.deleted);
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    write!(
        w,
        " {} file{} changed",
        files.len(),
        plural(files.len() as u64)
    )?;
    if adds > 0 || dels == 0 {
        write!(w, ", {adds} insertion{}(+)", plural(adds))?;
    }
    if dels > 0 || adds == 0 {
        write!(w, ", {dels} deletion{}(-)", plural(dels))?;
    }
    w.write_all(b"\n")?;
    Ok(())
}

/// `--shortstat`.
pub async fn write_shortstat(
    w: &mut impl Write,
    ctx: &DiffCtx<'_>,
    changes: &[TreeChange],
) -> Result<()> {
    let files = collect(ctx, changes).await?;
    if !files.is_empty() {
        summary_line(w, &files)?;
    }
    Ok(())
}

/// `--numstat`.
pub async fn write_numstat(
    w: &mut impl Write,
    ctx: &DiffCtx<'_>,
    changes: &[TreeChange],
) -> Result<()> {
    for f in collect(ctx, changes).await? {
        if f.binary {
            w.write_all(b"-\t-\t")?;
        } else {
            write!(w, "{}\t{}\t", f.added, f.deleted)?;
        }
        if ctx.opts.nul {
            if f.renamed {
                w.write_all(b"\0")?;
                w.write_all(&f.old)?;
                w.write_all(b"\0")?;
            }
            w.write_all(&f.new)?;
            w.write_all(b"\0")?;
        } else {
            w.write_all(&f.shown)?;
            w.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// `--stat[=<width>[,<name-width>[,<count>]]]`: a name column, a count and a scaled graph.
pub async fn write_stat(
    w: &mut impl Write,
    ctx: &DiffCtx<'_>,
    changes: &[TreeChange],
) -> Result<()> {
    let files = collect(ctx, changes).await?;
    if files.is_empty() {
        return Ok(());
    }
    let opts = ctx.opts;
    let shown = opts
        .stat_count
        .filter(|&n| n > 0)
        .map_or(files.len(), |n| n.min(files.len()));
    let (mut max_len, mut max_change, mut number_width, mut bin_width) = (0, 0u64, 0, 0);
    for f in &files[..shown] {
        max_len = max_len.max(f.shown.len());
        if f.binary {
            bin_width = bin_width.max(14 + digits(f.added) + digits(f.deleted));
            number_width = number_width.max(3);
            continue;
        }
        max_change = max_change.max(f.added + f.deleted);
    }
    number_width = number_width.max(digits(max_change));

    let mut width = opts.stat_width.unwrap_or(80);
    if width < 16 + 6 + number_width {
        width = 16 + 6 + number_width;
    }
    let mut graph_width = if max_change as usize + 4 > bin_width {
        max_change as usize
    } else {
        bin_width - 4
    };
    if let Some(g) = opts.stat_graph_width.filter(|&g| g > 0 && g < graph_width) {
        graph_width = g;
    }
    let mut name_width = match opts.stat_name_width {
        Some(n) if n > 0 && n < max_len => n,
        _ => max_len,
    };
    if name_width + number_width + 6 + graph_width > width {
        let cap = (width * 3 / 8).saturating_sub(number_width + 6);
        if graph_width > cap {
            graph_width = cap.max(6);
        }
        if let Some(g) = opts.stat_graph_width.filter(|&g| g > 0 && graph_width > g) {
            graph_width = g;
        }
        let room = width.saturating_sub(number_width + 6 + graph_width);
        if name_width > room {
            name_width = room;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }

    for f in &files[..shown] {
        let (mut prefix, mut name, mut len) = (&b""[..], &f.shown[..], name_width);
        if name_width < name.len() {
            prefix = b"...";
            len = name_width.saturating_sub(3);
            name = &name[name.len() - len..];
            if let Some(slash) = name.iter().position(|&b| b == b'/') {
                name = &name[slash..];
            }
        }
        w.write_all(b" ")?;
        w.write_all(prefix)?;
        w.write_all(name)?;
        let pad = len.saturating_sub(name.len());
        write!(w, "{:pad$} |", "")?;
        if f.binary {
            write!(w, " {:>number_width$}", "Bin")?;
            if f.added == 0 && f.deleted == 0 {
                w.write_all(b"\n")?;
            } else {
                writeln!(w, " {} -> {} bytes", f.deleted, f.added)?;
            }
            continue;
        }
        let (mut add, del) = (f.added, f.deleted);
        let (mut plus, mut minus) = (add as usize, del as usize);
        if graph_width as u64 <= max_change {
            let mut total = scale(add + del, graph_width, max_change);
            if total < 2 && add > 0 && del > 0 {
                total = 2;
            }
            if add < del {
                plus = scale(add, graph_width, max_change);
                minus = total - plus;
            } else {
                minus = scale(del, graph_width, max_change);
                plus = total - minus;
            }
        }
        add += del;
        write!(w, " {add:>number_width$}{}", if add > 0 { " " } else { "" })?;
        w.write_all("+".repeat(plus).as_bytes())?;
        w.write_all("-".repeat(minus).as_bytes())?;
        w.write_all(b"\n")?;
    }
    if shown < files.len() {
        w.write_all(b" ...\n")?;
    }
    summary_line(w, &files)
}
