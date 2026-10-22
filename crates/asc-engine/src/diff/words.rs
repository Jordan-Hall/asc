//! `--word-diff=plain`: removed words as `[-word-]`, added words as `{+word+}`.
//!
//! Hunks come from the line diff. Each run of removed and added lines is then diffed again by
//! whitespace-separated words, and context lines pass through unchanged.

use std::io::Write;

use super::line::{self, Algorithm, LineDiff};
use super::patch::write_hunk_header;
use super::{DiffOptions, Result};

pub(crate) fn write_hunks(w: &mut impl Write, ld: &LineDiff<'_>, opts: &DiffOptions) -> Result<()> {
    let mut funcs = line::FuncTracker::new();
    for g in ld.groups(opts.context, opts.inter_hunk) {
        let func = funcs.next(&ld.old, g.old.start);
        write_hunk_header(w, &g, func)?;
        let mut pos = g.old.start;
        let (mut minus, mut plus) = (Vec::new(), Vec::new());
        for h in &ld.hunks[g.hunks.clone()] {
            let (b0, b1) = (h.before.start as usize, h.before.end as usize);
            for l in &ld.old[pos..b0] {
                flush(w, &mut minus, &mut plus)?;
                context(w, l)?;
            }
            for l in &ld.old[b0..b1] {
                minus.extend_from_slice(l);
            }
            for l in &ld.new[h.after.start as usize..h.after.end as usize] {
                plus.extend_from_slice(l);
            }
            pos = b1;
        }
        flush(w, &mut minus, &mut plus)?;
        for l in &ld.old[pos..g.old.end] {
            context(w, l)?;
        }
    }
    Ok(())
}

fn context(w: &mut impl Write, line: &[u8]) -> Result<()> {
    w.write_all(line)?;
    if line.last() != Some(&b'\n') {
        w.write_all(b"\n")?;
    }
    Ok(())
}

/// Writes `text` with `open`/`close` around the non-empty part of each line.
fn wrapped(w: &mut impl Write, open: &[u8], close: &[u8], text: &[u8]) -> Result<()> {
    let mut rest = text;
    while !rest.is_empty() {
        let (seg, nl) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        if !seg.is_empty() {
            w.write_all(open)?;
            w.write_all(seg)?;
            w.write_all(close)?;
        }
        if nl {
            w.write_all(b"\n")?;
        }
        rest = &rest[(seg.len() + usize::from(nl))..];
    }
    Ok(())
}

/// Byte ranges of the whitespace-separated words of `text`.
fn words(text: &[u8]) -> Vec<(usize, usize)> {
    let space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut out = Vec::new();
    let mut start = None;
    for (i, &b) in text.iter().enumerate() {
        match (space(b), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

/// Emits and clears the pending removed and added text.
fn flush(w: &mut impl Write, minus: &mut Vec<u8>, plus: &mut Vec<u8>) -> Result<()> {
    let (m, p) = (std::mem::take(minus), std::mem::take(plus));
    if m.is_empty() && p.is_empty() {
        return Ok(());
    }
    if p.is_empty() {
        wrapped(w, b"[-", b"-]", &m)?;
    } else if m.is_empty() {
        wrapped(w, b"{+", b"+}", &p)?;
    } else {
        word_diff(w, &m, &p)?;
    }
    Ok(())
}

fn word_diff(w: &mut impl Write, minus: &[u8], plus: &[u8]) -> Result<()> {
    let (mw, pw) = (words(minus), words(plus));
    let one_per_line = |text: &[u8], ws: &[(usize, usize)]| -> Vec<u8> {
        ws.iter()
            .flat_map(|&(b, e)| text[b..e].iter().copied().chain([b'\n']))
            .collect()
    };
    let (ma, pa) = (one_per_line(minus, &mw), one_per_line(plus, &pw));
    let ld = line::diff(&ma, &pa, Algorithm::Myers, false);
    let mut current = 0;
    for h in &ld.hunks {
        let (m0, m1) = (h.before.start as usize, h.before.end as usize);
        let (p0, p1) = (h.after.start as usize, h.after.end as usize);
        let (p_begin, p_end) = if p1 > p0 {
            (pw[p0].0, pw[p1 - 1].1)
        } else {
            // A pure removal sits right after the previous word.
            let at = if p0 > 0 { pw[p0 - 1].1 } else { 0 };
            (at, at)
        };
        if current != p_begin {
            w.write_all(&plus[current..p_begin])?;
        }
        if m1 > m0 {
            wrapped(w, b"[-", b"-]", &minus[mw[m0].0..mw[m1 - 1].1])?;
        }
        if p1 > p0 {
            wrapped(w, b"{+", b"+}", &plus[p_begin..p_end])?;
        }
        current = p_end;
    }
    w.write_all(&plus[current..])?;
    if plus.last() != Some(&b'\n') {
        w.write_all(b"\n")?;
    }
    Ok(())
}
