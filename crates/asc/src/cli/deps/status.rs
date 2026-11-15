//! `asc deps status [--change ID] [--json]`.
//!
//! Exit code: 0 clean, 1 findings, 2 unknown or incomplete. Usage and transport errors keep the asc
//! error codes (128 and 129), which never collide with these.

use std::fmt::Write as _;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, remote_with_token};
use crate::cli::agent::conn::Conn;
use crate::cli::codeintel::clean_str;
use crate::cli::common::{CliError, Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// A change: the hex id (40 or 64 digits) of its head changeset, or a revision of the workspace.
    /// Trunk when absent.
    #[arg(long)]
    change: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

/// The severities in the order they are shown.
const SEVERITIES: [&str; 5] = ["critical", "high", "moderate", "low", "unknown"];

/// Hex digits of a changeset id as bytes; the server takes exactly 20 or 32 bytes.
/// `None` for anything else (a revision name or an abbreviation, which the workspace resolves).
fn change_bytes(s: &str) -> Option<Vec<u8>> {
    if (s.len() != 40 && s.len() != 64) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
    Some(
        s.as_bytes()
            .chunks(2)
            .map(|p| (v(p[0]) << 4) | v(p[1]))
            .collect(),
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

async fn change_arg(ctx: &Ctx, word: &str) -> Result<Vec<u8>> {
    if let Some(b) = change_bytes(word) {
        return Ok(b);
    }
    if ctx.ws_opt().is_none() {
        return Err(bad(
            "--change outside a workspace is the hex id of the head changeset",
        ));
    }
    let cs = crate::cli::restore::resolve_commit(ctx, word)
        .await?
        .ok_or_else(|| bad(format!("bad revision '{}'", clean_str(word))))?;
    let b = cs.as_bytes().to_vec();
    if b.len() != 20 && b.len() != 32 {
        return Err(bad(
            "--change must resolve to a changeset id of 20 or 32 bytes",
        ));
    }
    Ok(b)
}

/// Exit code of a response: 0 clean, 1 findings, 2 unknown, incomplete or unspecified.
pub(crate) fn exit_of(r: &wire::DepStatusResponse) -> u8 {
    if !r.reason.is_empty() {
        return 2;
    }
    match wire::DepStatus::try_from(r.status) {
        Ok(wire::DepStatus::Clean) if r.open_by_severity.values().any(|n| *n > 0) => 2,
        Ok(wire::DepStatus::Clean) => 0,
        Ok(wire::DepStatus::Findings) => 1,
        _ => 2,
    }
}

fn word(r: &wire::DepStatusResponse) -> &'static str {
    match wire::DepStatus::try_from(r.status) {
        Ok(wire::DepStatus::Clean) => "clean",
        Ok(wire::DepStatus::Findings) => "findings",
        _ => "unknown",
    }
}

/// The severities that have a count, in display order, then any other key sorted.
fn counts(r: &wire::DepStatusResponse) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = SEVERITIES
        .iter()
        .filter_map(|s| {
            r.open_by_severity
                .get(*s)
                .filter(|n| **n > 0)
                .map(|n| ((*s).to_owned(), *n))
        })
        .collect();
    let mut rest: Vec<(String, u32)> = r
        .open_by_severity
        .iter()
        .filter(|(k, n)| **n > 0 && !SEVERITIES.contains(&k.as_str()))
        .map(|(k, n)| (clean_str(k), *n))
        .collect();
    rest.sort();
    v.extend(rest);
    v
}

/// The human text: a status line, the counts, the moves.
pub(crate) fn render(r: &wire::DepStatusResponse) -> String {
    let mut out = format!("status: {}\n", word(r));
    if !r.reason.is_empty() {
        let _ = writeln!(out, "incomplete: {}", clean_str(&r.reason));
    }
    for (k, n) in counts(r) {
        let _ = writeln!(out, "{k}: {n}");
    }
    for m in &r.moves {
        let to = if m.to.is_empty() {
            "no fixed version yet".to_owned()
        } else {
            clean_str(&m.to)
        };
        let _ = writeln!(
            out,
            "move: {} {}\t{} -> {}",
            clean_str(&m.ecosystem),
            clean_str(&m.name),
            clean_str(&m.from),
            to
        );
    }
    out
}

/// The stable JSON document with the same fields as the response.
pub(crate) fn to_json(r: &wire::DepStatusResponse) -> serde_json::Value {
    let by: serde_json::Map<String, serde_json::Value> = r
        .open_by_severity
        .iter()
        .map(|(k, n)| (k.clone(), (*n).into()))
        .collect();
    serde_json::json!({
        "status": word(r),
        "incomplete": !r.reason.is_empty(),
        "reason": r.reason,
        "exit": exit_of(r),
        "open_by_severity": by,
        "moves": r.moves.iter().map(|m| serde_json::json!({
            "ecosystem": m.ecosystem, "name": m.name, "from": m.from, "to": m.to,
        })).collect::<Vec<_>>(),
        "scanned_at": r.scanned_at,
        "scanned": hex(&r.scanned),
    })
}

pub async fn run(ctx: &Ctx, a: StatusArgs) -> Result<ExitCode> {
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let change = match a.change.as_deref() {
        Some(w) => change_arg(ctx, w).await?,
        None => Vec::new(),
    };
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::DepStatusRequest {
                repo: Some(repo.clone()),
                change: change.clone().into(),
            };
            async move {
                Ok(client(&s)
                    .status(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    let text = if a.json {
        let t = serde_json::to_string(&to_json(&r))
            .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
        format!(
            "{t}
"
        )
    } else {
        render(&r)
    };
    let mut out = std::io::stdout().lock();
    emit(&mut out, &text, exit_of(&r))
}

/// Writes the whole text, then returns `code`. A reader that closed the pipe does not change the
/// code: `asc deps status | head -1` still exits 1 with findings.
fn emit(out: &mut dyn std::io::Write, text: &str, code: u8) -> Result<ExitCode> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(ExitCode::from(code)),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::from(code)),
        Err(e) => Err(CliError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: wire::DepStatus, reason: &str) -> wire::DepStatusResponse {
        wire::DepStatusResponse {
            status: status as i32,
            reason: reason.into(),
            ..Default::default()
        }
    }

    #[test]
    fn exit_codes_follow_the_status() {
        assert_eq!(exit_of(&resp(wire::DepStatus::Clean, "")), 0);
        assert_eq!(exit_of(&resp(wire::DepStatus::Findings, "")), 1);
        assert_eq!(exit_of(&resp(wire::DepStatus::Unknown, "stale")), 2);
        assert_eq!(exit_of(&resp(wire::DepStatus::Unspecified, "")), 2);
        assert_eq!(exit_of(&resp(wire::DepStatus::Clean, "lookup partial")), 2);
    }

    fn findings() -> wire::DepStatusResponse {
        let mut r = resp(wire::DepStatus::Findings, "");
        r.open_by_severity.insert("high".into(), 2);
        r.open_by_severity.insert("low".into(), 0);
        r.moves.push(wire::DepMove {
            ecosystem: "crates.io".into(),
            name: "serde".into(),
            from: "1.0.1".into(),
            to: "1.0.2".into(),
        });
        r.moves.push(wire::DepMove {
            ecosystem: "npm".into(),
            name: "left-pad".into(),
            from: "1.0.0".into(),
            to: String::new(),
        });
        r
    }

    #[test]
    fn the_text_lists_moves_and_skips_empty_severities() {
        let t = render(&findings());
        assert!(t.starts_with("status: findings\n"));
        assert!(t.contains("high: 2\n"));
        assert!(!t.contains("low"));
        assert!(!t.contains("critical"));
        assert!(t.contains("move: crates.io serde\t1.0.1 -> 1.0.2\n"));
        assert!(t.contains("move: npm left-pad\t1.0.0 -> no fixed version yet\n"));
        let u = render(&resp(wire::DepStatus::Unknown, "lookup partial"));
        assert!(u.contains("status: unknown\n") && u.contains("incomplete: lookup partial\n"));
    }

    #[test]
    fn the_json_has_a_stable_shape() {
        let j = to_json(&findings());
        assert_eq!(j["status"], "findings");
        assert_eq!(j["incomplete"], false);
        assert_eq!(j["exit"], 1);
        assert_eq!(j["open_by_severity"]["high"], 2);
        assert_eq!(j["moves"][1]["to"], "");
        assert_eq!(j["moves"][0]["name"], "serde");
        for k in ["reason", "scanned_at", "scanned"] {
            assert!(j.get(k).is_some(), "{k}");
        }
    }

    #[test]
    fn exact_hex_ids_parse_and_anything_else_is_a_revision() {
        assert_eq!(change_bytes(&"ab".repeat(20)).unwrap().len(), 20);
        assert_eq!(change_bytes(&"01".repeat(32)).unwrap()[0], 1);
        for w in ["cafe", &"ab".repeat(16), "HEAD~1", "main", ""] {
            assert!(change_bytes(w).is_none(), "{w}");
        }
    }

    #[test]
    fn a_clean_status_with_counts_or_a_reason_is_unknown() {
        let mut r = resp(wire::DepStatus::Clean, "");
        r.open_by_severity.insert("low".into(), 1);
        assert_eq!(exit_of(&r), 2);
        r.open_by_severity.insert("low".into(), 0);
        assert_eq!(exit_of(&r), 0);
        assert_eq!(exit_of(&resp(wire::DepStatus::Clean, "stale")), 2);
    }

    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_reader_keeps_the_exit_code() {
        let mut r = findings();
        for i in 0..400 {
            r.moves.push(wire::DepMove {
                ecosystem: "npm".into(),
                name: format!("package-{i}"),
                from: "1.0.0".into(),
                to: "1.0.1".into(),
            });
        }
        let text = render(&r);
        assert!(text.len() > 8192);
        let code = emit(&mut Closed, &text, exit_of(&r)).unwrap();
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let mut sink = Vec::new();
        emit(&mut sink, &text, 1).unwrap();
        assert_eq!(sink.len(), text.len());
    }
}
