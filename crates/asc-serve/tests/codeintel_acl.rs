//! `CodeIntel` authorisation per namespace and composed fan-out (PLAN I16, SPEC §23.4.1, §23.7).
//!
//! A main repository composes child repositories through gitlinks. A child the caller cannot
//! read is absent from every answer, at every result budget `k`; the decision on a namespace is
//! taken before that namespace is opened; and a page token is bound to the principal.

use std::sync::{Arc, Mutex};

use asc_core::git::ObjectKind;
use asc_core::git::blob_oid;
use asc_core::id::{ChangesetId, FileId, GitOid};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Credential, Principal};
use asc_rpc::codeintel as ci;
use asc_rpc::server::{RepoName, ServerInfo, wire};
use asc_rpc::services::code_intel_server::CodeIntel;
use asc_serve::codeintel::rpc::{CodeIntelRpc, CodeIntelService, Settings};
use asc_serve::migrate;
use asc_serve::registry::{Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use futures::StreamExt;
use tonic::Request;

const MAIN: &str = "o/main";
const CHILD: &str = "o/secretlib";
const SECRET_CHILD_WORD: &str = "zebrafish_in_child";
const MAIN_WORD: &str = "zebrafish_in_main";

/// Maps a token to the repositories it may read and records every decision asked.
struct Acl {
    grants: Vec<(&'static str, Vec<&'static str>)>,
    asked: Mutex<Vec<String>>,
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
        self.asked.lock().unwrap().push(repo.to_owned());
        for (name, repos) in &self.grants {
            if token_is(token, name) {
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
}

fn token_is(t: &BearerToken, name: &str) -> bool {
    Credential::bearer(t.clone()) == Credential::bearer(BearerToken::new(name).unwrap())
}

fn cred(name: &str) -> Credential {
    Credential::bearer(BearerToken::new(name).unwrap())
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

async fn commit(e: &RepoEntry, files: &[(&str, Vec<u8>)], links: &[(&str, GitOid)]) -> ChangesetId {
    let engine = e.backend.engine();
    let mut changes: Vec<(RepoPath, Change)> = Vec::new();
    for (p, b) in files {
        let id = FileId::of(b);
        e.objects
            .put(&ObjKey::File(id), encode_stored(b))
            .await
            .unwrap();
        changes.push((
            RepoPath::new(*p).unwrap(),
            Change::write(Kind::File, id, b.len() as u64, blob_oid(b), None).unwrap(),
        ));
    }
    for (p, g) in links {
        changes.push((RepoPath::new(*p).unwrap(), Change::Gitlink { commit: *g }));
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    let base = asc_engine::Engine::<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>::empty_tree_id();
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
    svc: CodeIntelService,
    acl: Arc<Acl>,
    _dir: tempfile::TempDir,
}

/// `modules` extra gitlinks (`.gitmodules` entries) beyond the real child, all unreadable and
/// absent; they cost a decision each and nothing else.
async fn rig(modules: usize, grants: Vec<(&'static str, Vec<&'static str>)>, files: usize) -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let main = registry
        .create(&RepoName::parse(MAIN).unwrap(), &Default::default())
        .await
        .unwrap();
    let child = registry
        .create(&RepoName::parse(CHILD).unwrap(), &Default::default())
        .await
        .unwrap();

    let mut cfiles: Vec<(String, Vec<u8>)> = Vec::new();
    let mut mfiles: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..files {
        cfiles.push((
            format!("c{i:04}.txt"),
            format!("{SECRET_CHILD_WORD} {i}\n").into_bytes(),
        ));
        mfiles.push((
            format!("m{i:04}.txt"),
            format!("{MAIN_WORD} {i}\n").into_bytes(),
        ));
    }
    let cf: Vec<(&str, Vec<u8>)> = cfiles
        .iter()
        .map(|(p, b)| (p.as_str(), b.clone()))
        .collect();
    let ccs = commit(&child, &cf, &[]).await;
    set_master(&registry, &child, ccs).await;
    let asc_commit = child.backend.engine().git_commit_of(ccs).await.unwrap();

    let mut modules_text = format!("[submodule \"lib\"]\n\tpath = lib\n\turl = ../secretlib.git\n");
    let mut links = vec![("lib", asc_commit)];
    for i in 0..modules {
        modules_text.push_str(&format!(
            "[submodule \"x{i}\"]\n\tpath = x{i}\n\turl = ../ghost{i}.git\n"
        ));
        links.push((Box::leak(format!("x{i}").into_boxed_str()), asc_commit));
    }
    let mut mf: Vec<(&str, Vec<u8>)> = mfiles
        .iter()
        .map(|(p, b)| (p.as_str(), b.clone()))
        .collect();
    mf.push((".gitmodules", modules_text.into_bytes()));
    let mcs = commit(&main, &mf, &links).await;
    set_master(&registry, &main, mcs).await;

    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::new(dir.path().join("ci"));
    settings.page_key = Some([9; 32]);
    let rpc = Arc::new(CodeIntelRpc::new(registry, settings, None).unwrap());
    let acl = Arc::new(Acl {
        grants,
        asked: Mutex::new(Vec::new()),
    });
    let svc = CodeIntelService::new(rpc, acl.clone(), ServerInfo::default());
    Rig {
        svc,
        acl,
        _dir: dir,
    }
}

fn search_req(pattern: &str, k: u32, token: &str) -> wire::SearchRequest {
    let r = ci::SearchRequest {
        scope: ci::Scope {
            repo: MAIN.to_owned(),
            id_mode: IdMode::Asc,
            at: ci::At::Rev("master".to_owned()),
            stack_diff_only: false,
            paths: Vec::new(),
            exclude_paths: Vec::new(),
            langs: Vec::new(),
            include_excluded: false,
        },
        pattern: pattern.to_owned(),
        mode: ci::SearchMode::Literal,
        case_sensitive: true,
        context_lines: 0,
        budget: ci::Budget {
            max_results: k,
            max_bytes: 0,
            max_tokens: 0,
        },
        page_token: token.to_owned(),
    };
    (&r).into()
}

async fn search(
    rig: &Rig,
    who: &str,
    pattern: &str,
    k: u32,
    token: &str,
) -> Result<wire::SearchResponse, tonic::Status> {
    let mut req = Request::new(search_req(pattern, k, token));
    req.extensions_mut().insert(cred(who));
    let mut s = rig.svc.search(req).await?.into_inner();
    s.next().await.expect("one response")
}

fn paths(r: &wire::SearchResponse) -> Vec<String> {
    r.files
        .iter()
        .map(|f| String::from_utf8_lossy(&f.path).into_owned())
        .collect()
}

fn leaks_child(r: &wire::SearchResponse) -> bool {
    let text = format!("{r:?}");
    text.contains(SECRET_CHILD_WORD) || paths(r).iter().any(|p| p.starts_with("lib/"))
}

const READER: &str = "reader";
const OUTSIDER: &str = "outsider";

fn grants() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![(READER, vec![MAIN, CHILD]), (OUTSIDER, vec![MAIN])]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readable_child_joins_the_query_so_the_deny_tests_are_not_vacuous() {
    let rig = rig(0, grants(), 3).await;
    let r = search(&rig, READER, SECRET_CHILD_WORD, 100, "")
        .await
        .unwrap();
    assert!(
        paths(&r).iter().any(|p| p.starts_with("lib/")),
        "the readable child was not searched: {:?}",
        paths(&r)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_the_caller_cannot_read_is_absent_at_every_k() {
    for k in [1u32, 10, 100, 1000] {
        let n = (k as usize).min(30);
        let rig = rig(0, grants(), n).await;
        // The word occurs in both namespaces; only the main one may answer.
        for word in [SECRET_CHILD_WORD, "zebrafish_in"] {
            let r = search(&rig, OUTSIDER, word, k, "").await.unwrap();
            assert!(!leaks_child(&r), "k={k} {word}: {r:?}");
            // Following every page never reaches the child either.
            let mut token = r.next_page_token.clone();
            let mut pages = 0;
            while !token.is_empty() && pages < 50 {
                let r = search(&rig, OUTSIDER, word, k, &token).await.unwrap();
                assert!(!leaks_child(&r), "k={k} {word} page: {r:?}");
                token = r.next_page_token;
                pages += 1;
            }
        }
        // The decision was asked, and refused.
        assert!(rig.acl.asked.lock().unwrap().iter().any(|n| n == CHILD));
        // The main repository's own hits are still returned.
        let r = search(&rig, OUTSIDER, MAIN_WORD, k, "").await.unwrap();
        assert!(!r.files.is_empty(), "k={k}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_unreadable_and_absent_modules_do_not_change_the_answer() {
    let rig = rig(40, grants(), 5).await;
    let base = search(&rig, OUTSIDER, "zebrafish_in", 100, "")
        .await
        .unwrap();
    assert!(!leaks_child(&base));
    assert!(!base.files.is_empty());
    let full = search(&rig, READER, "zebrafish_in", 100, "").await.unwrap();
    assert!(paths(&full).iter().any(|p| p.starts_with("lib/")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_who_cannot_read_the_main_repository_gets_nothing_and_nothing_is_opened() {
    let rig = rig(0, vec![(READER, vec![MAIN, CHILD]), ("nobody", vec![])], 2).await;
    let e = search(&rig, "nobody", MAIN_WORD, 10, "").await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    // Only the scope repository was asked about: no child was looked at.
    assert_eq!(*rig.acl.asked.lock().unwrap(), vec![MAIN.to_owned()]);
    let e = search(&rig, "stranger", MAIN_WORD, 10, "")
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_token_is_bound_to_its_principal_and_its_request() {
    let rig = rig(0, grants(), 12).await;
    let r = search(&rig, READER, "zebrafish_in", 3, "").await.unwrap();
    assert!(!r.next_page_token.is_empty());
    // The same token, presented by another principal, is refused.
    let e = search(&rig, OUTSIDER, "zebrafish_in", 3, &r.next_page_token)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    // And for another pattern.
    let e = search(&rig, READER, MAIN_WORD, 3, &r.next_page_token)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    // A flipped byte fails the MAC.
    let mut t = r.next_page_token.clone().into_bytes();
    let i = t.len() / 2;
    t[i] = if t[i] == b'A' { b'B' } else { b'A' };
    let e = search(
        &rig,
        READER,
        "zebrafish_in",
        3,
        &String::from_utf8(t).unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    // The owner continues.
    search(&rig, READER, "zebrafish_in", 3, &r.next_page_token)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_scope_is_refused_over_the_network() {
    let rig = rig(0, grants(), 1).await;
    let mut w = search_req(MAIN_WORD, 10, "");
    w.scope.as_mut().unwrap().at = Some(wire::scope::At::Workspace(true));
    let mut req = Request::new(w);
    req.extensions_mut().insert(cred(READER));
    let e = match rig.svc.search(req).await {
        Err(e) => e,
        Ok(r) => r.into_inner().next().await.unwrap().unwrap_err(),
    };
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
}
