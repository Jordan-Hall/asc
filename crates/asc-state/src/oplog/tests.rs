use super::redact::{MAX_ARG_LEN, MAX_ARGS};
use super::*;
use crate::home::{AscHome, RepoId};
use crate::state::repos::{NewRepo, RemoteKind};

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn redacts_url_credentials() {
    let r = redact_argv(&args(&[
        "clone",
        "https://user:pw@host.test/r.git",
        "ssh://git@h/x",
    ]));
    assert_eq!(r, "clone https://***@host.test/r.git ssh://***@h/x");
    assert!(!r.contains("pw"));
    // An `@` in the path is not a credential.
    assert_eq!(redact_argv(&args(&["https://h/a@b"])), "https://h/a@b");
}

#[test]
fn redacts_config_values() {
    assert_eq!(
        redact_argv(&args(&["-c", "http.extraHeader=Bearer abc", "fetch"])),
        "-c http.extraHeader=*** fetch"
    );
    assert_eq!(
        redact_argv(&args(&["-cuser.token=s3", "x"])),
        "-cuser.token=*** x"
    );
    // `-c` without key=value (for example `git commit -c <rev>`) is kept.
    assert_eq!(
        redact_argv(&args(&["commit", "-c", "HEAD1"])),
        "commit -c HEAD1"
    );
}

#[test]
fn drops_secret_option_values() {
    assert_eq!(
        redact_argv(&args(&[
            "login",
            "--token",
            "sekret",
            "--password=p",
            "--auth-x",
            "q",
            "-v"
        ])),
        "login --token --password --auth-x -v"
    );
}

#[test]
fn bounds_argument_count_and_length() {
    let many: Vec<String> = (0..10_000).map(|i| format!("a{i}")).collect();
    assert_eq!(redact_argv(&many).split(' ').count(), MAX_ARGS);
    let long = redact_argv(&[format!("é{}", "x".repeat(5000))]);
    assert!(long.len() <= MAX_ARG_LEN + 2);
}

async fn setup(dir: &tempfile::TempDir) -> (StateDb, RepoKey, WorkspaceId) {
    let home = AscHome::open(dir.path().join("home")).unwrap();
    let db = StateDb::open(&home).await.unwrap();
    let repo = crate::state::repos::insert(
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
    (db, repo, WorkspaceId::new())
}

fn op(ws: WorkspaceId, n: u8) -> NewOp {
    NewOp {
        op_id: OpId::new(),
        ws,
        argv: args(&["commit", "-m", "x"]),
        kind: "commit".into(),
        head_before: Some(ChangesetId::from_bytes([n; 32])),
        head_after: None,
        wc_snapshot: Some(ChangesetId::from_bytes([n; 32])),
        undoes: None,
    }
}

#[tokio::test]
async fn round_trips_and_lists_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo, ws) = setup(&dir).await;
    let a = record(&db, repo, &op(ws, 1), Limits::default())
        .await
        .unwrap();
    let mut second = op(ws, 2);
    second.undoes = Some(a);
    let b = record(&db, repo, &second, Limits::default()).await.unwrap();
    assert!(b > a);
    let got = get(&db, repo, b).await.unwrap().unwrap();
    assert_eq!(got.undoes, Some(a));
    assert_eq!(got.argv, "commit -m x");
    assert_eq!(got.head_after, None);
    assert_eq!(
        by_op_id(&db, repo, second.op_id)
            .await
            .unwrap()
            .unwrap()
            .seq,
        b
    );
    let all = list(&db, repo, 10).await.unwrap();
    assert_eq!(all.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![b, a]);
    assert_eq!(live_snapshots(&db, repo).await.unwrap().len(), 2);
}

#[tokio::test]
async fn prunes_by_count() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo, ws) = setup(&dir).await;
    let limits = Limits {
        max_days: 90,
        max_ops: 3,
    };
    for n in 0..6 {
        record(&db, repo, &op(ws, n), limits).await.unwrap();
    }
    assert_eq!(list(&db, repo, 100).await.unwrap().len(), 3);
    let snaps = live_snapshots(&db, repo).await.unwrap();
    assert_eq!(snaps.len(), 3);
    assert!(!snaps.contains(&ChangesetId::from_bytes([0; 32])));
    // maxOps = 0 still keeps the newest op.
    let zero = Limits {
        max_days: 90,
        max_ops: 0,
    };
    assert_eq!(prune(&db, repo, zero).await.unwrap(), 2);
    assert_eq!(list(&db, repo, 100).await.unwrap().len(), 1);
}

#[tokio::test]
async fn prunes_by_age_per_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo, ws) = setup(&dir).await;
    for n in 0..3 {
        record(&db, repo, &op(ws, n), Limits::default())
            .await
            .unwrap();
    }
    let old = now() - 91 * 86_400;
    sqlx::query("UPDATE op_log SET at = ? WHERE seq <= 2")
        .bind(old)
        .execute(db.pool())
        .await
        .unwrap();
    // A different repo's ops are untouched.
    let other = RepoKey(repo.0 + 1);
    assert_eq!(prune(&db, other, Limits::default()).await.unwrap(), 0);
    assert_eq!(prune(&db, repo, Limits::default()).await.unwrap(), 2);
    assert_eq!(list(&db, repo, 100).await.unwrap().len(), 1);
}

#[tokio::test]
async fn stores_redacted_argv_only() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo, ws) = setup(&dir).await;
    let mut o = op(ws, 1);
    o.argv = args(&["push", "https://u:topsecret@h/r", "--token", "tok123"]);
    let seq = record(&db, repo, &o, Limits::default()).await.unwrap();
    let raw: String = sqlx::query_scalar("SELECT argv FROM op_log WHERE seq = ?")
        .bind(seq.0)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(raw, "push https://***@h/r --token");
}
