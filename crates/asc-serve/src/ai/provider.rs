//! The provider clients: one small trait, one implementation per wire format.
//!
//! * [`Anthropic`]: the Messages API (`POST {base}/v1/messages`, `x-api-key`).
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
            Auth::Anthropic => self.key.to_string(),
            Auth::Bearer => format!("Bearer {}", self.key.as_str()),
        })
        .map_err(|_| ModelError::Unauthorized)?;
        secret.set_sensitive(true);
        req = match auth {
            Auth::Anthropic => req
                .header("x-api-key", secret)
                .header("anthropic-version", "2023-06-01"),
            Auth::Bearer => req.header("authorization", secret),
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
        .unwrap_or("");
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
