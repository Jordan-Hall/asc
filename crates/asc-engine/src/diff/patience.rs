//! Patience diff over interned line tokens, following xdiff's `xpatience.c`.
//!
//! Lines that occur exactly once on each side are anchors; the longest increasing run of anchors
//! splits the problem, and gaps are solved again. A gap with common lines but no unique anchor
//! falls back to Myers, as git does.

use std::collections::HashMap;
use std::ops::Range;

use imara_diff::Token;

use super::xdiff::{self, Flags};

/// Patience diff of two token sequences.
pub fn diff(before: &[Token], after: &[Token]) -> Flags {
    let mut flags = Flags {
        removed: vec![false; before.len()],
        added: vec![false; after.len()],
    };
    let mut work = vec![(0..before.len(), 0..after.len())];
    while let Some((b, a)) = work.pop() {
        solve(before, after, b, a, &mut flags, &mut work);
    }
    flags
}

/// One anchor: a line unique to both sides, at `(old index, new index)`.
type Anchor = (usize, usize);

fn solve(
    before: &[Token],
    after: &[Token],
    b: Range<usize>,
    a: Range<usize>,
    flags: &mut Flags,
    work: &mut Vec<(Range<usize>, Range<usize>)>,
) {
    if b.is_empty() || a.is_empty() {
        mark(&mut flags.removed, &b);
        mark(&mut flags.added, &a);
        return;
    }
    let (anchors, any_common) = unique_common(&before[b.clone()], &after[a.clone()]);
    if !any_common {
        mark(&mut flags.removed, &b);
        mark(&mut flags.added, &a);
        return;
    }
    let chain = longest_increasing(&anchors);
    if chain.is_empty() {
        classic(before, after, b, a, flags);
        return;
    }
    // Walk the chain, growing each anchor over equal neighbours and queueing the gaps.
    let (mut l1, mut l2) = (b.start, a.start);
    let mut i = 0;
    loop {
        let (mut n1, mut n2) = match chain.get(i) {
            Some(&(x, y)) => (b.start + x, a.start + y),
            None => (b.end, a.end),
        };
        while n1 > l1 && n2 > l2 && before[n1 - 1] == after[n2 - 1] {
            n1 -= 1;
            n2 -= 1;
        }
        while l1 < n1 && l2 < n2 && before[l1] == after[l2] {
            l1 += 1;
            l2 += 1;
        }
        if n1 > l1 || n2 > l2 {
            work.push((l1..n1, l2..n2));
        }
        if i == chain.len() {
            return;
        }
        // Skip a run of consecutive anchors.
        while i + 1 < chain.len()
            && chain[i + 1].0 == chain[i].0 + 1
            && chain[i + 1].1 == chain[i].1 + 1
        {
            i += 1;
        }
        l1 = b.start + chain[i].0 + 1;
        l2 = a.start + chain[i].1 + 1;
        i += 1;
    }
}

fn mark(flags: &mut [bool], r: &Range<usize>) {
    flags[r.clone()].fill(true);
}

/// Myers on a sub-range, merged into `flags`.
fn classic(before: &[Token], after: &[Token], b: Range<usize>, a: Range<usize>, flags: &mut Flags) {
    let sub = xdiff::diff(&before[b.clone()], &after[a.clone()], false);
    flags.removed[b].copy_from_slice(&sub.removed);
    flags.added[a].copy_from_slice(&sub.added);
}

/// Lines occurring once on each side, in old-side order, and whether any line is common at all.
fn unique_common(before: &[Token], after: &[Token]) -> (Vec<Anchor>, bool) {
    #[derive(Default)]
    struct Seen {
        old: u32,
        old_at: usize,
        new: u32,
        new_at: usize,
    }
    let mut map: HashMap<Token, Seen> = HashMap::with_capacity(before.len());
    for (i, &t) in before.iter().enumerate() {
        let s = map.entry(t).or_default();
        s.old += 1;
        s.old_at = i;
    }
    let mut any = false;
    for (j, &t) in after.iter().enumerate() {
        if let Some(s) = map.get_mut(&t) {
            any = true;
            s.new += 1;
            s.new_at = j;
        }
    }
    let anchors = before
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let s = &map[t];
            (s.old == 1 && s.new == 1).then_some((i, s.new_at))
        })
        .collect();
    (anchors, any)
}

/// Longest strictly increasing subsequence by new-side index (patience sorting).
fn longest_increasing(anchors: &[Anchor]) -> Vec<Anchor> {
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; anchors.len()];
    for (k, &(_, y)) in anchors.iter().enumerate() {
        let pos = tails.partition_point(|&t| anchors[t].1 < y);
        prev[k] = pos.checked_sub(1).map(|p| tails[p]);
        if pos == tails.len() {
            tails.push(k);
        } else {
            tails[pos] = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(k) = cur {
        out.push(anchors[k]);
        cur = prev[k];
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Token> {
        s.bytes().map(|b| Token(u32::from(b))).collect()
    }

    fn apply(before: &str, after: &str) -> (String, String) {
        let f = diff(&toks(before), &toks(after));
        let keep = |s: &str, fl: &[bool]| {
            s.chars()
                .zip(fl)
                .filter(|(_, c)| !**c)
                .map(|(c, _)| c)
                .collect::<String>()
        };
        (keep(before, &f.removed), keep(after, &f.added))
    }

    #[test]
    fn unchanged_lines_form_a_common_subsequence() {
        for (a, b) in [
            ("abcdef", "abxdef"),
            ("", "abc"),
            ("abc", ""),
            ("abab", "baba"),
            ("axbxc", "cxbxa"),
            ("abcabba", "cbabac"),
        ] {
            let (x, y) = apply(a, b);
            assert_eq!(x, y, "{a} vs {b}");
        }
    }

    #[test]
    fn unique_lines_anchor_the_match() {
        // Myers would match the repeated 'x' lines; patience keeps the unique ones aligned.
        let (kept, _) = apply("xaxbx", "xbxax");
        assert!(kept.len() >= 3);
        let f = diff(&toks("ab"), &toks("ba"));
        assert_eq!(f.removed.iter().filter(|c| **c).count(), 1);
    }
}
