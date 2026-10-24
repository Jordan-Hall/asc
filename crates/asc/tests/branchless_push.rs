//! B12 acceptance: `asc push origin feat` in branchless mode against an in-process `asc serve`
//! that advertises `branchless.v1`, whose `RepoConfig` says the repo is branchless and whose
//! `UpdateRefs` answers AGit pushes the way the Forgejo fork will (C34). The client is the built
//! `asc` binary; the git side of the output comparison is real `git` against a bare repository.
//!
//! What is shown: the user ref and one `refs/for/<trunk>/asc-<change>` per change go up in ONE
//! `UpdateRefs`, bottom to top; `asc.push.review` picks the draft option or sends the user ref
//! alone; `--all` and `--mirror` open no PR; a merge in the range is refused with nothing sent;
//! a per-change refusal fails the whole update and nothing moves; and the branch line of the
//! output is git's.
#![cfg(feature = "serve")]

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;

use async_trait::async_trait;
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{
    CallCtx, InsightsHandler, ObjectsHandler, RefsHandler, RepoName, RepoService, RpcStream,
    ServerInfo, rpc_status, wire,
};
use asc_serve::registry::{Extensions, NewRepo, Registry};
use asc_serve::update::ServerRefs;
use asc_serve::{migrate, read};
use serde_json::Value;
use support::{Sandbox, text};
use tonic::Status;
use tonic::codegen::{Service, http};
use tonic::server::NamedService;

const WRITE: &str = "w-token";

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

// ------------------------------------------------------------------------------ the server

#[derive(Default)]
struct State {
    /// Every `UpdateRefs` the server saw, in order.
    updates: Mutex<Vec<wire::UpdateRefsRequest>>,
    /// The AGit answer is this error (nothing is applied).
    reject: Mutex<Option<RpcError>>,
    /// `RepoConfig.branchless`.
    branchless: AtomicBool,
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

/// The real refs handler, except that AGit refs are recorded and answered here (the real
/// handler gets the rest), and that an armed rejection applies nothing at all.
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
        self.st.updates.lock().unwrap().push(req.clone());
        let (agit, plain): (Vec<_>, Vec<_>) = req
            .updates
            .iter()
            .cloned()
            .partition(|u| u.name.starts_with("refs/for/"));
        if agit.is_empty() {
            return self.real.update_refs(ctx, req).await;
        }
        if let Some(e) = self.st.reject.lock().unwrap().clone() {
            return Err(rpc_status(&e));
        }
        let mut results = Vec::new();
        if !plain.is_empty() {
            let mut rest = req.clone();
            rest.updates = plain;
            rest.push_options.clear();
            results.extend(self.real.update_refs(ctx, rest).await?.results);
        }
        results.extend(agit.iter().enumerate().map(|(i, u)| wire::RefResult {
            name: format!("refs/pull/{}/head", i + 1),
            old: None,
            new: u.new.clone(),
            user: false,
        }));
        Ok(wire::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: String::new(),
        })
    }
}

struct Insights(Arc<State>);

#[async_trait]
impl InsightsHandler for Insights {
    async fn repo_config(
        &self,
        _ctx: &CallCtx,
        _req: wire::RepoConfigRequest,
    ) -> Result<wire::RepoConfigResponse, Status> {
        Ok(wire::RepoConfigResponse {
            branchless: self.0.branchless.load(Ordering::SeqCst),
            ..wire::RepoConfigResponse::default()
        })
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
        st.branchless.store(true, Ordering::SeqCst);
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
                assert!(
                    ext.handlers
                        .set_insights(Arc::new(Insights(state.clone())))
                        .is_ok()
                );
                let _hooks = read::register(&ext, &registry);
                let auth = Arc::new(StaticTokens::new().with_token(WRITE, "alice", Access::Write));
                let info = ServerInfo {
                    features: vec!["branchless.v1".to_owned()],
                    ..ServerInfo::default()
                };
                let repo = Counting {
                    inner: RepoService::new(ext.handlers.clone(), auth, info).into_service(),
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

    /// The `UpdateRefs` requests since the last `forget`.
    fn updates(&self) -> Vec<wire::UpdateRefsRequest> {
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

fn run_of(out: Output) -> Run {
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Run {
    run_of(
        sb.asc(cwd)
            .envs(IDENT)
            .env("ASC_TOKEN", WRITE)
            .env("ASC_ALLOW_LOOPBACK_HTTP", "1")
            .stdin(std::process::Stdio::null())
            .args(args)
            .output()
            .expect("spawn asc"),
    )
}

fn git(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Run {
    run_of(
        sb.command("git", cwd)
            .envs(IDENT)
            .stdin(std::process::Stdio::null())
            .args(args)
            .output()
            .expect("spawn git"),
    )
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

struct Fx {
    sb: Sandbox,
    server: Server,
    repo: PathBuf,
}

impl Fx {
    /// `origin` is the server and `master` holds `base`, pushed; HEAD is on a new branch `feat`.
    fn new() -> Self {
        let sb = Sandbox::new();
        let server = Server::start();
        let repo = sb.dir("a");
        ok(&sb, &repo, &["init", "-q"]);
        ok(&sb, &repo, &["remote", "add", "origin", &server.url()]);
        commit(&sb, &repo, "base");
        ok(&sb, &repo, &["push", "origin", "master"]);
        ok(&sb, &repo, &["switch", "-q", "-c", "feat"]);
        server.forget();
        Self { sb, server, repo }
    }

    fn asc(&self, args: &[&str]) -> Run {
        asc(&self.sb, &self.repo, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        ok(&self.sb, &self.repo, args)
    }

    fn commit(&self, name: &str) {
        commit(&self.sb, &self.repo, name);
    }

    /// The change ids of the stack of HEAD, bottom first.
    fn changes(&self) -> Vec<String> {
        let v: Value = serde_json::from_str(self.ok(&["sl", "--json", "-s"]).trim()).unwrap();
        v["stacks"][0]["changes"]
            .as_array()
            .expect("a stack")
            .iter()
            .map(|c| c["change"].as_str().unwrap().to_owned())
            .collect()
    }

    fn agit_names(&self, changes: &[String]) -> Vec<String> {
        changes
            .iter()
            .map(|c| format!("refs/for/master/asc-{c}"))
            .collect()
    }

    /// The ref names of the only `UpdateRefs` the server saw.
    fn sent(&self) -> Vec<String> {
        let all = self.server.updates();
        assert_eq!(all.len(), 1, "one UpdateRefs per push: {all:?}");
        all[0].updates.iter().map(|u| u.name.clone()).collect()
    }

    fn remote_has(&self, name: &str) -> bool {
        let out = self.ok(&["ls-remote", "origin"]);
        out.lines().any(|l| l.ends_with(&format!("\t{name}")))
    }
}

// ------------------------------------------------------------------------------ the tests

#[test]
fn the_user_ref_and_every_change_go_up_in_one_update_refs_bottom_to_top() {
    let fx = Fx::new();
    for n in ["c1", "c2", "c3"] {
        fx.commit(n);
    }
    let changes = fx.changes();
    assert_eq!(changes.len(), 3);
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut want = vec!["refs/heads/feat".to_owned()];
    want.extend(fx.agit_names(&changes));
    assert_eq!(fx.sent(), want, "user ref first, then bottom to top");
    assert_eq!(fx.server.count("UpdateRefs"), 1);
    assert_eq!(fx.server.updates()[0].push_options, ["asc.draft=true"]);
    // The branch line is git's; the AGit refs are not in it.
    assert!(
        r.stderr.contains(" * [new branch]      feat -> feat"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("refs/for"), "{}", r.stderr);
    assert!(!r.stderr.contains("asc-"), "{}", r.stderr);
    // The remote-tracking ref moved.
    assert!(fx.ok(&["branch", "-r"]).contains("origin/feat"));

    // A second commit goes up on top of the same changes: the same AGit refs plus one.
    fx.commit("c4");
    let changes = fx.changes();
    assert_eq!(changes.len(), 4);
    fx.server.forget();
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut want = vec!["refs/heads/feat".to_owned()];
    want.extend(fx.agit_names(&changes));
    assert_eq!(fx.sent(), want);
}

#[test]
fn asc_push_review_picks_the_option_or_sends_the_user_ref_alone() {
    let fx = Fx::new();
    fx.commit("c1");
    let changes = fx.changes();

    fx.ok(&["config", "asc.push.review", "publish"]);
    assert_eq!(fx.asc(&["push", "origin", "feat"]).code, 0);
    assert_eq!(fx.server.updates()[0].push_options, ["asc.draft=false"]);
    assert_eq!(fx.sent().len(), 1 + changes.len());

    fx.ok(&["config", "asc.push.review", "none"]);
    fx.commit("c2");
    fx.server.forget();
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), ["refs/heads/feat"], "the user ref alone");
    assert!(fx.server.updates()[0].push_options.is_empty());

    // A value that cannot be read is an error, not a guess.
    fx.ok(&["config", "asc.push.review", "maybe"]);
    fx.commit("c3");
    fx.server.forget();
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_ne!(r.code, 0);
    assert!(fx.server.updates().is_empty(), "nothing was sent");
}

#[test]
fn a_shared_ref_a_branch_mode_repo_and_a_branch_mode_client_open_no_pr() {
    let fx = Fx::new();
    fx.commit("c1");
    // The trunk is a shared ref.
    ok(&fx.sb, &fx.repo, &["switch", "-q", "master"]);
    fx.commit("m1");
    fx.server.forget();
    assert_eq!(fx.asc(&["push", "origin", "master"]).code, 0);
    assert_eq!(fx.sent(), ["refs/heads/master"]);

    ok(&fx.sb, &fx.repo, &["switch", "-q", "feat"]);
    // The repo is in branch mode.
    fx.server.st.branchless.store(false, Ordering::SeqCst);
    fx.server.forget();
    assert_eq!(fx.asc(&["push", "origin", "feat"]).code, 0);
    assert_eq!(fx.sent(), ["refs/heads/feat"]);

    // The client is in branch mode.
    fx.server.st.branchless.store(true, Ordering::SeqCst);
    fx.ok(&["config", "asc.branchless", "false"]);
    fx.commit("c2");
    fx.server.forget();
    assert_eq!(fx.asc(&["push", "origin", "feat"]).code, 0);
    assert_eq!(fx.sent(), ["refs/heads/feat"]);
}

#[test]
fn push_all_creates_no_pr_and_mirror_is_rejected() {
    let fx = Fx::new();
    fx.commit("c1");
    ok(&fx.sb, &fx.repo, &["switch", "-q", "-c", "other"]);
    fx.commit("o1");
    fx.server.forget();
    let r = fx.asc(&["push", "--all", "origin"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let sent = fx.sent();
    assert!(sent.iter().any(|n| n == "refs/heads/feat"), "{sent:?}");
    assert!(sent.iter().any(|n| n == "refs/heads/other"), "{sent:?}");
    assert!(sent.iter().all(|n| !n.starts_with("refs/for/")), "{sent:?}");
    assert!(fx.server.updates()[0].push_options.is_empty());

    fx.server.forget();
    let r = fx.asc(&["push", "--mirror", "origin"]);
    assert_ne!(r.code, 0, "{}", r.stdout);
    assert!(r.stderr.contains("--mirror"), "{}", r.stderr);
    assert!(fx.server.updates().is_empty(), "nothing was sent");
    assert_eq!(fx.server.count("PutObjects"), 0);
}

/// `feat` holds a change and a merge of a newer trunk.
fn merge_fixture() -> Fx {
    let fx = Fx::new();
    fx.commit("c1");
    ok(&fx.sb, &fx.repo, &["switch", "-q", "master"]);
    fx.commit("m1");
    ok(&fx.sb, &fx.repo, &["switch", "-q", "feat"]);
    fx.ok(&["merge", "--no-ff", "-q", "-m", "merge master", "master"]);
    fx.server.forget();
    fx
}

#[test]
fn a_range_with_a_merge_is_rejected_and_nothing_moves() {
    let fx = merge_fixture();
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains(
            " ! [remote rejected] feat -> feat (asc: stack contains a merge commit; rebase it or submit changes individually)"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("error: failed to push some refs to"),
        "{}",
        r.stderr
    );
    assert!(fx.server.updates().is_empty(), "no UpdateRefs");
    assert_eq!(fx.server.count("PutObjects"), 0, "no upload");
    assert!(!fx.remote_has("refs/heads/feat"));
    assert!(!fx.ok(&["branch", "-r"]).contains("origin/feat"));

    // `asc.push.review=none` is the user ref only, so the merge does not matter.
    fx.ok(&["config", "asc.push.review", "none"]);
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(fx.sent(), ["refs/heads/feat"]);
}

#[test]
fn a_per_change_refusal_fails_the_whole_update_and_moves_nothing() {
    let fx = Fx::new();
    fx.commit("c1");
    fx.commit("c2");
    *fx.server.st.reject.lock().unwrap() = Some(RpcError::ChangeNotAllowed(ErrorInfo {
        paths: vec!["not allowed to update change abc".into()],
        ..ErrorInfo::default()
    }));
    let before = fx.ok(&["rev-parse", "HEAD"]);
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains(" ! [remote rejected] feat -> feat ("),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("CHANGE_NOT_ALLOWED"), "{}", r.stderr);
    assert!(
        r.stderr.contains("error: failed to push some refs to"),
        "{}",
        r.stderr
    );
    assert_eq!(
        fx.sent().len(),
        3,
        "the user ref and two changes were asked for"
    );
    assert!(
        !fx.remote_has("refs/heads/feat"),
        "the user ref did not move"
    );
    assert!(!fx.ok(&["branch", "-r"]).contains("origin/feat"));
    assert_eq!(fx.ok(&["rev-parse", "HEAD"]), before);

    // `-u` sets nothing up for a refused push.
    *fx.server.st.reject.lock().unwrap() = Some(RpcError::ChangeNotAllowed(ErrorInfo::default()));
    assert_eq!(fx.asc(&["push", "-u", "origin", "feat"]).code, 1);
    assert_eq!(fx.asc(&["config", "branch.feat.remote"]).code, 1);
}

#[test]
fn the_server_text_in_a_refusal_is_one_clean_line() {
    let fx = Fx::new();
    fx.commit("c1");
    *fx.server.st.reject.lock().unwrap() = Some(RpcError::ChangeNotAllowed(ErrorInfo {
        paths: vec!["bad\u{1b}[2J\nline".into()],
        ..ErrorInfo::default()
    }));
    let r = fx.asc(&["push", "origin", "feat"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(!r.stderr.contains('\u{1b}'), "{:?}", r.stderr);
    let line = r
        .stderr
        .lines()
        .find(|l| l.contains("[remote rejected]"))
        .expect("the branch line");
    assert!(
        line.starts_with(" ! [remote rejected] feat -> feat ("),
        "{line}"
    );
}

// ------------------------------------------------------------------------------ git's output

/// Output with what differs between a git server and an asc server removed: `remote:` lines, the
/// URL, object ids, and the program name in advice.
fn normalised(run: &Run) -> String {
    let mut out = String::new();
    for stream in [&run.stdout, &run.stderr] {
        for line in stream.lines() {
            if line.starts_with("remote:") {
                continue;
            }
            let line = if line.starts_with("To ") {
                "To URL".to_owned()
            } else if let Some(i) = line.find("failed to push some refs to") {
                format!("{}failed to push some refs to URL", &line[..i])
            } else {
                line.replace("'asc ", "'git ")
            };
            let mut shown = String::new();
            let mut hex = String::new();
            for c in line.chars().chain(std::iter::once(' ')) {
                if c.is_ascii_hexdigit() && c.is_ascii_lowercase() || c.is_ascii_digit() {
                    hex.push(c);
                    continue;
                }
                if hex.len() >= 7 && hex.chars().any(|h| h.is_ascii_alphabetic()) {
                    shown.push_str("<sha>");
                } else {
                    shown.push_str(&hex);
                }
                hex.clear();
                shown.push(c);
            }
            out.push_str(shown.trim_end());
            out.push('\n');
        }
        out.push_str("--\n");
    }
    out
}

struct GitFx {
    sb: Sandbox,
    repo: PathBuf,
}

impl GitFx {
    fn new() -> Self {
        let sb = Sandbox::new();
        let bare = sb.dir("remote.git");
        let r = git(&sb, &bare, &["init", "-q", "--bare", "-b", "master"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let repo = sb.dir("g");
        assert_eq!(git(&sb, &repo, &["init", "-q", "-b", "master"]).code, 0);
        assert_eq!(
            git(
                &sb,
                &repo,
                &["remote", "add", "origin", bare.to_str().unwrap()]
            )
            .code,
            0
        );
        let fx = Self { sb, repo };
        fx.commit("base");
        assert_eq!(fx.git(&["push", "-q", "origin", "master"]).code, 0);
        assert_eq!(fx.git(&["switch", "-q", "-c", "feat"]).code, 0);
        fx
    }

    fn git(&self, args: &[&str]) -> Run {
        git(&self.sb, &self.repo, args)
    }

    fn commit(&self, name: &str) {
        std::fs::write(self.repo.join(name), format!("{name}\n")).unwrap();
        assert_eq!(self.git(&["add", name]).code, 0);
        assert_eq!(self.git(&["commit", "-q", "-m", name]).code, 0);
    }
}

fn same_as_git(label: &str, asc_run: &Run, git_run: &Run) {
    assert_eq!(
        asc_run.code, git_run.code,
        "{label}: exit code\n{}",
        asc_run.stderr
    );
    assert_eq!(
        normalised(asc_run),
        normalised(git_run),
        "{label}: output differs from git's"
    );
}

#[test]
fn the_output_of_a_mapped_push_equals_gits() {
    let fx = Fx::new();
    let gx = GitFx::new();

    // A new branch.
    fx.commit("c1");
    fx.commit("c2");
    gx.commit("c1");
    gx.commit("c2");
    let (a, b) = (
        fx.asc(&["push", "origin", "feat"]),
        gx.git(&["push", "origin", "feat"]),
    );
    assert!(a.stderr.contains("[new branch]"), "{}", a.stderr);
    same_as_git("new branch", &a, &b);
    assert_eq!(fx.sent().len(), 3, "the push was mapped");

    // `-u` on a fast-forward.
    fx.commit("c3");
    gx.commit("c3");
    let (a, b) = (
        fx.asc(&["push", "-u", "origin", "feat"]),
        gx.git(&["push", "-u", "origin", "feat"]),
    );
    same_as_git("-u", &a, &b);

    // `--force-with-lease` on a fast-forward.
    fx.commit("c4");
    gx.commit("c4");
    let (a, b) = (
        fx.asc(&["push", "--force-with-lease", "origin", "feat"]),
        gx.git(&["push", "--force-with-lease", "origin", "feat"]),
    );
    same_as_git("--force-with-lease", &a, &b);

    // A rejected push: the branch was rewound and grew again.
    fx.ok(&["reset", "-q", "--hard", "HEAD~1"]);
    gx.git(&["reset", "-q", "--hard", "HEAD~1"]);
    fx.commit("other");
    gx.commit("other");
    let (a, b) = (
        fx.asc(&["push", "origin", "feat"]),
        gx.git(&["push", "origin", "feat"]),
    );
    assert_eq!(a.code, 1, "{}", a.stderr);
    assert!(a.stderr.contains("[rejected]"), "{}", a.stderr);
    same_as_git("rejected", &a, &b);

    // `--delete`.
    let (a, b) = (
        fx.asc(&["push", "--delete", "origin", "feat"]),
        gx.git(&["push", "--delete", "origin", "feat"]),
    );
    assert!(a.stderr.contains("[deleted]"), "{}", a.stderr);
    same_as_git("--delete", &a, &b);
}
