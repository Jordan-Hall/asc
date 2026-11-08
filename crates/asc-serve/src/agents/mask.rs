//! Keeping secrets out of run events.
//!
//! Everything a run records (what the model said, tool arguments, tool output, command output)
//! passes through [`Masker::mask`] before it is stored or streamed. Two layers:
//!
//! * the exact values this run knows are secret (its own agent token, the provider key it was
//!   given, the secrets of a workflow step) are replaced wherever they occur;
//! * the repository's secret rules (the built-in detectors of [`asc_core::secrets`]) replace
//!   anything else that looks like a credential.
//!
//! A masked value becomes `***`. Event text is also bounded, so a run cannot fill the database.

use std::ops::Range;

use aho_corasick::AhoCorasick;
use asc_core::secrets::RuleSet;
use zeroize::Zeroizing;

/// What replaces a secret.
pub const MASK: &str = "***";
/// Longest event text kept, in bytes.
pub const MAX_EVENT_TEXT: usize = 16 * 1024;

/// The context a credential rule needs on each side of a window: the longest credential the rules mask whole
/// (a PEM private key block).
pub const RULE_MARGIN: usize = 16 * 1024;

/// Masks text for storage.
pub struct Masker {
    rules: RuleSet,
    exact: Vec<Zeroizing<String>>,
    /// Finds every exact secret, overlapping ones included, in one pass; `None` when there is none.
    finder: Option<AhoCorasick>,
}

impl Masker {
    /// A masker with the built-in rules and the exact secrets of this run (values shorter than 6
    /// bytes are ignored: masking "ab" everywhere would only damage the text).
    pub fn new(exact: impl IntoIterator<Item = String>) -> Self {
        let mut m = Self {
            rules: RuleSet::builtin(),
            exact: Vec::new(),
            finder: None,
        };
        for s in exact {
            m.push(s);
        }
        m.build();
        m
    }

    /// Adds an exact secret.
    pub fn add(&mut self, secret: &str) {
        if self.push(secret.to_owned()) {
            self.build();
        }
    }

    fn push(&mut self, secret: String) -> bool {
        if secret.len() < 6 || self.exact.iter().any(|s| s.as_str() == secret) {
            return false;
        }
        self.exact.push(Zeroizing::new(secret));
        true
    }

    fn build(&mut self) {
        self.finder = if self.exact.is_empty() {
            None
        } else {
            // Literal patterns of bounded total size (the callers bound the count and the length) always build.
            AhoCorasick::new(self.exact.iter().map(|s| s.as_bytes())).ok()
        };
    }

    /// How many bytes of context on each side of a window let [`Masker::mask_window`] see every secret that
    /// touches it.
    pub fn margin(&self) -> usize {
        self.exact
            .iter()
            .map(|s| s.len())
            .max()
            .unwrap_or(0)
            .max(RULE_MARGIN)
    }

    /// The ranges of `data` that hold a secret, sorted and merged where they overlap.
    fn ranges(&self, data: &[u8]) -> Vec<Range<usize>> {
        let mut found = self.rules.ranges(data);
        if let Some(f) = &self.finder {
            found.extend(f.find_overlapping_iter(data).map(|m| m.range()));
        }
        found.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(found.len());
        for r in found {
            match merged.last_mut() {
                Some(last) if r.start < last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }

    /// `text` with every secret replaced, cut to [`MAX_EVENT_TEXT`] bytes.
    pub fn mask(&self, text: &str) -> String {
        let out = self.mask_bytes(text.as_bytes());
        truncate(String::from_utf8_lossy(&out).into_owned(), MAX_EVENT_TEXT)
    }

    /// `data` with every secret replaced and nothing cut; bytes that are not UTF-8 are kept as they are.
    pub fn mask_bytes(&self, data: &[u8]) -> Vec<u8> {
        self.mask_window(data, 0..data.len())
    }

    /// The bytes of `window` within `context`, with every secret that touches the window replaced, also one
    /// that starts or ends outside it: a part of a secret is never shown. `context` should reach
    /// [`Masker::margin`] bytes past each side of the window where the bytes exist.
    pub fn mask_window(&self, context: &[u8], window: Range<usize>) -> Vec<u8> {
        let window = window.start.min(context.len())..window.end.min(context.len());
        let mut out = Vec::with_capacity(window.len());
        let mut at = window.start;
        for r in self.ranges(context) {
            if r.end <= window.start {
                continue;
            }
            if r.start >= window.end {
                break;
            }
            let start = r.start.max(at);
            out.extend_from_slice(&context[at..start]);
            out.extend_from_slice(MASK.as_bytes());
            at = r.end.min(window.end);
        }
        out.extend_from_slice(&context[at..window.end]);
        out
    }
}

/// Cuts `s` to at most `max` bytes at a character boundary, saying so.
pub fn truncate(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut cut = max.saturating_sub(24);
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n[... truncated]");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_secrets_and_known_formats_are_masked() {
        let m = Masker::new(["supersecret-run-token".to_owned(), "ab".to_owned()]);
        let out = m.mask("token supersecret-run-token and again supersecret-run-token; ab stays");
        assert_eq!(out, "token *** and again ***; ab stays");
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n-----END RSA PRIVATE KEY-----\nafter";
        let out = m.mask(pem);
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "{out}");
        assert!(out.contains("before") && out.contains("after"));
        let aws = m.mask("key AKIAIOSFODNN7EXAMPLE end");
        assert!(!aws.contains("AKIAIOSFODNN7EXAMPLE"), "{aws}");
    }

    #[test]
    fn text_is_bounded_on_a_character_boundary() {
        let m = Masker::new([]);
        let long = "é".repeat(MAX_EVENT_TEXT);
        let out = m.mask(&long);
        assert!(out.len() <= MAX_EVENT_TEXT);
        assert!(out.ends_with("[... truncated]"));
        assert_eq!(m.mask("short"), "short");
    }

    #[test]
    fn secrets_can_be_added_later_once() {
        let mut m = Masker::new([]);
        m.add("a-late-secret-value");
        m.add("a-late-secret-value");
        m.add("tiny");
        assert_eq!(m.exact.len(), 1);
        assert_eq!(m.mask("x a-late-secret-value y tiny"), "x *** y tiny");
    }

    #[test]
    fn bytes_are_masked_whole_and_other_bytes_kept() {
        let m = Masker::new(["a-step-secret".to_owned()]);
        let mut log = b"\xff start a-step-secreta-step-secret end ".to_vec();
        log.extend(std::iter::repeat_n(b'x', MAX_EVENT_TEXT * 2));
        let out = m.mask_bytes(&log);
        assert!(
            out.starts_with(b"\xff start ****** end "),
            "{:?}",
            &out[..24]
        );
        assert_eq!(
            out.len(),
            log.len() - 2 * ("a-step-secret".len() - MASK.len()),
            "nothing is cut"
        );
        assert_eq!(
            m.mask_bytes(b"a-step-secre"),
            b"a-step-secre",
            "a prefix is not the secret"
        );
    }

    #[test]
    fn overlapping_secrets_are_masked_together() {
        let m = Masker::new(["abcdef12".to_owned(), "def12345".to_owned()]);
        assert_eq!(
            m.mask_bytes(b"x abcdef12345 y"),
            b"x *** y",
            "neither tail shows"
        );
        let m = Masker::new(["token-xyz".to_owned(), "token-xyz-longer".to_owned()]);
        assert_eq!(
            m.mask_bytes(b"[token-xyz-longer]"),
            b"[***]",
            "the longer secret is masked whole"
        );
    }

    #[test]
    fn a_window_never_shows_part_of_a_secret() {
        let m = Masker::new(["deploy-token-0123456789".to_owned()]);
        let ctx = b"aaa deploy-token-0123456789 bbb";
        // A window inside the secret, one that ends inside it and one that starts inside it.
        assert_eq!(m.mask_window(ctx, 10..12), b"***");
        assert_eq!(m.mask_window(ctx, 0..8), b"aaa ***");
        assert_eq!(m.mask_window(ctx, 20..31), b"*** bbb");
        assert_eq!(m.mask_window(ctx, 28..31), b"bbb", "past the secret");
        // Reading byte by byte gives back nothing of it.
        let joined: Vec<u8> = (0..ctx.len())
            .flat_map(|i| m.mask_window(ctx, i..i + 1))
            .collect();
        assert!(
            !joined.windows(4).any(|w| w == b"0123"),
            "{}",
            String::from_utf8_lossy(&joined)
        );
        assert!(m.margin() >= RULE_MARGIN);
    }

    #[test]
    fn rules_mask_in_binary_text_and_past_the_scan_cap() {
        let m = Masker::new([]);
        let mut log = b"\0binary\n".to_vec();
        for _ in 0..(asc_core::secrets::MAX_HITS_PER_FILE + 4) {
            log.extend_from_slice(b"key AKIAIOSFODNN7EXAMPLE\n");
        }
        let out = m.mask_bytes(&log);
        assert!(
            !out.windows(8).any(|w| w == b"AKIAIOSF"),
            "every credential is masked"
        );
    }
}
