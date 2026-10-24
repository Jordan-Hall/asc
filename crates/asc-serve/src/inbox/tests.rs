//! Tests of the `Inbox` service.

use bytes::Bytes;
use asc_rpc::server::wire;
use asc_rpc::services::ci_server::Ci;
use asc_rpc::services::inbox_server::Inbox;
use asc_rpc::services::review_server::Review;
use tonic::Status;

use crate::review::now;
use crate::review::tests::{
    ADMIN, ALICE, BOB, CAROL, DAVE, DEPLOY, Fx, cbytes, cid, code, fx, repo_ref, req,
    request_review,
};

use wire::InboxSection as S;

async fn list(f: &Fx, token: &str, sections: &[S], snoozed: bool) -> wire::InboxListResponse {
    f.inbox
        .list(req(
            wire::InboxListRequest {
                repo: repo_ref(),
                sections: sections.iter().map(|s| *s as i32).collect(),
                include_snoozed: snoozed,
                ..Default::default()
            },
            token,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
}

/// `(change number, section)` of each item, in order.
fn shape(r: &wire::InboxListResponse) -> Vec<(u8, i32)> {
    r.items.iter().map(|i| (i.change[0], i.section)).collect()
}

async fn count(f: &Fx, token: &str, etag: &str) -> wire::InboxCountResponse {
    f.inbox
        .count(req(
            wire::InboxCountRequest {
                repo: repo_ref(),
                if_none_match: etag.into(),
                ..Default::default()
            },
            token,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
}

async fn open_thread(f: &Fx, token: &str, change: u8) -> u64 {
    f.review
        .add_comment(req(
            wire::AddCommentRequest {
                repo: repo_ref(),
                change: cbytes(change),
                anchor: Some(wire::ThreadAnchor {
                    path: Bytes::from_static(b"a.rs"),
                    start_line: 1,
                    ..Default::default()
                }),
                body: "note".into(),
                ..Default::default()
            },
            token,
            true,
        ))
        .await
        .unwrap()
        .into_inner()
        .thread
        .unwrap()
        .id
}

async fn report(f: &Fx, change: u8, id: &str, status: wire::CiStatus) {
    f.ci.report_run(req(
        wire::ReportRunRequest {
            repo: repo_ref(),
            source: "ci".into(),
            external_id: id.into(),
            name: "build".into(),
            change: cbytes(change),
            status: status as i32,
            ..Default::default()
        },
        ADMIN,
        false,
    ))
    .await
    .unwrap();
}

async fn set_state(
    f: &Fx,
    token: &str,
    change: u8,
    pinned: Option<bool>,
    snoozed_until: Option<i64>,
) -> Result<wire::InboxSetStateResponse, Status> {
    f.inbox
        .set_state(req(
            wire::InboxSetStateRequest {
                repo: repo_ref(),
                change: cbytes(change),
                pinned,
                snoozed_until,
            },
            token,
            false,
        ))
        .await
        .map(tonic::Response::into_inner)
}

#[tokio::test]
async fn every_section_is_derived_for_the_right_person() {
    let f = fx().await;
    assert!(list(&f, ALICE, &[], false).await.items.is_empty());

    // A review request lands in the reviewer's NEEDS_REVIEW.
    request_review(&f, CAROL, 1, &["bob"], &[]).await.unwrap();
    let r = list(&f, BOB, &[], false).await;
    assert_eq!(shape(&r), [(1, S::NeedsReview as i32)]);
    let i = &r.items[0];
    assert_eq!(
        (i.requested_by.as_str(), i.author.as_str(), i.latest_version),
        ("carol", "alice", 2)
    );
    assert!(list(&f, ALICE, &[], false).await.items.is_empty());

    // Bob comments: his request is done, he waits on alice, alice is returned the change.
    let t = open_thread(&f, BOB, 1).await;
    assert_eq!(
        shape(&list(&f, BOB, &[], false).await),
        [(1, S::Waiting as i32)]
    );
    let a = list(&f, ALICE, &[], false).await;
    assert_eq!(shape(&a), [(1, S::Returned as i32)]);
    assert_eq!(a.items[0].open_threads, 1);

    // Alice's own thread on her own change is nobody's business; her thread on bob's change is.
    open_thread(&f, ALICE, 3).await;
    open_thread(&f, ALICE, 2).await;
    assert_eq!(
        shape(&list(&f, ALICE, &[], false).await)
            .iter()
            .filter(|(c, _)| *c == 3)
            .count(),
        0
    );
    let b = list(&f, BOB, &[S::Returned], false).await;
    assert_eq!(shape(&b), [(2, S::Returned as i32)]);
    assert_eq!(list(&f, ALICE, &[S::Waiting], false).await.total, 1);

    // A resolved thread leaves the inbox.
    f.review
        .resolve_thread(req(
            wire::ResolveThreadRequest {
                repo: repo_ref(),
                thread_id: t,
                resolved: true,
            },
            ALICE,
            false,
        ))
        .await
        .unwrap();
    assert!(list(&f, BOB, &[S::Waiting], false).await.items.is_empty());
    assert!(
        list(&f, ALICE, &[S::Returned], false)
            .await
            .items
            .is_empty()
    );

    // A failing newest run of alice's change; a later pass clears it.
    report(&f, 3, "r1", wire::CiStatus::Failure).await;
    assert_eq!(
        shape(&list(&f, ALICE, &[S::ChecksFailed], false).await),
        [(3, S::ChecksFailed as i32)]
    );
    assert!(
        list(&f, BOB, &[S::ChecksFailed], false)
            .await
            .items
            .is_empty()
    );
    report(&f, 3, "r2", wire::CiStatus::Success).await;
    assert!(
        list(&f, ALICE, &[S::ChecksFailed], false)
            .await
            .items
            .is_empty()
    );
    // Bob's change failing is not alice's problem.
    report(&f, 2, "r3", wire::CiStatus::Failure).await;
    assert!(
        list(&f, ALICE, &[S::ChecksFailed], false)
            .await
            .items
            .is_empty()
    );
    assert_eq!(list(&f, BOB, &[S::ChecksFailed], false).await.total, 1);
}

#[tokio::test]
async fn a_landed_change_needs_nobody() {
    let f = fx().await;
    open_thread(&f, BOB, 1).await;
    assert_eq!(list(&f, ALICE, &[], false).await.total, 1);
    sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, 1, $3)")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(format!("refs/heads/{}", f.e.default_branch))
        .bind(f.css[0].as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    assert!(list(&f, ALICE, &[], false).await.items.is_empty());
    assert!(list(&f, BOB, &[], false).await.items.is_empty());
}

#[tokio::test]
async fn the_count_is_cheap_and_honours_its_etag() {
    let f = fx().await;
    let empty = count(&f, ALICE, "").await;
    assert_eq!(
        (empty.total, empty.counts.len(), empty.not_modified),
        (0, 4, false)
    );
    assert!(!empty.etag.is_empty());
    assert!(count(&f, ALICE, &empty.etag).await.not_modified);

    open_thread(&f, BOB, 1).await;
    let c = count(&f, ALICE, &empty.etag).await;
    assert!(!c.not_modified);
    assert_eq!(c.total, 1);
    let returned = c
        .counts
        .iter()
        .find(|c| c.section == S::Returned as i32)
        .unwrap();
    assert_eq!(returned.count, 1);
    let again = count(&f, ALICE, &c.etag).await;
    assert!(again.not_modified && again.counts.is_empty());
    // Only the named sections are counted.
    let one = f
        .inbox
        .count(req(
            wire::InboxCountRequest {
                repo: repo_ref(),
                sections: vec![S::Waiting as i32],
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((one.counts.len(), one.total), (1, 0));
    let bad = f
        .inbox
        .count(req(
            wire::InboxCountRequest {
                repo: repo_ref(),
                sections: vec![S::Unspecified as i32],
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&bad), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn pin_sorts_first_and_snooze_hides_until_it_ends() {
    let f = fx().await;
    open_thread(&f, BOB, 1).await;
    open_thread(&f, BOB, 3).await;
    // Newest first: both threads were opened in the same second, so the tie breaks on the change.
    let base = shape(&list(&f, ALICE, &[], false).await);
    assert_eq!(base.len(), 2);
    let last = base[1].0;

    let s = set_state(&f, ALICE, last, Some(true), None).await.unwrap();
    assert_eq!((s.pinned, s.snoozed_until), (true, 0));
    assert_eq!(list(&f, ALICE, &[], false).await.items[0].change[0], last);
    assert!(list(&f, ALICE, &[], false).await.items[0].pinned);

    // Snoozed: hidden and not counted, unless pinned or asked for.
    let until = now() + 3600;
    let first = base[0].0;
    set_state(&f, ALICE, first, None, Some(until))
        .await
        .unwrap();
    let hidden = list(&f, ALICE, &[], false).await;
    assert_eq!(hidden.total, 1);
    assert_eq!(count(&f, ALICE, "").await.total, 1);
    let all = list(&f, ALICE, &[], true).await;
    assert_eq!(all.total, 2);
    assert_eq!(
        all.items
            .iter()
            .find(|i| i.change[0] == first)
            .unwrap()
            .snoozed_until,
        until
    );
    // Pinning a snoozed change shows it again; a snooze that is over, and unpinning, clear the row.
    set_state(&f, ALICE, first, Some(true), None).await.unwrap();
    assert_eq!(list(&f, ALICE, &[], false).await.total, 2);
    set_state(&f, ALICE, first, Some(false), Some(1))
        .await
        .unwrap();
    set_state(&f, ALICE, last, Some(false), None).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbox_state")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(list(&f, ALICE, &[], false).await.total, 2);
    // State is per user, and bad input is refused.
    set_state(&f, DAVE, first, Some(true), None).await.unwrap();
    assert!(
        list(&f, ALICE, &[], false)
            .await
            .items
            .iter()
            .all(|i| !i.pinned)
    );
    for (c, t) in [(9u8, Some(5)), (1, Some(-1)), (1, Some(i64::MAX))] {
        let e = set_state(&f, ALICE, c, None, t).await.unwrap_err();
        assert!(
            matches!(
                code(&e),
                tonic::Code::NotFound | tonic::Code::InvalidArgument
            ),
            "{e:?}"
        );
    }
}

#[tokio::test]
async fn the_list_pages() {
    let f = fx().await;
    for c in [1u8, 2, 3] {
        request_review(&f, CAROL, c, &["dave"], &[]).await.unwrap();
    }
    let page = |token: &str| {
        f.inbox.list(req(
            wire::InboxListRequest {
                repo: repo_ref(),
                limit: 2,
                page_token: token.into(),
                ..Default::default()
            },
            DAVE,
            false,
        ))
    };
    let p1 = page("").await.unwrap().into_inner();
    assert_eq!((p1.items.len(), p1.total), (2, 3));
    let p2 = page(&p1.next_page_token).await.unwrap().into_inner();
    assert_eq!(p2.items.len(), 1);
    assert!(p2.next_page_token.is_empty());
    let mut seen: Vec<u8> = p1
        .items
        .iter()
        .chain(&p2.items)
        .map(|i| i.change[0])
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, [1, 2, 3]);
    let _ = cid(1);
}

#[tokio::test]
async fn only_a_user_has_an_inbox() {
    let f = fx().await;
    let e = f
        .inbox
        .list(req(
            wire::InboxListRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            DEPLOY,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&e), tonic::Code::PermissionDenied);
    let e = set_state(&f, DEPLOY, 1, Some(true), None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), tonic::Code::PermissionDenied);
}
