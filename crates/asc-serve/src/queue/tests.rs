//! The merge queue: entries and order, estimates, fail-closed landing, pause, removal, listing,
//! preview and the live event stream. Each call is tried with the right caller, a caller without
//! the right, an unknown change and another repository's claim.

use std::sync::Arc;
use std::time::Duration;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::merge_queue_server::MergeQueue;
use asc_rpc::services::review_server::Review;
use futures::StreamExt as _;
use tonic::Code;

use super::rpc::QueueService;
use crate::review::Reviews;
use crate::review::tests::{ADMIN, ALICE, CAROL, Fx, cbytes, fx, repo_ref, req};
use crate::review::tests_acting::claimkit::{self, host};

fn svc(f: &Fx) -> (QueueService, asc_rpc::forwarded::ForwardedVerifier) {
    let (a, v) = claimkit::authorizer();
    (
        QueueService::new(
            Arc::new(Reviews::new(f.reg.clone())),
            a,
            ServerInfo::default(),
        ),
        v,
    )
}

async fn enqueue(
    q: &QueueService,
    token: &str,
    change: u8,
) -> Result<wire::QueueEntry, tonic::Status> {
    q.enqueue(req(
        wire::EnqueueRequest {
            repo: repo_ref(),
            change: cbytes(change),
            ..Default::default()
        },
        token,
        true,
    ))
    .await
    .map(|r| r.into_inner().entry.unwrap())
}

async fn approve(f: &Fx, change: u8) {
    f.review
        .record_approval(req(
            wire::RecordApprovalRequest {
                repo: repo_ref(),
                change: cbytes(change),
                version: 0,
                state: wire::ApprovalState::Approved as i32,
            },
            CAROL,
            true,
        ))
        .await
        .unwrap();
}

async fn report(
    q: &QueueService,
    token: &str,
    id: u64,
    state: wire::QueueEntryState,
    landed: &[u8],
) -> Result<wire::QueueEntry, tonic::Status> {
    q.report_entry(req(
        wire::ReportQueueEntryRequest {
            repo: repo_ref(),
            id,
            state: state as i32,
            landed_as: landed.to_vec().into(),
            stage: "landing".into(),
            progress_percent: 40,
            detail: "unit running on merged trunk".into(),
        },
        token,
        false,
    ))
    .await
    .map(|r| r.into_inner().entry.unwrap())
}

#[tokio::test]
async fn an_entry_waits_until_it_is_approved_and_the_queue_knows_its_position_and_eta() {
    let f = fx().await;
    let (q, v) = svc(&f);
    assert_eq!(
        enqueue(&q, ALICE, 1).await.unwrap_err().code(),
        Code::PermissionDenied,
        "a reader cannot queue"
    );
    assert_eq!(
        enqueue(&q, CAROL, 9).await.unwrap_err().code(),
        Code::NotFound
    );
    let m = || wire::EnqueueRequest {
        repo: repo_ref(),
        change: cbytes(1),
        ..Default::default()
    };
    assert_eq!(
        q.enqueue(host(&v, m(), "44:carol", Role::Admin, "o/other", true))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        q.enqueue(host(&v, m(), "44:carol", Role::Read, "o/r", true))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied,
        "a read claim cannot queue"
    );
    let e = enqueue(&q, CAROL, 1).await.unwrap();
    assert_eq!(e.state, wire::QueueEntryState::Waiting as i32);
    assert_eq!(e.position, 1);
    assert_eq!(e.enqueued_by, "carol");
    let rv = e.revalidation.unwrap();
    assert!(!rv.passed && rv.reasons == [wire::RevalidationReason::ApprovalsMissing as i32]);
    let t = e.timing.unwrap();
    assert_eq!(
        (t.eta_seconds, t.starts_in_seconds, t.stage.as_str()),
        (180, 0, "waiting")
    );
    assert_eq!(
        enqueue(&q, CAROL, 1).await.unwrap_err().code(),
        Code::AlreadyExists
    );
    // Approved: revalidation makes it READY; a second entry is behind the first.
    approve(&f, 1).await;
    let id = e.id;
    let r = q
        .revalidate(req(
            wire::RevalidateRequest {
                repo: repo_ref(),
                id,
            },
            CAROL,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(r.state, wire::QueueEntryState::Ready as i32);
    assert!(r.revalidation.unwrap().passed);
    let second = enqueue(&q, CAROL, 2).await.unwrap();
    let t = second.timing.unwrap();
    assert_eq!(
        (second.position, t.eta_seconds, t.starts_in_seconds),
        (2, 360, 180)
    );
    // A claim for this repository reads the queue; another repository's does not.
    let l = || wire::ListQueueRequest {
        repo: repo_ref(),
        ..Default::default()
    };
    assert_eq!(
        q.list(host(&v, l(), "42:alice", Role::Read, "o/other", false))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        q.list(host(&v, l(), "42:alice", Role::Read, "o/r", false))
            .await
            .unwrap()
            .into_inner()
            .entries
            .len(),
        2
    );
    // Preview: a third change would be third, with the estimate and what it would wait for.
    let p = q
        .preview(req(
            wire::PreviewEnqueueRequest {
                repo: repo_ref(),
                change: cbytes(3),
                target: String::new(),
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .preview
        .unwrap();
    assert_eq!(
        (p.position, p.stack.len(), p.timing.unwrap().eta_seconds),
        (3, 1, 540)
    );
    assert!(
        p.would_wait_for
            .contains(&(wire::RevalidationReason::ApprovalsMissing as i32))
    );
    let held = q
        .preview(req(
            wire::PreviewEnqueueRequest {
                repo: repo_ref(),
                change: cbytes(1),
                target: String::new(),
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .preview
        .unwrap();
    assert_eq!(held.already_queued.len(), 1);
    assert!(
        held.stack.is_empty(),
        "everything of the stack is queued already"
    );
    // A change with changes requested is blocked, not waiting.
    f.review
        .record_approval(req(
            wire::RecordApprovalRequest {
                repo: repo_ref(),
                change: cbytes(2),
                version: 0,
                state: wire::ApprovalState::ChangesRequested as i32,
            },
            CAROL,
            true,
        ))
        .await
        .unwrap();
    let r = q
        .revalidate(req(
            wire::RevalidateRequest {
                repo: repo_ref(),
                id: second.id,
            },
            CAROL,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(r.state, wire::QueueEntryState::Blocked as i32);
}

#[tokio::test]
async fn landing_is_fail_closed_and_only_an_administrator_reports_it() {
    let f = fx().await;
    let (q, _) = svc(&f);
    let first = enqueue(&q, CAROL, 1).await.unwrap();
    let second = enqueue(&q, CAROL, 2).await.unwrap();
    assert_eq!(first.state, wire::QueueEntryState::Waiting as i32);
    assert_eq!(
        report(&q, CAROL, first.id, wire::QueueEntryState::Landing, &[])
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied,
        "a writer cannot report"
    );
    assert_eq!(
        report(&q, ALICE, first.id, wire::QueueEntryState::Landing, &[])
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    // Not READY: landing is refused, whoever asks.
    assert_eq!(
        report(&q, ADMIN, first.id, wire::QueueEntryState::Landing, &[])
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        report(
            &q,
            ADMIN,
            first.id,
            wire::QueueEntryState::Landed,
            &[7u8; 32]
        )
        .await
        .unwrap_err()
        .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        report(&q, ADMIN, 999, wire::QueueEntryState::Landing, &[])
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        report(&q, ADMIN, first.id, wire::QueueEntryState::Waiting, &[])
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    approve(&f, 1).await;
    q.revalidate(req(
        wire::RevalidateRequest {
            repo: repo_ref(),
            id: first.id,
        },
        CAROL,
        false,
    ))
    .await
    .unwrap();
    let landing = report(&q, ADMIN, first.id, wire::QueueEntryState::Landing, &[])
        .await
        .unwrap();
    assert_eq!(landing.state, wire::QueueEntryState::Landing as i32);
    let t = landing.timing.unwrap();
    assert_eq!(
        (
            t.stage.as_str(),
            t.progress_percent,
            t.detail.as_str(),
            t.starts_in_seconds
        ),
        ("landing", 40, "unit running on merged trunk", 0)
    );
    assert_eq!(
        report(
            &q,
            ADMIN,
            first.id,
            wire::QueueEntryState::Landed,
            &[1u8; 5]
        )
        .await
        .unwrap_err()
        .code(),
        Code::FailedPrecondition,
        "the landed changeset is 32 bytes"
    );
    let landed = report(
        &q,
        ADMIN,
        first.id,
        wire::QueueEntryState::Landed,
        &[9u8; 32],
    )
    .await
    .unwrap();
    assert_eq!(
        (landed.state, landed.position, landed.landed_as.len()),
        (wire::QueueEntryState::Landed as i32, 0, 32)
    );
    assert!(landed.landed_at > 0);
    // The next entry moved up, and the list knows the median of landings.
    let list = q
        .list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.entries.len(), 1);
    assert_eq!(
        (list.entries[0].id, list.entries[0].position),
        (second.id, 1)
    );
    assert!(list.median_land_seconds >= 1);
    let all = q
        .list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                include_finished: true,
                limit: 1,
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        all.entries[0].id, second.id,
        "newest first with include_finished"
    );
    assert!(!all.next_page_token.is_empty());
    let page2 = q
        .list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                include_finished: true,
                limit: 1,
                page_token: all.next_page_token.clone(),
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page2.entries[0].id, first.id);
    // A token of one caller is not good for another.
    let stolen = q
        .list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                include_finished: true,
                limit: 1,
                page_token: all.next_page_token.clone(),
                ..Default::default()
            },
            CAROL,
            false,
        ))
        .await;
    assert!(stolen.is_err());
}

#[tokio::test]
async fn an_entry_is_removed_by_its_owner_or_an_administrator_and_a_paused_queue_takes_no_entries()
{
    let f = fx().await;
    let (q, _) = svc(&f);
    let e = enqueue(&q, CAROL, 1).await.unwrap();
    let dq = |token: &str, id: u64, reason: &str| {
        let r = req(
            wire::DequeueRequest {
                repo: repo_ref(),
                id,
                reason: reason.into(),
            },
            token,
            false,
        );
        let q = q.clone();
        async move { q.dequeue(r).await.map(|r| r.into_inner().entry.unwrap()) }
    };
    assert_eq!(
        dq(ALICE, e.id, "x").await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        dq(CAROL, e.id, &"x".repeat(257)).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        dq(CAROL, 12345, "x").await.unwrap_err().code(),
        Code::NotFound
    );
    let gone = dq(CAROL, e.id, "changed my mind").await.unwrap();
    assert_eq!(
        (gone.state, gone.removed_reason.as_str()),
        (wire::QueueEntryState::Removed as i32, "changed my mind")
    );
    assert_eq!(
        dq(CAROL, e.id, "again").await.unwrap().state,
        wire::QueueEntryState::Removed as i32,
        "idempotent"
    );
    let again = enqueue(&q, CAROL, 1).await.unwrap();
    assert_eq!(
        dq(ADMIN, again.id, "stale").await.unwrap().state,
        wire::QueueEntryState::Removed as i32
    );
    // Pausing is for administrators; a paused queue refuses Enqueue.
    let pause = |token: &str, paused: bool| {
        let r = req(
            wire::SetQueuePausedRequest {
                repo: repo_ref(),
                target: String::new(),
                paused,
                reason: "release freeze".into(),
            },
            token,
            false,
        );
        let q = q.clone();
        async move { q.set_paused(r).await.map(|r| r.into_inner().paused) }
    };
    assert_eq!(
        pause(CAROL, true).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert!(pause(ADMIN, true).await.unwrap());
    let e = enqueue(&q, CAROL, 3).await.unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert!(
        e.message().contains("QUEUE_PAUSED") && e.message().contains("release freeze"),
        "{e:?}"
    );
    let l = q
        .list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(l.paused && l.paused_reason == "release freeze");
    assert!(!pause(ADMIN, false).await.unwrap());
    assert!(enqueue(&q, CAROL, 3).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_watch_starts_with_the_live_entries_then_streams_changes_and_resumes_from_a_cursor() {
    let f = fx().await;
    let (q, _) = svc(&f);
    let first = enqueue(&q, CAROL, 1).await.unwrap();
    let watch = |token: &str, resume: Vec<u8>| {
        let r = req(
            wire::WatchQueueRequest {
                repo: repo_ref(),
                target: String::new(),
                resume: resume.into(),
            },
            token,
            false,
        );
        let q = q.clone();
        async move { q.watch(r).await.map(tonic::Response::into_inner) }
    };
    assert!(
        watch(ALICE, vec![1, 2, 3]).await.is_err(),
        "a cursor is 8 bytes"
    );
    let mut s = watch(ALICE, vec![]).await.unwrap();
    let snap = tokio::time::timeout(Duration::from_secs(3), s.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(snap.entry.unwrap().id, first.id);
    let second = enqueue(&q, CAROL, 2).await.unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(5), s.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ev.entry.as_ref().unwrap().id, second.id);
    assert_eq!(ev.resume.len(), 8);
    let cursor = ev.resume.to_vec();
    // The queue moves; a new watch from the cursor sees only what happened after it.
    q.dequeue(req(
        wire::DequeueRequest {
            repo: repo_ref(),
            id: first.id,
            reason: "x".into(),
        },
        CAROL,
        false,
    ))
    .await
    .unwrap();
    let mut s2 = watch(ALICE, cursor).await.unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(5), s2.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let e = ev.entry.unwrap();
    assert_eq!(
        (e.id, e.state),
        (first.id, wire::QueueEntryState::Removed as i32)
    );
    // Another repository's claim cannot watch.
    let (q2, v) = svc(&f);
    let m = wire::WatchQueueRequest {
        repo: repo_ref(),
        ..Default::default()
    };
    assert_eq!(
        q2.watch(host(&v, m, "42:alice", Role::Admin, "o/other", false))
            .await
            .err()
            .map(|e| e.code()),
        Some(Code::PermissionDenied)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_enqueues_of_one_change_at_once_leave_one_entry() {
    let f = fx().await;
    let (q, _) = svc(&f);
    let (a, b) = tokio::join!(enqueue(&q, CAROL, 1), enqueue(&q, CAROL, 1));
    let codes: Vec<Code> = [a, b]
        .iter()
        .map(|r| r.as_ref().map_or_else(|e| e.code(), |_| Code::Ok))
        .collect();
    assert_eq!(
        codes.iter().filter(|c| **c == Code::Ok).count(),
        1,
        "{codes:?}"
    );
    assert!(
        codes.contains(&Code::AlreadyExists) || codes.contains(&Code::Aborted),
        "{codes:?}"
    );
    let l = q
        .list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.entries.len(), 1);
    let nine = vec![1; 9];
    assert_eq!(
        q.list(req(
            wire::ListQueueRequest {
                repo: repo_ref(),
                states: nine,
                ..Default::default()
            },
            ALICE,
            false
        ))
        .await
        .unwrap_err()
        .code(),
        Code::InvalidArgument
    );
}

#[tokio::test]
async fn an_entry_that_stopped_passing_is_refused_at_the_moment_of_landing() {
    let f = fx().await;
    let (q, _) = svc(&f);
    let e = enqueue(&q, CAROL, 1).await.unwrap();
    approve(&f, 1).await;
    let r = q
        .revalidate(req(
            wire::RevalidateRequest {
                repo: repo_ref(),
                id: e.id,
            },
            CAROL,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(r.state, wire::QueueEntryState::Ready as i32);
    // Between READY and the landing, a reviewer asks for changes.
    f.review
        .record_approval(req(
            wire::RecordApprovalRequest {
                repo: repo_ref(),
                change: cbytes(1),
                version: 0,
                state: wire::ApprovalState::ChangesRequested as i32,
            },
            ADMIN,
            true,
        ))
        .await
        .unwrap();
    let err = report(&q, ADMIN, e.id, wire::QueueEntryState::Landing, &[])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("REVALIDATION_FAILED"), "{err:?}");
    let now = q
        .get(req(
            wire::GetQueueEntryRequest {
                repo: repo_ref(),
                id: e.id,
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(
        now.state,
        wire::QueueEntryState::Blocked as i32,
        "it went back, it did not land"
    );
    assert_eq!(
        report(&q, ADMIN, e.id, wire::QueueEntryState::Landed, &[9u8; 32])
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition,
        "and it cannot be reported landed"
    );
}
