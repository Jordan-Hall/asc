//! gRPC-Web and Connect unary against a running server (docs/WEB-UI.md section 4.1).
//!
//! A real tonic server on loopback answers `fetch`-style requests: one unary RPC, one server
//! streaming RPC, the credential edge (no token, bad token), CORS (own origin, configured origin,
//! stranger, preflight), the request cap, and the switch that turns the whole thing off.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use asc_core::git::{ObjectKind, blob_oid};
use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::server::{RepoName, ServerInfo, wire};
use asc_serve::config::{ByteSize, Limits, WebConfig};
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::migrate;
use asc_serve::registry::{Extensions, Registry};
use asc_serve::{read, web};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use prost::Message;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

const TOKEN: &str = "web-secret-token";
const REPO: &str = "o/web";
const APP: &str = "https://app.example";

struct Rig {
    base: String,
    host: String,
    http: reqwest::Client,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

/// One changeset holding `hello.txt`, as `refs/heads/master`.
async fn seed(registry: &Registry) {
    let e = registry
        .create(&RepoName::parse(REPO).unwrap(), &Default::default())
        .await
        .unwrap();
    let engine = e.backend.engine();
    let body = b"hello\n".to_vec();
    let id = FileId::of(&body);
    e.objects
        .put(&ObjKey::File(id), encode_stored(&body))
        .await
        .unwrap();
    let change = Change::write(Kind::File, id, body.len() as u64, blob_oid(&body), None).unwrap();
    let base = asc_engine::Engine::<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>::empty_tree_id();
    let root = engine
        .apply(base, &[(RepoPath::new("hello.txt").unwrap(), change)])
        .await
        .unwrap();
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
    set_master(registry, e.key, cs).await;
}

async fn set_master(r: &Registry, key: asc_store::refs::RepoKey, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    r.refs().update(key, &txn).await.unwrap();
}

async fn rig(web_cfg: WebConfig) -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    seed(&registry).await;

    let ext = Extensions::new();
    read::register(&ext, &registry);
    let auth = Arc::new(StaticTokens::new().with_token(TOKEN, "web", Access::Write));
    let admission = Arc::new(Admission::new(&Limits::default()));
    let routes = guarded_routes(&ext, auth, admission, ServerInfo::default());
    let routes = web::grpc_web::wrap(routes, &web_cfg, None);

    let incoming = TcpIncoming::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let addr = incoming.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .accept_http1(true)
            .add_routes(routes)
            .serve_with_incoming_shutdown(incoming, async {
                let _ = stopped.await;
            })
            .await
    });
    Rig {
        base: format!("http://{addr}"),
        host: addr.to_string(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap(),
        stop: Some(stop),
    }
}

fn enabled() -> WebConfig {
    WebConfig {
        enabled: true,
        cors_origins: vec![APP.to_owned()],
        ..WebConfig::default()
    }
}

fn frame(flag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![flag];
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// `(flag, payload)` of each frame in a gRPC-Web body.
fn frames(mut body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let len = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        out.push((body[0], body[5..5 + len].to_vec()));
        body = &body[5 + len..];
    }
    out
}

/// The `grpc-status` in the closing trailer frame.
fn status(frames: &[(u8, Vec<u8>)]) -> i32 {
    let (flag, block) = frames.last().expect("a trailer frame");
    assert_eq!(*flag, 0x80, "last frame is the trailer frame");
    String::from_utf8_lossy(block)
        .lines()
        .find_map(|l| l.strip_prefix("grpc-status:"))
        .expect("grpc-status")
        .trim()
        .parse()
        .unwrap()
}

fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: REPO.to_owned(),
        id_mode: 0,
    })
}

impl Rig {
    fn post(&self, method: &str, content_type: &str, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/{method}", self.base))
            .header("content-type", content_type)
            .header("x-grpc-web", "1")
            .body(body)
    }

    fn web(&self, method: &str, msg: &impl Message) -> reqwest::RequestBuilder {
        self.post(
            method,
            "application/grpc-web+proto",
            frame(0, &msg.encode_to_vec()),
        )
    }

    async fn call(
        req: reqwest::RequestBuilder,
    ) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>) {
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        (status, headers, res.bytes().await.unwrap().to_vec())
    }
}

fn list_refs() -> wire::ResolveRequest {
    wire::ResolveRequest {
        repo: repo_ref(),
        revspecs: vec!["master".to_owned()],
    }
}

#[tokio::test]
async fn unary_call_with_a_bearer_token() {
    let rig = rig(enabled()).await;
    let (status_code, headers, body) = Rig::call(
        rig.web("asc.v1.Repo/Resolve", &list_refs())
            .bearer_auth(TOKEN),
    )
    .await;
    assert_eq!(status_code, 200);
    assert_eq!(headers["content-type"], "application/grpc-web+proto");
    assert!(headers.get("set-cookie").is_none());
    let f = frames(&body);
    assert_eq!(f.len(), 2, "one message and the trailers");
    assert_eq!(f[0].0, 0);
    let reply = wire::ResolveResponse::decode(f[0].1.as_slice()).unwrap();
    assert_eq!(reply.ids.len(), 1);
    assert_eq!(reply.ids[0].len(), 32, "master resolves to a changeset id");
    assert_eq!(status(&f), 0);
}

#[tokio::test]
async fn server_streaming_call_delivers_messages_then_the_status() {
    let rig = rig(enabled()).await;
    let req = wire::LogRequest {
        repo: repo_ref(),
        include: vec!["master".to_owned()],
        ..Default::default()
    };
    let (status_code, _, body) =
        Rig::call(rig.web("asc.v1.Repo/Log", &req).bearer_auth(TOKEN)).await;
    assert_eq!(status_code, 200);
    let f = frames(&body);
    assert!(f.len() >= 2, "at least one message and the trailers");
    let entries: usize = f[..f.len() - 1]
        .iter()
        .map(|(flag, m)| {
            assert_eq!(*flag, 0);
            wire::LogResponse::decode(m.as_slice())
                .unwrap()
                .entries
                .len()
        })
        .sum();
    assert_eq!(entries, 1);
    assert_eq!(status(&f), 0);
}

#[tokio::test]
async fn a_call_without_a_valid_token_is_unauthenticated() {
    let rig = rig(enabled()).await;
    let (code, _, body) = Rig::call(rig.web("asc.v1.Repo/Resolve", &list_refs())).await;
    assert_eq!(code, 200, "gRPC-Web carries the status in the trailers");
    assert_eq!(status(&frames(&body)), 16);

    let (_, _, body) = Rig::call(
        rig.web("asc.v1.Repo/Resolve", &list_refs())
            .bearer_auth("not-a-token"),
    )
    .await;
    assert_eq!(status(&frames(&body)), 16);

    // A cookie is no credential.
    let (_, _, body) = Rig::call(
        rig.web("asc.v1.Repo/Resolve", &list_refs())
            .header("cookie", format!("authorization=Bearer {TOKEN}")),
    )
    .await;
    assert_eq!(status(&frames(&body)), 16);

    // The streaming path refuses too, before any message.
    let (_, _, body) = Rig::call(rig.web(
        "asc.v1.Repo/Log",
        &wire::LogRequest {
            repo: repo_ref(),
            ..Default::default()
        },
    ))
    .await;
    let f = frames(&body);
    assert_eq!(f.len(), 1);
    assert_eq!(status(&f), 16);
}

#[tokio::test]
async fn cross_origin_calls_follow_the_configured_origins() {
    let rig = rig(enabled()).await;
    let call = |origin: &'static str| {
        rig.web("asc.v1.Repo/Resolve", &list_refs())
            .bearer_auth(TOKEN)
            .header("origin", origin)
    };

    // Not configured: refused with no CORS headers, and the service never ran.
    let (code, headers, body) = Rig::call(call("https://evil.example")).await;
    assert_eq!(code, 403);
    assert!(headers.get("access-control-allow-origin").is_none());
    assert_eq!(body, b"origin not allowed");

    // Configured: echoed, no credentials.
    let (code, headers, _) = Rig::call(call(APP)).await;
    assert_eq!(code, 200);
    assert_eq!(headers["access-control-allow-origin"], APP);
    assert!(headers.get("access-control-allow-credentials").is_none());

    // The server's own origin needs no CORS headers.
    let own = format!("http://{}", rig.host);
    let (code, headers, _) = Rig::call(
        rig.web("asc.v1.Repo/Resolve", &list_refs())
            .bearer_auth(TOKEN)
            .header("origin", own),
    )
    .await;
    assert_eq!(code, 200);
    assert!(headers.get("access-control-allow-origin").is_none());

    // Preflight.
    let pre = |origin: &str| {
        rig.http
            .request(
                reqwest::Method::OPTIONS,
                format!("{}/asc.v1.Repo/Resolve", rig.base),
            )
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type",
            )
    };
    let (code, headers, _) = Rig::call(pre(APP)).await;
    assert_eq!(code, 204);
    assert_eq!(headers["access-control-allow-origin"], APP);
    assert!(
        headers["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .contains("authorization")
    );
    let (code, headers, _) = Rig::call(pre("https://evil.example")).await;
    assert_eq!(code, 403);
    assert!(headers.get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn an_oversized_request_is_refused_with_resource_exhausted() {
    let rig = rig(WebConfig {
        max_request: ByteSize(64),
        ..enabled()
    })
    .await;
    let big = wire::ResolveRequest {
        repo: repo_ref(),
        revspecs: vec!["x".repeat(4096)],
    };
    let (code, _, body) = Rig::call(rig.web("asc.v1.Repo/Resolve", &big).bearer_auth(TOKEN)).await;
    assert_eq!(code, 200);
    assert_eq!(status(&frames(&body)), 8);
}

#[tokio::test]
async fn connect_unary_returns_a_bare_message_or_a_json_error() {
    let rig = rig(enabled()).await;
    let (code, headers, body) = Rig::call(
        rig.post(
            "asc.v1.Repo/Resolve",
            "application/proto",
            list_refs().encode_to_vec(),
        )
        .bearer_auth(TOKEN),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(headers["content-type"], "application/proto");
    let reply = wire::ResolveResponse::decode(body.as_slice()).unwrap();
    assert_eq!(reply.ids.len(), 1);

    let (code, headers, body) = Rig::call(rig.post(
        "asc.v1.Repo/Resolve",
        "application/proto",
        list_refs().encode_to_vec(),
    ))
    .await;
    assert_eq!(code, 401);
    assert_eq!(headers["content-type"], "application/json");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "unauthenticated");
}

#[tokio::test]
async fn the_rpc_paths_stay_native_when_grpc_web_is_off() {
    let rig = rig(WebConfig::default()).await;
    let (_, headers, _) = Rig::call(
        rig.web("asc.v1.Repo/Resolve", &list_refs())
            .bearer_auth(TOKEN),
    )
    .await;
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(!ct.starts_with("application/grpc-web"), "{ct}");

    // `[web] enabled` implies it; `grpc_web = false` opts out.
    assert!(enabled().grpc_web_enabled());
    assert!(
        !WebConfig {
            grpc_web: Some(false),
            ..enabled()
        }
        .grpc_web_enabled()
    );
}
