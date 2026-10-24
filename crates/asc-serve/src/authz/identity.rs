//! What every provider-independent part of the server knows about who a caller is.
//!
//! A provider that authenticates real users (the token store does not; its principals are
//! `pat:<user>` and so on) mints a [`UserIdentity`] and records it in [`Principal::id`] with
//! [`UserIdentity::principal_id`]. The ref log, the push hooks and the post-receive effect all read
//! the same text back, so nothing needs shared state. Core code asks [`UserIdentity::of`] and never
//! parses a principal id itself.
//!
//! A host-service caller (the host application acting on its own behalf, for example to merge)
//! is [`HOST_SERVICE_PRINCIPAL`]; test it with [`is_host_service`].

use std::time::Duration;

use asc_rpc::auth::Principal;

/// How long a positive authorisation answer is cached (SPEC §15.1).
pub const POSITIVE_TTL: Duration = Duration::from_secs(60);
/// How long a negative authorisation answer is cached (SPEC §15.1).
pub const NEGATIVE_TTL: Duration = Duration::from_secs(5);

/// The principal of a caller that presented the host application's own service credential.
pub const HOST_SERVICE_PRINCIPAL: &str = "host-service";
/// The value [`HOST_SERVICE_PRINCIPAL`] had before it was made neutral. Ref-log actors and
/// token-store rows written by earlier versions carry it, so it still counts as the host service.
pub const LEGACY_HOST_SERVICE_PRINCIPAL: &str = "forgejo-internal";

/// Whether `principal_id` names the host service (the current or the legacy spelling).
pub fn is_host_service(principal_id: &str) -> bool {
    principal_id == HOST_SERVICE_PRINCIPAL || principal_id == LEGACY_HOST_SERVICE_PRINCIPAL
}

/// The user id of a CI task, which the provider maps to no real user.
const CI_TASK_USER_ID: i64 = -2;

/// A user (or a deploy key acting for its repository) a provider verified.
///
/// Encoded in [`Principal::id`] as `<user_id>:<handle>` or `<user_id>:<handle>:<deploy_key_id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserIdentity {
    /// The provider's numeric id of the user; 0 is the anonymous caller.
    pub user_id: i64,
    /// The user's login name.
    pub handle: String,
    /// The deploy key the caller used; 0 when the caller is not a deploy key.
    pub deploy_key_id: i64,
}

impl UserIdentity {
    /// A validated identity; `None` for a malformed handle or id.
    pub fn new(user_id: i64, handle: &str, deploy_key_id: i64) -> Option<Self> {
        let handle_ok = !handle.is_empty()
            && handle.len() <= 255
            && handle.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+')
            });
        let id_ok = user_id >= 0 || user_id == CI_TASK_USER_ID;
        (handle_ok && id_ok && deploy_key_id >= 0).then(|| Self {
            user_id,
            handle: handle.to_owned(),
            deploy_key_id,
        })
    }

    /// The anonymous caller.
    pub fn anonymous() -> Self {
        Self {
            user_id: 0,
            handle: "anonymous".into(),
            deploy_key_id: 0,
        }
    }

    /// The text stored in [`Principal::id`].
    pub fn principal_id(&self) -> String {
        if self.deploy_key_id == 0 {
            format!("{}:{}", self.user_id, self.handle)
        } else {
            format!("{}:{}:{}", self.user_id, self.handle, self.deploy_key_id)
        }
    }

    /// Parses a [`principal_id`](Self::principal_id); `None` for any other principal text.
    pub fn parse(id: &str) -> Option<Self> {
        let mut parts = id.split(':');
        let user_id = parts.next()?.parse().ok()?;
        let handle = parts.next()?;
        let deploy_key_id = match parts.next() {
            Some(d) => d.parse().ok()?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        Self::new(user_id, handle, deploy_key_id)
    }

    /// The identity a provider recorded on `principal`, if it did.
    pub fn of(principal: &Principal) -> Option<Self> {
        Self::parse(&principal.id)
    }

    /// Whether this is a real user acting as themselves: not anonymous, not a CI task and not a
    /// deploy key (which acts for its repository, not for its owner).
    pub fn is_user(&self) -> bool {
        self.user_id > 0 && self.deploy_key_id == 0
    }

    /// Whether the provider admitted the caller without a credential of its own.
    pub fn is_anonymous(&self) -> bool {
        self.user_id == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_other_principals() {
        let i = UserIdentity::new(7, "alice", 0).unwrap();
        assert_eq!(i.principal_id(), "7:alice");
        assert_eq!(UserIdentity::parse("7:alice"), Some(i.clone()));
        assert!(i.is_user() && !i.is_anonymous());
        let d = UserIdentity::new(7, "alice", 3).unwrap();
        assert_eq!(UserIdentity::parse(&d.principal_id()), Some(d.clone()));
        assert!(!d.is_user());
        assert!(UserIdentity::anonymous().is_anonymous());
        assert_eq!(UserIdentity::parse(HOST_SERVICE_PRINCIPAL), None);
        assert_eq!(UserIdentity::parse(LEGACY_HOST_SERVICE_PRINCIPAL), None);
        assert_eq!(UserIdentity::parse("pat:alice"), None);
        assert_eq!(UserIdentity::parse("1:a:b:c"), None);
        assert_eq!(UserIdentity::parse("-1:a"), None);
        assert!(!UserIdentity::parse("-2:ci").unwrap().is_user());
        assert!(UserIdentity::new(1, "a:b", 0).is_none());
        assert!(UserIdentity::new(1, "", 0).is_none());
    }

    #[test]
    fn the_legacy_host_service_value_is_still_the_host_service() {
        assert!(is_host_service(HOST_SERVICE_PRINCIPAL));
        assert!(is_host_service("forgejo-internal"));
        assert!(!is_host_service("pat:forgejo-internal"));
        assert!(!is_host_service("1:alice"));
    }
}
