//! Byte skip rules (SPEC §23.2.1): flags that the file bytes alone decide.

use std::collections::HashSet;

/// Largest file that is indexed (1 MiB).
pub const MAX_BYTES: usize = 1 << 20;
/// Longest line that is indexed, in bytes, excluding the newline.
pub const MAX_LINE: usize = 2_000;
/// Most distinct case-folded trigrams that a file may hold.
pub const MAX_GRAMS: usize = 20_000;
/// Files at least this large with a mean line above [`MINIFIED_MEAN_LINE`] count as minified.
const MINIFIED_MIN_BYTES: usize = 1024;
const MINIFIED_MEAN_LINE: usize = 200;

/// Bit set of the byte-decided flags of a doc row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(u8);

impl Flags {
    pub const BINARY: Flags = Flags(1);
    pub const INVALID_UTF8: Flags = Flags(1 << 1);
    pub const TOO_LARGE: Flags = Flags(1 << 2);
    pub const LONG_LINE: Flags = Flags(1 << 3);
    pub const TOO_MANY_GRAMS: Flags = Flags(1 << 4);
    pub const MINIFIED: Flags = Flags(1 << 5);
    pub const SECRET: Flags = Flags(1 << 6);
    /// Flags that keep a file out of the trigram index.
    const NOT_INDEXED: u8 = 0b1_1111;

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Rebuilds flags from stored bits; unknown bits are rejected.
    pub fn from_bits(b: u8) -> Option<Flags> {
        (b & 0x80 == 0).then_some(Flags(b))
    }

    pub const fn contains(self, o: Flags) -> bool {
        self.0 & o.0 == o.0
    }

    pub fn insert(&mut self, o: Flags) {
        self.0 |= o.0;
    }

    /// True when no skip rule fired, so the file gets grams and tags. `MINIFIED` and
    /// `SECRET` do not stop exact search; the secret flag only stops chunking and embedding.
    pub const fn indexable(self) -> bool {
        self.0 & Self::NOT_INDEXED == 0
    }
}

/// Applies the skip rules to the bytes, cheapest first. Work is linear in `min(len, 1 MiB)`.
pub fn classify(bytes: &[u8]) -> Flags {
    let mut f = Flags::default();
    if bytes.len() > MAX_BYTES {
        f.insert(Flags::TOO_LARGE);
        return f; // later rules would scan an unbounded input
    }
    if bytes.contains(&0) {
        f.insert(Flags::BINARY);
        return f;
    }
    if std::str::from_utf8(bytes).is_err() {
        f.insert(Flags::INVALID_UTF8);
        return f;
    }
    let mut lines = 0usize;
    let mut longest = 0usize;
    for l in bytes.split(|&b| b == b'\n') {
        lines += 1;
        longest = longest.max(l.strip_suffix(b"\r").unwrap_or(l).len());
    }
    if longest > MAX_LINE {
        f.insert(Flags::LONG_LINE);
    }
    if bytes.len() >= MINIFIED_MIN_BYTES && bytes.len() / lines > MINIFIED_MEAN_LINE {
        f.insert(Flags::MINIFIED);
    }
    if f.contains(Flags::LONG_LINE) {
        return f;
    }
    if distinct_grams_exceed(bytes, MAX_GRAMS) {
        f.insert(Flags::TOO_MANY_GRAMS);
    }
    f
}

/// Distinct ASCII-case-folded byte trigrams of `bytes`, as 24-bit values (SPEC §23.2.1).
/// Stops early once `limit + 1` are seen, so memory is bounded by `limit`.
pub fn distinct_grams_exceed(bytes: &[u8], limit: usize) -> bool {
    let mut seen: HashSet<u32> = HashSet::with_capacity(limit.min(bytes.len()) + 1);
    for w in bytes.windows(3) {
        let g = u32::from(w[0].to_ascii_lowercase()) << 16
            | u32::from(w[1].to_ascii_lowercase()) << 8
            | u32::from(w[2].to_ascii_lowercase());
        if seen.insert(g) && seen.len() > limit {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_code_is_indexable() {
        let f = classify(b"fn main() {\n    println!(\"hi\");\n}\n");
        assert_eq!(f, Flags::default());
        assert!(f.indexable());
    }

    #[test]
    fn nul_is_binary() {
        assert!(classify(b"ab\0cd").contains(Flags::BINARY));
    }

    #[test]
    fn invalid_utf8() {
        let f = classify(&[b'a', 0xff, 0xfe, b'b']);
        assert!(f.contains(Flags::INVALID_UTF8) && !f.indexable());
    }

    #[test]
    fn size_boundary() {
        let mut ok = vec![b'a'; MAX_BYTES];
        for i in (79..ok.len()).step_by(80) {
            ok[i] = b'\n';
        }
        // The limit is inclusive; one byte more is too large. Low-entropy text, so no other flag.
        assert!(!classify(&ok).contains(Flags::TOO_LARGE));
        ok.push(b'\n');
        let f = classify(&ok);
        assert!(f.contains(Flags::TOO_LARGE) && !f.indexable());
    }

    #[test]
    fn line_length_boundary() {
        let mut ok = vec![b'a'; MAX_LINE];
        ok.push(b'\n');
        assert!(!classify(&ok).contains(Flags::LONG_LINE));
        let mut bad = vec![b'a'; MAX_LINE + 1];
        bad.push(b'\n');
        assert!(classify(&bad).contains(Flags::LONG_LINE));
        // CRLF does not count toward the line.
        let mut crlf = vec![b'a'; MAX_LINE];
        crlf.extend_from_slice(b"\r\n");
        assert!(!classify(&crlf).contains(Flags::LONG_LINE));
    }

    #[test]
    fn too_many_grams() {
        // Distinct trigrams from a 3-byte-per-step counter over printable bytes.
        let mut v = Vec::new();
        'outer: for a in 33u8..127 {
            for b in 33u8..127 {
                for c in 33u8..127 {
                    v.extend_from_slice(&[a.to_ascii_lowercase(), b, c]);
                    if v.len() > 3 * (MAX_GRAMS + 200) {
                        break 'outer;
                    }
                }
                v.push(b'\n');
            }
        }
        assert!(classify(&v).contains(Flags::TOO_MANY_GRAMS));
        assert!(!distinct_grams_exceed(b"abcabcabc", 3));
        assert!(distinct_grams_exceed(b"abcd", 1));
        // Case folding: ABC and abc are one gram.
        assert!(!distinct_grams_exceed(b"ABC abc", 4));
    }

    #[test]
    fn minified() {
        let mut m = Vec::new();
        for _ in 0..10 {
            m.extend_from_slice(&[b'x'; 500]);
            m.push(b'\n');
        }
        let f = classify(&m);
        assert!(f.contains(Flags::MINIFIED) && !f.contains(Flags::LONG_LINE));
    }

    #[test]
    fn flag_bits_roundtrip() {
        assert_eq!(Flags::from_bits(Flags::SECRET.bits()), Some(Flags::SECRET));
        assert_eq!(Flags::from_bits(0x80), None);
        assert!(Flags::SECRET.indexable() && Flags::MINIFIED.indexable());
    }
}
