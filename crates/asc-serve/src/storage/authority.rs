//! Who may administer a storage target's owner, and who is a site administrator.
//!
//! Every storage and vault call asks this before it touches a row, and every answer that is not
//! a clear yes is a no (fail closed).
//!
//! * **A user owner** is administered by that user, authenticated as themselves (a deploy key or
//!   a CI task is not the user).
//! * **A repository owner** is administered by whoever has admin on the repository, as the
//!   server's authorizer says.
//! * **An organisation owner** is administered by the host application: the server cannot see an
//!   organisation's members, so the host (the hub) checks its own permissions first and then
//!   calls with its service credential. The same credential is the site administrator.
//!
//! A caller that is none of these gets `PERMISSION_DENIED` when it names an owner and
//! `NOT_FOUND` when it names an id, so owners and ids cannot be probed.

use std::sync::Arc;

use asc_rpc::auth::{Access, Authorizer, Credential, Principal};
use async_trait::async_trait;

use super::model::{Owner, OwnerKind};
use crate::authz::{UserIdentity, is_host_service};

/// An authenticated caller.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: Principal,
    /// The credential presented, for repository checks that go back to the authorizer.
    pub credential: Option<Credential>,
}

impl Caller {
    /// The text recorded as the actor of audit events.
    pub fn actor(&self) -> &str {
        &self.principal.id
    }

    pub fn is_host(&self) -> bool {
        is_host_service(&self.principal.id)
    }
}

/// The decisions. Replaceable so a host with its own notion of organisations can answer.
#[async_trait]
pub trait Authority: Send + Sync {
    /// May the caller create, change, verify and delete targets of `owner`?
    async fn can_admin_owner(&self, caller: &Caller, owner: &Owner) -> bool;
    /// May the caller administer repository `repo` (assign storage, migrate)?
    async fn can_admin_repo(&self, caller: &Caller, repo: &str) -> bool;
    /// May the caller use the vault calls?
    async fn is_site_admin(&self, caller: &Caller) -> bool;
}

/// The default decisions described in the module documentation.
pub struct DefaultAuthority {
    authorizer: Arc<dyn Authorizer>,
}

impl DefaultAuthority {
    pub fn new(authorizer: Arc<dyn Authorizer>) -> Self {
        Self { authorizer }
    }
}

#[async_trait]
impl Authority for DefaultAuthority {
    async fn can_admin_owner(&self, caller: &Caller, owner: &Owner) -> bool {
        if caller.is_host() {
            return true;
        }
        match owner.kind {
            OwnerKind::User => UserIdentity::of(&caller.principal)
                .filter(UserIdentity::is_user)
                .is_some_and(|u| u.handle.eq_ignore_ascii_case(&owner.name)),
            OwnerKind::Org => false,
            OwnerKind::Repo => self.can_admin_repo(caller, &owner.name).await,
        }
    }

    async fn can_admin_repo(&self, caller: &Caller, repo: &str) -> bool {
        if caller.is_host() {
            return true;
        }
        self.authorizer
            .authorize_credential(caller.credential.as_ref(), repo, Access::Admin)
            .await
            .is_ok()
    }

    async fn is_site_admin(&self, caller: &Caller) -> bool {
        caller.is_host()
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! An authority for tests with an explicit table.

    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::*;

    /// Allows exactly what the tests grant.
    #[derive(Default)]
    pub struct Table {
        pub owners: Mutex<HashSet<(String, String)>>,
        pub repos: Mutex<HashSet<(String, String)>>,
        pub admins: Mutex<HashSet<String>>,
    }

    impl Table {
        pub fn grant_owner(&self, actor: &str, owner: &Owner) {
            self.owners.lock().unwrap().insert((actor.into(), owner.scope()));
        }
        pub fn grant_repo(&self, actor: &str, repo: &str) {
            self.repos.lock().unwrap().insert((actor.into(), repo.into()));
        }
        pub fn grant_admin(&self, actor: &str) {
            self.admins.lock().unwrap().insert(actor.into());
        }
    }

    #[async_trait]
    impl Authority for Table {
        async fn can_admin_owner(&self, c: &Caller, o: &Owner) -> bool {
            self.owners.lock().unwrap().contains(&(c.actor().into(), o.scope()))
                || (o.kind == OwnerKind::Repo && self.can_admin_repo(c, &o.name).await)
        }
        async fn can_admin_repo(&self, c: &Caller, r: &str) -> bool {
            self.repos.lock().unwrap().contains(&(c.actor().into(), r.into()))
        }
        async fn is_site_admin(&self, c: &Caller) -> bool {
            self.admins.lock().unwrap().contains(c.actor())
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use asc_rpc::auth::{AuthError, BearerToken};

    use super::*;

    struct Repos(&'static str);

    #[async_trait]
    impl Authorizer for Repos {
        async fn authorize(
            &self,
            _t: Option<&BearerToken>,
            repo: &str,
            access: Access,
        ) -> Result<Principal, AuthError> {
            if repo == self.0 && access == Access::Admin {
                Ok(Principal { id: "x".into() })
            } else {
                Err(AuthError::Forbidden)
            }
        }
    }

    fn caller(id: &str) -> Caller {
        Caller {
            principal: Principal { id: id.into() },
            credential: None,
        }
    }

    #[tokio::test]
    async fn the_default_rules() {
        let a = DefaultAuthority::new(Arc::new(Repos("acme/app")));
        let alice = Owner::new(OwnerKind::User, "Alice").unwrap();
        let acme = Owner::new(OwnerKind::Org, "acme").unwrap();
        let app = Owner::new(OwnerKind::Repo, "acme/app").unwrap();
        let other = Owner::new(OwnerKind::Repo, "acme/other").unwrap();
        let me = caller("7:alice");
        assert!(a.can_admin_owner(&me, &alice).await);
        assert!(!a.can_admin_owner(&caller("8:bob"), &alice).await);
        // A deploy key of alice is not alice.
        assert!(!a.can_admin_owner(&caller("7:alice:3"), &alice).await);
        // Anonymous, a CI task and a stranger are nothing.
        assert!(!a.can_admin_owner(&caller("0:anonymous"), &alice).await);
        assert!(!a.can_admin_owner(&caller("pat:alice"), &alice).await);
        // Organisations are the host's.
        assert!(!a.can_admin_owner(&me, &acme).await);
        let host = caller(crate::authz::HOST_SERVICE_PRINCIPAL);
        assert!(a.can_admin_owner(&host, &acme).await && a.can_admin_owner(&host, &alice).await);
        // Repositories follow the authorizer.
        assert!(a.can_admin_owner(&me, &app).await);
        assert!(!a.can_admin_owner(&me, &other).await);
        assert!(a.can_admin_repo(&me, "acme/app").await);
        assert!(!a.can_admin_repo(&me, "acme/other").await);
        assert!(a.is_site_admin(&host).await && !a.is_site_admin(&me).await);
    }
}
