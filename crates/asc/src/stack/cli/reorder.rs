//! `asc reorder [--stack] [-n]` (SPEC §20.7): opens an editor on the list of changes from the
//! bottom of the stack to the checked-out change (`--stack`: to the top), one `<change> <subject>`
//! per line, and rebuilds the stack in the order the saved list gives.
//!
//! The editor is the sequence editor of `git rebase -i` (`GIT_SEQUENCE_EDITOR`,
//! `sequence.editor`, then the commit editor), started through `asc::child`. The list must keep
//! every change exactly once: a missing, repeated or unknown line stops the verb before anything
//! moves (use `asc abandon` to drop a change). Change ids survive; the rewrites are applied in
//! one transaction, so a conflict moves nothing (`-n` predicts it). `pre-rebase` runs once up
//! front and `post-rewrite rebase` once afterwards.

use std::io::Read as _;
use std::path::Path;
use std::process::ExitCode;

use asc_core::id::ChangesetId;

use super::info::resolve_change;
use super::move_::{Replay, commit_pairs, conflict_row, made_of, pre_rebase, replay};
use super::restack::Flow;
use super::reword::refuse_busy;
use super::rewrite::{Made, emit, emit_nothing, locate};
use super::sl::{Local, clean};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::rebase_todo;
use crate::native::NativeSpec;

/// Longest todo file read back.
const MAX_TODO: u64 = 4 * 1024 * 1024;
/// Shortest change-id prefix a line may use.
const MIN_PREFIX: usize = 4;

#[derive(clap::Parser, Debug)]
#[command(
    name = "reorder",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// List the whole stack, not only the part up to the checked-out change.
    #[arg(long)]
    pub stack: bool,
    /// Replay in memory and report; move nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
    /// Apply a `StackTodo` (JSON; pick, fold, drop, reword) as one operation; no editor.
    #[arg(long, value_name = "FILE")]
    pub todo: Option<std::path::PathBuf>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "reorder",
    flags: &["--stack", "-n", "--dry-run", "--json", "--todo"],
    placeholders: &[],
}];

super::verb!(REORDER, "reorder");

const HELP: &str = "\n# Reorder the lines above; the first line is the bottom of the stack.\n# Every change must stay exactly once. Lines starting with '#' are ignored.\n";

/// The order a saved list names: indexes into `names`.
fn parse(text: &str, names: &[String]) -> std::result::Result<Vec<usize>, String> {
    let mut order = Vec::with_capacity(names.len());
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let word = line.split_whitespace().next().unwrap_or_default();
        if word.len() < MIN_PREFIX {
            return Err(format!("'{}' is too short to name a change", clean(word)));
        }
        let hits: Vec<usize> = (0..names.len())
            .filter(|&i| names[i].starts_with(word))
            .collect();
        match hits.as_slice() {
            [i] if !order.contains(i) => order.push(*i),
            [_] => return Err(format!("change {} is listed twice", clean(word))),
            [] => return Err(format!("'{}' names no change of the list", clean(word))),
            _ => return Err(format!("'{}' names several changes", clean(word))),
        }
    }
    if order.len() != names.len() {
        return Err("a change is missing from the list (use 'asc abandon' to drop one)".into());
    }
    Ok(order)
}

async fn edit(ctx: &Ctx, file: &Path, text: &str) -> Result<String> {
    let _ = std::fs::remove_file(file);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    drop(f);
    let ok = rebase_todo::edit_todo(ctx, file).await;
    let read = || -> Result<String> {
        let meta = std::fs::symlink_metadata(file)?;
        if !meta.is_file() || meta.len() > MAX_TODO {
            return Err(CliError::Raw(
                "asc: reorder: the saved list is not a small regular file".into(),
                1,
            ));
        }
        let mut s = String::new();
        std::fs::File::open(file)?
            .take(MAX_TODO)
            .read_to_string(&mut s)?;
        Ok(s)
    };
    let out = if matches!(ok, Ok(true)) {
        Some(read())
    } else {
        None
    };
    let _ = std::fs::remove_file(file);
    match (ok?, out) {
        (true, Some(r)) => r,
        _ => Err(CliError::Fatal(
            "asc: reorder: the editor failed; nothing was changed".into(),
        )),
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if let Some(file) = &a.todo {
        return super::reorder_todo::run(ctx, &a, file).await;
    }
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
    if selected.len() < 2 {
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
    let mut text = String::new();
    for m in &made {
        let short: String = m.change.chars().take(12).collect();
        text.push_str(&format!("{short} {}\n", clean(&m.subject)));
    }
    text.push_str(HELP);

    let file = ws.projection.git_dir().join("asc-reorder-todo");
    let saved = edit(ctx, &file, &text).await?;
    let order = parse(&saved, &names)
        .map_err(|e| CliError::Raw(format!("asc: reorder: {e}; nothing was changed"), 1))?;
    if order.iter().enumerate().all(|(i, &j)| i == j) {
        return emit_nothing(ctx, a.json, "reorder");
    }

    let base = ws.engine.get_changeset(selected[0]).await?.parents[0];
    let new_order: Vec<ChangesetId> = order.iter().map(|&i| selected[i]).collect();
    let new_made: Vec<Made> = order
        .iter()
        .map(|&i| Made {
            cs: made[i].cs,
            change: made[i].change.clone(),
            subject: made[i].subject.clone(),
        })
        .collect();
    let mut steps = new_order;
    steps.extend(rest.iter().copied());
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let op_id = uuid::Uuid::now_v7();
    pre_rebase(ctx, ws, base, "reorder").await?;
    let n = new_made.len();
    match replay(ctx, ws, op_id, &[(base, steps)], sign && !a.dry_run).await? {
        Replay::Conflict(at) if a.dry_run => {
            let row = conflict_row(&l, at).await?;
            emit(ctx, a.json, "reorder", "Would reorder", &new_made, n, &Flow::Paused(row))
        }
        Replay::Conflict(_) => Err(CliError::Raw(
            "asc: reorder: the new order conflicts; nothing was changed (try a smaller change of order)".into(),
            1,
        )),
        Replay::Done(_) if a.dry_run => {
            emit(ctx, a.json, "reorder", "Would reorder", &new_made, n, &Flow::Done(Vec::new()))
        }
        Replay::Done(pairs) => {
            drop(l);
            let rows = commit_pairs(ctx, ws, op_id, &pairs).await?;
            emit(ctx, a.json, "reorder", "Reordered", &new_made, n, &Flow::Done(rows))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn names() -> Vec<String> {
        vec![
            "kkkkkkkkkkkkkkkk".into(),
            "llllllllllllllll".into(),
            "mmmmmmmmmmmmmmmm".into(),
        ]
    }

    #[test]
    fn a_permutation_parses_and_comments_are_skipped() {
        let t = "# c\nllll two\n\nmmmm three\nkkkk one\n";
        assert_eq!(parse(t, &names()).unwrap(), [1, 2, 0]);
    }

    #[test]
    fn a_missing_repeated_or_unknown_line_is_refused() {
        assert!(parse("kkkk\nllll\n", &names()).is_err());
        assert!(parse("kkkk\nkkkk\nllll\n", &names()).is_err());
        assert!(parse("kkkk\nllll\nzzzz\n", &names()).is_err());
        assert!(parse("kk\nllll\nmmmm\n", &names()).is_err());
    }
}
