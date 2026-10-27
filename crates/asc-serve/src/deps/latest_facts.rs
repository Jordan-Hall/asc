//! Asking registries for the facts a lockfile records about one version.
//!
//! [`Latest::facts`] is the full question: every digest comes from the registry and is checked against a
//! second path the same registry (or its checksum database) offers, and a package that publishes a
//! signature has it verified. [`Latest::declared`] is the short one, for a package that only has to say
//! what it depends on. Both fail with a reason a person can read; neither returns a digest it could not
//! verify.

use super::config::Endpoint;
use super::egress::Limits;
use super::facts::{
    Facts, GoSums, crates_index_path, cross_check_pypi, go_mod_h1, go_zip_h1, npm_keys,
    npm_signatures, parse_compact_index, parse_crates_index, parse_crates_version,
    parse_go_mod_facts, parse_npm_version, parse_packagist_version, parse_pypi_release,
    parse_pypi_simple, parse_rubygems_version, parse_sumdb, verify_npm_signature,
};
use super::latest::Latest;
use super::listing::go_escape;
use super::model::Ecosystem;
use super::ordered::Ordered;
use super::osv::join;
use super::safe;
use super::sumdb;

/// Files and unpacked bytes a module zip may have.
const ZIP_FILES: usize = 100_000;
const ZIP_UNPACKED: u64 = 1 << 30;

fn lim(ep: &Endpoint) -> Limits {
    Limits {
        timeout: std::time::Duration::from_secs(ep.timeout_secs),
        max_bytes: usize::try_from(ep.max_bytes.0).unwrap_or(usize::MAX),
    }
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && !s.contains("..")
        && s.len() <= 214
        && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}

/// Hosts the public registries serve Composer archives and sources from.
const COMPOSER_HOSTS: [&str; 7] = [
    "api.github.com",
    "codeload.github.com",
    "github.com",
    "gitlab.com",
    "bitbucket.org",
    "api.bitbucket.org",
    "repo.packagist.org",
];

impl Latest {
    /// Whether `url` names the registry's own host, a CDN the registry is known to use, or a host the
    /// operator listed.
    fn host_ok(&self, url: &str, ep: &Endpoint, builtin: &[&str]) -> bool {
        let Some(h) = safe::host_of(url) else {
            return false;
        };
        safe::host_of(&ep.url).is_some_and(|e| e == h)
            || builtin.contains(&h.as_str())
            || self.artifact_hosts.contains(&h)
    }

    /// The URL must be on a host the registry may serve it from.
    fn check_host(
        &self,
        what: &str,
        url: &str,
        ep: &Endpoint,
        builtin: &[&str],
    ) -> Result<(), String> {
        if self.host_ok(url, ep, builtin) {
            Ok(())
        } else {
            Err(format!(
                "{what} names a host ({}) other than the registry's; list it in deps.artifact_hosts if it is its CDN",
                safe::host_of(url).unwrap_or_default()
            ))
        }
    }

    async fn read(
        &self,
        ep: &Endpoint,
        what: &str,
        url: Result<String, String>,
        accept: Option<&str>,
        limits: Option<Limits>,
    ) -> Result<Vec<u8>, String> {
        if !ep.enabled() {
            return Err(format!("{what}: lookups are switched off"));
        }
        let url = url.map_err(|e| format!("{what}: {e}"))?;
        let limits = limits.unwrap_or_else(|| lim(ep));
        self.egress
            .get_retry(&url, &limits, accept)
            .await
            .map_err(|e| format!("{what}: {e}"))
    }

    /// What `name` at `version` declares it depends on, from the registry's primary answer only.
    pub async fn declared(
        &self,
        eco: Ecosystem,
        name: &str,
        version: &str,
    ) -> Result<Facts, String> {
        self.collect(eco, name, version, false).await
    }

    /// Every fact a lockfile entry for `name` at `version` needs, verified.
    pub async fn facts(&self, eco: Ecosystem, name: &str, version: &str) -> Result<Facts, String> {
        self.collect(eco, name, version, true).await
    }

    async fn collect(
        &self,
        eco: Ecosystem,
        name: &str,
        version: &str,
        full: bool,
    ) -> Result<Facts, String> {
        if !segment_ok(version) || !super::version::is_safe_literal(version) {
            return Err("the version is not usable".into());
        }
        super::names::check(eco, name)?;
        let regs = &self.regs;
        match eco {
            Ecosystem::Npm => self.npm(&regs.npm, name, version, full).await,
            Ecosystem::CratesIo => self.crates(name, version, full).await,
            Ecosystem::PyPi => self.pypi(&regs.pypi, name, version, full).await,
            Ecosystem::RubyGems => self.gems(&regs.rubygems, name, version, full).await,
            Ecosystem::Packagist => self.packagist(&regs.packagist, name, version).await,
            Ecosystem::Go => self.go(name, version, full).await,
            other => Err(format!("{other} lockfiles are not rewritten")),
        }
    }

    // -------------------------------------------------------------------------- npm

    async fn npm(
        &self,
        ep: &Endpoint,
        name: &str,
        version: &str,
        full: bool,
    ) -> Result<Facts, String> {
        let url = join(&ep.url, &[name, version]);
        let body = self.read(ep, "npm registry", url, None, None).await?;
        let mut facts =
            parse_npm_version(&body, name, version).map_err(|e| format!("npm registry: {e}"))?;
        if !full {
            return Ok(facts);
        }
        safe::check(&facts).map_err(|e| format!("npm registry: {e}"))?;
        if let Some(t) = &facts.tarball {
            self.check_host("the tarball", t, ep, &[])?;
        }
        let sigs = npm_signatures(&body);
        if sigs.is_empty() {
            // The public registry signs every package it serves; a package without a signature is
            // one it did not vouch for. A private registry that does not sign may be allowed by the
            // operator, and what it gives is then recorded as unverified, never as verified.
            if !self.npm_unsigned_ok {
                return Err(
                    "npm registry: the package has no registry signature (deps.npm_unsigned allows a registry that does not sign)"
                        .into(),
                );
            }
            facts
                .unverified
                .push("the npm registry published no signature for the package".into());
            return Ok(facts);
        }
        // A key list that could not be had is not cached: the next question asks again.
        let keys = self
            .npm_keys
            .get_or_try_init(|| async {
                let url = join(&ep.url, &["-", "npm", "v1", "keys"]);
                let mut last = String::new();
                for (i, wait) in [0u64, 200, 400].into_iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    }
                    match self.read(ep, "npm registry", url.clone(), None, None).await {
                        Ok(b) => {
                            let keys = npm_keys(&b);
                            if keys.is_empty() {
                                return Err("npm registry: it publishes no signing keys".to_owned());
                            }
                            return Ok(keys);
                        }
                        Err(e) => last = e,
                    }
                }
                Err(format!(
                    "the registry's signing keys could not be had ({last})"
                ))
            })
            .await?;
        let integrity = facts.integrity.clone().unwrap_or_default();
        if verify_npm_signature(name, version, &integrity, &sigs, keys)
            .map_err(|e| format!("npm registry: {e}"))?
        {
            facts
                .verified
                .push("the npm registry's signature of the package verifies".into());
            Ok(facts)
        } else {
            Err("npm registry: the package is signed by no key the registry publishes".into())
        }
    }

    // -------------------------------------------------------------------------- crates.io

    async fn crates(&self, name: &str, version: &str, full: bool) -> Result<Facts, String> {
        if !segment_ok(name) {
            return Err("the package name is not usable".into());
        }
        let ep = &self.regs.crates_index;
        let path = crates_index_path(name)?;
        let segs: Vec<&str> = path.iter().map(String::as_str).collect();
        let body = self
            .read(ep, "crates.io index", join(&ep.url, &segs), None, None)
            .await?;
        let mut facts = parse_crates_index(&body, name, version)
            .map_err(|e| format!("crates.io index: {e}"))?;
        if !full {
            return Ok(facts);
        }
        let api = &self.regs.crates_io;
        let b = self
            .read(
                api,
                "crates.io",
                join(&api.url, &["api", "v1", "crates", name, version]),
                None,
                None,
            )
            .await?;
        let sum = parse_crates_version(&b, version).map_err(|e| format!("crates.io: {e}"))?;
        if facts.sha256.as_deref() != Some(sum.as_str()) {
            return Err("the crates.io API and its index disagree on the checksum".into());
        }
        facts
            .verified
            .push("the crates.io API and index agree on the checksum".into());
        Ok(facts)
    }

    // -------------------------------------------------------------------------- PyPI

    async fn pypi(
        &self,
        ep: &Endpoint,
        name: &str,
        version: &str,
        full: bool,
    ) -> Result<Facts, String> {
        if !segment_ok(name) {
            return Err("the package name is not usable".into());
        }
        let body = self
            .read(
                ep,
                "PyPI",
                join(&ep.url, &["pypi", name, version, "json"]),
                None,
                None,
            )
            .await?;
        let mut facts = parse_pypi_release(&body, version).map_err(|e| format!("PyPI: {e}"))?;
        if !full {
            return Ok(facts);
        }
        safe::check(&facts).map_err(|e| format!("PyPI: {e}"))?;
        for f in &facts.files {
            self.check_host(
                &format!("the URL of {}", f.filename),
                &f.url,
                ep,
                &["files.pythonhosted.org"],
            )?;
        }
        let canon = Ecosystem::PyPi.canonical(name);
        let simple = self
            .read(
                ep,
                "PyPI simple index",
                join(&ep.url, &["simple", &canon, ""]),
                Some("application/vnd.pypi.simple.v1+json"),
                None,
            )
            .await?;
        let files = parse_pypi_simple(&simple).map_err(|e| format!("PyPI simple index: {e}"))?;
        cross_check_pypi(&facts.files, &files)?;
        facts
            .verified
            .push("the PyPI JSON API and the simple index agree on every file digest".into());
        Ok(facts)
    }

    // -------------------------------------------------------------------------- RubyGems

    async fn gems(
        &self,
        ep: &Endpoint,
        name: &str,
        version: &str,
        full: bool,
    ) -> Result<Facts, String> {
        if !segment_ok(name) {
            return Err("the gem name is not usable".into());
        }
        let body = self
            .read(
                ep,
                "RubyGems compact index",
                join(&ep.url, &["info", name]),
                None,
                None,
            )
            .await?;
        let mut facts = parse_compact_index(&body, version)
            .map_err(|e| format!("RubyGems compact index: {e}"))?;
        if !full {
            return Ok(facts);
        }
        let file = format!("{version}.json");
        let api = self
            .read(
                ep,
                "RubyGems",
                join(&ep.url, &["api", "v2", "rubygems", name, "versions", &file]),
                None,
                None,
            )
            .await?;
        let sum = parse_rubygems_version(&api, version).map_err(|e| format!("RubyGems: {e}"))?;
        if facts.sha256.as_deref() != Some(sum.as_str()) {
            return Err("the RubyGems API and its compact index disagree on the checksum".into());
        }
        facts
            .verified
            .push("the RubyGems API and compact index agree on the checksum".into());
        Ok(facts)
    }

    // -------------------------------------------------------------------------- Packagist

    async fn packagist(&self, ep: &Endpoint, name: &str, version: &str) -> Result<Facts, String> {
        let (vendor, package) = name
            .split_once('/')
            .ok_or("a Packagist name is vendor/package")?;
        let (vendor, package) = (vendor.to_ascii_lowercase(), package.to_ascii_lowercase());
        if !segment_ok(&vendor) || !segment_ok(&package) {
            return Err("the package name is not usable".into());
        }
        let file = format!("{package}.json");
        let body = self
            .read(
                ep,
                "Packagist",
                join(&ep.url, &["p2", &vendor, &file]),
                None,
                None,
            )
            .await?;
        let facts = parse_packagist_version(&body, &format!("{vendor}/{package}"), version)
            .map_err(|e| format!("Packagist: {e}"))?;
        safe::check(&facts).map_err(|e| format!("Packagist: {e}"))?;
        if let Some(entry) = &facts.entry {
            for part in ["dist", "source"] {
                if let Some(u) = entry
                    .get(part)
                    .and_then(|p| p.get("url"))
                    .and_then(Ordered::as_str)
                {
                    self.check_host(&format!("the {part} URL"), u, ep, &COMPOSER_HOSTS)?;
                }
            }
        }
        Ok(facts)
    }

    // -------------------------------------------------------------------------- Go

    async fn go(&self, module: &str, version: &str, full: bool) -> Result<Facts, String> {
        let escaped: Vec<String> = module.split('/').map(go_escape).collect();
        if escaped.iter().any(|s| !segment_ok(s)) {
            return Err("the module path is not usable".into());
        }
        let ver = go_escape(version);
        let proxy = &self.regs.go;
        let mut seg: Vec<&str> = escaped.iter().map(String::as_str).collect();
        seg.push("@v");
        let mod_file = format!("{ver}.mod");
        seg.push(&mod_file);
        let mod_bytes = self
            .read(proxy, "Go proxy", join(&proxy.url, &seg), None, None)
            .await?;
        let mut facts = parse_go_mod_facts(&mod_bytes).map_err(|e| format!("Go proxy: {e}"))?;
        let module_sum = go_mod_h1(&mod_bytes);
        if !full {
            // The hash of the go.mod is known; the zip's is only known with the checksum database.
            facts.go = Some(GoSums {
                zip: String::new(),
                module: module_sum,
            });
            return Ok(facts);
        }
        // The checksum database says what the hashes are; the proxy's files must hash to the same.
        let db = &self.regs.go_sumdb;
        let last = format!(
            "{}@{ver}",
            escaped.last().map(String::as_str).unwrap_or_default()
        );
        let mut lookup: Vec<&str> = vec!["lookup"];
        lookup.extend(escaped[..escaped.len() - 1].iter().map(String::as_str));
        lookup.push(&last);
        let body = self
            .read(
                db,
                "Go checksum database",
                join(&db.url, &lookup),
                None,
                None,
            )
            .await?;
        // The database's answer is believed only after its signed tree head verifies and the record is
        // shown to be in that tree.
        let lookup =
            sumdb::parse_lookup(&body).map_err(|e| format!("Go checksum database: {e}"))?;
        sumdb::verify_note(&lookup, &self.sumdb_key)
            .map_err(|e| format!("Go checksum database: {e}"))?;
        let mut tiles = std::collections::HashMap::new();
        for t in sumdb::tiles_needed(&lookup).map_err(|e| format!("Go checksum database: {e}"))? {
            let path = t.path();
            let segs: Vec<&str> = path.iter().map(String::as_str).collect();
            let data = self
                .read(db, "Go checksum database", join(&db.url, &segs), None, None)
                .await?;
            tiles.insert(t, data);
        }
        sumdb::verify_inclusion(&lookup, &tiles)
            .map_err(|e| format!("Go checksum database: {e}"))?;
        let recorded = parse_sumdb(&body, module, version)
            .map_err(|e| format!("Go checksum database: {e}"))?;
        if recorded.module != module_sum {
            return Err(
                "the go.mod the Go proxy serves does not match the checksum database".into(),
            );
        }
        let zip_file = format!("{ver}.zip");
        let mut zseg: Vec<&str> = escaped.iter().map(String::as_str).collect();
        zseg.push("@v");
        zseg.push(&zip_file);
        let zip = self
            .read(
                proxy,
                "Go proxy",
                join(&proxy.url, &zseg),
                None,
                Some(self.archive),
            )
            .await?;
        let zip_sum = go_zip_h1(&zip, module, version, ZIP_FILES, ZIP_UNPACKED)?;
        if zip_sum != recorded.zip {
            return Err(
                "the module zip the Go proxy serves does not match the checksum database".into(),
            );
        }
        facts.go = Some(GoSums {
            zip: zip_sum,
            module: module_sum,
        });
        facts
            .verified
            .push("the checksum database's signed tree head and inclusion proof verify, and the Go proxy's go.mod and zip hash to its records".into());
        Ok(facts)
    }
}
