//! `packages.lock.json` reading (SPEC §12A.4).
//!
//! The .NET `external_fp` is the lockfile's content id, so the parsed form
//! only has to be valid and bounded: [`parse`] keeps the sorted unique
//! `(name, resolved, contentHash)` rows. A lock over a limit or malformed
//! parses to the *unusable* marker; [`unusable`] reads it back.

use std::collections::BTreeSet;

use crate::canon::Value;
use crate::model::{MAX_LOCKFILE_BYTES, MAX_LOCKFILE_PACKAGES, Parsed};

/// Map key that marks a lockfile that could not be used, with the reason.
const UNUSABLE: &str = "unusable";
/// Map key of the package rows.
const PACKAGES: &str = "packages";

fn marker(reason: impl Into<String>) -> Parsed {
    Parsed(Value::Map(vec![(UNUSABLE.into(), Value::Str(reason.into()))]))
}

/// Parses a lockfile with the SPEC §12A.10 limits. Never fails.
pub fn parse(bytes: &[u8]) -> Parsed {
    parse_with(bytes, MAX_LOCKFILE_BYTES, MAX_LOCKFILE_PACKAGES)
}

/// [`parse`] with explicit limits.
pub fn parse_with(bytes: &[u8], max_bytes: usize, max_packages: usize) -> Parsed {
    if bytes.len() > max_bytes {
        return marker(format!("lockfile larger than {max_bytes} bytes"));
    }
    let doc: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(d) => d,
        Err(e) => return marker(format!("lockfile: {e}")),
    };
    let Some(frameworks) = doc.get("dependencies").and_then(|d| d.as_object()) else {
        return marker("lockfile has no dependencies object");
    };
    let mut rows = BTreeSet::new();
    for pkgs in frameworks.values().filter_map(|f| f.as_object()) {
        for (name, p) in pkgs {
            let field = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or_default();
            rows.insert((name.clone(), field("resolved").to_owned(), field("contentHash").to_owned()));
            if rows.len() > max_packages {
                return marker(format!("lockfile has more than {max_packages} packages"));
            }
        }
    }
    let rows = rows
        .into_iter()
        .map(|(n, r, h)| Value::Array(vec![Value::Str(n), Value::Str(r), Value::Str(h)]))
        .collect();
    Parsed(Value::Map(vec![(PACKAGES.into(), Value::Array(rows))]))
}

/// The reason a parsed lockfile cannot be used, or `None` when it is usable.
pub fn unusable(parsed: &Parsed) -> Option<String> {
    let Value::Map(m) = &parsed.0 else {
        return Some("malformed parsed lockfile".into());
    };
    match m.iter().find(|(k, _)| k == UNUSABLE) {
        Some((_, Value::Str(why))) => Some(why.clone()),
        Some(_) => Some("malformed parsed lockfile".into()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{"version":1,"dependencies":{
        "net8.0":{"Newtonsoft.Json":{"type":"Direct","requested":"[13.0.3, )","resolved":"13.0.3","contentHash":"abc"}},
        "net6.0":{"Newtonsoft.Json":{"type":"Direct","requested":"[13.0.3, )","resolved":"13.0.3","contentHash":"abc"},
                  "Serilog":{"type":"Transitive","resolved":"3.1.1","contentHash":"def"}}}}"#;

    #[test]
    fn rows_are_sorted_and_unique_across_frameworks() {
        let p = parse(LOCK.as_bytes());
        assert_eq!(unusable(&p), None);
        let Value::Map(m) = &p.0 else { panic!() };
        let Value::Array(rows) = &m[0].1 else { panic!() };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            Value::Array(vec![
                Value::Str("Newtonsoft.Json".into()),
                Value::Str("13.0.3".into()),
                Value::Str("abc".into())
            ])
        );
    }

    #[test]
    fn malformed_locks_are_unusable_values() {
        assert!(unusable(&parse(b"{")).is_some());
        assert!(unusable(&parse(b"[]")).is_some());
        assert!(unusable(&Parsed(Value::Null)).is_some());
    }

    #[test]
    fn over_limit_lock_is_unusable() {
        let why = unusable(&parse_with(LOCK.as_bytes(), 10, 100)).unwrap();
        assert!(why.contains("larger than 10"), "{why}");
        let why = unusable(&parse_with(LOCK.as_bytes(), 1 << 20, 1)).unwrap();
        assert!(why.contains("more than 1 "), "{why}");
        let big = vec![b' '; MAX_LOCKFILE_BYTES + 1];
        assert!(unusable(&parse(&big)).unwrap().contains("larger than"));
    }
}
