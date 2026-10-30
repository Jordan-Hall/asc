//! A push queues code-intelligence indexing (SPEC §23.5, PLAN I13): the `UpdateRefs` outbox row
//! lists `codeintel.enqueue`, the production effect finds the files the new changesets write, the
//! workers `serve()` starts index them, and `search` over `/mcp` finds the pushed code.
//!
//! The server is the real `serve()` on a loopback socket and the commits arrive by the native
//! push. With `[codeintel]` off the same push queues nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/serve_rig.rs"]
mod rig;

use rig::{Options, REPO, WRITE_TOKEN};
use serde_json::{Value, json};
use sqlx::Row;

const LIB: &str = "pub fn zebrafish_handler() -> u32 {\n    1\n}\n";
const MAIN_RS: &str = "fn main() {\n    zebrafish_handler();\n}\n";
const EXTRA: &str = "pub fn quokka_service() -> u32 {\n    2\n}\n";

async fn count(r: &rig::Rig, table: &str) -> i64 {
    let sql = match table {
        "ci_docs" => "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM ci_docs",
        "ci_jobs" => "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM ci_jobs",
        "outbox" => "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM outbox",
        other => panic!("no count for {other}"),
    };
    sqlx::query(sql)
        .fetch_one(&r.pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap()
}

/// The `effects` documents of the repository's outbox rows, oldest first.
async fn outbox(r: &rig::Rig) -> Vec<Value> {
    sqlx::query("SELECT effects FROM outbox WHERE repo_id = $1 ORDER BY id")
        .bind(r.repo_id)
        .fetch_all(&r.pool)
        .await
        .unwrap()
        .iter()
        .map(|row| serde_json::from_slice(&row.try_get::<Vec<u8>, _>("effects").unwrap()).unwrap())
        .collect()
}

async fn delivered(r: &rig::Rig) -> bool {
    let open: i64 =
        sqlx::query("SELECT CAST(COUNT(*) AS BIGINT) AS n FROM outbox WHERE done_at IS NULL")
            .fetch_one(&r.pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
    open == 0 && count(r, "outbox").await > 0
}

/// Searches `word` over `/mcp` and reports whether `path` is among the hits.
async fn finds(r: &rig::Rig, word: &str, path: &str) -> bool {
    let v = r
        .call_tool(
            WRITE_TOKEN,
            "search",
            json!({ "repo": REPO, "query": word, "mode": "text" }),
        )
        .await;
    v["result"]["isError"] != true && v["result"].to_string().contains(path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pushed_commit_becomes_searchable_without_any_further_step() {
    let rig = rig::start(Options {
        mcp: true,
        codeintel: true,
    })
    .await;
    assert!(
        !finds(&rig, "zebrafish_handler", "src/lib.rs").await,
        "nothing is indexed yet"
    );

    let c1 = rig
        .commit(None, &[("src/lib.rs", LIB), ("src/main.rs", MAIN_RS)])
        .await;
    rig.push(None, c1).await;
    // The row the push committed lists the effect, after the forge's.
    let rows = outbox(&rig).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["effects"],
        json!(["post_receive", "codeintel.enqueue", "notify.policy"])
    );

    rig::until("the pushed files to be searchable", 90, || async {
        finds(&rig, "zebrafish_handler", "src/lib.rs").await
    })
    .await;
    assert!(finds(&rig, "zebrafish_handler", "src/main.rs").await);
    assert_eq!(count(&rig, "ci_docs").await, 2);
    rig::until("the queue to drain", 30, || async {
        count(&rig, "ci_jobs").await == 0
    })
    .await;

    // A fast-forward adds one file: it is found, and only it was new work.
    let c2 = rig.commit(Some(c1), &[("src/extra.rs", EXTRA)]).await;
    rig.push(Some(c1), c2).await;
    rig::until("the new file to be searchable", 90, || async {
        finds(&rig, "quokka_service", "src/extra.rs").await
    })
    .await;
    assert_eq!(count(&rig, "ci_docs").await, 3);
    assert!(
        finds(&rig, "zebrafish_handler", "src/lib.rs").await,
        "the old files stay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_codeintel_off_a_push_queues_nothing() {
    let rig = rig::start(Options::default()).await;
    let c1 = rig.commit(None, &[("src/lib.rs", LIB)]).await;
    rig.push(None, c1).await;
    assert_eq!(
        outbox(&rig).await[0]["effects"],
        json!(["post_receive", "codeintel.enqueue", "notify.policy"]),
        "the row is the same; the effect is a no-op"
    );
    rig::until("the outbox row to be delivered", 30, || async {
        delivered(&rig).await
    })
    .await;
    assert_eq!(count(&rig, "ci_jobs").await, 0);
    assert_eq!(count(&rig, "ci_docs").await, 0);
}
