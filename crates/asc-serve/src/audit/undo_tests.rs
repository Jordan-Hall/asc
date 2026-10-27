//! Ref log and ref undo over SQLite and a real ref store: the four guards (own operation, newest
//! entry, restorable target, retried op id), the protected-ref window, undo of a land, undo of an
//! undo, the audit trail and what `ListRefLog` shows to whom.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{CallCtx, RepoName, status_rpc_error, wire};
use asc_store::refs::{Actor as RefActor, Expect, RefTxn, RefUpdate, Refs};
use sqlx::Row;
use tonic::Code;
use uuid::Uuid;

use super::store::{self, Filter};
use super::undo::{Caller, RefLog, VIA};
use super::{AuditConfig, Scope};
use crate::ingest::testutil::{commit, registry, repo};
use crate::registry::{Registry, RepoEntry};
use crate::update::PipelineHooks;

const ALICE: &str = "7:alice";
const BOB: &str = "8:bob";
/// A shared (protected) ref of the default policy.
const SHARED: &str = "refs/heads/release/1";
/// Alice's branch as the client names it, and as the server stores it.
const OWN: &str = "refs/heads/work";
const OWN_STORED: &str = "refs/users/7/heads/work";

struct Fx {
    reg: Arc<Registry>,
    entry: Arc<RepoEntry>,
    log: RefLog,
    cs: Vec<ChangesetId>,
}

async fn fx_with(window: u64) -> Fx {
    let reg = registry().await;
    let entry = repo(&reg).await;
    // Branchless, as the user refs of the tests need.
    sqlx::query("UPDATE repos SET branchless = TRUE")
        .execute(reg.pool())
        .await
        .unwrap();
    let log = RefLog::new(reg.clone(), Arc::new(PipelineHooks::default()), window);
    let mut cs = Vec::new();
    for n in 0u8..6 {
        let parents: Vec<ChangesetId> = cs.last().copied().into_iter().collect();
        cs.push(commit(&entry, &parents, &[("f.txt", &[n])]).await);
    }
    Fx {
        reg,
        entry,
        log,
        cs,
    }
}

async fn fx() -> Fx {
    fx_with(600).await
}

fn ctx(principal: &str) -> CallCtx {
    CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: IdMode::Asc,
        principal: Principal {
            id: principal.into(),
        },
        op_id: Some(Uuid::new_v4()),
        client: None,
        admin: Default::default(),
    }
}

fn caller(id: &str, maintainer: bool) -> Caller {
    Caller {
        id: id.into(),
        owner: id.split(':').next().and_then(|n| n.parse().ok()),
        maintainer,
    }
}

fn undo_req(name: &str, seq: u64) -> wire::UndoRefRequest {
    wire::UndoRefRequest {
        repo: None,
        r#ref: name.into(),
        seq,
        dry_run: false,
        reason: "oops".into(),
    }
}

impl Fx {
    /// Moves `name` from `old` to `new` as `actor` and returns the ref log sequence number.
    async fn mv(&self, name: &str, old: Option<usize>, new: Option<usize>, actor: &str) -> u64 {
        let op_id = Uuid::new_v4();
        let target = |i: Option<usize>| i.map(|i| RefTarget::Changeset(self.cs[i]));
        self.reg
            .refs()
            .update(
                self.entry.key,
                &RefTxn {
                    updates: vec![RefUpdate {
                        name: RefName::new(name.as_bytes()).unwrap(),
                        expect: target(old).map_or(Expect::Absent, Expect::Value),
                        new: target(new),
                        expires_at: None,
                    }],
                    actor: RefActor {
                        id: actor.into(),
                        via: "test".into(),
                    },
                    reason: "test".into(),
                    op_id,
                },
            )
            .await
            .unwrap();
        sqlx::query_scalar::<_, i64>(
            "SELECT CAST(MAX(seq) AS BIGINT) FROM ref_log WHERE op_id = $1",
        )
        .bind(op_id.as_bytes().to_vec())
        .fetch_one(self.reg.pool())
        .await
        .map(|s| s as u64)
        .unwrap()
    }

    async fn value(&self, name: &str) -> Option<RefTarget> {
        self.reg
            .refs()
            .get(self.entry.key, &RefName::new(name.as_bytes()).unwrap())
            .await
            .unwrap()
            .map(|e| e.target)
    }

    fn target(&self, i: usize) -> Option<RefTarget> {
        Some(RefTarget::Changeset(self.cs[i]))
    }

    async fn undo(
        &self,
        who: &str,
        maintainer: bool,
        name: &str,
        seq: u64,
    ) -> Result<wire::UndoRefResponse, tonic::Status> {
        self.log
            .undo(&ctx(who), &caller(who, maintainer), undo_req(name, seq))
            .await
    }

    async fn events(&self, action: &str) -> Vec<super::Event> {
        let filter = Filter {
            actions: vec![action.to_owned()],
            ..Filter::default()
        };
        store::list(
            self.reg.pool(),
            &Scope::repo("o/r"),
            &filter,
            true,
            100,
            None,
        )
        .await
        .unwrap()
        .events
    }

    async fn make_old(&self, seq: u64, secs: i64) {
        sqlx::query("UPDATE ref_log SET at = at - $1 WHERE seq = $2")
            .bind(secs)
            .bind(seq as i64)
            .execute(self.reg.pool())
            .await
            .unwrap();
    }
}

fn code_of(e: &tonic::Status) -> String {
    status_rpc_error(e)
        .map(|r| r.code().to_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_own_user_ref_is_put_back_and_the_undo_is_logged() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    let done = f.undo(ALICE, false, OWN, seq).await.unwrap();
    assert!(done.applied);
    assert!(done.new_seq > seq);
    assert_eq!(f.value(OWN_STORED).await, f.target(0));
    let r = done.result.unwrap();
    assert_eq!(
        (r.old, r.new),
        (
            Some(wire::RefTarget::from(&f.target(1).unwrap())),
            Some(wire::RefTarget::from(&f.target(0).unwrap()))
        )
    );
    // The ref log records the undo as its own row, by the caller, via "undo".
    let row = sqlx::query("SELECT actor, via, old, new FROM ref_log WHERE seq = $1")
        .bind(done.new_seq as i64)
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("actor"), ALICE);
    assert_eq!(row.get::<String, _>("via"), VIA);
    // The audit log has the event, with the undone sequence number and the reason.
    let events = f.events("repo.ref.undo").await;
    assert_eq!(events.len(), 1);
    let d = &events[0].details;
    assert_eq!(
        d.get("undone_seq").map(String::as_str),
        Some(seq.to_string().as_str())
    );
    assert_eq!(d.get("reason").map(String::as_str), Some("oops"));
    assert_eq!(events[0].outcome, super::Outcome::Success);
    assert!(
        store::verify_chain(f.reg.pool(), &Scope::repo("o/r"), 0, 0)
            .await
            .unwrap()
            .ok
    );
}

#[tokio::test]
async fn an_undo_can_be_undone_in_turn() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let moved = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    let first = f.undo(ALICE, false, OWN, moved).await.unwrap();
    assert_eq!(f.value(OWN_STORED).await, f.target(0));
    let second = f.undo(ALICE, false, OWN, first.new_seq).await.unwrap();
    assert_eq!(f.value(OWN_STORED).await, f.target(1));
    assert!(second.new_seq > first.new_seq);
    assert_eq!(f.events("repo.ref.undo").await.len(), 2);
    // The undone entries are no longer the newest: undoing the first move again is refused.
    let again = f.undo(ALICE, false, OWN, moved).await.unwrap_err();
    assert_eq!(code_of(&again), "VERSION_CONFLICT");
}

#[tokio::test]
async fn undoing_the_creation_of_a_ref_deletes_it() {
    let f = fx().await;
    let seq = f.mv(OWN_STORED, None, Some(2), ALICE).await;
    f.undo(ALICE, false, OWN, seq).await.unwrap();
    assert_eq!(f.value(OWN_STORED).await, None);
}

#[tokio::test]
async fn undoing_a_deletion_brings_the_ref_back() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(2), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(2), None, ALICE).await;
    f.undo(ALICE, false, OWN, seq).await.unwrap();
    assert_eq!(f.value(OWN_STORED).await, f.target(2));
}

#[tokio::test]
async fn a_ref_that_moved_since_is_a_conflict_and_nothing_changes() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    // Alice pushes again after the entry she wants to undo.
    f.mv(OWN_STORED, Some(1), Some(2), ALICE).await;
    let e = f.undo(ALICE, false, OWN, seq).await.unwrap_err();
    assert_eq!(e.code(), Code::Aborted);
    assert_eq!(code_of(&e), "VERSION_CONFLICT");
    assert_eq!(f.value(OWN_STORED).await, f.target(2));
    assert!(f.events("repo.ref.undo").await.is_empty());
    // A ref that moved away and came back is a different entry too, so the old one stays refused.
    f.mv(OWN_STORED, Some(2), Some(1), ALICE).await;
    assert_eq!(
        code_of(&f.undo(ALICE, false, OWN, seq).await.unwrap_err()),
        "VERSION_CONFLICT"
    );
}

#[tokio::test]
async fn the_race_between_the_check_and_the_write_is_closed_by_the_transaction() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    // Two undos of the same entry with different op ids: exactly one wins.
    let (a, b) = tokio::join!(
        f.undo(ALICE, false, OWN, seq),
        f.undo(ALICE, false, OWN, seq)
    );
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1);
    assert_eq!(f.value(OWN_STORED).await, f.target(0));
    assert_eq!(f.events("repo.ref.undo").await.len(), 1);
}

#[tokio::test]
async fn only_the_actor_of_an_update_can_undo_it() {
    let f = fx().await;
    f.mv(SHARED, None, Some(0), BOB).await;
    let seq = f.mv(SHARED, Some(0), Some(1), BOB).await;
    // Alice is a maintainer, and still not Bob.
    let e = f.undo(ALICE, true, SHARED, seq).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert_eq!(f.value(SHARED).await, f.target(1));
    // The refusal is audited as a denied attempt.
    let denied = f.events("repo.ref.undo").await;
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].outcome, super::Outcome::Denied);
    assert_eq!(denied[0].actor.name, "alice");
}

#[tokio::test]
async fn a_user_ref_of_another_user_is_not_undone_or_listed() {
    let f = fx().await;
    let theirs = "refs/users/8/heads/work";
    let seq = f.mv(theirs, None, Some(0), BOB).await;
    // Alice cannot even name Bob's namespace ...
    assert_eq!(
        f.undo(ALICE, true, theirs, seq).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // ... nor reach his entry through her own branch of the same name.
    assert_eq!(
        f.undo(ALICE, true, OWN, seq).await.unwrap_err().code(),
        Code::NotFound
    );
    // ... and never sees it in the list.
    let all = f
        .log
        .list(
            &ctx(ALICE),
            &caller(ALICE, true),
            wire::ListRefLogRequest::default(),
        )
        .await
        .unwrap();
    assert!(all.entries.is_empty());
}

#[tokio::test]
async fn a_protected_ref_needs_a_maintainer_and_the_window() {
    let f = fx().await;
    f.mv(SHARED, None, Some(0), ALICE).await;
    let seq = f.mv(SHARED, Some(0), Some(1), ALICE).await;
    // Without the right, the typed refusal says so and the way back is a revert.
    let e = f.undo(ALICE, false, SHARED, seq).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert_eq!(code_of(&e), "REF_UNDO_PROTECTED");
    assert_eq!(f.value(SHARED).await, f.target(1));
    // A maintainer inside the window can.
    f.undo(ALICE, true, SHARED, seq).await.unwrap();
    assert_eq!(f.value(SHARED).await, f.target(0));
}

#[tokio::test]
async fn a_protected_ref_is_not_undone_after_the_window() {
    let f = fx().await;
    f.mv(SHARED, None, Some(0), ALICE).await;
    let seq = f.mv(SHARED, Some(0), Some(1), ALICE).await;
    f.make_old(seq, 601).await;
    let e = f.undo(ALICE, true, SHARED, seq).await.unwrap_err();
    assert_eq!(code_of(&e), "REF_UNDO_PROTECTED");
    assert_eq!(f.value(SHARED).await, f.target(1));
}

#[tokio::test]
async fn a_window_of_zero_switches_protected_undo_off_but_not_own_refs() {
    let f = fx_with(0).await;
    f.mv(SHARED, None, Some(0), ALICE).await;
    let shared = f.mv(SHARED, Some(0), Some(1), ALICE).await;
    assert_eq!(
        code_of(&f.undo(ALICE, true, SHARED, shared).await.unwrap_err()),
        "REF_UNDO_PROTECTED"
    );
    let own = f.mv(OWN_STORED, None, Some(0), ALICE).await;
    f.undo(ALICE, false, OWN, own).await.unwrap();
}

#[tokio::test]
async fn a_retried_op_id_returns_the_first_answer_and_writes_once() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    let c = ctx(ALICE);
    let me = caller(ALICE, false);
    let first = f.log.undo(&c, &me, undo_req(OWN, seq)).await.unwrap();
    let second = f.log.undo(&c, &me, undo_req(OWN, seq)).await.unwrap();
    assert_eq!(first.new_seq, second.new_seq);
    assert!(second.applied);
    assert_eq!(f.events("repo.ref.undo").await.len(), 1);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ref_log WHERE via = 'undo'")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    // The same id for another request is refused, not answered with the first result.
    let other = f.log.undo(&c, &me, undo_req(OWN, seq + 1)).await;
    assert!(other.is_ok() || other.unwrap_err().code() != Code::Internal);
}

#[tokio::test]
async fn a_dry_run_changes_nothing() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    let mut req = undo_req(OWN, seq);
    req.dry_run = true;
    let out = f
        .log
        .undo(&ctx(ALICE), &caller(ALICE, false), req)
        .await
        .unwrap();
    assert!(!out.applied);
    assert_eq!(out.new_seq, 0);
    assert_eq!(f.value(OWN_STORED).await, f.target(1));
    assert!(f.events("repo.ref.undo").await.is_empty());
}

#[tokio::test]
async fn bad_requests_are_refused_before_anything_is_read() {
    let f = fx().await;
    let seq = f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let me = caller(ALICE, false);
    let mut c = ctx(ALICE);
    c.op_id = None;
    assert_eq!(
        f.log
            .undo(&c, &me, undo_req(OWN, seq))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.undo(ALICE, false, OWN, 0).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.undo(ALICE, false, "", seq).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        f.undo(ALICE, false, "refs/heads/a\nb", seq)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut long = undo_req(OWN, seq);
    long.reason = "x".repeat(257);
    assert_eq!(
        f.log.undo(&ctx(ALICE), &me, long).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // An entry of another ref, and one that does not exist.
    assert_eq!(
        f.undo(ALICE, false, "refs/heads/other", seq)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        f.undo(ALICE, false, OWN, seq + 1000)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(f.value(OWN_STORED).await, f.target(0));
}

#[tokio::test]
async fn an_entry_older_than_the_log_is_expired() {
    let f = fx().await;
    let old = f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let newer = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    sqlx::query("DELETE FROM ref_log WHERE seq = $1")
        .bind(old as i64)
        .execute(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(
        code_of(&f.undo(ALICE, false, OWN, old).await.unwrap_err()),
        "REF_LOG_EXPIRED"
    );
    f.undo(ALICE, false, OWN, newer).await.unwrap();
}

#[tokio::test]
async fn a_value_that_left_the_repository_is_not_restored() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let seq = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    // Pretend the log entry names a changeset the repository never had.
    sqlx::query("UPDATE ref_log SET old = $1 WHERE seq = $2")
        .bind(
            RefTarget::Changeset(ChangesetId::from_bytes([0xee; 32]))
                .to_bytes()
                .to_vec(),
        )
        .bind(seq as i64)
        .execute(f.reg.pool())
        .await
        .unwrap();
    let e = f.undo(ALICE, false, OWN, seq).await.unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert_eq!(f.value(OWN_STORED).await, f.target(1));
}

#[tokio::test]
async fn undoing_a_land_removes_what_the_land_recorded() {
    let f = fx().await;
    f.mv(SHARED, None, Some(0), ALICE).await;
    let land = f.mv(SHARED, Some(0), Some(2), ALICE).await;
    let op: Vec<u8> = sqlx::query_scalar("SELECT op_id FROM ref_log WHERE seq = $1")
        .bind(land as i64)
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    let ns = f.entry.ns.as_bytes().to_vec();
    sqlx::query("INSERT INTO land_map_ref (ns, ref, old_cs, new_cs, ref_log_seq, op_id) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(ns.clone())
        .bind(SHARED)
        .bind(vec![1u8; 32])
        .bind(vec![2u8; 32])
        .bind(land as i64)
        .bind(op.clone())
        .execute(f.reg.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_ref_meta (ns, owner, name, landed_at) VALUES ($1, 7, 'work', $2)",
    )
    .bind(ns.clone())
    .bind(crate::update::now())
    .execute(f.reg.pool())
    .await
    .unwrap();
    f.undo(ALICE, true, SHARED, land).await.unwrap();
    assert_eq!(f.value(SHARED).await, f.target(0));
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM land_map_ref WHERE ns = $1")
        .bind(ns.clone())
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    let landed: Option<i64> =
        sqlx::query_scalar("SELECT landed_at FROM user_ref_meta WHERE ns = $1")
            .bind(ns)
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
    assert_eq!(landed, None, "the stack shows as not landed again");
}

#[tokio::test]
async fn a_mirror_or_an_unknown_repository_is_not_undone() {
    let f = fx().await;
    let mut c = ctx(ALICE);
    c.repo = RepoName::parse("o/missing").unwrap();
    let e = f
        .log
        .undo(&c, &caller(ALICE, true), undo_req(OWN, 1))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
}

#[tokio::test]
async fn the_list_marks_what_can_be_undone_and_why_not() {
    let f = fx().await;
    f.mv(OWN_STORED, None, Some(0), ALICE).await;
    let mine = f.mv(OWN_STORED, Some(0), Some(1), ALICE).await;
    f.mv(SHARED, None, Some(0), BOB).await;
    let bobs = f.mv(SHARED, Some(0), Some(1), BOB).await;
    let list = f
        .log
        .list(
            &ctx(ALICE),
            &caller(ALICE, true),
            wire::ListRefLogRequest::default(),
        )
        .await
        .unwrap();
    // Newest first; Alice's own ref is shown under its client name.
    let seqs: Vec<u64> = list.entries.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(seqs, sorted);
    let by = |seq: u64| list.entries.iter().find(|e| e.seq == seq).unwrap();
    assert_eq!(by(mine).r#ref, "refs/heads/work");
    assert!(by(mine).undoable);
    assert!(by(mine).undo_blocked_reason.is_empty());
    assert!(!by(bobs).undoable);
    assert_eq!(by(bobs).undo_blocked_reason, "not your operation");
    assert_eq!(by(bobs).actor, "bob");
    // Entries that are no longer the newest of their ref say so.
    let first_of_own = list
        .entries
        .iter()
        .filter(|e| e.r#ref == "refs/heads/work")
        .min_by_key(|e| e.seq)
        .unwrap();
    assert!(!first_of_own.undoable);
    assert_eq!(first_of_own.undo_blocked_reason, "the ref changed since");
}

#[tokio::test]
async fn the_list_filters_and_pages() {
    let f = fx().await;
    for i in 0..5 {
        let (old, new) = (if i == 0 { None } else { Some(i - 1) }, Some(i));
        f.mv(SHARED, old, new, if i % 2 == 0 { ALICE } else { BOB })
            .await;
    }
    let me = caller(ALICE, false);
    let page = |req: wire::ListRefLogRequest| {
        let f = &f;
        let me = me.clone();
        async move { f.log.list(&ctx(ALICE), &me, req).await.unwrap() }
    };
    let first = page(wire::ListRefLogRequest {
        limit: 2,
        ..Default::default()
    })
    .await;
    assert_eq!(first.entries.len(), 2);
    assert!(!first.next_page_token.is_empty());
    let second = page(wire::ListRefLogRequest {
        limit: 2,
        page_token: first.next_page_token.clone(),
        ..Default::default()
    })
    .await;
    assert!(second.entries[0].seq < first.entries[1].seq);
    let rest = page(wire::ListRefLogRequest {
        limit: 10,
        page_token: second.next_page_token.clone(),
        ..Default::default()
    })
    .await;
    assert_eq!(
        first.entries.len() + second.entries.len() + rest.entries.len(),
        5
    );
    assert!(rest.next_page_token.is_empty());
    let bobs = page(wire::ListRefLogRequest {
        actor: "bob".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(bobs.entries.len(), 2);
    assert!(bobs.entries.iter().all(|e| e.actor == "bob"));
    let by_ref = page(wire::ListRefLogRequest {
        r#ref: SHARED.into(),
        ..Default::default()
    })
    .await;
    assert_eq!(by_ref.entries.len(), 5);
    let prefix = page(wire::ListRefLogRequest {
        r#ref: "refs/heads/".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(prefix.entries.len(), 5);
    let none = page(wire::ListRefLogRequest {
        r#ref: "refs/tags/".into(),
        ..Default::default()
    })
    .await;
    assert!(none.entries.is_empty());
    // Hostile input is refused.
    for bad in [
        wire::ListRefLogRequest {
            page_token: "1; DROP TABLE ref_log".into(),
            ..Default::default()
        },
        wire::ListRefLogRequest {
            since: 10,
            until: 5,
            ..Default::default()
        },
        wire::ListRefLogRequest {
            since: -1,
            ..Default::default()
        },
        wire::ListRefLogRequest {
            actor: "a\u{0}b".into(),
            ..Default::default()
        },
    ] {
        assert_eq!(
            f.log.list(&ctx(ALICE), &me, bad).await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }
    // A LIKE wildcard in the actor filter matches nothing it should not.
    let wild = page(wire::ListRefLogRequest {
        actor: "%".into(),
        ..Default::default()
    })
    .await;
    assert!(wild.entries.is_empty());
}

#[tokio::test]
async fn the_config_default_window_is_ten_minutes() {
    assert_eq!(AuditConfig::default().undo_window_secs, 600);
}
