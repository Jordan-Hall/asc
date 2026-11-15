//! The person profile an agent may read, for the RPC `Agents.GetPerson` and the sandbox tool
//! `get_person` alike.
//!
//! One function asks and audits, so both doors give the same answer. The only gate is the repository
//! read gate the caller already passed (a run's token is bound to its own repository) and the
//! person's own sharing setting at the hub. Nothing but the shared fields is ever returned: no email.

use serde_json::Value;

use super::AgentsHub;
use crate::profile::Profile;
use crate::review::valid_handle;

/// Most `get_person` calls one run makes; later calls are refused (limits profile scraping by an
/// agent a prompt told to).
pub const MAX_PERSON_CALLS: u32 = 20;
/// What the model reads when the person does not share a profile (or does not exist).
pub const NOT_SHARED: &str = "this person does not share a profile with agents";
/// Longest login.
const MAX_LOGIN: usize = 64;

/// Why a profile was not looked up at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The login is not 1 to 64 characters.
    Login,
    /// The run made all the lookups it may.
    Cap,
}

impl Refused {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Login => "a login is 1 to 64 characters".to_owned(),
            Self::Cap => format!("a run may look up at most {MAX_PERSON_CALLS} people"),
        }
    }
}

/// Whether `login` can name a person at the hub; a malformed one is answered "not shared" without
/// asking and without using a lookup of the run's allowance.
fn well_formed(login: &str) -> bool {
    valid_handle(login) && !login.contains(['@', '+'])
}

/// Asks the hub for the profile of `login` and audits the ask under `actor`. `run` is the run the
/// caller acts for, when it is one: its lookups are counted and capped. `Ok(None)` is every refusal
/// but the two of [`Refused`], the hub being down included.
pub(crate) async fn read_profile(
    hub: &AgentsHub,
    actor: &str,
    on_behalf: Option<&str>,
    repo: &str,
    login: &str,
    run: Option<u64>,
) -> Result<Option<Profile>, Refused> {
    if login.is_empty() || login.len() > MAX_LOGIN {
        return Err(Refused::Login);
    }
    let profile = if well_formed(login) {
        if let Some(id) = run
            && !hub.take_person_call(id)
        {
            return Err(Refused::Cap);
        }
        match hub.profiles.get() {
            Some(p) => p.get(login).await,
            None => None,
        }
    } else {
        None
    };
    hub.audit(
        "agent.profile.read",
        actor,
        on_behalf,
        repo,
        login,
        &[
            ("login", login.to_owned()),
            ("allowed", profile.is_some().to_string()),
        ],
    )
    .await;
    Ok(profile)
}

impl AgentsHub {
    /// Counts one lookup of run `run_id`; false once the run is over its allowance.
    fn take_person_call(&self, run_id: u64) -> bool {
        let mut m = self
            .person_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = m.entry(run_id).or_insert(0);
        if *n >= MAX_PERSON_CALLS {
            return false;
        }
        *n += 1;
        true
    }

    /// Forgets the count of a finished run.
    pub(crate) fn forget_person_calls(&self, run_id: u64) {
        self.person_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run_id);
    }

    /// The run an agent principal (`agent:<name>:<owner>:<token>`) is the token of, when it is a
    /// run's token; `None` for anyone else.
    /// The run a token principal acts for. A store error is an error, never "not a run", so a run
    /// cannot slip past its allowance while the store is busy.
    pub(crate) async fn run_of_principal(
        &self,
        principal: &str,
    ) -> Result<Option<u64>, sqlx::Error> {
        let Some(p) = super::tokens::parse_principal(principal) else {
            return Ok(None);
        };
        let id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM agent_runs WHERE token_id = $1 LIMIT 1")
                .bind(p.token_id)
                .fetch_optional(&self.core.pool)
                .await?;
        Ok(id.and_then(|n| u64::try_from(n).ok()))
    }
}

/// The sandbox tool `get_person`: the text for the model and whether it is an error. The profile is
/// one JSON object, so no field can pass for another. The caller masks and bounds it.
pub(crate) async fn person_tool(
    hub: &AgentsHub,
    run_id: u64,
    actor: &str,
    repo: &str,
    input: &Value,
) -> (String, bool) {
    let Some(m) = input.as_object() else {
        return ("the arguments must be a JSON object".to_owned(), true);
    };
    let Some(login) = m.get("login").and_then(Value::as_str) else {
        return ("login is required and must be a string".to_owned(), true);
    };
    match read_profile(hub, actor, None, repo, login, Some(run_id)).await {
        Err(e) => (e.text(), true),
        Ok(None) => (NOT_SHARED.to_owned(), false),
        Ok(Some(p)) => (
            serde_json::json!({
                "login": p.login,
                "full_name": p.full_name,
                "description": p.description,
                "location": p.location,
                "website": p.website,
                "pronouns": p.pronouns,
            })
            .to_string(),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::super::runs::EventKind;
    use super::super::runs::EventRow;
    use super::super::tests::{World, done, tools};
    use super::*;
    use crate::ai::owner::Owner;
    use crate::profile::{AgentProfiles, ProfileSource};

    struct Fixed(Option<Profile>);

    #[async_trait::async_trait]
    impl ProfileSource for Fixed {
        async fn profile(&self, _login: &str) -> Result<Option<Profile>, String> {
            Ok(self.0.clone())
        }
    }

    fn texts(ev: &[EventRow], kind: EventKind) -> Vec<String> {
        ev.iter()
            .filter(|e| e.kind == kind as i64)
            .map(|e| e.text.clone())
            .collect()
    }

    async fn world(p: Option<Profile>) -> World {
        let w = World::new().await;
        w.key(Owner::User("alice".into()), "mine").await;
        assert!(
            w.hub
                .set_profiles(Arc::new(AgentProfiles::new(Some(Arc::new(Fixed(p))))))
        );
        w
    }

    async fn audit_actors(w: &World) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT actor_name FROM audit_events WHERE action = 'agent.profile.read' AND scope_kind = (SELECT MIN(scope_kind) FROM audit_events WHERE action = 'agent.profile.read') ORDER BY seq",
        )
        .fetch_all(&w.pool)
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_tool_returns_the_shared_fields_masked_refuses_bad_input_and_stops_at_the_cap() {
        let key = "AKIAIOSFODNN7EXAMPLE";
        let w = world(Some(Profile {
            login: "maya".into(),
            full_name: "Maya Okafor".into(),
            description: format!("keys {key}"),
            location: "Lagos".into(),
            ..Default::default()
        }))
        .await;
        let mut calls = vec![
            ("get_person", json!({"login": "maya"})),
            ("get_person", json!({"login": ""})),
            ("get_person", json!({"login": "x".repeat(65)})),
            ("get_person", json!("maya")),
            ("get_person", json!({"login": 7})),
        ];
        // 19 more valid calls make 20; the 21st valid one is over the cap.
        calls.extend((0..20).map(|_| ("get_person", json!({"login": "maya"}))));
        w.script(vec![tools(calls), done("ok")]);
        let run = w.start_run("alice-token", "who is maya").await.unwrap();
        w.wait(run.id).await;
        let results = texts(&w.events(run.id).await, EventKind::ToolResult);
        assert_eq!(results.len(), 25, "{results:?}");
        let first: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(first["full_name"], "Maya Okafor");
        assert_eq!(first["location"], "Lagos");
        assert!(
            !results[0].contains(key) && results[0].contains("***"),
            "{}",
            results[0]
        );
        assert!(!results[0].to_lowercase().contains("email"));
        assert!(results[1].contains("1 to 64"), "{}", results[1]);
        assert!(results[2].contains("1 to 64"), "{}", results[2]);
        assert!(results[3].contains("JSON object"), "{}", results[3]);
        assert!(results[4].contains("must be a string"), "{}", results[4]);
        assert!(results[5..24].iter().all(|r| r.contains("Maya Okafor")));
        assert!(results[24].contains("at most 20"), "{}", results[24]);
        // Each answered ask is audited with the person who started the run, 20 in all.
        let actors = audit_actors(&w).await;
        assert_eq!(actors.len(), 20);
        assert!(actors.iter().all(|a| a.contains("alice")), "{actors:?}");
        // The count is dropped with the run.
        assert!(w.hub.person_calls.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_person_who_does_not_share_a_profile_is_refused_in_plain_words() {
        let w = world(None).await;
        w.script(vec![
            tools(vec![("get_person", json!({"login": "maya"}))]),
            done("ok"),
        ]);
        let run = w.start_run("alice-token", "who is maya").await.unwrap();
        w.wait(run.id).await;
        let ev = w.events(run.id).await;
        assert_eq!(texts(&ev, EventKind::ToolResult), vec![NOT_SHARED]);
        let actors = audit_actors(&w).await;
        assert!(
            actors.len() == 1 && actors[0].contains("alice"),
            "{actors:?}"
        );
    }

    fn calls(n: usize, login: &str) -> Vec<(&'static str, serde_json::Value)> {
        (0..n)
            .map(|_| ("get_person", json!({"login": login})))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_new_run_has_a_fresh_allowance_and_a_newline_stays_inside_its_field() {
        let w = world(Some(Profile {
            login: "maya".into(),
            description: "line one\nemail: maya@example.org".into(),
            ..Default::default()
        }))
        .await;
        w.script(vec![tools(calls(20, "maya")), done("ok")]);
        let a = w.start_run("alice-token", "first").await.unwrap();
        w.wait(a.id).await;
        let ra = texts(&w.events(a.id).await, EventKind::ToolResult);
        assert_eq!(ra.len(), 20);
        assert!(ra.iter().all(|r| !r.contains("at most")), "{ra:?}");
        let v: serde_json::Value = serde_json::from_str(&ra[0]).unwrap();
        assert_eq!(v["description"], "line one\nemail: maya@example.org");
        assert_eq!(v.as_object().unwrap().len(), 6);
        assert!(!ra[0].contains('\n'), "one line, the newline is escaped");
        // The second run is not charged for the first.
        w.script(vec![tools(calls(20, "maya")), done("ok")]);
        let b = w.start_run("alice-token", "second").await.unwrap();
        w.wait(b.id).await;
        let rb = texts(&w.events(b.id).await, EventKind::ToolResult);
        assert_eq!(rb.len(), 20);
        assert!(rb.iter().all(|r| !r.contains("at most")), "{rb:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_malformed_login_does_not_use_a_lookup() {
        let w = world(Some(Profile {
            login: "maya".into(),
            ..Default::default()
        }))
        .await;
        let mut c = calls(25, "a b");
        c.extend(calls(20, "maya"));
        w.script(vec![tools(c), done("ok")]);
        let run = w.start_run("alice-token", "x").await.unwrap();
        w.wait(run.id).await;
        let r = texts(&w.events(run.id).await, EventKind::ToolResult);
        assert_eq!(r.len(), 45);
        assert!(r[..25].iter().all(|t| t == NOT_SHARED), "{r:?}");
        assert!(
            r[25..].iter().all(|t| t.contains("\"login\":\"maya\"")),
            "{r:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_rpc_with_a_run_token_shares_the_runs_allowance_and_other_callers_have_none() {
        use asc_rpc::server::wire;
        use asc_rpc::services::agents_server::Agents;
        let w = world(Some(Profile {
            login: "maya".into(),
            full_name: "Maya".into(),
            ..Default::default()
        }))
        .await;
        let id = w
            .hub
            .core
            .runs
            .create(&super::super::runs::NewRun {
                repo: "acme/app".into(),
                agent: "fixer".into(),
                title: String::new(),
                target: "master".into(),
                base: w.base.clone(),
                task: "t".into(),
                paths: vec![],
                ai_key_id: String::new(),
                model: "m".into(),
                limits: super::super::tokens::Budget::default(),
                created_by: "7:alice".into(),
                starter: "alice".into(),
                owner_id: 7,
                token_id: String::new(),
                workflow_run_id: 0,
                workflow_step: String::new(),
            })
            .await
            .unwrap();
        let secret = super::super::tokens::new_secret();
        let t = w
            .hub
            .core
            .tokens
            .create(
                &super::super::tokens::NewToken {
                    repo: "acme/app".into(),
                    name: "run".into(),
                    agent: "fixer".into(),
                    paths: vec![],
                    max_changes_per_run: 0,
                    created_by: "7:alice".into(),
                    owner_id: 7,
                    created: 0,
                    expires_at: crate::ai::clock::now() + 600,
                    limits: super::super::tokens::Budget::default(),
                    run_id: id,
                },
                &secret,
            )
            .await
            .unwrap()
            .unwrap();
        w.hub.core.tokens.attach_to_run(id, &t.id).await.unwrap();
        assert_eq!(
            w.hub.run_of_principal(&t.principal()).await.unwrap(),
            Some(id)
        );
        assert_eq!(w.hub.run_of_principal("7:alice").await.unwrap(), None);
        let ask = |token: String| {
            let r = w.request(
                wire::GetAgentPersonRequest {
                    repo: super::super::tests::repo_ref(),
                    login: "maya".into(),
                },
                &token,
                None,
            );
            let svc = w.svc.clone();
            async move { svc.get_person(r).await }
        };
        for _ in 0..MAX_PERSON_CALLS {
            assert!(ask(secret.to_string()).await.unwrap().into_inner().allowed);
        }
        let over = ask(secret.to_string()).await.unwrap_err();
        assert_eq!(over.code(), tonic::Code::ResourceExhausted);
        // A person's own token is not a run and is not counted.
        for _ in 0..(MAX_PERSON_CALLS + 5) {
            assert!(
                ask("carol-token".into())
                    .await
                    .unwrap()
                    .into_inner()
                    .allowed
            );
        }
    }
}
