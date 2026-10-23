//! Trigram extraction (SPEC §23.2.1, `grams_ver` 1): distinct ASCII-case-folded byte
//! trigrams packed into 24 bits. Only ASCII letters fold; every other byte is kept as is,
//! so the query side must expand non-ASCII case variants itself (the planner does).

use std::collections::HashSet;

/// Version of the gram scheme. Bump it to force a re-index (`grams_ver` in SPEC §23.2.1).
pub const GRAMS_VER: u32 = 1;

/// A 24-bit folded byte trigram.
pub type Gram = u32;

/// Packs three bytes into a gram, folding ASCII case.
#[inline]
pub fn gram(a: u8, b: u8, c: u8) -> Gram {
    u32::from(a.to_ascii_lowercase()) << 16
        | u32::from(b.to_ascii_lowercase()) << 8
        | u32::from(c.to_ascii_lowercase())
}

/// Every trigram of `s` in order, with repeats. Empty when `s` is shorter than 3 bytes.
pub fn grams_of(s: &[u8]) -> impl Iterator<Item = Gram> + '_ {
    s.windows(3).map(|w| gram(w[0], w[1], w[2]))
}

/// Distinct trigrams of `bytes`, sorted ascending. Returns `None` as soon as more than
/// `limit` distinct grams are seen, so memory is bounded by `limit` whatever the input.
pub fn distinct_trigrams(bytes: &[u8], limit: usize) -> Option<Vec<Gram>> {
    let mut seen: HashSet<Gram> = HashSet::with_capacity(limit.min(bytes.len()).min(1 << 16));
    for g in grams_of(bytes) {
        if seen.insert(g) && seen.len() > limit {
            return None;
        }
    }
    let mut v: Vec<Gram> = seen.into_iter().collect();
    v.sort_unstable();
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_only() {
        assert_eq!(gram(b'A', b'b', b'C'), gram(b'a', b'B', b'c'));
        assert_eq!(gram(b'a', b'b', b'c'), 0x61_62_63);
        assert_ne!(gram(0xc3, 0x89, b'a'), gram(0xc3, 0xa9, b'a'));
    }

    #[test]
    fn distinct_sorted() {
        let v = distinct_trigrams(b"abcABCabc", 100).unwrap();
        assert_eq!(v, vec![gram(b'a', b'b', b'c'), gram(b'b', b'c', b'a'), gram(b'c', b'a', b'b')]);
        assert!(distinct_trigrams(b"ab", 100).unwrap().is_empty());
    }

    #[test]
    fn limit_is_enforced() {
        assert!(distinct_trigrams(b"abcdef", 4).is_some());
        assert!(distinct_trigrams(b"abcdef", 3).is_none());
    }
}
