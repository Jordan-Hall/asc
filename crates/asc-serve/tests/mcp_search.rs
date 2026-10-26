//! I21: the MCP `search` tool on the server, against the real in-process `CodeIntel` handler.
//!
//! `ServeBackend` runs the handler in-process (no socket), behind the per-call credential and the
//! repository gate. Each `mode` reaches its RPC and the reply carries `scope`, `coverage` and
//! `est_tokens`; the tool is listed only while the server advertises `search.v1`, which it does
//! exactly while a `CodeIntel` handler is registered.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::git::{ObjectKind, blob_oid};
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{GitMapRow, GitMapSink};
use asc_mcp::config::{McpConfig, parse_toolsets};
use asc_mcp::cursor::KeyRing;
use asc_mcp::handler::{Caller, Handler, HandlerConfig};
use asc_mcp::schema::Surface;
use asc_rpc::auth::StaticTokens;
use asc_rpc::server::{RepoName, ServerInfo};
use asc_serve::codeintel::features;
use asc_serve::codeintel::rpc::{CodeIntelRpc, CodeIntelService, CodeIntelSlot, Settings};
use asc_serve::config::Limits;
use asc_serve::limits::Admission;
use asc_serve::mcp::backend::{RepoGate, ServeAuthorizer, ServeBackend};
use asc_serve::migrate;
use asc_serve::registry::{Registry, RepoEntry};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tonic::service::Routes;

const REPO: &str = "acme/app";
const SECRET: &str = "acme/secret";
const LIB: &str = "pub fn zebrafish_handler() -> u32 {\n    1\n}\n";
const MAIN_RS: &str = "fn main() {\n    zebrafish_handler();\n}\n";

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

async fn commit(e: &RepoEntry, files: &[(&str, &str)]) -> ChangesetId {
    let engine = e.backend.engine();
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
    let base = asc_engine::Engine::<asc_store::bucket::BucketObjects, asc_store::sqlrefs::SqlRefs>::empty_tree_id();
    let root = engine.apply(base, &changes).await.unwrap();
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
    cs
}

async fn set_master(r: &Registry, e: &RepoEntry, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(b"refs/heads/master".to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(asc_core::id::RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    r.refs().update(e.key, &txn).await.unwrap();
}

/// `alice` may read `acme/app` only.
struct Gate;

#[async_trait]
impl RepoGate for Gate {
    async fn can_read(&self, principal: &str, repo: &str) -> bool {
        let ok: HashSet<(&str, &str)> = [("alice", REPO)].into();
        ok.contains(&(principal, repo))
    }
}

struct Rig {
    backend: ServeBackend,
    /// The server's `CodeIntel` registration, as `build_routes` reads it.
    slot: CodeIntelSlot,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let app = registry
        .create(&RepoName::parse(REPO).unwrap(), &Default::default())
        .await
        .unwrap();
    registry
        .create(&RepoName::parse(SECRET).unwrap(), &Default::default())
        .await
        .unwrap();
    let cs = commit(&app, &[("src/lib.rs", LIB), ("src/main.rs", MAIN_RS)]).await;
    set_master(&registry, &app, cs).await;

    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::new(dir.path().join("ci"));
    settings.page_key = Some([9; 32]);
    let rpc = Arc::new(CodeIntelRpc::new(registry, settings, None).unwrap());
    // The public authorizer refuses every real token: only per-call credentials get in.
    let scope = ServeAuthorizer::new(Arc::new(StaticTokens::new()));
    let svc = CodeIntelService::new(rpc.clone(), scope.clone(), ServerInfo::default());
    let routes = Routes::new(svc.into_service());
    let backend = ServeBackend::new(
        routes,
        scope,
        Arc::new(Gate),
        Arc::new(Admission::new(&Limits::default())),
        8,
    );
    let slot = CodeIntelSlot::default();
    assert!(slot.set(rpc));
    Rig {
        backend,
        slot,
        _dir: dir,
    }
}

/// A handler over `rig` listing tools by `advertised`; the directory holds the index.
fn handler(rig: Rig, advertised: &[String]) -> (Handler<ServeBackend>, tempfile::TempDir) {
    let mut hc = HandlerConfig::new(Surface::Http);
    hc.config = McpConfig {
        toolsets: parse_toolsets("core,search"),
        ..McpConfig::default()
    };
    hc.features = advertised.iter().cloned().collect();
    let Rig { backend, _dir, .. } = rig;
    (Handler::new(backend, hc, KeyRing::random().unwrap()), _dir)
}

async fn rpc(h: &Handler<ServeBackend>, who: &str, method: &str, params: Value) -> Value {
    let (tx, _rx) = mpsc::channel(8);
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let caller = Caller {
        principal: Some(who.to_owned()),
        server_write: false,
    };
    h.handle_message(msg, &caller, &tx)
        .await
        .expect("a response")
}

async fn search(h: &Handler<ServeBackend>, who: &str, args: Value) -> Value {
    rpc(
        h,
        who,
        "tools/call",
        json!({ "name": "search", "arguments": args }),
    )
    .await
}

fn listed(resp: &Value) -> bool {
    resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "search")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_is_listed_exactly_while_codeintel_is_registered() {
    let r = rig().await;
    let on = features::features(&r.slot);
    assert_eq!(on, ["search.v1"]);
    let off = features::features(&CodeIntelSlot::default());
    assert!(off.is_empty());

    let (h, _d1) = handler(rig().await, &on);
    assert!(listed(&rpc(&h, "alice", "tools/list", json!({})).await));
    let (h, _d2) = handler(r, &off);
    assert!(!listed(&rpc(&h, "alice", "tools/list", json!({})).await));
    let denied = search(
        &h,
        "alice",
        json!({"repo": REPO, "query": "zebrafish", "mode": "text"}),
    )
    .await;
    assert!(denied["error"].is_object(), "{denied}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_and_regex_modes_answer_from_the_real_service() {
    let r = rig().await;
    let (h, _dir) = handler(r, &["search.v1".to_owned()]);
    for (mode, query) in [
        ("text", "zebrafish_handler"),
        ("regex", "zebra[a-z]+_handler"),
    ] {
        let resp = search(
            &h,
            "alice",
            json!({"repo": REPO, "query": query, "mode": mode}),
        )
        .await;
        let result = &resp["result"];
        assert_ne!(result["isError"], true, "{mode}: {resp}");
        let text = result.to_string();
        assert!(text.contains("src/lib.rs"), "{mode}: {text}");
        assert!(text.contains("src/main.rs"), "{mode}: {text}");
        // The reply contract.
        assert!(text.contains("coverage"), "{mode}: {text}");
        assert!(text.contains("est_tokens"), "{mode}: {text}");
        assert!(text.contains("changeset"), "{mode}: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symbol_mode_reaches_symbols_and_reports_what_the_index_covers() {
    let r = rig().await;
    let (h, _dir) = handler(r, &["search.v1".to_owned()]);
    for op in ["def", "refs", "callers", "find"] {
        let resp = search(
            &h,
            "alice",
            json!({"repo": REPO, "query": "zebrafish_handler", "mode": "symbol", "op": op}),
        )
        .await;
        let result = &resp["result"];
        assert_ne!(result["isError"], true, "{op}: {resp}");
        // The symbol index is built by the workers, and this rig runs none: the service answers
        // from an index that covers nothing yet, and says so instead of guessing.
        let found = &result["structuredContent"];
        assert_eq!(found["coverage"], 0.0, "{op}: {resp}");
        assert!(found["est_tokens"].as_u64().unwrap() > 0, "{op}: {resp}");
        assert!(found["scope"]["changeset"].is_string(), "{op}: {resp}");
    }
    // An outline names a file, not a symbol; a path that is not relative never leaves the tool.
    let bad = search(
        &h,
        "alice",
        json!({"repo": REPO, "query": "../x.rs", "mode": "symbol", "op": "outline"}),
    )
    .await;
    assert_eq!(bad["result"]["isError"], true, "{bad}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn semantic_mode_names_the_missing_model_instead_of_failing_silently() {
    let r = rig().await;
    let (h, _dir) = handler(r, &["search.v1".to_owned()]);
    let resp = search(
        &h,
        "alice",
        json!({"repo": REPO, "query": "where is the handler", "mode": "semantic"}),
    )
    .await;
    // No embedding model is configured in this rig: the service says how to configure one, and the
    // tool reports it as an error result, not a protocol error.
    let result = &resp["result"];
    assert_eq!(result["isError"], true, "{resp}");
    assert!(
        result.to_string().to_lowercase().contains("model"),
        "{resp}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_repo_answers_like_a_missing_one() {
    let r = rig().await;
    let (h, _dir) = handler(r, &["search.v1".to_owned()]);
    let a = search(
        &h,
        "alice",
        json!({"repo": SECRET, "query": "zebrafish", "mode": "text"}),
    )
    .await;
    let b = search(
        &h,
        "alice",
        json!({"repo": "acme/nope", "query": "zebrafish", "mode": "text"}),
    )
    .await;
    assert_eq!(a["result"]["isError"], true, "{a}");
    assert_eq!(a["result"], b["result"]);
    assert!(!a.to_string().contains("zebrafish_handler"));
}
