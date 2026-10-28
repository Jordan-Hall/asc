//! The action catalogue and the details of an event (docs/BUILDOUT.md section 3).
//!
//! Every action has a dotted name, a description and the keys its `details` may hold. An event
//! whose action is not here is refused at append time, so the log has a closed vocabulary that
//! `ListActions` can publish. [`sanitize_details`] keeps only allowed keys, cuts text to
//! [`MAX_DETAIL_BYTES`], reduces a URL to scheme, host and path, and drops a value that looks like
//! a credential. Units that add an audited action add a line to [`CATALOGUE`].

use super::{Details, MAX_DETAIL_BYTES};

/// One audited action.
#[derive(Clone, Copy, Debug)]
pub struct ActionInfo {
    pub action: &'static str,
    pub description: &'static str,
    pub detail_keys: &'static [&'static str],
}

const fn a(
    action: &'static str,
    description: &'static str,
    detail_keys: &'static [&'static str],
) -> ActionInfo {
    ActionInfo {
        action,
        description,
        detail_keys,
    }
}

/// Every action the log accepts.
pub const CATALOGUE: &[ActionInfo] = &[
    // Refs and pushes.
    a(
        "repo.ref.update",
        "A ref was created, moved or deleted.",
        &["ref", "old", "new", "via", "op_id", "refs_total"],
    ),
    a(
        "repo.ref.undo",
        "A ref was put back to the value it had before an earlier update.",
        &[
            "ref",
            "old",
            "new",
            "undone_seq",
            "undone_actor",
            "undone_op",
            "reason",
            "op_id",
        ],
    ),
    a(
        "push.refused",
        "A push or ref update was refused by authorisation, policy or a hook.",
        &["ref", "reason", "via", "code", "refs_total"],
    ),
    // Repositories.
    a(
        "repo.created",
        "A repository was created.",
        &["name", "mode", "default_branch"],
    ),
    a("repo.deleted", "A repository was deleted.", &["name"]),
    a("repo.renamed", "A repository was renamed.", &["from", "to"]),
    a(
        "settings.updated",
        "A setting of a repository, an owner or the server changed.",
        &["setting", "old", "new"],
    ),
    // Policy.
    a(
        "policy.proposed",
        "A change to the repository policy was proposed.",
        &["change", "summary"],
    ),
    a(
        "policy.denied",
        "A change was refused by the repository policy.",
        &["path", "ref", "reason"],
    ),
    // Stacks and the merge queue.
    a(
        "stack.restacked",
        "A stack was rewritten onto a new base.",
        &["change", "onto", "count"],
    ),
    a(
        "stack.reverted",
        "A landed stack was reverted into new changes.",
        &["change", "onto", "count", "ref"],
    ),
    a(
        "queue.enqueued",
        "A change was added to the merge queue.",
        &["change", "target", "style"],
    ),
    a(
        "queue.dequeued",
        "A change left the merge queue without landing.",
        &["change", "target", "reason"],
    ),
    a(
        "queue.landed",
        "A queued change landed.",
        &["change", "target", "landed_as"],
    ),
    a(
        "queue.paused",
        "A merge queue was paused or resumed.",
        &["target", "paused", "reason"],
    ),
    // Dependency scanning.
    a(
        "deps.finding.updated",
        "The state of a dependency advisory finding changed.",
        &["finding", "state", "advisory"],
    ),
    a(
        "deps.proposed",
        "A dependency update was recorded as a change on an agent/deps ref.",
        &["ref", "change", "updates"],
    ),
    a(
        "deps.config.proposed",
        "A change to .asc/deps.toml was recorded on an agent/deps ref.",
        &["ref", "change"],
    ),
    // Secret scanning and signing.
    a(
        "secret.finding.updated",
        "The state of a secret finding changed.",
        &["finding", "state", "rule"],
    ),
    a(
        "secret.push.blocked",
        "A push was blocked because it held a secret.",
        &["ref", "rule", "count"],
    ),
    a(
        "signing.key.added",
        "A signing key was added.",
        &["fingerprint", "kind"],
    ),
    a(
        "signing.key.revoked",
        "A signing key was revoked.",
        &["fingerprint", "kind"],
    ),
    a(
        "signing.signature.submitted",
        "A signature was submitted for a change.",
        &["change", "version", "fingerprint"],
    ),
    // Storage, AI keys and the vault.
    a(
        "storage.target.created",
        "A storage target was created.",
        &["target", "kind", "endpoint", "bucket", "scope"],
    ),
    a(
        "storage.target.updated",
        "A storage target was changed.",
        &["target", "kind", "endpoint", "bucket", "scope"],
    ),
    a(
        "storage.target.verified",
        "A storage target passed or failed its probe.",
        &["target", "result"],
    ),
    a(
        "storage.target.enabled",
        "A storage target was switched on or off.",
        &["target", "enabled"],
    ),
    a(
        "storage.target.deleted",
        "A storage target was deleted.",
        &["target"],
    ),
    a(
        "storage.migration.started",
        "A move of objects between storage targets started.",
        &["from", "to", "objects"],
    ),
    a(
        "storage.migration.finished",
        "A move of objects between storage targets finished.",
        &["from", "to", "objects", "result"],
    ),
    a(
        "ai.key.created",
        "An AI provider key was added.",
        &["provider", "key_hint", "scope"],
    ),
    a(
        "ai.key.updated",
        "An AI provider key was replaced.",
        &["provider", "key_hint", "scope"],
    ),
    a(
        "ai.key.tested",
        "An AI provider key was tested.",
        &["provider", "result"],
    ),
    a(
        "ai.key.deleted",
        "An AI provider key was deleted.",
        &["provider", "scope"],
    ),
    a(
        "vault.rotated",
        "The master key of the vault was rotated.",
        &["generation"],
    ),
    // Authentication.
    a(
        "auth.token.created",
        "An access token was created.",
        &["token", "name", "kind", "expires_at"],
    ),
    a(
        "auth.token.revoked",
        "An access token was revoked.",
        &["token", "name"],
    ),
    a(
        "auth.ssh_key.added",
        "An SSH key was added.",
        &["fingerprint", "title"],
    ),
    a(
        "auth.ssh_key.removed",
        "An SSH key was removed.",
        &["fingerprint", "title"],
    ),
    // Agents and workflows.
    a(
        "agent.token.created",
        "An agent token was created.",
        &["token", "agent", "expires_at", "max_changes_per_run"],
    ),
    a(
        "agent.token.revoked",
        "An agent token was revoked.",
        &["token", "agent"],
    ),
    a(
        "agent.budget.exceeded",
        "An agent ran into a budget.",
        &["token", "budget", "limit"],
    ),
    a(
        "agent.run.created",
        "An agent run started.",
        &["run", "agent"],
    ),
    a(
        "review.comment.added",
        "A review comment was added to a change.",
        &["thread", "comment", "version", "path", "suggestion", "external_id"],
    ),
    a(
        "review.thread.resolved",
        "A review thread was resolved or reopened.",
        &["thread", "resolved"],
    ),
    a(
        "review.requested",
        "A review was requested from, or withdrawn from, people.",
        &["add", "remove"],
    ),
    a(
        "agent.run.cancelled",
        "An agent run was cancelled.",
        &["run", "agent", "reason"],
    ),
    a(
        "agent.run.finished",
        "An agent run finished.",
        &["run", "agent", "state"],
    ),
    a(
        "workflow.enabled",
        "A workflow was switched on or off.",
        &["workflow", "enabled"],
    ),
    a(
        "workflow.triggered",
        "A workflow run started.",
        &["workflow", "run", "trigger"],
    ),
    a(
        "workflow.approval.decided",
        "An approval of a workflow step was decided.",
        &["workflow", "run", "decision"],
    ),
    // CI.
    a(
        "ci.failure_group.state",
        "The state of a CI failure group changed (acknowledged, quarantined, resolved).",
        &["group", "state", "test", "job", "note"],
    ),
    // The log itself.
    a(
        "audit.exported",
        "The audit log was exported.",
        &["scope", "format", "filter"],
    ),
    a(
        "audit.retention.pruned",
        "Events older than the retention period were dropped.",
        &["scope", "events", "until_seq"],
    ),
];

/// The catalogue entry of `action`.
pub fn info(action: &str) -> Option<&'static ActionInfo> {
    CATALOGUE.iter().find(|i| i.action == action)
}

/// Cuts `s` to `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Replaces control characters with spaces and trims.
pub fn plain(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Reduces `value` to scheme, host (and port) and path when it is an `http(s)` URL: the user
/// info, the query and the fragment of a URL carry credentials and signatures.
fn strip_url(value: &str) -> Option<String> {
    let lower = value.get(..8).unwrap_or(value).to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    let (scheme, rest) = value.split_once("://")?;
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some(format!("{}://{host}{path}", scheme.to_ascii_lowercase()))
}

const SECRET_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "sk-",
    "sk_live_",
    "rk_live_",
    "bearer ",
    "basic ",
    "token ",
];

/// Whether `value` looks like a credential: a private key, a well known token prefix, a JWT, an
/// `key=value` assignment of a secret, user info with a password, or a long random-looking token.
/// It is a filter for an allow-listed field, so it errs on the side of dropping.
pub fn looks_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("-----begin ") || lower.contains("private key") {
        return true;
    }
    if SECRET_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // Cloud key ids: `AKIA`/`ASIA` and 16 upper case letters or digits; Google keys `AIza`.
    let cloud_id = |s: &str| {
        ((s.starts_with("AKIA") || s.starts_with("ASIA"))
            && s.len() >= 20
            && s.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
            || (s.starts_with("AIza") && s.len() >= 35)
    };
    if value.split_whitespace().any(cloud_id) {
        return true;
    }
    for needle in [
        "password=",
        "passwd=",
        "secret=",
        "token=",
        "api_key=",
        "apikey=",
        "api-key=",
        "access_key=",
        "authorization:",
        "x-amz-signature=",
        "signature=",
    ] {
        if lower.contains(needle) {
            return true;
        }
    }
    // `scheme://user:password@host`.
    if let Some((_, rest)) = value.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if let Some((userinfo, _)) = authority.rsplit_once('@')
            && userinfo.contains(':')
        {
            return true;
        }
    }
    // A JWT: three base64url parts, the first starting with `eyJ`.
    if value.starts_with("eyJ") && value.matches('.').count() == 2 {
        return true;
    }
    // A long run of base64 characters that is neither hex (object ids) nor a fingerprint.
    let token_like = |s: &str| {
        s.len() >= 40
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
            && s.bytes().any(|b| b.is_ascii_uppercase())
            && s.bytes().any(|b| b.is_ascii_lowercase())
            && s.bytes().any(|b| b.is_ascii_digit())
    };
    value.split_whitespace().any(token_like)
}

/// The details an event of `action` may keep: only allowed keys, text cut and cleaned, URLs
/// reduced, credential-like values dropped. An unknown action keeps nothing.
pub fn sanitize_details(action: &str, details: &[(String, String)]) -> Details {
    let Some(info) = info(action) else {
        return Details::new();
    };
    let mut out = Details::new();
    for (key, value) in details {
        if !info.detail_keys.contains(&key.as_str()) {
            continue;
        }
        let text = plain(value);
        let text = strip_url(&text).unwrap_or(text);
        if looks_secret(&text) {
            continue;
        }
        out.insert(key.clone(), cut(&text, MAX_DETAIL_BYTES));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn the_catalogue_has_unique_dotted_names_and_documented_keys() {
        let mut seen = std::collections::HashSet::new();
        for i in CATALOGUE {
            assert!(seen.insert(i.action), "{} twice", i.action);
            assert!(i.action.contains('.'), "{}", i.action);
            assert!(!i.description.is_empty());
            assert!(!i.detail_keys.is_empty(), "{}", i.action);
            for k in i.detail_keys {
                assert!(
                    k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                    "{k}"
                );
            }
        }
    }

    #[test]
    fn the_buildout_actions_are_all_present() {
        for name in [
            "policy.proposed",
            "repo.ref.update",
            "repo.ref.undo",
            "stack.reverted",
            "stack.restacked",
            "queue.enqueued",
            "queue.dequeued",
            "queue.landed",
            "queue.paused",
            "secret.finding.updated",
            "signing.key.added",
            "signing.key.revoked",
            "signing.signature.submitted",
            "storage.target.created",
            "storage.target.updated",
            "storage.target.verified",
            "storage.target.enabled",
            "storage.target.deleted",
            "storage.migration.started",
            "storage.migration.finished",
            "ai.key.created",
            "ai.key.updated",
            "ai.key.tested",
            "ai.key.deleted",
            "vault.rotated",
            "agent.token.created",
            "agent.token.revoked",
            "agent.run.created",
            "agent.run.cancelled",
            "agent.run.finished",
            "workflow.enabled",
            "workflow.triggered",
            "workflow.approval.decided",
            "secret.push.blocked",
            "agent.budget.exceeded",
            "policy.denied",
        ] {
            assert!(info(name).is_some(), "{name}");
        }
        assert!(info("nope.nothing").is_none());
    }

    #[test]
    fn only_allowed_keys_survive() {
        let d = sanitize_details(
            "repo.ref.update",
            &[
                kv("ref", "refs/heads/main"),
                kv("password", "x"),
                kv("via", "git-push"),
            ],
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d["ref"], "refs/heads/main");
        assert!(sanitize_details("no.such", &[kv("ref", "x")]).is_empty());
    }

    #[test]
    fn text_is_cleaned_and_cut() {
        let d = sanitize_details(
            "settings.updated",
            &[kv("setting", "a\nb\tc"), kv("new", &"é".repeat(300))],
        );
        assert_eq!(d["setting"], "a b c");
        assert!(d["new"].len() <= MAX_DETAIL_BYTES);
        assert!(d["new"].chars().all(|c| c == 'é'));
    }

    #[test]
    fn urls_lose_user_info_query_and_fragment() {
        let d = sanitize_details(
            "storage.target.created",
            &[kv(
                "endpoint",
                "https://AK:pw@s3.example.com:9000/base?X-Amz-Signature=abc&x=1#frag",
            )],
        );
        assert_eq!(d["endpoint"], "https://s3.example.com:9000/base");
        assert_eq!(
            sanitize_details("storage.target.created", &[kv("endpoint", "https://h")])["endpoint"],
            "https://h"
        );
    }

    #[test]
    fn credential_like_values_are_dropped() {
        let secrets = [
            "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            "github_pat_11AAAA",
            "glpat-abcdefghijklmnop",
            "sk-ant-api03-abcdef",
            "AKIAIOSFODNN7EXAMPLE",
            "Bearer abc.def",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
            "postgres://user:hunter2@db/x",
            "password=hunter2",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY0123",
            "x-amz-signature=deadbeef",
        ];
        for s in secrets {
            assert!(looks_secret(s), "{s}");
            let d = sanitize_details("settings.updated", &[kv("new", s)]);
            assert!(d.is_empty(), "{s} was kept");
        }
    }

    #[test]
    fn ordinary_values_are_kept() {
        for s in [
            "refs/users/7/heads/feature-with-a-long-name-for-the-thing-we-do",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "SHA256:AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfG",
            "alice",
            "main",
            "https://hub.example/acme/app",
        ] {
            assert!(!looks_secret(s), "{s}");
        }
    }
}
