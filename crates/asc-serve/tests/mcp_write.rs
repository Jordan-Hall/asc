//! M14: the server write tools `commit`, `submit`, `rebase`, `restack` (SPEC 22.3.3).
//!
//! `commit` and `rebase` take an empty `update_ref` or `refs/for/*` and nothing else (a branch is
//! never written); the tools need write access from the gate, `[mcp] write` (the installed
//! `WriteGate`) and an unreadable repository looks missing; identity fields are refused; `dry_run`
//! defaults to true; every call carries a write credential for its one repository and an op id.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::errors::ToolError;
use asc_rpc::IdMode;
use asc_rpc::auth::StaticTokens;
use asc_rpc::server::wire;
use asc_rpc::server::{
    CallCtx, MutationsHandler, RebaseHandler, RefsHandler, RepoHandlers, RepoService, ServerInfo,
};
use asc_rpc::services::changes_server::{Changes, ChangesServer};
use asc_serve::config::Limits;
use asc_serve::limits::Admission;
use asc_serve::mcp::backend::{RepoGate, ServeAuthorizer, ServeBackend, WriteGate};
use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::service::Routes;
use tonic::{Request, Response, Status};

const REPO: &str = "acme/app";

/// What the fake handlers saw.
#[derive(Default)]
struct Seen {
    commits: Mutex<Vec<(String, IdMode, bool, wire::CommitRequest)>>,
    rebases: Mutex<Vec<wire::RebaseRequest>>,
    updates: Mutex<Vec<wire::UpdateRefsRequest>>,
    restacks: Mutex<Vec<(bool, wire::RestackChangesRequest)>>,
}

struct Fake(Arc<Seen>);

#[async_trait]
impl MutationsHandler for Fake {
    async fn commit(
        &self,
        ctx: &CallCtx,
        req: wire::CommitRequest,
    ) -> Result<wire::CommitResponse, Status> {
        self.0.commits.lock().unwrap().push((
            ctx.principal.id.clone(),
            ctx.id_mode,
            ctx.op_id.is_some(),
            req,
        ));
        Ok(wire::CommitResponse {
            id: vec![1; 32].into(),
            git: vec![2; 20].into(),
        })
    }
}

#[async_trait]
impl RebaseHandler for Fake {
    async fn rebase(
        &self,
        _: &CallCtx,
        req: wire::RebaseRequest,
    ) -> Result<wire::RebaseResponse, Status> {
        self.0.rebases.lock().unwrap().push(req);
        Ok(wire::RebaseResponse::default())
    }
}

#[async_trait]
impl RefsHandler for Fake {
    async fn update_refs(
        &self,
        _: &CallCtx,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        self.0.updates.lock().unwrap().push(req);
        Ok(wire::UpdateRefsResponse::default())
    }
}

fn change_id() -> [u8; 16] {
    [5; 16]
}

fn letters() -> String {
    asc_core::change::ChangeId::from_bytes(change_id()).to_string()
}

struct FakeChanges(Arc<Seen>);

#[async_trait]
impl Changes for FakeChanges {
    async fn resolve_changes(
        &self,
        request: Request<wire::ResolveChangesRequest>,
    ) -> Result<Response<wire::ResolveChangesResponse>, Status> {
        let known = change_id().to_vec();
        let changes = request
            .get_ref()
            .changes
            .iter()
            .filter(|c| c.as_ref() == known.as_slice())
            .map(|c| wire::ChangeInfo {
                change: c.clone(),
                head: vec![7u8; 32].into(),
                version: 1,
                ..Default::default()
            })
            .collect();
        Ok(Response::new(wire::ResolveChangesResponse { changes }))
    }

    async fn stack(
        &self,
        _: Request<wire::StackRequest>,
    ) -> Result<Response<wire::StackResponse>, Status> {
        Ok(Response::new(wire::StackResponse {
            trunk: "master".into(),
            ..Default::default()
        }))
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
        request: Request<wire::RestackChangesRequest>,
    ) -> Result<Response<wire::RestackChangesResponse>, Status> {
        let write_credential = request
            .metadata()
            .get("authorization")
            .is_some_and(|v| v.to_str().is_ok_and(|s| s.starts_with("Bearer ascmcp.")));
        self.0
            .restacks
            .lock()
            .unwrap()
            .push((write_credential, request.into_inner()));
        Ok(Response::new(wire::RestackChangesResponse::default()))
    }

    async fn amend_change(
        &self,
        _: Request<wire::AmendChangeRequest>,
    ) -> Result<Response<wire::AmendChangeResponse>, Status> {
        Err(Status::unimplemented("amend_change"))
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

/// `(principal, repo)` pairs that may read.
struct Gate(HashSet<(String, String)>);

#[async_trait]
impl RepoGate for Gate {
    async fn can_read(&self, principal: &str, repo: &str) -> bool {
        self.0.contains(&(principal.to_owned(), repo.to_owned()))
    }
}

/// `(principal, repo)` pairs that may write.
struct Writers(HashSet<(String, String)>);

#[async_trait]
impl WriteGate for Writers {
    async fn can_write(&self, principal: &str, repo: &str) -> bool {
        self.0.contains(&(principal.to_owned(), repo.to_owned()))
    }
}

fn pairs(p: &[(&str, &str)]) -> HashSet<(String, String)> {
    p.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

struct Rig {
    backend: ServeBackend,
    seen: Arc<Seen>,
}

/// alice and bob read `acme/app`; alice writes it; carol reads nothing; `writes` false leaves the
/// backend without a write gate (`[mcp] write` unset).
fn rig(writes: bool) -> Rig {
    let seen = Arc::new(Seen::default());
    let handlers = Arc::new(RepoHandlers::new());
    let fake = Arc::new(Fake(Arc::clone(&seen)));
    handlers.set_mutations(fake.clone()).unwrap();
    handlers.set_rebase(fake.clone()).unwrap();
    handlers.set_refs(fake).unwrap();
    let scope = ServeAuthorizer::new(Arc::new(StaticTokens::new()));
    let routes = Routes::new(
        RepoService::new(handlers, scope.clone(), ServerInfo::default()).into_service(),
    )
    .add_service(ChangesServer::new(FakeChanges(Arc::clone(&seen))));
    let gate = Gate(pairs(&[("alice", REPO), ("bob", REPO)]));
    let mut backend = ServeBackend::new(
        routes,
        scope,
        Arc::new(gate),
        Arc::new(Admission::new(&Limits::default())),
        8,
    );
    if writes {
        backend = backend.with_writes(Arc::new(Writers(pairs(&[("alice", REPO)]))));
    }
    Rig { backend, seen }
}

fn ctx(principal: &str) -> CallContext {
    CallContext {
        principal: Some(principal.to_owned()),
        budget_bytes: 1 << 20,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    }
}

async fn run(r: &Rig, tool: &str, args: Value, who: &str) -> Result<Value, ToolError> {
    match r.backend.call(tool, &args, &ctx(who)).await? {
        Reply::Messages(s) => Ok(s
            .map(|m| m.unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .next()
            .unwrap_or(Value::Null)),
        Reply::Verb { .. } => panic!("a verb reply"),
    }
}

fn commit_args(update_ref: Option<&str>) -> Value {
    let mut v = json!({ "repo": REPO, "message": "hello", "ops": [] });
    if let Some(u) = update_ref {
        v["update_ref"] = json!(u);
    }
    v
}

#[tokio::test]
async fn commit_with_an_empty_update_ref_becomes_a_draft_change() {
    let r = rig(true);
    run(&r, "commit", commit_args(None), "alice").await.unwrap();
    run(&r, "commit", commit_args(Some("")), "alice")
        .await
        .unwrap();
    let seen = r.seen.commits.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (principal, id_mode, op_id, req) in seen.iter() {
        // The caller is the principal, ids are git oids, the mutating call carries an op id, and
        // no identity came from the arguments.
        assert_eq!(principal, "alice");
        assert_eq!(*id_mode, IdMode::Git);
        assert!(op_id);
        assert!(req.update_ref.is_empty());
        assert!(req.author.is_none() && req.committer.is_none() && !req.sign);
    }
}

#[tokio::test]
async fn commit_refuses_every_update_ref_but_empty_and_refs_for() {
    let r = rig(true);
    for bad in [
        "refs/heads/master",
        "master",
        "refs/heads/feature",
        "refs/tags/v1",
        "refs/for/",
        "refs/for/../heads/master",
        "refs/for//x",
        "refs/for/x\n",
        "REFS/for/master",
    ] {
        let e = run(&r, "commit", commit_args(Some(bad)), "alice")
            .await
            .unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "{bad:?}");
    }
    assert!(
        r.seen.commits.lock().unwrap().is_empty(),
        "no refused commit reached the handler"
    );
}

#[tokio::test]
async fn commit_to_refs_for_runs_the_pipeline_handler_with_the_ref() {
    let r = rig(true);
    run(&r, "commit", commit_args(Some("refs/for/master")), "alice")
        .await
        .unwrap();
    let seen = r.seen.commits.lock().unwrap();
    assert_eq!(seen[0].3.update_ref, "refs/for/master");
}

#[tokio::test]
async fn identity_mode_and_sign_are_never_arguments() {
    let r = rig(true);
    for (k, v) in [
        ("author", json!({ "name": "x", "email": "x@x", "time": 1 })),
        (
            "committer",
            json!({ "name": "x", "email": "x@x", "time": 1 }),
        ),
        ("sign", json!(true)),
    ] {
        let mut a = commit_args(None);
        a[k] = v;
        let e = run(&r, "commit", a, "alice").await.unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "{k}");
    }
    let e = run(
        &r,
        "rebase",
        json!({ "repo": REPO, "mode": "UPDATE_MODE_FORCE" }),
        "alice",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    let e = run(
        &r,
        "restack",
        json!({ "repo": REPO, "committer_name": "x" }),
        "alice",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    assert!(r.seen.commits.lock().unwrap().is_empty() && r.seen.rebases.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_tools_need_the_write_switch_and_write_access() {
    // `[mcp] write` unset: no write gate, nothing is served.
    let off = rig(false);
    for tool in ["commit", "submit", "rebase", "restack"] {
        let e = run(
            &off,
            tool,
            json!({ "repo": REPO, "changes": [letters()] }),
            "alice",
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "NOT_AVAILABLE", "{tool}");
    }
    let r = rig(true);
    // bob reads but does not write.
    let e = run(&r, "commit", commit_args(None), "bob")
        .await
        .unwrap_err();
    assert_eq!(e.code, "PERMISSION_DENIED");
    // carol cannot read: the very answer for a missing repository.
    let denied = run(&r, "commit", commit_args(None), "carol")
        .await
        .unwrap_err();
    let mut missing_args = commit_args(None);
    missing_args["repo"] = json!("acme/nope");
    let missing = run(&r, "commit", missing_args, "alice").await.unwrap_err();
    assert_eq!(
        (denied.code.as_str(), denied.message.as_str()),
        (missing.code.as_str(), missing.message.as_str())
    );
    assert!(r.seen.commits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_write_needs_a_principal_and_a_repo() {
    let r = rig(true);
    let mut c = ctx("alice");
    c.principal = None;
    let e = r
        .backend
        .call("commit", &commit_args(None), &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, "UNAUTHENTICATED");
    let e = run(&r, "commit", json!({ "message": "x" }), "alice")
        .await
        .unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
}

#[tokio::test]
async fn rebase_follows_the_update_ref_rule_and_defaults_to_a_dry_run() {
    let r = rig(true);
    let base = json!({ "repo": REPO, "base": "a", "head": "b", "onto": "c" });
    run(&r, "rebase", base.clone(), "alice").await.unwrap();
    let mut apply = base.clone();
    apply["dry_run"] = json!(false);
    apply["update_ref"] = json!("refs/for/master");
    run(&r, "rebase", apply, "alice").await.unwrap();
    {
        let seen = r.seen.rebases.lock().unwrap();
        assert!(seen[0].dry_run, "absent dry_run means true");
        assert!(!seen[1].dry_run);
        assert_eq!(seen[1].update_ref, "refs/for/master");
        assert_eq!(seen[1].mode, 0, "the update mode is never the caller's");
    }
    let mut branch = base.clone();
    branch["update_ref"] = json!("refs/heads/master");
    branch["dry_run"] = json!(false);
    let e = run(&r, "rebase", branch, "alice").await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    // Another repository cannot be the head.
    let mut other = base;
    other["head_repo"] = json!({ "name": "acme/secret" });
    let e = run(&r, "rebase", other, "alice").await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    assert_eq!(r.seen.rebases.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn restack_defaults_to_a_dry_run_on_a_write_credential() {
    let r = rig(true);
    run(
        &r,
        "restack",
        json!({ "repo": REPO, "onto": "master" }),
        "alice",
    )
    .await
    .unwrap();
    run(
        &r,
        "restack",
        json!({ "repo": REPO, "onto": "master", "dry_run": false }),
        "alice",
    )
    .await
    .unwrap();
    let seen = r.seen.restacks.lock().unwrap();
    assert!(seen[0].1.dry_run);
    assert!(!seen[1].1.dry_run);
    assert!(
        seen.iter().all(|(cred, _)| *cred),
        "a per-call credential, never the caller's token"
    );
}

#[tokio::test]
async fn submit_pushes_refs_for_trunk_through_update_refs() {
    let r = rig(true);
    run(
        &r,
        "submit",
        json!({ "repo": REPO, "changes": [letters()], "reviewers": ["bob", "dana"] }),
        "alice",
    )
    .await
    .unwrap();
    run(
        &r,
        "submit",
        json!({ "repo": REPO, "changes": [letters()], "draft": false }),
        "alice",
    )
    .await
    .unwrap();
    let seen = r.seen.updates.lock().unwrap();
    assert_eq!(seen[0].updates.len(), 1);
    assert_eq!(
        seen[0].updates[0].name,
        format!("refs/for/master/asc-{}", letters())
    );
    assert_eq!(
        seen[0].push_options,
        vec!["asc.draft".to_owned(), "asc.reviewers=bob,dana".to_owned()]
    );
    assert_eq!(seen[0].mode, 0, "no force unless asked");
    assert!(
        seen[1].push_options.is_empty(),
        "draft=false sends no asc.draft"
    );
}

#[tokio::test]
async fn submit_refuses_unknown_changes_and_odd_reviewers() {
    let r = rig(true);
    let other = asc_core::change::ChangeId::from_bytes([9; 16]).to_string();
    let e = run(
        &r,
        "submit",
        json!({ "repo": REPO, "changes": [other] }),
        "alice",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    for bad in ["a,b", "", "x y", "a\nb"] {
        let e = run(
            &r,
            "submit",
            json!({ "repo": REPO, "changes": [letters()], "reviewers": [bad] }),
            "alice",
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "{bad:?}");
    }
    let e = run(
        &r,
        "submit",
        json!({ "repo": REPO, "changes": [] }),
        "alice",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    assert!(
        r.seen.updates.lock().unwrap().is_empty(),
        "nothing was pushed"
    );
}
