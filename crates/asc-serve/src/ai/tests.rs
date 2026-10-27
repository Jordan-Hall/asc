//! Tests of the AI key service against a scripted provider on a loopback port.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use asc_rpc::auth::{Access, Authorizer, BearerToken, Credential, StaticTokens};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::ai_providers_server::AiProviders;
use serde_json::json;
use sqlx::AnyPool;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request};
use tracing_subscriber::fmt::MakeWriter;

use super::fake::{FakeProvider, Reply};
use super::model::{Message, ModelError, ModelRequest};
use super::owner::{AuthorizerOwners, Owner};
use super::prices::Prices;
use super::rpc::AiService;
use super::store::Provider;
use super::{AiError, AiHub, AiKeysConfig, CallInfo, KeyPatch, NewKey};
use crate::migrate;
use crate::netguard::{Guard, NetGuardConfig};
use crate::vault::Vault;
use crate::vault::keyring::KeyRing;

/// The planted provider key: it must never show up in a log, an error or a response.
pub(crate) const PLANTED: &str = "sk-planted-secret-0123456789abcdef";
pub(crate) const PRICES: &str = r#"{"model-*": [1000000, 2000000], "free-*": [0, 0]}"#;

#[derive(Clone, Default)]
pub(crate) struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl LogBuf {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;

    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

/// Captures every `tracing` event of this thread until the guard drops.
pub(crate) fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    (buf, tracing::subscriber::set_default(sub))
}

/// A database, a vault, a guard that allows loopback http, a hub and a fake provider.
pub(crate) struct Fx {
    pub hub: Arc<AiHub>,
    pub fake: FakeProvider,
    pub pool: AnyPool,
    pub vault: Arc<Vault>,
    #[allow(dead_code)] // held so the guard outlives the hub
    pub guard: Guard,
    pub authorizer: Arc<dyn Authorizer>,
    #[allow(dead_code)] // held so the database file outlives the test
    pub dir: tempfile::TempDir,
}

pub(crate) async fn pool_in(dir: &tempfile::TempDir) -> AnyPool {
    let url = format!(
        "sqlite:{}?mode=rwc",
        dir.path()
            .join("asc.db")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    let pool = migrate::connect(&url, 8).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    pool
}

pub(crate) fn authorizer() -> Arc<dyn Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token("alice-token", "pat:alice", Access::Admin)
            .with_token("bob-token", "pat:bob", Access::Admin)
            .with_token("reader-token", "pat:carol", Access::Read)
            .with_token("host-token", "host-service", Access::Admin),
    )
}

pub(crate) async fn fx_with(cfg: AiKeysConfig) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_in(&dir).await;
    let vault = Vault::in_memory(KeyRing::generate(1_700_000_000).unwrap(), pool.clone());
    let guard = Guard::new(NetGuardConfig {
        allow_loopback_http: true,
        ..NetGuardConfig::default()
    });
    let fake = FakeProvider::start(PLANTED).await;
    let authorizer = authorizer();
    let hub = AiHub::new(
        pool.clone(),
        vault.clone(),
        guard.clone(),
        cfg,
        Prices::parse(PRICES).unwrap(),
        Arc::new(AuthorizerOwners::new(authorizer.clone())),
    );
    Fx {
        hub,
        fake,
        pool,
        vault,
        guard,
        authorizer,
        dir,
    }
}

pub(crate) async fn fx() -> Fx {
    fx_with(AiKeysConfig::default()).await
}

pub(crate) fn new_key(label: &str, base: &str) -> NewKey {
    NewKey {
        label: label.into(),
        provider: Provider::OpenAiCompatible,
        api_key: zeroize::Zeroizing::new(PLANTED.into()),
        base_url: base.into(),
        default_model: String::new(),
        monthly_cap: 0,
    }
}

pub(crate) fn user(n: &str) -> Owner {
    Owner::User(n.into())
}

/// An active compatible key of `owner` pointing at the fake provider.
pub(crate) async fn active_key(f: &Fx, owner: &Owner, label: &str) -> super::store::KeyRow {
    let k = f
        .hub
        .create_key(owner, new_key(label, &f.fake.url()))
        .await
        .unwrap();
    f.hub
        .store
        .mark(
            &k.id,
            k.key_version,
            super::store::KeyState::Active,
            "",
            true,
        )
        .await
        .unwrap();
    f.hub.get_key(&k.id).await.unwrap()
}

pub(crate) fn request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system: "be brief".into(),
        messages: vec![Message::User("hi".into())],
        tools: vec![],
        max_output_tokens: 100,
    }
}

const WHO: CallInfo<'static> = CallInfo {
    repo: "acme/app",
    agent: "fixer",
};

// ------------------------------------------------------------------------------ keys

#[tokio::test(flavor = "current_thread")]
async fn a_key_is_sealed_masked_and_never_logged() {
    let (logs, _g) = capture_logs();
    let f = fx().await;
    let k = f
        .hub
        .create_key(&user("alice"), new_key("work", &f.fake.url()))
        .await
        .unwrap();
    assert!(k.id.starts_with("ak_") && k.id.len() == 23, "{}", k.id);
    assert_eq!(k.key_hint, "cdef");
    assert_eq!(k.state, super::store::KeyState::Untested);
    // The row holds a sealed blob, not the key.
    let blob: Vec<u8> = sqlx::query_scalar("SELECT sealed_key FROM ai_keys WHERE id = $1")
        .bind(k.id.clone())
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!blob.windows(PLANTED.len()).any(|w| w == PLANTED.as_bytes()));
    // The wire form shows only the hint and the flag.
    let w = k.to_wire(0);
    assert!(w.has_key && w.key_hint == "cdef");
    assert!(!format!("{w:?}{k:?}").contains(PLANTED));
    assert_eq!(w.state, wire::AiKeyState::Untested as i32);
    // Every column of every table of this feature is free of the key.
    for table in [
        "ai_keys",
        "ai_usage",
        "ai_month",
        "ai_reservations",
        "service_ops",
    ] {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM {table}")))
            .fetch_all(&f.pool)
            .await
            .unwrap();
        for r in rows {
            for i in 0..sqlx::Row::len(&r) {
                if let Ok(s) = sqlx::Row::try_get::<String, _>(&r, i) {
                    assert!(!s.contains(PLANTED), "{table}");
                }
            }
        }
    }
    assert!(!logs.text().contains(PLANTED));
}

#[tokio::test(flavor = "current_thread")]
async fn keys_are_validated() {
    let f = fx().await;
    let o = user("alice");
    let url = f.fake.url();
    let bad_key = |k: &str| NewKey {
        api_key: zeroize::Zeroizing::new(k.into()),
        ..new_key("a", &url)
    };
    for k in [
        "short",
        "has space in it 123",
        "ctrl\u{1}char-1234567",
        "naïve-unicode-12345",
        &"k".repeat(513),
    ] {
        assert!(
            matches!(
                f.hub.create_key(&o, bad_key(k)).await,
                Err(AiError::Invalid(_))
            ),
            "{k:?}"
        );
    }
    for label in ["", &"l".repeat(65), "bad\nlabel"] {
        assert!(
            matches!(
                f.hub.create_key(&o, new_key(label, &url)).await,
                Err(AiError::Invalid(_))
            ),
            "{label:?}"
        );
    }
    // A compatible provider needs a base_url.
    assert!(matches!(
        f.hub.create_key(&o, new_key("a", "")).await,
        Err(AiError::Invalid(_))
    ));
    // The default model is bounded text.
    let long_model = NewKey {
        default_model: "m".repeat(201),
        ..new_key("a", &url)
    };
    assert!(matches!(
        f.hub.create_key(&o, long_model).await,
        Err(AiError::Invalid(_))
    ));
    // The same label twice for one owner is refused; another owner may reuse it.
    f.hub.create_key(&o, new_key("dup", &url)).await.unwrap();
    assert!(matches!(
        f.hub.create_key(&o, new_key("dup", &url)).await,
        Err(AiError::Exists)
    ));
    f.hub
        .create_key(&user("bob"), new_key("dup", &url))
        .await
        .unwrap();
    // Google is not offered.
    assert!(Provider::from_wire(wire::AiProvider::Google as i32).is_err());
    assert!(Provider::from_wire(0).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn an_owner_holds_a_bounded_number_of_keys() {
    let f = fx_with(AiKeysConfig {
        max_keys_per_owner: 2,
        ..AiKeysConfig::default()
    })
    .await;
    let o = user("alice");
    for n in 0..2 {
        f.hub
            .create_key(&o, new_key(&format!("k{n}"), &f.fake.url()))
            .await
            .unwrap();
    }
    assert!(matches!(
        f.hub.create_key(&o, new_key("k2", &f.fake.url())).await,
        Err(AiError::TooMany(2))
    ));
    f.hub
        .create_key(&user("bob"), new_key("k0", &f.fake.url()))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn base_urls_go_through_the_network_guard() {
    // Loopback http is the operator's opt-in, which these tests set; everything inward stays out.
    let strict = Guard::new(NetGuardConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_in(&dir).await;
    let vault = Vault::in_memory(KeyRing::generate(1).unwrap(), pool.clone());
    let hub = AiHub::new(
        pool,
        vault,
        strict,
        AiKeysConfig::default(),
        Prices::default(),
        Arc::new(AuthorizerOwners::new(authorizer())),
    );
    for (url, slug) in [
        ("http://127.0.0.1:8080", "not_https"),
        ("https://127.0.0.1", "loopback"),
        ("https://10.0.0.5", "private"),
        ("https://192.168.1.1/v1", "private"),
        ("https://169.254.1.1", "link_local"),
        ("https://169.254.169.254", "metadata"),
        ("https://[::1]", "loopback"),
        ("https://user:pw@example.com", "credentials_in_url"),
        ("https://metadata.google.internal", "metadata"),
        ("https://example.com:22", "port"),
    ] {
        match hub.create_key(&user("alice"), new_key("x", url)).await {
            Err(AiError::Blocked(s)) => assert_eq!(s, slug, "{url}"),
            other => panic!("{url}: {other:?}"),
        }
    }
    let st = AiError::Blocked("private").status();
    assert_eq!(st.code(), Code::InvalidArgument);
    let detail = asc_rpc::server::status_rpc_error(&st).unwrap();
    assert_eq!(detail.code(), "AI_ENDPOINT_BLOCKED");
    assert_eq!(detail.info().paths, ["private"]);
}

#[tokio::test(flavor = "current_thread")]
async fn updating_the_secret_or_the_endpoint_untests_the_key() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    // A label or a cap change keeps the key active.
    let k2 = f
        .hub
        .update_key(
            &k.id,
            KeyPatch {
                label: Some("renamed".into()),
                monthly_cap: Some(5),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(k2.state, super::store::KeyState::Active);
    assert_eq!(k2.key_version, k.key_version);
    // A new key resets the test, bumps the version and changes the hint.
    let new_secret = "sk-another-secret-value-zz99";
    let k3 = f
        .hub
        .update_key(
            &k.id,
            KeyPatch {
                api_key: Some(zeroize::Zeroizing::new(new_secret.into())),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(k3.state, super::store::KeyState::Untested);
    assert_eq!(k3.key_version, k.key_version + 1);
    assert_eq!(k3.key_hint, "zz99");
    assert!(k3.last_error.is_empty());
    // The old value cannot be read back from the row.
    let blob: Vec<u8> = sqlx::query_scalar("SELECT sealed_key FROM ai_keys WHERE id = $1")
        .bind(k.id.clone())
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!blob.windows(PLANTED.len()).any(|w| w == PLANTED.as_bytes()));
    // A new base_url resets too, and is guarded (it comes with the key it sends there).
    f.hub
        .store
        .mark(
            &k.id,
            k3.key_version,
            super::store::KeyState::Active,
            "",
            true,
        )
        .await
        .unwrap();
    let fresh = || {
        Some(zeroize::Zeroizing::new(
            "sk-fresh-secret-value-ab12".to_owned(),
        ))
    };
    assert!(matches!(
        f.hub
            .update_key(
                &k.id,
                KeyPatch {
                    base_url: Some("https://10.1.1.1".into()),
                    api_key: fresh(),
                    ..KeyPatch::default()
                }
            )
            .await,
        Err(AiError::Blocked("private"))
    ));
    let k4 = f
        .hub
        .update_key(
            &k.id,
            KeyPatch {
                base_url: Some(format!("{}/other", f.fake.url())),
                api_key: fresh(),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(k4.state, super::store::KeyState::Untested);
    // A label already used by the owner is refused.
    f.hub
        .create_key(&o, new_key("taken", &f.fake.url()))
        .await
        .unwrap();
    assert!(matches!(
        f.hub
            .update_key(
                &k.id,
                KeyPatch {
                    label: Some("taken".into()),
                    ..KeyPatch::default()
                }
            )
            .await,
        Err(AiError::Exists)
    ));
    assert!(matches!(
        f.hub.update_key("ak_nothing", KeyPatch::default()).await,
        Err(AiError::NotFound)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn a_stale_update_loses() {
    let f = fx().await;
    let k = active_key(&f, &user("alice"), "work").await;
    let mut a = f.hub.get_key(&k.id).await.unwrap();
    let mut b = a.clone();
    a.label = "first".into();
    b.label = "second".into();
    assert!(f.hub.store.replace(&a).await.unwrap());
    assert!(
        !f.hub.store.replace(&b).await.unwrap(),
        "b read the old revision"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn deleting_removes_the_key() {
    let f = fx().await;
    let k = f
        .hub
        .create_key(&user("alice"), new_key("work", &f.fake.url()))
        .await
        .unwrap();
    f.hub.delete_key(&k.id).await.unwrap();
    assert!(matches!(f.hub.get_key(&k.id).await, Err(AiError::NotFound)));
    assert!(matches!(
        f.hub.delete_key(&k.id).await,
        Err(AiError::NotFound)
    ));
}

// ------------------------------------------------------------------------------ testing a key

#[tokio::test(flavor = "current_thread")]
async fn testing_a_key_activates_it_and_a_rejection_fails_it() {
    let (logs, _g) = capture_logs();
    let f = fx().await;
    let o = user("alice");
    let k = f
        .hub
        .create_key(&o, new_key("work", &f.fake.url()))
        .await
        .unwrap();
    // Without a default model the test lists models.
    let t = f.hub.test_key(&k.id).await.unwrap();
    assert!(t.ok && t.provider_status == "ok");
    assert_eq!(t.key.state, super::store::KeyState::Active);
    assert!(t.key.tested_at > 0);
    assert_eq!(f.fake.seen().last().unwrap().path, "/models");
    assert_eq!(
        f.fake.seen().last().unwrap().key,
        PLANTED,
        "the key is sent as the bearer token"
    );
    // With a default model it makes one one-token completion and records it.
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                default_model: Some("model-x".into()),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let t = f.hub.test_key(&k.id).await.unwrap();
    assert!(t.ok);
    let c = f.fake.completions();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].body["max_tokens"], 1);
    // The provider now rejects the key.
    f.fake.set_key("sk-rotated-elsewhere-123456");
    let t = f.hub.test_key(&k.id).await.unwrap();
    assert!(!t.ok);
    assert_eq!(t.provider_status, "unauthorized");
    assert_eq!(t.key.state, super::store::KeyState::Failed);
    assert_eq!(t.key.last_error, "unauthorized");
    // The provider's error text (it quoted a key) reaches nobody.
    assert!(!format!("{t:?}").contains("sk-LEAK"));
    assert!(!logs.text().contains("sk-LEAK") && !logs.text().contains(PLANTED));
}

#[tokio::test(flavor = "current_thread")]
async fn testing_is_rate_limited_per_key() {
    let f = fx_with(AiKeysConfig {
        test_per_key_per_hour: 2,
        ..AiKeysConfig::default()
    })
    .await;
    let k = f
        .hub
        .create_key(&user("alice"), new_key("work", &f.fake.url()))
        .await
        .unwrap();
    f.hub.test_key(&k.id).await.unwrap();
    f.hub.test_key(&k.id).await.unwrap();
    assert!(matches!(
        f.hub.test_key(&k.id).await,
        Err(AiError::CapReached("test_rate"))
    ));
    // Another key has its own allowance.
    let k2 = f
        .hub
        .create_key(&user("alice"), new_key("other", &f.fake.url()))
        .await
        .unwrap();
    f.hub.test_key(&k2.id).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_results_for_unreachable_and_odd_providers_leave_the_state_alone() {
    let f = fx().await;
    let k = f
        .hub
        .create_key(&user("alice"), new_key("work", &f.fake.url()))
        .await
        .unwrap();
    f.fake.set_default(Reply::Status(
        429,
        json!({"error": {"type": "rate_limit_error"}}),
    ));
    let k = f
        .hub
        .update_key(
            &k.id,
            KeyPatch {
                default_model: Some("model-x".into()),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let t = f.hub.test_key(&k.id).await.unwrap();
    assert!(!t.ok);
    assert_eq!(t.provider_status, "rate_limited");
    assert_eq!(
        t.key.state,
        super::store::KeyState::Untested,
        "a 429 says nothing about the key"
    );
    assert_eq!(t.key.last_error, "rate_limited");
    f.fake
        .set_default(Reply::Status(200, json!({"nonsense": true})));
    let t = f.hub.test_key(&k.id).await.unwrap();
    assert_eq!(t.provider_status, "bad_response");
}

// ------------------------------------------------------------------------------ calls

#[tokio::test(flavor = "current_thread")]
async fn only_a_tested_key_makes_calls() {
    let f = fx().await;
    let o = user("alice");
    let untested = f
        .hub
        .create_key(&o, new_key("work", &f.fake.url()))
        .await
        .unwrap();
    let c = CancellationToken::new();
    assert!(matches!(
        f.hub
            .complete(&untested, &WHO, &request("model-x"), false, &c)
            .await,
        Err(AiError::NotUsable("untested"))
    ));
    assert!(f.fake.completions().is_empty(), "nothing was sent");
    let k = active_key(&f, &o, "ok").await;
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                disabled: Some(true),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    assert!(matches!(
        f.hub
            .complete(&k, &WHO, &request("model-x"), false, &c)
            .await,
        Err(AiError::NotUsable("disabled"))
    ));
    let st = AiError::NotUsable("untested").status();
    assert_eq!(
        asc_rpc::server::status_rpc_error(&st).unwrap().code(),
        "AI_KEY_UNTESTED"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_is_priced_recorded_and_reported() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    f.fake.push(Reply::Text {
        text: "ok".into(),
        input: 1000,
        output: 500,
    });
    let c = CancellationToken::new();
    let done = f
        .hub
        .complete(&k, &WHO, &request("model-x"), true, &c)
        .await
        .unwrap();
    assert_eq!(done.response.text, "ok");
    // 1000 * 1 + 500 * 2 micro-USD per thousand... prices are per million tokens: 1000 + 1000 = 2000.
    assert_eq!(done.cost_micro_usd, Some(2000));
    let seen = f.fake.completions();
    assert_eq!(seen[0].body["model"], "model-x");
    assert_eq!(seen[0].key, PLANTED);
    let rows = f
        .hub
        .store
        .usage(
            &o,
            None,
            "2000-01-01",
            "2999-12-31",
            super::store::GroupBy::Model,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].requests,
            rows[0].input_tokens,
            rows[0].output_tokens,
            rows[0].cost_micro_usd
        ),
        (1, 1000, 500, 2000)
    );
    assert_eq!(f.hub.month_spent(&k.id).await, 2000);
    // By repo and agent.
    let by_repo = f
        .hub
        .store
        .usage(
            &o,
            None,
            "2000-01-01",
            "2999-12-31",
            super::store::GroupBy::Repo,
        )
        .await
        .unwrap();
    assert_eq!(by_repo[0].group, "acme/app");
    let by_agent = f
        .hub
        .store
        .usage(
            &o,
            Some(&k.id),
            "2000-01-01",
            "2999-12-31",
            super::store::GroupBy::Agent,
        )
        .await
        .unwrap();
    assert_eq!(by_agent[0].group, "fixer");
    // Another owner sees none of it.
    assert!(
        f.hub
            .store
            .usage(
                &user("bob"),
                None,
                "2000-01-01",
                "2999-12-31",
                super::store::GroupBy::Day
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_model_without_a_price_is_refused_under_a_cost_limit_and_flagged_otherwise() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    let c = CancellationToken::new();
    let err = f
        .hub
        .complete(&k, &WHO, &request("mystery-1"), true, &c)
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::NoPrice(_)));
    assert_eq!(err.status().code(), Code::FailedPrecondition);
    assert!(f.fake.completions().is_empty());
    // Without a cost limit the call runs, costs 0 and is flagged.
    let done = f
        .hub
        .complete(&k, &WHO, &request("mystery-1"), false, &c)
        .await
        .unwrap();
    assert_eq!(done.cost_micro_usd, None);
    let rows = f
        .hub
        .store
        .usage(
            &o,
            None,
            "2000-01-01",
            "2999-12-31",
            super::store::GroupBy::Model,
        )
        .await
        .unwrap();
    assert_eq!((rows[0].cost_micro_usd, rows[0].unpriced), (0, 1));
    // A key with a cap needs a price as well: the cap cannot be measured.
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                monthly_cap: Some(100),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    assert!(matches!(
        f.hub
            .complete(&k, &WHO, &request("mystery-1"), false, &c)
            .await,
        Err(AiError::NoPrice(_))
    ));
    // A free model is priced at 0.
    let done = f
        .hub
        .complete(&k, &WHO, &request("free-local"), false, &c)
        .await
        .unwrap();
    assert_eq!(done.cost_micro_usd, Some(0));
}

#[tokio::test(flavor = "current_thread")]
async fn the_monthly_cap_stops_use() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    // The worst case of one call (a small input and 100 output tokens) is a few hundred micro-USD.
    let worst = f
        .hub
        .prices()
        .cost("model-x", AiHub::estimate(&request("model-x")))
        .unwrap();
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                monthly_cap: Some(2200 + worst / 2),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    let c = CancellationToken::new();
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1000,
        output: 600,
    }); // 2200 micro-USD
    f.hub
        .complete(&k, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap();
    assert_eq!(f.hub.month_spent(&k.id).await, 2200);
    // The next call's worst case does not fit under the cap any more.
    let err = f
        .hub
        .complete(&k, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::CapReached("key")), "{err:?}");
    assert_eq!(
        asc_rpc::server::status_rpc_error(&err.status())
            .unwrap()
            .code(),
        "AI_CAP_REACHED"
    );
    assert_eq!(
        f.fake.completions().len(),
        1,
        "the refused call never reached the provider"
    );
    let w = f.hub.get_key(&k.id).await.unwrap().to_wire(2200);
    assert_eq!(w.month_spent_micro_usd, 2200);
    // Raising the cap lets calls through again.
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                monthly_cap: Some(1_000_000),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    f.hub
        .complete(&k, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn the_cap_holds_across_concurrent_calls() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    let worst = f
        .hub
        .prices()
        .cost("model-x", AiHub::estimate(&request("model-x")))
        .unwrap();
    // Room for exactly four calls in flight.
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                monthly_cap: Some(worst * 4 + worst / 2),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    f.fake.set_default(Reply::Delay(
        Duration::from_millis(600),
        Box::new(Reply::Text {
            text: "x".into(),
            input: 1,
            output: 1,
        }),
    ));
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let hub = f.hub.clone();
        let k = k.clone();
        tasks.push(tokio::spawn(async move {
            hub.complete(
                &k,
                &WHO,
                &request("model-x"),
                false,
                &CancellationToken::new(),
            )
            .await
            .map(drop)
        }));
    }
    let mut ok = 0;
    let mut capped = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(()) => ok += 1,
            Err(AiError::CapReached("key")) => capped += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!((ok, capped), (4, 8));
    // Nothing stays reserved once the calls settled.
    let reserved: i64 =
        sqlx::query_scalar("SELECT CAST(COALESCE(SUM(reserved), 0) AS BIGINT) FROM ai_month")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(reserved, 0);
    let rows: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM ai_reservations")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn the_instance_cap_applies_on_top() {
    let worst = Prices::parse(PRICES)
        .unwrap()
        .cost("model-x", AiHub::estimate(&request("model-x")))
        .unwrap();
    let f = fx_with(AiKeysConfig {
        instance_monthly_cap_micro_usd: 2600 + worst / 2,
        ..AiKeysConfig::default()
    })
    .await;
    let a = active_key(&f, &user("alice"), "work").await;
    let b = active_key(&f, &user("bob"), "work").await;
    let c = CancellationToken::new();
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1000,
        output: 800,
    }); // 2600
    f.hub
        .complete(&a, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap();
    // Bob has no cap of his own, but the instance's is nearly spent.
    let err = f
        .hub
        .complete(&b, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::CapReached("instance")), "{err:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_call_releases_its_reservation_and_a_sweep_returns_stale_ones() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                monthly_cap: Some(1_000_000),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    let c = CancellationToken::new();
    f.fake.set_default(Reply::Status(500, json!({})));
    assert!(matches!(
        f.hub
            .complete(&k, &WHO, &request("model-x"), false, &c)
            .await,
        Err(AiError::Model(ModelError::Status(500)))
    ));
    let reserved: i64 =
        sqlx::query_scalar("SELECT CAST(COALESCE(SUM(reserved), 0) AS BIGINT) FROM ai_month")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(reserved, 0);
    assert_eq!(
        f.hub.month_spent(&k.id).await,
        0,
        "a failed call costs nothing"
    );
    // A reservation left by a crashed process is returned by the sweep.
    let now = super::clock::now();
    let r = f
        .hub
        .store
        .reserve(&k.id, 1_000_000, 0, 500, now - 3 * 3600)
        .await
        .unwrap();
    std::mem::forget(r);
    assert_eq!(f.hub.store.sweep(now).await.unwrap(), 1);
    let reserved: i64 =
        sqlx::query_scalar("SELECT CAST(COALESCE(SUM(reserved), 0) AS BIGINT) FROM ai_month")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(reserved, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn provider_failures_map_to_fixed_errors_and_a_rejection_fails_the_key() {
    let (logs, _g) = capture_logs();
    let f = fx_with(AiKeysConfig {
        completion_timeout_secs: 1,
        ..AiKeysConfig::default()
    })
    .await;
    let k = active_key(&f, &user("alice"), "work").await;
    let c = CancellationToken::new();
    let run = |f: &Fx, k: &super::store::KeyRow| {
        let (hub, k) = (f.hub.clone(), k.clone());
        let c = c.clone();
        async move { hub.complete(&k, &WHO, &request("model-x"), false, &c).await }
    };
    f.fake.set_default(Reply::Status(
        429,
        json!({"error": {"type": "rate_limit_error"}}),
    ));
    assert!(matches!(
        run(&f, &k).await,
        Err(AiError::Model(ModelError::RateLimited { .. }))
    ));
    f.fake.set_default(Reply::Status(
        400,
        json!({"error": {"type": "invalid_request_error", "message": "model sk-LEAK is bad"}}),
    ));
    let e = run(&f, &k).await.unwrap_err();
    assert!(
        matches!(&e, AiError::Model(ModelError::Refused(s)) if s == "invalid_request_error"),
        "{e:?}"
    );
    assert!(!format!("{e:?}{}", e.status().message()).contains("sk-LEAK"));
    f.fake.set_default(Reply::Delay(
        Duration::from_secs(5),
        Box::new(Reply::text("late")),
    ));
    assert!(matches!(
        run(&f, &k).await,
        Err(AiError::Model(ModelError::Timeout))
    ));
    f.fake.set_default(Reply::Huge(17 * 1024 * 1024));
    assert!(matches!(
        run(&f, &k).await,
        Err(AiError::Model(ModelError::BadResponse))
    ));
    f.fake
        .set_default(Reply::Status(200, json!({"choices": []})));
    assert!(matches!(
        run(&f, &k).await,
        Err(AiError::Model(ModelError::BadResponse))
    ));
    // Cancelling ends a call at once.
    f.fake.set_default(Reply::Delay(
        Duration::from_secs(5),
        Box::new(Reply::text("late")),
    ));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let started = std::time::Instant::now();
    assert!(
        f.hub
            .complete(&k, &WHO, &request("model-x"), false, &cancel)
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    // The provider rejects the key: the key fails and stops being used.
    f.fake.set_key("sk-rotated-elsewhere-123456");
    assert!(matches!(
        run(&f, &k).await,
        Err(AiError::Model(ModelError::Unauthorized))
    ));
    let after = f.hub.get_key(&k.id).await.unwrap();
    assert_eq!(after.state, super::store::KeyState::Failed);
    assert!(matches!(
        f.hub
            .complete(&after, &WHO, &request("model-x"), false, &c)
            .await,
        Err(AiError::NotUsable("failed"))
    ));
    let text = logs.text();
    assert!(
        !text.contains(PLANTED) && !text.contains("sk-LEAK"),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn anthropic_keys_use_the_messages_api() {
    let f = fx().await;
    let o = user("alice");
    // Only a compatible key may name an endpoint, so the row is switched to Anthropic after the
    // guard has accepted the fake provider's address.
    let k = f
        .hub
        .create_key(&o, new_key("claude", &f.fake.url()))
        .await
        .unwrap();
    sqlx::query("UPDATE ai_keys SET provider = $2 WHERE id = $1")
        .bind(k.id.clone())
        .bind(i64::from(Provider::Anthropic.to_wire()))
        .execute(f.hub.store.pool())
        .await
        .unwrap();
    f.hub
        .store
        .mark(
            &k.id,
            k.key_version,
            super::store::KeyState::Active,
            "",
            true,
        )
        .await
        .unwrap();
    let k = f.hub.get_key(&k.id).await.unwrap();
    f.fake
        .push(Reply::tool("read_file", json!({"path": "a.txt"})));
    let done = f
        .hub
        .complete(
            &k,
            &WHO,
            &request("model-x"),
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(done.response.tool_calls[0].name, "read_file");
    assert_eq!(done.response.tool_calls[0].input["path"], "a.txt");
    let seen = &f.fake.completions()[0];
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.key, PLANTED, "x-api-key");
    assert_eq!(seen.body["system"], "be brief");
    assert_eq!(seen.body["max_tokens"], 100);
}

// ------------------------------------------------------------------------------ resolution

#[tokio::test(flavor = "current_thread")]
async fn a_key_is_found_repo_then_org_then_user_then_instance_and_never_across_owners() {
    let f = fx().await;
    let chain = Owner::chain("acme/app", Some("alice"));
    let inst = active_key(&f, &Owner::Instance, "inst").await;
    assert_eq!(f.hub.resolve(&chain, None).await.unwrap().id, inst.id);
    let alice = active_key(&f, &user("alice"), "mine").await;
    assert_eq!(f.hub.resolve(&chain, None).await.unwrap().id, alice.id);
    let org = active_key(&f, &Owner::Org("acme".into()), "team").await;
    assert_eq!(f.hub.resolve(&chain, None).await.unwrap().id, org.id);
    let repo = active_key(&f, &Owner::Repo("acme/app".into()), "repo").await;
    assert_eq!(f.hub.resolve(&chain, None).await.unwrap().id, repo.id);
    // A disabled key is skipped; a tested key is preferred to an untested one of the same owner.
    f.hub
        .update_key(
            &repo.id,
            KeyPatch {
                disabled: Some(true),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(f.hub.resolve(&chain, None).await.unwrap().id, org.id);
    // A named key must belong to an owner of the chain.
    let stranger = active_key(&f, &user("mallory"), "theirs").await;
    assert!(matches!(
        f.hub.resolve(&chain, Some(&stranger.id)).await,
        Err(AiError::NotFound)
    ));
    assert_eq!(
        f.hub.resolve(&chain, Some(&alice.id)).await.unwrap().id,
        alice.id
    );
    // Another repository never sees this repository's key.
    let other = Owner::chain("zed/other", Some("bob"));
    assert_eq!(f.hub.resolve(&other, None).await.unwrap().id, inst.id);
    f.hub.delete_key(&inst.id).await.unwrap();
    assert!(matches!(
        f.hub.resolve(&other, None).await,
        Err(AiError::NotFound)
    ));
}

// ------------------------------------------------------------------------------ the service

fn req<T>(msg: T, token: &str, op: Option<&str>) -> Request<T> {
    let mut r = Request::new(msg);
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new(token).unwrap()));
    if let Some(op) = op {
        r.metadata_mut().insert("x-asc-op-id", op.parse().unwrap());
    }
    r
}

fn owner_ref(kind: wire::OwnerKind, name: &str) -> Option<wire::OwnerRef> {
    Some(wire::OwnerRef {
        kind: kind as i32,
        name: name.into(),
    })
}

fn create_msg(owner: Option<wire::OwnerRef>, label: &str, base: &str) -> wire::CreateAiKeyRequest {
    wire::CreateAiKeyRequest {
        owner,
        label: label.into(),
        provider: wire::AiProvider::OpenaiCompatible as i32,
        api_key: PLANTED.into(),
        base_url: base.into(),
        default_model: "model-x".into(),
        monthly_cap_micro_usd: 0,
    }
}

fn service(f: &Fx) -> AiService {
    AiService::new(f.hub.clone(), f.authorizer.clone(), ServerInfo::default())
}

const OP1: &str = "11111111-1111-4111-8111-111111111111";
const OP2: &str = "22222222-2222-4222-8222-222222222222";

#[tokio::test(flavor = "current_thread")]
async fn the_service_serves_the_owner_and_hides_the_key_from_everyone_else() {
    let (logs, _g) = capture_logs();
    let f = fx().await;
    let svc = service(&f);
    let alice = owner_ref(wire::OwnerKind::User, "alice");
    // Alice creates a key for herself.
    let created = svc
        .create_key(req(
            create_msg(alice.clone(), "work", &f.fake.url()),
            "alice-token",
            Some(OP1),
        ))
        .await
        .unwrap()
        .into_inner();
    let key = created.key.unwrap();
    assert!(key.id.starts_with("ak_") && key.has_key && key.key_hint == "cdef");
    assert_eq!(key.state, wire::AiKeyState::Untested as i32);
    assert!(!format!("{key:?}").contains(PLANTED));
    // A retry with the same operation id returns the same key, not a second one.
    let again = svc
        .create_key(req(
            create_msg(alice.clone(), "work", &f.fake.url()),
            "alice-token",
            Some(OP1),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.key.unwrap().id, key.id);
    let list = svc
        .list_keys(req(
            wire::ListAiKeysRequest {
                owner: alice.clone(),
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.keys.len(), 1);
    // The same operation id for another call is refused.
    let e = svc
        .delete_key(req(
            wire::DeleteAiKeyRequest { id: key.id.clone() },
            "alice-token",
            Some(OP1),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    // A mutating call needs an operation id.
    let e = svc
        .create_key(req(
            create_msg(alice.clone(), "w2", &f.fake.url()),
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    // Bob cannot see, list, test, change or delete Alice's key: NOT_FOUND each time.
    let e = svc
        .get_key(req(
            wire::GetAiKeyRequest { id: key.id.clone() },
            "bob-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    let missing = svc
        .get_key(req(
            wire::GetAiKeyRequest {
                id: "ak_doesnotexist0000000".into(),
            },
            "bob-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(
        (missing.code(), missing.message()),
        (e.code(), e.message()),
        "a stranger cannot tell a key from none"
    );
    assert_eq!(
        svc.list_keys(req(
            wire::ListAiKeysRequest {
                owner: alice.clone()
            },
            "bob-token",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    assert_eq!(
        svc.test_key(req(
            wire::TestAiKeyRequest { id: key.id.clone() },
            "bob-token",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    assert_eq!(
        svc.delete_key(req(
            wire::DeleteAiKeyRequest { id: key.id.clone() },
            "bob-token",
            Some(OP2)
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    assert_eq!(
        svc.update_key(req(
            wire::UpdateAiKeyRequest {
                id: key.id.clone(),
                disabled: Some(true),
                ..Default::default()
            },
            "bob-token",
            Some(OP2)
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    assert_eq!(
        svc.list_models(req(
            wire::ListAiModelsRequest { id: key.id.clone() },
            "bob-token",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    // Bob cannot create a key in Alice's name, and anonymous callers get nothing.
    assert_eq!(
        svc.create_key(req(
            create_msg(alice.clone(), "evil", &f.fake.url()),
            "bob-token",
            Some(OP2)
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    let anon = Request::new(wire::ListAiKeysRequest {
        owner: alice.clone(),
    });
    assert_eq!(
        svc.list_keys(anon).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    assert_eq!(
        svc.list_keys(req(
            wire::ListAiKeysRequest {
                owner: alice.clone()
            },
            "forged",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::Unauthenticated
    );
    // The instance is for the host application only; an organisation or repository needs admin.
    let inst = owner_ref(wire::OwnerKind::Instance, "");
    assert_eq!(
        svc.create_key(req(
            create_msg(inst.clone(), "i", &f.fake.url()),
            "alice-token",
            Some(OP2)
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    let host = svc
        .create_key(req(
            create_msg(inst, "i", &f.fake.url()),
            "host-token",
            Some(OP2),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        host.key.unwrap().owner.unwrap().kind,
        wire::OwnerKind::Instance as i32
    );
    let repo = owner_ref(wire::OwnerKind::Repo, "acme/app");
    assert_eq!(
        svc.create_key(req(
            create_msg(repo.clone(), "r", &f.fake.url()),
            "reader-token",
            Some("33333333-3333-4333-8333-333333333333")
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    svc.create_key(req(
        create_msg(repo, "r", &f.fake.url()),
        "alice-token",
        Some("44444444-4444-4444-8444-444444444444"),
    ))
    .await
    .unwrap();
    // Test, then use through the service.
    let t = svc
        .test_key(req(
            wire::TestAiKeyRequest { id: key.id.clone() },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(t.ok);
    assert_eq!(t.key.unwrap().state, wire::AiKeyState::Active as i32);
    let models = svc
        .list_models(req(
            wire::ListAiModelsRequest { id: key.id.clone() },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(models.models[0].id, "model-a");
    assert_eq!(models.models[0].context_tokens, 200_000);
    // Update: masked again, and the new key shows its new hint.
    let upd = svc
        .update_key(req(
            wire::UpdateAiKeyRequest {
                id: key.id.clone(),
                api_key: Some("sk-brand-new-key-value-77".into()),
                ..Default::default()
            },
            "alice-token",
            Some("55555555-5555-4555-8555-555555555555"),
        ))
        .await
        .unwrap()
        .into_inner();
    let k = upd.key.unwrap();
    assert_eq!(
        (k.key_hint.as_str(), k.state, k.key_version),
        ("e-77", wire::AiKeyState::Untested as i32, 2)
    );
    // Delete.
    svc.delete_key(req(
        wire::DeleteAiKeyRequest { id: key.id.clone() },
        "alice-token",
        Some("66666666-6666-4666-8666-666666666666"),
    ))
    .await
    .unwrap();
    assert_eq!(
        svc.get_key(req(
            wire::GetAiKeyRequest { id: key.id },
            "alice-token",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    let text = logs.text();
    assert!(
        !text.contains(PLANTED) && !text.contains("brand-new-key"),
        "{text}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_service_validates_requests() {
    let f = fx().await;
    let svc = service(&f);
    let alice = owner_ref(wire::OwnerKind::User, "alice");
    let mut bad = create_msg(alice.clone(), "w", &f.fake.url());
    bad.api_key = "short".into();
    assert_eq!(
        svc.create_key(req(bad, "alice-token", Some(OP1)))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut bad = create_msg(alice.clone(), "w", &f.fake.url());
    bad.provider = wire::AiProvider::Google as i32;
    assert_eq!(
        svc.create_key(req(bad, "alice-token", Some(OP1)))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut bad = create_msg(alice.clone(), "w", "https://10.0.0.9");
    bad.provider = wire::AiProvider::OpenaiCompatible as i32;
    let e = svc
        .create_key(req(bad, "alice-token", Some(OP1)))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    assert_eq!(
        asc_rpc::server::status_rpc_error(&e).unwrap().code(),
        "AI_ENDPOINT_BLOCKED"
    );
    // A failed call does not burn its operation id.
    svc.create_key(req(
        create_msg(alice.clone(), "w", &f.fake.url()),
        "alice-token",
        Some(OP1),
    ))
    .await
    .unwrap();
    // The owner is required and checked.
    assert_eq!(
        svc.create_key(req(
            create_msg(None, "w", &f.fake.url()),
            "alice-token",
            Some(OP2)
        ))
        .await
        .unwrap_err()
        .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        svc.list_keys(req(
            wire::ListAiKeysRequest {
                owner: owner_ref(wire::OwnerKind::Unspecified, "x")
            },
            "alice-token",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::InvalidArgument
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_is_reported_per_owner_and_group() {
    let f = fx().await;
    let svc = service(&f);
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1000,
        output: 500,
    });
    for _ in 0..3 {
        f.hub
            .complete(
                &k,
                &WHO,
                &request("model-x"),
                false,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    f.hub
        .complete(
            &k,
            &CallInfo {
                repo: "acme/other",
                agent: "docs",
            },
            &request("mystery"),
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let alice = owner_ref(wire::OwnerKind::User, "alice");
    let by_model = svc
        .usage(req(
            wire::AiUsageRequest {
                owner: alice.clone(),
                group_by: wire::AiUsageGroup::Model as i32,
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    let m = by_model.rows.iter().find(|r| r.group == "model-x").unwrap();
    assert_eq!(
        (
            m.requests,
            m.input_tokens,
            m.output_tokens,
            m.cost_micro_usd
        ),
        (3, 3000, 1500, 6000)
    );
    let mystery = by_model.rows.iter().find(|r| r.group == "mystery").unwrap();
    assert_eq!((mystery.cost_micro_usd, mystery.unpriced_requests), (0, 1));
    let total = by_model.total.unwrap();
    assert_eq!(
        (
            total.requests,
            total.cost_micro_usd,
            total.unpriced_requests
        ),
        (4, 6000, 1)
    );
    let by_day = svc
        .usage(req(
            wire::AiUsageRequest {
                owner: alice.clone(),
                group_by: wire::AiUsageGroup::Day as i32,
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_day.rows.len(), 1);
    assert_eq!(by_day.rows[0].group, super::clock::day(super::clock::now()));
    // A key of another owner cannot be named; bob cannot read alice's usage; a range is bounded.
    let bob_key = active_key(&f, &user("bob"), "b").await;
    let e = svc
        .usage(req(
            wire::AiUsageRequest {
                owner: alice.clone(),
                key_id: bob_key.id,
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    assert_eq!(
        svc.usage(req(
            wire::AiUsageRequest {
                owner: alice.clone(),
                ..Default::default()
            },
            "bob-token",
            None
        ))
        .await
        .unwrap_err()
        .code(),
        Code::NotFound
    );
    let e = svc
        .usage(req(
            wire::AiUsageRequest {
                owner: alice,
                since: 1,
                until: 400 * 86_400 * 2,
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "current_thread")]
async fn a_rotation_reseals_every_key_and_each_stays_usable() {
    let f = fx().await;
    let k1 = active_key(&f, &user("alice"), "a").await;
    let k2 = active_key(&f, &Owner::Org("acme".into()), "b").await;
    let before: Vec<i64> = sqlx::query_scalar("SELECT key_gen FROM ai_keys")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert!(before.iter().all(|g| *g == before[0]));
    let rot = f.vault.rotate(false).await.unwrap();
    f.vault.wait_idle().await;
    let after: Vec<i64> = sqlx::query_scalar("SELECT key_gen FROM ai_keys")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert!(
        after.iter().all(|g| *g == i64::from(rot.to_generation)),
        "{after:?} {rot:?}"
    );
    let c = CancellationToken::new();
    let k1 = f.hub.get_key(&k1.id).await.unwrap();
    f.hub
        .complete(&k1, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap();
    let k2 = f.hub.get_key(&k2.id).await.unwrap();
    f.hub
        .complete(&k2, &WHO, &request("model-x"), false, &c)
        .await
        .unwrap();
    assert_eq!(f.fake.completions().last().unwrap().key, PLANTED);
}

#[tokio::test(flavor = "current_thread")]
async fn a_sealed_key_cannot_be_moved_to_another_owner_or_row() {
    let f = fx().await;
    let a = active_key(&f, &user("alice"), "a").await;
    let b = active_key(&f, &user("bob"), "b").await;
    // Copy Alice's sealed blob over Bob's row: it does not open there.
    sqlx::query("UPDATE ai_keys SET sealed_key = (SELECT sealed_key FROM ai_keys WHERE id = $1) WHERE id = $2")
        .bind(a.id.clone())
        .bind(b.id.clone())
        .execute(&f.pool)
        .await
        .unwrap();
    let b = f.hub.get_key(&b.id).await.unwrap();
    let err = f
        .hub
        .complete(
            &b,
            &WHO,
            &request("model-x"),
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Vault(_)), "{err:?}");
    assert!(f.fake.completions().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_base_url_without_a_new_key_is_refused_and_changes_nothing() {
    let f = fx().await;
    let k = active_key(&f, &user("alice"), "work").await;
    let other = format!("{}/other", f.fake.url());
    let err = f
        .hub
        .update_key(
            &k.id,
            KeyPatch {
                base_url: Some(other.clone()),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            AiError::CredentialsRequired {
                changed: "base_url",
                needs: "api_key"
            }
        ),
        "{err:?}"
    );
    let st = err.status();
    assert_eq!(st.code(), Code::InvalidArgument);
    assert!(
        st.message().contains("base_url") && st.message().contains("api_key"),
        "{}",
        st.message()
    );
    // The row is as it was: same destination, same sealed key, still active.
    let after = f.hub.get_key(&k.id).await.unwrap();
    assert_eq!(
        (after.base_url.as_str(), after.state, after.rev),
        (k.base_url.as_str(), super::store::KeyState::Active, k.rev)
    );
    assert_eq!(after.sealed_key, k.sealed_key);
    // Restating the current destination is not a change.
    f.hub
        .update_key(
            &k.id,
            KeyPatch {
                base_url: Some(k.base_url.clone()),
                label: Some("same-url".into()),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    // With a new key in the same request the move is allowed.
    let moved = f
        .hub
        .update_key(
            &k.id,
            KeyPatch {
                base_url: Some(other.clone()),
                api_key: Some(zeroize::Zeroizing::new("sk-fresh-secret-value-ab12".into())),
                ..KeyPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.base_url, other);
    assert_eq!(moved.state, super::store::KeyState::Untested);
    assert_ne!(moved.sealed_key, k.sealed_key);
}

#[tokio::test(flavor = "current_thread")]
async fn only_an_openai_compatible_provider_takes_a_base_url() {
    let f = fx().await;
    let o = user("alice");
    for provider in [Provider::OpenAi, Provider::Anthropic] {
        let err = f
            .hub
            .create_key(
                &o,
                NewKey {
                    provider,
                    ..new_key("x", &f.fake.url())
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AiError::Invalid(m) if m.contains("base_url")),
            "{err:?}"
        );
        // The vendor's own endpoint needs no base_url and the key is created.
        let k = f
            .hub
            .create_key(
                &o,
                NewKey {
                    provider,
                    ..new_key(&format!("{provider:?}"), "")
                },
            )
            .await
            .unwrap();
        // Nor can one be added later, with or without a key.
        let err = f
            .hub
            .update_key(
                &k.id,
                KeyPatch {
                    base_url: Some(f.fake.url()),
                    api_key: Some(zeroize::Zeroizing::new("sk-fresh-secret-value-ab12".into())),
                    ..KeyPatch::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AiError::Invalid(m) if m.contains("base_url")),
            "{err:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_service_will_not_send_a_stored_key_to_a_new_endpoint_and_answers_the_owner_only() {
    let f = fx().await;
    let svc = service(&f);
    let alice = owner_ref(wire::OwnerKind::User, "alice");
    let key = svc
        .create_key(req(
            create_msg(alice.clone(), "work", &f.fake.url()),
            "alice-token",
            Some(OP1),
        ))
        .await
        .unwrap()
        .into_inner()
        .key
        .unwrap();
    let other = format!("{}/other", f.fake.url());
    let mv = |api_key: Option<&str>| wire::UpdateAiKeyRequest {
        id: key.id.clone(),
        base_url: Some(other.clone()),
        api_key: api_key.map(Into::into),
        ..Default::default()
    };
    // The destination alone is refused with an error that names both fields.
    let e = svc
        .update_key(req(mv(None), "alice-token", Some(OP2)))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    assert!(
        e.message().contains("base_url") && e.message().contains("api_key"),
        "{}",
        e.message()
    );
    // The key stayed where it was.
    let now = svc
        .get_key(req(
            wire::GetAiKeyRequest { id: key.id.clone() },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .key
        .unwrap();
    assert_eq!(
        (now.base_url.as_str(), now.key_version),
        (key.base_url.as_str(), key.key_version)
    );
    // With a new secret it moves, and the answer carries the owner.
    let moved = svc
        .update_key(req(
            mv(Some("sk-fresh-secret-value-ab12")),
            "alice-token",
            Some("77777777-7777-4777-8777-777777777777"),
        ))
        .await
        .unwrap()
        .into_inner()
        .key
        .unwrap();
    assert_eq!(
        (moved.base_url.as_str(), moved.owner.clone()),
        (other.as_str(), alice.clone())
    );
    // Bob cannot move it either, nor read alice's answer back with her operation id.
    let e = svc
        .update_key(req(
            mv(Some("sk-bobs-own-secret-0001")),
            "bob-token",
            Some("88888888-8888-4888-8888-888888888888"),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    let e = svc
        .update_key(req(
            mv(Some("sk-fresh-secret-value-ab12")),
            "bob-token",
            Some("77777777-7777-4777-8777-777777777777"),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    // A vendor provider takes no base_url at all.
    let mut bad = create_msg(alice, "vendor", &f.fake.url());
    bad.provider = wire::AiProvider::Openai as i32;
    let e = svc
        .create_key(req(
            bad,
            "alice-token",
            Some("99999999-9999-4999-8999-999999999999"),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    assert!(e.message().contains("base_url"), "{}", e.message());
}

#[tokio::test(flavor = "current_thread")]
async fn the_hubs_claim_decides_who_administers_an_organisations_or_the_instances_keys() {
    use asc_rpc::forwarded::{ForwardedVerifier, Role};
    let f = fx().await;
    let v = ForwardedVerifier::new(vec![8u8; 32]).unwrap();
    let authz: Arc<dyn Authorizer> = Arc::new(crate::authz::forwarded::ForwardedAuthorizer::new(
        f.authorizer.clone(),
        v.clone(),
    ));
    let svc = AiService::new(f.hub.clone(), authz, ServerInfo::default());
    let now = crate::ai::clock::now();
    let call = |owner: Option<wire::OwnerRef>, subject: &str, user: &str, role: Role, at: i64| {
        let mut r = req(wire::ListAiKeysRequest { owner }, "host-token", None);
        for (k, val) in v.sign(user, role, subject, at) {
            r.metadata_mut().append(k, val.parse().unwrap());
        }
        svc.list_keys(r)
    };
    let acme = owner_ref(wire::OwnerKind::Org, "acme");
    // The organisation's owner, not a member who only writes, and not a claim for another org.
    assert!(
        call(acme.clone(), "org:acme", "7:alice", Role::Admin, now)
            .await
            .is_ok()
    );
    for (subject, role) in [("org:acme", Role::Write), ("org:zed", Role::Admin)] {
        let e = call(acme.clone(), subject, "8:bob", role, now)
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::NotFound, "{subject}");
    }
    // The instance's keys are the site administrator's.
    let inst = owner_ref(wire::OwnerKind::Instance, "");
    assert!(
        call(inst.clone(), "instance", "1:root", Role::Admin, now)
            .await
            .is_ok()
    );
    assert!(
        call(inst.clone(), "org:acme", "7:alice", Role::Admin, now)
            .await
            .is_err()
    );
    // A person's own keys by the account claim; another person's login is not that account.
    let alice = owner_ref(wire::OwnerKind::User, "alice");
    assert!(
        call(alice.clone(), "user:alice", "7:alice", Role::Write, now)
            .await
            .is_ok()
    );
    assert!(
        call(alice.clone(), "user:alice", "8:bob", Role::Write, now)
            .await
            .is_err()
    );
    // A stale claim is refused rather than read as the service's own power.
    assert!(
        call(acme, "org:acme", "7:alice", Role::Admin, now - 600)
            .await
            .is_err()
    );
}
