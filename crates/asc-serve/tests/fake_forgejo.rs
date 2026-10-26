//! U44: the Forgejo client against a fake Forgejo (an `axum` server that records every call).
#![cfg(feature = "forgejo")]

mod forgejo {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use asc_core::git::ObjectKind;
    use asc_core::id::{ChangesetId, GitOid, RefTarget};
    use asc_engine::derive::{GitMapRow, GitMapSink};
    use asc_rpc::IdMode;
    use asc_rpc::auth::{
        Access, AuthError, Authorizer, BearerToken, Credential, Decision, Principal, SshKey,
    };
    use asc_rpc::server::{CallCtx, RepoName};
    use asc_serve::config::CompositionGrant;
    use asc_serve::forgejo::{
        AuthzCacheConfig, ForgejoClient, ForgejoHooks, HookClient, HookOptions, InternalAuthz,
        InternalToken, PostReceiveEffect, PublicApiAuthz,
    };
    use asc_serve::outbox::{Effect, Job};
    use asc_serve::registry::{NewRepo, Registry, SqlGitMap};
    use asc_serve::update::{HookRequest, HookUpdate, PushHooks};
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};
    use uuid::Uuid;

    const INTERNAL: &str = "internal-secret";

    #[derive(Clone, Debug)]
    struct Call {
        path: String,
        method: String,
        auth: Option<String>,
        /// The `user:password` of an `Authorization: Basic` header.
        basic: Option<String>,
        body: Value,
        op_id: Option<String>,
    }

    #[derive(Default)]
    struct Inner {
        calls: Vec<Call>,
        /// token -> (id, login); a Basic login is keyed `basic/<user>:<password>`.
        users: HashMap<String, (i64, String)>,
        /// (token, repo) -> (admin, push, pull); token "" is anonymous.
        repos: HashMap<(String, String), (bool, bool, bool)>,
        /// (token, repo) -> the K07 answer
        authz: HashMap<(String, String), Value>,
        reject_pre: Option<String>,
        fail_post: u32,
        applied_ops: HashSet<String>,
        proc_answer: Option<Value>,
        /// Tokens without `write:repository`: Forgejo's scope middleware answers 403 to a POST.
        read_only_tokens: HashSet<String>,
        /// Overrides the scope probe's answer (a Forgejo that changed the route, or a 5xx).
        probe_status: Option<u16>,
        /// K19: key fingerprint -> the `ssh-key` answer (unknown: `known: false`).
        ssh_keys: HashMap<String, Value>,
        /// K19: (key fingerprint, `owner/repo`, mode) -> the `ssh_key` variant of the authz answer.
        ssh_authz: HashMap<(String, String, String), Value>,
        /// Overrides the status of the authz endpoint (with a body that would allow).
        authz_status: Option<u16>,
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

    /// The answer for a credential that did not authenticate: every field zero.
    fn zero_denial() -> Value {
        json!({"allowed": false, "user_id": 0, "username": "", "perm": "", "deploy_key_id": 0,
               "is_actions_task": false, "task": null, "branch_rules": {"require_signed": false}})
    }

    fn reply(status: u16, body: Value) -> (StatusCode, String) {
        (StatusCode::from_u16(status).unwrap(), body.to_string())
    }

    /// One known field of a request: its name and a check of its Go type.
    type KnownField<'a> = (&'a str, fn(&Value) -> bool);

    /// Forgejo's `decodeAscStrict`: the body is one JSON object whose fields are all known, and
    /// the values have the Go field types. `None` is a 400 (`modules/private/asc.go`).
    fn strict_fields(
        json: &Value,
        known: &[KnownField<'_>],
    ) -> Option<serde_json::Map<String, Value>> {
        let obj = json.as_object()?;
        obj.iter()
            .all(|(k, v)| known.iter().any(|(n, ok)| n == k && ok(v)))
            .then(|| obj.clone())
    }

    fn is_str(v: &Value) -> bool {
        v.is_string()
    }

    fn is_int(v: &Value) -> bool {
        v.is_i64() || v.is_u64()
    }

    fn is_obj(v: &Value) -> bool {
        v.is_object()
    }

    /// `AscSSHKeyRequest`: `{type, key, fingerprint}`.
    fn strict_key(json: &Value) -> Option<serde_json::Map<String, Value>> {
        let o = strict_fields(
            json,
            &[("type", is_str), ("key", is_str), ("fingerprint", is_str)],
        )?;
        let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("");
        (!s("type").is_empty() && !s("key").is_empty()).then_some(o)
    }

    /// `AscAuthzRequest` and `validateAscAuthzRequest`. The error text is the fork's `user_msg`.
    fn strict_authz(json: &Value) -> Result<(), &'static str> {
        const BAD: &str = "invalid request body";
        let o = strict_fields(
            json,
            &[
                ("token", is_str),
                ("user", is_str),
                ("key_id", is_int),
                ("ssh_key", is_obj),
                ("owner", is_str),
                ("repo", is_str),
                ("mode", is_str),
                ("ref", is_str),
            ],
        )
        .ok_or(BAD)?;
        let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("");
        if !matches!(s("mode"), "read" | "write") {
            return Err("mode must be read or write");
        }
        if s("owner").is_empty() || s("repo").is_empty() {
            return Err("invalid owner or repo");
        }
        if let Some(k) = o.get("ssh_key") {
            if ["token", "user", "key_id"]
                .iter()
                .any(|f| o.contains_key(*f))
            {
                return Err("ssh_key excludes token, user and key_id");
            }
            return strict_key(k).map(|_| ()).ok_or("invalid key");
        }
        if !s("user").is_empty() && s("token").is_empty() {
            return Err("user needs a token");
        }
        if o.contains_key("key_id") && !s("token").is_empty() {
            return Err("token and key_id are mutually exclusive");
        }
        Ok(())
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
        let basic = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|v| STANDARD.decode(v).ok())
            .and_then(|v| String::from_utf8(v).ok());
        let op_id = headers
            .get("x-asc-op-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let token = match (&bearer, &basic) {
            (Some(b), _) => b.clone(),
            (None, Some(b)) => format!("basic/{b}"),
            (None, None) => String::new(),
        };
        fake.with(|i| {
            i.calls.push(Call {
                path: path.clone(),
                method: method.to_string(),
                auth: bearer.clone(),
                basic: basic.clone(),
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
            if method == Method::POST
                && path.starts_with("/api/v1/repos/")
                && path.ends_with("/markdown")
            {
                // The scope check of the `/repos` group runs before any handler.
                if let Some(s) = i.probe_status {
                    return reply(s, json!({}));
                }
                if i.read_only_tokens.contains(&token) {
                    return reply(
                        403,
                        json!({"message": "token does not have at least one of required scope(s): [write:repository]"}),
                    );
                }
                return (StatusCode::OK, "<p>x</p>".to_owned());
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
            if path == "/api/internal/asc/ssh-key" {
                if strict_key(&json).is_none() {
                    return reply(400, json!({"user_msg": "invalid request body"}));
                }
                let fp = json["fingerprint"].as_str().unwrap_or("");
                return match i.ssh_keys.get(fp) {
                    Some(v) => reply(200, v.clone()),
                    None => reply(200, json!({"known": false})),
                };
            }
            if path == "/api/internal/asc/authz" {
                // Like the fork: an unknown field or a wrong name is a 400, and a denial is a 200
                // with `allowed: false`, never a 401 or 403. A credential that did not
                // authenticate gives every field zero; a refusal after authentication carries
                // the user's own `user_id` and `username` (K07c).
                if let Err(m) = strict_authz(&json) {
                    return reply(400, json!({"user_msg": m}));
                }
                if let Some(st) = i.authz_status {
                    return reply(st, json!({"allowed": true, "user_id": 1, "username": "x", "perm": "admin"}));
                }
            }
            if path == "/api/internal/asc/authz" && json["ssh_key"].is_object() {
                // K19: `{ssh_key, owner, repo, mode}`; the key excludes `token` and `user`.
                let k = (
                    json["ssh_key"]["fingerprint"].as_str().unwrap_or("").to_owned(),
                    format!(
                        "{}/{}",
                        json["owner"].as_str().unwrap_or(""),
                        json["repo"].as_str().unwrap_or("")
                    ),
                    json["mode"].as_str().unwrap_or("").to_owned(),
                );
                return match i.ssh_authz.get(&k) {
                    Some(v) => reply(200, v.clone()),
                    None => reply(200, json!({"allowed": false})),
                };
            }
            if path == "/api/internal/asc/authz" {
                // K07, K07b: `{token, user, owner, repo, mode}`.
                let mut t = json["token"].as_str().unwrap_or("").to_owned();
                if let Some(user) = json["user"].as_str().filter(|u| !u.is_empty()) {
                    // A Basic login: a wrong password is `allowed: false`, as any other denial.
                    t = format!("basic/{user}:{t}");
                    if !i.users.contains_key(&t) {
                        return reply(200, zero_denial());
                    }
                }
                let r = format!(
                    "{}/{}",
                    json["owner"].as_str().unwrap_or(""),
                    json["repo"].as_str().unwrap_or("")
                );
                if let Some(v) = i.authz.get(&(t.clone(), r)) {
                    return reply(200, v.clone());
                }
                // No answer for this repo: a refusal that names the user when the credential
                // is known, and the all-zero denial when it is not.
                let known = i.users.get(&t).cloned().or_else(|| {
                    i.authz.iter().find_map(|((tok, _), v)| {
                        (*tok == t && v["user_id"].as_i64().unwrap_or(0) != 0).then(|| {
                            (
                                v["user_id"].as_i64().unwrap_or(0),
                                v["username"].as_str().unwrap_or("").to_owned(),
                            )
                        })
                    })
                });
                return match known {
                    Some((id, name)) if !t.is_empty() => reply(
                        200,
                        json!({"allowed": false, "user_id": id, "username": name, "perm": "",
                               "deploy_key_id": 0, "is_actions_task": false, "task": null,
                               "branch_rules": {"require_signed": false}}),
                    ),
                    _ => reply(200, zero_denial()),
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
        // The fake Forgejo speaks plain http on the loopback interface (U89 opt-in).
        asc_core::transport_policy::set_config_allow_loopback_http(true);
        let fake = Fake::default();
        let app = Router::new().fallback(handle).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, url)
    }

    fn client(url: &str, internal: bool) -> Arc<ForgejoClient> {
        // Tests that never call `start()` (a dead loopback port) still need the http opt-in.
        asc_core::transport_policy::set_config_allow_loopback_http(true);
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

    fn stage1(url: &str) -> PublicApiAuthz {
        PublicApiAuthz::new(client(url, false), AuthzCacheConfig::default())
    }

    /// The user's `permissions.push` / `admin` belong to the user, not the token: a token without
    /// `write:repository` must never be granted Write or Admin.
    #[tokio::test]
    async fn public_api_denies_write_to_a_token_without_the_write_scope() {
        let (fake, url) = start().await;
        fake.with(|i| {
            for (t, id, login) in [("ro-tok", 1, "ro"), ("rw-tok", 2, "rw")] {
                i.users.insert(t.into(), (id, login.into()));
                i.repos
                    .insert((t.into(), "o/w".into()), (false, true, true));
                i.repos.insert((t.into(), "o/a".into()), (true, true, true));
            }
            i.read_only_tokens.insert("ro-tok".into());
        });
        let authz = stage1(&url);
        let (ro, rw) = (tok("ro-tok"), tok("rw-tok"));

        for (repo, access) in [
            ("o/w", Access::Write),
            ("o/a", Access::Write),
            ("o/a", Access::Admin),
        ] {
            assert_eq!(
                authz.authorize(Some(&ro), repo, access).await.unwrap_err(),
                AuthError::Forbidden,
                "{repo} {access:?}"
            );
            authz.authorize(Some(&rw), repo, access).await.unwrap();
        }
        // The read-only token still reads.
        authz
            .authorize(Some(&ro), "o/w", Access::Read)
            .await
            .unwrap();

        // The probe is a harmless POST (render markdown) and nothing else is ever sent.
        let posts: Vec<_> = fake
            .calls("/api/v1/")
            .into_iter()
            .filter(|c| c.method != "GET")
            .collect();
        assert!(!posts.is_empty());
        assert!(
            posts
                .iter()
                .all(|c| c.method == "POST" && c.path.ends_with("/markdown")),
            "{posts:?}"
        );
    }

    #[tokio::test]
    async fn public_api_fails_closed_when_the_scope_probe_is_not_a_clear_yes() {
        let (fake, url) = start().await;
        fake.with(|i| {
            i.users.insert("t".into(), (3, "t".into()));
            i.repos
                .insert(("t".into(), "o/w".into()), (false, true, true));
        });
        let t = tok("t");
        for status in [403, 404, 500, 204] {
            fake.with(|i| i.probe_status = Some(status));
            let authz = stage1(&url);
            assert_eq!(
                authz
                    .authorize(Some(&t), "o/w", Access::Write)
                    .await
                    .unwrap_err(),
                AuthError::Forbidden,
                "probe answered {status}"
            );
        }
        // Reading needs no probe at all.
        let before = fake.calls("/api/v1/repos/o/w/markdown").len();
        stage1(&url)
            .authorize(Some(&t), "o/w", Access::Read)
            .await
            .unwrap();
        assert_eq!(fake.calls("/api/v1/repos/o/w/markdown").len(), before);
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
        assert_eq!(
            calls[0].body,
            json!({"token": "pat", "owner": "o", "repo": "r", "mode": "write"})
        );
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
    async fn the_fake_refuses_the_old_request_shape_like_the_fork() {
        let (_fake, url) = start().await;
        let post = |body: Value| {
            let url = format!("{url}api/internal/asc/authz");
            async move {
                reqwest::Client::new()
                    .post(url)
                    .bearer_auth(INTERNAL)
                    .header("content-type", "application/json")
                    .body(body.to_string())
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        // The shape before K07: `repo: "o/r"` and `access`.
        assert_eq!(
            post(json!({"token": "t", "repo": "o/r", "access": "read"})).await,
            400
        );
        // The right names with one unknown field, a bad mode, no owner, a bad type.
        let good = json!({"token": "t", "owner": "o", "repo": "r", "mode": "read"});
        assert_eq!(post(good.clone()).await, 200);
        let with = |k: &str, v: Value| {
            let mut b = good.clone();
            b[k] = v;
            b
        };
        assert_eq!(post(with("access", json!("read"))).await, 400);
        assert_eq!(post(with("mode", json!("admin"))).await, 400);
        assert_eq!(post(with("repo", json!(7))).await, 400);
        let mut no_owner = good.clone();
        no_owner.as_object_mut().unwrap().remove("owner");
        assert_eq!(post(no_owner).await, 400);
        assert_eq!(post(json!([1])).await, 400);
    }

    #[tokio::test]
    async fn a_400_or_a_5xx_from_the_authz_endpoint_is_never_an_allow() {
        let (fake, url) = start().await;
        let authz = internal_authz(&url, Vec::new());
        let t = tok("pat");
        for status in [400, 404, 500, 502, 503] {
            fake.with(|i| i.authz_status = Some(status));
            assert!(
                authz.decide(Some(&t), "o/r", Access::Read).await.is_err(),
                "{status}"
            );
        }
        fake.with(|i| i.authz_status = None);
        // And the denial of the fork is a 200 with `allowed: false`.
        assert!(authz.decide(Some(&t), "o/r", Access::Read).await.is_err());
    }

    #[tokio::test]
    async fn task_token_reads_a_child_repo_only_with_a_grant() {
        let (fake, url) = start().await;
        let task = |bound: &str| {
            json!({"allowed": true, "user_id": -2, "username": "gitea-actions", "perm": "write",
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

        // Its own repo: normal permissions, and the Actions user id is negative.
        let own = with
            .decide(Some(&t), "asc/dds", Access::Write)
            .await
            .unwrap();
        assert_eq!(own.principal.id, "-2:gitea-actions");
        // Repository names compare exactly: the registry is case-sensitive, so another case
        // is another repo even when Forgejo answers "allowed" for it.
        for other in ["asc/DDS", "ASC/dds", "ASC/DDS"] {
            fake.with(|i| {
                i.authz
                    .insert(("task".into(), other.into()), task("asc/dds"));
            });
            assert_eq!(
                with.decide(Some(&t), other, Access::Write)
                    .await
                    .unwrap_err(),
                AuthError::Forbidden,
                "{other}"
            );
        }
        fake.with(|i| {
            i.authz
                .insert(("task".into(), "asc/asc".into()), task("asc/asc"));
            for other in ["asc/ASC", "ASC/asc", "ASC/ASC"] {
                i.authz
                    .insert(("task".into(), other.into()), task("asc/asc"));
            }
        });
        with.decide(Some(&t), "asc/asc", Access::Write)
            .await
            .unwrap();
        for other in ["asc/ASC", "ASC/asc", "ASC/ASC"] {
            assert_eq!(
                with.decide(Some(&t), other, Access::Read)
                    .await
                    .unwrap_err(),
                AuthError::Forbidden,
                "{other}"
            );
        }
        // A composition grant matches both names exactly: case-folded grants do not match.
        let folded = internal_authz(
            &url,
            vec![
                CompositionGrant {
                    parent: "asc/DDS".into(),
                    child: "asc/soma".into(),
                },
                CompositionGrant {
                    parent: "asc/dds".into(),
                    child: "asc/SOMA".into(),
                },
                CompositionGrant {
                    parent: "ASC/DDS".into(),
                    child: "ASC/SOMA".into(),
                },
            ],
        );
        assert_eq!(
            folded
                .decide(Some(&t), "asc/soma", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
        // Another repo the task cannot reach: Forgejo names the Actions user in the refusal.
        fake.with(|i| {
            i.authz.insert(
                ("task".into(), "asc/private".into()),
                json!({"allowed": false, "user_id": -2, "username": "gitea-actions"}),
            );
        });
        assert_eq!(
            with.decide(Some(&t), "asc/private", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
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

    // ------------------------------------------------------------------ Basic logins (U44 override of U78)

    fn basic(user: &str, secret: &str) -> Credential {
        Credential::basic(user, secret).unwrap()
    }

    fn password_fixture(fake: &Fake) {
        fake.with(|i| {
            for (key, id, login) in [
                ("basic/alice:pw-1", 7, "alice"),
                ("basic/bob:pw-1", 8, "bob"),
                ("basic/x-access-token:pat-1", 7, "alice"),
            ] {
                i.users.insert(key.into(), (id, login.into()));
                i.repos
                    .insert((key.into(), "o/w".into()), (false, true, true));
                i.repos
                    .insert((key.into(), "o/r".into()), (false, false, true));
            }
            i.users.insert("alice-tok".into(), (7, "alice".into()));
            i.repos
                .insert(("alice-tok".into(), "o/w".into()), (false, true, true));
        });
    }

    #[tokio::test]
    async fn a_basic_login_is_forwarded_to_forgejo_as_basic() {
        let (fake, url) = start().await;
        password_fixture(&fake);
        let authz = stage1(&url);
        let p = authz
            .authorize_credential(Some(&basic("alice", "pw-1")), "o/w", Access::Write)
            .await
            .unwrap();
        assert_eq!(p.id, "7:alice");
        // The user, the repository and the scope probe all carried Basic `user:password`, and
        // none of them a Bearer token.
        let calls = fake.calls("/api/v1/");
        assert_eq!(calls.len(), 3, "{calls:?}");
        for c in &calls {
            assert_eq!(c.basic.as_deref(), Some("alice:pw-1"), "{c:?}");
            assert_eq!(c.auth, None, "{c:?}");
        }
        assert_eq!(calls[2].method, "POST");
        assert!(calls[2].path.ends_with("/markdown"));
        // A PAT under `x-access-token` is Basic too; Forgejo reads the token from the password.
        let p = authz
            .authorize_credential(Some(&basic("x-access-token", "pat-1")), "o/r", Access::Read)
            .await
            .unwrap();
        assert_eq!(p.id, "7:alice");
        // When Forgejo answers the scope probe 403, the login is capped at read.
        fake.with(|i| {
            i.read_only_tokens.insert("basic/alice:pw-1".into());
        });
        assert_eq!(
            stage1(&url)
                .authorize_credential(Some(&basic("alice", "pw-1")), "o/w", Access::Write)
                .await
                .unwrap_err(),
            AuthError::Forbidden
        );
    }

    #[tokio::test]
    async fn a_wrong_password_is_unauthenticated_and_the_refusal_is_cached() {
        let (fake, url) = start().await;
        password_fixture(&fake);
        let authz = stage1(&url);
        for _ in 0..2 {
            assert_eq!(
                authz
                    .authorize_credential(Some(&basic("alice", "nope")), "o/w", Access::Read)
                    .await
                    .unwrap_err(),
                AuthError::Unauthenticated
            );
        }
        // A real user with another user's password is refused as well.
        assert_eq!(
            authz
                .authorize_credential(Some(&basic("carol", "pw-1")), "o/w", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
        let users = fake.calls("/api/v1/user");
        assert_eq!(users.len(), 2, "the negative answer was cached: {users:?}");
        assert_eq!(users[0].basic.as_deref(), Some("alice:nope"));
        assert_eq!(fake.calls("/api/v1/repos/").len(), 0);
    }

    #[tokio::test]
    async fn bearer_token_and_userless_basic_still_go_as_bearer() {
        let (fake, url) = start().await;
        password_fixture(&fake);
        let authz = stage1(&url);
        let bearer = Credential::bearer(tok("alice-tok"));
        let token = Credential::token(tok("alice-tok"));
        // `https://TOKEN@host` and `:TOKEN` carry no user name.
        let nameless = basic("", "alice-tok");
        assert_eq!(nameless.user(), None);
        for c in [&bearer, &token, &nameless] {
            let p = authz
                .authorize_credential(Some(c), "o/w", Access::Write)
                .await
                .unwrap();
            assert_eq!(p.id, "7:alice");
        }
        // One secret and no user name is one cache entry whatever the scheme.
        assert_eq!(fake.calls("/api/v1/user").len(), 1);
        for c in fake.calls("/api/v1/") {
            assert_eq!(c.auth.as_deref(), Some("alice-tok"), "{c:?}");
            assert_eq!(c.basic, None, "{c:?}");
        }
        // The plain `authorize` takes the Bearer path too.
        authz
            .authorize(Some(&tok("alice-tok")), "o/w", Access::Write)
            .await
            .unwrap();
        assert_eq!(fake.calls("/api/v1/user").len(), 1);
    }

    #[tokio::test]
    async fn the_cache_key_includes_the_basic_user_name() {
        let (fake, url) = start().await;
        password_fixture(&fake);
        let authz = stage1(&url);
        let alice = basic("alice", "pw-1");
        let bob = basic("bob", "pw-1");
        let who = |p: Result<Principal, AuthError>| p.unwrap().id;
        assert_eq!(
            who(authz
                .authorize_credential(Some(&alice), "o/r", Access::Read)
                .await),
            "7:alice"
        );
        // The same password under another user name is a different caller, not alice's entry.
        assert_eq!(
            who(authz
                .authorize_credential(Some(&bob), "o/r", Access::Read)
                .await),
            "8:bob"
        );
        assert_eq!(fake.calls("/api/v1/user").len(), 2);
        // Both are cached afterwards, each under its own key.
        assert_eq!(
            who(authz
                .authorize_credential(Some(&alice), "o/r", Access::Read)
                .await),
            "7:alice"
        );
        assert_eq!(
            who(authz
                .authorize_credential(Some(&bob), "o/r", Access::Read)
                .await),
            "8:bob"
        );
        assert_eq!(fake.calls("/api/v1/user").len(), 2);
        // The same text as a Bearer token is yet another caller, and Forgejo refuses it.
        assert_eq!(
            authz
                .authorize(Some(&tok("pw-1")), "o/r", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
        assert_eq!(fake.calls("/api/v1/user").len(), 3);
    }

    #[tokio::test]
    async fn internal_authz_forwards_the_basic_user_and_refuses_a_wrong_password() {
        let (fake, url) = start().await;
        fake.with(|i| {
            for (key, id, name) in [
                ("basic/alice:pw-1", 7, "alice"),
                ("basic/bob:pw-1", 8, "bob"),
            ] {
                i.users.insert(key.into(), (id, name.into()));
                i.authz.insert(
                    (key.into(), "o/r".into()),
                    json!({"allowed": true, "user_id": id, "username": name, "perm": "write"}),
                );
            }
            i.authz.insert(
                ("pw-1".into(), "o/r".into()),
                json!({"allowed": true, "user_id": 1, "username": "bearer", "perm": "read"}),
            );
        });
        let authz = internal_authz(&url, Vec::new());
        let a = authz
            .authorize_credential(Some(&basic("alice", "pw-1")), "o/r", Access::Write)
            .await
            .unwrap();
        assert_eq!(a.id, "7:alice");
        let calls = fake.calls("/api/internal/asc/authz");
        assert_eq!(calls[0].auth.as_deref(), Some(INTERNAL));
        assert_eq!(calls[0].body["user"], "alice");
        assert_eq!(calls[0].body["token"], "pw-1");
        // The cache key includes the user.
        let b = authz
            .authorize_credential(Some(&basic("bob", "pw-1")), "o/r", Access::Write)
            .await
            .unwrap();
        assert_eq!(b.id, "8:bob");
        authz
            .authorize_credential(Some(&basic("alice", "pw-1")), "o/r", Access::Write)
            .await
            .unwrap();
        assert_eq!(fake.calls("/api/internal/asc/authz").len(), 2);
        // Bearer is unchanged: no `user` in the body, and its own answer.
        let c = authz
            .authorize_credential(Some(&Credential::bearer(tok("pw-1"))), "o/r", Access::Read)
            .await
            .unwrap();
        assert_eq!(c.id, "1:bearer");
        let calls = fake.calls("/api/internal/asc/authz");
        assert_eq!(calls.len(), 3);
        assert!(calls[2].body.get("user").is_none(), "{:?}", calls[2].body);
        // Forgejo answers a wrong password with `allowed: false` and every field zero, which
        // is a 401 challenge, so git erases the stored credential (SPEC §15.6).
        assert_eq!(
            authz
                .authorize_credential(Some(&basic("alice", "wrong")), "o/r", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
        // A good login without permission on the repository is a 403-class refusal: the fork
        // names the user, and a missing repository looks the same.
        for repo in ["o/private", "o/missing"] {
            assert_eq!(
                authz
                    .authorize_credential(Some(&basic("alice", "pw-1")), repo, Access::Read)
                    .await
                    .unwrap_err(),
                AuthError::Forbidden,
                "{repo}"
            );
        }
        // A bearer token Forgejo does not know is unauthenticated, not forbidden.
        assert_eq!(
            authz
                .authorize(Some(&tok("not-a-token")), "o/r", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
    }

    #[tokio::test]
    async fn the_recording_wrapper_passes_basic_logins_to_the_forgejo_authorizer() {
        use asc_serve::config::Limits;
        use asc_serve::limits::Admission;
        let (fake, url) = start().await;
        password_fixture(&fake);
        // Admission keys its record on the hash of the parsed secret (limits.rs tests the throttle
        // itself) and the wrapper forwards the credential, so Forgejo sees the Basic user and a
        // refused password is a refused caller.
        let admission = Admission::new(&Limits::default());
        let authz = admission.recording(Arc::new(stage1(&url)));
        assert_eq!(
            authz
                .authorize_credential(Some(&basic("alice", "bad")), "o/r", Access::Read)
                .await
                .unwrap_err(),
            AuthError::Unauthenticated
        );
        let p = authz
            .authorize_credential(Some(&basic("alice", "pw-1")), "o/r", Access::Read)
            .await
            .unwrap();
        assert_eq!(p.id, "7:alice");
        let users = fake.calls("/api/v1/user");
        assert_eq!(users[1].basic.as_deref(), Some("alice:pw-1"));
    }

    // ------------------------------------------------------------------ logs never hold the secret

    /// Records every tracing event (level, target, fields) in one string.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<String>>);

    struct Fields<'a>(&'a mut String);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut out = self.0.lock().unwrap();
            let meta = event.metadata();
            out.push_str(&format!("{} {} ", meta.level(), meta.target()));
            event.record(&mut Fields(&mut out));
            out.push('\n');
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn the_secret_never_reaches_the_logs() {
        use base64::Engine as _;
        // Events from other tasks count too, so the subscriber is global.
        let capture = Capture::default();
        let _ = tracing::subscriber::set_global_default(capture.clone());

        let password = "Zq9-never-log-this-password";
        let wrong = "Zq9-never-log-this-wrong-one";
        let (fake, url) = start().await;
        fake.with(|i| {
            let key = format!("basic/alice:{password}");
            i.users.insert(key.clone(), (7, "alice".into()));
            i.repos
                .insert((key.clone(), "o/w".into()), (false, true, true));
            // The scope probe is refused, which logs at debug level.
            i.read_only_tokens.insert(key);
        });
        let stage1 = stage1(&url);
        let internal = internal_authz(&url, Vec::new());
        let creds = [basic("alice", password), basic("alice", wrong)];
        for c in &creds {
            let _ = stage1
                .authorize_credential(Some(c), "o/w", Access::Write)
                .await;
            let _ = internal
                .authorize_credential(Some(c), "o/w", Access::Write)
                .await;
        }
        // An unreachable Forgejo logs the failure; it must not log the request either.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_url = format!("http://{}/", dead.local_addr().unwrap());
        drop(dead);
        let dead1 = PublicApiAuthz::new(client(&dead_url, false), AuthzCacheConfig::default());
        let dead2 = InternalAuthz::new(
            client(&dead_url, true),
            Vec::new(),
            AuthzCacheConfig::default(),
        );
        for c in &creds {
            let _ = dead1
                .authorize_credential(Some(c), "o/w", Access::Read)
                .await;
            let _ = dead2
                .authorize_credential(Some(c), "o/w", Access::Read)
                .await;
        }
        assert_eq!(format!("{:?}", creds[0]), "Credential(basic, <redacted>)");

        let log = capture.0.lock().unwrap().clone();
        assert!(
            log.contains("lacks write") && log.contains("lookup failed"),
            "the failure paths logged nothing, so this test proves nothing: {log:?}"
        );
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let needles = [
            password.to_owned(),
            wrong.to_owned(),
            format!("alice:{password}"),
            format!("alice:{wrong}"),
            b64(&format!("alice:{password}")),
            b64(&format!("alice:{wrong}")),
            b64(password),
        ];
        for n in needles {
            assert!(!log.contains(&n), "{n} was logged:\n{log}");
        }
    }

    #[tokio::test]
    async fn only_the_internal_token_is_trusted_and_may_skip_pre() {
        let (fake, url) = start().await;
        let authz = internal_authz(&url, Vec::new());
        let p = authz
            .authorize(Some(&tok(INTERNAL)), "o/r", Access::Admin)
            .await
            .unwrap();
        assert_eq!(p.id, asc_serve::authz::HOST_SERVICE_PRINCIPAL);
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
        // Rows and logs written before the principal was renamed still name the host service.
        assert!(hooks.may_skip_pre(&ctx("forgejo-internal")));
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

    /// U85: a user key and a deploy key through the two K19 endpoints of the fake.
    #[tokio::test]
    async fn internal_authz_identifies_and_authorises_ssh_keys() {
        let (fake, url) = start().await;
        let alice = SshKey::new("ssh-ed25519", &[7; 51]).unwrap();
        let deploy = SshKey::new("ssh-ed25519", &[8; 51]).unwrap();
        let (fa, fd) = (
            alice.fingerprint().to_string(),
            deploy.fingerprint().to_string(),
        );
        fake.with(|i| {
            i.ssh_keys.insert(
                fa.clone(),
                json!({"known": true, "key_id": 11, "kind": "user", "user_id": 7, "username": "alice"}),
            );
            i.ssh_keys
                .insert(fd.clone(), json!({"known": true, "key_id": 12, "kind": "deploy"}));
            let user = |perm: &str| {
                json!({"allowed": true, "user_id": 7, "username": "alice", "perm": perm, "deploy_key_id": 0})
            };
            for mode in ["read", "write"] {
                i.ssh_authz.insert((fa.clone(), "o/w".into(), mode.into()), user("write"));
            }
            i.ssh_authz.insert((fa.clone(), "o/r".into(), "read".into()), user("read"));
            i.ssh_authz.insert(
                (fd.clone(), "o/d".into(), "read".into()),
                json!({"allowed": true, "user_id": 2, "username": "o", "perm": "read", "deploy_key_id": 5}),
            );
        });
        let a = internal_authz(&url, Vec::new());

        let who = a.identify_ssh_key(&alice).await.unwrap().unwrap();
        assert_eq!(
            (who.key_id.as_str(), who.principal.id.as_str()),
            ("11", "7:alice")
        );
        let who = a.identify_ssh_key(&deploy).await.unwrap().unwrap();
        assert_eq!(who.key_id, "12");
        assert_eq!(
            a.identify_ssh_key(&SshKey::new("ssh-ed25519", &[9; 51]).unwrap())
                .await,
            Ok(None)
        );

        let allowed = |id: &str| Decision::Allowed(Principal { id: id.into() });
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/w", Access::Write).await,
            Ok(allowed("7:alice"))
        );
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/r", Access::Read).await,
            Ok(allowed("7:alice"))
        );
        assert_eq!(
            a.authorize_ssh_key(&alice, "o/r", Access::Write).await,
            Ok(Decision::Denied)
        );
        assert_eq!(
            a.authorize_ssh_key(&deploy, "o/d", Access::Read).await,
            Ok(allowed("2:o:5"))
        );
        assert_eq!(
            a.authorize_ssh_key(&deploy, "o/d", Access::Write).await,
            Ok(Decision::Denied)
        );
        assert_eq!(
            a.authorize_ssh_key(&deploy, "o/other", Access::Read).await,
            Ok(Decision::Denied)
        );

        // The internal token, and a body with the key and no token fields.
        for c in fake.calls("/api/internal/asc/") {
            assert_eq!(c.auth.as_deref(), Some(INTERNAL));
        }
        let body = &fake.calls("/api/internal/asc/authz")[0].body;
        assert_eq!(body["ssh_key"]["fingerprint"], fa);
        assert!(body.get("token").is_none() && body.get("user").is_none());
    }
}
