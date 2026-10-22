//! Shared `--seed <url>` handling for `clone`, `ci warm --seed-from` and a cold daemon
//! (SPEC §12.3; PLAN U72). The archive is untrusted; [`asc_store::seed`] validates it.

use std::path::Path;

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
    match asc_store::seed::unpack_str(&c.url, cache, Some(repo)).await {
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
        assert_eq!(o.choose(d).unwrap().explicit, false);
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
}
