//! Tests of the usage service: the four measures, visibility, quotas and retention.

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::usage_server::Usage;
use sqlx::AnyPool;
use tonic::Code;

use super::rpc::UsageService;
use super::{Period, UsageConfig, UsageHub};
use crate::ai::clock;
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

struct Fx {
    k: Kit,
    hub: std::sync::Arc<UsageHub>,
    svc: UsageService,
    audit: std::sync::Arc<Recorder>,
}

async fn fx() -> Fx {
    let k = kit().await;
    let audit = std::sync::Arc::new(Recorder::default());
    let hub = UsageHub::new(k.pool.clone(), UsageConfig::default(), audit.clone());
    let svc = UsageService::new(hub.clone(), k.authz.clone(), ServerInfo::default());
    Fx { k, hub, svc, audit }
}

fn owner(kind: wire::OwnerKind, name: &str) -> Option<wire::OwnerRef> {
    Some(wire::OwnerRef {
        kind: kind as i32,
        name: name.into(),
    })
}

async fn token(pool: &AnyPool, id: &str, repo: &str, cost: i64) {
    sqlx::query(
        "INSERT INTO agent_tokens (id, repo, name, agent, secret_sha256, secret_hint, created_by, owner_id, created, expires_at, used_cost) \
         VALUES ($1, $2, 'n', 'a', $1, 'h', 'x', 1, 1, 9999999999, $3)",
    )
    .bind(id.to_owned())
    .bind(repo.to_owned())
    .bind(cost)
    .execute(pool)
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn run(
    pool: &AnyPool,
    id: i64,
    repo: &str,
    starter: &str,
    started: i64,
    finished: i64,
    token_id: &str,
    key: &str,
) {
    sqlx::query(
        "INSERT INTO agent_runs (id, repo, agent, state, target, base, task, created_by, starter, created, started, finished, token_id, ai_key_id) \
         VALUES ($1, $2, 'fixer', 4, 'main', $3, 't', $4, $4, $5, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(repo.to_owned())
    .bind(Vec::<u8>::new())
    .bind(starter.to_owned())
    .bind(started)
    .bind(finished)
    .bind(token_id.to_owned())
    .bind(key.to_owned())
    .execute(pool)
    .await
    .unwrap();
}

async fn ai_key(pool: &AnyPool, id: &str, kind: i64, owner: &str) {
    sqlx::query(
        "INSERT INTO ai_keys (id, owner_kind, owner_name, label, provider, sealed_key, key_gen, state, created, updated) \
         VALUES ($1, $2, $3, $1, 4, $4, 1, 2, 1, 1)",
    )
    .bind(id.to_owned())
    .bind(kind)
    .bind(owner.to_owned())
    .bind(vec![0u8; 4])
    .execute(pool)
    .await
    .unwrap();
}

async fn ai_usage(
    pool: &AnyPool,
    key: &str,
    kind: i64,
    owner: &str,
    day: &str,
    repo: &str,
    cost: i64,
) {
    sqlx::query(
        "INSERT INTO ai_usage (key_id, owner_kind, owner_name, day, repo, agent, model, requests, input_tokens, output_tokens, cost_micro_usd) \
         VALUES ($1, $2, $3, $4, $5, 'fixer', 'm', 3, 100, 50, $6)",
    )
    .bind(key.to_owned())
    .bind(kind)
    .bind(owner.to_owned())
    .bind(day.to_owned())
    .bind(repo.to_owned())
    .bind(cost)
    .execute(pool)
    .await
    .unwrap();
}

async fn spend(pool: &AnyPool, repo: &str, day: &str, cost: i64) {
    sqlx::query("INSERT INTO agent_spend (repo, day, agent, tokens, cost, requests) VALUES ($1, $2, 'fixer', 10, $3, 2)")
        .bind(repo.to_owned())
        .bind(day.to_owned())
        .bind(cost)
        .execute(pool)
        .await
        .unwrap();
}

/// Alice: runs in acme/web (60 min, $2) and alice/lab (30 min, $1, her own key); acme also has
/// a run by bob (120 min, $5) and its own storage.
async fn seed(f: &Fx, now: i64) {
    let p = &f.k.pool;
    let day = clock::day(now);
    ai_key(p, "ak_alice", 1, "alice").await;
    ai_key(p, "ak_acme", 2, "acme").await;
    token(p, "t1", "acme/web", 2_000_000).await;
    token(p, "t2", "alice/lab", 1_000_000).await;
    token(p, "t3", "acme/web", 5_000_000).await;
    let start = clock::month_start(now) + 3600;
    run(p, 1, "acme/web", "alice", start, start + 3600, "t1", "").await;
    run(
        p,
        2,
        "alice/lab",
        "alice",
        start,
        start + 1800,
        "t2",
        "ak_alice",
    )
    .await;
    run(p, 3, "acme/web", "bob", start, start + 7200, "t3", "").await;
    spend(p, "acme/web", &day, 7_000_000).await;
    spend(p, "alice/lab", &day, 1_000_000).await;
    ai_usage(p, "ak_acme", 2, "acme", &day, "acme/web", 4_000_000).await;
    ai_usage(p, "ak_alice", 1, "alice", &day, "alice/lab", 1_000_000).await;
    f.hub
        .store
        .report("acme/web", &day, Some(1_200_000), 5_000, false)
        .await
        .unwrap();
    f.hub
        .store
        .report("acme/web", &day, None, 2_500, false)
        .await
        .unwrap();
    f.hub
        .store
        .report("alice/lab", &day, Some(120_000), 400, true)
        .await
        .unwrap();
    f.hub
        .store
        .report("other/zed", &day, Some(9_999), 99, false)
        .await
        .unwrap();
}

fn as_user<T>(f: &Fx, msg: T, login: &str, role: Role) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(1))),
        &f.k,
        &format!("7:{login}"),
        role,
        &format!("user:{login}"),
    )
}

fn as_org<T>(f: &Fx, msg: T, org: &str, role: Role, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        "9:owner",
        role,
        &format!("org:{org}"),
    )
}

fn as_admin<T>(f: &Fx, msg: T, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        "1:root",
        Role::Admin,
        "instance",
    )
}

fn get(o: Option<wire::OwnerRef>) -> wire::GetUsageRequest {
    wire::GetUsageRequest {
        owner: o,
        period: String::new(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_person_sees_their_own_four_measures_and_repository_rows() {
    let f = fx().await;
    let now = clock::now();
    seed(&f, now).await;
    let r = f
        .svc
        .get_usage(as_user(
            &f,
            get(owner(wire::OwnerKind::User, "alice")),
            "alice",
            Role::Read,
        ))
        .await
        .unwrap()
        .into_inner();
    // Storage: only repositories alice owns; her BYO repository does not count.
    assert_eq!(r.storage.unwrap().used, 0);
    assert!(r.byo_storage);
    assert_eq!(r.bandwidth.unwrap().used, 400);
    // Minutes: her runs anywhere, 60 + 30.
    assert_eq!(r.agent_minutes.unwrap().used, 90);
    // Spend: her runs, $2 + $1; $1 of it on her own key.
    let ai = r.ai_spend.unwrap();
    assert_eq!((ai.used, r.byo_ai_spend_micro_usd), (3_000_000, 1_000_000));
    assert_eq!(r.ai_keys, 1);
    let repos: Vec<_> = r.repos.iter().map(|x| x.repo.as_str()).collect();
    assert_eq!(repos, ["acme/web", "alice/lab"]);
    assert!(!r.empty && !r.over_quota);
    // Never another owner's repository or run.
    assert!(!repos.contains(&"other/zed"));
    // The board of someone with nothing is empty, not an error.
    let e = f
        .svc
        .get_usage(as_user(
            &f,
            get(owner(wire::OwnerKind::User, "dora")),
            "dora",
            Role::Read,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(e.empty);
    assert!(e.resets_at > e.period_start);
}

#[tokio::test(flavor = "current_thread")]
async fn organisation_usage_is_for_its_owners_and_other_tenants_are_refused() {
    let f = fx().await;
    seed(&f, clock::now()).await;
    let org = || get(owner(wire::OwnerKind::Org, "acme"));
    let r = f
        .svc
        .get_usage(as_org(&f, org(), "acme", Role::Admin, 1))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.storage.unwrap().used, 1_200_000);
    assert_eq!(r.bandwidth.unwrap().used, 7_500);
    assert_eq!(r.agent_minutes.unwrap().used, 180);
    assert_eq!(r.ai_spend.unwrap().used, 7_000_000);
    assert_eq!(r.byo_ai_spend_micro_usd, 4_000_000);
    assert_eq!(r.repos.len(), 1);
    // A member (write) is not an owner; a claim for another organisation is not this one.
    for (role, org_claim) in [(Role::Write, "acme"), (Role::Admin, "zed")] {
        let e = f
            .svc
            .get_usage(as_org(&f, org(), org_claim, role, 2))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::PermissionDenied, "{org_claim}");
    }
    // A person's own token cannot read an organisation, nor someone else's account.
    let e = f
        .svc
        .get_usage(req(org(), "alice-token", None))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = f
        .svc
        .get_usage(req(
            get(owner(wire::OwnerKind::User, "bob")),
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // Her own account by her own token works.
    assert!(
        f.svc
            .get_usage(req(
                get(owner(wire::OwnerKind::User, "alice")),
                "alice-token",
                None
            ))
            .await
            .is_ok()
    );
    // The claim of one person does not read another's account.
    let e = f
        .svc
        .get_usage(as_user(
            &f,
            get(owner(wire::OwnerKind::User, "bob")),
            "alice",
            Role::Admin,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // The instance totals are the administrator's.
    let all = f
        .svc
        .get_usage(as_admin(&f, get(owner(wire::OwnerKind::Instance, "")), 3))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.repos.len(), 3);
    assert_eq!(all.agent_minutes.unwrap().used, 210);
    let e = f
        .svc
        .get_usage(as_org(
            &f,
            get(owner(wire::OwnerKind::Instance, "")),
            "acme",
            Role::Admin,
            4,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "current_thread")]
async fn quotas_are_set_by_the_site_administrator_and_meters_go_over() {
    let f = fx().await;
    seed(&f, clock::now()).await;
    let set = |o, q: wire::UsageQuota, n| {
        f.svc.set_quota(as_admin(
            &f,
            wire::SetQuotaRequest {
                owner: o,
                quota: Some(q),
            },
            n,
        ))
    };
    // The default for everyone, then an override for acme.
    set(
        owner(wire::OwnerKind::Instance, ""),
        wire::UsageQuota {
            storage_bytes: 5_000_000_000,
            agent_minutes: 600,
            ai_spend_micro_usd: 35_000_000,
            bandwidth_bytes: 100_000_000_000,
        },
        1,
    )
    .await
    .unwrap();
    let q = set(
        owner(wire::OwnerKind::Org, "acme"),
        wire::UsageQuota {
            agent_minutes: 100,
            ..Default::default()
        },
        2,
    )
    .await
    .unwrap()
    .into_inner()
    .quota
    .unwrap();
    let eff = q.effective.unwrap();
    assert_eq!(
        (eff.agent_minutes, eff.storage_bytes),
        (100, 5_000_000_000),
        "an override, else the default"
    );
    assert_eq!(q.r#override.unwrap().storage_bytes, 0);
    let r = f
        .svc
        .get_usage(as_org(
            &f,
            get(owner(wire::OwnerKind::Org, "acme")),
            "acme",
            Role::Admin,
            3,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(r.agent_minutes.unwrap().over, "180 of 100 minutes");
    assert!(r.over_quota);
    assert!(
        !r.ai_spend.unwrap().over,
        "the org's own key spend does not count: 7 - 4 < 35"
    );
    // Only the site administrator sets a quota; the organisation's owner does not.
    let e = f
        .svc
        .set_quota(as_org(
            &f,
            wire::SetQuotaRequest {
                owner: owner(wire::OwnerKind::Org, "acme"),
                quota: Some(Default::default()),
            },
            "acme",
            Role::Admin,
            9,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    // The owner reads their limits.
    let g = f
        .svc
        .get_quota(as_org(
            &f,
            wire::GetQuotaRequest {
                owner: owner(wire::OwnerKind::Org, "acme"),
            },
            "acme",
            Role::Admin,
            10,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(g.effective.unwrap().agent_minutes, 100);
    assert_eq!(f.audit.of("usage.quota.set").len(), 2);
    assert_eq!(
        f.audit.of("usage.quota.set")[0].2.as_deref(),
        Some("host-service")
    );
    let e = set(
        Some(wire::OwnerRef {
            kind: wire::OwnerKind::Repo as i32,
            name: "a/b".into(),
        }),
        Default::default(),
        11,
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "current_thread")]
async fn spend_is_grouped_by_key_user_repository_and_period() {
    let f = fx().await;
    seed(&f, clock::now()).await;
    let list =
        |o,
         g: wire::SpendGroup,
         req_: fn(&Fx, wire::ListSpendRequest) -> tonic::Request<wire::ListSpendRequest>| {
            let msg = wire::ListSpendRequest {
                owner: o,
                group_by: g as i32,
                since: 0,
                until: 0,
            };
            f.svc.list_spend(req_(&f, msg))
        };
    let org = |f: &Fx, m| as_org(f, m, "acme", Role::Admin, 1);
    let keys = list(
        owner(wire::OwnerKind::Org, "acme"),
        wire::SpendGroup::Key,
        org,
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(keys.rows.len(), 1);
    assert_eq!(
        (
            keys.rows[0].group.as_str(),
            keys.rows[0].cost_micro_usd,
            keys.rows[0].input_tokens
        ),
        ("ak_acme", 4_000_000, 100)
    );
    let users = list(
        owner(wire::OwnerKind::Org, "acme"),
        wire::SpendGroup::User,
        org,
    )
    .await
    .unwrap()
    .into_inner();
    let by: Vec<_> = users
        .rows
        .iter()
        .map(|r| (r.group.as_str(), r.cost_micro_usd))
        .collect();
    assert_eq!(
        by,
        [("bob", 5_000_000), ("alice", 2_000_000)],
        "per person, largest first"
    );
    assert_eq!(users.total.unwrap().cost_micro_usd, 7_000_000);
    let months = list(
        owner(wire::OwnerKind::Org, "acme"),
        wire::SpendGroup::Month,
        org,
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(months.rows.len(), 1);
    assert_eq!(months.rows[0].group, clock::month(clock::now()));
    // A person's own report holds only themself.
    let mine = list(
        owner(wire::OwnerKind::User, "alice"),
        wire::SpendGroup::User,
        |f, m| as_user(f, m, "alice", Role::Read),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(mine.rows.len(), 1);
    assert_eq!(mine.rows[0].group, "alice");
    // Out-of-range windows are refused.
    let bad = f
        .svc
        .list_spend(as_org(
            &f,
            wire::ListSpendRequest {
                owner: owner(wire::OwnerKind::Org, "acme"),
                group_by: 0,
                since: 1,
                until: 1 + 500 * 86_400,
            },
            "acme",
            Role::Admin,
            5,
        ))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_host_reports_usage_counters_are_bounded_and_swept() {
    let f = fx().await;
    let rep = |repo: &str, at: i64, n| {
        f.svc.report(as_admin(
            &f,
            wire::ReportUsageRequest {
                repo: repo.into(),
                storage_bytes: Some(10),
                bandwidth_bytes: 5,
                byo_storage: false,
                at,
            },
            n,
        ))
    };
    rep("acme/web", 0, 1).await.unwrap();
    // A person, an organisation owner and a bad repository name.
    let e = f
        .svc
        .report(req(
            wire::ReportUsageRequest {
                repo: "alice/x".into(),
                ..Default::default()
            },
            "alice-token",
            Some(&op(2)),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert_eq!(
        rep("not a repo", 0, 3).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // Too old, too far ahead.
    assert_eq!(
        rep("acme/web", 5, 4).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        rep("acme/web", clock::now() + 10 * 86_400, 5)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // Old rows go with the sweep.
    f.hub
        .store
        .report("old/repo", "2000-01-01", Some(1), 1, false)
        .await
        .unwrap();
    assert_eq!(f.hub.sweep().await, 1);
    f.hub.add_bandwidth("acme/web", 1000).await;
    let now = clock::now();
    let t = f
        .hub
        .totals(
            &crate::settings::access::Subject::Org("acme".into()),
            Period::parse("", now).unwrap(),
            now,
        )
        .await
        .unwrap();
    assert_eq!(t.bandwidth_bytes, 1005);
    assert_eq!(t.storage_bytes, 10);
}

#[test]
fn periods_are_calendar_months_and_never_in_the_future() {
    let now = 1_792_983_600; // 2026-10-26
    let p = Period::parse("2026-09", now).unwrap();
    assert_eq!(p.label(), "2026-09");
    assert_eq!(p.end - p.start, 30 * 86_400);
    assert_eq!(Period::parse("", now).unwrap().label(), "2026-10");
    assert_eq!(
        Period::parse("2026-12", now).unwrap_err().code(),
        Code::InvalidArgument
    );
    for bad in [
        "2026",
        "2026-13",
        "26-10",
        "2026-1",
        "abcd-ef",
        "2026-10-01",
    ] {
        assert!(Period::parse(bad, now).is_err(), "{bad}");
    }
    let dec = Period::parse("2025-12", now).unwrap();
    assert_eq!(clock::month(dec.end), "2026-01");
}

// ------------------------------------------------------------------------------ bandwidth metering

async fn month_bandwidth(f: &Fx, org: &str) -> u64 {
    let now = clock::now();
    f.hub
        .totals(
            &crate::settings::access::Subject::Org(org.into()),
            Period::parse("", now).unwrap(),
            now,
        )
        .await
        .unwrap()
        .bandwidth_bytes
}

async fn settled(f: &Fx, org: &str, want: u64) -> u64 {
    for _ in 0..100 {
        let got = month_bandwidth(f, org).await;
        if got >= want {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    month_bandwidth(f, org).await
}

fn chunks(n: usize) -> impl futures::Stream<Item = Vec<u8>> + Unpin {
    futures::stream::iter((0..n).map(|_| vec![0u8; 1000]))
}

#[tokio::test(flavor = "current_thread")]
async fn a_stream_cut_off_after_k_bytes_counts_k_and_a_finished_one_counts_all_once() {
    use futures::StreamExt as _;
    let f = fx().await;
    // Read three of ten chunks, then drop the stream.
    let mut s = super::meter::Metered::with_flush(
        chunks(10),
        f.hub.clone(),
        "acme/web".into(),
        |c: &Vec<u8>| c.len() as u64,
        u64::MAX,
    );
    for _ in 0..3 {
        s.next().await.unwrap();
    }
    drop(s);
    assert_eq!(settled(&f, "acme", 3000).await, 3000);
    // Read everything, with a small flush interval: every byte once, none twice.
    let mut s = super::meter::Metered::with_flush(
        chunks(10),
        f.hub.clone(),
        "acme/web".into(),
        |c: &Vec<u8>| c.len() as u64,
        2500,
    );
    while s.next().await.is_some() {}
    drop(s);
    assert_eq!(settled(&f, "acme", 13_000).await, 13_000);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        month_bandwidth(&f, "acme").await,
        13_000,
        "nothing is counted twice"
    );
    // A stream that never yields counts nothing; another repository is its own.
    drop(super::meter::Metered::new(
        chunks(0),
        f.hub.clone(),
        "zed/x".into(),
        |c: &Vec<u8>| c.len() as u64,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(month_bandwidth(&f, "zed").await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn counters_saturate_and_never_overflow() {
    let f = fx().await;
    let day = clock::day(clock::now());
    f.hub
        .store
        .report("acme/a", &day, None, u64::MAX, false)
        .await
        .unwrap();
    f.hub
        .store
        .report("acme/a", &day, None, u64::MAX, false)
        .await
        .unwrap();
    f.hub
        .store
        .report("acme/b", &day, None, 5, false)
        .await
        .unwrap();
    assert_eq!(
        month_bandwidth(&f, "acme").await,
        u64::try_from(i64::MAX).unwrap() + 5
    );
}
