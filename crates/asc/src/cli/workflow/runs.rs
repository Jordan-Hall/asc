//! `asc workflow list | show | validate | enable | disable | run | runs | status | watch | cancel |
//! approve | reject`.

use std::collections::BTreeMap;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, name_ok, run_state, step_state};
use crate::cli::agent::conn::{Conn, clean_text, dollars, short, status_err, when};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{CliError, Ctx, Result};

/// Largest workflow file the server checks, in bytes.
const MAX_FILE: usize = 64 * 1024;
/// Characters of a command step's output shown.
const OUTPUT_CHARS: usize = 4000;
/// Most inputs a run is given.
const MAX_INPUTS: usize = 32;
/// Longest value of an input, in bytes.
const MAX_INPUT_VALUE: usize = 4096;

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ShowArgs {
    name: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ValidateArgs {
    /// The workflow's name, which is its file's name without `.toml`.
    name: String,
    /// The file to check; `-` for standard input. The copy on trunk when absent.
    #[arg(short = 'F', long = "file")]
    file: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct EnableArgs {
    name: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    name: String,
    /// An input as `name=value`; repeat.
    #[arg(long = "input")]
    inputs: Vec<String>,
    /// The change the run is about (hex change id).
    #[arg(long)]
    change: Option<String>,
    /// Stay and follow the run until it ends.
    #[arg(short = 'w', long)]
    watch: bool,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct RunsArgs {
    /// Only runs of this workflow.
    name: Option<String>,
    /// Only runs in this state; repeat.
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
pub struct StatusArgs {
    id: u64,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct WatchArgs {
    id: u64,
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
pub struct DecideArgs {
    id: u64,
    /// The id of the approval step.
    step: String,
    #[arg(long)]
    comment: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex_change(s: &str) -> Result<Vec<u8>> {
    let ok = s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit());
    if !ok {
        return Err(bad("--change is a change id: 32 hex characters"));
    }
    (0..32)
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| bad("--change is a change id: 32 hex characters"))
        })
        .collect()
}

fn state_wire(name: &str) -> Result<i32> {
    use wire::WorkflowRunState as S;
    Ok(match name.to_ascii_lowercase().replace('-', "_").as_str() {
        "queued" => S::Queued,
        "running" => S::Running,
        "waiting_approval" | "waiting" => S::WaitingApproval,
        "succeeded" | "done" => S::Succeeded,
        "failed" => S::Failed,
        "cancelled" | "canceled" => S::Cancelled,
        other => return Err(bad(format!("unknown state {other:?}"))),
    } as i32)
}

fn is_over(n: i32) -> bool {
    !matches!(
        wire::WorkflowRunState::try_from(n),
        Ok(wire::WorkflowRunState::Queued
            | wire::WorkflowRunState::Running
            | wire::WorkflowRunState::WaitingApproval)
    )
}

/// `name=value` pairs; a name is `[A-Za-z0-9_.-]`, a repeat is refused, and the sizes are bounded.
pub(crate) fn parse_inputs(raw: &[String]) -> Result<BTreeMap<String, String>> {
    if raw.len() > MAX_INPUTS {
        return Err(bad(format!("at most {MAX_INPUTS} inputs")));
    }
    let mut out = BTreeMap::new();
    for r in raw {
        let (k, v) = r
            .split_once('=')
            .ok_or_else(|| bad(format!("{r:?} is not name=value")))?;
        if k.is_empty()
            || k.len() > 64
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err(bad(format!("{k:?} is not an input name")));
        }
        if v.len() > MAX_INPUT_VALUE {
            return Err(bad(format!(
                "the value of {k} is longer than {MAX_INPUT_VALUE} bytes"
            )));
        }
        if out.insert(k.to_owned(), v.to_owned()).is_some() {
            return Err(bad(format!("input {k} is given twice")));
        }
    }
    Ok(out)
}

fn trigger_names(d: &wire::WorkflowDefinition) -> String {
    let names: Vec<String> = d
        .triggers
        .iter()
        .map(|t| {
            let k = wire::WorkflowTriggerKind::try_from(t.kind).map_or("?".to_owned(), |k| {
                short(k.as_str_name(), "WORKFLOW_TRIGGER_KIND_")
            });
            if t.cron.is_empty() {
                k
            } else {
                format!("{k} {}", clean_str(&t.cron))
            }
        })
        .collect();
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join(", ")
    }
}

fn step_kind(n: i32) -> &'static str {
    match wire::WorkflowStepKind::try_from(n) {
        Ok(wire::WorkflowStepKind::Agent) => "agent",
        Ok(wire::WorkflowStepKind::Approval) => "approval",
        Ok(wire::WorkflowStepKind::Checks) => "checks",
        Ok(wire::WorkflowStepKind::Command) => "command",
        _ => "?",
    }
}

fn definition_line(d: &wire::WorkflowDefinition) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        clean_str(&d.name),
        if !d.valid {
            "invalid"
        } else if d.enabled {
            "enabled"
        } else {
            "disabled"
        },
        trigger_names(d),
        if d.valid {
            clean_str(&d.description)
        } else {
            clean_str(&d.error)
        }
    )
}

fn definition_block(
    out: &mut dyn std::io::Write,
    d: &wire::WorkflowDefinition,
) -> std::io::Result<()> {
    writeln!(out, "workflow   {}", clean_str(&d.name))?;
    writeln!(out, "file       {}", clean_str(&d.path))?;
    writeln!(
        out,
        "state      {}",
        if !d.valid {
            "invalid"
        } else if d.enabled {
            "enabled"
        } else {
            "disabled"
        }
    )?;
    if !d.valid {
        writeln!(out, "error      {}", clean_str(&d.error))?;
    }
    if !d.description.is_empty() {
        writeln!(out, "about      {}", clean_str(&d.description))?;
    }
    writeln!(out, "triggers   {}", trigger_names(d))?;
    for i in &d.inputs {
        writeln!(
            out,
            "input      {}{}{}",
            clean_str(&i.name),
            if i.required { " (required)" } else { "" },
            if i.default_value.is_empty() {
                String::new()
            } else {
                format!(" = {}", clean_str(&i.default_value))
            }
        )?;
    }
    for s in &d.steps {
        let needs = if s.needs.is_empty() {
            String::new()
        } else {
            format!(
                " after {}",
                s.needs
                    .iter()
                    .map(|n| clean_str(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let what = match wire::WorkflowStepKind::try_from(s.kind) {
            Ok(wire::WorkflowStepKind::Command) => format!(
                " `{}`",
                s.command
                    .iter()
                    .map(|c| clean_str(c))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Ok(wire::WorkflowStepKind::Agent) => format!(" agent {}", clean_str(&s.agent)),
            Ok(wire::WorkflowStepKind::Approval) => format!(
                " by {}",
                s.approvers
                    .iter()
                    .map(|n| clean_str(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Ok(wire::WorkflowStepKind::Checks) => format!(
                " {}",
                s.checks
                    .iter()
                    .map(|n| clean_str(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => String::new(),
        };
        writeln!(
            out,
            "step       {} [{}]{what}{needs}",
            clean_str(&s.id),
            step_kind(s.kind)
        )?;
    }
    Ok(())
}

pub async fn list(ctx: &Ctx, a: ListArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ListWorkflowDefinitionsRequest {
                repo: Some(repo.clone()),
            };
            async move {
                Ok(client(&s)
                    .list_definitions(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(
                &mut lines(out),
                "asc.v1.ListWorkflowDefinitionsResponse",
                &r,
            )?;
        } else {
            for d in &r.definitions {
                writeln!(out, "{}", definition_line(d))?;
            }
        }
        Ok(if r.definitions.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

pub async fn show(ctx: &Ctx, a: ShowArgs) -> Result<ExitCode> {
    name_ok("the workflow name", &a.name)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let name = a.name.clone();
    let r = conn
        .call(false, |s, _| {
            let m = wire::GetWorkflowDefinitionRequest {
                repo: Some(repo.clone()),
                name: name.clone(),
            };
            async move {
                Ok(client(&s)
                    .get_definition(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.GetWorkflowDefinitionResponse", &r)?;
        } else {
            definition_block(out, &r.definition.clone().unwrap_or_default())?;
            writeln!(out, "---")?;
            out.write_all(clean_text(&String::from_utf8_lossy(&r.text)).as_bytes())?;
            writeln!(out)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn read_file(path: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    if path == "-" {
        std::io::stdin()
            .take(MAX_FILE as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| {
                CliError::Fatal(format!("asc: cannot read standard input: {}", e.kind()))
            })?;
    } else {
        std::fs::File::open(path)
            .and_then(|f| f.take(MAX_FILE as u64 + 1).read_to_end(&mut buf))
            .map_err(|e| CliError::Fatal(format!("asc: cannot read {path}: {}", e.kind())))?;
    }
    if buf.len() > MAX_FILE {
        return Err(bad(format!("the file is longer than {MAX_FILE} bytes")));
    }
    Ok(buf)
}

pub async fn validate(ctx: &Ctx, a: ValidateArgs) -> Result<ExitCode> {
    name_ok("the workflow name", &a.name)?;
    let text = a
        .file
        .as_deref()
        .map(read_file)
        .transpose()?
        .unwrap_or_default();
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::ValidateWorkflowRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.clone(),
        text: text.into(),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .validate(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ValidateWorkflowResponse", &r)?;
        } else if r.valid {
            writeln!(out, "{}: valid", clean_str(&a.name))?;
        } else {
            for d in &r.diagnostics {
                writeln!(
                    out,
                    "{}:{}:{}: {}",
                    clean_str(&a.name),
                    d.line,
                    d.column,
                    clean_str(&d.message)
                )?;
            }
        }
        Ok(if r.valid {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        })
    })
}

pub async fn set_enabled(ctx: &Ctx, a: EnableArgs, enabled: bool) -> Result<ExitCode> {
    name_ok("the workflow name", &a.name)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::SetWorkflowEnabledRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.clone(),
        enabled,
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .set_enabled(s.wire.request(m, None))
                    .await?
                    .into_inner()
                    .definition
                    .unwrap_or_default())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.WorkflowDefinition", &r)?;
        } else {
            writeln!(out, "{}", definition_line(&r))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub async fn trigger(ctx: &Ctx, a: RunArgs) -> Result<ExitCode> {
    name_ok("the workflow name", &a.name)?;
    let inputs = parse_inputs(&a.inputs)?;
    let change = a
        .change
        .as_deref()
        .map(unhex_change)
        .transpose()?
        .unwrap_or_default();
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::TriggerWorkflowRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.clone(),
        inputs: inputs.into_iter().collect(),
        change: change.into(),
    };
    let run = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .trigger(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .run
                    .unwrap_or_default())
            }
        })
        .await?;
    if a.json {
        with_stdout(|out| {
            emit_json(&mut lines(out), "asc.v1.WorkflowRun", &run)?;
            Ok(ExitCode::SUCCESS)
        })?;
    } else {
        eprintln!(
            "asc: run {} of {} is {}",
            run.id,
            clean_str(&run.workflow),
            run_state(run.state)
        );
        println!("{}", run.id);
    }
    if a.watch {
        return follow(&conn, run.id, 0, a.json).await;
    }
    Ok(ExitCode::SUCCESS)
}

fn run_line(r: &wire::WorkflowRun) -> String {
    let steps = r.steps.len();
    let done = r
        .steps
        .iter()
        .filter(|s| s.state == wire::WorkflowStepState::Succeeded as i32)
        .count();
    format!(
        "{}\t{}\t{}\t{done}/{steps} steps\t{}\t{}",
        r.id,
        run_state(r.state),
        clean_str(&r.workflow),
        dollars(r.usage.unwrap_or_default().cost_micro_usd),
        when(r.created)
    )
}

pub async fn list_runs(ctx: &Ctx, a: RunsArgs) -> Result<ExitCode> {
    if let Some(n) = &a.name {
        name_ok("the workflow name", n)?;
    }
    let states = a
        .states
        .iter()
        .map(|s| state_wire(s))
        .collect::<Result<Vec<_>>>()?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::ListWorkflowRunsRequest {
        repo: Some(conn.repo_ref()),
        name: a.name.clone().unwrap_or_default(),
        states,
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
            emit_json(&mut lines(out), "asc.v1.ListWorkflowRunsResponse", &r)?;
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

async fn get(conn: &Conn, id: u64) -> Result<wire::WorkflowRun> {
    let repo = conn.repo_ref();
    conn.call(false, |s, _| {
        let m = wire::GetWorkflowRunRequest {
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

/// The end of a command's output, cleaned and bounded, one `|` line per output line.
fn output_text(s: &str) -> String {
    let clean = clean_text(s);
    let chars: Vec<char> = clean.chars().collect();
    if chars.len() <= OUTPUT_CHARS {
        return clean;
    }
    format!(
        "...{}",
        chars[chars.len() - OUTPUT_CHARS..]
            .iter()
            .collect::<String>()
    )
}

fn run_block(out: &mut dyn std::io::Write, r: &wire::WorkflowRun) -> std::io::Result<()> {
    writeln!(out, "run        {}", r.id)?;
    writeln!(out, "workflow   {}", clean_str(&r.workflow))?;
    writeln!(out, "state      {}", run_state(r.state))?;
    writeln!(out, "started by {}", clean_str(&r.started_by))?;
    writeln!(
        out,
        "trigger    {}",
        wire::WorkflowTriggerKind::try_from(r.trigger).map_or("?".to_owned(), |k| short(
            k.as_str_name(),
            "WORKFLOW_TRIGGER_KIND_"
        ))
    )?;
    if !r.change.is_empty() {
        writeln!(out, "change     {}", hex(&r.change))?;
    }
    for (k, v) in &r.inputs {
        writeln!(out, "input      {} = {}", clean_str(k), clean_str(v))?;
    }
    let u = r.usage.unwrap_or_default();
    writeln!(
        out,
        "used       {} changes, {} tokens, {}",
        u.changes,
        u.tokens,
        dollars(u.cost_micro_usd)
    )?;
    writeln!(out, "created    {}", when(r.created))?;
    writeln!(out, "finished   {}", when(r.finished))?;
    if !r.error.is_empty() {
        writeln!(out, "error      {}", clean_str(&r.error))?;
    }
    for s in &r.steps {
        write!(
            out,
            "step       {:<16} {}",
            clean_str(&s.id),
            step_state(s.state)
        )?;
        if s.agent_run_id > 0 {
            write!(out, "  agent run {}", s.agent_run_id)?;
        }
        if s.ci_run_id > 0 {
            write!(
                out,
                "  exit {}{}  check {}",
                s.exit_code,
                if s.timed_out { " (timed out)" } else { "" },
                s.ci_run_id
            )?;
        }
        if !s.decided_by.is_empty() {
            write!(out, "  decided by {}", clean_str(&s.decided_by))?;
        }
        writeln!(out)?;
        if !s.error.is_empty() {
            writeln!(out, "             {}", clean_str(&s.error))?;
        }
        if !s.comment.is_empty() {
            writeln!(out, "             \"{}\"", clean_str(&s.comment))?;
        }
        if !s.output.is_empty() {
            for line in output_text(&s.output).lines() {
                writeln!(out, "           | {line}")?;
            }
        }
    }
    Ok(())
}

pub async fn status(ctx: &Ctx, a: StatusArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let r = get(&conn, a.id).await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.WorkflowRun", &r)?;
        } else {
            run_block(out, &r)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

/// One line for a run event: the run state and the step that changed.
fn event_line(e: &wire::WorkflowRunEvent) -> String {
    let run = e.run.clone().unwrap_or_default();
    let step = if e.step.is_empty() {
        String::new()
    } else {
        let state = run
            .steps
            .iter()
            .find(|s| s.id == e.step)
            .map_or("?".to_owned(), |s| step_state(s.state));
        format!("  step {} {state}", clean_str(&e.step))
    };
    format!("{:>4}  {}{step}", e.seq, run_state(run.state))
}

/// Prints the events of a run until it ends, resuming when the server closes a long stream.
/// Exit 0 when the run succeeded, 1 for any other end.
pub(crate) async fn follow(conn: &Conn, id: u64, after: u64, json: bool) -> Result<ExitCode> {
    let repo = conn.repo_ref();
    let mut after = after;
    loop {
        let mut stream = conn
            .call(false, |s, _| {
                let m = wire::WatchWorkflowRunRequest {
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
                    emit_json(&mut lines(out), "asc.v1.WorkflowRunEvent", &ev)?;
                } else {
                    writeln!(out, "{}", event_line(&ev))?;
                }
                Ok(ExitCode::SUCCESS)
            })?;
        }
        let run = get(conn, id).await?;
        if is_over(run.state) {
            if !json {
                with_stdout(|out| {
                    run_block(out, &run)?;
                    Ok(ExitCode::SUCCESS)
                })?;
            }
            return Ok(if run.state == wire::WorkflowRunState::Succeeded as i32 {
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
    let msg = wire::CancelWorkflowRunRequest {
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
            emit_json(&mut lines(out), "asc.v1.WorkflowRun", &r)?;
        } else {
            writeln!(out, "{}", run_line(&r))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub async fn decide(ctx: &Ctx, a: DecideArgs, approve: bool) -> Result<ExitCode> {
    name_ok("the step id", &a.step)?;
    let comment = a.comment.clone().unwrap_or_default();
    if comment.len() > 1024 {
        return Err(bad("--comment is at most 1024 bytes"));
    }
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::DecideWorkflowApprovalRequest {
        repo: Some(conn.repo_ref()),
        id: a.id,
        step: a.step.clone(),
        approve,
        comment,
    };
    let r = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .decide_approval(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .run
                    .unwrap_or_default())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.WorkflowRun", &r)?;
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

    fn parse(args: &[&str]) -> std::result::Result<super::super::Args, clap::Error> {
        let mut argv = vec!["workflow"];
        argv.extend_from_slice(args);
        super::super::Args::try_parse_from(argv)
    }

    #[test]
    fn a_run_takes_inputs_a_change_and_follow() {
        let a = parse(&[
            "run",
            "fix-ci",
            "--input",
            "branch=main",
            "--input",
            "n=3",
            "--change",
            "0123456789abcdef0123456789abcdef",
            "-w",
        ])
        .unwrap();
        let super::super::Sub::Run(a) = a.cmd else {
            panic!("run")
        };
        let inputs = parse_inputs(&a.inputs).unwrap();
        assert_eq!(inputs.get("branch").map(String::as_str), Some("main"));
        assert_eq!(
            unhex_change(a.change.as_deref().unwrap()).unwrap().len(),
            16
        );
        assert!(a.watch);
        assert!(
            parse(&["run"]).is_err()
                && parse(&["approve", "1"]).is_err()
                && parse(&["nonsense"]).is_err()
        );
    }

    #[test]
    fn inputs_changes_names_and_states_are_checked() {
        for bad in [&["noequals"][..], &["=x"], &["a b=1"], &["k=1", "k=2"]] {
            let v: Vec<String> = bad.iter().map(|s| (*s).to_owned()).collect();
            assert!(parse_inputs(&v).is_err(), "{bad:?}");
        }
        assert!(
            parse_inputs(&["k=".to_owned()]).is_ok(),
            "an empty value is a value"
        );
        assert!(parse_inputs(&[format!("k={}", "v".repeat(MAX_INPUT_VALUE + 1))]).is_err());
        assert!(
            parse_inputs(
                &(0..=MAX_INPUTS)
                    .map(|i| format!("k{i}=1"))
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
        for bad in [
            "",
            "xyz",
            "0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(unhex_change(bad).is_err(), "{bad:?}");
        }
        assert!(name_ok("n", "fix-ci_2").is_ok());
        for bad in ["", "Fix", "a b", "../x", &"a".repeat(65)] {
            assert!(name_ok("n", bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            state_wire("waiting-approval").unwrap(),
            wire::WorkflowRunState::WaitingApproval as i32
        );
        assert!(state_wire("sleeping").is_err());
        assert!(
            is_over(wire::WorkflowRunState::Failed as i32)
                && !is_over(wire::WorkflowRunState::Running as i32)
        );
    }

    #[test]
    fn a_run_is_plain_text_with_the_command_result_and_the_end_of_its_output() {
        let r = wire::WorkflowRun {
            id: 7,
            workflow: "check\u{1b}[2J".into(),
            state: wire::WorkflowRunState::Failed as i32,
            started_by: "alice".into(),
            error: "step test failed".into(),
            inputs: [("who".to_owned(), "x\u{7}y".to_owned())]
                .into_iter()
                .collect(),
            steps: vec![
                wire::WorkflowStepRun {
                    id: "test".into(),
                    state: wire::WorkflowStepState::Failed as i32,
                    exit_code: 2,
                    timed_out: true,
                    ci_run_id: 9,
                    output: format!(
                        "{}\nlast \u{1b}]0;owned\u{7}line",
                        "x".repeat(OUTPUT_CHARS * 2)
                    ),
                    ..Default::default()
                },
                wire::WorkflowStepRun {
                    id: "gate".into(),
                    state: wire::WorkflowStepState::Succeeded as i32,
                    decided_by: "bob".into(),
                    comment: "ship it".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        run_block(&mut out, &r).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
        assert!(
            text.contains("exit 2 (timed out)  check 9") && text.contains("decided by bob"),
            "{text}"
        );
        assert!(
            text.contains("| ...") && text.contains("last"),
            "output is cut at its end"
        );
        assert!(text.len() < OUTPUT_CHARS * 2, "bounded");
        assert!(run_line(&r).contains("1/2 steps"));
    }

    #[test]
    fn a_definition_shows_its_steps_and_a_bad_one_its_error() {
        let d = wire::WorkflowDefinition {
            name: "fix-ci".into(),
            path: ".asc/workflows/fix-ci.toml".into(),
            valid: true,
            enabled: true,
            triggers: vec![wire::WorkflowTrigger {
                kind: wire::WorkflowTriggerKind::Schedule as i32,
                cron: "0 3 * * *".into(),
                ..Default::default()
            }],
            steps: vec![
                wire::WorkflowStep {
                    id: "test".into(),
                    kind: wire::WorkflowStepKind::Command as i32,
                    command: vec!["cargo".into(), "test".into()],
                    ..Default::default()
                },
                wire::WorkflowStep {
                    id: "ok".into(),
                    kind: wire::WorkflowStepKind::Approval as i32,
                    needs: vec!["test".into()],
                    approvers: vec!["role:maintainer".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        definition_block(&mut out, &d).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("step       test [command] `cargo test`")
                && text.contains("[approval] by role:maintainer after test"),
            "{text}"
        );
        assert!(
            definition_line(&d).contains("enabled")
                && definition_line(&d).contains("schedule 0 3 * * *")
        );
        let bad = wire::WorkflowDefinition {
            name: "x".into(),
            valid: false,
            error: "line 3: unknown key".into(),
            ..Default::default()
        };
        assert!(
            definition_line(&bad).contains("invalid")
                && definition_line(&bad).contains("unknown key")
        );
    }
}
