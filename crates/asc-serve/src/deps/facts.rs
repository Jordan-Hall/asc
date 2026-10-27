//! What registries say about one version of one package, in the shape the lockfiles need.
//!
//! A lockfile entry records more than a version: the hash of what was downloaded, the files of a release,
//! the dependencies the package declared. [`Facts`] holds those for one `(ecosystem, name, version)`,
//! read from the registry's answer by the parsers here. The parsers check what they read (a digest has
//! the length of its algorithm, a second path of the same registry agrees with the first) and refuse
//! anything else, because a digest that was not verified is not written into a lockfile.
//!
//! Everything in this file is pure: the bytes of an answer in, [`Facts`] or a reason out. The calls
//! themselves are in [`super::latest`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::model::Ecosystem;
use super::ordered::{Ordered, parse_ordered};

/// One file of a PyPI release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PyFile {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// `2022-07-13T14:00:09.552Z`, empty when the registry did not say.
    pub upload_time: String,
    pub sdist: bool,
}

/// The `h1:` digests `go.sum` holds for a module version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoSums {
    pub zip: String,
    pub module: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// SHA-256 as 64 hex digits: the crate (`Cargo.lock`), the gem (`Gemfile.lock` checksums).
    pub sha256: Option<String>,
    /// `sha512-<base64>`: the package tarball (`package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`).
    pub integrity: Option<String>,
    /// SHA-1 as 40 hex digits (the fragment of a yarn v1 `resolved`).
    pub shasum: Option<String>,
    pub tarball: Option<String>,
    /// The files of a PyPI release, ordered by file name.
    pub files: Vec<PyFile>,
    pub go: Option<GoSums>,
    /// The whole Composer package entry of the version.
    pub entry: Option<Ordered>,
    /// What the version depends on, as `kind:name` (with a marker or target where there is one) to the
    /// requirement. Two versions with the same map and the same [`Facts::traits`] lock the same way.
    pub deps: BTreeMap<String, String>,
    /// The other fields a lock entry carries and an install depends on (engines, platforms, binaries).
    pub traits: BTreeMap<String, String>,
    /// The independent checks the digests passed, for the proposal to report.
    pub verified: Vec<String>,
}

impl Facts {
    /// The differences between what `self` (the old version) and `other` (the new) declare, one line each.
    /// Empty: the lock entries differ only by version and digest. `traits` says whether the install
    /// properties (engines, platforms, binaries) count, which depends on what the lockfile records.
    pub fn differences(&self, other: &Self, traits: bool) -> Vec<String> {
        let mut out = Vec::new();
        diff_maps("dependency", &self.deps, &other.deps, &mut out);
        if traits {
            diff_maps("property", &self.traits, &other.traits, &mut out);
        }
        out
    }
}

fn diff_maps(
    what: &str,
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    out: &mut Vec<String>,
) {
    for (k, v) in old {
        match new.get(k) {
            None => out.push(format!("{what} {k} ({v}) is gone")),
            Some(n) if n != v => out.push(format!("{what} {k} changes from {v} to {n}")),
            _ => {}
        }
    }
    for (k, v) in new {
        if !old.contains_key(k) {
            out.push(format!("{what} {k} ({v}) is new"));
        }
    }
}

/// A fact to look up: `(ecosystem, canonical name, version)`.
pub type FactKey = (Ecosystem, String, String);

pub fn key(eco: Ecosystem, name: &str, version: &str) -> FactKey {
    (eco, eco.canonical(name), version.to_owned())
}

/// A batch of facts: the answers, or the reasons they could not be had.
pub type FactStore = HashMap<FactKey, Result<Arc<Facts>, String>>;

// ------------------------------------------------------------------------------ helpers

pub fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// A Subresource Integrity value of one algorithm: `sha512-<base64>` whose digest has the length the
/// algorithm gives. Returns the algorithm name and the digest bytes.
pub fn sri(value: &str) -> Option<(&'static str, Vec<u8>)> {
    for (prefix, name, len) in [
        ("sha512-", "sha512", 64),
        ("sha384-", "sha384", 48),
        ("sha256-", "sha256", 32),
        ("sha1-", "sha1", 20),
    ] {
        if let Some(b) = value.strip_prefix(prefix) {
            let bytes = B64.decode(b).ok()?;
            return (bytes.len() == len).then_some((name, bytes));
        }
    }
    None
}

/// `Value` with its object keys in order, as one string.
pub fn canon(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canon(&m[k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canon).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

fn json(body: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(body).map_err(|_| "the answer is not JSON".to_owned())
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

// ------------------------------------------------------------------------------ npm

/// Whether the integrity a lockfile holds for the version being replaced is what the registry publishes
/// for it. Values of another algorithm than the registry's strongest are compared by SHA-1 where the
/// registry gives one, and are otherwise not comparable and pass.
pub fn integrity_consistent(old: &str, registry: Option<&str>, shasum: Option<&str>) -> bool {
    let Some((alg, bytes)) = sri(old.split_whitespace().next().unwrap_or("")) else {
        return false;
    };
    if let Some((ralg, rbytes)) = registry.and_then(sri)
        && ralg == alg
    {
        return rbytes == bytes;
    }
    if alg == "sha1"
        && let Some(sum) = shasum.and_then(hex_decode)
    {
        return sum == bytes;
    }
    true
}

/// The tarball URL a lockfile should hold for the new version: the old `resolved` (its `#fragment`
/// aside) is the registry's own URL for the old version, possibly on another host name of the same
/// registry, and the new one keeps that host.
pub fn rebase_tarball(old: &str, registry_old: &str, registry_new: &str) -> Option<String> {
    let base = old.split('#').next().unwrap_or(old);
    if base == registry_old {
        return Some(registry_new.to_owned());
    }
    let (o, ro, rn) = (
        url::Url::parse(base).ok()?,
        url::Url::parse(registry_old).ok()?,
        url::Url::parse(registry_new).ok()?,
    );
    if o.path() != ro.path() || o.scheme() != "https" {
        return None;
    }
    let mut out = o;
    out.set_path(rn.path());
    out.set_query(None);
    Some(out.to_string())
}

/// The answer of `GET /<name>/<version>`: the manifest of that version with its `dist`.
pub fn parse_npm_version(body: &[u8], name: &str, version: &str) -> Result<Facts, String> {
    let doc = json(body)?;
    if str_of(&doc, "name") != Some(name) || str_of(&doc, "version") != Some(version) {
        return Err("the registry answered for another package or version".into());
    }
    let dist = doc.get("dist").ok_or("the version has no dist")?;
    let tarball = str_of(dist, "tarball").ok_or("the version has no tarball")?;
    let shasum = str_of(dist, "shasum").map(str::to_owned);
    if let Some(s) = &shasum
        && !is_hex(s, 40)
    {
        return Err("the registry's shasum is not 40 hex digits".into());
    }
    let integrity = match str_of(dist, "integrity") {
        Some(i) => {
            // Several hashes may be listed; the strongest valid one is the one a lockfile holds.
            let best = i
                .split_whitespace()
                .filter_map(|h| sri(h).map(|(alg, bytes)| (alg, h, bytes)))
                .max_by_key(|(alg, _, _)| match *alg {
                    "sha512" => 4,
                    "sha384" => 3,
                    "sha256" => 2,
                    _ => 1,
                })
                .ok_or("the registry's integrity is not shaNNN-base64 of the right length")?;
            if best.0 == "sha1"
                && let Some(s) = &shasum
                && hex_decode(s).as_deref() != Some(best.2.as_slice())
            {
                return Err("the registry's integrity and shasum disagree".into());
            }
            best.1.to_owned()
        }
        // Old packages carry the SHA-1 only; a lockfile records it as an integrity too.
        None => {
            let s = shasum
                .as_ref()
                .ok_or("the version has no integrity or shasum")?;
            format!(
                "sha1-{}",
                B64.encode(hex_decode(s).ok_or("shasum is not hex")?)
            )
        }
    };
    let mut deps = BTreeMap::new();
    for section in ["dependencies", "optionalDependencies", "peerDependencies"] {
        if let Some(m) = doc.get(section).and_then(Value::as_object) {
            for (k, v) in m {
                deps.insert(
                    format!("{section}:{k}"),
                    v.as_str()
                        .ok_or("a requirement is not a string")?
                        .to_owned(),
                );
            }
        }
    }
    if let Some(m) = doc.get("peerDependenciesMeta").and_then(Value::as_object) {
        for (k, v) in m {
            deps.insert(format!("peerDependenciesMeta:{k}"), canon(v));
        }
    }
    let mut traits = BTreeMap::new();
    for key in ["engines", "os", "cpu", "bin", "hasInstallScript"] {
        if let Some(v) = doc.get(key) {
            traits.insert(key.to_owned(), canon(v));
        }
    }
    // A package that ships its own lock resolves its tree differently.
    if doc.get("_hasShrinkwrap").and_then(Value::as_bool) == Some(true) {
        traits.insert("hasShrinkwrap".into(), "true".into());
    }
    Ok(Facts {
        integrity: Some(integrity),
        shasum,
        tarball: Some(tarball.to_owned()),
        deps,
        traits,
        ..Facts::default()
    })
}

/// The signatures of a version document: `(keyid, signature)` pairs.
pub fn npm_signatures(body: &[u8]) -> Vec<(String, String)> {
    let Ok(doc) = json(body) else {
        return Vec::new();
    };
    doc.get("dist")
        .and_then(|d| d.get("signatures"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    Some((str_of(s, "keyid")?.to_owned(), str_of(s, "sig")?.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The registry's signing keys: `(keyid, base64 SPKI)` pairs for ECDSA P-256.
pub fn npm_keys(body: &[u8]) -> Vec<(String, String)> {
    let Ok(doc) = json(body) else {
        return Vec::new();
    };
    doc.get("keys")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|k| str_of(k, "keytype") == Some("ecdsa-sha2-nistp256"))
                .filter_map(|k| {
                    Some((str_of(k, "keyid")?.to_owned(), str_of(k, "key")?.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The fixed prefix of the DER encoding of an uncompressed P-256 public key (SubjectPublicKeyInfo).
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// Checks the registry signature over `name@version:integrity`. `Ok(true)` verified, `Ok(false)` the
/// document carries no signature by a key the registry publishes, `Err` a signature that does not verify.
pub fn verify_npm_signature(
    name: &str,
    version: &str,
    integrity: &str,
    signatures: &[(String, String)],
    keys: &[(String, String)],
) -> Result<bool, String> {
    let message = format!("{name}@{version}:{integrity}");
    let mut seen = false;
    for (keyid, sig) in signatures {
        let Some((_, key)) = keys.iter().find(|(id, _)| id == keyid) else {
            continue;
        };
        seen = true;
        let spki = B64
            .decode(key)
            .map_err(|_| "a registry key is not base64".to_owned())?;
        let point = spki
            .strip_prefix(&P256_SPKI_PREFIX[..])
            .ok_or("a registry key is not an uncompressed P-256 key")?;
        let sig = B64
            .decode(sig)
            .map_err(|_| "a signature is not base64".to_owned())?;
        let pk = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_ASN1,
            point,
        );
        if pk.verify(message.as_bytes(), &sig).is_ok() {
            return Ok(true);
        }
    }
    if seen {
        Err("the registry's signature of the package does not verify".into())
    } else {
        Ok(false)
    }
}

// ------------------------------------------------------------------------------ crates.io

/// The answer of `GET /api/v1/crates/<name>/<version>`: the checksum.
pub fn parse_crates_version(body: &[u8], version: &str) -> Result<String, String> {
    let doc = json(body)?;
    let v = doc.get("version").ok_or("no version")?;
    if str_of(v, "num") != Some(version) {
        return Err("the registry answered for another version".into());
    }
    if v.get("yanked").and_then(Value::as_bool) == Some(true) {
        return Err("the version is yanked".into());
    }
    let c = str_of(v, "checksum").ok_or("the version has no checksum")?;
    if !is_hex(c, 64) {
        return Err("the registry's checksum is not 64 hex digits".into());
    }
    Ok(c.to_owned())
}

/// The sparse index path of a crate. The name is checked against crates.io's grammar first, and the
/// slices are taken with `get`, so no name can split a character.
pub fn crates_index_path(name: &str) -> Result<Vec<String>, String> {
    super::names::check(Ecosystem::CratesIo, name)?;
    let n = name.to_ascii_lowercase();
    let cut = |a: usize, b: usize| {
        n.get(a..b)
            .map(str::to_owned)
            .ok_or_else(|| "the crate name is not usable".to_owned())
    };
    Ok(match n.len() {
        1 => vec!["1".into(), n],
        2 => vec!["2".into(), n],
        3 => vec!["3".into(), cut(0, 1)?, n],
        _ => vec![cut(0, 2)?, cut(2, 4)?, n],
    })
}

/// The sparse index file of a crate (one JSON object per version, per line): checksum and dependencies
/// of `version`.
pub fn parse_crates_index(body: &[u8], name: &str, version: &str) -> Result<Facts, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the index is not UTF-8".to_owned())?;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value =
            serde_json::from_str(line).map_err(|_| "an index line is not JSON".to_owned())?;
        if str_of(&v, "vers") != Some(version) {
            continue;
        }
        if !str_of(&v, "name").is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Err("the index answered for another crate".into());
        }
        if v.get("yanked").and_then(Value::as_bool) == Some(true) {
            return Err("the version is yanked".into());
        }
        let c = str_of(&v, "cksum").ok_or("the index has no checksum")?;
        if !is_hex(c, 64) {
            return Err("the index's checksum is not 64 hex digits".into());
        }
        let mut deps = BTreeMap::new();
        for d in v
            .get("deps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let kind = str_of(d, "kind").unwrap_or("normal");
            // A renamed dependency is named by its `package`.
            let dep = str_of(d, "package")
                .or_else(|| str_of(d, "name"))
                .ok_or("a dependency has no name")?;
            let target = str_of(d, "target").unwrap_or("");
            let mut feats: Vec<&str> = d
                .get("features")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            feats.sort_unstable();
            deps.insert(
                format!("{kind}:{dep}:{target}"),
                format!(
                    "{}|{}|{}|{}",
                    str_of(d, "req").unwrap_or("*"),
                    feats.join(","),
                    d.get("optional").and_then(Value::as_bool).unwrap_or(false),
                    d.get("default_features")
                        .and_then(Value::as_bool)
                        .unwrap_or(true)
                ),
            );
        }
        let mut traits = BTreeMap::new();
        if let Some(f) = v.get("features") {
            traits.insert("features".into(), canon(f));
        }
        if let Some(r) = str_of(&v, "rust_version") {
            traits.insert("rust_version".into(), r.to_owned());
        }
        return Ok(Facts {
            sha256: Some(c.to_owned()),
            deps,
            traits,
            ..Facts::default()
        });
    }
    Err("the index does not list the version".into())
}

/// The requirement a crate's dependency map holds for `dep` (any kind that is not a dev dependency).
pub fn crate_requirements(deps: &BTreeMap<String, String>, dep: &str) -> Vec<String> {
    let want = dep.to_ascii_lowercase().replace('_', "-");
    deps.iter()
        .filter(|(k, _)| !k.starts_with("dev:"))
        .filter(|(k, _)| {
            k.split(':')
                .nth(1)
                .is_some_and(|n| n.to_ascii_lowercase().replace('_', "-") == want)
        })
        .map(|(_, v)| v.split('|').next().unwrap_or("*").to_owned())
        .collect()
}

// ------------------------------------------------------------------------------ PyPI

/// The answer of `GET /pypi/<name>/<version>/json`.
pub fn parse_pypi_release(body: &[u8], version: &str) -> Result<Facts, String> {
    let doc = json(body)?;
    let info = doc.get("info").ok_or("no info")?;
    if str_of(info, "version") != Some(version) {
        return Err("the registry answered for another version".into());
    }
    let mut files = Vec::new();
    for u in doc
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if u.get("yanked").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let sha = u
            .get("digests")
            .and_then(|d| str_of(d, "sha256"))
            .ok_or("a file has no sha256")?;
        if !is_hex(sha, 64) {
            return Err("a file's sha256 is not 64 hex digits".into());
        }
        files.push(PyFile {
            filename: str_of(u, "filename")
                .ok_or("a file has no name")?
                .to_owned(),
            url: str_of(u, "url").ok_or("a file has no URL")?.to_owned(),
            sha256: sha.to_owned(),
            size: u
                .get("size")
                .and_then(Value::as_u64)
                .ok_or("a file has no size")?,
            upload_time: str_of(u, "upload_time_iso_8601").unwrap_or("").to_owned(),
            sdist: str_of(u, "packagetype") == Some("sdist"),
        });
    }
    if files.is_empty() {
        return Err("the release has no files".into());
    }
    files.sort_by(|a, b| a.filename.cmp(&b.filename));
    let mut deps = BTreeMap::new();
    for r in info
        .get("requires_dist")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let r = r.as_str().ok_or("a requirement is not a string")?;
        deps.insert(
            r.split_whitespace().collect::<Vec<_>>().join(" "),
            String::new(),
        );
    }
    let mut traits = BTreeMap::new();
    if let Some(p) = str_of(info, "requires_python") {
        traits.insert("requires_python".into(), p.to_owned());
    }
    Ok(Facts {
        files,
        deps,
        traits,
        ..Facts::default()
    })
}

/// The answer of `GET /simple/<name>/` (PEP 691): file name to sha256.
pub fn parse_pypi_simple(body: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let doc = json(body)?;
    let mut out = BTreeMap::new();
    for f in doc
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(sha)) = (
            str_of(f, "filename"),
            f.get("hashes").and_then(|h| str_of(h, "sha256")),
        ) else {
            continue;
        };
        out.insert(name.to_owned(), sha.to_owned());
    }
    if out.is_empty() {
        return Err("the simple index lists no files".into());
    }
    Ok(out)
}

/// Every file of the release must be in the simple index with the same digest.
pub fn cross_check_pypi(files: &[PyFile], simple: &BTreeMap<String, String>) -> Result<(), String> {
    for f in files {
        match simple.get(&f.filename) {
            Some(s) if *s == f.sha256 => {}
            Some(_) => {
                return Err(format!(
                    "the two PyPI endpoints disagree on the digest of {}",
                    f.filename
                ));
            }
            None => return Err(format!("{} is not in the simple index", f.filename)),
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ RubyGems

/// One version of a gem in the compact index.
pub fn parse_compact_index(body: &[u8], version: &str) -> Result<Facts, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the index is not UTF-8".to_owned())?;
    for line in text.lines() {
        let Some((ver, rest)) = line.split_once(' ') else {
            continue;
        };
        if ver != version {
            continue;
        }
        let (deps_part, meta) = rest
            .split_once('|')
            .ok_or("the index line has no metadata")?;
        let mut deps = BTreeMap::new();
        for d in deps_part.split(',').filter(|d| !d.is_empty()) {
            let (name, reqs) = d.split_once(':').ok_or("a dependency has no requirement")?;
            deps.insert(format!("runtime:{name}"), reqs.replace('&', ", "));
        }
        let mut checksum = None;
        let mut traits = BTreeMap::new();
        for kv in meta.split(',') {
            let Some((k, v)) = kv.split_once(':') else {
                continue;
            };
            match k {
                "checksum" => checksum = Some(v.to_owned()),
                "ruby" | "rubygems" => {
                    traits.insert(k.to_owned(), v.replace('&', ", "));
                }
                _ => {}
            }
        }
        let c = checksum.ok_or("the index has no checksum for the version")?;
        if !is_hex(&c, 64) {
            return Err("the index's checksum is not 64 hex digits".into());
        }
        return Ok(Facts {
            sha256: Some(c),
            deps,
            traits,
            ..Facts::default()
        });
    }
    Err("the index does not list the version".into())
}

/// The answer of `GET /api/v2/rubygems/<name>/versions/<version>.json`: the checksum.
pub fn parse_rubygems_version(body: &[u8], version: &str) -> Result<String, String> {
    let doc = json(body)?;
    if str_of(&doc, "version") != Some(version) {
        return Err("the registry answered for another version".into());
    }
    if let Some(p) = str_of(&doc, "platform")
        && p != "ruby"
    {
        return Err("the version is a platform build".into());
    }
    let sha = str_of(&doc, "sha").ok_or("the version has no checksum")?;
    if !is_hex(sha, 64) {
        return Err("the registry's checksum is not 64 hex digits".into());
    }
    Ok(sha.to_owned())
}

// ------------------------------------------------------------------------------ Packagist

/// Expands a Composer v2 metadata list (each entry holds only what changed from the one before; the
/// string `__unset` removes a key) and returns the entry of `version`, with its keys in the order the
/// registry wrote them.
pub fn parse_packagist_version(body: &[u8], package: &str, version: &str) -> Result<Facts, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the answer is not UTF-8".to_owned())?;
    let doc = parse_ordered(text).map_err(|e| format!("the answer is not usable ({e})"))?;
    let Some(Ordered::Arr(list)) = doc.get("packages").and_then(|p| p.get(package)) else {
        return Err("no versions".into());
    };
    let minified = doc.get("minified").is_some();
    let mut cur: Vec<(String, Ordered)> = Vec::new();
    let want = version.strip_prefix('v').unwrap_or(version);
    let mut found: Option<Vec<(String, Ordered)>> = None;
    for e in list {
        let Ordered::Obj(m) = e else {
            return Err("an entry is not an object".into());
        };
        if minified {
            for (k, v) in m {
                if v.as_str() == Some("__unset") {
                    cur.retain(|(x, _)| x != k);
                } else if let Some(slot) = cur.iter_mut().find(|(x, _)| x == k) {
                    slot.1 = v.clone();
                } else {
                    cur.push((k.clone(), v.clone()));
                }
            }
        } else {
            cur.clone_from(m);
        }
        let v = cur
            .iter()
            .find(|(k, _)| k == "version")
            .and_then(|(_, v)| v.as_str())
            .unwrap_or("");
        if v.strip_prefix('v').unwrap_or(v) == want {
            found = Some(cur.clone());
            break;
        }
    }
    let entry = Ordered::Obj(found.ok_or("the registry does not list the version")?);
    let name = entry.get("name").and_then(Ordered::as_str).unwrap_or("");
    if !name.eq_ignore_ascii_case(package) {
        return Err("the registry answered for another package".into());
    }
    let dist = entry.get("dist").ok_or("the version has no dist")?;
    let dist_url = dist
        .get("url")
        .and_then(Ordered::as_str)
        .ok_or("the dist has no URL")?
        .to_owned();
    if dist.get("type").and_then(Ordered::as_str) != Some("zip") {
        return Err("the dist is not a zip".into());
    }
    let shasum = dist
        .get("shasum")
        .and_then(Ordered::as_str)
        .unwrap_or("")
        .to_owned();
    if !(shasum.is_empty() || is_hex(&shasum, 40)) {
        return Err("the dist's shasum is not 40 hex digits".into());
    }
    let mut deps = BTreeMap::new();
    for section in ["require", "require-dev", "conflict", "replace", "provide"] {
        if let Some(Ordered::Obj(m)) = entry.get(section) {
            for (k, v) in m {
                deps.insert(
                    format!("{section}:{k}"),
                    v.as_str().unwrap_or("").to_owned(),
                );
            }
        }
    }
    let mut traits = BTreeMap::new();
    for key in [
        "autoload",
        "autoload-dev",
        "bin",
        "type",
        "extra",
        "include-path",
        "php-ext",
    ] {
        if let Some(v) = entry.get(key) {
            traits.insert(key.to_owned(), v.canon());
        }
    }
    // Lock entries are of the package, not of how a registry shipped it.
    let Ordered::Obj(mut full) = entry else {
        return Err("an entry is not an object".into());
    };
    full.retain(|(k, _)| k != "uid" && k != "version_normalized");
    Ok(Facts {
        shasum: (!shasum.is_empty()).then_some(shasum),
        tarball: Some(dist_url),
        entry: Some(Ordered::Obj(full)),
        deps,
        traits,
        ..Facts::default()
    })
}

// ------------------------------------------------------------------------------ Go

/// The answer of `GET <sumdb>/lookup/<module>@<version>`: the two `h1:` digests of the record.
pub fn parse_sumdb(body: &[u8], module: &str, version: &str) -> Result<GoSums, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the answer is not UTF-8".to_owned())?;
    let mut zip = None;
    let mut module_sum = None;
    for line in text.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() != 3 || f[0] != module {
            continue;
        }
        if !f[2].starts_with("h1:") || sri_h1(f[2]).is_none() {
            return Err("a digest of the checksum database is not h1:<base64 of 32 bytes>".into());
        }
        if f[1] == version {
            zip = Some(f[2].to_owned());
        } else if f[1].strip_suffix("/go.mod") == Some(version) {
            module_sum = Some(f[2].to_owned());
        }
    }
    Ok(GoSums {
        zip: zip.ok_or("the checksum database has no record of the module zip")?,
        module: module_sum.ok_or("the checksum database has no record of the go.mod")?,
    })
}

fn sri_h1(h: &str) -> Option<Vec<u8>> {
    let b = B64.decode(h.strip_prefix("h1:")?).ok()?;
    (b.len() == 32).then_some(b)
}

fn h1_of_lines(lines: &[(String, [u8; 32])]) -> String {
    let mut h = Sha256::new();
    for (name, sum) in lines {
        let mut hex = String::with_capacity(64);
        for b in sum {
            hex.push_str(&format!("{b:02x}"));
        }
        h.update(format!("{hex}  {name}\n").as_bytes());
    }
    format!("h1:{}", B64.encode(h.finalize()))
}

/// The `h1:` digest of a `go.mod` file.
pub fn go_mod_h1(content: &[u8]) -> String {
    let sum: [u8; 32] = Sha256::digest(content).into();
    h1_of_lines(&[("go.mod".to_owned(), sum)])
}

/// The `h1:` digest of a module zip (`dirhash.Hash1` over its files). Every file has to live under
/// `<module>@<version>/`; the zip is read within `max_files` and `max_unpacked` bounds.
pub fn go_zip_h1(
    zip_bytes: &[u8],
    module: &str,
    version: &str,
    max_files: usize,
    max_unpacked: u64,
) -> Result<String, String> {
    use std::io::Read as _;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|_| "the module zip cannot be read".to_owned())?;
    if z.len() > max_files {
        return Err("the module zip has too many files".into());
    }
    let prefix = format!("{module}@{version}/");
    let mut lines: Vec<(String, [u8; 32])> = Vec::new();
    let mut total = 0u64;
    for i in 0..z.len() {
        let mut f = z
            .by_index(i)
            .map_err(|_| "the module zip cannot be read".to_owned())?;
        let name = f
            .name()
            .map_err(|_| "a file name in the module zip is not valid".to_owned())?
            .into_owned();
        if name.ends_with('/') {
            continue;
        }
        if !name.starts_with(&prefix) {
            return Err("the module zip holds a file outside the module".into());
        }
        total = total.saturating_add(f.size());
        if total > max_unpacked {
            return Err("the module zip unpacks larger than the limit".into());
        }
        let mut h = Sha256::new();
        let mut buf = [0u8; 16 * 1024];
        let mut read = 0u64;
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|_| "the module zip cannot be read".to_owned())?;
            if n == 0 {
                break;
            }
            read += n as u64;
            if read > f.size().saturating_add(1) {
                return Err("a file of the module zip is longer than it says".into());
            }
            h.update(&buf[..n]);
        }
        lines.push((name, h.finalize().into()));
    }
    if lines.is_empty() {
        return Err("the module zip has no files".into());
    }
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(h1_of_lines(&lines))
}

/// A `go.mod` file: the `go` directive and the requirements, as facts.
pub fn parse_go_mod_facts(content: &[u8]) -> Result<Facts, String> {
    let text = std::str::from_utf8(content).map_err(|_| "go.mod is not UTF-8".to_owned())?;
    let mut deps = BTreeMap::new();
    let mut traits = BTreeMap::new();
    let mut block = false;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if block {
            if line == ")" {
                block = false;
                continue;
            }
            record_go_require(line, &mut deps);
            continue;
        }
        if let Some(rest) = line.strip_prefix("require") {
            let rest = rest.trim();
            if rest == "(" {
                block = true;
            } else {
                record_go_require(rest, &mut deps);
            }
        } else if let Some(v) = line.strip_prefix("go ") {
            traits.insert("go".into(), v.trim().to_owned());
        }
    }
    Ok(Facts {
        deps,
        traits,
        ..Facts::default()
    })
}

fn record_go_require(line: &str, deps: &mut BTreeMap<String, String>) {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() >= 2 {
        // Whether a requirement is marked `// indirect` does not change the module graph.
        deps.insert(format!("require:{}", f[0]), f[1].to_owned());
    }
}

#[cfg(test)]
#[path = "facts/tests.rs"]
mod tests;
