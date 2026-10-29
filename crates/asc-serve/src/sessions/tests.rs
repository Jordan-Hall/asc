//! Tests of the sessions service.

use std::sync::Arc;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::sessions_server::Sessions;
use sqlx::AnyPool;
use tonic::Code;

use super::SessionsHub;
use super::rpc::SessionsService;
use crate::ai::clock;
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

struct Fx {
    k: Kit,
    svc: SessionsService,
    audit: Arc<Recorder>,
}

async fn fx() -> Fx {
    let k = kit().await;
    let audit = Arc::new(Recorder::default());
    let hub = SessionsHub::new(k.pool.clone(), audit.clone());
    let svc = SessionsService::new(hub, k.authz.clone(), ServerInfo::default());
    Fx { k, svc, audit }
}

async fn token(p: &AnyPool, id: &str, user: &str, label: &str, created: i64) {
    sqlx::query("INSERT INTO auth_tokens (id, kind, principal, digest, label, created) VALUES ($1, 'pat', $2, $3, $4, $5)")
        .bind(id.to_owned())
        .bind(format!("pat:{user}"))
        .bind(vec![0u8; 32])
        .bind(label.to_owned())
        .bind(created)
        .execute(p)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO auth_token_grants (token_id, repo, access) VALUES ($1, 'acme/*', 'write')",
    )
    .bind(id.to_owned())
    .execute(p)
    .await
    .unwrap();
}

async fn ssh(p: &AnyPool, id: &str, user: &str, fp: u8) {
    sqlx::query(
        "INSERT INTO ssh_keys (id, kind, principal, algo, public_key, fingerprint, title, created, last_used) \
         VALUES ($1, 'user', $2, 'ssh-ed25519', $3, $4, 'laptop', 100, 200)",
    )
    .bind(id.to_owned())
    .bind(user.to_owned())
    .bind(vec![fp; 8])
    .bind(vec![fp; 32])
    .execute(p)
    .await
    .unwrap();
}

async fn agent_token(p: &AnyPool, id: &str, created_by: &str) {
    sqlx::query(
        "INSERT INTO agent_tokens (id, repo, name, agent, secret_sha256, secret_hint, created_by, owner_id, created, expires_at) \
         VALUES ($1, 'acme/app', 'ci-bot', 'fixer', $1, 'h', $2, 1, 300, $3)",
    )
    .bind(id.to_owned())
    .bind(created_by.to_owned())
    .bind(clock::now() + 86_400)
    .execute(p)
    .await
    .unwrap();
}

async fn seed(f: &Fx) {
    token(&f.k.pool, "t1", "alice", "laptop cli", 400).await;
    token(&f.k.pool, "t2", "alice", "ci", 500).await;
    token(&f.k.pool, "t3", "bob", "bobs", 450).await;
    ssh(&f.k.pool, "k1", "alice", 1).await;
    ssh(&f.k.pool, "k2", "bob", 2).await;
    agent_token(&f.k.pool, "a1", "pat:alice").await;
    agent_token(&f.k.pool, "a2", "7:alice").await;
    agent_token(&f.k.pool, "a3", "8:alicia").await;
}

fn list_req(login: &str) -> wire::ListSessionsRequest {
    wire::ListSessionsRequest {
        login: login.into(),
    }
}

fn as_user<T>(f: &Fx, msg: T, login: &str, role: Role, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        &format!("7:{login}"),
        role,
        &format!("user:{login}"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_person_lists_only_their_own_credentials_without_secrets() {
    let f = fx().await;
    seed(&f).await;
    let r = f
        .svc
        .list_sessions(req(list_req(""), "alice-token", None))
        .await
        .unwrap()
        .into_inner();
    let ids: Vec<_> = r.sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids.len(), 5, "{ids:?}");
    for want in ["tok:t1", "tok:t2", "ssh:k1", "agt:a1", "agt:a2"] {
        assert!(ids.contains(&want), "{want}");
    }
    assert!(
        !ids.contains(&"tok:t3") && !ids.contains(&"ssh:k2") && !ids.contains(&"agt:a3"),
        "other people's credentials are not listed"
    );
    let ci = r.sessions.iter().find(|s| s.id == "tok:t2").unwrap();
    assert_eq!(
        (ci.label.as_str(), ci.scope.as_str()),
        ("ci", "acme/* write")
    );
    assert_eq!(
        r.sessions
            .iter()
            .find(|s| s.id == "ssh:k1")
            .unwrap()
            .last_used,
        200
    );
    // By the hub's claim: the account's own, read is enough.
    let c = f
        .svc
        .list_sessions(as_user(&f, list_req(""), "alice", Role::Read, 1))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.sessions.len(), 5);
    // Another login by a person, and by a claim of another account: refused.
    for r in [
        req(list_req("bob"), "alice-token", None),
        as_user(&f, list_req("bob"), "alice", Role::Admin, 2),
    ] {
        assert_eq!(
            f.svc.list_sessions(r).await.unwrap_err().code(),
            Code::PermissionDenied
        );
    }
    // The host's claim for the instance sees any account (the Admin user page).
    let admin = claim(
        req(list_req("bob"), "host-token", None),
        &f.k,
        "1:root",
        Role::Admin,
        "instance",
    );
    assert_eq!(
        f.svc
            .list_sessions(admin)
            .await
            .unwrap()
            .into_inner()
            .sessions
            .len(),
        2
    );
    assert_eq!(
        f.svc
            .list_sessions(req(list_req("bad name"), "host-token", None))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
}

#[tokio::test(flavor = "current_thread")]
async fn revoking_is_immediate_audited_and_never_reaches_another_persons_credential() {
    let f = fx().await;
    seed(&f).await;
    let revoke = |login: &str, id: &str, n| {
        f.svc.revoke_session(as_user(
            &f,
            wire::RevokeSessionRequest {
                login: login.into(),
                id: id.into(),
            },
            "alice",
            Role::Write,
            n,
        ))
    };
    revoke("", "tok:t1", 1).await.unwrap();
    let left = f
        .svc
        .list_sessions(req(list_req(""), "alice-token", None))
        .await
        .unwrap()
        .into_inner();
    assert!(!left.sessions.iter().any(|s| s.id == "tok:t1"));
    // Bob's token and key by alice: not found, and still alive for bob.
    for id in ["tok:t3", "ssh:k2", "agt:a3", "tok:../x", "nope", "tok:"] {
        assert_eq!(
            revoke("", id, 2).await.unwrap_err().code(),
            Code::NotFound,
            "{id}"
        );
    }
    let bobs = f
        .svc
        .list_sessions(as_user(&f, list_req(""), "bob", Role::Read, 3))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(bobs.sessions.len(), 2);
    // Revoking again is not found; a read claim cannot revoke.
    assert_eq!(
        revoke("", "tok:t1", 4).await.unwrap_err().code(),
        Code::NotFound
    );
    let e = f
        .svc
        .revoke_session(as_user(
            &f,
            wire::RevokeSessionRequest {
                login: String::new(),
                id: "tok:t2".into(),
            },
            "alice",
            Role::Read,
            5,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let rec = f.audit.of("session.revoked");
    assert_eq!(
        (
            rec.len(),
            rec[0].1.as_str(),
            rec[0].2.as_deref(),
            rec[0].3.as_str()
        ),
        (1, "7:alice", Some("host-service"), "tok:t1")
    );
    // The token row carries the revocation the token store reads.
    let revoked: Option<i64> =
        sqlx::query_scalar("SELECT CAST(revoked AS BIGINT) FROM auth_tokens WHERE id = 't1'")
            .fetch_one(&f.k.pool)
            .await
            .unwrap();
    assert!(revoked.is_some());
    // A replayed operation id does not revoke twice.
    let again = f
        .svc
        .revoke_session(as_user(
            &f,
            wire::RevokeSessionRequest {
                login: String::new(),
                id: "tok:t2".into(),
            },
            "alice",
            Role::Write,
            6,
        ))
        .await;
    assert!(again.is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn sign_out_everywhere_else_keeps_the_one_in_use() {
    let f = fx().await;
    seed(&f).await;
    let others = |keep: &str, n| {
        f.svc.revoke_other_sessions(as_user(
            &f,
            wire::RevokeOtherSessionsRequest {
                login: String::new(),
                keep_id: keep.into(),
            },
            "alice",
            Role::Write,
            n,
        ))
    };
    assert_eq!(
        others("garbage", 1).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    let n = others("tok:t2", 2).await.unwrap().into_inner().revoked;
    assert_eq!(n, 4);
    let left = f
        .svc
        .list_sessions(req(list_req(""), "alice-token", None))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        left.sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["tok:t2"]
    );
    // Bob and the other "alice"-like creator are untouched.
    let bobs = f
        .svc
        .list_sessions(as_user(&f, list_req(""), "bob", Role::Read, 3))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(bobs.sessions.len(), 2);
    let alicia: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_tokens WHERE id = 'a3' AND revoked_at = 0",
    )
    .fetch_one(&f.k.pool)
    .await
    .unwrap();
    assert_eq!(alicia, 1);
    assert_eq!(f.audit.of("session.revoked").len(), 4);
    // Nothing to sign out: zero, not an error.
    assert_eq!(others("tok:t2", 4).await.unwrap().into_inner().revoked, 0);
}
