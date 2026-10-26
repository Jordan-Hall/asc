//! B26: closing a `legacy_shared` PR converts its head branch to the author's user ref, once.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::server::RepoName;
use asc_serve::admin::branchless::ForgeFacts;
use asc_serve::admin::legacy_shared::{Outcome, convert_closed};
use asc_serve::migrate;
use asc_serve::registry::{NewRepo, Registry, RepoEntry};
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use async_trait::async_trait;
use sqlx::Row;
use tonic::Status;
use uuid::Uuid;

struct Open(Vec<&'static str>);

#[async_trait]
impl ForgeFacts for Open {
    async fn open_pr_heads(&self) -> Result<HashSet<String>, Status> {
        Ok(self.0.iter().map(|s| (*s).to_owned()).collect())
    }
    async fn pusher(&self, _: &str) -> Result<Option<i64>, Status> {
        Ok(None)
    }
}

struct Fx {
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
}

fn repo() -> RepoName {
    RepoName::parse("o/r").unwrap()
}

fn tip(n: u8) -> RefTarget {
    RefTarget::Changeset(ChangesetId::of(&[n]))
}

async fn fx(legacy: Option<&str>) -> Fx {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let entry = registry.create(&repo(), &NewRepo::default()).await.unwrap();
    sqlx::query("UPDATE repos SET branchless = TRUE, legacy_shared = $1 WHERE id = $2")
        .bind(legacy.map(str::to_owned))
        .bind(entry.key.0)
        .execute(registry.pool())
        .await
        .unwrap();
    Fx { registry, entry }
}

impl Fx {
    async fn set(&self, name: &str, t: RefTarget) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes()).unwrap(),
                expect: Expect::Any,
                new: Some(t),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "t".into(),
            op_id: Uuid::new_v4(),
        };
        self.registry
            .refs()
            .update(self.entry.key, &txn)
            .await
            .unwrap();
    }

    async fn get(&self, name: &str) -> Option<RefTarget> {
        self.registry
            .refs()
            .get(self.entry.key, &RefName::new(name.as_bytes()).unwrap())
            .await
            .unwrap()
            .map(|e| e.target)
    }

    async fn legacy(&self) -> Option<String> {
        sqlx::query("SELECT legacy_shared FROM repos WHERE id = $1")
            .bind(self.entry.key.0)
            .fetch_one(self.registry.pool())
            .await
            .unwrap()
            .get("legacy_shared")
    }

    async fn outbox(&self) -> usize {
        sqlx::query("SELECT id FROM outbox")
            .fetch_all(self.registry.pool())
            .await
            .unwrap()
            .len()
    }

    async fn close(&self, branch: &str, author: i64, open: Vec<&'static str>) -> Outcome {
        convert_closed(
            &self.registry,
            &repo(),
            branch,
            author,
            &Open(open),
            1_800_000_000,
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn closing_converts_the_branch_to_the_authors_user_ref_once() {
    let f = fx(Some(r#"["feat/pr","feat/other"]"#)).await;
    f.set("refs/heads/feat/pr", tip(1)).await;
    f.set("refs/heads/feat/other", tip(2)).await;
    assert_eq!(f.close("feat/pr", 42, vec![]).await, Outcome::Converted);
    assert_eq!(f.get("refs/heads/feat/pr").await, None);
    assert_eq!(f.get("refs/users/42/heads/feat/pr").await, Some(tip(1)));
    assert_eq!(f.legacy().await.as_deref(), Some(r#"["feat/other"]"#));
    assert_eq!(f.outbox().await, 1);
    // The second close event changes nothing.
    assert_eq!(f.close("feat/pr", 42, vec![]).await, Outcome::NotListed);
    assert_eq!(f.outbox().await, 1);
    assert_eq!(f.get("refs/heads/feat/other").await, Some(tip(2)));
    // The last listed name empties the column.
    assert_eq!(f.close("feat/other", 7, vec![]).await, Outcome::Converted);
    assert_eq!(f.legacy().await, None);
}

#[tokio::test]
async fn an_unlisted_or_shared_branch_is_never_converted() {
    let f = fx(Some(r#"["feat/pr"]"#)).await;
    f.set("refs/heads/master", tip(1)).await;
    f.set("refs/heads/feat/free", tip(2)).await;
    let err = convert_closed(&f.registry, &repo(), "master", 42, &Open(vec![]), 0)
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(f.close("feat/free", 42, vec![]).await, Outcome::NotListed);
    assert_eq!(f.get("refs/heads/master").await, Some(tip(1)));
    assert_eq!(f.get("refs/heads/feat/free").await, Some(tip(2)));
    assert_eq!(f.get("refs/users/42/heads/feat/free").await, None);
    assert!(
        convert_closed(&f.registry, &repo(), "feat/pr", 0, &Open(vec![]), 0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn another_open_pr_or_a_taken_user_ref_keeps_the_branch_shared() {
    let f = fx(Some(r#"["feat/pr"]"#)).await;
    f.set("refs/heads/feat/pr", tip(1)).await;
    assert_eq!(
        f.close("feat/pr", 42, vec!["feat/pr"]).await,
        Outcome::StillOpen
    );
    f.set("refs/users/42/heads/feat/pr", tip(9)).await;
    assert_eq!(f.close("feat/pr", 42, vec![]).await, Outcome::Kept);
    assert_eq!(f.get("refs/heads/feat/pr").await, Some(tip(1)));
    assert_eq!(f.get("refs/users/42/heads/feat/pr").await, Some(tip(9)));
    assert_eq!(f.legacy().await.as_deref(), Some(r#"["feat/pr"]"#));
    assert_eq!(f.outbox().await, 0);
}
