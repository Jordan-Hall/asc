//! The answers of the public registries, read into one shape.
//!
//! One function per registry turns the body of its answer into [`Versions`]: the stable releases, oldest
//! first, the newest of them, and for the two registries whose lockfiles hold the hash of what they pin
//! (crates.io and npm) the hash of each version. Pure: nothing here asks the network.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::parse::xml;
use super::version::{Scheme, Version};

/// Most versions kept, the newest ones.
pub const KEEP: usize = 400;

/// What a registry says about one version, where a lockfile needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub checksum: Option<String>,
    pub integrity: Option<String>,
    pub tarball: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    /// The newest stable version.
    pub latest: Option<String>,
    /// Stable versions, oldest first, at most [`KEEP`].
    pub versions: Vec<String>,
    pub meta: BTreeMap<String, Meta>,
}

/// Builds [`Versions`] from `(version, meta)` pairs: parseable, stable, ordered, newest [`KEEP`].
fn finish(scheme: Scheme, raw: Vec<(String, Meta)>, hint: Option<&str>) -> Versions {
    let mut v: Vec<(Version, String, Meta)> = raw
        .into_iter()
        .filter_map(|(s, m)| Version::parse(scheme, &s).map(|p| (p, s, m)))
        .filter(|(p, _, _)| !p.is_prerelease())
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.dedup_by(|a, b| a.0 == b.0);
    if v.len() > KEEP {
        v.drain(..v.len() - KEEP);
    }
    let latest = hint
        .filter(|h| v.iter().any(|(_, s, _)| s == h))
        .map(str::to_owned)
        .or_else(|| v.last().map(|(_, s, _)| s.clone()));
    let mut meta = BTreeMap::new();
    for (_, s, m) in &v {
        if *m != Meta::default() {
            meta.insert(s.clone(), m.clone());
        }
    }
    Versions {
        latest,
        versions: v.into_iter().map(|(_, s, _)| s).collect(),
        meta,
    }
}

fn plain(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

pub fn parse_crates(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let list = doc
        .get("versions")
        .and_then(Value::as_array)
        .ok_or("no versions")?;
    let raw = list
        .iter()
        .filter(|v| v.get("yanked").and_then(Value::as_bool) != Some(true))
        .filter_map(|v| {
            Some((
                plain(v.get("num"))?.to_owned(),
                Meta {
                    checksum: plain(v.get("checksum")).map(str::to_owned),
                    ..Meta::default()
                },
            ))
        })
        .collect();
    Ok(finish(Scheme::Semver, raw, None))
}

pub fn parse_npm(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let versions = doc
        .get("versions")
        .and_then(Value::as_object)
        .ok_or("no versions")?;
    let raw = versions
        .iter()
        .filter(|(_, v)| v.get("deprecated").is_none())
        .map(|(k, v)| {
            let dist = v.get("dist");
            (
                k.clone(),
                Meta {
                    integrity: plain(dist.and_then(|d| d.get("integrity"))).map(str::to_owned),
                    tarball: plain(dist.and_then(|d| d.get("tarball"))).map(str::to_owned),
                    ..Meta::default()
                },
            )
        })
        .collect();
    let hint = plain(doc.get("dist-tags").and_then(|t| t.get("latest")));
    Ok(finish(Scheme::Semver, raw, hint))
}

/// The answer of `/<name>/latest`: one version.
pub fn parse_npm_latest(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let v = plain(doc.get("version")).ok_or("no version")?;
    let dist = doc.get("dist");
    let raw = vec![(
        v.to_owned(),
        Meta {
            integrity: plain(dist.and_then(|d| d.get("integrity"))).map(str::to_owned),
            tarball: plain(dist.and_then(|d| d.get("tarball"))).map(str::to_owned),
            ..Meta::default()
        },
    )];
    Ok(finish(Scheme::Semver, raw, Some(v)))
}

pub fn parse_pypi(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let releases = doc
        .get("releases")
        .and_then(Value::as_object)
        .ok_or("no releases")?;
    let raw = releases
        .iter()
        .filter(|(_, files)| {
            files.as_array().is_some_and(|f| {
                !f.is_empty()
                    && f.iter()
                        .any(|x| x.get("yanked").and_then(Value::as_bool) != Some(true))
            })
        })
        .map(|(k, _)| (k.clone(), Meta::default()))
        .collect();
    let hint = plain(doc.get("info").and_then(|i| i.get("version")));
    Ok(finish(Scheme::Pep440, raw, hint))
}

pub fn parse_go(list: &[u8], latest: Option<&[u8]>) -> Result<Versions, String> {
    let text = std::str::from_utf8(list).map_err(|_| "not UTF-8".to_owned())?;
    let raw = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('v'))
        .map(|l| (l.to_owned(), Meta::default()))
        .collect();
    let hint = latest
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|d| plain(d.get("Version")).map(str::to_owned));
    let mut v = finish(Scheme::Semver, raw, hint.as_deref());
    // A module without tags has only a pseudo-version; it is its own latest.
    if v.latest.is_none() {
        v.latest = hint.filter(|h| Version::parse(Scheme::Semver, h).is_some());
    }
    Ok(v)
}

pub fn parse_maven(body: &[u8]) -> Result<Versions, String> {
    let text = std::str::from_utf8(body).map_err(|_| "not UTF-8".to_owned())?;
    let root = xml::parse(text)?;
    let versioning = root.child("versioning").ok_or("no versioning")?;
    let raw = versioning
        .child("versions")
        .map(|vs| {
            vs.named("version")
                .map(|v| (v.text.trim().to_owned(), Meta::default()))
                .collect()
        })
        .unwrap_or_default();
    let hint = versioning
        .text_of("release")
        .or_else(|| versioning.text_of("latest"));
    Ok(finish(Scheme::Maven, raw, hint.as_deref()))
}

pub fn parse_nuget(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let list = doc
        .get("versions")
        .and_then(Value::as_array)
        .ok_or("no versions")?;
    let raw = list
        .iter()
        .filter_map(Value::as_str)
        .map(|v| (v.to_owned(), Meta::default()))
        .collect();
    Ok(finish(Scheme::Semver, raw, None))
}

pub fn parse_packagist(body: &[u8], name: &str) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let list = doc
        .get("packages")
        .and_then(|p| p.get(name))
        .and_then(Value::as_array)
        .ok_or("no versions")?;
    let raw = list
        .iter()
        .filter_map(|v| plain(v.get("version")))
        .map(|v| v.strip_prefix('v').unwrap_or(v))
        .filter(|v| !v.starts_with("dev-") && !v.ends_with("-dev"))
        .map(|v| (v.to_owned(), Meta::default()))
        .collect();
    Ok(finish(Scheme::Composer, raw, None))
}

pub fn parse_rubygems(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let list = doc.as_array().ok_or("no versions")?;
    let raw = list
        .iter()
        .filter(|v| v.get("prerelease").and_then(Value::as_bool) != Some(true))
        .filter(|v| plain(v.get("platform")).is_none_or(|p| p == "ruby"))
        .filter_map(|v| plain(v.get("number")))
        .map(|v| (v.to_owned(), Meta::default()))
        .collect();
    Ok(finish(Scheme::Gem, raw, None))
}

pub fn parse_hex(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let list = doc
        .get("releases")
        .and_then(Value::as_array)
        .ok_or("no releases")?;
    let raw = list
        .iter()
        .filter_map(|v| plain(v.get("version")))
        .map(|v| (v.to_owned(), Meta::default()))
        .collect();
    let hint = plain(doc.get("latest_stable_version"));
    Ok(finish(Scheme::Semver, raw, hint))
}

pub fn parse_pub(body: &[u8]) -> Result<Versions, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let list = doc
        .get("versions")
        .and_then(Value::as_array)
        .ok_or("no versions")?;
    let raw = list
        .iter()
        .filter(|v| v.get("retracted").and_then(Value::as_bool) != Some(true))
        .filter_map(|v| plain(v.get("version")))
        .map(|v| (v.to_owned(), Meta::default()))
        .collect();
    let hint = plain(doc.get("latest").and_then(|l| l.get("version")));
    Ok(finish(Scheme::Semver, raw, hint))
}

/// Go escapes upper-case letters in a module path as `!` and the lower-case letter.
pub fn go_escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for c in segment.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn crates_io_skips_yanked_and_pre_releases_and_keeps_checksums() {
        let body = br#"{"crate":{"max_version":"2.0.0-rc.1"},"versions":[
            {"num":"2.0.0-rc.1","yanked":false,"checksum":"c3"},
            {"num":"1.10.0","yanked":false,"checksum":"c2"},
            {"num":"1.9.0","yanked":true,"checksum":"cy"},
            {"num":"1.2.0","yanked":false,"checksum":"c1"}]}"#;
        let got = parse_crates(body).unwrap();
        assert_eq!(got.versions, v(&["1.2.0", "1.10.0"]));
        assert_eq!(got.latest.as_deref(), Some("1.10.0"));
        assert_eq!(got.meta["1.10.0"].checksum.as_deref(), Some("c2"));
        assert!(!got.meta.contains_key("1.9.0"));
        assert!(parse_crates(b"{}").is_err());
        assert!(parse_crates(b"not json").is_err());
    }

    #[test]
    fn npm_reads_dist_tags_integrity_and_skips_deprecated() {
        let body = br#"{"dist-tags":{"latest":"4.17.21","next":"5.0.0-beta.1"},"versions":{
            "4.17.20":{"dist":{"tarball":"https://r/x-4.17.20.tgz","integrity":"sha512-a"}},
            "4.17.21":{"dist":{"tarball":"https://r/x-4.17.21.tgz","integrity":"sha512-b"}},
            "4.17.19":{"deprecated":"no","dist":{}},
            "5.0.0-beta.1":{"dist":{}}}}"#;
        let got = parse_npm(body).unwrap();
        assert_eq!(got.versions, v(&["4.17.20", "4.17.21"]));
        assert_eq!(got.latest.as_deref(), Some("4.17.21"));
        assert_eq!(got.meta["4.17.21"].integrity.as_deref(), Some("sha512-b"));
        assert_eq!(
            got.meta["4.17.20"].tarball.as_deref(),
            Some("https://r/x-4.17.20.tgz")
        );
        let one = parse_npm_latest(
            br#"{"version":"3.0.0","dist":{"integrity":"sha512-z","tarball":"t"}}"#,
        )
        .unwrap();
        assert_eq!(one.latest.as_deref(), Some("3.0.0"));
        assert_eq!(one.meta["3.0.0"].integrity.as_deref(), Some("sha512-z"));
        assert!(parse_npm_latest(b"{}").is_err());
    }

    #[test]
    fn pypi_needs_a_file_that_is_not_yanked() {
        let body = br#"{"info":{"version":"3.2.1"},"releases":{
            "3.2.0":[{"yanked":false}],
            "3.2.1":[{"yanked":false},{"yanked":true}],
            "3.2.2":[{"yanked":true}],
            "3.3.0":[],
            "4.0a1":[{"yanked":false}]}}"#;
        let got = parse_pypi(body).unwrap();
        assert_eq!(got.versions, v(&["3.2.0", "3.2.1"]));
        assert_eq!(got.latest.as_deref(), Some("3.2.1"));
    }

    #[test]
    fn go_reads_the_list_and_the_latest_even_without_tags() {
        let got = parse_go(
            b"v1.0.0\nv1.1.0\nv2.0.0-rc.1\nnot-a-version\nv1.2.0+incompatible\n",
            Some(br#"{"Version":"v1.1.0","Time":"x"}"#),
        )
        .unwrap();
        assert_eq!(
            got.versions,
            v(&["v1.0.0", "v1.1.0", "v1.2.0+incompatible"])
        );
        assert_eq!(
            got.latest.as_deref(),
            Some("v1.1.0"),
            "the proxy's latest wins"
        );
        let untagged = parse_go(
            b"",
            Some(br#"{"Version":"v0.0.0-20230101000000-abcdef123456"}"#),
        )
        .unwrap();
        assert!(untagged.versions.is_empty());
        assert_eq!(
            untagged.latest.as_deref(),
            Some("v0.0.0-20230101000000-abcdef123456")
        );
        assert_eq!(
            go_escape("github.com/Azure/Go-AutoRest"),
            "github.com/!azure/!go-!auto!rest"
        );
    }

    #[test]
    fn maven_metadata_gives_release_and_versions() {
        let body = br#"<?xml version="1.0"?><metadata><groupId>g</groupId><artifactId>a</artifactId>
            <versioning><latest>2.0-SNAPSHOT</latest><release>1.2.0</release>
            <versions><version>1.0.0</version><version>1.1.0</version><version>1.2.0</version>
            <version>2.0-SNAPSHOT</version><version>2.0-beta-1</version></versions></versioning></metadata>"#;
        let got = parse_maven(body).unwrap();
        assert_eq!(got.versions, v(&["1.0.0", "1.1.0", "1.2.0"]));
        assert_eq!(got.latest.as_deref(), Some("1.2.0"));
        assert!(parse_maven(b"<metadata/>").is_err());
        assert!(parse_maven(b"<metadata>").is_err());
    }

    #[test]
    fn nuget_packagist_rubygems_hex_and_pub() {
        let got = parse_nuget(br#"{"versions":["1.0.0","1.1.0-beta","13.0.1","13.0.3"]}"#).unwrap();
        assert_eq!(got.versions, v(&["1.0.0", "13.0.1", "13.0.3"]));
        let got = parse_packagist(
            br#"{"packages":{"monolog/monolog":[{"version":"3.5.0"},{"version":"v2.9.1"},{"version":"dev-main"},{"version":"2.x-dev"},{"version":"3.0.0-RC1"}]}}"#,
            "monolog/monolog",
        )
        .unwrap();
        assert_eq!(got.versions, v(&["2.9.1", "3.5.0"]));
        assert!(parse_packagist(br#"{"packages":{}}"#, "monolog/monolog").is_err());
        let got = parse_rubygems(
            br#"[{"number":"7.1.0","prerelease":false,"platform":"ruby"},{"number":"7.1.0","prerelease":false,"platform":"java"},{"number":"7.2.0.rc1","prerelease":true,"platform":"ruby"},{"number":"7.0.4","prerelease":false,"platform":"ruby"}]"#,
        )
        .unwrap();
        assert_eq!(got.versions, v(&["7.0.4", "7.1.0"]));
        let got = parse_hex(
            br#"{"latest_stable_version":"1.14.0","releases":[{"version":"1.13.0"},{"version":"1.14.0"},{"version":"2.0.0-rc.0"}]}"#,
        )
        .unwrap();
        assert_eq!(got.versions, v(&["1.13.0", "1.14.0"]));
        assert_eq!(got.latest.as_deref(), Some("1.14.0"));
        let got = parse_pub(
            br#"{"latest":{"version":"0.13.5"},"versions":[{"version":"0.13.4"},{"version":"0.13.5"},{"version":"0.13.6","retracted":true}]}"#,
        )
        .unwrap();
        assert_eq!(got.versions, v(&["0.13.4", "0.13.5"]));
    }

    #[test]
    fn only_the_newest_versions_are_kept() {
        let many: Vec<(String, Meta)> = (0..KEEP + 50)
            .map(|i| (format!("1.{i}.0"), Meta::default()))
            .collect();
        let got = finish(Scheme::Semver, many, None);
        assert_eq!(got.versions.len(), KEEP);
        assert_eq!(got.versions[0], "1.50.0");
        assert_eq!(got.latest, Some(format!("1.{}.0", KEEP + 49)));
    }
}
