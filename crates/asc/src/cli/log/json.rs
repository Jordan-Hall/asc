//! `log --json`: `LogResponse` messages as JSON lines through the shared writer (SPEC §11.2).

use std::io::Write;

use asc_core::sig::Signature;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};

use super::walk::{Entry, Walker};
use crate::cli::common::{CliError, Result, WsCtx};
use crate::cli::json::JsonLines;

/// Log entries per `LogResponse` line (the plugin API's page cap).
const PAGE: usize = 200;

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec().into())
}

fn signature(desc: &MessageDescriptor, s: &Signature) -> Value {
    let mut m = DynamicMessage::new(desc.clone());
    m.set_field_by_name("name", bytes(s.name()));
    m.set_field_by_name("email", bytes(s.email()));
    m.set_field_by_name("time", Value::I64(s.time()));
    m.set_field_by_name("tz_min", Value::I32(i32::from(s.tz_min())));
    Value::Message(m)
}

fn log_entry(pool: &DescriptorPool, e: &Entry) -> Result<Value> {
    let get = |n: &str| {
        pool.get_message_by_name(n)
            .ok_or_else(|| fatal(format!("asc: missing message {n}")))
    };
    let mut m = DynamicMessage::new(get("asc.v1.LogEntry")?);
    let sig = get("asc.v1.Signature")?;
    m.set_field_by_name("id", bytes(e.id.as_bytes()));
    m.set_field_by_name("git", bytes(e.git.as_bytes()));
    m.set_field_by_name(
        "parents",
        Value::List(e.cs.parents.iter().map(|p| bytes(p.as_bytes())).collect()),
    );
    m.set_field_by_name("author", signature(&sig, &e.cs.author));
    m.set_field_by_name("committer", signature(&sig, &e.cs.committer));
    m.set_field_by_name("message", bytes(&e.cs.message));
    m.set_field_by_name("gen", Value::U32(e.generation));
    Ok(Value::Message(m))
}

/// Streams `walker` as `LogResponse` lines of up to [`PAGE`] entries.
pub async fn emit(_ws: &WsCtx, mut walker: Walker<'_>, reverse: bool) -> Result<()> {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(fatal)?;
    let resp = pool
        .get_message_by_name("asc.v1.LogResponse")
        .ok_or_else(|| fatal("asc: missing message asc.v1.LogResponse"))?;
    let stdout = std::io::stdout();
    let mut lines = JsonLines::new(std::io::BufWriter::new(stdout.lock()));
    let mut page: Vec<Value> = Vec::new();
    let flush = |lines: &mut JsonLines<_>, page: &mut Vec<Value>| -> Result<()> {
        if page.is_empty() {
            return Ok(());
        }
        let mut m = DynamicMessage::new(resp.clone());
        m.set_field_by_name("count", Value::U64(page.len() as u64));
        m.set_field_by_name("entries", Value::List(std::mem::take(page)));
        lines.emit(&m).map_err(fatal)
    };
    let mut all: Vec<Entry> = Vec::new();
    while let Some(e) = walker.next().await? {
        if reverse {
            all.push(e);
            continue;
        }
        page.push(log_entry(&pool, &e)?);
        if page.len() == PAGE {
            flush(&mut lines, &mut page)?;
        }
    }
    for e in all.iter().rev() {
        page.push(log_entry(&pool, e)?);
        if page.len() == PAGE {
            flush(&mut lines, &mut page)?;
        }
    }
    flush(&mut lines, &mut page)?;
    lines.finish()?.flush()?;
    Ok(())
}
