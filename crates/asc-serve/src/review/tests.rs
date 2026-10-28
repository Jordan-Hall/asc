//! Tests of `Review`, and the fixture the `Inbox` and `Ci` tests share.

use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::id::ChangesetId;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::review_server::Review;
use bytes::Bytes;
use tonic::{Request, Status};
use uuid::Uuid;

use super::rpc::ReviewService;
use super::{Reviews, handle_of};
use crate::changes::index;
use crate::ci::rpc::CiService;
use crate::inbox::rpc::InboxService;
use crate::ingest::testutil::{commit, registry, repo};
use crate::registry::{Extensions, Registry, RepoEntry};

pub(crate) const ALICE: &str = "alice-token";
pub(crate) const BOB: &str = "bob-token";
pub(crate) const CAROL: &str = "carol-token";
pub(crate) const DAVE: &str = "dave-token";
pub(crate) const ADMIN: &str = "admin-token";
pub(crate) const DEPLOY: &str = "deploy-token";

pub(crate) struct Fx {
    pub reg: Arc<Registry>,
    pub e: Arc<RepoEntry>,
    pub review: ReviewService,
    pub inbox: InboxService,
    pub ci: CiService,
    pub css: Vec<ChangesetId>,
}

pub(crate) fn cid(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

pub(crate) fn cbytes(n: u8) -> Bytes {
    Bytes::copy_from_slice(cid(n).as_bytes())
}

pub(crate) fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "o/r".into(),
        id_mode: IdMode::Asc.to_wire(),
    })
}

pub(crate) fn req<T>(m: T, token: &str, op: bool) -> Request<T> {
    let mut r = Request::new(m);
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new(token).unwrap()));
    if op {
        r.metadata_mut()
            .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
    }
    r
}

/// Three changes: 1 (two versions) and 3 are alice's, 2 is bob's.
pub(crate) async fn fx() -> Fx {
    let reg = registry().await;
    let e = repo(&reg).await;
    let idx = index::ChangeIndex::new(reg.clone());
    let mut css: Vec<ChangesetId> = Vec::new();
    for i in 1..=3u8 {
        let parents: Vec<ChangesetId> = css.last().copied().into_iter().collect();
        let cs = commit(&e, &parents, &[("f", &[i])]).await;
        let mut tx = reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            e.key,
            &ChangeMeta::new(cs, cid(i), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        css.push(cs);
    }
    for (change, v, actor) in [
        (1u8, 1, "42:alice"),
        (1, 2, "42:alice"),
        (2, 1, "43:bob"),
        (3, 1, "pat:alice"),
    ] {
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
             VALUES ($1, $2, $3, $4, NULL, 1, $5, $6)",
        )
        .bind(e.ns.as_bytes().to_vec())
        .bind(cid(change).as_bytes().to_vec())
        .bind(v)
        .bind(css[usize::from(change) - 1].as_bytes().to_vec())
        .bind(actor)
        .bind(vec![change, u8::try_from(v).unwrap()])
        .execute(reg.pool())
        .await
        .unwrap();
    }
    let ext = Extensions::new();
    super::register(&ext, &reg);
    let hub: Arc<Reviews> = ext.review.get().unwrap().clone();
    let auth: Arc<dyn asc_rpc::auth::Authorizer> = Arc::new(
        StaticTokens::new()
            .with_token(ALICE, "42:alice", Access::Read)
            .with_token(BOB, "43:bob", Access::Read)
            .with_token(CAROL, "44:carol", Access::Write)
            .with_token(DAVE, "46:dave", Access::Read)
            .with_token(ADMIN, "45:root", Access::Admin)
            .with_token(DEPLOY, "deploy:o/r", Access::Read),
    );
    Fx {
        reg,
        e,
        review: ReviewService::new(hub.clone(), auth.clone(), ServerInfo::default()),
        inbox: InboxService::new(hub.clone(), auth.clone(), ServerInfo::default()),
        ci: CiService::new(hub, auth, ServerInfo::default()),
        css,
    }
}

pub(crate) fn code(s: &Status) -> tonic::Code {
    s.code()
}

fn anchor(path: &str, line: u32) -> Option<wire::ThreadAnchor> {
    Some(wire::ThreadAnchor {
        path: Bytes::copy_from_slice(path.as_bytes()),
        start_line: line,
        side: wire::DiffSide::New as i32,
        ..Default::default()
    })
}

async fn comment(
    f: &Fx,
    token: &str,
    change: u8,
    thread: u64,
    path: &str,
    body: &str,
) -> Result<wire::AddCommentResponse, Status> {
    f.review
        .add_comment(req(
            wire::AddCommentRequest {
                repo: repo_ref(),
                change: cbytes(change),
                thread_id: thread,
                anchor: if thread == 0 && !path.is_empty() {
                    anchor(path, 3)
                } else {
                    None
                },
                body: body.into(),
                ..Default::default()
            },
            token,
            true,
        ))
        .await
        .map(tonic::Response::into_inner)
}

pub(crate) async fn threads(
    f: &Fx,
    token: &str,
    change: u8,
    status: wire::ThreadStatus,
) -> wire::ListThreadsResponse {
    f.review
        .list_threads(req(
            wire::ListThreadsRequest {
                repo: repo_ref(),
                change: cbytes(change),
                status: status as i32,
                ..Default::default()
            },
            token,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
}

async fn resolve(
    f: &Fx,
    token: &str,
    thread: u64,
    resolved: bool,
) -> Result<wire::ReviewThread, Status> {
    f.review
        .resolve_thread(req(
            wire::ResolveThreadRequest {
                repo: repo_ref(),
                thread_id: thread,
                resolved,
            },
            token,
            false,
        ))
        .await
        .map(|r| r.into_inner().thread.unwrap())
}

pub(crate) async fn request_review(
    f: &Fx,
    token: &str,
    change: u8,
    add: &[&str],
    remove: &[&str],
) -> Result<wire::RequestReviewResponse, Status> {
    f.review
        .request_review(req(
            wire::RequestReviewRequest {
                repo: repo_ref(),
                change: cbytes(change),
                add: add.iter().map(|s| (*s).to_owned()).collect(),
                remove: remove.iter().map(|s| (*s).to_owned()).collect(),
            },
            token,
            false,
        ))
        .await
        .map(tonic::Response::into_inner)
}

#[test]
fn handles_come_from_verified_user_principals_only() {
    assert_eq!(handle_of("42:alice").as_deref(), Some("alice"));
    assert_eq!(handle_of("pat:bob").as_deref(), Some("bob"));
    for none in [
        "0:anonymous",
        "42:alice:7",
        "deploy:o/r",
        "ci:abc",
        "-2:actions",
        "pat:",
        "pat:a b",
        "",
    ] {
        assert_eq!(handle_of(none), None, "{none:?}");
    }
}

#[tokio::test]
async fn a_thread_is_opened_answered_resolved_and_reopened() {
    let f = fx().await;
    let first = comment(&f, BOB, 1, 0, "src/a.rs", "why this?")
        .await
        .unwrap();
    let t = first.thread.unwrap();
    assert_eq!((t.id, t.opened_by.as_str()), (1, "bob"));
    assert_eq!(t.status, wire::ThreadStatus::Open as i32);
    let a = t.anchor.unwrap();
    // The newest version when the anchor names none, and the side defaults to the new one.
    assert_eq!((a.version, a.start_line, a.end_line), (2, 3, 3));
    assert_eq!(a.path.as_ref(), b"src/a.rs");

    let reply = comment(&f, ALICE, 1, t.id, "", "because").await.unwrap();
    assert_eq!(reply.thread.unwrap().comments.len(), 2);

    let l = threads(&f, DAVE, 1, wire::ThreadStatus::Unspecified).await;
    assert_eq!((l.threads.len(), l.open_threads), (1, 1));
    let bodies: Vec<_> = l.threads[0]
        .comments
        .iter()
        .map(|c| (c.author.as_str(), c.body.as_str()))
        .collect();
    assert_eq!(bodies, [("bob", "why this?"), ("alice", "because")]);

    // The author resolves; a bystander cannot; the opener reopens; a writer resolves.
    let done = resolve(&f, ALICE, t.id, true).await.unwrap();
    assert_eq!(
        (done.status, done.resolved_by.as_str()),
        (wire::ThreadStatus::Resolved as i32, "alice")
    );
    assert!(done.resolved_at > 0);
    assert_eq!(
        code(&resolve(&f, DAVE, t.id, false).await.unwrap_err()),
        tonic::Code::PermissionDenied
    );
    let reopened = resolve(&f, BOB, t.id, false).await.unwrap();
    assert_eq!(
        (reopened.status, reopened.resolved_at),
        (wire::ThreadStatus::Open as i32, 0)
    );
    assert!(reopened.resolved_by.is_empty());
    assert_eq!(
        resolve(&f, CAROL, t.id, true).await.unwrap().resolved_by,
        "carol"
    );

    assert!(
        threads(&f, DAVE, 1, wire::ThreadStatus::Open)
            .await
            .threads
            .is_empty()
    );
    let resolved = threads(&f, DAVE, 1, wire::ThreadStatus::Resolved).await;
    assert_eq!((resolved.threads.len(), resolved.open_threads), (1, 0));
    // A comment on a resolved thread leaves it resolved.
    let again = comment(&f, BOB, 1, t.id, "", "thanks").await.unwrap();
    assert_eq!(
        again.thread.unwrap().status,
        wire::ThreadStatus::Resolved as i32
    );
}

#[tokio::test]
async fn a_retried_op_id_returns_the_first_answer() {
    let f = fx().await;
    let msg = wire::AddCommentRequest {
        repo: repo_ref(),
        change: cbytes(1),
        anchor: anchor("a", 1),
        body: "once".into(),
        ..Default::default()
    };
    let first = req(msg.clone(), BOB, true);
    let again = req(msg, BOB, false);
    let op = first.metadata().get("x-asc-op-id").unwrap().clone();
    let mut again = again;
    again.metadata_mut().insert("x-asc-op-id", op);
    let a = f.review.add_comment(first).await.unwrap().into_inner();
    let b = f.review.add_comment(again).await.unwrap().into_inner();
    assert_eq!(a.comment_id, b.comment_id);
    assert_eq!(
        threads(&f, BOB, 1, wire::ThreadStatus::Unspecified)
            .await
            .threads[0]
            .comments
            .len(),
        1
    );
}

#[tokio::test]
async fn bad_requests_and_unknowns_are_refused() {
    let f = fx().await;
    let bad = |r: Result<wire::AddCommentResponse, Status>| code(&r.unwrap_err());
    assert_eq!(
        bad(comment(&f, BOB, 1, 0, "a", "").await),
        tonic::Code::InvalidArgument
    );
    assert_eq!(
        bad(comment(&f, BOB, 1, 0, "a", "  \n").await),
        tonic::Code::InvalidArgument
    );
    assert_eq!(
        bad(comment(&f, BOB, 9, 0, "a", "x").await),
        tonic::Code::NotFound
    );
    assert_eq!(
        bad(comment(&f, BOB, 1, 77, "", "x").await),
        tonic::Code::NotFound
    );
    // A whole-change thread has no lines.
    let mut m = wire::AddCommentRequest {
        repo: repo_ref(),
        change: cbytes(1),
        body: "x".into(),
        anchor: Some(wire::ThreadAnchor {
            start_line: 4,
            ..Default::default()
        }),
        ..Default::default()
    };
    let e = f
        .review
        .add_comment(req(m.clone(), BOB, true))
        .await
        .unwrap_err();
    assert_eq!(code(&e), tonic::Code::InvalidArgument);
    // An unknown version is refused; a whole-change thread is fine.
    m.anchor = anchor("a", 1);
    m.anchor.as_mut().unwrap().version = 9;
    let e = f
        .review
        .add_comment(req(m.clone(), BOB, true))
        .await
        .unwrap_err();
    assert_eq!(code(&e), tonic::Code::InvalidArgument);
    m.anchor = None;
    let t = f
        .review
        .add_comment(req(m, BOB, true))
        .await
        .unwrap()
        .into_inner()
        .thread
        .unwrap();
    assert!(t.anchor.unwrap().path.is_empty());
    // A thread of change 1 cannot take a comment through change 2.
    assert_eq!(
        bad(comment(&f, BOB, 2, t.id, "", "x").await),
        tonic::Code::NotFound
    );
    // A mutating call needs an op id.
    let e = f
        .review
        .add_comment(req(
            wire::AddCommentRequest {
                repo: repo_ref(),
                change: cbytes(1),
                body: "x".into(),
                ..Default::default()
            },
            BOB,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&e), tonic::Code::InvalidArgument);
    assert_eq!(
        code(&resolve(&f, BOB, 99, true).await.unwrap_err()),
        tonic::Code::NotFound
    );
    let e = f
        .review
        .list_threads(req(
            wire::ListThreadsRequest {
                repo: repo_ref(),
                change: cbytes(9),
                ..Default::default()
            },
            BOB,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&e), tonic::Code::NotFound);
}

#[tokio::test]
async fn threads_page_oldest_first() {
    let f = fx().await;
    for i in 0..3 {
        comment(&f, BOB, 1, 0, "a", &format!("c{i}")).await.unwrap();
    }
    let page = |token: &str| {
        let m = wire::ListThreadsRequest {
            repo: repo_ref(),
            change: cbytes(1),
            limit: 2,
            page_token: token.into(),
            ..Default::default()
        };
        f.review.list_threads(req(m, BOB, false))
    };
    let p1 = page("").await.unwrap().into_inner();
    assert_eq!(p1.threads.iter().map(|t| t.id).collect::<Vec<_>>(), [1, 2]);
    let p2 = page(&p1.next_page_token).await.unwrap().into_inner();
    assert_eq!(p2.threads.iter().map(|t| t.id).collect::<Vec<_>>(), [3]);
    assert!(p2.next_page_token.is_empty());
    assert_eq!(
        code(&page("x1").await.unwrap_err()),
        tonic::Code::InvalidArgument
    );
}

#[tokio::test]
async fn commenting_needs_a_user_and_requesting_needs_write() {
    let f = fx().await;
    // A deploy credential reads but is nobody.
    assert!(
        threads(&f, DEPLOY, 1, wire::ThreadStatus::Unspecified)
            .await
            .threads
            .is_empty()
    );
    let e = comment(&f, DEPLOY, 1, 0, "a", "x").await.unwrap_err();
    assert_eq!(code(&e), tonic::Code::PermissionDenied);
    // Reading needs a credential at all.
    let mut anon = req(
        wire::ListThreadsRequest {
            repo: repo_ref(),
            change: cbytes(1),
            ..Default::default()
        },
        BOB,
        false,
    );
    anon.extensions_mut().remove::<Credential>();
    assert!(f.review.list_threads(anon).await.is_err());
    // Read access does not request reviews.
    let e = request_review(&f, BOB, 1, &["alice"], &[])
        .await
        .unwrap_err();
    assert!(
        matches!(
            code(&e),
            tonic::Code::PermissionDenied | tonic::Code::Unauthenticated
        ),
        "{e:?}"
    );
}

#[tokio::test]
async fn review_requests_are_added_listed_finished_by_a_comment_and_removed() {
    let f = fx().await;
    let r = request_review(&f, CAROL, 1, &["bob", "dave"], &[])
        .await
        .unwrap();
    let who: Vec<_> = r
        .requests
        .iter()
        .map(|q| (q.reviewer.as_str(), q.requested_by.as_str(), q.done_at))
        .collect();
    assert_eq!(who, [("bob", "carol", 0), ("dave", "carol", 0)]);
    // Again: nothing changes. Bad handles and long lists are refused.
    assert_eq!(
        request_review(&f, CAROL, 1, &["bob"], &[])
            .await
            .unwrap()
            .requests
            .len(),
        2
    );
    for bad in [vec!["a b"], vec![""], vec!["x/y"]] {
        let e = request_review(&f, CAROL, 1, &bad, &[]).await.unwrap_err();
        assert_eq!(code(&e), tonic::Code::InvalidArgument);
    }
    let many: Vec<String> = (0..17).map(|i| format!("u{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        code(&request_review(&f, CAROL, 1, &many, &[]).await.unwrap_err()),
        tonic::Code::InvalidArgument
    );
    assert_eq!(
        code(
            &request_review(&f, CAROL, 9, &["bob"], &[])
                .await
                .unwrap_err()
        ),
        tonic::Code::NotFound
    );

    // Bob's first comment finishes his request; asking again reopens it; removing deletes it.
    comment(&f, BOB, 1, 0, "a", "looked").await.unwrap();
    let r = request_review(&f, CAROL, 1, &[], &[]).await.unwrap();
    let done: Vec<_> = r
        .requests
        .iter()
        .map(|q| (q.reviewer.as_str(), q.done_at > 0))
        .collect();
    assert_eq!(done, [("bob", true), ("dave", false)]);
    let r = request_review(&f, CAROL, 1, &["bob"], &["dave"])
        .await
        .unwrap();
    assert_eq!(r.requests.len(), 1);
    assert_eq!(
        (r.requests[0].reviewer.as_str(), r.requests[0].done_at),
        ("bob", 0)
    );
}
