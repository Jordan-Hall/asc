//! The audit actor of a principal id.
//!
//! Principal ids are text the authorizer chose (`7:alice`, `7:alice:3` for a deploy key,
//! `-2:task` for a CI task, `pat:alice`, `host-service`, `agent:<name>`). The audit log stores a
//! kind and a name instead, so a reader can filter on "all agents" without knowing the encodings.

use super::Actor;
use crate::authz::UserIdentity;
use crate::authz::identity::is_host_service;

/// Longest actor name kept, in bytes.
const MAX_NAME: usize = 255;

fn clean(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if out.len() > MAX_NAME {
        let mut end = MAX_NAME;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// The actor `principal_id` stands for.
pub fn actor_of(principal_id: &str) -> Actor {
    if is_host_service(principal_id) {
        return Actor::new("host", "host");
    }
    if let Some(rest) = principal_id.strip_prefix("agent:") {
        return Actor::new("agent", &clean(rest));
    }
    if let Some(user) = principal_id.strip_prefix("pat:") {
        return Actor::new("user", &clean(user));
    }
    if let Some(id) = UserIdentity::parse(principal_id) {
        if id.is_anonymous() {
            return Actor::new("user", "anonymous");
        }
        if id.deploy_key_id != 0 {
            return Actor::new("deploy", &format!("{}:{}", clean(&id.handle), id.deploy_key_id));
        }
        if id.user_id < 0 {
            return Actor::new("ci", &clean(&id.handle));
        }
        return Actor::new("user", &clean(&id.handle));
    }
    if let Some(rest) = principal_id.strip_prefix("system:") {
        return Actor::system(&clean(rest));
    }
    Actor::new("user", &clean(principal_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principals_map_to_kinds() {
        let a = |id: &str| {
            let a = actor_of(id);
            (a.kind, a.name)
        };
        assert_eq!(a("7:alice"), ("user".into(), "alice".into()));
        assert_eq!(a("pat:bob"), ("user".into(), "bob".into()));
        assert_eq!(a("7:alice:3"), ("deploy".into(), "alice:3".into()));
        assert_eq!(a("-2:task"), ("ci".into(), "task".into()));
        assert_eq!(a("0:anonymous"), ("user".into(), "anonymous".into()));
        assert_eq!(a("host-service"), ("host".into(), "host".into()));
        assert_eq!(a("forgejo-internal"), ("host".into(), "host".into()));
        assert_eq!(a("agent:reviewer-1"), ("agent".into(), "reviewer-1".into()));
        assert_eq!(a("system:retention"), ("system".into(), "retention".into()));
        assert_eq!(a("alice"), ("user".into(), "alice".into()));
    }

    #[test]
    fn names_are_cleaned_and_bounded() {
        let a = actor_of("pat:a\nb\u{0}c");
        assert_eq!(a.name, "a b c");
        let long = "é".repeat(400);
        assert!(actor_of(&long).name.len() <= MAX_NAME);
    }
}
