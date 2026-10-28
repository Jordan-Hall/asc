//! `asc agent run | list | show | watch | cancel | say`.

use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::agents_client::AgentsClient;
use tonic::transport::Channel;

use super::bad;
use super::conn::{
    Conn, MAX_MESSAGE, dollars, parse_dollars, parse_seconds, short, status_err, when,
};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{CliError, Ctx, Result};
use crate::remote::server::Session;

/// Longest task, in bytes.
const MAX_TASK: usize = 65_536;
/// Characters of a tool call or result shown on one line.
const SHOW_CHARS: usize = 300;

fn client(s: &Session) -> AgentsClient<Channel> {
    AgentsClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// The name the agent acts under (1 to 40 of a-z, 0-9, -).
    #[arg(long, default_value = "assistant")]
    agent: String,
    #[arg(long)]
    title: Option<String>,
    /// The branch the changes are meant for; the default branch when absent.
    #[arg(long)]
    target: Option<String>,
    /// A changeset id (hex) to start from instead of the tip of the target.
    #[arg(long)]
    base: Option<String>,
    /// A path the agent may change (a glob); repeat. The policy still decides.
    #[arg(long = "path")]
    paths: Vec<String>,
    /// The id of the AI key to use; the repository's, owner's or your own when absent.
    #[arg(long = "key")]
    key: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long = "max-changes")]
    max_changes: Option<u32>,
    #[arg(long = "max-paths")]
    max_paths: Option<u32>,
    #[arg(long = "max-tokens")]
    max_tokens: Option<u64>,
    /// Most the run may spend, in dollars (`2.50`).
    #[arg(long = "max-cost")]
    max_cost: Option<String>,
    /// Longest the run may take: seconds, or a number with s, m, h.
    #[arg(long = "max-seconds")]
    max_seconds: Option<String>,
    /// Stay and follow the run until it ends.
    #[arg(short = 'w', long)]
    watch: bool,
    /// Read the task from this file (`-` for standard input).
    #[arg(short = 'F', long = "file")]
    file: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
    /// The task, as words.
    task: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Only runs in this state; repeat.
    #[arg(long = "state")]
    states: Vec<String>,
    #[arg(long)]
    agent: Option<String>,
    /// Only runs started by this person.
    #[arg(long = "by")]
    by: Option<String>,
    #[arg(long)]
    limit: Option<u32>,
    /// Continue after the page token a previous list printed.
    #[arg(long = "page")]
    page: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ShowArgs {
    id: u64,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct WatchArgs {
    id: u64,
    /// Start after this event number.
    #[arg(long)]
    after: Option<u64>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct CancelArgs {
    id: u64,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct SayArgs {
    id: u64,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
    /// The message, as words.
    text: Vec<String>,
}

fn state_wire(name: &str) -> Result<i32> {
    use wire::AgentRunState as S;
    Ok(match name.to_ascii_lowercase().replace('-', "_").as_str() {
        "queued" => S::Queued,
        "running" => S::Running,
        "waiting_input" | "waiting" => S::WaitingInput,
        "succeeded" | "done" => S::Succeeded,
        "failed" => S::Failed,
        "cancelled" | "canceled" => S::Cancelled,
        "budget_exceeded" | "budget" => S::BudgetExceeded,
        "expired" => S::Expired,
        other => return Err(bad(format!("unknown state {other:?}"))),
    } as i32)
}

pub(crate) fn state_name(n: i32) -> String {
    wire::AgentRunState::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |s| short(s.as_str_name(), "AGENT_RUN_STATE_"),
    )
}

fn is_over(n: i32) -> bool {
    !matches!(
        wire::AgentRunState::try_from(n),
        Ok(wire::AgentRunState::Queued
            | wire::AgentRunState::Running
            | wire::AgentRunState::WaitingInput)
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn limits_line(l: &wire::AgentBudgetLimits) -> String {
    let mut v = Vec::new();
    if l.max_changes > 0 {
        v.push(format!("{} changes", l.max_changes));
    }
    if l.max_paths > 0 {
        v.push(format!("{} paths", l.max_paths));
    }
    if l.max_tokens > 0 {
        v.push(format!("{} tokens", l.max_tokens));
    }
    if l.max_cost_micro_usd > 0 {
        v.push(dollars(l.max_cost_micro_usd));
    }
    if l.max_runtime_seconds > 0 {
        v.push(format!("{}s", l.max_runtime_seconds));
    }
    if v.is_empty() {
        "none".to_owned()
    } else {
        v.join(", ")
    }
}

fn usage_line(u: &wire::AgentBudgetUsage) -> String {
    format!(
        "{} changes, {} tokens, {}, {} requests, {}s",
        u.changes,
        u.tokens,
        dollars(u.cost_micro_usd),
        u.requests,
        u.runtime_seconds
    )
}

fn task_text(a: &RunArgs) -> Result<String> {
    use std::io::Read as _;
    let text = match a.file.as_deref() {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .take(MAX_TASK as u64 + 1)
                .read_to_string(&mut s)
                .map_err(|e| {
                    CliError::Fatal(format!("asc: cannot read standard input: {}", e.kind()))
                })?;
            s
        }
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| CliError::Fatal(format!("asc: cannot read {path}: {}", e.kind())))?,
        None => a.task.join(" "),
    };
    if text.trim().is_empty() {
        return Err(bad(
            "a task is required: give it as words or with -F <file>",
        ));
    }
    if text.len() > MAX_TASK {
        return Err(bad(format!("the task is longer than {MAX_TASK} bytes")));
    }
    Ok(text)
}

fn run_line(r: &wire::AgentRun) -> String {
    let u = r.usage.unwrap_or_default();
    format!(
        "{}\t{}\t{}\t{} changes\t{}\t{}\t{}",
        r.id,
        state_name(r.state),
        clean_str(&r.agent),
        r.changes.len(),
        dollars(u.cost_micro_usd),
        when(r.created),
        clean_str(&r.title)
    )
}

pub async fn run(ctx: &Ctx, a: RunArgs) -> Result<ExitCode> {
    let task = task_text(&a)?;
    let limits = wire::AgentBudgetLimits {
        max_changes: a.max_changes.unwrap_or(0),
        max_tokens: a.max_tokens.unwrap_or(0),
        max_cost_micro_usd: match &a.max_cost {
            Some(s) => parse_dollars(s)
                .ok_or_else(|| bad("--max-cost is an amount in dollars, like 2.50"))?,
            None => 0,
        },
        max_paths: a.max_paths.unwrap_or(0),
        max_runtime_seconds: match &a.max_seconds {
            Some(s) => parse_seconds(s)
                .ok_or_else(|| bad("--max-seconds is seconds, or a number with s, m or h"))?,
            None => 0,
        },
    };
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::CreateAgentRunRequest {
        repo: Some(conn.repo_ref()),
        agent: a.agent.clone(),
        title: a.title.clone().unwrap_or_default(),
        task,
        target: a.target.clone().unwrap_or_default(),
        base: a.base.clone().unwrap_or_default(),
        paths: a.paths.clone(),
        ai_key_id: a.key.clone().unwrap_or_default(),
        model: a.model.clone().unwrap_or_default(),
        limits: Some(limits),
        ..Default::default()
    };
    let run = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .create_run(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .run
                    .unwrap_or_default())
            }
        })
        .await?;
    if a.json {
        with_stdout(|out| {
            emit_json(&mut lines(out), "asc.v1.AgentRun", &run)?;
            Ok(ExitCode::SUCCESS)
        })?;
    } else {
        eprintln!(
            "asc: run {} queued as {} (model {})",
            run.id,
            clean_str(&run.agent),
            clean_str(&run.model)
        );
        println!("{}", run.id);
    }
    if a.watch {
        return follow(&conn, run.id, 0, a.json).await;
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn list(ctx: &Ctx, a: ListArgs) -> Result<ExitCode> {
    let states = a
        .states
        .iter()
        .map(|s| state_wire(s))
        .collect::<Result<Vec<_>>>()?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::ListAgentRunsRequest {
        repo: Some(conn.repo_ref()),
        states,
        agent: a.agent.clone().unwrap_or_default(),
        created_by: a.by.clone().unwrap_or_default(),
        limit: a.limit.unwrap_or(0),
        page_token: a.page.clone().unwrap_or_default(),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .list_runs(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListAgentRunsResponse", &r)?;
        } else {
            for run in &r.runs {
                writeln!(out, "{}", run_line(run))?;
            }
            if !r.next_page_token.is_empty() {
                eprintln!(
                    "asc: more runs follow; continue with --page {}",
                    clean_str(&r.next_page_token)
                );
            }
        }
        Ok(if r.runs.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

async fn get(conn: &Conn, id: u64) -> Result<wire::AgentRun> {
    let repo = conn.repo_ref();
    conn.call(false, |s, _| {
        let m = wire::GetAgentRunRequest {
            repo: Some(repo.clone()),
            id,
        };
        async move {
            Ok(client(&s)
                .get_run(s.wire.request(m, None))
                .await?
                .into_inner()
                .run
                .unwrap_or_default())
        }
    })
    .await
}

pub async fn show(ctx: &Ctx, a: ShowArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let r = get(&conn, a.id).await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.AgentRun", &r)?;
            return Ok(ExitCode::SUCCESS);
        }
        writeln!(out, "run       {}", r.id)?;
        writeln!(out, "state     {}", state_name(r.state))?;
        writeln!(out, "agent     {}", clean_str(&r.agent))?;
        if !r.title.is_empty() {
            writeln!(out, "title     {}", clean_str(&r.title))?;
        }
        writeln!(out, "started by {}", clean_str(&r.created_by))?;
        writeln!(
            out,
            "target    {} (never written by the agent)",
            clean_str(&r.target)
        )?;
        writeln!(
            out,
            "model     {} (key {})",
            clean_str(&r.model),
            clean_str(&r.ai_key_id)
        )?;
        writeln!(
            out,
            "limits    {}",
            limits_line(&r.limits.unwrap_or_default())
        )?;
        writeln!(
            out,
            "used      {}",
            usage_line(&r.usage.unwrap_or_default())
        )?;
        writeln!(out, "created   {}", when(r.created))?;
        writeln!(out, "finished  {}", when(r.finished))?;
        if r.workflow_run_id > 0 {
            writeln!(
                out,
                "workflow  run {} step {}",
                r.workflow_run_id,
                clean_str(&r.workflow_step)
            )?;
        }
        if !r.failure_code.is_empty() || !r.failure_message.is_empty() {
            writeln!(
                out,
                "ended     {}: {}",
                clean_str(&r.failure_code),
                clean_str(&r.failure_message)
            )?;
        }
        for (i, c) in r.changes.iter().enumerate() {
            writeln!(out, "change {}  {}", i + 1, hex(c))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn event_line(e: &wire::AgentRunEvent) -> String {
    use wire::AgentRunEventKind as K;
    let kind = K::try_from(e.kind).map_or("?", |k| match k {
        K::State => "state",
        K::Message => "agent",
        K::Input => "you",
        K::ToolCall => "call",
        K::ToolResult => "result",
        K::Change => "change",
        K::Usage => "usage",
        K::Error => "error",
        K::Unspecified => "?",
    });
    let mut text = e
        .text
        .lines()
        .map(clean_str)
        .collect::<Vec<_>>()
        .join(" | ");
    if text.chars().count() > SHOW_CHARS {
        text = format!("{}...", text.chars().take(SHOW_CHARS).collect::<String>());
    }
    let tool = if e.tool.is_empty() {
        String::new()
    } else {
        format!(" {}", clean_str(&e.tool))
    };
    let change = if e.change.is_empty() {
        String::new()
    } else {
        format!(" [{}]", hex(&e.change))
    };
    format!("{:>4}  {:<6}{tool}{change} {text}", e.seq, kind)
}

/// Prints the events of a run until it ends, resuming when the server closes a long stream.
/// Exit 0 when the run succeeded, 1 for any other end.
pub(crate) async fn follow(conn: &Conn, id: u64, after: u64, json: bool) -> Result<ExitCode> {
    let repo = conn.repo_ref();
    let mut after = after;
    loop {
        let mut stream = conn
            .call(false, |s, _| {
                let m = wire::WatchAgentRunRequest {
                    repo: Some(repo.clone()),
                    id,
                    after_seq: after,
                };
                async move {
                    Ok(client(&s)
                        .watch_run(s.wire.request(m, None))
                        .await?
                        .into_inner())
                }
            })
            .await?;
        while let Some(ev) = stream.message().await.map_err(|s| status_err(&s))? {
            after = ev.seq;
            with_stdout(|out| {
                if json {
                    emit_json(&mut lines(out), "asc.v1.AgentRunEvent", &ev)?;
                } else {
                    writeln!(out, "{}", event_line(&ev))?;
                }
                Ok(ExitCode::SUCCESS)
            })?;
        }
        let run = get(conn, id).await?;
        if is_over(run.state) {
            return Ok(if run.state == wire::AgentRunState::Succeeded as i32 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
    }
}

pub async fn watch(ctx: &Ctx, a: WatchArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    follow(&conn, a.id, a.after.unwrap_or(0), a.json).await
}

pub async fn cancel(ctx: &Ctx, a: CancelArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::CancelAgentRunRequest {
        repo: Some(conn.repo_ref()),
        id: a.id,
        reason: a.reason.clone().unwrap_or_default(),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .cancel_run(s.wire.request(m, None))
                    .await?
                    .into_inner()
                    .run
                    .unwrap_or_default())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.AgentRun", &r)?;
        } else {
            writeln!(out, "{}", run_line(&r))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub async fn say(ctx: &Ctx, a: SayArgs) -> Result<ExitCode> {
    let text = a.text.join(" ");
    if text.trim().is_empty() {
        return Err(bad("a message is required"));
    }
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::SendAgentInputRequest {
        repo: Some(conn.repo_ref()),
        id: a.id,
        text,
    };
    let r = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .send_input(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .run
                    .unwrap_or_default())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.AgentRun", &r)?;
        } else {
            writeln!(out, "{}", run_line(&r))?;
        }
        Ok(ExitCode::SUCCESS)
    })
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

    #[test]
    fn a_run_is_asked_for_in_words_with_limits_in_plain_units() {
        let p = Probe::try_parse_from([
            "agent",
            "run",
            "--agent",
            "fixer",
            "--path",
            "src/**",
            "--max-cost",
            "2.50",
            "-w",
            "fix",
            "the",
            "bug",
        ])
        .unwrap();
        let super::super::Sub::Run(a) = p.cmd else {
            panic!("run")
        };
        assert_eq!(
            (
                a.agent.as_str(),
                a.paths.as_slice(),
                a.max_cost.as_deref(),
                a.watch
            ),
            ("fixer", &["src/**".to_owned()][..], Some("2.50"), true)
        );
        assert_eq!(task_text(&a).unwrap(), "fix the bug");
        let none = Probe::try_parse_from(["agent", "run"]).unwrap();
        let super::super::Sub::Run(a) = none.cmd else {
            panic!("run")
        };
        assert!(task_text(&a).is_err(), "a task is required");
        assert!(Probe::try_parse_from(["agent", "run", "--bogus", "x"]).is_err());
        assert!(
            Probe::try_parse_from(["agent"]).is_err(),
            "a subcommand is required"
        );
    }

    #[test]
    fn states_have_names_and_events_are_one_clean_line() {
        assert_eq!(
            state_wire("waiting-input").unwrap(),
            wire::AgentRunState::WaitingInput as i32
        );
        assert_eq!(
            state_wire("Budget").unwrap(),
            wire::AgentRunState::BudgetExceeded as i32
        );
        assert!(state_wire("sleeping").is_err());
        assert_eq!(
            state_name(wire::AgentRunState::BudgetExceeded as i32),
            "budget_exceeded"
        );
        assert!(
            is_over(wire::AgentRunState::Failed as i32)
                && !is_over(wire::AgentRunState::Running as i32)
        );
        let e = wire::AgentRunEvent {
            seq: 7,
            kind: wire::AgentRunEventKind::ToolResult as i32,
            tool: "read\u{1b}_file".into(),
            text: "line one\nline two\u{7}".into(),
            ..Default::default()
        };
        let line = event_line(&e);
        assert!(
            !line.contains('\u{1b}') && !line.contains('\u{7}') && !line.contains('\n'),
            "{line:?}"
        );
        assert!(line.contains("result") && line.contains("line one | line two"));
        let long = wire::AgentRunEvent {
            text: "x".repeat(1000),
            ..Default::default()
        };
        assert!(event_line(&long).len() < 400);
    }
}
