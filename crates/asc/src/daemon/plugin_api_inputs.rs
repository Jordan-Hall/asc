//! `Workspace.Inputs` (docs/designs/build-cache.md §1): the content identity of work-tree paths
//! for build caches, so editor and build plugins get a cache key without a child process per
//! call of their own. Like the other plugin RPCs it runs `asc inputs --json` in the mounted
//! workspace and decodes the answer, so the RPC and the CLI cannot drift.

use std::sync::Arc;

use asc_rpc::server::wire;
use bytes::Bytes;
use serde_json::Value;
use tonic::Status;

use super::plugin_api::{first_line, internal, mounted, os, path_args, run_collect};
use super::service::State;

fn hex32(v: &Value) -> Result<Bytes, Status> {
    let text = v.as_str().unwrap_or_default();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(internal("asc inputs printed a bad id"));
    }
    let digit = |b: u8| char::from(b).to_digit(16).unwrap_or(0) as u8;
    Ok(text
        .as_bytes()
        .chunks(2)
        .map(|p| digit(p[0]) << 4 | digit(p[1]))
        .collect::<Vec<u8>>()
        .into())
}

fn kind_of(name: &str) -> Result<wire::ws_input_entry::Kind, Status> {
    use wire::ws_input_entry::Kind;
    Ok(match name {
        "tree" => Kind::Tree,
        "file" => Kind::File,
        "exec" => Kind::Exec,
        "link" => Kind::Link,
        other => return Err(internal(format!("asc inputs printed kind '{other}'"))),
    })
}

/// The response of one `asc inputs --json` document.
pub(super) fn response_of(doc: &Value) -> Result<wire::WsInputsResponse, Status> {
    let entries = doc
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|e| {
            Ok(wire::WsInputEntry {
                kind: kind_of(e.get("kind").and_then(Value::as_str).unwrap_or_default())? as i32,
                id: hex32(e.get("id").unwrap_or(&Value::Null))?,
                path: Bytes::from(
                    e.get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            })
        })
        .collect::<Result<Vec<_>, Status>>()?;
    Ok(wire::WsInputsResponse {
        digest: hex32(doc.get("digest").unwrap_or(&Value::Null))?,
        entries,
    })
}

pub async fn inputs(
    state: &Arc<State>,
    r: wire::WsInputsRequest,
) -> Result<wire::WsInputsResponse, Status> {
    let m = mounted(state, &r.ws_id)?;
    let mut args = os(&["inputs", "--json"]);
    if r.allow_missing {
        args.push("--allow-missing".into());
    }
    if r.digest_only {
        args.push("--digest".into());
    }
    args.push("--".into());
    args.extend(path_args(&r.paths)?);
    let out = run_collect(&m, args, None).await?;
    let doc: Value = serde_json::from_slice(first_line(&out)?)
        .map_err(|e| internal(format!("asc printed bad JSON: {e}")))?;
    response_of(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_cli_document() {
        let id = "ab".repeat(32);
        let doc = serde_json::json!({
            "digest": "01".repeat(32),
            "entries": [
                { "kind": "tree", "id": id, "path": "app" },
                { "kind": "exec", "id": id, "path": "bin/run" },
            ],
        });
        let r = response_of(&doc).unwrap();
        assert_eq!(r.digest.as_ref(), &[1u8; 32]);
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].kind, wire::ws_input_entry::Kind::Tree as i32);
        assert_eq!(r.entries[1].kind, wire::ws_input_entry::Kind::Exec as i32);
        assert_eq!(r.entries[1].path.as_ref(), b"bin/run");
        assert_eq!(r.entries[0].id.as_ref(), &[0xab; 32]);

        let only = response_of(&serde_json::json!({ "digest": "01".repeat(32) })).unwrap();
        assert!(only.entries.is_empty());

        assert!(response_of(&serde_json::json!({ "digest": "zz" })).is_err());
        let odd = serde_json::json!({ "digest": "01".repeat(32), "entries": [{ "kind": "dir", "id": id, "path": "" }] });
        assert!(response_of(&odd).is_err());
    }
}
