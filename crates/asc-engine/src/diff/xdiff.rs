//! Classic Myers diff as git's xdiff computes it (`xprepare.c` record cleanup plus `xdiffi.c`
//! divide and conquer with its cost heuristics).
//!
//! The heuristics decide which of several equally short edit scripts is chosen, so reproducing
//! them is what makes the output equal to git's byte for byte.

use std::collections::HashMap;

use imara_diff::Token;

/// Per-line "changed" flags for the old and new side.
pub struct Flags {
    pub removed: Vec<bool>,
    pub added: Vec<bool>,
}

const MAX_EQLIMIT: usize = 1024;
const SIMSCAN_WINDOW: isize = 100;
const KPDIS_RUN: usize = 4;
const MAX_COST_MIN: isize = 256;
const HEUR_MIN_COST: isize = 256;
const SNAKE_CNT: isize = 20;
const K_HEUR: isize = 4;

fn bogosqrt(mut n: usize) -> usize {
    let mut i = 1;
    while n > 0 {
        i <<= 1;
        n >>= 2;
    }
    i
}

/// Diffs two token sequences the way xdiff's default algorithm does; `minimal` asks for the
/// shortest script (`--minimal`).
pub fn diff(before: &[Token], after: &[Token], minimal: bool) -> Flags {
    let mut flags = Flags {
        removed: vec![false; before.len()],
        added: vec![false; after.len()],
    };
    // Common prefix and suffix are never changed.
    let lim = before.len().min(after.len());
    let start = (0..lim).take_while(|&i| before[i] == after[i]).count();
    let tail = (0..lim - start)
        .take_while(|&i| before[before.len() - 1 - i] == after[after.len() - 1 - i])
        .count();
    let (e1, e2) = (before.len() - tail, after.len() - tail);
    if start >= e1 && start >= e2 {
        return flags;
    }

    // Occurrences of each class in the other file, over the whole files as xdiff counts them.
    let mut counts: HashMap<Token, (u32, u32)> = HashMap::new();
    for t in before {
        counts.entry(*t).or_default().0 += 1;
    }
    for t in after {
        counts.entry(*t).or_default().1 += 1;
    }

    let (idx1, ha1) = cleanup(
        before,
        start,
        e1,
        &counts,
        true,
        minimal,
        &mut flags.removed,
    );
    let (idx2, ha2) = cleanup(after, start, e2, &counts, false, minimal, &mut flags.added);

    let ndiags = (ha1.len() + ha2.len() + 3) as isize;
    let mut env = Env {
        ha1: &ha1,
        ha2: &ha2,
        idx1: &idx1,
        idx2: &idx2,
        kvdf: vec![0; ndiags as usize],
        kvdb: vec![0; ndiags as usize],
        base: ha2.len() as isize + 1,
        mxcost: (bogosqrt(ndiags as usize) as isize).max(MAX_COST_MIN),
        flags: &mut flags,
    };
    env.recs_cmp(0, ha1.len() as isize, 0, ha2.len() as isize, minimal);
    flags
}

/// Marks lines with no useful match as changed and returns the surviving lines' original
/// indexes and classes.
fn cleanup(
    toks: &[Token],
    start: usize,
    end: usize,
    counts: &HashMap<Token, (u32, u32)>,
    first: bool,
    minimal: bool,
    changed: &mut [bool],
) -> (Vec<usize>, Vec<Token>) {
    let mlim = bogosqrt(toks.len()).min(MAX_EQLIMIT);
    let dis: Vec<u8> = toks[start..end]
        .iter()
        .map(|t| {
            let (c1, c2) = counts[t];
            let nm = if first { c2 } else { c1 } as usize;
            match nm {
                0 => 0,
                n if n >= mlim && !minimal => 2,
                _ => 1,
            }
        })
        .collect();
    let mut index = Vec::new();
    let mut ha = Vec::new();
    for (k, &d) in dis.iter().enumerate() {
        if d == 1 || (d == 2 && !clean_mmatch(&dis, k as isize, 0, dis.len() as isize - 1)) {
            index.push(start + k);
            ha.push(toks[start + k]);
        } else {
            changed[start + k] = true;
        }
    }
    (index, ha)
}

/// Whether a multi-match line sits inside a run of no-match lines and should be dropped.
fn clean_mmatch(dis: &[u8], i: isize, mut s: isize, mut e: isize) -> bool {
    if i - s > SIMSCAN_WINDOW {
        s = i - SIMSCAN_WINDOW;
    }
    if e - i > SIMSCAN_WINDOW {
        e = i + SIMSCAN_WINDOW;
    }
    let (mut rdis0, mut rpdis0) = (0usize, 1usize);
    let mut r = 1;
    while i - r >= s {
        match dis[(i - r) as usize] {
            0 => rdis0 += 1,
            2 => rpdis0 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis0 == 0 {
        return false;
    }
    let (mut rdis1, mut rpdis1) = (0usize, 1usize);
    r = 1;
    while i + r <= e {
        match dis[(i + r) as usize] {
            0 => rdis1 += 1,
            2 => rpdis1 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis1 == 0 {
        return false;
    }
    rdis1 += rdis0;
    rpdis1 += rpdis0;
    rpdis1 * KPDIS_RUN < rpdis1 + rdis1
}

struct Env<'a> {
    ha1: &'a [Token],
    ha2: &'a [Token],
    idx1: &'a [usize],
    idx2: &'a [usize],
    kvdf: Vec<isize>,
    kvdb: Vec<isize>,
    /// Offset of diagonal 0 in `kvdf` and `kvdb`.
    base: isize,
    mxcost: isize,
    flags: &'a mut Flags,
}

struct Split {
    i1: isize,
    i2: isize,
    min_lo: bool,
    min_hi: bool,
}

impl Env<'_> {
    fn f(&self, d: isize) -> isize {
        self.kvdf[(self.base + d) as usize]
    }

    fn b(&self, d: isize) -> isize {
        self.kvdb[(self.base + d) as usize]
    }

    fn set_f(&mut self, d: isize, v: isize) {
        let at = (self.base + d) as usize;
        self.kvdf[at] = v;
    }

    fn set_b(&mut self, d: isize, v: isize) {
        let at = (self.base + d) as usize;
        self.kvdb[at] = v;
    }

    fn eq(&self, i1: isize, i2: isize) -> bool {
        self.ha1[i1 as usize] == self.ha2[i2 as usize]
    }

    fn recs_cmp(
        &mut self,
        mut off1: isize,
        mut lim1: isize,
        mut off2: isize,
        mut lim2: isize,
        need_min: bool,
    ) {
        while off1 < lim1 && off2 < lim2 && self.eq(off1, off2) {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1 && off2 < lim2 && self.eq(lim1 - 1, lim2 - 1) {
            lim1 -= 1;
            lim2 -= 1;
        }
        if off1 == lim1 {
            for k in off2..lim2 {
                self.flags.added[self.idx2[k as usize]] = true;
            }
        } else if off2 == lim2 {
            for k in off1..lim1 {
                self.flags.removed[self.idx1[k as usize]] = true;
            }
        } else {
            let spl = self.split(off1, lim1, off2, lim2, need_min);
            self.recs_cmp(off1, spl.i1, off2, spl.i2, spl.min_lo);
            self.recs_cmp(spl.i1, lim1, spl.i2, lim2, spl.min_hi);
        }
    }

    fn split(
        &mut self,
        off1: isize,
        lim1: isize,
        off2: isize,
        lim2: isize,
        need_min: bool,
    ) -> Split {
        let (dmin, dmax) = (off1 - lim2, lim1 - off2);
        let (fmid, bmid) = (off1 - off2, lim1 - lim2);
        let odd = (fmid - bmid) & 1 != 0;
        let (mut fmin, mut fmax) = (fmid, fmid);
        let (mut bmin, mut bmax) = (bmid, bmid);
        self.set_f(fmid, off1);
        self.set_b(bmid, lim1);
        let mut ec = 1;
        loop {
            let mut got_snake = false;
            if fmin > dmin {
                fmin -= 1;
                self.set_f(fmin - 1, -1);
            } else {
                fmin += 1;
            }
            if fmax < dmax {
                fmax += 1;
                self.set_f(fmax + 1, -1);
            } else {
                fmax -= 1;
            }
            let mut d = fmax;
            while d >= fmin {
                let mut i1 = if self.f(d - 1) >= self.f(d + 1) {
                    self.f(d - 1) + 1
                } else {
                    self.f(d + 1)
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 < lim1 && i2 < lim2 && self.eq(i1, i2) {
                    i1 += 1;
                    i2 += 1;
                }
                if i1 - prev1 > SNAKE_CNT {
                    got_snake = true;
                }
                self.set_f(d, i1);
                if odd && bmin <= d && d <= bmax && self.b(d) <= i1 {
                    return Split {
                        i1,
                        i2,
                        min_lo: true,
                        min_hi: true,
                    };
                }
                d -= 2;
            }

            if bmin > dmin {
                bmin -= 1;
                self.set_b(bmin - 1, isize::MAX);
            } else {
                bmin += 1;
            }
            if bmax < dmax {
                bmax += 1;
                self.set_b(bmax + 1, isize::MAX);
            } else {
                bmax -= 1;
            }
            let mut d = bmax;
            while d >= bmin {
                let mut i1 = if self.b(d - 1) < self.b(d + 1) {
                    self.b(d - 1)
                } else {
                    self.b(d + 1) - 1
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 > off1 && i2 > off2 && self.eq(i1 - 1, i2 - 1) {
                    i1 -= 1;
                    i2 -= 1;
                }
                if prev1 - i1 > SNAKE_CNT {
                    got_snake = true;
                }
                self.set_b(d, i1);
                if !odd && fmin <= d && d <= fmax && i1 <= self.f(d) {
                    return Split {
                        i1,
                        i2,
                        min_lo: true,
                        min_hi: true,
                    };
                }
                d -= 2;
            }

            if need_min {
                ec += 1;
                continue;
            }

            if got_snake && ec > HEUR_MIN_COST {
                let mut best = 0;
                let mut found = None;
                let mut d = fmax;
                while d >= fmin {
                    let dd = (d - fmid).abs();
                    let i1 = self.f(d);
                    let i2 = i1 - d;
                    let v = (i1 - off1) + (i2 - off2) - dd;
                    if v > K_HEUR * ec
                        && v > best
                        && off1 + SNAKE_CNT <= i1
                        && i1 < lim1
                        && off2 + SNAKE_CNT <= i2
                        && i2 < lim2
                    {
                        let mut k = 1;
                        while self.eq(i1 - k, i2 - k) {
                            if k == SNAKE_CNT {
                                best = v;
                                found = Some((i1, i2));
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if let (true, Some((i1, i2))) = (best > 0, found) {
                    return Split {
                        i1,
                        i2,
                        min_lo: true,
                        min_hi: false,
                    };
                }

                let mut best = 0;
                let mut found = None;
                let mut d = bmax;
                while d >= bmin {
                    let dd = (d - bmid).abs();
                    let i1 = self.b(d);
                    let i2 = i1 - d;
                    let v = (lim1 - i1) + (lim2 - i2) - dd;
                    if v > K_HEUR * ec
                        && v > best
                        && off1 < i1
                        && i1 <= lim1 - SNAKE_CNT
                        && off2 < i2
                        && i2 <= lim2 - SNAKE_CNT
                    {
                        let mut k = 0;
                        while self.eq(i1 + k, i2 + k) {
                            if k == SNAKE_CNT - 1 {
                                best = v;
                                found = Some((i1, i2));
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if let (true, Some((i1, i2))) = (best > 0, found) {
                    return Split {
                        i1,
                        i2,
                        min_lo: false,
                        min_hi: true,
                    };
                }
            }

            if ec >= self.mxcost {
                let (mut fbest, mut fbest1) = (-1, -1);
                let mut d = fmax;
                while d >= fmin {
                    let mut i1 = self.f(d).min(lim1);
                    let mut i2 = i1 - d;
                    if lim2 < i2 {
                        i1 = lim2 + d;
                        i2 = lim2;
                    }
                    if fbest < i1 + i2 {
                        fbest = i1 + i2;
                        fbest1 = i1;
                    }
                    d -= 2;
                }
                let (mut bbest, mut bbest1) = (isize::MAX, isize::MAX);
                let mut d = bmax;
                while d >= bmin {
                    let mut i1 = off1.max(self.b(d));
                    let mut i2 = i1 - d;
                    if i2 < off2 {
                        i1 = off2 + d;
                        i2 = off2;
                    }
                    if i1 + i2 < bbest {
                        bbest = i1 + i2;
                        bbest1 = i1;
                    }
                    d -= 2;
                }
                return if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                    Split {
                        i1: fbest1,
                        i2: fbest - fbest1,
                        min_lo: true,
                        min_hi: false,
                    }
                } else {
                    Split {
                        i1: bbest1,
                        i2: bbest - bbest1,
                        min_lo: false,
                        min_hi: true,
                    }
                };
            }
            ec += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Token> {
        s.bytes().map(|b| Token(u32::from(b))).collect()
    }

    fn kept(s: &str, flags: &[bool]) -> String {
        s.chars()
            .zip(flags)
            .filter(|(_, c)| !**c)
            .map(|(c, _)| c)
            .collect()
    }

    #[test]
    fn unchanged_elements_form_a_common_subsequence() {
        for (a, b) in [
            ("abcabba", "cbabac"),
            ("", "xyz"),
            ("abc", "abc"),
            ("aXb", "ab"),
        ] {
            for minimal in [false, true] {
                let f = diff(&toks(a), &toks(b), minimal);
                assert_eq!(kept(a, &f.removed), kept(b, &f.added), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn minimal_finds_the_shortest_script() {
        let f = diff(&toks("abcabba"), &toks("cbabac"), true);
        let edits = f.removed.iter().chain(&f.added).filter(|c| **c).count();
        assert_eq!(edits, 5);
    }
}
