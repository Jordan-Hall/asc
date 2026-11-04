//! The whole-stack operations of `Stacks` (stacks.proto) over the change index.
//!
//! `Restack` takes the stack a change is in, from its bottom unlanded change up through every
//! descendant ([`super::rpc::stack_walk`]), and rebases it onto a target with
//! [`super::restack::Restacker::run`], settling the conflicts the caller resolved. Landed changes
//! are left where they are and answered `SKIPPED_LANDED`. Unless `allow_partial`, a conflict
//! anywhere writes nothing: a dry run answers it per item, an apply is refused with
//! `LAND_CONFLICT` and the conflicts in its detail. New heads are published as the next version
//! of each change with no ref moved; the caller moves the refs it keeps. `Audit` serialises an
//! apply per repository and checks the caller's pinned heads first.
//!
//! The changes must be the caller's own, or the forge's on behalf of their authors, as for
//! `RestackChanges`.

use std::collections::HashMap;
use std::sync::Arc;

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{CallCtx, rpc_status, wire};
use async_trait::async_trait;
use bytes::Bytes;
use tonic::Status;

use super::restack::{Outcome, Resolved};
use super::rpc::{ChangesRpc, Reader, change_of, stack_walk};
use crate::audit::stacks_rpc::StackOps;
use crate::commit_graph_rpc::internal;
use crate::registry::Extensions;

/// The [`StackOps`] of the server.
pub struct StackRestacker {
    changes: Arc<ChangesRpc>,
}

/// Registers the whole-stack operations with `Stacks` when both the audit state and
/// `RestackChanges` are there.
pub fn register(ext: &Extensions) {
    let (Some(audit), Some(changes)) = (ext.audit.get(), ext.changes.get()) else {
        return;
    };
    if changes.restack.get().is_none() {
        return;
    }
    assert!(
        audit
            .stack_ops
            .set(Arc::new(StackRestacker::new(changes.clone()))),
        "the stack operations are registered once"
    );
}

/// The changes of a stack in order, and the head of each landed one.
struct Walked {
    order: Vec<ChangeId>,
    landed: HashMap<ChangeId, ChangesetId>,
}

impl StackRestacker {
    pub fn new(changes: Arc<ChangesRpc>) -> Self {
        Self { changes }
    }

    async fn walk(
        &self,
        ctx: &CallCtx,
        anchor: ChangeId,
    ) -> Result<(Walked, crate::idmode::IdCodec), Status> {
        let (entry, ids, engine) = self.changes.open(ctx).await?;
        let (_, land_ref) = ChangesRpc::land_ref(&entry, "")?;
        let mut conn = self
            .changes
            .registry
            .pool()
            .acquire()
            .await
            .map_err(|e| internal("database", &e))?;
        let mut reader = Reader {
            conn: &mut conn,
            ids: &ids,
            engine: &engine,
            ns: entry.ns,
            land_ref,
            heads: HashMap::new(),
        };
        if reader.head(anchor).await?.is_none() {
            return Err(Status::not_found("unknown change"));
        }
        let (_, order) = stack_walk(&mut reader, anchor).await?;
        let mut landed = HashMap::new();
        for c in &order {
            if reader.landed(*c).await?.is_some()
                && let Some(head) = reader.head(*c).await?
            {
                landed.insert(*c, head.cs);
            }
        }
        Ok((Walked { order, landed }, ids))
    }
}

/// `LAND_CONFLICT` with the conflicts in its detail.
fn conflict_error(conflicts: &[wire::Conflict]) -> Status {
    let mut detail = ErrorInfo {
        paths: conflicts
            .iter()
            .map(|c| String::from_utf8_lossy(&c.path).into_owned())
            .collect(),
        ..ErrorInfo::default()
    };
    match conflicts
        .iter()
        .map(asc_rpc::convert::Conflict::try_from)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(records) => detail.conflicts = records,
        Err(e) => return internal("restack conflict", &e),
    }
    rpc_status(&RpcError::LandConflict(detail))
}

#[async_trait]
impl StackOps for StackRestacker {
    async fn restack(
        &self,
        ctx: &CallCtx,
        req: wire::RestackStackRequest,
    ) -> Result<wire::RestackStackResponse, Status> {
        use wire::RestackItemState as S;
        let restacker = self
            .changes
            .restack
            .get()
            .ok_or_else(|| Status::unimplemented("Stacks.Restack"))?
            .clone();
        let anchor = change_of(&req.change)?;
        let mut resolved = Resolved::parse(&req.resolutions)?;
        let (walked, ids) = self.walk(ctx, anchor).await?;
        let items: Vec<wire::RestackItem> = walked
            .order
            .iter()
            .filter(|c| !walked.landed.contains_key(c))
            .map(|c| wire::RestackItem {
                change: Bytes::copy_from_slice(c.as_bytes()),
                update_ref: String::new(),
                expect: None,
            })
            .collect();
        let run = if items.is_empty() {
            None
        } else {
            let call = wire::RestackChangesRequest {
                repo: req.repo.clone(),
                onto: req.onto.clone(),
                items,
                atomic: !req.allow_partial,
                dry_run: req.dry_run,
                ..wire::RestackChangesRequest::default()
            };
            Some(restacker.run(ctx, call, &mut resolved).await?)
        };
        if run.is_none() && resolved.unused().is_some() {
            return Err(Status::invalid_argument(
                "every change of the stack has landed; the resolutions settle nothing",
            ));
        }
        if let Some(run) = &run
            && !req.dry_run
            && !req.allow_partial
            && !run.response.conflicts.is_empty()
        {
            return Err(conflict_error(&run.response.conflicts));
        }
        let applied = !req.dry_run
            && run
                .as_ref()
                .is_some_and(|r| !r.response.rewritten.is_empty());
        let (mut by_change, onto, rewritten) = match run {
            Some(r) => (
                r.items
                    .into_iter()
                    .map(|i| (i.change, i))
                    .collect::<HashMap<_, _>>(),
                r.onto,
                r.response.rewritten,
            ),
            None => (HashMap::new(), None, Vec::new()),
        };
        let mut items = Vec::with_capacity(walked.order.len());
        for c in &walked.order {
            let change = Bytes::copy_from_slice(c.as_bytes());
            let Some(item) = by_change.remove(c) else {
                let old_head = match walked.landed.get(c) {
                    Some(&cs) => ids.changeset_out(cs).await?,
                    None => Bytes::new(),
                };
                items.push(wire::RestackItemResult {
                    change,
                    state: S::SkippedLanded as i32,
                    old_head,
                    ..wire::RestackItemResult::default()
                });
                continue;
            };
            let old_head = ids.changeset_out(item.old).await?;
            let mut out = wire::RestackItemResult {
                change,
                old_head,
                ..wire::RestackItemResult::default()
            };
            match item.outcome {
                Outcome::OnBase => out.state = S::Unchanged as i32,
                Outcome::Blocked => out.state = S::Blocked as i32,
                Outcome::Conflict(conflicts) => {
                    out.state = S::Conflict as i32;
                    out.conflicts = conflicts;
                }
                Outcome::Rewritten { new, same_patch } => {
                    out.state = if same_patch {
                        S::Unchanged
                    } else {
                        S::Rewritten
                    } as i32;
                    if applied {
                        out.new_head = ids.changeset_out(new).await?;
                        out.new_version = item.version.saturating_add(1);
                    }
                }
            }
            items.push(out);
        }
        let onto = match onto {
            Some(cs) => ids.changeset_out(cs).await?,
            None => Bytes::new(),
        };
        Ok(wire::RestackStackResponse {
            onto,
            items,
            rewritten,
            applied,
        })
    }

    async fn revert_landed(
        &self,
        _ctx: &CallCtx,
        _req: wire::RevertLandedRequest,
    ) -> Result<wire::RevertLandedResponse, Status> {
        Err(Status::unimplemented("Stacks.RevertLanded"))
    }
}

#[cfg(test)]
mod tests {
    use asc_core::path::RepoPath;
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::RepoName;
    use uuid::Uuid;

    use super::super::restack::tests::{Fx, cbytes, fx};
    use super::*;
    use crate::ingest::testutil::commit;

    use wire::RestackItemState as S;

    fn ctx(apply: bool) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal {
                id: "writer".into(),
            },
            op_id: apply.then(Uuid::new_v4),
            client: None,
            admin: Default::default(),
        }
    }

    fn req(n: u8, onto: ChangesetId, dry_run: bool) -> wire::RestackStackRequest {
        wire::RestackStackRequest {
            change: cbytes(n),
            onto: onto.to_string(),
            dry_run,
            ..wire::RestackStackRequest::default()
        }
    }

    fn resolve(cs: ChangesetId, path: &str, data: &'static [u8]) -> wire::Resolution {
        wire::Resolution {
            changeset: Bytes::copy_from_slice(cs.as_bytes()),
            path: Bytes::copy_from_slice(path.as_bytes()),
            op: Some(wire::FileOp {
                op: Some(wire::file_op::Op::PutContent(Bytes::from_static(data))),
                ..wire::FileOp::default()
            }),
        }
    }

    fn ops(f: &Fx) -> StackRestacker {
        StackRestacker::new(f.rpc.clone())
    }

    fn states(r: &wire::RestackStackResponse) -> Vec<(Bytes, i32)> {
        r.items
            .iter()
            .map(|i| (i.change.clone(), i.state))
            .collect()
    }

    fn bytes(cs: ChangesetId) -> Bytes {
        Bytes::copy_from_slice(cs.as_bytes())
    }

    #[tokio::test]
    async fn a_dry_run_answers_the_whole_stack_from_any_member_and_writes_nothing() {
        let f = fx().await;
        let r = ops(&f)
            .restack(&ctx(false), req(2, f.t, true))
            .await
            .unwrap();
        // Change 2 names the stack 1 -> 2; change 3 is another stack.
        assert_eq!(
            states(&r),
            vec![
                (cbytes(1), S::Unchanged as i32),
                (cbytes(2), S::Unchanged as i32)
            ]
        );
        assert_eq!(r.items[0].old_head, bytes(f.c[0]));
        assert_eq!(r.items[1].old_head, bytes(f.c[1]));
        assert!(
            r.items
                .iter()
                .all(|i| i.new_head.is_empty() && i.new_version == 0)
        );
        assert_eq!(r.onto, bytes(f.t));
        assert_eq!(r.rewritten.len(), 2);
        assert!(!r.applied);
        assert_eq!(f.latest(1).await.v, 1);
    }

    #[tokio::test]
    async fn an_apply_publishes_the_next_version_of_each_change() {
        let f = fx().await;
        let r = ops(&f)
            .restack(&ctx(true), req(1, f.t, false))
            .await
            .unwrap();
        assert!(r.applied);
        let (v1, v2) = (f.latest(1).await, f.latest(2).await);
        assert_eq!(r.items[0].new_head, bytes(v1.cs));
        assert_eq!(r.items[1].new_head, bytes(v2.cs));
        assert_eq!((r.items[0].new_version, r.items[1].new_version), (2, 2));
        assert_eq!((v1.v, v2.v), (2, 2));
        assert_eq!(f.cs(v1.cs).await.parents, vec![f.t]);
        assert_eq!(f.cs(v2.cs).await.parents, vec![v1.cs]);
    }

    #[tokio::test]
    async fn a_conflict_blocks_what_is_above_it_and_an_apply_writes_nothing() {
        let f = fx().await;
        let dry = ops(&f)
            .restack(&ctx(false), req(1, f.x, true))
            .await
            .unwrap();
        assert_eq!(
            states(&dry),
            vec![
                (cbytes(1), S::Conflict as i32),
                (cbytes(2), S::Blocked as i32)
            ]
        );
        assert_eq!(dry.items[0].conflicts.len(), 1);
        assert_eq!(dry.items[0].conflicts[0].path.as_ref(), b"one");
        assert_eq!(dry.items[0].conflicts[0].changeset, bytes(f.c[0]));

        let refused = ops(&f)
            .restack(&ctx(true), req(1, f.x, false))
            .await
            .unwrap_err();
        assert_eq!(
            refused.code(),
            tonic::Code::FailedPrecondition,
            "{refused:?}"
        );
        assert!(
            refused.message().starts_with("LAND_CONFLICT"),
            "{refused:?}"
        );
        assert!(refused.message().contains("one"), "{refused:?}");

        let partial = ops(&f)
            .restack(
                &ctx(true),
                wire::RestackStackRequest {
                    allow_partial: true,
                    ..req(1, f.x, false)
                },
            )
            .await
            .unwrap();
        assert!(!partial.applied, "nothing could move");
        assert_eq!(partial.items[0].state, S::Conflict as i32);
        assert_eq!((f.latest(1).await.v, f.latest(2).await.v), (1, 1));
    }

    #[tokio::test]
    async fn a_resolution_settles_the_conflict_and_the_stack_moves() {
        let f = fx().await;
        let m = wire::RestackStackRequest {
            resolutions: vec![resolve(f.c[0], "one", b"merged\n")],
            ..req(2, f.x, false)
        };
        let r = ops(&f).restack(&ctx(true), m).await.unwrap();
        assert!(r.applied);
        // The resolved change has another patch now; the one above it does not.
        assert_eq!(
            states(&r),
            vec![
                (cbytes(1), S::Rewritten as i32),
                (cbytes(2), S::Unchanged as i32)
            ]
        );
        let v1 = f.latest(1).await;
        let new = f.cs(v1.cs).await;
        assert_eq!(new.parents, vec![f.x]);
        assert_eq!(new.message, f.cs(f.c[0]).await.message);
        let engine = f.e.backend.engine();
        let one = RepoPath::new("one").unwrap();
        let got = engine.lookup(new.root, &one).await.unwrap().unwrap();
        let want_cs = commit(&f.e, &[f.a], &[("one", b"merged\n")]).await;
        let want_root = f.cs(want_cs).await.root;
        let want = engine.lookup(want_root, &one).await.unwrap().unwrap();
        assert_eq!(got.id, want.id);
        assert_eq!(f.cs(f.latest(2).await.cs).await.parents, vec![v1.cs]);
    }

    #[tokio::test]
    async fn a_resolution_nothing_needs_and_a_bad_one_are_refused() {
        let f = fx().await;
        let unused = wire::RestackStackRequest {
            resolutions: vec![resolve(f.c[0], "one", b"merged\n")],
            ..req(1, f.t, true)
        };
        let e = ops(&f).restack(&ctx(false), unused).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
        assert!(e.message().contains("settles no conflict"), "{e:?}");

        let mut moded = resolve(f.c[0], "one", b"x\n");
        moded.op.as_mut().unwrap().mode = 0o100755;
        for bad in [
            moded,
            resolve(f.c[0], "../one", b"x\n"),
            wire::Resolution {
                op: None,
                ..resolve(f.c[0], "one", b"x\n")
            },
        ] {
            let m = wire::RestackStackRequest {
                resolutions: vec![bad],
                ..req(1, f.x, true)
            };
            let e = ops(&f).restack(&ctx(false), m).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{e:?}");
        }
    }

    #[tokio::test]
    async fn a_landed_change_is_skipped_and_the_rest_sits_on_the_target() {
        let f = fx().await;
        let land_ref = format!("refs/heads/{}", f.e.default_branch);
        sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, 0, $3)")
            .bind(f.e.ns.as_bytes().to_vec())
            .bind(land_ref)
            .bind(f.c[0].as_bytes().to_vec())
            .execute(f.reg.pool())
            .await
            .unwrap();
        let r = ops(&f)
            .restack(&ctx(true), req(1, f.t, false))
            .await
            .unwrap();
        assert_eq!(
            states(&r),
            vec![
                (cbytes(1), S::SkippedLanded as i32),
                (cbytes(2), S::Unchanged as i32)
            ]
        );
        assert_eq!(r.items[0].old_head, bytes(f.c[0]));
        assert!(r.items[0].new_head.is_empty());
        assert_eq!(f.latest(1).await.v, 1);
        assert_eq!(f.cs(f.latest(2).await.cs).await.parents, vec![f.t]);
    }

    #[tokio::test]
    async fn an_unknown_change_is_not_found() {
        let f = fx().await;
        let e = ops(&f)
            .restack(&ctx(false), req(9, f.t, true))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
    }
}
