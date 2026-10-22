//! Backend-independent `Refs` conformance suite (behind `testkit`).

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use futures::future::join_all;
use uuid::Uuid;

use crate::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs, RepoKey};

fn target(n: u32) -> RefTarget {
    RefTarget::Changeset(ChangesetId::of(&n.to_be_bytes()))
}

fn name(s: &str) -> RefName {
    RefName::new(s).unwrap()
}

fn upd(n: &str, expect: Expect, new: Option<RefTarget>) -> RefUpdate {
    RefUpdate {
        name: name(n),
        expect,
        new,
        expires_at: None,
    }
}

fn txn(updates: Vec<RefUpdate>) -> RefTxn {
    RefTxn {
        updates,
        actor: Actor {
            id: "tester".into(),
            via: "suite".into(),
        },
        reason: "test".into(),
        op_id: Uuid::now_v7(),
    }
}

async fn apply<R: Refs + ?Sized>(
    r: &R,
    repo: RepoKey,
    updates: Vec<RefUpdate>,
) -> Result<(), RefError> {
    r.update(repo, &txn(updates)).await
}

fn conflict(res: Result<(), RefError>) -> (RefName, Option<RefTarget>) {
    match res {
        Err(RefError::Conflict(c)) => (c.name, c.actual),
        other => panic!("expected conflict, got {other:?}"),
    }
}

/// Runs every case against `refs`; panics on the first failure. Uses its own repo keys.
pub async fn run_refs_suite<R: Refs + 'static>(refs: Arc<R>) {
    create_and_cas(&*refs).await;
    atomicity(&*refs).await;
    delete_and_log(&*refs).await;
    list_and_expiry(&*refs).await;
    race(refs).await;
}

async fn create_and_cas<R: Refs>(r: &R) {
    let repo = RepoKey(9001);
    let main = name("refs/heads/master");
    assert!(r.get(repo, &main).await.unwrap().is_none());

    apply(
        r,
        repo,
        vec![upd("refs/heads/master", Expect::Absent, Some(target(1)))],
    )
    .await
    .unwrap();
    let e = r.get(repo, &main).await.unwrap().unwrap();
    assert_eq!((e.target, e.version), (target(1), 1));

    // Absent on an existing ref, and a stale Value, both conflict and report the actual value.
    let res = apply(
        r,
        repo,
        vec![upd("refs/heads/master", Expect::Absent, Some(target(2)))],
    )
    .await;
    assert_eq!(conflict(res), (main.clone(), Some(target(1))));
    let stale = upd(
        "refs/heads/master",
        Expect::Value(target(7)),
        Some(target(2)),
    );
    assert_eq!(
        conflict(apply(r, repo, vec![stale]).await),
        (main.clone(), Some(target(1)))
    );

    let good = upd(
        "refs/heads/master",
        Expect::Value(target(1)),
        Some(target(2)),
    );
    apply(r, repo, vec![good]).await.unwrap();
    apply(
        r,
        repo,
        vec![upd("refs/heads/master", Expect::Any, Some(target(3)))],
    )
    .await
    .unwrap();
    let e = r.get(repo, &main).await.unwrap().unwrap();
    assert_eq!((e.target, e.version), (target(3), 3));

    // Repos are isolated.
    assert!(r.get(RepoKey(9002), &main).await.unwrap().is_none());
}

async fn atomicity<R: Refs>(r: &R) {
    let repo = RepoKey(9010);
    apply(
        r,
        repo,
        vec![upd("refs/heads/a", Expect::Absent, Some(target(1)))],
    )
    .await
    .unwrap();

    // The second update fails, so the first must not be visible.
    let res = apply(
        r,
        repo,
        vec![
            upd("refs/heads/b", Expect::Absent, Some(target(2))),
            upd("refs/heads/a", Expect::Value(target(9)), Some(target(3))),
        ],
    )
    .await;
    assert_eq!(conflict(res).0, name("refs/heads/a"));
    assert!(r.get(repo, &name("refs/heads/b")).await.unwrap().is_none());
    assert!(
        r.log(repo, &name("refs/heads/b"), 10)
            .await
            .unwrap()
            .is_empty()
    );

    apply(
        r,
        repo,
        vec![
            upd("refs/heads/b", Expect::Absent, Some(target(2))),
            upd("refs/heads/a", Expect::Value(target(1)), Some(target(3))),
        ],
    )
    .await
    .unwrap();
    assert_eq!(r.list(repo, "refs/heads/").await.unwrap().len(), 2);
}

async fn delete_and_log<R: Refs>(r: &R) {
    let repo = RepoKey(9020);
    let n = name("refs/heads/x");
    apply(
        r,
        repo,
        vec![upd("refs/heads/x", Expect::Absent, Some(target(1)))],
    )
    .await
    .unwrap();
    let step = upd("refs/heads/x", Expect::Value(target(1)), Some(target(2)));
    apply(r, repo, vec![step]).await.unwrap();
    let res = apply(
        r,
        repo,
        vec![upd("refs/heads/x", Expect::Value(target(1)), None)],
    )
    .await;
    assert_eq!(conflict(res).1, Some(target(2)));
    apply(
        r,
        repo,
        vec![upd("refs/heads/x", Expect::Value(target(2)), None)],
    )
    .await
    .unwrap();
    assert!(r.get(repo, &n).await.unwrap().is_none());

    // Newest first: delete, update, create.
    let log = r.log(repo, &n, 10).await.unwrap();
    let moves: Vec<_> = log.iter().map(|l| (l.old, l.new)).collect();
    assert_eq!(
        moves,
        vec![
            (Some(target(2)), None),
            (Some(target(1)), Some(target(2))),
            (None, Some(target(1)))
        ]
    );
    assert!(log.windows(2).all(|w| w[0].seq > w[1].seq));
    let who = (
        log[0].actor.id.as_str(),
        log[0].actor.via.as_str(),
        log[0].reason.as_str(),
    );
    assert_eq!(who, ("tester", "suite", "test"));
    assert_eq!(r.log(repo, &n, 2).await.unwrap().len(), 2);
}

async fn list_and_expiry<R: Refs>(r: &R) {
    let repo = RepoKey(9030);
    let mut agent = upd("refs/heads/agent/one", Expect::Absent, Some(target(1)));
    agent.expires_at = Some(1_900_000_000);
    apply(
        r,
        repo,
        vec![
            agent,
            upd("refs/heads/agent_x", Expect::Absent, Some(target(2))),
            upd("refs/heads/master", Expect::Absent, Some(target(3))),
        ],
    )
    .await
    .unwrap();

    let got = r.list(repo, "refs/heads/agent/").await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].expires_at, Some(1_900_000_000));
    let one = r
        .get(repo, &name("refs/heads/agent/one"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(one.expires_at, Some(1_900_000_000));
    // `_` and `%` in the prefix are literals, not wildcards.
    assert_eq!(r.list(repo, "refs/heads/agent_").await.unwrap().len(), 1);
    assert!(r.list(repo, "refs/heads/%").await.unwrap().is_empty());
    assert_eq!(r.list(repo, "").await.unwrap().len(), 3);
    assert_eq!(
        r.get(repo, &name("refs/heads/master"))
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        None
    );
}

/// 100 tasks race to advance one ref by CAS, retrying on conflict: each version has one winner.
async fn race<R: Refs + 'static>(r: Arc<R>) {
    const N: u32 = 100;
    let repo = RepoKey(9040);
    let n = name("refs/heads/counter");
    apply(
        &*r,
        repo,
        vec![upd("refs/heads/counter", Expect::Absent, Some(target(0)))],
    )
    .await
    .unwrap();

    let tasks = (1..=N).map(|i| {
        let r = Arc::clone(&r);
        let n = n.clone();
        async move {
            loop {
                let cur = r.get(repo, &n).await.unwrap().unwrap().target;
                let up = RefUpdate {
                    name: n.clone(),
                    expect: Expect::Value(cur),
                    new: Some(target(i)),
                    expires_at: None,
                };
                match r.update(repo, &txn(vec![up])).await {
                    Ok(()) => return,
                    Err(RefError::Conflict(_)) => {}
                    Err(e) => panic!("{e}"),
                }
            }
        }
    });
    join_all(tasks).await;

    assert_eq!(
        r.get(repo, &n).await.unwrap().unwrap().version,
        u64::from(N) + 1
    );
    let mut log = r.log(repo, &n, N + 10).await.unwrap();
    log.reverse();
    assert_eq!(log.len(), N as usize + 1);
    // The log is a single chain: every entry starts where the previous one ended.
    for w in log.windows(2) {
        assert_eq!(w[1].old, w[0].new);
    }
}
