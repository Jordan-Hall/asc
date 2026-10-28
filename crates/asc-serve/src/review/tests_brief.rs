//! The review brief, risk, provenance, approvals with carry-over, version comparison, suggested
//! changes and reviewer suggestions. Each call is tried with the right caller, a caller without
//! the right, an unknown change and another repository's claim.

use asc_core::id::ChangesetId;
use asc_rpc::forwarded::Role;
use asc_rpc::server::wire;
use asc_rpc::services::review_server::Review;
use bytes::Bytes;
use tonic::Code;

use super::brief::classify;
use super::tests::{ALICE, BOB, CAROL, DAVE, Fx, cbytes, fx, repo_ref, req};
use super::tests_acting::claimkit::{self, host};
use crate::ingest::testutil::commit;

fn fstat(path: &str, added: u32, removed: u32) -> wire::ChangeFileStat {
    wire::ChangeFileStat {
        path: Bytes::copy_from_slice(path.as_bytes()),
        added,
        removed,
        binary: false,
    }
}

async fn add_version(f: &Fx, change: u8, v: i64, cs: ChangesetId, actor: &str) {
    sqlx::query(
        "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
         VALUES ($1, $2, $3, $4, NULL, 1, $5, $6)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(super::tests::cid(change).as_bytes().to_vec())
    .bind(v)
    .bind(cs.as_bytes().to_vec())
    .bind(actor.to_owned())
    .bind(vec![change, u8::try_from(v).unwrap(), 9])
    .execute(f.reg.pool())
    .await
    .unwrap();
}

async fn brief(f: &Fx, token: &str, change: u8) -> Result<wire::GetBriefResponse, tonic::Status> {
    f.review
        .get_brief(req(wire::GetBriefRequest { repo: repo_ref(), change: cbytes(change) }, token, false))
        .await
        .map(tonic::Response::into_inner)
}

async fn approve(f: &Fx, token: &str, change: u8, version: u32, state: wire::ApprovalState) -> Result<wire::RecordApprovalResponse, tonic::Status> {
    f.review
        .record_approval(req(
            wire::RecordApprovalRequest { repo: repo_ref(), change: cbytes(change), version, state: state as i32 },
            token,
            true,
        ))
        .await
        .map(tonic::Response::into_inner)
}

#[test]
fn the_risk_classifier_names_its_reasons() {
    let low = classify(&[fstat("src/lib.rs", 10, 2)], 10, 2);
    assert_eq!(low.level, wire::RiskLevel::Low as i32);
    let auth = classify(&[fstat("src/auth/session.rs", 3, 1)], 3, 1);
    assert_eq!(auth.level, wire::RiskLevel::High as i32);
    assert!(auth.reasons[0].contains("authentication"), "{:?}", auth.reasons);
    let mig = classify(&[fstat("db/migrations/0001.sql", 3, 0)], 3, 0);
    assert_eq!(mig.level, wire::RiskLevel::High as i32);
    let big = classify(&[fstat("src/a.rs", 400, 10)], 400, 10);
    assert_eq!(big.level, wire::RiskLevel::Medium as i32);
    assert!(big.reasons.iter().any(|r| r.contains("410 lines")));
    let huge = classify(&[fstat("src/a.rs", 1200, 0), fstat("src/a_test.rs", 1, 0)], 1201, 0);
    assert_eq!(huge.level, wire::RiskLevel::High as i32);
}

#[tokio::test]
async fn a_brief_is_written_by_its_author_or_a_writer_and_read_by_anyone_who_may_read() {
    let f = fx().await;
    let empty = brief(&f, DAVE, 1).await.unwrap();
    assert_eq!(empty.latest_version, 2);
    assert_eq!(empty.brief.unwrap().what, "");
    assert_eq!(empty.risk.unwrap().level, wire::RiskLevel::Low as i32);
    let b = |what: &str| wire::ReviewBrief {
        what: what.into(),
        why: "because".into(),
        how: "by hand".into(),
        risk_note: "none".into(),
        where_to_look: vec![wire::WhereToLook { path: Bytes::from_static(b"f"), line: 3, note: "start here".into() }],
        ..Default::default()
    };
    let set = |token: &str, change: u8, brief: wire::ReviewBrief| {
        let r = req(wire::SetBriefRequest { repo: repo_ref(), change: cbytes(change), brief: Some(brief) }, token, true);
        let svc = &f.review;
        async move { svc.set_brief(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(set(DAVE, 1, b("x")).await.unwrap_err().code(), Code::PermissionDenied, "a reader who is not the author");
    assert_eq!(set(BOB, 1, b("x")).await.unwrap_err().code(), Code::PermissionDenied, "bob wrote change 2, not 1");
    let by_author = set(ALICE, 1, b("by alice")).await.unwrap().brief.unwrap();
    assert_eq!((by_author.what.as_str(), by_author.updated_by.as_str()), ("by alice", "alice"));
    set(CAROL, 1, b("by carol")).await.unwrap();
    let got = brief(&f, DAVE, 1).await.unwrap().brief.unwrap();
    assert_eq!((got.what.as_str(), got.updated_by.as_str()), ("by carol", "carol"));
    assert_eq!(got.where_to_look[0].note, "start here");
    // Limits and unknown changes.
    assert_eq!(set(CAROL, 1, wire::ReviewBrief { what: "x".repeat(8 * 1024 + 1), ..Default::default() }).await.unwrap_err().code(), Code::InvalidArgument);
    let many = wire::ReviewBrief { where_to_look: vec![wire::WhereToLook::default(); 21], ..Default::default() };
    assert_eq!(set(CAROL, 1, many).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(set(CAROL, 9, b("x")).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(brief(&f, DAVE, 9).await.unwrap_err().code(), Code::NotFound);
    let short = f
        .review
        .get_brief(req(wire::GetBriefRequest { repo: repo_ref(), change: Bytes::from_static(b"short") }, DAVE, false))
        .await
        .unwrap_err();
    assert_eq!(short.code(), Code::InvalidArgument);
    // Another repository's claim is no claim here; the host with this repository's claim reads.
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        std::sync::Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    let msg = || wire::GetBriefRequest { repo: repo_ref(), change: cbytes(1) };
    assert_eq!(
        svc.get_brief(host(&v, msg(), "42:alice", Role::Admin, "o/other", false)).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert!(svc.get_brief(host(&v, msg(), "42:alice", Role::Read, "o/r", false)).await.is_ok());
    let w = wire::SetBriefRequest { repo: repo_ref(), change: cbytes(1), brief: Some(b("from the hub")) };
    assert!(svc.set_brief(host(&v, w, "42:alice", Role::Read, "o/r", true)).await.is_ok(), "alice wrote the change");
    let w = wire::SetBriefRequest { repo: repo_ref(), change: cbytes(1), brief: Some(b("nope")) };
    assert_eq!(svc.set_brief(host(&v, w, "46:dave", Role::Read, "o/r", true)).await.unwrap_err().code(), Code::PermissionDenied);
    assert!(claimkit::audited(f.reg.pool(), "review.brief.set").await.contains(&("alice".to_owned(), "host".to_owned())));
}

#[tokio::test]
async fn an_approval_carries_over_while_the_patch_is_the_same_and_names_the_files_that_differ() {
    let f = fx().await;
    assert_eq!(approve(&f, BOB, 1, 1, wire::ApprovalState::Approved).await.unwrap_err().code(), Code::PermissionDenied, "a reader cannot approve");
    assert_eq!(approve(&f, CAROL, 1, 1, wire::ApprovalState::Unspecified).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(approve(&f, CAROL, 1, 9, wire::ApprovalState::Approved).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(approve(&f, CAROL, 9, 1, wire::ApprovalState::Approved).await.unwrap_err().code(), Code::NotFound);
    // Versions 1 and 2 of change 1 are the same changeset: the approval of version 1 still applies.
    let r = approve(&f, CAROL, 1, 1, wire::ApprovalState::Approved).await.unwrap();
    assert_eq!(r.approvals.len(), 1);
    let a = &r.approvals[0];
    assert!(a.applies && a.carried && a.changed_files.is_empty(), "{a:?}");
    let gate = brief(&f, DAVE, 1).await.unwrap().gate.unwrap();
    assert_eq!((gate.approvals_current, gate.approvals_carried), (1, 1));
    // A third version that changes `f` and adds `g`: nothing carries over.
    let cs3 = commit(&f.e, &[], &[("f", &[9]), ("g", &[1])]).await;
    add_version(&f, 1, 3, cs3, "42:alice").await;
    let a = &brief(&f, DAVE, 1).await.unwrap().approvals[0];
    assert!(!a.applies && !a.carried, "{a:?}");
    assert_eq!(a.changed_files.len(), 2);
    // A fourth version keeps the patch of `f` and adds `g`: the approval does not apply to the new file, and the comparison says so.
    let cs4 = commit(&f.e, &[], &[("f", &[1]), ("g", &[2])]).await;
    add_version(&f, 1, 4, cs4, "42:alice").await;
    let a = &brief(&f, DAVE, 1).await.unwrap().approvals[0];
    assert!(!a.applies);
    assert_eq!((a.unchanged_files, a.changed_files.len()), (1, 1), "{a:?}");
    assert_eq!(a.changed_files[0].as_ref(), b"g");
    let cmp = f
        .review
        .compare_versions(req(
            wire::CompareVersionsRequest { repo: repo_ref(), change: cbytes(1), from_version: 1, to_version: 0 },
            DAVE,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    let state = |p: &str| cmp.files.iter().find(|x| x.path.as_ref() == p.as_bytes()).map(|x| x.state.clone());
    assert_eq!(state("f").as_deref(), Some("unchanged"));
    assert_eq!(state("g").as_deref(), Some("added"));
    assert_eq!((cmp.changed, cmp.unchanged), (1, 1));
    assert_eq!(cmp.approvals.len(), 1, "the verdicts on the version compared from");
    let bad = f
        .review
        .compare_versions(req(
            wire::CompareVersionsRequest { repo: repo_ref(), change: cbytes(1), from_version: 7, to_version: 0 },
            DAVE,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
    // A second verdict of the same person on the same version replaces the first; a dismissal does not apply.
    let r = approve(&f, CAROL, 1, 1, wire::ApprovalState::Dismissed).await.unwrap();
    assert_eq!(r.approvals.len(), 1);
    assert!(!r.approvals[0].applies);
}

#[tokio::test]
async fn the_agent_run_that_made_a_change_is_its_provenance() {
    let f = fx().await;
    assert_eq!(brief(&f, DAVE, 1).await.unwrap().provenance.unwrap().run_id, 0);
    let change_hex: String = super::tests::cid(1).as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    sqlx::query(
        "INSERT INTO agent_runs (id, repo, agent, title, state, target, base, task, paths_json, created_by, starter, created, started, finished, changes_json) \
         VALUES (5, 'o/r', 'fixer', 'Fix it', 4, 'master', $1, 'Rename the helper', '[\"src/**\"]', '42:alice', 'alice', 10, 11, 20, $2)",
    )
    .bind(vec![0u8; 0])
    .bind(format!("[\"{change_hex}\"]"))
    .execute(f.reg.pool())
    .await
    .unwrap();
    for (seq, tool) in [(1, "read_file"), (2, "read_file"), (3, "write_file")] {
        sqlx::query(
            "INSERT INTO agent_run_events (run_id, seq, kind, time_ms, state, text, tool, change) VALUES (5, $1, 4, 1, 2, '', $2, $3)",
        )
        .bind(seq)
        .bind(tool)
        .bind(vec![0u8; 0])
        .execute(f.reg.pool())
        .await
        .unwrap();
    }
    let p = brief(&f, DAVE, 1).await.unwrap().provenance.unwrap();
    assert_eq!((p.run_id, p.agent.as_str(), p.task.as_str(), p.created_by.as_str()), (5, "fixer", "Rename the helper", "alice"));
    assert_eq!((p.tool_calls, p.files_read, p.versions), (3, 2, 2));
    assert_eq!(p.run_state, "succeeded");
    assert_eq!(p.paths, ["src/**"]);
    assert_eq!(brief(&f, DAVE, 2).await.unwrap().provenance.unwrap().run_id, 0, "another change has none");
}

fn anchor(version: u32, line: u32) -> Option<wire::ThreadAnchor> {
    Some(wire::ThreadAnchor {
        path: Bytes::from_static(b"f"),
        version,
        side: wire::DiffSide::New as i32,
        start_line: line,
        end_line: line,
    })
}

async fn suggest(f: &Fx, token: &str, anchor: Option<wire::ThreadAnchor>, suggestion: &str) -> Result<wire::AddCommentResponse, tonic::Status> {
    f.review
        .add_comment(req(
            wire::AddCommentRequest {
                repo: repo_ref(),
                change: cbytes(1),
                anchor,
                body: "please change".into(),
                suggestion: suggestion.into(),
                ..Default::default()
            },
            token,
            true,
        ))
        .await
        .map(tonic::Response::into_inner)
}

#[tokio::test]
async fn a_suggestion_is_prepared_as_the_new_file_and_marked_when_a_version_applies_it() {
    let f = fx().await;
    // A suggestion needs a thread on lines.
    assert_eq!(suggest(&f, CAROL, None, "x\n").await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(suggest(&f, CAROL, anchor(0, 1), &"x".repeat(64 * 1024 + 1)).await.unwrap_err().code(), Code::InvalidArgument);
    let added = suggest(&f, CAROL, anchor(0, 1), "replaced\n").await.unwrap();
    let thread = added.thread.unwrap();
    assert_eq!(thread.comments[0].suggestion, "replaced\n");
    let prep = |token: &str, cid: u64| {
        let r = req(
            wire::PrepareSuggestionRequest { repo: repo_ref(), change: cbytes(1), thread_id: thread.id, comment_id: cid },
            token,
            false,
        );
        let svc = &f.review;
        async move { svc.prepare_suggestion(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(prep(DAVE, added.comment_id).await.unwrap_err().code(), Code::PermissionDenied, "preparing an apply needs write");
    assert_eq!(prep(CAROL, 9_999).await.unwrap_err().code(), Code::NotFound);
    let p = prep(CAROL, added.comment_id).await.unwrap();
    assert!(!p.stale, "{p:?}");
    assert_eq!(p.new_content.as_ref(), b"replaced\n");
    assert_eq!((p.version, p.latest_version, p.path.as_ref()), (2, 2, &b"f"[..]));
    // A comment without a suggestion cannot be prepared.
    let plain = f
        .review
        .add_comment(req(
            wire::AddCommentRequest { repo: repo_ref(), change: cbytes(1), thread_id: thread.id, body: "plain".into(), ..Default::default() },
            CAROL,
            true,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(prep(CAROL, plain.comment_id).await.unwrap_err().code(), Code::FailedPrecondition);
    // Marking: the version must exist and be later than the thread's.
    let mark = |token: &str, version: u32| {
        let r = req(
            wire::MarkSuggestionAppliedRequest { repo: repo_ref(), change: cbytes(1), thread_id: thread.id, comment_id: added.comment_id, version },
            token,
            true,
        );
        let svc = &f.review;
        async move { svc.mark_suggestion_applied(r).await.map(tonic::Response::into_inner) }
    };
    assert_eq!(mark(CAROL, 3).await.unwrap_err().code(), Code::InvalidArgument, "version 3 does not exist yet");
    assert_eq!(mark(CAROL, 2).await.unwrap_err().code(), Code::InvalidArgument, "not later than the thread's");
    let cs3 = commit(&f.e, &[], &[("f", b"replaced\n")]).await;
    add_version(&f, 1, 3, cs3, "44:carol").await;
    assert_eq!(mark(DAVE, 3).await.unwrap_err().code(), Code::PermissionDenied);
    let t = mark(CAROL, 3).await.unwrap().thread.unwrap();
    let c = t.comments.iter().find(|c| c.id == added.comment_id).unwrap();
    assert_eq!((c.applied_version, c.applied_by.as_str()), (3, "carol"));
    assert_eq!(mark(CAROL, 3).await.unwrap_err().code(), Code::AlreadyExists);
    let again = prep(CAROL, added.comment_id).await.unwrap();
    assert!(again.stale && again.reason.contains("applied already"), "{again:?}");
    // A thread on an older version is stale: the change has moved on.
    let old = suggest(&f, CAROL, anchor(1, 1), "older\n").await.unwrap();
    let r = f
        .review
        .prepare_suggestion(req(
            wire::PrepareSuggestionRequest { repo: repo_ref(), change: cbytes(1), thread_id: old.thread.unwrap().id, comment_id: old.comment_id },
            CAROL,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(r.stale && r.reason.contains("moved on"), "{r:?}");
}

#[tokio::test]
async fn reviewers_are_suggested_from_history_without_the_author() {
    let f = fx().await;
    let ns = f.e.ns.as_bytes().to_vec();
    let c1 = super::tests::cid(2).as_bytes().to_vec();
    for (who, n) in [("bob", 3), ("carol", 1), ("alice", 5), ("fixer", 2)] {
        for i in 0..n {
            let mut change = c1.clone();
            change.push(i);
            sqlx::query("INSERT INTO review_requests (ns, change, reviewer, requested_by, requested_at, done_at) VALUES ($1, $2, $3, 'x', 1, 2)")
                .bind(ns.clone())
                .bind(change)
                .bind(who.to_owned())
                .execute(f.reg.pool())
                .await
                .unwrap();
        }
    }
    sqlx::query(
        "INSERT INTO agent_runs (id, repo, agent, state, target, base, task, created_by, created) VALUES (7, 'o/r', 'fixer', 4, 'master', $1, 't', '42:alice', 1)",
    )
    .bind(vec![0u8; 0])
    .execute(f.reg.pool())
    .await
    .unwrap();
    let r = f
        .review
        .suggest_reviewers(req(wire::SuggestReviewersRequest { repo: repo_ref(), change: cbytes(1), limit: 0 }, DAVE, false))
        .await
        .unwrap()
        .into_inner();
    let names: Vec<&str> = r.reviewers.iter().map(|x| x.handle.as_str()).collect();
    assert_eq!(names, ["bob", "fixer", "carol"], "alice wrote change 1, so she is left out");
    assert!(r.reviewers.iter().find(|x| x.handle == "fixer").unwrap().is_agent);
    let one = f
        .review
        .suggest_reviewers(req(wire::SuggestReviewersRequest { repo: repo_ref(), change: cbytes(1), limit: 1 }, DAVE, false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(one.reviewers.len(), 1);
    let missing = f
        .review
        .suggest_reviewers(req(wire::SuggestReviewersRequest { repo: repo_ref(), change: cbytes(9), limit: 0 }, DAVE, false))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
}

#[tokio::test]
async fn an_author_and_an_agent_cannot_approve_and_an_old_verdict_of_either_does_not_count() {
    let f = fx().await;
    // Carol publishes version 3 of change 1, so she is one of its authors.
    let cs3 = commit(&f.e, &[], &[("f", &[7])]).await;
    add_version(&f, 1, 3, cs3, "44:carol").await;
    let e = approve(&f, CAROL, 1, 0, wire::ApprovalState::Approved).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(e.message().contains("author"), "{e:?}");
    // She may still ask for changes.
    assert!(approve(&f, CAROL, 1, 0, wire::ApprovalState::ChangesRequested).await.is_ok());
    // A verdict recorded before she was an author (or by a policy that changed) does not count.
    sqlx::query("INSERT INTO review_approvals (ns, change, reviewer, version, state, at) VALUES ($1, $2, 'carol', 1, 1, 1)")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(super::tests::cid(1).as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    // A person who is no author approves, and that counts.
    assert!(approve(&f, super::tests::ADMIN, 1, 0, wire::ApprovalState::Approved).await.is_ok());
    let b = brief(&f, DAVE, 1).await.unwrap();
    let carol = b.approvals.iter().find(|a| a.reviewer == "carol" && a.version == 1).unwrap();
    assert!(!carol.applies && !carol.carried, "{carol:?}");
    assert_eq!(b.gate.unwrap().approvals_current, 1);
    // An agent's name is not a person who may approve.
    sqlx::query(
        "INSERT INTO agent_runs (id, repo, agent, state, target, base, task, created_by, created) VALUES (9, 'o/r', 'robot', 4, 'master', $1, 't', '42:alice', 1)",
    )
    .bind(Vec::<u8>::new())
    .execute(f.reg.pool())
    .await
    .unwrap();
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        std::sync::Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    let m = || wire::RecordApprovalRequest { repo: repo_ref(), change: cbytes(2), version: 0, state: wire::ApprovalState::Approved as i32 };
    let e = svc.record_approval(host(&v, m(), "50:robot", Role::Write, "o/r", true)).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(e.message().contains("agent"), "{e:?}");
    assert!(svc.record_approval(host(&v, m(), "51:dana", Role::Write, "o/r", true)).await.is_ok());
    assert_eq!(
        svc.record_approval(host(&v, m(), "51:dana", Role::Read, "o/r", true)).await.unwrap_err().code(),
        Code::PermissionDenied,
        "a read claim does not approve"
    );
}

async fn set_trunk_policy(f: &Fx, toml: &str) {
    use asc_core::id::RefTarget;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs as _};
    let cs = commit(&f.e, &[], &[(".asc/policy.toml", toml.as_bytes())]).await;
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: asc_core::path::RefName::new(format!("refs/heads/{}", f.e.default_branch).as_bytes()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor { id: "t".into(), via: "test".into() },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    f.reg.refs().update(f.e.key, &txn).await.unwrap();
}

#[tokio::test]
async fn the_policy_can_let_an_author_approve_and_then_the_verdict_counts() {
    let f = fx().await;
    let cs3 = commit(&f.e, &[], &[("f", &[7])]).await;
    add_version(&f, 1, 3, cs3, "44:carol").await;
    // Without a policy the strict default holds.
    assert_eq!(approve(&f, CAROL, 1, 0, wire::ApprovalState::Approved).await.unwrap_err().code(), Code::PermissionDenied);
    set_trunk_policy(&f, "version = 1
[roles]
maintainer = [\"user:alice\"]
[review]
author_may_approve = true
").await;
    approve(&f, CAROL, 1, 0, wire::ApprovalState::Approved).await.unwrap();
    let b = brief(&f, DAVE, 1).await.unwrap();
    assert_eq!(b.gate.unwrap().approvals_current, 1, "the policy lets the author's verdict count");
    assert!(b.approvals.iter().any(|a| a.reviewer == "carol" && a.applies));
}

#[test]
fn a_suggestion_is_found_at_its_line_or_as_a_deletion() {
    use super::brief::contains_suggestion as has;
    let old = b"a
b
c
d
";
    assert!(has(old, b"a
B!
c
d
", 2, 2, "B!
"));
    assert!(has(old, b"a
x
y
d
", 2, 3, "x
y"), "the last suggestion line may lack its newline");
    assert!(has(old, b"a
B!
c
d
", 2, 2, "B!
"), "line endings are not compared");
    assert!(!has(old, old, 2, 2, "B!
"), "an unchanged file does not hold it");
    assert!(!has(old, b"B!
b
c
d
", 2, 2, "B!
"), "it must be at the line");
    assert!(has(old, b"a
d
", 2, 3, ""), "a deletion removes the lines");
    assert!(!has(old, old, 2, 3, ""));
    assert!(!has(old, b"x", 0, 0, "x"));
    assert!(!has(&[0xff], b"x", 1, 1, "x"));
}

#[tokio::test]
async fn marking_a_suggestion_applied_needs_a_version_that_holds_it() {
    let f = fx().await;
    let added = suggest(&f, CAROL, anchor(0, 1), "replaced
").await.unwrap();
    let thread = added.thread.unwrap();
    let mark = |version: u32| {
        let r = req(
            wire::MarkSuggestionAppliedRequest { repo: repo_ref(), change: cbytes(1), thread_id: thread.id, comment_id: added.comment_id, version },
            CAROL,
            true,
        );
        let svc = &f.review;
        async move { svc.mark_suggestion_applied(r).await.map(tonic::Response::into_inner) }
    };
    // Version 3 changes the file, but not to the suggestion.
    let other = commit(&f.e, &[], &[("f", b"something else
")]).await;
    add_version(&f, 1, 3, other, "44:carol").await;
    assert_eq!(mark(3).await.unwrap_err().code(), Code::FailedPrecondition);
    let right = commit(&f.e, &[], &[("f", b"replaced
")]).await;
    add_version(&f, 1, 4, right, "44:carol").await;
    assert!(mark(4).await.is_ok());
}
