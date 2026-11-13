//! The `CiInsights` tonic service. Reading needs repository read, reporting needs repository admin
//! (the runner's service credential), changing a group needs repository write, and quarantining
//! a maintainer: repository admin, or a member of the policy role `maintainer`.

use std::sync::Arc;

use asc_rpc::auth::{Access, AuthInterceptor, Authorizer};
use asc_rpc::server::{CallCtx, MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::ci_insights_server::{CiInsights, CiInsightsServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::Insights;
use crate::review::Env;

/// The `asc.v1.CiInsights` tonic service.
pub struct CiInsightsService {
    hub: Arc<Insights>,
    env: Env,
}

impl CiInsightsService {
    pub fn new(hub: Arc<Insights>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            hub,
            env: Env { authorizer, info },
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<CiInsightsServer<Self>, AuthInterceptor> {
        let server = CiInsightsServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Whether the caller is a maintainer: repository admin, or a member of the policy role.
    async fn maintainer(&self, ext: &tonic::Extensions, ctx: &CallCtx) -> bool {
        if self.env.has(ext, ctx, Access::Admin).await {
            return true;
        }
        let Ok(entry) = self.hub.registry.open(&ctx.repo).await else {
            return false;
        };
        let Ok(loaded) = self.hub.policy.in_force(&entry).await else {
            return false;
        };
        let Some(policy) = loaded.policy() else {
            return false;
        };
        let who = crate::policy::with_teams(
            &self.hub.pathrules,
            ctx.repo.as_str(),
            crate::policy::who_of(&ctx.principal.id),
        )
        .await;
        policy.is_maintainer(&who)
    }
}

#[tonic::async_trait]
impl CiInsights for CiInsightsService {
    async fn report_test_cases(
        &self,
        request: Request<wire::ReportTestCasesRequest>,
    ) -> Result<Response<wire::ReportTestCasesResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Admin, false)
            .await?;
        self.hub.report(&ctx, msg).await.map(Response::new)
    }

    async fn list_failure_groups(
        &self,
        request: Request<wire::ListFailureGroupsRequest>,
    ) -> Result<Response<wire::ListFailureGroupsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_groups(&ctx, msg).await.map(Response::new)
    }

    async fn get_failure_group(
        &self,
        request: Request<wire::GetFailureGroupRequest>,
    ) -> Result<Response<wire::GetFailureGroupResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.get_group(&ctx, msg).await.map(Response::new)
    }

    async fn set_failure_group_state(
        &self,
        request: Request<wire::SetFailureGroupStateRequest>,
    ) -> Result<Response<wire::SetFailureGroupStateResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Write, true)
            .await?;
        let quarantine = msg.state == wire::FailureGroupState::Quarantined as i32;
        let maintainer = quarantine && self.maintainer(&ext, &ctx).await;
        self.hub
            .set_state(&ctx, maintainer, msg)
            .await
            .map(Response::new)
    }

    async fn flaky_history(
        &self,
        request: Request<wire::FlakyHistoryRequest>,
    ) -> Result<Response<wire::FlakyHistoryResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.flaky_history(&ctx, msg).await.map(Response::new)
    }

    async fn list_flaky_tests(
        &self,
        request: Request<wire::ListFlakyTestsRequest>,
    ) -> Result<Response<wire::ListFlakyTestsResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.list_flaky(&ctx, msg).await.map(Response::new)
    }

    async fn cache_summary(
        &self,
        request: Request<wire::CacheSummaryRequest>,
    ) -> Result<Response<wire::CacheSummaryResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let ctx = self
            .env
            .context(&meta, &ext, msg.repo.as_ref(), Access::Read, false)
            .await?;
        self.hub.cache_summary(&ctx, msg).await.map(Response::new)
    }
}

#[cfg(test)]
mod cache_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use asc_rpc::IdMode;
    use asc_rpc::auth::{AuthError, BearerToken, Principal, parse_authorization};
    use asc_rpc::server::RepoName;
    use async_trait::async_trait;
    use tonic::Code;
    use uuid::Uuid;

    use super::*;
    use crate::ci::cache_stats::{self, Counters};
    use crate::ci::insights::{CACHE_FEATURE, InsightsConfig};
    use crate::ingest::testutil::{registry, repo};
    use crate::registry::Ns;

    const DAY: i64 = 86_400;
    const TODAY: i64 = 100 * DAY;
    /// One hour after midnight of day 100 since the epoch.
    const NOW: i64 = TODAY + 3600;

    /// Tokens: `reader` reads `o/r` only; `elsewhere` reads `x/y` only.
    struct Scoped;

    #[async_trait]
    impl Authorizer for Scoped {
        async fn authorize(
            &self,
            token: Option<&asc_rpc::auth::BearerToken>,
            repo: &str,
            access: Access,
        ) -> Result<Principal, AuthError> {
            let token: &BearerToken = token.ok_or(AuthError::Unauthenticated)?;
            match (token.expose(), repo) {
                ("reader", "o/r") | ("elsewhere", "x/y") if access <= Access::Read => {
                    Ok(Principal {
                        id: "7:rita".into(),
                    })
                }
                ("reader" | "elsewhere", _) => Err(AuthError::Forbidden),
                _ => Err(AuthError::Unauthenticated),
            }
        }
    }

    /// Counters whose five values follow `hits`.
    fn counters(hits: u64) -> Counters {
        Counters {
            hits,
            misses: 2,
            puts: 3,
            bytes_served: 1000 * hits,
            saved_ms: 50 * hits,
        }
    }

    fn wire_counters(hits: u64) -> wire::CacheCounters {
        wire::CacheCounters {
            hits,
            misses: 2,
            puts: 3,
            bytes_served: 1000 * hits,
            saved_ms: 50 * hits,
        }
    }

    fn ctx() -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal {
                id: "7:rita".into(),
            },
            op_id: Some(Uuid::new_v4()),
            client: None,
            admin: Default::default(),
        }
    }

    fn request(days: u32, changeset: &[u8]) -> wire::CacheSummaryRequest {
        wire::CacheSummaryRequest {
            repo: None,
            days,
            changeset: changeset.to_vec().into(),
        }
    }

    async fn hub() -> (Arc<Insights>, Ns) {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let hub = Arc::new(Insights::new(reg, InsightsConfig::default()));
        (hub, entry.ns)
    }

    /// Adds a daily row of `hits` for each `(days back from today, hits)`.
    async fn seed(hub: &Insights, ns: Ns, rows: &[(i64, u64)]) {
        for (back, hits) in rows {
            let day = TODAY - back * DAY;
            cache_stats::add_daily(hub.registry.pool(), ns, day, &counters(*hits))
                .await
                .unwrap();
        }
    }

    async fn summary(hub: &Insights, days: u32, changeset: &[u8]) -> wire::CacheSummaryResponse {
        hub.cache_summary_at(&ctx(), request(days, changeset), NOW)
            .await
            .unwrap()
    }

    fn service(hub: Arc<Insights>) -> CiInsightsService {
        CiInsightsService::new(hub, Arc::new(Scoped), ServerInfo::default())
    }

    fn call(repo: &str, token: Option<&str>) -> Request<wire::CacheSummaryRequest> {
        let mut r = Request::new(wire::CacheSummaryRequest {
            repo: Some(wire::RepoRef {
                name: repo.into(),
                id_mode: 0,
            }),
            days: 0,
            changeset: Default::default(),
        });
        if let Some(t) = token {
            let header = format!("Bearer {t}");
            r.extensions_mut()
                .insert(parse_authorization([header.as_bytes()]).unwrap().unwrap());
        }
        r
    }

    #[tokio::test]
    async fn cache_summary_returns_totals_and_days_oldest_first() {
        let (hub, ns) = hub().await;
        seed(&hub, ns, &[(0, 4), (2, 1), (5, 2)]).await;
        // Another repository's row and a day in the future are not counted.
        seed(&hub, Ns::random(), &[(0, 100)]).await;
        seed(&hub, ns, &[(-1, 500)]).await;
        let out = summary(&hub, 7, &[]).await;
        assert_eq!(out.since, TODAY - 6 * DAY);
        assert_eq!(
            out.totals,
            Some(wire::CacheCounters {
                hits: 7,
                misses: 6,
                puts: 9,
                bytes_served: 7000,
                saved_ms: 350,
            })
        );
        let days: Vec<(i64, u64)> = out
            .days
            .iter()
            .map(|d| (d.day, d.counters.as_ref().unwrap().hits))
            .collect();
        assert_eq!(
            days,
            vec![(TODAY - 5 * DAY, 2), (TODAY - 2 * DAY, 1), (TODAY, 4)]
        );
        assert_eq!(out.days[0].counters, Some(wire_counters(2)));
        assert!(!out.has_change);
        assert_eq!(out.change, None);
        // A window with no rows: zero totals, no days, `since` still set.
        let late = hub
            .cache_summary_at(&ctx(), request(7, &[]), NOW + 40 * DAY)
            .await
            .unwrap();
        assert_eq!(late.totals, Some(wire::CacheCounters::default()));
        assert!(late.days.is_empty());
        assert_eq!(late.since, TODAY + 34 * DAY);
    }

    #[tokio::test]
    async fn cache_summary_days_accepts_7_30_90_and_defaults_to_30() {
        let (hub, ns) = hub().await;
        let back = [0, 6, 7, 29, 30, 89, 90];
        let rows: Vec<(i64, u64)> = back.iter().map(|b| (*b, 1)).collect();
        seed(&hub, ns, &rows).await;
        let at = |days: u32| {
            let hub = hub.clone();
            async move {
                let out = summary(&hub, days, &[]).await;
                (out.days.len(), out.since)
            }
        };
        assert_eq!(at(7).await, (2, TODAY - 6 * DAY));
        assert_eq!(at(30).await, (4, TODAY - 29 * DAY));
        assert_eq!(at(90).await, (6, TODAY - 89 * DAY));
        for other in [0, 1, 6, 8, 29, 31, 91, 365, u32::MAX] {
            assert_eq!(at(other).await, (4, TODAY - 29 * DAY), "days = {other}");
        }
    }

    #[tokio::test]
    async fn cache_summary_rejects_a_changeset_that_is_not_32_bytes() {
        let (hub, _) = hub().await;
        for len in [1usize, 16, 31, 33, 64] {
            let e = hub
                .cache_summary_at(&ctx(), request(30, &vec![1; len]), NOW)
                .await
                .unwrap_err();
            assert_eq!(e.code(), Code::InvalidArgument, "{len} bytes");
        }
        summary(&hub, 30, &[1; 32]).await;
        summary(&hub, 30, &[]).await;
    }

    #[tokio::test]
    async fn cache_summary_has_change_only_when_counts_exist_for_it() {
        let (hub, ns) = hub().await;
        let pool = hub.registry.pool();
        let none = summary(&hub, 30, &[9; 32]).await;
        assert!(!none.has_change);
        assert_eq!(none.change, None);
        cache_stats::add_rev(pool, ns, &[9; 32], &counters(3), NOW)
            .await
            .unwrap();
        cache_stats::add_rev(pool, Ns::random(), &[8; 32], &counters(5), NOW)
            .await
            .unwrap();
        let some = summary(&hub, 30, &[9; 32]).await;
        assert!(some.has_change);
        assert_eq!(some.change, Some(wire_counters(3)));
        // Another change, a change of another repository, and no change asked for.
        for cs in [&[8u8; 32][..], &[7; 32][..], &[][..]] {
            let o = summary(&hub, 30, cs).await;
            assert!(!o.has_change, "{cs:?}");
            assert_eq!(o.change, None);
        }
    }

    #[tokio::test]
    async fn cache_summary_refuses_other_readers_like_an_unknown_repository() {
        let (hub, ns) = hub().await;
        // The service reads the real clock, so the row is today's.
        let today = asc_ci::insights::flaky::day_start(crate::review::now());
        cache_stats::add_daily(hub.registry.pool(), ns, today, &counters(4))
            .await
            .unwrap();
        let svc = service(hub);
        let ok = svc
            .cache_summary(call("o/r", Some("reader")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ok.totals.unwrap().hits, 4);
        // A reader of another repository gets one answer for the real repository and for one
        // that does not exist, so the call does not tell which repositories exist.
        let real = svc
            .cache_summary(call("o/r", Some("elsewhere")))
            .await
            .unwrap_err();
        let unknown = svc
            .cache_summary(call("o/nope", Some("elsewhere")))
            .await
            .unwrap_err();
        assert_eq!(real.code(), Code::PermissionDenied);
        assert_eq!(
            (real.code(), real.message()),
            (unknown.code(), unknown.message())
        );
        let unknown = svc
            .cache_summary(call("o/nope", Some("reader")))
            .await
            .unwrap_err();
        assert_eq!(unknown.code(), Code::PermissionDenied);
    }

    #[tokio::test]
    async fn cache_summary_refuses_an_anonymous_caller_on_a_private_repository() {
        let (hub, ns) = hub().await;
        seed(&hub, ns, &[(0, 4)]).await;
        let svc = service(hub);
        let e = svc.cache_summary(call("o/r", None)).await.unwrap_err();
        assert_eq!(e.code(), Code::Unauthenticated);
        let e = svc
            .cache_summary(call("o/r", Some("bogus")))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::Unauthenticated);
        let e = svc
            .cache_summary(Request::new(wire::CacheSummaryRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);
    }

    #[test]
    fn cache_summary_feature_is_advertised() {
        assert_eq!(CACHE_FEATURE, "ci-cache.v1");
        let lib = include_str!("../../lib.rs");
        assert!(
            lib.contains("features.push(ci::insights::CACHE_FEATURE.to_owned());"),
            "serve adds the feature to Version.features"
        );
    }
}
