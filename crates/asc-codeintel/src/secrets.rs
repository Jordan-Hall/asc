//! Secret scanner (SPEC §23.7): path rules and content rules.
//!
//! A flagged file is never chunked or embedded; a flagged line is masked or redacted by
//! the renderer. Exact search still matches flagged text.

use regex::bytes::{Regex, RegexSet};
use std::sync::LazyLock;

/// Minimum Shannon entropy, bits per char, of a keyword-assigned value.
pub const MIN_ENTROPY: f64 = 3.5;
/// Shortest keyword-assigned value that is scored.
const MIN_VALUE_LEN: usize = 20;
/// Bytes of one line that are scanned; a secret later in a longer line is missed.
const MAX_LINE_SCAN: usize = 4096;
/// Bytes of a file that are scanned.
const MAX_SCAN_BYTES: usize = 4 << 20;
/// Most flagged lines reported.
const MAX_FLAGGED: usize = 10_000;

/// True when the last path segment names a likely secret file: `.env*`, `*.pem`, `*.key`,
/// `*.p12`, `*.pfx`, `id_rsa*`, `*credentials*`, `secrets.*` (ASCII case-insensitive).
/// Both `/` and `\` separate segments, so a Windows-style path cannot dodge the rule.
pub fn path_is_secret(path: &str) -> bool {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    base.starts_with(".env")
        || base.starts_with("id_rsa")
        || base.contains("credentials")
        || base.starts_with("secrets.")
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|e| base.ends_with(e))
}

static TOKEN_SET: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----",
        r"AKIA[0-9A-Z]{16}",
        r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        // GitHub (classic, fine-grained), Forgejo/Gitea (`gta_`, `fj_`), GitLab and Slack prefixes.
        r"gh[pousr]_[A-Za-z0-9]{36,}",
        r"github_pat_[A-Za-z0-9_]{22,}",
        r"(?:gta|fj|forgejo)_[A-Za-z0-9]{32,}",
        r"glpat-[A-Za-z0-9_-]{20,}",
        r"xox[abeoprs]-[A-Za-z0-9-]{10,}",
    ])
    .expect("static secret patterns compile")
});

static KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:key|secret|token|password)["']?\s*[:=]\s*["']?([A-Za-z0-9+/=_.\-]{20,})"#)
        .expect("static keyword pattern compiles")
});

/// Shannon entropy of `s` in bits per byte.
pub fn entropy(s: &[u8]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in s {
        counts[usize::from(b)] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum()
}

/// True when one line (without its newline) holds a secret by the content rules.
pub fn line_is_secret(line: &[u8]) -> bool {
    let line = &line[..line.len().min(MAX_LINE_SCAN)];
    if TOKEN_SET.is_match(line) {
        return true;
    }
    KEYWORD.captures_iter(line).any(|c| {
        c.get(1)
            .is_some_and(|m| m.len() >= MIN_VALUE_LEN && entropy(m.as_bytes()) >= MIN_ENTROPY)
    })
}

/// Result of scanning one file's content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentScan {
    /// 1-based numbers of flagged lines, ascending, at most 10,000.
    pub lines: Vec<u32>,
}

impl ContentScan {
    pub fn is_secret(&self) -> bool {
        !self.lines.is_empty()
    }
}

/// Scans `bytes` line by line; work is bounded by 4 MiB of input and 4 KiB per line.
pub fn scan_content(bytes: &[u8]) -> ContentScan {
    let bytes = &bytes[..bytes.len().min(MAX_SCAN_BYTES)];
    let mut lines = Vec::new();
    for (i, l) in bytes.split(|&b| b == b'\n').enumerate() {
        if line_is_secret(l) {
            let Ok(n) = u32::try_from(i + 1) else { break };
            lines.push(n);
            if lines.len() >= MAX_FLAGGED {
                break;
            }
        }
    }
    ContentScan { lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules() {
        for p in [
            ".env",
            "a/.env.local",
            "x/server.PEM",
            "k.key",
            "a.p12",
            "a.pfx",
            "home/id_rsa",
            "id_rsa.pub",
            "aws_credentials.json",
            "config/secrets.yml",
            "a\\b\\.env",
        ] {
            assert!(path_is_secret(p), "{p}");
        }
        for p in [
            "src/main.rs",
            "environment.rs",
            "keyboard.rs",
            "secretsauce.md",
            "a.env/readme",
        ] {
            assert!(!path_is_secret(p), "{p}");
        }
    }

    #[test]
    fn content_fixtures_are_flagged() {
        let fixtures: [&[u8]; 8] = [
            b"-----BEGIN RSA PRIVATE KEY-----",
            b"-----BEGIN PRIVATE KEY-----",
            b"id = AKIAIOSFODNN7EXAMPLE",
            b"t = eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
            b"GH=ghp_aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6",
            b"slack xoxb-123456789012-abcdefghij",
            b"api_key = \"Zx9fQ2mT7vB4kL8pR1sW6yD3\"",
            b"password: aB3dE6gH9jK2mN5pQ8sT1vW",
        ];
        for f in fixtures {
            assert!(line_is_secret(f), "{}", String::from_utf8_lossy(f));
        }
    }

    #[test]
    fn plain_code_is_not_flagged() {
        let code = b"fn token_count(key: &str) -> usize {\n    let password = \"changeme\";\n    let secret = aaaaaaaaaaaaaaaaaaaaaaaaaaaa;\n    key.len() // AKIA is a prefix\n}\n";
        assert!(!scan_content(code).is_secret());
        // Long but low-entropy value after a keyword.
        assert!(!line_is_secret(b"token = \"aaaaaaaaaaaaaaaaaaaaaaaa\""));
        // Short value.
        assert!(!line_is_secret(b"key = abcdef123"));
    }

    #[test]
    fn reports_line_numbers() {
        let s =
            scan_content(b"one\ntwo\nAKIAIOSFODNN7EXAMPLE\nfour\n-----BEGIN EC PRIVATE KEY-----\n");
        assert_eq!(s.lines, vec![3, 5]);
    }

    #[test]
    fn entropy_values() {
        assert_eq!(entropy(b"aaaa"), 0.0);
        assert!((entropy(b"abcd") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn bounded_on_hostile_input() {
        let big = vec![b'k'; 8 << 20];
        let _ = scan_content(&big);
        let many = b"AKIAIOSFODNN7EXAMPLE\n".repeat(20_000);
        assert_eq!(scan_content(&many).lines.len(), MAX_FLAGGED);
    }
}
