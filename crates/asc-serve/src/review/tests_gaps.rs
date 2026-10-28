//! `Review.GetConflicts` and `Review.DraftContext`: positive, permission-denied, unknown and
//! another repository's claim.

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_rpc::forwarded::Role;
use asc_rpc::server::wire;
use asc_rpc::services::review_server::Review;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use tonic::Code;

use super::tests::{ALICE, DEPLOY, Fx, cbytes, cid, fx, repo_ref, req};
use super::tests_acting::claimkit::{self, host};
use crate::ingest::testutil::commit;

async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: asc_core::path::RefName::new(name.as_bytes()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    f.reg.refs().update(f.e.key, &txn).await.unwrap();
}

/// Trunk and change 3 both rewrite line 2 of `f`; change 4 is stacked on change 3.
async fn conflicting(f: &Fx) {
    let base = commit(&f.e, &[], &[("f", b"a\nb\nc\n"), ("keep", b"k\n")]).await;
    let main = commit(&f.e, &[base], &[("f", b"a\nMAIN\nc\n")]).await;
    let head = commit(&f.e, &[base], &[("f", b"a\nCHANGE\nc\n")]).await;
    set_ref(f, "refs/heads/master", main).await;
    sqlx::query("UPDATE change_versions SET cs = $1 WHERE change = $2")
        .bind(head.as_bytes().to_vec())
        .bind(cid(3).as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
         VALUES ($1, $2, 1, $3, $4, 1, '43:bob', $5)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(vec![4u8; 16])
    .bind(head.as_bytes().to_vec())
    .bind(cid(3).as_bytes().to_vec())
    .bind(vec![4u8])
    .execute(f.reg.pool())
    .await
    .unwrap();
}

fn conflicts_req(change: u8, target: &str) -> wire::GetChangeConflictsRequest {
    wire::GetChangeConflictsRequest {
        repo: repo_ref(),
        change: cbytes(change),
        target: target.into(),
    }
}

#[tokio::test]
async fn a_conflict_is_shown_with_both_sides_and_the_changes_it_holds_back() {
    let f = fx().await;
    conflicting(&f).await;
    let got = f
        .review
        .get_conflicts(req(conflicts_req(3, ""), ALICE, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.state, wire::ConflictCheck::Conflicts as i32);
    assert_eq!(got.files.len(), 1, "{got:?}");
    let file = &got.files[0];
    assert_eq!(file.path.as_ref(), b"f");
    assert_eq!(file.kind, wire::ConflictKind::Content as i32);
    assert_eq!(file.hunks.len(), 1);
    let h = &file.hunks[0];
    assert_eq!((h.main_start, h.change_start), (2, 2));
    assert_eq!(
        (h.main.as_slice(), h.change.as_slice()),
        (&["MAIN".to_owned()][..], &["CHANGE".to_owned()][..])
    );
    // The common ancestor's text is there when the merge keeps it.
    assert!(h.base.is_empty() || h.base == ["b"], "{:?}", h.base);
    // Change 4 is stacked on change 3 and waits; nothing else is named.
    assert_eq!(got.held_back.len(), 1);
    assert_eq!(got.held_back[0].as_ref(), cid(4).as_bytes());
    // The target can be spelled as a branch or a full ref name.
    for target in ["master", "refs/heads/master"] {
        let again = f
            .review
            .get_conflicts(req(conflicts_req(3, target), ALICE, false))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(again.files.len(), 1, "{target}");
    }
    // Nothing was written: the change is as it was.
    let n: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM change_versions")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(n, 5);
}

#[tokio::test]
async fn a_change_on_an_older_parent_or_without_conflict_has_no_files() {
    let f = fx().await;
    conflicting(&f).await;
    // Change 1's history shares nothing with trunk here: it needs a restack, and no file is shown.
    let got = f
        .review
        .get_conflicts(req(conflicts_req(1, ""), ALICE, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.state, wire::ConflictCheck::NeedsRestack as i32);
    assert!(got.files.is_empty());
    // A change at trunk itself has nothing to merge.
    let tip = commit(&f.e, &[], &[("z", b"z\n")]).await;
    set_ref(&f, "refs/heads/other", tip).await;
    sqlx::query("UPDATE change_versions SET cs = $1 WHERE change = $2")
        .bind(tip.as_bytes().to_vec())
        .bind(cid(2).as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    let got = f
        .review
        .get_conflicts(req(conflicts_req(2, "other"), ALICE, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.state, wire::ConflictCheck::Clean as i32);
    assert!(got.files.is_empty() && got.held_back.is_empty());
}

#[tokio::test]
async fn conflicts_are_refused_to_the_wrong_caller_and_for_what_is_unknown() {
    let f = fx().await;
    conflicting(&f).await;
    let call = |m: wire::GetChangeConflictsRequest, token: &str| {
        let r = req(m, token, false);
        let review = &f.review;
        async move {
            review
                .get_conflicts(r)
                .await
                .map(tonic::Response::into_inner)
        }
    };
    assert_eq!(
        call(conflicts_req(3, ""), "no-such-token")
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert!(
        call(conflicts_req(3, ""), DEPLOY).await.is_ok(),
        "a reader may look"
    );
    assert_eq!(
        call(conflicts_req(9, ""), ALICE).await.unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(
        call(conflicts_req(3, "nope"), ALICE)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        call(conflicts_req(3, "bad\nname"), ALICE)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // The hub's claim for another repository, and a claim that only reads, as the hub gives it.
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    assert_eq!(
        svc.get_conflicts(host(
            &v,
            conflicts_req(3, ""),
            "42:alice",
            Role::Admin,
            "o/other",
            false
        ))
        .await
        .unwrap_err()
        .code(),
        Code::PermissionDenied
    );
    assert!(
        svc.get_conflicts(host(
            &v,
            conflicts_req(3, ""),
            "42:alice",
            Role::Read,
            "o/r",
            false
        ))
        .await
        .is_ok()
    );
}

// ------------------------------------------------------------------------------ draft

fn draft(base: &str, head: &str) -> wire::DraftContextRequest {
    wire::DraftContextRequest {
        repo: repo_ref(),
        base: base.into(),
        head: head.into(),
    }
}

#[tokio::test]
async fn a_draft_is_written_from_the_subjects_files_size_and_risk_of_the_range() {
    let f = fx().await;
    let base = commit(&f.e, &[], &[("src/a.rs", b"1\n2\n")]).await;
    let one = commit(
        &f.e,
        &[base],
        &[("src/a.rs", b"1\n2\n3\n"), ("src/b.rs", b"x\n")],
    )
    .await;
    let two = commit(
        &f.e,
        &[one],
        &[("migrations/001.sql", b"create table t();\n")],
    )
    .await;
    crate::graph_sql::index_for_tests(&f.e, f.reg.pool(), &[base, one, two]).await;
    crate::graph_sql::index_for_tests(&f.e, f.reg.pool(), &[base, one, two]).await;
    set_ref(&f, "refs/heads/master", base).await;
    set_ref(&f, "refs/heads/topic", two).await;
    let got = f
        .review
        .draft_context(req(draft("", "topic"), ALICE, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.changesets, 2);
    assert!(!got.nothing_to_compare && !got.truncated);
    assert_eq!(got.messages, ["m", "m"]);
    let paths: Vec<String> = got
        .files
        .iter()
        .map(|x| String::from_utf8_lossy(&x.path).into_owned())
        .collect();
    assert_eq!(paths, ["migrations/001.sql", "src/a.rs", "src/b.rs"]);
    // The size is what the files add up to.
    assert_eq!(
        (got.added, got.removed),
        (
            got.files.iter().map(|f| f.added).sum::<u32>(),
            got.files.iter().map(|f| f.removed).sum::<u32>()
        )
    );
    assert!(got.added >= 3, "{got:?}");
    let risk = got.risk.unwrap();
    assert!(
        risk.reasons.iter().any(|r| r.contains("migrations")),
        "a migration is a reason: {risk:?}"
    );
    // The same revision twice holds nothing to compare.
    let empty = f
        .review
        .draft_context(req(draft("topic", "topic"), ALICE, false))
        .await
        .unwrap()
        .into_inner();
    assert!(empty.nothing_to_compare && empty.files.is_empty() && empty.changesets == 0);
}

#[tokio::test]
async fn a_draft_is_refused_for_the_wrong_caller_and_for_unknown_revisions() {
    let f = fx().await;
    let base = commit(&f.e, &[], &[("a", b"1\n")]).await;
    crate::graph_sql::index_for_tests(&f.e, f.reg.pool(), &[base]).await;
    set_ref(&f, "refs/heads/master", base).await;
    let call = |m: wire::DraftContextRequest, token: &str| {
        let r = req(m, token, false);
        let review = &f.review;
        async move {
            review
                .draft_context(r)
                .await
                .map(tonic::Response::into_inner)
        }
    };
    assert_eq!(
        call(draft("", ""), "no-such-token")
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert!(call(draft("", ""), DEPLOY).await.is_ok());
    assert_eq!(
        call(draft("", "no-such-branch"), ALICE)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        call(draft(&"x".repeat(2000), ""), ALICE)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    assert_eq!(
        svc.draft_context(host(
            &v,
            draft("", ""),
            "42:alice",
            Role::Admin,
            "o/other",
            false
        ))
        .await
        .unwrap_err()
        .code(),
        Code::PermissionDenied
    );
}

// ------------------------------------------------------------------------------ immutable ids

#[test]
fn the_user_id_is_the_one_the_hub_gave_and_a_token_has_none() {
    use super::{user_id_col, user_id_of};
    assert_eq!(user_id_of("42:alice"), Some(42));
    assert_eq!(user_id_col("42:alice"), 42);
    for none in [
        "pat:alice",
        "0:anonymous",
        "-2:task",
        "deploy:o/r",
        "42:alice:7",
        "host-service",
        "",
        "x:alice",
    ] {
        // `42:alice:7` is a deploy key of the user: it acts for the repository, not for the person.
        assert_eq!(user_id_of(none), None, "{none:?}");
        assert_eq!(user_id_col(none), 0);
    }
}

fn pat_service(f: &Fx) -> super::rpc::ReviewService {
    use asc_rpc::auth::{Access, StaticTokens};
    let auth = StaticTokens::new().with_token("pat-token", "pat:dave", Access::Write);
    super::rpc::ReviewService::new(
        Arc::new(super::Reviews::new(f.reg.clone())),
        Arc::new(auth),
        asc_rpc::server::ServerInfo::default(),
    )
}

#[tokio::test]
async fn a_comment_and_a_verdict_carry_the_hub_id_of_a_person_and_none_for_a_token() {
    let f = fx().await;
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    let comment = |body: &str| wire::AddCommentRequest {
        repo: repo_ref(),
        change: cbytes(1),
        body: body.into(),
        anchor: Some(wire::ThreadAnchor {
            path: bytes::Bytes::from_static(b"f"),
            start_line: 1,
            side: wire::DiffSide::New as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    // A relayed call of the hub: the claim names user 44, and the comment holds that id.
    svc.add_comment(host(
        &v,
        comment("from the hub"),
        "44:carol",
        Role::Write,
        "o/r",
        true,
    ))
    .await
    .unwrap();
    // A caller with a personal token only has a name.
    pat_service(&f)
        .add_comment(req(comment("from a token"), "pat-token", true))
        .await
        .unwrap();
    // And a provider-verified bearer carries its id as well.
    f.review
        .add_comment(req(comment("from alice"), super::tests::CAROL, true))
        .await
        .unwrap();
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT author, author_id FROM review_comments ORDER BY id")
            .fetch_all(f.reg.pool())
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            ("carol".to_owned(), 44),
            ("dave".to_owned(), 0),
            ("carol".to_owned(), 44)
        ]
    );
    // It comes back on the wire.
    let listed = f
        .review
        .list_threads(req(
            wire::ListThreadsRequest {
                repo: repo_ref(),
                change: cbytes(1),
                ..Default::default()
            },
            ALICE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    let mut got: Vec<(String, u64)> = listed
        .threads
        .iter()
        .flat_map(|t| t.comments.iter().map(|c| (c.author.clone(), c.author_id)))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("carol".to_owned(), 44),
            ("carol".to_owned(), 44),
            ("dave".to_owned(), 0)
        ]
    );
    // A verdict records the reviewer's id the same way.
    let approve = |token: &str| {
        let r = req(
            wire::RecordApprovalRequest {
                repo: repo_ref(),
                change: cbytes(2),
                version: 0,
                state: wire::ApprovalState::Approved as i32,
            },
            token,
            true,
        );
        let s = &f.review;
        async move { s.record_approval(r).await }
    };
    approve(super::tests::CAROL).await.unwrap();
    pat_service(&f)
        .record_approval(req(
            wire::RecordApprovalRequest {
                repo: repo_ref(),
                change: cbytes(2),
                version: 0,
                state: wire::ApprovalState::Approved as i32,
            },
            "pat-token",
            true,
        ))
        .await
        .unwrap();
    let ids: Vec<(String, i64)> =
        sqlx::query_as("SELECT reviewer, reviewer_id FROM review_approvals ORDER BY reviewer")
            .fetch_all(f.reg.pool())
            .await
            .unwrap();
    assert_eq!(ids, [("carol".to_owned(), 44), ("dave".to_owned(), 0)]);
}
