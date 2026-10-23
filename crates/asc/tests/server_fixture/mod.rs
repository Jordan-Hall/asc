//! An in-process asc server for the U50 tests: the real `asc-serve` handlers over an in-memory
//! database and object store, behind `StaticTokens`, on a loopback port. It runs on its own
//! runtime thread so tests can run the built `asc` binary against it. Every RPC path it serves is
//! recorded, which is how a test shows where a call went.
#![allow(dead_code)] // Each test crate uses a different part.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;

use async_trait::async_trait;
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::server::{CallCtx, ObjectsHandler, RepoName, RepoService, RpcStream, ServerInfo, wire};
use asc_serve::registry::{Extensions, NewRepo, Registry};
use asc_serve::{migrate, read, update};
use tonic::Status;
use tonic::codegen::{Service, http};
use tonic::server::NamedService;

/// `HasObjects`, `GetObjects` and `PutObjects` for every repository of a registry, through the
/// repository's own engine backend. (`asc serve` registers no primary object group yet; the
/// handler lives here so the client side is tested end to end.)
struct RegistryObjects(Arc<Registry>);

impl RegistryObjects {
    async fn entry(&self, ctx: &CallCtx) -> Result<Arc<asc_serve::registry::RepoEntry>, Status> {
        self.0
            .open(&ctx.repo)
            .await
            .map_err(|_| Status::not_found("no such repository"))
    }
}

#[async_trait]
impl ObjectsHandler for RegistryObjects {
    async fn has_objects(
        &self,
        ctx: &CallCtx,
        req: wire::HasObjectsRequest,
    ) -> Result<wire::HasObjectsResponse, Status> {
        self.entry(ctx).await?.backend.has_objects(ctx, req).await
    }

    async fn put_objects(
        &self,
        ctx: &CallCtx,
        req: wire::PutObjectsRequest,
    ) -> Result<wire::PutObjectsResponse, Status> {
        self.entry(ctx).await?.backend.put_objects(ctx, req).await
    }

    async fn get_objects(
        &self,
        ctx: &CallCtx,
        req: wire::GetObjectsRequest,
    ) -> Result<RpcStream<wire::GetObjectsResponse>, Status> {
        self.entry(ctx).await?.backend.get_objects(ctx, req).await
    }
}

/// The write token (`alice`, may push) and the read-only token (`bob`).
pub const WRITE: &str = "w-token";
pub const READ: &str = "r-token";

/// Records the path of every call, then forwards it.
#[derive(Clone)]
struct Counting<S> {
    inner: S,
    log: Arc<Mutex<Vec<String>>>,
}

impl<S, B> Service<http::Request<B>> for Counting<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        if let Ok(mut log) = self.log.lock() {
            log.push(req.uri().path().to_owned());
        }
        self.inner.call(req)
    }
}

impl<S: NamedService> NamedService for Counting<S> {
    const NAME: &'static str = S::NAME;
}

/// A running server; dropping it stops it.
pub struct Server {
    pub addr: SocketAddr,
    calls: Arc<Mutex<Vec<String>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// A server holding the empty primary repositories `repos` (`owner/name`).
    pub fn start(repos: &[&str]) -> Self {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let repos: Vec<String> = repos.iter().map(|r| (*r).to_owned()).collect();
        let log = calls.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let pool = migrate::connect("sqlite::memory:", 1).await.expect("db");
                migrate::migrate(&pool).await.expect("migrate");
                let registry = Arc::new(Registry::new(pool, "memory:///").expect("registry"));
                for r in &repos {
                    registry
                        .create(&RepoName::parse(r).expect("repo name"), &NewRepo::default())
                        .await
                        .expect("create repo");
                }
                let ext = Extensions::new();
                update::register(&ext, &registry);
                assert!(
                    ext.handlers
                        .set_objects(Arc::new(RegistryObjects(registry.clone())))
                        .is_ok(),
                    "one objects group"
                );
                let _hooks = read::register(&ext, &registry);
                let _warmer = asc_serve::projects::register(&ext, &registry);
                let auth = Arc::new(
                    StaticTokens::new()
                        .with_token(WRITE, "alice", Access::Write)
                        .with_token(READ, "bob", Access::Read),
                );
                let projects = Counting {
                    inner: asc_rpc::projects::ProjectsService::new(
                        ext.projects.get().expect("projects handler").clone(),
                        auth.clone(),
                        ServerInfo::default(),
                    )
                    .into_service(),
                    log: log.clone(),
                };
                let svc = Counting {
                    inner: RepoService::new(ext.handlers.clone(), auth, ServerInfo::default())
                        .into_service(),
                    log,
                };
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                addr_tx
                    .send(listener.local_addr().expect("addr"))
                    .expect("send");
                let incoming = futures::stream::unfold(listener, |l| async move {
                    let r = l.accept().await.map(|(s, _)| s);
                    Some((r, l))
                });
                let _ = tonic::transport::Server::builder()
                    .add_service(svc)
                    .add_service(projects)
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        let addr = addr_rx.recv().expect("the server started");
        Self {
            addr,
            calls,
            shutdown: Some(stop_tx),
            thread: Some(thread),
        }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `asc+http://127.0.0.1:<port>/<repo>`.
    pub fn url(&self, repo: &str) -> String {
        format!("asc+http://{}/{repo}", self.addr)
    }

    /// How many calls reached `rpc` (`Log`, `GetObjects`, ...).
    pub fn count(&self, rpc: &str) -> usize {
        let suffix = format!("/{rpc}");
        self.calls
            .lock()
            .map_or(0, |l| l.iter().filter(|p| p.ends_with(&suffix)).count())
    }

    /// Every path served so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|l| l.clone()).unwrap_or_default()
    }

    pub fn forget_calls(&self) {
        if let Ok(mut l) = self.calls.lock() {
            l.clear();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A one-route HTTP server that answers `GET /.well-known/asc` with `body` and everything else
/// with 404: a discovery document for the tests.
pub struct WellKnown {
    pub addr: SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WellKnown {
    pub fn start(body: String) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        conn.set_nonblocking(false).ok();
                        conn.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .ok();
                        let mut buf = [0u8; 4096];
                        let n = conn.read(&mut buf).unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let reply = if head.starts_with("GET /.well-known/asc ") {
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        } else {
                            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                                .to_owned()
                        };
                        let _ = conn.write_all(reply.as_bytes());
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self, repo: &str) -> String {
        format!("http://{}/{repo}", self.addr)
    }
}

impl Drop for WellKnown {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
