//! `asc migrate branchless [--prune-merged] [--json]` (SPEC §21.6.2, client column; B19).
//!
//! * Reports local branches that master already contains (`merged`) and unmerged ones whose tip
//!   is older than [`STALE_DAYS`] (`stale`). Reporting changes nothing.
//! * `--prune-merged` deletes the merged branches in one ref transaction, so one `asc undo`
//!   brings them all back. `master`, `release/*`, `green/*` and any branch a worktree has
//!   checked out are never deleted.
//! * Seeds `draft_heads` from the tips of the unmerged branches (the heads among them), with the
//!   change id the server derives for the same commit: an existing local change id, else the
//!   B18a rule (jj header, Gerrit trailer, derived id) scoped by the branch ref.
//!   Seeding is idempotent: a commit that already has a row is left alone. The rows are
//!   derived state and are not part of the op log; running the command again re-seeds them.
//!
//! Fail closed: with no `master` (local or `origin/master`) nothing is classified, because every
//! branch would look like a draft; a store or graph error stops the run before any ref changes.

use std::collections::HashMap;
use std::process::ExitCode;

use asc_core::change::ChangeId;
use asc_core::change_migrate::migration_id;
use asc_core::git::EXTRA_GIT_COMMIT;
use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::change::ChangeRows;
use asc_engine::history::HistoryApi;
use asc_git::projection::Head;
use asc_state::drafts::{self, Drafts};
use asc_store::refs::{Expect, RefTxn, RefUpdate, Refs as _};

use crate::cli::common::{CliError, Ctx, StagingAfterMove, WsCtx};
use crate::cli::restore::actor;
use crate::cli::switch::{fatal, held_anywhere, peel, ref_name};
use crate::ext;

type Result<T> = crate::cli::common::Result<T>;

/// Most local branches one run looks at; a larger set is refused.
pub const MAX_BRANCHES: usize = 100_000;
/// Most draft heads one run seeds (the server's `max_heads_per_workspace`).
pub const MAX_SEEDS: usize = 10_000;
/// Most branches one run deletes.
pub const MAX_PRUNE: usize = 10_000;
/// An unmerged branch whose tip is older than this many days is stale.
pub const STALE_DAYS: i64 = 90;
const DAY: i64 = 86_400;
const USAGE: &str = "usage: asc migrate branchless [--prune-merged] [--json]";
const QUEUE_MAX_KEY: &str = "asc.cloud.queueMax";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub prune_merged: bool,
    pub json: bool,
}

/// Parses the words after `branchless`.
pub fn parse(words: &[String]) -> std::result::Result<Opts, String> {
    let mut o = Opts::default();
    for w in words {
        match w.as_str() {
            "--prune-merged" => o.prune_merged = true,
            "--json" => o.json = true,
            other => return Err(format!("error: unknown argument '{other}'\n{USAGE}")),
        }
    }
    Ok(o)
}

/// One local branch.
#[derive(Clone, Debug)]
struct Branch {
    name: String,
    full: String,
    target: RefTarget,
    tip: ChangesetId,
}

/// What a run found and did.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub merged: Vec<String>,
    pub stale: Vec<String>,
    pub protected: Vec<String>,
    pub pruned: Vec<String>,
    /// `(commit, change id)` of each head seeded.
    pub seeded: Vec<(ChangesetId, ChangeId)>,
    pub already_seeded: usize,
    pub seed_truncated: bool,
}

fn is_protected(name: &str) -> bool {
    name == "master" || name.starts_with("release/") || name.starts_with("green/")
}

fn queue_max(ctx: &Ctx) -> usize {
    ctx.config
        .get(QUEUE_MAX_KEY)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(drafts::QUEUE_MAX, |n| n.clamp(1, drafts::QUEUE_MAX))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The value of the first `change-id` header of a raw git commit.
fn jj_header(raw: &[u8]) -> Option<&[u8]> {
    let end = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .unwrap_or(raw.len());
    raw[..end]
        .split(|&c| c == b'\n')
        .find_map(|l| l.strip_prefix(b"change-id "))
}

/// The value of the last `Change-Id:` line of the last paragraph of a message (same bounds as
/// the server's reader, so both sides derive the same id).
fn gerrit_trailer(message: &[u8]) -> Option<&[u8]> {
    let end = message
        .iter()
        .rposition(|c| !matches!(c, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    let message = &message[..end];
    let start = message.windows(2).rposition(|w| w == b"\n\n")? + 2;
    message[start..]
        .split(|&c| c == b'\n')
        .rev()
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line.iter().position(|&c| c == b':')?;
            let (key, value) = line.split_at(colon);
            if !key.eq_ignore_ascii_case(b"Change-Id") {
                return None;
            }
            let value = value[1..].trim_ascii();
            (!value.is_empty() && value.len() <= 128 && value.iter().all(|c| c.is_ascii_graphic()))
                .then_some(value)
        })
}

async fn change_of(ctx: &Ctx, ws: &WsCtx, cs: ChangesetId, scope: &str) -> Result<ChangeId> {
    let rows = crate::stack::change_rows(ctx)?;
    if let Some(row) = rows.get(cs).await.map_err(fatal)? {
        return Ok(row.change);
    }
    let c = ws.engine.get_changeset(cs).await.map_err(fatal)?;
    let raw = c.extra.get(&EXTRA_GIT_COMMIT.to_vec());
    let jj = raw.and_then(|r| jj_header(r));
    let gerrit = gerrit_trailer(&c.message);
    migration_id(&cs, jj, gerrit, scope.as_bytes())
        .ok_or_else(|| CliError::Fatal("asc: migrate branchless: no change id".into()))
}

async fn tip_of(ws: &WsCtx, full: &str) -> Result<Option<ChangesetId>> {
    match crate::cli::switch::ref_target(ws, full).await? {
        Some(t) => Ok(Some(peel(ws, t).await?)),
        None => Ok(None),
    }
}

async fn local_branches(ws: &WsCtx) -> Result<Vec<Branch>> {
    let entries = ws
        .db
        .refs()
        .list(ws.repo.key, "refs/heads/")
        .await
        .map_err(fatal)?;
    if entries.len() > MAX_BRANCHES {
        return Err(CliError::Fatal(format!(
            "asc: migrate branchless: more than {MAX_BRANCHES} local branches"
        )));
    }
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let full = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
        let Some(name) = full.strip_prefix("refs/heads/").map(str::to_owned) else {
            continue;
        };
        let tip = peel(ws, e.target).await?;
        out.push(Branch {
            name,
            full,
            target: e.target,
            tip,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn reaches(ws: &WsCtx, a: ChangesetId, to: &[ChangesetId]) -> Result<bool> {
    for b in to {
        if HistoryApi::is_ancestor(&ws.engine, a, *b)
            .await
            .map_err(fatal)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Classifies, seeds and optionally prunes.
pub async fn migrate(ctx: &Ctx, opts: Opts) -> Result<Report> {
    let ws = ctx.ws()?;
    let mut trunk = Vec::new();
    for full in ["refs/heads/master", "refs/remotes/origin/master"] {
        if let Some(t) = tip_of(ws, full).await? {
            trunk.push(t);
        }
    }
    if trunk.is_empty() {
        return Err(CliError::Fatal(
            "asc: migrate branchless: there is no master branch; nothing was changed".into(),
        ));
    }
    let branches = local_branches(ws).await?;
    let current = match ws.projection.read_head()? {
        Head::Branch(b) => Some(b),
        Head::Detached(_) => None,
    };
    let cutoff = now() - STALE_DAYS * DAY;
    let mut report = Report::default();
    let mut prune: Vec<&Branch> = Vec::new();
    let mut draft_tips: Vec<&Branch> = Vec::new();
    for b in &branches {
        if is_protected(&b.name) {
            report.protected.push(b.name.clone());
            continue;
        }
        if reaches(ws, b.tip, &trunk).await? {
            report.merged.push(b.name.clone());
            let held = current.as_deref() == Some(b.name.as_str())
                || held_anywhere(ws, &b.name).await?.is_some();
            if held {
                report.protected.push(b.name.clone());
            } else {
                prune.push(b);
            }
            continue;
        }
        let cs = ws.engine.get_changeset(b.tip).await.map_err(fatal)?;
        if cs.committer.time() < cutoff {
            report.stale.push(b.name.clone());
        }
        draft_tips.push(b);
    }

    // Seed first: a failure here leaves every branch in place.
    seed(ctx, ws, &draft_tips, &mut report).await?;

    if opts.prune_merged && !prune.is_empty() {
        if prune.len() > MAX_PRUNE {
            return Err(CliError::Fatal(format!(
                "asc: migrate branchless: more than {MAX_PRUNE} merged branches; nothing was deleted"
            )));
        }
        // `seed` can take long: look again, so a branch a worktree took meanwhile is kept.
        let current = match ws.projection.read_head()? {
            Head::Branch(b) => Some(b),
            Head::Detached(_) => None,
        };
        let mut keep: Vec<&Branch> = Vec::with_capacity(prune.len());
        for b in prune {
            let held = current.as_deref() == Some(b.name.as_str())
                || held_anywhere(ws, &b.name).await?.is_some();
            if held {
                report.protected.push(b.name.clone());
            } else {
                keep.push(b);
            }
        }
        let prune = keep;
        if prune.is_empty() {
            return Ok(report);
        }
        let mut updates = Vec::with_capacity(prune.len());
        for b in &prune {
            updates.push(RefUpdate {
                name: ref_name(&b.full)?,
                expect: Expect::Value(b.target),
                new: None,
                expires_at: None,
            });
        }
        let txn = RefTxn {
            updates,
            actor: actor(),
            reason: String::new(),
            op_id: ext::OpId::new().0,
        };
        ctx.update_local_refs(
            txn,
            "migrate branchless: pruned merged",
            StagingAfterMove::Keep,
        )
        .await?;
        report.pruned = prune.iter().map(|b| b.name.clone()).collect();
    }
    Ok(report)
}

/// Refuses a run whose distinct draft tips cannot all be seeded: finding the heads compares
/// every pair of tips, so the count is capped before that work starts. Nothing has changed yet.
fn check_tip_count(n: usize) -> Result<()> {
    if n > MAX_SEEDS {
        return Err(CliError::Fatal(format!(
            "asc: migrate branchless: more than {MAX_SEEDS} unmerged branch tips; nothing was changed"
        )));
    }
    Ok(())
}

/// Whether `draft_heads` (visible or hidden) already has a row for `cs`.
async fn has_head(ws: &WsCtx, cs: ChangesetId) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM draft_heads WHERE repo_id = ? AND cs = ?")
        .bind(ws.repo.key.0)
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(ws.db.pool())
        .await
        .map_err(fatal)?;
    Ok(row.is_some())
}

/// [`has_head`] inside the seeding transaction.
async fn has_head_tx(
    tx: &mut asc_state::state::Tx,
    repo: asc_store::refs::RepoKey,
    cs: ChangesetId,
) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM draft_heads WHERE repo_id = ? AND cs = ?")
        .bind(repo.0)
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(&mut **tx)
        .await
        .map_err(fatal)?;
    Ok(row.is_some())
}

async fn seed(ctx: &Ctx, ws: &WsCtx, tips: &[&Branch], report: &mut Report) -> Result<()> {
    // The first branch (by name) that names a tip scopes its change id.
    let mut scope: HashMap<ChangesetId, &Branch> = HashMap::new();
    for b in tips {
        scope.entry(b.tip).or_insert(b);
    }
    check_tip_count(scope.len())?;
    let mut distinct: Vec<ChangesetId> = scope.keys().copied().collect();
    distinct.sort();
    // Heads: tips no other tip contains.
    let mut heads = Vec::new();
    for &t in &distinct {
        let mut covered = false;
        for &o in &distinct {
            if o != t
                && HistoryApi::is_ancestor(&ws.engine, t, o)
                    .await
                    .map_err(fatal)?
            {
                covered = true;
                break;
            }
        }
        if !covered {
            heads.push(t);
        }
    }
    let d = Drafts::new(ws.repo.key);
    // Candidates: heads with no row at read time. The pool read only saves the id derivation;
    // the transaction below decides again, so a row written meanwhile (or beyond any list cap)
    // is never queued a second time.
    let mut todo = Vec::new();
    for h in heads {
        if has_head(ws, h).await? {
            report.already_seeded += 1;
        } else if todo.len() >= MAX_SEEDS {
            report.seed_truncated = true;
        } else {
            let b = scope[&h];
            todo.push((h, change_of(ctx, ws, h, &b.full).await?));
        }
    }
    let qmax = queue_max(ctx);
    let at = now();
    let mut tx = ws.db.begin().await.map_err(fatal)?;
    let mut seeded = Vec::with_capacity(todo.len());
    for (cs, change) in todo {
        if has_head_tx(&mut tx, ws.repo.key, cs).await? {
            report.already_seeded += 1;
            continue;
        }
        let parents = ws
            .engine
            .get_changeset(cs)
            .await
            .map_err(fatal)?
            .parents
            .clone();
        d.new_changeset(&mut tx, cs, change, &parents, at, qmax)
            .await
            .map_err(fatal)?;
        seeded.push((cs, change));
    }
    tx.commit().await.map_err(fatal)?;
    let todo = seeded;
    report.seeded = todo;
    Ok(())
}

/// The command: prints the report and returns the exit code (1 when the seed list was cut).
pub async fn run(ctx: &Ctx, opts: Opts) -> Result<ExitCode> {
    let r = migrate(ctx, opts).await?;
    if opts.json {
        let v = serde_json::json!({
            "version": 1,
            "merged": r.merged,
            "stale": r.stale,
            "protected": r.protected,
            "pruned": r.pruned,
            "seeded": r.seeded.iter().map(|(c, ch)| serde_json::json!({
                "changeset": c.to_string(),
                "change": ch.to_string(),
            })).collect::<Vec<_>>(),
            "already_seeded": r.already_seeded,
            "seed_truncated": r.seed_truncated,
        });
        println!("{v}");
    } else {
        for b in &r.merged {
            let verb = if r.pruned.contains(b) {
                "pruned"
            } else {
                "merged"
            };
            println!("{verb} {b}");
        }
        for b in &r.stale {
            println!("stale {b}");
        }
        println!(
            "seeded {} draft heads ({} already present)",
            r.seeded.len(),
            r.already_seeded
        );
        if r.seed_truncated {
            eprintln!("warning: asc: more than {MAX_SEEDS} draft heads; run again for the rest");
        }
    }
    Ok(if r.seed_truncated {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn tip_count_over_the_seed_cap_is_refused_before_any_graph_work() {
        assert!(check_tip_count(MAX_SEEDS).is_ok());
        assert!(check_tip_count(MAX_SEEDS + 1).is_err());
    }

    #[test]
    fn parses_flags_and_refuses_unknown() {
        assert_eq!(parse(&w(&[])).unwrap(), Opts::default());
        let o = parse(&w(&["--prune-merged", "--json"])).unwrap();
        assert!(o.prune_merged && o.json);
        assert!(parse(&w(&["--force"])).is_err());
    }

    #[test]
    fn protected_names() {
        for n in ["master", "release/1", "green/x"] {
            assert!(is_protected(n));
        }
        for n in ["main", "feature/release/x", "masterx"] {
            assert!(!is_protected(n));
        }
    }

    #[test]
    fn trailer_and_header_readers() {
        assert_eq!(
            gerrit_trailer(b"s\n\nbody\n\nChange-Id: Iab\n"),
            Some(&b"Iab"[..])
        );
        assert_eq!(gerrit_trailer(b"Change-Id: Iab\n"), None);
        assert_eq!(
            jj_header(b"tree x\nchange-id abc\n\nmsg"),
            Some(&b"abc"[..])
        );
    }
}
