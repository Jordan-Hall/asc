use super::*;
use crate::home::{AscHome, RepoId};
use crate::state::repos::{self, NewRepo, RemoteKind};

async fn open(dir: &tempfile::TempDir) -> (StateDb, RepoKey) {
    let home = AscHome::open(dir.path().join("home")).unwrap();
    let db = StateDb::open(&home).await.unwrap();
    let repo = add_repo(&db, None).await;
    (db, repo)
}

async fn add_repo(db: &StateDb, url: Option<&str>) -> RepoKey {
    repos::insert(
        db,
        &NewRepo {
            id: RepoId::new(),
            name: "r".into(),
            ns: vec![1],
            remote_url: url.map(str::to_owned),
            remote_kind: RemoteKind::Server,
            default_branch: "master".into(),
            windows_safe: true,
        },
    )
    .await
    .unwrap()
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

#[tokio::test]
async fn learn_merges_and_most_named_paths_come_first() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo) = open(&dir).await;
    assert_eq!(
        learn(&db, repo, "vite:app", &[p("a"), p("b")], false, 10)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        learn(&db, repo, "vite:app", &[p("b"), p("c")], false, 20)
            .await
            .unwrap(),
        3
    );
    let got = paths(&db, repo, "vite:app").await.unwrap();
    assert_eq!(got, vec![p("b"), p("a"), p("c")]);
    // Replacing keeps only the new paths.
    assert_eq!(
        learn(&db, repo, "vite:app", &[p("z")], true, 30)
            .await
            .unwrap(),
        1
    );
    assert_eq!(paths(&db, repo, "vite:app").await.unwrap(), vec![p("z")]);
    assert!(paths(&db, repo, "none").await.unwrap().is_empty());
}

#[tokio::test]
async fn list_recent_and_forget() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo) = open(&dir).await;
    learn(&db, repo, "old", &[p("a")], false, 100)
        .await
        .unwrap();
    learn(&db, repo, "new", &[p("a"), p("b")], false, 200)
        .await
        .unwrap();
    let l = list(&db, repo).await.unwrap();
    assert_eq!(
        l,
        vec![
            Profile {
                name: "new".into(),
                paths: 2,
                last_used: 200
            },
            Profile {
                name: "old".into(),
                paths: 1,
                last_used: 100
            },
        ]
    );
    assert_eq!(
        recent(&db, repo, 150).await.unwrap(),
        vec!["new".to_owned()]
    );
    assert!(forget(&db, repo, "old").await.unwrap());
    assert!(!forget(&db, repo, "old").await.unwrap());
    assert_eq!(list(&db, repo).await.unwrap().len(), 1);
}

#[tokio::test]
async fn names_and_sizes_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo) = open(&dir).await;
    for bad in ["", "a b", "tab\t", &"x".repeat(MAX_NAME_LEN + 1)] {
        assert!(matches!(
            learn(&db, repo, bad, &[p("a")], false, 1).await,
            Err(ProfileError::BadName)
        ));
    }
    let many: Vec<RepoPath> = (0..=MAX_PATHS).map(|i| p(&format!("f{i}"))).collect();
    assert!(matches!(
        learn(&db, repo, "big", &many, false, 1).await,
        Err(ProfileError::TooMany)
    ));
    // A full profile drops its least named paths first.
    learn(&db, repo, "big", &many[..MAX_PATHS], false, 1)
        .await
        .unwrap();
    learn(&db, repo, "big", &[p("f0")], false, 2).await.unwrap();
    assert_eq!(
        learn(&db, repo, "big", &[p("extra")], false, 3)
            .await
            .unwrap(),
        MAX_PATHS as u64
    );
    let got = paths(&db, repo, "big").await.unwrap();
    assert_eq!(got.len(), MAX_PATHS);
    assert_eq!(got[0], p("f0"));
    assert!(got.contains(&p("extra")));
    // Past the profile bound the least recently learned profile goes.
    for i in 0..MAX_PROFILES {
        learn(&db, repo, &format!("n{i}"), &[p("a")], false, 10 + i as i64)
            .await
            .unwrap();
    }
    let names: Vec<String> = list(&db, repo)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names.len(), MAX_PROFILES);
    assert!(!names.contains(&"big".to_owned()));
}

#[tokio::test]
async fn a_new_clone_adopts_the_profiles_of_the_same_remote() {
    let dir = tempfile::tempdir().unwrap();
    let (db, other) = open(&dir).await;
    let url = "https://example.com/r.git";
    let first = add_repo(&db, Some(url)).await;
    let second = add_repo(&db, Some(url)).await;
    learn(&db, first, "vite:app", &[p("a"), p("b")], false, 10)
        .await
        .unwrap();
    learn(&db, other, "vite:app", &[p("x")], false, 10)
        .await
        .unwrap();
    let fresh = add_repo(&db, Some(url)).await;
    learn(&db, second, "vite:app", &[p("b"), p("c")], false, 20)
        .await
        .unwrap();
    assert_eq!(adopt(&db, fresh, url).await.unwrap(), 3);
    let got = paths(&db, fresh, "vite:app").await.unwrap();
    assert_eq!(got.len(), 3);
    assert!(!got.contains(&p("x")), "another remote's paths stay there");
    // Adopting again changes nothing.
    assert_eq!(adopt(&db, fresh, url).await.unwrap(), 0);
}
