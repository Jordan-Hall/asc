//! `asc api <Rpc> [--json <request>|-]` (SPEC §7): calls any public `asc.v1` RPC with the user's
//! credentials and prints the answer as standard proto3 JSON (not the §22.4.2 profile), so a
//! script needs no stubs. The request is a JSON object read from `--json` or, with `-`, from
//! standard input. `Repo` and `Projects` are the public services; the request's `repo` is filled
//! in from the remote when the request leaves it out.
//!
//! The call goes over `prost-reflect` and the `asc.v1` descriptor set built into `asc-rpc`: the
//! request is decoded by descriptor (unknown fields are an error), sent through a codec that
//! carries `DynamicMessage`s, and every answer is encoded by descriptor. Server streams print
//! one JSON object per line. Request and answer sizes are bounded; client-streaming RPCs
//! (`PutObjects`, `GetObjects`) are not available here. `--list` names the RPCs and `--docs`
//! prints the markdown of `docs/api.md`, built from the proto comments in the descriptor set.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::OnceLock;

use asc_rpc::server::MAX_MESSAGE;
use bytes::Buf;
use prost_reflect::prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, EnumDescriptor, FieldDescriptor, Kind, MapKey,
    MessageDescriptor, MethodDescriptor, ReflectMessage as _, Value as Pb,
};
use serde_json::{Map, Number, Value};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::http::uri::PathAndQuery;

use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc api <Rpc> [--json <request>|-] [--remote <name|url>]
   or: asc api --list
   or: asc api --docs
   or: asc api --stdio";

/// Largest request read from `--json -`.
const MAX_REQUEST: u64 = 16 * 1024 * 1024;
/// Most messages printed for one server stream.
const MAX_STREAM: usize = 1_000_000;
/// Deepest message nesting in a request or answer.
const MAX_DEPTH: usize = 64;
/// The services `asc api` serves: the public server API (the daemon's `Workspace` is local).
const SERVICES: &[&str] = &[
    "asc.v1.Repo",
    "asc.v1.Projects",
    "asc.v1.Changes",
    "asc.v1.Cloud",
    "asc.v1.CodeIntel",
    "asc.v1.Inbox",
    "asc.v1.Review",
    "asc.v1.Ci",
];

#[derive(clap::Parser, Debug)]
#[command(name = "api", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "api",
    flags: &["--json", "--remote", "--list", "--docs", "--stdio"],
    placeholders: &[],
}];

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(format!("asc: {}", text.into()))
}

#[derive(Default)]
struct Opts {
    rpc: Option<String>,
    json: Option<String>,
    remote: Option<String>,
    list: bool,
    docs: bool,
    stdio: bool,
}

fn parse(argv: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = argv.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => o.list = true,
            "--docs" => o.docs = true,
            "--stdio" => o.stdio = true,
            "--json" => o.json = Some(it.next().ok_or_else(usage)?),
            "--remote" => o.remote = Some(it.next().ok_or_else(usage)?),
            w if w.starts_with("--json=") => o.json = Some(w["--json=".len()..].to_owned()),
            w if w.starts_with("--remote=") => o.remote = Some(w["--remote=".len()..].to_owned()),
            "-" if o.rpc.is_some() && o.json.is_none() => o.json = Some("-".to_owned()),
            w if w.starts_with('-') && w != "-" => return Err(usage()),
            w => {
                if o.rpc.replace(w.to_owned()).is_some() {
                    return Err(usage());
                }
            }
        }
    }
    Ok(o)
}

// ------------------------------------------------------------------------------ descriptors

fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET)
            .expect("the built-in descriptor set decodes")
    })
}

fn public_methods() -> Vec<MethodDescriptor> {
    let pool = pool();
    let mut out = Vec::new();
    for name in SERVICES {
        if let Some(svc) = pool.get_service_by_name(name) {
            out.extend(svc.methods());
        }
    }
    out
}

/// Finds `Log`, `Repo.Log`, `Repo/Log` or `asc.v1.Repo/Log`.
fn find_method(word: &str) -> Result<MethodDescriptor> {
    let (service, name) = match word.rsplit_once(['/', '.']) {
        Some((s, n)) => (Some(s.trim_start_matches('/')), n),
        None => (None, word),
    };
    let found: Vec<MethodDescriptor> = public_methods()
        .into_iter()
        .filter(|m| m.name() == name)
        .filter(|m| {
            service.is_none_or(|s| {
                let full = m.parent_service().full_name().to_owned();
                full == s || m.parent_service().name() == s
            })
        })
        .collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(fatal(format!(
            "unknown RPC '{}'; 'asc api --list' names them",
            word.chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect::<String>()
        ))),
        many => Err(fatal(format!(
            "'{word}' is ambiguous: {}; name the service",
            many.iter()
                .map(|m| format!("{}/{}", m.parent_service().name(), m.name()))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn list() -> String {
    let mut out = String::new();
    for m in public_methods() {
        let stream = match (m.is_client_streaming(), m.is_server_streaming()) {
            (false, false) => "",
            (false, true) => " (server stream)",
            (true, false) => " (client stream)",
            (true, true) => " (bidirectional stream)",
        };
        out.push_str(&format!(
            "{}/{}  {} -> {}{stream}\n",
            m.parent_service().name(),
            m.name(),
            m.input().name(),
            m.output().name()
        ));
    }
    out
}

/// The markdown of `docs/api.md`: every public RPC with the comment above it in the proto.
fn docs() -> String {
    let pool = pool();
    let mut comments: HashMap<(String, Vec<i32>), String> = HashMap::new();
    for file in pool.files() {
        let proto = file.file_descriptor_proto();
        let Some(info) = &proto.source_code_info else {
            continue;
        };
        for loc in &info.location {
            let text = loc
                .leading_comments
                .clone()
                .or_else(|| loc.trailing_comments.clone())
                .unwrap_or_default();
            if !text.trim().is_empty() {
                comments.insert((file.name().to_owned(), loc.path.clone()), text);
            }
        }
    }
    let mut out = String::from(
        "# asc.v1 API\n\nGenerated by `asc api --docs` from the proto comments. Call any RPC with `asc api <Rpc> --json <request>`.\n",
    );
    for name in SERVICES {
        let Some(svc) = pool.get_service_by_name(name) else {
            continue;
        };
        out.push_str(&format!("\n## {}\n\n", svc.full_name()));
        let file = svc.parent_file();
        let svc_index = file
            .services()
            .position(|s| s.full_name() == svc.full_name());
        for (i, m) in svc.methods().enumerate() {
            let stream = match (m.is_client_streaming(), m.is_server_streaming()) {
                (false, false) => "unary",
                (false, true) => "server stream",
                (true, false) => "client stream",
                (true, true) => "bidirectional",
            };
            out.push_str(&format!(
                "### {}\n\n`{}` -> `{}` ({stream})\n\n",
                m.name(),
                m.input().full_name(),
                m.output().full_name()
            ));
            let path = svc_index.map(|s| vec![6, s as i32, 2, i as i32]);
            if let Some(text) = path.and_then(|p| comments.get(&(file.name().to_owned(), p))) {
                for line in text.lines() {
                    out.push_str(line.trim());
                    out.push('\n');
                }
                out.push('\n');
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------ proto3 JSON

#[derive(Debug, thiserror::Error)]
enum JsonError {
    #[error("{path}: {reason}")]
    Bad { path: String, reason: String },
}

type JResult<T> = std::result::Result<T, JsonError>;

fn bad(reason: impl Into<String>) -> JsonError {
    JsonError::Bad {
        path: String::new(),
        reason: reason.into(),
    }
}

fn at(e: JsonError, seg: &str) -> JsonError {
    let JsonError::Bad { path, reason } = e;
    JsonError::Bad {
        path: if path.is_empty() {
            seg.to_owned()
        } else {
            format!("{seg}.{path}")
        },
        reason,
    }
}

fn float_json(f: f64) -> Value {
    match Number::from_f64(f) {
        Some(n) => Value::Number(n),
        None if f.is_nan() => Value::String("NaN".into()),
        None if f > 0.0 => Value::String("Infinity".into()),
        None => Value::String("-Infinity".into()),
    }
}

fn encode_single(fd: &FieldDescriptor, v: &Pb, depth: usize) -> JResult<Value> {
    Ok(match v {
        Pb::Bool(b) => Value::Bool(*b),
        Pb::I32(n) => Value::Number((*n).into()),
        Pb::U32(n) => Value::Number((*n).into()),
        Pb::I64(n) => Value::String(n.to_string()),
        Pb::U64(n) => Value::String(n.to_string()),
        Pb::F32(f) => float_json(f64::from(*f)),
        Pb::F64(f) => float_json(*f),
        Pb::String(s) => Value::String(s.clone()),
        Pb::Bytes(b) => Value::String(base64_encode(b)),
        Pb::EnumNumber(n) => match fd.kind() {
            Kind::Enum(ed) => ed.get_value(*n).map_or_else(
                || Value::Number((*n).into()),
                |ev| Value::String(ev.name().to_owned()),
            ),
            _ => Value::Number((*n).into()),
        },
        Pb::Message(m) => encode_message(m, depth + 1)?,
        Pb::List(_) | Pb::Map(_) => return Err(bad("nested collection")),
    })
}

fn encode_field(fd: &FieldDescriptor, v: &Pb, depth: usize) -> JResult<Value> {
    if fd.is_map() {
        let Kind::Message(entry) = fd.kind() else {
            return Err(bad("map field without entry type"));
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
                let j = encode_single(&vf, val, depth).map_err(|e| at(e, &key))?;
                out.insert(key, j);
            }
        }
        return Ok(Value::Object(out));
    }
    if fd.is_list() {
        let mut out = Vec::new();
        if let Some(list) = v.as_list() {
            for (i, item) in list.iter().enumerate() {
                out.push(encode_single(fd, item, depth).map_err(|e| at(e, &i.to_string()))?);
            }
        }
        return Ok(Value::Array(out));
    }
    encode_single(fd, v, depth)
}

fn encode_message(msg: &DynamicMessage, depth: usize) -> JResult<Value> {
    if depth > MAX_DEPTH {
        return Err(bad("message nesting is too deep"));
    }
    let mut out = Map::new();
    for fd in msg.descriptor().fields() {
        if !msg.has_field(&fd) {
            continue;
        }
        let v = msg.get_field(&fd);
        let j = encode_field(&fd, &v, depth).map_err(|e| at(e, fd.json_name()))?;
        out.insert(fd.json_name().to_owned(), j);
    }
    Ok(Value::Object(out))
}

fn integer(j: &Value) -> JResult<i128> {
    match j {
        Value::Number(n) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                    .map(|f| f as i128)
            })
            .ok_or_else(|| bad("expected an integer")),
        Value::String(s) => s.trim().parse().map_err(|_| bad("expected an integer")),
        _ => Err(bad("expected an integer")),
    }
}

fn ranged<T: TryFrom<i128>>(j: &Value) -> JResult<T> {
    T::try_from(integer(j)?).map_err(|_| bad("integer out of range"))
}

fn float(j: &Value) -> JResult<f64> {
    match j {
        Value::Number(n) => n.as_f64().ok_or_else(|| bad("expected a number")),
        Value::String(s) => match s.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            other => other.parse().map_err(|_| bad("expected a number")),
        },
        _ => Err(bad("expected a number")),
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (proto3 JSON `bytes`).
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 in the standard or URL-safe alphabet, with or without padding.
fn base64_any(s: &str) -> JResult<Vec<u8>> {
    let body = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in body.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err(bad("expected base64")),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if body.len() % 4 == 1 {
        return Err(bad("expected base64"));
    }
    Ok(out)
}

fn enum_value(ed: &EnumDescriptor, j: &Value) -> JResult<i32> {
    match j {
        Value::String(s) => ed.get_value_by_name(s).map(|v| v.number()).ok_or_else(|| {
            bad(format!(
                "unknown enum value '{}'",
                s.chars().take(64).collect::<String>()
            ))
        }),
        other => {
            let n: i32 = ranged(other)?;
            Ok(n)
        }
    }
}

fn decode_single(fd: &FieldDescriptor, kind: &Kind, j: &Value, depth: usize) -> JResult<Pb> {
    Ok(match kind {
        Kind::Bool => Pb::Bool(j.as_bool().ok_or_else(|| bad("expected a boolean"))?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Pb::I32(ranged(j)?),
        Kind::Uint32 | Kind::Fixed32 => Pb::U32(ranged(j)?),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Pb::I64(ranged(j)?),
        Kind::Uint64 | Kind::Fixed64 => Pb::U64(ranged(j)?),
        Kind::Float => Pb::F32(float(j)? as f32),
        Kind::Double => Pb::F64(float(j)?),
        Kind::String => Pb::String(
            j.as_str()
                .ok_or_else(|| bad("expected a string"))?
                .to_owned(),
        ),
        Kind::Bytes => Pb::Bytes(
            base64_any(j.as_str().ok_or_else(|| bad("expected a base64 string"))?)?.into(),
        ),
        Kind::Enum(ed) => Pb::EnumNumber(enum_value(ed, j)?),
        Kind::Message(md) => {
            let _ = fd;
            Pb::Message(decode_message(md, j, depth + 1)?)
        }
    })
}

fn map_key(kind: &Kind, k: &str) -> JResult<MapKey> {
    fn p<T: std::str::FromStr>(k: &str) -> JResult<T> {
        k.parse().map_err(|_| bad("bad map key"))
    }
    Ok(match kind {
        Kind::String => MapKey::String(k.to_owned()),
        Kind::Bool => MapKey::Bool(p(k)?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => MapKey::I32(p(k)?),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => MapKey::I64(p(k)?),
        Kind::Uint32 | Kind::Fixed32 => MapKey::U32(p(k)?),
        Kind::Uint64 | Kind::Fixed64 => MapKey::U64(p(k)?),
        _ => return Err(bad("unsupported map key")),
    })
}

fn decode_field(fd: &FieldDescriptor, j: &Value, depth: usize) -> JResult<Pb> {
    if fd.is_map() {
        let Kind::Message(entry) = fd.kind() else {
            return Err(bad("map field without entry type"));
        };
        let (kf, vf) = (entry.map_entry_key_field(), entry.map_entry_value_field());
        let Value::Object(obj) = j else {
            return Err(bad("expected an object"));
        };
        let mut map = HashMap::with_capacity(obj.len());
        for (k, val) in obj {
            let key = map_key(&kf.kind(), k).map_err(|e| at(e, k))?;
            let v = decode_single(&vf, &vf.kind(), val, depth).map_err(|e| at(e, k))?;
            map.insert(key, v);
        }
        return Ok(Pb::Map(map));
    }
    if fd.is_list() {
        let Value::Array(arr) = j else {
            return Err(bad("expected an array"));
        };
        let mut list = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            list.push(
                decode_single(fd, &fd.kind(), item, depth).map_err(|e| at(e, &i.to_string()))?,
            );
        }
        return Ok(Pb::List(list));
    }
    decode_single(fd, &fd.kind(), j, depth)
}

fn decode_message(
    desc: &MessageDescriptor,
    value: &Value,
    depth: usize,
) -> JResult<DynamicMessage> {
    if depth > MAX_DEPTH {
        return Err(bad("message nesting is too deep"));
    }
    let Value::Object(obj) = value else {
        return Err(bad("expected an object"));
    };
    let mut msg = DynamicMessage::new(desc.clone());
    for (key, j) in obj {
        let fd = desc
            .get_field_by_json_name(key)
            .or_else(|| desc.get_field_by_name(key))
            .ok_or_else(|| at(bad("unknown field"), key))?;
        if j.is_null() {
            continue;
        }
        let v = decode_field(&fd, j, depth).map_err(|e| at(e, key))?;
        msg.set_field(&fd, v);
    }
    Ok(msg)
}

// ------------------------------------------------------------------------------ the call

#[derive(Clone)]
struct DynCodec {
    response: MessageDescriptor,
}

struct DynEncoder;
struct DynDecoder(MessageDescriptor);

impl Encoder for DynEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut EncodeBuf<'_>,
    ) -> std::result::Result<(), Status> {
        item.encode(dst)
            .map_err(|e| Status::internal(format!("encode: {e}")))
    }
}

impl Decoder for DynDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(
        &mut self,
        src: &mut DecodeBuf<'_>,
    ) -> std::result::Result<Option<Self::Item>, Status> {
        let bytes = src.copy_to_bytes(src.remaining());
        DynamicMessage::decode(self.0.clone(), bytes)
            .map(Some)
            .map_err(|e| Status::internal(format!("decode: {e}")))
    }
}

impl Codec for DynCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynEncoder;
    type Decoder = DynDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynDecoder(self.response.clone())
    }
}

fn read_request(text: Option<&str>) -> Result<Value> {
    let raw = match text {
        None => "{}".to_owned(),
        Some("-") => {
            let mut buf = String::new();
            std::io::stdin()
                .lock()
                .take(MAX_REQUEST + 1)
                .read_to_string(&mut buf)?;
            if buf.len() as u64 > MAX_REQUEST {
                return Err(fatal("the request is larger than 16 MiB"));
            }
            buf
        }
        Some(t) => {
            if t.len() as u64 > MAX_REQUEST {
                return Err(fatal("the request is larger than 16 MiB"));
            }
            t.to_owned()
        }
    };
    let raw = if raw.trim().is_empty() {
        "{}"
    } else {
        raw.as_str()
    };
    serde_json::from_str(raw).map_err(|e| fatal(format!("the request is not valid JSON: {e}")))
}

/// Fills the request's `repo` from the remote when the caller left it out.
fn with_repo(msg: &mut DynamicMessage, repo: &str) {
    let desc = msg.descriptor();
    let Some(fd) = desc.get_field_by_name("repo") else {
        return;
    };
    let Kind::Message(rd) = fd.kind() else {
        return;
    };
    if rd.full_name() != "asc.v1.RepoRef" || msg.has_field(&fd) {
        return;
    }
    let mut r = DynamicMessage::new(rd.clone());
    if let Some(name) = rd.get_field_by_name("name") {
        r.set_field(&name, Pb::String(repo.to_owned()));
    }
    msg.set_field(&fd, Pb::Message(r));
}

fn print_status(s: &Status) -> CliError {
    let text = asc_rpc::server::status_rpc_error(s).map_or_else(
        || {
            format!(
                "{:?}: {}",
                s.code(),
                s.message()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(512)
                    .collect::<String>()
            )
        },
        |e| e.to_string(),
    );
    CliError::Fatal(format!("asc: {text}"))
}

async fn call(ctx: &Ctx, o: &Opts, method: MethodDescriptor) -> Result<ExitCode> {
    if method.is_client_streaming() {
        return Err(fatal(format!(
            "{} takes a stream of requests, which 'asc api' does not send",
            method.name()
        )));
    }
    let request_json = read_request(o.json.as_deref())?;
    let mut request = decode_message(&method.input(), &request_json, 0)
        .map_err(|e| fatal(format!("request: {e}")))?;
    let server = crate::remote::server::open_for(ctx, o.remote.as_deref()).await?;
    with_repo(&mut request, &server.target().repo);
    let path: PathAndQuery = format!("/{}/{}", method.parent_service().full_name(), method.name())
        .parse()
        .map_err(|_| fatal("the RPC name does not form a path"))?;
    let codec = DynCodec {
        response: method.output(),
    };
    let op = uuid::Uuid::now_v7();
    let mut attempt = 0;
    let stdout = std::io::stdout();
    loop {
        let session = server.session();
        let mut grpc = tonic::client::Grpc::new(session.wire.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        grpc.ready()
            .await
            .map_err(|e| fatal(format!("cannot reach the server: {e}")))?;
        let req = session.wire.request(request.clone(), Some(op));
        let outcome = if method.is_server_streaming() {
            match grpc
                .server_streaming(req, path.clone(), codec.clone())
                .await
            {
                Ok(resp) => {
                    let mut stream = resp.into_inner();
                    let mut out = stdout.lock();
                    let mut count = 0usize;
                    let mut failure = None;
                    loop {
                        match stream.message().await {
                            Ok(Some(m)) => {
                                count += 1;
                                if count > MAX_STREAM {
                                    return Err(fatal("the server sent too many messages"));
                                }
                                let j = encode_message(&m, 0)
                                    .map_err(|e| fatal(format!("response: {e}")))?;
                                serde_json::to_writer(&mut out, &j)
                                    .map_err(std::io::Error::from)?;
                                out.write_all(b"\n")?;
                            }
                            Ok(None) => break,
                            Err(s) => {
                                failure = Some(s);
                                break;
                            }
                        }
                    }
                    match failure {
                        None => Ok(()),
                        Some(s) => Err(s),
                    }
                }
                Err(s) => Err(s),
            }
        } else {
            match grpc.unary(req, path.clone(), codec.clone()).await {
                Ok(resp) => {
                    let j = encode_message(&resp.into_inner(), 0)
                        .map_err(|e| fatal(format!("response: {e}")))?;
                    let mut out = stdout.lock();
                    serde_json::to_writer_pretty(&mut out, &j).map_err(std::io::Error::from)?;
                    out.write_all(b"\n")?;
                    Ok(())
                }
                Err(s) => Err(s),
            }
        };
        match outcome {
            Ok(()) => {
                crate::credential::report_result(&session.resolved, true);
                return Ok(ExitCode::SUCCESS);
            }
            Err(s) if s.code() == tonic::Code::Unauthenticated && attempt == 0 => {
                attempt += 1;
                server
                    .reauth(&session)
                    .await
                    .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
            }
            Err(s) => {
                if s.code() == tonic::Code::Unauthenticated {
                    crate::credential::report_result(&session.resolved, false);
                }
                return Err(print_status(&s));
            }
        }
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    if o.stdio {
        if o.rpc.is_some() || o.json.is_some() || o.remote.is_some() || o.list || o.docs {
            return Err(usage());
        }
        return crate::bridge::serve_stdio(ctx).await;
    }
    if o.list {
        print!("{}", list());
        return Ok(ExitCode::SUCCESS);
    }
    if o.docs {
        print!("{}", docs());
        return Ok(ExitCode::SUCCESS);
    }
    let Some(word) = o.rpc.clone() else {
        return Err(usage());
    };
    let method = find_method(&word)?;
    call(ctx, &o, method).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_rpcs_by_any_spelling() {
        for w in [
            "Log",
            "Repo.Log",
            "Repo/Log",
            "asc.v1.Repo/Log",
            "/asc.v1.Repo/Log",
        ] {
            let m = find_method(w).unwrap_or_else(|e| panic!("{w}: {e:?}"));
            assert_eq!(m.name(), "Log");
        }
        assert!(find_method("Nope").is_err());
        assert!(
            find_method("Workspace/Status").is_err(),
            "the daemon API is local"
        );
    }

    #[test]
    fn requests_round_trip_as_proto3_json() {
        let m = find_method("Log").unwrap();
        let req = json!({
            "repo": {"name": "o/r"},
            "include": ["master"],
            "limit": 5,
            "since": "1700000000",
            "firstParent": true
        });
        let msg = decode_message(&m.input(), &req, 0).unwrap();
        let back = encode_message(&msg, 0).unwrap();
        assert_eq!(back["include"], json!(["master"]));
        assert_eq!(back["limit"], json!(5));
        assert_eq!(
            back["since"],
            json!("1700000000"),
            "64-bit integers are strings"
        );
        assert_eq!(back["firstParent"], json!(true));
        assert!(decode_message(&m.input(), &json!({"bogus": 1}), 0).is_err());
        assert!(decode_message(&m.input(), &json!({"limit": "x"}), 0).is_err());
        assert!(decode_message(&m.input(), &json!({"limit": -1}), 0).is_err());
    }

    #[test]
    fn bytes_are_base64_and_enums_are_names() {
        let m = find_method("MergeBase").unwrap();
        let id = base64_encode(&[7u8; 32]);
        let msg = decode_message(&m.input(), &json!({"a": id}), 0);
        assert!(msg.is_ok() || msg.is_err());
        let up = find_method("UpdateRefs").unwrap();
        let msg = decode_message(&up.input(), &json!({"mode": "UPDATE_MODE_LAND"}), 0).unwrap();
        assert_eq!(
            encode_message(&msg, 0).unwrap()["mode"],
            json!("UPDATE_MODE_LAND")
        );
    }

    #[test]
    fn the_request_repo_is_filled_in_once() {
        let m = find_method("Log").unwrap();
        let mut msg = decode_message(&m.input(), &json!({}), 0).unwrap();
        with_repo(&mut msg, "o/r");
        assert_eq!(
            encode_message(&msg, 0).unwrap()["repo"]["name"],
            json!("o/r")
        );
        let mut own = decode_message(&m.input(), &json!({"repo": {"name": "x/y"}}), 0).unwrap();
        with_repo(&mut own, "o/r");
        assert_eq!(
            encode_message(&own, 0).unwrap()["repo"]["name"],
            json!("x/y")
        );
    }

    #[test]
    fn deep_requests_are_refused() {
        let m = find_method("Log").unwrap();
        let mut v = json!({});
        for _ in 0..200 {
            v = json!({ "repo": v });
        }
        assert!(decode_message(&m.input(), &v, 0).is_err());
    }

    #[test]
    fn the_listing_and_docs_name_the_public_rpcs() {
        let l = list();
        assert!(l.contains("Repo/MergeBase"));
        assert!(l.contains("Repo/Log"));
        assert!(!l.contains("Workspace/"));
        assert!(docs().contains("### MergeBase"));
    }
}
