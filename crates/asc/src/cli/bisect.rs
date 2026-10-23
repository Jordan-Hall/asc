//! `bisect` (PLAN U27b): `start`, `good`, `bad`, `skip`, `reset`, `log` and `run`, with git's
//! state: `BISECT_START`, `BISECT_LOG`, `BISECT_TERMS`, `BISECT_NAMES`, `BISECT_EXPECTED_REV`,
//! `BISECT_ANCESTORS_OK` and the `refs/bisect/*` refs. The commit to test next is chosen as git
//! chooses it (`bisect.c`): the commits reachable from `bad` and not from a `good` revision are
//! listed in date order, each is weighted by its ancestors in that set, and the one nearest to
//! half is taken; a skipped commit is replaced by git's pseudo-random pick.
//!
//! Not served: custom terms (`--term-*`, `new`/`old`), `--no-checkout`, `--first-parent`,
//! pathspecs, `terms`, `next`, `replay`, `visualize`, and ranges for `skip`.
//! `bisect run` starts the command through `asc::child::command()` and the shell.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::diff::format::write_summary;
use asc_engine::diff::{DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_stat};
use asc_store::refs::{Expect, RefTxn, RefUpdate};

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::conflict;
use super::format::date::Mode;
use super::log::refs::tips;
use super::restore::{actor, resolve_commit, root_of};
use super::seqstate;
use super::switch::{self, Dest, Move, RevKind};
use crate::ext;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "bisect", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "bisect",
    flags: &[],
    placeholders: &[],
}];

/// Most commits a bisection looks at.
const MAX_COMMITS: usize = 2_000_000;
/// git's `SLOP`: how many uninteresting commits are read after the last interesting one.
const SLOP: u32 = 5;
const PRN_MODULO: u32 = 32768;

const USAGE: &str =
    "usage: git bisect start [--term-(bad|new)=<term-new> --term-(good|old)=<term-old>]
                        [--no-checkout] [--first-parent] [<bad> [<good>...]] [--] [<pathspec>...]
   or: git bisect (bad|new|<term-new>) [<rev>]
   or: git bisect (good|old|<term-old>) [<rev>...]
   or: git bisect terms [--term-(good|old) | --term-(bad|new)]
   or: git bisect skip [(<rev>|<range>)...]
   or: git bisect next
   or: git bisect reset [<commit>]
   or: git bisect (visualize|view)
   or: git bisect replay <logfile>
   or: git bisect log
   or: git bisect run <cmd> [<arg>...]
   or: git bisect help
";

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

fn usage(head: &str) -> CliError {
    CliError::Usage(format!("fatal: {head}\n\n{USAGE}"))
}

/// What a bisect step ended with.
enum Step {
    /// Waiting for more input, or a commit was checked out.
    Continue,
    /// The first bad commit was found.
    Found,
    /// Only skipped commits are left.
    OnlySkipped,
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let gitdir = ws.projection.git_dir().to_owned();
    let words: Vec<String> = a
        .rest
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .collect();
    let Some(cmd) = words.first() else {
        return Err(usage("need a command"));
    };
    let rest = &words[1..];
    match cmd.as_str() {
        "start" => start(ctx, ws, &gitdir, rest).await,
        "bad" => mark(ctx, ws, &gitdir, State::Bad, rest).await,
        "good" => mark(ctx, ws, &gitdir, State::Good, rest).await,
        "skip" => mark(ctx, ws, &gitdir, State::Skip, rest).await,
        "reset" => reset(ctx, ws, &gitdir, rest).await,
        "log" => log(&gitdir),
        "run" => run_cmd(ctx, ws, &gitdir, rest).await,
        other => Err(usage(&format!("unknown command: '{other}'"))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Bad,
    Good,
    Skip,
}

impl State {
    fn word(self) -> &'static str {
        match self {
            Self::Bad => "bad",
            Self::Good => "good",
            Self::Skip => "skip",
        }
    }
}

// ------------------------------------------------------------------ state files

fn bisecting(gitdir: &Path) -> bool {
    seqstate::exists(gitdir, "BISECT_START")
}

fn not_started() -> CliError {
    CliError::Raw("You need to start by \"git bisect start\"\n".into(), 1)
}

fn append_log(gitdir: &Path, text: &str) -> Result<()> {
    let mut log = seqstate::read(gitdir, "BISECT_LOG")?.unwrap_or_default();
    log.extend_from_slice(text.as_bytes());
    seqstate::write(gitdir, "BISECT_LOG", &log)
}

fn subject_of(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Quotes arguments as git's `sq_quote_argv`: each in single quotes, a space before each.
fn sq_argv(args: &[String]) -> String {
    args.iter()
        .map(|a| format!(" '{}'", a.replace('\'', "'\\''")))
        .collect()
}

async fn set_ref(ctx: &Ctx, ws: &WsCtx, name: &str, commit: Option<ChangesetId>) -> Result<()> {
    let ref_name = switch::ref_name(name)?;
    let new = match commit {
        Some(c) => {
            switch::ensure_mapped(ws, c).await?;
            Some(RefTarget::Changeset(c))
        }
        None => None,
    };
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: ref_name,
            expect: Expect::Any,
            new,
            expires_at: None,
        }],
        actor: actor(),
        reason: String::new(),
        op_id: ext::OpId::new().0,
    };
    ctx.update_local_refs(txn, "bisect", StagingAfterMove::Keep)
        .await
}

/// The commits of the `refs/bisect/*` refs: `(bad, goods, skipped)`.
async fn read_refs(
    ws: &WsCtx,
) -> Result<(Option<ChangesetId>, Vec<ChangesetId>, Vec<ChangesetId>)> {
    let (mut bad, mut good, mut skip) = (None, Vec::new(), Vec::new());
    for t in tips(ws, "refs/bisect/").await? {
        let leaf = t.name.trim_start_matches("refs/bisect/");
        if leaf == "bad" {
            bad = Some(t.commit);
        } else if leaf.starts_with("good-") {
            good.push(t.commit);
        } else if leaf.starts_with("skip-") {
            skip.push(t.commit);
        }
    }
    Ok((bad, good, skip))
}

/// Removes the bisect refs and files.
async fn clean_state(ctx: &Ctx, ws: &WsCtx, gitdir: &Path) -> Result<()> {
    for t in tips(ws, "refs/bisect/").await? {
        let name = switch::ref_name(&t.name)?;
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name,
                expect: Expect::Any,
                new: None,
                expires_at: None,
            }],
            actor: actor(),
            reason: String::new(),
            op_id: ext::OpId::new().0,
        };
        ctx.update_local_refs(txn, "bisect", StagingAfterMove::Keep)
            .await?;
    }
    for f in [
        "BISECT_START",
        "BISECT_LOG",
        "BISECT_TERMS",
        "BISECT_NAMES",
        "BISECT_EXPECTED_REV",
        "BISECT_ANCESTORS_OK",
        "BISECT_RUN",
    ] {
        seqstate::remove(gitdir, f);
    }
    Ok(())
}

// ------------------------------------------------------------------ commands

async fn rev(ctx: &Ctx, word: &str) -> Result<ChangesetId> {
    resolve_commit(ctx, word)
        .await?
        .ok_or_else(|| CliError::Raw(format!("error: Bad rev input: {word}"), 1))
}

/// Records one state for a commit: the ref and the log comment.
async fn record(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &Path,
    state: State,
    commit: ChangesetId,
) -> Result<String> {
    let git = conflict::git_id(ws, commit).await?.to_string();
    let name = match state {
        State::Bad => "refs/bisect/bad".to_owned(),
        State::Good => format!("refs/bisect/good-{git}"),
        State::Skip => format!("refs/bisect/skip-{git}"),
    };
    set_ref(ctx, ws, &name, Some(commit)).await?;
    let cs = ws.engine.get_changeset(commit).await?;
    append_log(
        gitdir,
        &format!("# {}: [{git}] {}\n", state.word(), subject_of(&cs.message)),
    )?;
    Ok(git)
}

async fn start(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, args: &[String]) -> Result<ExitCode> {
    let mut revs: Vec<&String> = Vec::new();
    for a in args {
        if a == "--" || (a.starts_with('-') && a.len() > 1) {
            return Err(CliError::Msg(crate::msg::Msg::AscOptionUnsupported {
                flag: a.clone(),
            }));
        }
        revs.push(a);
    }
    let mut commits = Vec::new();
    for r in &revs {
        commits.push(rev(ctx, r).await?);
    }
    let head = ws.head_commit().await?;
    let start_head = match seqstate::read(gitdir, "BISECT_START")? {
        Some(b) if !b.is_empty() => String::from_utf8_lossy(&b).trim().to_owned(),
        _ => match ws.projection.read_head()? {
            asc_git::projection::Head::Branch(b) => b,
            asc_git::projection::Head::Detached(_) => match head {
                Some(h) => conflict::git_id(ws, h).await?.to_string(),
                None => return Err(fatal("Needed a single revision")),
            },
        },
    };
    if bisecting(gitdir) {
        checkout_original(ctx, ws, &start_head).await?;
    }
    clean_state(ctx, ws, gitdir).await?;
    seqstate::write(gitdir, "BISECT_START", format!("{start_head}\n").as_bytes())?;
    seqstate::write(gitdir, "BISECT_NAMES", b"\n")?;
    seqstate::write(gitdir, "BISECT_TERMS", b"bad\ngood\n")?;
    if commits.is_empty() {
        append_log(gitdir, "git bisect start\n")?;
    } else {
        for (i, c) in commits.iter().enumerate() {
            record(
                ctx,
                ws,
                gitdir,
                if i == 0 { State::Bad } else { State::Good },
                *c,
            )
            .await?;
        }
        append_log(
            gitdir,
            &format!(
                "git bisect start{}\n",
                sq_argv(&revs.iter().map(|r| (*r).clone()).collect::<Vec<_>>())
            ),
        )?;
    }
    next(ctx, ws, gitdir).await.map(code_of)
}

fn code_of(step: Step) -> ExitCode {
    match step {
        Step::Continue | Step::Found => ExitCode::SUCCESS,
        Step::OnlySkipped => ExitCode::from(2),
    }
}

async fn mark(
    ctx: &Ctx,
    ws: &WsCtx,
    gitdir: &Path,
    state: State,
    args: &[String],
) -> Result<ExitCode> {
    if !bisecting(gitdir) {
        return Err(not_started());
    }
    if state == State::Bad && args.len() > 1 {
        return Err(CliError::Raw(
            "error: 'git bisect bad' can take only one argument.".into(),
            1,
        ));
    }
    let words: Vec<String> = if args.is_empty() {
        vec!["HEAD".to_owned()]
    } else {
        args.to_vec()
    };
    let mut commits = Vec::new();
    for w in &words {
        if w.contains("..") || w.starts_with('-') {
            return Err(CliError::Msg(crate::msg::Msg::AscOptionUnsupported {
                flag: w.clone(),
            }));
        }
        commits.push(rev(ctx, w).await?);
    }
    for c in commits {
        let git = record(ctx, ws, gitdir, state, c).await?;
        append_log(gitdir, &format!("git bisect {} {git}\n", state.word()))?;
    }
    next(ctx, ws, gitdir).await.map(code_of)
}

fn log(gitdir: &Path) -> Result<ExitCode> {
    let Some(text) = seqstate::read(gitdir, "BISECT_LOG")? else {
        return Err(CliError::Raw("error: We are not bisecting.".into(), 1));
    };
    std::io::stdout().write_all(&text)?;
    Ok(ExitCode::SUCCESS)
}

/// Checks out the branch or commit the bisection started from (`git checkout <start> --`).
async fn checkout_original(ctx: &Ctx, ws: &WsCtx, target: &str) -> Result<()> {
    let found = switch::resolve_rev(ctx, ws, target).await?.ok_or_else(|| {
        CliError::Raw(format!("error: Could not check out original HEAD '{target}'. Try 'git bisect reset <commit>'."), 1)
    })?;
    let dest = match &found.kind {
        RevKind::Branch(name) => Dest::Existing {
            branch: name.clone(),
            tip: Some(found.commit),
        },
        _ => Dest::Detach {
            commit: found.commit,
        },
    };
    switch::perform(
        ctx,
        Move {
            dest,
            opts: switch::Opts::default(),
            typed: target.to_owned(),
            keep_tree: false,
        },
    )
    .await?;
    Ok(())
}

async fn reset(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, args: &[String]) -> Result<ExitCode> {
    if args.len() > 1 {
        return Err(CliError::Raw(
            "error: 'git bisect reset' can take only one argument.".into(),
            1,
        ));
    }
    if !bisecting(gitdir) {
        return Ok(ExitCode::SUCCESS);
    }
    let target = match args.first() {
        Some(a) => a.clone(),
        None => {
            String::from_utf8_lossy(&seqstate::read(gitdir, "BISECT_START")?.unwrap_or_default())
                .trim()
                .to_owned()
        }
    };
    if !target.is_empty() {
        checkout_original(ctx, ws, &target).await?;
    }
    clean_state(ctx, ws, gitdir).await?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------ choosing the next commit

async fn next(ctx: &Ctx, ws: &WsCtx, gitdir: &Path) -> Result<Step> {
    let (bad, goods, skipped) = read_refs(ws).await?;
    let status = match (bad.is_some(), goods.is_empty()) {
        (false, true) => Some("status: waiting for both 'good' and 'bad' commits"),
        (true, true) => Some("status: waiting for 'good' commit(s), 'bad' commit known"),
        (false, false) => Some("status: waiting for 'bad' commit, 'good' commit(s) known"),
        (true, false) => None,
    };
    if let Some(s) = status {
        println!("{s}");
        append_log(gitdir, &format!("# {s}\n"))?;
        return Ok(Step::Continue);
    }
    let Some(bad) = bad else {
        return Ok(Step::Continue);
    };
    if !seqstate::exists(gitdir, "BISECT_ANCESTORS_OK") {
        seqstate::write(gitdir, "BISECT_ANCESTORS_OK", b"")?;
    }
    let list = limited(ws, bad, &goods).await?;
    let skipped: HashSet<ChangesetId> = skipped.into_iter().collect();
    let Some(found) = choose(ws, &list, bad, &skipped).await? else {
        println!("No testable commit found.\nMaybe you started with bad path parameters?");
        return Ok(Step::OnlySkipped);
    };
    match found {
        Choice::OnlySkipped(tried) => {
            only_skipped(ws, gitdir, &tried, None).await?;
            Ok(Step::OnlySkipped)
        }
        Choice::Next {
            id,
            all,
            reaches,
            tried,
        } => {
            if id == bad {
                if !tried.is_empty() {
                    only_skipped(ws, gitdir, &tried, Some(bad)).await?;
                    return Ok(Step::OnlySkipped);
                }
                let git = conflict::git_id(ws, bad).await?;
                let cs = ws.engine.get_changeset(bad).await?;
                append_log(
                    gitdir,
                    &format!(
                        "# first 'bad' commit: [{git}] {}\n",
                        subject_of(&cs.message)
                    ),
                )?;
                println!("{git} is the first 'bad' commit");
                show_diff_tree(ws, bad, switch::abbrev_len(ctx)).await?;
                return Ok(Step::Found);
            }
            let left = all - reaches - 1;
            let steps = estimate_steps(all);
            println!(
                "Bisecting: {left} revision{} left to test after this (roughly {steps} step{})",
                if left == 1 { "" } else { "s" },
                if steps == 1 { "" } else { "s" }
            );
            let git = conflict::git_id(ws, id).await?;
            seqstate::write(gitdir, "BISECT_EXPECTED_REV", format!("{git}\n").as_bytes())?;
            switch::perform(
                ctx,
                Move {
                    dest: Dest::Detach { commit: id },
                    opts: switch::Opts {
                        quiet: true,
                        ..switch::Opts::default()
                    },
                    typed: git.to_string(),
                    keep_tree: false,
                },
            )
            .await?;
            let cs = ws.engine.get_changeset(id).await?;
            println!("[{git}] {}", subject_of(&cs.message));
            Ok(Step::Continue)
        }
    }
}

/// git's `exit_if_skipped_commits`: names the commits the first bad one could be.
async fn only_skipped(
    ws: &WsCtx,
    gitdir: &Path,
    tried: &[ChangesetId],
    bad: Option<ChangesetId>,
) -> Result<()> {
    println!(
        "There are only 'skip'ped commits left to test.
The first 'bad' commit could be any of:"
    );
    for t in tried {
        println!("{}", conflict::git_id(ws, *t).await?);
    }
    if let Some(b) = bad {
        println!("{}", conflict::git_id(ws, b).await?);
    }
    println!("We cannot bisect more!");
    append_log(
        gitdir,
        "# only skipped commits left to test
",
    )?;
    for c in bad.iter().chain(tried.iter()) {
        let git = conflict::git_id(ws, *c).await?;
        let cs = ws.engine.get_changeset(*c).await?;
        append_log(
            gitdir,
            &format!(
                "# possible first 'bad' commit: [{git}] {}
",
                subject_of(&cs.message)
            ),
        )?;
    }
    Ok(())
}

fn estimate_steps(all: usize) -> usize {
    if all < 3 {
        return 0;
    }
    let n = usize::BITS as usize - 1 - all.leading_zeros() as usize;
    let e = 1usize << n;
    let x = all - e;
    if e < 3 * x { n } else { n - 1 }
}

/// What the commit list yields.
enum Choice {
    Next {
        id: ChangesetId,
        all: usize,
        reaches: usize,
        tried: Vec<ChangesetId>,
    },
    OnlySkipped(Vec<ChangesetId>),
}

/// The commits reachable from `bad` and from no `good`, newest first, in the order git's
/// `limit_list` produces (committer date, earlier insertions first on ties).
async fn limited(ws: &WsCtx, bad: ChangesetId, goods: &[ChangesetId]) -> Result<Vec<ChangesetId>> {
    #[derive(Default, Clone, Copy)]
    struct Flags {
        seen: bool,
        unint: bool,
    }
    let mut info: HashMap<ChangesetId, (i64, Vec<ChangesetId>)> = HashMap::new();
    let mut flags: HashMap<ChangesetId, Flags> = HashMap::new();

    async fn load(
        ws: &WsCtx,
        info: &mut HashMap<ChangesetId, (i64, Vec<ChangesetId>)>,
        id: ChangesetId,
    ) -> Result<()> {
        if info.contains_key(&id) {
            return Ok(());
        }
        if info.len() >= MAX_COMMITS {
            return Err(fatal("asc: the bisection reaches too many commits"));
        }
        let cs = ws.engine.get_changeset(id).await?;
        info.insert(id, (cs.committer.time(), cs.parents.to_vec()));
        Ok(())
    }

    fn insert(q: &mut Vec<(i64, ChangesetId)>, item: (i64, ChangesetId)) {
        let at = q.partition_point(|(d, _)| *d >= item.0);
        q.insert(at, item);
    }

    /// git's `mark_parents_uninteresting`: through the commits already read.
    fn mark_parents(
        info: &HashMap<ChangesetId, (i64, Vec<ChangesetId>)>,
        flags: &mut HashMap<ChangesetId, Flags>,
        c: ChangesetId,
    ) {
        let mut stack: Vec<ChangesetId> = info.get(&c).map(|i| i.1.clone()).unwrap_or_default();
        while let Some(mut cur) = stack.pop() {
            loop {
                let f = flags.entry(cur).or_default();
                if f.unint {
                    break;
                }
                f.unint = true;
                let Some(parents) = info.get(&cur).map(|i| i.1.clone()) else {
                    break;
                };
                if parents.is_empty() {
                    break;
                }
                stack.extend(parents[1..].iter().copied());
                cur = parents[0];
            }
        }
    }

    // Start commits, sorted by date (stable).
    let mut start: Vec<(i64, ChangesetId)> = Vec::new();
    let mut tips: Vec<(ChangesetId, bool)> = vec![(bad, false)];
    tips.extend(goods.iter().map(|g| (*g, true)));
    for (id, unint) in &tips {
        load(ws, &mut info, *id).await?;
        if *unint {
            flags.entry(*id).or_default().unint = true;
            mark_parents(&info, &mut flags, *id);
        }
        let f = flags.entry(*id).or_default();
        if !f.seen {
            f.seen = true;
            start.push((info[id].0, *id));
        }
    }
    start.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut queue = start;

    let mut newlist: Vec<ChangesetId> = Vec::new();
    let mut slop = SLOP;
    let mut date = i64::MAX;
    while !queue.is_empty() {
        let (_, c) = queue.remove(0);
        let parents = info[&c].1.clone();
        let c_unint = flags.get(&c).is_some_and(|f| f.unint);
        for p in parents {
            load(ws, &mut info, p).await?;
            if c_unint {
                flags.entry(p).or_default().unint = true;
                if info.get(&p).is_some_and(|i| !i.1.is_empty()) {
                    mark_parents(&info, &mut flags, p);
                }
            }
            let f = flags.entry(p).or_default();
            if f.seen {
                continue;
            }
            f.seen = true;
            insert(&mut queue, (info[&p].0, p));
        }
        if flags.get(&c).is_some_and(|f| f.unint) {
            mark_parents(&info, &mut flags, c);
            // git's `still_interesting`.
            slop = if queue.is_empty() {
                0
            } else if date <= queue[0].0
                || queue
                    .iter()
                    .any(|(_, q)| !flags.get(q).is_some_and(|f| f.unint))
            {
                SLOP
            } else {
                slop - 1
            };
            if slop > 0 {
                continue;
            }
            break;
        }
        date = info[&c].0;
        newlist.push(c);
        if newlist.len() > MAX_COMMITS {
            return Err(fatal("asc: the bisection reaches too many commits"));
        }
    }
    newlist.retain(|c| !flags.get(c).is_some_and(|f| f.unint));
    Ok(newlist)
}

fn get_prn(count: u32) -> u32 {
    let c = count.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    (c / 65536) % PRN_MODULO
}

/// git's `find_bisection` and the skip handling around it.
async fn choose(
    ws: &WsCtx,
    newest_first: &[ChangesetId],
    bad: ChangesetId,
    skipped: &HashSet<ChangesetId>,
) -> Result<Option<Choice>> {
    if newest_first.is_empty() {
        return Ok(None);
    }
    let list: Vec<ChangesetId> = newest_first.iter().rev().copied().collect();
    let index: HashMap<ChangesetId, usize> =
        list.iter().enumerate().map(|(i, c)| (*c, i)).collect();
    let mut parents: Vec<Vec<usize>> = Vec::with_capacity(list.len());
    for c in &list {
        let cs = ws.engine.get_changeset(*c).await?;
        parents.push(
            cs.parents
                .iter()
                .filter_map(|p| index.get(p).copied())
                .collect(),
        );
    }
    let nr = list.len();
    let find_all = !skipped.is_empty();
    let mut weight: Vec<i64> = vec![0; nr];
    let mut counted = 0usize;
    for i in 0..nr {
        match parents[i].len() {
            0 => {
                weight[i] = 1;
                counted += 1;
            }
            1 => weight[i] = -1,
            _ => weight[i] = -2,
        }
    }
    let halfway = |w: i64| matches!(2 * w - nr as i64, -1..=1);
    let mut early: Option<usize> = None;
    // Merges first, by counting their ancestors.
    #[allow(clippy::needless_range_loop)]
    for i in 0..nr {
        if weight[i] != -2 {
            continue;
        }
        let mut seen = vec![false; nr];
        let mut stack = vec![i];
        let mut n = 0i64;
        while let Some(x) = stack.pop() {
            if std::mem::replace(&mut seen[x], true) {
                continue;
            }
            n += 1;
            stack.extend(parents[x].iter().copied());
        }
        weight[i] = n;
        if !find_all && halfway(n) {
            early = Some(i);
            break;
        }
        counted += 1;
    }
    // Then the strands between them.
    if early.is_none() {
        'outer: while counted < nr {
            let before = counted;
            for i in 0..nr {
                if weight[i] >= 0 {
                    continue;
                }
                let Some(&q) = parents[i].iter().find(|&&q| weight[q] >= 0) else {
                    continue;
                };
                weight[i] = weight[q] + 1;
                counted += 1;
                if !find_all && halfway(weight[i]) {
                    early = Some(i);
                    break 'outer;
                }
            }
            if counted == before {
                break;
            }
        }
    }
    let dist = |w: i64| w.min(nr as i64 - w);
    let (ordered, reaches): (Vec<usize>, usize) = if let Some(i) = early {
        (vec![i], weight[i] as usize)
    } else if !find_all {
        let (mut best, mut best_d) = (0usize, -1i64);
        #[allow(clippy::needless_range_loop)]
        for i in 0..nr {
            let d = dist(weight[i]);
            if d > best_d {
                best = i;
                best_d = d;
            }
        }
        (vec![best], weight[best] as usize)
    } else {
        // git sorts by distance only; equal distances keep the list order (the order git's
        // `qsort` leaves them in depends on the C library).
        let mut all: Vec<usize> = (0..nr).collect();
        all.sort_by(|&a, &b| dist(weight[b]).cmp(&dist(weight[a])));
        let first = all[0];
        (all, weight[first] as usize)
    };
    if !find_all {
        return Ok(Some(Choice::Next {
            id: list[ordered[0]],
            all: nr,
            reaches,
            tried: Vec::new(),
        }));
    }
    // git's `managed_skipped`.
    let mut tried: Vec<ChangesetId> = Vec::new();
    let mut filtered: Vec<ChangesetId> = Vec::new();
    let mut skipped_first = false;
    let mut count = 0usize;
    for &i in &ordered {
        let c = list[i];
        if skipped.contains(&c) {
            skipped_first = true;
            tried.push(c);
        } else {
            if !skipped_first {
                filtered.push(c);
                break;
            }
            filtered.push(c);
            count += 1;
        }
    }
    let pick = if !skipped_first {
        filtered.first().copied()
    } else if filtered.is_empty() {
        None
    } else {
        let prn = get_prn(count as u32);
        let at = (count as u64 * u64::from(prn) / u64::from(PRN_MODULO)) as usize;
        let chosen = filtered.get(at).copied().unwrap_or(filtered[0]);
        if chosen != bad {
            Some(chosen)
        } else if at > 0 {
            Some(filtered[at - 1])
        } else {
            Some(filtered[0])
        }
    };
    Ok(Some(match pick {
        Some(id) => Choice::Next {
            id,
            all: nr,
            reaches,
            tried,
        },
        None => Choice::OnlySkipped(tried),
    }))
}

/// `git diff-tree --pretty --stat --summary` of the first bad commit.
async fn show_diff_tree(ws: &WsCtx, id: ChangesetId, abbrev: usize) -> Result<()> {
    let cs = ws.engine.get_changeset(id).await?;
    let git = conflict::git_id(ws, id).await?;
    let mut out: Vec<u8> = Vec::new();
    writeln!(out, "commit {git}")?;
    if cs.parents.len() > 1 {
        let mut shorts = Vec::new();
        for p in cs.parents.iter() {
            shorts.push(conflict::git_id(ws, *p).await?.to_string()[..abbrev].to_owned());
        }
        writeln!(out, "Merge: {}", shorts.join(" "))?;
    }
    writeln!(
        out,
        "Author: {} <{}>",
        String::from_utf8_lossy(cs.author.name()),
        String::from_utf8_lossy(cs.author.email())
    )?;
    let date = Mode::default().render(cs.author.time(), cs.author.tz_min(), cs.author.time());
    writeln!(out, "Date:   {date}\n")?;
    let text = String::from_utf8_lossy(&cs.message).into_owned();
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    for l in lines {
        if l.is_empty() {
            writeln!(out, "    ")?;
        } else {
            writeln!(out, "    {l}")?;
        }
    }
    if let Some(&parent) = cs.parents.first() {
        let old = root_of(ws, Some(parent)).await?;
        let opts = DiffOptions {
            renames: Some(RenameOptions::default()),
            ..DiffOptions::default()
        };
        let changes = ws
            .engine
            .diff_trees(old, cs.root, None, &opts)
            .await
            .map_err(|e| fatal(e.to_string()))?;
        if !changes.is_empty() {
            out.push(b'\n');
            let dctx = DiffCtx {
                opts: &opts,
                content: &ws.engine,
                attrs: &NoAttributes,
            };
            write_stat(&mut out, &dctx, &changes)
                .await
                .map_err(|e| fatal(e.to_string()))?;
            write_summary(&mut out, &changes, &opts).map_err(|e| fatal(e.to_string()))?;
        }
    }
    std::io::stdout().write_all(&out)?;
    Ok(())
}

// ------------------------------------------------------------------ run

async fn run_cmd(ctx: &Ctx, ws: &WsCtx, gitdir: &Path, args: &[String]) -> Result<ExitCode> {
    if args.is_empty() {
        return Err(CliError::Raw(
            "error: 'git bisect run' failed: no command provided.".into(),
            1,
        ));
    }
    if !bisecting(gitdir) {
        return Ok(ExitCode::from(1));
    }
    let quoted = sq_argv(args);
    let quoted = quoted.trim_start().to_owned();
    loop {
        println!("running {quoted}");
        std::io::stdout().flush()?;
        let code = run_shell(ctx, ws, &quoted).await?;
        if !(0..128).contains(&code) {
            return Err(CliError::Raw(
                format!(
                    "error: bisect run failed: exit code {code} from {quoted} is < 0 or >= 128"
                ),
                (-code & 0xff) as u8,
            ));
        }
        let state = match code {
            125 => State::Skip,
            0 => State::Good,
            _ => State::Bad,
        };
        let commit = ws
            .head_commit()
            .await?
            .ok_or_else(|| fatal("asc: HEAD is unborn"))?;
        let git = record(ctx, ws, gitdir, state, commit).await?;
        append_log(gitdir, &format!("git bisect {} {git}\n", state.word()))?;
        match next(ctx, ws, gitdir).await? {
            Step::Continue => {}
            Step::Found => {
                println!("bisect found first 'bad' commit");
                return Ok(ExitCode::SUCCESS);
            }
            Step::OnlySkipped => {
                eprintln!("error: bisect run cannot continue any more");
                return Ok(ExitCode::from(2));
            }
        }
    }
}

/// Runs `command` (already quoted) through the shell in the work tree, as git does.
async fn run_shell(ctx: &Ctx, ws: &WsCtx, command: &str) -> Result<i32> {
    let shell = ctx.shell();
    let workdir = ws.projection.toplevel().to_owned();
    let script = format!("{command} \"$@\"");
    let status = tokio::task::spawn_blocking(move || -> Result<i32> {
        let mut cmd = crate::child::command(&shell).map_err(|m| fatal(format!("{m:?}")))?;
        cmd.arg("-c").arg(&script).current_dir(&workdir);
        let status = cmd
            .status()
            .map_err(|e| fatal(format!("unable to run '{script}': {e}")))?;
        Ok(status.code().unwrap_or(-1))
    })
    .await
    .map_err(|_| fatal("bisect run did not finish"))??;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_estimates_follow_git() {
        assert_eq!(estimate_steps(1), 0);
        assert_eq!(estimate_steps(2), 0);
        assert_eq!(estimate_steps(7), 2);
        assert_eq!(estimate_steps(8), 2);
        assert_eq!(estimate_steps(12), 3);
        assert_eq!(estimate_steps(100), 6);
    }

    #[test]
    fn the_pseudo_random_pick_matches_git() {
        // 6 skipped-filtered candidates pick index 0, as git does for six.
        let prn = get_prn(6);
        assert_eq!(prn, 2726);
        assert_eq!(6 * prn / PRN_MODULO, 0);
    }

    #[test]
    fn arguments_are_quoted_like_sq_quote_argv() {
        assert_eq!(
            sq_argv(&["HEAD".into(), "it's".into()]),
            " 'HEAD' 'it'\\''s'"
        );
    }
}
