//! The profile of a person, for an agent (docs/BUILDOUT.md section 23).
//!
//! asc keeps no profiles: names, bios and locations belong to the hub, and a person decides there
//! whether agents may read them. Before asc gives any of it to an agent (a run's token, the `ascat_`
//! tokens, MCP clients), [`AgentProfiles::get`] asks the hub (`POST /api/internal/asc/agent-profile`
//! with the internal token) and returns what the hub returned, or nothing:
//!
//! * **Fail closed.** No hub configured, no internal token, a transport error, any status but 200,
//!   an answer that cannot be read, `allowed: false`: all give `None`, and the caller omits the data.
//!   An error is never cached; a refusal is, for [`DENIED_TTL`], so a loop of asks does not hammer the hub.
//! * **Nothing the hub did not give.** There is no email and no avatar in [`Profile`]; the hub sends none.
//! * **Bounded.** A login is checked before the call (the hub's alphabet, at most 64 bytes), the
//!   cache holds [`MAX_CACHED`] logins, and each text is cut to [`MAX_FIELD`] bytes on one line.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::notify::one_line;
use crate::review::valid_handle;

/// How long an answer is reused.
pub const ALLOWED_TTL: Duration = Duration::from_secs(60);
/// How long a refusal is reused.
pub const DENIED_TTL: Duration = Duration::from_secs(30);
/// Logins kept.
pub const MAX_CACHED: usize = 1024;
/// Longest text of a field, in bytes.
pub const MAX_FIELD: usize = 500;

/// The fields of a profile a person lets agents read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub login: String,
    pub full_name: String,
    pub description: String,
    pub location: String,
    pub website: String,
    pub pronouns: String,
}

/// The hub's answer as it comes over the wire (`allowed` and the fields).
#[derive(Debug, Default, serde::Deserialize)]
pub struct HubAnswer {
    #[serde(default)]
    pub allowed: bool,
    #[serde(default)]
    login: String,
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    website: String,
    #[serde(default)]
    pronouns: String,
}

impl HubAnswer {
    pub fn into_profile(self) -> Profile {
        let field = |s: &str| one_line(s, MAX_FIELD);
        Profile {
            login: field(&self.login),
            full_name: field(&self.full_name),
            description: field(&self.description),
            location: field(&self.location),
            website: field(&self.website),
            pronouns: field(&self.pronouns),
        }
    }
}

/// Where profiles come from.
#[async_trait]
pub trait ProfileSource: Send + Sync {
    /// `Ok(None)`: the person does not allow it (or does not exist). `Err`: the hub could not be asked.
    async fn profile(&self, login: &str) -> Result<Option<Profile>, String>;
}

/// The hub of `[forgejo]`.
#[cfg(feature = "forgejo")]
pub struct HubProfiles(pub Arc<crate::forgejo::ForgejoClient>);

#[cfg(feature = "forgejo")]
#[async_trait]
impl ProfileSource for HubProfiles {
    async fn profile(&self, login: &str) -> Result<Option<Profile>, String> {
        self.0.agent_profile(login).await.map_err(|e| e.to_string())
    }
}

/// The question "may this agent read this person's profile?", answered by the hub each time (with a
/// short memory).
pub struct AgentProfiles {
    source: Option<Arc<dyn ProfileSource>>,
    cache: Mutex<HashMap<String, (Instant, Option<Profile>)>>,
}

impl AgentProfiles {
    /// Without a source every ask is refused.
    pub fn new(source: Option<Arc<dyn ProfileSource>>) -> Self {
        Self {
            source,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The profile of `login`, or `None` for any reason at all.
    pub async fn get(&self, login: &str) -> Option<Profile> {
        // The hub's alphabet for a login; the hub refuses `/`, spaces and backslashes too.
        if login.len() > 64 || !valid_handle(login) || login.contains(['@', '+']) {
            return None;
        }
        let source = self.source.as_ref()?;
        let key = login.to_ascii_lowercase();
        if let Some((until, answer)) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            && *until > Instant::now()
        {
            return answer.clone();
        }
        let answer = match source.profile(login).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(event = "agent.profile", error = %one_line(&e, 200), "the hub could not be asked; the profile is withheld");
                return None;
            }
        };
        let ttl = if answer.is_some() {
            ALLOWED_TTL
        } else {
            DENIED_TTL
        };
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= MAX_CACHED {
            let now = Instant::now();
            cache.retain(|_, (until, _)| *until > now);
            if cache.len() >= MAX_CACHED {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now() + ttl, answer.clone()));
        answer
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Fake {
        calls: AtomicUsize,
        answer: Mutex<Result<Option<Profile>, String>>,
    }

    impl Fake {
        fn new(a: Result<Option<Profile>, String>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                answer: Mutex::new(a),
            })
        }
    }

    #[async_trait]
    impl ProfileSource for Fake {
        async fn profile(&self, _login: &str) -> Result<Option<Profile>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.lock().unwrap().clone()
        }
    }

    fn maya() -> Profile {
        Profile {
            login: "maya".into(),
            full_name: "Maya Okafor".into(),
            description: "Builds the checkout".into(),
            ..Profile::default()
        }
    }

    #[tokio::test]
    async fn what_the_hub_allows_is_given_and_remembered_for_a_while() {
        let fake = Fake::new(Ok(Some(maya())));
        let p = AgentProfiles::new(Some(fake.clone()));
        assert_eq!(p.get("maya").await, Some(maya()));
        assert_eq!(p.get("Maya").await, Some(maya()), "case does not matter");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn every_reason_to_say_no_gives_nothing() {
        // The hub says no.
        let no = Fake::new(Ok(None));
        let p = AgentProfiles::new(Some(no.clone()));
        assert_eq!(p.get("maya").await, None);
        assert_eq!(p.get("maya").await, None);
        assert_eq!(
            no.calls.load(Ordering::SeqCst),
            1,
            "a refusal is remembered briefly"
        );
        // The hub cannot be asked: nothing, and the next ask tries again.
        let down = Fake::new(Err("unreachable".into()));
        let p = AgentProfiles::new(Some(down.clone()));
        assert_eq!(p.get("maya").await, None);
        assert_eq!(p.get("maya").await, None);
        assert_eq!(
            down.calls.load(Ordering::SeqCst),
            2,
            "an error is never cached"
        );
        // No hub at all.
        assert_eq!(AgentProfiles::new(None).get("maya").await, None);
        // A login the hub would not accept is not even sent.
        let fake = Fake::new(Ok(Some(maya())));
        let p = AgentProfiles::new(Some(fake.clone()));
        for bad in ["", "a b", "a/b", "a\\b", "a@b", &"x".repeat(65), "x\ny"] {
            assert_eq!(p.get(bad).await, None, "{bad:?}");
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_hub_text_is_one_short_line_and_unknown_fields_are_dropped() {
        let a: HubAnswer = serde_json::from_str(
            r#"{"allowed":true,"login":"maya","full_name":"Maya\nOkafor","description":"x","email":"m@x.io","avatar":"u"}"#,
        )
        .unwrap();
        assert!(a.allowed);
        let p = a.into_profile();
        assert_eq!(p.full_name, "Maya Okafor");
        let long: HubAnswer = serde_json::from_value(
            serde_json::json!({"allowed": true, "description": "d".repeat(2000)}),
        )
        .unwrap();
        assert_eq!(long.into_profile().description.len(), MAX_FIELD);
        // An answer with no `allowed` is a no.
        let none: HubAnswer = serde_json::from_str(r#"{"full_name":"Maya"}"#).unwrap();
        assert!(!none.allowed);
    }
}
