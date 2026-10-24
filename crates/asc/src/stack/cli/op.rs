//! `asc op log [-n <n>]`, `asc op show <op>` and `asc op restore <op>` (SPEC §20.3, §20.7).
//!
//! `op log` lists the operation log newest first (`--json` wraps it as `{"ops": [...]}`), `op
//! show` prints one op with its ref movements, and `op restore` puts every ref (and a
//! detached HEAD of this workspace) back to the state right after the named op, the net of
//! undoing each later op. It refuses, like `undo`, when a ref moved since (unless `-f`).
//! `<op>` is a full id, a unique prefix of at least four hex digits, or `@` for the newest.

use std::io::Write;
use std::process::ExitCode;

use asc_state::oplog::Op;

use super::sl::{clean, fatal};
use super::undo::{
    Delta, Plan, all_ops, hints_for, net_deltas, net_deltas_upto, op_name, resolve_op, run_plan, target_hex,
    undone_set, utc,
};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::native::NativeSpec;
use crate::stack::json::{OpLog, OpRow, RefMove, line};

/// Default and largest `op log` page.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;
/// Most ref movements one row lists.
const MAX_ROW_REFS: usize = 200;
/// Most `ref_log` rows one `op log` or `op show` reads and resolves in all.
const MAX_LOG_REFS: usize = 5_000;

/// The page size for `-n`: the default when absent, never above [`MAX_LIMIT`].
fn log_limit(n: Option<usize>) -> usize {
    n.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

#[derive(clap::Parser, Debug)]
#[command(name = "op", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// The operation log, newest first.
    Log {
        /// Most ops listed.
        #[arg(short = 'n', long = "limit")]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// One operation.
    Show {
        op: String,
        #[arg(long)]
        json: bool,
    },
    /// Put the refs back to the state after an operation.
    Restore {
        op: String,
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
        #[arg(short = 'f', long = "force")]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "op",
    flags: &["-n", "--limit", "--dry-run", "-f", "--force", "--json"],
    placeholders: &[],
}];

super::verb!(OP, "op");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let ops = all_ops(ws).await?;
    match a.cmd {
        Sub::Log { limit, json } => {
            let n = log_limit(limit);
            let undone = undone_set(&ops);
            let mut rows = Vec::new();
            let mut budget = MAX_LOG_REFS;
            for o in ops.iter().rev().take(n) {
                rows.push(row(ws, o, undone.contains(&o.seq.0), &mut budget).await?);
            }
            if json {
                emit(ctx, &line(&OpLog::new(rows)).map_err(fatal)?)
            } else {
                let mut text = String::new();
                for r in &rows {
                    text.push_str(&head_line(r));
                }
                emit(ctx, &text)
            }
        }
        Sub::Show { op, json } => {
            let o = resolve_op(&ops, &op)?;
            let undone = undone_set(&ops);
            let r = row(ws, o, undone.contains(&o.seq.0), &mut { MAX_LOG_REFS }).await?;
            if json {
                return emit(ctx, &line(&OpLog::new(vec![r])).map_err(fatal)?);
            }
            let mut text = head_line(&r);
            if let (Some(b), Some(a)) = (o.head_before, o.head_after) {
                text.push_str(&format!(
                    "  head: {} -> {}\n",
                    short(&b.to_string()),
                    short(&a.to_string())
                ));
            }
            for m in &r.refs {
                text.push_str(&format!(
                    "  {}: {} -> {}\n",
                    clean(&m.name),
                    m.old.as_deref().map_or("(none)", short),
                    m.new.as_deref().map_or("(none)", short)
                ));
            }
            if let Some(s) = &r.snapshot {
                text.push_str(&format!("  snapshot: {}\n", short(s)));
            }
            emit(ctx, &text)
        }
        Sub::Restore {
            op,
            dry_run,
            force,
            json,
        } => {
            let o = resolve_op(&ops, &op)?;
            let plan = restore_plan(ws, &ops, o).await?;
            run_plan(
                ctx,
                ws,
                None,
                plan,
                force,
                dry_run,
                json,
                hints_for(&o.argv),
            )
            .await
        }
    }
}

fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}

fn head_line(r: &OpRow) -> String {
    format!(
        "{} {} {}{}\n",
        r.op.chars().take(8).collect::<String>(),
        utc(r.at),
        clean(&r.argv.join(" ")),
        if r.undone { " (undone)" } else { "" }
    )
}

/// One row. `budget` is what is left of the command's `ref_log` reads; a row past it lists no
/// refs.
async fn row(ws: &WsCtx, o: &Op, undone: bool, budget: &mut usize) -> Result<OpRow> {
    let mut refs = Vec::new();
    let (net, read) = if *budget == 0 {
        (Vec::new(), 0)
    } else {
        net_deltas_upto(ws, o, *budget).await?
    };
    *budget -= read;
    for (name, old, new) in net.into_iter().take(MAX_ROW_REFS) {
        refs.push(RefMove {
            name,
            old: target_hex(ws, old).await,
            new: target_hex(ws, new).await,
        });
    }
    Ok(OpRow {
        op: op_name(o),
        seq: u64::try_from(o.seq.0).unwrap_or(0),
        at: o.at,
        ws: o.ws.to_string(),
        argv: o.argv.split_whitespace().map(str::to_owned).collect(),
        refs,
        snapshot: o.wc_snapshot.map(|c| c.to_string()),
        undone,
    })
}

/// Everything after `target`, undone at once.
async fn restore_plan(ws: &WsCtx, ops: &[Op], target: &Op) -> Result<Plan> {
    let later: Vec<&Op> = ops.iter().filter(|o| o.seq > target.seq).collect();
    if later.is_empty() {
        return Err(CliError::Fatal(
            "asc: op restore: that is already the newest operation".into(),
        ));
    }
    let deltas = net_deltas(ws, &later)
        .await?
        .into_iter()
        .map(|(name, old, new)| Delta {
            name,
            cur: new,
            to: old,
        })
        .collect();
    let me = ws.workspace.ws.id;
    let mine_after = later.iter().rev().find(|o| o.ws == me);
    let head_cur = mine_after.and_then(|o| o.head_after);
    let head_to = mine_after.and_then(|_| {
        // The state after `target` in this workspace: the earliest later own op began there.
        later
            .iter()
            .find(|o| o.ws == me)
            .and_then(|o| o.head_before)
    });
    Ok(Plan {
        deltas,
        head_cur,
        head_to,
        snapshot: None,
        label: format!("restore {}", op_name(target)),
        argv: target.argv.clone(),
        id: op_name(target),
    })
}

fn emit(ctx: &Ctx, text: &str) -> Result<ExitCode> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    out.write_all(text.as_bytes())?;
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_limit_is_bounded() {
        assert_eq!(log_limit(None), DEFAULT_LIMIT);
        assert_eq!(log_limit(Some(3)), 3);
        assert_eq!(log_limit(Some(usize::MAX)), MAX_LIMIT);
        assert_eq!(log_limit(Some(10_000)), MAX_LIMIT);
        const { assert!(MAX_LIMIT * MAX_ROW_REFS > MAX_LOG_REFS, "the total budget binds") };
    }
}
