//! `Insights.ListOwnerContributors` (feature `insights-owner.v1`): the contributors of several
//! repositories of one owner, summed by person, for the organisation People page.
//!
//! Who may see which repository is decided once per call, like the other services:
//!
//! * the host service with a forwarded claim that verifies for the organisation (read) acts for
//!   the person of the claim. The hub names the repositories its permission model lets that person
//!   read; the claim is the whole of the authority, so a claim that does not verify, or a role
//!   below read, is `PERMISSION_DENIED`;
//! * the host service without a claim is system work: every named repository is counted;
//! * any other caller is asked about each repository in turn with its own credential. One it may
//!   not read, or one that does not exist, is left out and not counted, so the answer cannot tell
//!   a private repository from a missing one.
//!
//! Each repository is the `ListContributors` of that repository; the lists are summed by kind and
//! lower-case name. The median review time does not add up across repositories and is not given.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use asc_rpc::IdMode;
use asc_rpc::auth::{Access, AdminProof, AuthError, Authorizer, Credential, Principal};
use asc_rpc::forwarded::Role;
use asc_rpc::server::{CallCtx, RepoName, wire};
use tonic::metadata::MetadataMap;
use tonic::{Code, Status};

use super::{MAX_CONTRIBUTORS, WEEKS, Window};
use crate::authz::claim::{HostCall, classify, subject};
use crate::review::Reviews;

/// The feature string `Version.features` carries when the call is served.
pub const FEATURE: &str = "insights-owner.v1";
/// Repositories one call may name.
pub const MAX_REPOS: usize = 64;
/// How long one call may spend on its repositories; the rest are left out and the answer says
/// it is truncated.
pub const BUDGET: Duration = Duration::from_secs(20);
/// Longest owner name.
const MAX_OWNER: usize = 64;

/// Whose rights decide the repositories of a call.
enum Reader {
    /// Every named repository, as this principal (the person of a verified claim, or the host).
    All(Principal),
    /// Each repository by the caller's own credential.
    Each,
}

/// The owner, the named repositories (lower case, without duplicates) and the window, checked.
fn checked(req: &wire::ListOwnerContributorsRequest) -> Result<(String, Vec<RepoName>), Status> {
    let owner = req.owner.to_ascii_lowercase();
    let owner_ok = !owner.is_empty()
        && owner.len() <= MAX_OWNER
        && !owner.starts_with(['.', '-'])
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !owner_ok {
        return Err(Status::invalid_argument(
            "an owner is 1 to 64 characters of [A-Za-z0-9._-]",
        ));
    }
    if req.repos.is_empty() || req.repos.len() > MAX_REPOS {
        return Err(Status::invalid_argument(format!(
            "name 1 to {MAX_REPOS} repositories"
        )));
    }
    Window::new(req.days, req.until)?;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(req.repos.len());
    for r in &req.repos {
        let name = RepoName::parse(r)?;
        let lower = name.as_str().to_ascii_lowercase();
        if lower.split_once('/').map(|(o, _)| o) != Some(owner.as_str()) {
            return Err(Status::invalid_argument(
                "every repository is one of the owner's",
            ));
        }
        if seen.insert(lower) {
            out.push(name);
        }
    }
    Ok((owner, out))
}

#[derive(Default)]
struct Sum {
    name: String,
    landed: u32,
    reviews: u32,
    repos: u32,
    weekly: [u32; WEEKS],
    user_id: u64,
}

/// Adds one repository's list to the sums.
fn add(sums: &mut BTreeMap<(i32, String), Sum>, list: &[wire::Contributor]) {
    for c in list {
        let e = sums
            .entry((c.kind, c.name.to_ascii_lowercase()))
            .or_insert_with(|| Sum {
                name: c.name.clone(),
                ..Sum::default()
            });
        e.landed = e.landed.saturating_add(c.landed);
        e.reviews = e.reviews.saturating_add(c.reviews_given);
        e.repos += 1;
        for (w, n) in e.weekly.iter_mut().zip(&c.weekly_landed) {
            *w = w.saturating_add(*n);
        }
        if e.user_id == 0 {
            e.user_id = c.user_id;
        }
    }
}

/// The answer from the sums: landed first, then reviews given, then name.
fn listed(sums: BTreeMap<(i32, String), Sum>) -> Vec<wire::OwnerContributor> {
    let mut list: Vec<wire::OwnerContributor> = sums
        .into_iter()
        .map(|((kind, _), s)| wire::OwnerContributor {
            name: s.name,
            kind,
            landed: s.landed,
            reviews_given: s.reviews,
            repos: s.repos,
            weekly_landed: s.weekly.to_vec(),
            user_id: s.user_id,
        })
        .collect();
    list.sort_by(|a, b| {
        b.landed
            .cmp(&a.landed)
            .then(b.reviews_given.cmp(&a.reviews_given))
            .then(a.name.cmp(&b.name))
    });
    list.truncate(MAX_CONTRIBUTORS);
    list
}

/// Answers one call.
pub(crate) async fn list(
    hub: &Reviews,
    authorizer: &dyn Authorizer,
    meta: &MetadataMap,
    credential: Option<&Credential>,
    req: wire::ListOwnerContributorsRequest,
) -> Result<wire::ListOwnerContributorsResponse, Status> {
    let (owner, repos) = checked(&req)?;
    let id = authorizer
        .authenticate(credential)
        .await
        .map(|p| p.id)
        .unwrap_or_default();
    let reader = match classify(authorizer, &id, meta, &subject::org(&owner)) {
        HostCall::Person(c) if c.allows(Role::Read) => Reader::All(Principal { id: c.user }),
        HostCall::Person(_) | HostCall::Refused => {
            return Err(Status::permission_denied(
                "the claim the host service forwarded does not cover this call",
            ));
        }
        HostCall::System => Reader::All(Principal { id }),
        HostCall::NotHost => Reader::Each,
    };
    let deadline = tokio::time::Instant::now() + BUDGET;
    let mut sums = BTreeMap::new();
    let (mut landed, mut counted, mut truncated) = (0u32, 0u32, false);
    for repo in repos {
        if tokio::time::Instant::now() >= deadline {
            truncated = true;
            break;
        }
        let principal = match &reader {
            Reader::All(p) => p.clone(),
            Reader::Each => match authorizer
                .authorize_credential(credential, repo.as_str(), Access::Read)
                .await
            {
                Ok(p) => p,
                Err(AuthError::Forbidden | AuthError::Unauthenticated) => continue,
                Err(e @ AuthError::Malformed) => return Err(e.into()),
            },
        };
        let ctx = CallCtx {
            repo: repo.clone(),
            id_mode: IdMode::Asc,
            principal,
            op_id: None,
            client: None,
            admin: AdminProof::default(),
        };
        let one = wire::ListContributorsRequest {
            repo: Some(wire::RepoRef {
                name: repo.as_str().to_owned(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            days: req.days,
            kind: req.kind,
            until: req.until,
        };
        let got = match tokio::time::timeout_at(deadline, hub.contributors_all(&ctx, one)).await {
            Err(_) => {
                truncated = true;
                break;
            }
            Ok(Err(s)) if s.code() == Code::NotFound => continue,
            Ok(r) => r?,
        };
        counted += 1;
        landed = landed.saturating_add(got.landed);
        truncated |= got.truncated;
        add(&mut sums, &got.contributors);
    }
    Ok(wire::ListOwnerContributorsResponse {
        contributors: listed(sums),
        landed,
        counted_repos: counted,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, landed: u32, reviews: u32, user_id: u64) -> wire::Contributor {
        let mut weekly = vec![0; WEEKS];
        weekly[WEEKS - 1] = landed;
        wire::Contributor {
            name: name.into(),
            kind: wire::AuthorKind::Person as i32,
            landed,
            reviews_given: reviews,
            weekly_landed: weekly,
            user_id,
            ..wire::Contributor::default()
        }
    }

    #[test]
    fn the_lists_are_summed_by_kind_and_name_whatever_the_case() {
        let mut sums = BTreeMap::new();
        add(
            &mut sums,
            &[person("Alice", 2, 1, 0), person("bob", 1, 0, 7)],
        );
        add(&mut sums, &[person("alice", 3, 0, 42)]);
        let mut agent = person("alice", 5, 0, 0);
        agent.kind = wire::AuthorKind::Agent as i32;
        add(&mut sums, &[agent]);
        let got = listed(sums);
        let rows: Vec<(&str, i32, u32, u32, u32, u64)> = got
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.kind,
                    c.landed,
                    c.reviews_given,
                    c.repos,
                    c.user_id,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("Alice", wire::AuthorKind::Person as i32, 5, 1, 2, 42),
                ("alice", wire::AuthorKind::Agent as i32, 5, 0, 1, 0),
                ("bob", wire::AuthorKind::Person as i32, 1, 0, 1, 7),
            ]
        );
        assert_eq!(got[0].weekly_landed[WEEKS - 1], 5);
        assert_eq!(got[0].weekly_landed.len(), WEEKS);
    }

    #[test]
    fn the_request_names_repositories_of_the_owner_only() {
        let ask = |owner: &str, repos: &[&str]| wire::ListOwnerContributorsRequest {
            owner: owner.into(),
            repos: repos.iter().map(|r| (*r).to_owned()).collect(),
            ..wire::ListOwnerContributorsRequest::default()
        };
        let (owner, repos) = checked(&ask("Acme", &["acme/a", "ACME/b", "acme/A"])).unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(
            repos.iter().map(RepoName::as_str).collect::<Vec<_>>(),
            ["acme/a", "ACME/b"]
        );
        for bad in [
            ask("acme", &[]),
            ask("acme", &["orbit/a"]),
            ask("acme", &["acme"]),
            ask("", &["acme/a"]),
            ask("ac/me", &["ac/me/a"]),
            ask("-acme", &["-acme/a"]),
        ] {
            assert_eq!(checked(&bad).unwrap_err().code(), Code::InvalidArgument);
        }
        let many: Vec<String> = (0..=MAX_REPOS).map(|i| format!("acme/r{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            checked(&ask("acme", &refs)).unwrap_err().code(),
            Code::InvalidArgument
        );
        let mut far = ask("acme", &["acme/a"]);
        far.until = i64::MAX;
        assert_eq!(checked(&far).unwrap_err().code(), Code::InvalidArgument);
    }
}
