//! `asc agent token create | list | show | revoke | budget`: credentials for agents that run
//! outside the server (`AgentTokens`).
//!
//! The secret of a token exists once, in the answer to `create`; it is printed alone on standard
//! output so a script can capture it, and the details go to standard error. Nothing here can show
//! it again, because the server keeps only its hash.

use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::agent_tokens_client::AgentTokensClient;
use tonic::transport::Channel;

use super::bad;
use super::conn::{Conn, MAX_MESSAGE, dollars, parse_dollars, parse_seconds, short, when};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::Result;
use crate::remote::server::Session;

/// Longest a token may live: ninety days.
const MAX_TTL_SECS: u32 = 90 * 86_400;

fn client(s: &Session) -> AgentTokensClient<Channel> {
    AgentTokensClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Args, Debug)]
pub struct TokenArgs {
    #[command(subcommand)]
    cmd: TokenSub,
}

#[derive(clap::Subcommand, Debug)]
enum TokenSub {
    /// Makes a token; its secret is printed once.
    Create(CreateArgs),
    /// Lists the tokens of the repository.
    List(ListArgs),
    /// Shows one token and what is left of its budget.
    Show(IdArgs),
    /// Stops a token from working, now.
    Revoke(RevokeArgs),
    /// Sets the limits of a token (an absent limit becomes the default).
    Budget(BudgetArgs),
}

#[derive(clap::Args, Debug, Clone, Default)]
struct Limits {
    #[arg(long = "max-changes")]
    max_changes: Option<u32>,
    #[arg(long = "max-paths")]
    max_paths: Option<u32>,
    #[arg(long = "max-tokens")]
    max_tokens: Option<u64>,
    /// Most the token may spend, in dollars (`2.50`).
    #[arg(long = "max-cost")]
    max_cost: Option<String>,
    /// Longest the token's runs may take: seconds, or a number with s, m, h.
    #[arg(long = "max-seconds")]
    max_seconds: Option<String>,
}

impl Limits {
    fn wire(&self) -> Result<wire::AgentBudgetLimits> {
        Ok(wire::AgentBudgetLimits {
            max_changes: self.max_changes.unwrap_or(0),
            max_tokens: self.max_tokens.unwrap_or(0),
            max_cost_micro_usd: match &self.max_cost {
                Some(s) => parse_dollars(s)
                    .ok_or_else(|| bad("--max-cost is an amount in dollars, like 2.50"))?,
                None => 0,
            },
            max_paths: self.max_paths.unwrap_or(0),
            max_runtime_seconds: match &self.max_seconds {
                Some(s) => parse_seconds(s)
                    .ok_or_else(|| bad("--max-seconds is seconds, or a number with s, m or h"))?,
                None => 0,
            },
        })
    }
}

#[derive(clap::Args, Debug)]
struct CreateArgs {
    /// A name for the token, to tell it from others (1 to 64 bytes).
    #[arg(long)]
    name: String,
    /// The agent name the token acts under (1 to 40 of a-z, 0-9, -).
    #[arg(long, default_value = "assistant")]
    agent: String,
    /// A path the token may change (a glob); repeat. The policy's agent scope when absent.
    #[arg(long = "path")]
    paths: Vec<String>,
    /// Most changes one run of the agent may create.
    #[arg(long = "max-changes-per-run")]
    max_changes_per_run: Option<u32>,
    /// How long the token lives: seconds, or a number with s, m, h, d (at most 90d). One hour when absent.
    #[arg(long)]
    ttl: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct ListArgs {
    /// Only tokens in this state: active, expired, revoked or exhausted; repeat.
    #[arg(long = "state")]
    states: Vec<String>,
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long = "page")]
    page: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct IdArgs {
    id: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct RevokeArgs {
    id: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct BudgetArgs {
    id: String,
    #[command(flatten)]
    limits: Limits,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

fn state_wire(name: &str) -> Result<i32> {
    use wire::AgentTokenState as S;
    Ok(match name.to_ascii_lowercase().as_str() {
        "active" => S::Active,
        "expired" => S::Expired,
        "revoked" => S::Revoked,
        "exhausted" => S::Exhausted,
        other => return Err(bad(format!("unknown token state {other:?}"))),
    } as i32)
}

fn state_name(n: i32) -> String {
    wire::AgentTokenState::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |s| short(s.as_str_name(), "AGENT_TOKEN_STATE_"),
    )
}

fn id_ok(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(bad(
            "a token id looks like at_ followed by letters and digits",
        ));
    }
    Ok(())
}

fn limits_text(l: &wire::AgentBudgetLimits, u: &wire::AgentBudgetUsage) -> Vec<String> {
    let mut v = Vec::new();
    let mut row = |what: &str, used: String, limit: Option<String>| {
        v.push(format!(
            "{what:<10} {used}{}",
            limit.map_or_else(|| " (no limit)".to_owned(), |l| format!(" of {l}"))
        ));
    };
    row(
        "changes",
        u.changes.to_string(),
        (l.max_changes > 0).then(|| l.max_changes.to_string()),
    );
    row(
        "paths",
        u.paths.to_string(),
        (l.max_paths > 0).then(|| l.max_paths.to_string()),
    );
    row(
        "tokens",
        u.tokens.to_string(),
        (l.max_tokens > 0).then(|| l.max_tokens.to_string()),
    );
    row(
        "cost",
        dollars(u.cost_micro_usd),
        (l.max_cost_micro_usd > 0).then(|| dollars(l.max_cost_micro_usd)),
    );
    row(
        "runtime",
        format!("{}s", u.runtime_seconds),
        (l.max_runtime_seconds > 0).then(|| format!("{}s", l.max_runtime_seconds)),
    );
    v
}

fn token_block(out: &mut dyn std::io::Write, t: &wire::AgentToken) -> std::io::Result<()> {
    writeln!(out, "token      {}", clean_str(&t.id))?;
    writeln!(out, "name       {}", clean_str(&t.name))?;
    writeln!(out, "agent      {}", clean_str(&t.agent))?;
    writeln!(out, "state      {}", state_name(t.state))?;
    writeln!(out, "secret     ends in {}", clean_str(&t.secret_hint))?;
    if t.paths.is_empty() {
        writeln!(out, "paths      the policy's agent scope")?;
    } else {
        writeln!(
            out,
            "paths      {}",
            t.paths
                .iter()
                .map(|p| clean_str(p))
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    if t.max_changes_per_run > 0 {
        writeln!(out, "per run    {} changes", t.max_changes_per_run)?;
    }
    writeln!(
        out,
        "created    {} by {}",
        when(t.created),
        clean_str(&t.created_by)
    )?;
    writeln!(out, "expires    {}", when(t.expires_at))?;
    if t.revoked_at > 0 {
        writeln!(out, "revoked    {}", when(t.revoked_at))?;
    }
    writeln!(out, "last used  {}", when(t.last_used))?;
    for line in limits_text(&t.limits.unwrap_or_default(), &t.usage.unwrap_or_default()) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn token_line(t: &wire::AgentToken) -> String {
    format!(
        "{}\t{}\t{}\t{}\t...{}\t{}",
        clean_str(&t.id),
        state_name(t.state),
        clean_str(&t.agent),
        clean_str(&t.name),
        clean_str(&t.secret_hint),
        when(t.expires_at)
    )
}

fn print_token(json: bool, t: &wire::AgentToken) -> Result<ExitCode> {
    with_stdout(|out| {
        if json {
            emit_json(&mut lines(out), "asc.v1.AgentToken", t)?;
        } else {
            token_block(out, t)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub async fn run(ctx: &crate::cli::common::Ctx, a: TokenArgs) -> Result<ExitCode> {
    match a.cmd {
        TokenSub::Create(x) => create(ctx, x).await,
        TokenSub::List(x) => list(ctx, x).await,
        TokenSub::Show(x) => show(ctx, x).await,
        TokenSub::Revoke(x) => revoke(ctx, x).await,
        TokenSub::Budget(x) => budget(ctx, x).await,
    }
}

async fn create(ctx: &crate::cli::common::Ctx, a: CreateArgs) -> Result<ExitCode> {
    if a.name.trim().is_empty() || a.name.len() > 64 {
        return Err(bad("--name is 1 to 64 bytes"));
    }
    let ttl = match a.ttl.as_deref() {
        None => 0,
        Some(s) => {
            let n = parse_seconds(s)
                .ok_or_else(|| bad("--ttl is seconds, or a number with s, m, h or d"))?;
            if n == 0 || n > MAX_TTL_SECS {
                return Err(bad(
                    "--ttl is at least a second and at most 90d: a token always expires",
                ));
            }
            n
        }
    };
    let limits = a.limits.wire()?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::CreateAgentTokenRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.trim().to_owned(),
        agent: a.agent.clone(),
        paths: a.paths.clone(),
        max_changes_per_run: a.max_changes_per_run.unwrap_or(0),
        ttl_seconds: ttl,
        limits: Some(limits),
    };
    let r = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .create_token(s.wire.request(m, op))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    let token = r.token.clone().unwrap_or_default();
    if a.json {
        // The one place the secret is part of an answer: this is the only time the server sends it.
        return with_stdout(|out| {
            emit_json(&mut lines(out), "asc.v1.CreateAgentTokenResponse", &r)?;
            Ok(ExitCode::SUCCESS)
        });
    }
    let _ = token_block(&mut std::io::stderr(), &token);
    with_stdout(|out| {
        writeln!(out, "{}", r.secret)?;
        Ok(ExitCode::SUCCESS)
    })?;
    eprintln!(
        "asc: token {} made for agent {}; the secret above is shown once and cannot be read again; it expires {}",
        clean_str(&token.id),
        clean_str(&token.agent),
        when(token.expires_at)
    );
    Ok(ExitCode::SUCCESS)
}

async fn list(ctx: &crate::cli::common::Ctx, a: ListArgs) -> Result<ExitCode> {
    let states = a
        .states
        .iter()
        .map(|s| state_wire(s))
        .collect::<Result<Vec<_>>>()?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::ListAgentTokensRequest {
        repo: Some(conn.repo_ref()),
        states,
        limit: a.limit.unwrap_or(0),
        page_token: a.page.clone().unwrap_or_default(),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .list_tokens(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListAgentTokensResponse", &r)?;
        } else {
            for t in &r.tokens {
                writeln!(out, "{}", token_line(t))?;
            }
            if !r.next_page_token.is_empty() {
                eprintln!(
                    "asc: more tokens follow; continue with --page {}",
                    clean_str(&r.next_page_token)
                );
            }
        }
        Ok(if r.tokens.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

async fn show(ctx: &crate::cli::common::Ctx, a: IdArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let id = a.id.clone();
    let r = conn
        .call(false, |s, _| {
            let m = wire::GetAgentTokenRequest {
                repo: Some(repo.clone()),
                id: id.clone(),
            };
            async move {
                Ok(client(&s)
                    .get_token(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.GetAgentTokenResponse", &r)?;
        } else {
            token_block(out, &r.token.clone().unwrap_or_default())?;
            if let Some(rem) = &r.remaining {
                writeln!(
                    out,
                    "left       {} changes, {} tokens, {}, {} paths, {}s",
                    rem.changes,
                    rem.tokens,
                    dollars(rem.cost_micro_usd),
                    rem.paths,
                    rem.runtime_seconds
                )?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn revoke(ctx: &crate::cli::common::Ctx, a: RevokeArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::RevokeAgentTokenRequest {
        repo: Some(conn.repo_ref()),
        id: a.id.clone(),
        reason: a.reason.clone().unwrap_or_default(),
    };
    let t = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .revoke_token(s.wire.request(m, None))
                    .await?
                    .into_inner()
                    .token
                    .unwrap_or_default())
            }
        })
        .await?;
    print_token(a.json, &t)
}

async fn budget(ctx: &crate::cli::common::Ctx, a: BudgetArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let limits = a.limits.wire()?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::SetAgentTokenBudgetRequest {
        repo: Some(conn.repo_ref()),
        id: a.id.clone(),
        limits: Some(limits),
    };
    let t = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .set_budget(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .token
                    .unwrap_or_default())
            }
        })
        .await?;
    print_token(a.json, &t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(clap::Parser, Debug)]
    #[command(
        name = "agent",
        disable_help_flag = true,
        disable_version_flag = true,
        disable_help_subcommand = true
    )]
    struct Probe {
        #[command(subcommand)]
        cmd: super::super::Sub,
    }

    fn parse(args: &[&str]) -> std::result::Result<TokenArgs, clap::Error> {
        let mut argv = vec!["agent", "token"];
        argv.extend_from_slice(args);
        Probe::try_parse_from(argv).map(|p| match p.cmd {
            super::super::Sub::Token(k) => k,
            _ => unreachable!("token"),
        })
    }

    #[test]
    fn a_token_is_asked_for_with_paths_a_lifetime_and_limits_in_plain_units() {
        let a = parse(&[
            "create",
            "--name",
            "ci",
            "--agent",
            "fixer",
            "--path",
            "src/**",
            "--path",
            "docs/**",
            "--ttl",
            "2h",
            "--max-cost",
            "5",
            "--max-seconds",
            "10m",
        ])
        .unwrap();
        let TokenSub::Create(a) = a.cmd else {
            panic!("create")
        };
        assert_eq!(a.paths, ["src/**", "docs/**"]);
        let l = a.limits.wire().unwrap();
        assert_eq!(
            (l.max_cost_micro_usd, l.max_runtime_seconds),
            (5_000_000, 600)
        );
        assert!(parse(&["create"]).is_err(), "a name is required");
        let bad_cost = parse(&["budget", "at_1", "--max-cost", "lots"]).unwrap();
        let TokenSub::Budget(b) = bad_cost.cmd else {
            panic!("budget")
        };
        assert!(b.limits.wire().is_err());
    }

    #[test]
    fn states_ids_and_text_are_checked_and_clean() {
        assert_eq!(
            state_wire("Revoked").unwrap(),
            wire::AgentTokenState::Revoked as i32
        );
        assert!(state_wire("sleeping").is_err());
        assert_eq!(
            state_name(wire::AgentTokenState::Exhausted as i32),
            "exhausted"
        );
        assert!(id_ok("at_Abc123").is_ok());
        for bad in ["", "at 1", "../x", &"a".repeat(65)] {
            assert!(id_ok(bad).is_err(), "{bad:?}");
        }
        let t = wire::AgentToken {
            id: "at_1".into(),
            name: "ci\u{1b}[2J".into(),
            agent: "fixer".into(),
            state: wire::AgentTokenState::Active as i32,
            secret_hint: "wxyz".into(),
            paths: vec!["src/**".into()],
            max_changes_per_run: 3,
            limits: Some(wire::AgentBudgetLimits {
                max_changes: 10,
                max_cost_micro_usd: 2_500_000,
                ..Default::default()
            }),
            usage: Some(wire::AgentBudgetUsage {
                changes: 4,
                cost_micro_usd: 500_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut out = Vec::new();
        token_block(&mut out, &t).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(
            text.contains("ends in wxyz")
                && text.contains("4 of 10")
                && text.contains("$0.5 of $2.5"),
            "{text}"
        );
        assert!(text.contains("tokens     0 (no limit)"), "{text}");
        assert!(token_line(&t).contains("...wxyz"));
    }
}
