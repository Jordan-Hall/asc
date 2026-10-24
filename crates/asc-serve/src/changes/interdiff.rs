//! `Changes.InterDiff` (PLAN C20, SPEC §20.5): the diff between two versions of one change.
//!
//! The shell authorises the call (`Read`) before this handler runs. The handler reads the two
//! versions from `change_versions` (indexed lookups), asks the engine for the interdiff
//! ([`asc_engine::interdiff`]: paths the change's own patches touch, so a rebase onto a moved
//! trunk shows only what the author changed) and streams the result in the `Diff` formats.
//!
//! Choices where SPEC is silent:
//!
//! * `to_v = 0` means the newest version; `from_v = 0` means the version before `to_v`. A change
//!   with one version has nothing to compare, so `from_v = 0` there is `FAILED_PRECONDITION`.
//!   A version number that does not exist is `NOT_FOUND`; `from_v == to_v` is an empty diff.
//! * Renames are not detected, context is 3 lines, and at most [`MAX_FILES`] files are returned
//!   (`truncated` is set when more differ), as for `Diff` with no limit set.
//! * Whole-file buffers go through the text budget; a version patch of more than
//!   [`asc_engine::interdiff::MAX_PATCH_PATHS`] paths is `RESOURCE_EXHAUSTED`.

use std::sync::Arc;

use bytes::Bytes;
use asc_core::change::ChangeId;
use asc_core::id::FileId;
use asc_core::object::{Entry, Kind};
use asc_engine::diff::{
    self, DiffCtx, DiffOptions, NoAttributes, TreeChange, format as diff_format,
};
use asc_engine::interdiff::{self, Side};
use asc_engine::tree::Pathspec;
use asc_rpc::IdMode;
use asc_rpc::server::{CallCtx, MAX_LIST, MAX_STRING, RpcStream, wire};
use futures::StreamExt;
use futures::stream;
use tokio::sync::mpsc;
use tonic::Status;

use super::versions;
use crate::commit_graph_rpc::{commit_status, data_loss, internal};
use crate::idmode::IdCodec;
use crate::read::ReadHooks;
use crate::registry::{Extensions, Registry, RegistryError};

/// Files one call reports.
pub const MAX_FILES: usize = 10_000;
/// `PathChange`s in one `DiffResponse`, and text bytes in one.
const CHANGES_CHUNK: usize = 1_000;
const TEXT_CHUNK: usize = 256 * 1024;
/// Messages queued ahead of a slow reader.
const STREAM_DEPTH: usize = 4;
/// Files above this size are binary by size, so their patch buffers nothing.
const BIG_FILE: u64 = 64 * 1024 * 1024;

/// The `InterDiff` handler behind the `Changes` service.
pub struct InterDiffer {
    registry: Arc<Registry>,
    hooks: Arc<ReadHooks>,
}

/// The unit's one line in `register_all` (through [`super::register`]).
pub fn register(ext: &Extensions, registry: &Arc<Registry>, hooks: &Arc<ReadHooks>) {
    let handler = Arc::new(InterDiffer {
        registry: registry.clone(),
        hooks: hooks.clone(),
    });
    let changes = ext
        .changes
        .get()
        .expect("the Changes handler is registered before InterDiff");
    assert!(
        changes.interdiff.set(handler).is_ok(),
        "InterDiff is registered once"
    );
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

fn tree_status(e: asc_engine::tree::Error) -> Status {
    use asc_engine::tree::Error as T;
    match e {
        T::MissingTree(_) | T::Codec(_) => data_loss("tree", &e),
        T::TooLarge(_) | T::OutputTooLarge(_) => Status::resource_exhausted(e.to_string()),
        T::PathTooLong(_) | T::TooDeep => Status::out_of_range(e.to_string()),
        e => internal("tree", &e),
    }
}

fn diff_status(e: diff::Error) -> Status {
    match e {
        diff::Error::Tree(e) => tree_status(e),
        diff::Error::MissingFile(_) | diff::Error::Codec(_) => data_loss("file", &e),
        diff::Error::FileTooLarge(..) => Status::resource_exhausted(e.to_string()),
        e => internal("diff", &e),
    }
}

fn interdiff_status(e: interdiff::Error) -> Status {
    match e {
        interdiff::Error::Diff(e) => diff_status(e),
        interdiff::Error::Commit(e) => commit_status(e),
        e @ interdiff::Error::TooLarge(_) => Status::resource_exhausted(e.to_string()),
    }
}

fn pathspec(items: &[String]) -> Result<Option<Pathspec>, Status> {
    if items.len() > MAX_LIST || items.iter().any(|p| p.len() > MAX_STRING) {
        return Err(Status::invalid_argument("too many or too long paths"));
    }
    if items.is_empty() {
        return Ok(None);
    }
    Pathspec::parse(items)
        .map(Some)
        .map_err(|e| Status::invalid_argument(e.to_string()))
}

/// Bytes of file content a patch of `c` holds at once.
fn read_cost(c: &TreeChange) -> u64 {
    let sizes: Vec<u64> = [&c.old, &c.new]
        .into_iter()
        .flatten()
        .filter(|e| e.kind != Kind::Gitlink)
        .map(|e| e.size)
        .collect();
    if sizes.iter().any(|&s| s > BIG_FILE) {
        0
    } else {
        sizes.iter().sum()
    }
}

fn path_change(mode: IdMode, c: &TreeChange) -> wire::PathChange {
    let side = |e: &Option<Entry>| match e {
        Some(e) if e.kind != Kind::Gitlink => {
            let id = match mode {
                IdMode::Asc => Bytes::copy_from_slice(FileId::from_bytes(e.id).as_bytes()),
                IdMode::Git => Bytes::copy_from_slice(e.git.as_bytes()),
            };
            (id, e.size)
        }
        _ => (Bytes::new(), 0),
    };
    let git = |e: &Option<Entry>| {
        e.as_ref()
            .map_or_else(Bytes::new, |e| Bytes::copy_from_slice(e.git.as_bytes()))
    };
    let (old_id, old_size) = side(&c.old);
    let (new_id, new_size) = side(&c.new);
    wire::PathChange {
        path: Bytes::copy_from_slice(c.path.as_bytes()),
        status: c.status.letter().to_string(),
        old_path: if c.old_path == c.path {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(c.old_path.as_bytes())
        },
        old_mode: c.old_mode(),
        new_mode: c.new_mode(),
        old_git: git(&c.old),
        new_git: git(&c.new),
        old_id,
        new_id,
        old_size,
        new_size,
        similarity: c.status.score().map_or(0, u32::from),
    }
}

impl InterDiffer {
    /// Version `v` of `change`, or `NOT_FOUND`.
    async fn version(
        conn: &mut sqlx::AnyConnection,
        ns: crate::registry::Ns,
        change: ChangeId,
        v: u32,
    ) -> Result<versions::Version, Status> {
        let list = versions::list(conn, ns, change, Some(v.saturating_sub(1)), 1)
            .await
            .map_err(|e| internal("versions", &e))?;
        match list.into_iter().next() {
            Some(found) if found.v == v => Ok(found),
            _ => Err(Status::not_found(format!("no version {v} of the change"))),
        }
    }

    pub async fn inter_diff(
        &self,
        ctx: &CallCtx,
        req: wire::InterDiffRequest,
    ) -> Result<RpcStream<wire::DiffResponse>, Status> {
        use wire::DiffFormat as F;
        let change = <[u8; 16]>::try_from(&req.change[..])
            .map(ChangeId::from_bytes)
            .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))?;
        let format =
            F::try_from(req.format).map_err(|_| Status::invalid_argument("unknown diff format"))?;
        let spec = pathspec(&req.paths)?;

        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry).for_viewer(&ctx.principal);
        let engine = entry.backend.engine().clone();
        let mut conn = self
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| internal("database", &e))?;

        let to_v = match req.to_v {
            0 => {
                versions::latest(&mut conn, entry.ns, change)
                    .await
                    .map_err(|e| internal("versions", &e))?
                    .ok_or_else(|| Status::not_found("unknown change"))?
                    .v
            }
            v => v,
        };
        let from_v = match req.from_v {
            0 if to_v > 1 => to_v - 1,
            0 => {
                return Err(Status::failed_precondition(
                    "the change has no earlier version to compare",
                ));
            }
            v => v,
        };
        let from = Self::version(&mut conn, entry.ns, change, from_v).await?;
        let to = Self::version(&mut conn, entry.ns, change, to_v).await?;
        drop(conn);

        let a: Side = engine
            .version_side(from.cs)
            .await
            .map_err(interdiff_status)?;
        let b: Side = engine.version_side(to.cs).await.map_err(interdiff_status)?;
        let opts = DiffOptions::default();
        let mut changes = engine
            .interdiff(a, b, spec.as_ref(), &opts)
            .await
            .map_err(interdiff_status)?;
        let truncated = changes.len() > MAX_FILES;
        changes.truncate(MAX_FILES);

        let hooks = self.hooks.clone();
        let mode = ids.mode();
        Ok(spawn_stream(move |out| async move {
            let wire_changes: Vec<wire::PathChange> =
                changes.iter().map(|c| path_change(mode, c)).collect();
            for chunk in wire_changes.chunks(CHANGES_CHUNK) {
                send(
                    &out,
                    wire::DiffResponse {
                        changes: chunk.to_vec(),
                        ..wire::DiffResponse::default()
                    },
                )
                .await?;
            }
            let dctx = DiffCtx {
                opts: &opts,
                content: &*engine,
                attrs: &NoAttributes,
            };
            let mut buf: Vec<u8> = Vec::new();
            let flush = |buf: &mut Vec<u8>| wire::DiffResponse {
                text: Bytes::from(std::mem::take(buf)),
                ..wire::DiffResponse::default()
            };
            let hooks = &hooks;
            let budget = |bytes: u64| async move {
                hooks.text_budget.acquire(bytes).await.map_err(|_| {
                    Status::resource_exhausted("text memory budget exhausted; retry shortly")
                })
            };
            match format {
                F::Names | F::Status | F::Raw => {
                    for chunk in changes.chunks(CHANGES_CHUNK) {
                        match format {
                            F::Names => diff_format::write_name_only(&mut buf, chunk, &opts),
                            F::Status => diff_format::write_name_status(&mut buf, chunk, &opts),
                            _ => diff_format::write_raw(&mut buf, chunk, &opts),
                        }
                        .map_err(diff_status)?;
                        send(&out, flush(&mut buf)).await?;
                    }
                }
                F::Patch | F::Numstat => {
                    for c in &changes {
                        let _guard = budget(read_cost(c)).await?;
                        if format == F::Patch {
                            diff::write_patch(&mut buf, &dctx, std::slice::from_ref(c)).await
                        } else {
                            diff::write_numstat(&mut buf, &dctx, std::slice::from_ref(c)).await
                        }
                        .map_err(diff_status)?;
                        if buf.len() >= TEXT_CHUNK {
                            send(&out, flush(&mut buf)).await?;
                        }
                    }
                }
                F::Stat => {
                    let _guard = budget(changes.iter().map(read_cost).max().unwrap_or(0)).await?;
                    diff::write_stat(&mut buf, &dctx, &changes)
                        .await
                        .map_err(diff_status)?;
                }
            }
            send(
                &out,
                wire::DiffResponse {
                    text: Bytes::from(buf),
                    truncated,
                    ..wire::DiffResponse::default()
                },
            )
            .await
        }))
    }
}

type Out = mpsc::Sender<Result<wire::DiffResponse, Status>>;

/// Sends one message; a dropped stream ends the producer with `CANCELLED`.
async fn send(out: &Out, m: wire::DiffResponse) -> Result<(), Status> {
    out.send(Ok(m))
        .await
        .map_err(|_| Status::cancelled("the caller went away"))
}

/// Runs `f` on its own task, feeding a bounded queue, so a slow reader makes the producer wait.
fn spawn_stream<F, Fut>(f: F) -> RpcStream<wire::DiffResponse>
where
    F: FnOnce(Out) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), Status>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(STREAM_DEPTH);
    let err = tx.clone();
    tokio::spawn(async move {
        if let Err(s) = f(tx).await {
            let _ = err.send(Err(s)).await;
        }
    });
    stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) }).boxed()
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use asc_core::change::{ChangeMeta, RewriteOp};
    use asc_core::id::ChangesetId;
    use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
    use asc_rpc::server::{ServerInfo, wire::RepoRef};
    use asc_rpc::services::changes_server::Changes;
    use futures::TryStreamExt;
    use tonic::Request;

    use super::*;
    use crate::changes::index::ChangeIndex;
    use crate::changes::rpc::ChangesService;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::{TextBudget, TextBudgetExhausted};

    const READER: &str = "reader-token";

    struct Fx {
        svc: ChangesService,
        hooks: Arc<ReadHooks>,
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    /// Change 1 has versions 1 (on trunk0), 2 (an amend on trunk0) and 3 (rebased onto trunk1,
    /// which changed `t`, with a further edit of `a`). Change 2 has one version.
    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        let t0 = commit(&e, &[], &[("a", b"a0\n"), ("t", b"t0\n")]).await;
        let v1 = commit(&e, &[t0], &[("a", b"mine1\n"), ("n", b"new\n")]).await;
        let v2 = commit(&e, &[t0], &[("a", b"mine2\n"), ("n", b"new\n")]).await;
        let t1 = commit(&e, &[t0], &[("t", b"t1\n")]).await;
        let v3 = commit(&e, &[t1], &[("a", b"mine3\n"), ("n", b"new\n")]).await;
        let other = commit(&e, &[t0], &[("o", b"o\n")]).await;
        let ext = Extensions::new();
        let hooks = Arc::new(ReadHooks::default());
        super::super::rpc::register(&ext, &reg);
        register(&ext, &reg, &hooks);
        let auth = StaticTokens::new().with_token(READER, "reader", Access::Read);
        let svc = ChangesService::new(
            ext.changes.get().unwrap().clone(),
            Arc::new(auth),
            ServerInfo::default(),
        );
        let idx = ChangeIndex::new(reg.clone());
        let mut tx = reg.refs().begin().await.unwrap();
        let publish: [(u8, ChangesetId, Vec<ChangesetId>, RewriteOp); 4] = [
            (1, v1, vec![], RewriteOp::New),
            (1, v2, vec![v1], RewriteOp::Amend),
            (1, v3, vec![v2], RewriteOp::Rebase),
            (2, other, vec![], RewriteOp::New),
        ];
        for (n, cs, preds, op) in publish {
            idx.write_meta(&mut tx, e.key, &ChangeMeta::new(cs, cid(n), preds, op))
                .await
                .unwrap();
            versions::append(
                &mut tx,
                e.ns,
                &versions::NewVersion {
                    change: cid(n),
                    cs,
                    parent_change: None,
                    at: 10,
                    actor: "alice",
                    op_id: b"",
                    force: false,
                },
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        Fx { svc, hooks }
    }

    fn req(change: u8, from_v: u32, to_v: u32, format: wire::DiffFormat) -> wire::InterDiffRequest {
        wire::InterDiffRequest {
            repo: Some(RepoRef {
                name: "o/r".into(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            change: Bytes::copy_from_slice(cid(change).as_bytes()),
            from_v,
            to_v,
            format: format as i32,
            paths: Vec::new(),
        }
    }

    fn authed<T>(m: T) -> Request<T> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(READER).unwrap()));
        r
    }

    async fn run(f: &Fx, m: wire::InterDiffRequest) -> Result<Vec<wire::DiffResponse>, Status> {
        let s = f.svc.inter_diff(authed(m)).await?.into_inner();
        s.try_collect().await
    }

    fn text(v: &[wire::DiffResponse]) -> String {
        let all: Vec<u8> = v.iter().flat_map(|r| r.text.to_vec()).collect();
        String::from_utf8(all).unwrap()
    }

    #[tokio::test]
    async fn an_amend_shows_only_what_the_author_changed() {
        let f = fx().await;
        let got = run(&f, req(1, 1, 2, wire::DiffFormat::Patch))
            .await
            .unwrap();
        let t = text(&got);
        assert!(t.contains("-mine1") && t.contains("+mine2"), "{t}");
        let changed: Vec<_> = got.iter().flat_map(|r| &r.changes).collect();
        assert_eq!(changed.len(), 1);
        assert_eq!(&changed[0].path[..], b"a");
    }

    #[tokio::test]
    async fn a_rebase_onto_a_moved_trunk_hides_the_trunk_change() {
        let f = fx().await;
        let got = run(&f, req(1, 2, 3, wire::DiffFormat::Names))
            .await
            .unwrap();
        assert_eq!(text(&got), "a\n");
        assert!(got.iter().all(|r| !r.truncated));
    }

    #[tokio::test]
    async fn versions_default_to_the_last_two() {
        let f = fx().await;
        let got = run(&f, req(1, 0, 0, wire::DiffFormat::Names))
            .await
            .unwrap();
        assert_eq!(text(&got), "a\n");
        let got = run(&f, req(1, 0, 2, wire::DiffFormat::Names))
            .await
            .unwrap();
        assert_eq!(text(&got), "a\n");
        let got = run(&f, req(1, 2, 2, wire::DiffFormat::Names))
            .await
            .unwrap();
        assert_eq!(text(&got), "");
    }

    #[tokio::test]
    async fn bad_versions_and_changes_are_refused() {
        let f = fx().await;
        let e = run(&f, req(2, 0, 0, wire::DiffFormat::Names))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        let e = run(&f, req(1, 1, 9, wire::DiffFormat::Names))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let e = run(&f, req(1, 9, 3, wire::DiffFormat::Names))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let e = run(&f, req(7, 0, 0, wire::DiffFormat::Names))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        let mut bad = req(1, 1, 2, wire::DiffFormat::Names);
        bad.change = Bytes::from_static(b"short");
        let e = run(&f, bad).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_version_of_another_change_is_not_reachable() {
        let f = fx().await;
        // Change 2 has only version 1; versions 2 and 3 belong to change 1.
        let e = run(&f, req(2, 1, 2, wire::DiffFormat::Names))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn the_call_is_authorised() {
        let f = fx().await;
        let e = f
            .svc
            .inter_diff(Request::new(req(1, 1, 2, wire::DiffFormat::Names)))
            .await
            .err()
            .unwrap();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
        let mut r = Request::new(req(1, 1, 2, wire::DiffFormat::Names));
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new("wrong-token").unwrap()));
        let e = f.svc.inter_diff(r).await.err().unwrap();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn the_path_filter_narrows_the_result() {
        let f = fx().await;
        let mut m = req(1, 1, 2, wire::DiffFormat::Names);
        m.paths = vec!["n".into()];
        assert_eq!(text(&run(&f, m).await.unwrap()), "");
        let mut m = req(1, 1, 2, wire::DiffFormat::Names);
        m.paths = vec!["a".into()];
        assert_eq!(text(&run(&f, m).await.unwrap()), "a\n");
        let mut m = req(1, 1, 2, wire::DiffFormat::Names);
        m.paths = vec!["a".into(); MAX_LIST + 1];
        assert_eq!(
            run(&f, m).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn patch_text_goes_through_the_text_budget() {
        struct Never;
        #[async_trait]
        impl TextBudget for Never {
            async fn acquire(&self, _: u64) -> Result<crate::TextGuard, TextBudgetExhausted> {
                Err(TextBudgetExhausted)
            }
        }
        let f = fx().await;
        f.hooks.text_budget.set(Arc::new(Never)).ok().unwrap();
        let e = run(&f, req(1, 1, 2, wire::DiffFormat::Patch))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::ResourceExhausted);
        // Names need no file content.
        assert!(run(&f, req(1, 1, 2, wire::DiffFormat::Names)).await.is_ok());
    }
}
