//! The pin check and the per-repository lock around `Stacks.Restack`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{RepoName, status_rpc_error};

use super::*;

const REWRITTEN: i32 = wire::RestackItemState::Rewritten as i32;
const UNCHANGED: i32 = wire::RestackItemState::Unchanged as i32;

fn change(n: u8) -> bytes::Bytes {
    bytes::Bytes::from(vec![n; 16])
}

fn head(n: u8) -> bytes::Bytes {
    bytes::Bytes::from(vec![n; 32])
}

fn item(c: u8, h: u8, state: i32) -> wire::RestackItemResult {
    wire::RestackItemResult {
        change: change(c),
        old_head: head(h),
        state,
        ..Default::default()
    }
}

fn pin(c: u8, h: u8) -> wire::ExpectedHead {
    wire::ExpectedHead {
        change: change(c),
        old_head: head(h),
    }
}

/// Plans from `stack`, applies by counting; sleeps while applying so overlaps show.
struct Fake {
    stack: Vec<wire::RestackItemResult>,
    plans: AtomicUsize,
    applies: AtomicUsize,
    running: AtomicUsize,
    overlapped: AtomicUsize,
}

impl Fake {
    fn new(stack: Vec<wire::RestackItemResult>) -> Arc<Self> {
        Arc::new(Self {
            stack,
            plans: AtomicUsize::new(0),
            applies: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            overlapped: AtomicUsize::new(0),
        })
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.plans.load(Ordering::SeqCst),
            self.applies.load(Ordering::SeqCst),
        )
    }
}

#[async_trait]
impl StackOps for Fake {
    async fn restack(
        &self,
        _ctx: &CallCtx,
        req: wire::RestackStackRequest,
    ) -> Result<wire::RestackStackResponse, Status> {
        if req.dry_run {
            self.plans.fetch_add(1, Ordering::SeqCst);
        } else {
            self.applies.fetch_add(1, Ordering::SeqCst);
            if self.running.fetch_add(1, Ordering::SeqCst) > 0 {
                self.overlapped.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(wire::RestackStackResponse {
            items: self.stack.clone(),
            applied: !req.dry_run,
            ..Default::default()
        })
    }

    async fn revert_landed(
        &self,
        _ctx: &CallCtx,
        _req: wire::RevertLandedRequest,
    ) -> Result<wire::RevertLandedResponse, Status> {
        Err(Status::unimplemented("not under test"))
    }
}

fn ctx(repo: &str) -> CallCtx {
    CallCtx {
        repo: RepoName::parse(repo).unwrap(),
        id_mode: IdMode::Asc,
        principal: Principal {
            id: "7:alice".into(),
        },
        op_id: None,
        client: None,
    }
}

fn request(pins: Vec<wire::ExpectedHead>) -> wire::RestackStackRequest {
    wire::RestackStackRequest {
        change: change(1),
        expected_heads: pins,
        ..Default::default()
    }
}

fn stale_paths(e: &Status) -> Vec<String> {
    assert_eq!(e.code(), tonic::Code::Aborted, "{e:?}");
    let d = status_rpc_error(e).expect("typed error");
    assert_eq!(d.code(), "REF_CONFLICT");
    d.info().paths.clone()
}

#[tokio::test]
async fn pinned_heads_that_still_match_apply() {
    let fake = Fake::new(vec![
        item(1, 1, REWRITTEN),
        item(2, 2, REWRITTEN),
        item(3, 3, UNCHANGED),
    ]);
    let locks = RestackLocks::default();
    let out = restack_guarded(
        &locks,
        fake.as_ref(),
        &ctx("o/r"),
        request(vec![pin(1, 1), pin(2, 2)]),
    )
    .await
    .unwrap();
    assert!(out.applied);
    assert_eq!(fake.counts(), (1, 1));
}

#[tokio::test]
async fn a_pinned_head_that_moved_refuses_the_apply() {
    let fake = Fake::new(vec![item(1, 1, REWRITTEN), item(2, 9, REWRITTEN)]);
    let locks = RestackLocks::default();
    let e = restack_guarded(
        &locks,
        fake.as_ref(),
        &ctx("o/r"),
        request(vec![pin(1, 1), pin(2, 2)]),
    )
    .await
    .unwrap_err();
    assert_eq!(stale_paths(&e), vec![hex(&change(2))]);
    assert_eq!(fake.counts(), (1, 0), "planned, never applied");
}

#[tokio::test]
async fn a_rewritten_change_outside_the_pinned_set_refuses_the_apply() {
    // A change joined the stack after the caller looked: it would be rewritten unpinned.
    let fake = Fake::new(vec![item(1, 1, REWRITTEN), item(2, 2, REWRITTEN)]);
    let locks = RestackLocks::default();
    let e = restack_guarded(&locks, fake.as_ref(), &ctx("o/r"), request(vec![pin(1, 1)]))
        .await
        .unwrap_err();
    assert_eq!(stale_paths(&e), vec![hex(&change(2))]);
    assert_eq!(fake.counts(), (1, 0));
}

#[tokio::test]
async fn a_pinned_change_that_left_the_stack_refuses_the_apply() {
    let fake = Fake::new(vec![item(1, 1, REWRITTEN)]);
    let locks = RestackLocks::default();
    let e = restack_guarded(
        &locks,
        fake.as_ref(),
        &ctx("o/r"),
        request(vec![pin(1, 1), pin(5, 5)]),
    )
    .await
    .unwrap_err();
    assert_eq!(stale_paths(&e), vec![hex(&change(5))]);
    assert_eq!(fake.counts(), (1, 0));
}

#[tokio::test]
async fn one_change_pinned_to_two_heads_is_a_bad_request() {
    let fake = Fake::new(vec![item(1, 1, REWRITTEN)]);
    let locks = RestackLocks::default();
    let e = restack_guarded(
        &locks,
        fake.as_ref(),
        &ctx("o/r"),
        request(vec![pin(1, 1), pin(1, 2)]),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert_eq!(fake.counts(), (1, 0));
}

#[tokio::test]
async fn no_pins_apply_in_one_step_and_a_dry_run_ignores_them() {
    let fake = Fake::new(vec![item(1, 1, REWRITTEN)]);
    let locks = RestackLocks::default();
    restack_guarded(&locks, fake.as_ref(), &ctx("o/r"), request(Vec::new()))
        .await
        .unwrap();
    assert_eq!(fake.counts(), (0, 1));
    let dry = wire::RestackStackRequest {
        dry_run: true,
        ..request(vec![pin(7, 7)])
    };
    let out = restack_guarded(&locks, fake.as_ref(), &ctx("o/r"), dry)
        .await
        .unwrap();
    assert!(!out.applied);
}

#[tokio::test]
async fn pinned_restacks_of_one_repository_never_overlap_but_other_repositories_do() {
    let fake = Fake::new(vec![item(1, 1, REWRITTEN)]);
    let locks = Arc::new(RestackLocks::default());
    let run = |repo: &'static str| {
        let (fake, locks) = (fake.clone(), locks.clone());
        tokio::spawn(async move {
            restack_guarded(&locks, fake.as_ref(), &ctx(repo), request(vec![pin(1, 1)])).await
        })
    };
    let (a, b) = (run("o/r"), run("o/r"));
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    assert_eq!(fake.overlapped.load(Ordering::SeqCst), 0);
    assert_eq!(fake.counts().1, 2);
    // Another repository is not held up by the first.
    let (c, d) = (run("o/r"), run("o/other"));
    c.await.unwrap().unwrap();
    d.await.unwrap().unwrap();
    assert_eq!(fake.overlapped.load(Ordering::SeqCst), 1);
}
