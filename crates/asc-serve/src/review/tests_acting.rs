//! The hub's forwarded claim on the `Review` RPCs, and the helpers later families of host-made
//! calls reuse (`claimkit`).

use std::sync::Arc;

use asc_rpc::auth::{Access, Authorizer, BearerToken, Credential, StaticTokens};
use asc_rpc::forwarded::{ForwardedVerifier, Role};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::review_server::Review;
use bytes::Bytes;
use tonic::{Code, Request};
use uuid::Uuid;

use super::rpc::ReviewService;
use super::tests::{ALICE, Fx, cbytes, fx, repo_ref, req};
use super::{Reviews, handle_of};
use crate::authz::forwarded::ForwardedAuthorizer;

pub(crate) mod claimkit {
    use super::*;

    pub(crate) const HOST: &str = "host-token";

    /// An authorizer that knows the host service credential, the test users and the claim key.
    pub(crate) fn authorizer() -> (Arc<dyn Authorizer>, ForwardedVerifier) {
        let v = ForwardedVerifier::new(vec![7u8; 32]).unwrap();
        let inner: Arc<dyn Authorizer> = Arc::new(
            StaticTokens::new()
                .with_token(HOST, crate::authz::HOST_SERVICE_PRINCIPAL, Access::Admin)
                .with_token(ALICE, "42:alice", Access::Read)
                .with_token(super::super::tests::CAROL, "44:carol", Access::Write)
                .with_token(super::super::tests::ADMIN, "45:root", Access::Admin),
        );
        (Arc::new(ForwardedAuthorizer::new(inner, v.clone())), v)
    }

    pub(crate) fn now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
    }

    /// A request from the host service carrying a claim for `user` with `role` on `subject`.
    pub(crate) fn host<T>(
        v: &ForwardedVerifier,
        m: T,
        user: &str,
        role: Role,
        subject: &str,
        op: bool,
    ) -> Request<T> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(HOST).unwrap()));
        for (k, val) in v.sign(user, role, subject, now()) {
            r.metadata_mut().append(k, val.parse().unwrap());
        }
        if op {
            r.metadata_mut()
                .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
        }
        r
    }

    /// The `(actor_name, on_behalf_name)` of the audit events of `action`, oldest first.
    pub(crate) async fn audited(pool: &sqlx::AnyPool, action: &str) -> Vec<(String, String)> {
        use sqlx::Row;
        sqlx::query(
            "SELECT actor_name, on_behalf_name FROM audit_events WHERE action = $1 ORDER BY seq",
        )
        .bind(action.to_owned())
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get("actor_name"), r.get("on_behalf_name")))
        .collect()
    }
}

use claimkit::{audited, host};

fn svc(f: &Fx, a: Arc<dyn Authorizer>) -> ReviewService {
    ReviewService::new(
        Arc::new(Reviews::new(f.reg.clone())),
        a,
        ServerInfo::default(),
    )
}

fn add(body: &str, external: &str, thread: u64) -> wire::AddCommentRequest {
    wire::AddCommentRequest {
        repo: repo_ref(),
        change: cbytes(1),
        thread_id: thread,
        anchor: (thread == 0).then(|| wire::ThreadAnchor {
            path: Bytes::from_static(b"f"),
            start_line: 1,
            ..Default::default()
        }),
        body: body.into(),
        external_id: external.into(),
    }
}

#[tokio::test]
async fn a_hub_comment_is_written_as_the_person_and_audited_with_the_relay() {
    let f = fx().await;
    let (a, v) = claimkit::authorizer();
    let s = svc(&f, a);
    let r = s
        .add_comment(host(
            &v,
            add("from the hub", "hub:c:1", 0),
            "42:alice",
            Role::Read,
            "o/r",
            true,
        ))
        .await
        .unwrap()
        .into_inner();
    let t = r.thread.unwrap();
    assert_eq!(t.comments[0].author, "alice");
    assert_eq!(t.comments[0].external_id, "hub:c:1");
    assert_eq!(handle_of("42:alice").as_deref(), Some("alice"));
    let ev = audited(f.reg.pool(), "review.comment.added").await;
    assert!(ev.contains(&("alice".to_owned(), "host".to_owned())), "{ev:?}");
}

#[tokio::test]
async fn mirroring_a_comment_twice_stores_it_once_and_an_id_cannot_cross_changes() {
    let f = fx().await;
    let (a, v) = claimkit::authorizer();
    let s = svc(&f, a);
    let first = s
        .add_comment(host(&v, add("x", "hub:c:9", 0), "42:alice", Role::Write, "o/r", true))
        .await
        .unwrap()
        .into_inner();
    let again = s
        .add_comment(host(&v, add("x", "hub:c:9", 0), "42:alice", Role::Write, "o/r", true))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.comment_id, again.comment_id);
    assert_eq!(again.thread.unwrap().comments.len(), 1);
    // The same id on another change is refused.
    let mut other = add("x", "hub:c:9", 0);
    other.change = cbytes(2);
    let e = s
        .add_comment(host(&v, other, "43:bob", Role::Write, "o/r", true))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::AlreadyExists);
    // A malformed id is refused.
    let e = s
        .add_comment(host(&v, add("x", "bad id!", 0), "42:alice", Role::Write, "o/r", true))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn the_claim_is_the_whole_authority_and_fails_closed() {
    let f = fx().await;
    let (a, v) = claimkit::authorizer();
    let s = svc(&f, a);
    let list = |role, subject: &str| {
        host(
            &v,
            wire::ListThreadsRequest {
                repo: repo_ref(),
                change: cbytes(1),
                ..Default::default()
            },
            "42:alice",
            role,
            subject,
            false,
        )
    };
    assert!(s.list_threads(list(Role::Read, "o/r")).await.is_ok());
    // Another tenant's claim is no claim here.
    assert_eq!(
        s.list_threads(list(Role::Admin, "o/other"))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    // A review request needs write: a read claim is refused although the host credential is admin.
    let ask = |role| {
        host(
            &v,
            wire::RequestReviewRequest {
                repo: repo_ref(),
                change: cbytes(1),
                add: vec!["bob".into()],
                ..Default::default()
            },
            "42:alice",
            role,
            "o/r",
            false,
        )
    };
    assert_eq!(
        s.request_review(ask(Role::Read)).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert!(s.request_review(ask(Role::Write)).await.is_ok());
    let ev = audited(f.reg.pool(), "review.requested").await;
    assert!(ev.contains(&("alice".to_owned(), "host".to_owned())), "{ev:?}");
    // A forged claim (another key) is refused, never treated as system work.
    let forged = ForwardedVerifier::new(vec![1u8; 32]).unwrap();
    let r = host(
        &forged,
        wire::ListThreadsRequest {
            repo: repo_ref(),
            change: cbytes(1),
            ..Default::default()
        },
        "42:alice",
        Role::Admin,
        "o/r",
        false,
    );
    assert_eq!(
        s.list_threads(r).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    // A resolve by a read claim of a stranger is refused; by a write claim it works and is audited.
    let c = s
        .add_comment(host(&v, add("t", "", 0), "42:alice", Role::Read, "o/r", true))
        .await
        .unwrap()
        .into_inner();
    let tid = c.thread.unwrap().id;
    let res = |user: &str, role| {
        host(
            &v,
            wire::ResolveThreadRequest {
                repo: repo_ref(),
                thread_id: tid,
                resolved: true,
            },
            user,
            role,
            "o/r",
            false,
        )
    };
    assert_eq!(
        s.resolve_thread(res("46:dave", Role::Read))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert!(s.resolve_thread(res("46:dave", Role::Write)).await.is_ok());
    assert!(
        audited(f.reg.pool(), "review.thread.resolved")
            .await
            .contains(&("dave".to_owned(), "host".to_owned()))
    );
}

#[tokio::test]
async fn a_caller_that_is_not_the_host_cannot_use_a_claim() {
    let f = fx().await;
    let (a, v) = claimkit::authorizer();
    let s = svc(&f, a);
    // Alice's own token with a claim attached: the claim is ignored and her own rights decide.
    let mut r = req(
        wire::RequestReviewRequest {
            repo: repo_ref(),
            change: cbytes(1),
            add: vec!["bob".into()],
            ..Default::default()
        },
        ALICE,
        false,
    );
    for (k, val) in v.sign("45:root", Role::Admin, "o/r", claimkit::now()) {
        r.metadata_mut().append(k, val.parse().unwrap());
    }
    assert_eq!(
        s.request_review(r).await.unwrap_err().code(),
        Code::PermissionDenied
    );
}
