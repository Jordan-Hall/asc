//! The diff `add -p` walks (SPEC §9.5): one file's staged content against its work tree content,
//! as git's `diff-files -p` shows it, cut into hunks that can be shown, split, edited and
//! applied to the staged content. Pure data: no prompting and no I/O.

use std::ops::Range;

use asc_core::id::GitOid;
use asc_engine::diff::line::{self, Algorithm, FuncTracker};

/// Largest side `add -p` diffs in memory (the inline object limit, SPEC §2.4). A larger file is
/// not offered hunk by hunk; `add <path>` stages it whole.
pub const MAX_SIDE_BYTES: u64 = asc_core::content::CHUNK_THRESHOLD;

/// Most lines a side may have: each line is held on its own, so the bound keeps a file of empty
/// lines from costing more memory than its size says.
pub const MAX_SIDE_LINES: usize = 1 << 18;

/// Most bytes an edited hunk may have; a longer edit is refused as one that does not apply.
pub const MAX_EDIT_BYTES: usize = 4 << 20;

/// Width git keeps for a hunk summary line (`term_columns` of a non-terminal).
const SUMMARY_COLUMNS: usize = 80;

/// What the user decided about one hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Undecided,
    Use,
    Skip,
}

/// The diff settings `add -p` reads (`diff.*`).
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub context: usize,
    pub inter_hunk: usize,
    pub algorithm: Algorithm,
    pub indent_heuristic: bool,
}

/// How the file changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Modified,
    Added,
    Deleted,
}

/// One side of the comparison.
pub struct Side {
    pub content: Vec<u8>,
    pub mode: u32,
    /// `None` is the null id (an absent file, or an intent-to-add entry).
    pub oid: Option<GitOid>,
}

/// A run of changed lines: the base lines it removes and the work tree lines that replace them.
#[derive(Clone, Debug)]
struct Run {
    before: Range<usize>,
    after: Range<usize>,
}

/// One line of an edited hunk: its marker (` `, `+` or `-`) and text, newline included unless
/// the line had git's "no newline" notice.
#[derive(Clone, Debug)]
struct EdLine {
    marker: u8,
    text: Vec<u8>,
}

/// A hunk the user rewrote.
#[derive(Clone, Debug)]
pub struct Edited {
    lines: Vec<EdLine>,
    /// Where the edited hunk starts on each side, as `@@` headers count (0-based line index).
    old_start: usize,
    new_start: usize,
    /// The header said the old side starts at line 0 or 1 (git's "must match at the beginning").
    at_top: bool,
}

#[derive(Clone, Debug)]
pub enum Part {
    /// The mode change git shows as the first hunk of a file.
    Mode { old: u32, new: u32 },
    Text {
        /// Indices into the file's runs.
        runs: Range<usize>,
        /// Base and work tree lines the hunk shows, context included.
        old: Range<usize>,
        new: Range<usize>,
        /// Function context text of the `@@` line.
        extra: Option<Vec<u8>>,
        edited: Option<Edited>,
    },
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub part: Part,
    pub decision: Decision,
}

/// The edited hunk does not apply to the staged content.
#[derive(Debug, PartialEq, Eq)]
pub struct NoApply;

/// What an edit session produced.
#[derive(Debug, PartialEq, Eq)]
pub enum EditResult {
    /// Every line was removed: the edit is abandoned and the hunk stays as it was.
    Abandoned,
    Applied,
}

/// One file's diff and the decisions about its hunks.
pub struct FileDiff {
    pub shape: Shape,
    header: Vec<u8>,
    pub hunks: Vec<Hunk>,
    /// The decision for a file that has no hunk (an empty added file).
    pub head: Decision,
    /// The repository path, for messages.
    pub path: Vec<u8>,
    old: Vec<Vec<u8>>,
    new: Vec<Vec<u8>>,
    runs: Vec<Run>,
}

/// git's `quote_c_style` with `core.quotepath=true`.
fn quote(name: &[u8]) -> Vec<u8> {
    let special = |b: u8| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\' || b >= 0x80;
    if !name.iter().any(|&b| special(b)) {
        return name.to_vec();
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
    out
}

/// Hunk header range: `start[,count]`, where an empty range names the line before it.
fn range_text(start: usize, count: usize) -> String {
    let first = if count == 0 { start } else { start + 1 };
    if count == 1 { format!("{first}") } else { format!("{first},{count}") }
}

fn abbrev(oid: Option<&GitOid>, digits: usize) -> String {
    let full = oid.map_or_else(|| "0".repeat(40), ToString::to_string);
    full[..digits.min(40)].to_string()
}

/// A line with its marker; a missing final newline gets git's notice.
fn emit(out: &mut Vec<u8>, marker: u8, text: &[u8]) {
    out.push(marker);
    out.extend_from_slice(text);
    if text.last() != Some(&b'\n') {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

/// Builds the diff of `old` (the staged side) against `new` (the work tree side). `a` and `b`
/// are the display names, prefix included. `None` for a side means the file is absent there.
/// Binary content is the caller's to refuse.
pub fn build(
    a: &[u8],
    b: &[u8],
    old: Option<Side>,
    new: Option<Side>,
    digits: usize,
    s: &Settings,
) -> FileDiff {
    let shape = match (&old, &new) {
        (None, _) => Shape::Added,
        (_, None) => Shape::Deleted,
        _ => Shape::Modified,
    };
    let (old_bytes, new_bytes) = (
        old.as_ref().map_or(&[][..], |x| x.content.as_slice()),
        new.as_ref().map_or(&[][..], |x| x.content.as_slice()),
    );
    let ld = line::diff(old_bytes, new_bytes, s.algorithm, s.indent_heuristic);
    let (old_lines, new_lines): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (
        ld.old.iter().map(|l| l.to_vec()).collect(),
        ld.new.iter().map(|l| l.to_vec()).collect(),
    );
    let runs: Vec<Run> = ld
        .hunks
        .iter()
        .map(|h| Run {
            before: h.before.start as usize..h.before.end as usize,
            after: h.after.start as usize..h.after.end as usize,
        })
        .collect();

    let mut hunks = Vec::new();
    let modes = (old.as_ref().map(|x| x.mode), new.as_ref().map(|x| x.mode));
    let mode_change = matches!(modes, (Some(o), Some(n)) if o != n);
    if let (true, (Some(o), Some(n))) = (mode_change, modes) {
        hunks.push(Hunk { part: Part::Mode { old: o, new: n }, decision: Decision::Undecided });
    }
    let mut funcs = FuncTracker::new();
    for g in ld.groups(s.context, s.inter_hunk) {
        let extra = funcs.next(&ld.old, g.old.start).map(<[u8]>::to_vec);
        hunks.push(Hunk {
            part: Part::Text { runs: g.hunks, old: g.old, new: g.new, extra, edited: None },
            decision: Decision::Undecided,
        });
    }

    let mut header = Vec::new();
    header.extend_from_slice(b"diff --git ");
    header.extend_from_slice(&quote(a));
    header.push(b' ');
    header.extend_from_slice(&quote(b));
    header.push(b'\n');
    let mode = |m: Option<u32>| m.unwrap_or(0);
    match shape {
        Shape::Added => header.extend_from_slice(format!("new file mode {:06o}\n", mode(modes.1)).as_bytes()),
        Shape::Deleted => header.extend_from_slice(format!("deleted file mode {:06o}\n", mode(modes.0)).as_bytes()),
        Shape::Modified => {}
    }
    let (og, ng) = (old.as_ref().and_then(|x| x.oid.as_ref()), new.as_ref().and_then(|x| x.oid.as_ref()));
    if og != ng {
        header.extend_from_slice(format!("index {}..{}", abbrev(og, digits), abbrev(ng, digits)).as_bytes());
        if modes.0 == modes.1 {
            header.extend_from_slice(format!(" {:06o}", mode(modes.1)).as_bytes());
        }
        header.push(b'\n');
    }
    if !runs.is_empty() {
        // Git appends a tab to real names that contain a space, never to /dev/null.
        let label = |present: bool, name: &[u8]| -> Vec<u8> {
            if !present {
                return b"/dev/null".to_vec();
            }
            let mut q = quote(name);
            if name.contains(&b' ') {
                q.push(b'\t');
            }
            q
        };
        header.extend_from_slice(b"--- ");
        header.extend_from_slice(&label(old.is_some(), a));
        header.extend_from_slice(b"\n+++ ");
        header.extend_from_slice(&label(new.is_some(), b));
        header.push(b'\n');
    }
    FileDiff { shape, header, hunks, head: Decision::Undecided, path: Vec::new(), old: old_lines, new: new_lines, runs }
}

impl FileDiff {
    /// The text shown before the first hunk.
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// Whether the first hunk is the mode change.
    pub fn mode_change(&self) -> bool {
        matches!(self.hunks.first(), Some(Hunk { part: Part::Mode { .. }, .. }))
    }

    /// Whether the file offers anything to stage: an empty file added or deleted is one prompt
    /// without a hunk.
    pub fn offered(&self) -> bool {
        self.shape != Shape::Modified || !self.hunks.is_empty()
    }

    /// How many hunks `splitting` hunk `h` gives: the number of change runs it holds.
    pub fn splittable(&self, h: usize) -> usize {
        match &self.hunks[h].part {
            Part::Text { runs, edited: None, .. } => runs.len(),
            _ => 1,
        }
    }

    /// The line of the staged content hunk `h` starts at (1-based), for messages.
    pub fn old_line(&self, h: usize) -> usize {
        self.ranges(h).0 + 1
    }

    pub fn is_edited(&self, h: usize) -> bool {
        matches!(&self.hunks[h].part, Part::Text { edited: Some(_), .. })
    }

    /// Replaces hunk `h` by one hunk per change run; the context between two runs belongs to
    /// both, as in git. Returns how many hunks there are now.
    pub fn split(&mut self, h: usize) -> usize {
        let Hunk { part: Part::Text { runs, old, new, extra, edited: None }, decision } = self.hunks[h].clone()
        else {
            return 1;
        };
        let n = runs.len();
        let pieces: Vec<Hunk> = runs
            .clone()
            .enumerate()
            .map(|(k, r)| {
                let (first, last) = (k == 0, k + 1 == n);
                let o = if first { old.start } else { self.runs[r - 1].before.end }
                    ..if last { old.end } else { self.runs[r + 1].before.start };
                let w = if first { new.start } else { self.runs[r - 1].after.end }
                    ..if last { new.end } else { self.runs[r + 1].after.start };
                Hunk {
                    part: Part::Text {
                        runs: r..r + 1,
                        old: o,
                        new: w,
                        extra: if first { extra.clone() } else { None },
                        edited: None,
                    },
                    decision,
                }
            })
            .collect();
        self.hunks.splice(h..=h, pieces);
        n
    }

    /// The lines of hunk `h` with their markers.
    fn body(&self, h: usize) -> Vec<(u8, &[u8])> {
        let Part::Text { runs, old, new: _, edited, .. } = &self.hunks[h].part else {
            return Vec::new();
        };
        if let Some(e) = edited {
            return e.lines.iter().map(|l| (l.marker, l.text.as_slice())).collect();
        }
        let mut out = Vec::new();
        let mut pos = old.start;
        for r in &self.runs[runs.clone()] {
            out.extend(self.old[pos..r.before.start].iter().map(|l| (b' ', l.as_slice())));
            out.extend(self.old[r.before.clone()].iter().map(|l| (b'-', l.as_slice())));
            out.extend(self.new[r.after.clone()].iter().map(|l| (b'+', l.as_slice())));
            pos = r.before.end;
        }
        out.extend(self.old[pos..old.end].iter().map(|l| (b' ', l.as_slice())));
        out
    }

    /// `(old start, old count, new start, new count)` of the hunk header.
    fn ranges(&self, h: usize) -> (usize, usize, usize, usize) {
        match &self.hunks[h].part {
            Part::Text { old, new, edited: None, .. } => (old.start, old.len(), new.start, new.len()),
            Part::Text { edited: Some(e), .. } => {
                let count = |keep: &[u8]| e.lines.iter().filter(|l| keep.contains(&l.marker)).count();
                (e.old_start, count(b" -"), e.new_start, count(b" +"))
            }
            Part::Mode { .. } => (0, 0, 0, 0),
        }
    }

    /// Hunk `h` as git shows it: the `@@` line and the lines, or the mode lines.
    pub fn render(&self, h: usize) -> Vec<u8> {
        let mut out = Vec::new();
        match &self.hunks[h].part {
            Part::Mode { old, new } => {
                out.extend_from_slice(format!("old mode {old:06o}\nnew mode {new:06o}\n").as_bytes());
            }
            Part::Text { extra, .. } => {
                let (os, oc, ns, nc) = self.ranges(h);
                out.extend_from_slice(format!("@@ -{} +{} @@", range_text(os, oc), range_text(ns, nc)).as_bytes());
                if let Some(x) = extra {
                    out.push(b' ');
                    out.extend_from_slice(x);
                }
                out.push(b'\n');
                for (marker, text) in self.body(h) {
                    emit(&mut out, marker, text);
                }
            }
        }
        out
    }

    /// The line `g` lists for hunk `h`: its ranges and its first changed line.
    pub fn summary(&self, h: usize) -> Vec<u8> {
        let (os, oc, ns, nc) = self.ranges(h);
        let mut out = format!(" -{} +{}", range_text(os, oc), range_text(ns, nc)).into_bytes();
        if out.len() < 20 {
            out.resize(20, b' ');
        }
        if let Some((marker, text)) = self.body(h).into_iter().find(|(m, _)| *m != b' ') {
            out.push(marker);
            out.extend_from_slice(text.strip_suffix(b"\n").unwrap_or(text));
        }
        out.truncate(SUMMARY_COLUMNS);
        out
    }

    /// The file the editor opens for hunk `h`.
    pub fn edit_text(&self, h: usize) -> Vec<u8> {
        let mut out = b"# Manual hunk edit mode -- see bottom for a quick guide.\n".to_vec();
        out.extend_from_slice(&self.render(h));
        out.extend_from_slice(
            b"# ---\n\
              # To remove '-' lines, make them ' ' lines (context).\n\
              # To remove '+' lines, delete them.\n\
              # Lines starting with # will be removed.\n\
              # If the patch applies cleanly, the edited hunk will immediately be marked for staging.\n\
              # If it does not apply cleanly, you will be given an opportunity to\n\
              # edit again.  If all lines of the hunk are removed, then the edit is\n\
              # aborted and the hunk is left unchanged.\n",
        );
        out
    }

    /// Takes the editor's result for hunk `h`. The edit is checked against the staged content
    /// (`git apply --cached --recount`'s rules): on `Err` the hunk is unchanged.
    pub fn set_edit(&mut self, h: usize, text: &[u8]) -> Result<EditResult, NoApply> {
        if text.len() > MAX_EDIT_BYTES {
            return Err(NoApply);
        }
        let kept: Vec<&[u8]> = split_inclusive(text).into_iter().filter(|l| l.first() != Some(&b'#')).collect();
        if kept.is_empty() {
            return Ok(EditResult::Abandoned);
        }
        let edited = parse_edit(&kept).ok_or(NoApply)?;
        let before = self.hunks[h].part.clone();
        let Part::Text { edited: slot, .. } = &mut self.hunks[h].part else {
            return Err(NoApply);
        };
        *slot = Some(edited);
        if self.replacement(h).is_none() {
            self.hunks[h].part = before;
            return Err(NoApply);
        }
        Ok(EditResult::Applied)
    }

    /// Where hunk `h` edits the base: the lines it replaces and what replaces them. `None` when
    /// the edit's old side is not in the base where git would find it, or reaches into a change
    /// of another hunk.
    fn replacement(&self, h: usize) -> Option<(Range<usize>, Vec<Vec<u8>>)> {
        let Part::Text { runs, edited: Some(e), .. } = &self.hunks[h].part else {
            return None;
        };
        let seq = |keep: &[u8]| -> Vec<&[u8]> {
            e.lines.iter().filter(|l| keep.contains(&l.marker)).map(|l| l.text.as_slice()).collect()
        };
        let (want, put) = (seq(b" -"), seq(b" +"));
        let trailing = e.lines.iter().rev().take_while(|l| l.marker == b' ').count();
        let hi = self.old.len();
        let fits = |p: usize| {
            p + want.len() <= hi
                && self.old[p..p + want.len()].iter().zip(&want).all(|(a, b)| a.as_slice() == *b)
                && (!e.at_top || p == 0)
                && (trailing != 0 || p + want.len() == hi)
        };
        // Nearest to where the header says, looking backwards first (git's search order).
        let expected = e.old_start.min(hi);
        let pos = (0..=hi).find_map(|d| {
            expected
                .checked_sub(d)
                .filter(|&p| fits(p))
                .or_else(|| Some(expected + d).filter(|&p| p <= hi && fits(p)))
        })?;
        let range = pos..pos + want.len();
        // The edit may not touch a change that belongs to another hunk.
        let foreign = self.runs.iter().enumerate().filter(|(i, _)| !runs.contains(i));
        if foreign.into_iter().any(|(_, r)| overlap(&range, &r.before)) {
            return None;
        }
        Some((range, put.into_iter().map(<[u8]>::to_vec).collect()))
    }

    /// The staged content the hunks apply to.
    pub fn base(&self) -> Vec<u8> {
        self.old.concat()
    }

    pub fn mode_selected(&self) -> bool {
        self.mode_change() && self.hunks[0].decision == Decision::Use
    }

    /// Whether any hunk or the head is to be staged.
    pub fn any_used(&self) -> bool {
        self.head == Decision::Use || self.hunks.iter().any(|h| h.decision == Decision::Use)
    }

    /// The staged content with the chosen hunks applied.
    pub fn assemble(&self) -> Result<Vec<u8>, NoApply> {
        let mut edits: Vec<(Range<usize>, Vec<Vec<u8>>)> = Vec::new();
        for (i, h) in self.hunks.iter().enumerate() {
            let Part::Text { runs, edited, .. } = &h.part else { continue };
            if h.decision != Decision::Use {
                continue;
            }
            if edited.is_some() {
                edits.push(self.replacement(i).ok_or(NoApply)?);
            } else {
                for r in &self.runs[runs.clone()] {
                    edits.push((r.before.clone(), self.new[r.after.clone()].to_vec()));
                }
            }
        }
        // Hunks share context, never a change: an overlap is an edit gone wrong.
        edits.sort_by(|a, b| (a.0.start, a.0.end).cmp(&(b.0.start, b.0.end)));
        let mut out = Vec::new();
        let mut pos = 0;
        for (range, lines) in &edits {
            if range.start < pos {
                return Err(NoApply);
            }
            for l in &self.old[pos..range.start] {
                out.extend_from_slice(l);
            }
            for l in lines {
                out.extend_from_slice(l);
            }
            pos = range.end;
        }
        for l in &self.old[pos..] {
            out.extend_from_slice(l);
        }
        Ok(out)
    }
}

/// Whether two replaced ranges of lines clash: they share a line, or one is an insertion point
/// inside the other.
fn overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    match (a.is_empty(), b.is_empty()) {
        (false, false) => a.start < b.end && b.start < a.end,
        (true, false) => b.start < a.start && a.start < b.end,
        (false, true) => a.start < b.start && b.start < a.end,
        (true, true) => a.start == b.start,
    }
}

/// `text` cut after each `\n`.
fn split_inclusive(text: &[u8]) -> Vec<&[u8]> {
    text.split_inclusive(|&b| b == b'\n').collect()
}

/// Reads a hunk back from the editor's lines: the `@@` line, then lines with markers.
fn parse_edit(lines: &[&[u8]]) -> Option<Edited> {
    let (head, rest) = lines.split_first()?;
    let head = std::str::from_utf8(head).ok()?.strip_prefix("@@ -")?;
    let mut parts = head.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?.strip_prefix('+')?;
    let start = |s: &str| -> Option<usize> { s.split(',').next()?.parse().ok() };
    let (a, c) = (start(old)?, start(new)?);
    // The header start names the first line (1-based); the line before it when the side is empty.
    let mut body: Vec<EdLine> = Vec::new();
    for raw in rest {
        match raw.first() {
            Some(b'\\') => {
                let last = body.last_mut()?;
                if last.text.last() == Some(&b'\n') {
                    last.text.pop();
                }
            }
            Some(&m @ (b' ' | b'+' | b'-')) => body.push(EdLine { marker: m, text: raw[1..].to_vec() }),
            Some(b'\n') => body.push(EdLine { marker: b' ', text: raw.to_vec() }),
            _ => return None,
        }
    }
    if body.is_empty() {
        return None;
    }
    let old_count = body.iter().filter(|l| l.marker != b'+').count();
    let new_count = body.iter().filter(|l| l.marker != b'-').count();
    let zero = |n: usize, count: usize| if count == 0 { n } else { n.saturating_sub(1) };
    Some(Edited { old_start: zero(a, old_count), new_start: zero(c, new_count), at_top: a <= 1, lines: body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings { context: 3, inter_hunk: 0, algorithm: Algorithm::Myers, indent_heuristic: true }
    }

    fn side(text: &str) -> Option<Side> {
        let oid = asc_core::git::blob_oid(text.as_bytes());
        Some(Side { content: text.as_bytes().to_vec(), mode: 0o100644, oid: Some(oid) })
    }

    fn numbered(n: usize, change: &[usize]) -> String {
        (1..=n)
            .map(|i| if change.contains(&i) { format!("new{i}\n") } else { format!("{i}\n") })
            .collect()
    }

    fn diff(old: &str, new: &str) -> FileDiff {
        build(b"a/f", b"b/f", side(old), side(new), 7, &settings())
    }

    #[test]
    fn renders_like_git() {
        let d = diff(&numbered(12, &[]), &numbered(12, &[5]));
        assert_eq!(d.hunks.len(), 1);
        let want = "@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+new5\n 6\n 7\n 8\n";
        assert_eq!(String::from_utf8(d.render(0)).unwrap(), want);
        let header = String::from_utf8(d.header().to_vec()).unwrap();
        assert!(header.starts_with("diff --git a/f b/f\nindex "), "{header}");
        assert!(header.ends_with(" 100644\n--- a/f\n+++ b/f\n") || header.ends_with("\n--- a/f\n+++ b/f\n"));
    }

    #[test]
    fn split_shares_the_context_between_changes() {
        let mut d = diff(&numbered(14, &[]), &numbered(14, &[4, 8]));
        assert_eq!((d.hunks.len(), d.splittable(0)), (1, 2));
        assert_eq!(d.split(0), 2);
        assert_eq!(d.hunks.len(), 2);
        let first = String::from_utf8(d.render(0)).unwrap();
        let second = String::from_utf8(d.render(1)).unwrap();
        assert_eq!(first, "@@ -1,7 +1,7 @@\n 1\n 2\n 3\n-4\n+new4\n 5\n 6\n 7\n");
        assert_eq!(second, "@@ -5,7 +5,7 @@\n 5\n 6\n 7\n-8\n+new8\n 9\n 10\n 11\n");
        assert_eq!(d.splittable(0), 1);
    }

    #[test]
    fn chosen_hunks_apply_to_the_staged_content() {
        let old = numbered(30, &[]);
        let mut d = diff(&old, &numbered(30, &[3, 25]));
        assert_eq!(d.hunks.len(), 2);
        d.hunks[1].decision = Decision::Use;
        assert_eq!(String::from_utf8(d.assemble().unwrap()).unwrap(), numbered(30, &[25]));
        d.hunks[0].decision = Decision::Use;
        assert_eq!(String::from_utf8(d.assemble().unwrap()).unwrap(), numbered(30, &[3, 25]));
    }

    #[test]
    fn a_split_hunk_applies_one_change_at_a_time() {
        let mut d = diff(&numbered(14, &[]), &numbered(14, &[4, 8]));
        d.split(0);
        d.hunks[1].decision = Decision::Use;
        assert_eq!(String::from_utf8(d.assemble().unwrap()).unwrap(), numbered(14, &[8]));
    }

    #[test]
    fn missing_final_newline_is_kept_per_line() {
        let mut d = diff("one\ntwo", "one\ntwo\nthree");
        d.hunks[0].decision = Decision::Use;
        assert_eq!(d.assemble().unwrap(), b"one\ntwo\nthree");
        let text = String::from_utf8(d.render(0)).unwrap();
        assert!(text.contains("-two\n\\ No newline at end of file\n+two\n+three\n\\ No newline"), "{text}");
    }

    #[test]
    fn an_edit_replaces_the_hunk_and_must_apply() {
        let mut d = diff(&numbered(12, &[]), &numbered(12, &[5]));
        let shown = String::from_utf8(d.edit_text(0)).unwrap();
        assert!(shown.starts_with("# Manual hunk edit mode"), "{shown}");
        let edited = shown.replace("+new5\n", "+edited\n+more\n");
        assert_eq!(d.set_edit(0, edited.as_bytes()), Ok(EditResult::Applied));
        d.hunks[0].decision = Decision::Use;
        let got = String::from_utf8(d.assemble().unwrap()).unwrap();
        assert_eq!(got, numbered(12, &[5]).replace("new5\n", "edited\nmore\n"));
        assert!(String::from_utf8(d.render(0)).unwrap().starts_with("@@ -2,7 +2,8 @@\n"));
        // Context that is not in the file does not apply, and leaves the hunk as it was.
        let bad = edited.replace(" 3\n", " three\n");
        assert_eq!(d.set_edit(0, bad.as_bytes()), Err(NoApply));
        assert!(String::from_utf8(d.render(0)).unwrap().contains("+edited"));
    }

    #[test]
    fn removing_every_line_abandons_the_edit() {
        let mut d = diff(&numbered(12, &[]), &numbered(12, &[5]));
        assert_eq!(d.set_edit(0, b"# nothing left\n"), Ok(EditResult::Abandoned));
        assert!(!d.is_edited(0));
    }

    #[test]
    fn an_edit_without_trailing_context_must_reach_the_end() {
        let mut d = diff(&numbered(12, &[]), &numbered(12, &[5]));
        let shown = String::from_utf8(d.edit_text(0)).unwrap();
        let cut = shown.replace(" 6\n 7\n 8\n", "");
        assert_eq!(d.set_edit(0, cut.as_bytes()), Err(NoApply));
    }

    #[test]
    fn summary_pads_the_header_and_names_the_first_change() {
        let d = diff(&numbered(12, &[]), &numbered(12, &[5]));
        assert_eq!(String::from_utf8(d.summary(0)).unwrap(), " -2,7 +2,7          -5");
    }

    #[test]
    fn mode_change_is_the_first_hunk_and_leaves_the_header_bare() {
        let mut d = build(
            b"a/f",
            b"b/f",
            Some(Side { content: b"x\n".to_vec(), mode: 0o100755, oid: Some(asc_core::git::blob_oid(b"x\n")) }),
            Some(Side { content: b"x\n".to_vec(), mode: 0o100644, oid: Some(asc_core::git::blob_oid(b"x\n")) }),
            7,
            &settings(),
        );
        assert!(d.mode_change() && d.hunks.len() == 1);
        assert_eq!(d.header(), b"diff --git a/f b/f\n");
        assert_eq!(d.render(0), b"old mode 100755\nnew mode 100644\n");
        d.hunks[0].decision = Decision::Use;
        assert!(d.mode_selected());
    }

    #[test]
    fn empty_files_follow_git() {
        let added = build(b"a/f", b"b/f", None, side(""), 7, &settings());
        assert!(added.offered() && added.hunks.is_empty());
        assert_eq!(added.header(), b"diff --git a/f b/f\nnew file mode 100644\nindex 0000000..e69de29\n");
        let deleted = build(b"a/f", b"b/f", side(""), None, 7, &settings());
        assert!(deleted.offered() && deleted.hunks.is_empty());
    }
}
