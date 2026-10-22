//! Line diff: algorithm choice, slider compaction with git's indent heuristic, hunk grouping and
//! function-context lookup (all following xdiff, so that output matches git byte for byte).

use std::ops::Range;
use std::str::FromStr;

use imara_diff::sources::byte_lines;
use imara_diff::{Algorithm as Imara, Diff, InternedInput, Token};

pub use imara_diff::Hunk;

use super::{patience, xdiff};

/// Bytes probed for NUL when deciding whether content is binary (git's `FIRST_FEW_BYTES`).
pub const BINARY_PROBE: usize = 8000;

/// Diff algorithm (`diff.algorithm`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Algorithm {
    #[default]
    Myers,
    Minimal,
    Histogram,
    Patience,
}

impl FromStr for Algorithm {
    type Err = String;

    /// Parses git's names: `myers`/`default`, `minimal`, `patience`, `histogram`.
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "myers" | "default" => Ok(Self::Myers),
            "minimal" => Ok(Self::Minimal),
            "patience" => Ok(Self::Patience),
            "histogram" => Ok(Self::Histogram),
            other => Err(format!("unknown diff algorithm '{other}'")),
        }
    }
}

/// Git's binary test: a NUL byte in the first [`BINARY_PROBE`] bytes.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_PROBE)].contains(&0)
}

/// Splits into lines, each keeping its `\n`; the last may lack one.
pub fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    byte_lines(data).collect()
}

/// A line diff: both sides split into lines plus the changed ranges between them.
pub struct LineDiff<'a> {
    pub old: Vec<&'a [u8]>,
    pub new: Vec<&'a [u8]>,
    pub hunks: Vec<Hunk>,
}

/// Diffs `old` against `new`. `indent` enables git's indent heuristic.
pub fn diff<'a>(old: &'a [u8], new: &'a [u8], algo: Algorithm, indent: bool) -> LineDiff<'a> {
    let input = InternedInput::new(old, new);
    let (n, m) = (input.before.len(), input.after.len());
    let (mut removed, mut added) = match algo {
        Algorithm::Patience => {
            let f = patience::diff(&input.before, &input.after);
            (f.removed, f.added)
        }
        Algorithm::Myers | Algorithm::Minimal => {
            let f = xdiff::diff(&input.before, &input.after, algo == Algorithm::Minimal);
            (f.removed, f.added)
        }
        Algorithm::Histogram => {
            let d = Diff::compute(Imara::Histogram, &input);
            (
                (0..n as u32).map(|i| d.is_removed(i)).collect::<Vec<_>>(),
                (0..m as u32).map(|i| d.is_added(i)).collect::<Vec<_>>(),
            )
        }
    };
    let ind = |toks: &[Token]| -> Vec<i32> {
        toks.iter().map(|&t| indent_of(input.interner[t])).collect()
    };
    let (ind_old, ind_new) = (ind(&input.before), ind(&input.after));
    let mut a = Side::new(&input.before, &mut removed, ind_old);
    let mut b = Side::new(&input.after, &mut added, ind_new);
    compact(&mut a, &mut b, indent);
    compact(&mut b, &mut a, indent);
    let (removed, added) = (a.into_flags(), b.into_flags());
    LineDiff {
        old: split_lines(old),
        new: split_lines(new),
        hunks: hunks_of(&removed, &added),
    }
}

/// Changed ranges from the two per-line flag vectors.
fn hunks_of(removed: &[bool], added: &[bool]) -> Vec<Hunk> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < removed.len() || j < added.len() {
        let r = removed[i.min(removed.len())..]
            .iter()
            .take_while(|c| **c)
            .count();
        let a = added[j.min(added.len())..]
            .iter()
            .take_while(|c| **c)
            .count();
        if r > 0 || a > 0 {
            out.push(Hunk {
                before: i as u32..(i + r) as u32,
                after: j as u32..(j + a) as u32,
            });
        }
        i += r;
        j += a;
        // One unchanged line on both sides.
        i += 1;
        j += 1;
    }
    out
}

impl LineDiff<'_> {
    /// Added and deleted line counts.
    pub fn counts(&self) -> (usize, usize) {
        self.hunks
            .iter()
            .fold((0, 0), |(a, d), h| (a + h.after.len(), d + h.before.len()))
    }

    /// Hunks merged into output groups with `ctx` lines of context. Changes closer than
    /// `2 * ctx + inter_hunk` unchanged lines share a group.
    pub fn groups(&self, ctx: usize, inter_hunk: usize) -> Vec<Group> {
        let max_common = 2 * ctx + inter_hunk;
        let mut out: Vec<Group> = Vec::new();
        for (idx, h) in self.hunks.iter().enumerate() {
            let (b0, b1) = (h.before.start as usize, h.before.end as usize);
            let (a0, a1) = (h.after.start as usize, h.after.end as usize);
            if let Some(g) = out.last_mut() {
                let prev_end = self.hunks[g.hunks.end - 1].before.end as usize;
                if b0 - prev_end <= max_common {
                    g.hunks.end = idx + 1;
                    g.old.end = (b1 + ctx).min(self.old.len());
                    g.new.end = (a1 + ctx).min(self.new.len());
                    continue;
                }
            }
            out.push(Group {
                hunks: idx..idx + 1,
                old: b0.saturating_sub(ctx)..(b1 + ctx).min(self.old.len()),
                new: a0.saturating_sub(ctx)..(a1 + ctx).min(self.new.len()),
            });
        }
        out
    }
}

/// One output hunk: which changes it holds and the line ranges it covers on each side.
#[derive(Clone, Debug)]
pub struct Group {
    pub hunks: Range<usize>,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Longest function-context text git keeps (`struct func_line` buffer).
const FUNC_MAX: usize = 80;

/// Git's default `funcname` matcher: a line starting with a letter, `_` or `$`.
pub fn default_funcname(line: &[u8]) -> Option<&[u8]> {
    let first = *line.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut len = line.len().min(FUNC_MAX);
    while len > 0 && is_c_space(line[len - 1]) {
        len -= 1;
    }
    Some(&line[..len])
}

/// Function-context header text across the hunks of one file, as git's `xdl_emit_diff` keeps it:
/// each hunk searches back only as far as the previous hunk's search origin and otherwise
/// repeats the previous hunk's text.
pub struct FuncTracker<'a> {
    origin: isize,
    current: Option<&'a [u8]>,
}

impl<'a> FuncTracker<'a> {
    pub fn new() -> Self {
        Self {
            origin: -1,
            current: None,
        }
    }

    /// The text for a hunk whose old side starts at line `start`.
    pub fn next(&mut self, old: &[&'a [u8]], start: usize) -> Option<&'a [u8]> {
        if let Some(found) = function_context(old, start, self.origin) {
            self.current = Some(found);
        }
        self.origin = start as isize - 1;
        self.current
    }
}

impl Default for FuncTracker<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Function-context text for the hunk starting at old line `start`, searched backwards from
/// `start - 1` down to (excluding) `limit` (the previous hunk's search origin).
pub fn function_context<'a>(old: &[&'a [u8]], start: usize, limit: isize) -> Option<&'a [u8]> {
    let mut l = start as isize - 1;
    while l != limit && l >= 0 {
        if let Some(f) = old.get(l as usize).and_then(|line| default_funcname(line)) {
            return Some(f);
        }
        l -= 1;
    }
    None
}

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

// ---- slider compaction (xdl_change_compact) ----

const MAX_INDENT: i32 = 200;
const MAX_BLANKS: usize = 20;
const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;
const INDENT_MAX_SLIDING: isize = 100;

/// Indent width of a line, or -1 when it holds only whitespace.
fn indent_of(line: &[u8]) -> i32 {
    let mut ret = 0;
    for &c in line {
        match c {
            b' ' => ret += 1,
            b'\t' => ret += 8 - ret % 8,
            c if is_c_space(c) => {}
            _ => return ret,
        }
        if ret >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1
}

/// One side of the diff during compaction. `chg` is padded by one false slot at each end so that
/// index -1 and `n` read as unchanged.
struct Side<'a> {
    tok: &'a [Token],
    chg: Vec<bool>,
    ind: Vec<i32>,
}

impl<'a> Side<'a> {
    fn new(tok: &'a [Token], flags: &mut [bool], ind: Vec<i32>) -> Self {
        let mut chg = Vec::with_capacity(flags.len() + 2);
        chg.push(false);
        chg.extend_from_slice(flags);
        chg.push(false);
        Self { tok, chg, ind }
    }

    fn into_flags(self) -> Vec<bool> {
        self.chg[1..self.chg.len() - 1].to_vec()
    }

    fn n(&self) -> isize {
        self.tok.len() as isize
    }

    fn c(&self, i: isize) -> bool {
        self.chg[(i + 1) as usize]
    }

    fn set(&mut self, i: isize, v: bool) {
        self.chg[(i + 1) as usize] = v;
    }

    fn eq(&self, a: isize, b: isize) -> bool {
        self.tok[a as usize] == self.tok[b as usize]
    }
}

/// A maximal run of changed lines `[start, end)`.
#[derive(Clone, Copy)]
struct Group2 {
    start: isize,
    end: isize,
}

impl Group2 {
    fn init(s: &Side) -> Self {
        let mut end = 0;
        while s.c(end) {
            end += 1;
        }
        Self { start: 0, end }
    }

    fn next(&mut self, s: &Side) -> bool {
        if self.end == s.n() {
            return false;
        }
        self.start = self.end + 1;
        self.end = self.start;
        while s.c(self.end) {
            self.end += 1;
        }
        true
    }

    fn prev(&mut self, s: &Side) -> bool {
        if self.start == 0 {
            return false;
        }
        self.end = self.start - 1;
        self.start = self.end;
        while s.c(self.start - 1) {
            self.start -= 1;
        }
        true
    }

    fn slide_down(&mut self, s: &mut Side) -> bool {
        if self.end < s.n() && s.eq(self.start, self.end) {
            s.set(self.start, false);
            self.start += 1;
            s.set(self.end, true);
            self.end += 1;
            while s.c(self.end) {
                self.end += 1;
            }
            true
        } else {
            false
        }
    }

    fn slide_up(&mut self, s: &mut Side) -> bool {
        if self.start > 0 && s.eq(self.start - 1, self.end - 1) {
            self.start -= 1;
            s.set(self.start, true);
            self.end -= 1;
            s.set(self.end, false);
            while s.c(self.start - 1) {
                self.start -= 1;
            }
            true
        } else {
            false
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Score {
    effective_indent: i32,
    penalty: i32,
}

struct Split {
    end_of_file: bool,
    indent: i32,
    pre_blank: i32,
    pre_indent: i32,
    post_blank: i32,
    post_indent: i32,
}

fn measure_split(s: &Side, split: isize) -> Split {
    let n = s.n();
    let (end_of_file, indent) = if split >= n {
        (true, -1)
    } else {
        (false, s.ind[split as usize])
    };
    let (mut pre_blank, mut pre_indent) = (0, -1);
    let mut i = split - 1;
    while i >= 0 {
        pre_indent = s.ind[i as usize];
        if pre_indent != -1 {
            break;
        }
        pre_blank += 1;
        if pre_blank as usize == MAX_BLANKS {
            pre_indent = 0;
            break;
        }
        i -= 1;
    }
    let (mut post_blank, mut post_indent) = (0, -1);
    let mut i = split + 1;
    while i < n {
        post_indent = s.ind[i as usize];
        if post_indent != -1 {
            break;
        }
        post_blank += 1;
        if post_blank as usize == MAX_BLANKS {
            post_indent = 0;
            break;
        }
        i += 1;
    }
    Split {
        end_of_file,
        indent,
        pre_blank,
        pre_indent,
        post_blank,
        post_indent,
    }
}

fn score_add_split(m: &Split, s: &mut Score) {
    if m.pre_indent == -1 && m.pre_blank == 0 {
        s.penalty += START_OF_FILE_PENALTY;
    }
    if m.end_of_file {
        s.penalty += END_OF_FILE_PENALTY;
    }
    let post_blank = if m.indent == -1 { 1 + m.post_blank } else { 0 };
    let total_blank = m.pre_blank + post_blank;
    s.penalty += TOTAL_BLANK_WEIGHT * total_blank;
    s.penalty += POST_BLANK_WEIGHT * post_blank;
    let indent = if m.indent != -1 {
        m.indent
    } else {
        m.post_indent
    };
    let any_blanks = total_blank != 0;
    s.effective_indent += indent;
    if indent == -1 || m.pre_indent == -1 || indent == m.pre_indent {
    } else if indent > m.pre_indent {
        s.penalty += if any_blanks {
            RELATIVE_INDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_INDENT_PENALTY
        };
    } else if m.post_indent != -1 && m.post_indent > indent {
        s.penalty += if any_blanks {
            RELATIVE_OUTDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_OUTDENT_PENALTY
        };
    } else {
        s.penalty += if any_blanks {
            RELATIVE_DEDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_DEDENT_PENALTY
        };
    }
}

fn score_cmp(a: &Score, b: &Score) -> i32 {
    INDENT_WEIGHT * (a.effective_indent - b.effective_indent).signum() + (a.penalty - b.penalty)
}

/// Slides each change group to line up with the other side, or to the best split by indent.
fn compact(xdf: &mut Side, xdfo: &mut Side, indent_heuristic: bool) {
    let mut g = Group2::init(xdf);
    let mut go = Group2::init(xdfo);
    loop {
        if g.end != g.start {
            let (mut earliest_end, mut end_matching_other);
            loop {
                let groupsize = g.end - g.start;
                end_matching_other = -1;
                while g.slide_up(xdf) {
                    go.prev(xdfo);
                }
                earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = g.end;
                }
                while g.slide_down(xdf) {
                    go.next(xdfo);
                    if go.end > go.start {
                        end_matching_other = g.end;
                    }
                }
                if groupsize == g.end - g.start {
                    break;
                }
            }
            let groupsize = g.end - g.start;
            if g.end == earliest_end {
                // No shift possible.
            } else if end_matching_other != -1 {
                while go.end == go.start {
                    if !g.slide_up(xdf) {
                        break;
                    }
                    go.prev(xdfo);
                }
            } else if indent_heuristic {
                let mut shift = earliest_end;
                shift = shift.max(g.end - groupsize - 1);
                shift = shift.max(g.end - INDENT_MAX_SLIDING);
                let mut best: Option<(isize, Score)> = None;
                while shift <= g.end {
                    let mut score = Score::default();
                    score_add_split(&measure_split(xdf, shift), &mut score);
                    score_add_split(&measure_split(xdf, shift - groupsize), &mut score);
                    if best.is_none_or(|(_, b)| score_cmp(&score, &b) <= 0) {
                        best = Some((shift, score));
                    }
                    shift += 1;
                }
                let best_shift = best.map_or(g.end, |b| b.0);
                while g.end > best_shift {
                    if !g.slide_up(xdf) {
                        break;
                    }
                    go.prev(xdfo);
                }
            }
        }
        if !g.next(xdf) {
            break;
        }
        go.next(xdfo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(old: &str, new: &str, algo: Algorithm) -> Vec<(Range<u32>, Range<u32>)> {
        diff(old.as_bytes(), new.as_bytes(), algo, true)
            .hunks
            .into_iter()
            .map(|h| (h.before, h.after))
            .collect()
    }

    #[test]
    fn single_replacement_is_one_hunk_for_every_algorithm() {
        for algo in [
            Algorithm::Myers,
            Algorithm::Minimal,
            Algorithm::Histogram,
            Algorithm::Patience,
        ] {
            assert_eq!(run("a\nb\nc\n", "a\nx\nc\n", algo), vec![(1..2, 1..2)]);
        }
    }

    #[test]
    fn missing_final_newline_makes_the_last_line_differ() {
        assert_eq!(run("a\nb\n", "a\nb", Algorithm::Myers), vec![(1..2, 1..2)]);
    }

    #[test]
    fn indent_heuristic_slides_a_block_to_the_natural_edge() {
        // Inserting a second copy of a block can slide either way; the heuristic keeps the
        // closing brace with the block that owns it.
        let old = "fn a() {\n    x();\n}\n\nfn c() {\n    z();\n}\n";
        let new = "fn a() {\n    x();\n}\n\nfn b() {\n    y();\n}\n\nfn c() {\n    z();\n}\n";
        let d = diff(old.as_bytes(), new.as_bytes(), Algorithm::Myers, true);
        assert_eq!(d.hunks.len(), 1);
        assert_eq!(d.hunks[0].after, 4..8);
    }

    #[test]
    fn groups_merge_within_twice_the_context() {
        let old: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line3\n", "x\n").replace("line9\n", "y\n");
        let d = diff(old.as_bytes(), new.as_bytes(), Algorithm::Myers, true);
        // 5 unchanged lines between the changes: merged at -U3, split at -U2.
        assert_eq!(d.groups(3, 0).len(), 1);
        assert_eq!(d.groups(2, 0).len(), 2);
        assert_eq!(d.groups(2, 1).len(), 1);
    }

    #[test]
    fn function_context_skips_indented_lines() {
        let lines = split_lines(b"fn main() {\n    body\n}\n    tail\n");
        assert_eq!(function_context(&lines, 4, -1), Some(&b"fn main() {"[..]));
        assert_eq!(function_context(&lines, 1, -1), Some(&b"fn main() {"[..]));
        assert_eq!(function_context(&lines, 0, -1), None);
    }

    #[test]
    fn binary_probe_stops_at_8000_bytes() {
        let mut data = vec![b'a'; 9000];
        assert!(!is_binary(&data));
        data[7999] = 0;
        assert!(is_binary(&data));
        data[7999] = b'a';
        data[8000] = 0;
        assert!(!is_binary(&data));
    }
}
