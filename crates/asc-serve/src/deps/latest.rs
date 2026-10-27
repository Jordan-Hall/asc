//! Asking the public registries which versions a package has.
//!
//! The call goes through [`Egress`] with the registry's own timeout and size cap, its answer is read by
//! [`super::listing`], and the result is cached for `latest_ttl_secs`. A registry that does not answer is
//! an error naming the ecosystem, never "no update".

use std::sync::Arc;
use std::time::Duration;

use sqlx::AnyPool;

use super::config::Registries;
use super::egress::{Egress, FetchError, Limits};
use super::listing::{
    Versions, go_escape, parse_crates, parse_go, parse_hex, parse_maven, parse_npm,
    parse_npm_latest, parse_nuget, parse_packagist, parse_pub, parse_pypi, parse_rubygems,
};
use super::model::Ecosystem;
use super::osv::{Clock, join};
use super::store;

/// A name that is one path segment: no separator, no `..`, no control character.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && !s.contains("..")
        && s.len() <= 214
        && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}

/// An npm name: one segment, or `@scope/name`.
fn npm_name_ok(s: &str) -> bool {
    match s.strip_prefix('@') {
        Some(rest) => rest
            .split_once('/')
            .is_some_and(|(scope, name)| segment_ok(scope) && segment_ok(name)),
        None => segment_ok(s),
    }
}

pub struct Latest {
    pub(super) egress: Arc<Egress>,
    pool: AnyPool,
    pub(super) regs: Registries,
    ttl: u64,
    clock: Clock,
    /// The largest download read to verify a hash, and the time it may take.
    pub(super) archive: Limits,
    /// The registry keys npm signatures are checked against, once per process.
    pub(super) npm_keys: tokio::sync::OnceCell<Vec<(String, String)>>,
}

impl Latest {
    pub fn new(
        egress: Arc<Egress>,
        pool: AnyPool,
        regs: Registries,
        ttl: u64,
        clock: Clock,
    ) -> Self {
        Self {
            egress,
            pool,
            regs,
            ttl,
            clock,
            archive: Limits {
                timeout: Duration::from_secs(120),
                max_bytes: 128 << 20,
            },
            npm_keys: tokio::sync::OnceCell::new(),
        }
    }

    /// The bound on a download that verifies a hash (`archive_max_bytes`, `archive_timeout_secs`).
    #[must_use]
    pub fn with_archive(mut self, max_bytes: u64, timeout_secs: u64) -> Self {
        self.archive = Limits {
            timeout: Duration::from_secs(timeout_secs),
            max_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
        };
        self
    }

    /// Whether `eco` can be asked.
    pub fn available(&self, eco: Ecosystem) -> bool {
        self.regs.of(eco).is_some_and(|e| e.enabled())
    }

    async fn fetch(
        &self,
        eco: Ecosystem,
        url: &str,
        accept: Option<&str>,
    ) -> Result<Vec<u8>, FetchError> {
        let e = self
            .regs
            .of(eco)
            .ok_or(FetchError::Blocked("no registry"))?;
        let lim = Limits {
            timeout: Duration::from_secs(e.timeout_secs),
            max_bytes: usize::try_from(e.max_bytes.0).unwrap_or(usize::MAX),
        };
        self.egress.get_retry(url, &lim, accept).await
    }

    /// The versions of `name`, from the cache or the registry.
    pub async fn versions(
        &self,
        eco: Ecosystem,
        name: &str,
        refresh: bool,
    ) -> Result<Arc<Versions>, String> {
        let key = format!("{}|{name}", eco.osv_name());
        let now = (self.clock)();
        if !refresh
            && let Ok(Some(text)) =
                store::cache_get(&self.pool, "latest", &key, self.ttl, now).await
            && let Ok(v) = serde_json::from_str::<Versions>(&text)
        {
            return Ok(Arc::new(v));
        }
        let v = self
            .ask(eco, name)
            .await
            .map_err(|e| format!("{eco} registry: {e}"))?;
        if let Ok(text) = serde_json::to_string(&v) {
            let _ = store::cache_put(&self.pool, "latest", &key, &text, now).await;
        }
        Ok(Arc::new(v))
    }

    async fn ask(&self, eco: Ecosystem, name: &str) -> Result<Versions, String> {
        let base = match self.regs.of(eco) {
            Some(e) if e.enabled() => e.url.clone(),
            Some(_) => return Err("lookups are switched off".into()),
            None => return Err("the ecosystem has no registry API".into()),
        };
        super::names::check(eco, name)?;
        let err = |e: FetchError| e.to_string();
        let bad = |what: &str| format!("the answer is not usable ({what})");
        match eco {
            Ecosystem::CratesIo => {
                if !segment_ok(name) {
                    return Err("the package name is not usable".into());
                }
                let url = join(&base, &["api", "v1", "crates", name])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_crates(&b).map_err(|e| bad(&e))
            }
            Ecosystem::Npm => {
                if !npm_name_ok(name) {
                    return Err("the package name is not usable".into());
                }
                let url = join(&base, &[name])?;
                match self
                    .fetch(eco, &url, Some("application/vnd.npm.install-v1+json"))
                    .await
                {
                    Ok(b) => parse_npm(&b).map_err(|e| bad(&e)),
                    // A package with thousands of versions: its newest is still known.
                    Err(FetchError::TooLarge) => {
                        let url = join(&base, &[name, "latest"])?;
                        let b = self.fetch(eco, &url, None).await.map_err(err)?;
                        parse_npm_latest(&b).map_err(|e| bad(&e))
                    }
                    Err(e) => Err(err(e)),
                }
            }
            Ecosystem::PyPi => {
                if !segment_ok(name) {
                    return Err("the package name is not usable".into());
                }
                let url = join(&base, &["pypi", name, "json"])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_pypi(&b).map_err(|e| bad(&e))
            }
            Ecosystem::Go => {
                let escaped: Vec<String> = name.split('/').map(go_escape).collect();
                if escaped.iter().any(|s| !segment_ok(s)) {
                    return Err("the module path is not usable".into());
                }
                let mut seg: Vec<&str> = escaped.iter().map(String::as_str).collect();
                let mut list = seg.clone();
                list.extend(["@v", "list"]);
                seg.push("@latest");
                let list_b = self
                    .fetch(eco, &join(&base, &list)?, None)
                    .await
                    .map_err(err)?;
                let latest_b = self.fetch(eco, &join(&base, &seg)?, None).await.ok();
                parse_go(&list_b, latest_b.as_deref()).map_err(|e| bad(&e))
            }
            Ecosystem::Maven => {
                let (g, a) = name
                    .split_once(':')
                    .ok_or("a Maven name is group:artifact")?;
                let mut seg: Vec<&str> = g.split('.').collect();
                seg.push(a);
                seg.push("maven-metadata.xml");
                if seg.iter().any(|s| !segment_ok(s)) {
                    return Err("the coordinates are not usable".into());
                }
                let b = self
                    .fetch(eco, &join(&base, &seg)?, None)
                    .await
                    .map_err(err)?;
                parse_maven(&b).map_err(|e| bad(&e))
            }
            Ecosystem::NuGet => {
                let lower = name.to_ascii_lowercase();
                if !segment_ok(&lower) {
                    return Err("the package id is not usable".into());
                }
                let url = join(&base, &["v3-flatcontainer", &lower, "index.json"])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_nuget(&b).map_err(|e| bad(&e))
            }
            Ecosystem::Packagist => {
                let (vendor, package) = name
                    .split_once('/')
                    .ok_or("a Packagist name is vendor/package")?;
                let (vendor, package) = (vendor.to_ascii_lowercase(), package.to_ascii_lowercase());
                if !segment_ok(&vendor) || !segment_ok(&package) {
                    return Err("the package name is not usable".into());
                }
                let file = format!("{package}.json");
                let url = join(&base, &["p2", &vendor, &file])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_packagist(&b, &format!("{vendor}/{package}")).map_err(|e| bad(&e))
            }
            Ecosystem::RubyGems => {
                if !segment_ok(name) {
                    return Err("the gem name is not usable".into());
                }
                let file = format!("{name}.json");
                let url = join(&base, &["api", "v1", "versions", &file])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_rubygems(&b).map_err(|e| bad(&e))
            }
            Ecosystem::Hex => {
                if !segment_ok(name) {
                    return Err("the package name is not usable".into());
                }
                let url = join(&base, &["api", "packages", name])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_hex(&b).map_err(|e| bad(&e))
            }
            Ecosystem::Pub => {
                if !segment_ok(name) {
                    return Err("the package name is not usable".into());
                }
                let url = join(&base, &["api", "packages", name])?;
                let b = self.fetch(eco, &url, None).await.map_err(err)?;
                parse_pub(&b).map_err(|e| bad(&e))
            }
            Ecosystem::SwiftUrl | Ecosystem::CocoaPods => {
                Err("the ecosystem has no registry API".into())
            }
        }
    }
}
