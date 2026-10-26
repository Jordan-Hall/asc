//! The production `serve()` mounts `/mcp` (SPEC 22.2, 22.5, 22.7; PLAN M12b): a real server on a
//! loopback socket with `[mcp] enabled`, the credential edge in front, `ServeBackend` behind it,
//! the metadata document beside it and `mcp.v1` in `Version.features`.
//!
//! The walk is what a client does: `initialize`, `tools/list`, then a `search` call over a
//! repository that was pushed to the running server and indexed by its workers. Without a
//! credential every message is a 401 with the challenge; with the switch off nothing is mounted.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/serve_rig.rs"]
mod rig;

use asc_mcp::handler::SUPPORTED_VERSIONS;
use rig::{Options, READ_TOKEN, REPO, WRITE_TOKEN};
use serde_json::{Value, json};

const LIB: &str = "pub fn zebrafish_handler() -> u32 {\n    1\n}\n";
const MAIN_RS: &str = "fn main() {\n    zebrafish_handler();\n}\n";

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn tool_names(v: &Value) -> Vec<String> {
    v["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tool list in {v}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_list_and_search_over_a_pushed_repository() {
    let rig = rig::start(Options {
        mcp: true,
        codeintel: true,
    })
    .await;

    // initialize
    let (status, _, v) = rig
        .mcp(
            Some(WRITE_TOKEN),
            &rpc(
                1,
                "initialize",
                json!({ "protocolVersion": SUPPORTED_VERSIONS[0], "capabilities": {},
                        "clientInfo": { "name": "t", "version": "0" } }),
            ),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["serverInfo"]["name"], "asc", "{v}");

    // tools/list: the read tools and `search` (CodeIntel is on); `[mcp] write` is off, so no
    // write tool, whoever asks.
    let (status, _, v) = rig
        .mcp(Some(WRITE_TOKEN), &rpc(2, "tools/list", json!({})))
        .await;
    assert_eq!(status, 200, "{v}");
    let names = tool_names(&v);
    for want in ["log", "search"] {
        assert!(names.contains(&want.to_owned()), "{want} in {names:?}");
    }
    for write in ["commit", "submit", "rebase", "restack", "land"] {
        assert!(
            !names.contains(&write.to_owned()),
            "{write} listed: {names:?}"
        );
    }

    // A push to the running server, then the workers index it.
    let cs = rig
        .commit(None, &[("src/lib.rs", LIB), ("src/main.rs", MAIN_RS)])
        .await;
    rig.push(None, cs).await;
    let args = json!({ "repo": REPO, "query": "zebrafish_handler", "mode": "text" });
    rig::until("the pushed files to be searchable", 90, || async {
        let v = rig.call_tool(WRITE_TOKEN, "search", args.clone()).await;
        v["result"]["isError"] != true && v["result"].to_string().contains("src/lib.rs")
    })
    .await;
    let last = rig.call_tool(WRITE_TOKEN, "search", args.clone()).await;
    let text = last["result"].to_string();
    assert!(text.contains("src/main.rs"), "{text}");
    assert_eq!(last["id"], 7, "the client's id comes back");

    // A read-only credential reads through the same endpoint.
    let read = rig.call_tool(READ_TOKEN, "search", args).await;
    assert_ne!(read["result"]["isError"], true, "{read}");
    assert!(read["result"].to_string().contains("src/lib.rs"), "{read}");

    // A repository that does not exist is an error result, not a leak and not a crash.
    let missing = rig
        .call_tool(
            WRITE_TOKEN,
            "search",
            json!({ "repo": "acme/missing", "query": "zebrafish", "mode": "text" }),
        )
        .await;
    assert_eq!(missing["result"]["isError"], true, "{missing}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_credential_is_401_with_the_challenge_and_a_bad_one_too() {
    let rig = rig::start(Options {
        mcp: true,
        codeintel: false,
    })
    .await;
    let calls = [
        rpc(
            1,
            "initialize",
            json!({ "protocolVersion": SUPPORTED_VERSIONS[0] }),
        ),
        rpc(2, "tools/list", json!({})),
        rpc(
            3,
            "tools/call",
            json!({ "name": "log", "arguments": { "repo": REPO } }),
        ),
    ];
    for body in &calls {
        for token in [None, Some("not-a-token-at-all")] {
            let (status, headers, v) = rig.mcp(token, body).await;
            assert_eq!(status, 401, "{} {token:?}: {v}", body["method"]);
            // Basic first, then the Bearer challenge that names the metadata document.
            let challenge = headers
                .get_all("www-authenticate")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>()
                .join(" | ");
            assert!(!challenge.is_empty(), "a challenge");
            assert!(challenge.contains("Bearer"), "{challenge}");
            assert!(
                challenge.contains("resource_metadata=\"https://asc.example/.well-known/oauth-protected-resource/mcp\""),
                "{challenge}"
            );
            // The credential is never echoed.
            assert!(!challenge.contains("not-a-token"), "{challenge}");
            assert!(!v.to_string().contains("not-a-token"), "{v}");
        }
    }
    // Without CodeIntel the `search` tool is not listed.
    let (_, _, v) = rig
        .mcp(Some(READ_TOKEN), &rpc(2, "tools/list", json!({})))
        .await;
    assert!(!tool_names(&v).contains(&"search".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_and_origin_checks_and_the_metadata_document_are_live() {
    let rig = rig::start(Options {
        mcp: true,
        codeintel: false,
    })
    .await;
    let ping = rpc(1, "ping", json!({}));
    // An Origin that is not ours is refused before anything is read; the server's own is fine.
    let (status, _, _) = rig
        .mcp_with(
            Some(WRITE_TOKEN),
            &[("origin", "https://evil.example.net")],
            &ping,
        )
        .await;
    assert_eq!(status, 403);
    let (status, _, _) = rig
        .mcp_with(Some(WRITE_TOKEN), &[("origin", rig::PUBLIC_URL)], &ping)
        .await;
    assert_eq!(status, 200);
    // GET is not allowed on the stateless endpoint.
    let (status, _) = rig.get("/mcp").await;
    assert_eq!(status, 405);
    // The protected-resource document names the canonical resource.
    let (status, doc) = rig.get("/.well-known/oauth-protected-resource/mcp").await;
    assert_eq!(status, 200);
    assert_eq!(doc["resource"], "https://asc.example/mcp", "{doc}");
    assert_eq!(doc["bearer_methods_supported"], json!(["header"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_v1_is_advertised_exactly_when_it_is_mounted() {
    let on = rig::start(Options {
        mcp: true,
        codeintel: false,
    })
    .await;
    let features = on
        .client(REPO, WRITE_TOKEN)
        .server_version()
        .await
        .unwrap()
        .features;
    assert!(features.contains(&"mcp.v1".to_owned()), "{features:?}");

    let off = rig::start(Options::default()).await;
    let features = off
        .client(REPO, WRITE_TOKEN)
        .server_version()
        .await
        .unwrap()
        .features;
    assert!(!features.contains(&"mcp.v1".to_owned()), "{features:?}");
    // Nothing is mounted: the endpoint is a plain 404, the metadata document is still served.
    let (status, _, _) = off.mcp(Some(WRITE_TOKEN), &rpc(1, "ping", json!({}))).await;
    assert_eq!(status, 404);
    let (status, _) = off.get("/.well-known/oauth-protected-resource").await;
    assert_eq!(status, 200);
}
