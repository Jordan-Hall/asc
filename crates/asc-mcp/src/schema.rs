//! JSON Schema (2020-12) for the asc JSON profile `json.profile.v1` (SPEC 22.4.1, 22.4.2).
//!
//! The generator reads a message descriptor and prints the schema of what `asc_rpc::json::encode`
//! prints and `decode` accepts. Messages and enums become `$defs` entries reached by `$ref`, one
//! `$defs` object per schema, so every tool schema is self-contained. This file is also compiled
//! into `build.rs` (`#[path]`), so it depends on `prost-reflect` and `serde_json` only. No
//! `$schema` keyword is printed: MCP defaults to 2020-12.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use prost_reflect::{
    DescriptorPool, EnumDescriptor, ExtensionDescriptor, FieldDescriptor, Kind, MessageDescriptor,
};
use serde_json::{Map, Value, json};

/// The message package whose prefix is dropped from `$defs` names.
const PACKAGE_PREFIX: &str = "asc.v1.";
/// The `(asc.v1.json)` option the profile reads on `bytes` fields.
const JSON_OPTION: &str = "asc.v1.json";
/// The profile prints `Signature.tz_min` as `tz` (SPEC 22.4.2).
const SIGNATURE: &str = "asc.v1.Signature";
/// Longest field description kept, in characters.
const MAX_DESCRIPTION: usize = 140;
/// Enums with more values than this go to `$defs`; smaller ones are written inline.
const INLINE_ENUM_VALUES: usize = 3;

/// A schema could not be generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError(pub String);

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SchemaError {}

fn err<T>(msg: impl Into<String>) -> Result<T, SchemaError> {
    Err(SchemaError(msg.into()))
}

/// Which side of the tool the input is for (SPEC 22.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `asc mcp`: `repo` is injected, so it is hidden.
    Stdio,
    /// `asc serve /mcp`: `repo` (`"owner/name"`) is required and carries `x-mcp-header: "Repo"`.
    Http,
}

/// A field added to a tool's input or output by `tools.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    /// Field name.
    pub name: String,
    /// `string`, `integer`, `number`, `boolean` or `string[]`.
    pub ty: String,
    /// Description printed with the field.
    pub description: String,
}

/// A default or bound applied to one input field.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Limit {
    /// The field it applies to (a message field or an added field).
    pub field: String,
    /// `default`.
    pub default: Option<Value>,
    /// `maximum` for numbers.
    pub max: Option<u64>,
    /// `maxItems` for arrays.
    pub max_items: Option<u64>,
}

/// What `tools.toml` says about one tool's input.
#[derive(Debug, Clone, Copy)]
pub struct InputSpec<'a> {
    /// The surface the schema is for.
    pub surface: Surface,
    /// Request fields removed from the input.
    pub hidden: &'a [String],
    /// MCP-only fields.
    pub add: &'a [Added],
    /// Fields that must be present.
    pub required: &'a [String],
    /// Defaults and bounds.
    pub limits: &'a [Limit],
}

/// Per-schema state: the `$defs` collected so far.
struct Gen {
    ext: Option<ExtensionDescriptor>,
    defs: BTreeMap<String, Value>,
    seen: BTreeSet<String>,
    /// How many message levels below the root are expanded; deeper ones are plain objects.
    depth_limit: Option<usize>,
    depth: usize,
}

impl Gen {
    fn new(pool: &DescriptorPool) -> Self {
        Self {
            ext: pool.get_extension_by_name(JSON_OPTION),
            defs: BTreeMap::new(),
            seen: BTreeSet::new(),
            depth_limit: None,
            depth: 0,
        }
    }

    fn finish(self, mut root: Map<String, Value>) -> Value {
        if !self.defs.is_empty() {
            root.insert("$defs".into(), Value::Object(self.defs.into_iter().collect()));
        }
        Value::Object(root)
    }

    fn bytes_kind(&self, fd: &FieldDescriptor) -> i32 {
        let Some(ext) = &self.ext else { return 0 };
        let opts = fd.options();
        if opts.has_extension(ext) {
            opts.get_extension(ext).as_enum_number().unwrap_or(0)
        } else {
            0
        }
    }

    /// The schema of one value of `fd` (an element, for a list).
    fn single(&mut self, fd: &FieldDescriptor) -> Value {
        match fd.kind() {
            Kind::Bool => json!({"type": "boolean"}),
            Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Int64 | Kind::Sint64
            | Kind::Sfixed64 => json!({"type": "integer"}),
            Kind::Uint32 | Kind::Fixed32 | Kind::Uint64 | Kind::Fixed64 => {
                json!({"type": "integer", "minimum": 0})
            }
            Kind::Float | Kind::Double => json!({"type": "number"}),
            Kind::String => json!({"type": "string"}),
            Kind::Bytes => match self.bytes_kind(fd) {
                1 => json!({"type": "string", "pattern": "^([0-9a-f]{2})+$"}),
                2 => json!({"type": "string", "pattern": "^([k-z]{2})+$"}),
                3..=5 => json!({"anyOf": [
                    {"type": "string"},
                    {"type": "object", "properties": {"b64": {"type": "string"}},
                     "required": ["b64"], "additionalProperties": false}
                ]}),
                _ => json!({"type": "string", "contentEncoding": "base64"}),
            },
            Kind::Enum(ed) => self.enum_ref(&ed),
            Kind::Message(md) => self.message_ref(&md),
        }
    }

    /// The schema of the whole field: arrays for `repeated`, objects for maps.
    fn field(&mut self, fd: &FieldDescriptor) -> Value {
        if fd.is_map() {
            let Kind::Message(entry) = fd.kind() else {
                return json!({"type": "object"});
            };
            let value = entry.map_entry_value_field();
            let inner = self.single(&value);
            return json!({"type": "object", "additionalProperties": inner});
        }
        let one = self.single(fd);
        if fd.is_list() {
            json!({"type": "array", "items": one})
        } else {
            one
        }
    }

    fn enum_ref(&mut self, ed: &EnumDescriptor) -> Value {
        let names: Vec<Value> = ed
            .values()
            .map(|v| Value::String(short_name(ed, v.name())))
            .collect();
        if names.len() <= INLINE_ENUM_VALUES {
            return json!({"enum": names});
        }
        let key = def_name(ed.full_name());
        self.defs
            .entry(key.clone())
            .or_insert_with(|| json!({"enum": names}));
        json!({"$ref": format!("#/$defs/{key}")})
    }

    fn message_ref(&mut self, md: &MessageDescriptor) -> Value {
        if self.depth_limit.is_some_and(|l| self.depth >= l) {
            return json!({"type": "object"});
        }
        let key = def_name(md.full_name());
        if self.seen.insert(key.clone()) {
            // The placeholder stops a recursive message from being expanded twice.
            self.defs.insert(key.clone(), Value::Null);
            self.depth += 1;
            let body = self.message_body(md, &[], false);
            self.depth -= 1;
            self.defs.insert(key.clone(), Value::Object(body));
        }
        json!({"$ref": format!("#/$defs/{key}")})
    }

    /// The object schema of a message. `hidden` names are skipped; `describe` adds comments.
    fn message_body(
        &mut self,
        md: &MessageDescriptor,
        hidden: &[String],
        describe: bool,
    ) -> Map<String, Value> {
        let sig = md.full_name() == SIGNATURE;
        let mut props = Map::new();
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for fd in md.fields() {
            if hidden.iter().any(|h| h == fd.name()) {
                continue;
            }
            if sig && fd.name() == "tz_min" {
                props.insert(
                    "tz".into(),
                    json!({"type": "string", "pattern": "^[+-][0-9]{4}$"}),
                );
                continue;
            }
            let mut schema = self.field(&fd);
            if describe && let Some(text) = field_comment(&fd) {
                schema = with_description(schema, &text);
            }
            props.insert(fd.name().to_owned(), schema);
            if let Some(oneof) = fd.containing_oneof()
                && !oneof.is_synthetic()
            {
                let name = oneof.name().to_owned();
                match groups.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, members)) => members.push(fd.name().to_owned()),
                    None => groups.push((name, vec![fd.name().to_owned()])),
                }
            }
        }
        let mut body = Map::new();
        body.insert("type".into(), json!("object"));
        body.insert("properties".into(), Value::Object(props));
        let mut one_of_all: Vec<Value> = Vec::new();
        for (_, members) in groups.iter().filter(|(_, m)| m.len() > 1) {
            let mut alts: Vec<Value> = members.iter().map(|m| json!({"required": [m]})).collect();
            let any: Vec<Value> = members.iter().map(|m| json!({"required": [m]})).collect();
            alts.push(json!({"not": {"anyOf": any}}));
            one_of_all.push(json!({"oneOf": alts}));
        }
        match one_of_all.len() {
            0 => {}
            1 => {
                if let Some(Value::Object(only)) = one_of_all.pop() {
                    body.extend(only);
                }
            }
            _ => {
                body.insert("allOf".into(), Value::Array(one_of_all));
            }
        }
        body
    }
}

/// `asc.v1.DiffFormat` becomes `DiffFormat`; other packages keep their full name.
fn def_name(full: &str) -> String {
    full.strip_prefix(PACKAGE_PREFIX).unwrap_or(full).to_owned()
}

/// The screaming-snake prefix of an enum's values: `DiffFormat` gives `DIFF_FORMAT_`. Mirrors
/// `asc_rpc::json`; the `enum_names_match_the_codec` test fails if the two drift.
fn enum_prefix(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.extend(c.to_uppercase());
    }
    out.push('_');
    out
}

/// The profile name of an enum value: lowercase, without the type prefix.
fn short_name(ed: &EnumDescriptor, full: &str) -> String {
    let prefix = enum_prefix(ed.name());
    full.strip_prefix(&prefix)
        .filter(|r| !r.is_empty())
        .unwrap_or(full)
        .to_ascii_lowercase()
}

/// The profile names of an enum's values, in declaration order.
#[must_use]
pub fn enum_value_names(ed: &EnumDescriptor) -> Vec<String> {
    ed.values().map(|v| short_name(ed, v.name())).collect()
}

fn with_description(schema: Value, text: &str) -> Value {
    match schema {
        // A `$ref` may carry sibling keywords in 2020-12.
        Value::Object(mut m) => {
            m.insert("description".into(), Value::String(text.to_owned()));
            Value::Object(m)
        }
        other => other,
    }
}

/// The first sentence of the field's proto comment, whitespace collapsed and cut to a bound.
fn field_comment(fd: &FieldDescriptor) -> Option<String> {
    let parent = fd.parent_message().clone();
    let file = parent.parent_file();
    let fdp = file.file_descriptor_proto();
    let info = fdp.source_code_info.as_ref()?;
    let mut chain = vec![parent.clone()];
    let mut cur = parent;
    while let Some(p) = cur.parent_message() {
        chain.push(p.clone());
        cur = p;
    }
    chain.reverse();
    let mut path: Vec<i32> = Vec::new();
    let mut list = &fdp.message_type;
    let mut found = None;
    for (depth, m) in chain.iter().enumerate() {
        let idx = list.iter().position(|x| x.name() == m.name())?;
        path.push(if depth == 0 { 4 } else { 3 });
        path.push(i32::try_from(idx).ok()?);
        found = list.get(idx);
        list = &found?.nested_type;
    }
    let field_idx = found?.field.iter().position(|f| f.name() == fd.name())?;
    path.push(2);
    path.push(i32::try_from(field_idx).ok()?);
    let loc = info.location.iter().find(|l| l.path == path)?;
    let raw = if loc.leading_comments().trim().is_empty() {
        loc.trailing_comments()
    } else {
        loc.leading_comments()
    };
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = flat.trim_start_matches(['*', '/', ' ']).trim().to_owned();
    if flat.is_empty() {
        return None;
    }
    let first = match flat.find(". ") {
        Some(i) => &flat[..=i],
        None => flat.as_str(),
    };
    let cut: String = first.chars().take(MAX_DESCRIPTION).collect();
    Some(cut)
}

fn added_schema(a: &Added) -> Result<Value, SchemaError> {
    let mut v = match a.ty.as_str() {
        "string" => json!({"type": "string"}),
        "integer" => json!({"type": "integer", "minimum": 0}),
        "number" => json!({"type": "number"}),
        "boolean" => json!({"type": "boolean"}),
        "string[]" => json!({"type": "array", "items": {"type": "string"}}),
        other => return err(format!("added field {}: unknown type {other:?}", a.name)),
    };
    if !a.description.is_empty()
        && let Value::Object(m) = &mut v
    {
        m.insert("description".into(), Value::String(a.description.clone()));
    }
    Ok(v)
}

fn apply_limit(props: &mut Map<String, Value>, l: &Limit) -> Result<(), SchemaError> {
    let Some(Value::Object(field)) = props.get_mut(&l.field) else {
        return err(format!("limit names unknown input field {:?}", l.field));
    };
    if let Some(d) = &l.default {
        field.insert("default".into(), d.clone());
    }
    if let Some(m) = l.max {
        field.insert("maximum".into(), json!(m));
    }
    if let Some(m) = l.max_items {
        field.insert("maxItems".into(), json!(m));
    }
    Ok(())
}

/// The `inputSchema` of a tool: the request message projected as SPEC 22.3.1 says.
///
/// `request` is `None` for a tool whose input is only its added fields.
///
/// # Errors
/// Fails when `hidden`, `required` or a limit names a field the input does not have, or an added
/// field collides with a request field.
pub fn input_schema(
    request: Option<&MessageDescriptor>,
    spec: &InputSpec<'_>,
) -> Result<Value, SchemaError> {
    let mut generator = Gen::new(&request.map_or_else(DescriptorPool::new, |m| m.parent_pool().clone()));
    let mut props = Map::new();
    let mut body = Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut oneof_keys: Map<String, Value> = Map::new();
    if let Some(md) = request {
        for h in spec.hidden {
            if md.get_field_by_name(h).is_none() {
                return err(format!("{}: hidden field {h:?} does not exist", md.full_name()));
            }
        }
        let has_repo = md.get_field_by_name("repo").is_some();
        let mut hidden: Vec<String> = spec.hidden.to_vec();
        if has_repo {
            hidden.push("repo".into());
        }
        let mut inner = generator.message_body(md, &hidden, true);
        if let Some(Value::Object(p)) = inner.remove("properties") {
            props = p;
        }
        for k in ["oneOf", "allOf"] {
            if let Some(v) = inner.remove(k) {
                oneof_keys.insert(k.into(), v);
            }
        }
        if has_repo && spec.surface == Surface::Http {
            props.insert(
                "repo".into(),
                json!({"type": "string", "pattern": "^[^/]+/[^/]+$",
                       "description": "Repository as owner/name.", "x-mcp-header": "Repo"}),
            );
            required.push("repo".into());
        }
    }
    for a in spec.add {
        if props.contains_key(&a.name) {
            return err(format!("added field {:?} collides with an input field", a.name));
        }
        props.insert(a.name.clone(), added_schema(a)?);
    }
    for l in spec.limits {
        apply_limit(&mut props, l)?;
    }
    for r in spec.required {
        if !props.contains_key(r) {
            return err(format!("required field {r:?} is not an input field"));
        }
        if !required.contains(r) {
            required.push(r.clone());
        }
    }
    body.insert("type".into(), json!("object"));
    body.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        body.insert("required".into(), json!(required));
    }
    body.extend(oneof_keys);
    body.insert("additionalProperties".into(), json!(false));
    Ok(generator.finish(body))
}

/// Message levels an `outputSchema` expands below the response. Deeper messages are described
/// as plain objects, which every result still satisfies, so `tools/list` stays inside its token
/// budget (SPEC 22.4.1).
const OUTPUT_DEPTH: usize = 0;

/// The `outputSchema` of a tool: the response message in the profile, plus `add` fields
/// (`truncated` and `next_cursor` for shaped tools, the per-tool extras).
///
/// # Errors
/// Fails when an added field collides with a response field or has an unknown type.
pub fn output_schema(md: &MessageDescriptor, add: &[Added]) -> Result<Value, SchemaError> {
    let mut generator = Gen::new(md.parent_pool());
    generator.depth_limit = Some(OUTPUT_DEPTH);
    let mut body = generator.message_body(md, &[], false);
    if !add.is_empty() {
        let Some(Value::Object(props)) = body.get_mut("properties") else {
            return err("response schema has no properties");
        };
        for a in add {
            if props.contains_key(&a.name) {
                // The response message already has it (for example `truncated`).
                continue;
            }
            props.insert(a.name.clone(), added_schema(a)?);
        }
    }
    Ok(generator.finish(body))
}

/// The schema of a whole message, with field descriptions (used for `AscHandles`).
///
/// # Errors
/// Never fails today; the `Result` keeps the generator functions uniform.
pub fn message_schema(md: &MessageDescriptor) -> Result<Value, SchemaError> {
    let mut generator = Gen::new(md.parent_pool());
    let mut body = generator.message_body(md, &[], true);
    body.insert("additionalProperties".into(), json!(false));
    Ok(generator.finish(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> DescriptorPool {
        DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).expect("descriptor set decodes")
    }

    fn msg(name: &str) -> MessageDescriptor {
        pool().get_message_by_name(name).expect("message exists")
    }

    fn spec<'a>(surface: Surface) -> InputSpec<'a> {
        InputSpec {
            surface,
            hidden: &[],
            add: &[],
            required: &[],
            limits: &[],
        }
    }

    #[test]
    fn repo_is_hidden_on_stdio_and_required_on_http() {
        let md = msg("asc.v1.GrepRequest");
        let stdio = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        assert!(stdio["properties"].get("repo").is_none());
        assert!(stdio.get("required").is_none());
        let http = input_schema(Some(&md), &spec(Surface::Http)).unwrap();
        assert_eq!(http["properties"]["repo"]["x-mcp-header"], "Repo");
        assert_eq!(http["required"], json!(["repo"]));
    }

    #[test]
    fn hidden_fields_are_removed_and_typos_fail() {
        let md = msg("asc.v1.DiffRequest");
        let hidden = vec!["null_terminated".to_owned(), "binary".to_owned()];
        let s = InputSpec { hidden: &hidden, ..spec(Surface::Http) };
        let out = input_schema(Some(&md), &s).unwrap();
        assert!(out["properties"].get("binary").is_none());
        assert!(out["properties"].get("null_terminated").is_none());
        assert!(out["properties"].get("max_files").is_some());
        let typo = vec!["no_such_field".to_owned()];
        let s = InputSpec { hidden: &typo, ..spec(Surface::Http) };
        assert!(input_schema(Some(&md), &s).is_err());
    }

    #[test]
    fn scalar_mapping() {
        let md = msg("asc.v1.LogRequest");
        let out = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        let p = &out["properties"];
        assert_eq!(p["include"]["type"], "array");
        assert_eq!(p["include"]["items"]["type"], "string");
        assert_eq!(p["since"]["type"], "integer");
        assert_eq!(p["first_parent"]["type"], "boolean");
        assert_eq!(p["limit"]["minimum"], 0);
        assert_eq!(out["additionalProperties"], false);
    }

    #[test]
    fn bytes_kinds_follow_the_json_option() {
        let md = msg("asc.v1.ResolveChangesRequest");
        let out = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        let change = &out["properties"]["changes"]["items"];
        assert_eq!(change["pattern"], "^([k-z]{2})+$");
        let md = msg("asc.v1.ReadTreeRequest");
        let out = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        assert!(out["properties"]["path"]["anyOf"].is_array());
    }

    #[test]
    fn oneof_becomes_one_of() {
        let md = msg("asc.v1.ReadFileRequest");
        let out = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        let alts = out["oneOf"].as_array().expect("oneOf is printed");
        // One alternative per member and one for "none set".
        assert_eq!(alts.len(), 3);
        assert!(out["properties"].get("rev_path").is_some());
    }

    #[test]
    fn proto3_optional_is_not_a_oneof() {
        let md = msg("asc.v1.CreateArgs");
        let out = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        assert!(out.get("oneOf").is_none());
        assert!(out["properties"].get("onto").is_some());
    }

    #[test]
    fn enums_print_profile_names_in_defs() {
        let md = msg("asc.v1.DiffRequest");
        let out = input_schema(Some(&md), &spec(Surface::Http)).unwrap();
        let r = out["properties"]["format"]["$ref"].as_str().unwrap();
        let key = r.strip_prefix("#/$defs/").unwrap();
        let names = out["$defs"][key]["enum"].as_array().unwrap();
        assert!(names.contains(&json!("patch")), "{names:?}");
        assert!(names.iter().all(|n| n.as_str().unwrap().chars().all(|c| !c.is_uppercase())));
    }

    #[test]
    fn enum_names_match_the_codec() {
        // The generator mirrors the codec's enum naming; encoding every value must agree.
        let pool = pool();
        let md = pool.get_message_by_name("asc.v1.DiffRequest").unwrap();
        let fd = md.get_field_by_name("format").unwrap();
        let Kind::Enum(ed) = fd.kind() else { panic!("format is an enum") };
        for v in ed.values() {
            let mut m = prost_reflect::DynamicMessage::new(md.clone());
            m.set_field(&fd, prost_reflect::Value::EnumNumber(v.number()));
            let printed = asc_rpc::json::encode(&m).unwrap();
            let want = short_name(&ed, v.name());
            if v.number() != 0 {
                assert_eq!(printed["format"], want, "value {}", v.name());
            }
        }
        assert_eq!(enum_value_names(&ed).len(), ed.values().count());
    }

    #[test]
    fn messages_become_defs_and_recursion_terminates() {
        let md = msg("asc.v1.LogEntry");
        let out = message_schema(&md).unwrap();
        assert!(out["$defs"].as_object().is_some_and(|d| !d.is_empty()));
        let text = out.to_string();
        // No inline expansion: every nested message is a `$ref`.
        assert!(text.contains("\"$ref\":\"#/$defs/"));
        // Every `$ref` resolves.
        let defs = out["$defs"].as_object().unwrap();
        for part in text.split("\"$ref\":\"#/$defs/").skip(1) {
            let name = part.split('"').next().unwrap();
            assert!(defs.contains_key(name), "dangling $ref {name}");
        }
    }

    #[test]
    fn output_schemas_expand_only_the_response_level() {
        let md = msg("asc.v1.LogResponse");
        let out = output_schema(&md, &[]).unwrap();
        assert!(out.get("$defs").is_none());
        assert_eq!(out["properties"]["entries"]["items"]["type"], "object");
        assert_eq!(out["properties"]["count"]["type"], "integer");
    }

    #[test]
    fn signature_prints_tz_not_tz_min() {
        let md = msg("asc.v1.Signature");
        let out = message_schema(&md).unwrap();
        assert!(out["properties"].get("tz").is_some());
        assert!(out["properties"].get("tz_min").is_none());
    }

    #[test]
    fn added_fields_limits_and_required() {
        let md = msg("asc.v1.WsStatusRequest");
        let add = vec![Added {
            name: "max_files".into(),
            ty: "integer".into(),
            description: "Files.".into(),
        }];
        let limits = vec![Limit {
            field: "max_files".into(),
            default: Some(json!(500)),
            max: Some(5000),
            max_items: None,
        }];
        let hidden = vec!["ws_id".to_owned()];
        let s = InputSpec { hidden: &hidden, add: &add, limits: &limits, ..spec(Surface::Stdio) };
        let out = input_schema(Some(&md), &s).unwrap();
        assert_eq!(out["properties"]["max_files"]["default"], 500);
        assert_eq!(out["properties"]["max_files"]["maximum"], 5000);
        // A collision with a request field, a bad limit target and a bad required name all fail.
        let clash = vec![Added { name: "untracked".into(), ty: "boolean".into(), description: String::new() }];
        let s = InputSpec { add: &clash, ..spec(Surface::Stdio) };
        assert!(input_schema(Some(&md), &s).is_err());
        let bad = vec![Limit { field: "nope".into(), ..Limit::default() }];
        let s = InputSpec { limits: &bad, ..spec(Surface::Stdio) };
        assert!(input_schema(Some(&md), &s).is_err());
        let req = vec!["nope".to_owned()];
        let s = InputSpec { required: &req, ..spec(Surface::Stdio) };
        assert!(input_schema(Some(&md), &s).is_err());
    }

    #[test]
    fn input_only_message_with_no_request() {
        let add = vec![Added { name: "changes".into(), ty: "string[]".into(), description: String::new() }];
        let req = vec!["changes".to_owned()];
        let s = InputSpec { add: &add, required: &req, ..spec(Surface::Http) };
        let out = input_schema(None, &s).unwrap();
        assert_eq!(out["required"], json!(["changes"]));
        assert_eq!(out["properties"]["changes"]["type"], "array");
    }

    #[test]
    fn field_comments_become_descriptions() {
        let md = msg("asc.v1.CreateArgs");
        let out = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap();
        let desc = out["properties"]["message"]["description"].as_str().unwrap_or("");
        assert!(desc.contains("no editor"), "description: {desc:?}");
    }

    #[test]
    fn generation_is_deterministic() {
        let md = msg("asc.v1.LogRequest");
        let a = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap().to_string();
        let b = input_schema(Some(&md), &spec(Surface::Stdio)).unwrap().to_string();
        assert_eq!(a, b);
    }
}
