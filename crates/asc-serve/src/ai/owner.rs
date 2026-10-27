//! Who an AI key belongs to, and who may administer it.
//!
//! A key belongs to exactly one owner: a user, an organisation, a repository or the instance.
//! A caller who cannot administer the owner of a key gets `NOT_FOUND`, never `PERMISSION_DENIED`,
//! so key ids and owner names cannot be probed.

use std::sync::Arc;

use asc_rpc::auth::{Access, Authorizer, Credential, Principal};
use asc_rpc::server::{RepoName, wire};
use async_trait::async_trait;
use tonic::Status;

use crate::authz::is_host_service;
use crate::review::handle_of;

/// Longest user or organisation name.
pub const MAX_OWNER_NAME: usize = 64;

/// The owner of a key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    User(String),
    Org(String),
    /// `owner/name`.
    Repo(String),
    Instance,
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_OWNER_NAME
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Owner {
    /// Reads and validates a wire owner. Names are lower case: hosts treat them case-insensitively.
    pub fn from_wire(r: Option<&wire::OwnerRef>) -> Result<Self, Status> {
        let r = r.ok_or_else(|| Status::invalid_argument("missing owner"))?;
        let kind = wire::OwnerKind::try_from(r.kind)
            .map_err(|_| Status::invalid_argument("unknown owner kind"))?;
        let bad = |m: &str| Status::invalid_argument(m.to_owned());
        match kind {
            wire::OwnerKind::Unspecified => Err(bad("the owner kind is required")),
            wire::OwnerKind::User | wire::OwnerKind::Org => {
                if !name_ok(&r.name) {
                    return Err(bad("an owner name is 1 to 64 characters of [A-Za-z0-9._-]"));
                }
                let n = r.name.to_ascii_lowercase();
                Ok(if kind == wire::OwnerKind::User {
                    Self::User(n)
                } else {
                    Self::Org(n)
                })
            }
            wire::OwnerKind::Repo => {
                let n = RepoName::parse(&r.name)
                    .map_err(|_| bad("a repository owner is owner/name"))?;
                Ok(Self::Repo(n.as_str().to_ascii_lowercase()))
            }
            wire::OwnerKind::Instance => {
                if r.name.is_empty() {
                    Ok(Self::Instance)
                } else {
                    Err(bad("the instance owner has no name"))
                }
            }
        }
    }

    pub fn to_wire(&self) -> wire::OwnerRef {
        let (kind, name) = match self {
            Self::User(n) => (wire::OwnerKind::User, n.clone()),
            Self::Org(n) => (wire::OwnerKind::Org, n.clone()),
            Self::Repo(n) => (wire::OwnerKind::Repo, n.clone()),
            Self::Instance => (wire::OwnerKind::Instance, String::new()),
        };
        wire::OwnerRef {
            kind: kind as i32,
            name,
        }
    }

    /// The text the vault binds a secret to: `user:alice`, `org:acme`, `repo:acme/app`, `instance`.
    pub fn label(&self) -> String {
        match self {
            Self::User(n) => format!("user:{n}"),
            Self::Org(n) => format!("org:{n}"),
            Self::Repo(n) => format!("repo:{n}"),
            Self::Instance => "instance".to_owned(),
        }
    }

    /// The `(kind, name)` columns.
    pub fn columns(&self) -> (i64, String) {
        let w = self.to_wire();
        (i64::from(w.kind), w.name)
    }

    pub fn from_columns(kind: i64, name: &str) -> Option<Self> {
        let w = wire::OwnerRef {
            kind: i32::try_from(kind).ok()?,
            name: name.to_owned(),
        };
        Self::from_wire(Some(&w)).ok()
    }

    /// The owners whose keys a run on `repo` started by `starter` may use, most specific first:
    /// the repository, its organisation (or owning user), the starter and the instance.
    pub fn chain(repo: &str, starter: Option<&str>) -> Vec<Self> {
        let repo = repo.to_ascii_lowercase();
        let mut out = vec![Self::Repo(repo.clone())];
        if let Some((o, _)) = repo.split_once('/') {
            out.push(Self::Org(o.to_owned()));
            out.push(Self::User(o.to_owned()));
        }
        if let Some(s) = starter {
            let s = Self::User(s.to_ascii_lowercase());
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out.push(Self::Instance);
        out
    }
}

/// Decides who may administer an owner's keys.
#[async_trait]
pub trait OwnerAdmin: Send + Sync {
    /// Whether the caller may create, read, change and delete the keys of `owner`.
    async fn can_admin(
        &self,
        credential: Option<&Credential>,
        principal: &Principal,
        owner: &Owner,
    ) -> bool;
}

/// The rule the server applies on its own:
///
/// * a user administers their own keys (the handle of their principal names the owner);
/// * a repository is administered by whoever has admin on it;
/// * an organisation is administered by whoever has admin on the probe repository
///   `<org>/.asc-owner`, which a token with an `<org>/*` admin grant has and a forge authorizer
///   (which knows no such repository) does not, so in forge deployments the host application,
///   which has verified the organisation role itself, manages organisation keys;
/// * the instance is administered by the host application only;
/// * the host application's service credential administers everything.
pub struct AuthorizerOwners {
    authorizer: Arc<dyn Authorizer>,
}

impl AuthorizerOwners {
    pub fn new(authorizer: Arc<dyn Authorizer>) -> Self {
        Self { authorizer }
    }
}

#[async_trait]
impl OwnerAdmin for AuthorizerOwners {
    async fn can_admin(
        &self,
        credential: Option<&Credential>,
        principal: &Principal,
        owner: &Owner,
    ) -> bool {
        if is_host_service(&principal.id) {
            return true;
        }
        match owner {
            Owner::User(name) => {
                handle_of(&principal.id).is_some_and(|h| h.eq_ignore_ascii_case(name))
            }
            Owner::Repo(repo) => self
                .authorizer
                .authorize_credential(credential, repo, Access::Admin)
                .await
                .is_ok(),
            Owner::Org(org) => self
                .authorizer
                .authorize_credential(credential, &format!("{org}/.asc-owner"), Access::Admin)
                .await
                .is_ok(),
            Owner::Instance => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(kind: wire::OwnerKind, name: &str) -> wire::OwnerRef {
        wire::OwnerRef {
            kind: kind as i32,
            name: name.into(),
        }
    }

    #[test]
    fn owners_are_validated_and_normalised() {
        assert_eq!(
            Owner::from_wire(Some(&w(wire::OwnerKind::User, "Alice"))).unwrap(),
            Owner::User("alice".into())
        );
        assert_eq!(
            Owner::from_wire(Some(&w(wire::OwnerKind::Repo, "Acme/App"))).unwrap(),
            Owner::Repo("acme/app".into())
        );
        assert_eq!(
            Owner::from_wire(Some(&w(wire::OwnerKind::Instance, ""))).unwrap(),
            Owner::Instance
        );
        for (k, n) in [
            (wire::OwnerKind::Unspecified, "a"),
            (wire::OwnerKind::User, ""),
            (wire::OwnerKind::User, "a/b"),
            (wire::OwnerKind::User, "-a"),
            (wire::OwnerKind::Org, "a b"),
            (wire::OwnerKind::Org, &"a".repeat(65)),
            (wire::OwnerKind::Repo, "noslash"),
            (wire::OwnerKind::Instance, "x"),
        ] {
            assert!(Owner::from_wire(Some(&w(k, n))).is_err(), "{k:?} {n:?}");
        }
        assert!(Owner::from_wire(None).is_err());
        assert!(
            Owner::from_wire(Some(&wire::OwnerRef {
                kind: 99,
                name: "a".into()
            }))
            .is_err()
        );
    }

    #[test]
    fn labels_columns_and_wire_round_trip() {
        for o in [
            Owner::User("a".into()),
            Owner::Org("b".into()),
            Owner::Repo("c/d".into()),
            Owner::Instance,
        ] {
            let (k, n) = o.columns();
            assert_eq!(Owner::from_columns(k, &n), Some(o.clone()));
            assert_eq!(Owner::from_wire(Some(&o.to_wire())).unwrap(), o);
        }
        assert_eq!(Owner::Repo("c/d".into()).label(), "repo:c/d");
        assert_eq!(Owner::Instance.label(), "instance");
    }

    #[test]
    fn the_resolution_chain_is_repo_org_user_instance() {
        let c = Owner::chain("Acme/App", Some("Alice"));
        assert_eq!(
            c,
            [
                Owner::Repo("acme/app".into()),
                Owner::Org("acme".into()),
                Owner::User("acme".into()),
                Owner::User("alice".into()),
                Owner::Instance,
            ]
        );
        // The starter is not listed twice when they own the repository.
        let c = Owner::chain("alice/app", Some("alice"));
        assert_eq!(
            c.iter()
                .filter(|o| **o == Owner::User("alice".into()))
                .count(),
            1
        );
        assert_eq!(Owner::chain("a/b", None).last(), Some(&Owner::Instance));
    }
}
