//! U85: `InternalAuthz` identifies and authorises Forgejo user keys and deploy keys through K19,
//! against a fake Forgejo (SPEC §25.11.3 item 5).
#![cfg(feature = "forgejo")]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asc_rpc::auth::{Access, AuthError, Authorizer, Decision, Principal, SshKey, SshKeyKind};
use asc_serve::config::{ForgejoConfig, ServeConfig};
use asc_serve::forgejo::{
    AuthzCacheConfig, ForgejoClient, InternalAuthz, InternalToken, PublicApiAuthz, SshLookupLimits,
    SshSupport, ssh_support,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const INTERNAL: &str = "internal-secret";
/// A body the forge sends on a failure; it must never come back out.
const FORGE_SECRET: &str = "FORGE-ERROR-CANARY-/var/lib/forgejo/data";

// ------------------------------------------------------------------------------ fake forge

#[derive(Clone, Debug)]
struct Call {
    path: String,
    auth: Option<String>,
    body: Value,
}

/// What a key is in the fake's tables.
#[derive(Clone)]
struct Row {
    id: i64,
    kind: &'static str,
    /// The owner of a user key; unused for a deploy key.
    user: (i64, &'static str),
    /// A disabled or login-prohibited user, or a deploy key no repository uses.
    usable: bool,
    /// `repo -> grant`: a user key's permission on a repository, or the repositories a deploy key
    /// is registered on.
    grants: HashMap<String, Grant>,
}

#[derive(Clone)]
struct Grant {
    perm: &'static str,
    /// A deploy key acts as the repository owner.
    owner: (i64, &'static str),
    deploy_key_id: i64,
}

#[derive(Clone, Default)]
enum Behaviour {
    #[default]
    Normal,
    /// Every internal call answers this status and body.
    Reply(u16, String),
    /// Every internal call answers 302 to `/elsewhere`.
    Redirect,
    /// Every internal call waits this long first.
    Delay(Duration),
}

#[derive(Default)]
struct Inner {
    calls: Vec<Call>,
    keys: HashMap<String, Row>,
    behaviour: Behaviour,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Inner>>);

impl Fake {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    fn calls(&self, path: &str) -> Vec<Call> {
        self.with(|i| i.calls.iter().filter(|c| c.path == path).cloned().collect())
    }

    fn total(&self) -> usize {
        self.with(|i| i.calls.len())
    }

    fn add(&self, key: &SshKey, row: Row) {
        self.with(|i| {
            i.keys.insert(key.fingerprint().to_string(), row);
        });
    }
}

fn reply(status: u16, body: Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

/// Checks a key request as Forgejo does: one strict object, the blob is the authority, and a
/// fingerprint that is sent must agree with it.
fn parse_key(v: &Value) -> Result<String, &'static str> {
    let obj = v.as_object().ok_or("not an object")?;
    if obj
        .keys()
        .any(|k| !["type", "key", "fingerprint"].contains(&k.as_str()))
    {
        return Err("unknown field");
    }
    let algo = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let key = obj.get("key").and_then(Value::as_str).unwrap_or("");
    if algo.is_empty() || key.is_empty() {
        return Err("empty field");
    }
    let blob = STANDARD.decode(key).map_err(|_| "bad base64")?;
    let fp = format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&blob))
    );
    if let Some(sent) = obj.get("fingerprint").and_then(Value::as_str)
        && sent != fp
    {
        return Err("fingerprint does not match key");
    }
    Ok(fp)
}

async fn handle(
    State(fake): State<Fake>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let behaviour = fake.with(|i| {
        i.calls.push(Call {
            path: path.clone(),
            auth: auth.clone(),
            body: json.clone(),
        });
        i.behaviour.clone()
    });
    if auth.as_deref() != Some(&format!("Bearer {INTERNAL}")) {
        return reply(403, json!({"err": "bad internal token"}));
    }
    match behaviour {
        Behaviour::Normal => {}
        Behaviour::Reply(status, text) => {
            return axum::response::Response::builder()
                .status(status)
                .body(axum::body::Body::from(text))
                .unwrap();
        }
        Behaviour::Redirect => {
            return axum::response::Response::builder()
                .status(302)
                .header(header::LOCATION, "/elsewhere")
                .body(axum::body::Body::empty())
                .unwrap();
        }
        Behaviour::Delay(d) => tokio::time::sleep(d).await,
    }
    fake.with(|i| match path.as_str() {
        "/api/internal/asc/ssh-key" => {
            let fp = match parse_key(&json) {
                Ok(fp) => fp,
                Err(m) => return reply(400, json!({"user_msg": m})),
            };
            match i.keys.get(&fp) {
                Some(r) if r.usable => {
                    let (user_id, username) = if r.kind == "user" { r.user } else { (0, "") };
                    reply(
                        200,
                        json!({"known": true, "key_id": r.id, "kind": r.kind,
                               "user_id": user_id, "username": username}),
                    )
                }
                _ => reply(
                    200,
                    json!({"known": false, "key_id": 0, "kind": "", "user_id": 0, "username": ""}),
                ),
            }
        }
        "/api/internal/asc/authz" => {
            let obj = json.as_object().cloned().unwrap_or_default();
            let known = ["ssh_key", "owner", "repo", "mode"];
            if obj.keys().any(|k| !known.contains(&k.as_str())) {
                return reply(400, json!({"user_msg": "unknown field"}));
            }
            let field = |k: &str| obj.get(k).cloned().unwrap_or(Value::Null);
            let Ok(fp) = parse_key(&field("ssh_key")) else {
                return reply(400, json!({"user_msg": "invalid key"}));
            };
            let repo = format!(
                "{}/{}",
                field("owner").as_str().unwrap_or(""),
                field("repo").as_str().unwrap_or("")
            );
            let write = match field("mode").as_str() {
                Some("read") => false,
                Some("write") => true,
                _ => return reply(400, json!({"user_msg": "mode must be read or write"})),
            };
            let denied = || reply(200, json!({"allowed": false}));
            let Some(row) = i.keys.get(&fp).filter(|r| r.usable) else {
                return denied();
            };
            let Some(g) = row.grants.get(&repo) else {
                return denied();
            };
            if (write && g.perm == "read") || g.perm == "none" {
                return denied();
            }
            let (user_id, username) = if row.kind == "user" {
                row.user
            } else {
                g.owner
            };
            reply(
                200,
                json!({"allowed": true, "user_id": user_id, "username": username,
                       "perm": g.perm, "deploy_key_id": g.deploy_key_id}),
            )
        }
        _ => reply(404, json!({})),
    })
}

async fn start() -> (Fake, String) {
    // The fake Forgejo speaks plain http on the loopback interface (U89 opt-in).
    asc_core::transport_policy::set_config_allow_loopback_http(true);
    let fake = Fake::default();
    let app = Router::new().fallback(handle).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (fake, url)
}

fn authz(url: &str, cache: AuthzCacheConfig) -> InternalAuthz {
    let token = InternalToken::new(INTERNAL).unwrap();
    let client = Arc::new(ForgejoClient::new(url, Some(token)).unwrap());
    InternalAuthz::new(client, Vec::new(), cache)
}

fn short_cache(positive_ms: u64, negative_ms: u64) -> AuthzCacheConfig {
    AuthzCacheConfig {
        positive: Duration::from_millis(positive_ms),
        negative: Duration::from_millis(negative_ms),
        entries: 256,
    }
}

/// A wire blob that looks like an ed25519 key: the type string, then `seed` as the 32 key bytes.
fn blob_of(seed: &[u8]) -> Vec<u8> {
    let mut point = seed.to_vec();
    point.resize(32, b'.');
    let mut blob = Vec::new();
    for part in [&b"ssh-ed25519"[..], &point[..]] {
        blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        blob.extend_from_slice(part);
    }
    blob
}

fn key(seed: &str) -> SshKey {
    SshKey::new("ssh-ed25519", &blob_of(seed.as_bytes())).unwrap()
}

fn user_row(id: i64, user: (i64, &'static str), grants: &[(&str, &'static str)]) -> Row {
    Row {
        id,
        kind: "user",
        user,
        usable: true,
        grants: grants
            .iter()
            .map(|(repo, perm)| {
                (
                    (*repo).to_owned(),
                    Grant {
                        perm,
                        owner: (0, ""),
                        deploy_key_id: 0,
                    },
                )
            })
            .collect(),
    }
}

fn deploy_row(id: i64, grants: &[(&str, &'static str, (i64, &'static str), i64)]) -> Row {
    Row {
        id,
        kind: "deploy",
        user: (0, ""),
        usable: true,
        grants: grants
            .iter()
            .map(|(repo, perm, owner, dk)| {
                (
                    (*repo).to_owned(),
                    Grant {
                        perm,
                        owner: *owner,
                        deploy_key_id: *dk,
                    },
                )
            })
            .collect(),
    }
}

fn allowed(id: &str) -> Decision {
    Decision::Allowed(Principal { id: id.into() })
}

// ------------------------------------------------------------------------------ log capture

/// The tracing events of each thread, as text. One global subscriber serves every test of this
/// binary (a scoped one races with the tracing callsite cache when tests run in parallel), and a
/// test reads only the events of its own thread, which is the whole of a current-thread runtime.
#[derive(Default)]
struct Logs(Mutex<HashMap<std::thread::ThreadId, String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("{} ", event.metadata().target());
        event.record(&mut Fields(&mut line));
        let mut all = self.0.lock().unwrap();
        let mine = all.entry(std::thread::current().id()).or_default();
        mine.push_str(&line);
        mine.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

static LOGS: std::sync::OnceLock<&'static Logs> = std::sync::OnceLock::new();

/// Installs the global subscriber once and returns it.
fn logs() -> &'static Logs {
    LOGS.get_or_init(|| {
        let logs: &'static Logs = Box::leak(Box::default());
        struct Global(&'static Logs);
        impl tracing::Subscriber for Global {
            fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
                self.0.enabled(m)
            }
            fn new_span(&self, a: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                self.0.new_span(a)
            }
            fn record(&self, i: &tracing::span::Id, r: &tracing::span::Record<'_>) {
                self.0.record(i, r);
            }
            fn record_follows_from(&self, a: &tracing::span::Id, b: &tracing::span::Id) {
                self.0.record_follows_from(a, b);
            }
            fn event(&self, e: &tracing::Event<'_>) {
                self.0.event(e);
            }
            fn enter(&self, i: &tracing::span::Id) {
                self.0.enter(i);
            }
            fn exit(&self, i: &tracing::span::Id) {
                self.0.exit(i);
            }
        }
        tracing::subscriber::set_global_default(Global(logs)).expect("no other subscriber");
        logs
    })
}

impl Logs {
    /// What the calling thread logged so far.
    fn text(&self) -> String {
        let all = self.0.lock().unwrap();
        all.get(&std::thread::current().id())
            .cloned()
            .unwrap_or_default()
    }
}

// ------------------------------------------------------------------------------ tests

#[tokio::test]
async fn a_user_key_gets_its_users_rights_and_never_more() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(
        &alice,
        user_row(
            11,
            (7, "alice"),
            &[("o/rw", "write"), ("o/ro", "read"), ("o/none", "none")],
        ),
    );
    let a = authz(&url, AuthzCacheConfig::default());

    let who = a.identify_ssh_key(&alice).await.unwrap().unwrap();
    assert_eq!(who.key_id, "11");
    assert_eq!(who.kind, SshKeyKind::User);
    assert_eq!(who.principal.id, "7:alice");
    assert_eq!(who.repo, None);

    // Write permission: read and write.
    for access in [Access::Read, Access::Write] {
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/rw", access).await,
            Ok(allowed("7:alice"))
        );
    }
    // Read permission: read, not write.
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/ro", Access::Read).await,
        Ok(allowed("7:alice"))
    );
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/ro", Access::Write).await,
        Ok(Decision::Denied)
    );
    // A private repository the user cannot see, and one that does not exist, look the same.
    for repo in ["o/none", "o/private", "x/missing"] {
        assert_eq!(
            a.authorize_ssh_key(&alice, repo, Access::Read).await,
            Ok(Decision::Denied),
            "{repo}"
        );
    }

    // The request is K19's: the key as {type, key, fingerprint}, owner and repo apart, a mode, and
    // the internal token. It carries no `token`, `access` or `user` field.
    let first = fake.calls("/api/internal/asc/ssh-key").remove(0);
    assert_eq!(first.auth.as_deref(), Some("Bearer internal-secret"));
    assert_eq!(
        first.body,
        json!({"type": "ssh-ed25519", "key": STANDARD.encode(alice.blob()),
               "fingerprint": alice.fingerprint().to_string()})
    );
    let call = &fake.calls("/api/internal/asc/authz")[0];
    assert_eq!(call.auth.as_deref(), Some("Bearer internal-secret"));
    assert_eq!(
        call.body,
        json!({"ssh_key": first.body, "owner": "o", "repo": "rw", "mode": "read"})
    );
    for c in fake.calls("/api/internal/asc/authz") {
        assert_eq!(c.auth.as_deref(), Some("Bearer internal-secret"));
    }
}

#[tokio::test]
async fn a_deploy_key_reaches_its_repositories_read_only_unless_registered_with_write() {
    let (fake, url) = start().await;
    let ro = key("deploy-ro");
    let rw = key("deploy-rw");
    // One Forgejo deploy key is one row shared by every repository that uses it.
    fake.add(&ro, deploy_row(5, &[("org/d", "read", (2, "org"), 31)]));
    fake.add(&rw, deploy_row(6, &[("org/d", "write", (2, "org"), 32)]));
    let a = authz(&url, AuthzCacheConfig::default());

    let who = a.identify_ssh_key(&ro).await.unwrap().unwrap();
    assert_eq!(who.kind, SshKeyKind::Deploy);
    assert_eq!(who.key_id, "5");
    // The repository is known per command, and the placeholder never parses as a Forgejo user.
    assert_eq!(who.repo, None);
    assert!(who.principal.id.starts_with("forgejo-deploy-key:"));

    // Read-only: reads, and acts as the owner with its deploy key id (as the HTTP path does).
    assert_eq!(
        a.authorize_ssh_key(&ro, "org/d", Access::Read).await,
        Ok(allowed("2:org:31"))
    );
    assert_eq!(
        a.authorize_ssh_key(&ro, "org/d", Access::Write).await,
        Ok(Decision::Denied)
    );
    // With write: both.
    for access in [Access::Read, Access::Write] {
        assert_eq!(
            a.authorize_ssh_key(&rw, "org/d", access).await,
            Ok(allowed("2:org:32"))
        );
    }
    // Presented for another repository, even a readable one: denied.
    for k in [&ro, &rw] {
        for access in [Access::Read, Access::Write] {
            assert_eq!(
                a.authorize_ssh_key(k, "org/other", access).await,
                Ok(Decision::Denied)
            );
        }
    }
}

#[tokio::test]
async fn unknown_disabled_and_unused_keys_are_not_identified_and_get_nothing() {
    let (fake, url) = start().await;
    let unknown = key("unknown");
    let disabled = key("disabled");
    let unused = key("unused-deploy");
    let mut off = user_row(12, (8, "bob"), &[("o/r", "write")]);
    off.usable = false; // disabled, or login prohibited
    fake.add(&disabled, off);
    let mut idle = deploy_row(13, &[("o/r", "write", (2, "o"), 33)]);
    idle.usable = false; // a deploy key no repository uses
    fake.add(&unused, idle);
    let a = authz(&url, AuthzCacheConfig::default());

    for k in [&unknown, &disabled, &unused] {
        assert_eq!(a.identify_ssh_key(k).await, Ok(None));
        for access in [Access::Read, Access::Write] {
            assert_eq!(
                a.authorize_ssh_key(k, "o/r", access).await,
                Ok(Decision::Denied)
            );
        }
    }
}

#[tokio::test]
async fn answers_are_cached_per_key_repo_and_mode_and_expire() {
    let (fake, url) = start().await;
    let alice = key("alice");
    let bob = key("bob");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    // Bob's key is known but has no rights on o/r.
    fake.add(&bob, user_row(12, (8, "bob"), &[]));
    let a = authz(&url, short_cache(60_000, 250));
    let authz_calls = || fake.calls("/api/internal/asc/authz").len();
    let identify_calls = || fake.calls("/api/internal/asc/ssh-key").len();

    // Positive answers are cached; read and write are separate entries.
    for _ in 0..3 {
        assert!(a.identify_ssh_key(&alice).await.unwrap().is_some());
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
            Ok(allowed("7:alice"))
        );
    }
    assert_eq!((identify_calls(), authz_calls()), (1, 1));
    a.authorize_ssh_key(&alice, "o/r", Access::Write)
        .await
        .unwrap();
    assert_eq!(authz_calls(), 2);

    // Alice's cached yes is never served to another key for the same repository.
    assert_eq!(
        a.authorize_ssh_key(&bob, "o/r", Access::Read).await,
        Ok(Decision::Denied)
    );
    assert_eq!(authz_calls(), 3);

    // Negative answers are cached, but only for the short bound.
    for _ in 0..3 {
        assert_eq!(a.identify_ssh_key(&key("nobody")).await, Ok(None));
        assert_eq!(
            a.authorize_ssh_key(&bob, "o/r", Access::Read).await,
            Ok(Decision::Denied)
        );
    }
    assert_eq!((identify_calls(), authz_calls()), (2, 3));
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(a.identify_ssh_key(&key("nobody")).await, Ok(None));
    assert_eq!(
        a.authorize_ssh_key(&bob, "o/r", Access::Read).await,
        Ok(Decision::Denied)
    );
    assert_eq!(
        (identify_calls(), authz_calls()),
        (3, 4),
        "the negative bound ends"
    );

    // A key that gets rights is served once the negative entry has expired.
    fake.with(|i| {
        i.keys.insert(
            bob.fingerprint().to_string(),
            user_row(12, (8, "bob"), &[("o/r", "read")]),
        );
    });
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        a.authorize_ssh_key(&bob, "o/r", Access::Read).await,
        Ok(allowed("8:bob"))
    );
}

#[tokio::test]
async fn a_revoked_key_cannot_start_a_command_once_the_positive_bound_has_passed() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let a = authz(&url, short_cache(300, 5_000));
    assert!(
        a.authorize_ssh_key(&alice, "o/r", Access::Write)
            .await
            .is_ok()
    );
    assert!(a.identify_ssh_key(&alice).await.unwrap().is_some());

    fake.with(|i| i.keys.clear()); // revoked on the forge
    // Inside the bound the cached yes still answers; that bound is the documented one.
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/r", Access::Write).await,
        Ok(allowed("7:alice"))
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/r", Access::Write).await,
        Ok(Decision::Denied)
    );
    assert_eq!(a.identify_ssh_key(&alice).await, Ok(None));
}

#[tokio::test]
async fn every_failure_to_get_a_clear_answer_refuses_and_is_not_cached() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));

    let huge = format!(r#"{{"allowed":false,"pad":"{}"}}"#, "x".repeat(32 * 1024));
    let cases: Vec<(&str, Behaviour)> = vec![
        (
            "500",
            Behaviour::Reply(500, json!({"err": FORGE_SECRET}).to_string()),
        ),
        ("502", Behaviour::Reply(502, FORGE_SECRET.to_owned())),
        ("401", Behaviour::Reply(401, "{}".into())),
        (
            "403",
            Behaviour::Reply(403, json!({"err": FORGE_SECRET}).to_string()),
        ),
        ("404 (asc disabled)", Behaviour::Reply(404, String::new())),
        ("204", Behaviour::Reply(204, String::new())),
        (
            "html",
            Behaviour::Reply(200, format!("<html>{FORGE_SECRET}</html>")),
        ),
        ("empty", Behaviour::Reply(200, String::new())),
        (
            "wrong types",
            Behaviour::Reply(200, r#"{"known":"yes","allowed":1}"#.into()),
        ),
        (
            "truncated",
            Behaviour::Reply(200, r#"{"known":true,"key_"#.into()),
        ),
        ("oversize", Behaviour::Reply(200, huge)),
        ("redirect", Behaviour::Redirect),
    ];
    for (name, behaviour) in cases {
        // A fresh authorizer per case, so no earlier answer is in its cache.
        let a = authz(&url, AuthzCacheConfig::default());
        fake.with(|i| i.behaviour = behaviour);
        let before = fake.total();
        assert_eq!(
            a.identify_ssh_key(&alice).await,
            Err(AuthError::Forbidden),
            "{name}"
        );
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
            Err(AuthError::Forbidden),
            "{name}"
        );
        assert_eq!(fake.total() - before, 2, "{name}");
        // Nothing was cached: the next calls ask the forge again, and a forge that recovers is
        // believed at once.
        fake.with(|i| i.behaviour = Behaviour::Normal);
        assert!(
            a.identify_ssh_key(&alice).await.unwrap().is_some(),
            "{name}"
        );
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
            Ok(allowed("7:alice")),
            "{name}"
        );
    }
    // A redirect is never followed, so the internal token cannot be sent to another place.
    assert!(fake.calls("/elsewhere").is_empty());
}

#[tokio::test]
async fn a_failed_lookup_is_not_cached_for_the_same_authorizer() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let a = authz(&url, AuthzCacheConfig::default());
    fake.with(|i| i.behaviour = Behaviour::Reply(500, "{}".into()));
    for _ in 0..3 {
        assert_eq!(a.identify_ssh_key(&alice).await, Err(AuthError::Forbidden));
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
            Err(AuthError::Forbidden)
        );
    }
    assert_eq!(fake.total(), 6, "each failure asked the forge again");
    fake.with(|i| i.behaviour = Behaviour::Normal);
    assert!(a.identify_ssh_key(&alice).await.unwrap().is_some());
}

#[tokio::test]
async fn a_slow_forge_times_out_and_refuses() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let a = authz(&url, AuthzCacheConfig::default()).with_ssh_limits(SshLookupLimits {
        timeout: Duration::from_millis(150),
        max_in_flight: 8,
    });
    fake.with(|i| i.behaviour = Behaviour::Delay(Duration::from_secs(5)));
    let started = std::time::Instant::now();
    assert_eq!(a.identify_ssh_key(&alice).await, Err(AuthError::Forbidden));
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
        Err(AuthError::Forbidden)
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn lookups_over_the_in_flight_cap_are_refused_at_once() {
    let (fake, url) = start().await;
    let a = authz(&url, AuthzCacheConfig::default()).with_ssh_limits(SshLookupLimits {
        timeout: Duration::from_secs(5),
        max_in_flight: 2,
    });
    fake.with(|i| i.behaviour = Behaviour::Delay(Duration::from_millis(400)));
    let keys: Vec<SshKey> = (0..5).map(|n| key(&format!("k{n}"))).collect();
    let results = futures::future::join_all(keys.iter().map(|k| a.identify_ssh_key(k))).await;
    let refused = results.iter().filter(|r| r.is_err()).count();
    let answered = results.iter().filter(|r| matches!(r, Ok(None))).count();
    assert_eq!((answered, refused), (2, 3), "{results:?}");
    assert_eq!(
        fake.total(),
        2,
        "the refused lookups never reached the forge"
    );
    // The permits come back.
    assert_eq!(a.identify_ssh_key(&key("later")).await, Ok(None));
}

#[tokio::test]
async fn a_forge_that_contradicts_itself_is_not_believed() {
    let (fake, url) = start().await;
    let alice = key("alice");
    let a = authz(&url, AuthzCacheConfig::default());

    // Authz: each is refused (Denied) or fails (Err), and is never an Allowed.
    let bad_authz = [
        // Permission too low for the mode asked, whatever `allowed` says.
        (
            json!({"allowed": true, "user_id": 7, "username": "alice", "perm": "read"}),
            Access::Write,
            Ok(Decision::Denied),
        ),
        (
            json!({"allowed": true, "user_id": 7, "username": "alice", "perm": "none"}),
            Access::Read,
            Ok(Decision::Denied),
        ),
        (
            json!({"allowed": true, "user_id": 7, "username": "alice"}),
            Access::Read,
            Ok(Decision::Denied),
        ),
        // Not a real account.
        (
            json!({"allowed": true, "user_id": 0, "username": "anonymous", "perm": "write"}),
            Access::Read,
            Ok(Decision::Denied),
        ),
        // An Actions task, or a name that cannot be a user.
        (
            json!({"allowed": true, "user_id": 7, "username": "alice", "perm": "write", "is_actions_task": true}),
            Access::Read,
            Err(AuthError::Forbidden),
        ),
        (
            json!({"allowed": true, "user_id": 7, "username": "al:ice", "perm": "write"}),
            Access::Read,
            Err(AuthError::Forbidden),
        ),
        (
            json!({"allowed": true, "user_id": 7, "username": "alice", "perm": "write", "deploy_key_id": -4}),
            Access::Read,
            Err(AuthError::Forbidden),
        ),
    ];
    for (answer, access, want) in bad_authz {
        fake.with(|i| i.behaviour = Behaviour::Reply(200, answer.to_string()));
        assert_eq!(
            a.authorize_ssh_key(&alice, &format!("o/r{}", fake.total()), access)
                .await,
            want,
            "{answer}"
        );
    }
    // Identify: an answer that is `known` but cannot be a key identity fails the lookup.
    for answer in [
        json!({"known": true, "key_id": 0, "kind": "user", "user_id": 7, "username": "alice"}),
        json!({"known": true, "key_id": 3, "kind": "root", "user_id": 7, "username": "alice"}),
        json!({"known": true, "key_id": 3, "kind": "user", "user_id": 0, "username": "alice"}),
        json!({"known": true, "key_id": 3, "kind": "user", "user_id": 7, "username": ""}),
    ] {
        fake.with(|i| i.behaviour = Behaviour::Reply(200, answer.to_string()));
        assert_eq!(
            a.identify_ssh_key(&key(&answer.to_string())).await,
            Err(AuthError::Forbidden),
            "{answer}"
        );
    }
    // A 400 is the forge refusing this key: a definite no, not an outage.
    fake.with(|i| {
        i.behaviour = Behaviour::Reply(400, json!({"user_msg": "invalid key"}).to_string())
    });
    assert_eq!(a.identify_ssh_key(&key("bad400")).await, Ok(None));
    assert_eq!(
        a.authorize_ssh_key(&key("bad400"), "o/r", Access::Read)
            .await,
        Ok(Decision::Denied)
    );
}

#[tokio::test]
async fn admin_access_and_bad_repository_names_never_reach_the_forge() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let a = authz(&url, AuthzCacheConfig::default());
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/r", Access::Admin).await,
        Ok(Decision::Denied)
    );
    for repo in [
        "", "o", "../x", "o/..", "o/r/x", "o//r", "o/r\n", "o/r\"}", ".o/r",
    ] {
        for access in [Access::Read, Access::Write] {
            assert_eq!(
                a.authorize_ssh_key(&alice, repo, access).await,
                Ok(Decision::Denied),
                "{repo:?}"
            );
        }
    }
    assert_eq!(fake.total(), 0);
}

#[tokio::test]
async fn without_the_internal_token_nothing_is_sent_and_every_key_is_refused() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let client = Arc::new(ForgejoClient::new(&url, None).unwrap());
    let a = InternalAuthz::new(client, Vec::new(), AuthzCacheConfig::default());
    assert_eq!(a.identify_ssh_key(&alice).await, Err(AuthError::Forbidden));
    assert_eq!(
        a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
        Err(AuthError::Forbidden)
    );
    assert_eq!(fake.total(), 0);

    // A wrong token is refused by the forge, and so are we.
    let client =
        Arc::new(ForgejoClient::new(&url, Some(InternalToken::new("wrong").unwrap())).unwrap());
    let a = InternalAuthz::new(client, Vec::new(), AuthzCacheConfig::default());
    assert_eq!(a.identify_ssh_key(&alice).await, Err(AuthError::Forbidden));
}

#[tokio::test]
async fn logs_carry_the_fingerprint_and_never_the_blob_the_token_or_the_forge_body() {
    let logs = logs();
    let (fake, url) = start().await;
    // A blob with a recognisable body, so a leak of it in any encoding family is found.
    let canary = "BLOB-CANARY-PRIVATE-BYTES";
    let alice = SshKey::new("ssh-ed25519", &blob_of(canary.as_bytes())).unwrap();
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let a = authz(&url, AuthzCacheConfig::default());

    // The good path, then each way to fail. Every failure uses its own key, so no cached answer
    // hides the call; every key's blob holds the canary.
    a.identify_ssh_key(&alice).await.unwrap();
    a.authorize_ssh_key(&alice, "o/r", Access::Write)
        .await
        .unwrap();
    let mut keys = vec![alice];
    let failures = [
        Behaviour::Reply(
            500,
            json!({"err": FORGE_SECRET, "user_msg": FORGE_SECRET}).to_string(),
        ),
        Behaviour::Reply(200, format!("not json {FORGE_SECRET}")),
        Behaviour::Reply(
            200,
            json!({"known": true, "kind": FORGE_SECRET}).to_string(),
        ),
        Behaviour::Reply(
            200,
            json!({"allowed": true, "user_id": 7, "username": FORGE_SECRET, "perm": "write"})
                .to_string(),
        ),
        Behaviour::Redirect,
    ];
    for (n, b) in failures.into_iter().enumerate() {
        let k = SshKey::new("ssh-ed25519", &blob_of(format!("{canary}-{n}").as_bytes())).unwrap();
        fake.with(|i| i.behaviour = b);
        assert_eq!(
            a.identify_ssh_key(&k).await.is_err(),
            n != 3,
            "identify {n}"
        );
        assert_eq!(
            a.authorize_ssh_key(&k, "o/other", Access::Read)
                .await
                .is_err(),
            n != 2,
            "authorize {n}"
        );
        keys.push(k);
    }

    let text = logs.text();
    for k in &keys[1..] {
        assert!(
            text.contains(&k.fingerprint().to_string()),
            "failures name the key by fingerprint: {text}"
        );
    }
    for secret in [
        canary,
        &STANDARD.encode(canary),
        INTERNAL,
        FORGE_SECRET,
        "/var/lib/forgejo",
    ] {
        assert!(
            !text.contains(secret),
            "leaked {secret:?} in:
{text}"
        );
    }
    // No window of any key's base64 form appears either.
    for k in &keys {
        let b64 = STANDARD.encode(k.blob());
        for w in b64.as_bytes().windows(16) {
            assert!(
                !text.contains(std::str::from_utf8(w).unwrap()),
                "blob leaked"
            );
        }
        // `Debug` of the key is the fingerprint too.
        assert!(!format!("{k:?}").contains(canary));
    }
}

#[tokio::test]
async fn stage_one_cannot_resolve_ssh_keys() {
    let (fake, url) = start().await;
    let alice = key("alice");
    fake.add(&alice, user_row(11, (7, "alice"), &[("o/r", "write")]));
    let public = PublicApiAuthz::new(
        Arc::new(ForgejoClient::new(&url, None).unwrap()),
        AuthzCacheConfig::default(),
    );
    assert_eq!(PublicApiAuthz::SSH_KEYS, SshSupport::Unsupported);
    assert_eq!(InternalAuthz::SSH_KEYS, SshSupport::Supported);
    assert_eq!(public.identify_ssh_key(&alice).await, Ok(None));
    for access in [Access::Read, Access::Write] {
        assert_eq!(
            public.authorize_ssh_key(&alice, "o/r", access).await,
            Ok(Decision::Denied)
        );
    }
    assert_eq!(fake.total(), 0, "Stage 1 never asks the forge about a key");

    // The start check reads the configuration: the internal token file decides.
    let mut cfg = ServeConfig::default();
    assert_eq!(ssh_support(&cfg), SshSupport::Unsupported, "no [forgejo]");
    let mut forgejo = ForgejoConfig {
        url: url.clone(),
        internal_token_file: None,
        webhook_secret_file: None,
        repo_root: None,
        bridge_promote: false,
    };
    cfg.forgejo = Some(forgejo.clone());
    assert_eq!(ssh_support(&cfg), SshSupport::Unsupported, "Stage 1");
    forgejo.internal_token_file = Some("internal.token".into());
    cfg.forgejo = Some(forgejo);
    assert_eq!(ssh_support(&cfg), SshSupport::Supported, "Stage 2+");
}
