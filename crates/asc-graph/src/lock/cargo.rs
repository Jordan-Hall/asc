//! `Cargo.lock` reading and dependency closures (SPEC §12A.2.1, §12A.4).
//!
//! [`parse`] keeps only what fingerprints need, in a compact [`Value`]. A lock
//! over a limit or malformed becomes the `unusable` marker, so the linker can
//! fall back to the lockfile's content id. [`Lock::closure`] fingerprints the
//! third-party packages one crate reaches.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::Deserialize;

use crate::canon::Value;
use crate::lock::MAX_LOCKFILE_EDGES;
use crate::model::{Fingerprint, MAX_LOCKFILE_BYTES, MAX_LOCKFILE_PACKAGES, Parsed};

/// Map key that marks a lockfile that could not be used, with the reason.
const UNUSABLE: &str = "unusable";
/// Map key of the package array.
const PACKAGES: &str = "packages";

#[derive(Deserialize)]
struct RawLock {
    #[serde(default)]
    package: Vec<RawPackage>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    #[serde(default)]
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

fn unusable(reason: &str) -> Parsed {
    Parsed(Value::Map(vec![(
        UNUSABLE.to_owned(),
        Value::Str(reason.to_owned()),
    )]))
}

/// Parses lockfile bytes. Never fails: a lock over a SPEC §12A.10 limit or
/// malformed gets the `unusable` marker.
pub fn parse(bytes: &[u8]) -> Parsed {
    if bytes.len() > MAX_LOCKFILE_BYTES {
        return unusable("lockfile larger than 64 MiB");
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return unusable("lockfile is not UTF-8");
    };
    let raw: RawLock = match toml::from_str(text) {
        Ok(r) => r,
        Err(e) => return unusable(&format!("lockfile: {e}")),
    };
    if raw.package.len() > MAX_LOCKFILE_PACKAGES {
        return unusable("lockfile has more than 200000 packages");
    }
    let packages = raw
        .package
        .into_iter()
        .map(|p| {
            let s = |x: String| Value::Str(x);
            Value::Array(vec![
                s(p.name),
                s(p.version),
                s(p.source.unwrap_or_default()),
                s(p.checksum.unwrap_or_default()),
                Value::Array(p.dependencies.into_iter().map(s).collect()),
            ])
        })
        .collect();
    Parsed(Value::Map(vec![(
        PACKAGES.to_owned(),
        Value::Array(packages),
    )]))
}

/// One locked package. An empty `source` is a local (path) package.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Package {
    name: String,
    version: String,
    source: String,
    checksum: String,
}

impl Package {
    fn is_local(&self) -> bool {
        self.source.is_empty()
    }
}

/// A decoded lockfile with dependency references resolved to indexes.
#[derive(Debug)]
pub struct Lock {
    pkgs: Vec<Package>,
    deps: Vec<Vec<u32>>,
    by_name: BTreeMap<String, Vec<u32>>,
}

/// Why a parsed lockfile cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LockError {
    #[error("{0}")]
    Unusable(String),
    #[error("malformed parsed lockfile")]
    Malformed,
}

fn text(v: &Value) -> Option<&str> {
    match v {
        Value::Str(s) => Some(s),
        _ => None,
    }
}

impl Lock {
    /// Decodes the output of [`parse`].
    pub fn from_parsed(parsed: &Parsed) -> Result<Self, LockError> {
        let Value::Map(m) = &parsed.0 else {
            return Err(LockError::Malformed);
        };
        if let Some((_, v)) = m.iter().find(|(k, _)| k == UNUSABLE) {
            return Err(LockError::Unusable(text(v).unwrap_or_default().to_owned()));
        }
        let Some((_, Value::Array(rows))) = m.iter().find(|(k, _)| k == PACKAGES) else {
            return Err(LockError::Malformed);
        };
        let mut pkgs = Vec::with_capacity(rows.len());
        let mut refs: Vec<&[Value]> = Vec::with_capacity(rows.len());
        for row in rows {
            let Value::Array(f) = row else {
                return Err(LockError::Malformed);
            };
            let [name, version, source, checksum, Value::Array(deps)] = f.as_slice() else {
                return Err(LockError::Malformed);
            };
            let (Some(name), Some(version), Some(source), Some(checksum)) =
                (text(name), text(version), text(source), text(checksum))
            else {
                return Err(LockError::Malformed);
            };
            pkgs.push(Package {
                name: name.to_owned(),
                version: version.to_owned(),
                source: source.to_owned(),
                checksum: checksum.to_owned(),
            });
            refs.push(deps);
        }
        let mut by_name: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (i, p) in pkgs.iter().enumerate() {
            by_name.entry(p.name.clone()).or_default().push(i as u32);
        }
        let mut triples = HashSet::with_capacity(pkgs.len());
        if !pkgs
            .iter()
            .all(|p| triples.insert((&p.name, &p.version, &p.source)))
        {
            return Err(LockError::Malformed);
        }
        drop(triples);
        let mut lock = Self {
            pkgs,
            deps: Vec::new(),
            by_name,
        };
        // Running work counter: dependency strings and the candidates scanned
        // for each, checked while resolving.
        let mut work = 0usize;
        let mut all = Vec::with_capacity(refs.len());
        for deps in refs {
            let mut entries = BTreeSet::new();
            for d in deps {
                entries.insert(text(d).ok_or(LockError::Malformed)?);
            }
            let mut out = BTreeSet::new();
            for entry in entries {
                work = work.saturating_add(1 + lock.candidates(entry));
                if work > MAX_LOCKFILE_EDGES {
                    return Err(LockError::Unusable(format!(
                        "lockfile resolves more than {MAX_LOCKFILE_EDGES} dependency edges"
                    )));
                }
                let hits = lock.resolve(entry);
                if hits.len() > 1 {
                    return Err(LockError::Malformed);
                }
                out.extend(hits);
            }
            all.push(out.into_iter().collect());
        }
        lock.deps = all;
        Ok(lock)
    }

    /// How many packages share the name of a `dependencies` entry.
    fn candidates(&self, entry: &str) -> usize {
        let name = entry.split(' ').next().unwrap_or_default();
        self.by_name.get(name).map_or(0, Vec::len)
    }

    /// Resolves a `dependencies` entry: `name`, `name version` or
    /// `name version (source)`.
    fn resolve(&self, entry: &str) -> Vec<u32> {
        let mut parts = entry.splitn(3, ' ');
        let name = parts.next().unwrap_or_default();
        let version = parts.next();
        let source = parts
            .next()
            .map(|s| s.trim_start_matches('(').trim_end_matches(')'));
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| {
                let p = &self.pkgs[i as usize];
                version.is_none_or(|v| p.version == v) && source.is_none_or(|s| p.source == s)
            })
            .collect()
    }

    /// Number of packages.
    pub fn len(&self) -> usize {
        self.pkgs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pkgs.is_empty()
    }

    /// Fingerprint of the third-party packages reachable from the local
    /// package `name` (at `version` when known), as sorted unique
    /// `(name, version, source, checksum)`. Local packages stop the walk and
    /// are not included. `None` when the lock has no such local package.
    pub fn closure(&self, name: &str, version: Option<&str>) -> Option<Fingerprint> {
        let starts: Vec<u32> = self
            .by_name
            .get(name)?
            .iter()
            .copied()
            .filter(|&i| {
                let p = &self.pkgs[i as usize];
                p.is_local() && version.is_none_or(|v| p.version == v)
            })
            .collect();
        if starts.is_empty() {
            return None;
        }
        let mut seen: HashSet<u32> = starts.iter().copied().collect();
        let mut stack = starts;
        let mut found: Vec<&Package> = Vec::new();
        while let Some(i) = stack.pop() {
            for &d in &self.deps[i as usize] {
                let p = &self.pkgs[d as usize];
                if p.is_local() || !seen.insert(d) {
                    continue;
                }
                found.push(p);
                stack.push(d);
            }
        }
        found.sort_by(|a, b| {
            (&a.name, &a.version, &a.source, &a.checksum).cmp(&(
                &b.name,
                &b.version,
                &b.source,
                &b.checksum,
            ))
        });
        found.dedup();
        let rows = found
            .into_iter()
            .map(|p| {
                Value::Array(
                    [&p.name, &p.version, &p.source, &p.checksum]
                        .into_iter()
                        .map(|s| Value::Str(s.clone()))
                        .collect(),
                )
            })
            .collect();
        Some(Value::Array(rows).fingerprint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["lib", "serde 1.0.1", "tokio"]

[[package]]
name = "lib"
version = "0.1.0"
dependencies = ["serde 1.0.1 (registry+https://github.com/rust-lang/crates.io-index)"]

[[package]]
name = "tool"
version = "0.1.0"
dependencies = ["serde 1.0.1"]

[[package]]
name = "serde"
version = "1.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aa"

[[package]]
name = "tokio"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bb"
dependencies = ["mio"]

[[package]]
name = "mio"
version = "0.8.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "cc"
"#;

    fn lock(text: &str) -> Lock {
        Lock::from_parsed(&parse(text.as_bytes())).unwrap()
    }

    fn fp(l: &Lock, name: &str) -> Fingerprint {
        l.closure(name, None).unwrap()
    }

    #[test]
    fn closure_stops_at_local_packages() {
        let l = lock(LOCK);
        assert_eq!(l.len(), 6);
        // `app` reaches serde, tokio and mio but not through `lib`.
        let only_serde = lock(&LOCK.replace("\"tokio\"]", "]"));
        assert_ne!(fp(&l, "app"), fp(&only_serde, "app"));
        // `lib` and `tool` reach the same single package.
        assert_eq!(fp(&l, "lib"), fp(&l, "tool"));
        assert_ne!(fp(&l, "lib"), fp(&l, "app"));
    }

    #[test]
    fn a_bump_changes_only_crates_that_reach_it() {
        let a = lock(LOCK);
        let b = lock(&LOCK.replace("0.8.0", "0.8.1").replace("\"cc\"", "\"dd\""));
        assert_ne!(fp(&a, "app"), fp(&b, "app"));
        assert_eq!(fp(&a, "lib"), fp(&b, "lib"));
        assert_eq!(fp(&a, "tool"), fp(&b, "tool"));
    }

    #[test]
    fn unknown_or_wrong_version_crates_have_no_closure() {
        let l = lock(LOCK);
        assert!(l.closure("nope", None).is_none());
        assert!(l.closure("app", Some("9.9.9")).is_none());
        assert!(l.closure("app", Some("0.1.0")).is_some());
        // Third-party packages are not starting points.
        assert!(l.closure("serde", None).is_none());
    }

    #[test]
    fn empty_and_malformed_locks() {
        assert!(lock("").is_empty());
        let bad = Lock::from_parsed(&parse(b"[[package]\nname"));
        assert!(matches!(bad, Err(LockError::Unusable(_))), "{bad:?}");
        let bad = Lock::from_parsed(&Parsed(Value::Null));
        assert_eq!(bad.unwrap_err(), LockError::Malformed);
    }

    #[test]
    fn over_limit_lockfiles_are_unusable() {
        let big = vec![b' '; MAX_LOCKFILE_BYTES + 1];
        let e = Lock::from_parsed(&parse(&big)).unwrap_err();
        assert!(
            matches!(&e, LockError::Unusable(m) if m.contains("64 MiB")),
            "{e:?}"
        );
        let mut many = String::new();
        for i in 0..=MAX_LOCKFILE_PACKAGES {
            many.push_str(&format!("[[package]]\nname = \"p{i}\"\nversion = \"1\"\n"));
        }
        let e = Lock::from_parsed(&parse(many.as_bytes())).unwrap_err();
        assert!(
            matches!(&e, LockError::Unusable(m) if m.contains("200000")),
            "{e:?}"
        );
    }

    #[test]
    fn dependency_resolution_is_bounded_and_strict() {
        // Name-only entry matching two packages is ambiguous.
        let amb = r#"
[[package]]
name = "app"
version = "0.1.0"
dependencies = ["dup"]
[[package]]
name = "dup"
version = "1"
[[package]]
name = "dup"
version = "2"
"#;
        assert_eq!(
            Lock::from_parsed(&parse(amb.as_bytes())).unwrap_err(),
            LockError::Malformed
        );
        // Duplicate triples are malformed.
        let dup = "[[package]]
name = \"a\"
version = \"1\"
[[package]]
name = \"a\"
version = \"1\"
";
        assert_eq!(
            Lock::from_parsed(&parse(dup.as_bytes())).unwrap_err(),
            LockError::Malformed
        );
        // Repeated strings are deduplicated.
        let rep = "[[package]]
name = \"a\"
version = \"1\"
dependencies = [\"b\", \"b\", \"b 1\"]
[[package]]
name = \"b\"
version = \"1\"
source = \"s\"
";
        let l = lock(rep);
        assert_eq!(l.deps[0], [1]);
        // Many references to a many-version name exceed the edge cap.
        let n = 2_000;
        let mut big = String::new();
        for i in 0..n {
            big.push_str(&format!(
                "[[package]]
name = \"d\"
version = \"{i}\"
"
            ));
        }
        let refs: Vec<String> = (0..n).map(|i| format!("\"d {i}\"")).collect();
        for k in 0..(MAX_LOCKFILE_EDGES / (n * n) + 1) * 2 {
            big.push_str(&format!(
                "[[package]]
name = \"r{k}\"
version = \"1\"
dependencies = [{}]
",
                refs.join(",")
            ));
        }
        let e = Lock::from_parsed(&parse(big.as_bytes())).unwrap_err();
        assert!(matches!(e, LockError::Unusable(_)), "{e:?}");
    }
}
