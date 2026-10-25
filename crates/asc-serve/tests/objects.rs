//! The primary `asc serve` answers the objects group (`HasObjects`, `GetObjects`, `PutObjects`)
//! for every repository of its registry: the production `serve` wiring, a loopback socket and
//! the `asc-rpc` client, so a clone, fetch and push have the object transport they need.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use asc_core::id::FileId;
use asc_rpc::auth::{Access, BearerToken, StaticTokens};
use asc_rpc::client::{CallError, RpcClient, channel_with};
use asc_rpc::server::RepoName;
use asc_core::transport_policy::Policy;
use asc_serve::config::ServeConfig;
use asc_serve::migrate;
use asc_serve::registry::{NewRepo, Registry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use tonic::Code;

const WRITE: &str = "w-token";
const READ: &str = "r-token";
const REPO: &str = "acme/web";

struct Rig {
    url: String,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fwd(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// A primary `asc serve` over a file database and a file bucket, holding the empty repository
/// `acme/web`.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = format!("sqlite:{}?mode=rwc", fwd(&dir.path().join("asc.db")));
    let store = format!("file:///{}", fwd(&dir.path().join("bucket")).trim_start_matches('/'));
    std::fs::create_dir_all(dir.path().join("bucket")).unwrap();
    {
        let pool = migrate::connect(&db, 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Registry::new(pool, &store).unwrap();
        registry
            .create(&RepoName::parse(REPO).unwrap(), &NewRepo::default())
            .await
            .unwrap();
    }
    let (port, admin) = (free_port(), free_port());
    // Each rig has its own admin listener: the default port is one per machine.
    let cfg = ServeConfig::parse(&format!(
        "listen = \"127.0.0.1:{port}\"\npublic_url = \"https://asc.example\"\n\
         store = \"{store}\"\ndb = \"{db}\"\n\
         [admin]\nlisten = \"127.0.0.1:{admin}\"\n"
    ))
    .unwrap();
    let auth = Arc::new(
        StaticTokens::new()
            .with_token(WRITE, "alice", Access::Write)
            .with_token(READ, "bob", Access::Read),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        if let Err(e) = asc_serve::serve(cfg, auth, async {
            let _ = stopped.await;
        })
        .await
        {
            eprintln!("asc serve stopped: {e}");
        }
    });
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let end = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err() {
        assert!(Instant::now() < end, "asc serve never listened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Rig {
        url: format!("http://{addr}"),
        _dir: dir,
        stop: Some(stop),
    }
}

fn client(rig: &Rig, repo: &str, token: &str) -> RpcClient {
    let channel = channel_with(&rig.url, Policy::LOOPBACK_HTTP).unwrap();
    RpcClient::from_channel(channel, repo, Some(BearerToken::new(token).unwrap())).unwrap()
}

fn code(e: &CallError) -> Code {
    match e {
        CallError::Status { code, .. } => *code,
        other => panic!("not a plain status: {other:?}"),
    }
}

fn file(body: &[u8]) -> (ObjKey, Bytes) {
    (ObjKey::File(FileId::of(body)), encode_stored(body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_primary_server_stores_and_serves_objects() {
    let rig = rig().await;
    let c = client(&rig, REPO, WRITE);
    let (k1, v1) = file(b"hello\n");
    let (k2, v2) = file(b"world\n");
    let (absent, _) = file(b"never uploaded\n");

    assert_eq!(c.has_objects(&[k1, k2]).await.unwrap(), [false, false]);
    c.put_objects(vec![(k1, v1.clone()), (k2, v2.clone())])
        .await
        .expect("PutObjects is served");
    assert_eq!(
        c.has_objects(&[k1, absent, k2]).await.unwrap(),
        [true, false, true]
    );
    // The stored bytes come back as they went in; an absent key is reported, not an error.
    let objects = c.objects();
    assert_eq!(objects.get_stored(&k1).await.unwrap(), Some(v1));
    assert_eq!(objects.get_stored(&k2).await.unwrap(), Some(v2));
    assert_eq!(objects.get_stored(&absent).await.unwrap(), None);
    // A second upload of the same bytes is accepted.
    c.put_objects(vec![file(b"hello\n")]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn objects_follow_the_authorizer_and_the_namespace() {
    let rig = rig().await;
    let writer = client(&rig, REPO, WRITE);
    let (key, value) = file(b"secret\n");
    writer.put_objects(vec![(key, value.clone())]).await.unwrap();

    // A read-only token reads but cannot upload.
    let reader = client(&rig, REPO, READ);
    assert_eq!(reader.has_objects(&[key]).await.unwrap(), [true]);
    assert_eq!(reader.objects().get_stored(&key).await.unwrap(), Some(value));
    let denied = reader
        .put_objects(vec![file(b"read-only upload\n")])
        .await
        .unwrap_err();
    assert_eq!(code(&denied), Code::PermissionDenied, "{denied:?}");

    // No token at all is refused.
    let channel = channel_with(&rig.url, Policy::LOOPBACK_HTTP).unwrap();
    let anon = RpcClient::from_channel::<BearerToken>(channel, REPO, None).unwrap();
    let e = anon.has_objects(&[key]).await.unwrap_err();
    assert_eq!(code(&e), Code::Unauthenticated, "{e:?}");

    // An unknown repository is not found, and nothing leaks across repositories.
    let other = client(&rig, "acme/other", WRITE);
    let e = other.has_objects(&[key]).await.unwrap_err();
    assert_eq!(code(&e), Code::NotFound, "{e:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreferenced_upload_moves_no_ref() {
    let rig = rig().await;
    let c = client(&rig, REPO, WRITE);
    c.put_objects(vec![file(b"orphan\n")]).await.unwrap();
    assert!(c.list_refs_all().await.unwrap().is_empty());
}
