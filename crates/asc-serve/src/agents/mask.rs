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

use asc_core::secrets::RuleSet;
use zeroize::Zeroizing;

/// What replaces a secret.
pub const MASK: &str = "***";
/// Longest event text kept, in bytes.
pub const MAX_EVENT_TEXT: usize = 16 * 1024;

/// Masks text for storage.
pub struct Masker {
    rules: RuleSet,
    exact: Vec<Zeroizing<String>>,
}

impl Masker {
    /// A masker with the built-in rules and the exact secrets of this run (values shorter than 6
    /// bytes are ignored: masking "ab" everywhere would only damage the text).
    pub fn new(exact: impl IntoIterator<Item = String>) -> Self {
        Self {
            rules: RuleSet::builtin(),
            exact: exact
                .into_iter()
                .filter(|s| s.len() >= 6)
                .map(Zeroizing::new)
                .collect(),
        }
    }

    /// Adds an exact secret.
    pub fn add(&mut self, secret: &str) {
        if secret.len() >= 6 && !self.exact.iter().any(|s| s.as_str() == secret) {
            self.exact.push(Zeroizing::new(secret.to_owned()));
        }
    }

    /// `text` with every secret replaced, cut to [`MAX_EVENT_TEXT`] bytes.
    pub fn mask(&self, text: &str) -> String {
        let out = self.mask_bytes(text.as_bytes());
        truncate(String::from_utf8_lossy(&out).into_owned(), MAX_EVENT_TEXT)
    }

    /// `data` with every secret replaced and nothing cut; bytes that are not UTF-8 are kept as they are.
    pub fn mask_bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut s = data.to_vec();
        for e in &self.exact {
            s = replace_all(&s, e.as_bytes());
        }
        let scanned = self.rules.scan(&s);
        let mut out = Vec::with_capacity(s.len());
        let mut at = 0;
        for hit in &scanned.hits {
            let r = &hit.value;
            if r.start < at || r.end > s.len() {
                continue;
            }
            out.extend_from_slice(&s[at..r.start]);
            out.extend_from_slice(MASK.as_bytes());
            at = r.end;
        }
        out.extend_from_slice(&s[at..]);
        out
    }
}

/// `hay` with every occurrence of `needle` (not empty) replaced by [`MASK`].
fn replace_all(hay: &[u8], needle: &[u8]) -> Vec<u8> {
    if needle.is_empty() || hay.len() < needle.len() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut at = 0;
    while at + needle.len() <= hay.len() {
        if &hay[at..at + needle.len()] == needle {
            out.extend_from_slice(MASK.as_bytes());
            at += needle.len();
        } else {
            out.push(hay[at]);
            at += 1;
        }
    }
    out.extend_from_slice(&hay[at..]);
    out
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
        assert_eq!(replace_all(b"abc", b""), b"abc");
    }
}
