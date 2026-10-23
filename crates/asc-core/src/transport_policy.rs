//! The transport policy (U89): asc talks https or ssh, and nothing else crosses a network.
//!
//! One function decides whether a URL may be used: [`classify`]. `https://`, `ssh://` and
//! scp-style `[user@]host:path` pass. `http://` is refused with an error that names the origin
//! (scheme, host and port; never the userinfo, path or query), unless the opt-in is set
//! (`asc.allowLoopbackHttp=true` in the user's config, or `ASC_ALLOW_LOOPBACK_HTTP=1`) and the host
//! is loopback (127.0.0.0/8, `::1`, `localhost`). `git://`, `ftp://` and every other scheme are
//! refused. Plain local paths and `file://` are not network transports and classify as
//! [`Transport::Local`]; callers that need a network URL use [`require_network`].
//!
//! The opt-in is off by default and is a process setting: the binary copies the config key into
//! [`set_config_allow_loopback_http`] once its config is loaded, and tests that serve plain http
//! on the loopback interface call the same function. Code that must not depend on it takes a
//! [`Policy`] value.

use std::sync::atomic::{AtomicBool, Ordering};

use url::{Host, Url};

/// The variable that allows plain http to the local machine (tests and local development).
pub const LOOPBACK_ENV: &str = "ASC_ALLOW_LOOPBACK_HTTP";
/// The config key with the same meaning.
pub const LOOPBACK_CONFIG_KEY: &str = "asc.allowLoopbackHttp";

static CONFIG_OPT_IN: AtomicBool = AtomicBool::new(false);

/// Records the value of [`LOOPBACK_CONFIG_KEY`] for this process.
pub fn set_config_allow_loopback_http(on: bool) {
    CONFIG_OPT_IN.store(on, Ordering::Relaxed);
}

/// Whether [`LOOPBACK_ENV`] is `1` or `true`.
pub fn env_allows_loopback_http() -> bool {
    std::env::var(LOOPBACK_ENV).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Whether plain http to a loopback host is allowed in this process (environment or config).
pub fn allow_loopback_http() -> bool {
    env_allows_loopback_http() || CONFIG_OPT_IN.load(Ordering::Relaxed)
}

/// What a check may allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    allow_loopback_http: bool,
}

impl Policy {
    /// https and ssh only.
    pub const STRICT: Self = Self {
        allow_loopback_http: false,
    };

    /// Also plain http to a loopback host.
    pub const LOOPBACK_HTTP: Self = Self {
        allow_loopback_http: true,
    };

    /// The policy of this process: strict unless the opt-in is set.
    pub fn current() -> Self {
        Self {
            allow_loopback_http: allow_loopback_http(),
        }
    }

    pub fn allows_loopback_http(self) -> bool {
        self.allow_loopback_http
    }
}

/// What a permitted URL uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Https,
    /// `ssh://`, `git+ssh://` or scp-style.
    Ssh,
    /// Plain http to a loopback host, under the opt-in.
    LoopbackHttp,
    /// A path or `file://`: no network.
    Local,
}

/// Why a URL may not be used. No variant carries credentials, a path or a query.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error(
        "refusing plain http to {origin}: credentials and data would cross the network unprotected; use https or ssh"
    )]
    PlainHttp { origin: String },
    #[error(
        "refusing plain http to {origin}: asc talks https or ssh (to allow http to the local machine for tests and local development, set {LOOPBACK_CONFIG_KEY}=true or {LOOPBACK_ENV}=1)"
    )]
    LoopbackNotEnabled { origin: String },
    #[error("refusing {scheme}:// to {origin}: asc talks https or ssh")]
    Unsupported { scheme: String, origin: String },
    #[error("refusing a local path where a network URL is required")]
    NotNetwork,
    #[error("not a usable url")]
    Malformed,
}

/// Whether `host` (a name, an IP literal, bracketed or not) is the local machine. A DNS name
/// other than `localhost` never is: `127.example.com` resolves wherever its owner says.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn url_host_is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// `scheme://host[:port]`, the only part of a URL an error may name.
fn origin(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(p) => format!("{}://{host}:{p}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

/// [`classify`] for a parsed URL. `s3://` is judged by its `endpoint` option (an s3 bucket
/// without one is Amazon over https).
pub fn classify_url(url: &Url, policy: Policy) -> Result<Transport, TransportError> {
    let has_host = url.host_str().is_some_and(|h| !h.is_empty());
    match url.scheme() {
        "https" if has_host => Ok(Transport::Https),
        "ssh" if has_host => Ok(Transport::Ssh),
        "http" if has_host => {
            if !url_host_is_loopback(url) {
                Err(TransportError::PlainHttp {
                    origin: origin(url),
                })
            } else if !policy.allows_loopback_http() {
                Err(TransportError::LoopbackNotEnabled {
                    origin: origin(url),
                })
            } else {
                Ok(Transport::LoopbackHttp)
            }
        }
        "https" | "ssh" | "http" => Err(TransportError::Malformed),
        "file" => Ok(Transport::Local),
        "s3" => match url.query_pairs().find(|(k, _)| k == "endpoint") {
            Some((_, endpoint)) if !endpoint.is_empty() => {
                let endpoint = Url::parse(&endpoint).map_err(|_| TransportError::Malformed)?;
                match endpoint.scheme() {
                    "http" | "https" => classify_url(&endpoint, policy),
                    _ => Err(TransportError::Malformed),
                }
            }
            _ => Ok(Transport::Https),
        },
        other => Err(TransportError::Unsupported {
            scheme: other.to_owned(),
            origin: origin(url),
        }),
    }
}

/// Drops a leading `asc+` or `git+` (as `RemoteSpec` URLs carry them).
fn strip_wrappers(text: &str) -> &str {
    let mut rest = text;
    for prefix in ["asc+", "git+"] {
        if let Some(head) = rest.get(..prefix.len())
            && head.eq_ignore_ascii_case(prefix)
        {
            rest = &rest[prefix.len()..];
        }
    }
    rest
}

/// `[user@]host:path`: a colon before any slash, and more than a drive letter before it.
fn is_scp_like(text: &str) -> bool {
    let Some(colon) = text.find(':') else {
        return false;
    };
    let head = &text[..colon];
    colon > 1 && !head.contains(['/', '\\']) && !head.ends_with('@') && !head.starts_with('@')
}

/// Decides whether `url`, as stored or typed, may be used.
pub fn classify(url: &str, policy: Policy) -> Result<Transport, TransportError> {
    if url.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(TransportError::Malformed);
    }
    let text = strip_wrappers(url);
    if text.contains("://") {
        let parsed = Url::parse(text).map_err(|_| TransportError::Malformed)?;
        return classify_url(&parsed, policy);
    }
    if is_scp_like(text) {
        return Ok(Transport::Ssh);
    }
    Ok(Transport::Local)
}

/// [`classify`] under the process policy.
pub fn check(url: &str) -> Result<Transport, TransportError> {
    classify(url, Policy::current())
}

/// [`classify`] for a place that can only reach a network server: a local path is refused too.
pub fn require_network(url: &str, policy: Policy) -> Result<Transport, TransportError> {
    match classify(url, policy)? {
        Transport::Local => Err(TransportError::NotNetwork),
        t => Ok(t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRICT: Policy = Policy::STRICT;
    const DEV: Policy = Policy::LOOPBACK_HTTP;

    #[test]
    fn https_ssh_and_scp_style_pass() {
        for u in [
            "https://example.com/o/r.git",
            "HTTPS://Example.com:8443/o/r",
            "asc+https://example.com",
            "git+https://example.com/o/r",
            "ssh://git@example.com/o/r.git",
            "git+ssh://example.com/o/r",
            "git@example.com:o/r.git",
            "example.com:o/r.git",
            "s3://bucket/prefix",
            "s3://bucket/prefix?endpoint=https://minio.example:9000",
        ] {
            assert!(classify(u, STRICT).is_ok(), "{u}");
        }
        assert_eq!(classify("https://h/x", STRICT), Ok(Transport::Https));
        assert_eq!(classify("git@h:o/r", STRICT), Ok(Transport::Ssh));
        assert_eq!(classify("ssh://h/o/r", STRICT), Ok(Transport::Ssh));
    }

    #[test]
    fn plain_http_is_refused_everywhere_by_default() {
        for u in [
            "http://example.com/o/r",
            "http://192.0.2.7:8080",
            "asc+http://example.com",
            "git+http://example.com/o/r",
            "http://127.0.0.1:9",
            "http://localhost/",
            "http://[::1]:7890/",
            "s3://b/p?endpoint=http://minio.example:9000",
            "s3://b/p?endpoint=http://127.0.0.1:9000",
        ] {
            assert!(classify(u, STRICT).is_err(), "{u}");
        }
    }

    #[test]
    fn the_opt_in_allows_loopback_only() {
        for u in [
            "http://127.0.0.1:9",
            "http://127.9.9.9/",
            "http://localhost:7890",
            "http://LOCALHOST",
            "http://[::1]:7890/x",
            "asc+http://127.0.0.1:1",
            "s3://b/p?endpoint=http://127.0.0.1:9000",
        ] {
            assert_eq!(classify(u, DEV), Ok(Transport::LoopbackHttp), "{u}");
        }
        for u in [
            "http://example.com",
            "http://127.example.com",
            "http://localhost.evil",
            "http://localhost.",
            "http://192.0.2.7",
            "http://0.0.0.0:80",
            "http://[::ffff:192.0.2.7]",
            "http://localhost@example.com/",
            "http://127.0.0.1@example.com/",
            "http://example.com#@127.0.0.1",
            "s3://b/p?endpoint=http://example.com",
        ] {
            assert!(
                matches!(classify(u, DEV), Err(TransportError::PlainHttp { .. })),
                "{u}"
            );
        }
    }

    #[test]
    fn other_schemes_are_refused() {
        for u in [
            "git://example.com/o/r",
            "ftp://example.com/x",
            "ws://127.0.0.1/",
            "javascript://x/%0a1",
        ] {
            assert!(
                matches!(classify(u, DEV), Err(TransportError::Unsupported { .. })),
                "{u}"
            );
        }
        assert_eq!(classify("https://", DEV), Err(TransportError::Malformed));
        assert_eq!(classify("", DEV), Err(TransportError::Malformed));
        assert_eq!(
            classify("https://h/a b", DEV),
            Err(TransportError::Malformed)
        );
    }

    #[test]
    fn local_paths_are_not_network_transports() {
        for u in [
            "/srv/repo",
            "C:\\repos\\r",
            "C:/repos/r",
            "./r",
            "../r",
            "file:///srv/r",
        ] {
            assert_eq!(classify(u, STRICT), Ok(Transport::Local), "{u}");
            assert_eq!(
                require_network(u, STRICT),
                Err(TransportError::NotNetwork),
                "{u}"
            );
        }
        assert!(require_network("https://h", STRICT).is_ok());
    }

    #[test]
    fn errors_name_the_origin_and_never_the_credentials() {
        let url = "http://user:hunter2@example.com:8080/secret/path?token=abc#frag";
        for e in [
            classify(url, STRICT).unwrap_err(),
            classify("git://user:hunter2@example.com/secret", STRICT).unwrap_err(),
            classify(
                "http://user:hunter2@127.0.0.1:9/secret/path?token=abc",
                STRICT,
            )
            .unwrap_err(),
        ] {
            let text = e.to_string();
            for secret in ["hunter2", "user", "secret", "token", "abc", "frag"] {
                assert!(!text.contains(secret), "{text}");
            }
            assert!(
                text.contains("example.com") || text.contains("127.0.0.1"),
                "{text}"
            );
        }
        assert!(
            classify(url, STRICT)
                .unwrap_err()
                .to_string()
                .contains("http://example.com:8080")
        );
        // A URL that does not parse is not echoed at all.
        let e = classify("https://user:hunter2@[bad", STRICT).unwrap_err();
        assert!(!e.to_string().contains("hunter2"));
    }

    #[test]
    fn the_process_policy_follows_the_config_opt_in() {
        // The environment variable is read as-is; this test only drives the config flag.
        if env_allows_loopback_http() {
            return;
        }
        set_config_allow_loopback_http(false);
        assert!(check("http://127.0.0.1:1").is_err());
        set_config_allow_loopback_http(true);
        assert_eq!(check("http://127.0.0.1:1"), Ok(Transport::LoopbackHttp));
        assert!(check("http://example.com").is_err());
        set_config_allow_loopback_http(false);
        assert!(check("http://127.0.0.1:1").is_err());
    }

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "LocalHost",
            "127.0.0.1",
            "127.255.0.1",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "127.example.com",
            "localhost.evil",
            "0.0.0.0",
            "10.0.0.1",
            "example.com",
            "",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }
}
