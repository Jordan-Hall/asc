//! The `Usage` tonic service.
//!
//! A person reads their own usage, an organisation's owners read the organisation's, the site
//! administrator reads the server's and sets quotas. Nothing here lists accounts or
//! organisations: the owner is always named by the caller.

use std::collections::BTreeMap;
use std::sync::Arc;

use asc_rpc::auth::{AuthInterceptor, Authorizer};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{MAX_MESSAGE, ServerInfo, wire};
use asc_rpc::services::usage_server::{Usage, UsageServer};
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::store::{Grouping, Quota, Spend};
use super::{MAX_REPORT_DAYS, Period, Totals, UsageHub};
use crate::ai::audit::Entry;
use crate::ai::caller::{once, op_id};
use crate::ai::clock;
use crate::settings::access::{Actor, Gate, Subject};

/// The `asc.v1.Usage` service.
pub struct UsageService {
    hub: Arc<UsageHub>,
    gate: Gate,
}

fn db(e: sqlx::Error) -> Status {
    tracing::error!(class = %crate::ai::store::class(&e), "usage database error");
    Status::internal("internal error")
}

/// The subject a wire owner names: a user, an organisation or the instance.
pub fn subject_of(o: Option<&wire::OwnerRef>) -> Result<Subject, Status> {
    let o = o.ok_or_else(|| Status::invalid_argument("missing owner"))?;
    match wire::OwnerKind::try_from(o.kind) {
        Ok(wire::OwnerKind::User) => Subject::user(&o.name),
        Ok(wire::OwnerKind::Org) => Subject::org(&o.name),
        Ok(wire::OwnerKind::Instance) if o.name.is_empty() => Ok(Subject::Instance),
        _ => Err(Status::invalid_argument(
            "the owner is a user, an organisation or the instance",
        )),
    }
}

fn scope_of(s: &Subject) -> String {
    s.label()
}

fn wire_quota(q: Quota) -> wire::UsageQuota {
    wire::UsageQuota {
        storage_bytes: q.storage_bytes,
        agent_minutes: q.agent_minutes,
        ai_spend_micro_usd: q.ai_spend_micro_usd,
        bandwidth_bytes: q.bandwidth_bytes,
    }
}

fn meter(used: u64, limit: u64, counted: u64) -> wire::UsageMeter {
    wire::UsageMeter {
        used,
        limit,
        over: limit > 0 && counted > limit,
    }
}

impl UsageService {
    pub fn new(hub: Arc<UsageHub>, authorizer: Arc<dyn Authorizer>, _info: ServerInfo) -> Self {
        Self {
            hub,
            gate: Gate::new(authorizer),
        }
    }

    pub fn into_service(self) -> InterceptedService<UsageServer<Self>, AuthInterceptor> {
        let server = UsageServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    /// Reading the usage of `subject`: a person reads their own (read is enough), an
    /// organisation's owners and the site administrator read the rest.
    async fn reader(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
        subject: &Subject,
    ) -> Result<Actor, Status> {
        let caller = self.gate.caller(ext).await?;
        let needed = if matches!(subject, Subject::User(_)) {
            Role::Read
        } else {
            Role::Admin
        };
        self.gate.require(&caller, meta, subject, needed)
    }

    async fn site_admin(
        &self,
        ext: &tonic::Extensions,
        meta: &tonic::metadata::MetadataMap,
    ) -> Result<Actor, Status> {
        let caller = self.gate.caller(ext).await?;
        self.gate
            .require(&caller, meta, &Subject::Instance, Role::Admin)
    }

    async fn quota_of(&self, s: &Subject) -> Result<(Quota, Quota, Quota), Status> {
        let default = self.hub.store.quota("instance").await.map_err(db)?;
        if matches!(s, Subject::Instance) {
            return Ok((default, Quota::default(), default));
        }
        let own = self.hub.store.quota(&scope_of(s)).await.map_err(db)?;
        Ok((own.over(default), own, default))
    }
}

fn wire_spend(r: Spend) -> wire::SpendRow {
    wire::SpendRow {
        group: r.group,
        label: r.label,
        requests: r.requests,
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cost_micro_usd: r.cost,
    }
}

fn response_of(p: Period, t: Totals, q: Quota) -> wire::GetUsageResponse {
    let ai_counted = t.ai_spend.saturating_sub(t.byo_ai_spend);
    let empty = t.storage_bytes == 0
        && t.agent_minutes == 0
        && t.ai_spend == 0
        && t.bandwidth_bytes == 0
        && !t.byo_storage;
    let storage = meter(t.storage_bytes, q.storage_bytes, t.storage_bytes);
    let minutes = meter(t.agent_minutes, q.agent_minutes, t.agent_minutes);
    let ai = meter(t.ai_spend, q.ai_spend_micro_usd, ai_counted);
    let bw = meter(t.bandwidth_bytes, q.bandwidth_bytes, t.bandwidth_bytes);
    wire::GetUsageResponse {
        period: p.label(),
        period_start: p.start,
        resets_at: p.end,
        over_quota: storage.over || minutes.over || ai.over || bw.over,
        storage: Some(storage),
        agent_minutes: Some(minutes),
        ai_spend: Some(ai),
        bandwidth: Some(bw),
        ai_keys: t.ai_keys,
        byo_ai_spend_micro_usd: t.byo_ai_spend,
        byo_storage: t.byo_storage,
        empty,
        repos: t
            .repos
            .into_iter()
            .map(|r| wire::UsageRepo {
                repo: r.repo,
                storage_bytes: r.storage_bytes,
                agent_minutes: r.agent_minutes,
                ai_spend_micro_usd: r.ai_spend,
                bandwidth_bytes: r.bandwidth_bytes,
            })
            .collect(),
    }
}

#[tonic::async_trait]
impl Usage for UsageService {
    async fn get_usage(
        &self,
        request: Request<wire::GetUsageRequest>,
    ) -> Result<Response<wire::GetUsageResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let subject = subject_of(msg.owner.as_ref())?;
        self.reader(&ext, &meta, &subject).await?;
        let now = clock::now();
        let period = Period::parse(&msg.period, now)?;
        let totals = self.hub.totals(&subject, period, now).await.map_err(db)?;
        let (effective, _, _) = self.quota_of(&subject).await?;
        Ok(Response::new(response_of(period, totals, effective)))
    }

    async fn list_spend(
        &self,
        request: Request<wire::ListSpendRequest>,
    ) -> Result<Response<wire::ListSpendResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let subject = subject_of(msg.owner.as_ref())?;
        self.reader(&ext, &meta, &subject).await?;
        let now = clock::now();
        let since = if msg.since == 0 {
            clock::month_start(now)
        } else {
            msg.since
        };
        let until = if msg.until == 0 { now } else { msg.until };
        if since < 0 || until < since || (until - since) / 86_400 > MAX_REPORT_DAYS {
            return Err(Status::invalid_argument(
                "since and until must be a range of at most 400 days",
            ));
        }
        let (first, last) = (clock::day(since), clock::day(until));
        let group = wire::SpendGroup::try_from(msg.group_by)
            .map_err(|_| Status::invalid_argument("unknown group_by"))?;
        let store = &self.hub.store;
        let mut rows: Vec<Spend> = match group {
            wire::SpendGroup::Key => store
                .spend_by_key(&subject, &first, &last)
                .await
                .map_err(db)?,
            // An organisation or the server: agent spend by repository, day or month.
            wire::SpendGroup::Repo | wire::SpendGroup::Day | wire::SpendGroup::Month
                if !matches!(subject, Subject::User(_)) =>
            {
                let g = match group {
                    wire::SpendGroup::Repo => Grouping::Repo,
                    wire::SpendGroup::Day => Grouping::Day,
                    _ => Grouping::Month,
                };
                store
                    .spend_by_repo_time(&subject, g, &first, &last)
                    .await
                    .map_err(db)?
            }
            // Runs a person started, or the people of an organisation.
            _ => {
                let runs = store
                    .run_costs(&subject, since, until + 1)
                    .await
                    .map_err(db)?;
                let mut by: BTreeMap<String, Spend> = BTreeMap::new();
                for (person, created, repo, cost) in runs {
                    let g = match group {
                        wire::SpendGroup::User => person,
                        wire::SpendGroup::Repo => repo,
                        wire::SpendGroup::Day => clock::day(created),
                        _ => clock::month(created),
                    };
                    let e = by.entry(g.clone()).or_insert_with(|| Spend {
                        group: g,
                        ..Spend::default()
                    });
                    e.requests += 1;
                    e.cost += cost;
                }
                let mut v: Vec<Spend> = by.into_values().collect();
                if matches!(group, wire::SpendGroup::User | wire::SpendGroup::Repo) {
                    v.sort_by(|a, b| b.cost.cmp(&a.cost).then_with(|| a.group.cmp(&b.group)));
                }
                v
            }
        };
        rows.truncate(1000);
        let mut total = Spend {
            group: "total".into(),
            ..Spend::default()
        };
        for r in &rows {
            total.requests += r.requests;
            total.input_tokens += r.input_tokens;
            total.output_tokens += r.output_tokens;
            total.cost += r.cost;
        }
        Ok(Response::new(wire::ListSpendResponse {
            rows: rows.into_iter().map(wire_spend).collect(),
            total: Some(wire_spend(total)),
        }))
    }

    async fn get_quota(
        &self,
        request: Request<wire::GetQuotaRequest>,
    ) -> Result<Response<wire::GetQuotaResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let subject = subject_of(msg.owner.as_ref())?;
        self.reader(&ext, &meta, &subject).await?;
        let (effective, own, default) = self.quota_of(&subject).await?;
        Ok(Response::new(wire::GetQuotaResponse {
            effective: Some(wire_quota(effective)),
            r#override: Some(wire_quota(own)),
            instance_default: Some(wire_quota(default)),
        }))
    }

    async fn set_quota(
        &self,
        request: Request<wire::SetQuotaRequest>,
    ) -> Result<Response<wire::SetQuotaResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let subject = subject_of(msg.owner.as_ref())?;
        let actor = self.site_admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let q = msg
            .quota
            .ok_or_else(|| Status::invalid_argument("missing quota"))?;
        let quota = Quota {
            storage_bytes: q.storage_bytes,
            agent_minutes: q.agent_minutes,
            ai_spend_micro_usd: q.ai_spend_micro_usd,
            bandwidth_bytes: q.bandwidth_bytes,
        };
        // Stored as signed 64-bit numbers.
        let limit = u64::try_from(i64::MAX).unwrap_or(u64::MAX);
        if [
            quota.storage_bytes,
            quota.agent_minutes,
            quota.ai_spend_micro_usd,
            quota.bandwidth_bytes,
        ]
        .iter()
        .any(|v| *v > limit)
        {
            return Err(Status::invalid_argument("a quota is at most 2^63 - 1"));
        }
        let hub = self.hub.clone();
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Usage/SetQuota",
            true,
            || async {
                hub.store
                    .set_quota(&scope_of(&subject), quota, clock::now(), &actor.id)
                    .await
                    .map_err(db)?;
                hub.audit
                    .record(Entry {
                        owner: subject.audit_owner(),
                        target: &scope_of(&subject),
                        on_behalf: actor.relay.as_deref(),
                        details: &[
                            ("storage_bytes", quota.storage_bytes.to_string()),
                            ("agent_minutes", quota.agent_minutes.to_string()),
                            ("ai_spend_micro_usd", quota.ai_spend_micro_usd.to_string()),
                            ("bandwidth_bytes", quota.bandwidth_bytes.to_string()),
                        ],
                        ..Entry::new("usage.quota.set", &actor.id)
                    })
                    .await;
                let (effective, own, default) = self.quota_of(&subject).await?;
                Ok(wire::SetQuotaResponse {
                    quota: Some(wire::GetQuotaResponse {
                        effective: Some(wire_quota(effective)),
                        r#override: Some(wire_quota(own)),
                        instance_default: Some(wire_quota(default)),
                    }),
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn report(
        &self,
        request: Request<wire::ReportUsageRequest>,
    ) -> Result<Response<wire::ReportUsageResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        let actor = self.site_admin(&ext, &meta).await?;
        let op = op_id(&meta)?;
        let repo = asc_rpc::server::RepoName::parse(&msg.repo)
            .map_err(|_| Status::invalid_argument("repo is owner/name"))?;
        let now = clock::now();
        let at = if msg.at == 0 { now } else { msg.at };
        let oldest = now - i64::from(self.hub.cfg.retention_days) * 86_400;
        if at < oldest || at > now + 86_400 {
            return Err(Status::invalid_argument(
                "at is outside the retention window",
            ));
        }
        let hub = self.hub.clone();
        // Bandwidth is added, so a replayed report must not add twice.
        once(
            &hub.ops,
            op,
            &actor.replay_key,
            "Usage/Report",
            true,
            || async {
                hub.store
                    .report(
                        &repo.as_str().to_ascii_lowercase(),
                        &clock::day(at),
                        msg.storage_bytes,
                        msg.bandwidth_bytes,
                        msg.byo_storage,
                    )
                    .await
                    .map_err(db)?;
                Ok(wire::ReportUsageResponse {})
            },
        )
        .await
        .map(Response::new)
    }
}
