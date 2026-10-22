//! `uv.lock` reading and dependency closures (SPEC §12A.2.2, §12A.4).
//!
//! [`parse`] keeps only what fingerprints need, in a compact [`Value`]. A lock
//! over a limit or malformed becomes the `unusable` marker, so the linker can
//! fall back to the lockfile's content id. [`Lock::closure`] fingerprints the
//! third-party packages one workspace member reaches, stopping at workspace,
//! editable, virtual and directory sources.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::canon::Value;
use crate::lock::MAX_LOCKFILE_EDGES;
use crate::model::{Fingerprint, MAX_LOCKFILE_BYTES, MAX_LOCKFILE_PACKAGES, Parsed};

/// Map key that marks a lockfile that could not be used, with the reason.
const UNUSABLE: &str = "unusable";
/// Map key of the package array.
const PACKAGES: &str = "packages";
/// Source keys of packages that live in the repository.
const LOCAL_SOURCES: [&str; 4] = ["workspace", "editable", "virtual", "directory"];

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
    source: Option<toml::Table>,
    sdist: Option<RawDist>,
    #[serde(default)]
    wheels: Vec<RawDist>,
    #[serde(default)]
    dependencies: Vec<RawDep>,
    #[serde(default, rename = "optional-dependencies")]
    optional: BTreeMap<String, Vec<RawDep>>,
    #[serde(default, rename = "dev-dependencies")]
    dev: BTreeMap<String, Vec<RawDep>>,
}

#[derive(Deserialize)]
struct RawDist {
    hash: Option<String>,
}

#[derive(Deserialize)]
struct RawDep {
    name: String,
    version: Option<String>,
    source: Option<toml::Table>,
}

fn unusable(reason: &str) -> Parsed {
    Parsed(Value::Map(vec![(
        UNUSABLE.to_owned(),
        Value::Str(reason.to_owned()),
    )]))
}

/// A source table as `key=value` pairs joined by `;`, in key order.
fn source_text(t: Option<&toml::Table>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    let mut parts: Vec<String> = t
        .iter()
        .map(|(k, v)| match v {
            toml::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    parts.sort();
    parts.join(";")
}

/// The sdist hash, else the sorted wheel hashes joined by `,`.
fn dist_hash(sdist: Option<&RawDist>, wheels: &[RawDist]) -> String {
    if let Some(h) = sdist.and_then(|d| d.hash.as_deref()) {
        return h.to_owned();
    }
    let mut hashes: Vec<&str> = wheels.iter().filter_map(|w| w.hash.as_deref()).collect();
    hashes.sort_unstable();
    hashes.join(",")
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
    let entries = raw.package.iter().fold(0usize, |n, p| {
        let extra = p.optional.values().chain(p.dev.values()).map(Vec::len);
        n.saturating_add(p.dependencies.len())
            .saturating_add(extra.fold(0, usize::saturating_add))
    });
    if entries > MAX_LOCKFILE_EDGES {
        return unusable("lockfile has too many dependency entries");
    }
    let s = Value::Str;
    let packages = raw
        .package
        .into_iter()
        .map(|p| {
            let hash = dist_hash(p.sdist.as_ref(), &p.wheels);
            let deps = p
                .dependencies
                .into_iter()
                .chain(p.optional.into_values().flatten())
                .chain(p.dev.into_values().flatten())
                .map(|d| {
                    Value::Array(vec![
                        s(d.name),
                        s(d.version.unwrap_or_default()),
                        s(source_text(d.source.as_ref())),
                    ])
                })
                .collect();
            Value::Array(vec![
                s(p.name),
                s(p.version),
                s(source_text(p.source.as_ref())),
                s(hash),
                Value::Array(deps),
            ])
        })
        .collect();
    Parsed(Value::Map(vec![(
        PACKAGES.to_owned(),
        Value::Array(packages),
    )]))
}

/// One locked package. A local package lives in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Package {
    name: String,
    version: String,
    source: String,
    hash: String,
    local: bool,
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

fn is_local(source: &str) -> bool {
    source.split(';').any(|kv| {
        LOCAL_SOURCES
            .iter()
            .any(|k| kv.strip_prefix(k).is_some_and(|r| r.starts_with('=')))
    })
}

/// A dependency reference as decoded text: name, version, source.
type DepRef<'a> = (&'a str, &'a str, &'a str);

fn dep_ref(v: &Value) -> Result<DepRef<'_>, LockError> {
    let Value::Array(f) = v else {
        return Err(LockError::Malformed);
    };
    let [name, version, source] = f.as_slice() else {
        return Err(LockError::Malformed);
    };
    match (text(name), text(version), text(source)) {
        (Some(n), Some(v), Some(s)) => Ok((n, v, s)),
        _ => Err(LockError::Malformed),
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
            let [name, version, source, hash, Value::Array(deps)] = f.as_slice() else {
                return Err(LockError::Malformed);
            };
            let (Some(name), Some(version), Some(source), Some(hash)) =
                (text(name), text(version), text(source), text(hash))
            else {
                return Err(LockError::Malformed);
            };
            pkgs.push(Package {
                name: name.to_owned(),
                version: version.to_owned(),
                source: source.to_owned(),
                hash: hash.to_owned(),
                local: is_local(source),
            });
            refs.push(deps);
        }
        let mut by_name: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (i, p) in pkgs.iter().enumerate() {
            by_name.entry(p.name.clone()).or_default().push(i as u32);
        }
        let mut lock = Self {
            pkgs,
            deps: Vec::new(),
            by_name,
        };
        // Running work counter: dependency entries and the candidates scanned
        // for each, checked before resolving so a wildcard cannot fan out.
        let mut work = 0usize;
        let mut all = Vec::with_capacity(refs.len());
        for deps in refs {
            let mut out = Vec::new();
            for d in deps {
                let r = dep_ref(d)?;
                work = work.saturating_add(1 + lock.candidates(r.0));
                if work > MAX_LOCKFILE_EDGES {
                    return Err(LockError::Unusable(format!(
                        "lockfile resolves more than {MAX_LOCKFILE_EDGES} dependency edges"
                    )));
                }
                out.extend(lock.resolve(r));
            }
            out.sort_unstable();
            out.dedup();
            all.push(out);
        }
        lock.deps = all;
        Ok(lock)
    }

    /// How many packages share a dependency's name.
    fn candidates(&self, name: &str) -> usize {
        self.by_name.get(name).map_or(0, Vec::len)
    }

    /// Resolves a dependency reference; an empty version or source matches any.
    fn resolve(&self, (name, version, source): DepRef<'_>) -> Vec<u32> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| {
                let p = &self.pkgs[i as usize];
                (version.is_empty() || p.version == version)
                    && (source.is_empty() || p.source == source)
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
    /// package `name`, as sorted unique `(name, version, source, hash)`.
    /// Local packages stop the walk and are not included. `None` when the
    /// lock has no such local package.
    pub fn closure(&self, name: &str) -> Option<Fingerprint> {
        let starts: Vec<u32> = self
            .by_name
            .get(name)?
            .iter()
            .copied()
            .filter(|&i| self.pkgs[i as usize].local)
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
                if p.local || !seen.insert(d) {
                    continue;
                }
                found.push(p);
                stack.push(d);
            }
        }
        found.sort_by(|a, b| {
            (&a.name, &a.version, &a.source, &a.hash)
                .cmp(&(&b.name, &b.version, &b.source, &b.hash))
        });
        found.dedup();
        let rows = found
            .into_iter()
            .map(|p| {
                Value::Array(
                    [&p.name, &p.version, &p.source, &p.hash]
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
version = 1
requires-python = ">=3.11"

[[package]]
name = "app"
version = "0.1.0"
source = { editable = "app" }
dependencies = [
    { name = "lib" },
    { name = "numpy" },
    { name = "soma" },
]

[package.optional-dependencies]
extra = [{ name = "tqdm" }]

[[package]]
name = "lib"
version = "0.1.0"
source = { editable = "lib" }
dependencies = [{ name = "numpy" }]

[package.dev-dependencies]
dev = [{ name = "pytest" }]

[[package]]
name = "tool"
version = "0.1.0"
source = { virtual = "tool" }
dependencies = [{ name = "numpy" }]

[[package]]
name = "numpy"
version = "1.26.0"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://x/numpy.tar.gz", hash = "sha256:aa" }

[[package]]
name = "tqdm"
version = "4.0.0"
source = { registry = "https://pypi.org/simple" }
wheels = [{ url = "https://x/tqdm.whl", hash = "sha256:bb" }]

[[package]]
name = "pytest"
version = "8.0.0"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://x/pytest.tar.gz", hash = "sha256:cc" }

[[package]]
name = "soma"
version = "0.2.0"
source = { git = "https://example.com/soma.git?rev=abc#abc" }
dependencies = [{ name = "numpy", version = "1.26.0", source = { registry = "https://pypi.org/simple" } }]
"#;

    fn lock(text: &str) -> Lock {
        Lock::from_parsed(&parse(text.as_bytes())).unwrap()
    }

    fn fp(l: &Lock, name: &str) -> Fingerprint {
        l.closure(name).unwrap()
    }

    #[test]
    fn closure_follows_all_dependency_tables_and_stops_at_local_packages() {
        let l = lock(LOCK);
        assert_eq!(l.len(), 7);
        // `app` reaches numpy, tqdm (optional), soma (git) but not through `lib`.
        let without = lock(&LOCK.replace("    { name = \"soma\" },\n", ""));
        assert_ne!(fp(&l, "app"), fp(&without, "app"));
        // `lib` reaches numpy and pytest (dev); `tool` reaches numpy only.
        assert_ne!(fp(&l, "lib"), fp(&l, "tool"));
        assert_ne!(fp(&l, "lib"), fp(&l, "app"));
    }

    #[test]
    fn git_sources_are_part_of_the_external_closure() {
        let a = lock(LOCK);
        let b = lock(&LOCK.replace("rev=abc#abc", "rev=def#def"));
        assert_ne!(fp(&a, "app"), fp(&b, "app"));
        assert_eq!(fp(&a, "lib"), fp(&b, "lib"));
    }

    #[test]
    fn a_bump_changes_only_members_that_reach_it() {
        let a = lock(LOCK);
        let b = lock(
            &LOCK
                .replace("sha256:bb", "sha256:b2")
                .replace("4.0.0", "4.0.1"),
        );
        // tqdm is reached through `app`'s optional dependencies only.
        assert_ne!(fp(&a, "app"), fp(&b, "app"));
        assert_eq!(fp(&a, "lib"), fp(&b, "lib"));
        assert_eq!(fp(&a, "tool"), fp(&b, "tool"));
        // numpy's sdist hash reaches all three.
        let c = lock(&LOCK.replace("sha256:aa", "sha256:a2"));
        for m in ["app", "lib", "tool"] {
            assert_ne!(fp(&a, m), fp(&c, m), "{m}");
        }
    }

    #[test]
    fn unknown_or_third_party_names_have_no_closure() {
        let l = lock(LOCK);
        assert!(l.closure("nope").is_none());
        assert!(l.closure("numpy").is_none());
        assert!(l.closure("soma").is_none());
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
    fn wildcard_dependencies_over_the_edge_budget_are_unusable() {
        // Every package shares one name and every dependency is a wildcard.
        let n = 2_000;
        let mut big = String::from(
            "version = 1
",
        );
        for _ in 0..n {
            big.push_str(
                "[[package]]
name = \"x\"
dependencies = [{ name = \"x\" }]
",
            );
        }
        let e = Lock::from_parsed(&parse(big.as_bytes())).unwrap_err();
        assert!(matches!(e, LockError::Unusable(_)), "{e:?}");
        // Too many dependency entries is rejected while parsing.
        let m = MAX_LOCKFILE_EDGES / n + 1;
        let mut big = String::from(
            "version = 1
",
        );
        for _ in 0..n {
            big.push_str(&format!(
                "[[package]]
name = \"x\"
dependencies = [{}]
",
                vec!["{ name = \"x\" }"; m.min(1500)].join(",")
            ));
        }
        let e = Lock::from_parsed(&parse(big.as_bytes())).unwrap_err();
        assert!(matches!(e, LockError::Unusable(_)), "{e:?}");
    }
}
