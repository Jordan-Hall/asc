//! Package URLs (the purl specification) for the packages the dependency parsers produce.
//!
//! A name or version comes from a manifest in a repository, so it is untrusted: every segment is
//! percent-encoded to the unreserved set (plus `:`), so no character of it can end the segment, start a
//! qualifier or subpath, or hide a second `@`. A package that cannot form a valid purl gives `None`.

use super::model::{Dep, Ecosystem};

/// The longest name or version that becomes a purl.
const MAX_NAME: usize = 512;
const MAX_VERSION: usize = 256;

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~' | b':') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b & 15)]));
        }
    }
    out
}

/// A segment is usable when it is not empty and not a path dot segment.
fn segment_ok(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".."
}

/// `namespace/name` from a `/`-separated path: every part but the last is the namespace.
fn path_split(path: &str) -> Option<(Option<String>, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    if !parts.iter().all(|p| segment_ok(p)) {
        return None;
    }
    let (name, ns) = parts.split_last()?;
    let ns = if ns.is_empty() {
        None
    } else {
        Some(ns.iter().map(|p| encode(p)).collect::<Vec<_>>().join("/"))
    };
    Some((ns, encode(name)))
}

fn assemble(kind: &str, ns: Option<String>, name: String, version: &str) -> String {
    let mut out = format!("pkg:{kind}/");
    if let Some(ns) = ns {
        out.push_str(&ns);
        out.push('/');
    }
    out.push_str(&name);
    if !version.is_empty() {
        out.push('@');
        out.push_str(&encode(version));
    }
    out
}

/// The purl of `dep`, or `None` when its name or version cannot form one.
pub fn purl(dep: &Dep) -> Option<String> {
    purl_of(dep.ecosystem, &dep.name, Some(&dep.version))
}

/// The purl of `name` at `version` in `eco`.
pub fn purl_of(eco: Ecosystem, name: &str, version: Option<&str>) -> Option<String> {
    let version = version.unwrap_or("");
    if name.is_empty() || name.len() > MAX_NAME || version.len() > MAX_VERSION {
        return None;
    }
    let single = |kind: &str, n: String| -> Option<String> {
        segment_ok(&n).then(|| assemble(kind, None, encode(&n), version))
    };
    match eco {
        Ecosystem::CratesIo => single("cargo", name.to_owned()),
        Ecosystem::RubyGems => single("gem", name.to_owned()),
        Ecosystem::NuGet => single("nuget", name.to_owned()),
        Ecosystem::CocoaPods => single("cocoapods", name.to_owned()),
        Ecosystem::Hex => single("hex", name.to_lowercase()),
        Ecosystem::Pub => single("pub", name.to_lowercase()),
        Ecosystem::PyPi => single("pypi", eco.canonical(name)),
        Ecosystem::Npm => {
            let lower = name.to_lowercase();
            let (ns, n) = match lower.strip_prefix('@') {
                Some(rest) => {
                    let (scope, pkg) = rest.split_once('/')?;
                    if !segment_ok(scope) || !segment_ok(pkg) || pkg.contains('/') {
                        return None;
                    }
                    (Some(format!("%40{}", encode(scope))), pkg.to_owned())
                }
                None => {
                    if lower.contains('/') || !segment_ok(&lower) {
                        return None;
                    }
                    (None, lower)
                }
            };
            Some(assemble("npm", ns, encode(&n), version))
        }
        Ecosystem::Packagist => {
            let lower = name.to_lowercase();
            let (vendor, pkg) = lower.split_once('/')?;
            if !segment_ok(vendor) || !segment_ok(pkg) || pkg.contains('/') {
                return None;
            }
            Some(assemble(
                "composer",
                Some(encode(vendor)),
                encode(pkg),
                version,
            ))
        }
        Ecosystem::Maven => {
            let (group, artifact) = name.split_once(':')?;
            if !segment_ok(group) || !segment_ok(artifact) || artifact.contains(':') {
                return None;
            }
            Some(assemble(
                "maven",
                Some(encode(group)),
                encode(artifact),
                version,
            ))
        }
        Ecosystem::Go => {
            let (ns, n) = path_split(&name.to_lowercase())?;
            Some(assemble("golang", ns, n, version))
        }
        Ecosystem::SwiftUrl => {
            let (ns, n) = path_split(name)?;
            Some(assemble("swift", ns, n, version))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(eco: Ecosystem, name: &str, v: &str) -> Option<String> {
        purl_of(eco, name, Some(v))
    }

    #[test]
    fn per_ecosystem() {
        use Ecosystem::*;
        assert_eq!(
            p(CratesIo, "serde_json", "1.0.1").unwrap(),
            "pkg:cargo/serde_json@1.0.1"
        );
        assert_eq!(
            p(Npm, "@Angular/Core", "1.2.3").unwrap(),
            "pkg:npm/%40angular/core@1.2.3"
        );
        assert_eq!(
            p(Npm, "left-pad", "1.3.0").unwrap(),
            "pkg:npm/left-pad@1.3.0"
        );
        assert_eq!(
            p(Maven, "org.apache.commons:commons-lang3", "3.12.0").unwrap(),
            "pkg:maven/org.apache.commons/commons-lang3@3.12.0"
        );
        assert_eq!(
            p(Go, "github.com/Foo/Bar", "v1.2.3").unwrap(),
            "pkg:golang/github.com/foo/bar@v1.2.3"
        );
        assert_eq!(p(Go, "std", "v1").unwrap(), "pkg:golang/std@v1");
        assert_eq!(
            p(PyPi, "Foo_Bar.baz", "2.0").unwrap(),
            "pkg:pypi/foo-bar-baz@2.0"
        );
        assert_eq!(
            p(Packagist, "Symfony/Console", "6.0.0").unwrap(),
            "pkg:composer/symfony/console@6.0.0"
        );
        assert_eq!(
            p(RubyGems, "rails", "7.0.0").unwrap(),
            "pkg:gem/rails@7.0.0"
        );
        assert_eq!(
            p(NuGet, "Newtonsoft.Json", "13.0.1").unwrap(),
            "pkg:nuget/Newtonsoft.Json@13.0.1"
        );
        assert_eq!(p(Hex, "Phoenix", "1.7.0").unwrap(), "pkg:hex/phoenix@1.7.0");
        assert_eq!(p(Pub, "http", "1.0.0").unwrap(), "pkg:pub/http@1.0.0");
        assert_eq!(
            p(SwiftUrl, "github.com/apple/swift-argument-parser", "1.0.0").unwrap(),
            "pkg:swift/github.com/apple/swift-argument-parser@1.0.0"
        );
        assert_eq!(
            p(CocoaPods, "AFNetworking", "4.0.0").unwrap(),
            "pkg:cocoapods/AFNetworking@4.0.0"
        );
        assert_eq!(p(CratesIo, "a", "").unwrap(), "pkg:cargo/a");
        assert_eq!(
            p(CratesIo, "a", "1.0+build").unwrap(),
            "pkg:cargo/a@1.0%2Bbuild"
        );
    }

    #[test]
    fn invalid_names_give_none() {
        use Ecosystem::*;
        assert_eq!(p(CratesIo, "", "1"), None);
        assert_eq!(p(CratesIo, "..", "1"), None);
        assert_eq!(p(Npm, "@scope", "1"), None);
        assert_eq!(p(Npm, "a/b", "1"), None);
        assert_eq!(p(Maven, "noartifact", "1"), None);
        assert_eq!(p(Packagist, "novendor", "1"), None);
        assert_eq!(p(Go, "a//b", "1"), None);
        assert_eq!(p(Go, "a/../b", "1"), None);
        assert_eq!(p(CratesIo, &"a".repeat(MAX_NAME + 1), "1"), None);
        assert_eq!(p(CratesIo, "a", &"1".repeat(MAX_VERSION + 1)), None);
    }

    #[test]
    fn hostile_input_stays_in_its_segment() {
        use Ecosystem::*;
        let hostile = "a\"b/c?d#e@f g\u{0}\u{1b}\n%41";
        let mut some = 0;
        for eco in Ecosystem::ALL {
            let name = match eco {
                Maven => format!("g:{hostile}"),
                Packagist => format!("v/{}", hostile.replace('/', "_")),
                Npm => format!("@s/{}", hostile.replace('/', "_")),
                _ => hostile.to_owned(),
            };
            let out = purl_of(eco, &name, Some(hostile)).expect("hostile input still forms a purl");
            some += 1;
            let body = out.strip_prefix("pkg:").unwrap();
            assert!(!body.contains(['?', '#', '"', ' ', '\n', '\0']), "{out}");
            // exactly one `@` introduces the version (npm scopes use %40)
            assert_eq!(body.matches('@').count(), 1, "{out}");
            assert!(body.is_ascii(), "{out}");
            let (before, version) = body.split_once('@').unwrap();
            assert!(!version.contains('/'), "{out}");
            assert!(!before.contains('@'), "{out}");
        }
        assert_eq!(some, Ecosystem::ALL.len());
        assert_eq!(purl_of(CratesIo, "a", None).unwrap(), "pkg:cargo/a");
        let long = "x".repeat(10_000);
        assert_eq!(p(Npm, &long, "1"), None);
        assert_eq!(p(Npm, "a", &long), None);
        // a literal `%41` in a name stays a literal percent sign
        assert_eq!(p(CratesIo, "a%41", "1").unwrap(), "pkg:cargo/a%2541@1");
        assert_eq!(p(Npm, "@a/b@c", "1").unwrap(), "pkg:npm/%40a/b%40c@1");
    }
}
