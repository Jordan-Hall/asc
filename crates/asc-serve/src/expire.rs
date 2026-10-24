//! The expiring-refs worker (SPEC §8.8, PLAN U48d).
//!
//! `UpdateRefs.ttl_seconds` sets `refs.expires_at` on agent branches and snapshots. Every ten
//! minutes this worker deletes the refs whose time has passed, through the normal `UpdateRefs`
//! pipeline: the same compare-and-swap, `ref_log` row and outbox `post_receive` effect (with a
//! deletion) as a client's delete, so Forgejo's branch table stays in sync.
//!
//! Only names that may carry a TTL are ever deleted ([`ttl_eligible`]). The pipeline does not
//! restrict which refs get an expiry, so the worker is the second line of defence: a writer who
//! asked for `ttl_seconds` on `master` gets a ref that is never reaped.
//!
//! User refs (`refs/users/<owner-id>/heads/agent/*`, SPEC §21.5.1) are owner-only, so the worker
//! cannot delete them as the owner. It deletes them through `ServerRefs::expire_user_refs`, an
//! internal path no RPC reaches that may only delete expired user refs, in the same kind of
//! transaction (CAS on the scanned target, repository guard, `ref_log`) as any other delete.
//!
//! Replicas may run the worker at the same time. Each deletion is a CAS on the ref's target, so
//! the loser of a race finds the ref gone or moved and leaves it alone.

use std::sync::Arc;
use std::time::Duration;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{CallCtx, RefsHandler, RepoName, wire};
use asc_store::refs::Refs;
use sqlx::Row;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::authz::HOST_SERVICE_PRINCIPAL;
use crate::registry::{Registry, RepoMode};
use crate::update::{PipelineHooks, ServerRefs, now};

/// How often the worker runs (SPEC §8.8).
pub const EXPIRE_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Expired rows read per query.
const SCAN_PAGE: i64 = 500;
/// Refs deleted per `UpdateRefs` call. A failed batch is retried one ref at a time.
const DELETE_BATCH: usize = 100;
/// Refs one run deletes at most; the rest wait for the next run, so one run stays bounded.
pub const MAX_PER_RUN: usize = 100_000;
/// The actor recorded in the ref log when the server has no Forgejo to act as.
const ACTOR: &str = "asc-expiry";

/// Whether a ref may carry a TTL (SPEC §8.8): `refs/heads/agent/*`, `refs/snapshots/*` and, in a
/// branchless repo, the pusher's `refs/users/<owner>/heads/agent/*`.
pub fn ttl_eligible(name: &str) -> bool {
    let tail = |prefix: &str, s: &str| s.strip_prefix(prefix).is_some_and(|t| !t.is_empty());
    if tail("refs/heads/agent/", name) || tail("refs/snapshots/", name) {
        return true;
    }
    // Only a well-formed user ref (a canonical numeric owner id) can be deleted by the worker.
    crate::refpolicy::parse_user_ref(name).is_some_and(|(_, branch)| tail("agent/", branch))
}

/// What one run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExpireReport {
    pub deleted: usize,
    /// Refs that were not deleted because they moved, were refreshed or were already gone.
    pub kept: usize,
    /// Refs whose deletion failed for another reason (hook rejection, database error).
    pub failed: usize,
}

/// The worker.
pub struct Expirer {
    registry: Arc<Registry>,
    pipeline: Arc<PipelineHooks>,
    refs: ServerRefs,
    page: i64,
    batch: usize,
    max_per_run: usize,
    landed_ref_days: u32,
}

impl Expirer {
    pub fn new(registry: Arc<Registry>, pipeline: Arc<PipelineHooks>) -> Self {
        Self {
            refs: ServerRefs::new(registry.clone(), pipeline.clone()),
            registry,
            pipeline,
            page: SCAN_PAGE,
            batch: DELETE_BATCH,
            max_per_run: MAX_PER_RUN,
            landed_ref_days: crate::reftl::DEFAULT_LANDED_REF_DAYS,
        }
    }

    /// `[cloud] landed_ref_days`: a landed user ref is deleted this long after it landed
    /// (SPEC §21.2.4); `0` disables.
    #[must_use]
    pub fn with_landed_ref_days(mut self, days: u32) -> Self {
        self.landed_ref_days = days;
        self
    }

    /// Smaller pages, so a test can cross a page boundary without hundreds of refs.
    #[cfg(test)]
    fn with_limits(mut self, page: i64, batch: usize, max_per_run: usize) -> Self {
        self.page = page;
        self.batch = batch;
        self.max_per_run = max_per_run;
        self
    }

    /// Runs the worker until the returned handle is aborted.
    pub fn spawn_worker(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EXPIRE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let reaper = crate::reftl::LandedReaper::new(
                self.registry.clone(),
                self.clone(),
                self.landed_ref_days,
            );
            let mut n: u32 = 0;
            loop {
                tick.tick().await;
                // Landed user refs go hourly (SPEC §21.2.4), every sixth tick.
                if n.is_multiple_of(6) {
                    match reaper.run_once(now()).await {
                        Ok(r) if r != crate::reftl::ReapReport::default() => {
                            tracing::info!(?r, "expired landed user refs");
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "the landed-ref expiry failed"),
                    }
                }
                n = n.wrapping_add(1);
                match self.run_once(now()).await {
                    Ok(r) if r != ExpireReport::default() => {
                        tracing::info!(
                            deleted = r.deleted,
                            kept = r.kept,
                            failed = r.failed,
                            "expired refs"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "the expiry run failed"),
                }
            }
        })
    }

    /// Deletes every eligible ref whose `expires_at` is at or before `at`.
    pub async fn run_once(&self, at: i64) -> Result<ExpireReport, sqlx::Error> {
        let mut report = ExpireReport::default();
        // Keyset pagination on the primary key: ineligible or failing rows cannot starve the scan.
        let mut after: (i64, String) = (-1, String::new());
        let mut seen = 0usize;
        loop {
            let rows = sqlx::query(
                "SELECT r.repo_id AS repo_id, p.name AS repo, r.name AS name, r.target AS target \
                 FROM refs r JOIN repos p ON p.id = r.repo_id \
                 WHERE r.expires_at IS NOT NULL AND r.expires_at <= $1 \
                   AND (r.name LIKE 'refs/heads/agent/%' OR r.name LIKE 'refs/snapshots/%' \
                        OR r.name LIKE 'refs/users/%') \
                   AND (r.repo_id > $2 OR (r.repo_id = $2 AND r.name > $3)) \
                 ORDER BY r.repo_id, r.name LIMIT $4",
            )
            .bind(at)
            .bind(after.0)
            .bind(after.1.clone())
            .bind(self.page)
            .fetch_all(self.registry.pool())
            .await?;
            let full = i64::try_from(rows.len()).is_ok_and(|n| n >= self.page);
            let mut due: Vec<Due> = Vec::new();
            for row in &rows {
                let repo_id: i64 = row.try_get("repo_id")?;
                let name: String = row.try_get("name")?;
                after = (repo_id, name.clone());
                let target: Vec<u8> = row.try_get("target")?;
                let (Ok(target), true) = (RefTarget::from_bytes(&target), ttl_eligible(&name))
                else {
                    report.kept += 1;
                    continue;
                };
                due.push(Due {
                    repo: row.try_get("repo")?,
                    name,
                    target,
                });
            }
            seen += rows.len();
            self.delete_due(due, at, &mut report).await;
            if !full || seen >= self.max_per_run {
                return Ok(report);
            }
        }
    }

    /// Deletes `due`, grouped by repository.
    async fn delete_due(&self, due: Vec<Due>, at: i64, report: &mut ExpireReport) {
        let mut i = 0;
        while i < due.len() {
            let repo = due[i].repo.clone();
            let mut j = i;
            while j < due.len() && due[j].repo == repo {
                j += 1;
            }
            self.delete_in_repo(&repo, &due[i..j], at, report).await;
            i = j;
        }
    }

    async fn delete_in_repo(&self, repo: &str, due: &[Due], at: i64, report: &mut ExpireReport) {
        let Ok(name) = RepoName::parse(repo) else {
            report.failed += due.len();
            return;
        };
        let entry = match self.registry.open(&name).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(%repo, error = %e, "expiry: cannot open the repository");
                report.failed += due.len();
                return;
            }
        };
        if entry.mode == RepoMode::Mirror {
            report.kept += due.len();
            return;
        }
        // Re-read each ref right before deleting it: a push may have moved it or given it a new
        // TTL since the scan.
        let mut live: Vec<&Due> = Vec::new();
        for d in due {
            let Ok(ref_name) = RefName::new(d.name.as_bytes()) else {
                report.failed += 1;
                continue;
            };
            match self.registry.refs().get(entry.key, &ref_name).await {
                Ok(Some(e)) if e.target == d.target && e.expires_at.is_some_and(|x| x <= at) => {
                    live.push(d);
                }
                Ok(_) => report.kept += 1,
                Err(e) => {
                    tracing::warn!(%repo, error = %e, "expiry: cannot read a ref");
                    report.failed += 1;
                }
            }
        }
        let (users, live): (Vec<&Due>, Vec<&Due>) = live
            .into_iter()
            .partition(|d| d.name.starts_with(crate::refpolicy::USER_NS));
        self.delete_chunks(&name, &users, true, at, report).await;
        self.delete_chunks(&name, &live, false, at, report).await;
    }

    /// Deletes `live` in batches, `user` refs through the internal expiry path and the rest
    /// through `UpdateRefs`.
    async fn delete_chunks(
        &self,
        name: &RepoName,
        live: &[&Due],
        user: bool,
        at: i64,
        report: &mut ExpireReport,
    ) {
        let repo = name.to_string();
        for chunk in live.chunks(self.batch.max(1)) {
            if self.delete_any(name, chunk, user, at).await.is_ok() {
                report.deleted += chunk.len();
                continue;
            }
            // One lost race must not keep the others alive: retry one by one.
            for d in chunk {
                match self
                    .delete_any(name, std::slice::from_ref(d), user, at)
                    .await
                {
                    Ok(()) => report.deleted += 1,
                    Err(s) if s.code() == tonic::Code::Aborted => report.kept += 1,
                    Err(s) => {
                        tracing::warn!(%repo, code = ?s.code(), "expiry: a deletion failed");
                        report.failed += 1;
                    }
                }
            }
        }
    }

    /// Deletes one ref only if it still names `target` (`ABORTED` when it moved). A user ref
    /// must carry `expires_at <= at` (the internal path refuses any other).
    pub(crate) async fn delete_exact(
        &self,
        repo: &RepoName,
        name: &str,
        target: &RefTarget,
        at: i64,
    ) -> Result<(), tonic::Status> {
        let due = Due {
            repo: repo.to_string(),
            name: name.to_owned(),
            target: *target,
        };
        let user = crate::refpolicy::parse_user_ref(name).is_some();
        self.delete_any(repo, &[&due], user, at).await
    }

    async fn delete_any(
        &self,
        repo: &RepoName,
        refs: &[&Due],
        user: bool,
        at: i64,
    ) -> Result<(), tonic::Status> {
        if !user {
            return self.delete(repo, refs).await;
        }
        let named: Vec<(&str, RefTarget)> =
            refs.iter().map(|d| (d.name.as_str(), d.target)).collect();
        self.refs.expire_user_refs(repo, &named, ACTOR, at).await
    }

    /// One `UpdateRefs` call that deletes `refs`, each only if it still names the scanned target.
    async fn delete(&self, repo: &RepoName, refs: &[&Due]) -> Result<(), tonic::Status> {
        let hooks = self.pipeline.push_hooks();
        let mut ctx = CallCtx {
            repo: repo.clone(),
            id_mode: IdMode::Asc,
            principal: Principal {
                id: HOST_SERVICE_PRINCIPAL.into(),
            },
            op_id: Some(Uuid::new_v4()),
            client: None,
        };
        // The expiry is the server's own act, so Forgejo's pre-receive has no say in it. Without
        // Forgejo there is no hook to skip and the actor is the worker itself.
        let skip = hooks.may_skip_pre(&ctx);
        if !skip {
            ctx.principal.id = ACTOR.into();
        }
        let req = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: repo.to_string(),
                id_mode: 0,
            }),
            updates: refs
                .iter()
                .map(|d| wire::RefUpdate {
                    name: d.name.clone(),
                    expect: Some(wire::ref_update::Expect::Value(wire::RefTarget::from(
                        &d.target,
                    ))),
                    new: None,
                    delete: true,
                })
                .collect(),
            mode: wire::UpdateMode::FastForward as i32,
            hooks: if skip {
                wire::HookMode::SkipPre as i32
            } else {
                wire::HookMode::Run as i32
            },
            ..wire::UpdateRefsRequest::default()
        };
        self.refs.update_refs(&ctx, req).await.map(drop)
    }
}

/// One expired ref as the scan saw it.
struct Due {
    repo: String,
    name: String,
    target: RefTarget,
}

#[cfg(test)]
mod tests {
    use asc_core::id::ChangesetId;
    use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    const NOW: i64 = 1_000_000;

    fn set(name: &str, to: ChangesetId, expect: Expect, expires: Option<i64>) -> RefTxn {
        RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes()).unwrap(),
                expect,
                new: Some(RefTarget::Changeset(to)),
                expires_at: expires,
            }],
            actor: Actor {
                id: "alice".into(),
                via: "asc-push".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        }
    }

    struct Fx {
        registry: Arc<Registry>,
        entry: Arc<crate::registry::RepoEntry>,
        cs: ChangesetId,
        expirer: Expirer,
    }

    async fn fx() -> Fx {
        let registry = registry().await;
        let entry = repo(&registry).await;
        let cs = commit(&entry, &[], &[("a", b"a")]).await;
        let expirer = Expirer::new(registry.clone(), Arc::default());
        Fx {
            registry,
            entry,
            cs,
            expirer,
        }
    }

    impl Fx {
        async fn put(&self, name: &str, expires: Option<i64>) {
            self.registry
                .refs()
                .update(self.entry.key, &set(name, self.cs, Expect::Absent, expires))
                .await
                .unwrap();
        }

        async fn exists(&self, name: &str) -> bool {
            self.registry
                .refs()
                .get(self.entry.key, &RefName::new(name.as_bytes()).unwrap())
                .await
                .unwrap()
                .is_some()
        }

        async fn outbox(&self) -> Vec<serde_json::Value> {
            sqlx::query("SELECT effects FROM outbox ORDER BY id")
                .fetch_all(self.registry.pool())
                .await
                .unwrap()
                .iter()
                .map(|r| {
                    serde_json::from_slice(&r.get::<Vec<u8>, _>("effects")).expect("effects json")
                })
                .collect()
        }
    }

    #[test]
    fn only_agent_and_snapshot_names_may_expire() {
        for ok in [
            "refs/heads/agent/a",
            "refs/snapshots/abc",
            "refs/users/7/heads/agent/x",
        ] {
            assert!(ttl_eligible(ok), "{ok}");
        }
        for no in [
            "refs/heads/master",
            "refs/heads/agent/",
            "refs/heads/agentx/a",
            "refs/tags/v1",
            "refs/snapshots/",
            "refs/users/7/heads/feat",
            "refs/users//heads/agent/x",
            "refs/users/abc/heads/agent/x",
            "refs/users/07/heads/agent/x",
            "refs/users/7/heads/agent/",
            "refs/pull/1/head",
        ] {
            assert!(!ttl_eligible(no), "{no}");
        }
    }

    #[tokio::test]
    async fn an_expired_agent_ref_is_deleted_with_a_post_receive_deletion() {
        let f = fx().await;
        f.put("refs/heads/agent/old", Some(NOW - 1)).await;
        f.put("refs/heads/agent/new", Some(NOW + 3600)).await;
        f.put("refs/heads/agent/forever", None).await;
        f.put("refs/snapshots/s1", Some(NOW)).await;
        let report = f.expirer.run_once(NOW).await.unwrap();
        assert_eq!(report.deleted, 2);
        assert!(!f.exists("refs/heads/agent/old").await);
        assert!(!f.exists("refs/snapshots/s1").await);
        assert!(f.exists("refs/heads/agent/new").await);
        assert!(f.exists("refs/heads/agent/forever").await);
        // The deletion went through the pipeline: one outbox row with a post-receive effect that
        // reports each ref as deleted (`new` null) and names the ref it removed.
        let rows = f.outbox().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["effects"], serde_json::json!(["post_receive"]));
        let refs = rows[0]["refs"].as_array().unwrap();
        assert_eq!(refs.len(), 2);
        for r in refs {
            assert!(r["new"].is_null());
            assert!(r["old"].is_string());
        }
        // And the ref log records who and how.
        let log = f
            .registry
            .refs()
            .log(
                f.entry.key,
                &RefName::new(b"refs/heads/agent/old".as_slice()).unwrap(),
                5,
            )
            .await
            .unwrap();
        assert_eq!(log[0].new, None);
        // A second run finds nothing.
        assert_eq!(
            f.expirer.run_once(NOW).await.unwrap(),
            ExpireReport::default()
        );
    }

    #[tokio::test]
    async fn a_ref_that_may_not_expire_is_never_deleted() {
        let f = fx().await;
        // `UpdateRefs` stores whatever `expires_at` it is given, so the worker must refuse.
        f.put("refs/heads/master", Some(NOW - 10)).await;
        f.put("refs/tags/v1", Some(NOW - 10)).await;
        f.put("refs/users/7/heads/feat", Some(NOW - 10)).await;
        let report = f.expirer.run_once(NOW).await.unwrap();
        assert_eq!(report.deleted, 0);
        assert!(f.exists("refs/heads/master").await);
        assert!(f.exists("refs/tags/v1").await);
        assert!(f.exists("refs/users/7/heads/feat").await);
        assert!(f.outbox().await.is_empty());
    }

    #[tokio::test]
    async fn user_agent_refs_expire_like_shared_ones() {
        let f = fx().await;
        f.put("refs/users/7/heads/agent/x", Some(NOW - 1)).await;
        assert_eq!(f.expirer.run_once(NOW).await.unwrap().deleted, 1);
        assert!(!f.exists("refs/users/7/heads/agent/x").await);
    }

    async fn expire_user(f: &Fx, name: &str, at: i64) -> Result<(), tonic::Status> {
        let tip = RefTarget::Changeset(f.cs);
        let repo = RepoName::parse("o/r").unwrap();
        f.expirer
            .refs
            .expire_user_refs(&repo, &[(name, tip)], ACTOR, at)
            .await
    }

    #[tokio::test]
    async fn the_user_ref_path_deletes_only_expired_user_refs_and_logs_the_worker() {
        let f = fx().await;
        f.put("refs/users/7/heads/agent/old", Some(NOW - 1)).await;
        f.put("refs/users/7/heads/agent/new", Some(NOW + 100)).await;
        f.put("refs/users/7/heads/agent/forever", None).await;
        f.put("refs/heads/agent/shared", Some(NOW - 1)).await;
        // Not expired yet, expiry removed, or already gone: ABORTED, nothing deleted.
        for name in [
            "refs/users/7/heads/agent/new",
            "refs/users/7/heads/agent/forever",
            "refs/users/7/heads/agent/missing",
        ] {
            let e = expire_user(&f, name, NOW).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::Aborted, "{name}");
        }
        // Not a user ref, or a malformed one: refused, and the shared ref survives.
        for name in ["refs/heads/agent/shared", "refs/users/abc/heads/agent/x"] {
            let e = expire_user(&f, name, NOW).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{name}");
        }
        assert!(f.exists("refs/heads/agent/shared").await);
        assert!(f.exists("refs/users/7/heads/agent/new").await);
        // A target that moved is not deleted.
        let other = commit(&f.entry, &[f.cs], &[("b", b"b")]).await;
        let repo = RepoName::parse("o/r").unwrap();
        let moved = f
            .expirer
            .refs
            .expire_user_refs(
                &repo,
                &[("refs/users/7/heads/agent/old", RefTarget::Changeset(other))],
                ACTOR,
                NOW,
            )
            .await;
        assert!(moved.is_err());
        assert!(f.exists("refs/users/7/heads/agent/old").await);
        expire_user(&f, "refs/users/7/heads/agent/old", NOW)
            .await
            .unwrap();
        assert!(!f.exists("refs/users/7/heads/agent/old").await);
        let name = RefName::new(&b"refs/users/7/heads/agent/old"[..]).unwrap();
        let log = f.registry.refs().log(f.entry.key, &name, 10).await.unwrap();
        assert_eq!(log[0].new, None);
        assert_eq!(log[0].actor.id, ACTOR);
        assert_eq!(log[0].actor.via, "asc-expiry");
        // A user ref queues no post_receive.
        let outbox = f.outbox().await;
        assert_eq!(outbox.len(), 1);
        assert!(outbox[0]["effects"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_ref_that_moved_or_was_refreshed_after_the_scan_is_kept() {
        let f = fx().await;
        let other = commit(&f.entry, &[f.cs], &[("b", b"b")]).await;
        f.put("refs/heads/agent/a", Some(NOW - 1)).await;
        f.put("refs/heads/agent/b", Some(NOW - 1)).await;
        // `a` was refreshed (new TTL, same target); `b` was force-moved by a push without TTL.
        f.registry
            .refs()
            .update(
                f.entry.key,
                &set(
                    "refs/heads/agent/a",
                    f.cs,
                    Expect::Value(RefTarget::Changeset(f.cs)),
                    Some(NOW + 9999),
                ),
            )
            .await
            .unwrap();
        f.registry
            .refs()
            .update(
                f.entry.key,
                &set(
                    "refs/heads/agent/b",
                    other,
                    Expect::Value(RefTarget::Changeset(f.cs)),
                    None,
                ),
            )
            .await
            .unwrap();
        // The scan only sees what is expired at `NOW`, so run it with both refs due, as a worker
        // that read them just before the push would, by deleting from a stale snapshot.
        let stale = [
            Due {
                repo: "o/r".into(),
                name: "refs/heads/agent/a".into(),
                target: RefTarget::Changeset(f.cs),
            },
            Due {
                repo: "o/r".into(),
                name: "refs/heads/agent/b".into(),
                target: RefTarget::Changeset(f.cs),
            },
        ];
        let mut report = ExpireReport::default();
        f.expirer.delete_due(stale.into(), NOW, &mut report).await;
        assert_eq!(report.deleted, 0);
        assert_eq!(report.kept, 2);
        assert!(f.exists("refs/heads/agent/a").await);
        assert!(f.exists("refs/heads/agent/b").await);
    }

    #[tokio::test]
    async fn runs_cross_page_and_batch_boundaries_and_respect_the_cap() {
        let mut f = fx().await;
        f.expirer = Expirer::new(f.registry.clone(), Arc::default()).with_limits(2, 2, 5);
        for i in 0..7 {
            f.put(&format!("refs/heads/agent/r{i}"), Some(NOW - 1))
                .await;
        }
        // The cap bounds one run to whole pages: 3 pages of 2 would pass 5, so it stops at 6.
        let first = f.expirer.run_once(NOW).await.unwrap();
        assert_eq!(first.deleted, 6);
        let second = f.expirer.run_once(NOW).await.unwrap();
        assert_eq!(second.deleted, 1);
        assert_eq!(f.expirer.run_once(NOW).await.unwrap().deleted, 0);
    }

    #[tokio::test]
    async fn an_ineligible_row_does_not_starve_later_ones() {
        let mut f = fx().await;
        f.expirer = Expirer::new(f.registry.clone(), Arc::default()).with_limits(1, 1, 100);
        // `refs/users/7/heads/feat` matches the SQL prefix but not the strict rule; it sorts
        // before the agent ref of the same repo.
        f.put("refs/users/7/heads/feat", Some(NOW - 1)).await;
        f.put("refs/users/7/heads/zagent/x", Some(NOW - 1)).await;
        f.put("refs/users/8/heads/agent/y", Some(NOW - 1)).await;
        let report = f.expirer.run_once(NOW).await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!f.exists("refs/users/8/heads/agent/y").await);
        assert!(f.exists("refs/users/7/heads/feat").await);
    }

    #[tokio::test]
    async fn mirror_repos_are_left_alone() {
        let registry = registry().await;
        let entry = registry
            .create(
                &RepoName::parse("o/m").unwrap(),
                &crate::registry::NewRepo {
                    mode: RepoMode::Mirror,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let cs = commit(&entry, &[], &[("a", b"a")]).await;
        registry
            .refs()
            .update(
                entry.key,
                &set("refs/heads/agent/a", cs, Expect::Absent, Some(NOW - 1)),
            )
            .await
            .unwrap();
        let expirer = Expirer::new(registry.clone(), Arc::default());
        let report = expirer.run_once(NOW).await.unwrap();
        assert_eq!((report.deleted, report.kept), (0, 1));
    }
}
