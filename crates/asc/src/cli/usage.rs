//! `asc usage [--owner user:NAME|org:NAME|instance] [--period YYYY-MM] [--spend key|user|repo|day|month]`:
//! what an account or organisation used (`Usage`). Without `--owner` it is your own account.

use std::process::ExitCode;

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json, size};
use super::agent::conn::{Conn, MAX_MESSAGE, when};
use super::codeintel::{clean_str, with_stdout};
use super::common::{Ctx, Result};
use crate::native::NativeSpec;
use asc_rpc::services::usage_client::UsageClient;

use super::acct::parse_owner;

fn client(s: &crate::remote::server::Session) -> UsageClient<Channel> {
    UsageClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "usage",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    /// `user:NAME`, `org:NAME` or `instance`.
    #[arg(long)]
    owner: Option<String>,
    /// `YYYY-MM`; this month when absent.
    #[arg(long)]
    period: Option<String>,
    /// Group AI spend by `key`, `user`, `repo`, `day` or `month` instead of showing the meters.
    #[arg(long)]
    spend: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "usage",
    flags: &["--owner", "--period", "--spend", "--remote", "--json"],
    placeholders: &[],
}];

pub(crate) fn group_of(s: &str) -> Result<wire::SpendGroup> {
    Ok(match s {
        "key" => wire::SpendGroup::Key,
        "user" => wire::SpendGroup::User,
        "repo" => wire::SpendGroup::Repo,
        "day" => wire::SpendGroup::Day,
        "month" => wire::SpendGroup::Month,
        _ => return Err(bad("usage", "--spend is key, user, repo, day or month")),
    })
}

fn meter(name: &str, m: Option<&wire::UsageMeter>, bytes: bool) -> String {
    let m = m.cloned().unwrap_or_default();
    let f = |n: u64| if bytes { size(n) } else { n.to_string() };
    let of = if m.limit == 0 {
        String::new()
    } else {
        format!(" of {}", f(m.limit))
    };
    format!(
        "{name:<14}{}{of}{}",
        f(m.used),
        if m.over { "  OVER" } else { "" }
    )
}

fn dollars(micro: u64) -> String {
    format!("${}.{:02}", micro / 1_000_000, micro % 1_000_000 / 10_000)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let owner = match a.owner.as_deref() {
        Some(o) => parse_owner("usage", o)?,
        None => {
            return Err(bad(
                "usage",
                "name an owner with --owner user:NAME, org:NAME or instance",
            ));
        }
    };
    let group = a.spend.as_deref().map(group_of).transpose()?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    if let Some(g) = group {
        let msg = wire::ListSpendRequest {
            owner: Some(owner),
            group_by: g as i32,
            since: 0,
            until: 0,
        };
        let r = conn
            .call(false, |s, _| {
                let m = msg.clone();
                async move {
                    Ok(client(&s)
                        .list_spend(s.wire.request(m, None))
                        .await?
                        .into_inner())
                }
            })
            .await?;
        if a.json {
            return json("asc.v1.ListSpendResponse", &r);
        }
        return with_stdout(|out| {
            for row in &r.rows {
                let label = if row.label.is_empty() {
                    String::new()
                } else {
                    format!("  {}", clean_str(&row.label))
                };
                writeln!(
                    out,
                    "{}{}  {}  {} requests",
                    clean_str(&row.group),
                    label,
                    dollars(row.cost_micro_usd),
                    row.requests
                )?;
            }
            Ok(ExitCode::SUCCESS)
        });
    }
    let msg = wire::GetUsageRequest {
        owner: Some(owner),
        period: a.period.unwrap_or_default(),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .get_usage(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    if a.json {
        return json("asc.v1.GetUsageResponse", &r);
    }
    with_stdout(|out| {
        writeln!(
            out,
            "period {} (resets {})",
            clean_str(&r.period),
            when(r.resets_at)
        )?;
        writeln!(out, "{}", meter("storage", r.storage.as_ref(), true))?;
        writeln!(
            out,
            "{}",
            meter("agent minutes", r.agent_minutes.as_ref(), false)
        )?;
        let ai = r.ai_spend.clone().unwrap_or_default();
        writeln!(
            out,
            "{:<14}{}{}{}",
            "ai spend",
            dollars(ai.used),
            if ai.limit == 0 {
                String::new()
            } else {
                format!(" of {}", dollars(ai.limit))
            },
            if ai.over { "  OVER" } else { "" }
        )?;
        writeln!(out, "{}", meter("bandwidth", r.bandwidth.as_ref(), true))?;
        if r.byo_storage || r.byo_ai_spend_micro_usd > 0 {
            writeln!(
                out,
                "bring-your-own storage and keys do not count against the quota"
            )?;
        }
        for repo in &r.repos {
            writeln!(
                out,
                "  {}  {}  {} min  {}",
                clean_str(&repo.repo),
                size(repo.storage_bytes),
                repo.agent_minutes,
                dollars(repo.ai_spend_micro_usd)
            )?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_and_money_print() {
        assert_eq!(group_of("repo").unwrap(), wire::SpendGroup::Repo);
        assert!(group_of("team").is_err());
        assert_eq!(dollars(22_400_000), "$22.40");
        assert_eq!(dollars(5_000), "$0.00");
        let m = wire::UsageMeter {
            used: 412,
            limit: 600,
            over: false,
        };
        assert_eq!(
            meter("agent minutes", Some(&m), false),
            "agent minutes 412 of 600"
        );
        let m = wire::UsageMeter {
            used: 7,
            limit: 5,
            over: true,
        };
        assert!(meter("x", Some(&m), false).ends_with("OVER"));
    }
}
