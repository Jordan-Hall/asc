//! The `[refs]` rules of the policy: who may move a protected branch or tag, and whether it may
//! be rewritten, deleted or take unsigned changes.

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::policy::POLICY_PATH;
use asc_rpc::server::{CallCtx, wire};

use super::gate::{GateMode, GateRequest, GateUpdate, RefGate};
use super::tests::{ALICE, BOB, Fx, code, ctx, fx, paths, policy_text};
use crate::ingest::testutil::commit;

const RULES: &str = "[refs.\"stable/*\"]\nland = [\"maintainers\"]\n\
                     [refs.\"refs/tags/v*\"]\nland = [\"release-key\"]\n\
                     [refs.\"open/*\"]\nland = [\"queue\", \"maintainers\"]\nforce = true\ndelete = true\n\
                     [refs.\"agentic/*\"]\nland = [\"queue\", \"agents\"]\n\
                     [refs.\"signed/*\"]\nland = [\"maintainers\"]\nsigned = true\n";

/// A repository whose trunk carries `RULES`, with `a` the trunk tip and `b` a child of it.
async fn staged() -> (Fx, ChangesetId, ChangesetId) {
    let f = fx().await;
    let text = policy_text(RULES);
    let a = commit(&f.e, &[], &[(POLICY_PATH, text.as_bytes()), ("x", b"1")]).await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    f.ingest(b).await;
    (f, a, b)
}

fn moved(name: &str, old: Option<ChangesetId>, new: Option<ChangesetId>) -> [GateUpdate; 1] {
    [GateUpdate {
        name: name.into(),
        old: old.map(RefTarget::Changeset),
        new: new.map(RefTarget::Changeset),
        user: false,
    }]
}

async fn check(f: &Fx, c: &CallCtx, u: &[GateUpdate], mode: GateMode) -> Result<(), String> {
    f.gate
        .check(&GateRequest {
            ctx: c,
            entry: &f.e,
            updates: u,
            mode,
        })
        .await
        .map_err(|s| format!("{} {:?} {}", code(&s), paths(&s), s.message()))
}

const PUSH: GateMode = GateMode::Push { force: false };
const FORCE: GateMode = GateMode::Push { force: true };

#[tokio::test]
async fn a_rule_lets_only_the_movers_it_names_move_its_refs() {
    let (f, _, b) = staged().await;
    let (alice, bob, ci, deploy) = (ctx(ALICE), ctx(BOB), ctx("ci:abc"), ctx("deploy:o/r"));

    // stable/*: maintainers only
    let u = moved("refs/heads/stable/1", None, Some(b));
    check(&f, &alice, &u, PUSH).await.unwrap();
    let err = check(&f, &bob, &u, PUSH).await.unwrap_err();
    assert!(err.starts_with("POLICY_DENIED"), "{err}");
    assert!(
        err.contains("refs/heads/stable/1 is protected by [refs.") && err.contains("stable/*"),
        "{err}"
    );
    assert!(
        check(&f, &ci, &u, PUSH).await.is_err(),
        "a CI key is not a maintainer"
    );
    let err = check(&f, &alice, &u, land(b)).await.unwrap_err();
    assert!(err.contains("merge queue may not land"), "{err}");

    // tags v*: release keys only
    let t = moved("refs/tags/v1.0", None, Some(b));
    check(&f, &ci, &t, PUSH).await.unwrap();
    check(&f, &deploy, &t, PUSH).await.unwrap();
    let err = check(&f, &alice, &t, PUSH).await.unwrap_err();
    assert!(err.contains("does not let you move it directly"), "{err}");
    assert!(
        check(&f, &bob, &t, PUSH)
            .await
            .unwrap_err()
            .contains("only the merge queue")
    );

    // a ref no rule names, and trunk, are as before
    check(&f, &bob, &moved("refs/heads/feature", None, Some(b)), PUSH)
        .await
        .unwrap();
    check(&f, &bob, &moved("refs/tags/x1", None, Some(b)), PUSH)
        .await
        .unwrap();

    // and through a real push
    let any = || wire::ref_update::Expect::Any(true);
    let err = f
        .push(
            BOB,
            "refs/heads/stable/2",
            any(),
            b,
            wire::UpdateMode::FastForward,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    f.push(
        ALICE,
        "refs/heads/stable/2",
        any(),
        b,
        wire::UpdateMode::FastForward,
    )
    .await
    .unwrap();
}

fn land(cs: ChangesetId) -> GateMode {
    GateMode::Land {
        rewrites: vec![(cs, cs)],
    }
}

#[tokio::test]
async fn deleting_and_rewriting_need_the_rule_to_allow_them() {
    let (f, a, b) = staged().await;
    let alice = ctx(ALICE);
    let c = commit(&f.e, &[a], &[("z", b"3")]).await;
    f.ingest(c).await;

    let err = check(
        &f,
        &alice,
        &moved("refs/heads/stable/1", Some(b), None),
        FORCE,
    )
    .await
    .unwrap_err();
    assert!(err.contains("cannot be deleted"), "{err}");
    let err = check(
        &f,
        &alice,
        &moved("refs/heads/stable/1", Some(b), Some(c)),
        FORCE,
    )
    .await
    .unwrap_err();
    assert!(err.contains("cannot be rewritten"), "{err}");
    // a fast-forward is no rewrite
    let d = commit(&f.e, &[b], &[("w", b"4")]).await;
    f.ingest(d).await;
    check(
        &f,
        &alice,
        &moved("refs/heads/stable/1", Some(b), Some(d)),
        FORCE,
    )
    .await
    .unwrap();

    // open/*: force and delete allowed
    check(
        &f,
        &alice,
        &moved("refs/heads/open/1", Some(b), Some(c)),
        FORCE,
    )
    .await
    .unwrap();
    check(
        &f,
        &alice,
        &moved("refs/heads/open/1", Some(b), None),
        FORCE,
    )
    .await
    .unwrap();
    // and still only for its movers
    assert!(
        check(
            &f,
            &ctx(BOB),
            &moved("refs/heads/open/1", Some(b), None),
            FORCE
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn the_queue_lands_agent_work_only_where_the_rule_takes_it() {
    let (f, _, b) = staged().await;
    let alice = ctx(ALICE);
    check(
        &f,
        &alice,
        &moved("refs/heads/open/1", None, Some(b)),
        land(b),
    )
    .await
    .unwrap();
    // `b` becomes the first version of a change an agent made
    f.register(b, 7, 1, "agent:bot").await;
    let err = check(
        &f,
        &alice,
        &moved("refs/heads/open/1", None, Some(b)),
        land(b),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("does not take changes an agent wrote"),
        "{err}"
    );
    check(
        &f,
        &alice,
        &moved("refs/heads/agentic/1", None, Some(b)),
        land(b),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_signed_rule_needs_signed_changes() {
    let (f, _, b) = staged().await;
    let err = check(
        &f,
        &ctx(ALICE),
        &moved("refs/heads/signed/1", None, Some(b)),
        PUSH,
    )
    .await
    .unwrap_err();
    assert!(err.starts_with("UNSIGNED_CHANGE"), "{err}");
    check(
        &f,
        &ctx(ALICE),
        &moved("refs/heads/stable/1", None, Some(b)),
        PUSH,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn an_agent_still_never_moves_a_ruled_ref() {
    let (f, _, b) = staged().await;
    let err = check(
        &f,
        &ctx("agent:bot"),
        &moved("refs/heads/agentic/1", None, Some(b)),
        PUSH,
    )
    .await
    .unwrap_err();
    assert!(err.starts_with("AGENT_SCOPE_DENIED"), "{err}");
}
