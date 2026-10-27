//! `UpdateRefs` of `refs/for/<branch>/asc-<change>`: `asc submit` against an asc server.
//!
//! The one contract the client and the server share (SPEC section 20.5 flow 1): a `refs/for/` update
//! is never stored as a ref. It names a change (`asc-<change id>` after the target branch) and a
//! changeset, and means "publish this changeset as the next version of that change". The request
//! carries only such updates; they arrive bottom to top, and the change before one is its parent
//! change. A submit goes through the same gates as a push, in the same order:
//!
//! * write access (the service checked it) and the agent gate;
//! * the closure is ingested under the shared GC lock, recorded to the submitter;
//! * the policy gate in [`GateMode::Submit`]: a usable policy, the submitter's path rules, an
//!   agent's scope and budget, and the secret scan with its fail-closed truncation. Protection,
//!   approvals and signatures belong to the landing, which a submit never does;
//! * the push hooks: the bridge quarantine and `pre_receive` (the forge's, with the attributed
//!   user) unless a caller allowed to skip them says so, and an `abort` when the commit fails;
//! * one transaction: the owner and lease rules, the versions, the stored answer and the same
//!   outbox effects a push queues (post-receive, webhooks, fan-out).
//!
//! **Who owns a change.** The person who published its first version. That first version is
//! accepted only from the person who put its changeset on the server (the ingester the byte ledger
//! recorded for it) or from an administrator, so someone else's changeset cannot be published
//! under one's own name by being first. The recorded owner is the actor of version 1. Every later version, and a forced one, needs
//! the recorded owner or an administrator (`CHANGE_NOT_ALLOWED` otherwise); the lease of
//! `versions::append` still requires the latest version to be in the new changeset's `preds`
//! closure (`CHANGE_DIVERGED`) unless the push option `force-push` is set.

use super::*;
use crate::changes::{index, versions};
use crate::policy::gate::{GateMode, GateRequest, GateUpdate};

/// The longest branch name a `refs/for/` update may name.
const MAX_BRANCH: usize = 200;

struct Submission {
    text: String,
    change: asc_core::change::ChangeId,
    cs: ChangesetId,
}

/// `refs/for/<branch>/asc-<change>` to `(branch, change)`.
fn parse_topic(name: &str) -> Option<(String, asc_core::change::ChangeId)> {
    let rest = name.strip_prefix("refs/for/")?;
    let (branch, topic) = rest.rsplit_once("/asc-")?;
    if branch.is_empty() || branch.len() > MAX_BRANCH || branch.contains(['\0', '\\']) {
        return None;
    }
    let change = topic.parse().ok()?;
    Some((branch.to_owned(), change))
}

fn refusal(m: String) -> Status {
    rpc(RpcError::ChangeNotAllowed(info(vec![m])))
}

impl ServerRefs {
    pub(super) async fn submit(
        &self,
        ctx: &CallCtx,
        req: wire::UpdateRefsRequest,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let bad = |m: &str| Status::invalid_argument(m.to_owned());
        let op_id = ctx
            .op_id
            .ok_or_else(|| bad("mutating calls need x-asc-op-id"))?;
        if ctx.id_mode != IdMode::Asc {
            return Err(Status::unimplemented(
                "git id mode is not served by UpdateRefs",
            ));
        }
        if req.updates.is_empty() || req.updates.len() > MAX_REF_UPDATES {
            return Err(bad("bad number of updates"));
        }
        if req.mode == wire::UpdateMode::Land as i32 {
            return Err(bad("a submit does not land"));
        }
        let mut subs = Vec::with_capacity(req.updates.len());
        let mut seen = HashSet::new();
        for u in &req.updates {
            let Some((_, change)) = parse_topic(&u.name) else {
                return Err(bad(
                    "refs/for updates are refs/for/<branch>/asc-<change> and travel alone",
                ));
            };
            let cs = match (&u.new, u.delete) {
                (Some(t), false) => match RefTarget::try_from(t)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?
                {
                    RefTarget::Changeset(cs) => cs,
                    RefTarget::Tag(_) => return Err(bad("a change is published at a changeset")),
                },
                _ => return Err(bad("a submit sets a changeset, it does not delete")),
            };
            if !seen.insert(change) {
                return Err(bad("a change is submitted twice in one request"));
            }
            subs.push(Submission {
                text: u.name.clone(),
                change,
                cs,
            });
        }
        let hooks = self.hooks.push_hooks();
        if req.hooks == wire::HookMode::SkipPre as i32 && !hooks.may_skip_pre(ctx) {
            return Err(Status::permission_denied(
                "hooks cannot be skipped by this caller",
            ));
        }
        if req.push_options.len() > MAX_PUSH_OPTIONS
            || req
                .push_options
                .iter()
                .any(|o| o.len() > MAX_PUSH_OPTION_LEN)
        {
            return Err(bad("too many or too long push options"));
        }
        let force = req.push_options.iter().any(|o| o == "force-push");
        if let Some(agent) = self.hooks.agent_gate() {
            let refs: Vec<(String, bool)> = subs.iter().map(|s| (s.text.clone(), false)).collect();
            agent.check_refs(ctx, false, &refs).await?;
        }
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        if let Some(done) = self.stored(&entry, op_id).await? {
            return Ok(done);
        }
        if entry.mode == RepoMode::Mirror {
            return Err(rpc(RpcError::MirrorMode(info(vec![ctx.repo.to_string()]))));
        }

        // The closure of every changeset, under the shared GC lock held until the commit.
        let _gc = self.hooks.gc().shared().await.map_err(|e| {
            tracing::warn!(error = %e, "gc fence unavailable");
            Status::unavailable("garbage-collection fence unavailable; retry")
        })?;
        let submitter = crate::authz::UserIdentity::of(&ctx.principal)
            .filter(crate::authz::UserIdentity::is_user)
            .map(|u| u.user_id);
        let targets: Vec<RefTarget> = subs.iter().map(|s| RefTarget::Changeset(s.cs)).collect();
        let windows_safe = self.windows_safe(&entry).await?;
        Ingestor::new(
            self.registry.pool(),
            self.registry.refs(),
            &entry,
            windows_safe,
        )
        .by(submitter)
        .ingest(&targets, self.hooks.observer())
        .await
        .map_err(ingest_status)?;

        // Each changeset must be a version of the change its topic names.
        let css: Vec<ChangesetId> = subs.iter().map(|s| s.cs).collect();
        let rows = {
            let mut conn = self
                .registry
                .pool()
                .acquire()
                .await
                .map_err(|e| internal("database", &e))?;
            index::rows_of(&mut conn, entry.ns, &css)
                .await
                .map_err(|e| internal("change index", &e))?
        };
        for s in &subs {
            let ok = rows.iter().any(|r| r.cs == s.cs && r.change == s.change);
            if !ok {
                return Err(bad(
                    "the changeset is not a version of the change its topic names",
                ));
            }
        }

        // The policy gate, as for a push (see `GateMode::Submit`).
        let gate_updates: Vec<GateUpdate> = subs
            .iter()
            .map(|s| GateUpdate {
                name: s.text.clone(),
                old: None,
                new: Some(RefTarget::Changeset(s.cs)),
                user: false,
            })
            .collect();
        if let Some(gate) = self.hooks.gate() {
            gate.check(&GateRequest {
                ctx,
                entry: &entry,
                updates: &gate_updates,
                mode: GateMode::Submit,
            })
            .await?;
        }

        // The hook points, as for a push: the bridge quarantine, then the forge's pre-receive.
        let hook_updates: Vec<HookUpdate> = subs
            .iter()
            .map(|s| HookUpdate {
                name: s.text.clone(),
                old: None,
                new: Some(RefTarget::Changeset(s.cs)),
            })
            .collect();
        let hook_req = HookRequest {
            repo: &ctx.repo,
            op_id,
            principal: &ctx.principal,
            updates: &hook_updates,
            push_options: &req.push_options,
        };
        hooks
            .bridge_quarantine(&hook_req)
            .await
            .map_err(|e| rpc(RpcError::HookRejected(info(vec![e.0]))))?;
        if req.hooks != wire::HookMode::SkipPre as i32
            && let Err(e) = hooks.pre_receive(&hook_req).await
        {
            hooks.abort(&hook_req).await;
            return Err(rpc(RpcError::HookRejected(info(vec![e.0]))));
        }

        // An agent's budget is charged for what it publishes and given back if the commit fails.
        let agent_updates: Vec<RefChange> = subs
            .iter()
            .map(|s| RefChange {
                name: s.text.clone(),
                old: None,
                new: Some(RefTarget::Changeset(s.cs)),
            })
            .collect();
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
        let committed = self
            .publish(
                ctx,
                &entry,
                op_id,
                &subs,
                force,
                submitter,
                &hook_updates,
                hooks,
            )
            .await;
        if let (Some(g), Some(c)) = (self.hooks.agent_gate(), charge) {
            g.settle(c, committed.is_ok()).await;
        }
        match committed {
            Ok(response) => Ok(response),
            Err(e) => {
                hooks.abort(&hook_req).await;
                // A retry of the same op that committed in the meantime is not a failure.
                if let Some(done) = self.stored(&entry, op_id).await? {
                    return Ok(done);
                }
                Err(e)
            }
        }
    }

    /// The transaction: owner and lease rules, the versions, the stored answer, the outbox.
    #[allow(clippy::too_many_arguments)] // the pieces `submit` has already made
    async fn publish(
        &self,
        ctx: &CallCtx,
        entry: &RepoEntry,
        op_id: Uuid,
        subs: &[Submission],
        force: bool,
        submitter: Option<i64>,
        hook_updates: &[HookUpdate],
        hooks: &dyn PushHooks,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let mut tx = self
            .registry
            .refs()
            .begin()
            .await
            .map_err(|e| internal("refs", &e))?;
        let admin = ctx.admin.holds().await;
        let actor = ctx.principal.id.clone();
        let at = now();
        let mut parent = None;
        let mut results = Vec::with_capacity(subs.len());
        for s in subs {
            // The owner is whoever published version 1.
            let owner: Option<String> = sqlx::query_scalar(
                "SELECT actor FROM change_versions WHERE ns = $1 AND change = $2 AND v = 1",
            )
            .bind(entry.ns.as_bytes().to_vec())
            .bind(s.change.as_bytes().to_vec())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| internal("change_versions", &e))?;
            match owner {
                // Later versions: the recorded owner, or an administrator.
                Some(owner) if owner != actor && !admin => {
                    return Err(refusal(format!(
                        "not allowed to update change {}",
                        s.change
                    )));
                }
                Some(_) => {}
                // The first version: the person who put this changeset on the server, or nobody
                // in particular (a changeset the ledger has no owner for).
                None if !admin => {
                    let ingester: Option<i64> = sqlx::query_scalar(
                        "SELECT CAST(ingester AS BIGINT) FROM cs_ledger WHERE ns = $1 AND cs = $2",
                    )
                    .bind(entry.ns.as_bytes().to_vec())
                    .bind(s.cs.as_bytes().to_vec())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| internal("cs_ledger", &e))?;
                    let mine = submitter.unwrap_or(0);
                    if ingester.is_some_and(|i| i != 0 && i != mine) {
                        return Err(refusal(format!(
                            "not allowed to publish change {}: its changeset was put here by someone else",
                            s.change
                        )));
                    }
                }
                None => {}
            }
            versions::append(
                &mut tx,
                entry.ns,
                &versions::NewVersion {
                    change: s.change,
                    cs: s.cs,
                    parent_change: parent,
                    at,
                    actor: &actor,
                    op_id: op_id.as_bytes(),
                    force,
                },
            )
            .await
            .map_err(Status::from)?;
            parent = Some(s.change);
            results.push(wire::RefResult {
                name: s.text.clone(),
                old: None,
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(s.cs))),
                user: false,
            });
        }
        let response = wire::UpdateRefsResponse {
            results,
            ..wire::UpdateRefsResponse::default()
        };
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(response.encode_to_vec())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("op_results", &e))?;
        // The effects a push queues: post-receive and what hangs on it.
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: hooks.outbox_effects(),
            pusher: &ctx.principal.id,
            refs: hook_updates
                .iter()
                .map(|u| EffectRef {
                    name: u.name.clone(),
                    old: u.old.as_ref().map(target_hex),
                    new: u.new.as_ref().map(target_hex),
                })
                .collect(),
        })
        .map_err(|e| internal("outbox", &e))?;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(entry.key.0)
        .bind(op_id.as_bytes().to_vec())
        .bind(outbox)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("outbox", &e))?;
        tx.commit().await.map_err(|e| internal("refs", &e))?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_topic_names_a_branch_and_a_change() {
        let id = asc_core::change::ChangeId::from_bytes([7; 16]);
        let (branch, change) = parse_topic(&format!("refs/for/master/asc-{id}")).unwrap();
        assert_eq!((branch.as_str(), change), ("master", id));
        let (branch, _) = parse_topic(&format!("refs/for/release/1.x/asc-{id}")).unwrap();
        assert_eq!(branch, "release/1.x");
        for bad in [
            "refs/heads/master",
            "refs/for/master",
            "refs/for//asc-x",
            "refs/for/master/asc-short",
            "refs/for/master/topic",
            "refs/for/ma\\ster/asc-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(parse_topic(bad).is_none(), "{bad}");
        }
    }

    use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
    use asc_rpc::server::RepoName;

    use crate::ingest::testutil::{commit, registry, repo};

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    fn call(who: &str, op: Uuid) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: who.into() },
            op_id: Some(op),
            client: None,
            admin: Default::default(),
        }
    }

    fn submit_req(updates: &[(u8, ChangesetId)], options: &[&str]) -> wire::UpdateRefsRequest {
        wire::UpdateRefsRequest {
            updates: updates
                .iter()
                .map(|(n, cs)| wire::RefUpdate {
                    name: format!("refs/for/master/asc-{}", cid(*n)),
                    expect: Some(wire::ref_update::Expect::Any(true)),
                    new: Some(wire::RefTarget::from(&RefTarget::Changeset(*cs))),
                    delete: false,
                })
                .collect(),
            push_options: options.iter().map(|o| (*o).to_owned()).collect(),
            ..wire::UpdateRefsRequest::default()
        }
    }

    async fn meta(
        r: &Arc<Registry>,
        e: &RepoEntry,
        change: u8,
        cs: ChangesetId,
        preds: Vec<ChangesetId>,
    ) {
        let op = if preds.is_empty() {
            RewriteOp::New
        } else {
            RewriteOp::Amend
        };
        let idx = index::ChangeIndex::new(r.clone());
        let mut tx = r.refs().begin().await.unwrap();
        idx.write_meta(&mut tx, e.key, &ChangeMeta::new(cs, cid(change), preds, op))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn versions_of(r: &Registry, change: u8) -> Vec<(i64, String)> {
        sqlx::query("SELECT CAST(v AS BIGINT) AS v, actor FROM change_versions WHERE change = $1 ORDER BY v")
            .bind(cid(change).as_bytes().to_vec())
            .fetch_all(r.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get("v"), row.get("actor")))
            .collect()
    }

    #[tokio::test]
    async fn a_stack_is_published_bottom_to_top_and_nothing_is_stored_as_a_ref() {
        let r = registry().await;
        let e = repo(&r).await;
        let s = ServerRefs::new(r.clone(), Arc::default());
        let a = commit(&e, &[], &[("a", b"1")]).await;
        let b = commit(&e, &[a], &[("b", b"2")]).await;
        meta(&r, &e, 1, a, vec![]).await;
        meta(&r, &e, 2, b, vec![]).await;
        let op = Uuid::new_v4();
        let res = s
            .update_refs(&call("alice", op), submit_req(&[(1, a), (2, b)], &[]))
            .await
            .unwrap();
        assert_eq!(res.results.len(), 2);
        assert_eq!(
            res.results[0].name,
            format!("refs/for/master/asc-{}", cid(1))
        );
        assert_eq!(versions_of(&r, 1).await, [(1, "alice".to_owned())]);
        assert_eq!(versions_of(&r, 2).await, [(1, "alice".to_owned())]);
        let parent: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT parent_change FROM change_versions WHERE change = $1")
                .bind(cid(2).as_bytes().to_vec())
                .fetch_one(r.pool())
                .await
                .unwrap();
        assert_eq!(
            parent.as_deref(),
            Some(cid(1).as_bytes().as_slice()),
            "the change below is the parent"
        );
        let refs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refs")
            .fetch_one(r.pool())
            .await
            .unwrap();
        assert_eq!(refs, 0, "refs/for is never stored");
        // A retry with the same operation id is the first answer, and publishes nothing twice.
        let again = s
            .update_refs(&call("alice", op), submit_req(&[(1, a), (2, b)], &[]))
            .await
            .unwrap();
        assert_eq!(again.results.len(), 2);
        assert_eq!(versions_of(&r, 1).await.len(), 1);
    }

    #[tokio::test]
    async fn only_its_publisher_or_an_administrator_updates_a_change_and_the_lease_holds() {
        let r = registry().await;
        let e = repo(&r).await;
        let s = ServerRefs::new(r.clone(), Arc::default());
        let v1 = commit(&e, &[], &[("a", b"1")]).await;
        let v2 = commit(&e, &[], &[("a", b"2")]).await;
        let other = commit(&e, &[], &[("a", b"3")]).await;
        meta(&r, &e, 1, v1, vec![]).await;
        meta(&r, &e, 1, v2, vec![v1]).await;
        meta(&r, &e, 1, other, vec![]).await;
        let go = |who: &'static str, cs, opts: &'static [&'static str]| {
            let s = &s;
            async move {
                s.update_refs(&call(who, Uuid::new_v4()), submit_req(&[(1, cs)], opts))
                    .await
            }
        };
        go("alice", v1, &[]).await.unwrap();
        // Someone else may not take the change over.
        let e1 = go("mallory", v2, &[]).await.unwrap_err();
        assert_eq!(crate::update::tests_code(&e1), "CHANGE_NOT_ALLOWED");
        // The publisher moves it on along its history: version 2.
        go("alice", v2, &[]).await.unwrap();
        assert_eq!(versions_of(&r, 1).await.len(), 2);
        // A changeset that does not descend from the latest version is divergent...
        let e2 = go("alice", other, &[]).await.unwrap_err();
        assert_eq!(crate::update::tests_code(&e2), "CHANGE_DIVERGED");
        // ...unless the publisher forces it.
        go("alice", other, &["force-push"]).await.unwrap();
        assert_eq!(versions_of(&r, 1).await.len(), 3);
    }

    #[tokio::test]
    async fn a_submit_travels_alone_and_names_a_changeset_of_its_change() {
        let r = registry().await;
        let e = repo(&r).await;
        let s = ServerRefs::new(r.clone(), Arc::default());
        let a = commit(&e, &[], &[("a", b"1")]).await;
        meta(&r, &e, 1, a, vec![]).await;
        // A topic for another change than the changeset is a version of.
        let wrong = s
            .update_refs(&call("alice", Uuid::new_v4()), submit_req(&[(2, a)], &[]))
            .await
            .unwrap_err();
        assert_eq!(wrong.code(), tonic::Code::InvalidArgument);
        // A submit mixed with a branch update is refused whole.
        let mut mixed = submit_req(&[(1, a)], &[]);
        mixed.updates.push(wire::RefUpdate {
            name: "refs/heads/master".into(),
            expect: Some(wire::ref_update::Expect::Absent(true)),
            new: Some(wire::RefTarget::from(&RefTarget::Changeset(a))),
            delete: false,
        });
        let e = s
            .update_refs(&call("alice", Uuid::new_v4()), mixed)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert!(versions_of(&r, 1).await.is_empty());
    }
}
