//! Tests of the path rules service.

use std::sync::Arc;

use asc_core::pathacl::Level;
use asc_core::policy::{Kind, Who};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::path_rules_server::PathRules;
use tonic::Code;

use super::PathRulesHub;
use super::rpc::PathRulesService;
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

struct Fx {
    k: Kit,
    hub: Arc<PathRulesHub>,
    svc: PathRulesService,
    audit: Arc<Recorder>,
}

async fn fx() -> Fx {
    let k = kit().await;
    let audit = Arc::new(Recorder::default());
    let hub = PathRulesHub::new(k.pool.clone(), audit.clone());
    let svc = PathRulesService::new(hub.clone(), k.authz.clone(), ServerInfo::default());
    Fx { k, hub, svc, audit }
}

fn rule(dir: &str, subject: &str, level: &str) -> wire::PathRule {
    wire::PathRule {
        dir: dir.into(),
        subject: subject.into(),
        level: level.into(),
    }
}

fn as_admin<T>(f: &Fx, msg: T, subject: &str, role: Role, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        "7:alice",
        role,
        subject,
    )
}

fn set(repo: &str, rules: Vec<wire::PathRule>, expected: u64) -> wire::SetPathRulesRequest {
    wire::SetPathRulesRequest {
        repo: repo.into(),
        rules,
        expected_revision: expected,
    }
}

fn list(repo: &str) -> wire::ListPathRulesRequest {
    wire::ListPathRulesRequest { repo: repo.into() }
}

#[tokio::test(flavor = "current_thread")]
async fn an_administrator_replaces_rules_by_revision_and_every_change_is_audited() {
    let f = fx().await;
    let rules = vec![
        rule("", "*", "read"),
        rule("secret", "*", "none"),
        rule("secret", "team:core", "write"),
    ];
    let r = f
        .svc
        .set_path_rules(as_admin(
            &f,
            set("acme/mono", rules.clone(), 0),
            "acme/mono",
            Role::Admin,
            1,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.revision, 1);
    let got = f
        .svc
        .list_path_rules(as_admin(&f, list("Acme/Mono"), "acme/mono", Role::Admin, 2))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.rules, rules, "kept in the order set, names lower-cased");
    assert_eq!((got.revision, got.updated_by.as_str()), (1, "7:alice"));
    // A stale writer is refused and nothing changes.
    let stale = f
        .svc
        .set_path_rules(as_admin(
            &f,
            set("acme/mono", vec![], 0),
            "acme/mono",
            Role::Admin,
            3,
        ))
        .await
        .unwrap_err();
    assert_eq!(stale.code(), Code::FailedPrecondition);
    assert_eq!(f.hub.get("acme/mono").await.unwrap().rules.len(), 3);
    // The right revision replaces the whole set; the same op id replays the first answer.
    let again = || {
        f.svc.set_path_rules(as_admin(
            &f,
            set("acme/mono", vec![rule("docs", "user:bob", "write")], 1),
            "acme/mono",
            Role::Admin,
            4,
        ))
    };
    assert_eq!(again().await.unwrap().into_inner().revision, 2);
    assert_eq!(again().await.unwrap().into_inner().revision, 2);
    assert_eq!(f.hub.get("acme/mono").await.unwrap().revision, 2);
    let actions: Vec<String> = f
        .audit
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.0.clone())
        .collect();
    assert_eq!(actions, ["pathrules.set", "pathrules.set"]);
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_rules_are_refused_naming_the_rule_and_nothing_is_stored() {
    let f = fx().await;
    for (n, bad) in [
        rule("src/*", "*", "read"),
        rule("../x", "*", "read"),
        rule("/abs", "*", "read"),
        rule("a", "everyone", "read"),
        rule("a", "*", "owner"),
    ]
    .into_iter()
    .enumerate()
    {
        let e = f
            .svc
            .set_path_rules(as_admin(
                &f,
                set("acme/mono", vec![rule("", "*", "read"), bad], 0),
                "acme/mono",
                Role::Admin,
                10 + u32::try_from(n).unwrap(),
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);
        assert!(e.message().starts_with("rule 2:"), "{}", e.message());
    }
    let dup = f
        .svc
        .set_path_rules(as_admin(
            &f,
            set(
                "acme/mono",
                vec![rule("a", "*", "read"), rule("a", "*", "none")],
                0,
            ),
            "acme/mono",
            Role::Admin,
            20,
        ))
        .await
        .unwrap_err();
    assert_eq!(dup.code(), Code::InvalidArgument);
    assert_eq!(f.hub.get("acme/mono").await.unwrap().revision, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn only_repository_administrators_see_or_change_rules() {
    let f = fx().await;
    // A claim with a lower role, a claim for another repository, a person's own token without
    // admin access: all refused, before an op id is looked at.
    for r in [
        as_admin(&f, list("acme/mono"), "acme/mono", Role::Write, 1),
        as_admin(&f, list("acme/mono"), "acme/other", Role::Admin, 2),
        req(list("acme/mono"), "reader-token", None),
    ] {
        assert_eq!(
            f.svc.list_path_rules(r).await.unwrap_err().code(),
            Code::PermissionDenied
        );
    }
    assert_eq!(
        f.svc
            .list_path_rules(req(list("acme/mono"), "nobody", None))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert_eq!(
        f.svc
            .list_path_rules(req(list("no-slash"), "host-token", None))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    // A mutating call needs an op id.
    let no_op = claim(
        req(set("acme/mono", vec![], 0), "host-token", None),
        &f.k,
        "7:alice",
        Role::Admin,
        "acme/mono",
    );
    assert_eq!(
        f.svc.set_path_rules(no_op).await.unwrap_err().code(),
        Code::InvalidArgument
    );
}

#[tokio::test(flavor = "current_thread")]
async fn teams_resolve_per_organisation_and_check_previews_a_viewer() {
    let f = fx().await;
    f.svc
        .set_path_rules(as_admin(
            &f,
            set(
                "acme/mono",
                vec![
                    rule("", "*", "read"),
                    rule("secret", "*", "none"),
                    rule("secret", "team:core", "write"),
                    rule("secret/ci", "ci:*", "build"),
                ],
                0,
            ),
            "acme/mono",
            Role::Admin,
            1,
        ))
        .await
        .unwrap();
    let team = |org: &str, members: Vec<&str>, n| {
        f.svc.set_team_members(as_admin(
            &f,
            wire::SetTeamMembersRequest {
                org: org.into(),
                team: "core".into(),
                members: members.into_iter().map(String::from).collect(),
            },
            &format!("org:{org}"),
            Role::Admin,
            n,
        ))
    };
    team("acme", vec!["Bob", "agent:fixer", "bob"], 2)
        .await
        .unwrap();
    // The same team name in another organisation does not count for acme.
    team("other", vec!["carol"], 3).await.unwrap();
    let teams = f
        .svc
        .list_teams(as_admin(
            &f,
            wire::ListTeamsRequest { org: "acme".into() },
            "org:acme",
            Role::Admin,
            4,
        ))
        .await
        .unwrap()
        .into_inner()
        .teams;
    assert_eq!(teams.len(), 1);
    assert_eq!(
        teams[0].members,
        ["agent:fixer", "bob"],
        "sorted, deduplicated, logins lower case"
    );
    let check = |principal: &str, repo_level: &str, n| {
        f.svc.check_path_access(as_admin(
            &f,
            wire::CheckPathAccessRequest {
                repo: "acme/mono".into(),
                principal: principal.into(),
                repo_level: repo_level.into(),
                paths: vec![
                    "README".into(),
                    "secret/key.pem".into(),
                    "secret/ci/out".into(),
                ],
            },
            "acme/mono",
            Role::Admin,
            n,
        ))
    };
    let levels = |r: wire::CheckPathAccessResponse| {
        r.access
            .into_iter()
            .map(|a| (a.level, a.decided_by))
            .collect::<Vec<_>>()
    };
    let s = |l: &str, d: &str| (l.to_owned(), d.to_owned());
    let bob = check("user:bob", "write", 5).await.unwrap().into_inner();
    assert_eq!(bob.teams, ["core"]);
    assert_eq!(
        levels(bob),
        [s("read", ""), s("write", "secret"), s("write", "secret")],
        "the deepest directory with a rule matching the viewer decides"
    );
    let carol = check("user:carol", "admin", 6).await.unwrap().into_inner();
    assert_eq!(
        levels(carol),
        [s("read", ""), s("none", "secret"), s("none", "secret")]
    );
    let ci = check("ci:build-7", "read", 7).await.unwrap().into_inner();
    assert_eq!(
        levels(ci),
        [s("read", ""), s("none", "secret"), s("build", "secret/ci")]
    );
    // The repository level caps every rule.
    let capped = check("user:bob", "read", 8).await.unwrap().into_inner();
    assert_eq!(levels(capped)[1], s("read", "secret"));
    assert_eq!(
        check("bob", "read", 9).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(
        check("user:bob", "build", 10).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // Removing every member removes the team.
    team("acme", vec![], 11).await.unwrap();
    let v = f
        .hub
        .viewer("acme/mono", Who::new(Kind::User, "bob"), Level::Write)
        .await
        .unwrap();
    assert!(v.teams.is_empty());
    // An organisation claim of another organisation may not set teams.
    let e = f
        .svc
        .set_team_members(as_admin(
            &f,
            wire::SetTeamMembersRequest {
                org: "acme".into(),
                team: "core".into(),
                members: vec![],
            },
            "org:other",
            Role::Admin,
            12,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "current_thread")]
async fn the_compiled_rules_are_cached_and_a_change_here_is_seen_at_once() {
    let f = fx().await;
    assert!(f.hub.acl("acme/mono").await.unwrap().is_empty());
    let rules = [super::parse_rule("x", "*", "none").unwrap()];
    f.hub
        .set("acme/mono", &rules, 0, "pat:alice")
        .await
        .unwrap();
    let acl = f.hub.acl("ACME/mono").await.unwrap();
    let v = f
        .hub
        .viewer("acme/mono", Who::new(Kind::User, "bob"), Level::Admin)
        .await
        .unwrap();
    assert!(!acl.can_read(&v, b"x/y"));
    assert!(acl.can_read(&v, b"z"));
}

#[tokio::test(flavor = "current_thread")]
async fn the_guard_refuses_restricted_callers_and_admits_the_host_and_unrestricted_repos() {
    use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken};
    let f = fx().await;
    let slot = Arc::new(super::PathRulesSlot::default());
    assert!(slot.set(f.hub.clone()));
    let guard = super::guard::PathGuard::new(f.k.authz.clone(), slot);
    let tok = |t: &str| BearerToken::new(t).unwrap();
    // No rules: everyone the inner authorizer admits is admitted.
    let p = guard
        .authorize(Some(&tok("reader-token")), "acme/mono", Access::Read)
        .await
        .unwrap();
    assert!(!p.id.is_empty());
    let rules = [super::parse_rule("secret", "*", "none").unwrap()];
    f.hub
        .set("acme/mono", &rules, 0, "pat:alice")
        .await
        .unwrap();
    for t in ["reader-token", "alice-token"] {
        let e = guard
            .authorize(Some(&tok(t)), "acme/mono", Access::Read)
            .await
            .unwrap_err();
        assert!(matches!(e, AuthError::Forbidden), "{t} is restricted");
    }
    // The host's own work, and another repository, are not restricted.
    guard
        .authorize(Some(&tok("host-token")), "acme/mono", Access::Read)
        .await
        .unwrap();
    guard
        .authorize(Some(&tok("reader-token")), "acme/other", Access::Read)
        .await
        .unwrap();
}
