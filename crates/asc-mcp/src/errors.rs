//! Error mapping (SPEC 22.3.1, 22.5).
//!
//! Two channels exist. A protocol fault (unknown tool, bad arguments, bad cursor) is a JSON-RPC
//! error ([`RpcError`], -32602 and friends). Everything else is a tool result with
//! `isError: true` whose `structuredContent.error` is a [`ToolError`]. A child verb's exit code 1
//! (paused on a conflict) and 2 (ambiguous) are results, not errors ([`map_exit`]).
//! All text that leaves the server passes through [`redact`].

use serde_json::{Map, Value, json};

/// JSON-RPC "parse error".
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC "invalid request".
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC "method not found".
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC "invalid params".
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC "internal error".
pub const INTERNAL_ERROR: i32 = -32603;

/// Error codes the MCP layer adds to `ErrorDetail.code` (SPEC 22.3.1).
pub mod code {
    /// A destructive call needs the user's confirmation.
    pub const NEEDS_CONFIRMATION: &str = "NEEDS_CONFIRMATION";
    /// The call would exceed a bound; the message names the bounded alternative.
    pub const BUDGET: &str = "BUDGET";
    /// The backend has no answer in this mode.
    pub const NOT_AVAILABLE: &str = "NOT_AVAILABLE";
    /// An operation-log position moved.
    pub const STALE_OP: &str = "STALE_OP";
    /// Too many calls; `retry_after_ms` says when to retry.
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    /// A path policy refused the call.
    pub const PATH_POLICY: &str = "PATH_POLICY";
    /// The repository or object does not exist (or may not be seen).
    pub const NOT_FOUND: &str = "NOT_FOUND";
    /// The server failed in a way the caller cannot fix.
    pub const INTERNAL: &str = "INTERNAL";
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} ({code})")]
pub struct RpcError {
    /// The JSON-RPC code.
    pub code: i32,
    /// Human text (already redacted).
    pub message: String,
}

impl RpcError {
    /// An error with `code` and a redacted `message`.
    #[must_use]
    pub fn new(code: i32, message: impl AsRef<str>) -> Self {
        Self {
            code,
            message: redact(message.as_ref()),
        }
    }

    /// -32602.
    #[must_use]
    pub fn invalid_params(message: impl AsRef<str>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    /// The `error` member of a response.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

impl From<crate::cursor::CursorError> for RpcError {
    fn from(e: crate::cursor::CursorError) -> Self {
        Self::new(e.code(), e.to_string())
    }
}

/// The `structuredContent.error` of an `isError` result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} ({code})")]
pub struct ToolError {
    /// `ErrorDetail.code` or one of [`code`].
    pub code: String,
    /// Human text (redacted when the error is rendered).
    pub message: String,
    /// Paths the error concerns.
    pub paths: Vec<String>,
    /// Conflicted paths.
    pub conflicts: Vec<String>,
    /// When to retry, for `RATE_LIMITED`.
    pub retry_after_ms: Option<u64>,
}

impl ToolError {
    /// An error with a code and message.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            paths: vec![],
            conflicts: vec![],
            retry_after_ms: None,
        }
    }

    /// `NEEDS_CONFIRMATION`.
    #[must_use]
    pub fn needs_confirmation(message: impl Into<String>) -> Self {
        Self::new(code::NEEDS_CONFIRMATION, message)
    }

    /// `BUDGET`.
    #[must_use]
    pub fn budget(message: impl Into<String>) -> Self {
        Self::new(code::BUDGET, message)
    }

    /// `NOT_AVAILABLE`.
    #[must_use]
    pub fn not_available(message: impl Into<String>) -> Self {
        Self::new(code::NOT_AVAILABLE, message)
    }

    /// `RATE_LIMITED` with a retry hint.
    #[must_use]
    pub fn rate_limited(message: impl Into<String>, retry_after_ms: u64) -> Self {
        Self {
            retry_after_ms: Some(retry_after_ms),
            ..Self::new(code::RATE_LIMITED, message)
        }
    }

    /// The server is full: 8 calls run and 32 wait (SPEC 22.5, bounded memory).
    #[must_use]
    pub fn busy() -> Self {
        Self::rate_limited("busy", 1000)
    }

    /// `INTERNAL`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(code::INTERNAL, message)
    }

    /// Adds the paths the error concerns.
    #[must_use]
    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = paths;
        self
    }

    /// The `error` object: all five members are always present.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "message": redact(&self.message),
            "paths": self.paths.iter().map(|p| redact(p)).collect::<Vec<_>>(),
            "conflicts": self.conflicts.iter().map(|p| redact(p)).collect::<Vec<_>>(),
            "retry_after_ms": self.retry_after_ms.unwrap_or(0),
        })
    }

    /// The whole `tools/call` result: `isError: true`, a text block and `structuredContent.error`.
    #[must_use]
    pub fn to_result(&self) -> Value {
        let err = self.to_json();
        let text = format!(
            "{}: {}",
            self.code,
            err["message"].as_str().unwrap_or_default()
        );
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": { "error": err },
            "isError": true,
        })
    }
}

/// What a child verb's exit code means to the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A result with `isError: false`; `status` is set to `paused` or `ambiguous` for exit 1 or 2.
    Ok(Value),
    /// An `isError` result.
    Err(ToolError),
}

/// Maps a child verb's exit code and its `--json` output (SPEC 22.3.1). 0 is success; 1 (paused on
/// a conflict) and 2 (ambiguous) are results with `OpResult.status` `paused` or `ambiguous`;
/// 128, 129 and every other code are errors.
#[must_use]
pub fn map_exit(exit_code: i32, mut result: Value) -> Outcome {
    let status = match exit_code {
        0 => return Outcome::Ok(result),
        1 => "paused",
        2 => "ambiguous",
        _ => {
            let message = result
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map_or_else(
                    || format!("the command failed with exit code {exit_code}"),
                    str::to_owned,
                );
            let code = result
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or(code::INTERNAL)
                .to_owned();
            return Outcome::Err(ToolError::new(code, message));
        }
    };
    if !result.is_object() {
        result = Value::Object(Map::new());
    }
    if let Some(o) = result.as_object_mut() {
        o.insert("status".into(), json!(status));
    }
    Outcome::Ok(result)
}

const MASK: &str = "***";

/// Strips credentials from text that leaves the server (SPEC 22.5, secrets): the userinfo of a
/// URL, the value after `--token`, `--password` or `--auth`, the value of `-c key=value`, and
/// `Authorization` header values.
#[must_use]
pub fn redact(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    let mut after_c = false;
    let mut after_auth = 0u8;
    for w in text.split(' ') {
        if mask_next {
            mask_next = false;
            out.push(MASK.into());
            continue;
        }
        if after_c {
            after_c = false;
            out.push(match w.split_once('=') {
                Some((k, _)) => format!("{k}={MASK}"),
                None => w.to_owned(),
            });
            continue;
        }
        if after_auth > 0 {
            after_auth -= 1;
            out.push(MASK.into());
            continue;
        }
        let lower = w.to_ascii_lowercase();
        let secret_flag = |f: &str| matches!(f, "--token" | "--password" | "--auth");
        if secret_flag(&lower) || lower == "--header" {
            mask_next = true;
            out.push(w.to_owned());
        } else if let Some((flag, _)) = w
            .split_once('=')
            .filter(|(f, _)| secret_flag(&f.to_ascii_lowercase()))
        {
            out.push(format!("{flag}={MASK}"));
        } else if w == "-c" {
            after_c = true;
            out.push(w.to_owned());
        } else if lower.trim_end_matches(':') == "authorization" {
            // "Authorization: Bearer abc" or "Authorization: abc".
            after_auth = 2;
            out.push(w.to_owned());
        } else {
            out.push(redact_url(w));
        }
    }
    out.join(" ")
}

/// Replaces `user:pass@` in a `scheme://` URL of one word.
fn redact_url(word: &str) -> String {
    let Some(i) = word.find("://") else {
        return word.to_owned();
    };
    let (head, rest) = word.split_at(i + 3);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{head}{MASK}@{}", &rest[at + 1..]),
        None => word.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_one_and_two_are_results() {
        let Outcome::Ok(v) = map_exit(1, json!({"conflicts": ["a"]})) else {
            panic!("exit 1 is a result")
        };
        assert_eq!(v["status"], "paused");
        assert_eq!(v["conflicts"][0], "a");
        let Outcome::Ok(v) = map_exit(2, json!({"candidates": ["x", "y"]})) else {
            panic!("exit 2 is a result")
        };
        assert_eq!(v["status"], "ambiguous");
        assert_eq!(v["candidates"].as_array().map(Vec::len), Some(2));
        assert!(
            matches!(map_exit(0, json!({"status": "ok"})), Outcome::Ok(v) if v["status"] == "ok")
        );
    }

    #[test]
    fn exit_128_and_129_are_errors() {
        for c in [128, 129, 3, 255] {
            assert!(matches!(map_exit(c, json!({})), Outcome::Err(_)), "{c}");
        }
        let Outcome::Err(e) = map_exit(
            128,
            json!({"error": {"code": "NOT_FOUND", "message": "no such change"}}),
        ) else {
            panic!("error expected")
        };
        assert_eq!(
            (e.code.as_str(), e.message.as_str()),
            ("NOT_FOUND", "no such change")
        );
    }

    #[test]
    fn error_result_shape() {
        let r = ToolError::busy().to_result();
        assert_eq!(r["isError"], true);
        assert_eq!(r["structuredContent"]["error"]["code"], "RATE_LIMITED");
        assert_eq!(r["structuredContent"]["error"]["message"], "busy");
        assert_eq!(r["structuredContent"]["error"]["retry_after_ms"], 1000);
        assert!(r["structuredContent"]["error"]["paths"].is_array());
    }

    #[test]
    fn redaction_strips_credentials() {
        let r = redact(
            "fetch https://bob:s3cret@host.example/repo.git failed --token abc123 -c http.extraHeader=Bearer-xyz",
        );
        assert!(
            !r.contains("s3cret") && !r.contains("abc123") && !r.contains("Bearer-xyz"),
            "{r}"
        );
        assert!(r.contains("host.example/repo.git"), "{r}");
        let r = redact("Authorization: Bearer topsecret rejected");
        assert!(!r.contains("topsecret"), "{r}");
        assert_eq!(redact("--password=hunter2"), "--password=***");
        assert_eq!(redact("plain text stays"), "plain text stays");
    }

    #[test]
    fn tool_error_text_is_redacted_when_rendered() {
        let r = ToolError::internal("push to https://u:p@h/x failed").to_result();
        assert!(!r.to_string().contains("u:p@"));
    }
}
