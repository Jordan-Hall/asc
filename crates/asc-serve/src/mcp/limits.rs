//! Body caps of the `/mcp` endpoint (SPEC 22.5, "HTTP transport").

use axum::body::Body;
use futures::StreamExt;
use serde_json::Value;

/// The largest request body of any tool but `commit`.
pub const MAX_BODY: usize = 1024 * 1024;
/// The largest request body of a `commit` call.
pub const MAX_COMMIT_BODY: usize = 4 * 1024 * 1024;

/// Why a body was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum BodyError {
    /// More bytes than the cap allows (HTTP 413).
    TooLarge,
    /// The stream failed before the end.
    Read,
}

/// Reads at most [`MAX_COMMIT_BODY`] bytes, then applies [`MAX_BODY`] unless the call is a
/// `tools/call` of `commit`. A declared `Content-Length` over the larger cap is refused before
/// any byte is read.
pub async fn read_body(body: Body, declared: Option<u64>) -> Result<(Vec<u8>, Option<Value>), BodyError> {
    if declared.is_some_and(|n| n > MAX_COMMIT_BODY as u64) {
        return Err(BodyError::TooLarge);
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BodyError::Read)?;
        if bytes.len() + chunk.len() > MAX_COMMIT_BODY {
            return Err(BodyError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() <= MAX_BODY {
        return Ok((bytes, None));
    }
    // Over 1 MiB: only a parseable `commit` call may be this large.
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| BodyError::TooLarge)?;
    if is_commit_call(&value) {
        Ok((bytes, Some(value)))
    } else {
        Err(BodyError::TooLarge)
    }
}

fn is_commit_call(v: &Value) -> bool {
    v.get("method").and_then(Value::as_str) == Some("tools/call")
        && v.pointer("/params/name").and_then(Value::as_str) == Some("commit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn small_passes_and_large_non_commit_is_refused() {
        let ok = read_body(Body::from(vec![b' '; 10]), Some(10)).await;
        assert!(ok.is_ok());
        let pad = "x".repeat(MAX_BODY + 1);
        let big = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"log","pad":pad}});
        let r = read_body(Body::from(big.to_string()), None).await;
        assert_eq!(r.unwrap_err(), BodyError::TooLarge);
        let big = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"commit","pad":pad}});
        assert!(read_body(Body::from(big.to_string()), None).await.is_ok());
    }
}
