//! M12b: `ServeBackend`. The read tools run through the in-process Repo handlers with
//! `id_mode = GIT`; each call is charged to its SPEC 16.3 class; 8 in-flight calls per principal,
//! then `RATE_LIMITED`; an unreadable repository answers the same `NOT_FOUND` as a missing one;
//! `Version.features` lists `json.profile.v1`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use asc_mcp::backend::{CallContext, McpBackend, Reply};
use asc_mcp::errors::ToolError;
use asc_mcp::rpc_backend::DynCodec;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::server::wire::{CompareRequest, CompareResponse, GrepRequest, GrepResponse};
use asc_rpc::server::{
    CallCtx, InsightsHandler, ReadsHandler, RepoHandlers, RepoService, RpcStream, ServerInfo,
};
use asc_serve::config::Limits;
use asc_serve::limits::{Admission, Class};
use asc_serve::mcp::backend::{RepoGate, ServeAuthorizer, ServeBackend};
use futures::StreamExt as _;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tonic::client::Grpc;
use tonic::service::Routes;
use tonic::{Request, Status};

/// What the fake handlers saw.
#[derive(Default)]
struct Seen {
    /// `(principal, id_mode, repo)` of each `Compare`.
    compares: Mutex<Vec<(String, IdMode, String)>>,
}

struct Fake(Arc<Seen>);

#[async_trait]
impl InsightsHandler for Fake {
    async fn compare(&self, ctx: &CallCtx, _req: CompareRequest) -> Result<CompareResponse, Status> {
        self.0.compares.lock().unwrap().push((ctx.principal.id.clone(), ctx.id_mode, ctx.repo.as_str().to_owned()));
        Ok(CompareResponse { ahead: 3, ..CompareResponse::default() })
    }
}

#[async_trait]
impl ReadsHandler for Fake {
    async fn grep(&self, _ctx: &CallCtx, _req: GrepRequest) -> Result<RpcStream<GrepResponse>, Status> {
        Ok(futures::stream::iter(vec![Ok(GrepResponse::default())]).chain(futures::stream::pending()).boxed())
    }
}

/// Readable pairs `(principal, repo)`; everything else, including a missing repo, is `false`.
struct Gate(HashSet<(String, String)>);

#[async_trait]
impl RepoGate for Gate {
    async fn can_read(&self, principal: &str, repo: &str) -> bool {
        self.0.contains(&(principal.to_owned(), repo.to_owned()))
    }
}

struct Rig {
    backend: ServeBackend,
    seen: Arc<Seen>,
    routes: Routes,
}

fn rig_with(limits: &Limits, max_inflight: u32) -> Rig {
    let seen = Arc::new(Seen::default());
    let handlers = Arc::new(RepoHandlers::new());
    let fake = Arc::new(Fake(Arc::clone(&seen)));
    handlers.set_insights(fake.clone()).unwrap();
    handlers.set_reads(fake).unwrap();
    // The public authorizer refuses every real token: only per-call credentials get in.
    let scope = ServeAuthorizer::new(Arc::new(StaticTokens::new()));
    let routes = Routes::new(RepoService::new(handlers, scope.clone(), ServerInfo::default()).into_service());
    let gate = Gate(
        [("alice", "acme/app"), ("alice", "acme/other"), ("bob", "acme/app")]
            .into_iter()
            .map(|(p, r)| (p.to_owned(), r.to_owned()))
            .collect(),
    );
    let backend = ServeBackend::new(routes.clone(), scope, Arc::new(gate), Arc::new(Admission::new(limits)), max_inflight);
    Rig { backend, seen, routes }
}

fn rig() -> Rig {
    rig_with(&Limits::default(), 8)
}

fn ctx(principal: &str) -> CallContext {
    CallContext {
        principal: Some(principal.to_owned()),
        budget_bytes: 18_000,
        position: None,
        cancel: CancellationToken::new(),
        progress: None,
    }
}

fn compare(repo: &str) -> Value {
    json!({ "repo": repo, "a": "main", "b": "topic" })
}

async fn drain(reply: Reply) -> Vec<Value> {
    match reply {
        Reply::Messages(s) => s.map(|m| m.unwrap()).collect().await,
        Reply::Verb { .. } => panic!("a verb reply"),
    }
}

fn err_json(e: &ToolError) -> (String, String) {
    (e.code.clone(), e.message.clone())
}

#[tokio::test]
async fn a_read_tool_runs_through_the_handlers_with_git_ids() {
    let r = rig();
    let reply = r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap();
    let out = drain(reply).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["ahead"], json!(3));
    let seen = r.seen.compares.lock().unwrap().clone();
    // The handler authorised the call as the principal, on the gated repository, in git id mode.
    assert_eq!(seen, vec![("alice".to_owned(), IdMode::Git, "acme/app".to_owned())]);
}

#[tokio::test]
async fn the_repo_argument_cannot_widen_the_gate() {
    let r = rig();
    // bob may read acme/app only: acme/other is refused before any handler runs.
    let e = r.backend.call("compare", &compare("acme/other"), &ctx("bob")).await.unwrap_err();
    assert_eq!(e.code, "NOT_FOUND");
    assert!(r.seen.compares.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unreadable_repo_answers_the_same_not_found_as_a_missing_one() {
    let r = rig();
    // `acme/secret` is not readable by bob; `acme/nope` does not exist. Same code, same text.
    let unreadable = r.backend.call("compare", &compare("acme/other"), &ctx("bob")).await.unwrap_err();
    let missing = r.backend.call("compare", &compare("acme/nope"), &ctx("bob")).await.unwrap_err();
    assert_eq!(unreadable.code, "NOT_FOUND");
    assert_eq!(err_json(&unreadable), err_json(&missing));
    assert!(r.seen.compares.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_call_needs_a_principal_and_a_valid_repo() {
    let r = rig();
    let mut anon = ctx("alice");
    anon.principal = None;
    let e = r.backend.call("compare", &compare("acme/app"), &anon).await.unwrap_err();
    assert_eq!(e.code, "UNAUTHENTICATED");
    let e = r.backend.call("compare", &compare("../etc"), &ctx("alice")).await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    let e = r.backend.call("compare", &json!({"a": "x", "b": "y"}), &ctx("alice")).await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
}

#[tokio::test]
async fn write_tools_and_workspace_tools_are_not_served() {
    let r = rig();
    for tool in ["commit", "rebase", "status", "no_such_tool"] {
        let e = r.backend.call(tool, &json!({"repo": "acme/app"}), &ctx("alice")).await.unwrap_err();
        assert_eq!(e.code, "NOT_AVAILABLE", "{tool}");
    }
}

#[tokio::test]
async fn eight_in_flight_calls_per_principal_then_rate_limited() {
    let r = rig();
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap());
    }
    let e = r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap_err();
    assert_eq!(e.code, "RATE_LIMITED");
    assert!(e.retry_after_ms.is_some());
    // Another principal has slots of its own.
    r.backend.call("compare", &compare("acme/app"), &ctx("bob")).await.unwrap();
    // A finished call frees its slot.
    drop(held.pop());
    r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap();
}

#[tokio::test]
async fn each_call_is_charged_to_its_rate_class() {
    let r = rig();
    assert_eq!(r.backend.class_of_tool("compare"), Some(Class::Cheap));
    assert_eq!(r.backend.class_of_tool("ls_tree"), Some(Class::Cheap));
    assert_eq!(r.backend.class_of_tool("read_file"), Some(Class::Objects));
    assert_eq!(r.backend.class_of_tool("grep"), Some(Class::Expensive));
    assert_eq!(r.backend.class_of_tool("log"), Some(Class::Expensive));
    assert_eq!(r.backend.class_of_tool("status"), None);

    // Observable charge: with one expensive slot per principal, a second open `grep` is refused
    // while a cheap `compare` of the same principal still runs.
    let limits = Limits { expensive_per_principal: 1, ..Limits::default() };
    let r = rig_with(&limits, 8);
    let grep = json!({ "repo": "acme/app", "rev": "main", "pattern": "x" });
    let first = r.backend.call("grep", &grep, &ctx("alice")).await.unwrap();
    let e = r.backend.call("grep", &grep, &ctx("alice")).await.unwrap_err();
    assert_eq!(e.code, "RATE_LIMITED");
    r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap();
    // Another principal is not affected.
    r.backend.call("grep", &grep, &ctx("bob")).await.unwrap();
    drop(first);
    r.backend.call("grep", &grep, &ctx("alice")).await.unwrap();
}

#[tokio::test]
async fn a_spent_cheap_bucket_refuses_with_a_retry_hint() {
    let limits = Limits { cheap_rps: 1, cheap_burst: 2, ..Limits::default() };
    let r = rig_with(&limits, 8);
    for _ in 0..2 {
        drain(r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap()).await;
    }
    let e = r.backend.call("compare", &compare("acme/app"), &ctx("alice")).await.unwrap_err();
    assert_eq!(e.code, "RATE_LIMITED");
    assert!(e.retry_after_ms.unwrap_or(0) > 0);
}

#[tokio::test]
async fn version_features_list_the_json_profile() {
    let r = rig();
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let m = pool.get_service_by_name("asc.v1.Repo").unwrap().methods().find(|m| m.name() == "Version").unwrap();
    let mut grpc = Grpc::new(r.routes.clone());
    grpc.ready().await.unwrap();
    let resp = grpc
        .unary(
            Request::new(DynamicMessage::new(m.input())),
            "/asc.v1.Repo/Version".parse().unwrap(),
            DynCodec::new(m.output()),
        )
        .await
        .unwrap()
        .into_inner();
    let v = asc_rpc::json::encode(&resp).unwrap();
    let features: Vec<&str> = v["features"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(features.contains(&"json.profile.v1"), "{features:?}");
}

#[tokio::test]
async fn the_public_routes_do_not_accept_a_forged_call_credential() {
    let r = rig();
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let m = pool.get_service_by_name("asc.v1.Repo").unwrap().methods().find(|m| m.name() == "Compare").unwrap();
    for token in ["ascmcp.0000.1", "ascmcp.", "ascmcp", "anything"] {
        let mut req_msg = DynamicMessage::new(m.input());
        let repo_desc = m.input().get_field_by_name("repo").unwrap().kind().as_message().unwrap().clone();
        let mut repo = DynamicMessage::new(repo_desc);
        repo.set_field_by_name("name", prost_reflect::Value::String("acme/app".into()));
        req_msg.set_field_by_name("repo", prost_reflect::Value::Message(repo));
        let mut req = Request::new(req_msg);
        req.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        let mut grpc = Grpc::new(r.routes.clone());
        grpc.ready().await.unwrap();
        let res = grpc.unary(req, "/asc.v1.Repo/Compare".parse().unwrap(), DynCodec::new(m.output())).await;
        assert!(res.is_err(), "{token} was accepted");
    }
    assert!(r.seen.compares.lock().unwrap().is_empty());
    let _ = Access::Read;
}
