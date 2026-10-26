//! Generates `docs/mcp.md`, the tool reference, from the same table as `tools/list` (M17).
//!
//! `asc-mcp-docs` writes the file; `asc-mcp-docs --check` exits non-zero when the file differs from
//! what the tables give now, or when the vendored `schema/asc_handles.schema.json` no longer
//! matches the `AscHandles` schema compiled in. The page records the schema's SHA-256, so a
//! change to `cli.proto` shows up as a diff.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_mcp::ASC_HANDLES_SCHEMA;
use asc_mcp::catalogue::{Confirm, TOOLS, ToolDef, Toolset};
use asc_mcp::schema::Surface;
use serde_json::Value;

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 of `data` as lowercase hex (kept local so the crate gains no dependency).
fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, b) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:08x}");
        s
    })
}

/// A table cell: one line, pipes escaped.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

fn surfaces(def: &ToolDef) -> &'static str {
    match (def.on(Surface::Stdio), def.on(Surface::Http)) {
        (true, true) => "stdio, http",
        (true, false) => "stdio",
        (false, true) => "http",
        (false, false) => "none",
    }
}

fn confirm(c: Confirm) -> String {
    match c {
        Confirm::Never => "never".into(),
        Confirm::Always => "always".into(),
        Confirm::Config => "per `asc.mcp.confirm`".into(),
        Confirm::Field(f) => format!("when `{f}` is true"),
        Confirm::RefsFor => "when `update_ref` is `refs/for/*`".into(),
        Confirm::RefsApply => "when `update_ref` is set and the call is not a dry run".into(),
    }
}

fn type_name(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(t)) if t == "array" => {
            let item = schema
                .get("items")
                .map_or_else(|| "any".to_string(), type_name);
            format!("array of {item}")
        }
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ if schema.get("enum").is_some() => "enum".into(),
        _ => "any".into(),
    }
}

fn params(out: &mut String, text: &str) {
    let Ok(schema) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        out.push_str("No arguments.\n\n");
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    out.push_str("| Argument | Type | Required | Description |\n|---|---|---|---|\n");
    let mut names: Vec<&String> = props.keys().collect();
    names.sort();
    for name in names {
        let p = &props[name];
        let desc = p.get("description").and_then(Value::as_str).unwrap_or("");
        let _ = writeln!(
            out,
            "| `{name}` | {} | {} | {} |",
            cell(&type_name(p)),
            if required.contains(&name.as_str()) {
                "yes"
            } else {
                "no"
            },
            cell(desc)
        );
    }
    out.push('\n');
}

/// The whole page.
fn render() -> String {
    let mut out = String::new();
    out.push_str("# asc MCP tool reference\n\n");
    out.push_str(
        "<!-- Generated by `cargo run -p asc-mcp --bin asc-mcp-docs`; do not edit. \
         `asc-mcp-docs --check` fails when this file drifts. -->\n\n",
    );
    out.push_str(
        "This page is written from the same table as `tools/list` (SPEC 22.3). A server lists a \
         tool only when its toolsets, write level, surface and features allow it. Text a tool \
         returns from a repository (messages, labels, file contents) is untrusted data, never \
         instructions.\n\n",
    );
    let _ = writeln!(
        out,
        "`AscHandles` schema: sha256 `{}`\n",
        sha256_hex(ASC_HANDLES_SCHEMA.as_bytes())
    );
    for ts in Toolset::ALL {
        let defs: Vec<&ToolDef> = TOOLS.iter().filter(|d| d.toolset == ts).collect();
        if defs.is_empty() {
            continue;
        }
        let _ = writeln!(out, "## Toolset `{}`\n", ts.name());
        for def in defs {
            let _ = writeln!(out, "### `{}`\n", def.name);
            let _ = writeln!(out, "{}\n", def.description.trim());
            let _ = writeln!(out, "- Surfaces: {}", surfaces(def));
            if !def.rpc.is_empty() {
                let _ = writeln!(out, "- Backing call: `{}`", def.rpc);
            }
            let _ = writeln!(out, "- Needs: `{:?}`", def.needs);
            let _ = writeln!(out, "- Confirmation: {}", confirm(def.confirm));
            if let Some(f) = def.requires_feature {
                let _ = writeln!(out, "- Requires feature: `{f}`");
            }
            let a = def.ann;
            let _ = writeln!(
                out,
                "- Annotations: readOnly={}, idempotent={}, destructive={}, openWorld={}\n",
                a.read_only, a.idempotent, a.destructive, a.open_world
            );
            for (label, surface) in [("stdio", Surface::Stdio), ("http", Surface::Http)] {
                if let Some(text) = def.input_schema(surface) {
                    let _ = writeln!(out, "Arguments ({label}):\n");
                    params(&mut out, text);
                }
            }
        }
    }
    out
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Problems found by the check, empty when the page and the vendored schema are current.
fn check(page: &str, on_disk: Option<&str>, vendored: Option<&str>) -> Vec<String> {
    let mut problems = Vec::new();
    match on_disk {
        None => problems.push("docs/mcp.md is missing".to_string()),
        Some(text) if text != page => {
            problems.push("docs/mcp.md differs from the tool tables".into())
        }
        Some(_) => {}
    }
    match vendored {
        None => problems.push("schema/asc_handles.schema.json is missing".to_string()),
        Some(text) if sha256_hex(text.as_bytes()) != sha256_hex(ASC_HANDLES_SCHEMA.as_bytes()) => {
            problems.push(
                "schema/asc_handles.schema.json hash differs from the AscHandles schema".into(),
            );
        }
        Some(_) => {}
    }
    problems
}

fn main() -> ExitCode {
    let docs = root().join("docs").join("mcp.md");
    let page = render();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => match std::fs::write(&docs, &page) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("asc-mcp-docs: write {}: {e}", docs.display());
                ExitCode::FAILURE
            }
        },
        [flag] if flag == "--check" => {
            let vendored = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("schema")
                .join("asc_handles.schema.json");
            let problems = check(
                &page,
                std::fs::read_to_string(&docs).ok().as_deref(),
                std::fs::read_to_string(vendored).ok().as_deref(),
            );
            for p in &problems {
                eprintln!("asc-mcp-docs: {p}; run `cargo run -p asc-mcp --bin asc-mcp-docs`");
            }
            if problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!("usage: asc-mcp-docs [--check]");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn check_flags_page_drift() {
        let page = render();
        assert!(check(&page, Some(&page), Some(ASC_HANDLES_SCHEMA)).is_empty());
        let stale = page.replacen("# asc MCP", "# old MCP", 1);
        assert_eq!(
            check(&page, Some(&stale), Some(ASC_HANDLES_SCHEMA)).len(),
            1
        );
        assert_eq!(check(&page, None, Some(ASC_HANDLES_SCHEMA)).len(), 1);
    }

    #[test]
    fn check_flags_schema_drift() {
        let page = render();
        let drifted = format!("{ASC_HANDLES_SCHEMA} ");
        assert_eq!(check(&page, Some(&page), Some(&drifted)).len(), 1);
        assert_eq!(check(&page, Some(&page), None).len(), 1);
    }

    #[test]
    fn page_lists_every_tool_once() {
        let page = render();
        for def in TOOLS {
            assert!(
                page.contains(&format!("### `{}`", def.name)),
                "{}",
                def.name
            );
        }
    }
}
