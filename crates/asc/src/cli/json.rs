//! The shared JSON writer (SPEC §11.2): JSON lines, one object per stream message, for
//! `status|diff|log|blame --json`, `asc watch --json` and `asc affected --json`. Every caller goes
//! through [`JsonLines`], so the encoding of a message is chosen here and nowhere else.
#![allow(dead_code)] // Used by the commands of later units.

use std::io::{self, Write};

use prost_reflect::DynamicMessage;
use serde_json::Value;

/// Why a message could not be written.
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error("json: {0}")]
    Encode(#[from] asc_rpc::json::JsonError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Encodes one message as a JSON value.
///
/// This is the single seam M01c switches (SPEC §22.4.2): it prints through the asc JSON profile
/// codec `asc_rpc::json`, whose lowercase hex ids, snake_case names and number-valued 64-bit
/// integers the plugin API documents.
pub fn encode_message(msg: &DynamicMessage) -> Result<Value, JsonError> {
    Ok(asc_rpc::json::encode(msg)?)
}

/// Writes JSON lines to `W`.
pub struct JsonLines<W: Write> {
    out: W,
}

impl<W: Write> JsonLines<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    /// One object on one line.
    pub fn emit_value(&mut self, value: &Value) -> Result<(), JsonError> {
        serde_json::to_writer(&mut self.out, value).map_err(io::Error::from)?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    /// One protobuf message on one line.
    pub fn emit(&mut self, msg: &DynamicMessage) -> Result<(), JsonError> {
        self.emit_value(&encode_message(msg)?)
    }

    /// Flushes and returns the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn one_object_per_line() {
        let mut w = JsonLines::new(Vec::new());
        w.emit_value(&json!({"a": 1})).unwrap();
        w.emit_value(&json!({"b": "x\ny"})).unwrap();
        let out = String::from_utf8(w.finish().unwrap()).unwrap();
        assert_eq!(out, "{\"a\":1}\n{\"b\":\"x\\ny\"}\n");
        assert_eq!(out.lines().count(), 2);
    }
}
