//! A scripted model server for tests: speaks the Anthropic Messages API (`/v1/messages`,
//! `/v1/models`), OpenAI chat completions (`/chat/completions`, `/models`) and Gemini
//! (`/v1beta/models/{model}:generateContent`, `/v1beta/models`) on a loopback port.
//!
//! The test enables `allow_loopback_http` on the network guard, which is the one way a plain
//! loopback endpoint is accepted, so the whole path (guard, pinned client, wire format) runs.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use serde_json::{Value, json};

/// What the next call is answered with.
#[derive(Clone)]
pub(crate) enum Reply {
    /// Text and token counts.
    Text {
        text: String,
        input: u64,
        output: u64,
    },
    /// Tool calls (name, arguments), optional text and token counts.
    Tools {
        text: String,
        calls: Vec<(String, Value)>,
        input: u64,
        output: u64,
    },
    /// A bare status with a JSON body.
    Status(u16, Value),
    /// Waits, then answers with `then`.
    Delay(Duration, Box<Reply>),
    /// Waits until the test calls `notify_one` on the gate (a call made before the wait is kept),
    /// then answers with `then`: a pause in a model call that no clock decides.
    Gate(Arc<tokio::sync::Notify>, Box<Reply>),
    /// A body of this many bytes.
    Huge(usize),
    /// Chosen from the request body.
    Dynamic(Arc<dyn Fn(&Value) -> Reply + Send + Sync>),
}

impl Reply {
    pub(crate) fn text(t: &str) -> Self {
        Self::Text {
            text: t.into(),
            input: 10,
            output: 5,
        }
    }

    pub(crate) fn tool(name: &str, args: Value) -> Self {
        Self::Tools {
            text: String::new(),
            calls: vec![(name.into(), args)],
            input: 100,
            output: 20,
        }
    }
}

/// A request the server saw.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub path: String,
    pub method: String,
    /// The `x-api-key` or bearer token presented.
    pub key: String,
    pub body: Value,
}

struct Inner {
    valid_key: String,
    script: VecDeque<Reply>,
    default: Reply,
    seen: Vec<Seen>,
}

/// The running server.
#[derive(Clone)]
pub(crate) struct FakeProvider {
    pub addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
}

impl FakeProvider {
    /// Starts on a loopback port; `valid_key` is the only key it accepts.
    pub(crate) async fn start(valid_key: &str) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            valid_key: valid_key.to_owned(),
            script: VecDeque::new(),
            default: Reply::text("done"),
            seen: Vec::new(),
        }));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, inner }
    }

    /// `http://127.0.0.1:<port>` for Anthropic, `.../v1` style roots are the caller's business.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn push(&self, r: Reply) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .script
            .push_back(r);
    }

    pub(crate) fn set_default(&self, r: Reply) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .default = r;
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seen
            .clone()
    }

    /// Completions requested so far (not model listings).
    pub(crate) fn completions(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "POST")
            .collect()
    }

    pub(crate) fn set_key(&self, key: &str) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .valid_key = key.to_owned();
    }
}

fn json_response(status: u16, v: &Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

async fn handle(State(st): State<Arc<Mutex<Inner>>>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let anthropic = path.starts_with("/v1/");
    let google = path.starts_with("/v1beta/");
    let key = if anthropic || google {
        parts
            .headers
            .get(if google {
                "x-goog-api-key"
            } else {
                "x-api-key"
            })
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    } else {
        parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_owned()
    };
    let bytes = to_bytes(body, 8 * 1024 * 1024).await.unwrap_or_default();
    let body_json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let (valid, reply) = {
        let mut g = st.lock().unwrap_or_else(PoisonError::into_inner);
        g.seen.push(Seen {
            path: path.clone(),
            method: parts.method.to_string(),
            key: key.clone(),
            body: body_json.clone(),
        });
        let valid = key == g.valid_key;
        let is_completion = path.ends_with("/messages")
            || path.ends_with("/chat/completions")
            || path.ends_with(":generateContent");
        let reply = if is_completion {
            Some(g.script.pop_front().unwrap_or_else(|| g.default.clone()))
        } else {
            None
        };
        (valid, reply)
    };
    if !valid && google {
        return json_response(
            403,
            &json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "message": "API key not valid sk-LEAK"}}),
        );
    }
    if !valid {
        return json_response(
            401,
            &json!({"error": {"type": "authentication_error", "message": "invalid x-api-key sk-LEAK"}}),
        );
    }
    let Some(reply) = reply else {
        if google {
            return json_response(
                200,
                &json!({"models": [
                    {"name": "models/gemini-a", "displayName": "Gemini A", "inputTokenLimit": 1_048_576,
                     "supportedGenerationMethods": ["generateContent", "countTokens"]},
                    {"name": "models/embed-a", "displayName": "Embed", "supportedGenerationMethods": ["embedContent"]},
                ]}),
            );
        }
        let data = json!({"data": [
            {"id": "model-a", "display_name": "Model A", "context_window": 200_000},
            {"id": "model-b", "display_name": "Model B"},
        ]});
        return json_response(200, &data);
    };
    let mut reply = reply;
    loop {
        match reply {
            Reply::Delay(d, then) => {
                tokio::time::sleep(d).await;
                reply = *then;
            }
            Reply::Gate(gate, then) => {
                gate.notified().await;
                reply = *then;
            }
            Reply::Dynamic(f) => reply = f(&body_json),
            other => {
                reply = other;
                break;
            }
        }
    }
    match reply {
        Reply::Status(code, v) => json_response(code, &v),
        Reply::Huge(n) => Response::builder()
            .status(200)
            .body(Body::from(vec![b'x'; n]))
            .unwrap(),
        Reply::Text {
            text,
            input,
            output,
        } => completion(wire_of(anthropic, google), &text, &[], input, output),
        Reply::Tools {
            text,
            calls,
            input,
            output,
        } => completion(wire_of(anthropic, google), &text, &calls, input, output),
        Reply::Delay(..) | Reply::Gate(..) | Reply::Dynamic(_) => unreachable!(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Anthropic,
    OpenAi,
    Google,
}

fn wire_of(anthropic: bool, google: bool) -> Wire {
    if anthropic {
        Wire::Anthropic
    } else if google {
        Wire::Google
    } else {
        Wire::OpenAi
    }
}

fn completion(
    wire: Wire,
    text: &str,
    calls: &[(String, Value)],
    input: u64,
    output: u64,
) -> Response<Body> {
    if wire == Wire::Google {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(json!({"text": text}));
        }
        for (name, args) in calls {
            parts.push(json!({"functionCall": {"name": name, "args": args}}));
        }
        return json_response(
            200,
            &json!({
                "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": input, "candidatesTokenCount": output},
            }),
        );
    }
    if wire == Wire::Anthropic {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        for (i, (name, args)) in calls.iter().enumerate() {
            content.push(json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": args}));
        }
        json_response(
            200,
            &json!({
                "content": content,
                "stop_reason": if calls.is_empty() { "end_turn" } else { "tool_use" },
                "usage": {"input_tokens": input, "output_tokens": output},
            }),
        )
    } else {
        let tool_calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| {
                json!({
                    "id": format!("call_{i}"), "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                })
            })
            .collect();
        let mut message = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }
        json_response(
            200,
            &json!({
                "choices": [{"message": message, "finish_reason": if calls.is_empty() { "stop" } else { "tool_calls" }}],
                "usage": {"prompt_tokens": input, "completion_tokens": output},
            }),
        )
    }
}
