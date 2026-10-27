//! `CrossSearch` over several repositories (build-out feature 14, `cross-search.v1`).
//!
//! The service asks the authorizer about every repository it would search, on every call. A
//! repository the caller cannot read, or that does not exist, leaves no trace: not in the hits, not
//! in a count, not in an error, and a page token cannot be replayed by someone else.

use std::sync::Arc;

use asc_core::git::{ObjectKind, blob_oid};
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Credential, Principal};
use asc_rpc::server::{RepoName, ServerInfo, wire};
use asc_rpc::services::cross_search_server::CrossSearch;
use asc_serve::codeintel::cross::{CrossSearchService, Limits};
use asc_serve::codeintel::rpc::{CodeIntelRpc, Settings};
use asc_serve::migrate;
use asc_serve::registry::{Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use futures::StreamExt;
use tonic::Request;

const WORD: &str = "quokka_in_every_repo";

/// A token is its own name; each is granted the repositories listed for it.
struct Acl {
    grants: Vec<(&'static str, Vec<&'static str>)>,
    /// Questions asked of `authorize`.
    asked: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        _access: Access,
    ) -> Result<Principal, AuthError> {
        let token = token.ok_or(AuthError::Unauthenticated)?;
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        for (name, repos) in &self.grants {
            if Credential::bearer(token.clone()) == cred(name) {
                return if repos.contains(&repo) {
                    Ok(Principal {
                        id: (*name).to_owned(),
                    })
                } else {
                    Err(AuthError::Forbidden)
                };
            }
        }
        Err(AuthError::Unauthenticated)
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        let credential = credential.ok_or(AuthError::Unauthenticated)?;
        for (name, _) in &self.grants {
            if *credential == cred(name) {
                return Ok(Principal {
                    id: (*name).to_owned(),
                });
            }
        }
        Err(AuthError::Unauthenticated)
    }
}

fn cred(name: &str) -> Credential {
    Credential::bearer(BearerToken::new(name).unwrap())
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

async fn commit(e: &RepoEntry, files: &[(String, Vec<u8>)]) -> ChangesetId {
    let engine = e.backend.engine();
    let mut changes: Vec<(RepoPath, Change)> = Vec::new();
    for (p, b) in files {
        let id = FileId::of(b);
        e.objects
            .put(&ObjKey::File(id), encode_stored(b))
            .await
            .unwrap();
        changes.push((
            RepoPath::new(p.as_str()).unwrap(),
            Change::write(Kind::File, id, b.len() as u64, blob_oid(b), None).unwrap(),
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
                message: b"m".to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap();
    let git = engine.git_commit_of(cs).await.unwrap();
    e.git_map
        .put(&[GitMapRow {
            git,
            kind: ObjectKind::Commit,
            asc: *cs.as_bytes(),
        }])
        .await
        .unwrap();
    cs
}

async fn set_master(r: &Registry, e: &RepoEntry, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(asc_core::id::RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    r.refs().update(e.key, &txn).await.unwrap();
}

struct Rig {
    svc: CrossSearchService,
    acl: Arc<Acl>,
    _dir: tempfile::TempDir,
}

/// Repositories `o/a`, `o/b`, `o/c` and `p/d`, each with `files` files that hold [`WORD`] and a
/// word of their own.
async fn rig(files: usize, limits: Limits, grants: Vec<(&'static str, Vec<&'static str>)>) -> Rig {
    rig_with(files, 0, limits, grants).await
}

/// [`rig`] with `extra` empty repositories `big/r<i>` that nobody may read.
async fn rig_with(
    files: usize,
    extra: usize,
    limits: Limits,
    grants: Vec<(&'static str, Vec<&'static str>)>,
) -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    for name in ["o/a", "o/b", "o/c", "p/d"] {
        let e = registry
            .create(&RepoName::parse(name).unwrap(), &Default::default())
            .await
            .unwrap();
        let tag = name.replace('/', "_");
        let fs: Vec<(String, Vec<u8>)> = (0..files)
            .map(|i| {
                (
                    format!("f{i:03}.txt"),
                    format!("{WORD} {tag}_only_{i}\n").into_bytes(),
                )
            })
            .collect();
        let cs = commit(&e, &fs).await;
        set_master(&registry, &e, cs).await;
    }
    for i in 0..extra {
        registry
            .create(
                &RepoName::parse(&format!("big/r{i:05}")).unwrap(),
                &Default::default(),
            )
            .await
            .unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::new(dir.path().join("ci"));
    settings.page_key = Some([7; 32]);
    settings.cross = limits;
    let rpc = Arc::new(CodeIntelRpc::new(registry, settings, None).unwrap());
    let acl = Arc::new(Acl {
        grants,
        asked: Default::default(),
    });
    let svc = CrossSearchService::new(rpc, acl.clone(), ServerInfo::default());
    Rig {
        svc,
        acl,
        _dir: dir,
    }
}

fn target(repos: &[&str]) -> wire::CrossSearchTarget {
    wire::CrossSearchTarget {
        repos: repos.iter().map(|r| (*r).to_owned()).collect(),
        ..Default::default()
    }
}

fn scope(kind: wire::OwnerKind, name: &str) -> wire::CrossSearchTarget {
    wire::CrossSearchTarget {
        scope: Some(wire::OwnerRef {
            kind: kind as i32,
            name: name.to_owned(),
        }),
        ..Default::default()
    }
}

fn request(
    t: wire::CrossSearchTarget,
    pattern: &str,
    max: u32,
    token: &str,
) -> wire::CrossSearchRequest {
    wire::CrossSearchRequest {
        target: Some(t),
        pattern: pattern.to_owned(),
        case_sensitive: true,
        budget: Some(wire::Budget {
            max_results: max,
            ..Default::default()
        }),
        page_token: token.to_owned(),
        ..Default::default()
    }
}

async fn search(
    rig: &Rig,
    who: &str,
    req: wire::CrossSearchRequest,
) -> Result<wire::CrossSearchResponse, tonic::Status> {
    let mut r = Request::new(req);
    r.extensions_mut().insert(cred(who));
    let mut s = rig.svc.search(r).await?.into_inner();
    s.next().await.expect("one response")
}

fn repos(r: &wire::CrossSearchResponse) -> Vec<String> {
    r.repos
        .iter()
        .map(|h| h.repo.as_ref().unwrap().name.clone())
        .collect()
}

fn grants() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("both", vec!["o/a", "o/b", "o/c", "p/d"]),
        ("some", vec!["o/a", "p/d"]),
        ("none", vec![]),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repositories_the_caller_cannot_read_leave_no_trace() {
    let rig = rig(2, Limits::default(), grants()).await;
    let named = || target(&["o/a", "o/b", "o/c", "p/d", "o/ghost"]);
    let all = search(&rig, "both", request(named(), WORD, 100, ""))
        .await
        .unwrap();
    assert_eq!(repos(&all), ["o/a", "o/b", "o/c", "p/d"]);
    assert_eq!(all.searched_repos, 4);

    let some = search(&rig, "some", request(named(), WORD, 100, ""))
        .await
        .unwrap();
    assert_eq!(repos(&some), ["o/a", "p/d"]);
    assert_eq!(
        some.searched_repos, 2,
        "unreadable repositories are not counted"
    );
    let text = format!("{some:?}");
    assert!(
        !text.contains("o_b_only") && !text.contains("o_c_only"),
        "{text}"
    );

    // A reader of nothing sees what a search of nothing shows, with no error that names a repository.
    let none = search(&rig, "none", request(named(), WORD, 100, ""))
        .await
        .unwrap();
    assert!(none.repos.is_empty());
    assert_eq!(none.searched_repos, 0);
    assert!(!none.truncated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scope_expands_to_the_repositories_the_caller_may_read() {
    let rig = rig(1, Limits::default(), grants()).await;
    let user = search(
        &rig,
        "some",
        request(scope(wire::OwnerKind::User, "o"), WORD, 100, ""),
    )
    .await
    .unwrap();
    assert_eq!(repos(&user), ["o/a"]);
    let instance = search(
        &rig,
        "some",
        request(scope(wire::OwnerKind::Instance, ""), WORD, 100, ""),
    )
    .await
    .unwrap();
    assert_eq!(repos(&instance), ["o/a", "p/d"]);
    let org = search(
        &rig,
        "both",
        request(scope(wire::OwnerKind::Org, "p"), WORD, 100, ""),
    )
    .await
    .unwrap();
    assert_eq!(repos(&org), ["p/d"]);
    // An owner name is a whole segment: "o" is not a prefix of "ox".
    let nothing = search(
        &rig,
        "both",
        request(scope(wire::OwnerKind::User, "ox"), WORD, 100, ""),
    )
    .await
    .unwrap();
    assert!(nothing.repos.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pages_walk_the_repositories_once_each_and_belong_to_their_caller() {
    let rig = rig(3, Limits::default(), grants()).await;
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut token = String::new();
    let mut pages = 0;
    loop {
        let r = search(
            &rig,
            "both",
            request(target(&["o/a", "o/b", "o/c", "p/d"]), WORD, 4, &token),
        )
        .await
        .unwrap();
        for h in &r.repos {
            for f in &h.files {
                seen.push((
                    h.repo.as_ref().unwrap().name.clone(),
                    String::from_utf8_lossy(&f.path).into_owned(),
                ));
            }
        }
        pages += 1;
        assert!(pages < 20, "the walk does not end");
        if r.next_page_token.is_empty() {
            break;
        }
        assert!(r.truncated);
        token = r.next_page_token;
        // Someone else cannot continue the walk.
        let stolen = search(
            &rig,
            "some",
            request(target(&["o/a", "o/b", "o/c", "p/d"]), WORD, 4, &token),
        )
        .await
        .unwrap_err();
        assert_eq!(stolen.code(), tonic::Code::InvalidArgument);
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "a hit came twice: {seen:?}");
    assert_eq!(
        seen.len(),
        12,
        "3 files in each of 4 repositories: {seen:?}"
    );
    assert!(pages > 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_repository_limit_cuts_a_scope_walk_and_says_so() {
    let limits = Limits {
        max_repos: 2,
        ..Limits::default()
    };
    let rig = rig(1, limits, grants()).await;
    let r = search(
        &rig,
        "both",
        request(scope(wire::OwnerKind::Instance, ""), WORD, 100, ""),
    )
    .await
    .unwrap();
    assert_eq!(r.searched_repos, 2);
    assert!(r.truncated);
    assert!(!r.next_page_token.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pattern_that_matches_everything_is_too_broad() {
    let rig = rig(1, Limits::default(), grants()).await;
    for (pattern, mode) in [
        ("ab", wire::SearchMode::Literal),
        ("", wire::SearchMode::Literal),
        (".*", wire::SearchMode::Regex),
        ("(a|)", wire::SearchMode::Regex),
    ] {
        let mut req = request(target(&["o/a"]), pattern, 10, "");
        req.mode = mode as i32;
        let e = search(&rig, "both", req).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{pattern:?}");
        let detail = asc_rpc::server::status_rpc_error(&e);
        assert!(
            matches!(detail, Some(asc_rpc::RpcError::SearchTooBroad(_))),
            "{pattern:?}: {e:?}"
        );
    }
    let mut ok = request(target(&["o/a"]), "quokka.*repo", 10, "");
    ok.mode = wire::SearchMode::Regex as i32;
    assert!(!search(&rig, "both", ok).await.unwrap().repos.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_than_sixty_four_named_repositories_are_refused() {
    let rig = rig(1, Limits::default(), grants()).await;
    let names: Vec<String> = (0..65).map(|i| format!("o/r{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let e = search(&rig, "both", request(target(&refs), WORD, 10, ""))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
}

/// Thousands of repositories the caller cannot read cost a bounded number of questions to the
/// authorizer and finish in time, for a search, a symbol lookup and a symbol panel; the walk gives
/// a token to go on from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousands_of_unreadable_repositories_cost_a_bounded_number_of_authorizer_calls() {
    let limits = Limits {
        max_repos: 200,
        timeout: std::time::Duration::from_secs(60),
    };
    let rig = rig_with(1, 5000, limits, grants()).await;
    let cap = asc_serve::codeintel::cross::MAX_ASKS;
    let asked = |rig: &Rig| rig.acl.asked.load(std::sync::atomic::Ordering::SeqCst);

    let r = search(
        &rig,
        "none",
        request(scope(wire::OwnerKind::Instance, ""), WORD, 100, ""),
    )
    .await
    .unwrap();
    assert!(r.repos.is_empty());
    assert!(r.truncated && !r.next_page_token.is_empty(), "{r:?}");
    assert!(asked(&rig) <= cap, "{} questions", asked(&rig));

    // The token goes on from there, and still within its own allowance.
    let before = asked(&rig);
    let r2 = search(
        &rig,
        "none",
        request(
            scope(wire::OwnerKind::Instance, ""),
            WORD,
            100,
            &r.next_page_token,
        ),
    )
    .await
    .unwrap();
    assert!(r2.truncated);
    assert!(asked(&rig) - before <= cap);

    let before = asked(&rig);
    let sym = wire::LookupSymbolRequest {
        target: Some(scope(wire::OwnerKind::Instance, "")),
        name: "anything".to_owned(),
        ..Default::default()
    };
    let mut req = Request::new(sym);
    req.extensions_mut().insert(cred("none"));
    let out = rig.svc.lookup_symbol(req).await.unwrap().into_inner();
    assert!(out.hits.is_empty() && out.truncated);
    assert!(asked(&rig) - before <= cap);

    // The panel's repository is readable; the cross-repository part is bounded all the same.
    let before = asked(&rig);
    let panel = wire::SymbolPanelRequest {
        repo: Some(wire::RepoRef {
            name: "o/a".to_owned(),
            id_mode: 0,
        }),
        name: "anything".to_owned(),
        cross_repo: true,
        ..Default::default()
    };
    let mut req = Request::new(panel);
    req.extensions_mut().insert(cred("some"));
    let _ = rig.svc.symbol_panel(req).await;
    assert!(
        asked(&rig) - before <= 8 * 20 + 1,
        "{}",
        asked(&rig) - before
    );
}

/// A call past its deadline stops asking and answers `DEADLINE_EXCEEDED`, for the lookup and the
/// panel as well as the search.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_deadline_stops_every_call() {
    let limits = Limits {
        max_repos: 200,
        timeout: std::time::Duration::from_millis(1),
    };
    let rig = rig_with(1, 300, limits, grants()).await;
    let r = search(
        &rig,
        "none",
        request(scope(wire::OwnerKind::Instance, ""), WORD, 100, ""),
    )
    .await;
    // Either it ran out of time, or it stopped asking and said it was cut short.
    match r {
        Err(e) => assert_eq!(e.code(), tonic::Code::DeadlineExceeded),
        Ok(r) => assert!(r.truncated || r.repos.is_empty()),
    }
}
