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
        let mut s = text.to_owned();
        for e in &self.exact {
            if s.contains(e.as_str()) {
                s = s.replace(e.as_str(), MASK);
            }
        }
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let scanned = self.rules.scan(bytes);
        let mut at = 0;
        for hit in &scanned.hits {
            let r = &hit.value;
            if r.start < at || r.end > bytes.len() {
                continue;
            }
            out.push_str(&String::from_utf8_lossy(&bytes[at..r.start]));
            out.push_str(MASK);
            at = r.end;
        }
        out.push_str(&String::from_utf8_lossy(&bytes[at..]));
        truncate(out, MAX_EVENT_TEXT)
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
}
