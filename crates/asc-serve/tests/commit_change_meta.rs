//! M18 (SPEC §8.7, §20.4): a server `Commit` writes a draft `ChangeMeta` (`op = New`,
//! `preds = []`) and a `changes` row in pipeline step 4; the change resolves through
//! `ResolveChanges`; an existing row is never replaced.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;
use asc_rpc::auth::{Access, BearerToken, Credential, Principal, StaticTokens};
use asc_rpc::server::{CallCtx, MutationsHandler, RepoName, ServerInfo, wire};
use asc_rpc::services::changes_server::Changes;
use asc_rpc::{IdMode, WireId};
use asc_serve::changes::index::{self, ChangeRow};
use asc_serve::changes::{self, rpc::ChangesService};
use asc_serve::migrate;
use asc_serve::mutations::ServerMutations;
use asc_serve::read::ReadHooks;
use asc_serve::registry::{Extensions, NewRepo, Registry, RepoEntry};
use bytes::Bytes;
use tonic::Request;
use uuid::Uuid;

const TOKEN: &str = "reader-token";

struct Fx {
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
    m: ServerMutations,
    svc: ChangesService,
}

async fn fx() -> Fx {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let entry = registry
        .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
        .await
        .unwrap();
    let ext = Extensions::new();
    let hooks = Arc::new(ReadHooks::default());
    changes::register(&ext, &registry, &hooks);
    let m = ServerMutations::new(registry.clone(), ext.pipeline.clone(), hooks);
    let auth = StaticTokens::new().with_token(TOKEN, "reader", Access::Read);
    let svc = ChangesService::new(
        ext.changes.get().unwrap().clone(),
        Arc::new(auth),
        ServerInfo::default(),
    );
    Fx {
        registry,
        entry,
        m,
        svc,
    }
}

fn ctx() -> CallCtx {
    CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: IdMode::Asc,
        principal: Principal { id: "alice".into() },
        op_id: Some(Uuid::new_v4()),
        client: None,
        admin: Default::default(),
    }
}

fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "o/r".into(),
        id_mode: IdMode::Asc as i32,
    })
}

fn commit_req(parent: &str, path: &str, content: &[u8]) -> wire::CommitRequest {
    let sig = wire::Signature {
        name: Bytes::from_static(b"Al"),
        email: Bytes::from_static(b"al@x"),
        time: 1_700_000_000,
        tz_min: 0,
    };
    wire::CommitRequest {
        repo: repo_ref(),
        parent: parent.into(),
        ops: vec![wire::FileOp {
            path: Bytes::copy_from_slice(path.as_bytes()),
            op: Some(wire::file_op::Op::PutContent(Bytes::copy_from_slice(
                content,
            ))),
            mode: 0,
        }],
        message: Bytes::from_static(b"m"),
        author: Some(sig.clone()),
        committer: Some(sig),
        ..wire::CommitRequest::default()
    }
}

impl Fx {
    async fn commit(&self, parent: &str, path: &str, content: &[u8]) -> ChangesetId {
        let got = MutationsHandler::commit(&self.m, &ctx(), commit_req(parent, path, content))
            .await
            .unwrap();
        ChangesetId::from_slice(&got.id).unwrap()
    }

    async fn rows(&self, cs: &[ChangesetId]) -> Vec<ChangeRow> {
        let mut conn = self.registry.pool().acquire().await.unwrap();
        index::rows_of(&mut conn, self.entry.ns, cs).await.unwrap()
    }

    async fn change_rows(&self) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM changes WHERE ns = $1")
            .bind(self.entry.ns.as_bytes().to_vec())
            .fetch_one(self.registry.pool())
            .await
            .unwrap()
    }

    async fn resolve(&self, rev: &str) -> wire::ResolveChangesResponse {
        let mut r = Request::new(wire::ResolveChangesRequest {
            repo: repo_ref(),
            changes: vec![],
            revs: vec![rev.into()],
        });
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(TOKEN).unwrap()));
        self.svc.resolve_changes(r).await.unwrap().into_inner()
    }
}

#[tokio::test]
async fn commit_without_a_ref_writes_a_draft_meta_and_the_change_resolves() {
    let f = fx().await;
    let cs = f.commit("", "a.txt", b"one\n").await;
    let rows = f.rows(&[cs]).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].op, RewriteOp::New as i64);
    assert!(rows[0].preds.is_empty());
    // No ref, no ref log.
    let refs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refs")
        .fetch_one(f.registry.pool())
        .await
        .unwrap();
    assert_eq!(refs, 0);

    let got = f.resolve(&cs.to_string()).await;
    assert_eq!(got.changes.len(), 1);
    assert_eq!(got.changes[0].change.as_ref(), rows[0].change.as_bytes());
}

#[tokio::test]
async fn each_commit_gets_its_own_change_and_a_retry_changes_nothing() {
    let f = fx().await;
    let a = f.commit("", "a.txt", b"one\n").await;
    let b = f.commit(&a.to_string(), "b.txt", b"two\n").await;
    let ra = f.rows(&[a]).await;
    let rb = f.rows(&[b]).await;
    assert_ne!(ra[0].change, rb[0].change);
    assert_eq!(f.change_rows().await, 2);

    // The identical commit again: same changeset, the stored meta is not replaced.
    let again = f.commit(&a.to_string(), "b.txt", b"two\n").await;
    assert_eq!(again, b);
    assert_eq!(f.rows(&[b]).await, rb);
    assert_eq!(f.change_rows().await, 2);
}

#[tokio::test]
async fn an_existing_meta_is_never_overwritten() {
    use asc_core::change::{ChangeId, ChangeMeta};
    let f = fx().await;
    let a = f.commit("", "a.txt", b"one\n").await;
    // A second changeset on top of `a`, whose meta is claimed first by someone else.
    let b = f.commit(&a.to_string(), "b.txt", b"two\n").await;
    let before = f.rows(&[b]).await;
    let idx = index::ChangeIndex::new(f.registry.clone());
    let mut tx = f.registry.refs().begin().await.unwrap();
    let other = ChangeMeta::new(b, ChangeId::from_bytes([7; 16]), vec![a], RewriteOp::Amend);
    let (_, wrote) = idx.write_meta(&mut tx, f.entry.key, &other).await.unwrap();
    tx.commit().await.unwrap();
    assert!(!wrote, "the first meta wins");
    let again = f.commit(&a.to_string(), "b.txt", b"two\n").await;
    assert_eq!(again, b);
    assert_eq!(f.rows(&[b]).await, before);
}
