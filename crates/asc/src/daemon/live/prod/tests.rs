//! Tests of the production live driver pieces: settings, the child command, the cloud wake and
//! the repository scan over a real `state.db`.

use asc_state::home::{RepoId, WorkspaceId};
use asc_state::state::repos::NewRepo;
use asc_state::state::workspaces::{Mode as WsMode, NewWorkspace};

use super::*;

fn home() -> (tempfile::TempDir, AscHome) {
    let dir = tempfile::tempdir().unwrap();
    let home = AscHome::open(dir.path().join("home")).unwrap();
    (dir, home)
}

// ------------------------------------------------------------------------------ settings

#[test]
fn durations_are_digits_and_a_unit() {
    for (text, want) in [
        ("2s", Some(Duration::from_secs(2))),
        ("2", Some(Duration::from_secs(2))),
        ("500ms", Some(Duration::from_millis(500))),
        ("5m", Some(Duration::from_secs(300))),
        (" 1h ", Some(Duration::from_secs(3600))),
        ("0s", Some(Duration::ZERO)),
        ("", None),
        ("s", None),
        ("-1s", None),
        ("1.5s", None),
        ("2d", None),
        ("2 s", Some(Duration::from_secs(2))),
        ("99999999999999999999s", None),
        ("18446744073709551615h", None),
    ] {
        assert_eq!(parse_duration(text), want, "{text:?}");
    }
}

#[test]
fn timing_uses_the_defaults_and_holds_values_to_a_range() {
    let none = |_: &str| None;
    assert_eq!(timing_of(&none), Timing::default());

    let set = |k: &str| match k {
        "asc.live.debounce" => Some("500ms".to_owned()),
        "asc.live.pollInterval" => Some("2m".to_owned()),
        _ => None,
    };
    let t = timing_of(&set);
    assert_eq!(t.debounce, Duration::from_millis(500));
    assert_eq!(t.poll_interval, Duration::from_secs(120));

    // Silly values are held to the range; text that is no duration is the default.
    let silly = |k: &str| match k {
        "asc.live.debounce" => Some("99h".to_owned()),
        "asc.live.pollInterval" => Some("1s".to_owned()),
        _ => None,
    };
    let t = timing_of(&silly);
    assert_eq!(t.debounce, DEBOUNCE_RANGE.1);
    assert_eq!(t.poll_interval, POLL_RANGE.0);
    let junk = |_: &str| Some("soon".to_owned());
    assert_eq!(timing_of(&junk), Timing::default());
}

// ------------------------------------------------------------------------------ the child

fn runner(home: &AscHome) -> ProdRunner {
    ProdRunner::new(
        home.clone(),
        PathBuf::from("asc-under-test"),
        RepoKey(1),
        Arc::new(EventLog::none()),
    )
}

#[tokio::test]
async fn the_child_runs_the_pass_with_an_empty_hooks_path_and_no_terminal_or_daemon() {
    let (_dir, home) = home();
    let r = runner(&home);
    let cmd = r.command(".").unwrap();
    let std = cmd.as_std();
    assert_eq!(std.get_program(), "asc-under-test");
    assert_eq!(std.get_args().collect::<Vec<_>>(), ["live", "run"]);

    let envs: HashMap<String, Option<String>> = std
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let at = |k: &str| envs.get(k).cloned().flatten();
    assert_eq!(at("ASC_NO_DAEMON").as_deref(), Some("1"));
    assert_eq!(at("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    // Our entry follows whatever the daemon inherited, so it wins and drops nothing.
    let inherited: usize = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    assert_eq!(
        at("GIT_CONFIG_COUNT").as_deref(),
        Some((inherited + 1).to_string().as_str())
    );
    assert_eq!(
        at(&format!("GIT_CONFIG_KEY_{inherited}")).as_deref(),
        Some("core.hooksPath")
    );
    let hookless = PathBuf::from(at(&format!("GIT_CONFIG_VALUE_{inherited}")).unwrap());
    assert_eq!(hookless, crate::cli::live::hookless_dir(&home));
    assert!(hookless.is_dir(), "the directory exists");
    assert_eq!(
        std::fs::read_dir(&hookless).unwrap().count(),
        0,
        "and is empty"
    );
    // Variables that would steer discovery or prompt are removed from the child.
    for gone in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_ASKPASS"] {
        assert_eq!(envs.get(gone), Some(&None), "{gone}");
    }
}

#[tokio::test]
async fn a_child_that_cannot_start_is_recorded_as_failed() {
    let (dir, home) = home();
    let db = StateDb::open(&home).await.unwrap();
    let repo = repos::insert(
        &db,
        &new_repo("r", Some("asc+http://h/r"), RemoteKind::Server),
    )
    .await
    .unwrap();
    let wt = dir.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let id = WorkspaceId::new();
    workspaces::insert(&db, &new_ws(id, repo, &wt))
        .await
        .unwrap();

    let r = ProdRunner::new(
        home.clone(),
        dir.path().join("no-such-asc"),
        repo,
        Arc::new(EventLog::none()),
    );
    assert!(r.run(&id.to_string()).await.is_err());
    let state = crate::cli::live::load(&home, &id.to_string()).unwrap();
    assert_eq!(state.last_run.unwrap().outcome, "failed");
    assert!(!state.paused, "a failed run is retried, not paused");

    assert!(r.run("not an id").await.is_err());
    assert!(r.run(&WorkspaceId::new().to_string()).await.is_err());
}

#[tokio::test]
async fn output_is_kept_to_a_bound_and_read_to_the_end() {
    let big = vec![b'x'; MAX_OUT * 3];
    let kept = drain(Some(&big[..])).await;
    assert_eq!(kept.len(), MAX_OUT);
    assert_eq!(drain(None::<&[u8]>).await, "");
}

// ------------------------------------------------------------------------------ the cloud wake

#[tokio::test]
async fn a_cloud_wake_reaches_only_the_driver_of_its_repository() {
    let repo = RepoKey(7_001);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(forward_wakes(repo, tx));
    tokio::task::yield_now().await;

    cloud_wake().notify(RepoKey(7_002));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "another repository's wake is not ours"
    );
    cloud_wake().notify(repo);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(())
    );
    // The forwarder ends with its receiver.
    drop(rx);
    cloud_wake().notify(repo);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the forwarder ends")
        .unwrap();
}

// ------------------------------------------------------------------------------ the scan

fn new_repo(name: &str, url: Option<&str>, kind: RemoteKind) -> NewRepo {
    NewRepo {
        id: RepoId::new(),
        name: name.into(),
        ns: vec![1],
        remote_url: url.map(str::to_owned),
        remote_kind: kind,
        default_branch: "master".into(),
        windows_safe: true,
    }
}

fn new_ws(id: WorkspaceId, repo: RepoKey, path: &std::path::Path) -> NewWorkspace {
    let path = path.to_string_lossy().into_owned();
    NewWorkspace {
        id,
        repo,
        mount: path.clone(),
        path,
        mode: WsMode::Plain,
        head: "refs/heads/master".into(),
        base_cs: None,
        sparse: None,
        ephemeral_until: None,
    }
}

#[tokio::test]
async fn only_asc_server_repositories_with_a_workspace_are_driven() {
    let (dir, home) = home();
    let db = StateDb::open(&home).await.unwrap();
    let server = repos::insert(
        &db,
        &new_repo("s", Some("asc+http://h/s"), RemoteKind::Server),
    )
    .await
    .unwrap();
    let git = repos::insert(&db, &new_repo("g", Some("git@h:g"), RemoteKind::GitRemote))
        .await
        .unwrap();
    let local = repos::insert(&db, &new_repo("l", None, RemoteKind::LocalOnly))
        .await
        .unwrap();
    let bare = repos::insert(
        &db,
        &new_repo("b", Some("asc+http://h/b"), RemoteKind::Server),
    )
    .await
    .unwrap();
    let mut dirs = Vec::new();
    for (i, repo) in [server, git, local].into_iter().enumerate() {
        let d = dir.path().join(format!("wt{i}"));
        std::fs::create_dir_all(&d).unwrap();
        workspaces::insert(&db, &new_ws(WorkspaceId::new(), repo, &d))
            .await
            .unwrap();
        dirs.push(d);
    }
    // A second worktree of the server repo, and a workspace whose directory is gone.
    let second = dir.path().join("second");
    std::fs::create_dir_all(&second).unwrap();
    let w2 = WorkspaceId::new();
    workspaces::insert(&db, &new_ws(w2, server, &second))
        .await
        .unwrap();
    workspaces::insert(
        &db,
        &new_ws(WorkspaceId::new(), server, &dir.path().join("deleted")),
    )
    .await
    .unwrap();
    let _ = bare; // a server repo with no workspace has nothing to run in

    let found = server_repos(&db).await.unwrap();
    assert_eq!(found.len(), 1);
    let (key, url, paths) = &found[0];
    assert_eq!((*key, url.as_str()), (server, "asc+http://h/s"));
    assert_eq!(
        paths,
        &vec![dirs[0].clone(), second.clone()],
        "existing dirs only"
    );

    // The runner names worktrees by workspace id and skips the one whose directory is gone.
    let r = ProdRunner::new(
        home.clone(),
        PathBuf::from("asc"),
        server,
        Arc::new(EventLog::none()),
    );
    let names = r.worktrees().await;
    assert_eq!(names.len(), 2);
    assert!(names.contains(&w2.to_string()));
    assert!(!r.paused(&w2.to_string()).await);
    crate::cli::live::record_conflict(&home, &w2.to_string(), "wt", server.0, "x").unwrap();
    assert!(
        r.paused(&w2.to_string()).await,
        "the pause file is the one of `asc live pause`"
    );
}

#[tokio::test]
async fn the_supervisor_is_off_with_the_daemon_switched_off_and_stops_cleanly_otherwise() {
    let (_dir, home) = home();
    let events = Arc::new(EventLog::none());
    // No server repository: the supervisor idles and stops on request.
    if !lifecycle::disabled(&|k| std::env::var_os(k)) {
        let sup = Supervisor::start(home, PathBuf::from("asc"), events).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(10), sup.stop())
            .await
            .expect("stops");
    }
}
