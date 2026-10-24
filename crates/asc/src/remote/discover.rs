//! Discovery of an asc server behind an `https://` remote (SPEC §6.2).
//!
//! `GET <origin>/.well-known/asc` returns the server's address, the optional `objects` tier list
//! and `seeds` URL, the per-repository `mode` and the oldest client the server accepts. The
//! answer is untrusted: the body is bounded, every URL is parsed and checked, and a document
//! that does not validate counts as "no asc server here" (git-remote mode), never as a partial
//! answer. Discovery runs only over TLS (or plain http to the local machine, the rule of
//! `server::channel`): the document names the endpoints that receive the user's credentials, so
//! a document fetched over plain http to another host could be forged. Such a remote is a plain
//! git remote, and nothing fetched without TLS is cached. The endpoints a document names must
//! follow the same rule. The result is cached in `state.db` for [`TTL`] (a 404 for [`MISS_TTL`]); a request
//! that fails to reach the host is not cached and falls back to git, so git reports the
//! network failure itself.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_core::transport_policy::Policy;
use asc_state::StateDb;
use url::Url;

/// How long a discovery document is trusted (SPEC §6.2: one hour).
pub const TTL: Duration = Duration::from_secs(3600);
/// How long "this host has no asc server" is remembered.
pub const MISS_TTL: Duration = Duration::from_secs(60);
/// Largest document read.
pub const MAX_BODY: usize = 64 * 1024;
/// Most `objects` tier URLs accepted.
pub const MAX_OBJECTS: usize = 8;
/// Longest URL accepted in a document.
const MAX_URL: usize = 2048;
/// Most `mode` entries kept.
const MAX_MODES: usize = 10_000;

/// A validated `/.well-known/asc` document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    /// The gRPC endpoint: scheme, host and port only.
    pub server: String,
    /// Object tier URLs, in order (may be empty).
    pub objects: Vec<String>,
    /// The seed download base, when the server has one.
    pub seeds: Option<String>,
    /// `<owner>/<repo>` to `mirror` or `primary`.
    pub mode: BTreeMap<String, String>,
    /// The oldest client version the server accepts.
    pub min_client: Option<String>,
}

impl Document {
    /// Whether `repo` (`owner/name`) is served in mirror mode (pushes go to the forge).
    pub fn is_mirror(&self, repo: &str) -> bool {
        self.mode.get(repo).is_some_and(|m| m == "mirror")
    }
}

/// Why a document was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoverError {
    #[error("the discovery document is larger than {MAX_BODY} bytes")]
    TooLarge,
    #[error("the discovery document is not valid JSON")]
    NotJson,
    #[error("the discovery document has no usable '{0}'")]
    Field(&'static str),
    /// The remote's own transport is not allowed (plain http); nothing was requested.
    #[error("{0}")]
    Refused(String),
}

/// What a document may name, from how it was fetched.
#[derive(Clone, Copy, Debug)]
struct Rules {
    /// Whether the document itself came over TLS: it may then name https endpoints only.
    secure: bool,
    /// The transport policy: plain http to the local machine only behind the opt-in.
    policy: Policy,
}

impl Rules {
    fn of(origin: &Url, policy: Policy) -> Self {
        Self {
            secure: origin.scheme() == "https",
            policy,
        }
    }
}

/// A URL the document names, reduced to what a client may use: https, or http to the local
/// machine when allowed; a host, no userinfo, no query or fragment. Credentials go to these
/// endpoints, so nothing else passes.
fn clean_url(text: &str, keep_path: bool, rules: Rules) -> Option<String> {
    if text.len() > MAX_URL || text.chars().any(char::is_control) {
        return None;
    }
    let mut u = Url::parse(text).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return None;
    }
    if super::server::transport_refusal(&u, rules.policy).is_some() {
        return None;
    }
    if rules.secure && u.scheme() != "https" {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    u.set_query(None);
    u.set_fragment(None);
    if !keep_path {
        u.set_path("");
    }
    Some(u.as_str().trim_end_matches('/').to_owned())
}

/// Parses and validates a document that came from `origin`.
pub fn parse_from(body: &[u8], origin: &Url) -> Result<Document, DiscoverError> {
    parse_as(body, Rules::of(origin, Policy::current()))
}

fn parse_as(body: &[u8], rules: Rules) -> Result<Document, DiscoverError> {
    if body.len() > MAX_BODY {
        return Err(DiscoverError::TooLarge);
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| DiscoverError::NotJson)?;
    let obj = v.as_object().ok_or(DiscoverError::NotJson)?;
    let server = obj
        .get("server")
        .and_then(|s| s.as_str())
        .and_then(|s| clean_url(s, false, rules))
        .ok_or(DiscoverError::Field("server"))?;
    let mut objects = Vec::new();
    match obj.get("objects") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(items)) => {
            if items.len() > MAX_OBJECTS {
                return Err(DiscoverError::Field("objects"));
            }
            for item in items {
                let url = item
                    .as_str()
                    .and_then(|s| clean_url(s, false, rules))
                    .ok_or(DiscoverError::Field("objects"))?;
                objects.push(url);
            }
        }
        Some(_) => return Err(DiscoverError::Field("objects")),
    }
    let seeds = match obj.get("seeds") {
        None | Some(serde_json::Value::Null) => None,
        Some(s) => Some(
            s.as_str()
                .and_then(|s| clean_url(s, true, rules))
                .ok_or(DiscoverError::Field("seeds"))?,
        ),
    };
    let mut mode = BTreeMap::new();
    if let Some(m) = obj.get("mode") {
        let map = m.as_object().ok_or(DiscoverError::Field("mode"))?;
        for (repo, value) in map.iter().take(MAX_MODES) {
            if let Some(word) = value.as_str()
                && matches!(word, "mirror" | "primary")
            {
                mode.insert(repo.clone(), word.to_owned());
            }
        }
    }
    let min_client = match obj.get("min_client") {
        None | Some(serde_json::Value::Null) => None,
        Some(s) => {
            let s = s.as_str().ok_or(DiscoverError::Field("min_client"))?;
            if s.len() > 64 || !s.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return Err(DiscoverError::Field("min_client"));
            }
            Some(s.to_owned())
        }
    };
    Ok(Document {
        server,
        objects,
        seeds,
        mode,
        min_client,
    })
}

/// Whether `ours` (`x.y.z`) is older than `min`.
pub fn older_than(ours: &str, min: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(ours), parts(min));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

async fn ensure_table(db: &StateDb) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS discovery_cache (\
         origin TEXT PRIMARY KEY, fetched_at INTEGER NOT NULL, body TEXT NOT NULL)",
    )
    .execute(db.pool())
    .await
    .map(|_| ())
}

/// A cached answer: `Some(None)` is a remembered miss, `None` is nothing usable.
async fn cached(db: &StateDb, origin: &str) -> Option<Option<Document>> {
    ensure_table(db).await.ok()?;
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT fetched_at, body FROM discovery_cache WHERE origin = $1")
            .bind(origin.to_owned())
            .fetch_optional(db.pool())
            .await
            .ok()?;
    let (at, body) = row?;
    let age = now().saturating_sub(at);
    if age < 0 {
        return None;
    }
    let age = Duration::from_secs(age.unsigned_abs());
    if body.is_empty() {
        return (age < MISS_TTL).then_some(None);
    }
    if age >= TTL {
        return None;
    }
    let origin = Url::parse(origin).ok()?;
    parse_from(body.as_bytes(), &origin).ok().map(Some)
}

async fn store(db: &StateDb, origin: &str, body: &str) {
    if ensure_table(db).await.is_err() {
        return;
    }
    let _ = sqlx::query(
        "INSERT INTO discovery_cache (origin, fetched_at, body) VALUES ($1, $2, $3) \
         ON CONFLICT (origin) DO UPDATE SET fetched_at = excluded.fetched_at, body = excluded.body",
    )
    .bind(origin.to_owned())
    .bind(now())
    .bind(body.to_owned())
    .execute(db.pool())
    .await;
}

/// Refuses a document whose `server` (or, unless `objects_configured`, `objects`) is on another
/// origin than the remote's own: the user's credential goes to those endpoints, and a document
/// is only the remote host's word. The user trusts another origin by configuring it
/// (`remote.<name>.ascServer`, `ascObjects`). The error names both origins.
pub fn check_origins(remote: &Url, doc: &Document, objects_configured: bool) -> Result<(), String> {
    let home = origin_of(remote).unwrap_or_default();
    let named = std::iter::once(("server", &doc.server)).chain(
        doc.objects
            .iter()
            .filter(|_| !objects_configured)
            .map(|o| ("objects", o)),
    );
    for (what, endpoint) in named {
        let same = Url::parse(endpoint).is_ok_and(|e| crate::credential::same_origin(remote, &e));
        if !same {
            return Err(format!(
                "asc: {home} names {what} {endpoint}, which is a different origin; no credential is sent there. To trust it, set remote.<name>.ascServer (or ascObjects) to that URL"
            ));
        }
    }
    Ok(())
}

/// Reads at most [`MAX_BODY`] bytes of a response.
async fn bounded(mut response: reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > MAX_BODY {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// `GET target`: over h3 when this https origin has advertised it (U89b), else over h1 or h2.
/// The body is `None` when it is larger than [`MAX_BODY`]. A response teaches `up` the origin's
/// `Alt-Svc`, so later requests of this process to the same origin can use h3.
async fn get(
    up: &asc_rpc::h3::Upgrader,
    target: &str,
) -> Option<(reqwest::StatusCode, Option<Vec<u8>>)> {
    let user_agent = concat!("asc/", env!("CARGO_PKG_VERSION"));
    if let Ok(req) = http::Request::get(target)
        .header(http::header::USER_AGENT, user_agent)
        .body(bytes::Bytes::new())
        && let asc_rpc::h3::Attempt::Done(r) = up.send(&req, MAX_BODY).await
    {
        return Some((r.status, (!r.truncated).then(|| r.body.to_vec())));
    }
    let client = reqwest::Client::builder()
        .redirect(crate::credential::redirect_policy())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(user_agent)
        .build()
        .ok()?;
    let response = client.get(target).send().await.ok()?;
    if let Ok(served_by) = response.url().as_str().parse::<http::Uri>() {
        up.note_response(&served_by, response.headers());
    }
    let status = response.status();
    Some((status, bounded(response).await))
}

/// The origin (`scheme://host[:port]`) of `url`, as the cache key and the discovery host.
pub fn origin_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}", url.scheme()))
}

/// The discovery document of `url`'s host: cached, else fetched. `Ok(None)` means no asc server
/// (a 4xx answer, an unreadable document, or a host that could not be reached). A remote over
/// plain http is refused with an error before any request: https, or http to the local machine
/// with `ASC_ALLOW_LOOPBACK_HTTP=1`, are the only transports.
pub async fn lookup(db: &StateDb, url: &Url) -> Result<Option<Document>, DiscoverError> {
    lookup_at(db, url, None, Policy::current()).await
}

/// [`lookup`], fetching from `base` instead of `url`'s origin (a test seam; the transport rule
/// still applies to `url`).
async fn lookup_at(
    db: &StateDb,
    url: &Url,
    base: Option<&str>,
    policy: Policy,
) -> Result<Option<Document>, DiscoverError> {
    if let Some(why) = super::server::transport_refusal(url, policy) {
        return Err(DiscoverError::Refused(why));
    }
    let Some(origin) = origin_of(url) else {
        return Ok(None);
    };
    // Only an answer fetched over TLS is cached or trusted from the cache.
    let tls = url.scheme() == "https";
    if tls && let Some(hit) = cached(db, &origin).await {
        return Ok(hit);
    }
    Ok(fetch(db, url, &origin, base, tls, policy).await)
}

async fn fetch(
    db: &StateDb,
    url: &Url,
    origin: &str,
    base: Option<&str>,
    tls: bool,
    policy: Policy,
) -> Option<Document> {
    let target = format!("{}/.well-known/asc", base.unwrap_or(origin));
    let (status, body) = get(asc_rpc::h3::Upgrader::shared(), &target).await?;
    if status.is_client_error() {
        if tls {
            store(db, origin, "").await;
        }
        return None;
    }
    if !status.is_success() {
        return None;
    }
    let body = body?;
    let doc = parse_as(&body, Rules::of(url, policy)).ok()?;
    if tls {
        store(db, origin, std::str::from_utf8(&body).ok()?).await;
    }
    Some(doc)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use asc_state::home::AscHome;

    use super::*;

    fn parse(body: &[u8]) -> Result<Document, DiscoverError> {
        parse_as(
            body,
            Rules {
                secure: true,
                policy: Policy::STRICT,
            },
        )
    }

    /// A server on the loopback interface that answers every request with `body` and counts them.
    pub(super) fn mock(body: &str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let (count, body) = (hits.clone(), body.to_owned());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut buf = [0u8; 2048];
                let _ = conn.read(&mut buf);
                count.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK
content-length: {}
connection: close

{body}",
                    body.len()
                );
            }
        });
        (base, hits)
    }

    async fn state(dir: &std::path::Path) -> StateDb {
        StateDb::open(&AscHome::open(dir.join("home")).unwrap())
            .await
            .unwrap()
    }

    async fn cache_rows(db: &StateDb) -> i64 {
        ensure_table(db).await.unwrap();
        sqlx::query_scalar("SELECT COUNT(*) FROM discovery_cache")
            .fetch_one(db.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn plain_http_is_refused_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let db = state(tmp.path()).await;
        let (base, hits) = mock(r#"{"server":"https://evil.example"}"#);
        // Another host: refused with or without the loopback opt-in. Loopback: refused unless opted in.
        let (strict, opted) = (Policy::STRICT, Policy::LOOPBACK_HTTP);
        for (remote, policy) in [
            ("http://example.com/o/r", strict),
            ("http://example.com/o/r", opted),
            ("http://192.0.2.7:8080/o/r", opted),
            ("http://127.0.0.1:8080/o/r", strict),
            ("http://localhost/o/r", strict),
        ] {
            let url = Url::parse(remote).unwrap();
            let err = lookup_at(&db, &url, Some(&base), policy)
                .await
                .expect_err(remote);
            assert!(matches!(err, DiscoverError::Refused(_)), "{remote}");
            assert!(err.to_string().contains("http"), "{err}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a request was sent");
        assert_eq!(cache_rows(&db).await, 0);
    }

    #[tokio::test]
    async fn opted_in_loopback_http_discovers_but_is_never_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let db = state(tmp.path()).await;
        let (base, hits) = mock(r#"{"server":"http://127.0.0.1:9","mode":{"o/r":"primary"}}"#);
        let url = Url::parse(&format!("{base}/o/r")).unwrap();
        let doc = lookup_at(&db, &url, None, Policy::LOOPBACK_HTTP)
            .await
            .unwrap()
            .expect("an opted-in loopback remote discovers");
        assert_eq!(doc.server, "http://127.0.0.1:9");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            cache_rows(&db).await,
            0,
            "nothing fetched without TLS is cached"
        );
        // A planted cache row is not trusted for an http remote either.
        store(
            &db,
            &origin_of(&url).unwrap(),
            r#"{"server":"https://planted.example"}"#,
        )
        .await;
        let doc = lookup_at(&db, &url, None, Policy::LOOPBACK_HTTP)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.server, "http://127.0.0.1:9");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_document_names_only_protected_endpoints() {
        let (strict, opted) = (Policy::STRICT, Policy::LOOPBACK_HTTP);
        let from = |origin: &str, policy: Policy, doc: &str| {
            parse_as(
                doc.as_bytes(),
                Rules::of(&Url::parse(origin).unwrap(), policy),
            )
        };
        // An https document may not point credentials at plain http, loopback or not.
        for server in [
            "http://attacker.example",
            "http://127.0.0.1:1",
            "http://localhost",
        ] {
            for doc in [
                format!(r#"{{"server":"{server}"}}"#),
                format!(r#"{{"server":"https://ok.example","objects":["{server}"]}}"#),
                format!(r#"{{"server":"https://ok.example","seeds":"{server}/s"}}"#),
            ] {
                assert!(from("https://host.example", opted, &doc).is_err(), "{doc}");
            }
        }
        // A loopback http document (opted in) may name loopback http, never a remote http host.
        let doc = r#"{"server":"http://attacker.example"}"#;
        assert!(from("http://127.0.0.1:1", opted, doc).is_err());
        let doc = r#"{"server":"http://127.0.0.1:2","objects":["https://c.example"]}"#;
        assert!(from("http://127.0.0.1:1", opted, doc).is_ok());
        // Without the opt-in even loopback http is not an endpoint.
        assert!(from("http://127.0.0.1:1", strict, doc).is_err());
        let doc = r#"{"server":"https://asc.other.example:443"}"#;
        assert!(from("https://host.example", strict, doc).is_ok());
    }

    #[test]
    fn endpoints_on_another_origin_are_refused_unless_configured() {
        let remote = Url::parse("https://host.example/o/r").unwrap();
        let doc = |server: &str, objects: &[&str]| Document {
            server: server.to_owned(),
            objects: objects.iter().map(|o| (*o).to_owned()).collect(),
            ..Document::default()
        };
        // The default port is the same origin.
        assert!(check_origins(&remote, &doc("https://host.example", &[]), false).is_ok());
        assert!(check_origins(&remote, &doc("https://host.example:443", &[]), false).is_ok());
        let err = check_origins(&remote, &doc("https://other.example", &[]), false).unwrap_err();
        assert!(
            err.contains("https://host.example") && err.contains("https://other.example"),
            "{err}"
        );
        for other in [
            "https://host.example:8443",
            "https://sub.host.example",
            "http://host.example",
        ] {
            assert!(
                check_origins(&remote, &doc(other, &[]), false).is_err(),
                "{other}"
            );
        }
        // An objects tier elsewhere needs `ascObjects`; the server never does.
        let tier = doc("https://host.example", &["https://cache.example"]);
        assert!(check_origins(&remote, &tier, false).is_err());
        assert!(check_origins(&remote, &tier, true).is_ok());
        assert!(check_origins(&remote, &doc("https://x.example", &[]), true).is_err());
    }

    #[test]
    fn parses_the_spec_document() {
        let doc = parse(
            br#"{"server":"https://asc.example:443","objects":["https://c1.example:7890"],
            "seeds":"https://seeds.example/s","repos":"*",
            "mode":{"o/r":"mirror","o/p":"primary","o/x":"weird"},"min_client":"0.1.0"}"#,
        )
        .unwrap();
        assert_eq!(doc.server, "https://asc.example");
        assert_eq!(doc.objects, ["https://c1.example:7890"]);
        assert_eq!(doc.seeds.as_deref(), Some("https://seeds.example/s"));
        assert!(doc.is_mirror("o/r"));
        assert!(!doc.is_mirror("o/p"));
        assert!(!doc.is_mirror("o/x"));
        assert_eq!(doc.min_client.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn refuses_hostile_documents() {
        for bad in [
            r#"{"server":"ftp://x"}"#,
            r#"{"server":"https://u:p@x"}"#,
            r#"{"server":"https://x","objects":"https://y"}"#,
            r#"{"server":"https://x","objects":["javascript:1"]}"#,
            r#"{"server":"https://x","min_client":"1.0; rm"}"#,
            r#"{"server":"https://x","seeds":5}"#,
            r#"[1]"#,
            r#"nope"#,
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad}");
        }
        let many = format!(
            r#"{{"server":"https://x","objects":[{}]}}"#,
            [r#""https://y""#; MAX_OBJECTS + 1].join(",")
        );
        assert!(parse(many.as_bytes()).is_err());
        assert_eq!(
            parse(&vec![b' '; MAX_BODY + 1]),
            Err(DiscoverError::TooLarge)
        );
    }

    #[test]
    fn compares_versions() {
        assert!(older_than("0.1.0", "0.2.0"));
        assert!(older_than("1.9", "1.10.0"));
        assert!(!older_than("1.0.0", "1.0.0"));
        assert!(!older_than("2.0.0", "1.9.9"));
    }
}

/// Discovery over HTTP/3 (U89b), against a real `asc serve` h3 listener with a test CA.
#[cfg(all(test, feature = "http3", feature = "serve"))]
mod h3_tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{HeaderMap, HeaderValue, header};
    use asc_rpc::h3::Upgrader;
    use asc_serve::h3_listener::{self, H3LimitsConfig};
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};

    use super::*;

    const DOC: &str = r#"{"server":"https://127.0.0.1:8443","mode":{"o/r":"primary"}}"#;

    /// An h3 listener whose `/.well-known/asc` answers `DOC` and counts its hits.
    fn server() -> (h3_listener::Handle, u16, Vec<u8>, Arc<AtomicUsize>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "asc h3 test ca");
        let ca = CertifiedIssuer::self_signed(ca, ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        leaf.distinguished_name
            .push(DnType::CommonName, "asc h3 test leaf");
        let leaf = leaf.signed_by(&leaf_key, &ca).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let router = Router::new().route(
            "/.well-known/asc",
            axum::routing::get(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { DOC }
            }),
        );
        let limits = H3LimitsConfig::default();
        let ep = h3_listener::bind(
            leaf.pem().as_bytes(),
            leaf_key.serialize_pem().as_bytes(),
            SocketAddr::from(([127, 0, 0, 1], 0)),
            &limits,
        )
        .unwrap();
        let handle = h3_listener::spawn(ep, router, limits);
        let port = handle.local_addr().unwrap().port();
        (handle, port, ca.der().to_vec(), hits)
    }

    fn advert(port: u16) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::ALT_SVC,
            HeaderValue::from_str(&format!("h3=\":{port}\"; ma=60")).unwrap(),
        );
        h
    }

    #[tokio::test]
    async fn discovery_goes_over_h3_once_the_origin_advertised_it() {
        let (_h, port, ca, hits) = server();
        let up = Upgrader::with_extra_roots(
            vec![rustls::pki_types::CertificateDer::from(ca)],
            Default::default(),
        );
        let target = "https://127.0.0.1:8443/.well-known/asc";
        up.note_response(&target.parse().unwrap(), &advert(port));
        let (status, body) = get(&up, target).await.expect("answered over h3");
        assert_eq!(status, 200);
        assert_eq!(body.as_deref(), Some(DOC.as_bytes()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn discovery_over_plain_http_is_never_upgraded() {
        let (_h, port, ca, hits) = server();
        let up = Upgrader::with_extra_roots(
            vec![rustls::pki_types::CertificateDer::from(ca)],
            Default::default(),
        );
        let (base, mock_hits) = super::tests::mock(r#"{"server":"http://127.0.0.1:9"}"#);
        let target = format!("{base}/.well-known/asc");
        // Even if a header named an h3 service for this plain-http origin, it is not used.
        up.note_response(&target.parse().unwrap(), &advert(port));
        let (status, body) = get(&up, &target).await.expect("answered over http");
        assert_eq!(status, 200);
        assert!(body.is_some());
        assert_eq!(mock_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no h3 request went out");
        assert!(!up.would_try(&target.parse().unwrap()));
    }
}
