//! Linked `UpdateRefs` (SPEC §10.6, writable mounts): a commit under a writable mount makes a
//! changeset in the child repository and one in the parent that moves the pin, and both land
//! together. The request names the parent; `linked` names each other repository of this server
//! with its updates. Every repository goes through the same steps as a plain push, on its own:
//! classification, ingest, fast-forward, the policy gate (repository policy, path rules,
//! required checks, secret scanning, signatures), the agent gate and both hook points. Then one
//! SQL transaction writes every repository's refs, op result, outbox row and audit events, so the
//! group lands all or nothing. A refusal on any side refuses the group with that side's error.
//!
//! A linked group moves shared branches only: no LAND, no `refs/for/`, no user refs, no expiry
//! and no skipped hooks. The service authorised write on every member before this runs.

use std::sync::Arc;

use asc_core::id::RefTarget;
use asc_engine::graph::Graph;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{CallCtx, LinkedRefsHandler, wire};
use asc_store::refs::{Actor, RefTxn};
use async_trait::async_trait;
use tonic::Status;
use uuid::Uuid;

use super::{
    Checked, DEFAULT_VIA, EffectRef, HookRequest, HookUpdate, MAX_PUSH_OPTION_LEN,
    MAX_PUSH_OPTIONS, OutboxEffects, ServerRefs, acting_principal, info, ingest_status, internal,
    plan_updates, registry_status, rpc, target_hex,
};
use crate::agents::gate::Charge;
use crate::graph_sql::SqlGraph;
use crate::ingest::Ingestor;
use crate::policy::gate::{GateMode, GateRequest};
use crate::refpolicy::{self, UserRefWriter};
use crate::registry::{RepoEntry, RepoMode};

/// The op id a member's own result is stored under: one op of the group is one row per
/// repository, and the parent's row (under the request's op id) holds the whole answer.
pub(crate) fn member_op_id(op_id: Uuid, repo: &str) -> Uuid {
    let mut h = blake3::Hasher::new();
    h.update(b"asc.linked-op.v1\0");
    h.update(op_id.as_bytes());
    h.update(repo.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&h.finalize().as_bytes()[..16]);
    // An RFC 9562 version 8 (custom) id, so it never collides with a client's v4 or v7.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// One repository of the group, checked and ready for the transaction.
struct Side<'a> {
    ctx: &'a CallCtx,
    entry: Arc<RepoEntry>,
    op_id: Uuid,
    acting: Principal,
    txn: RefTxn,
    results: Vec<wire::RefResult>,
    hook_updates: Vec<HookUpdate>,
    outbox: Vec<u8>,
    charge: Option<Charge>,
    /// The bridge or pre-receive ran, so a failure must abort it.
    hooked: bool,
}

impl ServerRefs {
    async fn abort_sides(&self, sides: &[Side<'_>], push_options: &[String]) {
        let hooks = self.hooks.push_hooks();
        for s in sides.iter().filter(|s| s.hooked) {
            hooks
                .abort(&HookRequest {
                    repo: &s.ctx.repo,
                    op_id: s.op_id,
                    principal: &s.acting,
                    updates: &s.hook_updates,
                    push_options,
                })
                .await;
        }
    }

    async fn settle_sides(&self, sides: &mut [Side<'_>], committed: bool) {
        if let Some(g) = self.hooks.agent_gate() {
            for s in sides {
                if let Some(c) = s.charge.take() {
                    g.settle(c, committed).await;
                }
            }
        }
    }

    /// Steps 1 to 7 for one repository of the group. On an error nothing of this side is left
    /// running; the caller aborts and settles the sides before it.
    async fn prepare_side<'a>(
        &self,
        ctx: &'a CallCtx,
        updates: &[wire::RefUpdate],
        op_id: Uuid,
        mode: wire::UpdateMode,
        push_options: &[String],
    ) -> Result<Side<'a>, Status> {
        let planned = plan_updates(updates)?;
        if planned.iter().any(|p| p.text.starts_with("refs/for/")) {
            return Err(Status::invalid_argument(
                "a linked update cannot publish a change for review",
            ));
        }
        if let Some(agent) = self.hooks.agent_gate() {
            let refs: Vec<(String, bool)> = planned
                .iter()
                .map(|p| (p.text.clone(), p.new.is_none()))
                .collect();
            agent.check_refs(ctx, false, &refs).await?;
        }
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }
        // A branch that this repository's policy keeps per user would become a user ref; a
        // linked group moves shared branches only.
        let policy = refpolicy::RepoPolicy::load(self.registry.pool(), entry.key.0).await?;
        for p in &planned {
            let owner = refpolicy::draft_owner_of(&ctx.principal, &p.text);
            if let refpolicy::Class::User { .. } = policy.classify(owner, &p.text)? {
                return Err(Status::failed_precondition(format!(
                    "{}: {} is not a shared branch, and a linked update moves shared branches only",
                    ctx.repo, p.text
                )));
            }
        }

        // Step 4: ingest, under the GC lock the caller holds.
        let targets: Vec<RefTarget> = planned.iter().filter_map(|p| p.new).collect();
        let windows_safe = self.windows_safe(&entry).await?;
        Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            &entry,
            windows_safe,
        )
        .by(refpolicy::accounting_owner_of(&ctx.principal))
        .staged_for(&ctx.principal.id)
        .ingest(&targets, self.hooks.observer())
        .await
        .map_err(ingest_status)?;

        // Step 5.
        let graph = Graph::new(SqlGraph::new(
            self.registry.pool().clone(),
            entry.ns,
            entry.backend.engine().clone(),
        ));
        let Checked {
            updates,
            results,
            hook_updates,
            agent_updates,
            gate_updates,
        } = self
            .check_planned(&entry, &graph, planned, mode, None)
            .await?;
        if let Some(gate) = self.hooks.gate() {
            gate.check(&GateRequest {
                ctx,
                entry: &entry,
                updates: &gate_updates,
                mode: GateMode::Push {
                    force: mode == wire::UpdateMode::Force,
                },
            })
            .await?;
        }

        // Steps 6 and 7.
        let hooks = self.hooks.push_hooks();
        let acting = acting_principal(ctx);
        let hook_req = HookRequest {
            repo: &ctx.repo,
            op_id,
            principal: &acting,
            updates: &hook_updates,
            push_options,
        };
        hooks
            .bridge_quarantine(&hook_req)
            .await
            .map_err(|e| rpc(RpcError::HookRejected(info(vec![e.0]))))?;
        if let Err(e) = hooks.pre_receive(&hook_req).await {
            hooks.abort(&hook_req).await;
            return Err(rpc(RpcError::HookRejected(info(vec![e.0]))));
        }
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: hooks.outbox_effects(),
            pusher: &acting.id,
            refs: hook_updates
                .iter()
                .map(|u| EffectRef {
                    name: u.name.clone(),
                    old: u.old.as_ref().map(target_hex),
                    new: u.new.as_ref().map(target_hex),
                })
                .collect(),
        })
        .map_err(|e| internal("outbox", &e));
        let outbox = match outbox {
            Ok(o) => o,
            Err(e) => {
                hooks.abort(&hook_req).await;
                return Err(e);
            }
        };
        let charge = match self.hooks.agent_gate() {
            Some(g) => match g.charge(ctx, &entry, &agent_updates).await {
                Ok(c) => c,
                Err(e) => {
                    hooks.abort(&hook_req).await;
                    return Err(e);
                }
            },
            None => None,
        };
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: ctx.principal.id.clone(),
                via: DEFAULT_VIA.into(),
            },
            reason: "UpdateRefs (linked)".into(),
            op_id,
        };
        Ok(Side {
            ctx,
            entry,
            op_id,
            acting,
            txn,
            results,
            hook_updates,
            outbox,
            charge,
            hooked: true,
        })
    }

    async fn linked(
        &self,
        ctx: &CallCtx,
        members: &[CallCtx],
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let op_id = ctx
            .op_id
            .ok_or_else(|| Status::invalid_argument("mutating calls need x-asc-op-id"))?;
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode is not served by UpdateRefs",
            ));
        }
        let mode = wire::UpdateMode::try_from(req.mode)
            .map_err(|_| Status::invalid_argument("unknown update mode"))?;
        if mode == wire::UpdateMode::Land {
            return Err(Status::invalid_argument("a linked update cannot land"));
        }
        if req.hooks != wire::HookMode::Run as i32 {
            return Err(Status::permission_denied("a linked update runs every hook"));
        }
        if req.ttl_seconds != 0 {
            return Err(Status::invalid_argument(
                "a linked update moves shared branches, which do not expire",
            ));
        }
        if req.no_content_merge {
            return Err(Status::invalid_argument(
                "no_content_merge is for LAND only",
            ));
        }
        if req.push_options.len() > MAX_PUSH_OPTIONS
            || req
                .push_options
                .iter()
                .any(|o| o.len() > MAX_PUSH_OPTION_LEN)
        {
            return Err(Status::invalid_argument(
                "too many or too long push options",
            ));
        }
        if members.len() != req.linked.len() {
            return Err(Status::internal("linked members do not match the request"));
        }
        let primary = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        // Step 2: a retry of a group that landed gets the stored answer.
        if let Some(done) = self.stored(&primary, op_id).await? {
            return Ok(done);
        }

        let _gc = self.hooks.gc().shared().await.map_err(|e| {
            tracing::warn!(error = %e, "gc fence unavailable");
            Status::unavailable("garbage-collection fence unavailable; retry")
        })?;
        let mut sides: Vec<Side<'_>> = Vec::with_capacity(1 + members.len());
        let groups = std::iter::once((ctx, op_id, req.updates.as_slice())).chain(
            members.iter().zip(&req.linked).map(|(m, l)| {
                (
                    m,
                    member_op_id(op_id, m.repo.as_str()),
                    l.updates.as_slice(),
                )
            }),
        );
        for (side_ctx, side_op, updates) in groups {
            match self
                .prepare_side(side_ctx, updates, side_op, mode, &req.push_options)
                .await
            {
                Ok(s) => sides.push(s),
                Err(e) => {
                    self.abort_sides(&sides, &req.push_options).await;
                    self.settle_sides(&mut sides, false).await;
                    return Err(e);
                }
            }
        }

        // Step 8: one transaction for the whole group.
        let response = wire::UpdateRefsResponse {
            results: sides[0].results.clone(),
            rewritten: Vec::new(),
            message: String::new(),
            linked: sides[1..]
                .iter()
                .map(|s| wire::LinkedResult {
                    repo: s.ctx.repo.to_string(),
                    results: s.results.clone(),
                })
                .collect(),
        };
        let committed = self.commit_group(&sides, &response).await;
        self.settle_sides(&mut sides, committed.is_ok()).await;
        match committed {
            Ok(()) => Ok(response),
            Err(e) => {
                self.abort_sides(&sides, &req.push_options).await;
                if let Some(done) = self.stored(&primary, op_id).await? {
                    return Ok(done);
                }
                Err(e)
            }
        }
    }

    async fn commit_group(
        &self,
        sides: &[Side<'_>],
        response: &wire::UpdateRefsResponse,
    ) -> Result<(), Status> {
        let refs = self.registry.refs();
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        for (i, s) in sides.iter().enumerate() {
            let own;
            let stored = if i == 0 {
                response
            } else {
                own = wire::UpdateRefsResponse {
                    results: s.results.clone(),
                    ..wire::UpdateRefsResponse::default()
                };
                &own
            };
            self.write_in_tx(
                &mut tx,
                &s.entry,
                &s.txn,
                stored,
                &s.outbox,
                None,
                UserRefWriter::Client,
            )
            .await
            .map_err(|e| {
                // Name the repository whose side refused, so the person knows which one.
                asc_rpc::server::restate(&e, e.code(), format!("{}: {}", s.ctx.repo, e.message()))
            })?;
        }
        tx.commit().await.map_err(|e| internal("commit", &e))
    }
}

#[async_trait]
impl LinkedRefsHandler for ServerRefs {
    async fn update_refs_linked(
        &self,
        ctx: &CallCtx,
        members: &[CallCtx],
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let refs: Vec<String> = std::iter::once(&req.updates)
            .chain(req.linked.iter().map(|l| &l.updates))
            .flatten()
            .map(|u| u.name.clone())
            .collect();
        let result = self.linked(ctx, members, req).await;
        if let Err(status) = &result
            && let Some(code) = crate::audit::hooks::refusal_code(status)
        {
            crate::audit::hooks::record_refused(
                self.registry.pool(),
                ctx,
                &refs,
                DEFAULT_VIA,
                &code,
            )
            .await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use asc_core::id::ChangesetId;
    use asc_core::path::RefName;
    use asc_rpc::server::RepoName;
    use asc_store::refs::Refs;
    use sqlx::Row;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::registry::Registry;

    fn ctx_for(name: &str, op: Uuid) -> CallCtx {
        CallCtx {
            repo: RepoName::parse(name).unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(op),
            client: None,
            admin: Default::default(),
        }
    }

    fn update(expect: wire::ref_update::Expect, to: ChangesetId) -> wire::RefUpdate {
        wire::RefUpdate {
            name: "refs/heads/master".into(),
            expect: Some(expect),
            new: Some(wire::RefTarget::from(&RefTarget::Changeset(to))),
            delete: false,
        }
    }

    fn absent() -> wire::ref_update::Expect {
        wire::ref_update::Expect::Absent(true)
    }

    fn group(
        parent: ChangesetId,
        child: (wire::ref_update::Expect, ChangesetId),
    ) -> wire::UpdateRefsRequest {
        wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            updates: vec![update(absent(), parent)],
            linked: vec![wire::LinkedUpdate {
                repo: Some(wire::RepoRef {
                    name: "o/lib".into(),
                    id_mode: 0,
                }),
                updates: vec![update(child.0, child.1)],
            }],
            mode: wire::UpdateMode::FastForward as i32,
            ..wire::UpdateRefsRequest::default()
        }
    }

    async fn setup() -> (Arc<Registry>, Arc<RepoEntry>, Arc<RepoEntry>, ServerRefs) {
        let r = registry().await;
        let p = repo(&r).await;
        let c = r
            .create(&RepoName::parse("o/lib").unwrap(), &Default::default())
            .await
            .unwrap();
        let s = ServerRefs::new(r.clone(), Arc::default());
        (r, p, c, s)
    }

    async fn head(r: &Registry, e: &RepoEntry) -> Option<RefTarget> {
        let name = RefName::new("refs/heads/master").unwrap();
        r.refs().get(e.key, &name).await.unwrap().map(|v| v.target)
    }

    async fn count(r: &Registry, sql: &str) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(r.pool())
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    #[tokio::test]
    async fn a_group_lands_in_both_repositories_with_one_result_row_each() {
        let (r, p, c, s) = setup().await;
        let a = commit(&p, &[], &[("a", b"1")]).await;
        let b = commit(&c, &[], &[("b", b"2")]).await;
        let op = Uuid::new_v4();
        let members = [ctx_for("o/lib", op)];
        let res = s
            .update_refs_linked(&ctx_for("o/r", op), &members, group(a, (absent(), b)))
            .await
            .unwrap();
        assert_eq!(res.results.len(), 1);
        assert_eq!(res.linked.len(), 1);
        assert_eq!(res.linked[0].repo, "o/lib");
        assert_eq!(head(&r, &p).await, Some(RefTarget::Changeset(a)));
        assert_eq!(head(&r, &c).await, Some(RefTarget::Changeset(b)));
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 2);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM outbox").await, 2);

        // A retry of the same op gets the stored answer and moves nothing.
        let again = s
            .update_refs_linked(&ctx_for("o/r", op), &members, group(a, (absent(), b)))
            .await
            .unwrap();
        assert_eq!(again, res);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 2);
    }

    #[tokio::test]
    async fn a_refusal_on_the_child_leaves_both_repositories_unchanged() {
        let (r, p, c, s) = setup().await;
        let a = commit(&p, &[], &[("a", b"1")]).await;
        let b = commit(&c, &[], &[("b", b"2")]).await;
        let wrong =
            wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(b)));
        let op = Uuid::new_v4();
        let err = s
            .update_refs_linked(
                &ctx_for("o/r", op),
                &[ctx_for("o/lib", op)],
                group(a, (wrong, b)),
            )
            .await
            .unwrap_err();
        assert!(err.message().contains("o/lib"), "{err:?}");
        assert_eq!(head(&r, &p).await, None);
        assert_eq!(head(&r, &c).await, None);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM op_results").await, 0);
        assert_eq!(count(&r, "SELECT COUNT(*) FROM outbox").await, 0);
    }

    #[tokio::test]
    async fn land_ttl_skipped_hooks_and_review_refs_are_refused() {
        let (r, p, c, s) = setup().await;
        let a = commit(&p, &[], &[("a", b"1")]).await;
        let b = commit(&c, &[], &[("b", b"2")]).await;
        let op = Uuid::new_v4();
        let members = [ctx_for("o/lib", op)];
        let parent = ctx_for("o/r", op);
        let mut land = group(a, (absent(), b));
        land.mode = wire::UpdateMode::Land as i32;
        let mut ttl = group(a, (absent(), b));
        ttl.ttl_seconds = 60;
        let mut skip = group(a, (absent(), b));
        skip.hooks = wire::HookMode::SkipPre as i32;
        let mut review = group(a, (absent(), b));
        review.linked[0].updates[0].name = "refs/for/master".into();
        for req in [land, ttl, skip, review] {
            assert!(s.update_refs_linked(&parent, &members, req).await.is_err());
        }
        assert_eq!(head(&r, &p).await, None);
        assert_eq!(head(&r, &c).await, None);
    }

    #[test]
    fn member_op_ids_differ_by_repository_and_are_version_8() {
        let op = Uuid::new_v4();
        let a = member_op_id(op, "o/a");
        assert_ne!(a, member_op_id(op, "o/b"));
        assert_eq!(a, member_op_id(op, "o/a"));
        assert_eq!(a.get_version_num(), 8);
    }
}
