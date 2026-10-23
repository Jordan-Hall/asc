//! `asc up|down|top|bottom` and their aliases `next|prev` (SPEC §20.7): move HEAD along the
//! draft changes of a stack. The move is a detached switch through the checkout path of
//! `cli::switch`, so local changes are kept or refused exactly as `git switch --detach`
//! does. At a fork (several live children) nothing moves and the verb exits 2 with the
//! candidates; asc never prompts (the fixed "never interactive" decision), so the terminal
//! and `--json` cases are the same.

use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;

use asc_core::change::ChangePrefix;
use asc_core::id::ChangesetId;
use asc_engine::change::ChangeRows;
use clap::Parser;

use super::sl::{LStacks, Local, change_out, clean, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::switch::{Dest, Move, Opts, perform};
use crate::stack::json::{Candidate, OpResult, Status, line};

/// Most steps one call walks; `up 4294967295` is clamped, not an unbounded loop.
const MAX_STEPS: usize = 100_000;
/// Most draft changes read to find children.
const MAX_DRAFTS: usize = 100_000;

#[derive(Parser, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// How many changes to move (`up` and `down`; default 1).
    pub n: Option<usize>,
    /// At a fork, take the child with this change prefix or branch name.
    #[arg(long)]
    pub to: Option<String>,
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Top,
    Bottom,
}

const FLAGS: &[&str] = &["--to", "-n", "--dry-run", "--json"];

/// What a navigation decided.
enum Plan {
    /// Nothing to move to.
    Nothing(&'static str),
    /// Stopped at a fork.
    Fork(Vec<Candidate>),
    Go { target: ChangesetId, change: String, subject: String },
}

macro_rules! nav_verb {
    ($m:ident, $slot:ident, $word:literal, $dir:expr) => {
        mod $m {
            use std::process::ExitCode;

            use super::{Args, Dir, FLAGS, nav};
            use crate::cli::common::{Ctx, Result};
            use crate::native::NativeSpec;

            pub const NATIVE: &[NativeSpec] = &[NativeSpec { cmd: $word, flags: FLAGS, placeholders: &[] }];

            pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
                let dir: Dir = $dir;
                nav(ctx, a, dir).await
            }

            crate::stack::cli::verb!($slot, $word);
        }
    };
}

nav_verb!(up, UP, "up", Dir::Up);
nav_verb!(next, NEXT, "next", Dir::Up);
nav_verb!(down, DOWN, "down", Dir::Down);
nav_verb!(prev, PREV, "prev", Dir::Down);
nav_verb!(top, TOP, "top", Dir::Top);
nav_verb!(bottom, BOTTOM, "bottom", Dir::Bottom);

/// Live (not obsolete) draft children by parent, one pass over the draft set.
async fn child_map(l: &Local<'_>, st: &LStacks<'_, '_>) -> Result<HashMap<ChangesetId, Vec<ChangesetId>>> {
    let drafts = st.draft_set().await.map_err(stack_err)?;
    if drafts.len() > MAX_DRAFTS {
        return Err(fatal(format!("more than {MAX_DRAFTS} draft changes")));
    }
    let mut map: HashMap<ChangesetId, Vec<ChangesetId>> = HashMap::new();
    for d in drafts {
        if st.is_obsolete(d).await.map_err(stack_err)? {
            continue;
        }
        if let Some(p) = l.graph.node(d).await.map_err(fatal)?.parents.first() {
            map.entry(*p).or_default().push(d);
        }
    }
    for v in map.values_mut() {
        v.sort();
    }
    Ok(map)
}

async fn candidates(l: &Local<'_>, st: &LStacks<'_, '_>, kids: &[ChangesetId]) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for k in kids {
        if let Some(row) = st.draft_row(*k).await.map_err(stack_err)? {
            let c = change_out(l, st, *k, &row).await?;
            out.push(Candidate { change: c.change, subject: clean(&c.subject) });
        }
    }
    Ok(out)
}

/// The one child `--to` names: a change prefix or a branch at the child. Two matches name
/// nothing.
async fn pick(l: &Local<'_>, kids: &[ChangesetId], to: &str) -> Result<Option<ChangesetId>> {
    let prefix = ChangePrefix::parse(to).ok();
    let mut hit = None;
    for k in kids {
        let by_branch = l.env.branches_at(*k).iter().any(|b| b == to);
        let by_change = match (&prefix, l.rows.get(*k).await.map_err(fatal)?) {
            (Some(p), Some(row)) => p.matches(&row.change),
            _ => false,
        };
        if by_branch || by_change {
            if hit.is_some() {
                return Ok(None);
            }
            hit = Some(*k);
        }
    }
    Ok(hit)
}

async fn plan(ctx: &Ctx, a: &Args, dir: Dir) -> Result<Plan> {
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let Some(start) = st.current_head() else { return Ok(Plan::Nothing("HEAD is not on a commit")) };
    let steps = match dir {
        Dir::Up | Dir::Down => a.n.unwrap_or(1).min(MAX_STEPS),
        Dir::Top | Dir::Bottom => {
            if a.n.is_some() {
                return Err(CliError::Usage("usage: asc top|bottom [--dry-run] [--json]".into()));
            }
            MAX_STEPS
        }
    };
    let mut at = start;
    match dir {
        Dir::Down | Dir::Bottom => {
            for _ in 0..steps {
                let parent = l.graph.node(at).await.map_err(fatal)?.parents.first().copied();
                match parent {
                    Some(p) if st.draft_row(p).await.map_err(stack_err)?.is_some() => at = p,
                    _ => break,
                }
            }
        }
        Dir::Up | Dir::Top => {
            let kids_of = child_map(&l, &st).await?;
            for _ in 0..steps {
                let kids = kids_of.get(&at).map(Vec::as_slice).unwrap_or_default();
                match kids {
                    [] => break,
                    [one] => at = *one,
                    many => {
                        let chosen = match &a.to {
                            Some(to) => pick(&l, many, to).await?,
                            None => None,
                        };
                        match chosen {
                            Some(c) => at = c,
                            None => return Ok(Plan::Fork(candidates(&l, &st, many).await?)),
                        }
                    }
                }
            }
        }
    }
    if at == start {
        return Ok(Plan::Nothing(match dir {
            Dir::Up | Dir::Top => "already at the top of the stack",
            Dir::Down | Dir::Bottom => "already at the bottom of the stack",
        }));
    }
    let row = st
        .draft_row(at)
        .await
        .map_err(stack_err)?
        .ok_or_else(|| CliError::Raw("asc: not a draft change".into(), 1))?;
    let c = change_out(&l, &st, at, &row).await?;
    Ok(Plan::Go { target: at, change: c.change, subject: clean(&c.subject) })
}

async fn nav(ctx: &Ctx, a: Args, dir: Dir) -> Result<ExitCode> {
    let plan = plan(ctx, &a, dir).await?;
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    let (result, note, body) = match plan {
        Plan::Nothing(why) => (OpResult::new(Status::Nothing), format!("asc: {why}\n"), String::new()),
        Plan::Fork(cands) => {
            let body = cands.iter().map(|c| format!("{} {}\n", c.change, c.subject)).collect();
            let note = "asc: ambiguous: this change has several children; name one with --to\n".to_owned();
            (OpResult::ambiguous(cands), note, body)
        }
        Plan::Go { target, change, subject } => {
            if !a.dry_run {
                let opts = Opts { quiet: true, detach: true, ..Opts::default() };
                let mv = Move { dest: Dest::Detach { commit: target }, opts, typed: change.clone(), keep_tree: false };
                perform(ctx, mv).await?;
            }
            (OpResult::new(Status::Ok), String::new(), format!("{change} {subject}\n"))
        }
    };
    if a.json {
        out.write_all(line(&result).map_err(fatal)?.as_bytes())?;
    } else {
        // The explanation goes to stderr, the change list to stdout, so a script reads the list.
        eprint!("{note}");
        out.write_all(body.as_bytes())?;
    }
    out.finish()?;
    Ok(result.status.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_count_and_target() {
        let a = Args::try_parse_from(["up", "3", "--to", "kk"]).unwrap();
        assert_eq!((a.n, a.to.as_deref()), (Some(3), Some("kk")));
        assert!(Args::try_parse_from(["up", "-1"]).is_err());
    }

    #[test]
    fn all_six_words_are_registered() {
        for w in ["up", "down", "top", "bottom", "next", "prev"] {
            assert!(super::super::VERBS.iter().any(|v| v.name == w), "{w}");
        }
    }
}
