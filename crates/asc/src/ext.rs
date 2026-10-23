//! Extension points (PLAN "Stubs and extension points"). Each trait has a no-op or local
//! default and is selected in `Ctx::open` from a registry of `OnceLock<Box<dyn …>>` setters, so
//! a later unit registers its implementation with one line in `main.rs`.
#![allow(dead_code)] // The extension points are used by later units.

use std::collections::HashMap;
use std::ffi::OsString;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::change::RewriteOp;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::{RefName, RepoPath};
use asc_engine::blame::BlameApi;
use asc_engine::history::HistoryApi;
use asc_state::WorkspaceId;
use asc_state::state::repos::RemoteKind;
use asc_state::state::workspaces::{Mode, Workspace};
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{Objects, Result as ObjResult};
use asc_store::refs::RepoKey;
use futures::stream::{self, BoxStream, StreamExt};
use uuid::Uuid;

/// What an extension point returns when it fails.
pub type SinkError = Box<dyn std::error::Error + Send + Sync>;
/// Result of a sink call.
pub type SinkResult<T = ()> = Result<T, SinkError>;

/// One operation of the op log: the newtype of `RefTxn.op_id` (SPEC §20.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub Uuid);

impl OpId {
    /// A fresh operation id.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for OpId {
    fn default() -> Self {
        Self::new()
    }
}

/// Keeps the synthetic `.git/index` in step with HEAD and stage set 0 (SPEC §9.4; U36).
#[async_trait]
pub trait SyntheticIndexSink: Send + Sync {
    /// A commit was written and HEAD now names `head`.
    async fn on_commit(&self, _ws: WorkspaceId, _head: ChangesetId) -> SinkResult {
        Ok(())
    }

    /// HEAD's commit changed from `old` to `new`.
    async fn on_head_moved(
        &self,
        _ws: WorkspaceId,
        _old: Option<ChangesetId>,
        _new: Option<ChangesetId>,
    ) -> SinkResult {
        Ok(())
    }

    /// Stage set 0 changed at `paths` (called after every change; SPEC §9.4).
    async fn on_stage_changed(&self, _ws: WorkspaceId, _paths: &[RepoPath]) -> SinkResult {
        Ok(())
    }
}

/// Where history and blame come from when the default remote is a server (U50).
pub trait HistorySelector: Send + Sync {
    /// `None` means the local engine serves both.
    fn select(&self, _remote: &RemoteSpec) -> Option<(Arc<dyn HistoryApi>, Arc<dyn BlameApi>)> {
        None
    }
}

/// The project-graph API. G07 moves it to `asc_engine::projects`; the marker keeps the selector's
/// shape until then.
pub trait ProjectsApi: Send + Sync {}

/// Where the project graph comes from (G10).
pub trait ProjectsSelector: Send + Sync {
    /// `None` means the local engine serves it.
    fn select(&self, _remote: &RemoteSpec) -> Option<Arc<dyn ProjectsApi>> {
        None
    }
}

/// One ref change in a [`RefTxnView`], with the values before and after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefChange {
    pub name: RefName,
    pub old: Option<RefTarget>,
    pub new: Option<RefTarget>,
}

/// What a `reference-transaction` hook sees.
#[derive(Clone, Debug)]
pub struct RefTxnView {
    pub repo: RepoKey,
    pub workspace: WorkspaceId,
    pub op: OpId,
    pub changes: Vec<RefChange>,
}

/// A hook refused the transaction.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HookRejected(pub String);

/// Runs `reference-transaction prepared|committed|aborted` around every local ref update
/// (SPEC §9.4.1; U22).
#[async_trait]
pub trait RefTxnHook: Send + Sync {
    /// Before the ref store changes; an `Err` aborts the update.
    async fn prepared(&self, _tx: &RefTxnView) -> Result<(), HookRejected> {
        Ok(())
    }
    /// After the ref store changed.
    async fn committed(&self, _tx: &RefTxnView) {}
    /// After a failed or refused update.
    async fn aborted(&self, _tx: &RefTxnView) {}
}

/// Creates and converts workspaces (U53a).
#[async_trait]
pub trait WorkspaceFactory: Send + Sync {
    /// The mode of a new plain workspace.
    fn plain(&self) -> Mode {
        Mode::Plain
    }

    /// The mode of a new virtual workspace; `None` where virtual mode is unavailable.
    fn virtual_mode(&self) -> Option<Mode> {
        None
    }

    /// `worktree convert --plain`: a no-op for plain workspaces, unsupported for virtual ones
    /// until the daemon lands.
    async fn convert_plain(&self, ws: &Workspace) -> SinkResult {
        match ws.ws.mode {
            Mode::Plain => Ok(()),
            Mode::Virtual => Err("asc: 'worktree convert' is not supported yet".into()),
        }
    }
}

/// Records that `new` rewrites `preds` (SPEC §20.3). Called by the command that writes the
/// changeset; C04 replaces the no-op default.
pub trait RewriteSink: Send + Sync {
    fn record(&self, _new: ChangesetId, _preds: &[ChangesetId], _kind: RewriteOp, _op: OpId) {}
}

/// Brackets one CLI invocation in the op log and snapshots the working copy before a
/// destructive path (SPEC §20.3). C04 replaces the no-op default.
pub trait OpSink: Send + Sync {
    fn begin(&self, _argv: &[OsString]) -> OpId {
        OpId::new()
    }
    fn end(&self, _op: OpId) {}
    fn snapshot_wc(&self) {}
}

/// The default remote as the selectors see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSpec {
    pub kind: RemoteKind,
    pub url: Option<String>,
}

/// The remote tier of the object stack (SPEC §3.5), by remote kind.
pub trait RemoteObjectsSelector: Send + Sync {
    /// `None` when this kind has no object source.
    fn objects(&self, spec: &RemoteSpec) -> Option<Arc<dyn Objects>>;
}

/// An object source that always misses: the remote tier of local-only and git-remote modes.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRemote;

#[async_trait]
impl Objects for NoRemote {
    async fn get(&self, _key: &ObjKey) -> ObjResult<Option<Bytes>> {
        Ok(None)
    }
    async fn get_stored(&self, _key: &ObjKey) -> ObjResult<Option<Bytes>> {
        Ok(None)
    }
    async fn get_range(&self, _key: &ObjKey, _r: Range<u64>) -> ObjResult<Option<Bytes>> {
        Ok(None)
    }
    async fn put(&self, _key: &ObjKey, _stored: Bytes) -> ObjResult<()> {
        Ok(())
    }
    async fn has_many(&self, keys: &[ObjKey]) -> ObjResult<Vec<bool>> {
        Ok(vec![false; keys.len()])
    }
    async fn delete(&self, _key: &ObjKey) -> ObjResult<()> {
        Ok(())
    }
    fn list(&self, _prefix: KeyPrefix) -> BoxStream<'_, ObjResult<(ObjKey, SystemTime)>> {
        stream::empty().boxed()
    }
}

/// The no-op default of every sink.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSink;

impl SyntheticIndexSink for NoSink {}
impl HistorySelector for NoSink {}
impl ProjectsSelector for NoSink {}
impl RefTxnHook for NoSink {}
impl WorkspaceFactory for NoSink {}
impl RewriteSink for NoSink {}
impl OpSink for NoSink {}

/// Defines the `OnceLock` slot, its setter and its getter for one extension point.
macro_rules! slot {
    ($slot:ident, $set:ident, $get:ident, $tr:ident) => {
        static $slot: OnceLock<Box<dyn $tr>> = OnceLock::new();

        /// Registers the implementation; fails if one is already registered.
        pub fn $set(v: Box<dyn $tr>) -> Result<(), Box<dyn $tr>> {
            $slot.set(v)
        }

        /// The registered implementation, else the no-op default.
        pub fn $get() -> &'static dyn $tr {
            match $slot.get() {
                Some(v) => v.as_ref(),
                None => &NoSink,
            }
        }
    };
}

slot!(INDEX_SINK, set_index_sink, index_sink, SyntheticIndexSink);
slot!(
    HISTORY,
    set_history_selector,
    history_selector,
    HistorySelector
);
slot!(
    PROJECTS,
    set_projects_selector,
    projects_selector,
    ProjectsSelector
);
slot!(REF_HOOK, set_ref_txn_hook, ref_txn_hook, RefTxnHook);
slot!(
    FACTORY,
    set_workspace_factory,
    workspace_factory,
    WorkspaceFactory
);
slot!(REWRITES, set_rewrite_sink, rewrite_sink, RewriteSink);
slot!(OPS, set_op_sink, op_sink, OpSink);

type RemoteRegistry = Mutex<HashMap<&'static str, Arc<dyn RemoteObjectsSelector>>>;

fn remotes() -> &'static RemoteRegistry {
    static REMOTES: OnceLock<RemoteRegistry> = OnceLock::new();
    REMOTES.get_or_init(Default::default)
}

/// Registers the object source of `kind` (bucket: U32; server: U50). A second registration for
/// the same kind replaces the first.
pub fn register_remote_objects(kind: RemoteKind, selector: Arc<dyn RemoteObjectsSelector>) {
    if let Ok(mut map) = remotes().lock() {
        map.insert(kind.as_str(), selector);
    }
}

/// The remote tier for `spec`: the registered source, else `None` (local-only and git-remote
/// modes, and kinds nobody registered).
pub fn remote_objects(spec: &RemoteSpec) -> Option<Arc<dyn Objects>> {
    let selector = remotes().lock().ok()?.get(spec.kind.as_str())?.clone();
    selector.objects(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_no_ops() {
        let op = op_sink().begin(&[OsString::from("status")]);
        op_sink().end(op);
        op_sink().snapshot_wc();
        rewrite_sink().record(ChangesetId::from_bytes([1; 32]), &[], RewriteOp::New, op);
        assert!(
            history_selector()
                .select(&RemoteSpec {
                    kind: RemoteKind::LocalOnly,
                    url: None
                })
                .is_none()
        );
    }

    #[tokio::test]
    async fn no_remote_always_misses() {
        let key = ObjKey::File(asc_core::id::FileId::of(b"x"));
        let r = NoRemote;
        assert!(r.get(&key).await.unwrap().is_none());
        assert_eq!(r.has_many(&[key]).await.unwrap(), [false]);
        r.put(&key, Bytes::new()).await.unwrap();
    }

    struct Fixed;
    impl RemoteObjectsSelector for Fixed {
        fn objects(&self, _: &RemoteSpec) -> Option<Arc<dyn Objects>> {
            Some(Arc::new(NoRemote))
        }
    }

    #[test]
    fn remote_registry_is_keyed_by_kind() {
        register_remote_objects(RemoteKind::Bucket, Arc::new(Fixed));
        let spec = |kind| RemoteSpec { kind, url: None };
        assert!(remote_objects(&spec(RemoteKind::Bucket)).is_some());
        assert!(remote_objects(&spec(RemoteKind::LocalOnly)).is_none());
    }
}
