use std::sync::{Arc, Mutex};

use futures::stream;
use prost_reflect::DescriptorPool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::schema::Surface;

fn call_ctx(budget_bytes: u64) -> CallContext {
    CallContext { principal: None, budget_bytes, position: None, cancel: CancellationToken::new(), progress: None }
}

fn p(args: Value) -> Result<Plan, ToolError> {
    plan(&args, 12_000, None)
}

/// Decodes `plan.request` as the wire message of its RPC, so a wrong field or enum name fails here.
fn assert_wire(plan: &Plan) {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let svc = pool.get_service_by_name("asc.v1.CodeIntel").unwrap();
    let m = svc.methods().find(|m| m.name() == plan.rpc.method()).unwrap();
    asc_rpc::json::decode(&m.input(), &plan.request).unwrap_or_else(|e| panic!("{}: {e}", plan.request));
}

#[test]
fn modes_map_onto_the_three_rpcs() {
    let text = p(json!({"query": "fn main", "mode": "text"})).unwrap();
    assert_eq!(text.rpc, Rpc::Search);
    assert_eq!(text.request["mode"], "literal");
    assert_eq!(text.request["pattern"], "fn main");
    assert_eq!(text.request["context_lines"], 2);
    assert_eq!(text.request["budget"]["max_results"], 50);
    assert_eq!(text.request["budget"]["max_tokens"], 4000);
    let re = p(json!({"query": "a.+b", "mode": "regex"})).unwrap();
    assert_eq!((re.rpc, re.request["mode"].clone()), (Rpc::Search, json!("regex")));
    let sym = p(json!({"query": "Foo", "mode": "symbol"})).unwrap();
    assert_eq!((sym.rpc, sym.request["op"].clone(), sym.request["name"].clone()), (Rpc::Symbols, json!("def"), json!("Foo")));
    let sem = p(json!({"query": "where is auth checked", "mode": "semantic"})).unwrap();
    assert_eq!(sem.rpc, Rpc::Semantic);
    assert_eq!(sem.request["k"], 10);
    for plan in [&text, &re, &sym, &sem] {
        assert_wire(plan);
    }
}

#[test]
fn optional_fields_reach_the_scope_and_request() {
    let a = p(json!({
        "query": "x", "mode": "text", "stack": "kzkzkzkz", "diff_only": true, "paths": ["src", "a\\b"],
        "include_excluded": true, "context_lines": 5, "limit": 7, "repo": "o/r",
    }))
    .unwrap();
    assert_eq!(a.repo.as_deref(), Some("o/r"));
    let s = &a.request["scope"];
    assert_eq!(s["stack_top"], "kzkzkzkz");
    assert_eq!(s["stack_diff_only"], true);
    assert_eq!(s["paths"], json!(["src", "a/b"]));
    assert_eq!(s["include_excluded"], true);
    assert_eq!(a.request["context_lines"], 5);
    assert_eq!(a.request["budget"]["max_results"], 7);
    assert!(s.get("repo").is_none(), "the connection decides the repository");

    let c = p(json!({"query": "x", "mode": "text", "change": "kzkzkzkz"})).unwrap();
    assert_eq!(c.request["scope"]["change"], "kzkzkzkz");
    let o = p(json!({"query": "src/lib.rs", "mode": "symbol", "op": "outline"})).unwrap();
    assert_eq!((o.request["path"].clone(), o.request["name"].clone()), (json!("src/lib.rs"), json!("")));
    let sem = p(json!({"query": "q", "mode": "semantic", "k": 3, "rerank": true})).unwrap();
    assert_eq!((sem.request["k"].clone(), sem.request["rerank"].clone()), (json!(3), json!(true)));
    for plan in [&a, &c, &o, &sem] {
        assert_wire(plan);
    }
}

#[test]
fn op_is_refused_outside_symbol_mode() {
    for mode in ["text", "regex", "semantic"] {
        let e = p(json!({"query": "x", "mode": mode, "op": "refs"})).unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "{mode}");
        assert!(e.message.contains("symbol mode"));
    }
    for op in ["def", "refs", "callers", "outline", "find"] {
        assert!(p(json!({"query": "x", "mode": "symbol", "op": op})).is_ok(), "{op}");
    }
    assert!(p(json!({"query": "x", "mode": "symbol", "op": "delete"})).is_err());
}

#[test]
fn bad_arguments_are_refused() {
    let cases = [
        json!({"query": "x"}),
        json!({"mode": "text"}),
        json!({"query": "", "mode": "text"}),
        json!({"query": "x", "mode": "fuzzy"}),
        json!({"query": "x", "mode": "text", "workspace": true}),
        json!({"query": "x", "mode": "text", "rev": "a", "change": "b"}),
        json!({"query": "x", "mode": "text", "diff_only": true}),
        json!({"query": "x", "mode": "text", "k": 3}),
        json!({"query": "x", "mode": "symbol", "rerank": true}),
        json!({"query": "x", "mode": "text", "limit": 0}),
        json!({"query": "x\u{0}y", "mode": "text"}),
        json!({"query": "x".repeat(MAX_QUERY_BYTES + 1), "mode": "text"}),
    ];
    for c in cases {
        assert_eq!(p(c.clone()).unwrap_err().code, "INVALID_ARGUMENT", "{c}");
    }
}

#[test]
fn unsafe_paths_are_refused() {
    for bad in ["../x", "a/../../b", "/etc/passwd", "C:/x", "a\\..\\b", "", "a\u{0}b"] {
        let e = p(json!({"query": "x", "mode": "text", "paths": [bad]})).unwrap_err();
        assert_eq!(e.code, crate::errors::code::PATH_POLICY, "{bad:?}");
    }
    let e = p(json!({"query": "../../etc/passwd", "mode": "symbol", "op": "outline"})).unwrap_err();
    assert_eq!(e.code, crate::errors::code::PATH_POLICY);
    let many: Vec<String> = (0..=MAX_PATHS).map(|i| format!("d{i}")).collect();
    assert!(p(json!({"query": "x", "mode": "text", "paths": many})).is_err());
}

#[test]
fn limits_are_clamped_and_max_tokens_is_capped_by_the_call_budget() {
    let a = p(json!({"query": "x", "mode": "text", "limit": 9999, "context_lines": 99, "max_tokens": 100_000})).unwrap();
    assert_eq!(a.request["budget"]["max_results"], MAX_LIMIT);
    assert_eq!(a.request["context_lines"], MAX_CONTEXT);
    // 12,000 budget bytes = 4,000 tokens: asc.mcp.maxTokens wins over the argument.
    assert_eq!(a.request["budget"]["max_tokens"], 4000);
    let small = plan(&json!({"query": "x", "mode": "text"}), 300, None).unwrap();
    assert_eq!(small.request["budget"]["max_tokens"], 100);
    let k = p(json!({"query": "x", "mode": "semantic", "k": 5000})).unwrap();
    assert_eq!(k.request["k"], MAX_K);
}

#[test]
fn the_page_token_reaches_paged_rpcs() {
    let a = plan(&json!({"query": "x", "mode": "text"}), 12_000, Some("tok")).unwrap();
    assert_eq!(a.request["page_token"], "tok");
    let b = plan(&json!({"query": "x", "mode": "symbol"}), 12_000, Some("tok")).unwrap();
    assert_eq!(b.request["page_token"], "tok");
}

fn ctx(features: &[&str]) -> Context {
    let mut c = Context::new(Surface::Stdio).with_all_reads();
    c.features = features.iter().map(|s| (*s).to_owned()).collect();
    c
}

fn names(c: &Context) -> Vec<&'static str> {
    catalogue::tools(c).iter().map(|l| l.def.name).collect()
}

#[test]
fn the_row_is_listed_only_with_search_v1() {
    for surface in [Surface::Stdio, Surface::Http] {
        let mut c = ctx(&[]);
        c.surface = surface;
        assert!(!names(&c).contains(&"search"), "{surface:?} without the feature");
        c.features.insert(FEATURE.into());
        assert!(names(&c).contains(&"search"), "{surface:?} with the feature");
    }
    // The toolset gate still applies: the default (core only) never lists it.
    let mut core = Context::new(Surface::Stdio);
    core.features.insert(FEATURE.into());
    assert!(!names(&core).contains(&"search"));
}

#[test]
fn the_row_declares_the_additive_fields_and_defaults() {
    let def = catalogue::find("search", Surface::Stdio).unwrap();
    assert!(def.ann.read_only);
    let schema: Value = serde_json::from_str(def.input_schema(Surface::Stdio).unwrap()).unwrap();
    let props = &schema["properties"];
    for f in [
        "query", "mode", "rev", "paths", "limit", "change", "stack", "diff_only", "op", "k", "context_lines", "max_tokens",
        "rerank", "include_excluded",
    ] {
        assert!(props.get(f).is_some(), "{f}");
    }
    assert_eq!(props["limit"]["default"], 50);
    assert_eq!(props["limit"]["maximum"], 200);
    assert_eq!(props["k"]["default"], 10);
    assert_eq!(props["context_lines"]["default"], 2);
    assert_eq!(props["max_tokens"]["default"], 4000);
    assert!(props.get("repo").is_none());
    let http_def = catalogue::find("search", Surface::Http).unwrap();
    let http: Value = serde_json::from_str(http_def.input_schema(Surface::Http).unwrap()).unwrap();
    assert!(http["required"].as_array().unwrap().contains(&json!("repo")));
    for w in ["text or regex first", "untrusted", "confidence", "semantic", "scope.changeset"] {
        assert!(def.description.contains(w), "{w}");
    }
    assert!(def.output.contains("\"coverage\"") && def.output.contains("\"est_tokens\"") && def.output.contains("\"scope\""));
}

#[test]
fn list_changed_fires_when_search_v1_appears() {
    let c = ctx(&[]);
    let none = BTreeSet::new();
    let with: BTreeSet<String> = [FEATURE.to_owned()].into();
    assert!(list_changed(&c, &none, &with));
    assert!(list_changed(&c, &with, &none));
    assert!(!list_changed(&c, &with, &with));
    let other: BTreeSet<String> = ["json.profile.v1".to_owned()].into();
    assert!(!list_changed(&c, &none, &other));
    // A client that did not enable the toolset sees no change.
    let core = Context::new(Surface::Stdio);
    assert!(!list_changed(&core, &none, &with));
}

#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Vec<Plan>>>,
}

#[async_trait::async_trait]
impl CodeIntelClient for Fake {
    async fn call(&self, plan: Plan, _ctx: &CallContext) -> Result<BoxStream<'static, Result<Value, ToolError>>, ToolError> {
        let rpc = plan.rpc;
        self.seen.lock().unwrap().push(plan);
        let scope = json!({"changeset": "ab", "root": "cd", "generation": 3, "coverage": 0.75});
        let messages = match rpc {
            Rpc::Search => vec![
                json!({"scope": scope, "files": [{"path": "a.rs", "matches": [{"line": 1, "text": "hit"}]}], "est_tokens": 9}),
                json!({"scope": scope, "files": []}),
            ],
            Rpc::Symbols => vec![json!({"scope": scope, "symbols": [{"name": "Foo", "confidence": "name_match"}]})],
            Rpc::Semantic => vec![json!({"scope": scope, "chunks": [], "est_tokens": 1, "coverage": 0.5})],
        };
        Ok(stream::iter(messages.into_iter().map(Ok)).boxed())
    }
}

async fn run(tool: &SearchTool<Fake>, args: Value) -> Result<Vec<Value>, ToolError> {
    match tool.call("search", &args, &call_ctx(12_000)).await? {
        Reply::Messages(s) => Ok(s.map(|m| m.unwrap()).collect().await),
        Reply::Verb { .. } => panic!("search answers with messages"),
    }
}

#[tokio::test]
async fn every_mode_returns_scope_coverage_and_est_tokens() {
    let fake = Fake::default();
    let tool = SearchTool::new(fake.clone());
    for (mode, rpc) in [("text", Rpc::Search), ("regex", Rpc::Search), ("symbol", Rpc::Symbols), ("semantic", Rpc::Semantic)] {
        let out = run(&tool, json!({"query": "q", "mode": mode})).await.unwrap();
        assert!(!out.is_empty());
        for m in &out {
            assert_eq!(m["scope"]["changeset"], "ab", "{mode}");
            assert!(m["coverage"].is_number(), "{mode}");
            assert!(m["est_tokens"].is_number(), "{mode}");
        }
        assert_eq!(fake.seen.lock().unwrap().last().unwrap().rpc, rpc);
    }
    // The service's own values are kept; missing ones are filled.
    let out = run(&tool, json!({"query": "q", "mode": "text"})).await.unwrap();
    assert_eq!((out[0]["coverage"].clone(), out[0]["est_tokens"].clone()), (json!(0.75), json!(9)));
    assert!(out[1]["est_tokens"].as_u64().unwrap() > 0);
    let sem = run(&tool, json!({"query": "q", "mode": "semantic"})).await.unwrap();
    assert_eq!(sem[0]["coverage"], 0.5);
}

#[tokio::test]
async fn a_refused_call_never_reaches_the_service() {
    let fake = Fake::default();
    let tool = SearchTool::new(fake.clone());
    let e = run(&tool, json!({"query": "q", "mode": "text", "op": "refs"})).await.unwrap_err();
    assert_eq!(e.code, "INVALID_ARGUMENT");
    let e = run(&tool, json!({"query": "q", "mode": "text", "paths": ["../x"]})).await.unwrap_err();
    assert_eq!(e.code, crate::errors::code::PATH_POLICY);
    assert!(fake.seen.lock().unwrap().is_empty());
    let other = tool.call("grep", &json!({}), &call_ctx(1000)).await.err().unwrap();
    assert_eq!(other.code, crate::errors::code::NOT_AVAILABLE);
}
