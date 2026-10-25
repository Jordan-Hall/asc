//! `asc split [--change <rev>] --by-file <pathspec>… [-m <msg>]…` (SPEC §20.2, §20.7): splits one
//! change into a stack, one piece per pathspec, bottom first. What no pathspec selects stays in
//! the top piece, which keeps the original message and the change id; the other pieces get new
//! change ids. The changes above restack onto the top piece with no merge (it has the original
//! tree). `-m` gives the messages of the lower pieces, bottom first.
//!
//! `--by-hunk` and `--by-commit` are refused: asc never opens a prompt, and a change is one
//! commit. The non-interactive hunk split is `--hunks` (unit P46).

use std::collections::HashSet;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;

use super::info::resolve_change;
use super::rewrite::{Made, Piece, emit, finish, locate, record, refuse_in_progress, write_piece};
use super::sl::{Local, clean};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::diff::pathspec;
use crate::cli::restore::root_of;
use crate::native::NativeSpec;

/// Most pieces one split makes.
const MAX_PIECES: usize = 64;
/// Longest message accepted from `-m`.
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "split", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The change to split; default the current HEAD.
    #[arg(long = "change", value_name = "REV")]
    pub change: Option<String>,
    /// One piece per pathspec, bottom first.
    #[arg(long = "by-file", num_args = 1.., value_name = "PATHSPEC")]
    pub by_file: Vec<OsString>,
    #[arg(long = "by-hunk")]
    pub by_hunk: bool,
    #[arg(long = "by-commit")]
    pub by_commit: bool,
    /// The message of a lower piece; repeat, bottom first.
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,
    /// Split from a `SplitSpec` (JSON file) with no prompt (unit P46).
    #[arg(long = "hunks", value_name = "SPEC")]
    pub hunks: Option<std::path::PathBuf>,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "split",
    flags: &["--change", "--by-file", "--by-hunk", "--by-commit", "--hunks", "-m", "--message", "--json"],
    placeholders: &[],
}];

super::verb!(SPLIT, "split");

fn with_newline(mut s: String) -> Vec<u8> {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.into_bytes()
}

pub async fn run(ctx: &Ctx, mut a: Args) -> Result<ExitCode> {
    if let Some(file) = a.hunks.take() {
        return super::split_hunks::run(ctx, a, &file).await;
    }
    if a.by_hunk {
        return Err(CliError::Raw(
            "asc: split: --by-hunk needs a prompt, which asc never opens; use --by-file".into(),
            1,
        ));
    }
    if a.by_commit {
        return Err(CliError::Raw("asc: split: --by-commit has no meaning for a one-commit change; use --by-file".into(), 1));
    }
    if a.by_file.is_empty() {
        return Err(CliError::Usage("usage: asc split [--change <rev>] --by-file <pathspec>... [-m <msg>]...".into()));
    }
    if a.by_file.len() >= MAX_PIECES || a.message.len() > MAX_PIECES || a.message.iter().any(|m| m.len() > MAX_MESSAGE) {
        return Err(CliError::Raw("asc: split: too many pieces or a message is too long".into(), 1));
    }
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_in_progress(ws, "split")?;
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let loc = locate(&st, x, "split").await?;
    drop(st);
    let cx = ws.engine.get_changeset(x).await?;
    if cx.parents.len() != 1 {
        return Err(CliError::Raw("asc: split: the change is a merge".into(), 1));
    }
    let base = cx.parents[0];
    let base_root = root_of(ws, Some(base)).await?;

    // The tree of each piece, bottom first.
    let mut trees = Vec::new();
    let mut cur = base_root;
    let mut taken: HashSet<Vec<u8>> = HashSet::new();
    for arg in &a.by_file {
        let spec = pathspec(ctx, std::slice::from_ref(arg))?
            .ok_or_else(|| CliError::Raw("asc: split: an empty pathspec".into(), 1))?;
        let changes: Vec<_> = ws
            .engine
            .leafdiff(base_root, cx.root, Some(&spec))
            .await?
            .into_iter()
            .filter(|(p, _)| !taken.contains(p.as_bytes()))
            .collect();
        if changes.is_empty() {
            return Err(CliError::Raw(
                format!("asc: split: '{}' matches no file this change touches", clean(&arg.to_string_lossy())),
                1,
            ));
        }
        taken.extend(changes.iter().map(|(p, _)| p.as_bytes().to_vec()));
        cur = ws.engine.apply(cur, &changes).await?;
        trees.push(cur);
    }
    if cur != cx.root {
        trees.push(cx.root);
    }
    if trees.len() < 2 {
        return Err(CliError::Raw("asc: split: the pathspecs cover the whole change; nothing to split".into(), 1));
    }
    let lower = trees.len() - 1;
    if a.message.len() > lower {
        return Err(CliError::Usage(format!("asc split: {} -m given for {lower} lower pieces", a.message.len())));
    }

    let op_id = uuid::Uuid::now_v7();
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let subject = String::from_utf8_lossy(cx.message.split(|&b| b == b'\n').next().unwrap_or_default()).into_owned();
    let mut prev = base;
    let mut made: Vec<Made> = Vec::new();
    for (i, root) in trees.iter().copied().enumerate() {
        let top = i == lower;
        let message = if top {
            cx.message.clone()
        } else {
            match a.message.get(i) {
                Some(m) => with_newline(m.clone()),
                None => with_newline(format!("{subject} (split {}/{})", i + 1, lower + 1)),
            }
        };
        let new = write_piece(ctx, ws, Piece { parent: prev, root, author: cx.author.clone(), message }, sign).await?;
        made.push(record(ws, new, &[x], RewriteOp::Split, Some(top), op_id).await?);
        prev = new;
    }
    let pairs: Vec<(ChangesetId, ChangesetId)> = vec![(x, prev)];
    let flow = finish(ctx, ws, op_id, pairs, loc.upper.clone(), prev).await?;
    emit(ctx, a.json, "split", "Split", &made, 1, &flow)
}
