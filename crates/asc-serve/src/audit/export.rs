//! Audit export: JSON lines or CSV, in chunks of at most 1 MiB cut at a row end, with the BLAKE3
//! digest of the whole export (docs/BUILDOUT.md section 3).

use serde_json::{Value, json};
use sqlx::AnyPool;

use super::chain::{Event, hex};
use super::store::{Filter, SCAN_PAGE, page};
use super::{AuditError, Scope};

/// Largest chunk handed to the sink, in bytes.
pub const CHUNK_BYTES: usize = 1024 * 1024;

/// The export formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jsonl,
    Csv,
}

/// The CSV header row.
pub const CSV_HEADER: &str = "scope,seq,id,time_ms,time,actor_kind,actor_name,on_behalf_kind,on_behalf_name,action,repo,target,outcome,request_id,remote_addr,user_agent,details,prev_hash,hash\r\n";

/// `1970-01-01T00:00:00.000Z` for a unix time in milliseconds.
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

fn actor_json(a: &super::Actor) -> Value {
    json!({ "kind": a.kind, "name": a.name })
}

/// One JSON line (with its newline).
pub fn jsonl_row(scope: &Scope, e: &Event) -> Vec<u8> {
    let v = json!({
        "scope": scope.to_string(),
        "seq": e.seq,
        "id": e.id,
        "time_ms": e.time_ms,
        "time": rfc3339(e.time_ms),
        "actor": actor_json(&e.actor),
        "on_behalf_of": e.on_behalf_of.as_ref().map(actor_json),
        "action": e.action,
        "repo": e.repo,
        "target": e.target,
        "outcome": e.outcome.as_str(),
        "request_id": e.request_id,
        "remote_addr": e.remote_addr,
        "user_agent": e.user_agent,
        "details": e.details,
        "prev_hash": hex(&e.prev_hash),
        "hash": hex(&e.hash),
    });
    let mut line = serde_json::to_vec(&v).unwrap_or_default();
    line.push(b'\n');
    line
}

/// A CSV cell: quoted when it needs it, and neutralised when a spreadsheet would read it as a
/// formula.
pub fn csv_cell(s: &str) -> String {
    let mut text = s.to_owned();
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        text.insert(0, '\'');
    }
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

/// One CSV row (with its line end).
pub fn csv_row(scope: &Scope, e: &Event) -> Vec<u8> {
    let behalf = e.on_behalf_of.as_ref();
    let details = serde_json::to_string(&e.details).unwrap_or_default();
    let cells = [
        scope.to_string(),
        e.seq.to_string(),
        e.id.clone(),
        e.time_ms.to_string(),
        rfc3339(e.time_ms),
        e.actor.kind.clone(),
        e.actor.name.clone(),
        behalf.map(|a| a.kind.clone()).unwrap_or_default(),
        behalf.map(|a| a.name.clone()).unwrap_or_default(),
        e.action.clone(),
        e.repo.clone(),
        e.target.clone(),
        e.outcome.as_str().to_owned(),
        e.request_id.clone(),
        e.remote_addr.clone(),
        e.user_agent.clone(),
        details,
        hex(&e.prev_hash),
        hex(&e.hash),
    ];
    let mut row = cells
        .iter()
        .map(|c| csv_cell(c))
        .collect::<Vec<_>>()
        .join(",")
        .into_bytes();
    row.extend_from_slice(b"\r\n");
    row
}

/// The end of an export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub events: u64,
    pub digest: [u8; 32],
    /// More events matched than the export holds.
    pub truncated: bool,
}

/// Cuts rows into chunks of at most [`CHUNK_BYTES`] and hashes everything it is given.
pub struct Chunker {
    buf: Vec<u8>,
    hasher: blake3::Hasher,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            hasher: blake3::Hasher::new(),
        }
    }

    /// Adds a row; returns the chunk that was full when the row did not fit into it.
    pub fn push(&mut self, row: &[u8]) -> Option<Vec<u8>> {
        self.hasher.update(row);
        let flushed = if !self.buf.is_empty() && self.buf.len() + row.len() > CHUNK_BYTES {
            Some(std::mem::take(&mut self.buf))
        } else {
            None
        };
        self.buf.extend_from_slice(row);
        flushed
    }

    /// The last chunk and the digest of everything pushed.
    pub fn finish(self) -> (Vec<u8>, [u8; 32]) {
        (self.buf, *self.hasher.finalize().as_bytes())
    }
}

/// Reads the matching events of `scope`, oldest first, at most `max`, and hands the encoded chunks
/// to `sink` (which returns false when the reader is gone). Returns what the export holds.
pub async fn run<F, Fut>(
    pool: &AnyPool,
    scope: &Scope,
    filter: &Filter,
    format: Format,
    max: u64,
    mut sink: F,
) -> Result<Option<Summary>, AuditError>
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut chunker = Chunker::new();
    if format == Format::Csv
        && let Some(c) = chunker.push(CSV_HEADER.as_bytes())
        && !sink(c).await
    {
        return Ok(None);
    }
    let mut events = 0u64;
    let mut after = None;
    let mut truncated = false;
    'pages: loop {
        let want = usize::try_from(max.saturating_sub(events) + 1)
            .unwrap_or(usize::MAX)
            .min(SCAN_PAGE);
        let p = page(pool, scope, filter, true, want, after).await?;
        for e in &p.events {
            if events == max {
                truncated = true;
                break 'pages;
            }
            let row = match format {
                Format::Jsonl => jsonl_row(scope, e),
                Format::Csv => csv_row(scope, e),
            };
            if let Some(c) = chunker.push(&row)
                && !sink(c).await
            {
                return Ok(None);
            }
            events += 1;
        }
        match p.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    let (last, digest) = chunker.finish();
    if !last.is_empty() && !sink(last).await {
        return Ok(None);
    }
    Ok(Some(Summary {
        events,
        digest,
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::chain::tests_support::chain;

    #[test]
    fn times_print_as_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn jsonl_rows_are_one_json_object_per_line() {
        let (scope, events) = chain(2);
        let row = jsonl_row(&scope, &events[1]);
        assert_eq!(row.last(), Some(&b'\n'));
        let v: Value = serde_json::from_slice(&row).unwrap();
        assert_eq!(v["seq"], 2);
        assert_eq!(v["scope"], "repo:acme/app");
        assert_eq!(v["actor"]["name"], "alice");
        assert_eq!(v["on_behalf_of"], Value::Null);
        assert_eq!(v["outcome"], "success");
        assert_eq!(v["details"]["via"], "asc-push");
        assert_eq!(v["hash"].as_str().unwrap().len(), 64);
        assert_eq!(v["user_agent"], "asc/1");
    }

    #[test]
    fn csv_cells_are_quoted_and_formulas_neutralised() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(csv_cell("=cmd|' /C calc'!A0"), "'=cmd|' /C calc'!A0");
        assert_eq!(csv_cell("-1"), "'-1");
        assert_eq!(csv_cell("@sum"), "'@sum");
    }

    #[test]
    fn a_csv_row_has_one_cell_per_header_column() {
        let (scope, events) = chain(1);
        let row = String::from_utf8(csv_row(&scope, &events[0])).unwrap();
        let cols = CSV_HEADER.trim_end().split(',').count();
        // The details cell is quoted JSON with commas inside only when it has two keys; the
        // fixture has one, so a plain split counts the cells.
        assert_eq!(row.trim_end().split(',').count(), cols);
        assert!(row.ends_with("\r\n"));
    }

    #[test]
    fn chunks_are_cut_at_row_ends_and_the_digest_covers_everything() {
        let mut c = Chunker::new();
        let row = vec![b'x'; CHUNK_BYTES / 2 + 1];
        assert!(c.push(&row).is_none());
        let first = c.push(&row).expect("the second row does not fit");
        assert_eq!(first.len(), row.len());
        let (last, digest) = c.finish();
        assert_eq!(last.len(), row.len());
        let mut all = first;
        all.extend_from_slice(&last);
        assert_eq!(digest, *blake3::hash(&all).as_bytes());
    }
}
