//! `asc deps status [--change ID] [--json]`.
//!
//! Exit code: 0 clean, 1 findings, 2 unknown or incomplete. Usage and transport errors keep the asc
//! error codes (128 and 129), which never collide with these.

use std::fmt::Write as _;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, remote_with_token};
use crate::cli::agent::conn::Conn;
use crate::cli::codeintel::{clean_str, with_stdout};
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
/// `None` when `s` is not hex at all (a revision name).
fn change_bytes(s: &str) -> Result<Option<Vec<u8>>> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    if s.len() != 40 && s.len() != 64 {
        return Err(bad(format!(
            "--change must be a changeset id of 20 or 32 bytes (40 or 64 hex digits), not {} digits",
            s.len()
        )));
    }
    let v = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
    Ok(Some(
        s.as_bytes()
            .chunks(2)
            .map(|p| (v(p[0]) << 4) | v(p[1]))
            .collect(),
    ))
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

async fn change_arg(ctx: &Ctx, word: &str) -> Result<Vec<u8>> {
    if let Some(b) = change_bytes(word)? {
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
    Ok(cs.as_bytes().to_vec())
}

/// Exit code of a response: 0 clean, 1 findings, 2 unknown, incomplete or unspecified.
pub(crate) fn exit_of(r: &wire::DepStatusResponse) -> u8 {
    if !r.reason.is_empty() {
        return 2;
    }
    match wire::DepStatus::try_from(r.status) {
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
    with_stdout(|out| {
        if a.json {
            let text = serde_json::to_string(&to_json(&r))
                .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
            writeln!(out, "{text}")?;
        } else {
            write!(out, "{}", render(&r))?;
        }
        Ok(ExitCode::from(exit_of(&r)))
    })
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
    fn change_ids_are_20_or_32_bytes() {
        assert_eq!(change_bytes(&"ab".repeat(20)).unwrap().unwrap().len(), 20);
        assert_eq!(change_bytes(&"01".repeat(32)).unwrap().unwrap()[0], 1);
        assert!(change_bytes(&"ab".repeat(16)).is_err());
        assert!(change_bytes("abc").is_err());
        assert!(change_bytes("HEAD~1").unwrap().is_none());
    }
}
