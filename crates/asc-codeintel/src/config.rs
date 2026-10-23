//! `.asc/codeintel.toml` (SPEC §23.2.1, §23.10): path excludes only.
//!
//! The file comes from the repository, so it is untrusted. It can hold nothing but
//! `exclude` globs: every other key (a provider, a URL, a model) is a parse error, so a
//! repo can never enable an external embedding provider.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Largest config file read.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Most exclude patterns.
pub const MAX_PATTERNS: usize = 256;
/// Longest single pattern, in bytes.
pub const MAX_PATTERN_LEN: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("codeintel.toml is larger than {MAX_CONFIG_BYTES} bytes")]
    TooLarge,
    #[error("codeintel.toml is not UTF-8")]
    NotUtf8,
    #[error("codeintel.toml: {0}")]
    Toml(String),
    #[error("more than {MAX_PATTERNS} exclude patterns")]
    TooManyPatterns,
    #[error("invalid exclude pattern {0:?}")]
    BadPattern(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    exclude: Vec<String>,
}

/// Parsed repo config: a compiled exclude matcher and a digest of the rules.
#[derive(Clone, Debug)]
pub struct CodeintelConfig {
    patterns: Vec<String>,
    set: GlobSet,
}

impl Default for CodeintelConfig {
    fn default() -> Self {
        CodeintelConfig {
            patterns: Vec::new(),
            set: GlobSet::empty(),
        }
    }
}

impl CodeintelConfig {
    /// Parses and validates the file bytes. Bounded in size, pattern count and length.
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| ConfigError::NotUtf8)?;
        let raw: Raw =
            toml::from_str(text).map_err(|e| ConfigError::Toml(e.message().to_owned()))?;
        if raw.exclude.len() > MAX_PATTERNS {
            return Err(ConfigError::TooManyPatterns);
        }
        let mut b = GlobSetBuilder::new();
        for p in &raw.exclude {
            if p.is_empty() || p.len() > MAX_PATTERN_LEN || p.contains('\0') {
                return Err(ConfigError::BadPattern(p.chars().take(64).collect()));
            }
            let g =
                Glob::new(p).map_err(|_| ConfigError::BadPattern(p.chars().take(64).collect()))?;
            b.add(g);
        }
        let set = b.build().map_err(|e| ConfigError::Toml(e.to_string()))?;
        Ok(CodeintelConfig {
            patterns: raw.exclude,
            set,
        })
    }

    /// True when the repo-relative, `/`-separated `path` matches an exclude.
    pub fn is_excluded(&self, path: &str) -> bool {
        !self.set.is_empty() && self.set.is_match(path)
    }

    pub fn excludes(&self) -> &[String] {
        &self.patterns
    }

    /// Digest of the sorted patterns; keys the per-tree `xdocs` cache (SPEC §23.2.3).
    pub fn rules_digest(&self) -> [u8; 32] {
        let mut sorted: Vec<&str> = self.patterns.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut h = blake3::Hasher::new_derive_key("asc v1 ci codeintel.toml rules");
        for p in sorted {
            h.update(&(p.len() as u32).to_le_bytes());
            h.update(p.as_bytes());
        }
        *h.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_match() {
        let c = CodeintelConfig::parse(b"exclude = [\"vendor/**\", \"**/*.min.js\"]\n").unwrap();
        assert!(c.is_excluded("vendor/a/b.rs"));
        assert!(c.is_excluded("web/app.min.js"));
        assert!(!c.is_excluded("src/lib.rs"));
    }

    #[test]
    fn empty_and_default_exclude_nothing() {
        assert!(!CodeintelConfig::parse(b"").unwrap().is_excluded("a"));
        assert!(!CodeintelConfig::default().is_excluded("a"));
    }

    #[test]
    fn cannot_enable_a_provider() {
        for bad in [
            "provider = \"openai-compatible\"\nallow_external = true\n",
            "[embed]\nprovider = \"openai-compatible\"\nurl = \"http://x\"\n",
            "exclude = []\nallow_external = true\n",
        ] {
            assert!(
                matches!(
                    CodeintelConfig::parse(bad.as_bytes()),
                    Err(ConfigError::Toml(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn bounds() {
        assert!(matches!(
            CodeintelConfig::parse(&vec![b'#'; MAX_CONFIG_BYTES + 1]),
            Err(ConfigError::TooLarge)
        ));
        assert!(matches!(
            CodeintelConfig::parse(&[0xff]),
            Err(ConfigError::NotUtf8)
        ));
        let many = format!("exclude = [{}]", "\"a\",".repeat(MAX_PATTERNS + 1));
        assert!(matches!(
            CodeintelConfig::parse(many.as_bytes()),
            Err(ConfigError::TooManyPatterns)
        ));
        let long = format!("exclude = [\"{}\"]", "a".repeat(MAX_PATTERN_LEN + 1));
        assert!(matches!(
            CodeintelConfig::parse(long.as_bytes()),
            Err(ConfigError::BadPattern(_))
        ));
        assert!(matches!(
            CodeintelConfig::parse(b"exclude = [\"[\"]"),
            Err(ConfigError::BadPattern(_))
        ));
    }

    #[test]
    fn digest_ignores_order_and_duplicates() {
        let a = CodeintelConfig::parse(b"exclude = [\"a\", \"b\"]").unwrap();
        let b = CodeintelConfig::parse(b"exclude = [\"b\", \"a\", \"a\"]").unwrap();
        let c = CodeintelConfig::parse(b"exclude = [\"a\"]").unwrap();
        assert_eq!(a.rules_digest(), b.rules_digest());
        assert_ne!(a.rules_digest(), c.rules_digest());
    }
}
