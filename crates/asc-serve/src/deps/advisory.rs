//! OSV advisory records: reading them, deciding whether a package version is affected, and the offline
//! advisory database.
//!
//! The record format is the OSV schema (<https://ossf.github.io/osv-schema/>). A version is affected when
//! the record lists it in `versions`, or when it falls inside a `SEMVER` or `ECOSYSTEM` range (events
//! `introduced`, `fixed`, `last_affected`, compared by the ordering of the package's ecosystem). `GIT`
//! ranges name commits and cannot be judged from a version, so they never match. A version that cannot be
//! ordered is treated as possibly affected: an answer that cannot be shown to be clean is not clean.
//!
//! [`Index`] holds records read from a file for a server that is offline or keeps its own advisories: a
//! JSON array, JSON lines, or the zip OSV publishes (one record per entry).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use asc_core::policy::DepSeverity;
use serde::Deserialize;
use serde_json::Value;

use super::cvss;
use super::model::{Advisory, Ecosystem};
use super::version::Version;

/// Most records an index holds.
pub const MAX_RECORDS: usize = 2_000_000;
/// Largest single record.
pub const MAX_RECORD_BYTES: u64 = 8 << 20;
/// Longest summary kept.
const MAX_SUMMARY: usize = 300;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Record {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub severity: Vec<Sev>,
    #[serde(default)]
    pub affected: Vec<Affected>,
    #[serde(default)]
    pub references: Vec<Reference>,
    pub withdrawn: Option<String>,
    pub database_specific: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Sev {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub score: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Pkg {
    #[serde(default)]
    pub ecosystem: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Affected {
    pub package: Option<Pkg>,
    #[serde(default)]
    pub ranges: Vec<Range>,
    #[serde(default)]
    pub versions: Vec<String>,
    pub ecosystem_specific: Option<Value>,
    pub database_specific: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Range {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Event {
    pub introduced: Option<String>,
    pub fixed: Option<String>,
    pub last_affected: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Reference {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub url: String,
}

fn label(v: Option<&Value>) -> Option<DepSeverity> {
    DepSeverity::parse(v?.get("severity")?.as_str()?)
}

impl Affected {
    /// Whether this entry is about the package.
    fn is_for(&self, eco: Ecosystem, canon: &str) -> bool {
        let Some(p) = &self.package else {
            return false;
        };
        // `Debian:11` and `Alpine:v3.18` are other ecosystems.
        p.ecosystem.eq_ignore_ascii_case(eco.osv_name()) && eco.canonical(&p.name) == canon
    }

    /// Whether `version` is inside this entry.
    fn affects(&self, eco: Ecosystem, version: &str) -> bool {
        if self.versions.iter().any(|v| v == version) {
            return true;
        }
        let scheme = eco.scheme();
        let Some(v) = Version::parse(scheme, version) else {
            // Only an entry that could say something about a version can make an unreadable one suspect.
            return self
                .ranges
                .iter()
                .any(|r| matches!(r.kind.as_str(), "SEMVER" | "ECOSYSTEM"));
        };
        if self
            .versions
            .iter()
            .filter_map(|x| Version::parse(scheme, x))
            .any(|x| x == v)
        {
            return true;
        }
        self.ranges
            .iter()
            .filter(|r| matches!(r.kind.as_str(), "SEMVER" | "ECOSYSTEM"))
            .any(|r| in_range(eco, &v, r))
    }

    fn fixed(&self) -> impl Iterator<Item = &str> {
        self.ranges
            .iter()
            .filter(|r| matches!(r.kind.as_str(), "SEMVER" | "ECOSYSTEM"))
            .flat_map(|r| r.events.iter().filter_map(|e| e.fixed.as_deref()))
    }
}

/// The OSV evaluation of one range: events in version order, each one switching the state.
fn in_range(eco: Ecosystem, v: &Version, r: &Range) -> bool {
    let scheme = eco.scheme();
    enum Kind {
        Introduced,
        Fixed,
        LastAffected,
    }
    // `introduced: "0"` is the start of time.
    let mut events: Vec<(Option<Version>, Kind)> = Vec::new();
    for e in &r.events {
        if let Some(s) = &e.introduced {
            if s == "0" {
                events.push((None, Kind::Introduced));
            } else if let Some(x) = Version::parse(scheme, s) {
                events.push((Some(x), Kind::Introduced));
            }
        }
        if let Some(Some(x)) = e.fixed.as_deref().map(|s| Version::parse(scheme, s)) {
            events.push((Some(x), Kind::Fixed));
        }
        if let Some(Some(x)) = e
            .last_affected
            .as_deref()
            .map(|s| Version::parse(scheme, s))
        {
            events.push((Some(x), Kind::LastAffected));
        }
    }
    events.sort_by(|a, b| match (&a.0, &b.0) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });
    let mut affected = false;
    for (at, kind) in &events {
        match kind {
            Kind::Introduced => {
                if at.as_ref().is_none_or(|a| v >= a) {
                    affected = true;
                }
            }
            Kind::Fixed => {
                if at.as_ref().is_some_and(|a| v >= a) {
                    affected = false;
                }
            }
            Kind::LastAffected => {
                if at.as_ref().is_some_and(|a| v > a) {
                    affected = false;
                }
            }
        }
    }
    affected
}

impl Record {
    pub fn is_withdrawn(&self) -> bool {
        self.withdrawn.as_deref().is_some_and(|w| !w.is_empty())
    }

    /// Whether the record covers `version` of the package.
    pub fn affects(&self, eco: Ecosystem, name: &str, version: &str) -> bool {
        if self.is_withdrawn() {
            return false;
        }
        let canon = eco.canonical(name);
        self.affected
            .iter()
            .any(|a| a.is_for(eco, &canon) && a.affects(eco, version))
    }

    /// The advisory for the package, whether or not this version is judged affected here: the caller
    /// already knows the version is (OSV matched it).
    pub fn advisory(&self, eco: Ecosystem, name: &str) -> Advisory {
        let canon = eco.canonical(name);
        let entries: Vec<&Affected> = self
            .affected
            .iter()
            .filter(|a| a.is_for(eco, &canon))
            .collect();
        let mut fixed: Vec<String> = Vec::new();
        for f in entries.iter().flat_map(|a| a.fixed()) {
            if !fixed.iter().any(|x| x == f) {
                fixed.push(f.to_owned());
            }
        }
        let mut score = 0.0f64;
        for s in &self.severity {
            let n = if s.kind.starts_with("CVSS_V3") || s.score.starts_with("CVSS:3") {
                cvss::score(&s.score)
            } else {
                // A bare number is a score, whatever version of CVSS made it.
                s.score
                    .parse::<f64>()
                    .ok()
                    .filter(|n| (0.0..=10.0).contains(n))
            };
            if let Some(n) = n {
                score = score.max(n);
            }
        }
        let severity = label(self.database_specific.as_ref())
            .or_else(|| {
                entries
                    .iter()
                    .find_map(|a| label(a.ecosystem_specific.as_ref()))
            })
            .or_else(|| {
                entries
                    .iter()
                    .find_map(|a| label(a.database_specific.as_ref()))
            })
            .or_else(|| cvss::band(score));
        let summary = if self.summary.trim().is_empty() {
            self.details
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or_default()
                .to_owned()
        } else {
            self.summary.trim().to_owned()
        };
        let summary: String = summary.chars().take(MAX_SUMMARY).collect();
        let url = ["ADVISORY", "WEB"]
            .iter()
            .find_map(|k| {
                self.references
                    .iter()
                    .find(|r| r.kind == *k && r.url.starts_with("https://"))
            })
            .map_or_else(
                || format!("https://osv.dev/vulnerability/{}", self.id),
                |r| r.url.clone(),
            );
        Advisory {
            id: self.id.clone(),
            aliases: self.aliases.clone(),
            summary,
            severity,
            cvss: score,
            fixed,
            url,
        }
    }

    /// The advisory when `version` of the package is affected.
    pub fn advisory_if_affected(
        &self,
        eco: Ecosystem,
        name: &str,
        version: &str,
    ) -> Option<Advisory> {
        self.affects(eco, name, version)
            .then(|| self.advisory(eco, name))
    }
}

/// Records read from a file, by package.
#[derive(Debug, Default)]
pub struct Index {
    by_package: HashMap<(Ecosystem, String), Vec<Arc<Record>>>,
    records: usize,
}

impl Index {
    pub fn len(&self) -> usize {
        self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    fn add(&mut self, r: Record) -> Result<(), String> {
        if self.records >= MAX_RECORDS {
            return Err(format!(
                "the advisory file has more than {MAX_RECORDS} records"
            ));
        }
        if r.id.is_empty() {
            return Err("a record has no id".into());
        }
        self.records += 1;
        let r = Arc::new(r);
        let mut seen: Vec<(Ecosystem, String)> = Vec::new();
        for a in &r.affected {
            let Some(p) = &a.package else { continue };
            let Some(eco) = Ecosystem::ALL
                .into_iter()
                .find(|e| e.osv_name().eq_ignore_ascii_case(&p.ecosystem))
            else {
                continue;
            };
            let key = (eco, eco.canonical(&p.name));
            if !seen.contains(&key) {
                seen.push(key.clone());
                self.by_package.entry(key).or_default().push(r.clone());
            }
        }
        Ok(())
    }

    /// Reads records from `bytes`: a zip of one record per `.json` entry, a JSON array of records, a
    /// single record, or JSON lines. `max_bytes` bounds what is read, after decompression.
    pub fn from_bytes(bytes: &[u8], max_bytes: u64) -> Result<Self, String> {
        let mut idx = Self::default();
        if bytes.starts_with(b"PK\x03\x04") {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .map_err(|e| format!("not a zip: {e}"))?;
            let mut total = 0u64;
            for i in 0..zip.len() {
                let mut f = zip.by_index(i).map_err(|e| format!("zip entry: {e}"))?;
                let name = f
                    .name()
                    .map_err(|e| format!("zip entry: {e}"))?
                    .into_owned();
                if !f.is_file() || !name.ends_with(".json") {
                    continue;
                }
                total = total.saturating_add(f.size());
                if f.size() > MAX_RECORD_BYTES || total > max_bytes {
                    return Err("the advisory zip is larger than its bound".into());
                }
                let mut buf = Vec::new();
                (&mut f)
                    .take(MAX_RECORD_BYTES + 1)
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("zip entry: {e}"))?;
                if buf.len() as u64 > MAX_RECORD_BYTES {
                    return Err("an advisory record is too large".into());
                }
                let r: Record = serde_json::from_slice(&buf)
                    .map_err(|e| format!("{name}: not an OSV record: {e}"))?;
                idx.add(r)?;
            }
            return Ok(idx);
        }
        if bytes.len() as u64 > max_bytes {
            return Err("the advisory file is larger than its bound".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "the advisory file is not UTF-8")?;
        let t = text.trim_start();
        if t.starts_with('[') {
            let all: Vec<Record> =
                serde_json::from_str(t).map_err(|e| format!("not an array of OSV records: {e}"))?;
            for r in all {
                idx.add(r)?;
            }
        } else if let Ok(one) = serde_json::from_str::<Record>(t) {
            idx.add(one)?;
        } else {
            for (n, line) in t.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                if line.len() as u64 > MAX_RECORD_BYTES {
                    return Err(format!("line {}: an advisory record is too large", n + 1));
                }
                let r: Record = serde_json::from_str(line)
                    .map_err(|e| format!("line {}: not an OSV record: {e}", n + 1))?;
                idx.add(r)?;
            }
        }
        Ok(idx)
    }

    /// Reads the file at `path`.
    pub fn load(path: &Path, max_bytes: u64) -> Result<Self, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.len() > max_bytes {
            return Err("the advisory file is larger than its bound".into());
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_bytes(&bytes, max_bytes)
    }

    /// The advisories that apply to `version` of the package, in id order.
    pub fn lookup(&self, eco: Ecosystem, name: &str, version: &str) -> Vec<Advisory> {
        let mut out: Vec<Advisory> = self
            .by_package
            .get(&(eco, eco.canonical(name)))
            .into_iter()
            .flatten()
            .filter_map(|r| r.advisory_if_affected(eco, name, version))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn record(v: Value) -> Record {
        serde_json::from_value(v).unwrap()
    }

    fn ghsa() -> Record {
        record(json!({
            "id": "GHSA-aaaa-bbbb-cccc",
            "aliases": ["CVE-2021-0001"],
            "summary": "Prototype pollution in lodash",
            "details": "Longer text.",
            "severity": [{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}],
            "affected": [{
                "package": {"name": "lodash", "ecosystem": "npm"},
                "ranges": [{"type": "SEMVER", "events": [
                    {"introduced": "0"}, {"fixed": "4.17.12"},
                    {"introduced": "5.0.0"}, {"fixed": "5.0.3"}
                ]}],
                "versions": ["4.17.11"]
            }, {
                "package": {"name": "lodash", "ecosystem": "Debian:11"},
                "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}]}]
            }],
            "references": [
                {"type": "WEB", "url": "http://insecure.example/x"},
                {"type": "ADVISORY", "url": "https://github.com/advisories/GHSA-aaaa-bbbb-cccc"}
            ],
            "database_specific": {"severity": "MODERATE"}
        }))
    }

    #[test]
    fn ranges_are_evaluated_in_version_order() {
        let r = ghsa();
        let hit = |v: &str| r.affects(Ecosystem::Npm, "lodash", v);
        assert!(hit("4.17.11"));
        assert!(hit("0.1.0"));
        assert!(hit("4.17.12-rc.1"), "a pre-release of the fix is before it");
        assert!(!hit("4.17.12"));
        assert!(!hit("4.17.20"));
        assert!(!hit("4.99.0"));
        assert!(hit("5.0.0"));
        assert!(hit("5.0.2"));
        assert!(!hit("5.0.3"));
        assert!(!r.affects(Ecosystem::Npm, "left-pad", "1.0.0"));
        assert!(!r.affects(Ecosystem::PyPi, "lodash", "1.0.0"));
    }

    #[test]
    fn last_affected_and_a_withdrawn_record() {
        let r = record(json!({
            "id": "RUSTSEC-2020-0001",
            "affected": [{"package": {"name": "Foo_Bar", "ecosystem": "crates.io"},
                "ranges": [{"type": "SEMVER", "events": [{"introduced": "1.0.0"}, {"last_affected": "1.2.0"}]}]}]
        }));
        assert!(r.affects(Ecosystem::CratesIo, "foo-bar", "1.0.0"));
        assert!(r.affects(Ecosystem::CratesIo, "foo-bar", "1.2.0"));
        assert!(!r.affects(Ecosystem::CratesIo, "foo-bar", "1.2.1"));
        assert!(!r.affects(Ecosystem::CratesIo, "foo-bar", "0.9.0"));
        let mut w = r.clone();
        w.withdrawn = Some("2021-01-01T00:00:00Z".into());
        assert!(!w.affects(Ecosystem::CratesIo, "foo-bar", "1.0.0"));
    }

    #[test]
    fn git_ranges_never_match_and_an_unreadable_version_is_suspect() {
        let git = record(json!({
            "id": "X-1",
            "affected": [{"package": {"name": "p", "ecosystem": "npm"},
                "ranges": [{"type": "GIT", "events": [{"introduced": "0"}, {"fixed": "abcdef"}]}]}]
        }));
        assert!(!git.affects(Ecosystem::Npm, "p", "1.0.0"));
        assert!(!git.affects(Ecosystem::Npm, "p", "not-a-version"));
        let semver = record(json!({
            "id": "X-2",
            "affected": [{"package": {"name": "p", "ecosystem": "npm"},
                "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": "2.0.0"}]}]}]
        }));
        assert!(
            semver.affects(Ecosystem::Npm, "p", "latest"),
            "cannot be shown clean"
        );
    }

    #[test]
    fn the_advisory_has_a_label_a_score_fixes_and_a_safe_link() {
        let a = ghsa().advisory(Ecosystem::Npm, "lodash");
        assert_eq!(a.id, "GHSA-aaaa-bbbb-cccc");
        assert_eq!(
            a.severity,
            Some(DepSeverity::Moderate),
            "the label wins over the vector"
        );
        assert_eq!(a.cvss, 9.8);
        assert_eq!(a.fixed, ["4.17.12", "5.0.3"]);
        assert_eq!(a.url, "https://github.com/advisories/GHSA-aaaa-bbbb-cccc");
        assert_eq!(
            a.fixed_after(Ecosystem::Npm.scheme(), "4.17.11").as_deref(),
            Some("4.17.12")
        );
        assert_eq!(
            a.fixed_after(Ecosystem::Npm.scheme(), "5.0.0").as_deref(),
            Some("5.0.3")
        );
        assert_eq!(a.fixed_after(Ecosystem::Npm.scheme(), "9.0.0"), None);
    }

    #[test]
    fn severity_falls_back_to_the_vector_then_to_nothing() {
        let mut r = ghsa();
        r.database_specific = None;
        assert_eq!(
            r.advisory(Ecosystem::Npm, "lodash").severity,
            Some(DepSeverity::Critical)
        );
        r.severity = vec![Sev {
            kind: "CVSS_V4".into(),
            score: "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N".into(),
        }];
        assert_eq!(r.advisory(Ecosystem::Npm, "lodash").severity, None);
        r.severity = vec![Sev {
            kind: "CVSS_V4".into(),
            score: "7.5".into(),
        }];
        assert_eq!(
            r.advisory(Ecosystem::Npm, "lodash").severity,
            Some(DepSeverity::High)
        );
        let eco = record(json!({
            "id": "Y-1", "summary": "",
            "details": "\n\n  First line.\nSecond.",
            "affected": [{"package": {"name": "p", "ecosystem": "npm"},
                "ecosystem_specific": {"severity": "critical"}}]
        }));
        let a = eco.advisory(Ecosystem::Npm, "p");
        assert_eq!(a.severity, Some(DepSeverity::Critical));
        assert_eq!(a.summary, "First line.");
        assert_eq!(a.url, "https://osv.dev/vulnerability/Y-1");
    }

    fn two() -> Vec<Value> {
        vec![
            json!({
                "id": "B-2", "affected": [{"package": {"name": "a", "ecosystem": "PyPI"},
                "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}, {"fixed": "2.0"}]}]}]
            }),
            json!({
                "id": "A-1", "affected": [{"package": {"name": "A", "ecosystem": "PyPI"},
                "versions": ["1.0"]}]
            }),
        ]
    }

    #[test]
    fn an_index_reads_arrays_lines_single_records_and_zips() {
        let arr = serde_json::to_vec(&two()).unwrap();
        let lines = two()
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let single = two()[0].to_string();
        let mut zip_bytes = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let o = zip::write::SimpleFileOptions::default();
            for (i, v) in two().iter().enumerate() {
                z.start_file(format!("{i}.json"), o).unwrap();
                z.write_all(v.to_string().as_bytes()).unwrap();
            }
            z.start_file("README.txt", o).unwrap();
            z.write_all(b"ignored").unwrap();
            z.finish().unwrap();
        }
        for (what, bytes, n) in [
            ("array", arr, 2),
            ("lines", lines.into_bytes(), 2),
            ("single", single.into_bytes(), 1),
            ("zip", zip_bytes, 2),
        ] {
            let idx = Index::from_bytes(&bytes, 1 << 20).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(idx.len(), n, "{what}");
            let hits = idx.lookup(Ecosystem::PyPi, "a", "1.0");
            let ids: Vec<_> = hits.iter().map(|a| a.id.as_str()).collect();
            assert_eq!(
                ids,
                if n == 2 {
                    vec!["A-1", "B-2"]
                } else {
                    vec!["B-2"]
                },
                "{what}"
            );
            assert!(idx.lookup(Ecosystem::PyPi, "a", "2.0").is_empty(), "{what}");
            assert!(idx.lookup(Ecosystem::Npm, "a", "1.0").is_empty());
        }
    }

    #[test]
    fn an_index_refuses_what_it_cannot_read_whole() {
        assert!(Index::from_bytes(b"[{\"id\": \"x\"}", 1 << 20).is_err());
        assert!(Index::from_bytes(b"{\"id\": \"\"}", 1 << 20).is_err());
        assert!(Index::from_bytes(b"{\"id\": \"a\"}\nnot json\n", 1 << 20).is_err());
        assert!(Index::from_bytes(b"\xff\xfe", 1 << 20).is_err());
        assert!(
            Index::from_bytes(&[b' '; 100], 10).is_err(),
            "over its bound"
        );
        assert!(Index::from_bytes(b"PK\x03\x04garbage", 1 << 20).is_err());
        let mut z = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut z));
            w.start_file("big.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(&vec![b' '; 5000]).unwrap();
            w.finish().unwrap();
        }
        assert!(
            Index::from_bytes(&z, 1000).is_err(),
            "a zip counts its inflated size"
        );
        let e = Index::load(Path::new("/nonexistent/advisories.json"), 1 << 20).unwrap_err();
        assert!(e.contains("advisories.json"));
    }
}
