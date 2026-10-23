//! The `RewriteSink` and `OpSink` implementations (SPEC §20.3; C04). They are invisible: they
//! print nothing and never fail a command. The commands that write changesets or destroy the
//! working copy make the calls (`ext::rewrite_sink().record`, `ext::op_sink().snapshot_wc`);
//! `dispatch` brackets each invocation with `begin` and `end` and binds the invocation's
//! context with [`bind`], because the sink traits carry no workspace.
//!
//! - `record` indexes the changeset in `changes`/`change_preds` through C02's rules table.
//! - `begin` opens an op: its id is also the `op_id` of every `ref_log` row the command writes
//!   (`ext::OpId::new` returns it while the op is open).
//! - `snapshot_wc` saves the dirty tracked files and the staging rows as a stash-shaped
//!   changeset in `store/` before a destructive path, once per op (the earliest state wins).
//! - `end` writes one `op_log` row when the command moved a ref, took a snapshot or ran
//!   `clean`; read-only commands leave no row, so they cannot push real ops out of the bounded
//!   log. `asc.opLog=false` stops the rows and the snapshots, not the change index.

use std::ffi::OsString;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use asc_core::change::RewriteOp;
use asc_core::id::ChangesetId;
use asc_engine::change::{self, Recorder, RewriteSink as _};
use asc_state::oplog::{self, Limits, NewOp};
use tokio::runtime::{Handle, RuntimeFlavor};

use crate::cli::common::{Ctx, Result};
use crate::cli::restore::{index_tree, root_of};
use crate::cli::stash::save::{self, Opts};
use crate::ext::{self, OpId, OpSink, RewriteSink};

/// Most arguments kept for an op's `argv`.
const MAX_ARGS: usize = 256;

/// The sink implementations; register with `ext::set_rewrite_sink` and `ext::set_op_sink`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sinks;

/// The op `begin` opened.
struct Active {
    id: OpId,
    argv: Vec<String>,
    head_before: Option<ChangesetId>,
    /// The newest `ref_log` row when the op began; later rows belong to this op.
    start_seq: i64,
    snapshot: Option<ChangesetId>,
}

#[derive(Default)]
struct State {
    ctx: Option<Arc<Ctx>>,
    op: Option<Active>,
}

static STATE: Mutex<State> = Mutex::new(State {
    ctx: None,
    op: None,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Binds the context of the running invocation. The last binding wins.
pub fn bind(ctx: Arc<Ctx>) {
    let mut s = state();
    s.ctx = Some(ctx);
    s.op = None;
}

/// Drops the binding and any op still open.
pub fn unbind() {
    let mut s = state();
    s.ctx = None;
    s.op = None;
    drop(s);
    ext::set_current_op(None);
}

fn bound() -> Option<Arc<Ctx>> {
    state().ctx.clone()
}

/// Whether the op log is on (`asc.opLog`, default true).
fn op_log_enabled(ctx: &Ctx) -> bool {
    ctx.config.bool("asc.opLog").unwrap_or(true)
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

/// Runs the future `make` builds to completion from synchronous code. The future need not be
/// `Send`: `make` runs on the thread that drives it.
fn run_sync<M, F, T>(make: M) -> T
where
    M: FnOnce() -> F + Send,
    F: Future<Output = T>,
    T: Send,
{
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| h.block_on(make()))
        }
        _ => {
            // No runtime, or a single-threaded one that must not be blocked: use a private one.
            let run = move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| rt.block_on(make()))
            };
            let out = std::thread::scope(|s| {
                s.spawn(run)
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e))
            });
            match out {
                Ok(v) => v,
                Err(e) => panic!("asc: cannot start a runtime for the op log: {e}"),
            }
        }
    }
}

/// The arguments an op keeps in memory until `oplog::record` stores them.
///
/// Arguments are not cut by length here: `oplog::record` redacts credentials and only then
/// bounds each argument, so a clipped secret (`https://user:tok`) can never lose the `@`
/// that marks it and leak a prefix.
fn clip(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .take(MAX_ARGS)
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// `git clean` without `-n`: removes untracked files that no snapshot can capture, but `asc
/// undo` must still find the op to print the gap hint (SPEC §20.3).
fn is_destructive_clean(argv: &[String]) -> bool {
    argv.first().map(String::as_str) == Some("clean")
        && !argv.iter().skip(1).any(|a| {
            a == "--dry-run" || (!a.starts_with("--") && a.starts_with('-') && a.contains('n'))
        })
}

async fn head_of(ctx: &Ctx) -> Option<ChangesetId> {
    ctx.ws().ok()?.head_commit().await.ok().flatten()
}

async fn ref_log_seq(ctx: &Ctx) -> i64 {
    let Ok(ws) = ctx.ws() else { return 0 };
    sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(seq) FROM ref_log WHERE repo_id = ?")
        .bind(ws.repo.key.0)
        .fetch_one(ws.db.pool())
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Whether the command wrote `ref_log` rows. Commands that mint their own transaction id are
/// folded into this op, so `asc undo` finds every delta of the command under one `op_id`.
async fn adopt_ref_log(ctx: &Ctx, active: &Active) -> bool {
    let Ok(ws) = ctx.ws() else { return false };
    let id = active.id.0.as_bytes().to_vec();
    let wrote = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM ref_log WHERE repo_id = ? AND seq > ? AND op_id <> ?",
    )
    .bind(ws.repo.key.0)
    .bind(active.start_seq)
    .bind(id.clone())
    .fetch_one(ws.db.pool())
    .await
    .unwrap_or(0);
    if wrote == 0 {
        // Nothing foreign: either nothing moved or the command used this op's id.
        return sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM ref_log WHERE repo_id = ? AND seq > ?",
        )
        .bind(ws.repo.key.0)
        .bind(active.start_seq)
        .fetch_one(ws.db.pool())
        .await
        .is_ok_and(|n| n > 0);
    }
    let _ =
        sqlx::query("UPDATE ref_log SET op_id = ? WHERE repo_id = ? AND seq > ? AND op_id <> ?")
            .bind(id.clone())
            .bind(ws.repo.key.0)
            .bind(active.start_seq)
            .bind(id)
            .execute(ws.db.pool())
            .await;
    true
}

/// The snapshot of the dirty tracked files and the staging rows, or `None` when there is
/// nothing to save, no commit yet, or unresolved conflicts (documented gap).
async fn take_snapshot(ctx: &Ctx) -> Result<Option<ChangesetId>> {
    let ws = ctx.ws()?;
    let Some(head) = ws.head_commit().await? else {
        return Ok(None);
    };
    let root = root_of(ws, Some(head)).await?;
    let (_, unmerged) = index_tree(ws, root).await?;
    if !unmerged.is_empty() {
        return Ok(None);
    }
    Ok(save::make(ctx, ws, &Opts::default(), false)
        .await?
        .map(|made| made.commit))
}

async fn write_row(ctx: &Ctx, active: Active) {
    let Ok(ws) = ctx.ws() else { return };
    let moved = adopt_ref_log(ctx, &active).await;
    let recordable = moved || active.snapshot.is_some() || is_destructive_clean(&active.argv);
    if !recordable {
        return;
    }
    // The kind is the first argument; it is redacted like the rest of the line.
    let kind = oplog::redact_argv(&active.argv[..active.argv.len().min(1)]);
    let row = NewOp {
        op_id: oplog::OpId(active.id.0),
        ws: ws.workspace.ws.id,
        argv: active.argv,
        kind,
        head_before: active.head_before,
        head_after: head_of(ctx).await,
        wc_snapshot: active.snapshot,
        undoes: None,
    };
    // Invisible: a full or locked state.db must not fail a command that already succeeded.
    let _ = oplog::record(&ws.db, ws.repo.key, &row, limits(ctx)).await;
}

impl RewriteSink for Sinks {
    fn record(&self, new: ChangesetId, preds: &[ChangesetId], kind: RewriteOp, op: OpId) {
        let Some(ctx) = bound() else { return };
        let Ok(rows) = super::change_rows(&ctx) else {
            return;
        };
        let recorder = Recorder::new(rows);
        // A rule violation (no predecessor for an amend, too many predecessors) is the
        // caller's bug; the command itself has already succeeded.
        run_sync(|| async {
            if let Ok(meta) = recorder.record(new, preds, kind, change::OpId(op.0)).await {
                crate::branchless::heads_sink::record(&ctx, new, meta.change, preds, kind).await;
            }
        });
    }
}

impl OpSink for Sinks {
    fn begin(&self, argv: &[OsString]) -> OpId {
        let id = OpId::fresh();
        ext::set_current_op(Some(id));
        let Some(ctx) = bound() else { return id };
        if ctx.ws().is_err() || !op_log_enabled(&ctx) {
            return id;
        }
        let (head_before, start_seq) =
            run_sync(|| async { (head_of(&ctx).await, ref_log_seq(&ctx).await) });
        state().op = Some(Active {
            id,
            argv: clip(argv),
            head_before,
            start_seq,
            snapshot: None,
        });
        id
    }

    fn end(&self, op: OpId) {
        let (ctx, active) = {
            let mut s = state();
            let active = match s.op.take() {
                Some(a) if a.id == op => Some(a),
                other => {
                    s.op = other;
                    None
                }
            };
            (s.ctx.clone(), active)
        };
        ext::set_current_op(None);
        if let (Some(ctx), Some(active)) = (ctx, active) {
            run_sync(|| write_row(&ctx, active));
        }
    }

    fn snapshot_wc(&self) {
        let Some(ctx) = bound() else { return };
        // One snapshot per op: the first one holds the state before anything was destroyed.
        if !matches!(&state().op, Some(a) if a.snapshot.is_none()) {
            return;
        }
        let snap = run_sync(|| take_snapshot(&ctx)).ok().flatten();
        if let Some(a) = state().op.as_mut() {
            a.snapshot = snap;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `oplog::record` stores for `argv`: the op's arguments, then redaction.
    fn stored(argv: &[&str]) -> String {
        let os: Vec<OsString> = argv.iter().map(OsString::from).collect();
        oplog::redact_argv(&clip(&os))
    }

    #[test]
    fn credentials_in_urls_are_not_stored() {
        let line = stored(&["clone", "https://user:tok3n@host.test/x"]);
        assert_eq!(line, "clone https://***@host.test/x");
        let line = stored(&["push", "https://host.test/x?token=s3cret&ref=main"]);
        assert_eq!(line, "push 'https://host.test/x?token=***&ref=main'");
        assert!(!stored(&["fetch", "user:pw@host.test:path/x"]).contains("pw"));
    }

    #[test]
    fn config_secrets_are_not_stored() {
        let line = stored(&["-c", "http.extraHeader=Authorization: Bearer abc", "fetch"]);
        assert_eq!(line, "-c http.extraHeader=*** fetch");
        assert_eq!(
            stored(&["-chttp.x.extraheader=Bearer abc"]),
            "-chttp.x.extraheader=***"
        );
        assert_eq!(
            stored(&["--config-env=user.token=TOK", "fetch"]),
            "--config-env=user.token=*** fetch"
        );
        assert_eq!(stored(&["alias.token=abc", "x"]), "alias.token=*** x");
        assert_eq!(
            stored(&["credential.helper=store"]),
            "credential.helper=***"
        );
        let line = stored(&["url.https://u:p@h/.insteadOf=https://h/"]);
        assert!(!line.contains("u:p"), "{line}");
        assert_eq!(
            stored(&["login", "--token", "abc", "--password=p"]),
            "login --token --password"
        );
    }

    #[test]
    fn a_secret_at_the_clip_boundary_leaks_no_prefix() {
        for pad in 4070..4100 {
            let url = format!("https://{}:s3cretvalue@host.test/x", "u".repeat(pad - 8));
            let line = stored(&["clone", &url]);
            assert!(
                !line.contains("s3cret") && !line.contains("uuu"),
                "pad {pad}: {line}"
            );
            assert!(line.contains("***@host.test"), "pad {pad}: {line}");
        }
    }

    #[test]
    fn plain_arguments_are_unchanged() {
        assert_eq!(
            stored(&["commit", "-a", "-m", "fix: a=b", "--author=Jo <jo@x.test>"]),
            "commit -a -m 'fix: a=b' '--author=Jo <jo@x.test>'"
        );
        assert_eq!(
            stored(&["push", "origin", "https://host.test/x"]),
            "push origin https://host.test/x"
        );
        assert_eq!(
            stored(&["log", "--format=%H", "src/main.rs"]),
            "log --format=%H src/main.rs"
        );
    }
}
