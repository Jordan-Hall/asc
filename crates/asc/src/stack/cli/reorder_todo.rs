//! `asc reorder --todo <file>` (SPEC §11.2, §24.3.2): applies a `StackTodo` as one operation.
//!
//! The file is the proto3 JSON of `StackTodo { items }` (TODO(M02): the message moves to
//! `cli.proto` when M02 lands; the JSON shape is the one it will have). Each item is
//! `{"action": "pick"|"fold"|"drop"|"reword", "change": "<id prefix>", "message": "..."}`,
//! ordered bottom to top. Every change of the selected part of the stack (up to the checked-out
//! change; `--stack`: the whole stack) is named exactly once.
//!
//! * `pick` keeps the change, in its new place.
//! * `reword` keeps it and sets `message` (required).
//! * `fold` folds the change into the nearest kept change below it in the list: the result has
//!   that change's id, author and message (or `message` when given) and the folded change's tree.
//! * `drop` removes the change from the stack, as `asc abandon` does: refs that pointed at it
//!   follow the kept change below it.
//!
//! Everything is replayed in memory first, so a conflict moves nothing; the refs then move in
//! one transaction (`restack::apply`), which is one op-log entry. The file is read only when it
//! is a small regular file (the bridge writes it into its private `$ASC_PLUGIN_TMP`).

use std::io::Read as _;
use std::path::Path;
use std::process::ExitCode;

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;
use serde::Deserialize;

use super::info::resolve_change;
use super::move_::{Replay, commit_pairs, conflict_row, made_of, pre_rebase, replay};
use super::reorder::Args;
use super::restack::Flow;
use super::reword::refuse_busy;
use super::rewrite::{Made, Piece, emit, emit_nothing, locate, record, write_piece};
use super::sl::{Local, clean};
use super::uncommit::subject_of;
use crate::cli::common::{CliError, Ctx, Result};

/// Longest todo file read back.
const MAX_FILE: u64 = 1024 * 1024;
/// Most items in one todo.
const MAX_ITEMS: usize = 4096;
/// Longest message of one item.
const MAX_MESSAGE: usize = 64 * 1024;
/// Shortest change-id prefix an item may use.
const MIN_PREFIX: usize = 4;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct StackTodo {
    pub items: Vec<TodoItem>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub action: Action,
    pub change: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pick,
    Fold,
    Drop,
    Reword,
}

/// One validated item: which selected change, what to do, and the message to use.
#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    pub idx: usize,
    pub action: Action,
    pub message: Option<Vec<u8>>,
}

fn normal(message: &str) -> Vec<u8> {
    let mut m = message.trim_end().as_bytes().to_vec();
    m.push(b'\n');
    m
}

/// Checks `todo` against the change ids of the selected stack (bottom first).
pub fn plan(todo: &StackTodo, names: &[String]) -> std::result::Result<Vec<Step>, String> {
    if todo.items.len() > MAX_ITEMS {
        return Err("the todo has too many items".into());
    }
    let mut seen = vec![false; names.len()];
    let mut steps = Vec::with_capacity(todo.items.len());
    let mut kept = 0usize;
    for it in &todo.items {
        let word = it.change.trim();
        if word.len() < MIN_PREFIX {
            return Err(format!("'{}' is too short to name a change", clean(word)));
        }
        let hits: Vec<usize> = (0..names.len())
            .filter(|&i| names[i].starts_with(word))
            .collect();
        let idx = match hits.as_slice() {
            [i] if !seen[*i] => *i,
            [_] => return Err(format!("change {} is listed twice", clean(word))),
            [] => return Err(format!("'{}' names no change of the list", clean(word))),
            _ => return Err(format!("'{}' names several changes", clean(word))),
        };
        seen[idx] = true;
        let message = match (it.action, it.message.as_deref()) {
            (Action::Reword, Some(m)) if !m.trim().is_empty() => Some(m),
            (Action::Reword, _) => {
                return Err(format!("reword of {} needs a message", clean(word)));
            }
            (Action::Fold, m) => m.filter(|m| !m.trim().is_empty()),
            (_, None) => None,
            (_, Some(_)) => {
                return Err(format!(
                    "only reword and fold take a message ({})",
                    clean(word)
                ));
            }
        };
        if let Some(m) = message {
            if m.len() > MAX_MESSAGE {
                return Err(format!("the message of {} is too long", clean(word)));
            }
            if m.contains('\0') {
                return Err(format!("the message of {} has a NUL byte", clean(word)));
            }
        }
        if it.action == Action::Fold && kept == 0 {
            return Err(format!(
                "{} has no kept change below it to fold into",
                clean(word)
            ));
        }
        if it.action != Action::Drop {
            kept += 1;
        }
        steps.push(Step {
            idx,
            action: it.action,
            message: message.map(normal),
        });
    }
    if seen.iter().any(|s| !s) {
        return Err("a change is missing from the todo (name it with 'drop' to remove it)".into());
    }
    if kept == 0 {
        return Err("the todo drops every change (use 'asc abandon')".into());
    }
    Ok(steps)
}

fn read_todo(file: &Path) -> Result<StackTodo> {
    let bad = |m: &str| CliError::Raw(format!("asc: reorder: {m}; nothing was changed"), 1);
    let meta = std::fs::symlink_metadata(file).map_err(|_| bad("the todo file cannot be read"))?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(bad("the todo is not a small regular file"));
    }
    let mut s = String::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(MAX_FILE).read_to_string(&mut s))
        .map_err(|_| bad("the todo file cannot be read"))?;
    serde_json::from_str(&s).map_err(|e| {
        bad(&format!(
            "the todo is not a valid StackTodo ({})",
            clean(&e.to_string())
        ))
    })
}

/// A change in the new stack and the old changes it stands for.
struct Entry {
    olds: Vec<ChangesetId>,
    new: ChangesetId,
    reworded: bool,
}

fn conflicted() -> CliError {
    CliError::Raw(
        "asc: reorder: the todo conflicts; nothing was changed (try a smaller edit)".into(),
        1,
    )
}

pub async fn run(ctx: &Ctx, a: &Args, file: &Path) -> Result<ExitCode> {
    let todo = read_todo(file)?;
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_busy(ws, "reorder")?;
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, &l, &st, None).await?;
    let loc = locate(&st, x, "reorder").await?;
    drop(st);
    let mut selected = loc.lower.clone();
    let mut rest = loc.upper.clone();
    if a.stack {
        selected.append(&mut rest);
    }
    if selected.is_empty() {
        return emit_nothing(ctx, a.json, "reorder");
    }
    let mut made: Vec<Made> = Vec::with_capacity(selected.len());
    for &cs in &selected {
        made.push(made_of(&l, cs).await?);
    }
    for &cs in selected.iter().chain(rest.iter()) {
        if ws.engine.get_changeset(cs).await?.parents.len() != 1 {
            return Err(CliError::Raw("asc: reorder: a change is a merge".into(), 1));
        }
    }
    let names: Vec<String> = made.iter().map(|m| m.change.clone()).collect();
    let steps = plan(&todo, &names)
        .map_err(|e| CliError::Raw(format!("asc: reorder: {e}; nothing was changed"), 1))?;
    if steps
        .iter()
        .enumerate()
        .all(|(i, s)| s.idx == i && s.action == Action::Pick)
    {
        return emit_nothing(ctx, a.json, "reorder");
    }

    let base = ws.engine.get_changeset(selected[0]).await?.parents[0];
    let sign = ctx.config.bool("commit.gpgsign") == Some(true) && !a.dry_run;
    let op_id = uuid::Uuid::now_v7();
    pre_rebase(ctx, ws, base, "reorder").await?;

    let mut entries: Vec<Entry> = Vec::new();
    // Dropped changes with the number of entries below them: they follow the entry below.
    let mut dropped: Vec<(ChangesetId, usize)> = Vec::new();
    let mut tip = base;
    for (k, step) in steps.iter().enumerate() {
        let old = selected[step.idx];
        if step.action == Action::Drop {
            dropped.push((old, entries.len()));
            continue;
        }
        let next_folds = steps[k + 1..]
            .iter()
            .find(|s| s.action != Action::Drop)
            .is_some_and(|s| s.action == Action::Fold);
        let transient = step.action != Action::Pick || next_folds;
        let r = match replay(ctx, ws, op_id, &[(tip, vec![old])], sign && !transient).await? {
            Replay::Done(p) => p[0].1,
            Replay::Conflict(at) if a.dry_run => {
                let row = conflict_row(&l, at).await?;
                let n = selected.len();
                return emit(
                    ctx,
                    a.json,
                    "reorder",
                    "Would reorder",
                    &made,
                    n,
                    &Flow::Paused(row),
                );
            }
            Replay::Conflict(_) => return Err(conflicted()),
        };
        match step.action {
            Action::Pick => {
                entries.push(Entry {
                    olds: vec![old],
                    new: r,
                    reworded: false,
                });
                tip = r;
            }
            Action::Reword => {
                let c = ws.engine.get_changeset(r).await?;
                let new = write_piece(
                    ctx,
                    ws,
                    Piece {
                        parent: c.parents[0],
                        root: c.root,
                        author: c.author.clone(),
                        message: step.message.clone().unwrap_or_default(),
                    },
                    sign && !next_folds,
                )
                .await?;
                entries.push(Entry {
                    olds: vec![old],
                    new,
                    reworded: true,
                });
                tip = new;
            }
            Action::Fold => {
                let Some(prev) = entries.pop() else {
                    return Err(CliError::Raw("asc: reorder: nothing to fold into".into(), 1));
                };
                let cr = ws.engine.get_changeset(r).await?;
                let cp = ws.engine.get_changeset(prev.new).await?;
                let new = write_piece(
                    ctx,
                    ws,
                    Piece {
                        parent: cp.parents[0],
                        root: cr.root,
                        author: cp.author.clone(),
                        message: step.message.clone().unwrap_or_else(|| cp.message.to_vec()),
                    },
                    sign && !next_folds,
                )
                .await?;
                let mut olds = prev.olds;
                olds.push(old);
                entries.push(Entry {
                    olds,
                    new,
                    reworded: prev.reworded,
                });
                tip = new;
            }
            Action::Drop => {}
        }
    }

    let mut out_made = Vec::with_capacity(entries.len());
    for e in &entries {
        let c = ws.engine.get_changeset(e.new).await?;
        let first = selected.iter().position(|&s| s == e.olds[0]).unwrap_or(0);
        out_made.push(Made {
            cs: e.new,
            change: made[first].change.clone(),
            subject: subject_of(&c.message),
        });
    }
    let mut pairs: Vec<(ChangesetId, ChangesetId)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        pairs.extend(e.olds.iter().map(|&o| (o, e.new)));
        // A dropped change with no kept change below it has nothing to follow; its refs stay.
        pairs.extend(
            dropped
                .iter()
                .filter(|d| d.1 == i + 1)
                .map(|d| (d.0, e.new)),
        );
    }
    let n = selected.len();
    if a.dry_run {
        return emit(
            ctx,
            a.json,
            "reorder",
            "Would reorder",
            &out_made,
            n,
            &Flow::Done(Vec::new()),
        );
    }
    if !rest.is_empty() {
        match replay(ctx, ws, op_id, &[(tip, rest.clone())], sign).await? {
            Replay::Done(p) => pairs.extend(p),
            Replay::Conflict(_) => return Err(conflicted()),
        }
    }
    for e in &entries {
        if e.olds.len() > 1 {
            record(ws, e.new, &e.olds, RewriteOp::Fold, None, op_id).await?;
        } else if e.reworded {
            record(ws, e.new, &e.olds, RewriteOp::Amend, None, op_id).await?;
        }
    }
    drop(l);
    let rows = commit_pairs(ctx, ws, op_id, &pairs).await?;
    emit(
        ctx,
        a.json,
        "reorder",
        "Reordered",
        &out_made,
        n,
        &Flow::Done(rows),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec![
            "kkkkkkkkkkkkkkkk".into(),
            "llllllllllllllll".into(),
            "mmmmmmmmmmmmmmmm".into(),
        ]
    }

    fn todo(s: &str) -> StackTodo {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn a_valid_todo_plans_in_list_order() {
        let t = todo(
            r#"{"items":[{"action":"pick","change":"mmmm"},{"action":"reword","change":"kkkk","message":"new"},{"action":"fold","change":"llll"}]}"#,
        );
        let s = plan(&t, &names()).unwrap();
        assert_eq!(s.iter().map(|s| s.idx).collect::<Vec<_>>(), [2, 0, 1]);
        assert_eq!(s[1].message.as_deref(), Some(&b"new\n"[..]));
    }

    #[test]
    fn bad_todos_are_refused() {
        let bad = [
            r#"{"items":[{"action":"pick","change":"kkkk"},{"action":"pick","change":"llll"}]}"#,
            r#"{"items":[{"action":"pick","change":"kkkk"},{"action":"pick","change":"kkkk"},{"action":"pick","change":"llll"}]}"#,
            r#"{"items":[{"action":"fold","change":"kkkk"},{"action":"pick","change":"llll"},{"action":"pick","change":"mmmm"}]}"#,
            r#"{"items":[{"action":"drop","change":"kkkk"},{"action":"fold","change":"llll"},{"action":"pick","change":"mmmm"}]}"#,
            r#"{"items":[{"action":"pick","change":"kkkk"},{"action":"reword","change":"llll"},{"action":"pick","change":"mmmm"}]}"#,
            r#"{"items":[{"action":"pick","change":"kkkk","message":"x"},{"action":"pick","change":"llll"},{"action":"pick","change":"mmmm"}]}"#,
            r#"{"items":[{"action":"pick","change":"kk"},{"action":"pick","change":"llll"},{"action":"pick","change":"mmmm"}]}"#,
            r#"{"items":[{"action":"drop","change":"kkkk"},{"action":"drop","change":"llll"},{"action":"drop","change":"mmmm"}]}"#,
            r#"{"items":[{"action":"pick","change":"zzzz"},{"action":"pick","change":"llll"},{"action":"pick","change":"mmmm"}]}"#,
        ];
        for b in bad {
            assert!(plan(&todo(b), &names()).is_err(), "{b}");
        }
        assert!(
            serde_json::from_str::<StackTodo>(r#"{"items":[{"action":"squash","change":"kkkk"}]}"#)
                .is_err()
        );
    }
}
