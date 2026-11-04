//! Tests of feature routing and of key replacement.

use asc_rpc::server::wire;
use asc_rpc::services::ai_providers_server::AiProviders;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::fake::Reply;
use super::feature::Feature;
use super::owner::Owner;
use super::store::KeyState;
use super::tests::{PLANTED, active_key, fx, request, user};
use super::{AiError, AiHub, CallInfo};

const APP: CallInfo<'static> = CallInfo {
    repo: "acme/app",
    agent: "fixer",
    feature: Some(Feature::AgentRuns),
};

fn org() -> Owner {
    Owner::Org("acme".into())
}

fn repo() -> Owner {
    Owner::Repo("acme/app".into())
}

#[tokio::test(flavor = "current_thread")]
async fn a_replacement_key_is_tried_first_and_a_refused_one_changes_nothing() {
    let f = fx().await;
    let o = user("alice");
    let k = active_key(&f, &o, "work").await;
    let fresh = "sk-fresh-key-123456789";

    // The provider still only accepts the old key: the new one is refused and the old one kept.
    let t = f
        .hub
        .replace_key(&k.id, Zeroizing::new(fresh.into()))
        .await
        .unwrap();
    assert!(!t.replaced);
    assert_eq!(t.provider_status, "unauthorized");
    assert_eq!(
        f.fake.seen().last().unwrap().key,
        fresh,
        "the new key was tried"
    );
    let kept = f.hub.get_key(&k.id).await.unwrap();
    assert_eq!(kept.state, KeyState::Active, "the old key keeps working");
    assert_eq!(kept.key_version, k.key_version);
    assert_eq!(kept.key_hint, k.key_hint);
    assert!(kept.last_error.is_empty());

    // Once the provider accepts it, it replaces the old key and the key stays active.
    f.fake.set_key(fresh);
    let t = f
        .hub
        .replace_key(&k.id, Zeroizing::new(fresh.into()))
        .await
        .unwrap();
    assert!(t.replaced && t.provider_status == "ok");
    assert_eq!(t.key.state, KeyState::Active);
    assert_eq!(t.key.key_version, k.key_version + 1);
    assert_eq!(t.key.key_hint, "6789");
    assert!(t.key.tested_at > 0);
    let k = f.hub.get_key(&k.id).await.unwrap();
    f.hub
        .complete(
            &k,
            &CallInfo {
                repo: "",
                agent: "fixer",
                feature: None,
            },
            &request("model-x"),
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(f.fake.seen().last().unwrap().key, fresh);

    // A malformed key is refused before the provider sees it.
    let seen = f.fake.seen().len();
    assert!(matches!(
        f.hub
            .replace_key(&k.id, Zeroizing::new("short".into()))
            .await,
        Err(AiError::Invalid(_))
    ));
    assert_eq!(f.fake.seen().len(), seen);
}

#[tokio::test(flavor = "current_thread")]
async fn the_nearest_setting_chooses_the_key_of_a_feature() {
    let f = fx().await;
    let a = active_key(&f, &org(), "a").await;
    let b = active_key(&f, &org(), "b").await;

    // Nothing set: the first usable key, no setting.
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert_eq!(v.feature, Feature::AgentRuns);
    assert!(v.applying.is_none());
    assert!(v.key.is_some());

    // The organisation chooses b; its repository inherits it.
    f.hub
        .set_feature(&org(), Feature::AgentRuns, &b.id, 0, "pat:alice")
        .await
        .unwrap();
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    let s = v.applying.as_ref().unwrap();
    assert_eq!(s.owner, org());
    assert_eq!(s.updated_by, "pat:alice");
    assert_eq!(v.key.as_ref().unwrap().id, b.id);

    // The repository's own setting wins over the organisation's.
    f.hub
        .set_feature(&repo(), Feature::AgentRuns, &a.id, 0, "pat:bob")
        .await
        .unwrap();
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert_eq!(v.applying.as_ref().unwrap().owner, repo());
    assert_eq!(v.key.as_ref().unwrap().id, a.id);
    // Cleared, the organisation's applies again.
    f.hub
        .clear_feature(&repo(), Feature::AgentRuns)
        .await
        .unwrap();
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert_eq!(v.key.as_ref().unwrap().id, b.id);

    // Deleting the chosen key sends the setting back to the first usable key.
    f.hub.delete_key(&b.id).await.unwrap();
    let s = f
        .hub
        .own_setting(&org(), Feature::AgentRuns)
        .await
        .unwrap()
        .unwrap();
    assert!(s.key_id.is_empty());
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert_eq!(v.key.as_ref().unwrap().id, a.id);
}

#[tokio::test(flavor = "current_thread")]
async fn a_feature_can_only_choose_a_key_its_owner_can_use() {
    let f = fx().await;
    let bobs = active_key(&f, &user("bob"), "bobs").await;
    let orgs = active_key(&f, &org(), "orgs").await;
    assert!(matches!(
        f.hub
            .set_feature(&org(), Feature::AgentRuns, &bobs.id, 0, "pat:alice")
            .await,
        Err(AiError::NotFound)
    ));
    assert!(
        f.hub
            .own_setting(&org(), Feature::AgentRuns)
            .await
            .unwrap()
            .is_none(),
        "nothing is stored"
    );
    // A repository may choose a key of its organisation.
    f.hub
        .set_feature(&repo(), Feature::AgentRuns, &orgs.id, 0, "pat:alice")
        .await
        .unwrap();
    // An organisation may not choose a key of one of its repositories.
    let repos = active_key(&f, &repo(), "repos").await;
    assert!(matches!(
        f.hub
            .set_feature(&org(), Feature::AgentRuns, &repos.id, 0, "pat:alice")
            .await,
        Err(AiError::NotFound)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn a_feature_cap_pauses_the_feature_until_it_is_raised() {
    let f = fx().await;
    let k = active_key(&f, &org(), "work").await;
    let worst = f
        .hub
        .prices()
        .cost("model-x", AiHub::estimate(&request("model-x")))
        .unwrap();
    f.hub
        .set_feature(
            &repo(),
            Feature::AgentRuns,
            "",
            2200 + worst / 2,
            "pat:alice",
        )
        .await
        .unwrap();
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1000,
        output: 600,
    }); // 2200 micro-USD
    let c = CancellationToken::new();
    f.hub
        .complete(&k, &APP, &request("model-x"), false, &c)
        .await
        .unwrap();
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert_eq!(v.spent, 2200);
    assert!(!v.paused());
    assert_eq!(
        f.hub.month_spent(&k.id).await,
        2200,
        "the key counts it too"
    );

    // The next call's worst case does not fit under the feature's cap.
    let err = f
        .hub
        .complete(&k, &APP, &request("model-x"), false, &c)
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::CapReached("feature")), "{err:?}");
    assert_eq!(f.fake.completions().len(), 1);
    // Another feature, or a call for no feature, is not held by it.
    let other = CallInfo {
        feature: None,
        ..APP
    };
    f.hub
        .complete(&k, &other, &request("model-x"), false, &c)
        .await
        .unwrap();

    // At the cap the feature is paused until the next month.
    f.hub
        .set_feature(&repo(), Feature::AgentRuns, "", 2200, "pat:alice")
        .await
        .unwrap();
    assert!(
        f.hub
            .feature_paused("acme/app", Feature::AgentRuns)
            .await
            .unwrap()
    );
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert!(v.paused());
    let w = super::feature::wire_feature(&f.hub, v).await;
    assert!(w.paused);
    let now = super::clock::now();
    assert_eq!(
        w.resumes_at,
        super::clock::next_month_start(super::clock::month_start(now))
    );
    assert_eq!(w.month_spent_micro_usd, 2200);
    assert_eq!(w.monthly_cap_micro_usd, 2200);
    assert_eq!(w.feature, wire::AiFeature::AgentRuns as i32);

    // Raised, it runs again.
    f.hub
        .set_feature(&repo(), Feature::AgentRuns, "", 1_000_000, "pat:alice")
        .await
        .unwrap();
    assert!(
        !f.hub
            .feature_paused("acme/app", Feature::AgentRuns)
            .await
            .unwrap()
    );
    f.hub
        .complete(&k, &APP, &request("model-x"), false, &c)
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_feature_without_a_cap_still_counts_its_spending() {
    let f = fx().await;
    let k = active_key(&f, &org(), "work").await;
    f.hub
        .set_feature(&org(), Feature::AgentRuns, "", 0, "pat:alice")
        .await
        .unwrap();
    f.fake.set_default(Reply::Text {
        text: "x".into(),
        input: 1000,
        output: 600,
    });
    f.hub
        .complete(
            &k,
            &APP,
            &request("model-x"),
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    // The organisation's setting counts the spending of its repository.
    let v = &f.hub.features(&org()).await.unwrap()[0];
    assert_eq!(v.spent, 2200);
    let v = &f.hub.features(&repo()).await.unwrap()[0];
    assert_eq!(v.spent, 2200);
    assert!(!v.paused());
}

#[tokio::test(flavor = "current_thread")]
async fn the_service_sets_features_and_replaces_keys_for_the_owner_only() {
    use super::tests::{OP1, OP2, create_msg, owner_ref, req, service};
    let f = fx().await;
    let svc = service(&f);
    let alice = owner_ref(wire::OwnerKind::User, "alice");
    // A key remembers who added it.
    let made = svc
        .create_key(req(
            create_msg(alice.clone(), "svc", &f.fake.url()),
            "alice-token",
            Some("44444444-4444-4444-8444-444444444444"),
        ))
        .await
        .unwrap()
        .into_inner()
        .key
        .unwrap();
    assert_eq!(made.created_by, "pat:alice");
    assert_eq!(
        f.hub.get_key(&made.id).await.unwrap().created_by,
        "pat:alice"
    );
    let k = active_key(&f, &user("alice"), "work").await;
    assert!(
        k.created_by.is_empty(),
        "a key made by the hub itself names no one"
    );

    let set = wire::SetAiFeatureRequest {
        owner: alice.clone(),
        feature: wire::AiFeature::AgentRuns as i32,
        key_id: k.id.clone(),
        monthly_cap_micro_usd: 5_000_000,
        clear: false,
    };
    // Bob does not administer Alice's settings, and is told nothing exists.
    let err = svc
        .set_feature(req(set.clone(), "bob-token", Some(OP1)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
    // Without an op id a mutating call is refused.
    assert!(
        svc.set_feature(req(set.clone(), "alice-token", None))
            .await
            .is_err()
    );
    let got = svc
        .set_feature(req(set.clone(), "alice-token", Some(OP1)))
        .await
        .unwrap()
        .into_inner()
        .feature
        .unwrap();
    assert_eq!(got.key_id, k.id);
    assert_eq!(got.monthly_cap_micro_usd, 5_000_000);
    assert_eq!(got.updated_by, "pat:alice");
    assert_eq!(got.set_by, alice);
    assert_eq!(got.key.unwrap().id, k.id);
    let unknown = wire::SetAiFeatureRequest {
        feature: 99,
        ..set.clone()
    };
    assert_eq!(
        svc.set_feature(req(unknown, "alice-token", Some(OP2)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    let list = |token: &'static str| {
        svc.list_features(req(
            wire::ListAiFeaturesRequest {
                owner: alice.clone(),
            },
            token,
            None,
        ))
    };
    assert_eq!(
        list("bob-token").await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    let features = list("alice-token").await.unwrap().into_inner().features;
    assert_eq!(features.len(), 1);
    assert_eq!(features[0].monthly_cap_micro_usd, 5_000_000);

    // Replacing: Bob cannot, Alice can once the provider accepts the key.
    let fresh = "sk-fresh-key-123456789";
    let replace = wire::ReplaceAiKeyRequest {
        id: k.id.clone(),
        api_key: fresh.into(),
    };
    assert_eq!(
        svc.replace_key(req(replace.clone(), "bob-token", Some(OP2)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let r = svc
        .replace_key(req(replace.clone(), "alice-token", Some(OP2)))
        .await
        .unwrap()
        .into_inner();
    assert!(!r.replaced);
    assert_eq!(r.provider_status, "unauthorized");
    assert_ne!(r.key.as_ref().unwrap().key_hint, "6789");
    f.fake.set_key(fresh);
    let r = svc
        .replace_key(req(
            replace,
            "alice-token",
            Some("33333333-3333-4333-8333-333333333333"),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(r.replaced);
    let key = r.key.unwrap();
    assert_eq!(key.key_hint, "6789");
    assert_eq!(key.state, wire::AiKeyState::Active as i32);
    assert!(!format!("{key:?}").contains(fresh) && !format!("{key:?}").contains(PLANTED));
}
