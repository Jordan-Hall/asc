//! The network side of the scanner against a mock OSV and registry server on a loopback port.
//!
//! The network guard's loopback opt-in is what lets a plain-http loopback endpoint be called; every
//! other rule (the allowlist, the size cap, the timeout, no redirects, no sharing of cache entries
//! between spellings OSV tells apart) runs as in production.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use serde_json::{Value, json};
use sqlx::AnyPool;

use super::config::Config;
use super::egress::{Egress, FetchError, Limits};
use super::latest::Latest;
use super::model::{Ecosystem, Lookup};
use super::osv::{Clock, Osv, Query};
use crate::ingest::testutil::registry;
use crate::netguard::{Guard, NetGuardConfig, StaticResolver};

/// What the mock knows and how it misbehaves.
#[derive(Default)]
pub(super) struct MockState {
    /// `(ecosystem, name, version)` to advisory ids, as OSV would answer a query.
    pub vulns: HashMap<(String, String, String), Vec<String>>,
    pub records: HashMap<String, Value>,
    /// Path to `(status, body)`.
    pub routes: HashMap<String, (u16, String)>,
    pub fail_osv: bool,
    /// Answers `querybatch` with one result too few.
    pub short_results: bool,
    /// Path to the number of failures still to give before the route answers.
    pub flaky: HashMap<String, u32>,
    pub delay: Option<Duration>,
    /// `"METHOD path"` of every request.
    pub hits: Vec<String>,
    /// The number of queries of every `querybatch`.
    pub batches: Vec<usize>,
    /// Path to a binary body.
    pub blobs: HashMap<String, Vec<u8>>,
    /// First-page answers that carry a `next_page_token`, by `(name, version)`.
    pub paged: HashMap<(String, String), (Vec<String>, Vec<String>)>,
}

#[derive(Clone)]
pub(super) struct Mock {
    pub addr: SocketAddr,
    pub st: Arc<Mutex<MockState>>,
}

fn reply(status: u16, body: String) -> Response<Body> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

async fn handle(State(st): State<Arc<Mutex<MockState>>>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let bytes = to_bytes(body, 8 << 20).await.unwrap_or_default();
    let delay = {
        let mut g = st.lock().unwrap_or_else(PoisonError::into_inner);
        g.hits.push(format!("{} {path}", parts.method));
        g.delay
    };
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    let mut g = st.lock().unwrap_or_else(PoisonError::into_inner);
    if path == "/v1/querybatch" {
        if g.fail_osv {
            return reply(500, "{}".into());
        }
        let doc: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let queries = doc["queries"].as_array().cloned().unwrap_or_default();
        g.batches.push(queries.len());
        let mut results: Vec<Value> = queries
            .iter()
            .map(|q| {
                let key = (
                    q["package"]["ecosystem"].as_str().unwrap_or("").to_owned(),
                    q["package"]["name"].as_str().unwrap_or("").to_owned(),
                    q["version"].as_str().unwrap_or("").to_owned(),
                );
                if let Some((first, second)) = g.paged.get(&(key.1.clone(), key.2.clone())) {
                    let ids =
                        |l: &Vec<String>| l.iter().map(|i| json!({"id": i})).collect::<Vec<_>>();
                    return if q.get("page_token").is_some() {
                        json!({"vulns": ids(second)})
                    } else {
                        json!({"vulns": ids(first), "next_page_token": "p2"})
                    };
                }
                match g.vulns.get(&key) {
                    Some(ids) => json!({"vulns": ids
                        .iter()
                        .map(|i| json!({"id": i, "modified": "2024-01-01T00:00:00Z"}))
                        .collect::<Vec<_>>()}),
                    None => json!({}),
                }
            })
            .collect();
        if g.short_results {
            results.pop();
        }
        return reply(200, json!({ "results": results }).to_string());
    }
    if let Some(id) = path.strip_prefix("/v1/vulns/") {
        return match g.records.get(id) {
            Some(r) => reply(200, r.to_string()),
            None => reply(404, "{}".into()),
        };
    }
    if let Some(n) = g.flaky.get_mut(&path)
        && *n > 0
    {
        *n -= 1;
        return reply(500, "{}".into());
    }
    if let Some(b) = g.blobs.get(&path) {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/octet-stream")
            .body(Body::from(b.clone()))
            .unwrap();
    }
    match g.routes.get(&path) {
        Some((status, body)) => reply(*status, body.clone()),
        None => reply(404, "{}".into()),
    }
}

impl Mock {
    pub async fn start() -> Self {
        let st = Arc::new(Mutex::new(MockState::default()));
        let app = Router::new().fallback(handle).with_state(st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, st }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        f(&mut self.st.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn hits(&self) -> Vec<String> {
        self.with(|s| s.hits.clone())
    }

    pub fn count(&self, what: &str) -> usize {
        self.hits().iter().filter(|h| h.starts_with(what)).count()
    }

    /// The mock knows that `name` at `version` has the advisory `id`, with a record that affects it
    /// until `fixed`.
    pub fn vuln(
        &self,
        eco: Ecosystem,
        name: &str,
        version: &str,
        id: &str,
        fixed: &str,
        sev: &str,
    ) {
        self.with(|s| {
            s.vulns
                .entry((
                    eco.osv_name().to_owned(),
                    name.to_owned(),
                    version.to_owned(),
                ))
                .or_default()
                .push(id.to_owned());
            s.records
                .insert(id.to_owned(), record(eco, name, id, fixed, sev));
        });
    }

    pub fn blob(&self, path: &str, body: &[u8]) {
        self.with(|s| {
            s.blobs.insert(path.to_owned(), body.to_vec());
        });
    }

    pub fn route(&self, path: &str, status: u16, body: &str) {
        self.with(|s| {
            s.routes.insert(path.to_owned(), (status, body.to_owned()));
        });
    }
}

/// An OSV record for `name` affected from the start until `fixed`.
pub(super) fn record(eco: Ecosystem, name: &str, id: &str, fixed: &str, sev: &str) -> Value {
    json!({
        "id": id,
        "summary": format!("{id} in {name}"),
        "database_specific": {"severity": sev},
        "affected": [{
            "package": {"name": name, "ecosystem": eco.osv_name()},
            "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}, {"fixed": fixed}]}]
        }]
    })
}

pub(super) fn net() -> NetGuardConfig {
    NetGuardConfig {
        allow_loopback_http: true,
        ..NetGuardConfig::default()
    }
}

/// A config that sends everything to the mock.
pub(super) fn config(mock: &Mock) -> Config {
    let url = mock.url();
    let mut c = Config::default();
    c.osv.url = url.clone();
    for e in [
        &mut c.registries.crates_io,
        &mut c.registries.crates_index,
        &mut c.registries.npm,
        &mut c.registries.pypi,
        &mut c.registries.go,
        &mut c.registries.go_sumdb,
        &mut c.registries.maven,
        &mut c.registries.nuget,
        &mut c.registries.packagist,
        &mut c.registries.rubygems,
        &mut c.registries.hex,
        &mut c.registries.pub_dev,
    ] {
        e.url = url.clone();
        e.timeout_secs = 5;
    }
    c.osv.timeout_secs = 5;
    c
}

pub(super) fn egress_for(c: &Config) -> Arc<Egress> {
    Arc::new(Egress::new(Guard::new(net()), c.allowed_hosts()))
}

/// A clock the test moves.
pub(super) fn clock(start: i64) -> (Clock, Arc<AtomicI64>) {
    let t = Arc::new(AtomicI64::new(start));
    let t2 = t.clone();
    (Arc::new(move || t2.load(Ordering::SeqCst)), t)
}

pub(super) async fn pool() -> AnyPool {
    registry().await.pool().clone()
}

fn q(eco: Ecosystem, name: &str, version: &str) -> Query {
    Query {
        eco,
        name: name.into(),
        version: version.into(),
    }
}

fn ids_of(l: &Lookup) -> Vec<String> {
    match l {
        Lookup::Known(a) => a.iter().map(|x| x.id.clone()).collect(),
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------------------------ egress

fn lim(ms: u64, max: usize) -> Limits {
    Limits {
        timeout: Duration::from_millis(ms),
        max_bytes: max,
    }
}

#[tokio::test]
async fn egress_only_calls_allowed_hosts_and_follows_no_redirect() {
    let m = Mock::start().await;
    m.route("/ok", 200, "hello");
    m.route("/moved", 302, "");
    let e = Egress::new(Guard::new(net()), vec!["127.0.0.1".into()]);
    let url = |p: &str| format!("{}{p}", m.url());
    assert_eq!(
        e.get(&url("/ok"), &lim(2000, 100), None).await.unwrap(),
        b"hello"
    );
    assert_eq!(
        e.get(&url("/moved"), &lim(2000, 100), None).await,
        Err(FetchError::Status(302))
    );
    assert_eq!(
        e.get(&url("/missing"), &lim(2000, 100), None).await,
        Err(FetchError::Status(404))
    );
    assert_eq!(m.count("GET /moved"), 1, "the redirect was not followed");
    // A host that is not on the list is refused before anything is sent.
    let other = Egress::new(Guard::new(net()), vec!["registry.example".into()]);
    let before = m.hits().len();
    assert_eq!(
        other.get(&url("/ok"), &lim(2000, 100), None).await,
        Err(FetchError::NotAllowed)
    );
    assert_eq!(m.hits().len(), before);
    assert!(!other.allows("evil.example"));
    assert!(other.allows("registry.example"));
}

#[tokio::test]
async fn egress_stops_a_large_slow_or_failing_answer() {
    let m = Mock::start().await;
    m.route("/big", 200, &"x".repeat(5000));
    let e = Egress::new(Guard::new(net()), vec!["127.0.0.1".into()]);
    let url = |p: &str| format!("{}{p}", m.url());
    assert_eq!(
        e.get(&url("/big"), &lim(2000, 1000), None).await,
        Err(FetchError::TooLarge)
    );
    assert_eq!(
        e.get(&url("/big"), &lim(2000, 5000), None)
            .await
            .unwrap()
            .len(),
        5000
    );
    m.with(|s| s.delay = Some(Duration::from_millis(400)));
    assert_eq!(
        e.get(&url("/big"), &lim(50, 10_000), None).await,
        Err(FetchError::Timeout)
    );
    m.with(|s| s.delay = None);
    // A transient failure is asked again once; a persistent one is reported.
    m.route("/flaky", 200, "fine");
    m.with(|s| {
        s.flaky.insert("/flaky".into(), 1);
    });
    assert_eq!(
        e.get_retry(&url("/flaky"), &lim(2000, 100), None)
            .await
            .unwrap(),
        b"fine"
    );
    assert_eq!(m.count("GET /flaky"), 2);
    m.with(|s| {
        s.flaky.insert("/flaky".into(), 5);
    });
    assert_eq!(
        e.get_retry(&url("/flaky"), &lim(2000, 100), None).await,
        Err(FetchError::Status(500))
    );
    // A 404 is an answer, not a reason to ask again.
    let before = m.count("GET /nothing");
    let _ = e.get_retry(&url("/nothing"), &lim(2000, 100), None).await;
    assert_eq!(m.count("GET /nothing"), before + 1);
    let msg = FetchError::Transport("x".into()).to_string();
    assert!(!msg.contains("http"), "{msg}");
}

#[tokio::test]
async fn egress_is_held_to_the_network_guard() {
    // https to a name that resolves to a private address.
    let r = Arc::new(StaticResolver::default());
    r.script(
        "api.example.test",
        vec![vec!["10.0.0.5".parse::<IpAddr>().unwrap()]],
    );
    let e = Egress::new(
        Guard::with_resolver(NetGuardConfig::default(), r.clone()),
        vec!["api.example.test".into()],
    );
    assert_eq!(
        e.get("https://api.example.test/x", &lim(500, 100), None)
            .await,
        Err(FetchError::Blocked("private"))
    );
    // A name that also points inwards is refused as a whole.
    r.script(
        "mixed.example.test",
        vec![vec![
            "93.184.216.34".parse().unwrap(),
            "169.254.169.254".parse().unwrap(),
        ]],
    );
    let e = Egress::new(
        Guard::with_resolver(NetGuardConfig::default(), r),
        vec![
            "mixed.example.test".into(),
            "169.254.169.254".into(),
            "127.0.0.1".into(),
        ],
    );
    assert!(matches!(
        e.get("https://mixed.example.test/x", &lim(500, 100), None)
            .await,
        Err(FetchError::Blocked(_))
    ));
    // The metadata address and loopback are refused even when the list names them.
    assert!(matches!(
        e.get("https://169.254.169.254/latest", &lim(500, 100), None)
            .await,
        Err(FetchError::Blocked(_))
    ));
    assert_eq!(
        e.get("http://127.0.0.1:9/x", &lim(500, 100), None).await,
        Err(FetchError::Blocked("not_https")),
        "plain http needs the operator's loopback opt-in"
    );
    assert_eq!(
        e.get("not a url", &lim(500, 100), None).await,
        Err(FetchError::Blocked("malformed"))
    );
}

// ------------------------------------------------------------------------------ OSV

async fn osv_with(m: &Mock, now: i64) -> (Osv, Arc<AtomicI64>) {
    let c = config(m);
    let (clock, t) = clock(now);
    (Osv::new(egress_for(&c), pool().await, &c, clock), t)
}

#[tokio::test]
async fn osv_answers_are_asked_once_and_cached_for_their_time() {
    let m = Mock::start().await;
    m.vuln(
        Ecosystem::Npm,
        "left-pad",
        "1.0.0",
        "GHSA-aaaa",
        "1.3.0",
        "HIGH",
    );
    let (osv, t) = osv_with(&m, 1_000_000).await;
    let queries = [
        q(Ecosystem::Npm, "left-pad", "1.0.0"),
        q(Ecosystem::Npm, "left-pad", "1.3.0"),
    ];
    let got = osv.lookup(&queries, false).await;
    assert_eq!(ids_of(&got.lookups[0]), ["GHSA-aaaa"]);
    assert!(ids_of(&got.lookups[1]).is_empty(), "clean is an answer too");
    let Lookup::Known(adv) = &got.lookups[0] else {
        panic!()
    };
    assert_eq!(adv[0].severity, Some(asc_core::policy::DepSeverity::High));
    assert_eq!(adv[0].fixed, ["1.3.0"]);
    assert_eq!((m.count("POST"), m.count("GET /v1/vulns")), (1, 1));
    // Again: from the cache.
    let again = osv.lookup(&queries, false).await;
    assert_eq!(again.lookups, got.lookups);
    assert_eq!((m.count("POST"), m.count("GET /v1/vulns")), (1, 1));
    // After the answer's time (6 h) the question is asked again; the record (24 h) is still good.
    t.fetch_add(7 * 3600, Ordering::SeqCst);
    osv.lookup(&queries, false).await;
    assert_eq!((m.count("POST"), m.count("GET /v1/vulns")), (2, 1));
    t.fetch_add(25 * 3600, Ordering::SeqCst);
    osv.lookup(&queries, false).await;
    assert_eq!((m.count("POST"), m.count("GET /v1/vulns")), (3, 2));
    // `refresh` skips the cache.
    osv.lookup(&queries, true).await;
    assert_eq!((m.count("POST"), m.count("GET /v1/vulns")), (4, 3));
    assert_eq!(osv.source_name(), "osv-mirror");
}

#[tokio::test]
async fn a_failed_lookup_is_unknown_never_clean_and_is_not_cached() {
    let m = Mock::start().await;
    m.vuln(
        Ecosystem::Npm,
        "bad",
        "1.0.0",
        "GHSA-bbbb",
        "2.0.0",
        "CRITICAL",
    );
    m.with(|s| s.fail_osv = true);
    let (osv, _) = osv_with(&m, 1_000_000).await;
    let queries = [
        q(Ecosystem::Npm, "bad", "1.0.0"),
        q(Ecosystem::Npm, "fine", "1.0.0"),
    ];
    let got = osv.lookup(&queries, false).await;
    for l in &got.lookups {
        assert!(matches!(l, Lookup::Unknown(_)), "{l:?}");
    }
    assert!(
        got.errors[0].contains("OSV query failed"),
        "{:?}",
        got.errors
    );
    assert!(
        !got.errors.iter().any(|e| e.contains("127.0.0.1")),
        "no address in an error"
    );
    assert_eq!(
        m.count("POST"),
        2,
        "asked once more after the first failure"
    );
    // The failure left nothing behind: when OSV is back the answer is real.
    m.with(|s| s.fail_osv = false);
    let got = osv.lookup(&queries, false).await;
    assert_eq!(ids_of(&got.lookups[0]), ["GHSA-bbbb"]);
    // An answer that does not match the question is a failure too.
    m.with(|s| s.short_results = true);
    let (osv2, _) = osv_with(&m, 2_000_000).await;
    let got = osv2.lookup(&queries, true).await;
    assert!(got.lookups.iter().all(|l| matches!(l, Lookup::Unknown(_))));
}

#[tokio::test]
async fn an_advisory_record_that_cannot_be_read_makes_only_its_package_unknown() {
    let m = Mock::start().await;
    m.vuln(Ecosystem::Npm, "a", "1.0.0", "GHSA-ok", "2.0.0", "LOW");
    m.vuln(Ecosystem::Npm, "b", "1.0.0", "GHSA-gone", "2.0.0", "LOW");
    m.with(|s| {
        s.records.remove("GHSA-gone");
    });
    let (osv, _) = osv_with(&m, 1_000_000).await;
    let got = osv
        .lookup(
            &[
                q(Ecosystem::Npm, "a", "1.0.0"),
                q(Ecosystem::Npm, "b", "1.0.0"),
            ],
            false,
        )
        .await;
    assert_eq!(ids_of(&got.lookups[0]), ["GHSA-ok"]);
    assert!(
        matches!(&got.lookups[1], Lookup::Unknown(e) if e.contains("GHSA-gone")),
        "{:?}",
        got.lookups[1]
    );
}

#[tokio::test]
async fn big_batches_are_split_and_result_pages_are_followed() {
    let m = Mock::start().await;
    let (osv, _) = osv_with(&m, 1_000_000).await;
    let many: Vec<Query> = (0..2001)
        .map(|i| q(Ecosystem::Npm, &format!("p{i}"), "1.0.0"))
        .collect();
    let got = osv.lookup(&many, false).await;
    assert_eq!(got.lookups.len(), 2001);
    assert!(got.lookups.iter().all(|l| ids_of(l).is_empty()));
    assert_eq!(m.with(|s| s.batches.clone()), [1000, 1000, 1]);
    // A first page with a token, then the rest.
    m.with(|s| {
        s.paged.insert(
            ("paged".into(), "1.0.0".into()),
            (vec!["GHSA-1".into()], vec!["GHSA-2".into()]),
        );
        s.records.insert(
            "GHSA-1".into(),
            record(Ecosystem::Npm, "paged", "GHSA-1", "2.0.0", "LOW"),
        );
        s.records.insert(
            "GHSA-2".into(),
            record(Ecosystem::Npm, "paged", "GHSA-2", "2.0.0", "LOW"),
        );
    });
    let got = osv
        .lookup(&[q(Ecosystem::Npm, "paged", "1.0.0")], false)
        .await;
    assert_eq!(ids_of(&got.lookups[0]), ["GHSA-1", "GHSA-2"]);
}

#[tokio::test]
async fn an_ecosystem_without_advisory_data_is_uncovered_and_asks_nothing() {
    let m = Mock::start().await;
    let (osv, _) = osv_with(&m, 1_000_000).await;
    let got = osv
        .lookup(&[q(Ecosystem::CocoaPods, "Alamofire", "5.6.1")], false)
        .await;
    assert_eq!(got.lookups, [Lookup::Uncovered]);
    assert!(m.hits().is_empty());
}

#[tokio::test]
async fn names_osv_tells_apart_do_not_share_an_answer_or_a_cache_entry() {
    let m = Mock::start().await;
    // npm is case-sensitive: only `foo` is vulnerable.
    m.vuln(Ecosystem::Npm, "foo", "1.0.0", "GHSA-foo", "2.0.0", "HIGH");
    for order in [["foo", "Foo"], ["Foo", "foo"]] {
        let (osv, _) = osv_with(&m, 1_000_000).await;
        let got = osv
            .lookup(
                &[
                    q(Ecosystem::Npm, order[0], "1.0.0"),
                    q(Ecosystem::Npm, order[1], "1.0.0"),
                ],
                false,
            )
            .await;
        for (name, l) in order.iter().zip(&got.lookups) {
            let want: Vec<&str> = if *name == "foo" {
                vec!["GHSA-foo"]
            } else {
                vec![]
            };
            assert_eq!(ids_of(l), want, "{name} in {order:?}");
        }
    }
    // Asked one at a time, with the cache between them: still not shared.
    let (osv, _) = osv_with(&m, 1_000_000).await;
    let a = osv
        .lookup(&[q(Ecosystem::Npm, "Foo", "1.0.0")], false)
        .await;
    let b = osv
        .lookup(&[q(Ecosystem::Npm, "foo", "1.0.0")], false)
        .await;
    assert!(ids_of(&a.lookups[0]).is_empty());
    assert_eq!(ids_of(&b.lookups[0]), ["GHSA-foo"]);
}

#[tokio::test]
async fn a_name_spelled_unlike_the_osv_one_still_finds_its_advisories_per_ecosystem() {
    let m = Mock::start().await;
    // The mock knows only the canonical spelling of each.
    m.vuln(
        Ecosystem::PyPi,
        "django-rest-framework",
        "1.0.0",
        "PYSEC-1",
        "2.0",
        "HIGH",
    );
    m.vuln(
        Ecosystem::Packagist,
        "symfony/console",
        "1.0.0",
        "GHSA-php",
        "2.0",
        "HIGH",
    );
    m.vuln(
        Ecosystem::CratesIo,
        "serde_json",
        "1.0.0",
        "RUSTSEC-1",
        "2.0",
        "HIGH",
    );
    m.vuln(Ecosystem::Hex, "plug", "1.0.0", "GHSA-hex", "2.0", "HIGH");
    m.vuln(
        Ecosystem::NuGet,
        "Newtonsoft.Json",
        "1.0.0",
        "GHSA-nuget",
        "2.0",
        "HIGH",
    );
    let (osv, _) = osv_with(&m, 1_000_000).await;
    let cases = [
        (Ecosystem::PyPi, "Django_REST.framework", "PYSEC-1"),
        (Ecosystem::Packagist, "Symfony/Console", "GHSA-php"),
        (Ecosystem::CratesIo, "Serde-Json", "RUSTSEC-1"),
        (Ecosystem::CratesIo, "serde-json", "RUSTSEC-1"),
        (Ecosystem::Hex, "Plug", "GHSA-hex"),
        (Ecosystem::NuGet, "Newtonsoft.Json", "GHSA-nuget"),
    ];
    for (eco, name, want) in cases {
        let got = osv.lookup(&[q(eco, name, "1.0.0")], false).await;
        assert_eq!(ids_of(&got.lookups[0]), [want], "{eco} {name}");
    }
    // NuGet written in lower case: OSV keeps the published casing, which cannot be derived, so a
    // lookup that finds nothing is unknown, not clean.
    let got = osv
        .lookup(&[q(Ecosystem::NuGet, "newtonsoft.json", "1.0.0")], false)
        .await;
    assert!(
        matches!(&got.lookups[0], Lookup::Unknown(e) if e.contains("not counted clean")),
        "{:?}",
        got.lookups
    );
    assert!(!got.errors.is_empty());
    // A name spelled as the registry spells it and found clean is clean.
    let got = osv
        .lookup(&[q(Ecosystem::NuGet, "Serilog", "1.0.0")], false)
        .await;
    assert!(ids_of(&got.lookups[0]).is_empty());
    for (eco, name) in [
        (Ecosystem::PyPi, "something-else"),
        (Ecosystem::Packagist, "vendor/other"),
        (Ecosystem::CratesIo, "other_crate"),
        (Ecosystem::Hex, "other"),
        (Ecosystem::Npm, "other"),
        (Ecosystem::Maven, "g:a"),
        (Ecosystem::Go, "example.com/m"),
        (Ecosystem::RubyGems, "rails"),
    ] {
        let got = osv.lookup(&[q(eco, name, "1.0.0")], false).await;
        assert!(ids_of(&got.lookups[0]).is_empty(), "{eco} {name}");
    }
}

#[tokio::test]
async fn an_advisory_file_works_alone_with_the_api_and_when_it_cannot_be_read() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("osv.zip");
    let write_zip = |records: &[Value]| {
        let mut bytes = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            for (i, r) in records.iter().enumerate() {
                z.start_file(
                    format!("{i}.json"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
                z.write_all(r.to_string().as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
    };
    write_zip(&[record(
        Ecosystem::Npm,
        "offline",
        "OFF-1",
        "3.0.0",
        "CRITICAL",
    )]);
    let m = Mock::start().await;
    // File only: no network is used at all.
    let mut c = config(&m);
    c.osv.url = String::new();
    c.advisory_file = Some(path.clone());
    c.validate(&net()).unwrap();
    let (clk, _) = clock(1_000_000);
    let osv = Osv::new(egress_for(&c), pool().await, &c, clk.clone());
    assert_eq!(osv.source_name(), "file");
    let got = osv
        .lookup(
            &[
                q(Ecosystem::Npm, "offline", "1.0.0"),
                q(Ecosystem::Npm, "offline", "3.0.0"),
                q(Ecosystem::Npm, "x", "1.0.0"),
            ],
            false,
        )
        .await;
    assert_eq!(ids_of(&got.lookups[0]), ["OFF-1"]);
    assert!(ids_of(&got.lookups[1]).is_empty());
    assert!(ids_of(&got.lookups[2]).is_empty());
    assert!(m.hits().is_empty());
    // The file changes: it is read again when asked to refresh.
    write_zip(&[
        record(Ecosystem::Npm, "offline", "OFF-1", "3.0.0", "CRITICAL"),
        record(Ecosystem::Npm, "x", "OFF-2", "2.0.0", "LOW"),
    ]);
    let got = osv.lookup(&[q(Ecosystem::Npm, "x", "1.0.0")], true).await;
    assert_eq!(ids_of(&got.lookups[0]), ["OFF-2"]);
    // Both sources: their answers are merged.
    m.vuln(Ecosystem::Npm, "x", "1.0.0", "GHSA-x", "2.0.0", "HIGH");
    let mut both = config(&m);
    both.advisory_file = Some(path.clone());
    let osv = Osv::new(egress_for(&both), pool().await, &both, clk.clone());
    assert_eq!(osv.source_name(), "osv+file");
    let got = osv.lookup(&[q(Ecosystem::Npm, "x", "1.0.0")], false).await;
    assert_eq!(ids_of(&got.lookups[0]), ["GHSA-x", "OFF-2"]);
    // The API failing makes it unknown even though the file answered.
    m.with(|s| s.fail_osv = true);
    let osv = Osv::new(egress_for(&both), pool().await, &both, clk.clone());
    let got = osv.lookup(&[q(Ecosystem::Npm, "x", "1.0.0")], false).await;
    assert!(matches!(got.lookups[0], Lookup::Unknown(_)));
    // A file that is not a record set is unknown for everything, whatever the API says.
    m.with(|s| s.fail_osv = false);
    std::fs::write(&path, b"garbage").unwrap();
    let osv = Osv::new(egress_for(&both), pool().await, &both, clk);
    let got = osv.lookup(&[q(Ecosystem::Npm, "x", "1.0.0")], false).await;
    assert!(
        matches!(&got.lookups[0], Lookup::Unknown(e) if e.contains("advisory file")),
        "{:?}",
        got.lookups
    );
}

// ------------------------------------------------------------------------------ registries

pub(super) async fn latest_with(m: &Mock, now: i64) -> (Latest, Arc<AtomicI64>) {
    let c = config(m);
    let (clock, t) = clock(now);
    (
        Latest::new(
            egress_for(&c),
            pool().await,
            c.registries.clone(),
            c.latest_ttl_secs,
            clock,
        ),
        t,
    )
}

#[tokio::test]
async fn every_registry_is_asked_at_its_own_path_and_read_into_one_shape() {
    let m = Mock::start().await;
    m.route(
        "/api/v1/crates/serde",
        200,
        r#"{"versions":[{"num":"1.0.1","yanked":false,"checksum":"c"},{"num":"1.0.0","yanked":false}]}"#,
    );
    m.route(
        "/@scope%2Fpkg",
        200,
        r#"{"dist-tags":{"latest":"2.0.0"},"versions":{"1.0.0":{"dist":{}},"2.0.0":{"dist":{"integrity":"sha512-a","tarball":"https://r/x.tgz"}}}}"#,
    );
    m.route(
        "/pypi/requests/json",
        200,
        r#"{"info":{"version":"2.31.0"},"releases":{"2.30.0":[{"yanked":false}],"2.31.0":[{"yanked":false}]}}"#,
    );
    m.route(
        "/github.com/!azure/go-autorest/@v/list",
        200,
        "v1.0.0\nv1.1.0\n",
    );
    m.route(
        "/github.com/!azure/go-autorest/@latest",
        200,
        r#"{"Version":"v1.1.0"}"#,
    );
    m.route(
        "/org/apache/commons/commons-lang3/maven-metadata.xml",
        200,
        "<metadata><versioning><release>3.14.0</release><versions><version>3.12.0</version><version>3.14.0</version></versions></versioning></metadata>",
    );
    m.route(
        "/v3-flatcontainer/newtonsoft.json/index.json",
        200,
        r#"{"versions":["12.0.3","13.0.1"]}"#,
    );
    m.route(
        "/p2/monolog/monolog.json",
        200,
        r#"{"packages":{"monolog/monolog":[{"version":"3.5.0"},{"version":"2.9.1"}]}}"#,
    );
    m.route(
        "/api/v1/versions/rails.json",
        200,
        r#"[{"number":"7.1.0","prerelease":false,"platform":"ruby"}]"#,
    );
    m.route(
        "/api/packages/plug",
        200,
        r#"{"latest_stable_version":"1.14.0","releases":[{"version":"1.14.0"}]}"#,
    );
    m.route(
        "/api/packages/http",
        200,
        r#"{"latest":{"version":"1.2.0"},"versions":[{"version":"1.2.0"}]}"#,
    );
    let (l, t) = latest_with(&m, 1_000_000).await;
    let get = |eco, name: &'static str| {
        let l = &l;
        async move { l.versions(eco, name, false).await.unwrap() }
    };
    let crates = get(Ecosystem::CratesIo, "serde").await;
    assert_eq!(crates.latest.as_deref(), Some("1.0.1"));
    let npm = get(Ecosystem::Npm, "@scope/pkg").await;
    assert_eq!(npm.latest.as_deref(), Some("2.0.0"));
    assert_eq!(
        get(Ecosystem::PyPi, "requests").await.versions,
        ["2.30.0", "2.31.0"]
    );
    assert_eq!(
        get(Ecosystem::Go, "github.com/Azure/go-autorest")
            .await
            .latest
            .as_deref(),
        Some("v1.1.0")
    );
    assert_eq!(
        get(Ecosystem::Maven, "org.apache.commons:commons-lang3")
            .await
            .latest
            .as_deref(),
        Some("3.14.0")
    );
    assert_eq!(
        get(Ecosystem::NuGet, "Newtonsoft.Json")
            .await
            .latest
            .as_deref(),
        Some("13.0.1")
    );
    assert_eq!(
        get(Ecosystem::Packagist, "Monolog/Monolog")
            .await
            .latest
            .as_deref(),
        Some("3.5.0")
    );
    assert_eq!(
        get(Ecosystem::RubyGems, "rails").await.latest.as_deref(),
        Some("7.1.0")
    );
    assert_eq!(
        get(Ecosystem::Hex, "plug").await.latest.as_deref(),
        Some("1.14.0")
    );
    assert_eq!(
        get(Ecosystem::Pub, "http").await.latest.as_deref(),
        Some("1.2.0")
    );
    // The answers are cached for their time.
    let n = m.hits().len();
    get(Ecosystem::PyPi, "requests").await;
    assert_eq!(m.hits().len(), n);
    t.fetch_add(3601, Ordering::SeqCst);
    get(Ecosystem::PyPi, "requests").await;
    assert_eq!(m.hits().len(), n + 1);
    assert!(l.available(Ecosystem::Npm));
    assert!(!l.available(Ecosystem::SwiftUrl));
    assert!(
        l.versions(Ecosystem::SwiftUrl, "x", false)
            .await
            .unwrap_err()
            .contains("no registry")
    );
}

#[tokio::test]
async fn a_registry_that_fails_or_answers_oddly_is_an_error_naming_the_ecosystem() {
    let m = Mock::start().await;
    m.route("/api/v1/crates/broken", 200, "not json");
    m.route("/api/v1/crates/huge", 200, &"x".repeat(5000));
    m.route("/pypi/boom/json", 500, "");
    let mut c = config(&m);
    c.registries.crates_io.max_bytes = crate::config::ByteSize(1024);
    let (clk, _) = clock(1);
    let l = Latest::new(egress_for(&c), pool().await, c.registries.clone(), 60, clk);
    let e = l
        .versions(Ecosystem::CratesIo, "broken", false)
        .await
        .unwrap_err();
    assert!(e.contains("crates.io") && e.contains("not usable"), "{e}");
    let e = l
        .versions(Ecosystem::CratesIo, "huge", false)
        .await
        .unwrap_err();
    assert!(e.contains("larger than the limit"), "{e}");
    let e = l
        .versions(Ecosystem::PyPi, "boom", false)
        .await
        .unwrap_err();
    assert!(e.contains("PyPI") && e.contains("500"), "{e}");
    // A name that is a path is not asked about.
    for bad in ["../etc", "a/b", "a\\b", "x..y"] {
        let e = l
            .versions(Ecosystem::CratesIo, bad, false)
            .await
            .unwrap_err();
        assert!(e.contains("not usable"), "{bad}: {e}");
    }
    assert!(
        m.hits()
            .iter()
            .all(|h| !h.contains("..") && !h.contains("etc")),
        "{:?}",
        m.hits()
    );
    // npm with a package too big for the abbreviated listing falls back to its latest.
    m.route("/bigpkg", 200, &"x".repeat(5000));
    m.route(
        "/bigpkg/latest",
        200,
        r#"{"version":"4.0.0","dist":{"integrity":"sha512-z","tarball":"https://r/b.tgz"}}"#,
    );
    let mut c = config(&m);
    c.registries.npm.max_bytes = crate::config::ByteSize(1024);
    let (clk, _) = clock(1);
    let l = Latest::new(egress_for(&c), pool().await, c.registries.clone(), 60, clk);
    let got = l.versions(Ecosystem::Npm, "bigpkg", false).await.unwrap();
    assert_eq!(got.latest.as_deref(), Some("4.0.0"));
    // A registry switched off is said to be.
    let mut off = config(&m);
    off.registries.hex.url = String::new();
    let (clk, _) = clock(1);
    let l = Latest::new(
        egress_for(&off),
        pool().await,
        off.registries.clone(),
        60,
        clk,
    );
    assert!(
        l.versions(Ecosystem::Hex, "plug", false)
            .await
            .unwrap_err()
            .contains("switched off")
    );
}
