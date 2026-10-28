//! C24 acceptance: `asc submit` against an in-process `asc serve` whose `UpdateRefs` answers
//! AGit pushes the way the Forgejo fork will (C34) and whose `Changes` service is a small fake.
//! Everything else on the server is the real handler stack over an in-memory database, so the
//! closure upload is real. The client is the built `asc` binary.
//!
//! What is shown: one `UpdateRefs` per submit with the `refs/for/<trunk>/asc-<change>` refs
//! bottom to top; the push options; the refusals that must happen before anything is sent (bad
//! reviewer, needs-restack, divergent, landed, `--update-only` without PRs, a plain git
//! remote); and that server text is printed without control characters.
#![cfg(feature = "serve")]

mod support;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;

use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{
    CallCtx, ObjectsHandler, RefsHandler, RepoName, RepoService, RpcStream, ServerInfo, rpc_status,
    wire,
};
use asc_rpc::services::changes_server::{Changes, ChangesServer};
use asc_serve::registry::{Extensions, NewRepo, Registry};
use asc_serve::update::ServerRefs;
use asc_serve::{migrate, read};
use async_trait::async_trait;
use serde_json::Value;
use support::{Sandbox, text};
use tonic::codegen::{Service, http};
use tonic::server::NamedService;
use tonic::{Request, Response, Status};

const WRITE: &str = "w-token";
const READ: &str = "r-token";

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

// ------------------------------------------------------------------------------ the server

/// What the fake server records and is told to do.
#[derive(Default)]
struct State {
    /// Every `UpdateRefs` that carried a `refs/for/` ref.
    updates: Mutex<Vec<wire::UpdateRefsRequest>>,
    /// Change id (the 32 letter form) to its PR: number, url, state.
    reviews: Mutex<HashMap<String, (u64, String, String)>>,
    /// Change ids the server says have landed.
    landed: Mutex<HashSet<String>>,
    /// `ResolveChanges` answers UNIMPLEMENTED.
    no_changes: AtomicBool,
    /// The message the AGit answer carries.
    message: Mutex<String>,
    /// The AGit answer is this error.
    reject: Mutex<Option<RpcError>>,
    /// Every RPC path served, in order.
    calls: Mutex<Vec<String>>,
}

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

/// The real refs handler, except that an AGit push is recorded and answered here.
struct Refs {
    real: ServerRefs,
    st: Arc<State>,
}

#[async_trait]
impl RefsHandler for Refs {
    async fn list_refs(
        &self,
        ctx: &CallCtx,
        req: wire::ListRefsRequest,
    ) -> Result<wire::ListRefsResponse, Status> {
        self.real.list_refs(ctx, req).await
    }

    async fn update_refs(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        if !req.updates.iter().any(|u| u.name.starts_with("refs/for/")) {
            return self.real.update_refs(ctx, req).await;
        }
        self.st.updates.lock().unwrap().push(req.clone());
        if let Some(e) = self.st.reject.lock().unwrap().clone() {
            return Err(rpc_status(&e));
        }
        let results = req
            .updates
            .iter()
            .enumerate()
            .map(|(i, u)| wire::RefResult {
                name: format!("refs/pull/{}/head", i + 1),
                old: None,
                new: u.new.clone(),
                user: false,
            })
            .collect();
        Ok(wire::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: self.st.message.lock().unwrap().clone(),
        })
    }
}

struct FakeChanges(Arc<State>);

fn letters(change: &[u8]) -> String {
    let id: [u8; 16] = change.try_into().expect("16 byte change id");
    asc_core::change::ChangeId::from_bytes(id).to_string()
}

#[async_trait]
impl Changes for FakeChanges {
    async fn resolve_changes(
        &self,
        request: Request<wire::ResolveChangesRequest>,
    ) -> Result<Response<wire::ResolveChangesResponse>, Status> {
        if self.0.no_changes.load(Ordering::SeqCst) {
            return Err(Status::unimplemented("no Changes service"));
        }
        let reviews = self.0.reviews.lock().unwrap();
        let landed = self.0.landed.lock().unwrap();
        let mut changes = Vec::new();
        for c in &request.get_ref().changes {
            let key = letters(c);
            let review = reviews.get(&key).map(|(n, url, state)| wire::ReviewState {
                number: *n,
                url: url.clone(),
                state: state.clone(),
                ..wire::ReviewState::default()
            });
            let is_landed = landed.contains(&key);
            if review.is_none() && !is_landed {
                continue;
            }
            changes.push(wire::ChangeInfo {
                change: c.clone(),
                head: vec![7u8; 32].into(),
                landed_as: if is_landed {
                    vec![9u8; 32].into()
                } else {
                    Default::default()
                },
                review,
                version: 1,
                ..wire::ChangeInfo::default()
            });
        }
        Ok(Response::new(wire::ResolveChangesResponse { changes }))
    }

    async fn stack(
        &self,
        _: Request<wire::StackRequest>,
    ) -> Result<Response<wire::StackResponse>, Status> {
        Err(Status::unimplemented("stack"))
    }

    async fn versions(
        &self,
        _: Request<wire::VersionsRequest>,
    ) -> Result<Response<wire::VersionsResponse>, Status> {
        Err(Status::unimplemented("versions"))
    }

    type InterDiffStream = futures::stream::BoxStream<'static, Result<wire::DiffResponse, Status>>;

    async fn inter_diff(
        &self,
        _: Request<wire::InterDiffRequest>,
    ) -> Result<Response<Self::InterDiffStream>, Status> {
        Err(Status::unimplemented("inter_diff"))
    }

    async fn mergeability(
        &self,
        _: Request<wire::MergeabilityRequest>,
    ) -> Result<Response<wire::MergeabilityResponse>, Status> {
        Err(Status::unimplemented("mergeability"))
    }

    async fn overlaps(
        &self,
        _: Request<wire::OverlapsRequest>,
    ) -> Result<Response<wire::OverlapsResponse>, Status> {
        Err(Status::unimplemented("overlaps"))
    }

    async fn restack_changes(
        &self,
        _: Request<wire::RestackChangesRequest>,
    ) -> Result<Response<wire::RestackChangesResponse>, Status> {
        Err(Status::unimplemented("restack_changes"))
    }

    async fn set_review_state(
        &self,
        _: Request<wire::SetReviewStateRequest>,
    ) -> Result<Response<wire::SetReviewStateResponse>, Status> {
        Err(Status::unimplemented("set_review_state"))
    }

    async fn request_merge(
        &self,
        _: Request<wire::RequestMergeRequest>,
    ) -> Result<Response<wire::RequestMergeResponse>, Status> {
        Err(Status::unimplemented("request_merge"))
    }
}

/// Records the path of every call, then forwards it.
#[derive(Clone)]
struct Counting<S> {
    inner: S,
    st: Arc<State>,
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
        if let Ok(mut log) = self.st.calls.lock() {
            log.push(req.uri().path().to_owned());
        }
        self.inner.call(req)
    }
}

impl<S: NamedService> NamedService for Counting<S> {
    const NAME: &'static str = S::NAME;
}

struct Server {
    addr: SocketAddr,
    st: Arc<State>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let st = Arc::new(State::default());
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let state = st.clone();
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
                registry
                    .create(
                        &RepoName::parse("acme/app").expect("repo name"),
                        &NewRepo::default(),
                    )
                    .await
                    .expect("create repo");
                let ext = Extensions::new();
                let refs = Refs {
                    real: ServerRefs::new(registry.clone(), ext.pipeline.clone()),
                    st: state.clone(),
                };
                assert!(ext.handlers.set_refs(Arc::new(refs)).is_ok());
                assert!(
                    ext.handlers
                        .set_objects(Arc::new(RegistryObjects(registry.clone())))
                        .is_ok()
                );
                let _hooks = read::register(&ext, &registry);
                let auth = Arc::new(
                    StaticTokens::new()
                        .with_token(WRITE, "alice", Access::Write)
                        .with_token(READ, "bob", Access::Read),
                );
                let repo = Counting {
                    inner: RepoService::new(ext.handlers.clone(), auth, ServerInfo::default())
                        .into_service(),
                    st: state.clone(),
                };
                let changes = Counting {
                    inner: ChangesServer::new(FakeChanges(state.clone())),
                    st: state.clone(),
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
                    .add_service(repo)
                    .add_service(changes)
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        let addr = addr_rx.recv().expect("the server started");
        Self {
            addr,
            st,
            shutdown: Some(stop_tx),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("asc+http://{}/acme/app", self.addr)
    }

    fn count(&self, rpc: &str) -> usize {
        let suffix = format!("/{rpc}");
        self.st
            .calls
            .lock()
            .map_or(0, |l| l.iter().filter(|p| p.ends_with(&suffix)).count())
    }

    fn forget(&self) {
        self.st.calls.lock().unwrap().clear();
        self.st.updates.lock().unwrap().clear();
    }

    /// The AGit requests the server saw.
    fn agit(&self) -> Vec<wire::UpdateRefsRequest> {
        self.st.updates.lock().unwrap().clone()
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

// ------------------------------------------------------------------------------ the client

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn asc_as(sb: &Sandbox, cwd: &Path, token: &str, args: &[&str]) -> Run {
    let out: Output = sb
        .asc(cwd)
        .envs(IDENT)
        .env("ASC_TOKEN", token)
        .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .expect("spawn asc");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Run {
    asc_as(sb, cwd, WRITE, args)
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let r = asc(sb, cwd, args);
    assert_eq!(r.code, 0, "asc {args:?}: {}", r.stderr);
    r.stdout
}

fn commit(sb: &Sandbox, repo: &Path, name: &str) {
    std::fs::write(repo.join(name), format!("{name}\n")).unwrap();
    ok(sb, repo, &["add", name]);
    ok(sb, repo, &["commit", "-q", "-m", name]);
}

/// One change of a stack as `asc sl --json` shows it.
#[derive(Clone, Debug)]
struct Ch {
    change: String,
    short: String,
    commit: String,
}

struct Fx {
    sb: Sandbox,
    server: Server,
    repo: PathBuf,
}

impl Fx {
    /// `origin` is the server and `master` holds `base`, pushed.
    fn new() -> Self {
        let sb = Sandbox::new();
        let server = Server::start();
        let repo = sb.dir("a");
        ok(&sb, &repo, &["init", "-q"]);
        ok(&sb, &repo, &["remote", "add", "origin", &server.url()]);
        commit(&sb, &repo, "base");
        ok(&sb, &repo, &["push", "origin", "master"]);
        Self { sb, server, repo }
    }

    /// `n` more commits on `master`, named `c1`, `c2`, ...
    fn stack(&self, n: usize) -> Vec<Ch> {
        for i in 1..=n {
            commit(&self.sb, &self.repo, &format!("c{i}"));
        }
        self.changes()
    }

    fn asc(&self, args: &[&str]) -> Run {
        self.asc_as(WRITE, args)
    }

    fn asc_as(&self, token: &str, args: &[&str]) -> Run {
        asc_as(&self.sb, &self.repo, token, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        ok(&self.sb, &self.repo, args)
    }

    /// The changes of the stack of HEAD, bottom first.
    fn changes(&self) -> Vec<Ch> {
        let v: Value = serde_json::from_str(self.ok(&["sl", "--json", "-s"]).trim()).unwrap();
        v["stacks"][0]["changes"]
            .as_array()
            .expect("a stack")
            .iter()
            .map(|c| Ch {
                change: c["change"].as_str().unwrap().to_owned(),
                short: c["short"].as_str().unwrap().to_owned(),
                commit: c["commit"].as_str().unwrap().to_owned(),
            })
            .collect()
    }

    /// The ref names of the last AGit request, in order.
    fn sent(&self) -> Vec<String> {
        let all = self.server.agit();
        assert_eq!(all.len(), 1, "one UpdateRefs per submit");
        all[0].updates.iter().map(|u| u.name.clone()).collect()
    }

    fn agit_names(&self, chs: &[Ch]) -> Vec<String> {
        chs.iter()
            .map(|c| format!("refs/for/master/asc-{}", c.change))
            .collect()
    }
}

// ------------------------------------------------------------------------------ the tests

#[test]
fn a_stack_goes_up_as_one_update_refs_bottom_to_top() {
    let fx = Fx::new();
    let chs = fx.stack(3);
    assert_eq!(chs.len(), 3);
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs), "bottom to top");
    assert_eq!(fx.server.count("UpdateRefs"), 1, "{:?}", fx.server.st.calls);
    let req = &fx.server.agit()[0];
    assert_eq!(req.push_options, ["asc.draft=true"]);
    assert_eq!(req.mode, wire::UpdateMode::FastForward as i32);
    assert!(fx.server.count("PutObjects") >= 1, "the closure goes up");
    assert!(
        r.stdout.contains("Submitted 3 changes to origin (draft)"),
        "{}",
        r.stdout
    );
    for c in &chs {
        assert!(r.stdout.contains(&c.short), "{}", r.stdout);
    }

    // Nothing changed: the same refs go again, and no object does.
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs));
    assert_eq!(fx.server.count("PutObjects"), 0);
}

#[test]
fn asc_stack_submit_is_asc_submit_with_the_same_gate_and_refusals() {
    let fx = Fx::new();
    let chs = fx.stack(3);
    fx.server.forget();
    let r = fx.asc(&["stack", "submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!r.stderr.contains("not supported yet"), "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs), "bottom to top");
    assert_eq!(fx.server.count("UpdateRefs"), 1);
    assert_eq!(fx.server.agit()[0].push_options, ["asc.draft=true"]);
    assert!(
        r.stdout.contains("Submitted 3 changes to origin (draft)"),
        "{}",
        r.stdout
    );
    // The options and the client-side refusals are the same as `asc submit`'s.
    fx.server.forget();
    let r = fx.asc(&["stack", "submit", "--reviewer=a=b"]);
    assert_eq!(r.code, 129, "{}", r.stderr);
    assert!(fx.sent().is_empty(), "nothing is sent for a bad reviewer");
    fx.server.forget();
    let r = fx.asc(&["stack", "submit", "--publish", "--auto-merge"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        fx.server.agit()[0].push_options,
        ["asc.draft=false", "asc.auto_merge=true"]
    );
}

#[test]
fn a_named_change_sends_bottom_through_it_and_s_sends_everything() {
    let fx = Fx::new();
    let chs = fx.stack(3);
    fx.server.forget();
    let r = fx.asc(&["submit", &chs[1].short]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs[..2]));

    fx.server.forget();
    let r = fx.asc(&["submit", "-s", &chs[0].short]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs), "-s is the whole stack");

    // Without -s the current change is the top; at the bottom only the bottom goes.
    fx.ok(&["switch", "--detach", &chs[0].commit]);
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs[..1]));
}

#[test]
fn the_options_carry_draft_reviewers_update_only_auto_merge_and_force() {
    let fx = Fx::new();
    let chs = fx.stack(2);
    // `--update-only` sends only changes with a PR.
    fx.server.st.reviews.lock().unwrap().insert(
        chs[0].change.clone(),
        (11, "http://forge/acme/app/pulls/11".into(), "open".into()),
    );
    fx.server.forget();
    let r = fx.asc(&[
        "submit",
        "--publish",
        "--reviewer",
        "alice,Bob",
        "--reviewer",
        "alice",
        "--update-only",
        "--auto-merge",
        "--force",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        fx.sent(),
        fx.agit_names(&chs[..1]),
        "only the change with a PR"
    );
    let req = &fx.server.agit()[0];
    assert_eq!(
        req.push_options,
        [
            "asc.draft=false",
            "asc.reviewers=alice,Bob",
            "asc.update_only=true",
            "asc.auto_merge=true",
            "force-push",
        ]
    );
    assert_eq!(req.mode, wire::UpdateMode::Force as i32);

    // The default follows `asc.stack.submit.draft`.
    fx.ok(&["config", "asc.stack.submit.draft", "false"]);
    fx.server.forget();
    assert_eq!(fx.asc(&["submit"]).code, 0);
    assert_eq!(fx.server.agit()[0].push_options, ["asc.draft=false"]);
    fx.server.forget();
    assert_eq!(fx.asc(&["submit", "--draft"]).code, 0);
    assert_eq!(fx.server.agit()[0].push_options, ["asc.draft=true"]);
    assert_eq!(fx.asc(&["submit", "--draft", "--publish"]).code, 129);
}

#[test]
fn a_reviewer_name_that_could_rewrite_the_option_is_refused_before_anything_is_sent() {
    let fx = Fx::new();
    fx.stack(1);
    fx.server.forget();
    for bad in ["a=b", "x y", "a;b", "-r", ".dot", "ünï", "a\u{1b}[31m"] {
        let r = fx.asc(&["submit", &format!("--reviewer={bad}")]);
        assert_eq!(r.code, 129, "{bad:?}: {}", r.stderr);
        assert!(r.stderr.contains("not a valid user name"), "{}", r.stderr);
        assert!(!r.stderr.contains('\u{1b}'), "{:?}", r.stderr);
    }
    assert!(fx.server.agit().is_empty());
    assert_eq!(fx.server.count("HasObjects"), 0, "nothing was uploaded");
    assert_eq!(fx.server.count("PutObjects"), 0);
}

#[test]
fn a_change_that_needs_a_restack_is_refused_and_nothing_is_sent() {
    let fx = Fx::new();
    let chs = fx.stack(3);
    // Amend the bottom change on a side branch: it is obsolete, its child needs a restack.
    fx.ok(&["switch", "--detach", &chs[0].commit]);
    fx.ok(&["commit", "-q", "--amend", "-m", "c1 again"]);
    fx.ok(&["branch", "keep"]);
    fx.ok(&["switch", "master"]);
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 128, "{}{}", r.stdout, r.stderr);
    assert!(r.stderr.contains("needs a restack"), "{}", r.stderr);
    assert!(r.stderr.contains(&chs[1].short), "{}", r.stderr);
    assert!(fx.server.agit().is_empty());
    assert_eq!(fx.server.count("PutObjects"), 0);
}

#[test]
fn a_divergent_change_is_refused_and_nothing_is_sent() {
    let fx = Fx::new();
    let chs = fx.stack(1);
    for (i, branch) in ["left", "right"].iter().enumerate() {
        fx.ok(&["switch", "--detach", &chs[0].commit]);
        fx.ok(&["commit", "-q", "--amend", "-m", &format!("c1 v{i}")]);
        fx.ok(&["branch", branch]);
    }
    fx.ok(&["switch", "left"]);
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 128, "{}{}", r.stdout, r.stderr);
    assert!(r.stderr.contains("divergent"), "{}", r.stderr);
    assert!(fx.server.agit().is_empty());
}

#[test]
fn a_change_the_server_says_has_landed_is_refused() {
    let fx = Fx::new();
    let chs = fx.stack(2);
    fx.server
        .st
        .landed
        .lock()
        .unwrap()
        .insert(chs[1].change.clone());
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 128, "{}", r.stderr);
    assert!(r.stderr.contains("already landed"), "{}", r.stderr);
    assert!(r.stderr.contains(&chs[1].short), "{}", r.stderr);
    assert!(fx.server.agit().is_empty());
    assert_eq!(fx.server.count("PutObjects"), 0);

    // A merged PR is refused the same way.
    fx.server.st.landed.lock().unwrap().clear();
    fx.server.st.reviews.lock().unwrap().insert(
        chs[0].change.clone(),
        (3, "http://forge/pulls/3".into(), "merged".into()),
    );
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 128, "{}", r.stderr);
    assert!(r.stderr.contains("already merged"), "{}", r.stderr);
    assert!(fx.server.agit().is_empty());
}

#[test]
fn update_only_without_a_pr_sends_nothing_and_without_the_service_fails_closed() {
    let fx = Fx::new();
    fx.stack(2);
    fx.server.forget();
    let r = fx.asc(&["submit", "--update-only"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Nothing to submit"), "{}", r.stdout);
    assert!(fx.server.agit().is_empty());
    assert_eq!(fx.server.count("PutObjects"), 0);

    fx.server.st.no_changes.store(true, Ordering::SeqCst);
    let r = fx.asc(&["submit", "--update-only"]);
    assert_eq!(r.code, 128, "{}", r.stderr);
    assert!(r.stderr.contains("Changes service"), "{}", r.stderr);
    assert!(fx.server.agit().is_empty());

    // A plain submit still works against a server without the service.
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.server.agit().len(), 1);
}

#[test]
fn a_fork_above_the_named_change_is_ambiguous_under_s() {
    let fx = Fx::new();
    let chs = fx.stack(1);
    for (branch, file) in [("f1", "x1"), ("f2", "x2")] {
        fx.ok(&["switch", "--detach", &chs[0].commit]);
        fx.ok(&["switch", "-c", branch]);
        commit(&fx.sb, &fx.repo, file);
    }
    fx.server.forget();
    let r = fx.asc(&["submit", "-s", &chs[0].short, "--json"]);
    assert_eq!(r.code, 2, "{}{}", r.stdout, r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["status"], "ambiguous");
    assert_eq!(v["candidates"].as_array().unwrap().len(), 2, "{v}");
    assert!(fx.server.agit().is_empty());

    // Without -s the bottom change alone is unambiguous.
    let r = fx.asc(&["submit", &chs[0].short]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), fx.agit_names(&chs));
}

#[test]
fn nothing_to_submit_at_the_trunk_is_a_success() {
    let fx = Fx::new();
    fx.server.forget();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Nothing to submit"), "{}", r.stdout);
    assert!(fx.server.agit().is_empty());
}

#[test]
fn dry_run_sends_and_uploads_nothing() {
    let fx = Fx::new();
    let chs = fx.stack(2);
    fx.server.forget();
    let r = fx.asc(&["submit", "-n"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Would submit 2 changes"), "{}", r.stdout);
    assert!(r.stdout.contains(&chs[0].short), "{}", r.stdout);
    assert!(fx.server.agit().is_empty());
    assert_eq!(fx.server.count("PutObjects"), 0);
    assert_eq!(fx.server.count("UpdateRefs"), 0);
}

#[test]
fn json_names_every_change_its_ref_and_its_pr() {
    let fx = Fx::new();
    let chs = fx.stack(2);
    fx.server.st.reviews.lock().unwrap().insert(
        chs[0].change.clone(),
        (41, "http://forge/acme/app/pulls/41".into(), "open".into()),
    );
    let r = fx.asc(&["submit", "--json", "--reviewer", "carol"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["version"], 1);
    assert_eq!(v["status"], "submitted");
    assert_eq!(v["trunk"], "master");
    assert_eq!(v["draft"], true);
    assert_eq!(v["reviewers"][0], "carol");
    let cs = v["changes"].as_array().unwrap();
    assert_eq!(cs.len(), 2);
    for (got, want) in cs.iter().zip(&chs) {
        assert_eq!(got["change"], want.change.as_str());
        assert_eq!(got["commit"], want.commit.as_str());
        assert_eq!(
            got["ref"],
            format!("refs/for/master/asc-{}", want.change).as_str()
        );
    }
    assert_eq!(cs[0]["review"]["number"], 41);
    assert!(cs[1].get("review").is_none(), "{}", cs[1]);
}

#[test]
fn what_the_server_says_is_printed_without_control_characters() {
    let fx = Fx::new();
    fx.stack(1);
    *fx.server.st.message.lock().unwrap() =
        "https://forge/acme/app/pulls/7\n\u{1b}[31mred\u{7}".to_owned();
    let r = fx.asc(&["submit"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stderr.contains("remote: https://forge/acme/app/pulls/7"),
        "{}",
        r.stderr
    );
    // The transport drops control characters from the message; whatever it keeps is cleaned again.
    assert!(r.stderr.contains("remote: [31mred"), "{}", r.stderr);
    assert!(!r.stderr.contains('\u{1b}') && !r.stderr.contains('\u{7}'));
    assert!(!r.stdout.contains('\u{1b}'));
}

#[test]
fn a_server_refusal_is_reported_with_a_hint_and_moves_nothing_locally() {
    let fx = Fx::new();
    let chs = fx.stack(2);
    let before = fx.ok(&["rev-parse", "HEAD"]);
    for (code, want) in [
        (
            RpcError::ChangeNotAllowed(ErrorInfo {
                paths: vec!["not allowed to update change abc".into()],
                ..ErrorInfo::default()
            }),
            "may not update",
        ),
        (RpcError::ChangeDiverged(ErrorInfo::default()), "--force"),
    ] {
        *fx.server.st.reject.lock().unwrap() = Some(code);
        fx.server.forget();
        let r = fx.asc(&["submit"]);
        assert_eq!(r.code, 128, "{}", r.stderr);
        assert!(r.stderr.contains(want), "{}", r.stderr);
        assert!(r.stderr.contains("CHANGE_"), "{}", r.stderr);
        assert_eq!(fx.sent().len(), chs.len());
    }
    assert_eq!(fx.ok(&["rev-parse", "HEAD"]), before);
}

#[test]
fn a_read_only_token_cannot_submit() {
    let fx = Fx::new();
    fx.stack(1);
    fx.server.forget();
    let r = fx.asc_as(READ, &["submit"]);
    assert_ne!(r.code, 0, "{}", r.stdout);
    assert!(fx.server.agit().is_empty());
    assert!(
        !r.stderr.contains(READ) && !r.stdout.contains(READ),
        "the token is never printed"
    );
}

#[test]
fn a_plain_git_remote_exits_128_without_asking_anything() {
    let sb = Sandbox::new();
    let wk = {
        // A host with no discovery document is a git remote.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let t = std::thread::spawn(move || {
            use std::io::{Read, Write};
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut c, _)) => {
                        c.set_nonblocking(false).ok();
                        c.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .ok();
                        let mut buf = [0u8; 4096];
                        let _ = c.read(&mut buf);
                        let _ = c.write_all(
                            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        );
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
        });
        (addr, stop, t)
    };
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q"]);
    ok(
        &sb,
        &a,
        &["remote", "add", "origin", &format!("http://{}/o/r", wk.0)],
    );
    commit(&sb, &a, "one");
    let r = asc(&sb, &a, &["submit"]);
    wk.1.store(true, Ordering::SeqCst);
    wk.2.join().unwrap();
    assert_eq!(r.code, 128, "{}{}", r.stdout, r.stderr);
    assert!(
        r.stderr
            .contains("asc: submit needs an asc server (not available on a plain git remote)"),
        "{}",
        r.stderr
    );
}

#[test]
fn without_a_remote_submit_fails() {
    let sb = Sandbox::new();
    let a = sb.dir("solo");
    ok(&sb, &a, &["init", "-q"]);
    commit(&sb, &a, "f");
    let r = asc(&sb, &a, &["submit"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("origin"), "{}", r.stderr);
}
