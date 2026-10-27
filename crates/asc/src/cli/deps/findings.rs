//! `asc deps findings | dismiss | reopen`.

use std::process::ExitCode;

use asc_rpc::server::wire;

use super::scan::finding_line;
use super::{bad, client, ecosystems, remote_with_token};
use crate::cli::agent::conn::Conn;
use crate::cli::codeintel::{emit_json, lines, with_stdout};
use crate::cli::common::{Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct FindingsArgs {
    /// Only findings in this state: open, dismissed or fixed; repeat.
    #[arg(long = "state")]
    states: Vec<String>,
    #[arg(long = "ecosystem")]
    ecosystems: Vec<String>,
    /// Only findings at or above this severity: low, moderate, high or critical.
    #[arg(long)]
    severity: Option<String>,
    /// Only this package (exact name).
    #[arg(long)]
    package: Option<String>,
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long = "page")]
    page: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct DismissArgs {
    /// The id of the finding.
    id: u64,
    /// Why: not-used, tolerable, inaccurate or fix-started.
    #[arg(long)]
    reason: String,
    /// A note, at most 1,024 bytes.
    #[arg(long)]
    note: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ReopenArgs {
    id: u64,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

pub(crate) fn state_of(name: &str) -> Result<i32> {
    use wire::DepFindingState as S;
    Ok(match name.to_ascii_lowercase().as_str() {
        "open" => S::Open,
        "dismissed" => S::Dismissed,
        "fixed" => S::Fixed,
        other => {
            return Err(bad(format!(
                "unknown state {other:?}; it is open, dismissed or fixed"
            )));
        }
    } as i32)
}

pub(crate) fn severity_of(name: &str) -> Result<i32> {
    use wire::DepSeverity as S;
    Ok(match name.to_ascii_lowercase().as_str() {
        "low" => S::Low,
        "moderate" | "medium" => S::Moderate,
        "high" => S::High,
        "critical" => S::Critical,
        other => {
            return Err(bad(format!(
                "unknown severity {other:?}; it is low, moderate, high or critical"
            )));
        }
    } as i32)
}

pub(crate) fn reason_of(name: &str) -> Result<i32> {
    use wire::DepDismissReason as R;
    Ok(match name.to_ascii_lowercase().replace('_', "-").as_str() {
        "not-used" => R::NotUsed,
        "tolerable" => R::Tolerable,
        "inaccurate" => R::Inaccurate,
        "fix-started" => R::FixStarted,
        other => {
            return Err(bad(format!(
                "unknown reason {other:?}; it is not-used, tolerable, inaccurate or fix-started"
            )));
        }
    } as i32)
}

pub async fn list(ctx: &Ctx, a: FindingsArgs) -> Result<ExitCode> {
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let states = a
        .states
        .iter()
        .map(|s| state_of(s))
        .collect::<Result<Vec<_>>>()?;
    let min = a
        .severity
        .as_deref()
        .map(severity_of)
        .transpose()?
        .unwrap_or(0);
    let ecos = ecosystems(&a.ecosystems)?;
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ListDepFindingsRequest {
                repo: Some(repo.clone()),
                states: states.clone(),
                ecosystems: ecos.clone(),
                min_severity: min,
                name: a.package.clone().unwrap_or_default(),
                limit: a.limit.unwrap_or(0),
                page_token: a.page.clone().unwrap_or_default(),
            };
            async move {
                Ok(client(&s)
                    .list_findings(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListDepFindingsResponse", &r)?;
        } else {
            for f in &r.findings {
                writeln!(out, "{}\t#{}", finding_line(f), f.id)?;
            }
            if !r.next_page_token.is_empty() {
                writeln!(out, "# more: --page {}", r.next_page_token)?;
            }
        }
        Ok(if r.findings.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

async fn set(
    ctx: &Ctx,
    remote: Option<String>,
    token_env: Option<String>,
    id: u64,
    reason: i32,
    note: String,
    json: bool,
) -> Result<ExitCode> {
    let remote = remote_with_token(remote.as_deref(), token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    if note.len() > 1024 {
        return Err(bad("--note is at most 1,024 bytes"));
    }
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(true, |s, op| {
            let m = wire::DismissDepFindingRequest {
                repo: Some(repo.clone()),
                id,
                reason,
                note: note.clone(),
            };
            async move {
                Ok(client(&s)
                    .dismiss(s.wire.request(m, op))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if json {
            emit_json(&mut lines(out), "asc.v1.DismissDepFindingResponse", &r)?;
        } else if let Some(f) = &r.finding {
            writeln!(out, "{}", finding_line(f))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub async fn dismiss(ctx: &Ctx, a: DismissArgs) -> Result<ExitCode> {
    let reason = reason_of(&a.reason)?;
    set(
        ctx,
        a.remote,
        a.token_env,
        a.id,
        reason,
        a.note.unwrap_or_default(),
        a.json,
    )
    .await
}

pub async fn reopen(ctx: &Ctx, a: ReopenArgs) -> Result<ExitCode> {
    set(ctx, a.remote, a.token_env, a.id, 0, String::new(), a.json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_values_become_enum_values_or_a_usage_error() {
        assert_eq!(
            state_of("OPEN").unwrap(),
            wire::DepFindingState::Open as i32
        );
        assert!(state_of("closed").is_err());
        assert_eq!(
            severity_of("Medium").unwrap(),
            wire::DepSeverity::Moderate as i32
        );
        assert_eq!(
            severity_of("critical").unwrap(),
            wire::DepSeverity::Critical as i32
        );
        assert!(severity_of("severe").is_err());
        assert_eq!(
            reason_of("not_used").unwrap(),
            wire::DepDismissReason::NotUsed as i32
        );
        assert_eq!(
            reason_of("fix-started").unwrap(),
            wire::DepDismissReason::FixStarted as i32
        );
        assert!(reason_of("because").is_err());
    }
}
