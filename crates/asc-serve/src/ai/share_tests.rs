//! Tests of AI key sharing and the cap rules.

use std::sync::Arc;

use asc_rpc::auth::Authorizer;
use asc_rpc::forwarded::{ForwardedVerifier, Role};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::ai_key_shares_server::AiKeyShares;
use tonic::Code;

use super::owner::Owner;
use super::share_rpc::AiKeySharesService;
use super::tests::{Fx, active_key, fx, user};
use super::{KeyPatch, clock};
use crate::settings::testkit::{Recorder, claim_with, op, req};

struct S {
    f: Fx,
    svc: AiKeySharesService,
    v: ForwardedVerifier,
    audit: Arc<Recorder>,
}

async fn setup() -> S {
    let f = fx().await;
    let v = ForwardedVerifier::new(vec![8u8; 32]).unwrap();
    let authz: Arc<dyn Authorizer> = Arc::new(crate::authz::forwarded::ForwardedAuthorizer::new(
        f.authorizer.clone(),
        v.clone(),
    ));
    let audit = Arc::new(Recorder::default());
    f.hub.set_audit(audit.clone());
    let svc = AiKeySharesService::new(f.hub.clone(), authz, ServerInfo::default());
    S { f, svc, v, audit }
}

fn grantee(kind: wire::OwnerKind, name: &str) -> Option<wire::OwnerRef> {
    Some(wire::OwnerRef {
        kind: kind as i32,
        name: name.into(),
    })
}

fn as_user<T>(s: &S, msg: T, login: &str, n: u32) -> tonic::Request<T> {
    claim_with(
        req(msg, "host-token", Some(&op(n))),
        &s.v,
        &format!("7:{login}"),
        Role::Write,
        &format!("user:{login}"),
    )
}

fn as_org<T>(s: &S, msg: T, org: &str, role: Role, n: u32) -> tonic::Request<T> {
    claim_with(
        req(msg, "host-token", Some(&op(n))),
        &s.v,
        "9:owner",
        role,
        &format!("org:{org}"),
    )
}

async fn share(
    s: &S,
    key: &str,
    g: Option<wire::OwnerRef>,
    see: bool,
    n: u32,
) -> Result<wire::AiKeyShare, tonic::Status> {
    s.svc
        .share_key(as_user(
            s,
            wire::ShareAiKeyRequest {
                key_id: key.into(),
                grantee: g,
                allow_use: true,
                see_spend: see,
            },
            "alice",
            n,
        ))
        .await
        .map(|r| r.into_inner().share.unwrap())
}

async fn usage(s: &S, key: &str, repo: &str, cost: i64) {
    sqlx::query(
        "INSERT INTO ai_usage (key_id, owner_kind, owner_name, day, repo, agent, model, requests, input_tokens, output_tokens, cost_micro_usd) \
         VALUES ($1, 1, 'alice', $2, $3, 'fixer', 'm', 2, 10, 5, $4)",
    )
    .bind(key.to_owned())
    .bind(clock::day(clock::now()))
    .bind(repo.to_owned())
    .bind(cost)
    .execute(s.f.hub.store.pool())
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_owner_shares_and_other_tenants_get_not_found() {
    let s = setup().await;
    let key = active_key(&s.f, &user("alice"), "mine").await;
    let sh = share(&s, &key.id, grantee(wire::OwnerKind::Org, "acme"), false, 1)
        .await
        .unwrap();
    assert!(sh.allow_use && !sh.see_spend);
    assert_eq!(sh.grantee.unwrap().name, "acme");
    // The same grantee again changes the permissions; it does not add a second share.
    let again = share(&s, &key.id, grantee(wire::OwnerKind::Org, "Acme"), true, 2)
        .await
        .unwrap();
    assert_eq!((again.id.as_str(), again.see_spend), (sh.id.as_str(), true));
    let list = s
        .svc
        .list_shares(as_user(
            &s,
            wire::ListAiKeySharesRequest {
                key_id: key.id.clone(),
            },
            "alice",
            3,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.shares.len(), 1);
    // Bob cannot share, list or revoke alice's key, and cannot tell it exists.
    for r in [
        s.svc
            .share_key(as_user(
                &s,
                wire::ShareAiKeyRequest {
                    key_id: key.id.clone(),
                    grantee: grantee(wire::OwnerKind::Org, "zed"),
                    allow_use: true,
                    see_spend: false,
                },
                "bob",
                4,
            ))
            .await
            .map(drop),
        s.svc
            .list_shares(as_user(
                &s,
                wire::ListAiKeySharesRequest {
                    key_id: key.id.clone(),
                },
                "bob",
                5,
            ))
            .await
            .map(drop),
        s.svc
            .revoke_share(as_user(
                &s,
                wire::RevokeAiKeyShareRequest {
                    key_id: key.id.clone(),
                    share_id: sh.id.clone(),
                },
                "bob",
                6,
            ))
            .await
            .map(drop),
        s.svc
            .share_key(as_user(
                &s,
                wire::ShareAiKeyRequest {
                    key_id: "ak_nothing".into(),
                    grantee: grantee(wire::OwnerKind::Org, "zed"),
                    allow_use: true,
                    see_spend: false,
                },
                "alice",
                7,
            ))
            .await
            .map(drop),
    ] {
        assert_eq!(r.unwrap_err().code(), Code::NotFound);
    }
    // Bad grantees, self-share, an instance or repository-owned key.
    for g in [
        None,
        grantee(wire::OwnerKind::Instance, ""),
        grantee(wire::OwnerKind::User, "alice"),
        grantee(wire::OwnerKind::Org, "bad name"),
        grantee(wire::OwnerKind::Repo, "noslash"),
    ] {
        assert_eq!(
            share(&s, &key.id, g, false, 8).await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }
    let repo_key = active_key(&s.f, &Owner::Repo("acme/app".into()), "repokey").await;
    let e = s
        .svc
        .share_key(as_org(
            &s,
            wire::ShareAiKeyRequest {
                key_id: repo_key.id.clone(),
                grantee: grantee(wire::OwnerKind::Org, "zed"),
                allow_use: true,
                see_spend: false,
            },
            "acme",
            Role::Admin,
            9,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    // The audit log names the person and the host.
    let rec = s.audit.of("ai.key.shared");
    assert_eq!(
        (rec.len(), rec[0].1.as_str(), rec[0].2.as_deref()),
        (2, "7:alice", Some("host-service"))
    );
    // Revoking removes it.
    s.svc
        .revoke_share(as_user(
            &s,
            wire::RevokeAiKeyShareRequest {
                key_id: key.id.clone(),
                share_id: sh.id.clone(),
            },
            "alice",
            10,
        ))
        .await
        .unwrap();
    assert!(s.f.hub.shares(&key.id).await.unwrap().is_empty());
    assert_eq!(s.audit.of("ai.key.unshared").len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_shared_key_serves_the_grantees_runs_and_only_theirs() {
    let s = setup().await;
    let key = active_key(&s.f, &user("alice"), "mine").await;
    let chain_acme = Owner::chain("acme/app", Some("carol"));
    let chain_zed = Owner::chain("zed/app", Some("dave"));
    assert!(
        s.f.hub.resolve(&chain_acme, None).await.is_err(),
        "no key before sharing"
    );
    share(&s, &key.id, grantee(wire::OwnerKind::Org, "acme"), false, 1)
        .await
        .unwrap();
    assert_eq!(s.f.hub.resolve(&chain_acme, None).await.unwrap().id, key.id);
    assert_eq!(
        s.f.hub
            .resolve(&chain_acme, Some(&key.id))
            .await
            .unwrap()
            .id,
        key.id
    );
    // Another organisation's run cannot use it, named or not.
    assert!(s.f.hub.resolve(&chain_zed, None).await.is_err());
    assert!(s.f.hub.resolve(&chain_zed, Some(&key.id)).await.is_err());
    // A share without use permission does not serve a run.
    s.svc
        .share_key(as_user(
            &s,
            wire::ShareAiKeyRequest {
                key_id: key.id.clone(),
                grantee: grantee(wire::OwnerKind::Org, "acme"),
                allow_use: false,
                see_spend: true,
            },
            "alice",
            2,
        ))
        .await
        .unwrap();
    assert!(s.f.hub.resolve(&chain_acme, None).await.is_err());
    // A key the grantee owns itself still wins over a shared one.
    s.svc
        .share_key(as_user(
            &s,
            wire::ShareAiKeyRequest {
                key_id: key.id.clone(),
                grantee: grantee(wire::OwnerKind::Org, "acme"),
                allow_use: true,
                see_spend: true,
            },
            "alice",
            3,
        ))
        .await
        .unwrap();
    let own = active_key(&s.f, &Owner::Org("acme".into()), "acmes").await;
    assert_eq!(s.f.hub.resolve(&chain_acme, None).await.unwrap().id, own.id);
    // Deleting the key takes its shares with it.
    s.f.hub.delete_key(&key.id).await.unwrap();
    assert!(s.f.hub.shares(&key.id).await.unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn usage_is_attributed_to_the_most_specific_sharer_and_the_grantee_sees_no_secret() {
    let s = setup().await;
    let key = active_key(&s.f, &user("alice"), "mine").await;
    share(&s, &key.id, grantee(wire::OwnerKind::Org, "acme"), true, 1)
        .await
        .unwrap();
    share(
        &s,
        &key.id,
        grantee(wire::OwnerKind::Repo, "zed/lab"),
        false,
        2,
    )
    .await
    .unwrap();
    usage(&s, &key.id, "acme/web", 3_000_000).await;
    usage(&s, &key.id, "acme/api", 1_000_000).await;
    usage(&s, &key.id, "zed/lab", 500_000).await;
    usage(&s, &key.id, "alice/own", 200_000).await;
    let r = s
        .svc
        .share_usage(as_user(
            &s,
            wire::AiKeyShareUsageRequest {
                key_id: key.id.clone(),
                since: 0,
                until: 0,
            },
            "alice",
            3,
        ))
        .await
        .unwrap()
        .into_inner();
    let by: Vec<_> = r
        .rows
        .iter()
        .map(|x| {
            (
                x.grantee
                    .as_ref()
                    .map(|g| g.name.clone())
                    .unwrap_or_default(),
                x.cost_micro_usd,
            )
        })
        .collect();
    // The owner's own use is last; shares are in the order they were made (same second here).
    assert_eq!(by.last().unwrap(), &(String::new(), 200_000));
    let mut by = by;
    by.sort();
    assert_eq!(
        by,
        [
            (String::new(), 200_000),
            ("acme".to_owned(), 4_000_000),
            ("zed/lab".to_owned(), 500_000),
        ]
    );
    // Bob cannot read the usage of alice's key.
    let e = s
        .svc
        .share_usage(as_user(
            &s,
            wire::AiKeyShareUsageRequest {
                key_id: key.id.clone(),
                since: 0,
                until: 0,
            },
            "bob",
            4,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    // The organisation's owners see the key's label and, with see_spend, their own cost: nothing else.
    let mine = s
        .svc
        .list_shared_with_me(as_org(
            &s,
            wire::ListSharedAiKeysRequest {
                grantee: grantee(wire::OwnerKind::Org, "acme"),
            },
            "acme",
            Role::Admin,
            5,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.keys.len(), 1);
    let k = &mine.keys[0];
    assert_eq!(
        (
            k.label.as_str(),
            k.key_id.as_str(),
            k.usable,
            k.month_spent_micro_usd
        ),
        ("mine", key.id.as_str(), true, 4_000_000)
    );
    assert_eq!(k.owner.as_ref().unwrap().name, "alice");
    // A repository without see_spend sees no figure. A member (write) and another org are refused.
    let repo = s
        .svc
        .list_shared_with_me(claim_with(
            req(
                wire::ListSharedAiKeysRequest {
                    grantee: grantee(wire::OwnerKind::Repo, "zed/lab"),
                },
                "host-token",
                None,
            ),
            &s.v,
            "9:owner",
            Role::Admin,
            "zed/lab",
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(repo.keys[0].month_spent_micro_usd, 0);
    for (org, role) in [("acme", Role::Write), ("zed", Role::Admin)] {
        let e = s
            .svc
            .list_shared_with_me(as_org(
                &s,
                wire::ListSharedAiKeysRequest {
                    grantee: grantee(wire::OwnerKind::Org, "acme"),
                },
                org,
                role,
                6,
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::PermissionDenied, "{org}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_warning_goes_out_once_a_month_and_needs_a_cap() {
    let s = setup().await;
    let key = active_key(&s.f, &user("alice"), "mine").await;
    let set = |warn, stop, n| {
        s.svc.set_key_caps(as_user(
            &s,
            wire::SetAiKeyCapsRequest {
                key_id: key.id.clone(),
                warn_percent: warn,
                stop_at_cap: stop,
            },
            "alice",
            n,
        ))
    };
    // A warning needs a cap to be a percentage of.
    assert_eq!(
        set(80, false, 1).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    s.f.hub
        .update_key(
            &key.id,
            KeyPatch {
                monthly_cap: Some(1_000_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        set(101, false, 2).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    let caps = set(80, true, 3).await.unwrap().into_inner().caps.unwrap();
    assert_eq!(
        (
            caps.warn_percent,
            caps.stop_at_cap,
            caps.monthly_cap_micro_usd,
            caps.warned
        ),
        (80, true, 1_000_000, false)
    );
    let key = s.f.hub.get_key(&key.id).await.unwrap();
    let now = clock::now();
    // 70% spent: no warning. 85%: one. Another call: still one.
    s.f.hub.add_spent(&key.id, 700_000, now).await;
    s.f.hub.check_warn(&key, now).await;
    assert!(s.audit.of("ai.key.cap.warning").is_empty());
    s.f.hub.add_spent(&key.id, 150_000, now).await;
    s.f.hub.check_warn(&key, now).await;
    s.f.hub.check_warn(&key, now).await;
    let w = s.audit.of("ai.key.cap.warning");
    assert_eq!(w.len(), 1);
    assert!(w[0].4.iter().any(|(k, v)| k == "percent" && v == "80"));
    let got = s
        .svc
        .get_key_caps(as_user(
            &s,
            wire::GetAiKeyCapsRequest {
                key_id: key.id.clone(),
            },
            "alice",
            4,
        ))
        .await
        .unwrap()
        .into_inner()
        .caps
        .unwrap();
    assert!(got.warned && got.month_spent_micro_usd == 850_000);
    // Another person cannot read or set them.
    let e = s
        .svc
        .get_key_caps(as_user(
            &s,
            wire::GetAiKeyCapsRequest {
                key_id: key.id.clone(),
            },
            "bob",
            5,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    // A new month warns again.
    sqlx::query("UPDATE ai_key_caps SET warned_month = '2000-01' WHERE key_id = $1")
        .bind(key.id.clone())
        .execute(s.f.hub.store.pool())
        .await
        .unwrap();
    s.f.hub.check_warn(&key, now).await;
    assert_eq!(s.audit.of("ai.key.cap.warning").len(), 2);
}
