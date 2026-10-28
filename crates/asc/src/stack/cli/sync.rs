//! `asc sync [--no-fetch] [--no-restack] [-d] [--all-trunks]` (SPEC §20.7, "Stage 0 fallback").
//!
//! In order: fetch (`fetch --prune` of one remote); fast-forward the local trunks no other
//! workspace has checked out; drop the changes that landed; restack every stack onto its
//! trunk; list (or with `-d` delete) the merged branches.
//!
//! A change has landed when
//! * its commit is reachable from the trunk: it is no longer a draft, so nothing is left to
//!   drop and the branch that held it is reported as merged;
//! * on an asc server, `FindLanding` finds it (the landing map and the ref log: never the
//!   message);
//! * on a plain git remote (Stage 0), its patch id equals a trunk commit's since the merge
//!   base (`asc cherry`). A squash merge cannot be told apart from an unmerged change: a
//!   branch whose remote branch vanished at the fetch without either test matching is
//!   reported as "cannot tell" and left alone.
//!
//! Dropping a landed change is `asc abandon`: the changes above it restack onto its parent and
//! the branches on it move there. A stack that conflicts while it restacks is aborted, left
//! as it was and reported. The trunk fast-forward and the branch deletion go through the
//! `merge --ff-only` and `branch` verbs, so their checks (a dirty work tree, a branch checked
//! out elsewhere) are git's own. Every child runs with its output captured: this verb prints
//! one report.

use std::collections::{BTreeSet, HashSet};
use std::process::{ExitCode, Stdio};

use asc_core::id::{ChangesetId, RefTarget};
use asc_git::projection::Head;
use asc_state::state::workspaces;
use asc_store::refs::Refs as _;
use serde::Serialize;

use super::restack::{self, Flow};
use super::sl::{LStacks, Local, TrunkInfo, clean, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::native::NativeSpec;
use crate::remote::{self, Intent};
use crate::stack::json::{VERSION, line};
use crate::stack::opstate::{self, StackOp};

/// Most changes asked of the server about, and most of one stack's patch ids compared.
const MAX_CHANGES: usize = 4096;
/// Most branches read and, under `-d`, deleted by one run.
const MAX_BRANCHES: usize = 100_000;
/// Most restack rounds of one run (each round settles at least one stack or skips it).
const MAX_ROUNDS: usize = 256;
/// Largest reply taken from `FindLanding`.
const MAX_MESSAGE: usize = 1 << 20;
/// Longest child output kept.
const MAX_CHILD: usize = 16 * 1024 * 1024;
/// Longest part of a child's error text shown.
const MAX_SHOWN: usize = 2048;

#[derive(clap::Parser, Debug)]
#[command(name = "sync", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Do not fetch first.
    #[arg(long = "no-fetch")]
    pub no_fetch: bool,
    /// Drop landed changes but restack nothing.
    #[arg(long = "no-restack")]
    pub no_restack: bool,
    /// Delete the merged branches.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// Fast-forward every configured trunk, not only the current stack's.
    #[arg(long = "all-trunks")]
    pub all_trunks: bool,
    /// Report what would change; change nothing (no fetch either).
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "sync",
    flags: &[
        "--no-fetch",
        "--no-restack",
        "-d",
        "--delete",
        "--all-trunks",
        "-n",
        "--dry-run",
        "--json",
    ],
    placeholders: &[],
}];

super::verb!(SYNC, "sync");
// `asc stack sync` is the same command as `asc sync` (SPEC 20.7 lists the stack verbs under both spellings).
super::info::sub!(STACK_SYNC, "sync", Args, run);

// ---------------------------------------------------------------- report

#[derive(Serialize, Default)]
struct Moved {
    trunk: String,
    to: String,
    /// The full ids the trunk moved from and to (`Bridge.Run` maps them to an `OpResult`).
    old: String,
    new: String,
}

#[derive(Serialize, Default)]
struct Dropped {
    change: String,
    commit: String,
    subject: String,
    /// `server` (FindLanding) or `patch-id`.
    by: String,
}

#[derive(Serialize, Default)]
struct Restacked {
    change: String,
    subject: String,
    old: String,
    new: String,
}

#[derive(Serialize, Default)]
struct Skipped {
    what: String,
    reason: String,
}

#[derive(Serialize, Default)]
struct Report {
    version: u32,
    dry_run: bool,
    fetched: Option<String>,
    fast_forwarded: Vec<Moved>,
    landed: Vec<Dropped>,
    restacked: Vec<Restacked>,
    skipped: Vec<Skipped>,
    cannot_tell: Vec<String>,
    merged_branches: Vec<String>,
    deleted_branches: Vec<String>,
}

fn render(r: &Report) -> String {
    let mut out = String::new();
    let verb = |done: &str, would: &str| {
        if r.dry_run {
            would.to_owned()
        } else {
            done.to_owned()
        }
    };
    if let Some(f) = &r.fetched {
        out.push_str(&format!("Fetched {}\n", clean(f)));
    }
    for m in &r.fast_forwarded {
        out.push_str(&format!(
            "{} {} to {}\n",
            verb("Fast-forwarded", "Would fast-forward"),
            clean(&m.trunk),
            m.to
        ));
    }
    for d in &r.landed {
        let short: String = d.change.chars().take(12).collect();
        out.push_str(&format!(
            "{} {short} {} (landed, by {})\n",
            verb("Dropped", "Would drop"),
            clean(&d.subject),
            d.by
        ));
    }
    for d in &r.restacked {
        let short: String = d.change.chars().take(12).collect();
        out.push_str(&format!("Restacked {short} {}\n", clean(&d.subject)));
    }
    for s in &r.skipped {
        out.push_str(&format!(
            "Skipped {}: {}\n",
            clean(&s.what),
            clean(&s.reason)
        ));
    }
    for b in &r.cannot_tell {
        out.push_str(&format!(
            "Cannot tell whether '{}' landed (its remote branch is gone: a squash merge?)\n",
            clean(b)
        ));
    }
    for b in &r.deleted_branches {
        out.push_str(&format!("Deleted branch {}\n", clean(b)));
    }
    if !r.merged_branches.is_empty() && r.deleted_branches.is_empty() {
        let names: Vec<String> = r.merged_branches.iter().map(|b| clean(b)).collect();
        out.push_str(&format!(
            "Merged branches: {} (run 'asc sync -d' to delete them)\n",
            names.join(", ")
        ));
    }
    out
}

// ---------------------------------------------------------------- children

struct Child {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `asc <args>` with the output captured.
fn child(ctx: &Ctx, args: &[&str]) -> Result<Child> {
    let exe = std::env::current_exe().map_err(|e| fatal(format!("cannot find asc: {e}")))?;
    let mut run = ctx
        .child(exe)?
        .args(["-c", "advice.mergeConflict=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| fatal(format!("cannot run asc: {e}")))?;
    let (Some(out), Some(err)) = (run.stdout.take(), run.stderr.take()) else {
        return Err(fatal("cannot read the output of asc"));
    };
    let out = std::thread::spawn(move || read_bounded(out, MAX_CHILD));
    let err = std::thread::spawn(move || read_bounded(err, MAX_CHILD));
    let status = run
        .wait()
        .map_err(|e| fatal(format!("cannot run asc: {e}")))?;
    let text = |h: std::thread::JoinHandle<Vec<u8>>| {
        String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned()
    };
    Ok(Child {
        code: status.code().unwrap_or(1),
        stdout: text(out),
        stderr: text(err),
    })
}

/// The first `max` bytes of `r`. The rest is read and thrown away, so the child never blocks on
/// a full pipe and memory stays bounded.
fn read_bounded(mut r: impl std::io::Read, max: usize) -> Vec<u8> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let _ = r.by_ref().take(max as u64).read_to_end(&mut buf);
    let _ = std::io::copy(&mut r, &mut std::io::sink());
    buf
}

/// The first part of a child's error text, safe to print.
fn shown(c: &Child) -> String {
    let mut s = clean(c.stderr.trim());
    if s.is_empty() {
        s = clean(c.stdout.trim());
    }
    if s.len() > MAX_SHOWN {
        let mut end = MAX_SHOWN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

// ---------------------------------------------------------------- helpers

fn busy(ws: &WsCtx) -> Result<()> {
    let git = ws.projection.git_dir();
    if opstate::exists(git) {
        return Err(CliError::Fatal(
            "asc: a restack is already in progress (fix the conflicts, then run 'asc continue', or run 'asc abort')".into(),
        ));
    }
    for f in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "sequencer",
    ] {
        if git.join(f).exists() {
            return Err(CliError::Fatal(format!(
                "asc: cannot sync: an operation is in progress ({f}); finish it with 'asc continue' or 'asc abort'"
            )));
        }
    }
    Ok(())
}

/// The remote to fetch from and ask: the first trunk's remote, else `origin`, else the first.
fn pick_remote(ctx: &Ctx) -> Option<String> {
    choose_remote(remote::remote_names(ctx))
}

/// `origin`, else the first name. A name that starts with `-` could be read as an option by the
/// child, so it is never used.
fn choose_remote(mut names: Vec<String>) -> Option<String> {
    names.retain(|n| !n.starts_with('-'));
    if names.iter().any(|n| n == "origin") {
        return Some("origin".to_owned());
    }
    names.into_iter().next()
}

async fn subject_of(ws: &WsCtx, cs: ChangesetId) -> String {
    let Ok(c) = ws.engine.get_changeset(cs).await else {
        return String::new();
    };
    let line = c.message.split(|&b| b == b'\n').next().unwrap_or_default();
    let mut s = String::from_utf8_lossy(line).trim_end().to_owned();
    if s.len() > 200 {
        let mut end = 200;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Whether the change adds nothing over its parent (every empty patch has the same id, so an
/// empty change must never match a trunk commit by patch id).
async fn is_empty_change(ws: &WsCtx, cs: ChangesetId) -> Result<bool> {
    let c = ws.engine.get_changeset(cs).await?;
    let Some(&p) = c.parents.first() else {
        return Ok(false);
    };
    Ok(ws.engine.get_changeset(p).await?.root == c.root)
}

/// Branches checked out by another workspace of this repository.
async fn checked_out_elsewhere(ws: &WsCtx) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
        if w.ws.id == ws.workspace.ws.id {
            continue;
        }
        if let Some(b) = w.ws.head.strip_prefix("refs/heads/") {
            out.push((b.to_owned(), w.ws.path.clone()));
        }
    }
    Ok(out)
}

fn current_branch(ws: &WsCtx) -> Result<Option<String>> {
    Ok(match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    })
}

/// The trunks `sync` fast-forwards: all with `--all-trunks`, else the one under HEAD's stack
/// (the first when HEAD is on none).
async fn selected_trunks(
    st: &LStacks<'_, '_>,
    infos: Vec<TrunkInfo>,
    all: bool,
) -> Result<Vec<TrunkInfo>> {
    if all {
        return Ok(infos);
    }
    let mut tip = None;
    if let Some(head) = st.current_head() {
        tip = st.stack_of(head).await.map_err(stack_err)?.trunk;
    }
    let mut it = infos.into_iter();
    let first = it.next();
    let rest: Vec<TrunkInfo> = it.collect();
    Ok(match tip {
        Some(t) => rest
            .into_iter()
            .chain(first)
            .filter(|i| i.tip == t)
            .take(1)
            .collect(),
        None => first.into_iter().collect(),
    })
}

// ---------------------------------------------------------------- trunks

async fn fast_forward(ctx: &Ctx, a: &Args, rep: &mut Report) -> Result<()> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    let st = l.stacks().await?;
    let picked = selected_trunks(&st, l.env.trunk_info(), a.all_trunks).await?;
    let elsewhere = checked_out_elsewhere(ws).await?;
    let here = current_branch(ws)?;
    for t in picked {
        let Some(remote_name) = t.remote.clone() else {
            continue;
        };
        let Some(local) = crate::cli::switch::branch_tip(ws, &t.name).await? else {
            continue;
        };
        if local == t.tip || l.graph.is_ancestor(t.tip, local).await.map_err(fatal)? {
            continue;
        }
        if !l.graph.is_ancestor(local, t.tip).await.map_err(fatal)? {
            rep.skipped.push(Skipped {
                what: format!("trunk {}", t.name),
                reason: format!(
                    "the local branch and {remote_name}/{} have diverged",
                    t.name
                ),
            });
            continue;
        }
        if let Some((_, path)) = elsewhere.iter().find(|(b, _)| *b == t.name) {
            rep.skipped.push(Skipped {
                what: format!("trunk {}", t.name),
                reason: format!("checked out in another workspace ({path})"),
            });
            continue;
        }
        let tip = l.git_id(t.tip).await?;
        let old = l.git_id(local).await?;
        let moved = Moved {
            trunk: t.name.clone(),
            to: tip.chars().take(12).collect(),
            old: old.clone(),
            new: tip.clone(),
        };
        if a.dry_run {
            rep.fast_forwarded.push(moved);
            continue;
        }
        let done = if here.as_deref() == Some(t.name.as_str()) {
            child(ctx, &["merge", "--ff-only", "-q", &tip])?
        } else {
            // Only while the branch still holds the commit it was checked against.
            let argv = move_args(&t.name, &tip, &old);
            child(ctx, &argv.each_ref().map(String::as_str))?
        };
        if done.code == 0 {
            rep.fast_forwarded.push(moved);
        } else {
            rep.skipped.push(Skipped {
                what: format!("trunk {}", t.name),
                reason: shown(&done),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- landed

/// What the landed changes of every stack are, and the branches that pointed at them.
struct Landed {
    drops: Vec<(ChangesetId, String)>,
    branches: BTreeSet<String>,
}

async fn find_landed(ctx: &Ctx, remote_name: Option<&str>, rep: &mut Report) -> Result<Landed> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    let st = l.stacks().await?;
    let mut out = Landed {
        drops: Vec::new(),
        branches: BTreeSet::new(),
    };

    // (draft changeset, the trunk tip of its stack, that trunk's name)
    let mut heads: Vec<ChangesetId> = st.heads().to_vec();
    heads.extend(st.current_head());
    let mut seen_head = HashSet::new();
    let mut stacks = Vec::new();
    for h in heads {
        if !seen_head.insert(h) {
            continue;
        }
        let stack = st.stack_of(h).await.map_err(stack_err)?;
        if !stack.changes.is_empty() && stack.trunk.is_some() {
            stacks.push(stack);
        }
    }
    let trunk_name = |tip: ChangesetId| {
        l.env
            .trunk_info()
            .into_iter()
            .find(|t| t.tip == tip)
            .map(|t| t.name)
            .unwrap_or_default()
    };

    // The server answers by change id through FindLanding; a plain git remote by patch id.
    let server = match remote_name {
        Some(name) => match remote::open(ctx, ws, name, Intent::Read).await {
            Ok(o) => o.server,
            Err(_) => None,
        },
        None => None,
    };
    let mut found: Vec<(ChangesetId, &'static str)> = Vec::new();
    let mut asked = HashSet::new();
    let mut total = 0usize;
    for stack in &stacks {
        let Some(trunk) = stack.trunk else { continue };
        if let Some(server) = &server {
            let conn = server.session().wire.clone();
            let mut stub = asc_rpc::services::repo_client::RepoClient::new(conn.channel.clone())
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE);
            for &cs in &stack.changes {
                if !asked.insert(cs) {
                    continue;
                }
                total += 1;
                if total > MAX_CHANGES {
                    return Err(CliError::Fatal(format!(
                        "asc: sync: more than {MAX_CHANGES} changes to check"
                    )));
                }
                let request = conn.request(
                    asc_rpc::server::wire::FindLandingRequest {
                        repo: Some(asc_rpc::server::wire::RepoRef {
                            name: conn.repo.clone(),
                            id_mode: asc_rpc::IdMode::Asc.to_wire(),
                        }),
                        changeset: bytes::Bytes::copy_from_slice(cs.as_bytes()),
                        r#ref: format!("refs/heads/{}", trunk_name(trunk)),
                    },
                    None,
                );
                let reply = stub
                    .find_landing(request)
                    .await
                    .map_err(|s| CliError::Fatal(format!("asc: sync: {}", clean(s.message()))))?
                    .into_inner();
                if reply.found {
                    found.push((cs, "server"));
                }
            }
        } else {
            let (Some(&top), trunk_git) = (stack.changes.last(), l.git_id(trunk).await?) else {
                continue;
            };
            let top_git = l.git_id(top).await?;
            let run = child(ctx, &["cherry", &trunk_git, &top_git])?;
            if run.code != 0 {
                rep.skipped.push(Skipped {
                    what: "patch-id check".into(),
                    reason: shown(&run),
                });
                continue;
            }
            let mut by_git = std::collections::HashMap::new();
            for &cs in &stack.changes {
                total += 1;
                if total > MAX_CHANGES {
                    return Err(CliError::Fatal(format!(
                        "asc: sync: more than {MAX_CHANGES} changes to check"
                    )));
                }
                by_git.insert(l.git_id(cs).await?, cs);
            }
            for row in run.stdout.lines() {
                let Some(sha) = row.strip_prefix("- ") else {
                    continue;
                };
                if let Some(&cs) = by_git.get(sha.trim())
                    && asked.insert(cs)
                    && !is_empty_change(ws, cs).await?
                {
                    found.push((cs, "patch-id"));
                }
            }
        }
    }

    let mut dropped = HashSet::new();
    for (cs, by) in found {
        if !dropped.insert(cs) {
            continue;
        }
        let Some(row) = st.draft_row(cs).await.map_err(stack_err)? else {
            continue;
        };
        for b in l.env.branches_at(cs) {
            out.branches.insert(b);
        }
        rep.landed.push(Dropped {
            change: row.change.to_string(),
            commit: l.git_id(cs).await?,
            subject: subject_of(ws, cs).await,
            by: by.to_owned(),
        });
        // Named by change id: the stack verbs resolve a change id, not a full commit id.
        out.drops.push((cs, row.change.to_string()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- restack

/// One stack that needs work: restack for an obsolete or landed parent (the verb does it), or
/// move its base onto the trunk tip it is behind.
enum Work {
    Verb {
        change: String,
    },
    OntoTrunk {
        steps: Vec<ChangesetId>,
        onto: ChangesetId,
    },
}

async fn next_work(
    l: &Local<'_>,
    skip: &HashSet<ChangesetId>,
) -> Result<Option<(ChangesetId, Work)>> {
    let st = l.stacks().await?;
    let mut heads: Vec<ChangesetId> = st.heads().to_vec();
    heads.extend(st.current_head());
    let mut seen = HashSet::new();
    for h in heads {
        if !seen.insert(h) {
            continue;
        }
        let stack = st.stack_of(h).await.map_err(stack_err)?;
        for &c in &stack.changes {
            if !skip.contains(&c) && st.needs_restack(c).await.map_err(stack_err)? {
                let row = st.draft_row(c).await.map_err(stack_err)?;
                let Some(row) = row else { continue };
                return Ok(Some((
                    c,
                    Work::Verb {
                        change: row.change.to_string(),
                    },
                )));
            }
        }
        let (Some(&bottom), Some(trunk), Some(base)) =
            (stack.changes.first(), stack.trunk, stack.base)
        else {
            continue;
        };
        if skip.contains(&bottom) || base == trunk {
            continue;
        }
        if st.is_immutable(base).await.map_err(stack_err)?
            && l.graph.is_ancestor(base, trunk).await.map_err(fatal)?
        {
            return Ok(Some((
                bottom,
                Work::OntoTrunk {
                    steps: stack.changes.clone(),
                    onto: trunk,
                },
            )));
        }
    }
    Ok(None)
}

async fn restack_all(ctx: &Ctx, rep: &mut Report) -> Result<()> {
    let mut skip: HashSet<ChangesetId> = HashSet::new();
    for _ in 0..MAX_ROUNDS {
        let l = Local::open(ctx).await?;
        let Some((key, work)) = next_work(&l, &skip).await? else {
            return Ok(());
        };
        let ws = l.ws;
        let label = subject_of(ws, key).await;
        skip.insert(key);
        match work {
            Work::Verb { change } => {
                let run = child(ctx, &["restack", "--json", &change])?;
                if run.code == 0 {
                    // A rewritten change has a new id; one that is not rewritten stays in `skip`
                    // so a verb that settles nothing cannot loop.
                    collect_rows(&run.stdout, rep);
                } else {
                    if opstate::exists(ws.projection.git_dir()) {
                        let _ = child(ctx, &["abort"])?;
                    }
                    rep.skipped.push(Skipped {
                        what: format!("stack at '{}'", label),
                        reason: if run.code == 1 {
                            "restack stopped on a conflict".into()
                        } else {
                            shown(&run)
                        },
                    });
                }
            }
            Work::OntoTrunk { steps, onto } => {
                let head = ws
                    .head_commit()
                    .await?
                    .ok_or_else(|| CliError::Fatal("asc: sync: HEAD is not on a change".into()))?;
                let op = StackOp {
                    op_id: uuid::Uuid::now_v7(),
                    head_branch: current_branch(ws)?,
                    head,
                    follow: restack::followers(ws, &steps).await?,
                    done: Vec::new(),
                    at: steps[0],
                    at_onto: onto,
                    rest: Vec::new(),
                    sign: ctx.config.bool("commit.gpgsign") == Some(true),
                };
                match restack::drive(ctx, ws, op, Vec::new(), onto, steps).await {
                    Ok(Flow::Done(rows)) => {
                        for r in rows {
                            rep.restacked.push(Restacked {
                                change: r.change,
                                subject: r.subject,
                                old: r.old,
                                new: r.new,
                            });
                        }
                    }
                    Ok(Flow::Paused(_)) => {
                        let _ = child(ctx, &["abort"])?;
                        rep.skipped.push(Skipped {
                            what: format!("stack at '{label}'"),
                            reason: "restack onto the trunk stopped on a conflict".into(),
                        });
                    }
                    Err(e) => {
                        if opstate::exists(ws.projection.git_dir()) {
                            let _ = child(ctx, &["abort"])?;
                        }
                        rep.skipped.push(Skipped {
                            what: format!("stack at '{label}'"),
                            reason: clean(&e.to_string()),
                        });
                    }
                }
            }
        }
    }
    rep.skipped.push(Skipped {
        what: "restack".into(),
        reason: format!("stopped after {MAX_ROUNDS} rounds"),
    });
    Ok(())
}

/// Adds the rows of an `asc restack --json` result.
fn collect_rows(stdout: &str, rep: &mut Report) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return;
    };
    let Some(rows) = v.get("rewrites").and_then(|r| r.as_array()) else {
        return;
    };
    for r in rows.iter().take(MAX_CHANGES) {
        let text = |k: &str| {
            r.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        rep.restacked.push(Restacked {
            change: text("change"),
            subject: text("subject"),
            old: text("old"),
            new: text("new"),
        });
    }
}

// ---------------------------------------------------------------- branches

/// The branches that `sync -d` may delete, each with the git id it was judged merged at.
type MergedTips = Vec<(String, String)>;

async fn merged_branches(
    ctx: &Ctx,
    landed: &BTreeSet<String>,
    rep: &mut Report,
) -> Result<MergedTips> {
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    let entries = ws.db.refs().list(ws.repo.key, "refs/heads/").await?;
    if entries.len() > MAX_BRANCHES {
        return Err(fatal(format!("more than {MAX_BRANCHES} branches")));
    }
    let infos = l.env.trunk_info();
    let trunk_names: HashSet<&str> = l.env.trunk_names().iter().map(String::as_str).collect();
    let here = current_branch(ws)?;
    let elsewhere: HashSet<String> = checked_out_elsewhere(ws)
        .await?
        .into_iter()
        .map(|(b, _)| b)
        .collect();
    let mut merged = std::collections::BTreeMap::new();
    for e in entries {
        let RefTarget::Changeset(tip) = e.target else {
            continue;
        };
        let Ok(full) = String::from_utf8(e.name.as_bytes().to_vec()) else {
            continue;
        };
        let Some(name) = full.strip_prefix("refs/heads/") else {
            continue;
        };
        if trunk_names.contains(name) || here.as_deref() == Some(name) || elsewhere.contains(name) {
            continue;
        }
        let gone = upstream_gone(ctx, ws, name).await?;
        let mut is_merged = false;
        let mut at_tip = false;
        for t in &infos {
            if t.tip == tip {
                at_tip = true;
            } else if l.graph.is_ancestor(tip, t.tip).await.map_err(fatal)? {
                is_merged = true;
            }
        }
        // A branch level with the trunk is merged only when it had a remote branch that is gone
        // (a fresh branch also sits on the tip), or when its change was found landed.
        if is_merged || landed.contains(name) || (at_tip && gone) {
            merged.insert(name.to_owned(), tip);
        } else if gone {
            rep.cannot_tell.push(name.to_owned());
        }
    }
    let mut tips = Vec::with_capacity(merged.len());
    for (name, tip) in merged {
        tips.push((name, l.git_id(tip).await?));
    }
    rep.merged_branches = tips.iter().map(|(n, _)| n.clone()).collect();
    Ok(tips)
}

/// `asc update-ref` arguments that move `branch` to `new` only while it holds `old`.
fn move_args(branch: &str, new: &str, old: &str) -> [String; 5] {
    [
        "update-ref".to_owned(),
        "--".to_owned(),
        format!("refs/heads/{branch}"),
        new.to_owned(),
        old.to_owned(),
    ]
}

/// `asc update-ref` arguments that delete `branch` only while it holds `tip`.
fn delete_args(branch: &str, tip: &str) -> [String; 5] {
    [
        "update-ref".to_owned(),
        "-d".to_owned(),
        "--".to_owned(),
        format!("refs/heads/{branch}"),
        tip.to_owned(),
    ]
}

/// Whether the branch tracks a remote branch that no longer exists.
async fn upstream_gone(ctx: &Ctx, ws: &WsCtx, branch: &str) -> Result<bool> {
    let (Some(remote_name), Some(merge)) = (
        ctx.config.get(&format!("branch.{branch}.remote")),
        ctx.config.get(&format!("branch.{branch}.merge")),
    ) else {
        return Ok(false);
    };
    let Some(short) = merge.strip_prefix("refs/heads/") else {
        return Ok(false);
    };
    if remote_name == "." {
        return Ok(false);
    }
    let Ok(name) =
        asc_core::path::RefName::new(format!("refs/remotes/{remote_name}/{short}").into_bytes())
    else {
        return Ok(false);
    };
    Ok(ws.db.refs().get(ws.repo.key, &name).await?.is_none())
}

// ---------------------------------------------------------------- run

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    busy(ws)?;
    let mut rep = Report {
        version: VERSION,
        dry_run: a.dry_run,
        ..Report::default()
    };
    let remote_name = pick_remote(ctx);

    if !a.no_fetch
        && !a.dry_run
        && let Some(r) = &remote_name
    {
        let run = child(ctx, &["fetch", "-q", "--prune", "--", r])?;
        if run.code != 0 {
            return Err(CliError::Raw(
                format!("asc: sync: fetch failed\n{}", shown(&run)),
                128,
            ));
        }
        rep.fetched = Some(r.clone());
    }

    fast_forward(ctx, &a, &mut rep).await?;

    let landed = find_landed(ctx, remote_name.as_deref(), &mut rep).await?;
    if !a.dry_run && !landed.drops.is_empty() {
        let mut argv: Vec<&str> = vec!["abandon", "--json"];
        // Top first is `abandon`'s own business; it takes the set in any order.
        argv.extend(landed.drops.iter().map(|(_, g)| g.as_str()));
        let run = child(ctx, &argv)?;
        if run.code != 0 {
            rep.landed.clear();
            rep.skipped.push(Skipped {
                what: "landed changes".into(),
                reason: shown(&run),
            });
        }
    }

    if !a.no_restack && !a.dry_run {
        restack_all(ctx, &mut rep).await?;
    }

    let tips = merged_branches(ctx, &landed.branches, &mut rep).await?;
    if a.delete && !a.dry_run {
        rep.merged_branches.clear();
        for (b, tip) in tips {
            // Another workspace may have taken the branch since it was judged merged.
            if let Some(path) = crate::cli::switch::held_anywhere(ws, &b).await? {
                rep.skipped.push(Skipped {
                    what: format!("branch {b}"),
                    reason: format!("checked out in another workspace ({path})"),
                });
                rep.merged_branches.push(b);
                continue;
            }
            // Deleted only while it still holds the commit that was judged merged; a commit
            // made since then makes the delete fail instead of being lost.
            let argv = delete_args(&b, &tip);
            let run = child(ctx, &argv.each_ref().map(String::as_str))?;
            if run.code == 0 {
                crate::cli::switch::remove_branch_section(ctx, &b).await;
                rep.deleted_branches.push(b);
            } else {
                rep.skipped.push(Skipped {
                    what: format!("branch {b}"),
                    reason: shown(&run),
                });
                rep.merged_branches.push(b);
            }
        }
    }

    if a.json {
        let text = line(&rep).map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
        print!("{text}");
    } else {
        print!("{}", render(&rep));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_name_that_looks_like_an_option_is_never_chosen() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(choose_remote(names(&["--upload-pack=x"])), None);
        assert_eq!(
            choose_remote(names(&["-o", "up"])).as_deref(),
            Some("up")
        );
        assert_eq!(
            choose_remote(names(&["--x", "origin"])).as_deref(),
            Some("origin")
        );
        assert_eq!(choose_remote(names(&["b", "a"])).as_deref(), Some("b"));
    }

    #[test]
    fn the_trunk_move_and_branch_delete_name_the_value_they_were_checked_against() {
        let m = move_args("master", "new", "old");
        assert_eq!(m, ["update-ref", "--", "refs/heads/master", "new", "old"]);
        let d = delete_args("topic", "tip");
        assert_eq!(d, ["update-ref", "-d", "--", "refs/heads/topic", "tip"]);
    }

    #[test]
    fn child_output_is_bounded_and_the_rest_drained() {
        let data = [b'x'; 100];
        assert_eq!(read_bounded(&data[..], 10).len(), 10);
        assert_eq!(read_bounded(&data[..], 1000).len(), 100);
        let mut r = &data[..];
        let _ = read_bounded(&mut r, 10);
        assert!(r.is_empty(), "the remainder must be drained");
    }
}
