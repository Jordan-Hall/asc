//! `asc ci warm` (SPEC §12.3 "Host cache warmer"): the parts that need no repository. The
//! validated plan, the 0-30 s jitter, the seed URL policy, and the `asc_warm_*` metrics written
//! as a Prometheus textfile. The CLI drives clone and fetch of trunk into the host cache.
//! Only this trusted process writes the host cache; PR jobs mount it read-only.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longest jitter, in seconds (SPEC §12.3).
pub const MAX_JITTER_SECS: u32 = 30;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WarmError {
    #[error("{0}")]
    Input(String),
}

fn bad<T>(m: &str) -> Result<T, WarmError> {
    Err(WarmError::Input(m.to_owned()))
}

/// Raw inputs of the command.
#[derive(Clone, Debug)]
pub struct Options {
    /// `owner/repo`.
    pub repo: String,
    /// The branch to keep warm (`master`).
    pub reference: String,
    /// The asc server's public URL (`https`, or loopback `http`).
    pub server: String,
    /// The host cache directory the warmer owns and writes (`/var/cache/asc/cache`).
    pub host_cache: PathBuf,
    /// Where the warmer keeps its own clone and `ASC_HOME` (default: next to the host cache).
    pub state_dir: Option<PathBuf>,
    pub seed_from: Option<String>,
    pub jitter_secs: u32,
    pub metrics_file: Option<PathBuf>,
}

/// A validated plan.
#[derive(Clone, Debug)]
pub struct Plan {
    pub repo: String,
    pub reference: String,
    pub url: String,
    pub host_cache: PathBuf,
    pub asc_home: PathBuf,
    pub work: PathBuf,
    pub seed: Option<Seed>,
    pub jitter: u32,
    pub metrics_file: Option<PathBuf>,
}

/// An accepted seed source (SPEC §12.3: an explicit seed may be `https`, a `file:` URL without
/// a host, or a local path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seed {
    Https(String),
    Local(PathBuf),
}

impl Seed {
    /// The text handed to `asc clone --seed`.
    pub fn arg(&self) -> String {
        match self {
            Self::Https(u) => u.clone(),
            Self::Local(p) => p.display().to_string(),
        }
    }
}

fn no_ctl(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

fn slug_ok(s: &str) -> bool {
    let mut parts = s.split('/');
    let part = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    };
    part(parts.next()) && part(parts.next()) && parts.next().is_none()
}

fn loopback_http(lower: &str) -> bool {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            lower
                .strip_prefix(p)
                .is_some_and(|r| r.is_empty() || r.starts_with(':') || r.starts_with('/'))
        })
}

/// The seed URL policy for a user-named seed.
pub fn parse_seed(text: &str) -> Result<Seed, WarmError> {
    if text.is_empty() || !no_ctl(text) || text.len() > 4096 {
        return bad("--seed-from is not a usable URL or path");
    }
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(Seed::Https(text.to_owned()));
    }
    if lower.starts_with("http://") {
        return bad("--seed-from: plain http is refused");
    }
    // A network share sends credentials on Windows.
    if text.starts_with("\\\\") || text.starts_with("//") {
        return bad("--seed-from: a network share path is refused");
    }
    if let Some(rest) = lower.strip_prefix("file:") {
        // `file:///path` and `file:/path`; `file://host/path` has a host.
        let path = if let Some(r) = rest.strip_prefix("//") {
            if !r.starts_with('/') {
                return bad("--seed-from: a file: URL with a host is refused");
            }
            &text["file://".len()..]
        } else {
            &text["file:".len()..]
        };
        if path.starts_with("//") || path.starts_with("\\\\") {
            return bad("--seed-from: a network share path is refused");
        }
        return Ok(Seed::Local(PathBuf::from(path)));
    }
    if text.contains("://") {
        return bad("--seed-from: only https, file: and local paths are accepted");
    }
    Ok(Seed::Local(PathBuf::from(text)))
}

impl Plan {
    pub fn new(o: Options) -> Result<Self, WarmError> {
        if !slug_ok(&o.repo) {
            return bad("--repo must be <owner>/<repo>");
        }
        let r = &o.reference;
        if r.is_empty()
            || r.starts_with('-')
            || r.len() > 255
            || !no_ctl(r)
            || r.contains(' ')
            || r.contains("..")
        {
            return bad("--ref is not a usable branch name");
        }
        let lower = o.server.to_ascii_lowercase();
        if !(lower.starts_with("https://") || loopback_http(&lower)) || !no_ctl(&o.server) {
            return bad("the server URL must be https (plain http only to loopback)");
        }
        if o.host_cache.as_os_str().is_empty() {
            return bad("--host-cache is empty");
        }
        if o.jitter_secs > MAX_JITTER_SECS {
            return bad("--jitter is at most 30 seconds");
        }
        let seed = o.seed_from.as_deref().map(parse_seed).transpose()?;
        let state = o
            .state_dir
            .unwrap_or_else(|| o.host_cache.parent().unwrap_or(&o.host_cache).join("warm"));
        Ok(Self {
            url: format!("{}/{}", o.server.trim_end_matches('/'), o.repo),
            repo: o.repo,
            reference: o.reference,
            host_cache: o.host_cache,
            asc_home: state.join("home"),
            work: state.join("work"),
            seed,
            jitter: o.jitter_secs,
            metrics_file: o.metrics_file,
        })
    }

    /// The tier stack of the warmer: the host cache is its writable cache directory.
    pub fn config_env(&self) -> Vec<(String, String)> {
        let dir = self.host_cache.display().to_string().replace('\\', "/");
        vec![
            ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_KEY_0".to_owned(), "asc.cache.dir".to_owned()),
            ("GIT_CONFIG_VALUE_0".to_owned(), dir),
        ]
    }
}

/// A delay in `0..=max_secs` seconds from `entropy`.
pub fn jitter(max_secs: u32, entropy: u128) -> Duration {
    let max = u128::from(max_secs.min(MAX_JITTER_SECS)) * 1000;
    Duration::from_millis(u64::try_from(entropy % (max + 1)).unwrap_or(0))
}

/// The outcome of one run, for the metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    /// Changesets fetched by this run.
    pub objects: u64,
    /// Age of the trunk tip in the cache, when known.
    pub lag_secs: Option<u64>,
}

/// The value of an unlabelled counter in a previous textfile.
fn previous(text: &str, name: &str) -> u64 {
    text.lines()
        .filter_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .find_map(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// The Prometheus text for the node exporter's textfile collector. Counters continue from
/// `prev`, the file the last run wrote.
pub fn render_metrics(prev: Option<&str>, repo: &str, o: &Outcome) -> String {
    let prev = prev.unwrap_or("");
    let objects = previous(prev, "asc_warm_objects_total").saturating_add(o.objects);
    let failures = previous(prev, "asc_warm_failures_total").saturating_add(u64::from(!o.ok));
    let mut s = String::new();
    s.push_str("# TYPE asc_warm_objects_total counter\n");
    s.push_str(&format!("asc_warm_objects_total {objects}\n"));
    s.push_str("# TYPE asc_warm_failures_total counter\n");
    s.push_str(&format!("asc_warm_failures_total {failures}\n"));
    if let Some(lag) = o.lag_secs {
        s.push_str("# TYPE asc_warm_lag_seconds gauge\n");
        s.push_str(&format!("asc_warm_lag_seconds {lag}\n"));
    }
    // The repo is a label-safe slug (checked by the plan); written as an info line.
    s.push_str("# TYPE asc_warm_info gauge\n");
    let repo: String = repo
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        .collect();
    s.push_str(&format!("asc_warm_info{{repo=\"{repo}\"}} 1\n"));
    s
}

/// Writes the textfile atomically (temp file in the same directory, then rename), continuing
/// the counters of the file already there.
pub fn write_metrics(path: &Path, repo: &str, o: &Outcome) -> std::io::Result<()> {
    let prev = std::fs::read_to_string(path).ok();
    let text = render_metrics(prev.as_deref(), repo, o);
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("warm"),
        std::process::id()
    ));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            repo: "asc/dds".into(),
            reference: "master".into(),
            server: "https://git.example.com".into(),
            host_cache: PathBuf::from("/var/cache/asc/cache"),
            state_dir: None,
            seed_from: None,
            jitter_secs: 30,
            metrics_file: None,
        }
    }

    #[test]
    fn plan_derives_paths_and_url() {
        let p = Plan::new(opts()).unwrap();
        assert_eq!(p.url, "https://git.example.com/asc/dds");
        assert_eq!(p.asc_home, PathBuf::from("/var/cache/asc/warm/home"));
        let env = p.config_env();
        assert!(env.contains(&("GIT_CONFIG_VALUE_0".into(), "/var/cache/asc/cache".into())));
    }

    #[test]
    fn plan_rejects_hostile_input() {
        let with = |f: fn(&mut Options)| {
            let mut o = opts();
            f(&mut o);
            Plan::new(o).is_err()
        };
        assert!(with(|o| o.repo = "a/b/c".into()));
        assert!(with(|o| o.repo = "../x".into()));
        assert!(with(|o| o.reference = "--upload-pack=x".into()));
        assert!(with(|o| o.reference = "a\nb".into()));
        assert!(with(|o| o.server = "http://example.com".into()));
        assert!(with(|o| o.server = "ftp://example.com".into()));
        assert!(with(|o| o.jitter_secs = 31));
        assert!(!with(|o| o.server = "http://127.0.0.1:8080".into()));
        assert!(with(|o| o.server = "http://127.0.0.1.evil.com".into()));
    }

    #[test]
    fn seed_policy() {
        assert!(matches!(
            parse_seed("https://s/x.tar.zst"),
            Ok(Seed::Https(_))
        ));
        assert_eq!(
            parse_seed("file:///var/seed.tar.zst"),
            Ok(Seed::Local(PathBuf::from("/var/seed.tar.zst")))
        );
        assert_eq!(
            parse_seed("/var/seed"),
            Ok(Seed::Local(PathBuf::from("/var/seed")))
        );
        for bad in [
            "http://s/x",
            "HTTP://s/x",
            "file://host/x",
            "file:////share/x",
            "\\\\server\\share\\x",
            "//server/share",
            "ftp://s/x",
            "",
            "a\nb",
        ] {
            assert!(parse_seed(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn jitter_is_bounded() {
        assert_eq!(jitter(30, 0), Duration::ZERO);
        assert_eq!(jitter(30, 30_000), Duration::from_secs(30));
        assert!(jitter(30, u128::MAX) <= Duration::from_secs(30));
        assert!(jitter(999, u128::MAX) <= Duration::from_secs(30));
        assert_eq!(jitter(0, 12345), Duration::ZERO);
    }

    #[test]
    fn metrics_counters_continue_across_runs() {
        let a = render_metrics(
            None,
            "asc/dds",
            &Outcome {
                ok: true,
                objects: 5,
                lag_secs: Some(12),
            },
        );
        assert!(a.contains("asc_warm_objects_total 5\n"));
        assert!(a.contains("asc_warm_failures_total 0\n"));
        assert!(a.contains("asc_warm_lag_seconds 12\n"));
        let b = render_metrics(
            Some(&a),
            "asc/dds",
            &Outcome {
                ok: false,
                objects: 0,
                lag_secs: None,
            },
        );
        assert!(b.contains("asc_warm_objects_total 5\n"));
        assert!(b.contains("asc_warm_failures_total 1\n"));
        assert!(!b.contains("asc_warm_lag_seconds"));
    }

    #[test]
    fn metrics_file_is_replaced_atomically() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("asc_warm.prom");
        let o = Outcome {
            ok: true,
            objects: 2,
            lag_secs: Some(1),
        };
        write_metrics(&f, "a/b", &o).unwrap();
        write_metrics(&f, "a/b", &o).unwrap();
        let t = std::fs::read_to_string(&f).unwrap();
        assert!(t.contains("asc_warm_objects_total 4\n"), "{t}");
        let leftovers = std::fs::read_dir(d.path()).unwrap().count();
        assert_eq!(leftovers, 1);
    }
}
