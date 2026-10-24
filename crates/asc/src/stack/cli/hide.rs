//! `asc hide <rev>…` and `asc unhide <id>…` (SPEC §21.2.2, BD8, §21.7): hide a draft and the drafts
//! above it, or bring one back. Nothing is deleted: the objects stay addressable by id until the
//! retention of §21.2.4 ends, and `asc unhide <id>` recovers a draft within it.
//!
//! The effect is on the local draft head rows (`draft_heads.hidden_at`, `cloud_queue`) as the
//! maintenance table of §21.2.2 says. The op log is ref based, so each change also writes a
//! marker ref in the same op: `refs/asc/hidden/<head>` for a head that was hidden and
//! `refs/asc/exposed/<cs>` for a parent that became a head. `asc undo` and `asc redo` move the
//! markers; [`reconcile`] then brings the rows in line with the newest marker movement of each
//! name. A row with no marker history is never touched, so rows hidden by other paths (expiry,
//! the server) are left alone.
//!
//! A marker points at a trunk tip, not at the draft: refs are GC roots, and a marker on the draft
//! would keep a hidden draft alive past `asc.draft.hiddenDays` (B06).

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_engine::branchless::DraftHead as EngineHead;
use asc_engine::branchless::visible::{HidePlan, hide_plan, unhide_plan};
use asc_engine::change::ChangeRows as _;
use asc_engine::stack::StackEnv as _;
use asc_state::drafts::{self, Drafts};
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};

use super::info::resolve_change;
use super::restack::Flow;
use super::reword::refuse_busy;
use super::rewrite::{Made, emit, emit_nothing};
use super::sl::{Local, clean, fatal};
use super::uncommit::subject_of;
use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::restore::actor;
use crate::ext;
use crate::native::NativeSpec;

/// Most revisions one run takes.
const MAX_REVS: usize = 1024;
/// Most draft head rows read.
const MAX_HEADS: usize = 100_000;
/// Most `ref_log` rows [`reconcile`] reads.
const MAX_LOG: i64 = 200_000;
/// A `ref_log` row: the ref name and its new value (`None` when it was deleted).
type LogRow = (String, Option<Vec<u8>>);
/// Marker ref prefixes.
const HIDDEN: &str = "refs/asc/hidden/";
const EXPOSED: &str = "refs/asc/exposed/";
/// Key of the upload queue bound.
const QUEUE_MAX_KEY: &str = "asc.cloud.queueMax";

#[derive(clap::Parser, Debug)]
#[command(name = "hide", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The draft changes to hide: a change prefix or a revision.
    #[arg(required = true, num_args = 1..)]
    pub revs: Vec<String>,
    /// Report what would be hidden; change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "hide",
    flags: &["-n", "--dry-run", "--json"],
    placeholders: &[],
}];

super::verb!(HIDE, "hide");

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn queue_max(ctx: &Ctx) -> usize {
    ctx.config
        .get(QUEUE_MAX_KEY)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(drafts::QUEUE_MAX, |n| n.clamp(1, drafts::QUEUE_MAX))
}

fn drafts_err(e: drafts::DraftsError) -> CliError {
    fatal(e)
}

fn db_err(e: sqlx::Error) -> CliError {
    fatal(asc_state::Error::from(e))
}

fn engine_head(h: drafts::DraftHead) -> EngineHead {
    EngineHead {
        cs: h.cs,
        change: h.change,
        added: h.added,
        hidden_at: h.hidden_at,
    }
}

fn marker(prefix: &str, cs: ChangesetId) -> Result<RefName> {
    RefName::new(format!("{prefix}{cs}").into_bytes()).map_err(fatal)
}

/// What a marker points at: a trunk tip, so it roots no draft.
async fn anchor(l: &Local<'_>, fallback: ChangesetId) -> ChangesetId {
    l.env
        .trunk_tips()
        .await
        .ok()
        .and_then(|t| t.first().copied())
        .unwrap_or(fallback)
}

/// Sets (`Some`) or removes (`None`) the markers of one step, in the running op.
async fn write_markers(
    ctx: &Ctx,
    ws: &WsCtx,
    set: &[(RefName, Option<ChangesetId>)],
    reason: &str,
) -> Result<()> {
    let mut updates = Vec::new();
    for (name, to) in set {
        let cur = ws.refs.get(ws.repo.key, name).await?.map(|e| e.target);
        let new = to.map(RefTarget::Changeset);
        if cur == new {
            continue;
        }
        updates.push(RefUpdate {
            name: name.clone(),
            expect: cur.map_or(Expect::Absent, Expect::Value),
            new,
            expires_at: None,
        });
    }
    if updates.is_empty() {
        return Ok(());
    }
    let txn = RefTxn {
        updates,
        actor: actor(),
        reason: String::new(),
        op_id: ext::OpId::new().0,
    };
    ctx.update_local_refs(txn, reason, StagingAfterMove::Keep)
        .await
}

/// Brings `draft_heads` in line with the newest marker movement of each name (after `asc undo`
/// or `asc redo`, which move refs only). A name with no history changes nothing.
pub(crate) async fn reconcile(ctx: &Ctx) -> Result<()> {
    let ws = ctx.ws()?;
    let rows: Vec<LogRow> = sqlx::query_as(
        "SELECT name, new FROM ref_log WHERE repo_id = ? AND (name LIKE 'refs/asc/hidden/%' OR name LIKE 'refs/asc/exposed/%') \
         ORDER BY seq LIMIT ?",
    )
    .bind(ws.repo.key.0)
    .bind(MAX_LOG)
    .fetch_all(ws.db.pool())
    .await
    .map_err(db_err)?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut latest: HashMap<String, bool> = HashMap::new();
    for (name, new) in rows {
        latest.insert(name, new.is_some());
    }
    let d = Drafts::new(ws.repo.key);
    let mut have: HashMap<ChangesetId, drafts::DraftHead> = HashMap::new();
    for h in d.visible(&ws.db, MAX_HEADS).await.map_err(drafts_err)? {
        have.insert(h.cs, h);
    }
    for h in d.hidden(&ws.db, MAX_HEADS).await.map_err(drafts_err)? {
        have.insert(h.cs, h);
    }
    let (at, qmax) = (now(), queue_max(ctx));
    let mut tx = ws.db.begin().await.map_err(fatal)?;
    let mut names: Vec<_> = latest.into_iter().collect();
    names.sort();
    for (name, present) in names {
        let (hidden, hex) = match name.strip_prefix(HIDDEN) {
            Some(h) => (true, h),
            None => match name.strip_prefix(EXPOSED) {
                Some(h) => (false, h),
                None => continue,
            },
        };
        let Ok(cs) = hex.parse::<ChangesetId>() else {
            continue;
        };
        match (hidden, present, have.get(&cs)) {
            // Hide (again): the head goes.
            (true, true, Some(r)) if r.hidden_at.is_none() => {
                d.hide(&mut tx, &[cs], None, at, qmax)
                    .await
                    .map_err(drafts_err)?;
            }
            // Hide undone: the head is back.
            (true, false, Some(r)) if r.hidden_at.is_some() => {
                d.unhide(&mut tx, cs, r.change, at, qmax)
                    .await
                    .map_err(drafts_err)?;
            }
            // Exposure undone: the parent is no head again.
            (false, false, Some(r)) if r.hidden_at.is_none() => {
                d.immutable(&mut tx, &[cs]).await.map_err(drafts_err)?;
            }
            // Exposure redone: the parent is a head again.
            (false, true, None) => {
                if let Some(c) = crate::stack::change_rows(ctx)?
                    .get(cs)
                    .await
                    .map_err(fatal)?
                {
                    d.unhide(&mut tx, cs, c.change, at, qmax)
                        .await
                        .map_err(drafts_err)?;
                }
            }
            _ => {}
        }
    }
    tx.commit().await.map_err(db_err)?;
    Ok(())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.revs.len() > MAX_REVS {
        return Err(CliError::Raw("asc: hide: too many revisions".into(), 1));
    }
    let ws = ctx.ws()?;
    refuse_busy(ws, "hide")?;
    reconcile(ctx).await?;

    let mut targets: Vec<(ChangesetId, Made)> = Vec::new();
    {
        let l = Local::open(ctx).await?;
        let st = l.stacks().await?;
        let mut seen = HashSet::new();
        for w in &a.revs {
            let (cs, row) = resolve_change(ctx, &l, &st, Some(w)).await?;
            if !seen.insert(cs) {
                continue;
            }
            let c = ws.engine.get_changeset(cs).await?;
            targets.push((
                cs,
                Made {
                    cs,
                    change: row.change.to_string(),
                    subject: subject_of(&c.message),
                },
            ));
        }
    }
    if a.dry_run {
        let made: Vec<Made> = targets.into_iter().map(|t| t.1).collect();
        return emit(
            ctx,
            a.json,
            "hide",
            "Would hide",
            &made,
            0,
            &Flow::Done(Vec::new()),
        );
    }

    let d = Drafts::new(ws.repo.key);
    let mut done: Vec<Made> = Vec::new();
    let mut hidden_roots: Vec<ChangesetId> = Vec::new();
    for (cs, made) in targets {
        let l = Local::open(ctx).await?;
        let st = l.stacks().await?;
        let rows: Vec<EngineHead> = d
            .visible(&ws.db, MAX_HEADS)
            .await
            .map_err(drafts_err)?
            .into_iter()
            .map(engine_head)
            .collect();
        let plan: HidePlan = match hide_plan(&st, &rows, cs).await {
            Ok(p) => p,
            Err(asc_engine::branchless::Error::NotVisible(_)) => {
                // An earlier target of this run already hid it with its descendants.
                let mut covered = false;
                for r in &hidden_roots {
                    covered |= l.graph.is_ancestor(*r, cs).await.map_err(fatal)?;
                }
                if covered {
                    continue;
                }
                return Err(CliError::Raw(
                    format!("asc: hide: {} is not visible", clean(&made.change)),
                    1,
                ));
            }
            Err(e) => return Err(CliError::Raw(format!("asc: hide: {e}"), 1)),
        };
        let to = anchor(&l, cs).await;
        let mut set = Vec::new();
        for h in &plan.heads {
            set.push((marker(HIDDEN, *h)?, Some(to)));
        }
        if let Some((p, _)) = plan.expose {
            set.push((marker(EXPOSED, p)?, Some(to)));
        }
        drop(st);
        drop(l);
        // Markers first: if the rows fail, the next `reconcile` finishes the job.
        write_markers(ctx, ws, &set, "hide").await?;
        let mut tx = ws.db.begin().await.map_err(fatal)?;
        d.hide(&mut tx, &plan.heads, plan.expose, now(), queue_max(ctx))
            .await
            .map_err(drafts_err)?;
        tx.commit().await.map_err(db_err)?;
        hidden_roots.push(cs);
        done.push(made);
    }
    if done.is_empty() {
        return emit_nothing(ctx, a.json, "hide");
    }
    emit(
        ctx,
        a.json,
        "hide",
        "Hid",
        &done,
        0,
        &Flow::Done(Vec::new()),
    )
}

/// `asc unhide <id>…`.
pub mod unhide {
    use super::*;

    #[derive(clap::Parser, Debug)]
    #[command(name = "unhide", disable_help_flag = true, disable_version_flag = true)]
    pub struct Args {
        /// The drafts to bring back: a change prefix of a hidden head or a revision.
        #[arg(required = true, num_args = 1..)]
        pub ids: Vec<String>,
        /// Report what would come back; change nothing.
        #[arg(short = 'n', long = "dry-run")]
        pub dry_run: bool,
        #[arg(long)]
        pub json: bool,
    }

    pub const NATIVE: &[NativeSpec] = &[NativeSpec {
        cmd: "unhide",
        flags: &["-n", "--dry-run", "--json"],
        placeholders: &[],
    }];

    crate::stack::cli::verb!(UNHIDE, "unhide");

    pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        if a.ids.len() > MAX_REVS {
            return Err(CliError::Raw("asc: unhide: too many revisions".into(), 1));
        }
        let ws = ctx.ws()?;
        refuse_busy(ws, "unhide")?;
        reconcile(ctx).await?;
        let d = Drafts::new(ws.repo.key);

        // Resolve everything before anything changes.
        let mut targets: Vec<(ChangesetId, Made)> = Vec::new();
        {
            let l = Local::open(ctx).await?;
            let st = l.stacks().await?;
            let hidden = d.hidden(&ws.db, MAX_HEADS).await.map_err(drafts_err)?;
            let mut seen = HashSet::new();
            for w in &a.ids {
                let cs = match by_hidden_prefix(&hidden, w)? {
                    Some(cs) => cs,
                    None => crate::cli::restore::resolve_commit(ctx, w)
                        .await?
                        .ok_or_else(|| CliError::Fatal(format!("bad revision '{}'", clean(w))))?,
                };
                if !seen.insert(cs) {
                    continue;
                }
                let (_, change) = unhide_plan(&st, cs)
                    .await
                    .map_err(|e| CliError::Raw(format!("asc: unhide: {e}"), 1))?;
                let c = ws.engine.get_changeset(cs).await?;
                targets.push((
                    cs,
                    Made {
                        cs,
                        change: change.to_string(),
                        subject: subject_of(&c.message),
                    },
                ));
            }
        }
        if a.dry_run {
            let made: Vec<Made> = targets.into_iter().map(|t| t.1).collect();
            return emit(
                ctx,
                a.json,
                "unhide",
                "Would unhide",
                &made,
                0,
                &Flow::Done(Vec::new()),
            );
        }

        let mut done = Vec::new();
        for (cs, made) in targets {
            let visible = d.visible(&ws.db, MAX_HEADS).await.map_err(drafts_err)?;
            if visible.iter().any(|h| h.cs == cs) {
                continue;
            }
            let hidden_row = d
                .hidden(&ws.db, MAX_HEADS)
                .await
                .map_err(drafts_err)?
                .into_iter()
                .any(|h| h.cs == cs);
            let l = Local::open(ctx).await?;
            let to = anchor(&l, cs).await;
            let change = l
                .stacks()
                .await?
                .draft_row(cs)
                .await
                .map_err(super::super::sl::stack_err)?
                .ok_or_else(|| CliError::Raw("asc: unhide: not a draft change".into(), 1))?
                .change;
            drop(l);
            // A hidden head loses its marker; any other draft becomes a head and is marked.
            let set = if hidden_row {
                vec![(marker(HIDDEN, cs)?, None)]
            } else {
                vec![(marker(EXPOSED, cs)?, Some(to))]
            };
            write_markers(ctx, ws, &set, "unhide").await?;
            let mut tx = ws.db.begin().await.map_err(fatal)?;
            d.unhide(&mut tx, cs, change, now(), queue_max(ctx))
                .await
                .map_err(drafts_err)?;
            tx.commit().await.map_err(db_err)?;
            done.push(made);
        }
        if done.is_empty() {
            return emit_nothing(ctx, a.json, "unhide");
        }
        emit(
            ctx,
            a.json,
            "unhide",
            "Unhid",
            &done,
            0,
            &Flow::Done(Vec::new()),
        )
    }

    /// The hidden head whose change id starts with `word`, when exactly one change matches.
    fn by_hidden_prefix(hidden: &[drafts::DraftHead], word: &str) -> Result<Option<ChangesetId>> {
        let Ok(p) = asc_core::change::ChangePrefix::parse(word) else {
            return Ok(None);
        };
        let hits: Vec<_> = hidden.iter().filter(|h| p.matches(&h.change)).collect();
        let Some(first) = hits.first() else {
            return Ok(None);
        };
        if hits.iter().any(|h| h.change != first.change) {
            return Err(CliError::Fatal(format!(
                "ambiguous change prefix '{}'",
                clean(word)
            )));
        }
        // The newest hidden version.
        Ok(hits.iter().max_by_key(|h| h.hidden_at).map(|h| h.cs))
    }
}
