//! Shared `--seed <url>` handling for `clone`, `ci warm --seed-from` and a cold daemon
//! (SPEC §12.3; PLAN U72). The archive is untrusted; [`asc_store::seed`] validates it. A URL
//! from the discovery document may only be `https` (see [`Origin`]).

use std::path::Path;

use asc_store::seed::Origin;

use crate::cli::common::{CliError, Result};

/// The `--seed <url>` flag of `asc clone`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct SeedOpts {
    /// Unpack a seed snapshot into the cache before the first fetch (default: the discovery
    /// document's `seeds`). `none` turns seeding off.
    #[arg(long, value_name = "url")]
    pub seed: Option<String>,
}

/// Where a seed comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub url: String,
    /// Named on the command line, so a failure is fatal rather than a warning.
    pub explicit: bool,
}

impl SeedOpts {
    /// The explicit flag wins over the discovery document; `none` disables both.
    pub fn choose(&self, discovered: Option<&str>) -> Option<Choice> {
        choose(self.seed.as_deref(), discovered)
    }
}

/// See [`SeedOpts::choose`]; `ci warm --seed-from` passes its value as `explicit`.
pub fn choose(explicit: Option<&str>, discovered: Option<&str>) -> Option<Choice> {
    match (explicit, discovered) {
        (Some("none"), _) => None,
        (Some(u), _) => Some(Choice {
            url: u.to_owned(),
            explicit: true,
        }),
        (None, Some(u)) => Some(Choice {
            url: u.to_owned(),
            explicit: false,
        }),
        (None, None) => None,
    }
}

/// Unpacks the chosen seed into `cache` (a missing choice does nothing). A seed only saves
/// network, so a failure of a discovered seed warns and the fetch carries on with a full
/// download; an explicit seed fails the command. Returns the files installed.
pub async fn apply(choice: Option<&Choice>, cache: &Path, repo: &str) -> Result<u64> {
    let Some(c) = choice else {
        return Ok(0);
    };
    // A discovered URL comes from the server, which may be hostile: https only.
    let origin = if c.explicit {
        Origin::Explicit
    } else {
        Origin::Discovered
    };
    match asc_store::seed::unpack_str(&c.url, cache, Some(repo), origin).await {
        Ok(u) => Ok(u.installed),
        Err(e) if c.explicit => Err(CliError::Fatal(format!("seed {}: {e}", c.url))),
        Err(e) => {
            eprintln!("warning: ignoring seed {}: {e}", c.url);
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_beats_discovery_and_none_disables() {
        let d = Some("https://seeds.example/acme/web/latest");
        let o = SeedOpts::default();
        assert!(!o.choose(d).unwrap().explicit);
        assert!(o.choose(None).is_none());
        let o = SeedOpts {
            seed: Some("file:///s.tar.zst".into()),
        };
        let c = o.choose(d).unwrap();
        assert_eq!((c.url.as_str(), c.explicit), ("file:///s.tar.zst", true));
        let o = SeedOpts {
            seed: Some("none".into()),
        };
        assert!(o.choose(d).is_none());
    }

    #[tokio::test]
    async fn bad_seed_is_fatal_only_when_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = |explicit| Choice {
            url: "ftp://nowhere/x".into(),
            explicit,
        };
        assert!(apply(Some(&bad(true)), tmp.path(), "a/b").await.is_err());
        assert_eq!(
            apply(Some(&bad(false)), tmp.path(), "a/b").await.unwrap(),
            0
        );
        assert_eq!(apply(None, tmp.path(), "a/b").await.unwrap(), 0);
    }

    use asc_store::seed::{SeedManifest, pack};

    /// A valid one-file seed for `a/b`.
    fn seed_file(dir: &Path) -> std::path::PathBuf {
        let data = b"alpha";
        let key = asc_store::key::ObjKey::File(asc_core::id::FileId::of(data));
        let rel = key.path();
        let src = dir.join("src");
        std::fs::create_dir_all(src.join(&rel).parent().unwrap()).unwrap();
        std::fs::write(src.join(&rel), data).unwrap();
        let m = SeedManifest::new("a/b", &"ab".repeat(32), 1);
        let file = dir.join("s.tar.zst");
        let mut out = Vec::new();
        pack(&src, &m, &[rel], &mut out).unwrap();
        std::fs::write(&file, out).unwrap();
        file
    }

    fn file_url(file: &Path) -> String {
        let p = file.to_string_lossy().replace('\\', "/");
        format!("file:///{}", p.trim_start_matches('/'))
    }

    fn cache_is_empty(cache: &Path) -> bool {
        !cache.exists() || std::fs::read_dir(cache).unwrap().next().is_none()
    }

    #[tokio::test]
    async fn discovered_seed_must_be_https() {
        let tmp = tempfile::tempdir().unwrap();
        let file = seed_file(tmp.path());
        let furl = file_url(&file);
        for url in [furl, "http://seeds.example/s.tar.zst".to_owned()] {
            let cache = tmp.path().join("cache");
            let c = choose(None, Some(&url)).unwrap();
            assert!(!c.explicit);
            // Not fatal (a seed only saves network), but nothing is read or installed.
            assert_eq!(apply(Some(&c), &cache, "a/b").await.unwrap(), 0, "{url}");
            assert!(cache_is_empty(&cache), "{url}");
        }
    }

    #[tokio::test]
    async fn explicit_seed_may_be_a_local_file_but_not_a_share() {
        let tmp = tempfile::tempdir().unwrap();
        let file = seed_file(tmp.path());
        let furl = file_url(&file);
        let cache = tmp.path().join("cache");
        let c = choose(Some(&furl), None).unwrap();
        assert_eq!(apply(Some(&c), &cache, "a/b").await.unwrap(), 1);

        let cache = tmp.path().join("cache2");
        for url in [
            r"\\127.0.0.1\seed-test\s.tar.zst",
            "//127.0.0.1/seed-test/s.tar.zst",
            "file://127.0.0.1/seed-test/s.tar.zst",
        ] {
            let c = choose(Some(url), None).unwrap();
            let e = apply(Some(&c), &cache, "a/b").await.unwrap_err();
            assert!(e.to_string().contains("seed refused"), "{url}: {e}");
        }
        assert!(cache_is_empty(&cache));
    }
}
