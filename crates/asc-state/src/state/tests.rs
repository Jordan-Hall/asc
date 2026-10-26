use std::collections::BTreeMap;
use std::process::Command;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::Kind;
use asc_core::path::RefName;
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs, RepoKey};
use asc_store::sqlrefs::SqlRefs;
use sqlx::Row;
use sqlx::any::AnyPoolOptions;
use uuid::Uuid;

use super::repos::{NewRepo, RemoteKind};
use super::stage::{Leaf, LeafDelta, SetId, StageRow, StageSet};
use super::workspaces::{Mode, NewWorkspace};
use super::*;
use crate::home::{RepoId, WorkspaceId};

async fn open(dir: &tempfile::TempDir) -> StateDb {
    let home = AscHome::open(dir.path().join("home")).unwrap();
    StateDb::open(&home).await.unwrap()
}

async fn repo(db: &StateDb, name: &str) -> RepoKey {
    repos::insert(
        db,
        &NewRepo {
            id: RepoId::new(),
            name: name.into(),
            ns: vec![1, 2, 3],
            remote_url: Some("https://example.test/r".into()),
            remote_kind: RemoteKind::Server,
            default_branch: "master".into(),
            windows_safe: true,
        },
    )
    .await
    .unwrap()
}

async fn workspace(db: &StateDb, repo: RepoKey, path: &str, head: &str) -> WorkspaceId {
    let id = WorkspaceId::new();
    workspaces::insert(
        db,
        &NewWorkspace {
            id,
            repo,
            path: path.into(),
            mount: path.into(),
            mode: Mode::Plain,
            head: head.into(),
            base_cs: None,
            sparse: None,
            ephemeral_until: None,
        },
    )
    .await
    .unwrap();
    id
}

fn leaf(n: u8) -> Leaf {
    Leaf {
        kind: Kind::File,
        file: FileId::of(&[n]),
        size: u64::from(n),
        git: GitOid::from_bytes([n; 20]),
    }
}

fn row(path: &str, leaf: Option<&Leaf>) -> StageRow {
    StageRow::from_leaf(path.as_bytes().to_vec(), leaf)
}

#[tokio::test]
async fn open_twice_is_idempotent_and_sets_pragmas() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let id = db.machine_id().await.unwrap();
    assert_eq!(db.schema_version().await.unwrap(), SCHEMA_VERSION);
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    let timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(timeout, 5000);
    drop(db);

    let db = open(&dir).await;
    assert_eq!(db.machine_id().await.unwrap(), id);
    let migrations: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(migrations >= 1);
}

async fn table_info(pool: &sqlx::AnyPool, table: &str) -> Vec<(String, String, i64, i64)> {
    sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("name"),
                r.get::<String, _>("type"),
                r.get::<i64, _>("notnull"),
                r.get::<i64, _>("pk"),
            )
        })
        .collect()
}

#[tokio::test]
async fn refs_tables_equal_the_store_migration() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;

    let path = dir
        .path()
        .join("server.db")
        .to_string_lossy()
        .replace('\\', "/");
    let server = AnyPoolOptions::new()
        .connect(&format!("sqlite:{path}?mode=rwc"))
        .await
        .unwrap();
    SqlRefs::new(server.clone()).migrate().await.unwrap();

    for table in ["refs", "ref_log"] {
        let ours = table_info(db.pool(), table).await;
        assert!(!ours.is_empty());
        assert_eq!(ours, table_info(&server, table).await, "{table}");
    }
}

#[tokio::test]
async fn older_binary_refuses_a_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    sqlx::query("UPDATE meta SET schema_version = 99, asc_version = '9.9.9'")
        .execute(db.pool())
        .await
        .unwrap();
    drop(db);
    let home = AscHome::open(dir.path().join("home")).unwrap();
    let err = StateDb::open(&home).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "state.db is from asc 9.9.9; run asc upgrade"
    );
}

#[tokio::test]
async fn local_refs_share_the_pool() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let name = RefName::new("refs/heads/master").unwrap();
    let target = RefTarget::Changeset(ChangesetId::of(b"c"));
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: name.clone(),
            expect: Expect::Absent,
            new: Some(target),
            expires_at: None,
        }],
        actor: Actor {
            id: "me".into(),
            via: "cli".into(),
        },
        reason: "test".into(),
        op_id: Uuid::now_v7(),
    };
    db.refs().update(r, &txn).await.unwrap();
    assert_eq!(
        db.refs().get(r, &name).await.unwrap().unwrap().target,
        target
    );
}

/// Child half of [`concurrent_processes_do_not_hit_busy`]: runs only when the parent test
/// re-executes this binary with `ASC_STATE_TEST_CHILD` set.
#[tokio::test]
async fn child_writer() {
    let Some(home) = std::env::var_os("ASC_STATE_TEST_CHILD") else {
        return;
    };
    let home = AscHome::open(home.into()).unwrap();
    let db = StateDb::open(&home).await.unwrap();
    let ws: WorkspaceId = std::env::var("ASC_STATE_TEST_WS").unwrap().parse().unwrap();
    for i in 0..50u8 {
        journal::append(
            &db,
            ws,
            &[journal::Entry {
                path: vec![i],
                op: journal::Op::Modified,
            }],
        )
        .await
        .unwrap();
        stage::upsert(
            &db,
            ws,
            StageSet::Default,
            &[row(&format!("p{i}"), Some(&leaf(i)))],
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn concurrent_processes_do_not_hit_busy() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "w", "refs/heads/master").await;
    let home = dir.path().join("home");
    let exe = std::env::current_exe().unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (exe, home) = (exe.clone(), home.clone());
            std::thread::spawn(move || {
                Command::new(exe)
                    .args(["--exact", "state::tests::child_writer", "--nocapture"])
                    .env("ASC_STATE_TEST_CHILD", home)
                    .env("ASC_STATE_TEST_WS", ws.to_string())
                    .output()
                    .unwrap()
            })
        })
        .collect();
    for h in handles {
        let out = h.join().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let n = journal::since(&db, ws, 0, 1000).await.unwrap().len();
    assert_eq!(n, 200);
}

#[tokio::test]
async fn repos_and_bucket_refs_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let a = repo(&db, "alpha").await;
    let got = repos::get(&db, a).await.unwrap().unwrap();
    assert_eq!(got.repo.name, "alpha");
    assert!(got.repo.windows_safe);
    assert_eq!(repos::by_name(&db, "alpha").await.unwrap().len(), 1);

    let b1 = repos::bucket_refs_row(&db, "s3://bucket/x").await.unwrap();
    let b2 = repos::bucket_refs_row(&db, "s3://bucket/x").await.unwrap();
    let b3 = repos::bucket_refs_row(&db, "s3://bucket/y").await.unwrap();
    assert_eq!(b1, b2);
    assert_ne!(b1, b3);
    assert_ne!(b1, a);
    let row = repos::get(&db, b1).await.unwrap().unwrap();
    assert_eq!(row.repo.remote_kind, RemoteKind::BucketLocalRefs);
    // The refs row is not a repository to find by name.
    assert!(
        repos::by_name(&db, "s3://bucket/x")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn workspaces_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let w1 = workspace(&db, r, "/a", "refs/heads/feature").await;
    let w2 = workspace(&db, r, "/b", "").await;

    assert_eq!(
        workspaces::get(&db, w1).await.unwrap().unwrap().ws.path,
        "/a"
    );
    assert_eq!(
        workspaces::by_path(&db, "/b").await.unwrap().unwrap().ws.id,
        w2
    );
    assert_eq!(
        workspaces::by_mount(&db, "/a")
            .await
            .unwrap()
            .unwrap()
            .ws
            .id,
        w1
    );
    let holder = workspaces::holding_branch(&db, r, "feature").await.unwrap();
    assert_eq!(holder.unwrap().ws.id, w1);
    assert!(
        workspaces::holding_branch(&db, r, "other")
            .await
            .unwrap()
            .is_none()
    );

    let base = ChangesetId::of(b"base");
    workspaces::update_head_base(&db, w2, "refs/heads/x", Some(base))
        .await
        .unwrap();
    let w = workspaces::get(&db, w2).await.unwrap().unwrap();
    assert_eq!(
        (w.ws.head.as_str(), w.ws.base_cs),
        ("refs/heads/x", Some(base))
    );
    assert_eq!(workspaces::bump_epoch(&db, w2).await.unwrap(), 1);
    assert_eq!(workspaces::bump_epoch(&db, w2).await.unwrap(), 2);
    assert_eq!(workspaces::list(&db, Some(r)).await.unwrap().len(), 2);

    stage::upsert(&db, w1, StageSet::Default, &[row("f", Some(&leaf(1)))])
        .await
        .unwrap();
    workspaces::remove(&db, w1).await.unwrap();
    assert!(workspaces::get(&db, w1).await.unwrap().is_none());
    assert!(
        stage::overlay(&db, w1, StageSet::Default)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn temporary_set_never_touches_set_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "/a", "refs/heads/master").await;
    stage::upsert(
        &db,
        ws,
        StageSet::Default,
        &[row("a", Some(&leaf(1))), row("b", None)],
    )
    .await
    .unwrap();
    let before = stage::overlay(&db, ws, StageSet::Default).await.unwrap();

    let set = stage_sets::register(&db, ws, "/tmp/idx").await.unwrap();
    let temp = StageSet::Temp(set);
    stage::upsert(
        &db,
        ws,
        temp,
        &[row("a", Some(&leaf(9))), row("c", Some(&leaf(3)))],
    )
    .await
    .unwrap();
    stage::remove(&db, ws, temp, b"a").await.unwrap();
    assert_eq!(stage::overlay(&db, ws, temp).await.unwrap().len(), 1);
    stage::clear(&db, ws, temp).await.unwrap();
    assert_eq!(
        stage::overlay(&db, ws, StageSet::Default).await.unwrap(),
        before
    );

    let found = stage_sets::lookup_by_path(&db, "/tmp/idx")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((found.ws, found.set), (ws, set));
    let imported = stage_sets::import(&db, ws, "/tmp/other", &[row("z", Some(&leaf(4)))])
        .await
        .unwrap();
    assert_ne!(imported, set);
    assert_eq!(stage_sets::list(&db, ws).await.unwrap().len(), 2);
    stage_sets::drop_set(&db, ws, imported).await.unwrap();
    assert!(
        stage_sets::lookup_by_path(&db, "/tmp/other")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        stage::overlay(&db, ws, StageSet::Temp(imported))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        stage::overlay(&db, ws, StageSet::Default).await.unwrap(),
        before
    );
    assert!(SetId::new(0).is_none());
}

#[tokio::test]
async fn replace_is_atomic_and_rejects_bad_stage() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "/a", "refs/heads/master").await;
    stage::upsert(&db, ws, StageSet::Default, &[row("a", Some(&leaf(1)))])
        .await
        .unwrap();
    let before = stage::overlay(&db, ws, StageSet::Default).await.unwrap();

    // The second row fails after the delete ran: the old rows must survive.
    let mut bad = row("z", Some(&leaf(3)));
    bad.stage = 9;
    let err = stage::replace(&db, ws, StageSet::Default, &[row("b", Some(&leaf(2))), bad]).await;
    assert!(matches!(err, Err(Error::Corrupt(_))));
    assert_eq!(
        stage::overlay(&db, ws, StageSet::Default).await.unwrap(),
        before
    );

    let new = vec![row("b", Some(&leaf(2))), row("c", None)];
    stage::replace(&db, ws, StageSet::Default, &new)
        .await
        .unwrap();
    assert_eq!(
        stage::overlay(&db, ws, StageSet::Default).await.unwrap(),
        new
    );
    stage::replace(&db, ws, StageSet::Default, &[])
        .await
        .unwrap();
    assert!(
        stage::overlay(&db, ws, StageSet::Default)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn snapshot_and_restore_set_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "/a", "refs/heads/master").await;
    stage::upsert(&db, ws, StageSet::Default, &[row("a", Some(&leaf(1)))])
        .await
        .unwrap();
    let before = stage::overlay(&db, ws, StageSet::Default).await.unwrap();

    let snap = stage::snapshot(&db, ws).await.unwrap();
    stage::clear(&db, ws, StageSet::Default).await.unwrap();
    stage::upsert(&db, ws, StageSet::Default, &[row("x", None)])
        .await
        .unwrap();
    stage::restore(&db, ws, snap).await.unwrap();
    assert_eq!(
        stage::overlay(&db, ws, StageSet::Default).await.unwrap(),
        before
    );
    stage_sets::drop_set(&db, ws, snap).await.unwrap();
    let gone = stage::restore(&db, ws, snap).await;
    assert!(matches!(gone, Err(Error::NotFound(_))));
}

/// The effective index: the tree overlaid with the rows.
fn effective(tree: &BTreeMap<&str, Leaf>, rows: &[StageRow]) -> BTreeMap<String, Leaf> {
    let mut out: BTreeMap<String, Leaf> =
        tree.iter().map(|(k, v)| ((*k).into(), v.clone())).collect();
    for r in rows {
        let p = String::from_utf8(r.path.clone()).unwrap();
        match (r.file, r.git) {
            (Some(file), Some(git)) => {
                out.insert(
                    p,
                    Leaf {
                        kind: r.kind,
                        file,
                        size: r.size,
                        git,
                    },
                );
            }
            _ => {
                out.remove(&p);
            }
        }
    }
    out
}

fn deltas(from: &BTreeMap<&str, Leaf>, to: &BTreeMap<&str, Leaf>) -> Vec<LeafDelta> {
    let mut paths: Vec<&str> = from.keys().chain(to.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .filter(|p| from.get(p) != to.get(p))
        .map(|p| LeafDelta {
            path: p.as_bytes().to_vec(),
            from: from.get(p).cloned(),
            to: to.get(p).cloned(),
        })
        .collect()
}

#[tokio::test]
async fn rebase_keeps_the_effective_index_across_a_two_commit_move() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "/a", "refs/heads/master").await;

    let a: BTreeMap<&str, Leaf> = [
        ("keep", leaf(1)),
        ("mod", leaf(2)),
        ("del", leaf(3)),
        ("same", leaf(4)),
    ]
    .into_iter()
    .collect();
    // The move changes `mod` (to what is staged), deletes `del` (as staged), edits `keep`, adds `new`.
    let b: BTreeMap<&str, Leaf> = [
        ("keep", leaf(10)),
        ("mod", leaf(20)),
        ("same", leaf(4)),
        ("new", leaf(30)),
    ]
    .into_iter()
    .collect();
    stage::upsert(
        &db,
        ws,
        StageSet::Default,
        &[
            row("mod", Some(&leaf(20))),
            row("del", None),
            row("added", Some(&leaf(40))),
            row("new", Some(&leaf(30))),
        ],
    )
    .await
    .unwrap();
    let old_rows = stage::overlay(&db, ws, StageSet::Default).await.unwrap();
    let old = effective(&a, &old_rows);

    stage::rebase(&db, ws, &deltas(&a, &b)).await.unwrap();
    let rows = stage::overlay(&db, ws, StageSet::Default).await.unwrap();
    assert_eq!(effective(&b, &rows), old);

    let paths: Vec<_> = rows
        .iter()
        .map(|r| String::from_utf8(r.path.clone()).unwrap())
        .collect();
    // `mod`, `del` and `new` now equal the target tree and lose their rows; `keep` gains one
    // holding the old content; `added` is untouched.
    assert_eq!(paths, ["added", "keep"]);

    // A second hop back restores the original rows.
    stage::rebase(&db, ws, &deltas(&b, &a)).await.unwrap();
    let rows = stage::overlay(&db, ws, StageSet::Default).await.unwrap();
    assert_eq!(effective(&a, &rows), old);
}

#[tokio::test]
async fn git_map_cs_gen_and_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let sink = git_map::RepoGitMap {
        db: db.clone(),
        repo: r,
    };
    let rows = [
        GitMapRow {
            git: GitOid::from_bytes([1; 20]),
            kind: ObjectKind::Blob,
            asc: [7; 32],
        },
        GitMapRow {
            git: GitOid::from_bytes([1; 20]),
            kind: ObjectKind::Tree,
            asc: [8; 32],
        },
    ];
    sink.put(&rows).await.unwrap();
    sink.put(&rows).await.unwrap();
    let g = GitOid::from_bytes([1; 20]);
    assert_eq!(
        git_map::git_to_asc(&db, r, g, ObjectKind::Blob)
            .await
            .unwrap(),
        Some([7; 32])
    );
    assert_eq!(
        git_map::git_to_asc(&db, r, g, ObjectKind::Commit)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        git_map::asc_to_git(&db, r, &[8; 32]).await.unwrap(),
        [(g, ObjectKind::Tree)]
    );

    let (c1, c2) = (ChangesetId::of(b"1"), ChangesetId::of(b"2"));
    let row = cs_gen::CsGen {
        cs: c2,
        gen_no: 2,
        parents: vec![c1],
    };
    cs_gen::insert_many(&db, r, std::slice::from_ref(&row))
        .await
        .unwrap();
    assert_eq!(cs_gen::get(&db, r, c2).await.unwrap(), Some(row));
    assert_eq!(cs_gen::get(&db, r, c1).await.unwrap(), None);

    assert!(!history_boundary::contains(&db, r, c1).await.unwrap());
    history_boundary::insert(&db, r, c1).await.unwrap();
    assert!(history_boundary::contains(&db, r, c1).await.unwrap());
    history_boundary::remove(&db, r, c1).await.unwrap();
    assert!(!history_boundary::contains(&db, r, c1).await.unwrap());
}

#[tokio::test]
async fn workspace_bookkeeping_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "/a", "refs/heads/master").await;

    let e = |p: &str, op| journal::Entry {
        path: p.as_bytes().to_vec(),
        op,
    };
    journal::append(
        &db,
        ws,
        &[e("a", journal::Op::Created), e("b", journal::Op::Deleted)],
    )
    .await
    .unwrap();
    let last = journal::append(&db, ws, &[e("c", journal::Op::DirRenamedTo)])
        .await
        .unwrap();
    assert_eq!(last, 3);
    let tail = journal::since(&db, ws, 1, 10).await.unwrap();
    assert_eq!(tail.iter().map(|r| r.seq).collect::<Vec<_>>(), [2, 3]);
    journal::clear(&db, ws).await.unwrap();
    assert!(journal::since(&db, ws, 0, 10).await.unwrap().is_empty());

    entry_times::set(&db, ws, b"p", 5).await.unwrap();
    entry_times::set(&db, ws, b"p", 6).await.unwrap();
    assert_eq!(entry_times::get(&db, ws, b"p").await.unwrap(), Some(6));
    entry_times::remove(&db, ws, b"p").await.unwrap();
    assert_eq!(entry_times::get(&db, ws, b"p").await.unwrap(), None);

    let intent = switch_intent::SwitchIntent {
        from_root: TreeId::of(b"f"),
        to_root: TreeId::of(b"t"),
        to_head: "refs/heads/x".into(),
        pending: vec![b"a/b".to_vec(), Vec::new(), b"c".to_vec()],
        started: 0,
    };
    switch_intent::set(&db, ws, &intent).await.unwrap();
    let got = switch_intent::get(&db, ws).await.unwrap().unwrap();
    assert_eq!(
        (got.from_root, got.to_root, got.pending),
        (intent.from_root, intent.to_root, intent.pending)
    );
    switch_intent::clear(&db, ws).await.unwrap();
    assert!(switch_intent::get(&db, ws).await.unwrap().is_none());

    let renames = vec![
        (b"a".to_vec(), b"b".to_vec()),
        (b"c".to_vec(), b"d".to_vec()),
    ];
    ws_ops::set_pending_renames(&db, ws, &renames)
        .await
        .unwrap();
    assert_eq!(ws_ops::pending_renames(&db, ws).await.unwrap(), renames);
    ws_ops::clear(&db, ws).await.unwrap();
    assert!(ws_ops::pending_renames(&db, ws).await.unwrap().is_empty());

    inodes::assign(&db, ws, 7, b"x/y").await.unwrap();
    assert_eq!(
        inodes::path_of(&db, ws, 7).await.unwrap(),
        Some(b"x/y".to_vec())
    );
    assert_eq!(inodes::ino_of(&db, ws, b"x/y").await.unwrap(), Some(7));
    assert!(inodes::assign(&db, ws, 8, b"x/y").await.is_err());
    inodes::remove(&db, ws, 7).await.unwrap();
    assert_eq!(inodes::path_of(&db, ws, 7).await.unwrap(), None);
}

#[tokio::test]
async fn entry_sizes_above_i64_max_round_trip_and_rebase_completes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).await;
    let r = repo(&db, "r").await;
    let ws = workspace(&db, r, "/a", "refs/heads/master").await;
    stage::upsert(&db, ws, StageSet::Default, &[row("ok", Some(&leaf(1)))])
        .await
        .unwrap();
    // A hostile HEAD tree holds sizes the signed column cannot hold as a number.
    let mut big = leaf(2);
    big.size = 1 << 63;
    let mut max = leaf(3);
    max.size = u64::MAX;
    let deltas = [
        LeafDelta {
            path: b"big".to_vec(),
            from: Some(big.clone()),
            to: None,
        },
        LeafDelta {
            path: b"max".to_vec(),
            from: Some(max.clone()),
            to: None,
        },
    ];
    // The HEAD move changes both paths and has no staged row for them: it must not wedge.
    stage::rebase(&db, ws, &deltas).await.unwrap();
    let rows = stage::overlay(&db, ws, StageSet::Default).await.unwrap();
    assert_eq!(rows.len(), 3);
    for (path, leaf) in [(&b"big"[..], &big), (&b"max"[..], &max)] {
        let got = rows.iter().find(|r| r.path == path).unwrap();
        assert_eq!(got.size, leaf.size);
        assert_eq!(got.file, Some(leaf.file));
    }
    // A later move back onto the same leaf drops the row again: equality is exact.
    let back = [LeafDelta {
        path: b"big".to_vec(),
        from: None,
        to: Some(big),
    }];
    stage::rebase(&db, ws, &back).await.unwrap();
    let rows = stage::overlay(&db, ws, StageSet::Default).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.path != b"big"));
}
