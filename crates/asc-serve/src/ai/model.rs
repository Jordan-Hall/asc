//! What a model call looks like, whatever provider answers it.
//!
//! The agent runtime speaks these types only. Each provider module turns them into its own wire
//! format and back. Everything the model says (text and tool arguments) is untrusted data.

use std::time::Duration;

use serde_json::Value;

/// One tool the model may call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// A JSON Schema object for the arguments.
    pub schema: Value,
}

/// A call the model made.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The provider's id for the call; the result must carry it back.
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// The answer to a call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// One turn of the conversation.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    User(String),
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    /// The results of the calls of the assistant turn before it, in the order of the calls.
    ToolResults(Vec<ToolResult>),
}

/// A request to complete the conversation.
#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// Most tokens the model may write in this answer.
    pub max_output_tokens: u32,
}

/// Why the model stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// It finished its turn.
    End,
    /// It asked for tools.
    ToolUse,
    /// It hit `max_output_tokens`.
    MaxTokens,
    /// A filter or an unknown reason.
    Other,
}

/// Tokens a call used, as the provider counted them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The model's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop: Stop,
}

/// A model a key can use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_tokens: u32,
}

/// Why a provider call failed. The text is fixed; it never holds a response body, a URL or a key.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// 401 or 403: the key is wrong, revoked or lacks access.
    #[error("the provider rejected the key")]
    Unauthorized,
    /// 429, or 529 overloaded.
    #[error("the provider is rate limiting this key")]
    RateLimited { retry_after: Option<Duration> },
    #[error("the provider call timed out")]
    Timeout,
    /// No connection, or the connection broke.
    #[error("the provider could not be reached")]
    Unreachable,
    /// The endpoint is refused by the network guard (the slug says why).
    #[error("the endpoint is not allowed: {0}")]
    Blocked(&'static str),
    /// The answer is not what the provider's API returns, or is over the size limit.
    #[error("the provider's answer could not be understood")]
    BadResponse,
    /// Any other HTTP status; the class only.
    #[error("the provider answered with status {0}")]
    Status(u16),
    /// The request itself was refused as malformed (400, 404, 422): usually a wrong model name.
    #[error("the provider refused the request: {0}")]
    Refused(String),
}

impl ModelError {
    /// The `provider_status` slug of `TestKey`.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::RateLimited { .. } => "rate_limited",
            Self::Timeout | Self::Unreachable | Self::Status(_) => "unreachable",
            Self::Blocked(_) => "blocked_address",
            Self::BadResponse | Self::Refused(_) => "bad_response",
        }
    }
}
