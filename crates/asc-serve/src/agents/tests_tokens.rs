//! Agent tokens as credentials, the token service, and the ref gate an agent passes on every
//! update: scope, budget, own branches only. These run against the real services and pipeline.

use asc_rpc::auth::{Access, AuthError, BearerToken};
use asc_rpc::server::wire;
use asc_rpc::services::agent_tokens_server::AgentTokens;
use bytes::Bytes;
use tonic::Code;

use super::gate::{AgentGate, ServerAgentGate};
use super::tests::{Opts, World, repo_ref, sig};
use crate::ai::clock;

/// The typed code of a refusal.
pub(crate) fn code(s: &tonic::Status) -> String {
    asc_rpc::server::status_rpc_error(s)
        .map(|e| e.code().to_owned())
        .unwrap_or_default()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A token for agent `fixer`; returns its row and secret.
async fn make(
    w: &World,
    paths: &[&str],
    limits: wire::AgentBudgetLimits,
    max_changes_per_run: u32,
) -> (wire::AgentToken, String) {
    let tokens = w.svc.tokens();
    let op = uuid::Uuid::new_v4().to_string();
    let r = tokens
        .create_token(w.request(
            wire::CreateAgentTokenRequest {
                repo: repo_ref(),
                name: "ci agent".into(),
                agent: "fixer".into(),
                paths: paths.iter().map(|p| (*p).to_owned()).collect(),
                max_changes_per_run,
                ttl_seconds: 3600,
                limits: Some(limits),
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap()
        .into_inner();
    (r.token.unwrap(), r.secret)
}

/// The branch `name` in the namespace of the token that `secret` is.
async fn mine(w: &World, secret: &str, name: &str) -> String {
    let row = w.hub.core.tokens.by_secret(secret).await.unwrap().unwrap();
    format!("refs/heads/agent/fixer/{}/{name}", row.id)
}

fn put(path: &str, content: &str) -> wire::FileOp {
    wire::FileOp {
        path: Bytes::copy_from_slice(path.as_bytes()),
        op: Some(wire::file_op::Op::PutContent(Bytes::copy_from_slice(
            content.as_bytes(),
        ))),
        mode: 0,
    }
}

/// A commit by `secret` on `update_ref`, on top of `parent` (hex, empty for none) with `expect`
/// as the ref's current value.
async fn commit_as(
    w: &World,
    secret: &str,
    update_ref: &str,
    parent: &[u8],
    expect: Option<&[u8]>,
    files: &[(&str, &str)],
) -> Result<Vec<u8>, tonic::Status> {
    let r = w
        .inproc
        .commit(
            secret,
            wire::CommitRequest {
                repo: repo_ref(),
                parent: hex(parent),
                ops: files.iter().map(|(p, c)| put(p, c)).collect(),
                message: Bytes::from_static(b"by an agent"),
                author: Some(sig()),
                committer: Some(sig()),
                update_ref: update_ref.into(),
                expect: expect.map(|e| wire::RefTarget {
                    t: Some(wire::ref_target::T::Changeset(Bytes::copy_from_slice(e))),
                }),
                sign: false,
            },
        )
        .await?;
    Ok(r.id.to_vec())
}

// ------------------------------------------------------------------------------ the token service

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_secret_is_shown_once_and_only_its_hash_is_kept() {
    let w = World::new().await;
    let (token, secret) = make(&w, &["src/**"], wire::AgentBudgetLimits::default(), 0).await;
    assert!(
        secret.starts_with("ascat_") && secret.len() == 49,
        "{secret}"
    );
    assert_eq!(token.secret_hint, secret[secret.len() - 4..]);
    assert_eq!(token.state, wire::AgentTokenState::Active as i32);
    assert_eq!(token.paths, ["src/**"]);
    assert!(token.expires_at > token.created && token.expires_at <= token.created + 3601);
    // The stored form is the SHA-256 and nothing else holds the secret.
    let stored: Vec<(String, String)> =
        sqlx::query_as("SELECT id, secret_sha256 FROM agent_tokens")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0].1, secret);
    assert_eq!(stored[0].1, super::tokens::hash(&secret));
    // No read returns the secret again.
    let listed = w
        .svc
        .tokens()
        .list_tokens(w.request(
            wire::ListAgentTokensRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.tokens.len(), 1);
    assert!(!format!("{listed:?}").contains(&secret));
    let got = w
        .svc
        .tokens()
        .get_token(w.request(
            wire::GetAgentTokenRequest {
                repo: repo_ref(),
                id: token.id.clone(),
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!format!("{got:?}").contains(&secret));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creating_a_token_is_validated_and_needs_repository_admin() {
    let w = World::new().await;
    let tokens = w.svc.tokens();
    let base = || wire::CreateAgentTokenRequest {
        repo: repo_ref(),
        name: "n".into(),
        agent: "fixer".into(),
        ttl_seconds: 60,
        ..Default::default()
    };
    let call = |who: &'static str, msg: wire::CreateAgentTokenRequest| {
        let op = uuid::Uuid::new_v4().to_string();
        let tokens = tokens.clone();
        let req = w.request(msg, who, Some(&op));
        async move { tokens.create_token(req).await }
    };
    // Write access is not enough; reading is not either; nobody is not either.
    assert_eq!(
        call("bob-token", base()).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        call("carol-token", base()).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    let anon = {
        let op = uuid::Uuid::new_v4().to_string();
        let mut r = tonic::Request::new(base());
        r.metadata_mut().insert("x-asc-op-id", op.parse().unwrap());
        tokens.create_token(r).await.unwrap_err()
    };
    assert_eq!(anon.code(), Code::Unauthenticated);
    // Bad input.
    for (name, msg) in [
        (
            "empty name",
            wire::CreateAgentTokenRequest {
                name: String::new(),
                ..base()
            },
        ),
        (
            "long name",
            wire::CreateAgentTokenRequest {
                name: "n".repeat(65),
                ..base()
            },
        ),
        (
            "control char",
            wire::CreateAgentTokenRequest {
                name: "a\nb".into(),
                ..base()
            },
        ),
        (
            "bad agent",
            wire::CreateAgentTokenRequest {
                agent: "Fixer!".into(),
                ..base()
            },
        ),
        (
            "no agent",
            wire::CreateAgentTokenRequest {
                agent: String::new(),
                ..base()
            },
        ),
        (
            "bad glob",
            wire::CreateAgentTokenRequest {
                paths: vec!["[".into()],
                ..base()
            },
        ),
        (
            "too long ttl",
            wire::CreateAgentTokenRequest {
                ttl_seconds: u32::MAX,
                ..base()
            },
        ),
        (
            "too many paths",
            wire::CreateAgentTokenRequest {
                paths: (0..33).map(|i| format!("p{i}/**")).collect(),
                ..base()
            },
        ),
    ] {
        let e = call("alice-token", msg).await.unwrap_err();
        assert!(
            matches!(e.code(), Code::InvalidArgument | Code::PermissionDenied),
            "{name}: {e:?}"
        );
    }
    // Paths the repository policy does not let this agent write are refused, not trimmed.
    let e = call(
        "alice-token",
        wire::CreateAgentTokenRequest {
            paths: vec!["docs/**".into()],
            ..base()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    let e = call(
        "alice-token",
        wire::CreateAgentTokenRequest {
            paths: vec!["src/secret/**".into()],
            ..base()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    // A token never lives forever: no ttl means the default hour.
    let ok = call(
        "alice-token",
        wire::CreateAgentTokenRequest {
            ttl_seconds: 0,
            ..base()
        },
    )
    .await
    .unwrap()
    .into_inner();
    let t = ok.token.unwrap();
    assert_eq!(t.expires_at - t.created, 3600);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_are_created_once_per_operation_and_the_secret_is_not_replayed() {
    let w = World::new().await;
    let tokens = w.svc.tokens();
    let msg = wire::CreateAgentTokenRequest {
        repo: repo_ref(),
        name: "n".into(),
        agent: "fixer".into(),
        ttl_seconds: 60,
        ..Default::default()
    };
    let op = uuid::Uuid::new_v4().to_string();
    let first = tokens
        .create_token(w.request(msg.clone(), "alice-token", Some(&op)))
        .await
        .unwrap()
        .into_inner();
    // The retry of the same operation does not make a second token, and does not hand the secret out again.
    let second = tokens
        .create_token(w.request(msg, "alice-token", Some(&op)))
        .await;
    if let Ok(r) = &second {
        assert_ne!(
            r.get_ref().secret,
            first.secret,
            "a retry never replays the secret"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM agent_tokens")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_a_token_takes_effect_on_the_next_call_and_is_idempotent() {
    let w = World::new().await;
    let (token, secret) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    let bearer = BearerToken::new(&secret).unwrap();
    assert!(
        w.authorizer
            .authorize(Some(&bearer), "acme/app", Access::Write)
            .await
            .is_ok()
    );
    let tokens = w.svc.tokens();
    // Only a repository admin revokes.
    let denied = tokens
        .revoke_token(w.request(
            wire::RevokeAgentTokenRequest {
                repo: repo_ref(),
                id: token.id.clone(),
                reason: String::new(),
            },
            "bob-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), Code::PermissionDenied);
    for _ in 0..2 {
        let r = tokens
            .revoke_token(w.request(
                wire::RevokeAgentTokenRequest {
                    repo: repo_ref(),
                    id: token.id.clone(),
                    reason: "leaked\u{7}".into(),
                },
                "alice-token",
                None,
            ))
            .await
            .unwrap()
            .into_inner()
            .token
            .unwrap();
        assert_eq!(r.state, wire::AgentTokenState::Revoked as i32);
        assert!(r.revoked_at > 0);
    }
    let after = w
        .authorizer
        .authorize(Some(&bearer), "acme/app", Access::Read)
        .await;
    assert!(
        matches!(after, Err(AuthError::Unauthenticated)),
        "{after:?}"
    );
    // The gate agrees: a revoked token cannot commit even if it got that far.
    let e = commit_as(
        &w,
        &secret,
        &mine(&w, &secret, "x").await,
        &w.base,
        None,
        &[("src/a.rs", "x")],
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    // A token that does not exist is the same answer as a token of another repository.
    let missing = tokens
        .revoke_token(w.request(
            wire::RevokeAgentTokenRequest {
                repo: repo_ref(),
                id: "at_nope".into(),
                reason: String::new(),
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_authorises_its_own_repository_never_admin_and_nothing_once_expired_or_exhausted() {
    let w = World::new().await;
    let other = crate::registry::NewRepo::default();
    w.registry
        .create(
            &asc_rpc::server::RepoName::parse("acme/other").unwrap(),
            &other,
        )
        .await
        .unwrap();
    let (token, secret) = make(
        &w,
        &[],
        wire::AgentBudgetLimits {
            max_tokens: 1000,
            ..Default::default()
        },
        0,
    )
    .await;
    let bearer = BearerToken::new(&secret).unwrap();
    let a = &w.authorizer;
    assert!(
        a.authorize(Some(&bearer), "acme/app", Access::Read)
            .await
            .is_ok()
    );
    assert!(
        a.authorize(Some(&bearer), "acme/app", Access::Write)
            .await
            .is_ok()
    );
    assert!(matches!(
        a.authorize(Some(&bearer), "acme/app", Access::Admin).await,
        Err(AuthError::Forbidden)
    ));
    assert!(matches!(
        a.authorize(Some(&bearer), "acme/other", Access::Read).await,
        Err(AuthError::Forbidden)
    ));
    assert!(matches!(
        a.authorize(Some(&bearer), "acme/other", Access::Write)
            .await,
        Err(AuthError::Forbidden)
    ));
    // The principal is the agent, under the name the policy knows.
    let p = a
        .authorize(Some(&bearer), "acme/app", Access::Read)
        .await
        .unwrap();
    assert!(p.id.starts_with("agent:fixer:"), "{}", p.id);
    // A near-miss secret is not a token.
    let wrong = BearerToken::new(format!("{}x", &secret[..secret.len() - 1])).unwrap();
    assert!(matches!(
        a.authorize(Some(&wrong), "acme/app", Access::Read).await,
        Err(AuthError::Unauthenticated)
    ));
    // Exhausted: reads still work (the agent can see why), writes do not.
    w.hub
        .core
        .tokens
        .charge_usage(&token.id, 1000, 0, 1)
        .await
        .unwrap()
        .ok();
    assert!(
        a.authorize(Some(&bearer), "acme/app", Access::Read)
            .await
            .is_ok()
    );
    assert!(matches!(
        a.authorize(Some(&bearer), "acme/app", Access::Write).await,
        Err(AuthError::Unauthenticated)
    ));
    // Expired: nothing.
    sqlx::query("UPDATE agent_tokens SET expires_at = $2 WHERE id = $1")
        .bind(token.id.clone())
        .bind(clock::now() - 1)
        .execute(&w.pool)
        .await
        .unwrap();
    assert!(matches!(
        a.authorize(Some(&bearer), "acme/app", Access::Read).await,
        Err(AuthError::Unauthenticated)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_agents_off_a_token_authenticates_nothing_and_the_services_say_so() {
    let w = World::with(Opts {
        cfg: Box::new(|c| c.enabled = false),
        ..Opts::default()
    })
    .await;
    // A token made while agents were on does not work once they are off.
    let off = super::tokens::new_secret();
    let created = w
        .hub
        .core
        .tokens
        .create(
            &super::tokens::NewToken {
                repo: "acme/app".into(),
                name: "t".into(),
                agent: "fixer".into(),
                paths: vec![],
                max_changes_per_run: 0,
                created_by: "7:alice".into(),
                owner_id: 7,
                created: 0,
                expires_at: clock::now() + 600,
                limits: super::tokens::Budget::default(),
                run_id: 0,
            },
            off.as_str(),
        )
        .await
        .unwrap();
    assert!(created.is_some());
    let bearer = BearerToken::new(off.as_str()).unwrap();
    assert!(matches!(
        w.authorizer
            .authorize(Some(&bearer), "acme/app", Access::Read)
            .await,
        Err(AuthError::Unauthenticated)
    ));
    let op = uuid::Uuid::new_v4().to_string();
    let e = w
        .svc
        .tokens()
        .create_token(w.request(
            wire::CreateAgentTokenRequest {
                repo: repo_ref(),
                name: "n".into(),
                agent: "fixer".into(),
                ttl_seconds: 60,
                ..Default::default()
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENTS_DISABLED");
    let e = w.start_run("alice-token", "do it").await.unwrap_err();
    assert_eq!(code(&e), "AGENTS_DISABLED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reads_only_itself_and_an_admin_reads_every_token_of_the_repository() {
    let w = World::new().await;
    let (a, a_secret) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    let (b, _) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    let tokens = w.svc.tokens();
    let get = |who: &str, id: &str| {
        tokens.get_token(w.request(
            wire::GetAgentTokenRequest {
                repo: repo_ref(),
                id: id.to_owned(),
            },
            who,
            None,
        ))
    };
    assert!(get("alice-token", &a.id).await.is_ok());
    assert!(get(&a_secret, &a.id).await.is_ok(), "the token itself");
    assert_eq!(
        get(&a_secret, &b.id).await.unwrap_err().code(),
        Code::NotFound,
        "not another token"
    );
    assert_eq!(
        get("bob-token", &a.id).await.unwrap_err().code(),
        Code::NotFound,
        "a writer is not an admin"
    );
    assert_eq!(
        get("carol-token", &a.id).await.unwrap_err().code(),
        Code::NotFound
    );
    // Listing is for admins only.
    for who in ["bob-token", "carol-token", a_secret.as_str()] {
        let e = tokens
            .list_tokens(w.request(
                wire::ListAgentTokensRequest {
                    repo: repo_ref(),
                    ..Default::default()
                },
                who,
                None,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(e.code(), Code::PermissionDenied | Code::Unauthenticated),
            "{who}: {e:?}"
        );
    }
    // Paging by a cursor shows every token once, newest first.
    let page1 = tokens
        .list_tokens(w.request(
            wire::ListAgentTokensRequest {
                repo: repo_ref(),
                limit: 1,
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page1.tokens.len(), 1);
    assert!(!page1.next_page_token.is_empty());
    let page2 = tokens
        .list_tokens(w.request(
            wire::ListAgentTokensRequest {
                repo: repo_ref(),
                limit: 1,
                page_token: page1.next_page_token.clone(),
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page2.tokens.len(), 1);
    assert_ne!(page1.tokens[0].id, page2.tokens[0].id);
    let bad = tokens
        .list_tokens(w.request(
            wire::ListAgentTokensRequest {
                repo: repo_ref(),
                page_token: "x".into(),
                ..Default::default()
            },
            "alice-token",
            None,
        ))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reported_usage_that_crosses_a_limit_is_recorded_in_full_and_exhausts_the_token() {
    let w = World::new().await;
    let (token, secret) = make(
        &w,
        &[],
        wire::AgentBudgetLimits {
            max_tokens: 1000,
            max_cost_micro_usd: 500,
            ..Default::default()
        },
        0,
    )
    .await;
    let tokens = w.svc.tokens();
    let report = |input: u64, output: u64, cost: u64| {
        let op = uuid::Uuid::new_v4().to_string();
        tokens.report_usage(w.request(
            wire::ReportAgentUsageRequest {
                repo: repo_ref(),
                input_tokens: input,
                output_tokens: output,
                cost_micro_usd: cost,
                requests: 1,
            },
            &secret,
            Some(&op),
        ))
    };
    let r = report(300, 100, 100).await.unwrap().into_inner();
    assert_eq!(r.usage.unwrap().tokens, 400);
    assert_eq!(r.remaining.unwrap().tokens, 600);
    // Crossing the token limit: the whole report is kept, the answer is BUDGET_EXCEEDED.
    let e = report(700, 100, 10).await.unwrap_err();
    assert_eq!(code(&e), "BUDGET_EXCEEDED");
    assert_eq!(e.code(), Code::ResourceExhausted);
    let row = w.hub.core.tokens.get(&token.id).await.unwrap().unwrap();
    assert_eq!(row.spent.tokens, 1200, "recorded in full");
    assert!(row.exhausted);
    // Exhausted: the token still reports (authenticates), and is still over.
    let e = report(1, 1, 0).await.unwrap_err();
    assert_eq!(code(&e), "BUDGET_EXCEEDED");
    // Only the token itself reports; a person does not.
    let op = uuid::Uuid::new_v4().to_string();
    let e = tokens
        .report_usage(w.request(
            wire::ReportAgentUsageRequest {
                repo: repo_ref(),
                input_tokens: 1,
                ..Default::default()
            },
            "alice-token",
            Some(&op),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budgets_can_be_raised_and_lowered_by_an_admin_within_the_policy_and_defaults() {
    let w = World::new().await;
    let (token, _) = make(
        &w,
        &[],
        wire::AgentBudgetLimits {
            max_tokens: 1000,
            ..Default::default()
        },
        0,
    )
    .await;
    let tokens = w.svc.tokens();
    let set = |who: &str, limits: wire::AgentBudgetLimits| {
        tokens.set_budget(w.request(
            wire::SetAgentTokenBudgetRequest {
                repo: repo_ref(),
                id: token.id.clone(),
                limits: Some(limits),
            },
            who,
            None,
        ))
    };
    let t = set(
        "alice-token",
        wire::AgentBudgetLimits {
            max_tokens: 5000,
            max_changes: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_inner()
    .token
    .unwrap();
    assert_eq!(t.limits.unwrap().max_tokens, 5000);
    assert_eq!(
        set("bob-token", wire::AgentBudgetLimits::default())
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    // A revoked token cannot be given more.
    w.hub.core.tokens.revoke(&token.id, "x").await.unwrap();
    let e = set(
        "alice-token",
        wire::AgentBudgetLimits {
            max_tokens: 9,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "AGENT_TOKEN_REVOKED");
}

// ------------------------------------------------------------------------------ the ref gate

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_writes_its_own_branch_and_never_trunk_or_anyone_elses() {
    let w = World::new().await;
    let (_, secret) = make(&w, &["src/**"], wire::AgentBudgetLimits::default(), 0).await;
    let trunk = w.trunk().await;
    // Its own branch works and builds a stack.
    let first = commit_as(
        &w,
        &secret,
        &mine(&w, &secret, "fix-1").await,
        &w.base,
        None,
        &[("src/a.rs", "pub fn a() {}\n")],
    )
    .await
    .unwrap();
    let second = commit_as(
        &w,
        &secret,
        &mine(&w, &secret, "fix-1").await,
        &first,
        Some(&first),
        &[("src/b.rs", "pub fn b() {}\n")],
    )
    .await
    .unwrap();
    assert_ne!(first, second);
    // Trunk, a tag, another agent's branch, a plain feature branch: all refused with the refs named.
    for target in [
        "refs/heads/master",
        "refs/heads/main",
        "refs/heads/agent/other/x",
        "refs/heads/agent/fixer",
        "refs/heads/feature",
        "refs/tags/v1",
        "refs/heads/agent/fixer/../../master",
    ] {
        let e = commit_as(&w, &secret, target, &w.base, None, &[("src/z.rs", "x")])
            .await
            .unwrap_err();
        assert!(
            matches!(code(&e).as_str(), "AGENT_SCOPE_DENIED") || e.code() == Code::InvalidArgument,
            "{target}: {e:?}"
        );
    }
    assert_eq!(w.trunk().await, trunk, "trunk did not move");
    assert!(w.ref_target("refs/heads/feature").await.is_none());
    assert!(w.ref_target("refs/tags/v1").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_land_is_refused_for_an_agent_and_other_principals_pass_the_gate() {
    let w = World::new().await;
    let gate = ServerAgentGate::new(w.hub.core.clone());
    let agent_ctx = |id: &str| asc_rpc::server::CallCtx {
        repo: asc_rpc::server::RepoName::parse("acme/app").unwrap(),
        id_mode: asc_rpc::IdMode::Asc,
        principal: asc_rpc::auth::Principal { id: id.to_owned() },
        op_id: None,
        client: None,
        admin: Default::default(),
    };
    let (_, secret) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    let bearer = BearerToken::new(&secret).unwrap();
    let p = w
        .authorizer
        .authorize(Some(&bearer), "acme/app", Access::Write)
        .await
        .unwrap();
    let token_id = p.id.rsplit(':').next().unwrap().to_owned();
    let own = format!("refs/heads/agent/fixer/{token_id}/x");
    let refs = vec![(own.clone(), false)];
    let e = gate
        .check_refs(&agent_ctx(&p.id), true, &refs)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    assert!(
        gate.check_refs(&agent_ctx(&p.id), false, &refs)
            .await
            .is_ok()
    );
    // Another token of the same agent name has its own namespace; this one cannot write it, nor a
    // person's branch, nor the old shared `agent/<name>/x`.
    for other in [
        "refs/heads/agent/fixer/at_other/x",
        "refs/heads/agent/fixer/x",
        "refs/heads/feature",
        "refs/users/7/heads/feature",
    ] {
        let e = gate
            .check_refs(&agent_ctx(&p.id), false, &[(other.to_owned(), false)])
            .await
            .unwrap_err();
        assert_eq!(code(&e), "AGENT_SCOPE_DENIED", "{other}");
    }
    // A run ref belongs to the token that started the run and to no other.
    let e = gate
        .check_refs(
            &agent_ctx(&p.id),
            false,
            &[("refs/heads/agent/fixer/run-999".to_owned(), false)],
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED", "an unknown run");
    // The number must be plain ASCII digits that parse: overlong, zero-led, signed and non-ASCII
    // digits are no run, and are refused rather than waved through.
    for odd in [
        "refs/heads/agent/fixer/run-99999999999999999999999",
        "refs/heads/agent/fixer/run-0001",
        "refs/heads/agent/fixer/run-0",
        "refs/heads/agent/fixer/run--1",
        "refs/heads/agent/fixer/run-+1",
        "refs/heads/agent/fixer/run-١٢",
        "refs/heads/agent/fixer/run-１２",
        "refs/heads/agent/fixer/run-1x",
        "refs/heads/agent/fixer/run-1/extra",
    ] {
        let e = gate
            .check_refs(&agent_ctx(&p.id), false, &[(odd.to_owned(), false)])
            .await
            .unwrap_err();
        assert_eq!(code(&e), "AGENT_SCOPE_DENIED", "{odd}");
    }
    // A real run of this token is its own; the same ref is refused to another token.
    let new_run = |token_id: &str| super::runs::NewRun {
        repo: "acme/app".into(),
        agent: "fixer".into(),
        title: String::new(),
        target: "master".into(),
        base: w.base.clone(),
        task: "t".into(),
        paths: vec![],
        ai_key_id: String::new(),
        model: "m".into(),
        limits: super::tokens::Budget::default(),
        created_by: "7:alice".into(),
        starter: "alice".into(),
        owner_id: 7,
        token_id: token_id.into(),
        workflow_run_id: 0,
        workflow_step: String::new(),
    };
    let mine = w.hub.core.runs.create(&new_run(&token_id)).await.unwrap();
    let theirs = w.hub.core.runs.create(&new_run("at_other")).await.unwrap();
    let own_run = format!("refs/heads/agent/fixer/run-{mine}");
    assert!(
        gate.check_refs(&agent_ctx(&p.id), false, &[(own_run, false)])
            .await
            .is_ok()
    );
    let e = gate
        .check_refs(
            &agent_ctx(&p.id),
            false,
            &[(format!("refs/heads/agent/fixer/run-{theirs}"), false)],
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED", "another token's run");
    // A person is not an agent: the gate lets them through to the rest of the pipeline.
    assert!(
        gate.check_refs(
            &agent_ctx("7:alice"),
            true,
            &[("refs/heads/master".to_owned(), false)]
        )
        .await
        .is_ok()
    );
    assert!(
        gate.check_refs(
            &agent_ctx("pat:alice"),
            false,
            &[("refs/heads/master".to_owned(), true)]
        )
        .await
        .is_ok()
    );
    // A lookalike principal is not an agent either, and carries no agent rights.
    assert!(
        gate.check_refs(&agent_ctx("agent:fixer"), true, &refs)
            .await
            .is_ok()
    );
    // An agent has no owner for ownership checks; only its own namespace maps to its owner's.
    let principal = asc_rpc::auth::Principal { id: p.id.clone() };
    assert_eq!(crate::refpolicy::owner_of(&principal), None);
    assert!(crate::refpolicy::draft_owner_of(&principal, &own).is_some());
    assert_eq!(
        crate::refpolicy::draft_owner_of(&principal, "refs/heads/agent/fixer/at_other/x"),
        None
    );
    assert_eq!(
        crate::refpolicy::draft_owner_of(&principal, "refs/heads/feature"),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_paths_and_the_policy_both_bound_what_an_agent_may_change() {
    let w = World::new().await;
    // The token admits src/** only; the policy admits src/** and tests/** minus src/secret/**.
    let (_, secret) = make(&w, &["src/**"], wire::AgentBudgetLimits::default(), 0).await;
    async fn commit(
        w: &World,
        secret: &str,
        tip: Option<&[u8]>,
        files: Vec<(&'static str, &'static str)>,
    ) -> Result<Vec<u8>, tonic::Status> {
        let parent = tip.map_or_else(|| w.base.clone(), <[u8]>::to_vec);
        commit_as(
            w,
            secret,
            &mine(w, secret, "scope").await,
            &parent,
            tip,
            &files,
        )
        .await
    }
    // Outside the token's paths (but inside the policy).
    let e = commit(&w, &secret, None, vec![("tests/t.rs", "x")])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    assert!(e.message().len() < 400);
    // Outside the policy (but under the token's glob).
    let e = commit(&w, &secret, None, vec![("src/secret/key.rs", "x")])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    // Outside both; every refused path is listed.
    let e = commit(
        &w,
        &secret,
        None,
        vec![("docs/a.md", "x"), ("src/ok.rs", "x"), ("docs/b.md", "x")],
    )
    .await
    .unwrap_err();
    let detail = asc_rpc::server::status_rpc_error(&e).unwrap();
    let mut paths = detail.info().paths.clone();
    paths.sort();
    assert_eq!(paths, ["docs/a.md", "docs/b.md"]);
    // Nothing was written by the refused attempts.
    assert!(
        w.ref_target(&mine(&w, &secret, "scope").await)
            .await
            .is_none()
    );
    // Inside both works.
    let tip = commit(&w, &secret, None, vec![("src/ok.rs", "pub fn ok() {}\n")])
        .await
        .unwrap();
    assert!(!tip.is_empty());
    assert!(
        w.ref_target(&mine(&w, &secret, "scope").await)
            .await
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_change_budget_of_a_token_counts_every_commit_and_a_refused_one_costs_nothing() {
    let w = World::new().await;
    let (token, secret) = make(
        &w,
        &[],
        wire::AgentBudgetLimits {
            max_changes: 2,
            ..Default::default()
        },
        0,
    )
    .await;
    let r = &mine(&w, &secret, "budget").await;
    let one = commit_as(&w, &secret, r, &w.base, None, &[("src/1.rs", "1")])
        .await
        .unwrap();
    let two = commit_as(&w, &secret, r, &one, Some(&one), &[("src/2.rs", "2")])
        .await
        .unwrap();
    let e = commit_as(&w, &secret, r, &two, Some(&two), &[("src/3.rs", "3")])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "BUDGET_EXCEEDED");
    assert_eq!(
        asc_rpc::server::status_rpc_error(&e).unwrap().info().paths,
        ["changes"]
    );
    let row = w.hub.core.tokens.get(&token.id).await.unwrap().unwrap();
    assert_eq!(row.spent.changes, 2);
    // An update that is refused for scope does not spend the budget.
    let (token2, secret2) = make(
        &w,
        &["src/**"],
        wire::AgentBudgetLimits {
            max_changes: 1,
            ..Default::default()
        },
        0,
    )
    .await;
    let e = commit_as(
        &w,
        &secret2,
        &mine(&w, &secret2, "s2").await,
        &w.base,
        None,
        &[("docs/x.md", "x")],
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    assert_eq!(
        w.hub
            .core
            .tokens
            .get(&token2.id)
            .await
            .unwrap()
            .unwrap()
            .spent
            .changes,
        0
    );
    commit_as(
        &w,
        &secret2,
        &mine(&w, &secret2, "s2").await,
        &w.base,
        None,
        &[("src/x.rs", "x")],
    )
    .await
    .unwrap();
    assert_eq!(
        w.hub
            .core
            .tokens
            .get(&token2.id)
            .await
            .unwrap()
            .unwrap()
            .spent
            .changes,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_per_run_change_cap_of_a_token_and_of_the_policy_bound_one_update() {
    let w = World::new().await;
    // A stack of `n` unreferenced commits on the base, then one update that points a ref at its top.
    async fn stack(w: &World, secret: &str, n: usize) -> Vec<u8> {
        let mut parent = w.base.clone();
        for i in 0..n {
            let path = format!("src/s{i}.rs");
            parent = commit_as(w, secret, "", &parent, None, &[(path.as_str(), "x")])
                .await
                .unwrap();
        }
        parent
    }
    async fn point(
        w: &World,
        secret: &str,
        name: &str,
        tip: &[u8],
    ) -> Result<wire::UpdateRefsResponse, tonic::Status> {
        w.inproc
            .update_refs(
                secret,
                wire::UpdateRefsRequest {
                    repo: repo_ref(),
                    updates: vec![wire::RefUpdate {
                        name: name.to_owned(),
                        expect: Some(wire::ref_update::Expect::Absent(true)),
                        new: Some(wire::RefTarget {
                            t: Some(wire::ref_target::T::Changeset(Bytes::copy_from_slice(tip))),
                        }),
                        delete: false,
                    }],
                    ..Default::default()
                },
            )
            .await
    }
    // A token capped at two changes per update refuses a push of three.
    let (_, capped) = make(&w, &[], wire::AgentBudgetLimits::default(), 2).await;
    let top = stack(&w, &capped, 3).await;
    let e = point(&w, &capped, &mine(&w, &capped, "three").await, &top)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "BUDGET_EXCEEDED", "{e:?}");
    assert!(
        w.ref_target(&mine(&w, &capped, "three").await)
            .await
            .is_none()
    );
    let top = stack(&w, &capped, 2).await;
    point(&w, &capped, &mine(&w, &capped, "two").await, &top)
        .await
        .unwrap();
    // The policy caps a run at five changes, whatever the token says.
    let (_, open) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    let top = stack(&w, &open, 6).await;
    let e = point(&w, &open, &mine(&w, &open, "six").await, &top)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "BUDGET_EXCEEDED");
    let top = stack(&w, &open, 5).await;
    point(&w, &open, &mine(&w, &open, "five").await, &top)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_can_never_land_whatever_it_asks_for() {
    let w = World::new().await;
    let (_, secret) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    let tip = commit_as(
        &w,
        &secret,
        &mine(&w, &secret, "land-me").await,
        &w.base,
        None,
        &[("src/l.rs", "l")],
    )
    .await
    .unwrap();
    let trunk = w.trunk().await;
    for mode in [
        wire::UpdateMode::Land,
        wire::UpdateMode::Force,
        wire::UpdateMode::FastForward,
    ] {
        let e = w
            .inproc
            .update_refs(
                &secret,
                wire::UpdateRefsRequest {
                    repo: repo_ref(),
                    updates: vec![wire::RefUpdate {
                        name: "refs/heads/master".into(),
                        expect: Some(wire::ref_update::Expect::Any(true)),
                        new: Some(wire::RefTarget {
                            t: Some(wire::ref_target::T::Changeset(Bytes::copy_from_slice(&tip))),
                        }),
                        delete: false,
                    }],
                    mode: mode as i32,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), "AGENT_SCOPE_DENIED", "{mode:?}: {e:?}");
    }
    // Nor can it delete trunk or its own branch's protection away by deleting someone else's.
    let e = w
        .inproc
        .update_refs(
            &secret,
            wire::UpdateRefsRequest {
                repo: repo_ref(),
                updates: vec![wire::RefUpdate {
                    name: "refs/heads/master".into(),
                    expect: Some(wire::ref_update::Expect::Any(true)),
                    new: None,
                    delete: true,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
    assert_eq!(w.trunk().await, trunk);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_of_another_agent_name_cannot_borrow_the_branch_of_this_one() {
    let w = World::new().await;
    let (_, secret) = make(&w, &[], wire::AgentBudgetLimits::default(), 0).await;
    // The token is `fixer`'s; the branch of `reviewer` is refused even though the path is fine.
    let e = commit_as(
        &w,
        &secret,
        "refs/heads/agent/reviewer/x",
        &w.base,
        None,
        &[("src/r.rs", "r")],
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "AGENT_SCOPE_DENIED");
}

/// An authorizer that knows no tokens and answers what a forge says of a person.
struct Forge(std::sync::Mutex<std::collections::HashMap<(i64, Access), asc_rpc::auth::Standing>>);

#[async_trait::async_trait]
impl asc_rpc::auth::Authorizer for Forge {
    async fn authorize(
        &self,
        _t: Option<&BearerToken>,
        _repo: &str,
        _a: Access,
    ) -> Result<asc_rpc::auth::Principal, AuthError> {
        Err(AuthError::Unauthenticated)
    }

    async fn user_standing(
        &self,
        user_id: i64,
        _repo: &str,
        access: Access,
    ) -> asc_rpc::auth::Standing {
        self.0
            .lock()
            .unwrap()
            .get(&(user_id, access))
            .copied()
            .unwrap_or(asc_rpc::auth::Standing::Unknown)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_is_only_as_good_as_the_access_of_the_person_who_made_it() {
    let w = World::new().await;
    let secret = super::tokens::new_secret();
    let make = |created_by: &str, owner_id: i64, secret: &str| {
        let n = super::tokens::NewToken {
            repo: "acme/app".into(),
            name: "t".into(),
            agent: "fixer".into(),
            paths: vec![],
            max_changes_per_run: 0,
            created_by: created_by.into(),
            owner_id,
            created: 0,
            expires_at: clock::now() + 600,
            limits: super::tokens::Budget::default(),
            run_id: 0,
        };
        let tokens = w.hub.core.tokens.clone();
        let secret = secret.to_owned();
        async move { tokens.create(&n, &secret).await.unwrap().unwrap() }
    };
    make("7:alice", 7, secret.as_str()).await;
    let plain = super::tokens::new_secret();
    make("pat:alice", 7, plain.as_str()).await;
    use asc_rpc::auth::{Authorizer as _, Standing};
    let forge = std::sync::Arc::new(Forge(Default::default()));
    let authz = super::authz::AgentAuthorizer::new(forge.clone(), w.hub.core.tokens.clone(), true);
    let bearer = BearerToken::new(secret.as_str()).unwrap();
    let on = |access| authz.authorize(Some(&bearer), "acme/app", access);
    // The forge is asked, and a yes lets the token work.
    {
        let mut m = forge.0.lock().unwrap();
        m.insert((7, Access::Read), Standing::Person(true));
        m.insert((7, Access::Write), Standing::Person(true));
    }
    assert!(on(Access::Read).await.is_ok() && on(Access::Write).await.is_ok());
    // The person loses write access: the token still reads, and no longer writes.
    forge
        .0
        .lock()
        .unwrap()
        .insert((7, Access::Write), Standing::Person(false));
    assert!(on(Access::Read).await.is_ok());
    assert_eq!(on(Access::Write).await.unwrap_err(), AuthError::Forbidden);
    // The person loses every access: the token is dead on the next call.
    forge
        .0
        .lock()
        .unwrap()
        .insert((7, Access::Read), Standing::Person(false));
    assert_eq!(on(Access::Read).await.unwrap_err(), AuthError::Forbidden);
    // An authorizer that does not say refuses the token: not knowing is not a pass.
    forge.0.lock().unwrap().clear();
    assert_eq!(on(Access::Write).await.unwrap_err(), AuthError::Forbidden);
    assert_eq!(on(Access::Read).await.unwrap_err(), AuthError::Forbidden);
    // A server that has no people to ask (a token store) lets the token stand on its own.
    {
        let mut m = forge.0.lock().unwrap();
        m.insert((7, Access::Read), Standing::NoPeople);
        m.insert((7, Access::Write), Standing::NoPeople);
    }
    assert!(on(Access::Write).await.is_ok());
    let anon = BearerToken::new(plain.as_str()).unwrap();
    forge
        .0
        .lock()
        .unwrap()
        .insert((7, Access::Write), Standing::Person(false));
    assert!(
        authz
            .authorize(Some(&anon), "acme/app", Access::Write)
            .await
            .is_ok(),
        "a token with no forge person behind it is not judged by one"
    );
}

/// Every wrapper hands the question to what it wraps: a wrapper that forgot would turn a forge's
/// answer into `Unknown`, which refuses the token.
#[tokio::test]
async fn every_wrapper_forwards_the_standing_of_a_person() {
    use asc_rpc::auth::{Authorizer, Standing};
    let forge = std::sync::Arc::new(Forge(Default::default()));
    forge
        .0
        .lock()
        .unwrap()
        .insert((7, Access::Read), Standing::Person(true));
    let inner: std::sync::Arc<dyn Authorizer> = forge;
    let ask = |a: std::sync::Arc<dyn Authorizer>| async move {
        (
            a.user_standing(7, "o/r", Access::Read).await,
            a.user_standing(8, "o/r", Access::Read).await,
        )
    };
    let want = (Standing::Person(true), Standing::Unknown);
    let logging: std::sync::Arc<dyn Authorizer> =
        std::sync::Arc::new(crate::logging::LoggingAuthorizer::new(inner.clone()));
    assert_eq!(ask(logging).await, want, "logging");
    let verifier = asc_rpc::forwarded::ForwardedVerifier::new(vec![1u8; 32]).unwrap();
    let forwarded: std::sync::Arc<dyn Authorizer> = std::sync::Arc::new(
        crate::authz::forwarded::ForwardedAuthorizer::new(inner.clone(), verifier),
    );
    assert_eq!(ask(forwarded).await, want, "forwarded");
    let recording =
        crate::limits::Admission::new(&crate::config::Limits::default()).recording(inner.clone());
    assert_eq!(ask(recording).await, want, "recording");
    let serve: std::sync::Arc<dyn Authorizer> =
        crate::mcp::backend::ServeAuthorizer::new(inner.clone());
    assert_eq!(ask(serve).await, want, "mcp");
    // The agent authorizer itself answers for what it wraps too.
    let w = World::new().await;
    let agents: std::sync::Arc<dyn Authorizer> =
        super::authz::AgentAuthorizer::new(inner, w.hub.core.tokens.clone(), true);
    assert_eq!(ask(agents).await, want, "agents");
    // Authorizers without people say so, or say nothing, and neither is a pass for a person's token.
    assert_eq!(
        asc_rpc::auth::StaticTokens::new()
            .user_standing(7, "o/r", Access::Read)
            .await,
        Standing::NoPeople
    );
    struct Silent;
    #[async_trait::async_trait]
    impl Authorizer for Silent {
        async fn authorize(
            &self,
            _t: Option<&BearerToken>,
            _r: &str,
            _a: Access,
        ) -> Result<asc_rpc::auth::Principal, AuthError> {
            Err(AuthError::Unauthenticated)
        }
    }
    assert_eq!(
        Silent.user_standing(7, "o/r", Access::Read).await,
        Standing::Unknown,
        "the default denies"
    );
}
