use super::*;
use crate::home::{AscHome, RepoId};
use crate::state::repos::{self, NewRepo, RemoteKind};

fn cs(n: u8) -> ChangesetId {
    ChangesetId::of(&[n])
}

fn ch(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

async fn open(dir: &tempfile::TempDir) -> (StateDb, Drafts) {
    let home = AscHome::open(dir.path().join("home")).unwrap();
    let db = StateDb::open(&home).await.unwrap();
    let repo = repos::insert(
        &db,
        &NewRepo {
            id: RepoId::new(),
            name: "r".into(),
            ns: vec![1],
            remote_url: None,
            remote_kind: RemoteKind::Server,
            default_branch: "master".into(),
            windows_safe: true,
        },
    )
    .await
    .unwrap();
    (db, Drafts::new(repo))
}

async fn heads(db: &StateDb, d: &Drafts) -> Vec<ChangesetId> {
    let mut v: Vec<_> = d
        .visible(db, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.cs)
        .collect();
    v.sort();
    v
}

async fn queued(db: &StateDb, d: &Drafts) -> Vec<ChangesetId> {
    let mut v: Vec<_> = d
        .due(db, i64::MAX, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.cs)
        .collect();
    v.sort();
    v
}

fn sorted(mut v: Vec<ChangesetId>) -> Vec<ChangesetId> {
    v.sort();
    v
}

#[tokio::test]
async fn new_changeset_replaces_parent_head_and_coalesces_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(1), ch(1), &[cs(0)], 10, QUEUE_MAX)
        .await
        .unwrap();
    d.new_changeset(&mut tx, cs(2), ch(2), &[cs(1)], 11, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(heads(&db, &d).await, vec![cs(2)]);
    assert_eq!(queued(&db, &d).await, vec![cs(2)]);
}

#[tokio::test]
async fn started_parent_stays_queued() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(1), ch(1), &[], 10, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    d.queue_failed(&db, cs(1), QueueState::Failed, 99, "boom")
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(2), ch(2), &[cs(1)], 11, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(heads(&db, &d).await, vec![cs(2)]);
    // The failed parent is not pending, so only the new head is due; the parent row survives.
    assert_eq!(queued(&db, &d).await, vec![cs(2)]);
    assert_eq!(d.queue_len(&db).await.unwrap(), 2);
}

#[tokio::test]
async fn rewrite_swaps_head_and_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(1), ch(1), &[], 10, QUEUE_MAX)
        .await
        .unwrap();
    d.rewrite(&mut tx, cs(1), (cs(5), ch(1)), &[], 12, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(heads(&db, &d).await, vec![cs(5)]);
    assert_eq!(queued(&db, &d).await, vec![cs(5)]);
    assert_eq!(d.visible(&db, 10).await.unwrap()[0].change, ch(1));
}

#[tokio::test]
async fn immutable_drops_heads_and_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    for n in 1..=3 {
        d.new_changeset(&mut tx, cs(n), ch(n), &[], 10, QUEUE_MAX)
            .await
            .unwrap();
    }
    d.immutable(&mut tx, &[cs(1), cs(3), cs(9)]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(heads(&db, &d).await, vec![cs(2)]);
    assert_eq!(queued(&db, &d).await, vec![cs(2)]);
}

#[tokio::test]
async fn hide_and_unhide() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(2), ch(2), &[], 10, QUEUE_MAX)
        .await
        .unwrap();
    d.hide(&mut tx, &[cs(2)], Some((cs(1), ch(1))), 20, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(heads(&db, &d).await, vec![cs(1)]);
    assert_eq!(queued(&db, &d).await, vec![cs(1)]);
    let hidden = d.hidden(&db, 10).await.unwrap();
    assert_eq!(hidden.len(), 1);
    assert_eq!((hidden[0].cs, hidden[0].hidden_at), (cs(2), Some(20)));

    let mut tx = db.begin().await.unwrap();
    d.unhide(&mut tx, cs(2), ch(2), 30, QUEUE_MAX)
        .await
        .unwrap();
    d.unhide(&mut tx, cs(7), ch(7), 30, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(heads(&db, &d).await, sorted(vec![cs(1), cs(2), cs(7)]));
    assert_eq!(queued(&db, &d).await, sorted(vec![cs(1), cs(2), cs(7)]));
    assert!(d.hidden(&db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn maintenance_rolls_back_with_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(1), ch(1), &[], 10, QUEUE_MAX)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(heads(&db, &d).await.is_empty());
    assert_eq!(d.queue_len(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn queue_cap_keeps_heads_local() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    let a = d
        .new_changeset(&mut tx, cs(1), ch(1), &[], 1, 1)
        .await
        .unwrap();
    let b = d
        .new_changeset(&mut tx, cs(2), ch(2), &[], 2, 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((a, b), (Enqueued::Queued, Enqueued::Full));
    assert_eq!(heads(&db, &d).await.len(), 2);
    assert_eq!(queued(&db, &d).await, vec![cs(1)]);
}

#[tokio::test]
async fn failure_backoff_and_done() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, cs(1), ch(1), &[], 1, QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let long = "é".repeat(MAX_ERROR_LEN);
    d.queue_failed(&db, cs(1), QueueState::Pending, 50, &long)
        .await
        .unwrap();
    assert!(d.due(&db, 49, 10).await.unwrap().is_empty());
    let row = d.due(&db, 50, 10).await.unwrap().remove(0);
    assert_eq!(row.attempts, 1);
    assert!(row.last_error.unwrap().len() <= MAX_ERROR_LEN);
    d.queue_done(&db, cs(1)).await.unwrap();
    assert_eq!(d.queue_len(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn cloud_state_and_base_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    assert_eq!(d.cloud_state(&db).await.unwrap(), None);
    let s = CloudState {
        ws_name: "laptop".into(),
        version: 3,
        last_sync: 40,
    };
    d.set_cloud_state(&db, &s).await.unwrap();
    let s2 = CloudState { version: 4, ..s };
    d.set_cloud_state(&db, &s2).await.unwrap();
    assert_eq!(d.cloud_state(&db).await.unwrap(), Some(s2));
    d.set_base(&db, &[cs(2), cs(1), cs(2)]).await.unwrap();
    assert_eq!(d.base(&db).await.unwrap(), sorted(vec![cs(1), cs(2)]));
    d.set_base(&db, &[]).await.unwrap();
    assert!(d.base(&db).await.unwrap().is_empty());
}

#[tokio::test]
async fn oversized_batches_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (db, d) = open(&dir).await;
    let mut tx = db.begin().await.unwrap();
    let many = vec![cs(1); MAX_BATCH + 1];
    assert!(matches!(
        d.immutable(&mut tx, &many).await,
        Err(DraftsError::TooMany(_))
    ));
}
