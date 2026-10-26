//! Generates the tool table from `tools.toml` and `asc_rpc::DESCRIPTOR_SET` (SPEC 22.4.1).
//!
//! `OUT_DIR/tools_gen.rs` holds a static `TOOLS` table: for each tool its metadata and its
//! `inputSchema` (one per surface) and `outputSchema` as `&'static str` JSON. The same run writes
//! `schema/asc_handles.schema.json` (the `AscHandles` message of `cli.proto`, SPEC 22.3.4) next to
//! the crate sources, where a test and the M17 drift check read it.
//!
//! A bad row fails the build with the tool name and the reason, so a typo in `hidden`, `limits`
//! or a message name never reaches `tools/list`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use prost_reflect::DescriptorPool;
use serde::Deserialize;
use serde_json::{Value, json};

#[path = "src/schema.rs"]
#[allow(dead_code)]
mod schema;

use schema::{Added, InputSpec, Limit, Surface};

type Failure = Box<dyn std::error::Error>;

const TOOLSETS: &[&str] = &["core", "history", "stack", "projects", "search", "write"];
const ANNOTATIONS: &[&str] = &["read_only", "idempotent", "destructive", "open_world"];
const NEEDS: &[(&str, &str)] = &[
    ("none", "None"),
    ("write_local", "WriteLocal"),
    ("write_remote", "WriteRemote"),
    ("write_local_or_remote", "WriteLocalOrRemote"),
    ("server_write", "ServerWrite"),
    ("server_write_mcp", "ServerWriteMcp"),
];
const MAX_TOKENS_DOC: &str = "Cap on the reply size in tokens (estimated as bytes / 3).";
const CURSOR_DOC: &str = "Continuation from a previous reply's next_cursor.";
const WORKTREE_DOC: &str = "Worktree path from the worktrees tool; omit for the launch workspace.";

#[derive(Deserialize)]
struct File {
    tool: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddedRow {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitRow {
    field: String,
    default: Option<toml::Value>,
    max: Option<u64>,
    max_items: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    name: String,
    toolset: String,
    surfaces: Vec<String>,
    rpc: String,
    request: String,
    response: String,
    description: String,
    #[serde(default)]
    hidden: Vec<String>,
    #[serde(default)]
    add: Vec<AddedRow>,
    #[serde(default)]
    add_out: Vec<AddedRow>,
    #[serde(default)]
    limits: Vec<LimitRow>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    shaped: bool,
    #[serde(default)]
    worktree: bool,
    #[serde(default)]
    ann: Vec<String>,
    confirm: Option<String>,
    needs: Option<String>,
    requires_feature: Option<String>,
}

fn main() -> Result<(), Failure> {
    println!("cargo:rerun-if-changed=tools.toml");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/schema.rs");
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or("no CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("no OUT_DIR")?);
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET)?;
    let text = std::fs::read_to_string(manifest.join("tools.toml"))?;
    let file: File = toml::from_str(&text)?;

    let mut seen: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut table = String::from("pub static TOOLS: &[ToolDef] = &[\n");
    for row in &file.tool {
        let one =
            emit_row(&pool, row, &mut seen).map_err(|e| format!("tool {:?}: {e}", row.name))?;
        table.push_str(&one);
    }
    table.push_str("];\n");

    let handles = pool
        .get_message_by_name("asc.v1.AscHandles")
        .ok_or("cli.proto has no AscHandles")?;
    let schema_json = schema::message_schema(&handles)?;
    let mut pretty = serde_json::to_string_pretty(&schema_json)?;
    pretty.push('\n');
    let _ = writeln!(table, "pub const ASC_HANDLES_SCHEMA: &str = {pretty:?};");
    std::fs::write(out.join("tools_gen.rs"), table)?;
    vendor(
        &manifest.join("schema").join("asc_handles.schema.json"),
        &pretty,
    );
    Ok(())
}

/// Writes the vendored schema when it differs. A read-only checkout only warns: the embedded copy
/// is what the crate serves, and the drift test reports the difference.
fn vendor(path: &std::path::Path, content: &str) {
    if std::fs::read_to_string(path).is_ok_and(|old| old.replace("\r\n", "\n") == content) {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(path.parent().unwrap_or(path))
        .and_then(|()| std::fs::write(path, content))
    {
        println!("cargo:warning=cannot write {}: {e}", path.display());
    }
}

fn added(rows: &[AddedRow]) -> Vec<Added> {
    rows.iter()
        .map(|a| Added {
            name: a.name.clone(),
            ty: a.ty.clone(),
            description: a.description.clone(),
        })
        .collect()
}

fn to_json(v: &toml::Value) -> Result<Value, Failure> {
    Ok(match v {
        toml::Value::Integer(i) => json!(i),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Float(f) => json!(f),
        toml::Value::String(s) => json!(s),
        other => return Err(format!("unsupported default {other}").into()),
    })
}

fn emit_row(
    pool: &DescriptorPool,
    row: &Row,
    seen: &mut BTreeSet<(String, &'static str)>,
) -> Result<String, Failure> {
    if row.name.is_empty()
        || !row
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_')
    {
        return Err("name must match [a-z_]+".into());
    }
    if !TOOLSETS.contains(&row.toolset.as_str()) {
        return Err(format!("unknown toolset {:?}", row.toolset).into());
    }
    let mut mask = 0u8;
    let mut surfaces = Vec::new();
    for s in &row.surfaces {
        let (bit, surface, key) = match s.as_str() {
            "stdio" => (1, Surface::Stdio, "stdio"),
            "http" => (2, Surface::Http, "http"),
            other => return Err(format!("unknown surface {other:?}").into()),
        };
        if !seen.insert((row.name.clone(), key)) {
            return Err(format!("duplicate tool on surface {key}").into());
        }
        mask |= bit;
        surfaces.push(surface);
    }
    if mask == 0 {
        return Err("no surface".into());
    }
    for a in &row.ann {
        if !ANNOTATIONS.contains(&a.as_str()) {
            return Err(format!("unknown annotation {a:?}").into());
        }
    }
    let needs = row.needs.as_deref().unwrap_or("none");
    let needs_variant = NEEDS
        .iter()
        .find(|(k, _)| *k == needs)
        .map(|(_, v)| *v)
        .ok_or_else(|| format!("unknown needs {needs:?}"))?;
    if row.toolset == "write" && needs == "none" {
        return Err("a write tool must declare needs".into());
    }
    if row.toolset != "write" && needs != "none" {
        return Err("only write tools declare needs".into());
    }
    if row.toolset != "write" && !(row.ann.len() == 1 && row.ann[0] == "read_only") {
        return Err("a read tool must be annotated read_only and nothing else".into());
    }

    let request = if row.request.is_empty() {
        None
    } else {
        Some(
            pool.get_message_by_name(&row.request)
                .ok_or_else(|| format!("unknown request message {}", row.request))?,
        )
    };
    let response = pool
        .get_message_by_name(&row.response)
        .ok_or_else(|| format!("unknown response message {}", row.response))?;

    let mut add_in = added(&row.add);
    if row.shaped {
        add_in.push(Added {
            name: "max_tokens".into(),
            ty: "integer".into(),
            description: MAX_TOKENS_DOC.into(),
        });
        add_in.push(Added {
            name: "cursor".into(),
            ty: "string".into(),
            description: CURSOR_DOC.into(),
        });
    }
    let mut limits = Vec::new();
    for l in &row.limits {
        limits.push(Limit {
            field: l.field.clone(),
            default: l.default.as_ref().map(to_json).transpose()?,
            max: l.max,
            max_items: l.max_items,
        });
    }
    let mut add_out = added(&row.add_out);
    if row.shaped {
        add_out.push(Added {
            name: "truncated".into(),
            ty: "boolean".into(),
            description: "True when the reply was cut; see next_cursor.".into(),
        });
        add_out.push(Added {
            name: "next_cursor".into(),
            ty: "string".into(),
            description: "Pass as cursor to continue.".into(),
        });
    }

    let confirm = match row.confirm.as_deref().unwrap_or("never") {
        "never" => "Confirm::Never".to_owned(),
        "always" => "Confirm::Always".to_owned(),
        "config" => "Confirm::Config".to_owned(),
        "refs_for" => "Confirm::RefsFor".to_owned(),
        "refs_apply" => "Confirm::RefsApply".to_owned(),
        other => match other.strip_prefix("field:") {
            Some(f)
                if add_in.iter().any(|a| a.name == f)
                    || request
                        .as_ref()
                        .is_some_and(|m| m.get_field_by_name(f).is_some()) =>
            {
                format!("Confirm::Field({f:?})")
            }
            _ => return Err(format!("bad confirm rule {other:?}").into()),
        },
    };

    let mut inputs = [String::new(), String::new()];
    for (slot, surface) in [(0usize, Surface::Stdio), (1, Surface::Http)] {
        if !surfaces.contains(&surface) {
            continue;
        }
        let mut add = add_in.clone();
        if row.worktree && surface == Surface::Stdio {
            add.push(Added {
                name: "worktree".into(),
                ty: "string".into(),
                description: WORKTREE_DOC.into(),
            });
        }
        let spec = InputSpec {
            surface,
            hidden: &row.hidden,
            add: &add,
            required: &row.required,
            limits: &limits,
        };
        let input = schema::input_schema(request.as_ref(), &spec)?;
        inputs[slot] = serde_json::to_string(&input)?;
    }
    let output = serde_json::to_string(&schema::output_schema(&response, &add_out)?)?;

    let ann = |k: &str| row.ann.iter().any(|a| a == k);
    let feature = match &row.requires_feature {
        Some(f) => format!("Some({f:?})"),
        None => "None".to_owned(),
    };
    let toolset: String = {
        let mut c = row.toolset.chars();
        c.next()
            .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
            .unwrap_or_default()
    };
    Ok(format!(
        "    ToolDef {{ name: {:?}, toolset: Toolset::{toolset}, surfaces: {mask}, rpc: {:?}, \
         request: {:?}, response: {:?}, description: {:?}, \
         ann: Ann {{ read_only: {}, idempotent: {}, destructive: {}, open_world: {} }}, \
         confirm: {confirm}, needs: Needs::{needs_variant}, requires_feature: {feature}, \
         input_stdio: {:?}, input_http: {:?}, output: {output:?} }},\n",
        row.name,
        row.rpc,
        row.request,
        row.response,
        row.description,
        ann("read_only"),
        ann("idempotent") || ann("read_only"),
        ann("destructive"),
        ann("open_world"),
        inputs[0],
        inputs[1],
    ))
}
