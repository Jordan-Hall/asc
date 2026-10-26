//! `CodeIntel.UploadScip` and the precise tier of `Symbols` (PLAN I31 and I16, SPEC §23.6 CI-D3).
//!
//! The service runs on loopback behind the real `AuthInterceptor`, so these calls go through the
//! same streaming, size and authorisation paths an `asc codeintel upload-scip` client reaches. The
//! upload needs `Write` on the repository (a reader is refused before any data is read), keeps
//! only documents whose path exists in the revision and whose embedded text matches the file, and
//! the uploaded rows answer `Symbols` only inside the query's own scope.

use std::net::SocketAddr;
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
use asc_rpc::services::code_intel_client::CodeIntelClient;
use asc_serve::codeintel::rpc::{CodeIntelRpc, CodeIntelService, MAX_UPLOAD_PART, Settings};
use asc_serve::codeintel::scip::{MAX_INDEX_BYTES, WireDocument, WireIndex, WireOccurrence};
use asc_serve::migrate;
use asc_serve::registry::{Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use bytes::Bytes;
use prost::Message as _;
use tonic::transport::Channel;
use tonic::{Code, Request};

const REPO: &str = "o/app";
const OTHER: &str = "o/other";
const LIB: &str = "pub fn helper() {}\n";
const MAIN_RS: &str = "fn main() {\n    helper();\n}\n";
const SYM: &str = "scip-rust cargo demo 0.1.0 util/helper().";

const WRITER: &str = "writer";
const READER: &str = "reader";

/// `writer` may write `o/app`; `reader` may only read it; neither has anything on `o/other`.
struct Acl;

#[async_trait::async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let token = token.ok_or(AuthError::Unauthenticated)?;
        let is = |name: &str| {
            Credential::bearer(token.clone()) == Credential::bearer(BearerToken::new(name).unwrap())
        };
        let (name, max) = if is(WRITER) {
            (WRITER, Access::Write)
        } else if is(READER) {
            (READER, Access::Read)
        } else {
            return Err(AuthError::Unauthenticated);
        };
        if repo == REPO && access <= max {
            Ok(Principal { id: name.into() })
        } else {
            Err(AuthError::Forbidden)
        }
    }
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

async fn commit(e: &RepoEntry, files: &[(&str, &str)]) -> ChangesetId {
    let engine = e.backend.engine();
    let mut changes: Vec<(RepoPath, Change)> = Vec::new();
    for (p, text) in files {
        let b = text.as_bytes();
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
    client: CodeIntelClient<Channel>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

async fn rig() -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let app = registry
        .create(&RepoName::parse(REPO).unwrap(), &Default::default())
        .await
        .unwrap();
    registry
        .create(&RepoName::parse(OTHER).unwrap(), &Default::default())
        .await
        .unwrap();
    let cs = commit(&app, &[("src/lib.rs", LIB), ("src/main.rs", MAIN_RS)]).await;
    set_master(&registry, &app, cs).await;

    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::new(dir.path().join("ci"));
    settings.page_key = Some([9; 32]);
    let rpc = Arc::new(CodeIntelRpc::new(registry, settings, None).unwrap());
    let svc = CodeIntelService::new(rpc, Arc::new(Acl), ServerInfo::default());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let incoming = futures::stream::unfold(listener, |l| async move {
        let r = l.accept().await.map(|(s, _)| s);
        Some((r, l))
    });
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(svc.into_service())
            .serve_with_incoming_shutdown(incoming, async {
                let _ = stop_rx.await;
            })
            .await;
    });
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    Rig {
        client: CodeIntelClient::new(channel),
        stop: Some(stop),
        _dir: dir,
    }
}

fn with_auth<T>(who: &str, msg: T) -> Request<T> {
    let mut r = Request::new(msg);
    r.metadata_mut()
        .insert("authorization", format!("Bearer {who}").parse().unwrap());
    r
}

fn occ(range: Vec<i32>, roles: i32) -> WireOccurrence {
    WireOccurrence {
        range,
        symbol: SYM.into(),
        symbol_roles: roles,
    }
}

fn doc(path: &str, text: &str, occs: Vec<WireOccurrence>) -> WireDocument {
    WireDocument {
        relative_path: path.into(),
        occurrences: occs,
        text: text.into(),
    }
}

/// The definition in `src/lib.rs`, one reference in `src/main.rs`, a document the revision does
/// not have, and a second `src/lib.rs` whose embedded text is not what is committed.
fn index() -> Vec<u8> {
    WireIndex {
        documents: vec![
            doc("src/lib.rs", LIB, vec![occ(vec![0, 7, 13], 1)]),
            doc("src/main.rs", "", vec![occ(vec![1, 4, 10], 0)]),
            doc("gone.rs", "", vec![occ(vec![0, 0, 3], 1)]),
            doc("src/lib.rs", "not what is committed", vec![]),
        ],
    }
    .encode_to_vec()
}

fn header(rev: &str, root: &str, size: usize) -> wire::UploadScipRequest {
    wire::UploadScipRequest {
        part: Some(wire::upload_scip_request::Part::Header(
            wire::UploadScipHeader {
                repo: Some(wire::RepoRef {
                    name: REPO.into(),
                    id_mode: wire::IdMode::Asc as i32,
                }),
                rev: rev.into(),
                root: root.into(),
                size: size as u64,
            },
        )),
    }
}

fn data(bytes: &[u8]) -> wire::UploadScipRequest {
    wire::UploadScipRequest {
        part: Some(wire::upload_scip_request::Part::Data(
            Bytes::copy_from_slice(bytes),
        )),
    }
}

/// A well-formed stream for `bytes`: the header, then parts of 1000 bytes.
fn stream(bytes: &[u8]) -> Vec<wire::UploadScipRequest> {
    let mut m = vec![header("master", "", bytes.len())];
    m.extend(bytes.chunks(1000).map(data));
    m
}

async fn upload(
    rig: &Rig,
    who: &str,
    msgs: Vec<wire::UploadScipRequest>,
) -> Result<wire::UploadScipResponse, tonic::Status> {
    let req = with_auth(who, futures::stream::iter(msgs));
    rig.client
        .clone()
        .upload_scip(req)
        .await
        .map(tonic::Response::into_inner)
}

fn scope() -> wire::Scope {
    wire::Scope {
        repo: Some(wire::RepoRef {
            name: REPO.into(),
            id_mode: wire::IdMode::Asc as i32,
        }),
        at: Some(wire::scope::At::Rev("master".into())),
        ..Default::default()
    }
}

async fn symbols(
    rig: &Rig,
    op: wire::SymbolOp,
    name: &str,
    f: impl FnOnce(&mut wire::SymbolsRequest),
) -> Vec<wire::Symbol> {
    let mut q = wire::SymbolsRequest {
        scope: Some(scope()),
        op: op as i32,
        name: name.into(),
        ..Default::default()
    };
    f(&mut q);
    rig.client
        .clone()
        .symbols(with_auth(READER, q))
        .await
        .unwrap()
        .into_inner()
        .symbols
}

fn precise(s: &[wire::Symbol]) -> Vec<(String, u32, bool)> {
    s.iter()
        .filter(|s| s.confidence == "precise")
        .map(|s| {
            (
                String::from_utf8_lossy(&s.path).into_owned(),
                s.line,
                s.is_def,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_uploads_and_only_documents_of_the_revision_are_kept() {
    let rig = rig().await;
    let r = upload(&rig, WRITER, stream(&index())).await.unwrap();
    assert_eq!((r.documents, r.occurrences), (2, 2));
    assert_eq!((r.unknown_path, r.stale, r.skipped_occurrences), (1, 1, 0));
    assert_eq!(r.changeset.len(), 32);
    // A re-upload replaces the rows of the same files instead of adding to them.
    let again = upload(&rig, WRITER, stream(&index())).await.unwrap();
    assert_eq!((again.documents, again.occurrences), (2, 2));
    let defs = symbols(&rig, wire::SymbolOp::Def, "helper", |_| {}).await;
    assert_eq!(precise(&defs), vec![("src/lib.rs".to_owned(), 1, true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn precise_definitions_and_references_lead_the_answer() {
    let rig = rig().await;
    upload(&rig, WRITER, stream(&index())).await.unwrap();
    let defs = symbols(&rig, wire::SymbolOp::Def, "helper", |_| {}).await;
    assert_eq!(defs[0].confidence, "precise");
    assert_eq!(defs[0].end_line, 1);
    // One precise symbol of that name: its references are exact, wherever they are.
    let refs = symbols(&rig, wire::SymbolOp::Refs, "helper", |_| {}).await;
    assert_eq!(precise(&refs), vec![("src/main.rs".to_owned(), 2, false)]);
    // A position that is not on the definition names no symbol, so the tag results stand alone.
    let refs = symbols(&rig, wire::SymbolOp::Refs, "helper", |q| {
        q.def_path = Bytes::from_static(b"src/main.rs");
        q.def_line = 2;
    })
    .await;
    assert!(precise(&refs).is_empty());
    // A position on the definition does.
    let refs = symbols(&rig, wire::SymbolOp::Refs, "helper", |q| {
        q.def_path = Bytes::from_static(b"src/lib.rs");
        q.def_line = 1;
    })
    .await;
    assert_eq!(precise(&refs), vec![("src/main.rs".to_owned(), 2, false)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn precise_rows_stay_inside_the_scope_of_the_query() {
    let rig = rig().await;
    upload(&rig, WRITER, stream(&index())).await.unwrap();
    // A path filter that excludes the file, a kind filter and a name pattern never match.
    let out = symbols(&rig, wire::SymbolOp::Def, "helper", |q| {
        q.scope.as_mut().unwrap().exclude_paths = vec!["src".into()];
    })
    .await;
    assert!(precise(&out).is_empty(), "{out:?}");
    let out = symbols(&rig, wire::SymbolOp::Def, "helper", |q| {
        q.scope.as_mut().unwrap().paths = vec!["src/main.rs".into()];
    })
    .await;
    assert!(precise(&out).is_empty(), "{out:?}");
    let out = symbols(&rig, wire::SymbolOp::Def, "helper", |q| {
        q.kinds = vec!["function".into()];
    })
    .await;
    assert!(precise(&out).is_empty(), "{out:?}");
    let out = symbols(&rig, wire::SymbolOp::Def, "help*", |_| {}).await;
    assert!(precise(&out).is_empty(), "{out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_or_a_stranger_cannot_upload() {
    let rig = rig().await;
    let e = upload(&rig, READER, stream(&index())).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = upload(&rig, "stranger", stream(&index()))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    // Nothing was stored.
    let defs = symbols(&rig, wire::SymbolOp::Def, "helper", |_| {}).await;
    assert!(precise(&defs).is_empty());
    // A repository the writer has no grant on is refused, whatever the header says.
    let mut m = stream(&index());
    if let Some(wire::upload_scip_request::Part::Header(h)) = m[0].part.as_mut() {
        h.repo.as_mut().unwrap().name = OTHER.into();
    }
    let e = upload(&rig, WRITER, m).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stream_is_bounded_and_must_match_its_header() {
    let rig = rig().await;
    let idx = index();
    // No header first.
    let e = upload(&rig, WRITER, vec![data(&idx)]).await.unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    // A second header.
    let mut m = stream(&idx);
    m.push(header("master", "", idx.len()));
    assert_eq!(
        upload(&rig, WRITER, m).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // Declared size over the cap, and zero.
    let big = vec![header("master", "", MAX_INDEX_BYTES + 1)];
    assert_eq!(
        upload(&rig, WRITER, big).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
    let none = vec![header("master", "", 0)];
    assert_eq!(
        upload(&rig, WRITER, none).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // More data than declared, less than declared.
    let mut over = stream(&idx);
    over.push(data(b"x"));
    assert_eq!(
        upload(&rig, WRITER, over).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    let mut short = stream(&idx);
    short.pop();
    assert_eq!(
        upload(&rig, WRITER, short).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // One part over the part cap.
    let part = vec![0u8; MAX_UPLOAD_PART + 1];
    let m = vec![header("master", "", part.len()), data(&part)];
    assert_eq!(
        upload(&rig, WRITER, m).await.unwrap_err().code(),
        Code::InvalidArgument
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_content_and_bad_arguments_are_invalid_not_internal() {
    let rig = rig().await;
    // Not a SCIP index.
    let junk = vec![0xffu8; 64];
    let e = upload(&rig, WRITER, stream(&junk)).await.unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    // A document path that is not a repository path.
    let bad = WireIndex {
        documents: vec![doc("../etc/passwd", "", vec![])],
    }
    .encode_to_vec();
    let e = upload(&rig, WRITER, stream(&bad)).await.unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    // An unknown revision, and a root that leaves the repository.
    let idx = index();
    let mut m = stream(&idx);
    m[0] = header("no-such-rev", "", idx.len());
    assert_eq!(
        upload(&rig, WRITER, m).await.unwrap_err().code(),
        Code::NotFound
    );
    let mut m = stream(&idx);
    m[0] = header("master", "../x", idx.len());
    assert_eq!(
        upload(&rig, WRITER, m).await.unwrap_err().code(),
        Code::InvalidArgument
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_prefixes_the_document_paths() {
    let rig = rig().await;
    // The indexer ran in `src`: its documents are relative to it.
    let idx = WireIndex {
        documents: vec![doc("lib.rs", LIB, vec![occ(vec![0, 7, 13], 1)])],
    }
    .encode_to_vec();
    let mut m = stream(&idx);
    m[0] = header("master", "src", idx.len());
    let r = upload(&rig, WRITER, m).await.unwrap();
    assert_eq!((r.documents, r.unknown_path), (1, 0));
    let defs = symbols(&rig, wire::SymbolOp::Def, "helper", |_| {}).await;
    assert_eq!(precise(&defs), vec![("src/lib.rs".to_owned(), 1, true)]);
}
