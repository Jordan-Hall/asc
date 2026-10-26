//! A real `asc serve` on a loopback socket for end-to-end tests: the production `serve()` over a
//! file database and a file bucket, with `/mcp` and the indexing workers switched on as asked.
//! Commits are made in a local repository and pushed over the wire with the native push
//! (`PutObjects`, then `UpdateRefs`), exactly as `asc push` does.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_core::git::{ObjectKind, blob_oid};
use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_core::transport_policy::Policy;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_engine::history::MemBoundaries;
use asc_engine::sync::{PushMode, PushUpdate, SyncOptions};
use asc_rpc::auth::{Access, BearerToken, StaticTokens};
use asc_rpc::client::{RpcClient, channel_with};
use asc_rpc::server::RepoName;
use asc_serve::config::ServeConfig;
use asc_serve::migrate;
use asc_serve::registry::{NewRepo, Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Expect;
use serde_json::{Value, json};
use sqlx::AnyPool;

pub const WRITE_TOKEN: &str = "w-token-0123456789";
pub const READ_TOKEN: &str = "r-token-0123456789";
pub const REPO: &str = "acme/app";
pub const PUBLIC_URL: &str = "https://asc.example";

/// What the server under test switches on.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// `[mcp] enabled`.
    pub mcp: bool,
    /// `[codeintel] enabled` (with a cache disk for the workers).
    pub codeintel: bool,
}

pub struct Rig {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    /// The server's database, for looking at the queue and the outbox.
    pub pool: AnyPool,
    /// The server's repository id of [`REPO`].
    pub repo_id: i64,
    http: reqwest::Client,
    local: Arc<RepoEntry>,
    _local_registry: Arc<Registry>,
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

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap()
}

/// Starts a primary server holding the empty repository [`REPO`].
pub async fn start(o: Options) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = format!("sqlite:{}?mode=rwc", fwd(&dir.path().join("asc.db")));
    let store = format!(
        "file:///{}",
        fwd(&dir.path().join("bucket")).trim_start_matches('/')
    );
    std::fs::create_dir_all(dir.path().join("bucket")).unwrap();
    let pool = migrate::connect(&db, 2).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Registry::new(pool.clone(), &store).unwrap();
    let repo_id = registry
        .create(&RepoName::parse(REPO).unwrap(), &NewRepo::default())
        .await
        .unwrap()
        .key
        .0;

    let (port, admin) = (free_port(), free_port());
    let mut toml = format!(
        "listen = \"127.0.0.1:{port}\"\npublic_url = \"{PUBLIC_URL}\"\n\
         store = \"{store}\"\ndb = \"{db}\"\n\
         [admin]\nlisten = \"127.0.0.1:{admin}\"\n\
         [cache]\ndisk_dir = \"{}\"\n",
        fwd(&dir.path().join("cache")),
    );
    if o.mcp {
        toml.push_str("[mcp]\nenabled = true\n");
    }
    if o.codeintel {
        toml.push_str("[codeintel]\nenabled = true\nworkers = 2\n");
    }
    let cfg = ServeConfig::parse(&toml).unwrap();
    let auth = Arc::new(
        StaticTokens::new()
            .with_token(WRITE_TOKEN, "alice", Access::Write)
            .with_token(READ_TOKEN, "bob", Access::Read),
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
    let end = Instant::now() + Duration::from_secs(60);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err() {
        assert!(Instant::now() < end, "asc serve never listened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The pusher's own repository: in memory, with no server in it.
    let lpool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&lpool).await.unwrap();
    let local_registry = Arc::new(Registry::new(lpool, "memory:///").unwrap());
    let local = local_registry
        .create(&RepoName::parse("local/app").unwrap(), &NewRepo::default())
        .await
        .unwrap();
    Rig {
        base: format!("http://{addr}"),
        pool,
        repo_id,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
        local,
        _local_registry: local_registry,
        _dir: dir,
        stop: Some(stop),
    }
}

impl Rig {
    pub fn client(&self, repo: &str, token: &str) -> RpcClient {
        let channel = channel_with(&self.base, Policy::LOOPBACK_HTTP).unwrap();
        RpcClient::from_channel(channel, repo, Some(BearerToken::new(token).unwrap())).unwrap()
    }

    /// Commits `files` on top of `parent` in the local repository.
    pub async fn commit(&self, parent: Option<ChangesetId>, files: &[(&str, &str)]) -> ChangesetId {
        let e = &self.local;
        let engine = e.backend.engine();
        let base = match parent {
            Some(p) => engine.get_changeset(p).await.unwrap().root,
            None => asc_engine::Engine::<
                asc_store::bucket::BucketObjects,
                asc_store::sqlrefs::SqlRefs,
            >::empty_tree_id(),
        };
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
        let root = engine.apply(base, &changes).await.unwrap();
        let cs = engine
            .create_changeset(
                NewChangeset {
                    parents: parent.into_iter().collect(),
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

    /// Pushes `new` to `refs/heads/master` of the server's [`REPO`] with the native push, after
    /// `old` (the value the server holds, `None` for a first push).
    pub async fn push(&self, old: Option<ChangesetId>, new: ChangesetId) {
        let remote = self.client(REPO, WRITE_TOKEN);
        let boundaries = MemBoundaries::default();
        let opts = SyncOptions::new(&boundaries);
        let update = PushUpdate {
            dst: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
            new: Some(RefTarget::Changeset(new)),
            expect: old.map_or(Expect::Absent, |o| Expect::Value(RefTarget::Changeset(o))),
        };
        self.local
            .backend
            .engine()
            .push(&remote, &[update], PushMode::FastForward, &opts)
            .await
            .expect("push to the running server");
    }

    /// POST `/mcp`; returns the status, the headers and the JSON body (`Null` when it has none).
    pub async fn mcp(
        &self,
        token: Option<&str>,
        body: &Value,
    ) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Value) {
        self.mcp_with(token, &[], body).await
    }

    /// As [`mcp`](Self::mcp), with more request headers.
    pub async fn mcp_with(
        &self,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Value) {
        let mut req = self
            .http
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body.to_string());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.bytes().await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, json)
    }

    /// A plain GET of a server path.
    pub async fn get(&self, path: &str) -> (reqwest::StatusCode, Value) {
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.bytes().await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// A JSON-RPC `tools/call` of `name` as the write token's owner.
    pub async fn call_tool(&self, token: &str, name: &str, args: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                           "params": { "name": name, "arguments": args } });
        let (status, _, v) = self.mcp(Some(token), &body).await;
        assert_eq!(status, 200, "{v}");
        v
    }
}

/// Polls `f` until it holds, for at most `secs` seconds.
pub async fn until<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
