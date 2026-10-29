//! Who may call a settings service, decided like the hub's permission model.
//!
//! The settings services (`Backups`, `Usage`, `OrgSettings`, `Sessions`, `DataExport`) name an
//! account, an organisation or the whole instance. A call is allowed when:
//!
//! * it comes from the host service with a forwarded claim (see [`asc_rpc::forwarded`]) that
//!   verifies for exactly that subject and whose role covers the call. The claim is the whole of
//!   the authority; the call then acts as the person of the claim, with the host as the relay;
//! * it comes from the host service with a claim that does not verify: refused;
//! * it comes from the host service without a claim: system work, allowed (the host's own
//!   credential administers everything, like every other service of this server);
//! * it comes from anyone else: allowed only for the caller's own account. An organisation or
//!   the instance is never administered directly: the hub knows the roles and says so with a
//!   claim.
//!
//! A refusal is `PERMISSION_DENIED` with one fixed message, so the answer says nothing about
//! which accounts or organisations exist.

use std::sync::Arc;

use asc_rpc::auth::{Authorizer, Principal};
use asc_rpc::forwarded::Role;
use tonic::Status;
use tonic::metadata::MetadataMap;

use crate::ai::caller::{Caller, authenticate};
use crate::authz::claim::{HostCall, classify, subject};
use crate::review::handle_of;

/// Longest account or organisation name accepted.
pub const MAX_NAME: usize = 64;

/// What a settings call is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Subject {
    /// The whole server.
    Instance,
    /// An organisation, lower case.
    Org(String),
    /// An account, lower case.
    User(String),
    /// A repository, `owner/name`, lower case. Decided by the hub's claim only.
    Repo(String),
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Subject {
    /// A user or organisation name, normalised, else `INVALID_ARGUMENT`.
    pub fn user(name: &str) -> Result<Self, Status> {
        Self::named(name).map(Self::User)
    }

    pub fn org(name: &str) -> Result<Self, Status> {
        Self::named(name).map(Self::Org)
    }

    /// A repository, `owner/name`.
    pub fn repo(name: &str) -> Result<Self, Status> {
        asc_rpc::server::RepoName::parse(name)
            .map(|n| Self::Repo(n.as_str().to_ascii_lowercase()))
            .map_err(|_| Status::invalid_argument("a repository is owner/name"))
    }

    fn named(name: &str) -> Result<String, Status> {
        if name_ok(name) {
            Ok(name.to_ascii_lowercase())
        } else {
            Err(Status::invalid_argument(
                "a name is 1 to 64 characters of [A-Za-z0-9._-]",
            ))
        }
    }

    /// The subject string of the hub's claim.
    pub fn claim_subject(&self) -> String {
        match self {
            Self::Instance => subject::INSTANCE.to_owned(),
            Self::Org(n) => subject::org(n),
            Self::User(n) => subject::user(n),
            Self::Repo(n) => subject::repo(n),
        }
    }

    /// The scope written to the audit log: the account or organisation name, empty for the instance.
    pub fn audit_owner(&self) -> &str {
        match self {
            Self::Instance => "",
            Self::Org(n) | Self::User(n) => n,
            Self::Repo(n) => n.split_once('/').map_or("", |(o, _)| o),
        }
    }

    /// `user:alice`, `org:acme` or `instance`: the key rows of the settings tables use.
    pub fn label(&self) -> String {
        self.claim_subject()
    }
}

/// Who a call acts as once it has been allowed.
#[derive(Clone, Debug)]
pub struct Actor {
    /// The principal id of the person (the claim's `<id>:<login>`), or of the direct caller.
    pub id: String,
    /// The host service, when it made the call for the person.
    pub relay: Option<String>,
    /// What an operation id is bound to, so an answer made for one never replays to another.
    pub replay_key: String,
    /// The login of the acting person, lower case, when there is one.
    pub handle: Option<String>,
    /// A call of the host service itself (no person).
    pub system: bool,
}

/// The access rule of the settings services.
#[derive(Clone)]
pub struct Gate {
    authorizer: Arc<dyn Authorizer>,
}

impl Gate {
    pub fn new(authorizer: Arc<dyn Authorizer>) -> Self {
        Self { authorizer }
    }

    /// The authenticated caller of a request; anonymous is `UNAUTHENTICATED`.
    pub async fn caller(&self, ext: &tonic::Extensions) -> Result<Caller, Status> {
        authenticate(&self.authorizer, ext).await
    }

    /// The refusal every denied call gets.
    pub fn denied() -> Status {
        Status::permission_denied("this account may not do that")
    }

    /// The actor, provided `caller` may act on `subject` with `needed`; else `None`.
    pub fn allow(
        &self,
        caller: &Caller,
        meta: &MetadataMap,
        subject: &Subject,
        needed: Role,
    ) -> Option<Actor> {
        let claim_subject = subject.claim_subject();
        let principal: &Principal = &caller.principal;
        match classify(&*self.authorizer, &principal.id, meta, &claim_subject) {
            HostCall::Person(c) => {
                let own =
                    !matches!(subject, Subject::User(n) if !c.login().eq_ignore_ascii_case(n));
                if !(c.allows(needed) && own) {
                    return None;
                }
                Some(Actor {
                    replay_key: format!("{}\n{}\n{claim_subject}", principal.id, c.user),
                    handle: Some(c.login().to_ascii_lowercase()),
                    relay: Some(principal.id.clone()),
                    id: c.user,
                    system: false,
                })
            }
            HostCall::Refused => None,
            HostCall::System => Some(Actor {
                id: principal.id.clone(),
                relay: None,
                replay_key: principal.id.clone(),
                handle: None,
                system: true,
            }),
            HostCall::NotHost => match subject {
                Subject::User(n)
                    if handle_of(&principal.id).is_some_and(|h| h.eq_ignore_ascii_case(n)) =>
                {
                    Some(Actor {
                        id: principal.id.clone(),
                        relay: None,
                        replay_key: principal.id.clone(),
                        handle: Some(n.clone()),
                        system: false,
                    })
                }
                _ => None,
            },
        }
    }

    /// The actor, or `PERMISSION_DENIED`.
    pub fn require(
        &self,
        caller: &Caller,
        meta: &MetadataMap,
        subject: &Subject,
        needed: Role,
    ) -> Result<Actor, Status> {
        self.allow(caller, meta, subject, needed)
            .ok_or_else(Self::denied)
    }

    /// The person's own account: the claim or the direct caller must be a person. The host's
    /// system calls have no person and are refused (used by calls that act "as me").
    pub fn person(
        &self,
        caller: &Caller,
        meta: &MetadataMap,
        needed: Role,
    ) -> Result<(Subject, Actor), Status> {
        let handle = if crate::authz::is_host_service(&caller.principal.id) {
            let user = meta
                .get(asc_rpc::forwarded::USER)
                .and_then(|v| v.to_str().ok())
                .and_then(|u| u.split_once(':'))
                .map(|(_, l)| l.to_ascii_lowercase());
            user.ok_or_else(Self::denied)?
        } else {
            handle_of(&caller.principal.id)
                .ok_or_else(Self::denied)?
                .to_ascii_lowercase()
        };
        let subject = Subject::user(&handle).map_err(|_| Self::denied())?;
        let actor = self.require(caller, meta, &subject, needed)?;
        Ok((subject, actor))
    }
}
