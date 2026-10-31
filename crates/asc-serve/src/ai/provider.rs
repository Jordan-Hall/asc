//! The provider clients: one small trait, one implementation per wire format.
//!
//! * [`Anthropic`]: the Messages API (`POST {base}/v1/messages`, `x-api-key`).
//! * [`Google`]: Gemini `generateContent` (`POST {base}/v1beta/models/{model}:generateContent`, the key in
//!   the `x-goog-api-key` header, never in the URL).
//! * [`OpenAi`]: chat completions (`POST {base}/chat/completions`, `Authorization: Bearer`), for
//!   OpenAI itself and for any OpenAI-compatible endpoint (`base` is the API root, usually
//!   ending in `/v1`).
//!
//! Every client is built over a [`Pinned`] endpoint: the network guard has resolved the host,
//! judged every address and fixed the addresses the connection may use, so a user-supplied base
//! URL cannot reach an inward address, even through DNS rebinding or a redirect. The key lives
//! in a [`Zeroizing`] string, is put into one header marked sensitive and appears nowhere else:
//! errors are the fixed [`ModelError`] phrases and never quote a response body, a URL or the key.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::model::{
    Message, ModelError, ModelInfo, ModelRequest, ModelResponse, Stop, ToolCall, Usage,
};
use crate::netguard::{Pinned, Profile};

/// Largest response body read from a provider.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Longest tool-call argument string kept from a model.
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
/// The default API roots.
pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";
pub const GOOGLE_BASE: &str = "https://generativelanguage.googleapis.com";

/// A provider.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// One completion. `cancel` ends the call early (the result is then [`ModelError::Timeout`]).
    async fn complete(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelResponse, ModelError>;

    /// The models the key can use.
    async fn list_models(&self, cancel: &CancellationToken) -> Result<Vec<ModelInfo>, ModelError>;
}

/// The HTTP part both clients share.
struct Transport {
    client: reqwest::Client,
    base: String,
    key: Zeroizing<String>,
    timeout: Duration,
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Transport(<redacted>)")
    }
}

impl Transport {
    fn new(pinned: &Pinned, key: Zeroizing<String>, timeout: Duration) -> Result<Self, ModelError> {
        let client = pinned
            .client(Profile::Api)
            .map_err(|_| ModelError::Unreachable)?;
        Ok(Self {
            client,
            base: pinned.endpoint().as_string(),
            key,
            timeout,
        })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        auth: Auth,
        body: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, ModelError> {
        let url = format!("{}{path}", self.base);
        let mut req = self
            .client
            .request(method, url)
            .timeout(self.timeout)
            .header("content-type", "application/json");
        let mut secret = reqwest::header::HeaderValue::from_str(&match auth {
            Auth::Anthropic | Auth::Google => self.key.to_string(),
            Auth::Bearer => format!("Bearer {}", self.key.as_str()),
        })
        .map_err(|_| ModelError::Unauthorized)?;
        secret.set_sensitive(true);
        req = match auth {
            Auth::Anthropic => req
                .header("x-api-key", secret)
                .header("anthropic-version", "2023-06-01"),
            Auth::Bearer => req.header("authorization", secret),
            Auth::Google => req.header("x-goog-api-key", secret),
        };
        if let Some(b) = body {
            req = req.body(serde_json::to_vec(b).map_err(|_| ModelError::BadResponse)?);
        }
        let run = async {
            let resp = req.send().await.map_err(map_reqwest)?;
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(3600)));
            let bytes = read_capped(resp).await?;
            Ok::<_, ModelError>((status.as_u16(), retry_after, bytes))
        };
        let (status, retry_after, bytes) = tokio::select! {
            r = run => r?,
            () = cancel.cancelled() => return Err(ModelError::Timeout),
        };
        match status {
            200..=299 => serde_json::from_slice(&bytes).map_err(|_| ModelError::BadResponse),
            401 | 403 => Err(ModelError::Unauthorized),
            429 | 529 => Err(ModelError::RateLimited { retry_after }),
            400 | 404 | 413 | 422 => Err(ModelError::Refused(error_slug(&bytes))),
            other => Err(ModelError::Status(other)),
        }
    }
}

#[derive(Clone, Copy)]
enum Auth {
    Anthropic,
    Bearer,
    Google,
}

fn map_reqwest(e: reqwest::Error) -> ModelError {
    if e.is_timeout() {
        ModelError::Timeout
    } else {
        ModelError::Unreachable
    }
}

async fn read_capped(resp: reqwest::Response) -> Result<Vec<u8>, ModelError> {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest)?;
        if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ModelError::BadResponse);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The provider's error `type` or `code` as a short slug, or `"invalid_request"`. Only
/// `[a-z0-9_.-]` survives, so nothing the provider wrote can carry text into our messages.
fn error_slug(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let raw = v
        .pointer("/error/type")
        .or_else(|| v.pointer("/error/code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        // Google: {"error": {"code": 400, "status": "INVALID_ARGUMENT"}}
        .or_else(|| {
            v.pointer("/error/status")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_default();
    let slug: String = raw
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
        .take(48)
        .collect();
    if slug.is_empty() {
        "invalid_request".to_owned()
    } else {
        slug
    }
}

fn as_u64(v: Option<&Value>) -> u64 {
    v.and_then(Value::as_u64).unwrap_or(0)
}

// ------------------------------------------------------------------------------ Anthropic

/// The Anthropic Messages API.
pub struct Anthropic {
    t: Transport,
}

impl Anthropic {
    pub fn new(
        pinned: &Pinned,
        key: Zeroizing<String>,
        timeout: Duration,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            t: Transport::new(pinned, key, timeout)?,
        })
    }

    /// The request body for `req`.
    pub fn body(req: &ModelRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        let mut push = |role: &str, blocks: Vec<Value>| {
            if blocks.is_empty() {
                return;
            }
            // Adjacent turns of one role are one turn.
            if let Some(last) = messages.last_mut()
                && last["role"] == role
                && let Some(arr) = last["content"].as_array_mut()
            {
                arr.extend(blocks);
                return;
            }
            messages.push(json!({ "role": role, "content": blocks }));
        };
        for m in &req.messages {
            match m {
                Message::User(t) => push("user", vec![json!({"type": "text", "text": t})]),
                Message::Assistant { text, tool_calls } => {
                    let mut blocks = Vec::new();
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    for c in tool_calls {
                        blocks.push(json!({
                            "type": "tool_use", "id": c.id, "name": c.name, "input": c.input,
                        }));
                    }
                    push("assistant", blocks);
                }
                Message::ToolResults(rs) => push(
                    "user",
                    rs.iter()
                        .map(|r| {
                            json!({
                                "type": "tool_result", "tool_use_id": r.call_id,
                                "content": r.content, "is_error": r.is_error,
                            })
                        })
                        .collect(),
                ),
            }
        }
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens,
            "messages": messages,
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
                    .collect(),
            );
        }
        body
    }

    /// Reads a Messages API answer.
    pub fn parse(v: &Value) -> Result<ModelResponse, ModelError> {
        let blocks = v
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ModelError::BadResponse)?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("tool_use") => {
                    let name = b
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or(ModelError::BadResponse)?;
                    let id = b
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or(ModelError::BadResponse)?;
                    tool_calls.push(ToolCall {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        input: b.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
                _ => {}
            }
        }
        let stop = match v.get("stop_reason").and_then(Value::as_str) {
            Some("end_turn" | "stop_sequence") => Stop::End,
            Some("tool_use") => Stop::ToolUse,
            Some("max_tokens") => Stop::MaxTokens,
            _ if !tool_calls.is_empty() => Stop::ToolUse,
            _ => Stop::Other,
        };
        let usage = Usage {
            input_tokens: as_u64(v.pointer("/usage/input_tokens"))
                + as_u64(v.pointer("/usage/cache_creation_input_tokens"))
                + as_u64(v.pointer("/usage/cache_read_input_tokens")),
            output_tokens: as_u64(v.pointer("/usage/output_tokens")),
        };
        Ok(ModelResponse {
            text,
            tool_calls,
            usage,
            stop,
        })
    }
}

#[async_trait]
impl ModelClient for Anthropic {
    async fn complete(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let body = Self::body(req);
        let v = self
            .t
            .send(
                reqwest::Method::POST,
                "/v1/messages",
                Auth::Anthropic,
                Some(&body),
                cancel,
            )
            .await?;
        Self::parse(&v)
    }

    async fn list_models(&self, cancel: &CancellationToken) -> Result<Vec<ModelInfo>, ModelError> {
        let v = self
            .t
            .send(
                reqwest::Method::GET,
                "/v1/models?limit=100",
                Auth::Anthropic,
                None,
                cancel,
            )
            .await?;
        models(&v, "display_name")
    }
}

fn models(v: &Value, name_key: &str) -> Result<Vec<ModelInfo>, ModelError> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or(ModelError::BadResponse)?;
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            if id.is_empty() || id.len() > 200 || id.chars().any(char::is_control) {
                return None;
            }
            let display: String = m
                .get(name_key)
                .and_then(Value::as_str)
                .unwrap_or(id)
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            Some(ModelInfo {
                id: id.to_owned(),
                display_name: display,
                context_tokens: u32::try_from(as_u64(
                    m.get("context_window")
                        .or_else(|| m.get("max_input_tokens")),
                ))
                .unwrap_or(0),
            })
        })
        .take(500)
        .collect())
}

// ------------------------------------------------------------------------------ OpenAI

/// OpenAI chat completions, and compatible endpoints.
pub struct OpenAi {
    t: Transport,
    /// OpenAI's own API wants `max_completion_tokens`; compatible servers want `max_tokens`.
    official: bool,
}

impl OpenAi {
    pub fn new(
        pinned: &Pinned,
        key: Zeroizing<String>,
        timeout: Duration,
        official: bool,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            t: Transport::new(pinned, key, timeout)?,
            official,
        })
    }

    /// The request body for `req`.
    pub fn body(req: &ModelRequest, official: bool) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            match m {
                Message::User(t) => messages.push(json!({"role": "user", "content": t})),
                Message::Assistant { text, tool_calls } => {
                    let mut msg = json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { json!(text) },
                    });
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = Value::Array(
                            tool_calls
                                .iter()
                                .map(|c| {
                                    json!({
                                        "id": c.id, "type": "function",
                                        "function": {"name": c.name, "arguments": c.input.to_string()},
                                    })
                                })
                                .collect(),
                        );
                    }
                    messages.push(msg);
                }
                Message::ToolResults(rs) => {
                    for r in rs {
                        messages.push(json!({"role": "tool", "tool_call_id": r.call_id, "content": r.content}));
                    }
                }
            }
        }
        let mut body = json!({"model": req.model, "messages": messages});
        body[if official {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }] = json!(req.max_output_tokens);
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.schema}})
                    })
                    .collect(),
            );
        }
        body
    }

    /// Reads a chat completions answer.
    pub fn parse(v: &Value) -> Result<ModelResponse, ModelError> {
        let choice = v
            .pointer("/choices/0")
            .filter(|c| c.is_object())
            .ok_or(ModelError::BadResponse)?;
        let msg = choice.get("message").ok_or(ModelError::BadResponse)?;
        let text = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut tool_calls = Vec::new();
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let name = c
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .ok_or(ModelError::BadResponse)?;
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(ModelError::BadResponse)?;
                let raw = c
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let input = if raw.len() > MAX_ARGUMENT_BYTES {
                    Value::Null
                } else if raw.trim().is_empty() {
                    json!({})
                } else {
                    // Not JSON: keep the raw text as a string so the runtime reports it to the model.
                    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
                };
                tool_calls.push(ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input,
                });
            }
        }
        let stop = match choice.get("finish_reason").and_then(Value::as_str) {
            _ if !tool_calls.is_empty() => Stop::ToolUse,
            Some("stop") => Stop::End,
            Some("length") => Stop::MaxTokens,
            _ => Stop::Other,
        };
        let usage = Usage {
            input_tokens: as_u64(v.pointer("/usage/prompt_tokens")),
            output_tokens: as_u64(v.pointer("/usage/completion_tokens")),
        };
        Ok(ModelResponse {
            text,
            tool_calls,
            usage,
            stop,
        })
    }
}

#[async_trait]
impl ModelClient for OpenAi {
    async fn complete(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let body = Self::body(req, self.official);
        let v = self
            .t
            .send(
                reqwest::Method::POST,
                "/chat/completions",
                Auth::Bearer,
                Some(&body),
                cancel,
            )
            .await?;
        Self::parse(&v)
    }

    async fn list_models(&self, cancel: &CancellationToken) -> Result<Vec<ModelInfo>, ModelError> {
        let v = self
            .t
            .send(reqwest::Method::GET, "/models", Auth::Bearer, None, cancel)
            .await?;
        models(&v, "id")
    }
}

// ------------------------------------------------------------------------------ Google

/// Gemini `generateContent`.
pub struct Google {
    t: Transport,
}

/// The schema keywords Gemini's function declarations accept (an OpenAPI subset); anything else is dropped, because
/// the API refuses a declaration that carries a keyword it does not know.
const GOOGLE_SCHEMA_KEYS: &[&str] = &[
    "type",
    "format",
    "description",
    "nullable",
    "enum",
    "properties",
    "required",
    "items",
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
    "title",
];

/// A JSON Schema as Gemini takes it: unknown keywords removed at every level, `type` as one string.
fn google_schema(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut out = serde_json::Map::new();
            for (k, val) in o {
                if !GOOGLE_SCHEMA_KEYS.contains(&k.as_str()) {
                    continue;
                }
                let kept = match k.as_str() {
                    "properties" => match val {
                        Value::Object(props) => Value::Object(
                            props
                                .iter()
                                .map(|(n, s)| (n.clone(), google_schema(s)))
                                .collect(),
                        ),
                        _ => continue,
                    },
                    "items" => google_schema(val),
                    // `"type": ["string", "null"]` is a string that may be null.
                    "type" => match val {
                        Value::Array(types) => {
                            let first = types.iter().find(|t| t.as_str() != Some("null"));
                            if types.iter().any(|t| t.as_str() == Some("null")) {
                                out.insert("nullable".into(), Value::Bool(true));
                            }
                            match first {
                                Some(t) => t.clone(),
                                None => continue,
                            }
                        }
                        other => other.clone(),
                    },
                    _ => val.clone(),
                };
                out.insert(k.clone(), kept);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// A model id that is safe as one path segment of the URL.
fn google_model_ok(m: &str) -> bool {
    let m = m.strip_prefix("models/").unwrap_or(m);
    !m.is_empty()
        && m.len() <= 100
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Google {
    pub fn new(
        pinned: &Pinned,
        key: Zeroizing<String>,
        timeout: Duration,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            t: Transport::new(pinned, key, timeout)?,
        })
    }

    /// Gemini has no call ids, so one is made from the function name and its place; a result carries it back and the name
    /// is read out of it again.
    fn call_id(name: &str, n: usize) -> String {
        format!("{name}#{n}")
    }

    fn name_of(call_id: &str) -> &str {
        call_id.rsplit_once('#').map_or(call_id, |(n, _)| n)
    }

    /// The request body for `req` (the model is in the URL).
    pub fn body(req: &ModelRequest) -> Value {
        let mut contents: Vec<Value> = Vec::new();
        let mut push = |role: &str, parts: Vec<Value>| {
            if parts.is_empty() {
                return;
            }
            // Adjacent turns of one role are one turn.
            if let Some(last) = contents.last_mut()
                && last["role"] == role
                && let Some(arr) = last["parts"].as_array_mut()
            {
                arr.extend(parts);
                return;
            }
            contents.push(json!({ "role": role, "parts": parts }));
        };
        for m in &req.messages {
            match m {
                Message::User(t) => push("user", vec![json!({"text": t})]),
                Message::Assistant { text, tool_calls } => {
                    let mut parts = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({"text": text}));
                    }
                    for c in tool_calls {
                        // `args` is an object; anything else the model sent is kept under one key.
                        let args = if c.input.is_object() {
                            c.input.clone()
                        } else {
                            json!({"value": c.input})
                        };
                        parts.push(json!({"functionCall": {"name": c.name, "args": args}}));
                    }
                    push("model", parts);
                }
                Message::ToolResults(rs) => push(
                    "user",
                    rs.iter()
                        .map(|r| {
                            let key = if r.is_error { "error" } else { "output" };
                            json!({"functionResponse": {
                                "name": Self::name_of(&r.call_id),
                                "response": {key: r.content},
                            }})
                        })
                        .collect(),
                ),
            }
        }
        let mut body = json!({
            "contents": contents,
            "generationConfig": {"maxOutputTokens": req.max_output_tokens},
        });
        if !req.system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": req.system}]});
        }
        if !req.tools.is_empty() {
            let declarations: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut d = json!({"name": t.name, "description": t.description});
                    let schema = google_schema(&t.schema);
                    // A function without arguments takes no `parameters` at all.
                    if schema
                        .get("properties")
                        .and_then(Value::as_object)
                        .is_some_and(|p| !p.is_empty())
                    {
                        d["parameters"] = schema;
                    }
                    d
                })
                .collect();
            body["tools"] = json!([{ "functionDeclarations": declarations }]);
        }
        body
    }

    /// Reads a `generateContent` answer.
    pub fn parse(v: &Value) -> Result<ModelResponse, ModelError> {
        let usage = Usage {
            input_tokens: as_u64(v.pointer("/usageMetadata/promptTokenCount")),
            output_tokens: as_u64(v.pointer("/usageMetadata/candidatesTokenCount"))
                + as_u64(v.pointer("/usageMetadata/thoughtsTokenCount")),
        };
        let Some(cand) = v.pointer("/candidates/0").filter(|c| c.is_object()) else {
            // No candidate: the prompt was blocked (promptFeedback) or the answer is not Gemini's.
            return if v.pointer("/promptFeedback/blockReason").is_some() {
                Err(ModelError::Refused("prompt_blocked".into()))
            } else {
                Err(ModelError::BadResponse)
            };
        };
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        if let Some(parts) = cand.pointer("/content/parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(call) = p.get("functionCall") {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|n| !n.is_empty() && !n.contains('#'))
                        .ok_or(ModelError::BadResponse)?;
                    let input = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    let input = if input.to_string().len() > MAX_ARGUMENT_BYTES {
                        Value::Null
                    } else {
                        input
                    };
                    tool_calls.push(ToolCall {
                        id: Self::call_id(name, tool_calls.len()),
                        name: name.to_owned(),
                        input,
                    });
                } else if p.get("thought").and_then(Value::as_bool) != Some(true)
                    && let Some(t) = p.get("text").and_then(Value::as_str)
                {
                    text.push_str(t);
                }
            }
        }
        let stop = match cand.get("finishReason").and_then(Value::as_str) {
            _ if !tool_calls.is_empty() => Stop::ToolUse,
            Some("STOP") => Stop::End,
            Some("MAX_TOKENS") => Stop::MaxTokens,
            _ => Stop::Other,
        };
        Ok(ModelResponse {
            text,
            tool_calls,
            usage,
            stop,
        })
    }
}

#[async_trait]
impl ModelClient for Google {
    async fn complete(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        if !google_model_ok(&req.model) {
            return Err(ModelError::Refused("invalid_model".into()));
        }
        let model = req.model.strip_prefix("models/").unwrap_or(&req.model);
        let body = Self::body(req);
        let v = self
            .t
            .send(
                reqwest::Method::POST,
                &format!("/v1beta/models/{model}:generateContent"),
                Auth::Google,
                Some(&body),
                cancel,
            )
            .await?;
        Self::parse(&v)
    }

    async fn list_models(&self, cancel: &CancellationToken) -> Result<Vec<ModelInfo>, ModelError> {
        let v = self
            .t
            .send(
                reqwest::Method::GET,
                "/v1beta/models?pageSize=200",
                Auth::Google,
                None,
                cancel,
            )
            .await?;
        let list = v
            .get("models")
            .and_then(Value::as_array)
            .ok_or(ModelError::BadResponse)?;
        Ok(list
            .iter()
            .filter(|m| {
                // Only models that can answer a prompt are of use here (not embeddings, not image-only).
                m.get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .is_some_and(|ms| ms.iter().any(|x| x == "generateContent"))
            })
            .filter_map(|m| {
                let name = m.get("name").and_then(Value::as_str)?;
                let id = name.strip_prefix("models/").unwrap_or(name);
                if !google_model_ok(id) {
                    return None;
                }
                let display: String = m
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(200)
                    .collect();
                Some(ModelInfo {
                    id: id.to_owned(),
                    display_name: display,
                    context_tokens: u32::try_from(as_u64(m.get("inputTokenLimit"))).unwrap_or(0),
                })
            })
            .take(500)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::model::{ToolResult, ToolSpec};

    fn req() -> ModelRequest {
        ModelRequest {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![
                Message::User("task".into()),
                Message::Assistant {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        input: json!({"path": "a"}),
                    }],
                },
                Message::ToolResults(vec![ToolResult {
                    call_id: "c1".into(),
                    content: "data".into(),
                    is_error: false,
                }]),
                Message::User("steer".into()),
            ],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "d".into(),
                schema: json!({"type": "object"}),
            }],
            max_output_tokens: 99,
        }
    }

    #[test]
    fn the_anthropic_body_merges_adjacent_user_turns_and_skips_empty_text() {
        let b = Anthropic::body(&req());
        assert_eq!(b["system"], "sys");
        assert_eq!(b["max_tokens"], 99);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3, "user, assistant, user(tool_result + steer)");
        assert_eq!(
            m[1]["content"].as_array().unwrap().len(),
            1,
            "no empty text block"
        );
        assert_eq!(m[1]["content"][0]["type"], "tool_use");
        let last = m[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[0]["tool_use_id"], "c1");
        assert_eq!(last[1]["text"], "steer");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn the_openai_body_uses_the_tool_role_and_the_right_token_key() {
        let b = OpenAi::body(&req(), true);
        assert_eq!(b["max_completion_tokens"], 99);
        assert!(b.get("max_tokens").is_none());
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "system");
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert_eq!(m[4]["role"], "user");
        let c = OpenAi::body(&req(), false);
        assert_eq!(c["max_tokens"], 99);
    }

    #[test]
    fn answers_are_read_into_text_calls_and_usage() {
        let a = Anthropic::parse(&json!({
            "content": [{"type": "text", "text": "hi"}, {"type": "tool_use", "id": "t1", "name": "x", "input": {"k": 1}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 2},
        }))
        .unwrap();
        assert_eq!(a.text, "hi");
        assert_eq!(a.tool_calls[0].input["k"], 1);
        assert_eq!(a.stop, Stop::ToolUse);
        assert_eq!(
            a.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 5
            }
        );
        assert_eq!(
            Anthropic::parse(&json!({"nope": 1})).unwrap_err(),
            ModelError::BadResponse
        );

        let o = OpenAi::parse(&json!({
            "choices": [{"message": {"content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "x", "arguments": "{\"a\": 2}"}},
                {"id": "d", "type": "function", "function": {"name": "y", "arguments": "not json"}},
            ]}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3},
        }))
        .unwrap();
        assert_eq!(
            o.stop,
            Stop::ToolUse,
            "tool calls win over a compatible server's finish_reason"
        );
        assert_eq!(o.tool_calls[0].input["a"], 2);
        assert_eq!(o.tool_calls[1].input, Value::String("not json".into()));
        assert_eq!(o.usage.total(), 10);
        assert_eq!(
            OpenAi::parse(&json!({"choices": []})).unwrap_err(),
            ModelError::BadResponse
        );
    }

    #[test]
    fn the_google_body_maps_roles_calls_results_and_schema() {
        let mut r = req();
        r.tools[0].schema = json!({
            "$schema": "x", "type": "object", "additionalProperties": false,
            "properties": {"path": {"type": ["string", "null"], "default": "a"}, "n": {"type": "integer", "minimum": 1}},
            "required": ["path"],
        });
        let b = Google::body(&r);
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 99);
        let c = b["contents"].as_array().unwrap();
        assert_eq!(c.len(), 3, "user, model, user(function response + steer)");
        assert_eq!(c[1]["role"], "model");
        assert_eq!(c[1]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(c[1]["parts"][0]["functionCall"]["args"]["path"], "a");
        let last = c[2]["parts"].as_array().unwrap();
        assert_eq!(
            last[0]["functionResponse"]["name"], "c1",
            "no # in the id: the id is the name"
        );
        assert_eq!(last[0]["functionResponse"]["response"]["output"], "data");
        assert_eq!(last[1]["text"], "steer");
        let d = &b["tools"][0]["functionDeclarations"][0];
        assert_eq!(d["name"], "read_file");
        assert!(d["parameters"].get("$schema").is_none());
        assert!(d["parameters"].get("additionalProperties").is_none());
        assert_eq!(d["parameters"]["properties"]["path"]["type"], "string");
        assert_eq!(d["parameters"]["properties"]["path"]["nullable"], true);
        assert!(
            d["parameters"]["properties"]["path"]
                .get("default")
                .is_none()
        );
        assert_eq!(d["parameters"]["properties"]["n"]["minimum"], 1);
        // a function without arguments has no parameters
        let b = Google::body(&req());
        assert!(
            b["tools"][0]["functionDeclarations"][0]
                .get("parameters")
                .is_none()
        );
    }

    #[test]
    fn google_answers_are_read_into_text_calls_and_usage() {
        let g = Google::parse(&json!({
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "thinking", "thought": true},
                {"text": "hi"},
                {"functionCall": {"name": "x", "args": {"k": 1}}},
                {"functionCall": {"name": "y"}},
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4, "thoughtsTokenCount": 3},
        }))
        .unwrap();
        assert_eq!(g.text, "hi", "a thought is not text");
        assert_eq!(g.tool_calls[0].id, "x#0");
        assert_eq!(g.tool_calls[1].id, "y#1");
        assert_eq!(g.tool_calls[0].input["k"], 1);
        assert_eq!(g.tool_calls[1].input, json!({}));
        assert_eq!(g.stop, Stop::ToolUse);
        assert_eq!(
            g.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 7
            }
        );
        let m = Google::parse(&json!({"candidates": [{"content": {"parts": [{"text": "a"}]}, "finishReason": "MAX_TOKENS"}]})).unwrap();
        assert_eq!(m.stop, Stop::MaxTokens);
        assert_eq!(
            Google::parse(&json!({"promptFeedback": {"blockReason": "SAFETY"}})).unwrap_err(),
            ModelError::Refused("prompt_blocked".into())
        );
        assert_eq!(
            Google::parse(&json!({"nope": 1})).unwrap_err(),
            ModelError::BadResponse
        );
        assert_eq!(
            Google::parse(&json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "a#b"}}]}}]})).unwrap_err(),
            ModelError::BadResponse,
            "a name with the separator of the id is refused"
        );
        // a result finds its function again through the id
        assert_eq!(Google::name_of("read_file#3"), "read_file");
    }

    #[test]
    fn a_google_model_id_is_one_safe_path_segment() {
        for ok in [
            "gemini-2.5-pro",
            "models/gemini-2.0-flash",
            "gemini_1.5-pro-001",
        ] {
            assert!(google_model_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "models/",
            "a/b",
            "a:generateContent",
            "../x",
            "a b",
            "a?key=1",
            "a%2f",
        ] {
            assert!(!google_model_ok(bad), "{bad}");
        }
        assert_eq!(
            error_slug(br#"{"error":{"code":400,"status":"INVALID_ARGUMENT","message":"API key AIza..."}}"#),
            "invalid_argument"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_google_client_speaks_to_a_server_and_keeps_the_key_out_of_its_errors() {
        use crate::ai::fake::{FakeProvider, Reply};
        use crate::netguard::{Guard, NetGuardConfig, Shape};
        let key = "AIzaPLANTEDKEY0123456789";
        let fake = FakeProvider::start(key).await;
        let guard = Guard::new(NetGuardConfig {
            allow_loopback_http: true,
            ..NetGuardConfig::default()
        });
        let pinned = guard.pin(&fake.url(), Shape::Url).await.unwrap();
        let g = Google::new(
            &pinned,
            Zeroizing::new(key.to_owned()),
            Duration::from_secs(5),
        )
        .unwrap();
        let cancel = CancellationToken::new();

        let mut r = req();
        r.model = "gemini-a".into();
        fake.push(Reply::tool("read_file", json!({"path": "b"})));
        let a = g.complete(&r, &cancel).await.unwrap();
        assert_eq!(a.tool_calls[0].name, "read_file");
        assert_eq!(a.tool_calls[0].id, "read_file#0");
        assert_eq!(a.stop, Stop::ToolUse);
        let seen = fake.completions();
        assert_eq!(seen[0].path, "/v1beta/models/gemini-a:generateContent");
        assert_eq!(
            seen[0].key, key,
            "the key travels in the x-goog-api-key header"
        );
        assert!(!seen[0].path.contains("key"), "never in the URL");
        // models that cannot answer a prompt are not listed
        let models = g.list_models(&cancel).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-a");
        assert_eq!(models[0].context_tokens, 1_048_576);
        // a model id that is not one path segment never leaves the process
        r.model = "a/../b".into();
        let n = fake.seen().len();
        assert_eq!(
            g.complete(&r, &cancel).await.unwrap_err(),
            ModelError::Refused("invalid_model".into())
        );
        assert_eq!(fake.seen().len(), n);
        // a rejected key: the fixed phrase, nothing from the body or the key
        let bad = Google::new(
            &pinned,
            Zeroizing::new("AIzaWRONG0123456789012345".into()),
            Duration::from_secs(5),
        )
        .unwrap();
        r.model = "gemini-a".into();
        let e = bad.complete(&r, &cancel).await.unwrap_err();
        assert_eq!(e, ModelError::Unauthorized);
        let shown = format!("{e} {e:?} {:?}", bad.t);
        assert!(
            !shown.contains("sk-LEAK") && !shown.contains("AIza"),
            "{shown}"
        );
        // a refusal carries the status as a slug only
        fake.push(Reply::Status(400, json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "AIzaPLANTEDKEY0123456789"}})));
        let e = g.complete(&r, &cancel).await.unwrap_err();
        assert_eq!(e, ModelError::Refused("invalid_argument".into()));
    }

    #[test]
    fn error_slugs_cannot_carry_text() {
        assert_eq!(
            error_slug(br#"{"error":{"type":"invalid_request_error","message":"sk-secret"}}"#),
            "invalid_request_error"
        );
        assert_eq!(
            error_slug(br#"{"error":{"type":"Bad Type! <script>"}}"#),
            "adypescript"
        );
        assert_eq!(error_slug(b"not json at all sk-secret"), "invalid_request");
        assert_eq!(
            error_slug(br#"{"error":{"code":"model_not_found"}}"#),
            "model_not_found"
        );
    }
}
