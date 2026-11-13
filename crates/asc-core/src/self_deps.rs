//! The registry packages asc was built against, embedded from the workspace `Cargo.lock`.
//!
//! The lockfile is a superset of what one binary links, so a consumer checking these names
//! against an advisory database can over-report but not miss a dependency. An empty list means
//! the lockfile was not available at build time; callers must treat that as "unknown", not clean.

use std::sync::OnceLock;

/// Stanzas written by `build.rs`.
const EMBEDDED: &str = include_str!(concat!(env!("OUT_DIR"), "/self_deps.txt"));

/// `(name, version)` of every registry package, sorted and unique. Parsed once.
pub fn packages() -> &'static [(&'static str, &'static str)] {
    static CELL: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    CELL.get_or_init(|| parse_lock(EMBEDDED))
}

/// Parses `[[package]]` stanzas, keeping only registry sources (git and path packages are
/// skipped). Result is sorted by name then version, without duplicates.
pub fn parse_lock(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let (mut name, mut version, mut registry) = (None, None, false);
    // A stanza ends at the next header or a blank line; a closing pass handles the last one.
    for line in text.lines().chain(std::iter::once("")) {
        if line.starts_with("[[") || line.trim().is_empty() {
            if let (Some(n), Some(v), true) = (name.take(), version.take(), registry) {
                out.push((n, v));
            }
            registry = false;
        } else if let Some(v) = quoted(line, "name") {
            name = Some(v);
        } else if let Some(v) = quoted(line, "version") {
            version = Some(v);
        } else if let Some(v) = quoted(line, "source") {
            registry = v.starts_with("registry+");
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Value of a `key = "value"` line, or `None` for any other line.
fn quoted<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
    rest.trim().strip_prefix('"')?.strip_suffix('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
version = 4

[[package]]
name = "zeta"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"
dependencies = [
 "alpha",
]

[[package]]
name = "local"
version = "0.1.0"

[[package]]
name = "gitdep"
version = "0.2.0"
source = "git+https://example.invalid/x#abc"

[[package]]
name = "alpha"
version = "0.9.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "alpha"
version = "0.10.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "alpha"
version = "0.9.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

    #[test]
    fn skips_git_and_path_packages() {
        let got = parse_lock(SAMPLE);
        assert!(got.iter().all(|(n, _)| *n != "local" && *n != "gitdep"));
        assert!(got.contains(&("zeta", "1.2.3")));
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(parse_lock("").is_empty());
        assert!(parse_lock("version = 4\n").is_empty());
    }

    #[test]
    fn sorted_and_unique() {
        let got = parse_lock(SAMPLE);
        assert_eq!(
            got,
            vec![("alpha", "0.10.0"), ("alpha", "0.9.0"), ("zeta", "1.2.3")]
        );
    }

    #[test]
    fn embedded_list_is_sorted_unique_and_has_workspace_deps() {
        let p = packages();
        assert!(p.windows(2).all(|w| w[0] < w[1]));
        assert!(p.iter().any(|(n, _)| *n == "blake3"));
    }
}
