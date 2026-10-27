//! The secret scan of the gate must not wave an update through on a scan that stopped early: a
//! push padded with files past `[secrets] max_files` (or `max_scan_bytes`) is refused when the
//! policy blocks on push.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;
use std::sync::Arc;

use asc_core::content::{Body, ingest};
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{CallCtx, RepoName, status_rpc_error};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use tonic::Status;

use super::PolicyStore;
use super::gate::{GateMode, GateRequest, GateUpdate, RefGate, ServerGate};
use crate::graph_sql::{self, NewNode};
use crate::migrate;
use crate::registry::{NewRepo, Registry, RepoEntry};
use crate::secretscan::{Config, MAX_CHANGESETS, Scanner, added_files};
use crate::signing::{self, rpc::Signing};

/// A GitHub-shaped token the built-in rules find.
fn secret() -> String {
    const ALPHABET: &[u8] = b"q7Zk3MxR9vTb2LpWd8NcYf4HgJa6SeUo5Ei1XrB0";
    let body: String = (0..36)
        .map(|i| char::from(ALPHABET[(i * 7 + i / 3) % ALPHABET.len()]))
        .collect();
    format!("ghp_{body}")
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap()
}

struct Env {
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
}

async fn env() -> Env {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let entry = registry
        .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
        .await
        .unwrap();
    Env { registry, entry }
}

/// A root changeset that writes `files`, indexed in `cs_graph` like an ingested one.
async fn commit(env: &Env, files: &[(&str, String)]) -> ChangesetId {
    let engine = env.entry.backend.engine();
    let mut changes: Vec<(RepoPath, Change)> = Vec::new();
    for (path, text) in files {
        let got = ingest(Cursor::new(text.as_bytes())).unwrap();
        let Body::Inline(b) = got.body else {
            unreachable!("small test file")
        };
        env.entry
            .objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        changes.push((
            RepoPath::new(*path).unwrap(),
            Change::write(Kind::File, got.file, got.size, got.git, None).unwrap(),
        ));
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    let base = asc_engine::Engine::<
        asc_store::bucket::BucketObjects,
        asc_store::sqlrefs::SqlRefs,
    >::empty_tree_id();
    let root = engine.apply(base, &changes).await.unwrap();
    let cs = engine
        .create_changeset(
            NewChangeset {
                parents: Vec::new(),
                author: sig(),
                committer: sig(),
                message: b"m\n".to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap();
    let mut tx = env.registry.pool().begin().await.unwrap();
    graph_sql::insert_nodes(
        &mut tx,
        env.entry.ns,
        &[NewNode {
            id: cs,
            generation: 1,
            parents: &[],
        }],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    cs
}

fn gate(env: &Env, secrets: Config) -> ServerGate {
    ServerGate::new(
        env.registry.clone(),
        Arc::new(PolicyStore::new(env.registry.clone())),
        Arc::new(Scanner::new(env.registry.clone(), secrets)),
        Arc::new(Signing::new(
            env.registry.clone(),
            signing::Config::default(),
            None,
        )),
    )
}

async fn push_trunk(env: &Env, gate: &ServerGate, new: ChangesetId) -> Result<(), Status> {
    let ctx = CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: IdMode::Asc,
        principal: Principal {
            id: "7:alice".into(),
        },
        op_id: None,
        client: None,
        admin: Default::default(),
    };
    let updates = [GateUpdate {
        name: format!("refs/heads/{}", env.entry.default_branch),
        old: None,
        new: Some(RefTarget::Changeset(new)),
        user: false,
    }];
    gate.check(&GateRequest {
        ctx: &ctx,
        entry: &env.entry,
        updates: &updates,
        mode: GateMode::Push { force: false },
    })
    .await
}

fn code(s: &Status) -> String {
    status_rpc_error(s).map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
}

/// Three files in path order; the secret is in the last, past a `max_files` of two.
fn padded() -> Vec<(&'static str, String)> {
    vec![
        ("a.txt", "just text\n".into()),
        ("b.txt", "more text\n".into()),
        ("c.txt", format!("token = {}\n", secret())),
    ]
}

#[tokio::test]
async fn a_push_over_the_file_cap_with_a_secret_beyond_the_cutoff_is_refused() {
    let env = env().await;
    let cs = commit(&env, &padded()).await;
    let gate = gate(
        &env,
        Config {
            max_files: 2,
            ..Config::default()
        },
    );
    let err = push_trunk(&env, &gate, cs).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED", "{err:?}");
    let detail = status_rpc_error(&err).unwrap();
    assert!(
        detail.info().paths[0].contains("too large to scan for secrets; push it in parts"),
        "{:?}",
        detail.info().paths
    );
}

#[tokio::test]
async fn the_same_push_inside_the_cap_is_refused_for_the_secret_it_holds() {
    let env = env().await;
    let cs = commit(&env, &padded()).await;
    let gate = gate(&env, Config::default());
    let err = push_trunk(&env, &gate, cs).await.unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED", "{err:?}");
}

#[tokio::test]
async fn a_clean_push_inside_the_cap_passes() {
    let env = env().await;
    let cs = commit(&env, &padded()[..2]).await;
    let gate = gate(
        &env,
        Config {
            max_files: 2,
            ..Config::default()
        },
    );
    push_trunk(&env, &gate, cs).await.unwrap();
}

#[tokio::test]
async fn the_added_listing_says_when_it_left_changesets_out() {
    let env = env().await;
    let cs = commit(&env, &padded()[..1]).await;
    let engine = env.entry.backend.engine();
    let within = added_files(engine, &vec![cs; MAX_CHANGESETS])
        .await
        .unwrap();
    assert!(!within.truncated);
    let over = added_files(engine, &vec![cs; MAX_CHANGESETS + 1])
        .await
        .unwrap();
    assert!(over.truncated);
    assert_eq!(over.files.len(), within.files.len());
}
