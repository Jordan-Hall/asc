//! A CycloneDX 1.6 JSON bill of materials for the dependencies of one revision.
//!
//! The document is a pure function of its input: no clock, no randomness. The serial number derives from
//! the changeset id and components are sorted, so one revision gives the same bytes every time. The
//! parsers keep no dependency graph, so `dependencies` holds only root to direct dependencies.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{Value, json};

use super::model::{Dep, DepKind, Ecosystem};
use super::purl::purl_of;

/// The result of [`cyclonedx`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sbom {
    /// The JSON document.
    pub document: Vec<u8>,
    /// Components in the document.
    pub components: usize,
    /// More distinct packages existed than the limit allowed.
    pub truncated: bool,
}

/// The `urn:uuid:` serial number for a changeset id: a v8 UUID over a BLAKE3 hash.
pub fn serial_number(changeset: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"asc-sbom-serial-v1\0");
    h.update(changeset.as_bytes());
    let hash = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    let id = uuid::Builder::from_custom_bytes(bytes).into_uuid();
    format!("urn:uuid:{id}")
}

struct Entry {
    name: String,
    purl: Option<String>,
    direct: bool,
    path: String,
    pinned: bool,
    ranges: BTreeSet<String>,
}

/// The name two spellings of one package share: the ecosystem's canonical form, lower-cased for the
/// registries whose purl is lower-cased.
fn canon_key(d: &Dep) -> String {
    let c = d.ecosystem.canonical(&d.name);
    match d.ecosystem {
        Ecosystem::Npm | Ecosystem::Hex => c.to_lowercase(),
        _ => c,
    }
}

/// Builds the document for `deps` of repository `repo` at `changeset`, with at most `limit` components.
///
/// A manifest floor is not an installed version, so a package known only from a manifest has no
/// `version` and an unversioned purl, and a manifest entry is dropped when a pin of the package exists.
pub fn cyclonedx(deps: &[Dep], repo: &str, changeset: &str, limit: usize) -> Sbom {
    let pinned_names: HashSet<(&'static str, String)> = deps
        .iter()
        .filter(|d| d.kind == DepKind::Lock)
        .map(|d| (d.ecosystem.osv_name(), canon_key(d)))
        .collect();
    // (ecosystem, name, version); an unpinned entry has the empty version.
    let mut map: BTreeMap<(&'static str, String, String), Entry> = BTreeMap::new();
    for d in deps {
        let eco = d.ecosystem.osv_name();
        let canon = canon_key(d);
        let pinned = d.kind == DepKind::Lock;
        if !pinned && pinned_names.contains(&(eco, canon.clone())) {
            continue;
        }
        let version = if pinned {
            d.version.clone()
        } else {
            String::new()
        };
        let e = map.entry((eco, canon, version)).or_insert_with(|| Entry {
            name: d.name.clone(),
            purl: purl_of(d.ecosystem, &d.name, pinned.then_some(d.version.as_str())),
            direct: false,
            path: d.path.clone(),
            pinned,
            ranges: BTreeSet::new(),
        });
        e.direct |= d.direct;
        if !pinned && !d.requirement.is_empty() {
            e.ranges.insert(d.requirement.clone());
        }
        if (d.path.as_str(), d.name.as_str()) < (e.path.as_str(), e.name.as_str()) {
            e.path.clone_from(&d.path);
            e.name.clone_from(&d.name);
            e.purl = purl_of(d.ecosystem, &d.name, pinned.then_some(d.version.as_str()));
        }
    }
    // A dropped floor still says the package is named by a manifest.
    let direct_names: HashSet<(&'static str, String)> = deps
        .iter()
        .filter(|d| d.direct)
        .map(|d| (d.ecosystem.osv_name(), canon_key(d)))
        .collect();
    for ((eco, canon, _), e) in &mut map {
        e.direct |= direct_names.contains(&(*eco, canon.clone()));
    }
    let truncated = map.len() > limit;
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert("root".to_owned());
    let mut components = Vec::new();
    let mut direct_refs = Vec::new();
    for ((eco, canon, version), e) in map.into_iter().take(limit) {
        let base = e.purl.clone().unwrap_or_else(|| {
            let mut h = blake3::Hasher::new();
            h.update(canon.as_bytes());
            h.update(&[0]);
            h.update(version.as_bytes());
            format!("asc:{eco}:{}", h.finalize().to_hex())
        });
        let mut bom_ref = base.clone();
        let mut n = 1u32;
        while !seen.insert(bom_ref.clone()) {
            n += 1;
            bom_ref = format!("{base}#{n}");
        }
        let mut props = vec![
            json!({"name": "asc:ecosystem", "value": eco}),
            json!({"name": "asc:direct", "value": e.direct.to_string()}),
            json!({"name": "asc:path", "value": e.path}),
        ];
        if !e.pinned {
            props.push(json!({"name": "asc:pinned", "value": "false"}));
            if !e.ranges.is_empty() {
                let range = e.ranges.iter().cloned().collect::<Vec<_>>().join(", ");
                props.push(json!({"name": "asc:range", "value": range}));
            }
        }
        let mut c = json!({
            "type": "library",
            "bom-ref": bom_ref,
            "name": e.name,
            "scope": "required",
            "properties": props,
        });
        if let Some(o) = c.as_object_mut() {
            if e.pinned {
                o.insert("version".into(), Value::String(version));
            }
            if let Some(p) = e.purl {
                o.insert("purl".into(), Value::String(p));
            }
        }
        if e.direct {
            direct_refs.push(bom_ref);
        }
        components.push(c);
    }
    let count = components.len();
    let mut doc = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": serial_number(changeset),
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "bom-ref": "root",
                "name": repo,
                "version": changeset,
            },
        },
        "components": components,
        "dependencies": [{"ref": "root", "dependsOn": direct_refs}],
    });
    if truncated && let Some(o) = doc.as_object_mut() {
        o.insert(
            "compositions".into(),
            json!([{"aggregate": "incomplete", "assemblies": ["root"]}]),
        );
        o.insert(
            "metadata".into(),
            json!({
                "component": {
                    "type": "application",
                    "bom-ref": "root",
                    "name": repo,
                    "version": changeset,
                },
                "properties": [{"name": "asc:truncated", "value": "true"}],
            }),
        );
    }
    Sbom {
        document: serde_json::to_vec_pretty(&doc).unwrap_or_default(),
        components: count,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::model::Ecosystem;

    fn sample() -> Vec<Dep> {
        vec![
            Dep::lock(Ecosystem::Npm, "left-pad", "1.3.0", "package-lock.json"),
            Dep::manifest(Ecosystem::CratesIo, "serde", "1", "1.0.0", "Cargo.toml"),
            Dep::lock(Ecosystem::CratesIo, "serde", "1.0.0", "Cargo.lock"),
        ]
    }

    fn parse(s: &Sbom) -> Value {
        serde_json::from_slice(&s.document).unwrap()
    }

    #[test]
    fn golden() {
        let s = cyclonedx(&sample(), "demo", "abc123", 100);
        let want = json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.6",
            "serialNumber": serial_number("abc123"),
            "version": 1,
            "metadata": {"component": {"type": "application", "bom-ref": "root", "name": "demo", "version": "abc123"}},
            "components": [
                {
                    "type": "library", "bom-ref": "pkg:cargo/serde@1.0.0", "name": "serde",
                    "version": "1.0.0", "scope": "required", "purl": "pkg:cargo/serde@1.0.0",
                    "properties": [
                        {"name": "asc:ecosystem", "value": "crates.io"},
                        {"name": "asc:direct", "value": "true"},
                        {"name": "asc:path", "value": "Cargo.lock"}
                    ]
                },
                {
                    "type": "library", "bom-ref": "pkg:npm/left-pad@1.3.0", "name": "left-pad",
                    "version": "1.3.0", "scope": "required", "purl": "pkg:npm/left-pad@1.3.0",
                    "properties": [
                        {"name": "asc:ecosystem", "value": "npm"},
                        {"name": "asc:direct", "value": "false"},
                        {"name": "asc:path", "value": "package-lock.json"}
                    ]
                }
            ],
            "dependencies": [{"ref": "root", "dependsOn": ["pkg:cargo/serde@1.0.0"]}]
        });
        assert_eq!(parse(&s), want);
        assert!(!String::from_utf8(s.document).unwrap().contains("timestamp"));
        assert_eq!(s.components, 2);
        assert!(!s.truncated);
    }

    #[test]
    fn serial_is_v8_and_stable() {
        let a = serial_number("x");
        assert_eq!(a, serial_number("x"));
        assert_ne!(a, serial_number("y"));
        let id = uuid::Uuid::parse_str(a.strip_prefix("urn:uuid:").unwrap()).unwrap();
        assert_eq!(id.get_version_num(), 8);
    }

    #[test]
    fn deterministic_and_order_free() {
        let mut d = sample();
        let a = cyclonedx(&d, "demo", "c1", 100);
        assert_eq!(a, cyclonedx(&d, "demo", "c1", 100));
        d.reverse();
        assert_eq!(a, cyclonedx(&d, "demo", "c1", 100));
    }

    #[test]
    fn manifest_floor_is_marked_unpinned() {
        let d = vec![Dep::manifest(
            Ecosystem::Npm,
            "a",
            "^1",
            "1.0.0",
            "package.json",
        )];
        let v = parse(&cyclonedx(&d, "r", "c", 10));
        let props = v["components"][0]["properties"].as_array().unwrap();
        assert!(props.contains(&json!({"name": "asc:pinned", "value": "false"})));
        let c = &v["components"][0];
        assert!(c.get("version").is_none());
        assert_eq!(c["purl"], "pkg:npm/a");
        assert!(props.contains(&json!({"name": "asc:range", "value": "^1"})));
    }

    #[test]
    fn pin_drops_floor_of_the_same_package() {
        let d = vec![
            Dep::manifest(Ecosystem::Npm, "A", "^1", "1.0.0", "package.json"),
            Dep::lock(Ecosystem::Npm, "a", "1.4.2", "package-lock.json"),
        ];
        let s = cyclonedx(&d, "r", "c", 10);
        assert_eq!(s.components, 1);
        let v = parse(&s);
        assert_eq!(v["components"][0]["version"], "1.4.2");
    }

    #[test]
    fn npm_and_hex_spellings_merge() {
        let d = vec![
            Dep::lock(Ecosystem::Npm, "Foo", "1.0.0", "a"),
            Dep::lock(Ecosystem::Npm, "foo", "1.0.0", "b"),
            Dep::lock(Ecosystem::Hex, "Ecto", "3.0.0", "a"),
            Dep::lock(Ecosystem::Hex, "ecto", "3.0.0", "b"),
        ];
        assert_eq!(cyclonedx(&d, "r", "c", 10).components, 2);
    }

    #[test]
    fn truncation_is_declared() {
        let d: Vec<Dep> = (0..3)
            .map(|i| Dep::lock(Ecosystem::CratesIo, &format!("c{i}"), "1.0.0", "Cargo.lock"))
            .collect();
        let v = parse(&cyclonedx(&d, "r", "c", 2));
        assert_eq!(
            v["compositions"],
            json!([{"aggregate": "incomplete", "assemblies": ["root"]}])
        );
        assert_eq!(
            v["metadata"]["properties"],
            json!([{"name": "asc:truncated", "value": "true"}])
        );
        let v = parse(&cyclonedx(&d, "r", "c", 3));
        assert!(v.get("compositions").is_none());
        assert!(v["metadata"].get("properties").is_none());
    }

    #[test]
    fn fallback_bom_ref_is_bounded() {
        let name = format!("a\"b/c{}", "x".repeat(400));
        let d = vec![Dep::lock(Ecosystem::Npm, &name, "1", "p")];
        let v = parse(&cyclonedx(&d, "r", "c", 10));
        let r = v["components"][0]["bom-ref"].as_str().unwrap();
        assert!(r.starts_with("asc:npm:"));
        assert_eq!(r.len(), "asc:npm:".len() + 64);
    }

    #[test]
    fn dedup_by_canonical_name_and_version() {
        let d = vec![
            Dep::lock(Ecosystem::PyPi, "Foo_Bar", "1.0", "a/requirements.txt"),
            Dep::lock(Ecosystem::PyPi, "foo-bar", "1.0", "b/requirements.txt"),
            Dep::lock(Ecosystem::PyPi, "foo-bar", "2.0", "b/requirements.txt"),
        ];
        let s = cyclonedx(&d, "r", "c", 10);
        assert_eq!(s.components, 2);
    }

    #[test]
    fn truncation() {
        let d: Vec<Dep> = (0..5)
            .map(|i| Dep::lock(Ecosystem::CratesIo, &format!("c{i}"), "1.0.0", "Cargo.lock"))
            .collect();
        let s = cyclonedx(&d, "r", "c", 3);
        assert!(s.truncated);
        assert_eq!(s.components, 3);
        let v = parse(&s);
        assert_eq!(v["components"].as_array().unwrap().len(), 3);
        assert_eq!(v["components"][0]["name"], "c0");
        let s = cyclonedx(&d, "r", "c", 5);
        assert!(!s.truncated);
        assert_eq!(s.components, 5);
    }

    #[test]
    fn hostile_name_stays_valid() {
        let name = format!("a\"b/c?d#e@f\u{0}\u{7}\n{}", "x".repeat(5000));
        let d = vec![
            Dep::lock(
                Ecosystem::CratesIo,
                "a\"b/c?d#e@f\u{0}\u{7}\n",
                "1\"?#@",
                "p",
            ),
            Dep::lock(Ecosystem::Npm, &name, "1", "p"),
        ];
        let s = cyclonedx(&d, "re\"po\n", "c", 10);
        let v = parse(&s);
        let comps = v["components"].as_array().unwrap();
        assert_eq!(comps.len(), 2);
        let cargo = comps.iter().find(|c| c["purl"].is_string()).unwrap();
        let p = cargo["purl"].as_str().unwrap();
        assert!(!p.contains(['?', '#', '"', '\n', '\0']));
        assert_eq!(p.matches('@').count(), 1);
        // the too-long name has no purl but still a unique bom-ref
        let long = comps.iter().find(|c| c.get("purl").is_none()).unwrap();
        assert!(long["bom-ref"].as_str().unwrap().starts_with("asc:npm:"));
    }
}
