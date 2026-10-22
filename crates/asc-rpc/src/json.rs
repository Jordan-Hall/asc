//! The asc JSON profile `json.profile.v1` (SPEC 22.4.2): the one codec behind `--json` and MCP.
//!
//! It encodes and decodes a [`DynamicMessage`] by descriptor. The `(asc.v1.json)` field option picks
//! how a `bytes` field prints; decoding accepts exactly what encoding prints (enums also accept
//! the full value name). `asc api` keeps standard proto3 JSON and never uses this module.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use prost_reflect::{
    DescriptorPool, DynamicMessage, EnumDescriptor, ExtensionDescriptor, FieldDescriptor, Kind,
    MapKey, MessageDescriptor, ReflectMessage as _, Value as PbValue,
};
use serde_json::{Map, Number, Value};

/// The profile name, advertised in `Version.features` and `server/discover`.
pub const PROFILE: &str = "json.profile.v1";

const SIGNATURE: &str = "asc.v1.Signature";
const OPTION: &str = "asc.v1.json";
/// Maximum message nesting accepted in either direction.
const MAX_DEPTH: usize = 64;
/// Largest integer a JSON number carries exactly.
const MAX_SAFE: u64 = 1 << 53;

/// A profile encode or decode failure; `path` is the dotted location in the document.
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    /// A value does not fit the field it is for.
    #[error("{path}: {reason}")]
    Invalid {
        /// Location of the bad value.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// Messages nest deeper than the bound.
    #[error("message nesting exceeds {MAX_DEPTH}")]
    TooDeep,
}

impl JsonError {
    fn new(reason: impl Into<String>) -> Self {
        Self::Invalid {
            path: String::new(),
            reason: reason.into(),
        }
    }

    fn at(self, seg: &str) -> Self {
        match self {
            Self::Invalid { path, reason } => Self::Invalid {
                path: if path.is_empty() {
                    seg.to_owned()
                } else {
                    format!("{seg}.{path}")
                },
                reason,
            },
            other => other,
        }
    }
}

type Result<T> = std::result::Result<T, JsonError>;

/// How a `bytes` field prints (the `(asc.v1.json)` option).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BytesKind {
    Plain,
    Hex,
    Change,
    Path,
    Text,
    Data,
}

/// Per-call state: the option descriptor and a cache of each field's kind.
struct Ctx {
    ext: Option<ExtensionDescriptor>,
    kinds: HashMap<String, BytesKind>,
}

impl Ctx {
    fn new(pool: &DescriptorPool) -> Self {
        Self {
            ext: pool.get_extension_by_name(OPTION),
            kinds: HashMap::new(),
        }
    }

    fn kind(&mut self, fd: &FieldDescriptor) -> BytesKind {
        let Some(ext) = &self.ext else {
            return BytesKind::Plain;
        };
        if let Some(k) = self.kinds.get(fd.full_name()) {
            return *k;
        }
        let opts = fd.options();
        let k = if opts.has_extension(ext) {
            match opts.get_extension(ext).as_enum_number() {
                Some(1) => BytesKind::Hex,
                Some(2) => BytesKind::Change,
                Some(3) => BytesKind::Path,
                Some(4) => BytesKind::Text,
                Some(5) => BytesKind::Data,
                _ => BytesKind::Plain,
            }
        } else {
            BytesKind::Plain
        };
        self.kinds.insert(fd.full_name().to_owned(), k);
        k
    }
}

/// Encodes `msg` as a JSON value in the profile.
///
/// # Errors
/// Fails on a 64-bit integer above 2^53, a non-finite float, or nesting beyond the bound.
pub fn encode(msg: &DynamicMessage) -> Result<Value> {
    let mut ctx = Ctx::new(msg.descriptor().parent_pool());
    encode_message(&mut ctx, msg, 0).map(Value::Object)
}

/// Decodes a JSON value in the profile into a message of type `desc`.
///
/// # Errors
/// Fails on an unknown key, a value of the wrong type or range, or nesting beyond the bound.
pub fn decode(desc: &MessageDescriptor, value: &Value) -> Result<DynamicMessage> {
    let mut ctx = Ctx::new(desc.parent_pool());
    decode_message(&mut ctx, desc, value, 0)
}

fn is_signature(d: &MessageDescriptor) -> bool {
    d.full_name() == SIGNATURE
}

fn tz_string(min: i32) -> String {
    let abs = min.unsigned_abs();
    let sign = if min < 0 { '-' } else { '+' };
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

fn parse_tz(s: &str) -> Result<i32> {
    let b = s.as_bytes();
    if b.len() != 5 || !(b[0] == b'+' || b[0] == b'-') || !b[1..].iter().all(u8::is_ascii_digit) {
        return Err(JsonError::new("tz must look like +0530"));
    }
    let n = |i: usize| i32::from(b[i] - b'0') * 10 + i32::from(b[i + 1] - b'0');
    let (h, m) = (n(1), n(3));
    if m >= 60 {
        return Err(JsonError::new("tz minutes must be below 60"));
    }
    let v = h * 60 + m;
    Ok(if b[0] == b'-' { -v } else { v })
}

fn encode_message(ctx: &mut Ctx, msg: &DynamicMessage, depth: usize) -> Result<Map<String, Value>> {
    if depth > MAX_DEPTH {
        return Err(JsonError::TooDeep);
    }
    let desc = msg.descriptor();
    let sig = is_signature(&desc);
    let mut out = Map::new();
    for fd in desc.fields() {
        if sig && fd.name() == "tz_min" {
            let min = msg.get_field(&fd).as_i32().unwrap_or(0);
            out.insert("tz".into(), Value::String(tz_string(min)));
            continue;
        }
        if !sig && !msg.has_field(&fd) {
            continue;
        }
        let v = msg.get_field(&fd);
        if ctx.kind(&fd) == BytesKind::Data
            && !fd.is_list()
            && !fd.is_map()
            && v.as_bytes().is_some_and(|b| is_binary(b))
        {
            out.entry("binary").or_insert(Value::Bool(true));
            continue;
        }
        let j = encode_field(ctx, &fd, &v, depth).map_err(|e| e.at(fd.name()))?;
        out.insert(fd.name().to_owned(), j);
    }
    Ok(out)
}

fn is_binary(b: &[u8]) -> bool {
    b.contains(&0) || std::str::from_utf8(b).is_err()
}

fn encode_field(ctx: &mut Ctx, fd: &FieldDescriptor, v: &PbValue, depth: usize) -> Result<Value> {
    if fd.is_map() {
        let Kind::Message(entry) = fd.kind() else {
            return Err(JsonError::new("map field without entry type"));
        };
        let vf = entry.map_entry_value_field();
        let mut out = Map::new();
        if let Some(map) = v.as_map() {
            for (k, val) in map {
                let key = match k {
                    MapKey::Bool(b) => b.to_string(),
                    MapKey::I32(n) => n.to_string(),
                    MapKey::I64(n) => n.to_string(),
                    MapKey::U32(n) => n.to_string(),
                    MapKey::U64(n) => n.to_string(),
                    MapKey::String(s) => s.clone(),
                };
                let j = encode_single(ctx, &vf, val, depth).map_err(|e| e.at(&key))?;
                out.insert(key, j);
            }
        }
        return Ok(Value::Object(out));
    }
    if fd.is_list() {
        let mut out = Vec::new();
        if let Some(list) = v.as_list() {
            for (i, item) in list.iter().enumerate() {
                out.push(encode_single(ctx, fd, item, depth).map_err(|e| e.at(&i.to_string()))?);
            }
        }
        return Ok(Value::Array(out));
    }
    encode_single(ctx, fd, v, depth)
}

fn int(n: i64) -> Result<Value> {
    if n.unsigned_abs() > MAX_SAFE {
        return Err(JsonError::new("integer exceeds 2^53"));
    }
    Ok(Value::Number(n.into()))
}

fn float(f: f64) -> Result<Value> {
    Number::from_f64(f)
        .map(Value::Number)
        .ok_or_else(|| JsonError::new("non-finite float"))
}

fn encode_single(ctx: &mut Ctx, fd: &FieldDescriptor, v: &PbValue, depth: usize) -> Result<Value> {
    Ok(match v {
        PbValue::Bool(b) => Value::Bool(*b),
        PbValue::I32(n) => Value::Number((*n).into()),
        PbValue::U32(n) => Value::Number((*n).into()),
        PbValue::I64(n) => int(*n)?,
        PbValue::U64(n) => {
            if *n > MAX_SAFE {
                return Err(JsonError::new("integer exceeds 2^53"));
            }
            Value::Number((*n).into())
        }
        PbValue::F32(f) => float(f64::from(*f))?,
        PbValue::F64(f) => float(*f)?,
        PbValue::String(s) => Value::String(s.clone()),
        PbValue::Bytes(b) => encode_bytes(ctx.kind(fd), b),
        PbValue::EnumNumber(n) => match fd.kind() {
            Kind::Enum(ed) => enum_name(&ed, *n),
            _ => Value::Number((*n).into()),
        },
        PbValue::Message(m) => Value::Object(encode_message(ctx, m, depth + 1)?),
        PbValue::List(_) | PbValue::Map(_) => return Err(JsonError::new("nested collection")),
    })
}

fn encode_bytes(kind: BytesKind, b: &[u8]) -> Value {
    match kind {
        BytesKind::Plain => Value::String(B64.encode(b)),
        BytesKind::Hex => Value::String(hex_encode(b)),
        BytesKind::Change => Value::String(change_encode(b)),
        BytesKind::Path | BytesKind::Text | BytesKind::Data => match std::str::from_utf8(b) {
            Ok(s) => Value::String(s.to_owned()),
            Err(_) => {
                let mut m = Map::new();
                m.insert("b64".into(), Value::String(B64.encode(b)));
                Value::Object(m)
            }
        },
    }
}

fn hex_encode(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(char::from(D[usize::from(x >> 4)]));
        s.push(char::from(D[usize::from(x & 15)]));
    }
    s
}

/// A hex digit `d` becomes `'z' - d` (SPEC 20.2).
fn change_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(char::from(b'z' - (x >> 4)));
        s.push(char::from(b'z' - (x & 15)));
    }
    s
}

/// The screaming-snake prefix of an enum's values: `DiffFormat` gives `DIFF_FORMAT_`.
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

fn short_name(ed: &EnumDescriptor, full: &str) -> String {
    let prefix = enum_prefix(ed.name());
    full.strip_prefix(&prefix)
        .filter(|r| !r.is_empty())
        .unwrap_or(full)
        .to_ascii_lowercase()
}

fn enum_name(ed: &EnumDescriptor, n: i32) -> Value {
    match ed.get_value(n) {
        Some(v) => Value::String(short_name(ed, v.name())),
        None => Value::Number(n.into()),
    }
}

fn decode_message(
    ctx: &mut Ctx,
    desc: &MessageDescriptor,
    value: &Value,
    depth: usize,
) -> Result<DynamicMessage> {
    if depth > MAX_DEPTH {
        return Err(JsonError::TooDeep);
    }
    let Value::Object(obj) = value else {
        return Err(JsonError::new("expected an object"));
    };
    let sig = is_signature(desc);
    let mut msg = DynamicMessage::new(desc.clone());
    for (key, j) in obj {
        if sig && key == "tz" {
            let s = j
                .as_str()
                .ok_or_else(|| JsonError::new("tz must be a string").at(key))?;
            let min = parse_tz(s).map_err(|e| e.at(key))?;
            if let Some(fd) = desc.get_field_by_name("tz_min") {
                msg.set_field(&fd, PbValue::I32(min));
            }
            continue;
        }
        let fd = desc
            .get_field_by_name(key)
            .filter(|f| !(sig && f.name() == "tz_min"));
        let Some(fd) = fd else {
            if key == "binary" && j.is_boolean() && has_data_field(ctx, desc) {
                continue;
            }
            return Err(JsonError::new("unknown field").at(key));
        };
        if j.is_null() {
            continue;
        }
        let v = decode_field(ctx, &fd, j, depth).map_err(|e| e.at(key))?;
        msg.set_field(&fd, v);
    }
    Ok(msg)
}

fn has_data_field(ctx: &mut Ctx, desc: &MessageDescriptor) -> bool {
    desc.fields().any(|f| ctx.kind(&f) == BytesKind::Data)
}

fn decode_field(ctx: &mut Ctx, fd: &FieldDescriptor, j: &Value, depth: usize) -> Result<PbValue> {
    if fd.is_map() {
        let Kind::Message(entry) = fd.kind() else {
            return Err(JsonError::new("map field without entry type"));
        };
        let (kf, vf) = (entry.map_entry_key_field(), entry.map_entry_value_field());
        let Value::Object(obj) = j else {
            return Err(JsonError::new("expected an object"));
        };
        let mut map = HashMap::with_capacity(obj.len());
        for (k, val) in obj {
            let key = parse_map_key(&kf.kind(), k).map_err(|e| e.at(k))?;
            let v = decode_single(ctx, &vf, val, depth).map_err(|e| e.at(k))?;
            map.insert(key, v);
        }
        return Ok(PbValue::Map(map));
    }
    if fd.is_list() {
        let Value::Array(arr) = j else {
            return Err(JsonError::new("expected an array"));
        };
        let mut list = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            list.push(decode_single(ctx, fd, item, depth).map_err(|e| e.at(&i.to_string()))?);
        }
        return Ok(PbValue::List(list));
    }
    decode_single(ctx, fd, j, depth)
}

fn parse_map_key(kind: &Kind, k: &str) -> Result<MapKey> {
    fn p<T: std::str::FromStr>(k: &str) -> Result<T> {
        k.parse().map_err(|_| JsonError::new("bad map key"))
    }
    Ok(match kind {
        Kind::String => MapKey::String(k.to_owned()),
        Kind::Bool => MapKey::Bool(p(k)?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => MapKey::I32(p(k)?),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => MapKey::I64(p(k)?),
        Kind::Uint32 | Kind::Fixed32 => MapKey::U32(p(k)?),
        Kind::Uint64 | Kind::Fixed64 => MapKey::U64(p(k)?),
        _ => return Err(JsonError::new("unsupported map key type")),
    })
}

fn want_i64(j: &Value) -> Result<i64> {
    j.as_i64()
        .ok_or_else(|| JsonError::new("expected an integer"))
}

fn want_u64(j: &Value) -> Result<u64> {
    j.as_u64()
        .ok_or_else(|| JsonError::new("expected an unsigned integer"))
}

fn want_f64(j: &Value) -> Result<f64> {
    j.as_f64()
        .ok_or_else(|| JsonError::new("expected a number"))
}

fn want_str(j: &Value) -> Result<&str> {
    j.as_str()
        .ok_or_else(|| JsonError::new("expected a string"))
}

fn decode_single(ctx: &mut Ctx, fd: &FieldDescriptor, j: &Value, depth: usize) -> Result<PbValue> {
    let range = |_| JsonError::new("integer out of range");
    Ok(match fd.kind() {
        Kind::Bool => PbValue::Bool(
            j.as_bool()
                .ok_or_else(|| JsonError::new("expected a bool"))?,
        ),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            PbValue::I32(want_i64(j)?.try_into().map_err(range)?)
        }
        Kind::Uint32 | Kind::Fixed32 => PbValue::U32(want_u64(j)?.try_into().map_err(range)?),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => PbValue::I64(want_i64(j)?),
        Kind::Uint64 | Kind::Fixed64 => PbValue::U64(want_u64(j)?),
        #[allow(clippy::cast_possible_truncation)]
        Kind::Float => PbValue::F32(want_f64(j)? as f32),
        Kind::Double => PbValue::F64(want_f64(j)?),
        Kind::String => PbValue::String(want_str(j)?.to_owned()),
        Kind::Bytes => PbValue::Bytes(decode_bytes(ctx.kind(fd), j)?.into()),
        Kind::Enum(ed) => PbValue::EnumNumber(decode_enum(&ed, j)?),
        Kind::Message(md) => PbValue::Message(decode_message(ctx, &md, j, depth + 1)?),
    })
}

fn decode_enum(ed: &EnumDescriptor, j: &Value) -> Result<i32> {
    match j {
        Value::Number(n) => n
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| JsonError::new("enum number out of range")),
        Value::String(s) => ed
            .values()
            .find(|v| v.name() == s || short_name(ed, v.name()) == *s)
            .map(|v| v.number())
            .ok_or_else(|| JsonError::new(format!("unknown {} value", ed.name()))),
        _ => Err(JsonError::new("expected an enum name")),
    }
}

fn decode_bytes(kind: BytesKind, j: &Value) -> Result<Vec<u8>> {
    let b64 = |s: &str| B64.decode(s).map_err(|_| JsonError::new("invalid base64"));
    match kind {
        BytesKind::Plain => b64(want_str(j)?),
        BytesKind::Hex => hex_decode(want_str(j)?),
        BytesKind::Change => change_decode(want_str(j)?),
        BytesKind::Path | BytesKind::Text | BytesKind::Data => match j {
            Value::String(s) => Ok(s.as_bytes().to_vec()),
            Value::Object(o) if o.len() == 1 => b64(o
                .get("b64")
                .and_then(Value::as_str)
                .ok_or_else(|| JsonError::new("expected {\"b64\": ...}"))?),
            _ => Err(JsonError::new("expected a string or {\"b64\": ...}")),
        },
    }
}

fn nibbles(s: &str, digit: impl Fn(u8) -> Option<u8>) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return Err(JsonError::new("odd length"));
    }
    b.chunks_exact(2)
        .map(|p| match (digit(p[0]), digit(p[1])) {
            (Some(h), Some(l)) => Ok(h << 4 | l),
            _ => Err(JsonError::new("invalid digit")),
        })
        .collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    nibbles(s, |c| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    })
}

fn change_decode(s: &str) -> Result<Vec<u8>> {
    nibbles(s, |c| (b'k'..=b'z').contains(&c).then(|| b'z' - c))
}

#[cfg(test)]
mod tests;
