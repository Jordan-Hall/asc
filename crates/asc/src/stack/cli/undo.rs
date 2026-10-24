//! `asc undo [-n] [-f] [--op <id>]` and `asc redo [--op <id>]` (SPEC §20.3, §20.7).
//!
//! `undo` applies the inverse of an op's `ref_log` deltas with a CAS against each ref's
//! recorded new value, puts HEAD and the work tree back (as `reset --hard` does), then applies
//! the op's working-copy snapshot. Undo is itself an op (`kind` `undo`, `undoes_seq` set), so
//! `redo` undoes the latest undo that is still in effect. Both walk the log of the current
//! workspace: a second `undo` reaches the op before the one already undone.
//!
//! Refused (exit 128): a ref moved since the op, a branch another workspace has checked out
//! (both unless `-f`), a stale `--op` (`STALE_OP`), and an undo that would leave HEAD on an
//! unborn branch. Remote effects (push, submit, land) are never undone; a `hint:` names them.
//! The work tree is saved as a snapshot first, so the undo itself can be redone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::{RefName, RepoPath};
use asc_git::projection::Head;
use asc_state::oplog::{self, Limits, NewOp, Op};
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs};
use asc_wc::status;

use super::sl::fatal;
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::restore::{WcParts, actor, index_tree, root_of, sync_worktree};
use crate::cli::stash::apply as stash_apply;
use crate::cli::stash::save;
use crate::cli::switch::held_elsewhere;
use crate::ext;
use crate::native::NativeSpec;
use crate::stack::json::{OpResult, RefMove, Status, line};

/// Most ref deltas one op may carry (a bound on untrusted `state.db` content).
const MAX_DELTAS: usize = 10_000;

#[derive(clap::Parser, Debug)]
#[command(name = "undo", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Report what would change; change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    /// Undo even when a ref moved since the op or another workspace has the branch.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    /// Refuse (STALE_OP) unless this op is the latest one in the log.
    #[arg(long = "op")]
    pub op: Option<String>,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "undo",
    flags: &["-n", "--dry-run", "-f", "--force", "--op", "--json"],
    placeholders: &[],
}];

super::verb!(UNDO, "undo");

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    inverse(
        ctx,
        Which::Undo,
        a.op.as_deref(),
        a.force,
        a.dry_run,
        a.json,
    )
    .await
}

/// `asc redo`.
pub mod redo {
    use std::process::ExitCode;

    use super::Which;
    use crate::cli::common::{Ctx, Result};
    use crate::native::NativeSpec;

    #[derive(clap::Parser, Debug)]
    #[command(name = "redo", disable_help_flag = true, disable_version_flag = true)]
    pub struct Args {
        /// Report what would change; change nothing.
        #[arg(short = 'n', long = "dry-run")]
        pub dry_run: bool,
        /// Redo even when a ref moved since the undo.
        #[arg(short = 'f', long = "force")]
        pub force: bool,
        /// Refuse (STALE_OP) unless this is the latest undone op.
        #[arg(long = "op")]
        pub op: Option<String>,
        #[arg(long)]
        pub json: bool,
    }

    pub const NATIVE: &[NativeSpec] = &[NativeSpec {
        cmd: "redo",
        flags: &["-n", "--dry-run", "-f", "--force", "--op", "--json"],
        placeholders: &[],
    }];

    super::super::verb!(REDO, "redo");

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        super::inverse(
            ctx,
            Which::Redo,
            a.op.as_deref(),
            a.force,
            a.dry_run,
            a.json,
        )
        .await
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Which {
    Undo,
    Redo,
}

/// One ref's net movement.
#[derive(Clone, Debug)]
pub(crate) struct Delta {
    pub name: String,
    /// What the ref must hold now.
    pub cur: Option<RefTarget>,
    /// What it is set to.
    pub to: Option<RefTarget>,
}

/// What to put back.
pub(crate) struct Plan {
    pub deltas: Vec<Delta>,
    /// Where the detached HEAD must be now, and where it goes.
    pub head_cur: Option<ChangesetId>,
    pub head_to: Option<ChangesetId>,
    pub snapshot: Option<ChangesetId>,
    pub label: String,
    pub argv: String,
    pub id: String,
}

// ------------------------------------------------------------------- the log

/// Every op of the repository, oldest first.
pub(crate) async fn all_ops(ws: &WsCtx) -> Result<Vec<Op>> {
    let mut ops = oplog::list(&ws.db, ws.repo.key, oplog::MAX_LIST).await?;
    ops.reverse();
    Ok(ops)
}

/// Seqs of ops whose effect was undone and not redone since.
pub(crate) fn undone_set(ops: &[Op]) -> HashSet<i64> {
    let by_seq: HashMap<i64, &Op> = ops.iter().map(|o| (o.seq.0, o)).collect();
    let mut set = HashSet::new();
    for o in ops {
        if let Some(x) = o.undoes {
            set.insert(x.0);
            if let Some(y) = by_seq.get(&x.0).and_then(|p| p.undoes) {
                set.remove(&y.0);
            }
        }
    }
    set
}

fn simple(op: &Op) -> String {
    op.op_id.0.simple().to_string()
}

/// The full id printed for an op.
pub(crate) fn op_name(op: &Op) -> String {
    op.op_id.0.to_string()
}

/// An op by `@` (the newest), a full id, or a unique prefix of at least four hex digits.
pub(crate) fn resolve_op<'a>(ops: &'a [Op], word: &str) -> Result<&'a Op> {
    if word == "@" {
        return ops
            .last()
            .ok_or_else(|| fatal("the operation log is empty"));
    }
    let w: String = word
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if w.len() < 4 || w.len() > 32 || !w.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(fatal(format!(
            "not an operation id: '{}'",
            crate::stack::cli::sl::clean(word)
        )));
    }
    let mut hits = ops.iter().filter(|o| simple(o).starts_with(&w));
    let first = hits
        .next()
        .ok_or_else(|| fatal(format!("unknown operation '{w}'")))?;
    if hits.next().is_some() {
        return Err(fatal(format!("ambiguous operation id '{w}'")));
    }
    Ok(first)
}

/// Seconds since the epoch as `YYYY-MM-DD HH:MM:SS` (UTC).
pub(crate) fn utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A ref target as git hex (a changeset), or the asc id of anything else.
pub(crate) async fn target_hex(ws: &WsCtx, t: Option<RefTarget>) -> Option<String> {
    match t? {
        RefTarget::Changeset(c) => Some(match ws.engine.git_commit_of(c).await {
            Ok(g) => g.to_string(),
            Err(_) => c.to_string(),
        }),
        RefTarget::Tag(t) => Some(t.to_string()),
    }
}

/// The `ref_log` movements of one op, oldest first: `(seq, name, old, new)`.
pub(crate) async fn ref_rows(
    ws: &WsCtx,
    op: &Op,
) -> Result<Vec<(i64, String, Option<RefTarget>, Option<RefTarget>)>> {
    let rows: Vec<(i64, String, Option<Vec<u8>>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT seq, name, old, new FROM ref_log WHERE repo_id = ? AND op_id = ? \
         ORDER BY seq LIMIT ?",
    )
    .bind(ws.repo.key.0)
    .bind(op.op_id.0.as_bytes().to_vec())
    .bind((MAX_DELTAS + 1) as i64)
    .fetch_all(ws.db.pool())
    .await
    .map_err(asc_state::Error::from)?;
    if rows.len() > MAX_DELTAS {
        return Err(fatal("the operation moved too many refs"));
    }
    let dec = |b: Option<Vec<u8>>| -> Result<Option<RefTarget>> {
        b.map(|b| RefTarget::from_bytes(&b).map_err(|_| fatal("corrupt ref_log row")))
            .transpose()
    };
    rows.into_iter()
        .map(|(s, n, o, w)| Ok((s, n, dec(o)?, dec(w)?)))
        .collect()
}

/// The net movement of every ref the `ops` touched (`HEAD` is tracked by the op rows).
pub(crate) async fn net_deltas(
    ws: &WsCtx,
    ops: &[&Op],
) -> Result<Vec<(String, Option<RefTarget>, Option<RefTarget>)>> {
    let mut rows = Vec::new();
    for op in ops {
        rows.extend(ref_rows(ws, op).await?);
        if rows.len() > MAX_DELTAS {
            return Err(fatal("the operations moved too many refs"));
        }
    }
    rows.sort_by_key(|r| r.0);
    let mut net: BTreeMap<String, (Option<RefTarget>, Option<RefTarget>)> = BTreeMap::new();
    for (_, name, old, new) in rows {
        if name == "HEAD" {
            continue;
        }
        net.entry(name)
            .and_modify(|e| e.1 = new)
            .or_insert((old, new));
    }
    Ok(net
        .into_iter()
        .filter(|(_, (o, n))| o != n)
        .map(|(k, (o, n))| (k, o, n))
        .collect())
}

fn stale_op(json: bool, msg: &str) -> CliError {
    if json {
        let v = serde_json::json!({"version": 1, "error": {"code": "STALE_OP", "message": msg}});
        CliError::Raw(v.to_string(), 128)
    } else {
        CliError::Raw(format!("fatal: {msg} (STALE_OP)"), 128)
    }
}

// ------------------------------------------------------------------- undo and redo

async fn inverse(
    ctx: &Ctx,
    which: Which,
    sel: Option<&str>,
    force: bool,
    dry: bool,
    json: bool,
) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let me = ws.workspace.ws.id;
    let verb = if which == Which::Undo { "undo" } else { "redo" };
    let ops = all_ops(ws).await?;
    let undone = undone_set(&ops);
    let target = match which {
        Which::Undo => ops
            .iter()
            .rev()
            .find(|o| o.ws == me && o.undoes.is_none() && !undone.contains(&o.seq.0)),
        Which::Redo => ops
            .iter()
            .rev()
            .find(|o| o.ws == me && o.kind == "undo" && !undone.contains(&o.seq.0)),
    };
    let Some(target) = target else {
        return nothing(ctx, json, verb);
    };
    if let Some(word) = sel {
        // `--op` names the op that is the latest (undo) or the latest undone (redo).
        let named = match which {
            Which::Undo => Some(target),
            Which::Redo => target.undoes.and_then(|s| ops.iter().find(|o| o.seq == s)),
        };
        let want = resolve_op(&ops, word).map_err(|_| {
            stale_op(
                json,
                &format!(
                    "{verb}: operation '{}' is not in the log",
                    crate::stack::cli::sl::clean(word)
                ),
            )
        })?;
        if named.map(|n| n.seq) != Some(want.seq) {
            return Err(stale_op(
                json,
                &format!(
                    "{verb}: operation {} is not the {} operation",
                    op_name(want),
                    if which == Which::Undo {
                        "latest"
                    } else {
                        "latest undone"
                    }
                ),
            ));
        }
    }
    let plan = plan_for(ws, target, true).await?;
    let hints = hints_for(&target.argv);
    execute(ctx, ws, which, target, plan, force, dry, json, hints).await
}

fn nothing(ctx: &Ctx, json: bool, verb: &str) -> Result<ExitCode> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    if json {
        out.write_all(
            line(&OpResult::new(Status::Nothing))
                .map_err(fatal)?
                .as_bytes(),
        )?;
    } else {
        writeln!(out, "Nothing to {verb}.")?;
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

/// The inverse of `op` as a plan.
pub(crate) async fn plan_for(ws: &WsCtx, op: &Op, with_snapshot: bool) -> Result<Plan> {
    let deltas = net_deltas(ws, &[op])
        .await?
        .into_iter()
        .map(|(name, old, new)| Delta {
            name,
            cur: new,
            to: old,
        })
        .collect();
    let same = op.ws == ws.workspace.ws.id;
    Ok(Plan {
        deltas,
        head_cur: if same { op.head_after } else { None },
        head_to: if same { op.head_before } else { None },
        snapshot: if same && with_snapshot {
            op.wc_snapshot
        } else {
            None
        },
        label: op.kind.clone(),
        argv: op.argv.clone(),
        id: op_name(op),
    })
}

/// `hint:` lines for what undo cannot reach.
pub(crate) fn hints_for(argv: &str) -> Vec<String> {
    let mut words = argv.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next().unwrap_or("");
    let mut out = Vec::new();
    if matches!(first, "push" | "submit" | "land" | "sync" | "share")
        || (first == "stack" && second == "merge")
    {
        out.push(format!(
            "'{first}' had remote effects; undo does not change the remote."
        ));
    }
    if first == "clean" {
        out.push("untracked and ignored files removed by 'clean' are not restored.".to_owned());
    }
    out
}

fn stale_ref(name: &str) -> CliError {
    CliError::Fatal(format!(
        "asc: '{name}' has moved since the operation; use -f to override"
    ))
}

/// The paths a hard move to `target` must rewrite (as `reset --hard` computes them).
async fn hard_paths(
    ctx: &Ctx,
    index: asc_core::id::TreeId,
    target: asc_core::id::TreeId,
) -> Result<Vec<RepoPath>> {
    let ws = ctx.ws()?;
    let mut paths: std::collections::BTreeSet<RepoPath> = ws
        .engine
        .leafdiff(index, target, None)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &ws.workspace)
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    paths.extend(st.unstaged.into_iter().map(|c| c.path));
    paths.extend(st.unmerged);
    Ok(paths.into_iter().collect())
}

/// The dirty tracked files and the staging rows as a snapshot, or `None` when nothing is
/// dirty, there is no commit, or conflicts are unresolved.
async fn snapshot_now(ctx: &Ctx, ws: &WsCtx) -> Result<Option<ChangesetId>> {
    let Some(head) = ws.head_commit().await? else {
        return Ok(None);
    };
    let root = root_of(ws, Some(head)).await?;
    let (_, unmerged) = index_tree(ws, root).await?;
    if !unmerged.is_empty() {
        return Ok(None);
    }
    Ok(save::make(ctx, ws, &save::Opts::default(), false)
        .await?
        .map(|m| m.commit))
}

fn limits(ctx: &Ctx) -> Limits {
    let num = |key: &str, default: u32| {
        ctx.config
            .get(key)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(default)
    };
    Limits {
        max_days: num("asc.opLog.maxDays", oplog::DEFAULT_MAX_DAYS),
        max_ops: num("asc.opLog.maxOps", oplog::DEFAULT_MAX_OPS),
    }
}

fn ref_name(name: &str) -> Result<RefName> {
    RefName::new(name.as_bytes().to_vec())
        .or_else(|_| RefName::new_onelevel(name.as_bytes().to_vec()))
        .map_err(fatal)
}

/// Applies `plan` and records the op. `target` is the op undone (`None` for `op restore`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute(
    ctx: &Ctx,
    ws: &WsCtx,
    which: Which,
    target: &Op,
    plan: Plan,
    force: bool,
    dry: bool,
    json: bool,
    hints: Vec<String>,
) -> Result<ExitCode> {
    run_plan(
        ctx,
        ws,
        Some((which, target)),
        plan,
        force,
        dry,
        json,
        hints,
    )
    .await
}

/// Shared by `undo`, `redo` and `op restore`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_plan(
    ctx: &Ctx,
    ws: &WsCtx,
    of: Option<(Which, &Op)>,
    plan: Plan,
    force: bool,
    dry: bool,
    json: bool,
    hints: Vec<String>,
) -> Result<ExitCode> {
    let repo = ws.repo.key;
    let verb = match of {
        Some((Which::Undo, _)) => "undo",
        Some((Which::Redo, _)) => "redo",
        None => "op",
    };
    let cur_head = ws.head_commit().await?;
    let head_ref = ws.projection.read_head()?;

    // Check every ref against what the op left, before anything moves.
    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut moves: Vec<(String, Option<RefTarget>, Option<RefTarget>)> = Vec::new();
    let mut new_head = cur_head;
    for d in &plan.deltas {
        let name = ref_name(&d.name)?;
        let now = ws.refs.get(repo, &name).await?.map(|e| e.target);
        if now != d.cur && !force {
            return Err(stale_ref(&d.name));
        }
        if now == d.to {
            continue;
        }
        if let Some(branch) = d.name.strip_prefix("refs/heads/") {
            if !force && let Some(path) = held_elsewhere(ws, branch).await? {
                return Err(CliError::Fatal(format!(
                    "asc: '{branch}' is checked out in the worktree at '{path}'; use -f to override"
                )));
            }
            if matches!(&head_ref, Head::Branch(b) if b == branch) {
                match d.to {
                    Some(RefTarget::Changeset(c)) => new_head = Some(c),
                    _ => {
                        return Err(CliError::Fatal(format!(
                            "asc: {verb}: would leave HEAD on the unborn branch '{branch}'"
                        )));
                    }
                }
            }
        }
        updates.push(RefUpdate {
            name,
            expect: now.map_or(Expect::Absent, Expect::Value),
            new: d.to,
            expires_at: None,
        });
        moves.push((d.name.clone(), now, d.to));
    }
    if let (Head::Detached(_), Some(to)) = (&head_ref, plan.head_to) {
        if cur_head != plan.head_cur && !force {
            return Err(stale_ref("HEAD"));
        }
        if cur_head != Some(to) {
            updates.push(RefUpdate {
                name: ref_name("HEAD")?,
                expect: cur_head.map_or(Expect::Absent, |c| Expect::Value(RefTarget::Changeset(c))),
                new: Some(RefTarget::Changeset(to)),
                expires_at: None,
            });
            new_head = Some(to);
        }
    }
    if updates.is_empty() && plan.snapshot.is_none() {
        return nothing(ctx, json, verb);
    }

    let mut result = OpResult::new(Status::Ok);
    for (n, o, w) in &moves {
        result.refs.push(RefMove {
            name: n.clone(),
            old: target_hex(ws, *o).await,
            new: target_hex(ws, *w).await,
        });
    }
    result.hints = hints;
    let lead = if dry { "Would undo" } else { "Undid" };
    let short: String = plan.id.chars().take(8).collect();
    if dry {
        result.op = None;
        return report(
            ctx,
            json,
            &format!("{lead} operation {short}: {}", plan.argv),
            &result,
        );
    }

    // Save the work tree first, so this op can itself be undone.
    let log_on = ctx.config.bool("asc.opLog").unwrap_or(true);
    let saved = if log_on {
        snapshot_now(ctx, ws).await.ok().flatten()
    } else {
        None
    };
    let head_before = cur_head;

    let head_root = root_of(ws, cur_head).await?;
    let target_root = root_of(ws, new_head).await?;
    let (index, _) = index_tree(ws, head_root).await?;
    let paths = hard_paths(ctx, index, target_root).await?;
    sync_worktree(ctx, target_root, &paths, true).await?;
    let gitdir = ws.projection.git_dir().to_owned();
    crate::cli::seqstate::clear_merge(&gitdir);
    crate::cli::seqstate::clear_replay(&gitdir);

    let op_id = ext::OpId::new().0;
    if !updates.is_empty() {
        let txn = RefTxn {
            updates,
            actor: actor(),
            reason: String::new(),
            op_id,
        };
        ctx.update_local_refs(
            txn,
            &format!("{verb}: {}", plan.label),
            StagingAfterMove::Set(Vec::new()),
        )
        .await?;
    }
    if let Some(snap) = plan.snapshot {
        let clean = stash_apply::apply(
            ctx,
            ws,
            snap,
            stash_apply::Opts {
                index: true,
                quiet: true,
            },
        )
        .await?;
        if !clean {
            result.hints.push(
                "the saved working copy conflicts with the restored state; resolve the conflicts."
                    .to_owned(),
            );
        }
    }

    if log_on {
        let undoes = of.and_then(|(_, t)| (verb != "op").then_some(t.seq));
        let row = NewOp {
            op_id: oplog::OpId(op_id),
            ws: ws.workspace.ws.id,
            argv: vec![verb.to_owned()],
            kind: verb.to_owned(),
            head_before,
            head_after: ws.head_commit().await?,
            wc_snapshot: saved,
            undoes,
        };
        // The dispatch bracket writes the same id; this row carries `undoes`.
        oplog::record(&ws.db, repo, &row, limits(ctx)).await?;
    }
    result.op = Some(op_id.to_string());
    report(
        ctx,
        json,
        &format!("{lead} operation {short}: {}", plan.argv),
        &result,
    )
}

fn report(ctx: &Ctx, json: bool, text: &str, r: &OpResult) -> Result<ExitCode> {
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    if json {
        out.write_all(line(r).map_err(fatal)?.as_bytes())?;
    } else {
        writeln!(out, "{}", crate::stack::cli::sl::clean(text))?;
        for m in &r.refs {
            writeln!(
                out,
                "  {}: {} -> {}",
                crate::stack::cli::sl::clean(&m.name),
                m.old.as_deref().map_or("(none)", |s| &s[..s.len().min(12)]),
                m.new.as_deref().map_or("(none)", |s| &s[..s.len().min(12)])
            )?;
        }
    }
    out.finish()?;
    if !json {
        for h in &r.hints {
            eprintln!("hint: {h}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00");
    }

    #[test]
    fn hints_name_remote_effects_and_clean() {
        assert_eq!(hints_for("push origin master").len(), 1);
        assert_eq!(hints_for("stack merge x").len(), 1);
        assert_eq!(hints_for("clean -fd").len(), 1);
        assert!(hints_for("rebase master").is_empty());
    }
}
