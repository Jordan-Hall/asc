//! Tests of the organisation settings service.

use std::sync::Arc;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::org_settings_server::OrgSettings;
use tonic::Code;

use super::OrgHub;
use super::rpc::OrgService;
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

struct Fx {
    k: Kit,
    hub: Arc<OrgHub>,
    svc: OrgService,
    audit: Arc<Recorder>,
}

async fn fx() -> Fx {
    let k = kit().await;
    let audit = Arc::new(Recorder::default());
    let hub = OrgHub::new(k.pool.clone(), audit.clone());
    let svc = OrgService::new(hub.clone(), k.authz.clone(), ServerInfo::default());
    Fx { k, hub, svc, audit }
}

fn on<T>(f: &Fx, msg: T, org: &str, role: Role, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        "9:owner",
        role,
        &format!("org:{org}"),
    )
}

fn root<T>(f: &Fx, msg: T, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        "1:root",
        Role::Admin,
        "instance",
    )
}

fn agent(name: &str) -> wire::OrgAgent {
    wire::OrgAgent {
        name: name.into(),
        display_name: "Fixer".into(),
        run_by: "Priya".into(),
        paths: vec!["src/**".into()],
        repos: vec!["acme/app".into()],
        ..Default::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn defaults_then_the_agents_card_is_set_by_owners_and_audited() {
    let f = fx().await;
    let get = |org: &str, role, n| {
        f.svc.get_org_settings(on(
            &f,
            wire::GetOrgSettingsRequest { org: org.into() },
            org,
            role,
            n,
        ))
    };
    let s = get("acme", Role::Read, 1).await.unwrap().into_inner();
    let a = s.agents.unwrap();
    assert!(
        a.allow_agents && a.agent_changesets_need_person,
        "agents are on and a person approves by default"
    );
    assert!(!s.merge.unwrap().set);
    let set = |org: &str, role, n| {
        f.svc.set_agent_policy(on(
            &f,
            wire::SetOrgAgentPolicyRequest {
                org: org.into(),
                agents: Some(wire::OrgAgentPolicy {
                    allow_agents: true,
                    agent_changesets_need_person: false,
                    monthly_spend_ceiling_micro_usd: 50_000_000,
                }),
            },
            org,
            role,
            n,
        ))
    };
    // A member who only reads or writes cannot change it.
    for role in [Role::Read, Role::Write] {
        assert_eq!(
            set("acme", role, 2).await.unwrap_err().code(),
            Code::PermissionDenied
        );
    }
    let out = set("acme", Role::Admin, 3)
        .await
        .unwrap()
        .into_inner()
        .settings
        .unwrap();
    assert_eq!(
        out.agents.unwrap().monthly_spend_ceiling_micro_usd,
        50_000_000
    );
    assert_eq!(out.effective_spend_ceiling_micro_usd, 50_000_000);
    let rec = f.audit.of("org.agents.updated");
    assert_eq!(
        (rec.len(), rec[0].1.as_str(), rec[0].2.as_deref()),
        (1, "9:owner", Some("host-service"))
    );
    // Another organisation is untouched.
    let other = get("zed", Role::Read, 4).await.unwrap().into_inner();
    assert!(other.agents.unwrap().agent_changesets_need_person);
    // A claim for another organisation, and a direct token, are refused.
    let e = f
        .svc
        .get_org_settings(on(
            &f,
            wire::GetOrgSettingsRequest { org: "acme".into() },
            "zed",
            Role::Admin,
            5,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = f
        .svc
        .get_org_settings(req(
            wire::GetOrgSettingsRequest { org: "acme".into() },
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = f
        .svc
        .get_org_settings(on(
            &f,
            wire::GetOrgSettingsRequest {
                org: "bad name".into(),
            },
            "acme",
            Role::Read,
            6,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "current_thread")]
async fn the_agent_gate_combines_the_instance_and_the_organisation() {
    let f = fx().await;
    let g = f.hub.agent_gate("acme").await.unwrap();
    assert!(g.allowed && g.need_person && g.ceiling == 0);
    // The instance sets a default budget; the organisation's smaller ceiling wins, a larger one does not.
    let inst = |allow, budget, n| {
        f.svc.set_instance_agents(root(
            &f,
            wire::SetInstanceAgentsRequest {
                allow_agents: allow,
                default_budget_micro_usd: budget,
            },
            n,
        ))
    };
    inst(true, 20_000_000, 1).await.unwrap();
    assert_eq!(f.hub.agent_gate("acme").await.unwrap().ceiling, 20_000_000);
    let set_org = |ceiling, n| {
        f.svc.set_agent_policy(on(
            &f,
            wire::SetOrgAgentPolicyRequest {
                org: "acme".into(),
                agents: Some(wire::OrgAgentPolicy {
                    allow_agents: true,
                    agent_changesets_need_person: true,
                    monthly_spend_ceiling_micro_usd: ceiling,
                }),
            },
            "acme",
            Role::Admin,
            n,
        ))
    };
    set_org(5_000_000, 2).await.unwrap();
    assert_eq!(f.hub.agent_gate("acme").await.unwrap().ceiling, 5_000_000);
    set_org(90_000_000, 3).await.unwrap();
    assert_eq!(
        f.hub.agent_gate("acme").await.unwrap().ceiling,
        20_000_000,
        "the organisation cannot raise the instance default"
    );
    // Instance off means off everywhere.
    inst(false, 0, 4).await.unwrap();
    assert!(!f.hub.agent_gate("acme").await.unwrap().allowed);
    let got = f
        .svc
        .get_org_settings(on(
            &f,
            wire::GetOrgSettingsRequest { org: "acme".into() },
            "acme",
            Role::Read,
            5,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!got.instance_allows_agents);
    // Only the site administrator touches the instance.
    let e = f
        .svc
        .set_instance_agents(on(
            &f,
            wire::SetInstanceAgentsRequest::default(),
            "acme",
            Role::Admin,
            6,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let e = f
        .svc
        .get_instance_agents(req(wire::GetInstanceAgentsRequest {}, "alice-token", None))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(
        f.svc
            .get_instance_agents(root(&f, wire::GetInstanceAgentsRequest {}, 7))
            .await
            .is_ok()
    );
    assert_eq!(f.audit.of("instance.agents.updated").len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn merge_defaults_are_validated_stored_and_resolved_against_a_repository() {
    let f = fx().await;
    let defaults = wire::MergeDefaults {
        require_approvals: 2,
        require_checks: vec!["lint".into(), "build".into(), "build".into()],
        require_signed: true,
        strategy: "rebase-disjoint".into(),
        ..Default::default()
    };
    let set = |d: wire::MergeDefaults, org: &str, role, n| {
        f.svc.set_merge_defaults(on(
            &f,
            wire::SetOrgMergeDefaultsRequest {
                org: org.into(),
                merge: Some(d),
            },
            org,
            role,
            n,
        ))
    };
    assert_eq!(
        set(defaults.clone(), "acme", Role::Write, 1)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    for bad in [
        wire::MergeDefaults {
            require_approvals: 500,
            ..Default::default()
        },
        wire::MergeDefaults {
            strategy: "squash".into(),
            ..Default::default()
        },
        wire::MergeDefaults {
            require_checks: vec![String::new()],
            ..Default::default()
        },
    ] {
        assert_eq!(
            set(bad, "acme", Role::Admin, 2).await.unwrap_err().code(),
            Code::InvalidArgument
        );
    }
    let out = set(defaults, "acme", Role::Admin, 3)
        .await
        .unwrap()
        .into_inner()
        .settings
        .unwrap()
        .merge
        .unwrap();
    assert!(out.set);
    assert_eq!(
        out.require_checks,
        ["build", "lint"],
        "sorted and without duplicates"
    );
    // A repository under it: looser values are tightened and named; stricter ones stay.
    let resolve = |repo: &str, own: wire::MergeDefaults, n| {
        f.svc.resolve_merge_policy(on(
            &f,
            wire::ResolveMergePolicyRequest {
                org: "acme".into(),
                repo: repo.into(),
                repo_values: Some(own),
            },
            "acme",
            Role::Read,
            n,
        ))
    };
    let r = resolve(
        "Acme/App",
        wire::MergeDefaults {
            require_approvals: 1,
            allow_force: true,
            ..Default::default()
        },
        4,
    )
    .await
    .unwrap()
    .into_inner();
    let e = r.effective.unwrap();
    assert_eq!(
        (e.require_approvals, e.allow_force, e.require_signed),
        (2, false, true)
    );
    assert!(
        r.tightened.contains(&"require_approvals".to_owned())
            && r.tightened.contains(&"allow_force".to_owned())
    );
    let r = resolve(
        "acme/app",
        wire::MergeDefaults {
            require_approvals: 5,
            ..Default::default()
        },
        5,
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(r.effective.unwrap().require_approvals, 5);
    // Another organisation's repository is not resolved here.
    assert_eq!(
        resolve("zed/app", Default::default(), 6)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        resolve("acme/a/b", Default::default(), 7)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // The land gate's hook: the requirements a repository states, under the defaults.
    let d = f.hub.merge_defaults("acme").await.unwrap();
    let own = asc_core::policy::LandRequirements {
        approvals: 1,
        checks: vec![],
        signed: false,
        strategy: asc_core::policy::Strategy::Rebase,
    };
    let got = super::policy::apply(Some(&d), own);
    assert_eq!((got.approvals, got.signed), (2, true));
    assert!(f.hub.merge_defaults("zed").await.is_none());
    assert_eq!(f.audit.of("org.merge.updated").len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn org_agents_are_created_listed_paused_and_deleted_within_their_organisation() {
    let f = fx().await;
    let create = |org: &str, a: wire::OrgAgent, role, n| {
        f.svc.create_agent(on(
            &f,
            wire::CreateOrgAgentRequest {
                org: org.into(),
                agent: Some(a),
            },
            org,
            role,
            n,
        ))
    };
    let made = create("acme", agent("fixer"), Role::Admin, 1)
        .await
        .unwrap()
        .into_inner()
        .agent
        .unwrap();
    assert_eq!(
        (made.name.as_str(), made.run_by.as_str()),
        ("fixer", "priya")
    );
    assert_eq!(
        create("acme", agent("fixer"), Role::Admin, 2)
            .await
            .unwrap_err()
            .code(),
        Code::AlreadyExists
    );
    assert_eq!(
        create("acme", agent("late"), Role::Write, 3)
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    for bad in [
        agent("Bad Name"),
        agent("-lead"),
        wire::OrgAgent {
            repos: vec!["zed/app".into()],
            ..agent("x")
        },
        wire::OrgAgent {
            repos: vec!["acme/a/b".into()],
            ..agent("x")
        },
        wire::OrgAgent {
            paths: vec!["/abs".into()],
            ..agent("x")
        },
        wire::OrgAgent {
            run_by: "no spaces".into(),
            ..agent("x")
        },
        wire::OrgAgent {
            paths: vec!["a".into(); 65],
            ..agent("x")
        },
    ] {
        assert_eq!(
            create("acme", bad, Role::Admin, 4)
                .await
                .unwrap_err()
                .code(),
            Code::InvalidArgument
        );
    }
    // The same name in another organisation is another agent.
    create(
        "zed",
        wire::OrgAgent {
            repos: vec![],
            ..agent("fixer")
        },
        Role::Admin,
        5,
    )
    .await
    .unwrap();
    let list = |org: &str, n| {
        f.svc.list_agents(on(
            &f,
            wire::ListOrgAgentsRequest { org: org.into() },
            org,
            Role::Read,
            n,
        ))
    };
    assert_eq!(list("acme", 6).await.unwrap().into_inner().agents.len(), 1);
    // Update: pause it and replace its paths.
    let upd = |org: &str, n| {
        f.svc.update_agent(on(
            &f,
            wire::UpdateOrgAgentRequest {
                org: org.into(),
                name: "fixer".into(),
                state: Some(wire::OrgAgentState::Paused as i32),
                replace_paths: true,
                paths: vec!["docs/**".into()],
                ..Default::default()
            },
            org,
            Role::Admin,
            n,
        ))
    };
    let u = upd("acme", 7).await.unwrap().into_inner().agent.unwrap();
    assert_eq!(
        (u.state, u.paths.as_slice()),
        (
            wire::OrgAgentState::Paused as i32,
            ["docs/**".to_owned()].as_slice()
        )
    );
    // The organisation's owner cannot touch another organisation's agent: the name is looked up in their own.
    let e = f
        .svc
        .update_agent(on(
            &f,
            wire::UpdateOrgAgentRequest {
                org: "acme".into(),
                name: "nobody".into(),
                ..Default::default()
            },
            "acme",
            Role::Admin,
            8,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    assert_eq!(
        list("zed", 9).await.unwrap().into_inner().agents[0].state,
        wire::OrgAgentState::Active as i32
    );
    let del = |org: &str, n| {
        f.svc.delete_agent(on(
            &f,
            wire::DeleteOrgAgentRequest {
                org: org.into(),
                name: "fixer".into(),
            },
            org,
            Role::Admin,
            n,
        ))
    };
    del("acme", 10).await.unwrap();
    assert_eq!(del("acme", 11).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(list("zed", 12).await.unwrap().into_inner().agents.len(), 1);
    assert_eq!(f.audit.of("org.agent.created").len(), 2);
    // The count is bounded.
    for i in 0..super::MAX_AGENTS {
        f.hub
            .create_agent(
                "big",
                super::AgentRow {
                    name: format!("a{i}"),
                    display: String::new(),
                    run_by: String::new(),
                    paths: vec![],
                    repos: vec![],
                    paused: false,
                    created: 0,
                    created_by: String::new(),
                    updated: 0,
                },
                "x",
            )
            .await
            .unwrap();
    }
    let e = f
        .hub
        .create_agent(
            "big",
            super::AgentRow {
                name: "one-more".into(),
                display: String::new(),
                run_by: String::new(),
                paths: vec![],
                repos: vec![],
                paused: false,
                created: 0,
                created_by: String::new(),
                updated: 0,
            },
            "x",
        )
        .await
        .unwrap_err();
    assert!(matches!(e, super::OrgError::TooMany));
}
