//! What the two Windows backends share: ProjFS (SPEC §10.2) and Cloud Files (SPEC §10.2b).
//!
//! * The daemon's side of a mount: [`Config`], the placeholder times ([`TimeSource`], [`DbTimes`])
//!   and the journal sink ([`JournalSink`]).
//! * The path rules every notification applies first ([`paths`]).
//! * The switch engine ([`engine`]): the intent, the per-path work, retries of held-open paths and
//!   the pending list. Each backend supplies only how one path is brought to the target version.

use std::future::Future;
use std::io;
use std::sync::Arc;

use asc_core::path::RepoPath;
use asc_state::StateDb;
use asc_state::state::workspaces::Workspace;
use asc_store::cachedir::CacheDir;
use asc_store::objects::Objects;
use asc_wc::journal::{Journal, Op};
use asc_wc::redirect::Redirections;
use asc_wc::times::entry_times_of;
use tokio::runtime::Handle;

use crate::core::CoreFs;

pub mod engine;
pub mod paths;

/// The placeholder times of paths (SPEC §10.1).
pub trait TimeSource: Send + Sync {
    /// The time, in Unix seconds, of each path, in order.
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>>;
}

/// Where notifications append their change hints.
pub trait JournalSink: Send + Sync {
    /// Never blocks and never fails.
    fn record(&self, op: Op, path: &[u8]);
    /// Hints were lost (a change watcher overflowed): the next reader must reconcile.
    fn lost(&self) {}
}

impl JournalSink for Journal {
    fn record(&self, op: Op, path: &[u8]) {
        Journal::record(self, op, path);
    }

    fn lost(&self) {
        Journal::mark_lost(self);
    }
}

/// [`TimeSource`] over `state.db`'s `entry_times` (U54v `times`).
pub struct DbTimes {
    pub db: StateDb,
    pub ws: Workspace,
    pub rt: Handle,
}

impl TimeSource for DbTimes {
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
        block(&self.rt, entry_times_of(&self.db, &self.ws, paths))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// What a Windows provider needs from the daemon.
pub struct Config {
    /// The workspace uuid: the ProjFS instance GUID, the Cloud Files sync root identity, and the
    /// provider id of every placeholder.
    pub workspace: [u8; 16],
    pub core: Arc<CoreFs>,
    pub objects: Arc<dyn Objects>,
    /// The local content cache; content found there is served without leaving the callback.
    pub local: Option<CacheDir>,
    pub times: Arc<dyn TimeSource>,
    pub journal: Arc<dyn JournalSink>,
    pub redirections: Redirections,
    /// The runtime the callbacks run async work on.
    pub rt: Handle,
}

impl Config {
    /// Whether the content of `id` is in the local cache.
    pub fn is_local(&self, id: asc_core::id::FileId) -> bool {
        self.local
            .as_ref()
            .is_some_and(|l| l.content_path(id).is_some())
    }
}

/// Runs `f` to completion from a thread that may or may not be inside the runtime. Callback
/// threads of the OS are not; `DiskState` callers may be.
pub(crate) fn block<F>(rt: &Handle, f: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| rt.block_on(f))
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
        })
    } else {
        rt.block_on(f)
    }
}

/// Converts a root-relative backslash path (`""` for the root) to a [`RepoPath`]. `Ok(None)` is
/// the root; `Err` is a path that must be refused.
pub(crate) fn rel_path(w: &str) -> Result<Option<RepoPath>, ()> {
    if w.is_empty() {
        return Ok(None);
    }
    if w.contains('/') {
        return Err(());
    }
    RepoPath::new(w.replace('\\', "/").into_bytes())
        .map(Some)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_converts_and_refuses() {
        assert_eq!(rel_path(""), Ok(None));
        assert_eq!(
            rel_path("src\\lib.rs").unwrap().unwrap().as_bytes(),
            b"src/lib.rs"
        );
        for bad in [
            "a/b", "\\a", "a\\\\b", "a\\", "..\\x", "a\\..\\b", "a\\.\\b",
        ] {
            assert!(rel_path(bad).is_err(), "{bad}");
        }
    }
}
