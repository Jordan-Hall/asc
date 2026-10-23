//! The helper process `asc codeintel worker` (SPEC §23.2.5).
//!
//! It indexes the `HEAD` of its workspace into `$ASC_HOME/ci` and nothing else: no network (file
//! and tree objects come from `store/` and `cache/` only), no model, at most [`THREADS`] analysis
//! threads at low priority, a [`WRITER_HEAP`](super::WRITER_HEAP) tantivy heap, disk inside
//! `codeintel.local.maxSize`. It starts work only when the workspace is idle and exits
//! [`EXIT_AFTER`] after its last job (`ASC_CODEINTEL_EXIT_SECS` shortens that for tests).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use asc_core::id::ChangesetId;
use asc_core::object::Kind;
use asc_engine::Engine;
use futures::TryStreamExt;

use super::build::{self, Entry, Job};
use super::store::{CiRoot, MAX_PATH, MAX_REV_FILES};
use super::{DEFAULT_MAX_SIZE, EXIT_AFTER, IDLE_AFTER, POLL, THREADS, hex};
use crate::cli::common::{CliError, Ctx, ObjectStack, Result, WsCtx};
use crate::daemon::scheduler::parse_size;
use crate::refs_guard::ReadOnlyRefs;

/// The `worker` flags.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Index the current `HEAD` and exit.
    pub once: bool,
    /// Do not wait for the workspace to be idle.
    pub now: bool,
}

/// `codeintel.local.maxSize`.
pub fn max_size(ctx: &Ctx) -> Result<u64> {
    match ctx.config.get("codeintel.local.maxSize") {
        None => Ok(DEFAULT_MAX_SIZE),
        Some(text) => parse_size(&text).ok_or_else(|| {
            CliError::Fatal(format!(
                "bad numeric config value '{text}' for 'codeintel.local.maxSize'"
            ))
        }),
    }
}

fn exit_after() -> Duration {
    std::env::var("ASC_CODEINTEL_EXIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(EXIT_AFTER, Duration::from_secs)
}

/// Time since the newest of `paths` changed; `None` when none can be read.
pub fn quiet_for(paths: &[PathBuf]) -> Option<Duration> {
    let newest = paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()?;
    Some(
        SystemTime::now()
            .duration_since(newest)
            .unwrap_or(Duration::ZERO),
    )
}

/// The files whose change means the workspace is being worked on.
pub fn activity_files(common_dir: &Path) -> Vec<PathBuf> {
    ["HEAD", "index", "asc-index"]
        .iter()
        .map(|n| common_dir.join(n))
        .collect()
}

/// A workspace is idle when nothing it keeps changed for [`IDLE_AFTER`]. A workspace whose
/// files cannot be read counts as idle.
pub fn is_idle(common_dir: &Path) -> bool {
    quiet_for(&activity_files(common_dir)).is_none_or(|d| d >= IDLE_AFTER)
}

#[cfg(unix)]
fn lower_priority() {
    let _ = rustix::process::setpriority_process(None, 10);
}

#[cfg(not(unix))]
fn lower_priority() {
    // Windows: the starter creates the process in the idle priority class (`start`).
}

/// The tree of `head` as index entries, from local objects only.
async fn entries(
    ws: &WsCtx,
    objects: Arc<ObjectStack>,
    head: ChangesetId,
) -> Result<(Vec<Entry>, u64, Option<String>)> {
    let engine = Engine::new(objects, Arc::new(ReadOnlyRefs(ws.db.refs())), ws.repo.key);
    let root = engine
        .get_changeset(head)
        .await
        .map_err(|e| {
            CliError::Fatal(format!(
                "asc: codeintel: the commit is not on this machine: {e}"
            ))
        })?
        .root;
    let git = engine.git_commit_of(head).await.ok().map(|g| g.to_string());
    let mut out = Vec::new();
    let mut extra = 0u64;
    let mut walk = engine.walk(root, None);
    while let Some((path, entry)) = walk.try_next().await.map_err(|e| {
        CliError::Fatal(format!("asc: codeintel: a tree is not on this machine: {e}"))
    })? {
        if !matches!(entry.kind, Kind::File | Kind::Exec) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(path.as_bytes()) else {
            continue;
        };
        if text.len() > MAX_PATH || text.contains('\0') {
            continue;
        }
        if out.len() >= MAX_REV_FILES {
            extra += 1;
            continue;
        }
        out.push(Entry {
            path: text.to_owned(),
            id: asc_core::id::FileId::from_bytes(entry.id),
            size: entry.size,
        });
    }
    Ok((out, extra, git))
}

pub async fn run(ctx: &Ctx, opts: Options) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let cap = max_size(ctx)?;
    let common = ws.projection.git_dir().to_path_buf();
    let linger = exit_after();
    lower_priority();

    // Wait for an idle workspace; give up (exit 0) after the exit time.
    let started = Instant::now();
    if !opts.now {
        while !is_idle(&common) {
            if started.elapsed() >= linger {
                return Ok(ExitCode::SUCCESS);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    let ci = CiRoot::new(ws.home.root());
    let store = ci
        .repo(&ws.repo.repo.id.to_string())
        .map_err(|e| CliError::Fatal(format!("asc: codeintel: {e}")))?;
    let Some(lock) = store.lock() else {
        // Another helper serves this repository.
        return Ok(ExitCode::SUCCESS);
    };
    let _ = store.save_meta(&ws.repo.repo.name);
    // Local tiers only: no remote object source, so nothing is fetched.
    let objects = Arc::new(
        ObjectStack::build(&ws.home, &ctx.config, None)
            .map_err(|e| CliError::Fatal(format!("asc: codeintel: {e}")))?,
    );
    // A crashed run may have left scratch space behind.
    let _ = std::fs::remove_dir_all(store.work());
    let _ = super::store::ensure_dir(&store.work());

    let mut last: Option<ChangesetId> = None;
    let mut idle_since = Instant::now();
    loop {
        let head = ws.head_commit().await?;
        if let Some(head) = head
            && last != Some(head)
            && (opts.now || is_idle(&common))
        {
            let (list, extra, git) = entries(ws, Arc::clone(&objects), head).await?;
            let built = build::index_rev(Job {
                ci: &ci,
                store: &store,
                lock: Some(&lock),
                cap,
                changeset: *head.as_bytes(),
                git,
                entries: list,
                extra_missing: extra,
                objects: &*objects,
                threads: THREADS,
            })
            .await
            .map_err(|e| CliError::Fatal(format!("asc: codeintel: {e}")))?;
            println!(
                "indexed {} files={} skipped={} missing={} coverage={:.3} segments={}",
                hex(head.as_bytes()),
                built.rev.total,
                built.rev.skipped,
                built.rev.missing,
                built.rev.coverage(),
                built.segments
            );
            last = Some(head);
            idle_since = Instant::now();
        }
        if opts.once || idle_since.elapsed() >= linger {
            break;
        }
        lock.refresh();
        tokio::time::sleep(POLL).await;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recently_written_workspace_is_busy_and_an_old_one_idle() {
        let d = tempfile::tempdir().unwrap();
        assert!(is_idle(d.path()), "no files: nothing to wait for");
        let head = d.path().join("HEAD");
        std::fs::write(&head, b"ref: refs/heads/master\n").unwrap();
        assert!(!is_idle(d.path()));
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&head)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(is_idle(d.path()));
    }

    #[test]
    fn exit_time_defaults_to_sixty_seconds() {
        if std::env::var_os("ASC_CODEINTEL_EXIT_SECS").is_none() {
            assert_eq!(exit_after(), Duration::from_secs(60));
        }
    }
}
