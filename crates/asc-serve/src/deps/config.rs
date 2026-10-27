//! `[deps]` of the server config: where the scanner may call, and its bounds.
//!
//! The endpoints are the operator's choice and nothing a repository can change. Every URL is judged by
//! the network guard (https, no private or metadata address, no credentials) and has to name a host the
//! egress allowlist admits. An empty URL switches that source off.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

use super::model::Ecosystem;
use crate::config::ByteSize;
use crate::netguard::{BlockReason, Guard, NetGuardConfig, Shape, classify};

/// One public service the scanner calls.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Endpoint {
    /// Base URL, no trailing slash. Empty in a file: the built-in one. Empty after
    /// [`Config::resolved`]: the endpoint is not used.
    pub url: String,
    /// `true` switches the endpoint off.
    pub off: bool,
    /// Seconds one request may take; 0 in a file: the built-in value.
    pub timeout_secs: u64,
    /// Largest response read, in bytes, more is an error, never a prefix; 0 in a file: the built-in
    /// value.
    pub max_bytes: ByteSize,
}

impl Endpoint {
    fn new(url: &str, timeout_secs: u64, max: u64) -> Self {
        Self {
            url: url.to_owned(),
            off: false,
            timeout_secs,
            max_bytes: ByteSize(max),
        }
    }

    /// The endpoint with what a file left out taken from `built_in`.
    fn filled(mut self, built_in: &Self) -> Self {
        if self.off {
            self.url.clear();
        } else if self.url.is_empty() {
            self.url = built_in.url.clone();
        }
        if self.timeout_secs == 0 {
            self.timeout_secs = built_in.timeout_secs;
        }
        if self.max_bytes.0 == 0 {
            self.max_bytes = built_in.max_bytes;
        }
        self
    }

    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }
}

impl Default for Endpoint {
    /// What a table that names nothing means: all the built-in values.
    fn default() -> Self {
        Self::new("", 0, 0)
    }
}

/// The public registries the newest versions are read from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Registries {
    pub crates_io: Endpoint,
    pub npm: Endpoint,
    pub pypi: Endpoint,
    pub go: Endpoint,
    pub maven: Endpoint,
    pub nuget: Endpoint,
    pub packagist: Endpoint,
    pub rubygems: Endpoint,
    pub hex: Endpoint,
    #[serde(rename = "pub")]
    pub pub_dev: Endpoint,
}

impl Default for Registries {
    fn default() -> Self {
        let m = ByteSize::MIB;
        Self {
            crates_io: Endpoint::new("https://crates.io", 20, 8 * m),
            npm: Endpoint::new("https://registry.npmjs.org", 20, 8 * m),
            pypi: Endpoint::new("https://pypi.org", 15, 8 * m),
            go: Endpoint::new("https://proxy.golang.org", 15, m),
            maven: Endpoint::new("https://repo1.maven.org/maven2", 15, 2 * m),
            nuget: Endpoint::new("https://api.nuget.org", 15, 4 * m),
            packagist: Endpoint::new("https://repo.packagist.org", 15, 8 * m),
            rubygems: Endpoint::new("https://rubygems.org", 15, 8 * m),
            hex: Endpoint::new("https://hex.pm", 15, 4 * m),
            pub_dev: Endpoint::new("https://pub.dev", 15, 8 * m),
        }
    }
}

impl Registries {
    /// The registry of `eco`; `None` for ecosystems without a public registry API.
    pub fn of(&self, eco: Ecosystem) -> Option<&Endpoint> {
        Some(match eco {
            Ecosystem::CratesIo => &self.crates_io,
            Ecosystem::Npm => &self.npm,
            Ecosystem::PyPi => &self.pypi,
            Ecosystem::Go => &self.go,
            Ecosystem::Maven => &self.maven,
            Ecosystem::NuGet => &self.nuget,
            Ecosystem::Packagist => &self.packagist,
            Ecosystem::RubyGems => &self.rubygems,
            Ecosystem::Hex => &self.hex,
            Ecosystem::Pub => &self.pub_dev,
            Ecosystem::SwiftUrl | Ecosystem::CocoaPods => return None,
        })
    }

    fn all(&self) -> [(&'static str, &Endpoint); 10] {
        [
            ("crates_io", &self.crates_io),
            ("npm", &self.npm),
            ("pypi", &self.pypi),
            ("go", &self.go),
            ("maven", &self.maven),
            ("nuget", &self.nuget),
            ("packagist", &self.packagist),
            ("rubygems", &self.rubygems),
            ("hex", &self.hex),
            ("pub", &self.pub_dev),
        ]
    }
}

/// `[deps]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Serve `DepsService` and answer the policy gate. Off, a policy that sets `[deps] block_severity`
    /// refuses every landing (it cannot be checked).
    pub enabled: bool,
    /// The OSV API: `https://api.osv.dev`, or a self-hosted mirror that speaks `/v1/querybatch` and
    /// `/v1/vulns/<id>`. Empty: advisories come from `advisory_file` only.
    pub osv: Endpoint,
    /// A file of OSV records (a zip of one record per entry as OSV publishes, a JSON array, or JSON
    /// lines) read in addition to, or instead of, the API. Read again when it changes.
    pub advisory_file: Option<PathBuf>,
    /// Largest advisory file, after decompression.
    pub advisory_max_bytes: ByteSize,
    /// Hosts the scanner may call, exact or `*.suffix`. Empty: the hosts of the URLs above.
    pub egress_allow: Vec<String>,
    pub registries: Registries,
    /// Seconds an advisory answer for a package version is reused.
    pub osv_ttl_secs: u64,
    /// Seconds an advisory record is reused.
    pub vuln_ttl_secs: u64,
    /// Seconds a registry answer is reused.
    pub latest_ttl_secs: u64,
    /// Most manifest and lock files one scan reads.
    pub max_manifest_files: usize,
    /// Most bytes of those files one scan reads.
    pub max_manifest_bytes: ByteSize,
    /// Most distinct package versions one scan looks up.
    pub max_packages: usize,
    /// Requests in flight at once for one scan.
    pub concurrency: usize,
    /// Scans (and update plans) running at once, server wide.
    pub max_concurrent_scans: usize,
    /// Seconds the landing gate waits for the advisory lookup before it counts it failed.
    pub gate_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            osv: Endpoint::new("https://api.osv.dev", 30, 16 * ByteSize::MIB),
            advisory_file: None,
            advisory_max_bytes: ByteSize(512 * ByteSize::MIB),
            egress_allow: Vec::new(),
            registries: Registries::default(),
            osv_ttl_secs: 6 * 3600,
            vuln_ttl_secs: 24 * 3600,
            latest_ttl_secs: 3600,
            max_manifest_files: 2000,
            max_manifest_bytes: ByteSize(64 * ByteSize::MIB),
            max_packages: 100_000,
            concurrency: 8,
            max_concurrent_scans: 2,
            gate_timeout_secs: 25,
        }
    }
}

/// Whether `host` is admitted by `pattern`: equal, or `*.suffix` and a subdomain of the suffix.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let (p, h) = (
        pattern.trim_end_matches('.').to_ascii_lowercase(),
        host.trim_end_matches('.').to_ascii_lowercase(),
    );
    match p.strip_prefix("*.") {
        Some(suffix) => h.len() > suffix.len() + 1 && h.ends_with(&format!(".{suffix}")),
        None => p == h,
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

impl Config {
    /// The config with every value a file left out taken from the built-in one, and every endpoint
    /// that is switched off without a URL. The scanner only ever sees a resolved config.
    #[must_use]
    pub fn resolved(mut self) -> Self {
        let d = Self::default();
        self.osv = self.osv.filled(&d.osv);
        let r = &mut self.registries;
        let b = &d.registries;
        r.crates_io = r.crates_io.clone().filled(&b.crates_io);
        r.npm = r.npm.clone().filled(&b.npm);
        r.pypi = r.pypi.clone().filled(&b.pypi);
        r.go = r.go.clone().filled(&b.go);
        r.maven = r.maven.clone().filled(&b.maven);
        r.nuget = r.nuget.clone().filled(&b.nuget);
        r.packagist = r.packagist.clone().filled(&b.packagist);
        r.rubygems = r.rubygems.clone().filled(&b.rubygems);
        r.hex = r.hex.clone().filled(&b.hex);
        r.pub_dev = r.pub_dev.clone().filled(&b.pub_dev);
        self
    }

    /// The URLs the config names, with the key of each.
    fn urls(&self) -> Vec<(String, &str)> {
        let mut v = vec![("osv".to_owned(), self.osv.url.as_str())];
        for (k, e) in self.registries.all() {
            v.push((format!("registries.{k}"), e.url.as_str()));
        }
        v.into_iter().filter(|(_, u)| !u.is_empty()).collect()
    }

    /// The hosts the scanner may call.
    pub fn allowed_hosts(&self) -> Vec<String> {
        if !self.egress_allow.is_empty() {
            return self.egress_allow.clone();
        }
        let mut hosts: Vec<String> = self.urls().iter().filter_map(|(_, u)| host_of(u)).collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// Checks the bounds and every URL against the network guard and the allowlist. The error names the
    /// key.
    pub fn validate(&self, net: &NetGuardConfig) -> Result<(), String> {
        let guard = Guard::new(net.clone());
        let mut endpoints: Vec<(String, &Endpoint)> = vec![("osv".to_owned(), &self.osv)];
        for (k, e) in self.registries.all() {
            endpoints.push((format!("registries.{k}"), e));
        }
        for (key, e) in &endpoints {
            if e.timeout_secs == 0 || e.timeout_secs > 600 {
                return Err(format!("deps.{key}.timeout_secs must be 1 to 600"));
            }
            if e.max_bytes.0 < ByteSize::KIB || e.max_bytes.0 > 256 * ByteSize::MIB {
                return Err(format!("deps.{key}.max_bytes must be 1KiB to 256MiB"));
            }
            if e.url.is_empty() {
                continue;
            }
            if e.url.ends_with('/') {
                return Err(format!("deps.{key}.url must not end with a slash"));
            }
            guard
                .parse_endpoint(&e.url, Shape::Endpoint)
                .map_err(|b| format!("deps.{key}.url is refused: {}", b.0.message()))?;
            // An address in the URL is judged here; a name is judged when it is resolved.
            if let Some(ip) =
                host_of(&e.url).and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
                && let Some(r) = classify(ip)
                && !(r == BlockReason::Loopback && net.allow_loopback_http)
            {
                return Err(format!("deps.{key}.url is refused: {}", r.message()));
            }
        }
        for h in &self.egress_allow {
            let bare = h.strip_prefix("*.").unwrap_or(h);
            if bare.is_empty() || bare.contains(['/', ':', ' ', '*']) {
                return Err(format!(
                    "deps.egress_allow: {h:?} is not a host or *.suffix"
                ));
            }
        }
        if self.egress_allow.len() > 64 {
            return Err("deps.egress_allow holds at most 64 entries".into());
        }
        let allowed = self.allowed_hosts();
        for (key, url) in self.urls() {
            let host = host_of(url).unwrap_or_default();
            if !allowed.iter().any(|p| host_matches(p, &host)) {
                return Err(format!(
                    "deps.{key}.url names {host}, which deps.egress_allow does not admit"
                ));
            }
        }
        if self.osv.url.is_empty() && self.advisory_file.is_none() {
            return Err("deps needs deps.osv.url or deps.advisory_file".into());
        }
        if self.advisory_max_bytes.0 < ByteSize::MIB
            || self.advisory_max_bytes.0 > 8 * ByteSize::GIB
        {
            return Err("deps.advisory_max_bytes must be 1MiB to 8GiB".into());
        }
        for (k, v, max) in [
            ("osv_ttl_secs", self.osv_ttl_secs, 30 * 86_400),
            ("vuln_ttl_secs", self.vuln_ttl_secs, 30 * 86_400),
            ("latest_ttl_secs", self.latest_ttl_secs, 30 * 86_400),
            ("gate_timeout_secs", self.gate_timeout_secs, 600),
        ] {
            if v == 0 || v > max {
                return Err(format!("deps.{k} must be 1 to {max}"));
            }
        }
        if self.max_manifest_files == 0 || self.max_manifest_files > 100_000 {
            return Err("deps.max_manifest_files must be 1 to 100000".into());
        }
        if self.max_manifest_bytes.0 < ByteSize::MIB
            || self.max_manifest_bytes.0 > 4 * ByteSize::GIB
        {
            return Err("deps.max_manifest_bytes must be 1MiB to 4GiB".into());
        }
        if self.max_packages == 0 || self.max_packages > 2_000_000 {
            return Err("deps.max_packages must be 1 to 2000000".into());
        }
        if self.concurrency == 0 || self.concurrency > 64 {
            return Err("deps.concurrency must be 1 to 64".into());
        }
        if self.max_concurrent_scans == 0 || self.max_concurrent_scans > 64 {
            return Err("deps.max_concurrent_scans must be 1 to 64".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> NetGuardConfig {
        NetGuardConfig::default()
    }

    #[test]
    fn the_defaults_are_valid_and_name_the_public_services() {
        let c = Config::default();
        c.validate(&net()).unwrap();
        assert!(c.enabled);
        let hosts = c.allowed_hosts();
        for h in [
            "api.osv.dev",
            "crates.io",
            "registry.npmjs.org",
            "pypi.org",
            "proxy.golang.org",
            "repo1.maven.org",
            "api.nuget.org",
            "repo.packagist.org",
            "rubygems.org",
            "hex.pm",
            "pub.dev",
        ] {
            assert!(hosts.iter().any(|x| x == h), "{h} in {hosts:?}");
        }
        for eco in Ecosystem::ALL {
            assert_eq!(c.registries.of(eco).is_some(), eco.has_registry(), "{eco}");
        }
    }

    #[test]
    fn a_toml_section_overrides_and_unknown_keys_are_errors() {
        let c: Config = toml::from_str(
            "osv.url = \"https://osv.mirror.example\"\negress_allow = [\"*.example\", \"crates.io\", \"registry.npmjs.org\", \"pypi.org\", \"proxy.golang.org\", \"repo1.maven.org\", \"api.nuget.org\", \"repo.packagist.org\", \"rubygems.org\", \"hex.pm\", \"pub.dev\"]\n[registries.pub]\noff = true\n[registries.npm]\ntimeout_secs = 5\nmax_bytes = \"1MiB\"\n",
        )
        .unwrap();
        let c = c.resolved();
        c.validate(&net()).unwrap();
        assert_eq!(c.osv.url, "https://osv.mirror.example");
        assert_eq!(
            c.osv.timeout_secs, 30,
            "unnamed keys keep their built-in value"
        );
        assert!(!c.registries.pub_dev.enabled());
        assert_eq!(c.registries.npm.timeout_secs, 5);
        assert_eq!(c.registries.npm.max_bytes.0, ByteSize::MIB);
        assert_eq!(
            c.registries.npm.url, "https://registry.npmjs.org",
            "unnamed keys keep their default"
        );
        assert!(toml::from_str::<Config>("bogus = 1\n").is_err());
        assert!(toml::from_str::<Config>("[registries.cobol]\nurl = \"x\"\n").is_err());
    }

    #[test]
    fn urls_are_judged_by_the_guard_and_the_allowlist() {
        let with = |f: &dyn Fn(&mut Config)| {
            let mut c = Config::default();
            f(&mut c);
            c.validate(&net())
        };
        let e = with(&|c| c.osv.url = "http://api.osv.dev".into()).unwrap_err();
        assert!(e.contains("deps.osv.url"), "{e}");
        let e = with(&|c| c.osv.url = "https://127.0.0.1".into()).unwrap_err();
        assert!(e.contains("deps.osv.url"), "{e}");
        let e = with(&|c| c.osv.url = "https://10.1.2.3:8443".into()).unwrap_err();
        assert!(e.contains("deps.osv.url"), "{e}");
        let e = with(&|c| c.osv.url = "https://user:pw@osv.example".into()).unwrap_err();
        assert!(e.contains("deps.osv.url"), "{e}");
        let e = with(&|c| c.osv.url = "https://osv.example/".into()).unwrap_err();
        assert!(e.contains("slash"), "{e}");
        let e = with(&|c| c.egress_allow = vec!["api.osv.dev".into()]).unwrap_err();
        assert!(e.contains("does not admit"), "{e}");
        let e = with(&|c| c.egress_allow = vec!["https://x".into()]).unwrap_err();
        assert!(e.contains("egress_allow"), "{e}");
        // The operator's loopback opt-in lets a local mirror be plain http.
        let mut n = net();
        n.allow_loopback_http = true;
        let mut c = Config::default();
        c.osv.url = "http://127.0.0.1:8080".into();
        c.validate(&n).unwrap();
        let e = with(&|c| {
            c.osv.url = String::new();
            c.advisory_file = None;
        })
        .unwrap_err();
        assert!(e.contains("advisory_file"), "{e}");
        with(&|c| {
            c.osv.url = String::new();
            c.advisory_file = Some("/var/lib/asc/osv.zip".into());
        })
        .unwrap();
    }

    #[test]
    fn bounds_are_checked_and_name_their_key() {
        for (f, key) in [
            (
                Box::new(|c: &mut Config| c.osv.timeout_secs = 601) as Box<dyn Fn(&mut Config)>,
                "osv.timeout_secs",
            ),
            (
                Box::new(|c| c.registries.go.max_bytes = ByteSize(10)),
                "registries.go.max_bytes",
            ),
            (Box::new(|c| c.osv_ttl_secs = 0), "osv_ttl_secs"),
            (Box::new(|c| c.gate_timeout_secs = 601), "gate_timeout_secs"),
            (Box::new(|c| c.max_manifest_files = 0), "max_manifest_files"),
            (Box::new(|c| c.concurrency = 65), "concurrency"),
            (Box::new(|c| c.max_packages = 0), "max_packages"),
            (
                Box::new(|c| c.max_concurrent_scans = 0),
                "max_concurrent_scans",
            ),
            (
                Box::new(|c| c.advisory_max_bytes = ByteSize(1)),
                "advisory_max_bytes",
            ),
        ] {
            let mut c = Config::default();
            f(&mut c);
            let e = c.validate(&net()).unwrap_err();
            assert!(e.contains(key), "{key}: {e}");
        }
    }

    #[test]
    fn host_patterns_match_exactly_or_by_subdomain() {
        assert!(host_matches("api.osv.dev", "API.OSV.DEV"));
        assert!(host_matches("*.example.com", "a.example.com"));
        assert!(host_matches("*.example.com", "a.b.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(!host_matches("*.example.com", "evilexample.com"));
        assert!(!host_matches("example.com", "a.example.com"));
        assert!(host_matches("example.com.", "example.com"));
    }
}
