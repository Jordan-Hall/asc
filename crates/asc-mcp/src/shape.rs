//! Token-aware shaping (SPEC 22.6.2): the budget, stream folding and per-shape truncation.
//!
//! There is no tokenizer: `tokens = ceil(utf8_bytes / 3)`. One [`Budget`] covers a whole result,
//! so what `structuredContent` spends and what the text block spends come out of the same pool.
//! Streams are consumed lazily and dropped (which cancels the producer) as soon as the budget
//! runs out. Cuts fall on whole entries, whole hunks or whole lines, never inside a record, except
//! where a single record alone is larger than the budget and the call would otherwise never
//! advance.

use crate::cursor::Position;

/// `asc.mcp.maxTokens` default.
pub const DEFAULT_MAX_TOKENS: u64 = 6_000;
/// The hard cap: the Claude Code default tool-result limit.
pub const HARD_MAX_TOKENS: u64 = 25_000;
/// Files a diff lists per call.
pub const MAX_DIFF_FILES: usize = 300;
/// Bytes per estimated token.
pub const BYTES_PER_TOKEN: u64 = 3;

/// `ceil(bytes / 3)`.
#[must_use]
pub const fn estimate_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// `min(max_tokens argument, configured, 25,000)`, at least 1. `configured` is `asc.mcp.maxTokens`.
#[must_use]
pub fn effective_max_tokens(arg: Option<u64>, configured: u64) -> u64 {
    arg.unwrap_or(configured)
        .min(configured)
        .clamp(1, HARD_MAX_TOKENS)
}

/// A byte pool shared by every part of one result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    limit: u64,
    used: u64,
}

impl Budget {
    /// A pool of `max_tokens` tokens (use [`effective_max_tokens`] first).
    #[must_use]
    pub const fn new(max_tokens: u64) -> Self {
        Self {
            limit: max_tokens.saturating_mul(BYTES_PER_TOKEN),
            used: 0,
        }
    }

    /// Bytes still free.
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.limit - self.used
    }

    /// Estimated tokens spent so far.
    #[must_use]
    pub const fn tokens_used(&self) -> u64 {
        estimate_tokens(self.used)
    }

    /// Spend `bytes` if they fit; otherwise spend nothing and return false.
    pub fn try_charge(&mut self, bytes: u64) -> bool {
        if bytes <= self.remaining() {
            self.used += bytes;
            true
        } else {
            false
        }
    }

    /// Spend `bytes` whether or not they fit, saturating at the limit (for parts that are always
    /// kept, such as a diff's file list).
    pub fn charge_forced(&mut self, bytes: u64) {
        self.used = self.used.saturating_add(bytes).min(self.limit);
    }
}

/// The result of folding a stream under a budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold<T> {
    /// The whole entries that fit, in order.
    pub kept: Vec<T>,
    /// The stream had more entries (the rest was dropped, not read).
    pub more: bool,
    /// Nothing was kept although an entry existed: that entry alone exceeds the budget and the
    /// caller must cut inside it.
    pub stalled: bool,
}

/// Take whole entries from `stream` while `size(entry)` fits `budget`. At most one entry past the
/// last kept one is pulled, to learn whether more exist; then the stream is dropped.
pub fn fold<I, T>(stream: I, budget: &mut Budget, size: impl Fn(&T) -> u64) -> Fold<T>
where
    I: IntoIterator<Item = T>,
{
    let mut kept = Vec::new();
    for item in stream {
        if !budget.try_charge(size(&item)) {
            let stalled = kept.is_empty();
            return Fold {
                kept,
                more: true,
                stalled,
            };
        }
        kept.push(item);
    }
    Fold {
        kept,
        more: false,
        stalled: false,
    }
}

/// A page of a list-like tool (log, grep, tree, blame hunks, lists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage<T> {
    pub items: Vec<T>,
    /// The position to continue from; `None` when the list ended.
    pub next: Option<Position>,
    /// The first entry alone was over budget and was not returned.
    pub stalled: bool,
}

/// Fold a list that starts at entry `start` (the stream must already start there).
pub fn shape_list<I, T>(
    stream: I,
    start: u64,
    budget: &mut Budget,
    size: impl Fn(&T) -> u64,
) -> ListPage<T>
where
    I: IntoIterator<Item = T>,
{
    let f = fold(stream, budget, size);
    let next = f.more.then(|| Position::Index {
        index: start + f.kept.len() as u64,
    });
    ListPage {
        items: f.kept,
        next,
        stalled: f.stalled,
    }
}

// ---------------------------------------------------------------- diff

/// One hunk of a patch: its header line and body lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<String>,
    /// Set when the hunk was cut inside because it alone exceeded the budget.
    pub truncated: bool,
}

impl Hunk {
    fn bytes(&self) -> u64 {
        (self.header.len() + 1 + self.lines.iter().map(|l| l.len() + 1).sum::<usize>()) as u64
    }
}

/// One changed file with its full patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn summary_bytes(&self) -> u64 {
        (self.path.len() + self.status.len() + 24) as u64
    }
}

/// A file in the `changes` list: path, status and stats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSummary {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
}

/// A file's kept hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: String,
    pub hunks: Vec<Hunk>,
}

/// What was left out of a diff result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Omitted {
    /// Files beyond the 300 of this page (counted, not listed).
    pub files: u64,
    /// Hunks of listed files whose text was cut.
    pub hunks: u64,
    /// Lines in those hunks.
    pub lines: u64,
}

/// The shaped diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapedDiff {
    /// Up to 300 files with statuses and stats, in path order.
    pub changes: Vec<FileSummary>,
    /// Patch text for the files whose hunks fit.
    pub patches: Vec<FilePatch>,
    pub omitted: Omitted,
    /// Where the next call resumes, `None` when nothing was left out.
    pub next: Option<Position>,
}

/// Shape a diff. `files` is sorted into path order here. `start` is `(file index, hunk index)`
/// from a [`Position::Diff`] cursor. The list of up to `max_files` files is always kept (and
/// charged); patch text is added whole hunk by whole hunk, file by file, until the budget runs
/// out. If the very first hunk of the page alone exceeds the budget it is cut by lines and flagged
/// `truncated`, so every call makes progress.
pub fn shape_diff(
    mut files: Vec<FileDiff>,
    start: (u64, u64),
    max_files: usize,
    budget: &mut Budget,
) -> ShapedDiff {
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let first = usize::try_from(start.0)
        .unwrap_or(usize::MAX)
        .min(files.len());
    let end = first.saturating_add(max_files).min(files.len());
    let beyond = (files.len() - end) as u64;
    let page = &files[first..end];

    let changes: Vec<FileSummary> = page
        .iter()
        .map(|f| FileSummary {
            path: f.path.clone(),
            status: f.status.clone(),
            additions: f.additions,
            deletions: f.deletions,
        })
        .collect();
    for f in page {
        budget.charge_forced(f.summary_bytes());
    }

    let mut patches = Vec::new();
    let mut omitted = Omitted {
        files: beyond,
        ..Omitted::default()
    };
    let mut resume: Option<(u64, u64)> = None;
    let mut closed = false;
    let mut any_kept = false;
    for (i, f) in page.iter().enumerate() {
        let skip = if i == 0 {
            usize::try_from(start.1).unwrap_or(usize::MAX)
        } else {
            0
        };
        let mut kept = Vec::new();
        for (h, hunk) in f.hunks.iter().enumerate().skip(skip) {
            if closed {
                omitted.hunks += 1;
                omitted.lines += hunk.lines.len() as u64;
                continue;
            }
            if budget.try_charge(hunk.bytes()) {
                any_kept = true;
                kept.push(hunk.clone());
                continue;
            }
            closed = true;
            resume = Some((first as u64 + i as u64, h as u64));
            if any_kept || !kept.is_empty() {
                omitted.hunks += 1;
                omitted.lines += hunk.lines.len() as u64;
            } else {
                // First hunk of the page is larger than the whole budget: cut inside it.
                let cut = cut_hunk(hunk, budget);
                any_kept = true;
                omitted.lines += (hunk.lines.len() - cut.lines.len()) as u64;
                resume = Some((first as u64 + i as u64, h as u64 + 1));
                kept.push(cut);
            }
        }
        if !kept.is_empty() {
            patches.push(FilePatch {
                path: f.path.clone(),
                hunks: kept,
            });
        }
    }
    // A cut that landed on the last hunk of the page leaves nothing to resume within the page.
    if let Some((fi, hi)) = resume {
        let fidx = usize::try_from(fi).unwrap_or(usize::MAX);
        if files
            .get(fidx)
            .is_none_or(|f| usize::try_from(hi).unwrap_or(usize::MAX) >= f.hunks.len())
        {
            resume = (end < files.len()).then_some((end as u64, 0));
        }
    } else if beyond > 0 {
        resume = Some((end as u64, 0));
    }
    let next = resume.map(|(file, hunk)| Position::Diff { file, hunk });
    ShapedDiff {
        changes,
        patches,
        omitted,
        next,
    }
}

fn cut_hunk(h: &Hunk, budget: &mut Budget) -> Hunk {
    let mut out = Hunk {
        header: h.header.clone(),
        lines: Vec::new(),
        truncated: true,
    };
    let head = h.header.len() as u64 + 1;
    if !budget.try_charge(head) {
        budget.charge_forced(head);
    }
    for l in &h.lines {
        if !budget.try_charge(l.len() as u64 + 1) {
            break;
        }
        out.lines.push(l.clone());
    }
    out
}

// ---------------------------------------------------------------- file

/// A line stream element: text including its trailing newline, if it had one.
pub type RawLine = String;

/// A page of `read_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePage {
    pub text: String,
    pub first_line: u64,
    /// Last line included; `first_line - 1` when empty.
    pub last_line: u64,
    /// The cursor position (byte offset and line of the next line), or `None` at the end of the
    /// requested range or file.
    pub next: Option<Position>,
    /// The single line at `first_line` was longer than the budget and was cut at a character
    /// boundary; the cursor then continues at the next line.
    pub line_truncated: bool,
}

/// Shape a `read_file` page. `lines` starts at byte `offset` / line number `line` (the backend
/// seeks there, so the cost is O(page)); it is read lazily, at most one line past the page, and
/// not at all past `end_line`.
pub fn shape_file<I>(
    lines: I,
    offset: u64,
    line: u64,
    end_line: Option<u64>,
    budget: &mut Budget,
) -> FilePage
where
    I: IntoIterator<Item = RawLine>,
{
    let mut text = String::new();
    let mut off = offset;
    let mut n = line;
    let mut line_truncated = false;
    let mut next = None;
    for l in lines {
        if end_line.is_some_and(|e| n > e) {
            break;
        }
        let len = l.len() as u64;
        if !budget.try_charge(len) {
            if text.is_empty() {
                // One line alone over budget: keep a prefix and move on to the next line.
                let room = usize::try_from(budget.remaining())
                    .unwrap_or(usize::MAX)
                    .min(l.len());
                let mut cut = room;
                while !l.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.push_str(&l[..cut]);
                budget.charge_forced(cut as u64);
                line_truncated = true;
                off += len;
                n += 1;
            }
            next = Some(Position::File {
                offset: off,
                line: n,
            });
            break;
        }
        text.push_str(&l);
        off += len;
        n += 1;
    }
    // The page ended by budget only when `next` is set; otherwise the range or file ended.
    FilePage {
        text,
        first_line: line,
        last_line: n - 1,
        next,
        line_truncated,
    }
}

// ---------------------------------------------------------------- status

/// A status entry: a path with a short code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub code: String,
    pub path: String,
}

impl StatusEntry {
    fn bytes(&self) -> u64 {
        (self.code.len() + self.path.len() + 4) as u64
    }
}

/// Status groups after shaping. Counts are of entries cut from the end of each group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapedStatus {
    pub conflicts: Vec<StatusEntry>,
    pub staged: Vec<StatusEntry>,
    pub unstaged: Vec<StatusEntry>,
    pub untracked: Vec<String>,
    pub omitted_conflicts: u64,
    pub omitted_staged: u64,
    pub omitted_unstaged: u64,
    pub omitted_untracked: u64,
}

/// Fill the budget with conflicts first, then staged, then unstaged, then untracked paths. A group
/// that does not fit entirely is cut at its tail and every later group is counted, not listed.
pub fn shape_status(
    conflicts: Vec<StatusEntry>,
    staged: Vec<StatusEntry>,
    unstaged: Vec<StatusEntry>,
    untracked: Vec<String>,
    budget: &mut Budget,
) -> ShapedStatus {
    let mut out = ShapedStatus::default();
    let mut full = false;
    let mut take = |v: Vec<StatusEntry>, full: &mut bool| -> (Vec<StatusEntry>, u64) {
        let total = v.len() as u64;
        if *full {
            return (Vec::new(), total);
        }
        let f = fold(v, budget, StatusEntry::bytes);
        *full = f.more;
        let omitted = total - f.kept.len() as u64;
        (f.kept, omitted)
    };
    (out.conflicts, out.omitted_conflicts) = take(conflicts, &mut full);
    (out.staged, out.omitted_staged) = take(staged, &mut full);
    (out.unstaged, out.omitted_unstaged) = take(unstaged, &mut full);
    let total = untracked.len() as u64;
    if !full {
        out.untracked = fold(untracked, budget, |p: &String| p.len() as u64 + 3).kept;
    }
    out.omitted_untracked = total - out.untracked.len() as u64;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(n: usize, width: usize) -> Hunk {
        Hunk {
            header: "@@ -1 +1 @@".into(),
            lines: (0..n).map(|_| "x".repeat(width)).collect(),
            truncated: false,
        }
    }

    fn file(i: usize, hunks: usize) -> FileDiff {
        FileDiff {
            path: format!("f{i:04}"),
            status: "M".into(),
            additions: 1,
            deletions: 1,
            hunks: (0..hunks).map(|_| hunk(2, 20)).collect(),
        }
    }

    #[test]
    fn estimate_is_ceil_bytes_over_three() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(3), 1);
        assert_eq!(estimate_tokens(4), 2);
        assert_eq!(Budget::new(10).remaining(), 30);
    }

    #[test]
    fn max_tokens_is_the_minimum_of_arg_config_and_cap() {
        assert_eq!(effective_max_tokens(None, 6000), 6000);
        assert_eq!(effective_max_tokens(Some(100), 6000), 100);
        assert_eq!(effective_max_tokens(Some(50_000), 6000), 6000);
        assert_eq!(effective_max_tokens(Some(50_000), 90_000), HARD_MAX_TOKENS);
        assert_eq!(effective_max_tokens(Some(0), 6000), 1);
    }

    #[test]
    fn budget_is_shared_between_structured_and_text() {
        let mut b = Budget::new(10); // 30 bytes
        assert!(b.try_charge(20)); // structuredContent
        assert!(!b.try_charge(11)); // the text block no longer fits
        assert!(b.try_charge(10));
        assert_eq!(b.remaining(), 0);
        assert_eq!(b.tokens_used(), 10);
        // an entry costing structured + text bytes folds against the same pool
        let mut b = Budget::new(10);
        let f = fold(vec![(5u64, 5u64), (5, 5), (5, 5)], &mut b, |&(s, t)| s + t);
        assert_eq!(f.kept.len(), 3);
        let mut b = Budget::new(10);
        let f = fold(vec![(10u64, 10u64), (10, 10)], &mut b, |&(s, t)| s + t);
        assert_eq!((f.kept.len(), f.more), (1, true));
    }

    #[test]
    fn fold_stops_pulling_after_one_extra_entry() {
        let mut pulled = 0u32;
        let stream = (0..1_000_000u32).inspect(|_| pulled += 1);
        let mut b = Budget::new(10);
        let f = fold(stream, &mut b, |_| 3);
        assert_eq!(f.kept.len(), 10);
        assert!(f.more && !f.stalled);
        assert_eq!(pulled, 11);
    }

    #[test]
    fn fold_reports_a_first_entry_over_budget() {
        let mut b = Budget::new(1);
        let f = fold(vec![100u64], &mut b, |&n| n);
        assert!(f.kept.is_empty() && f.more && f.stalled);
        let p = shape_list(vec![1u64, 1, 1, 1], 7, &mut Budget::new(1), |&n| n);
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.next, Some(Position::Index { index: 10 }));
        assert_eq!(
            shape_list(vec![1u64], 0, &mut Budget::new(1), |&n| n).next,
            None
        );
    }

    #[test]
    fn diff_caps_at_300_files_and_counts_the_rest() {
        let files: Vec<_> = (0..350).rev().map(|i| file(i, 1)).collect();
        let mut b = Budget::new(HARD_MAX_TOKENS);
        let d = shape_diff(files.clone(), (0, 0), MAX_DIFF_FILES, &mut b);
        assert_eq!(d.changes.len(), 300);
        assert_eq!(d.changes[0].path, "f0000");
        assert_eq!(d.changes[299].path, "f0299");
        assert_eq!(
            d.omitted,
            Omitted {
                files: 50,
                hunks: 0,
                lines: 0
            }
        );
        assert_eq!(d.next, Some(Position::Diff { file: 300, hunk: 0 }));
        // the cursor continues with the last 50
        let mut b = Budget::new(HARD_MAX_TOKENS);
        let d2 = shape_diff(files, (300, 0), MAX_DIFF_FILES, &mut b);
        assert_eq!(d2.changes.len(), 50);
        assert_eq!(d2.changes[0].path, "f0300");
        assert_eq!(d2.omitted.files, 0);
        assert_eq!(d2.next, None);
    }

    #[test]
    fn diff_cuts_at_whole_hunks_and_counts_omitted() {
        // each hunk: header 12 + 2 * 21 = 54 bytes; the file list costs 3 * 33 = 99 bytes.
        let files = vec![file(0, 3), file(1, 3), file(2, 3)];
        let total_lines: u64 = 9 * 2;
        let mut b = Budget::new((99 + 54 * 4) / 3 + 1); // room for the list and four hunks
        let d = shape_diff(files, (0, 0), MAX_DIFF_FILES, &mut b);
        assert_eq!(d.changes.len(), 3, "the file list is kept whole");
        let kept: usize = d.patches.iter().map(|p| p.hunks.len()).sum();
        assert_eq!(kept, 4);
        assert_eq!(d.patches[0].hunks.len(), 3);
        assert_eq!(d.patches[1].hunks.len(), 1);
        assert!(
            d.patches
                .iter()
                .flat_map(|p| &p.hunks)
                .all(|h| !h.truncated && h.lines.len() == 2)
        );
        assert_eq!(
            d.omitted,
            Omitted {
                files: 0,
                hunks: 5,
                lines: 10
            }
        );
        assert_eq!(d.omitted.lines + 4 * 2, total_lines);
        assert_eq!(d.next, Some(Position::Diff { file: 1, hunk: 1 }));
    }

    #[test]
    fn diff_resumes_inside_a_file_and_always_progresses() {
        let files = vec![file(0, 3)];
        let mut b = Budget::new(HARD_MAX_TOKENS);
        let d = shape_diff(files.clone(), (0, 2), MAX_DIFF_FILES, &mut b);
        assert_eq!(d.patches[0].hunks.len(), 1);
        assert_eq!(d.next, None);
        // a hunk larger than the whole budget is cut by lines and flagged
        let big = vec![FileDiff {
            path: "a".into(),
            status: "M".into(),
            additions: 0,
            deletions: 0,
            hunks: vec![hunk(100, 30), hunk(1, 1)],
        }];
        let mut b = Budget::new(40);
        let d = shape_diff(big, (0, 0), MAX_DIFF_FILES, &mut b);
        let h = &d.patches[0].hunks[0];
        assert!(h.truncated && !h.lines.is_empty() && h.lines.len() < 100);
        assert_eq!(d.next, Some(Position::Diff { file: 0, hunk: 1 }));
        assert_eq!(
            d.omitted.lines,
            100 - h.lines.len() as u64 + 1,
            "cut lines plus the later hunk"
        );
    }

    fn lines(n: usize) -> impl Iterator<Item = String> {
        (1..=n).map(|i| format!("line {i:05}\n"))
    }

    #[test]
    fn file_page_cursor_carries_offset_and_line() {
        let mut b = Budget::new(10); // 30 bytes = 2 lines of 11 bytes
        let p = shape_file(lines(100), 0, 1, None, &mut b);
        assert_eq!(p.text, "line 00001\nline 00002\n");
        assert_eq!((p.first_line, p.last_line), (1, 2));
        assert_eq!(
            p.next,
            Some(Position::File {
                offset: 22,
                line: 3
            })
        );
    }

    #[test]
    fn file_continuation_costs_o_page_not_a_restream() {
        // the backend seeks to offset 22; the iterator it gives starts at line 3
        let mut pulled = 0;
        let src = (3..=1_000_000usize)
            .map(|i| format!("line {i:05}\n"))
            .inspect(|_| pulled += 1);
        let mut b = Budget::new(10);
        let p = shape_file(src, 22, 3, None, &mut b);
        assert_eq!(p.first_line, 3);
        assert_eq!(
            p.next,
            Some(Position::File {
                offset: 22 + 2 * 11,
                line: 5
            })
        );
        assert!(pulled <= 4, "pulled {pulled} lines for a 2-line page");
    }

    #[test]
    fn file_stops_at_end_line_without_reading_on() {
        let mut pulled = 0;
        let mut b = Budget::new(HARD_MAX_TOKENS);
        let p = shape_file(lines(1000).inspect(|_| pulled += 1), 0, 1, Some(5), &mut b);
        assert_eq!((p.last_line, p.next), (5, None));
        assert_eq!(pulled, 6);
        assert_eq!(p.text.lines().count(), 5);
    }

    #[test]
    fn file_line_longer_than_budget_is_cut_and_advances() {
        let long = format!("{}é\n", "a".repeat(100));
        let mut b = Budget::new(10);
        let p = shape_file(vec![long.clone(), "next\n".into()], 0, 1, None, &mut b);
        assert!(p.line_truncated && p.text.len() <= 30);
        assert_eq!(
            p.next,
            Some(Position::File {
                offset: long.len() as u64,
                line: 2
            })
        );
        let mut b = Budget::new(10);
        let p = shape_file(vec![format!("{}é\n", "a".repeat(29))], 0, 1, None, &mut b);
        assert!(p.line_truncated);
        assert!(std::str::from_utf8(p.text.as_bytes()).is_ok());
    }

    #[test]
    fn status_orders_conflicts_staged_unstaged_then_counts_untracked() {
        let e = |c: &str, p: &str| StatusEntry {
            code: c.into(),
            path: p.into(),
        };
        let untracked: Vec<String> = (0..100).map(|i| format!("u{i}")).collect();
        let mut b = Budget::new(10); // 30 bytes
        let s = shape_status(
            vec![e("UU", "c")],
            vec![e("M", "s")],
            vec![e("M", "w")],
            untracked,
            &mut b,
        );
        assert_eq!(
            (s.conflicts.len(), s.staged.len(), s.unstaged.len()),
            (1, 1, 1)
        );
        assert_eq!(s.untracked.len() as u64 + s.omitted_untracked, 100);
        assert!(s.omitted_untracked > 0);
        // a full budget before untracked paths hides all of them but still counts them
        let mut b = Budget::new(4);
        let s = shape_status(
            vec![e("UU", "a"), e("UU", "b"), e("UU", "c")],
            vec![e("M", "s")],
            vec![],
            vec!["u".into()],
            &mut b,
        );
        assert_eq!(s.conflicts.len(), 1);
        assert_eq!(s.omitted_conflicts, 2);
        assert_eq!(s.omitted_staged, 1);
        assert_eq!(s.omitted_untracked, 1);
    }
}
