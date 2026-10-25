//! M32: the server `land` and `classify` tools. `land` calls `UpdateRefs` with `UPDATE_MODE_LAND`
//! and nothing else; a preview (the default) writes nothing; the branch must accept land
//! requests; the pipeline's refusals come back with their own code; `classify` is cut at 300
//! files.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::catalogue::{self, Confirm, Needs};
use asc_mcp::config::ConfirmMode;
use asc_mcp::errors::ToolError;
use asc_mcp::handler::needs_confirmation;
use asc_mcp::schema::Surface;
use asc_rpc::auth::StaticTokens;
use asc_rpc::convert::{Conflict, ConflictKind, ModeId};
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::wire::{
    ClassifyRequest, ClassifyResponse, FileClass, IsAncestorRequest, IsAncestorResponse, MergeRequest, MergeResponse,
    ResolveRequest, ResolveResponse, UpdateMode, UpdateRefsRequest, UpdateRefsResponse,
};
use asc_rpc::server::{
    CallCtx, ClassifyHandler, HistoryHandler, MutationsHandler, RefsHandler, RepoHandlers, RepoService, ServerInfo,
    rpc_status,
};
use asc_serve::config::Limits;
use asc_serve::limits::Admission;
use asc_serve::mcp::backend::{RepoGate, ServeAuthorizer, ServeBackend, WriteGate};
use asc_serve::mcp::land::{LandBackend, LandPolicy};
use asc_serve::mcp::write::check_update_ref;
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::service::Routes;
use tonic::Status;

/// What the fake handlers saw.
#[derive(Default)]
struct Seen {
    resolves: Mutex<Vec<Vec<String>>>,
    merges: Mutex<Vec<(String, String, bool)>>,
    /// `(principal, op id present, mode, ref, new tip)`.
    updates: Mutex<Vec<(String, bool, i32, String, Vec<u8>)>>,
}

/// How the fake pipeline answers `UpdateRefs`.
#[derive(Default)]
struct Script {
    update_error: Mutex<Option<Status>>,
    merge_conflicts: Mutex<Vec<&'static str>>,
    ancestor: Mutex<bool>,
    classify_files: Mutex<usize>,
}

struct Fake(Arc<Seen>, Arc<Script>);

#[async_trait]
impl HistoryHandler for Fake {
    async fn resolve(&self, _ctx: &CallCtx, req: ResolveRequest) -> Result<ResolveResponse, Status> {
        self.0.resolves.lock().unwrap().push(req.revspecs.clone());
        let ids = req
            .revspecs
            .iter()
            .map(|s| match s.as_str() {
                "nope" => Vec::new(),
                s if s.starts_with("refs/heads/") => vec![0xB0; 20],
                s => vec![s.as_bytes()[0]; 20],
            })
            .map(Into::into)
            .collect();
        Ok(ResolveResponse { ids })
    }

    async fn is_ancestor(&self, _ctx: &CallCtx, _req: IsAncestorRequest) -> Result<IsAncestorResponse, Status> {
        Ok(IsAncestorResponse { result: *self.1.ancestor.lock().unwrap() })
    }
}

#[async_trait]
impl MutationsHandler for Fake {
    async fn merge(&self, _ctx: &CallCtx, req: MergeRequest) -> Result<MergeResponse, Status> {
        self.0.merges.lock().unwrap().push((req.base.clone(), req.head.clone(), req.dry_run));
        let conflicts = self.1.merge_conflicts.lock().unwrap().iter().map(|p| p.as_bytes().to_vec().into()).collect();
        Ok(MergeResponse { conflicts, ..MergeResponse::default() })
    }
}

#[async_trait]
impl RefsHandler for Fake {
    async fn update_refs(&self, ctx: &CallCtx, req: UpdateRefsRequest) -> Result<UpdateRefsResponse, Status> {
        let u = &req.updates[0];
        self.0.updates.lock().unwrap().push((
            ctx.principal.id.clone(),
            ctx.op_id.is_some(),
            req.mode,
            u.name.clone(),
            u.new.as_ref().and_then(|t| t.t.as_ref()).map_or_else(Vec::new, |t| match t {
                asc_rpc::server::wire::ref_target::T::Changeset(b) => b.to_vec(),
                asc_rpc::server::wire::ref_target::T::Tag(b) => b.to_vec(),
            }),
        ));
        if let Some(e) = self.1.update_error.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(UpdateRefsResponse { message: "landed".into(), ..UpdateRefsResponse::default() })
    }
}

#[async_trait]
impl ClassifyHandler for Fake {
    async fn classify(&self, _ctx: &CallCtx, _req: ClassifyRequest) -> Result<ClassifyResponse, Status> {
        let n = *self.1.classify_files.lock().unwrap();
        let files = (0..n).map(|i| FileClass { path: format!("src/f{i}.rs").into_bytes().into(), class: 8 }).collect();
        Ok(ClassifyResponse { files, overall: 8 })
    }
}

/// Readable and writable pairs.
struct Gate {
    read: HashSet<(String, String)>,
    write: HashSet<(String, String)>,
}

#[async_trait]
impl RepoGate for Gate {
    async fn can_read(&self, principal: &str, repo: &str) -> bool {
        self.read.contains(&(principal.to_owned(), repo.to_owned()))
    }
}

#[async_trait]
impl WriteGate for Gate {
    async fn can_write(&self, principal: &str, repo: &str) -> bool {
        self.write.contains(&(principal.to_owned(), repo.to_owned()))
    }
}

/// `master` and `refs/heads/release` accept land requests on `acme/app`.
struct Policy;

#[async_trait]
impl LandPolicy for Policy {
    async fn accepts_land(&self, repo: &str, refname: &str) -> bool {
        repo == "acme/app" && matches!(refname, "refs/heads/master" | "refs/heads/release")
    }
}

struct Rig {
    backend: LandBackend,
    seen: Arc<Seen>,
    script: Arc<Script>,
}

fn pairs(v: &[(&str, &str)]) -> HashSet<(String, String)> {
    v.iter().map(|(p, r)| ((*p).to_owned(), (*r).to_owned())).collect()
}

fn rig_with(writes: bool) -> Rig {
    let seen = Arc::new(Seen::default());
    let script = Arc::new(Script::default());
    let handlers = Arc::new(RepoHandlers::new());
    let fake = Arc::new(Fake(Arc::clone(&seen), Arc::clone(&script)));
    handlers.set_history(fake.clone()).unwrap();
    handlers.set_mutations(fake.clone()).unwrap();
    handlers.set_refs(fake.clone()).unwrap();
    handlers.set_classify(fake).unwrap();
    let scope = ServeAuthorizer::new(Arc::new(StaticTokens::new()));
    let routes = Routes::new(RepoService::new(handlers, scope.clone(), ServerInfo::default()).into_service());
    let gate = Arc::new(Gate {
        read: pairs(&[("alice", "acme/app"), ("bob", "acme/app"), ("alice", "acme/other")]),
        write: pairs(&[("alice", "acme/app"), ("alice", "acme/other")]),
    });
    let mut inner = ServeBackend::new(routes, scope, gate.clone(), Arc::new(Admission::new(&Limits::default())), 8);
    if writes {
        inner = inner.with_writes(gate);
    }
    Rig { backend: LandBackend::new(inner, Arc::new(Policy)), seen, script }
}

fn rig() -> Rig {
    rig_with(true)
}

fn ctx(principal: &str) -> CallContext {
    CallContext {
        principal: Some(principal.to_owned()),
        budget_bytes: 1_000_000,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    }
}

async fn run(r: &Rig, who: &str, args: Value) -> Result<Value, ToolError> {
    match r.backend.call("land", &args, &ctx(who)).await? {
        Reply::Messages(s) => {
            let mut all: Vec<Value> = s.map(|m| m.unwrap()).collect().await;
            assert_eq!(all.len(), 1);
            Ok(all.remove(0))
        }
        Reply::Verb { .. } => panic!("a verb reply"),
    }
}

fn land(extra: Value) -> Value {
    let mut v = json!({ "repo": "acme/app", "tip": "tip1", "branch": "master" });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn no_writes(r: &Rig) {
    assert!(r.seen.updates.lock().unwrap().is_empty(), "UpdateRefs ran");
}

#[tokio::test]
async fn dry_run_defaults_to_true_and_writes_nothing() {
    let r = rig();
    let v = run(&r, "alice", land(json!({}))).await.unwrap();
    assert_eq!(v["dry_run"], json!(true));
    assert_eq!(v["would_land"], json!(true));
    no_writes(&r);
    let merges = r.seen.merges.lock().unwrap().clone();
    assert_eq!(merges.len(), 1);
    assert_eq!(merges[0].0, "refs/heads/master");
    assert!(merges[0].2);
    // Only the boolean `false` turns the preview off; a string does not.
    let e = run(&r, "alice", land(json!({"dry_run": "false"}))).await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    no_writes(&r);
}

#[tokio::test]
async fn a_preview_reports_conflicts_as_data() {
    let r = rig();
    r.script.merge_conflicts.lock().unwrap().push("Cargo.lock");
    let v = run(&r, "alice", land(json!({}))).await.unwrap();
    assert_eq!(v["would_land"], json!(false));
    assert_eq!(v["conflicts"], json!(["Cargo.lock"]));
    no_writes(&r);
}

#[tokio::test]
async fn a_land_is_update_refs_in_land_mode_as_the_principal() {
    let r = rig();
    let v = run(&r, "alice", land(json!({"dry_run": false, "branch": "refs/heads/release"}))).await.unwrap();
    assert_eq!(v["dry_run"], json!(false));
    assert_eq!(v["message"], json!("landed"));
    let u = r.seen.updates.lock().unwrap().clone();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].0, "alice");
    assert!(u[0].1, "an op id");
    assert_eq!(u[0].2, UpdateMode::Land as i32);
    assert_eq!(u[0].3, "refs/heads/release");
    assert_eq!(u[0].4, vec![b't'; 20]);
    // A preview ran no merge for a real land.
    assert!(r.seen.merges.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_branch_outside_land_branches_is_refused_before_anything_is_read() {
    let r = rig();
    for branch in ["develop", "refs/heads/develop", "refs/tags/v1", "refs/for/master", "../master"] {
        let e = run(&r, "alice", land(json!({"dry_run": false, "branch": branch}))).await.unwrap_err();
        assert!(matches!(e.code.as_str(), "LAND_BRANCH_REFUSED" | "INVALID_ARGUMENT"), "{branch}: {}", e.code);
    }
    let e = run(&r, "alice", land(json!({"dry_run": false, "branch": "develop"}))).await.unwrap_err();
    assert_eq!(e.code, "LAND_BRANCH_REFUSED");
    no_writes(&r);
    assert!(r.seen.resolves.lock().unwrap().is_empty());
    assert!(r.seen.merges.lock().unwrap().is_empty());
    // Another repository's policy is its own.
    let e = run(&r, "alice", land(json!({"repo": "acme/other", "dry_run": false}))).await.unwrap_err();
    assert_eq!(e.code, "LAND_BRANCH_REFUSED");
    no_writes(&r);
}

#[tokio::test]
async fn the_pipeline_decides_protections_approvals_and_hooks() {
    let r = rig();
    let cases: [(&str, fn(ErrorInfo) -> RpcError); 5] = [
        ("HOOK_REJECTED", RpcError::HookRejected),
        ("PATH_POLICY", RpcError::PathPolicy),
        ("CHANGE_NOT_ALLOWED", RpcError::ChangeNotAllowed),
        ("LAND_NEEDS_CLIENT_REBASE", RpcError::LandNeedsClientRebase),
        ("QUOTA_EXCEEDED", RpcError::QuotaExceeded),
    ];
    for (code, make) in cases {
        let info = ErrorInfo { paths: vec!["secrets/".into()], ..ErrorInfo::default() };
        let err = rpc_status(&make(info));
        *r.script.update_error.lock().unwrap() = Some(err);
        let e = run(&r, "alice", land(json!({"dry_run": false}))).await.unwrap_err();
        assert_eq!(e.code, code);
    }
    // The call did reach the pipeline each time, and only through UpdateRefs.
    assert_eq!(r.seen.updates.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn land_conflict_returns_the_conflicts_as_data() {
    let r = rig();
    let conflict = Conflict {
        changeset: ModeId::Asc(ChangesetId::from_bytes([1; 32])),
        path: RepoPath::new("bun.lock").unwrap(),
        kind: ConflictKind::Content,
        base: None,
        ours: None,
        theirs: None,
        marked: None,
    };
    let err = rpc_status(&RpcError::LandConflict(ErrorInfo { conflicts: vec![conflict], ..ErrorInfo::default() }));
    *r.script.update_error.lock().unwrap() = Some(err);
    let e = run(&r, "alice", land(json!({"dry_run": false}))).await.unwrap_err();
    assert_eq!(e.code, "LAND_CONFLICT");
    assert_eq!(e.conflicts, vec!["bun.lock".to_owned()]);
}

#[tokio::test]
async fn a_land_needs_write_access_and_the_write_switch() {
    let r = rig();
    // bob reads acme/app but may not write it.
    let e = run(&r, "bob", land(json!({"dry_run": false}))).await.unwrap_err();
    assert_eq!(e.code, "PERMISSION_DENIED");
    // A preview needs it too: it runs on the write credential.
    let e = run(&r, "bob", land(json!({}))).await.unwrap_err();
    assert_eq!(e.code, "PERMISSION_DENIED");
    // carol cannot read the repository: the answer is a missing repository's.
    let e = run(&r, "carol", land(json!({"dry_run": false}))).await.unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    let mut anon = ctx("alice");
    anon.principal = None;
    let args = land(json!({"dry_run": false}));
    let e = r.backend.call("land", &args, &anon).await.err().unwrap();
    assert_eq!(e.code, "UNAUTHENTICATED");
    no_writes(&r);
    assert!(r.seen.resolves.lock().unwrap().is_empty());
    // Without `[mcp] write` (no write gate) the tool is not served at all.
    let off = rig_with(false);
    let e = run(&off, "alice", land(json!({"dry_run": false}))).await.unwrap_err();
    assert_eq!(e.code, "NOT_AVAILABLE");
    no_writes(&off);
}

#[tokio::test]
async fn confirmation_is_always_required_and_the_tool_is_write_gated() {
    let def = catalogue::find("land", Surface::Http).expect("land is listed on http");
    assert_eq!(def.confirm, Confirm::Always);
    assert_eq!(def.needs, Needs::ServerWriteMcp);
    assert!(!def.ann.read_only && def.ann.destructive && def.ann.open_world);
    assert!(catalogue::find("land", Surface::Stdio).is_none());
    for mode in [ConfirmMode::Always, ConfirmMode::Destructive] {
        for args in [json!({}), land(json!({})), land(json!({"dry_run": false}))] {
            assert!(needs_confirmation(def, &args, mode));
        }
    }
}

#[tokio::test]
async fn land_takes_changes_or_a_tip_never_both_and_no_identity() {
    let r = rig();
    for extra in [
        json!({"changes": ["kkkk"]}),
        json!({"tip": null}),
        json!({"mode": "UPDATE_MODE_FORCE"}),
        json!({"update_ref": "refs/heads/master"}),
        json!({"committer": "x"}),
        json!({"hooks": "HOOK_MODE_SKIP_PRE"}),
    ] {
        let e = run(&r, "alice", land(extra.clone())).await.unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "{extra}");
    }
    no_writes(&r);
    let v = run(&r, "alice", json!({"repo": "acme/app", "changes": ["kkkk"], "branch": "master", "dry_run": false}))
        .await
        .unwrap();
    assert_eq!(v["dry_run"], json!(false));
}

#[tokio::test]
async fn several_changes_must_form_one_stack_and_unknown_revisions_are_not_found() {
    let r = rig();
    let args = json!({"repo": "acme/app", "changes": ["kkkk", "mmmm"], "branch": "master", "dry_run": false});
    let e = run(&r, "alice", args.clone()).await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    no_writes(&r);
    *r.script.ancestor.lock().unwrap() = true;
    run(&r, "alice", args).await.unwrap();
    // The last change is the tip.
    assert_eq!(r.seen.updates.lock().unwrap()[0].4, vec![b'm'; 20]);
    let e = run(&r, "alice", land(json!({"tip": "nope", "dry_run": false}))).await.unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    assert_eq!(r.seen.updates.lock().unwrap().len(), 1);
}

#[test]
fn commit_and_rebase_name_no_branch() {
    for bad in ["master", "refs/heads/master", "refs/tags/v1", "refs/users/1/heads/x", "refs/for/", "refs/for/../heads/x", "refs/for//x"] {
        assert!(check_update_ref(bad).is_err(), "{bad}");
    }
    for ok in ["", "refs/for/master", "refs/for/release/1"] {
        assert!(check_update_ref(ok).is_ok(), "{ok}");
    }
    // `land` has no `update_ref` argument at all.
    let def = catalogue::find("land", Surface::Http).unwrap();
    assert!(!def.input_http.contains("update_ref"));
}

#[tokio::test]
async fn classify_is_cut_at_300_files_and_says_so() {
    let r = rig();
    *r.script.classify_files.lock().unwrap() = 350;
    let args = json!({"repo": "acme/app", "base": "a", "head": "b"});
    let Reply::Messages(s) = r.backend.call("classify", &args, &ctx("alice")).await.unwrap() else { panic!() };
    let out: Vec<Value> = s.map(|m| m.unwrap()).collect().await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["files"].as_array().unwrap().len(), 300);
    assert_eq!(out[0]["omitted_files"], json!(50));
    assert!(out[0].get("overall").is_some());
    // The token budget cuts it further.
    let mut small = ctx("alice");
    small.budget_bytes = 400;
    let Reply::Messages(s) = r.backend.call("classify", &args, &small).await.unwrap() else { panic!() };
    let out: Vec<Value> = s.map(|m| m.unwrap()).collect().await;
    assert!(out[0]["files"].as_array().unwrap().len() < 20);
    // An unreadable repository is a missing one.
    let e = r.backend.call("classify", &json!({"repo": "acme/other", "base": "a", "head": "b"}), &ctx("bob")).await.err().unwrap();
    assert_eq!(e.code, "NOT_FOUND");
}
