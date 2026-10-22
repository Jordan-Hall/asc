//! U44: the Forgejo client against a fake Forgejo (an `axum` server that records every call).

mod forgejo {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use asc_core::git::ObjectKind;
    use asc_core::id::{ChangesetId, GitOid, RefTarget};
    use asc_engine::derive::{GitMapRow, GitMapSink};
    use asc_rpc::IdMode;
    use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
    use asc_rpc::server::{CallCtx, RepoName};
    use asc_serve::config::CompositionGrant;
    use asc_serve::forgejo::{
        AuthzCacheConfig, ForgejoClient, ForgejoHooks, HookClient, HookOptions, InternalAuthz,
        InternalToken, PostReceiveEffect, PublicApiAuthz,
    };
    use asc_serve::outbox::{Effect, Job};
    use asc_serve::registry::{NewRepo, Registry, SqlGitMap};
    use asc_serve::update::{HookRequest, HookUpdate, PushHooks};
    use serde_json::{Value, json};
    use uuid::Uuid;

    const INTERNAL: &str = "internal-secret";

    #[derive(Clone, Debug)]
    struct Call {
        path: String,
        method: String,
        auth: Option<String>,
        body: Value,
        op_id: Option<String>,
    }

    #[derive(Default)]
    struct Inner {
        calls: Vec<Call>,
        /// token -> (id, login)
        users: HashMap<String, (i64, String)>,
        /// (token, repo) -> (admin, push, pull); token "" is anonymous.
        repos: HashMap<(String, String), (bool, bool, bool)>,
        /// (token, repo) -> the K07 answer
        authz: HashMap<(String, String), Value>,
        reject_pre: Option<String>,
        fail_post: u32,
        applied_ops: HashSet<String>,
        proc_answer: Option<Value>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Inner>>);

    impl Fake {
        fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }

        fn calls(&self, path_prefix: &str) -> Vec<Call> {
            self.with(|i| {
                i.calls
                    .iter()
                    .filter(|c| c.path.starts_with(path_prefix))
                    .cloned()
                    .collect()
            })
        }
    }

    fn reply(status: u16, body: Value) -> (StatusCode, String) {
        (StatusCode::from_u16(status).unwrap(), body.to_string())
    }

    async fn handle(
        State(fake): State<Fake>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, String) {
        let path = uri.path().to_owned();
        let bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_owned);
        let op_id = headers
            .get("x-asc-op-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let token = bearer.clone().unwrap_or_default();
        fake.with(|i| {
            i.calls.push(Call {
                path: path.clone(),
                method: method.to_string(),
                auth: bearer.clone(),
                body: json.clone(),
                op_id: op_id.clone(),
            });
            if path.starts_with("/api/internal/") && bearer.as_deref() != Some(INTERNAL) {
                return reply(403, json!({"err": "bad internal token"}));
            }
            if path == "/api/v1/user" {
                return match i.users.get(&token) {
                    Some((id, login)) => reply(200, json!({"id": id, "login": login})),
                    None => reply(401, json!({})),
                };
            }
            if let Some(repo) = path.strip_prefix("/api/v1/repos/") {
                return match i.repos.get(&(token, repo.to_owned())) {
                    Some((admin, push, pull)) => reply(
                        200,
                        json!({"permissions": {"admin": admin, "push": push, "pull": pull}}),
                    ),
                    None => reply(404, json!({})),
                };
            }
            if path == "/api/internal/asc/authz" {
                let t = json["token"].as_str().unwrap_or("").to_owned();
                let r = json["repo"].as_str().unwrap_or("").to_owned();
                return match i.authz.get(&(t, r)) {
                    Some(v) => reply(200, v.clone()),
                    None => reply(200, json!({"allowed": false})),
                };
            }
            if path.starts_with("/api/internal/hook/pre-receive/") {
                return match &i.reject_pre {
                    Some(m) => reply(403, json!({"user_msg": m})),
                    None => reply(200, json!({})),
                };
            }
            if path.starts_with("/api/internal/hook/post-receive/") {
                if i.fail_post > 0 {
                    i.fail_post -= 1;
                    return reply(500, json!({"err": "boom"}));
                }
                // Idempotent by op id: a redelivery is acknowledged and not applied again.
                if let Some(op) = &op_id {
                    i.applied_ops.insert(op.clone());
                }
                return reply(200, json!({"Results": [], "RepoWasEmpty": false}));
            }
            if path.starts_with("/api/internal/hook/proc-receive/") {
                return reply(
                    200,
                    i.proc_answer
                        .clone()
                        .unwrap_or_else(|| json!({"Results": []})),
                );
            }
            reply(404, json!({}))
        })
    }

    async fn start() -> (Fake, String) {
        let fake = Fake::default();
        let app = Router::new().fallback(handle).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, url)
    }

    fn client(url: &str, internal: bool) -> Arc<ForgejoClient> {
        let token = internal.then(|| InternalToken::new(INTERNAL).unwrap());
        Arc::new(ForgejoClient::new(url, token).unwrap())
    }

    fn tok(s: &str) -> BearerToken {
        BearerToken::new(s).unwrap()
    }

    // ------------------------------------------------------------------ stage 1

    #[tokio::test]
    async fn public_api_reads_permissions_and_caches() {
        let (fake, url) = start().await;
        fake.with(|i| {
            i.users.insert("alice-tok".into(), (7, "alice".into()));
            i.repos
                .insert(("alice-tok".into(), "o/r".into()), (false, false, true));
            i.repos
                .insert(("alice-tok".into(), "o/w".into()), (false, true, true));
            i.repos
                .insert((String::new(), "o/pub".into()), (false, false, false));
        });
        let authz = PublicApiAuthz::new(
            client(&url, false),
            AuthzCacheConfig {
                positive: Duration::from_secs(60),
                negative: Duration::from_millis(150),
                entries: 64,
            },
        );
        let alice = tok("alice-tok");
        let p = authz
            .authorize(Some(&alice), "o/r", Access::Read)
            .await
            .unwrap();
        assert_eq!(p.id, "7:alice");
        authz
            .authorize(Some(&alice), "o/r", Access::Read)
            .await
            .unwrap();
        assert_eq!(
            fake.calls("/api/v1/repos/o/r").len(),
            1,
            "positive answers are cached"
        );

        // pull only: write is refused, and the refusal is cached for a short time.
        for _ in 0..2 {
            assert_eq!(
                authz
                    .authorize(Some(&alice), "o/r", Access::Write)
                    .await
                    .unwrap_err(),
                AuthError::Forbidden
            );
        }
        assert_eq!(fake.calls("/api/v1/repos/o/r").len(), 2);
        tokio::time::sleep(Duration::from_millis(250)).await;
        authz
            .authorize(Some(&alice), "o/r", Access::Write)
            .await
            .unwrap_err();
        assert_eq!(
            fake.calls("/api/v1/repos/o/r").len(),
            3,
            "negative answers expire"
        );

        authz
            .authorize(Some(&alice), "o/w", Access::Write)
            .await
            .unwrap();
        // A repository the token cannot see is Forbidden; a bad token is Unauthenticated.
        assert_eq!(
            authz
                .authorize(Some(&alice), "o/none", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        assert_eq!(
            authz
                .authorize(Some(&tok("bad")), "o/r", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
        // Anonymous callers read public repositories and nothing else.
        authz.authorize(None, "o/pub", Access::Read).await.unwrap();
        assert_eq!(
            authz
                .authorize(None, "o/pub", Access::Write)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
        // A malformed repository name never reaches Forgejo.
        assert!(
            authz
                .authorize(Some(&alice), "../x", Access::Read)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_unreachable_forgejo_fails_closed_and_is_not_cached() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let authz = PublicApiAuthz::new(client(&url, false), AuthzCacheConfig::default());
        assert_eq!(
            authz
                .authorize(Some(&tok("t")), "o/r", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
    }

    // ------------------------------------------------------------------ stage 2+

    fn internal_authz(url: &str, grants: Vec<CompositionGrant>) -> InternalAuthz {
        InternalAuthz::new(client(url, true), grants, AuthzCacheConfig::default())
    }

    #[tokio::test]
    async fn internal_authz_reads_identity_perm_and_branch_rules() {
        let (fake, url) = start().await;
        fake.with(|i| {
            i.authz.insert(
                ("pat".into(), "o/r".into()),
                json!({"allowed": true, "user_id": 9, "username": "bob", "perm": "write",
                       "deploy_key_id": 0, "is_actions_task": false,
                       "branch_rules": {"require_signed": true}}),
            );
        });
        let authz = internal_authz(&url, Vec::new());
        let t = tok("pat");
        let d = authz.decide(Some(&t), "o/r", Access::Write).await.unwrap();
        assert_eq!(d.principal.id, "9:bob");
        assert!(d.require_signed);
        authz.decide(Some(&t), "o/r", Access::Read).await.unwrap();
        assert_eq!(
            authz
                .decide(Some(&t), "o/r", Access::Admin)
                .await
                .unwrap_err(),
            AuthError::Forbidden,
            "write permission is not admin"
        );
        // The request carried the internal token and the caller's token in the body.
        let calls = fake.calls("/api/internal/asc/authz");
        assert_eq!(calls[0].auth.as_deref(), Some(INTERNAL));
        assert_eq!(calls[0].body["token"], "pat");
        assert_eq!(calls[0].body["access"], "write");
        // Unknown repos and tokens are refused.
        assert_eq!(
            authz
                .decide(Some(&t), "o/other", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        assert_eq!(
            authz
                .decide(None, "o/other", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
    }

    #[tokio::test]
    async fn task_token_reads_a_child_repo_only_with_a_grant() {
        let (fake, url) = start().await;
        let task = |bound: &str| {
            json!({"allowed": true, "user_id": 3, "username": "actions", "perm": "write",
                   "is_actions_task": true,
                   "task": {"run_id": 5, "event": "push", "ref": "refs/heads/master",
                            "pr_index": 0, "is_fork_pr": false, "repo": bound}})
        };
        fake.with(|i| {
            // Forgejo may say "allowed" for a sibling repo; the grant rule still decides.
            i.authz
                .insert(("task".into(), "asc/dds".into()), task("asc/dds"));
            i.authz
                .insert(("task".into(), "asc/soma".into()), task("asc/dds"));
            i.authz
                .insert(("task".into(), "asc/other".into()), task("asc/dds"));
            // A task answer without its bound repo is refused outright.
            let mut unbound = task("x");
            unbound["task"].as_object_mut().unwrap().remove("repo");
            i.authz
                .insert(("unbound".into(), "asc/dds".into()), unbound);
        });
        let grant = CompositionGrant {
            parent: "asc/dds".into(),
            child: "asc/soma".into(),
        };
        let with = internal_authz(&url, vec![grant]);
        let without = internal_authz(&url, Vec::new());
        let t = tok("task");

        // Its own repo: normal permissions.
        with.decide(Some(&t), "asc/dds", Access::Write)
            .await
            .unwrap();
        // The child: read with a grant, never write, and not at all without one.
        let d = with
            .decide(Some(&t), "asc/soma", Access::Read)
            .await
            .unwrap();
        assert!(d.is_actions_task);
        assert_eq!(
            with.decide(Some(&t), "asc/soma", Access::Write)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        assert_eq!(
            without
                .decide(Some(&t), "asc/soma", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        // A grant for another child does not help.
        assert_eq!(
            with.decide(Some(&t), "asc/other", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        assert_eq!(
            with.decide(Some(&tok("unbound")), "asc/dds", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
    }

    #[tokio::test]
    async fn only_the_internal_token_is_trusted_and_may_skip_pre() {
        let (fake, url) = start().await;
        let authz = internal_authz(&url, Vec::new());
        let p = authz
            .authorize(Some(&tok(INTERNAL)), "o/r", Access::Admin)
            .await
            .unwrap();
        assert_eq!(p.id, asc_serve::forgejo::INTERNAL_PRINCIPAL);
        assert!(
            fake.calls("/api/internal/asc/authz").is_empty(),
            "answered locally"
        );

        let (registry, _dir) = registry().await;
        let hooks = ForgejoHooks::new(HookClient::new(client(&url, true)), registry);
        let ctx = |id: &str| CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: id.into() },
            op_id: Some(Uuid::new_v4()),
            client: None,
        };
        assert!(hooks.may_skip_pre(&ctx(&p.id)));
        assert!(!hooks.may_skip_pre(&ctx("7:alice")));
        // A Forgejo user cannot pose as the internal principal: user ids carry a colon.
        assert!(!hooks.may_skip_pre(&ctx("0:forgejo-internal")));
    }

    // ------------------------------------------------------------------ hooks

    #[test]
    fn hook_options_use_the_go_field_names() {
        let mut o = HookOptions::for_push(None, &["topic=x".into(), "force-push".into()]);
        o.push_ref("refs/heads/master", None, Some("aa"));
        let v = serde_json::to_value(&o).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        let mut want = vec![
            "OldCommitIDs",
            "NewCommitIDs",
            "RefFullNames",
            "UserID",
            "UserName",
            "GitObjectDirectory",
            "GitAlternativeObjectDirectories",
            "GitQuarantinePath",
            "GitPushOptions",
            "PullRequestID",
            "PushTrigger",
            "DeployKeyID",
            "IsWiki",
            "ActionPerm",
        ];
        want.sort_unstable();
        assert_eq!(keys, want);
        assert_eq!(v["PushTrigger"], "asc");
        assert_eq!(v["GitPushOptions"]["topic"], "x");
        assert_eq!(v["GitPushOptions"]["force-push"], "true");
        assert_eq!(v["OldCommitIDs"][0], "0".repeat(40));
        assert_eq!(v["NewCommitIDs"][0], "aa");
    }

    async fn registry() -> (Arc<Registry>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = format!(
            "sqlite:{}?mode=rwc",
            dir.path()
                .join("asc.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let store = url::Url::from_directory_path(dir.path().join("bucket")).unwrap();
        std::fs::create_dir_all(dir.path().join("bucket")).unwrap();
        let cfg =
            asc_serve::ServeConfig::parse(&format!("db = {db:?}\nstore = {:?}", store.as_str()))
                .unwrap();
        asc_serve::run_migrate(&cfg).await.unwrap();
        let pool = asc_serve::migrate::connect(&db, 4).await.unwrap();
        let registry = Arc::new(Registry::new(pool, store.as_str()).unwrap());
        registry
            .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        (registry, dir)
    }

    async fn map_commit(registry: &Registry, asc: [u8; 32], git: [u8; 20]) {
        let entry = registry
            .open(&RepoName::parse("o/r").unwrap())
            .await
            .unwrap();
        SqlGitMap::new(registry.pool().clone(), entry.ns)
            .put(&[GitMapRow {
                git: GitOid::from_bytes(git),
                kind: ObjectKind::Commit,
                asc,
            }])
            .await
            .unwrap();
    }

    fn target(b: u8) -> RefTarget {
        RefTarget::Changeset(ChangesetId::from_bytes([b; 32]))
    }

    #[tokio::test]
    async fn pre_receive_sends_git_ids_and_propagates_the_rejection() {
        let (fake, url) = start().await;
        let (registry, _dir) = registry().await;
        map_commit(&registry, [1; 32], [0xaa; 20]).await;
        map_commit(&registry, [2; 32], [0xbb; 20]).await;
        let hooks = ForgejoHooks::new(HookClient::new(client(&url, true)), registry);
        let repo = RepoName::parse("o/r").unwrap();
        let principal = Principal {
            id: "7:alice".into(),
        };
        let updates = [HookUpdate {
            name: "refs/heads/master".into(),
            old: Some(target(1)),
            new: Some(target(2)),
        }];
        let opts = vec!["topic=t".to_owned()];
        let req = HookRequest {
            repo: &repo,
            op_id: Uuid::new_v4(),
            principal: &principal,
            updates: &updates,
            push_options: &opts,
        };
        hooks.pre_receive(&req).await.unwrap();
        let call = &fake.calls("/api/internal/hook/pre-receive/o/r")[0];
        assert_eq!(call.method, "POST");
        assert_eq!(call.auth.as_deref(), Some(INTERNAL));
        assert_eq!(call.body["OldCommitIDs"][0], "aa".repeat(20));
        assert_eq!(call.body["NewCommitIDs"][0], "bb".repeat(20));
        assert_eq!(call.body["RefFullNames"][0], "refs/heads/master");
        assert_eq!(call.body["UserID"], 7);
        assert_eq!(call.body["UserName"], "alice");
        assert_eq!(call.body["PushTrigger"], "asc");
        assert_eq!(call.body["GitPushOptions"]["topic"], "t");

        fake.with(|i| i.reject_pre = Some("protected branch: force push denied".into()));
        let err = hooks.pre_receive(&req).await.unwrap_err();
        assert_eq!(err.0, "protected branch: force push denied");

        // An object without a git id cannot be checked, so the push is refused.
        let unmapped = [HookUpdate {
            name: "refs/heads/x".into(),
            old: None,
            new: Some(target(9)),
        }];
        let req = HookRequest {
            updates: &unmapped,
            ..req
        };
        assert!(hooks.pre_receive(&req).await.is_err());
    }

    #[tokio::test]
    async fn pre_receive_fails_closed_when_forgejo_is_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let hooks = HookClient::new(client(&url, true));
        let repo = RepoName::parse("o/r").unwrap();
        assert!(
            hooks
                .pre_receive(&repo, &HookOptions::default())
                .await
                .is_err()
        );
        // Without the internal token the call is not even attempted.
        let no_token = HookClient::new(client(&url, false));
        assert!(
            no_token
                .pre_receive(&repo, &HookOptions::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn proc_receive_returns_the_pr_refs() {
        let (fake, url) = start().await;
        fake.with(|i| {
            i.proc_answer = Some(json!({"Results": [{
                "OldOID": "0".repeat(40), "NewOID": "ab".repeat(20),
                "Ref": "refs/pull/4/head", "OriginalRef": "refs/for/master/topic",
                "IsForcePush": false, "IsCreatePR": true, "Err": ""}]}));
        });
        let hooks = HookClient::new(client(&url, true));
        let repo = RepoName::parse("o/r").unwrap();
        let r = hooks
            .proc_receive(&repo, &HookOptions::default())
            .await
            .unwrap();
        assert_eq!(r[0].ref_name, "refs/pull/4/head");
        assert!(r[0].is_create_pr);

        fake.with(|i| i.proc_answer = Some(json!({"Results": [], "Err": "no such branch"})));
        let e = hooks
            .proc_receive(&repo, &HookOptions::default())
            .await
            .unwrap_err();
        assert_eq!(e.0, "no such branch");
    }

    #[tokio::test]
    async fn post_receive_is_delivered_once_even_after_a_forced_retry() {
        let (fake, url) = start().await;
        let (registry, _dir) = registry().await;
        map_commit(&registry, [1; 32], [0xaa; 20]).await;
        let entry = registry
            .open(&RepoName::parse("o/r").unwrap())
            .await
            .unwrap();
        let effect =
            PostReceiveEffect::new(HookClient::new(client(&url, true)), registry.pool().clone());
        let hex = |t: RefTarget| {
            t.to_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let op = Uuid::new_v4();
        let job = Job {
            id: 1,
            repo_id: entry.key.0,
            op_id: op.as_bytes().to_vec(),
            doc: json!({
                "effects": ["post_receive"],
                "pusher": "7:alice",
                "refs": [
                    {"name": "refs/heads/master", "old": null, "new": hex(target(1))},
                    {"name": "refs/users/9/heads/x", "old": null, "new": hex(target(1))},
                ],
            }),
        };
        // The first delivery fails (Forgejo is down for a moment) and is retried by the outbox.
        fake.with(|i| i.fail_post = 1);
        effect.run(&job).await.unwrap_err();
        assert!(fake.with(|i| i.applied_ops.is_empty()));
        effect.run(&job).await.unwrap();
        // A duplicate delivery (the worker crashed before marking the row done) changes nothing.
        effect.run(&job).await.unwrap();
        assert_eq!(fake.with(|i| i.applied_ops.len()), 1);
        let calls = fake.calls("/api/internal/hook/post-receive/o/r");
        assert_eq!(calls.len(), 3);
        assert!(
            calls
                .iter()
                .all(|c| c.op_id.as_deref() == Some(&op.to_string()))
        );
        let body = &calls[1].body;
        assert_eq!(
            body["RefFullNames"],
            json!(["refs/heads/master"]),
            "user refs stay private"
        );
        assert_eq!(body["OldCommitIDs"][0], "0".repeat(40));
        assert_eq!(body["NewCommitIDs"][0], "aa".repeat(20));
        assert_eq!(body["UserID"], 7);
        assert_eq!(body["PushTrigger"], "asc");
    }
}
